//! Per-process address spaces.
//!
//! An address space is a page-table root, a VAD tree describing what the address space *means*,
//! and a commit charge describing what it costs. The two halves are deliberately separate: the
//! tree is where the POSIX semantics live (`mprotect` splits a region, `munmap` merges its
//! neighbours back, a guard page is a VAD of kind `Guard`), and the page tables are where the
//! hardware looks. This module keeps them in step, and [`resolve`] is the one place that repairs
//! the difference when the hardware wins the race -- a page a program has committed but not yet
//! touched does not exist until the first access faults.
//!
//! # What is shared with the kernel
//!
//! A new address space starts as an empty user half and the kernel's own half: the PML4 entries
//! for the direct map, the image window, and the recursive slot are copied from the root the
//! kernel is running on (see `pml4_index(KERNEL_BASE)`). Those entries point at the *same*
//! PDPTs the kernel uses, so a kernel frame has one mapping no matter which process is running
//! and no code ever walks two tables for one address. Only the user half -- PML4 entries
//! `0..256` -- is private to the process, and that is the half
//! [`AddressSpace::create_in_place`] leaves empty.
//!
//! # The fault path
//!
//! `docs/architecture/03-memory-manager.md` names four causes and one handler. All four are
//! implemented here, and the boot self test drives them by faulting on purpose:
//!
//! | Cause | What [`resolve`] does |
//! |---|---|
//! | first touch of a committed private page | allocates a zeroed frame, maps it, counts a soft fault |
//! | write to a shared read-only page | allocates a frame, copies 4 KiB, remaps both, drops the shared reference |
//! | access inside a `mapped-file` VAD | refused: file-backed pages need the Cache Manager, which is M7 |
//! | stack growth | extends a grow-down VAD by one page, and refuses below the space's stack floor |
//!
//! Everything else -- an address in no VAD, a write to a read-only region, a fault in the
//! kernel half of the address space -- is refused. The caller turns that refusal into a bug
//! check today, and into `SIGSEGV` with `si_addr` once there is a process to signal (M6).
//!
//! # Borrowing discipline
//!
//! The processor's current address space is reachable from two directions: the code that called
//! [`AddressSpace::activate`], and the fault handler, which can run at an arbitrary instruction
//! while user memory is being touched. The kernel is single-processor, so there is no data race
//! -- but there *is* aliasing, and it is the caller's job not to hold a reference to the current
//! address space across an access to a user address. The self test follows that rule literally:
//! it borrows a space to set a mapping up, drops the borrow, faults, and borrows again.

pub mod selftest;

use nolx_mm_core::buddy::Frame;
use nolx_mm_core::commit::{CommitCharge, CommitError, PoolKind};
use nolx_mm_core::paging::{
    pml4_index, EntryFlags, MapError, Mapping, PageTables, Protection, TableStore as _,
    KERNEL_BASE, PAGE_4K,
};
use nolx_mm_core::vad::{
    Vad, VadError, VadFlags, VadKind, VadRequest, VadTree, USER_SPACE_HIGH, USER_SPACE_LOW,
};

use crate::mm::tables::{self, KernelTableStore};
use crate::mm::{self, phys, PAGE_SIZE};
use crate::sync::SyncCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

/// Virtual address descriptors one address space can hold.
///
/// The arena is inline in the tree, and the tree is inline in the address space, because the
/// structure that describes a process's address space must not need that address space (or any
/// other) to be allocated. A `fork` copies the descriptors; a program that maps and unmaps
/// hundreds of thousands of times still needs only as many nodes as it has live regions.
pub const SPACE_NODES: usize = 256;

/// Bytes one address space may commit before the limit becomes a quota object.
///
/// Boot-time policy, as `mm::pool`'s limits are: large enough for the self test and for a
/// realistic process, small enough that exhausting it is a bug rather than a surprise. M5
/// replaces it with the per-process quota the object manager charges.
pub const COMMIT_LIMIT: u64 = 256 * 1024 * 1024;

/// How far below a grow-down VAD a single fault may extend it.
///
/// A fault a megabyte below the stack is not a stack; it is a wild pointer, and honouring it
/// would grow the stack by 256 committed pages on the strength of one bad access.
pub const STACK_GROW_LIMIT: u64 = 1024 * 1024;

/// Round an address down to its page.
const fn page_down(address: u64) -> u64 {
    address & !(PAGE_SIZE as u64 - 1)
}

/// The address space type the kernel runs a process in.
pub type Space = AddressSpace<SPACE_NODES>;

