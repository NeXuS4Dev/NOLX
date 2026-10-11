//! Four-level paging: index arithmetic, entry encoding, and a page-table walker.
//!
//! The layout is the x86-64 one: a PML4 holds 512 PML4 entries, each pointing at a page
//! directory pointer table, each of those at a page directory, each of those at a page
//! table of 512 page-table entries. A 4 KiB mapping consumes all four levels; a 2 MiB
//! mapping stops at the page directory, and a 1 GiB mapping stops at the directory pointer
//! table.
//!
//! NOLX uses 4 KiB and 2 MiB mappings, and the walker here creates, splits, and reclaims
//! the tables needed for them. A 1 GiB page is recognised -- the boot layer builds several,
//! and the walker must never silently overwrite one -- but the walker does not create them.
//!
//! Reaching the tables is the one thing this crate cannot do, so it goes through
//! [`TableStore`]: the kernel implements it over the direct map, the tests implement it
//! over a map. What is tested here -- which table index an address belongs to, how a 2 MiB
//! page is split, when an empty table may be reclaimed -- is the same code in both.
//!
//! See `docs/architecture/03-memory-manager.md` for the address-space layout this module
//! assumes.

use crate::buddy::Frame;
use crate::PAGE_MASK;

/// Highest address in the user half of the address space (48-bit canonical).
pub const USER_TOP: u64 = 0x0000_7fff_ffff_ffff;

/// Base of the kernel half: the direct map, the pools, and the kernel image.
pub const KERNEL_BASE: u64 = 0xffff_8000_0000_0000;

/// Base of the direct map of physical memory.
///
/// The boot layer maps the first 2 GiB of RAM here before it enters long mode, and every
/// physical address the kernel computes is turned into a pointer by adding this constant
/// (`kernel/src/mm/phys.rs`). `DIRECT_MAP_BASE + p` is the same memory as `p` was under the
/// boot layer's identity map, which is the point: firmware structures keep working across
/// the switch, and nothing else does.
pub const DIRECT_MAP_BASE: u64 = KERNEL_BASE;

/// Base of the kernel's image window: the kernel's own frames at `KERNEL_IMAGE_BASE + p`.
///
/// The kernel is linked in the top 2 GiB of the address space, `0xffff_ffff_8000_0000` and
/// up, because that is the range x86-64 code reaches with 32-bit *sign-extended* absolute
/// addressing -- what the LLVM `kernel` code model assumes, and therefore what lets the
/// kernel address its own statics without loading an address first. The kernel is built with
/// that code model (`KERNEL_RUSTFLAGS` in the Makefile) and `kernel/linker.ld` links the image
/// window here; the two have to agree or every static access is a relink away from an
/// out-of-range relocation error.
///
/// The top 2 GiB is the last PML4 slot, so the image window owns slot 511 and the debugger's
/// recursive mapping moves down one slot (`RECURSIVE_SLOT`).
pub const KERNEL_IMAGE_BASE: u64 = 0xffff_ffff_8000_0000;

/// The PML4 slot reserved for the recursive mapping used by the debugger.
///
/// The slot above it holds the kernel image window, which cannot share: the top 2 GiB is
/// inside slot 511, and a recursive mapping needs a whole slot to point at the tables.
pub const RECURSIVE_SLOT: usize = 510;

/// The layout the boot trampoline and the linker script both assume, checked here rather
/// than in a test: a kernel linked outside the top 2 GiB does not merely run slowly, it does
/// not link at all under the `kernel` code model, and the failure would be an out-of-range
/// relocation rather than a sentence explaining the address-space invariant that broke.
const _: () = {
    assert!(
        KERNEL_IMAGE_BASE >= 0xffff_ffff_8000_0000,
        "the image window must be inside the top 2 GiB"
    );
    assert!(
        KERNEL_IMAGE_BASE >= KERNEL_BASE,
        "the image window must be in the kernel half"
    );
    assert!(
        pml4_index(KERNEL_IMAGE_BASE) == RECURSIVE_SLOT + 1,
        "the image window owns the slot above the debugger's recursive mapping"
    );
    assert!(
        pml4_index(u64::MAX) == RECURSIVE_SLOT + 1,
        "the whole top 2 GiB must be one PML4 slot"
    );
};

/// The number of entries in every table.
pub const ENTRIES: usize = 512;

/// A 4 KiB mapping.
pub const PAGE_4K: u64 = 1 << 12;

/// A 2 MiB mapping.
pub const PAGE_2M: u64 = 1 << 21;

/// A 1 GiB mapping.
pub const PAGE_1G: u64 = 1 << 30;

/// The largest physical address the four-level format can encode.
pub const MAX_PHYSICAL: u64 = 1 << 52;

/// Index into the PML4 for `virt`.
#[must_use]
pub const fn pml4_index(virt: u64) -> usize {
    ((virt >> 39) & 0x1ff) as usize
}

/// Index into a page directory pointer table for `virt`.
#[must_use]
pub const fn pdpt_index(virt: u64) -> usize {
    ((virt >> 30) & 0x1ff) as usize
}

/// Index into a page directory for `virt`.
#[must_use]
pub const fn pd_index(virt: u64) -> usize {
    ((virt >> 21) & 0x1ff) as usize
}

/// Index into a page table for `virt`.
#[must_use]
pub const fn pt_index(virt: u64) -> usize {
    ((virt >> 12) & 0x1ff) as usize
}

/// All four indices, outermost first: `[pml4, pdpt, pd, pt]`.
#[must_use]
pub const fn indices(virt: u64) -> [usize; 4] {
    [
        pml4_index(virt),
        pdpt_index(virt),
        pd_index(virt),
        pt_index(virt),
    ]
}

/// Whether `virt` is canonical for a 48-bit implementation: bits 63..48 must repeat bit 47.
#[must_use]
pub const fn is_canonical(virt: u64) -> bool {
    let sign = virt >> 47;
    sign == 0 || sign == 0x1_ffff
}

/// Sign-extend a 48-bit address into a canonical `u64`.
#[must_use]
pub const fn sign_extend(virt: u64) -> u64 {
    (((virt << 16) as i64) >> 16) as u64
}

/// Whether `virt` belongs to the user half of the address space.
#[must_use]
pub const fn is_user_address(virt: u64) -> bool {
    virt <= USER_TOP
}

/// Whether `virt` belongs to the kernel half of the address space.
#[must_use]
pub const fn is_kernel_address(virt: u64) -> bool {
    virt >= KERNEL_BASE
}

/// The protections a mapping may carry, as POSIX `mmap` states them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Protection(u8);

impl Protection {
    /// Readable.
    pub const READ: u8 = 1 << 0;
    /// Writable.
    pub const WRITE: u8 = 1 << 1;
    /// Executable.
    pub const EXECUTE: u8 = 1 << 2;
    /// No access at all: the guard page.
    pub const NONE: u8 = 0;

    /// From the three bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & (Self::READ | Self::WRITE | Self::EXECUTE))
    }

    /// The three bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every permission in `other` is also in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        other.0 & !self.0 == 0
    }

    /// Whether the readable bit is set.
    #[must_use]
    pub const fn can_read(self) -> bool {
        self.0 & Self::READ != 0
    }

    /// Whether the writable bit is set.
    #[must_use]
    pub const fn can_write(self) -> bool {
        self.0 & Self::WRITE != 0
    }

    /// Whether the executable bit is set.
    #[must_use]
    pub const fn can_execute(self) -> bool {
        self.0 & Self::EXECUTE != 0
    }

    /// The read-write mapping a `fork` or a `brk` needs.
    #[must_use]
    pub const fn read_write() -> Self {
        Self(Self::READ | Self::WRITE)
    }
}

/// Bits in a page-table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EntryFlags(u64);

impl EntryFlags {
    /// The entry is present.
    pub const PRESENT: u64 = 1 << 0;
    /// The mapping is writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// The mapping is reachable from ring 3.
    pub const USER: u64 = 1 << 2;
    /// Write-through caching.
    pub const WRITE_THROUGH: u64 = 1 << 3;
    /// Caching disabled: what MMIO needs.
    pub const CACHE_DISABLE: u64 = 1 << 4;
    /// The entry has been used.
    pub const ACCESSED: u64 = 1 << 5;
    /// The entry's page has been written.
    pub const DIRTY: u64 = 1 << 6;
    /// A page directory entry maps 2 MiB directly; a directory-pointer entry, 1 GiB.
    pub const HUGE: u64 = 1 << 7;
    /// A global translation: not flushed from the TLB on an address-space switch.
    pub const GLOBAL: u64 = 1 << 8;
    /// No execute: the only way to express "not executable" on x86-64.
    pub const NO_EXECUTE: u64 = 1 << 63;

    /// No flags.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// From raw bits.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The flags with `other` added.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The flags with `other` removed.
    #[must_use]
    pub const fn minus(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        other.0 & !self.0 == 0
    }

    /// The flags for a mapping with `protection` in ring 3.
    ///
    /// A read-only mapping is mapped without the write bit; the fault handler upgrades it
    /// when a write arrives. That is what makes copy-on-write a decision in the handler
    /// rather than a shadow page-table mechanism.
    #[must_use]
    pub const fn for_user(protection: Protection) -> Self {
        let mut bits = Self::PRESENT | Self::USER;
        if protection.can_write() {
            bits |= Self::WRITABLE;
        }
        if !protection.can_execute() {
            bits |= Self::NO_EXECUTE;
        }
        Self(bits)
    }

    /// The flags for a kernel mapping with `protection`.
    #[must_use]
    pub const fn for_kernel(protection: Protection) -> Self {
        let mut bits = Self::PRESENT | Self::GLOBAL;
        if protection.can_write() {
            bits |= Self::WRITABLE;
        }
        if !protection.can_execute() {
            bits |= Self::NO_EXECUTE;
        }
        Self(bits)
    }

    /// The flags an intermediate table entry needs for a leaf with `leaf` flags.
    ///
    /// x86-64 checks the whole path, not just the leaf: a page reachable from ring 3 needs
    /// the user bit at every level, and a writable leaf needs a writable path. Missing this
    /// is a page fault in ring 3 with no obvious cause, which is exactly the class of bug
    /// the host tests exist to catch.
    #[must_use]
    pub const fn intermediate(leaf: Self) -> Self {
        let mut bits = Self::PRESENT;
        if leaf.0 & Self::WRITABLE != 0 {
            bits |= Self::WRITABLE;
        }
        if leaf.0 & Self::USER != 0 {
            bits |= Self::USER;
        }
        Self(bits)
    }
}

/// A page-table entry: a physical address plus flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry(u64);

impl Entry {
    /// The bits of an entry that hold the physical address, for a 4 KiB page.
    pub const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

    /// An entry pointing at `address` with `flags`.
    ///
    /// Returns `None` when the address is not page-aligned or does not fit the format,
    /// which is the check that keeps a bad address out of the tables rather than in them.
    #[must_use]
    pub const fn new(address: u64, flags: EntryFlags) -> Option<Self> {
        if address & PAGE_MASK != 0 || address >= MAX_PHYSICAL {
            return None;
        }
        Some(Self(address | flags.bits()))
    }