/// A process address space.
pub struct AddressSpace<const NODES: usize> {
    /// The PML4 frame: what CR3 is loaded with.
    root: Frame,
    /// Page tables allocated for this space and not yet released. The PML4 is not counted here:
    /// it is taken by [`AddressSpace::create_in_place`] before there is a store to record it in.
    tables: KernelTableStore,
    /// What the address space means.
    vads: VadTree<NODES>,
    /// What it costs.
    commit: CommitCharge,
    /// Pages demand-paged in since the space was created.
    mapped: u64,
    /// Faults resolved since the space was created.
    faults: u64,
    /// Copy-on-write pages duplicated since the space was created.
    copies: u64,
    /// The address below which a grow-down VAD is not allowed to extend.
    stack_floor: u64,
}

impl<const NODES: usize> AddressSpace<NODES> {
    /// Create an address space in `place`: an empty user half, and the kernel's own half
    /// shared.
    ///
    /// The space is built *in place*, one field at a time, rather than returned by value, and
    /// that is a requirement rather than a style choice: a value of this type is most of the
    /// kernel stack -- the tree alone is [`SPACE_NODES`] descriptors, tens of kilobytes -- so
    /// a kernel that moves one through a caller's temporaries runs off the end of its stack
    /// before it has started a single process. The boot self test found exactly that: the
    /// space and the `Some(space)` that wrapped it put fifty kilobytes of uninitialised
    /// storage in one frame, and the teardown put a hundred. Nothing in this module may
    /// materialise an `AddressSpace` by value.
    ///
    /// Returns `None` when the frame allocator or the system PTE pool cannot supply the PML4,
    /// which at boot means the machine cannot run another process at all.
    pub fn create_in_place(place: &mut MaybeUninit<Self>) -> Option<&mut Self> {
        let root_frame = mm::allocate_frame_zeroed()?;
        mm::mark_page_table(root_frame);
        if mm::pool::charge(PoolKind::SystemPte, PAGE_SIZE as u64).is_err() {
            let _ = mm::free_frame(root_frame);
            return None;
        }
        let root = root_frame as Frame;

        // The kernel half: from the half's first PML4 slot to the last. `KERNEL_BASE` is slot
        // 256, so the loop covers the whole second half of the table -- the direct map, the
        // image window, the APIC and HPET windows the boot layer installed, and the recursive
        // slot. They are shared by reference (the entries point at the kernel's own PDPTs),
        // not copied, so a kernel mapping costs one table no matter how many address spaces
        // see it.
        //
        // The source is the *kernel's* root, not whatever root is currently loaded: a spawn
        // runs under the parent's user CR3, and the parent's PML4 is a copy that could in
        // principle be stale.
        let mut store = KernelTableStore::new();
        let kernel_root_frame = kernel_root();
        for slot in pml4_index(KERNEL_BASE)..512 {
            let entry = store.entry(kernel_root_frame, slot);
            if entry != 0 {
                // SAFETY: the entry names a PDPT the kernel owns and is already walking, and
                // the new PML4 is not the processor's root yet, so nothing else observes this
                // write.
                store.set_entry(root, slot, entry);
            }
        }

        // SAFETY: `place` is uninitialised storage for exactly this type. Each field is
        // written exactly once below, through a raw pointer to the field -- never through a
        // reference to the whole, which would be a reference to uninitialised memory -- and no
        // write reads the storage it is about to overwrite.
        unsafe {
            let space = place.as_mut_ptr();
            addr_of_mut!((*space).root).write(root);
            addr_of_mut!((*space).tables).write(KernelTableStore::new());
            addr_of_mut!((*space).vads).write(VadTree::new(USER_SPACE_LOW, USER_SPACE_HIGH));
            addr_of_mut!((*space).commit).write(CommitCharge::new("address space", COMMIT_LIMIT));
            addr_of_mut!((*space).mapped).write(0);
            addr_of_mut!((*space).faults).write(0);
            addr_of_mut!((*space).copies).write(0);
            addr_of_mut!((*space).stack_floor).write(USER_SPACE_LOW);
        }
        // SAFETY: every field was written just above, so the storage is initialised.
        Some(unsafe { place.assume_init_mut() })
    }

    /// The PML4 frame.
    #[must_use]
    pub const fn root(&self) -> Frame {
        self.root
    }

    /// Page tables this space currently holds, beyond its PML4.
    #[must_use]
    pub const fn live_tables(&self) -> u64 {
        self.tables.live_tables()
    }

    /// Bytes committed.
    #[must_use]
    pub const fn commit_used(&self) -> u64 {
        self.commit.current()
    }

    /// Pages demand-paged in.
    #[must_use]
    pub const fn pages_mapped(&self) -> u64 {
        self.mapped
    }

    /// Faults resolved against this space.
    #[must_use]
    pub const fn faults_resolved(&self) -> u64 {
        self.faults
    }

    /// Copy-on-write pages duplicated.
    #[must_use]
    pub const fn copies(&self) -> u64 {
        self.copies
    }

    /// The lowest address a grow-down VAD may extend to.
    #[must_use]
    pub const fn stack_floor(&self) -> u64 {
        self.stack_floor
    }

    /// Lower the floor a grow-down VAD may extend to.
    pub fn set_stack_floor(&mut self, floor: u64) {
        self.stack_floor = floor;
    }

    /// The VAD tree, for inspection and for the POSIX subsystem's range queries.
    #[must_use]
    pub const fn vads(&self) -> &VadTree<NODES> {
        &self.vads
    }

    /// What maps `address` in this address space, if anything does.
    ///
    /// The page tables are the truth about what is *mapped*: a range the tree describes but
    /// that has never been touched answers `None` here, which is exactly the difference
    /// between a reservation and a resident page.
    #[must_use]
    pub fn translate(&self, address: u64) -> Option<Mapping> {
        PageTables::new(self.root).translate(&self.tables, address)
    }

    /// Describe a range in the address space and charge it to the commit quota.
    ///
    /// No page is mapped and no frame is taken: that is the point of a VAD tree. The first
    /// access to the range is what allocates, and [`resolve`] is what notices.
    pub fn reserve(&mut self, request: VadRequest) -> Result<(), SpaceError> {
        if request.start >= request.end
            || request.start < USER_SPACE_LOW
            || request.end > USER_SPACE_HIGH
        {
            return Err(SpaceError::InvalidRange);
        }
        if request.flags.bits() & VadFlags::HUGEPAGE != 0 {
            // A 2 MiB user mapping is a page-table decision this module has not made yet, and
            // silently mapping 4 KiB pages for a request that asked for a huge one would be a
            // lie about the TLB footprint.
            return Err(SpaceError::Unsupported);
        }
        // A file-backed descriptor is allowed to exist -- `mmap` has to be able to describe a
        // range before anything touches it -- but the fault path refuses it, because the pages
        // behind it come from the Cache Manager, which is M7. Describing is cheap and
        // reversible; backing it is not.
        let charge = charge_for(&request);
        if charge > 0 {
            self.commit.charge(charge).map_err(SpaceError::Commit)?;
        }
        if let Err(error) = self.vads.insert(request) {
            if charge > 0 {
                let _ = self.commit.release(charge);
            }
            return Err(SpaceError::Vads(error));
        }
        Ok(())
    }

    /// Undescribe a range, unmapping everything it covered.
    ///
    /// Frames go back before the descriptors do: if unmapping fails part-way the tree still
    /// describes what is actually mapped, which is the recoverable way round.
    pub fn release(&mut self, start: u64, end: u64) -> Result<u64, SpaceError> {
        let unmapped = self.unmap_pages(start, end)?;

        let mut charge = 0;
        self.vads
            .visit_range(start, end, |vad| charge += vad.commit_charge());
        self.vads.remove(start, end).map_err(SpaceError::Vads)?;
        if charge > 0 {
            self.commit.release(charge).map_err(SpaceError::Commit)?;
        }
        Ok(unmapped)
    }

    /// Change the protection of a mapped range, in the tree and in the page tables.
    ///
    /// The tree decides first: `mprotect` fails as a whole when any covered descriptor's
    /// maximum protection forbids the request, and only then are the entries rewritten. A page
    /// that is not present has nothing to rewrite, which is correct -- it will be created with
    /// the new protection when it is first touched.
    pub fn protect(
        &mut self,
        start: u64,
        end: u64,
        protection: Protection,
    ) -> Result<(), SpaceError> {
        self.vads
            .protect(start, end, protection)
            .map_err(SpaceError::Vads)?;

        let mut page = start;
        while page < end {
            if let Some(mapping) = PageTables::new(self.root).translate(&self.tables, page) {
                if mapping.size == PAGE_4K {
                    let flags = EntryFlags::for_user(protection);
                    PageTables::new(self.root)
                        .protect(&mut self.tables, page, flags)
                        .map_err(SpaceError::Table)?;
                    self.shoot_down(page);
                }
            }
            page += PAGE_SIZE as u64;
        }
        Ok(())
    }