    /// Wrap raw entry bits.
    #[must_use]
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }

    /// A clear entry.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The physical address in the entry.
    #[must_use]
    pub const fn address(self) -> u64 {
        self.0 & Self::ADDRESS_MASK
    }

    /// The frame the entry points at.
    #[must_use]
    pub const fn frame(self) -> Frame {
        self.address() / 4096
    }

    /// The flags in the entry.
    #[must_use]
    pub const fn flags(self) -> EntryFlags {
        EntryFlags(self.0 & !Self::ADDRESS_MASK)
    }

    /// Whether the entry is present.
    #[must_use]
    pub const fn is_present(self) -> bool {
        self.0 & EntryFlags::PRESENT != 0
    }

    /// Whether the entry is writable.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        self.0 & EntryFlags::WRITABLE != 0
    }

    /// Whether the entry is reachable from ring 3.
    #[must_use]
    pub const fn is_user(self) -> bool {
        self.0 & EntryFlags::USER != 0
    }

    /// Whether the entry maps a large page directly.
    #[must_use]
    pub const fn is_huge(self) -> bool {
        self.0 & EntryFlags::HUGE != 0
    }

    /// Whether the entry forbids execution.
    #[must_use]
    pub const fn is_no_execute(self) -> bool {
        self.0 & EntryFlags::NO_EXECUTE != 0
    }
}

/// A mapping found by the walker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// The physical address the virtual address translates to.
    pub physical: u64,
    /// The entry that does it, from the level that stopped the walk.
    pub entry: Entry,
    /// The size of the mapping in bytes: 4096, 2 MiB, or 1 GiB.
    pub size: u64,
}

/// Storage the walker needs, and the one part of this module the kernel supplies.
///
/// Every method names a *table* by its frame number. The kernel implements this over the
/// direct map; nothing here assumes the tables are contiguous with each other.
pub trait TableStore {
    /// Read entry `index` of `table`.
    #[must_use]
    fn entry(&self, table: Frame, index: usize) -> u64;

    /// Write entry `index` of `table`.
    fn set_entry(&mut self, table: Frame, index: usize, value: u64);

    /// Allocate a zeroed page to be used as a page table.
    fn allocate_table(&mut self) -> Option<Frame>;

    /// Return a page table that is no longer referenced.
    fn release_table(&mut self, table: Frame);

    /// Whether every entry of `table` is zero, so it can be reclaimed.
    #[must_use]
    fn is_empty_table(&self, table: Frame) -> bool;
}

/// What the walker has done, for the boot log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageTableStats {
    /// Page tables allocated since boot.
    pub tables_allocated: u64,
    /// Page tables reclaimed.
    pub tables_released: u64,
    /// 4 KiB mappings created.
    pub pages_mapped: u64,
    /// 2 MiB mappings created.
    pub huge_pages_mapped: u64,
    /// 2 MiB mappings taken apart because a 4 KiB mapping needed to go inside one.
    pub demotions: u64,
    /// Mappings removed.
    pub unmapped: u64,
    /// Operations that failed, for any reason.
    pub failures: u64,
}

/// Why a mapping operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Something is already mapped, either at this address or over it by a large page.
    AlreadyMapped,
    /// Nothing is mapped at this address.
    NotMapped,
    /// The virtual address is not canonical, or a physical address is not page-aligned.
    Misaligned,
    /// A page table was needed and could not be allocated.
    OutOfMemory,
    /// The mapping covering the address is not a 2 MiB page, so there is nothing to split.
    NotHuge,
}

/// A set of page tables rooted at one PML4.
///
/// The kernel owns one of these for its own address space and one per process; that second
/// use is what makes the walker a value rather than a set of global functions.
#[derive(Clone, Copy, Debug)]
pub struct PageTables {
    root: Frame,
    stats: PageTableStats,
}

impl PageTables {
    /// The tables rooted at `root`.
    #[must_use]
    pub const fn new(root: Frame) -> Self {
        Self {
            root,
            stats: PageTableStats {
                tables_allocated: 0,
                tables_released: 0,
                pages_mapped: 0,
                huge_pages_mapped: 0,
                demotions: 0,
                unmapped: 0,
                failures: 0,
            },
        }
    }

    /// The PML4 frame.
    #[must_use]
    pub const fn root(&self) -> Frame {
        self.root
    }

    /// What the walker has done.
    #[must_use]
    pub const fn stats(&self) -> PageTableStats {
        self.stats
    }

    /// Translate `virt`, returning what maps it and how large the mapping is.
    #[must_use]
    pub fn translate(&self, store: &impl TableStore, virt: u64) -> Option<Mapping> {
        if !is_canonical(virt) {
            return None;
        }
        let [i4, i3, i2, i1] = indices(virt);

        let pml4 = Entry::from_raw(store.entry(self.root, i4));
        if !pml4.is_present() {
            return None;
        }
        let pdpt = Entry::from_raw(store.entry(pml4.frame(), i3));
        if !pdpt.is_present() {
            return None;
        }
        if pdpt.is_huge() {
            return Some(huge_mapping(virt, pdpt, PAGE_1G));
        }
        let pd = Entry::from_raw(store.entry(pdpt.frame(), i2));
        if !pd.is_present() {
            return None;
        }
        if pd.is_huge() {
            return Some(huge_mapping(virt, pd, PAGE_2M));
        }
        let pt = Entry::from_raw(store.entry(pd.frame(), i1));
        if !pt.is_present() {
            return None;
        }
        Some(Mapping {
            physical: pt.address() + (virt & PAGE_MASK),
            entry: pt,
            size: PAGE_4K,
        })
    }