    /// Share `[start, end)` with `target` as copy-on-write, the way `fork` needs.
    ///
    /// Two things happen, and both are necessary. The descriptors are described again in the
    /// target -- so an untouched committed page in the parent becomes an untouched committed
    /// page in the child, which will be filled with zeros on first touch, exactly as the
    /// parent's would have been. Then every page the parent has already faulted in is mapped
    /// into the target *read-only*, and the parent's own mapping is made read-only too: from
    /// here a write in either address space takes the fault path, copies the page, and leaves
    /// the other one alone.
    ///
    /// `self` must be the active address space: the parent's entries change under a processor
    /// that may have them cached, and only the active space can have them shot down.
    ///
    /// Returns the number of pages shared.
    pub fn share_into(
        &mut self,
        target: &mut Self,
        start: u64,
        end: u64,
    ) -> Result<u64, SpaceError> {
        debug_assert!(
            current_pointer() == self as *const Self as *mut (),
            "shared an address space that the processor is not running in"
        );

        let mut failure: Option<SpaceError> = None;
        self.vads.visit_range(start, end, |vad| {
            if failure.is_some() || vad.is_free() {
                return;
            }
            let request = VadRequest {
                start: vad.start.max(start),
                end: vad.end.min(end),
                kind: vad.kind,
                protection: vad.protection,
                maximum: vad.maximum,
                flags: vad.flags,
                backing: vad.backing,
            };
            if request.start < request.end {
                if let Err(error) = target.reserve(request) {
                    failure = Some(error);
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }

        let read_only = |protection: Protection| {
            EntryFlags::for_user(Protection::from_bits(
                protection.bits() & !Protection::WRITE,
            ))
        };

        let mut shared = 0;
        let mut page = start;
        while page < end {
            if let Some(mapping) = PageTables::new(self.root).translate(&self.tables, page) {
                if mapping.size != PAGE_4K {
                    return Err(SpaceError::Unsupported);
                }
                let Some(vad) = self.vads.find(page) else {
                    page += PAGE_SIZE as u64;
                    continue;
                };
                let Ok(frame) = usize::try_from(mapping.entry.frame()) else {
                    return Err(SpaceError::InvalidRange);
                };
                let flags = read_only(vad.protection);

                // The parent loses its writable mapping first, so a write in the parent
                // between these two steps cannot race past the copy-on-write decision.
                PageTables::new(self.root)
                    .protect(&mut self.tables, page, flags)
                    .map_err(SpaceError::Table)?;
                self.shoot_down(page);

                mm::retain_frame(frame);
                if let Err(error) = PageTables::new(target.root).map_page(
                    &mut target.tables,
                    page,
                    mm::address_of(frame),
                    flags,
                ) {
                    mm::release_frame(frame);
                    return Err(SpaceError::Table(error));
                }
                mm::mark_copy_on_write(frame);
                target.mapped += 1;
                shared += 1;
            }
            page += PAGE_SIZE as u64;
        }
        Ok(shared)
    }

    /// Give up the address space: unmap everything, free every table, free the PML4.
    ///
    /// The space must not be the active one. Returns what it gave back, which is what the boot
    /// self test checks against the frame allocator's own counters.
    /// Takes `&mut self` rather than `self` for the reason [`AddressSpace::create_in_place`]
    /// documents: the value is far too large to travel through a function boundary, and the
    /// space this gives up lives in a caller's own storage -- a process object, or the boot
    /// self test's static.
    ///
    /// # Panics
    ///
    /// Panics if the space has already been torn down. The PML4 is freed exactly once, and a
    /// second teardown would free a frame the allocator has already handed to someone else.
    pub fn teardown(&mut self) -> Result<Teardown, SpaceError> {
        assert!(self.root != 0, "an address space was torn down twice");
        assert!(
            current_pointer().is_null(),
            "tore down an address space while it was active"
        );
        let mut vads_removed = 0;
        let mut pages_unmapped = 0;
        let mut commit_released = 0;
        // Child tables this space holds right now: the pages below free them as they empty, so
        // this is what the teardown is about to give back.
        let tables_freed = self.tables.live_tables();

        // Release the lowest live descriptor until none is left. The free holes are the tree's
        // own bookkeeping and disappear with it.
        loop {
            let mut lowest: Option<(u64, u64)> = None;
            self.vads.visit(|vad| {
                if vad.is_free() {
                    return;
                }
                match lowest {
                    Some((start, _)) if start <= vad.start => {}
                    _ => lowest = Some((vad.start, vad.end)),
                }
            });
            let Some((start, end)) = lowest else {
                break;
            };
            self.vads.visit_range(start, end, |vad| {
                commit_released += vad.commit_charge();
            });
            pages_unmapped += self.release(start, end)?;
            vads_removed += 1;
        }

        if mm::pool::release(PoolKind::SystemPte, PAGE_SIZE as u64).is_err() {
            return Err(SpaceError::Commit(CommitError::Accounting(
                "the PML4 charge was not in the system PTE pool",
            )));
        }
        // Every child table was reclaimed by `unmap` as its last entry went, so the store must
        // agree: what is left to free is the PML4 itself.
        if self.tables.live_tables() != 0 {
            return Err(SpaceError::Commit(CommitError::Accounting(
                "page tables outlived the mappings they belonged to",
            )));
        }
        mm::free_frame(self.root as usize).map_err(|_| SpaceError::NotMapped)?;
        // A torn-down space has no root. Leaving the frame number behind would make a second
        // teardown look legitimate until it freed a frame that now belongs to someone else.
        self.root = 0;

        Ok(Teardown {
            vads_removed,
            pages_unmapped,
            commit_released,
            tables_freed,
        })
    }

    /// Unmap every present page in `[start, end)`, releasing its frame.
    fn unmap_pages(&mut self, start: u64, end: u64) -> Result<u64, SpaceError> {
        let mut unmapped = 0;
        let mut page = start;
        while page < end {
            if let Some(mapping) = PageTables::new(self.root).translate(&self.tables, page) {
                if mapping.size != PAGE_4K {
                    return Err(SpaceError::Unsupported);
                }
                PageTables::new(self.root)
                    .unmap(&mut self.tables, page)
                    .map_err(SpaceError::Table)?;
                let frame = mapping.entry.frame() as usize;
                unmapped += 1;
                if mm::release_frame(frame) == 0 {
                    mm::clear_copy_on_write(frame);
                    let _ = mm::free_frame(frame);
                }
                self.shoot_down(page);
            }
            page += PAGE_SIZE as u64;
        }
        Ok(unmapped)
    }

    /// Flush one address from the TLB if the page tables changed under the running processor.
    fn shoot_down(&self, address: u64) {
        if current_pointer() == self as *const Self as *mut () {
            // SAFETY: the address belongs to the address space the processor is running in,
            // and its entry was just changed; `invlpg` is the flush for one page.
            unsafe { tables::invalidate_page(address) };
        }
    }

    /// Check the tree's invariants and the space's own accounting.
    pub fn check(&self) -> Result<(), &'static str> {
        self.vads.check()?;
        if self.vads.commit_total() != self.commit.current() {
            return Err("the VAD tree and the commit charge disagree");
        }
        Ok(())
    }

    /// Resolve a fault at `address` in *this* address space.
    ///
    /// The entry point [`resolve`] uses on the active space; the self test calls it directly so
    /// that a refusal can be asserted without stopping the machine.
    pub fn resolve_here(&mut self, address: u64, error_code: u64) -> Result<Fault, FaultError> {
        // The user half, and only the user half: everything outside it is either the kernel's
        // own address space or not a canonical address at all, and neither is something a
        // fault in a process can be about.
        if !(USER_SPACE_LOW..USER_SPACE_HIGH).contains(&address) {
            return Err(FaultError::KernelAddress);
        }
        let write = error_code & ERROR_WRITE != 0;
        // A hole is where a grow-down region gets its chance: the fault is below the stack,
        // not inside anything, which is what stack growth looks like from the hardware's point
        // of view.
        let Some(vad) = self
            .vads
            .find(address)
            .copied()
            .filter(|vad| !vad.is_free())
        else {
            return self.grow_stack(address);
        };
        match vad.kind {
            VadKind::Guard => Err(FaultError::GuardPage),
            VadKind::MappedFile | VadKind::Image => Err(FaultError::Unsupported),
            _ => {
                if let Some(mapping) = PageTables::new(self.root).translate(&self.tables, address) {
                    if mapping.size != PAGE_4K {
                        return Err(FaultError::Unsupported);
                    }
                    let frame = mapping.entry.frame() as usize;
                    if write && !mapping.entry.is_writable() {
                        // Two different things look identical from here: a page that is
                        // read-only because it is shared (copy on write), and a page that is
                        // read-only because the program asked for it. The VAD knows the
                        // second, the frame allocator knows the first.
                        if vad.protection.can_write() && mm::is_copy_on_write(frame) {
                            return self.copy_on_write(address, frame, vad.protection);
                        }
                        return Err(FaultError::Protection);
                    }
                    // Present, but this access is not one the mapping allows: an instruction
                    // fetch from a page marked no-execute, or a read from a mapping whose
                    // protection bits forbid it.
                    return Err(FaultError::Protection);
                }
                self.demand_zero(address, vad.protection)
            }
        }
    }

    /// The first cause: a committed page that has never been touched.
    fn demand_zero(&mut self, address: u64, protection: Protection) -> Result<Fault, FaultError> {
        let frame = mm::allocate_frame_zeroed().ok_or(FaultError::OutOfFrames)?;
        mm::retain_frame(frame);
        let flags = EntryFlags::for_user(protection);
        if let Err(error) = PageTables::new(self.root).map_page(
            &mut self.tables,
            address,
            mm::address_of(frame),
            flags,
        ) {
            mm::release_frame(frame);
            if mm::frame_refcount(frame) == 0 {
                let _ = mm::free_frame(frame);
            }
            return Err(FaultError::Table(error));
        }
        self.shoot_down(address);
        self.mapped += 1;
        self.faults += 1;
        Ok(Fault::DemandZero { frame })
    }

    /// The second cause: a write to a page that is mapped read-only in more than one place.
    ///
    /// The copy-on-write marker stays set until the frame's last mapping is gone. Clearing it
    /// as soon as one owner is left would make the *next* write to that frame's still
    /// read-only entry look like a `PROT_READ` violation instead of the artefact it is.
    fn copy_on_write(
        &mut self,
        address: u64,
        frame: usize,
        protection: Protection,
    ) -> Result<Fault, FaultError> {
        let writable = EntryFlags::for_user(protection);

        // The single-owner shortcut: the frame is mapped here and nowhere else, so the
        // read-only entry is an artefact of an earlier share rather than a reason to copy.
        if mm::frame_refcount(frame) <= 1 {
            PageTables::new(self.root)
                .protect(&mut self.tables, address, writable)
                .map_err(FaultError::Table)?;
            self.shoot_down(address);
            self.faults += 1;
            return Ok(Fault::CopyOnWrite {
                source: frame,
                destination: frame,
            });
        }

        let fresh = mm::allocate_frame_zeroed().ok_or(FaultError::OutOfFrames)?;
        // SAFETY: both frames are inside the direct map -- one owns a user page in this address
        // space, the other was just allocated -- and the copy is a 4 KiB `memcpy` between two
        // page-aligned addresses that cannot overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys::direct(mm::address_of(frame)) as *const u8,
                phys::direct(mm::address_of(fresh)) as *mut u8,
                PAGE_SIZE,
            );
        }
        mm::retain_frame(fresh);

        if let Err(error) = PageTables::new(self.root).unmap(&mut self.tables, address) {
            mm::release_frame(fresh);
            let _ = mm::free_frame(fresh);
            return Err(FaultError::Table(error));
        }
        if let Err(error) = PageTables::new(self.root).map_page(
            &mut self.tables,
            address,
            mm::address_of(fresh),
            writable,
        ) {
            // Put the original mapping back read-only: losing the page would be worse than
            // failing the write that caused the fault.
            let _ = PageTables::new(self.root).map_page(
                &mut self.tables,
                address,
                mm::address_of(frame),
                EntryFlags::for_user(Protection::from_bits(
                    protection.bits() & !Protection::WRITE,
                )),
            );
            mm::release_frame(fresh);
            let _ = mm::free_frame(fresh);
            return Err(FaultError::Table(error));
        }
        self.shoot_down(address);

        // This space no longer references the old frame. The marker stays set while the frame
        // is still mapped read-only somewhere; the next writer takes this same path, and finds
        // the frame with one owner and uses the shortcut above.
        mm::release_frame(frame);
        self.copies += 1;
        self.faults += 1;
        Ok(Fault::CopyOnWrite {
            source: frame,
            destination: fresh,
        })
    }

    /// The fourth cause: a fault below a grow-down region.
    ///
    /// The lowest grow-down VAD above the address grows to cover the faulting page -- and,
    /// because the descriptor is inserted with the stack's own attributes, the two coalesce
    /// back into one region, which is what keeps a growing stack one VAD instead of one per
    /// page. Below the space's stack floor the fault is refused, and the caller reports it;
    /// that refusal is what `SIGSEGV` will be built on.
    fn grow_stack(&mut self, address: u64) -> Result<Fault, FaultError> {
        let page = page_down(address);
        let mut stack: Option<Vad> = None;
        self.vads.visit(|vad| {
            if vad.flags.bits() & VadFlags::GROWSDOWN == 0 || vad.start <= address {
                return;
            }
            if vad.start - address > STACK_GROW_LIMIT {
                return;
            }
            match stack {
                Some(current) if current.start <= vad.start => {}
                _ => stack = Some(*vad),
            }
        });
        let Some(stack) = stack else {
            return Err(FaultError::NotMapped);
        };
        if page < self.stack_floor || page < USER_SPACE_LOW {
            return Err(FaultError::StackLimit);
        }

        self.reserve(VadRequest {
            start: page,
            end: stack.start,
            kind: stack.kind,
            protection: stack.protection,
            maximum: stack.maximum,
            flags: stack.flags,
            backing: None,
        })
        .map_err(|_| FaultError::NotMapped)?;
        let _ = self.demand_zero(page, stack.protection)?;
        Ok(Fault::StackGrowth {
            from: stack.start,
            to: page,
        })
    }
}