    /// Map one 4 KiB page.
    ///
    /// A 2 MiB mapping covering `virt` is split so that this page can be mapped inside it;
    /// that is page-table demotion, and it is invisible to the caller apart from the
    /// statistics.
    pub fn map_page(
        &mut self,
        store: &mut impl TableStore,
        virt: u64,
        physical: u64,
        flags: EntryFlags,
    ) -> Result<(), MapError> {
        if !is_canonical(virt) || virt & PAGE_MASK != 0 || physical & PAGE_MASK != 0 {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        }
        let Some(entry) = Entry::new(physical, flags) else {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        };
        let [i4, i3, i2, i1] = indices(virt);

        let pdpt = self.child(store, self.root, i4, flags)?;
        if Entry::from_raw(store.entry(pdpt, i3)).is_huge() {
            // A 1 GiB page covers this address. Splitting one would mean building a page
            // directory and up to 512 page tables; nothing in NOLX needs that, and silently
            // refusing is better than silently replacing a gigabyte of mappings.
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        let pd = self.child(store, pdpt, i3, flags)?;
        if Entry::from_raw(store.entry(pd, i2)).is_huge() {
            // A 2 MiB page covers this address. Callers that mean to replace it with 4 KiB
            // pages say so with `demote`, so that the decision -- and its cost -- is
            // visible at the call site instead of hidden in a mapping helper.
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        let pt = self.child(store, pd, i2, flags)?;

        if Entry::from_raw(store.entry(pt, i1)).is_present() {
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        store.set_entry(pt, i1, entry.bits());
        self.stats.pages_mapped += 1;
        Ok(())
    }

    /// Map a 2 MiB page.
    ///
    /// The virtual and physical addresses must both be 2 MiB aligned. A mapping inside the
    /// range is an error: NOLX never replaces a live mapping implicitly.
    pub fn map_huge(
        &mut self,
        store: &mut impl TableStore,
        virt: u64,
        physical: u64,
        flags: EntryFlags,
    ) -> Result<(), MapError> {
        const SIZE: u64 = 1 << 21;
        if !is_canonical(virt)
            || virt & (SIZE - 1) != 0
            || physical & (SIZE - 1) != 0
            || physical >= MAX_PHYSICAL
        {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        }
        let [i4, i3, i2, _] = indices(virt);
        let pdpt = self.child(store, self.root, i4, flags)?;

        let pdpt_entry = Entry::from_raw(store.entry(pdpt, i3));
        if pdpt_entry.is_huge() {
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        let pd = self.child(store, pdpt, i3, flags)?;

        if Entry::from_raw(store.entry(pd, i2)).is_present() {
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        let Some(entry) = Entry::new(
            physical,
            flags.union(EntryFlags::from_bits(EntryFlags::HUGE)),
        ) else {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        };
        store.set_entry(pd, i2, entry.bits());
        self.stats.huge_pages_mapped += 1;
        Ok(())
    }

    /// Map a contiguous range of physical memory, using 2 MiB pages where both addresses
    /// allow it and 4 KiB pages elsewhere.
    pub fn map_range(
        &mut self,
        store: &mut impl TableStore,
        virt: u64,
        physical: u64,
        length: u64,
        flags: EntryFlags,
    ) -> Result<(), MapError> {
        let mut done = 0;
        while done < length {
            let v = virt + done;
            let p = physical + done;
            if v & ((1 << 21) - 1) == 0 && p & ((1 << 21) - 1) == 0 && length - done >= (1 << 21) {
                self.map_huge(store, v, p, flags)?;
                done += 1 << 21;
            } else {
                self.map_page(store, v, p, flags)?;
                done += 4096;
            }
        }
        Ok(())
    }

    /// Remove the mapping at `virt`, reclaiming any table that becomes empty.
    ///
    /// A 2 MiB mapping is removed whole, and reported as such: a caller that wanted one
    /// 4 KiB page unmapped from inside a large page used [`PageTables::map_page`], which
    /// splits it, rather than this.
    pub fn unmap(&mut self, store: &mut impl TableStore, virt: u64) -> Result<Mapping, MapError> {
        if !is_canonical(virt) || virt & PAGE_MASK != 0 {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        }
        let [i4, i3, i2, i1] = indices(virt);

        let pml4 = Entry::from_raw(store.entry(self.root, i4));
        if !pml4.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        let pdpt_frame = pml4.frame();
        let pdpt_entry = Entry::from_raw(store.entry(pdpt_frame, i3));
        if !pdpt_entry.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        if pdpt_entry.is_huge() {
            store.set_entry(pdpt_frame, i3, 0);
            self.release_if_empty(store, self.root, i4, pdpt_frame);
            self.stats.unmapped += 1;
            return Ok(huge_mapping(virt, pdpt_entry, PAGE_1G));
        }

        let pd_frame = pdpt_entry.frame();
        let pd_entry = Entry::from_raw(store.entry(pd_frame, i2));
        if !pd_entry.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        if pd_entry.is_huge() {
            store.set_entry(pd_frame, i2, 0);
            if self.release_if_empty(store, pdpt_frame, i3, pd_frame) {
                self.release_if_empty(store, self.root, i4, pdpt_frame);
            }
            self.stats.unmapped += 1;
            return Ok(huge_mapping(virt, pd_entry, PAGE_2M));
        }

        let pt_frame = pd_entry.frame();
        let pt_entry = Entry::from_raw(store.entry(pt_frame, i1));
        if !pt_entry.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        store.set_entry(pt_frame, i1, 0);
        // The page table can go if that was its last mapping, and so can its parents.
        if self.release_if_empty(store, pd_frame, i2, pt_frame)
            && self.release_if_empty(store, pdpt_frame, i3, pd_frame)
        {
            self.release_if_empty(store, self.root, i4, pdpt_frame);
        }
        self.stats.unmapped += 1;
        Ok(Mapping {
            physical: pt_entry.address() + (virt & PAGE_MASK),
            entry: pt_entry,
            size: PAGE_4K,
        })
    }

    /// Change the flags of the mapping at `virt`, keeping its physical address.
    pub fn protect(
        &mut self,
        store: &mut impl TableStore,
        virt: u64,
        flags: EntryFlags,
    ) -> Result<(), MapError> {
        let Some(mapping) = self.translate(store, virt) else {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        };
        let [i4, i3, i2, i1] = indices(virt);
        let pml4 = Entry::from_raw(store.entry(self.root, i4));
        let pdpt = Entry::from_raw(store.entry(pml4.frame(), i3));

        let (table, index) = match mapping.size {
            PAGE_1G => (self.root, i3),
            PAGE_2M => (pml4.frame(), i2),
            _ => (Entry::from_raw(store.entry(pdpt.frame(), i2)).frame(), i1),
        };
        let preserved = mapping.entry.bits()
            & (Entry::ADDRESS_MASK | EntryFlags::HUGE | EntryFlags::ACCESSED | EntryFlags::DIRTY);
        store.set_entry(table, index, preserved | flags.bits());
        Ok(())
    }

    /// Walk to a child table, creating it if it is missing.
    ///
    /// A table that exists but was created for a different protection is upgraded rather
    /// than reused as-is: the architecture requires the user bit at every level of a
    /// ring-3 mapping, and a shared table may have been created for a kernel mapping.
    fn child(
        &mut self,
        store: &mut impl TableStore,
        table: Frame,
        index: usize,
        leaf_flags: EntryFlags,
    ) -> Result<Frame, MapError> {
        let entry = Entry::from_raw(store.entry(table, index));
        if entry.is_present() {
            let wanted = EntryFlags::intermediate(leaf_flags);
            if !entry.flags().contains(wanted) {
                store.set_entry(
                    table,
                    index,
                    entry.address() | entry.flags().union(wanted).bits(),
                );
            }
            return Ok(entry.frame());
        }
        let Some(child) = store.allocate_table() else {
            self.stats.failures += 1;
            return Err(MapError::OutOfMemory);
        };
        let Some(entry) = Entry::new(child * 4096, EntryFlags::intermediate(leaf_flags)) else {
            store.release_table(child);
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        };
        store.set_entry(table, index, entry.bits());
        self.stats.tables_allocated += 1;
        Ok(child)
    }

    /// Take the 2 MiB page covering `virt` apart into 512 4 KiB pages.
    ///
    /// This is page-table demotion, and it is explicit: `map_page` refuses to bury a large
    /// page rather than splitting one behind the caller's back, because the cost -- a page
    /// table, 512 entries, and 2 MiB of TLB invalidation -- belongs at a call site that
    /// knows it is paying it.
    ///
    /// Returns [`MapError::NotHuge`] when the covering mapping is already 4 KiB, and
    /// [`MapError::AlreadyMapped`] when a 1 GiB page covers the address.
    ///
    /// The new page table is allocated before the old entry is cleared, so a failure
    /// leaves the mapping exactly as it was.
    pub fn demote(&mut self, store: &mut impl TableStore, virt: u64) -> Result<(), MapError> {
        if !is_canonical(virt) || virt & PAGE_MASK != 0 {
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        }
        let [i4, i3, i2, _] = indices(virt);
        let pml4 = Entry::from_raw(store.entry(self.root, i4));
        if !pml4.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        let pdpt = Entry::from_raw(store.entry(pml4.frame(), i3));
        if !pdpt.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        if pdpt.is_huge() {
            self.stats.failures += 1;
            return Err(MapError::AlreadyMapped);
        }
        let pd_frame = pdpt.frame();
        let entry = Entry::from_raw(store.entry(pd_frame, i2));
        if !entry.is_present() {
            self.stats.failures += 1;
            return Err(MapError::NotMapped);
        }
        if !entry.is_huge() {
            self.stats.failures += 1;
            return Err(MapError::NotHuge);
        }

        let Some(table) = store.allocate_table() else {
            self.stats.failures += 1;
            return Err(MapError::OutOfMemory);
        };
        self.stats.tables_allocated += 1;

        let base = entry.address();
        let flags = entry.flags().minus(EntryFlags::from_bits(EntryFlags::HUGE));
        for page in 0..ENTRIES {
            let Some(page_entry) = Entry::new(base + (page as u64) * 4096, flags) else {
                store.release_table(table);
                self.stats.failures += 1;
                return Err(MapError::Misaligned);
            };
            store.set_entry(table, page, page_entry.bits());
        }
        let wanted = EntryFlags::intermediate(flags);
        let Some(table_entry) = Entry::new(table * 4096, wanted) else {
            store.release_table(table);
            self.stats.failures += 1;
            return Err(MapError::Misaligned);
        };
        store.set_entry(pd_frame, i2, table_entry.bits());
        self.stats.demotions += 1;
        Ok(())
    }

    /// Free `table`, clearing `parent[index]`, if every entry in it is zero.
    ///
    /// Returns whether the table was released. Callers use the answer to decide whether to
    /// look one level further up: a table can only have become empty because its last child
    /// was just released.
    fn release_if_empty(
        &mut self,
        store: &mut impl TableStore,
        parent: Frame,
        index: usize,
        table: Frame,
    ) -> bool {
        if !store.is_empty_table(table) {
            return false;
        }
        store.set_entry(parent, index, 0);
        store.release_table(table);
        self.stats.tables_released += 1;
        true
    }
}

/// Build a [`Mapping`] for a large page at `virt`.
#[must_use]
fn huge_mapping(virt: u64, entry: Entry, size: u64) -> Mapping {
    Mapping {
        physical: entry.address() + (virt & (size - 1)),
        entry,
        size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;
    use std::collections::BTreeMap;

    /// A `TableStore` backed by a map of 512-entry tables.
    struct MemStore {
        root: Frame,
        tables: BTreeMap<Frame, [u64; ENTRIES]>,
        free_frames: Vec<Frame>,
        allocated: u64,
        released: u64,
    }

    impl MemStore {
        fn new(root: Frame, first: Frame, count: Frame) -> Self {
            let mut tables = BTreeMap::new();
            tables.insert(root, [0; ENTRIES]);
            Self {
                root,
                tables,
                free_frames: (first..first + count).rev().collect(),
                allocated: 0,
                released: 0,
            }
        }

        /// Tables the walker allocated and has not released, not counting the PML4.
        fn live_tables(&self) -> usize {
            self.tables
                .keys()
                .filter(|frame| **frame != self.root)
                .count()
        }
    }

    impl TableStore for MemStore {
        fn entry(&self, table: Frame, index: usize) -> u64 {
            self.tables.get(&table).map_or(0, |entries| entries[index])
        }

        fn set_entry(&mut self, table: Frame, index: usize, value: u64) {
            let entries = self.tables.entry(table).or_insert([0; ENTRIES]);
            entries[index] = value;
        }

        fn allocate_table(&mut self) -> Option<Frame> {
            let frame = self.free_frames.pop()?;
            assert!(
                !self.tables.contains_key(&frame),
                "the store handed out frame {frame} twice"
            );
            self.tables.insert(frame, [0; ENTRIES]);
            self.allocated += 1;
            Some(frame)
        }

        fn release_table(&mut self, table: Frame) {
            assert_ne!(table, self.root, "the PML4 is never released");
            assert!(
                self.tables.remove(&table).is_some(),
                "released frame {table}, which was not a live table"
            );
            self.free_frames.push(table);
            self.released += 1;
        }

        fn is_empty_table(&self, table: Frame) -> bool {
            self.tables
                .get(&table)
                .is_none_or(|entries| entries.iter().all(|entry| *entry == 0))
        }
    }

    /// A user read-write mapping: what an anonymous `mmap` gets.
    fn user_flags() -> EntryFlags {
        EntryFlags::for_user(Protection::read_write())
    }

    #[test]
    fn known_addresses_split_into_the_expected_indices() {
        assert_eq!(indices(0), [0, 0, 0, 0]);
        assert_eq!(indices(0x0000_7fff_ffff_ffff), [255, 511, 511, 511]);
        assert_eq!(indices(KERNEL_BASE), [256, 0, 0, 0]);
        assert_eq!(indices(0xffff_ffff_ffff_ffff), [511, 511, 511, 511]);
        assert_eq!(
            pml4_index(KERNEL_BASE),
            256,
            "the kernel half starts at slot 256"
        );
        assert_eq!(
            indices(KERNEL_IMAGE_BASE),
            [RECURSIVE_SLOT + 1, RECURSIVE_SLOT, 0, 0],
            "the top 2 GiB is the last PML4 slot, and the image window starts at the \
             second-to-last 1 GiB entry inside it"
        );
        assert_eq!(
            indices(KERNEL_IMAGE_BASE + 0x4000_0000 - 1),
            [RECURSIVE_SLOT + 1, RECURSIVE_SLOT, 511, 511],
            "the last 2 MiB page of the image window's first 4 GiB is still inside its \
             own PML4 slot: the window can hold 4 GiB of image before it needs a second \
             page-directory-pointer entry"
        );
    }

    #[test]
    fn canonical_addresses_are_recognised() {
        assert!(is_canonical(0));
        assert!(is_canonical(USER_TOP));
        assert!(
            !is_canonical(0x0000_8000_0000_0000),
            "the hole is not mapped"
        );
        assert!(is_canonical(KERNEL_BASE));
        assert!(is_canonical(u64::MAX));
        assert!(!is_canonical(0x0001_0000_0000_0000));
        assert_eq!(sign_extend(0x0000_8000_0000_0000), 0xffff_8000_0000_0000);
    }

    #[test]
    fn the_kernel_and_user_halves_do_not_overlap() {
        assert!(is_user_address(0));
        assert!(is_user_address(USER_TOP));
        assert!(!is_user_address(KERNEL_BASE));
        assert!(is_kernel_address(KERNEL_BASE));
        assert!(is_kernel_address(u64::MAX));
        assert!(!is_kernel_address(USER_TOP));
    }

    #[test]
    fn an_entry_round_trips_an_address_and_its_flags() {
        let flags = user_flags();
        let entry = Entry::new(0x1234_5000, flags).expect("a valid entry");
        assert_eq!(entry.address(), 0x1234_5000);
        assert_eq!(entry.frame(), 0x12345);
        assert!(entry.is_present());
        assert!(entry.is_writable());
        assert!(entry.is_user());
        assert!(!entry.is_huge());
        assert!(entry.is_no_execute(), "read-write is not executable");
        assert_eq!(entry.flags(), flags);

        assert_eq!(Entry::new(0x1234_5678, flags), None, "not page-aligned");
        assert_eq!(Entry::new(1 << 60, flags), None, "not an encodable address");
        assert_eq!(Entry::empty().bits(), 0);
    }

    #[test]
    fn the_no_execute_bit_is_the_only_way_to_say_not_executable() {
        let read_only = EntryFlags::for_user(Protection::from_bits(Protection::READ));
        assert!(!read_only.contains(EntryFlags::from_bits(EntryFlags::WRITABLE)));
        assert!(read_only.contains(EntryFlags::from_bits(EntryFlags::NO_EXECUTE)));
        assert!(read_only.contains(EntryFlags::from_bits(EntryFlags::USER)));

        let executable = EntryFlags::for_user(Protection::from_bits(
            Protection::READ | Protection::EXECUTE,
        ));
        assert!(!executable.contains(EntryFlags::from_bits(EntryFlags::NO_EXECUTE)));

        let kernel =
            EntryFlags::for_kernel(Protection::from_bits(Protection::READ | Protection::WRITE));
        assert!(!kernel.contains(EntryFlags::from_bits(EntryFlags::USER)));
        assert!(kernel.contains(EntryFlags::from_bits(EntryFlags::GLOBAL)));
    }

    #[test]
    fn intermediate_entries_carry_what_the_leaf_needs() {
        let user = EntryFlags::intermediate(user_flags());
        assert!(user.contains(EntryFlags::from_bits(EntryFlags::PRESENT)));
        assert!(user.contains(EntryFlags::from_bits(EntryFlags::USER)));
        assert!(user.contains(EntryFlags::from_bits(EntryFlags::WRITABLE)));

        let read_only = EntryFlags::intermediate(EntryFlags::for_user(Protection::from_bits(
            Protection::READ,
        )));
        assert!(!read_only.contains(EntryFlags::from_bits(EntryFlags::WRITABLE)));

        let kernel = EntryFlags::intermediate(EntryFlags::for_kernel(Protection::from_bits(
            Protection::READ,
        )));
        assert!(!kernel.contains(EntryFlags::from_bits(EntryFlags::USER)));
    }

    #[test]
    fn protection_is_a_subset_relation() {
        let rw = Protection::read_write();
        let r = Protection::from_bits(Protection::READ);
        assert!(rw.contains(r));
        assert!(!r.contains(rw));
        assert!(rw.contains(Protection::from_bits(Protection::NONE)));
        assert!(Protection::from_bits(Protection::READ | Protection::EXECUTE).can_execute());
        assert!(!rw.can_execute());
        assert_eq!(
            Protection::from_bits(0xff).bits(),
            Protection::READ | Protection::WRITE | Protection::EXECUTE
        );
    }

    #[test]
    fn mapping_a_page_makes_it_translate_and_allocates_two_tables() {
        let mut store = MemStore::new(1, 100, 16);
        let mut tables = PageTables::new(1);

        tables
            .map_page(&mut store, 0x4000_0000, 0x8000_0000, user_flags())
            .expect("mapped");

        assert_eq!(tables.stats().pages_mapped, 1);
        assert_eq!(tables.stats().tables_allocated, 3, "pdpt, pd, and pt");
        let mapping = tables.translate(&store, 0x4000_0000).expect("mapped");
        assert_eq!(mapping.physical, 0x8000_0000);
        assert_eq!(mapping.size, 4096);
        // The offset within the page is preserved.
        assert_eq!(
            tables
                .translate(&store, 0x4000_0123)
                .expect("mapped")
                .physical,
            0x8000_0123
        );
        // One page over is not mapped.
        assert!(tables.translate(&store, 0x4000_1000).is_none());
        // The path is user-reachable at every level, not just the leaf.
        let pml4 = Entry::from_raw(store.entry(1, pml4_index(0x4000_0000)));
        let pdpt = Entry::from_raw(store.entry(pml4.frame(), pdpt_index(0x4000_0000)));
        assert!(pml4.is_user() && pdpt.is_user());
    }

    #[test]
    fn mapping_over_a_live_mapping_is_an_error() {
        let mut store = MemStore::new(1, 100, 16);
        let mut tables = PageTables::new(1);
        tables
            .map_page(&mut store, 0x1000, 0x9000, user_flags())
            .expect("mapped");
        assert_eq!(
            tables.map_page(&mut store, 0x1000, 0xa000, user_flags()),
            Err(MapError::AlreadyMapped)
        );
        assert_eq!(
            tables
                .translate(&store, 0x1000)
                .expect("still mapped")
                .physical,
            0x9000
        );
    }

    #[test]
    fn an_unaligned_or_non_canonical_address_is_rejected() {
        let mut store = MemStore::new(1, 100, 16);
        let mut tables = PageTables::new(1);
        assert_eq!(
            tables.map_page(&mut store, 0x1001, 0x9000, user_flags()),
            Err(MapError::Misaligned)
        );
        assert_eq!(
            tables.map_page(&mut store, 0x1000, 0x9001, user_flags()),
            Err(MapError::Misaligned)
        );
        assert_eq!(
            tables.map_page(&mut store, 0x0000_8000_0000_0000, 0x9000, user_flags()),
            Err(MapError::Misaligned)
        );
        assert!(tables.translate(&store, 0x0000_8000_0000_0000).is_none());
    }

    #[test]
    fn a_huge_page_is_demoted_when_a_small_page_must_go_inside_it() {
        let mut store = MemStore::new(1, 100, 64);
        let mut tables = PageTables::new(1);
        let flags = EntryFlags::for_kernel(Protection::read_write());

        tables
            .map_huge(&mut store, 0x4000_0000, 0x1_0000_0000, flags)
            .expect("huge mapping");
        assert_eq!(tables.stats().huge_pages_mapped, 1);
        let mapping = tables.translate(&store, 0x4000_1234).expect("huge");
        assert_eq!(mapping.size, 1 << 21);
        assert_eq!(mapping.physical, 0x1_0000_1234);

        // Mapping a 4 KiB page inside the huge page is refused while it is still huge.
        assert_eq!(
            tables.map_page(&mut store, 0x4000_0000, 0x2_0000_0000, flags),
            Err(MapError::AlreadyMapped)
        );
        assert_eq!(tables.stats().demotions, 0);
        assert_eq!(
            tables.translate(&store, 0x4000_1234).expect("huge").size,
            1 << 21
        );

        // Demotion is an explicit decision, and after it the whole range is 4 KiB pages
        // carrying the same physical addresses.
        tables.demote(&mut store, 0x4000_0000).expect("demoted");
        assert_eq!(tables.stats().demotions, 1);
        assert_eq!(
            tables.translate(&store, 0x4000_1234).expect("mapped").size,
            4096
        );
        assert_eq!(
            tables
                .translate(&store, 0x4000_1234)
                .expect("mapped")
                .physical,
            0x1_0000_1234
        );
        assert_eq!(
            tables.demote(&mut store, 0x4000_0000),
            Err(MapError::NotHuge),
            "splitting twice is a caller bug"
        );

        // Now the page inside can be replaced.
        tables.unmap(&mut store, 0x4000_0000).expect("unmapped");
        tables
            .map_page(&mut store, 0x4000_0000, 0x2_0000_0000, flags)
            .expect("remapped");
        assert_eq!(
            tables
                .translate(&store, 0x4000_0000)
                .expect("mapped")
                .physical,
            0x2_0000_0000
        );
        // The rest of the former huge page is still mapped, one 4 KiB page at a time.
        let rest = tables.translate(&store, 0x4000_2000).expect("mapped");
        assert_eq!(rest.physical, 0x1_0000_2000);
        assert_eq!(rest.size, 4096);

        // The split page table is reclaimed only when its last mapping goes.
        tables.unmap(&mut store, 0x4000_0000).expect("unmapped");
        assert!(tables.translate(&store, 0x4000_2000).is_some());
        assert_eq!(
            store.live_tables(),
            3,
            "the directory pointer table, the page directory, and the page table holding 511 live pages"
        );
    }

    #[test]
    fn unmapping_reclaims_empty_tables() {
        let mut store = MemStore::new(1, 100, 64);
        let mut tables = PageTables::new(1);
        let flags = EntryFlags::for_kernel(Protection::from_bits(Protection::READ));

        tables
            .map_page(&mut store, 0x3000_0000, 0x8000_0000, flags)
            .expect("mapped");
        assert_eq!(store.live_tables(), 3);

        let mapping = tables.unmap(&mut store, 0x3000_0000).expect("unmapped");
        assert_eq!(mapping.physical, 0x8000_0000);
        assert!(tables.translate(&store, 0x3000_0000).is_none());
        assert_eq!(store.live_tables(), 0, "every table was reclaimed");
        assert_eq!(tables.stats().tables_released, 3);
        assert_eq!(store.released, store.allocated);

        assert_eq!(
            tables.unmap(&mut store, 0x3000_0000),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn a_shared_page_table_is_not_reclaimed_while_it_is_still_needed() {
        let mut store = MemStore::new(1, 100, 64);
        let mut tables = PageTables::new(1);
        let flags = EntryFlags::for_kernel(Protection::from_bits(Protection::READ));

        // Two pages in the same page table, and a third in another.
        tables
            .map_page(&mut store, 0x3000_0000, 0x8000_0000, flags)
            .expect("mapped");
        tables
            .map_page(&mut store, 0x3000_1000, 0x8000_1000, flags)
            .expect("mapped");
        tables
            .map_page(&mut store, 0x6000_2000_0000, 0x8000_2000_0000, flags)
            .expect("mapped");

        tables.unmap(&mut store, 0x3000_0000).expect("unmapped");
        assert!(tables.translate(&store, 0x3000_1000).is_some());
        assert!(tables.translate(&store, 0x6000_2000_0000).is_some());

        tables.unmap(&mut store, 0x3000_1000).expect("unmapped");
        tables
            .unmap(&mut store, 0x6000_2000_0000)
            .expect("unmapped");
        assert_eq!(store.live_tables(), 0);
        assert_eq!(store.released, store.allocated);
    }

    #[test]
    fn protecting_a_mapping_keeps_its_address_and_changes_its_flags() {
        let mut store = MemStore::new(1, 100, 16);
        let mut tables = PageTables::new(1);
        let read = EntryFlags::for_user(Protection::from_bits(Protection::READ));
        tables
            .map_page(&mut store, 0x1000, 0x9000, read)
            .expect("mapped");

        tables
            .protect(&mut store, 0x1000, user_flags())
            .expect("protected");
        let mapping = tables.translate(&store, 0x1000).expect("mapped");
        assert_eq!(mapping.physical, 0x9000);
        assert!(mapping.entry.is_writable());

        assert_eq!(
            tables.protect(&mut store, 0x2000, read),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn mapping_a_contiguous_range_uses_huge_pages_where_it_can() {
        let mut store = MemStore::new(1, 100, 512);
        let mut tables = PageTables::new(1);
        let flags = EntryFlags::for_kernel(Protection::read_write());

        // 2 MiB plus three pages: one huge page and three small ones.
        tables
            .map_range(
                &mut store,
                0x4000_0000,
                0x1_0000_0000,
                (1 << 21) + 3 * 4096,
                flags,
            )
            .expect("mapped");
        assert_eq!(tables.stats().huge_pages_mapped, 1);
        assert_eq!(tables.stats().pages_mapped, 3);
        assert_eq!(
            tables.translate(&store, 0x4000_0000).expect("mapped").size,
            1 << 21
        );
        assert_eq!(
            tables.translate(&store, 0x4020_0000).expect("mapped").size,
            4096
        );
        assert_eq!(
            tables
                .translate(&store, 0x4020_1000)
                .expect("mapped")
                .physical,
            0x1_0020_1000
        );
    }

    #[test]
    fn running_out_of_table_frames_fails_without_corrupting_anything() {
        let mut store = MemStore::new(1, 100, 2); // room for two tables, not three
        let mut tables = PageTables::new(1);
        let flags = EntryFlags::for_kernel(Protection::from_bits(Protection::READ));
        assert_eq!(
            tables.map_page(&mut store, 0x1000, 0x9000, flags),
            Err(MapError::OutOfMemory)
        );
        assert!(tables.translate(&store, 0x1000).is_none());
        assert_eq!(tables.stats().failures, 1);
        assert_eq!(
            tables.stats().tables_allocated,
            2,
            "the two that fit were used"
        );
        assert_eq!(tables.stats().pages_mapped, 0);
    }

    #[test]
    fn random_map_unmap_agrees_with_a_model() {
        let mut rng = Rng::new(0x007a_b1e5);
        let mut store = MemStore::new(1, 100, 4096);
        let mut tables = PageTables::new(1);
        let flags = user_flags();
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();

        for step in 0..2000 {
            // A small address space, so collisions and re-mappings happen often.
            let virt = 0x2000_0000 + rng.below(64) * 4096;
            if rng.chance(60) {
                let phys = 0x8000_0000 + rng.below(4096) * 4096;
                match tables.map_page(&mut store, virt, phys, flags) {
                    Ok(()) => {
                        assert!(model.insert(virt, phys).is_none(), "duplicate mapping");
                    }
                    Err(MapError::AlreadyMapped) => {
                        assert!(model.contains_key(&virt), "the model disagrees");
                    }
                    Err(other) => panic!("unexpected error {other:?} at step {step}"),
                }
            } else if tables.unmap(&mut store, virt).is_ok() {
                assert!(
                    model.remove(&virt).is_some(),
                    "unmapped something untracked"
                );
            }
            // The model and the tables agree about every address.
            assert_eq!(
                tables.translate(&store, virt).map(|m| m.physical),
                model.get(&virt).copied()
            );
        }

        // Tear everything down: no table may be left behind, and no frame may leak.
        for virt in model.keys().copied().collect::<Vec<_>>() {
            tables.unmap(&mut store, virt).expect("unmapped");
        }
        assert_eq!(store.live_tables(), 0);
        assert_eq!(store.allocated, store.released);
        assert_eq!(tables.stats().pages_mapped, tables.stats().unmapped);
    }
}