impl AddressSpace<SPACE_NODES> {
    /// Make this the processor's address space, and the one the fault handler resolves against.
    ///
    /// Only the kernel's own space type can be activated: the fault handler reaches the active
    /// space through a type-erased pointer, and a space whose descriptor arena had a different
    /// size would be read back with the wrong layout.
    ///
    /// # Panics
    ///
    /// Panics if a different address space is already active: nesting address spaces is not
    /// something any caller should do, and continuing after one would mean the fault handler
    /// resolving a fault in the wrong address space.
    pub fn activate(&self) {
        assert!(
            current_pointer().is_null(),
            "an address space was activated while another was active"
        );
        // Capture the kernel's root *before* CR3 changes: afterwards, the running root is this
        // address space's PML4, and `current_root` would read that one instead.
        let _ = kernel_root();
        // SAFETY: single boot processor. The root maps this code, this stack, and the direct
        // map -- the kernel half is shared with the address space the kernel is running in.
        unsafe { tables::activate(self.root) };
        set_current(self as *const Self as *mut ());
    }

    /// Return to the kernel's own address space.
    ///
    /// # Panics
    ///
    /// Panics if this address space is not the active one, which means a caller is tearing
    /// down a space that is not running -- or that the bookkeeping is already wrong.
    pub fn deactivate(&self) {
        assert_eq!(
            current_pointer(),
            self as *const Self as *mut (),
            "deactivated an address space that was not active"
        );
        set_current(core::ptr::null_mut());
        if self.root != kernel_root() {
            // SAFETY: the kernel's own root was captured while the kernel was running on it,
            // so it maps everything this code needs.
            unsafe { tables::activate(kernel_root()) };
        }
    }
}

/// What a resolved fault did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A committed page was allocated, zeroed, and mapped.
    DemandZero {
        /// The frame now mapped.
        frame: usize,
    },
    /// A shared read-only page was copied, or -- when `source == destination` -- made writable
    /// because it had exactly one owner left.
    CopyOnWrite {
        /// The frame the mapping used to point at.
        source: usize,
        /// The frame it points at now.
        destination: usize,
    },
    /// A grow-down region grew to cover the faulting page.
    StackGrowth {
        /// Where the region started before the fault.
        from: u64,
        /// Where it starts now.
        to: u64,
    },
}

/// Why a fault could not be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultError {
    /// The address is in the kernel half of the address space, or below the user minimum.
    KernelAddress,
    /// The address is in no VAD, and no grow-down region can reach it.
    NotMapped,
    /// The page is mapped, but not in a way that permits this access.
    Protection,
    /// The address is inside an explicit guard region.
    GuardPage,
    /// A grow-down region could not reach the address without crossing its floor.
    StackLimit,
    /// The page belongs to something this milestone does not back yet.
    Unsupported,
    /// The frame allocator is out of frames.
    OutOfFrames,
    /// A page table could not be allocated or edited.
    Table(MapError),
}

/// Why a range operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceError {
    /// The range was empty, unaligned, or outside the user address space.
    InvalidRange,
    /// The VAD tree rejected the operation.
    Vads(VadError),
    /// The commit quota refused the charge.
    Commit(CommitError),
    /// A page table could not be allocated or edited.
    Table(MapError),
    /// A 2 MiB mapping or a file-backed range, neither of which this milestone implements.
    Unsupported,
    /// A frame could not be returned to the allocator.
    NotMapped,
}

/// What tearing an address space down gave back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Teardown {
    /// Live descriptors removed.
    pub vads_removed: u64,
    /// Pages unmapped, and whose frames were released.
    pub pages_unmapped: u64,
    /// Bytes of commit charge released.
    pub commit_released: u64,
    /// Page tables the space still held when its last range was released.
    pub tables_freed: u64,
}

/// Bit 1 of a page-fault error code: the access was a write.
pub const ERROR_WRITE: u64 = 1 << 1;
/// Bit 2 of a page-fault error code: the fault came from ring 3.
pub const ERROR_USER: u64 = 1 << 2;
/// Bit 4 of a page-fault error code: the access was an instruction fetch.
pub const ERROR_INSTRUCTION: u64 = 1 << 4;

/// The commit charge a request will add to its address space.
fn charge_for(request: &VadRequest) -> u64 {
    if request.kind.is_committed() {
        request.end - request.start
    } else {
        0
    }
}

/// The address space the processor is running in.
///
/// A null pointer means the kernel's own address space: there is no VAD tree to consult, and
/// every fault is a kernel bug. That is the state the machine boots in, and the state it
/// returns to when a process's address space is torn down.
static CURRENT: SyncCell<*mut ()> = SyncCell::new(core::ptr::null_mut());

/// The kernel's own page-table root, captured the first time an address space is activated.
static KERNEL_ROOT: SyncCell<Frame> = SyncCell::new(0);

fn current_pointer() -> *mut () {
    // SAFETY: written only by `activate`/`deactivate`, which run on the boot processor.
    unsafe { *CURRENT.get() }
}

fn set_current(space: *mut ()) {
    // SAFETY: as above. The pointer is to a `Space` that outlives every use of it: the caller
    // either owns it in a static or holds it in a process object for the process's lifetime.
    unsafe {
        *CURRENT.get() = space;
    }
}

fn kernel_root() -> Frame {
    // SAFETY: single boot processor; the root is captured once, while the kernel is running on
    // it, and never changes.
    unsafe {
        if *KERNEL_ROOT.get() == 0 {
            *KERNEL_ROOT.get() = tables::current_root();
        }
        *KERNEL_ROOT.get()
    }
}

/// Whether a process address space is active.
#[must_use]
pub fn is_active() -> bool {
    !current_pointer().is_null()
}

/// The active address space, for code that knows no user access can happen while it holds the
/// reference.
///
/// # Safety
///
/// The caller must not hold the returned reference across an access to a user address. The
/// fault handler resolves faults by taking the same reference, so a caller that does is
/// aliasing the address space against itself.
#[must_use]
pub unsafe fn active() -> Option<&'static mut Space> {
    if current_pointer().is_null() {
        return None;
    }
    // SAFETY: the caller upholds the no-aliasing rule above; `activate` only ever stores a
    // pointer to an `AddressSpace<SPACE_NODES>`, so the erased pointer is a `*mut Space`.
    Some(unsafe { &mut *(current_pointer() as *mut Space) })
}

/// Resolve a page fault in the active address space.
///
/// This is the function the interrupt dispatcher calls for vector 14. `true` means the fault
/// was a demand for memory the address space had committed: the page is now mapped and the
/// faulting instruction can be retried. `false` means the dispatcher should treat the fault as
/// fatal -- a kernel bug, or (once there is a process to signal) a `SIGSEGV`.
pub fn handle(address: u64, error_code: u64) -> bool {
    // SAFETY: the reference does not outlive this call, and this is the fault path itself --
    // nothing else can be holding one.
    let Some(space) = (unsafe { active() }) else {
        return false;
    };
    space.resolve_here(address, error_code).is_ok()
}

/// Resolve a fault at `address` against the active address space, without faulting.
///
/// The self test's probe, and the entry point a future `mincore`/`madvise` can use to ask the
/// same question the hardware asked.
pub fn resolve(address: u64, error_code: u64) -> Result<Fault, FaultError> {
    // SAFETY: the reference does not outlive this call; the caller must not hold one either.
    let Some(space) = (unsafe { active() }) else {
        return Err(FaultError::NotMapped);
    };
    space.resolve_here(address, error_code)
}

/// The active address space's page-mapping and fault statistics, for reporting.
#[must_use]
pub fn active_stats() -> Option<(u64, u64, u64)> {
    // SAFETY: as `resolve`: a read under the same rules.
    unsafe { active() }.map(|space| {
        (
            space.pages_mapped(),
            space.faults_resolved(),
            space.copies(),
        )
    })
}
