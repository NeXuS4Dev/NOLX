//! Page tables that live in frames, and the switch to the kernel's own page tables.
//!
//! `nolx_mm_core::paging` walks and edits page tables; the one thing it cannot do is reach
//! a table by its frame number, which is what [`TableStore`] asks for. The kernel's answer
//! is this module: page tables are ordinary frames from the frame allocator, and they are
//! read and written through the direct map, exactly like the free-list links in
//! `kernel/src/mm/store.rs`.
//!
//! Everything here is single-processor boot code today: the page-table page is taken, is
//! written once, and is only reachable from this processor. When a second processor exists,
//! the store grows a lock and the counters become per-processor; the trait does not change.
//!
//! # The identity map
//!
//! The boot layer installs an identity map so that the 32-bit trampoline can keep running
//! while it builds the higher-half tables, and so that the kernel starts executing at the
//! address the loader chose. It is dead weight the moment the kernel's own GDT, IDT, and
//! stack are live, and it is what makes a physical address usable as a pointer -- the thing
//! the higher-half switch exists to stop. [`drop_identity_map`] clears it and reports what
//! the address space looks like afterwards.

use crate::mm::{self, phys, PAGE_SHIFT, PAGE_SIZE};
use crate::sync::SyncCell;
use nolx_mm_core::buddy::Frame;
use nolx_mm_core::commit::PoolKind;
use nolx_mm_core::paging::{pml4_index, Entry, PageTables, TableStore, ENTRIES};

/// Page tables allocated from the frame allocator since boot.
static ALLOCATED: SyncCell<u64> = SyncCell::new(0);
/// Page tables returned to the frame allocator since boot.
static RELEASED: SyncCell<u64> = SyncCell::new(0);
/// 4 KiB of page table charged to the system PTE pool.
const TABLE_BYTES: u64 = PAGE_SIZE as u64;

/// The [`TableStore`] over frames reached through the direct map.
///
/// A value rather than a static because the trait takes `&mut self`: the walker is re-entrant
/// by construction (each operation gets its own store), while the counters and the pool
/// charges are process-wide and live above it.
pub struct KernelTableStore {
    /// Page tables this store allocated and has not released yet.
    live: u64,
}

impl KernelTableStore {
    /// A store that has allocated nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { live: 0 }
    }

    /// Page tables this store allocated and has not released.
    #[must_use]
    pub const fn live_tables(&self) -> u64 {
        self.live
    }

    /// The kernel's pointer to entry `index` of `table`.
    ///
    /// The frame is a page table: it was taken from the frame allocator and marked
    /// [`crate::mm::FrameState::PageTable`], so nothing else owns it, and it lies inside the
    /// first 2 GiB because that is all the allocator manages.
    fn entry_pointer(table: Frame, index: usize) -> *mut u64 {
        debug_assert!(index < ENTRIES, "page-table index out of range");
        let byte = table * PAGE_SIZE as u64 + (index as u64) * 8;
        phys::direct_ptr::<u64>(byte)
    }
}

impl Default for KernelTableStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TableStore for KernelTableStore {
    fn entry(&self, table: Frame, index: usize) -> u64 {
        // SAFETY: the frame is a page table owned by the kernel, mapped by the direct map,
        // and this processor is the only reader and writer. The read is volatile because the
        // table is live: the processor walks it too, and a later TLB fill must observe
        // exactly the bytes written here.
        unsafe { core::ptr::read_volatile(Self::entry_pointer(table, index)) }
    }

    fn set_entry(&mut self, table: Frame, index: usize, value: u64) {
        // SAFETY: as `entry`, and this is the only writer of this table.
        unsafe { core::ptr::write_volatile(Self::entry_pointer(table, index), value) }
    }

    fn allocate_table(&mut self) -> Option<Frame> {
        // The walker treats a fresh table as zero, and so does the processor: an entry that
        // is not cleared is an entry that maps something nobody asked for.
        let frame = mm::allocate_frame_zeroed()?;
        mm::mark_page_table(frame);
        if mm::pool::charge(PoolKind::SystemPte, TABLE_BYTES).is_err() {
            let _ = mm::free_frame(frame);
            return None;
        }
        self.live += 1;
        // SAFETY: single boot processor; the counter is only ever bumped here.
        unsafe { *ALLOCATED.get() += 1 };
        Some(frame as Frame)
    }

    fn release_table(&mut self, table: Frame) {
        self.live = self.live.saturating_sub(1);
        // SAFETY: single boot processor; see `allocate_table`.
        unsafe { *RELEASED.get() += 1 };
        // A zeroed frame came back to a table that is done; the block is a single frame, so
        // the frame-map state decides whether the free is legal. `PageTable` is one of the
        // states `free_block` accepts, which is what makes this the symmetric operation.
        let _ = mm::free_frame(table as usize);
        let _ = mm::pool::release(PoolKind::SystemPte, TABLE_BYTES);
    }

    fn is_empty_table(&self, table: Frame) -> bool {
        for index in 0..ENTRIES {
            if Entry::from_raw(self.entry(table, index)).is_present() {
                return false;
            }
        }
        true
    }
}

/// Page tables allocated and released since boot.
#[must_use]
pub fn totals() -> (u64, u64) {
    // SAFETY: single boot processor; both counters are read together only in reports.
    unsafe { (*ALLOCATED.get(), *RELEASED.get()) }
}

/// The frame the processor is using as its page-table root.
#[must_use]
pub fn current_root() -> Frame {
    let cr3: u64;
    // SAFETY: reading CR3 has no side effects and is available at every privilege level.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags))
    };
    (cr3 & !0xFFF) >> PAGE_SHIFT
}

/// Make `root` the processor's page-table root.
///
/// # Safety
///
/// `root` must be the PML4 of an address space that maps this code, this stack, and the
/// direct map. Loading anything else is an immediate triple fault, not a recoverable error.
pub unsafe fn activate(root: Frame) {
    let address = root << PAGE_SHIFT;
    // SAFETY: the caller guarantees `root` is a valid root that keeps the running code and
    // stack mapped. Writing CR3 also flushes non-global TLB entries, which is why this is
    // the flush used after changing the tables.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) address, options(nostack, preserves_flags));
    }
}

/// Flush one page's translation from the TLB.
///
/// # Safety
///
/// `virt` is only used as an operand to `invlpg`; the instruction has no other effect.
#[inline]
pub unsafe fn invalidate_page(virt: u64) {
    // SAFETY: as documented. `invlpg` takes a memory operand, hence the `in(reg)` + braces.
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

/// What the address space looked like after the identity map was dropped.
#[derive(Clone, Copy, Debug)]
pub struct SwitchReport {
    /// PML4 entries that were present and have been cleared.
    pub dropped_entries: usize,
    /// Whether a low physical address is still reachable through the identity map.
    pub identity_still_reachable: bool,
    /// Whether the same physical address is reachable through the direct map.
    pub direct_map_reachable: bool,
    /// Whether the kernel's own code is reachable through the image window.
    pub image_reachable: bool,
}

/// Drop the boot identity map -- PML4 entries for the low half -- and prove it is gone.
///
/// Runs once, after `hal::tables::init` has replaced the trampoline's GDT and IDT with the
/// kernel's own and after the kernel has moved to its own stack: those are the last things
/// that needed a low address to be a valid address. Everything afterwards reaches physical
/// memory through the direct map, which this leaves untouched.
///
/// Returns what the address space looks like afterwards, for the boot log. A `false` in
/// [`SwitchReport::direct_map_reachable`] or [`SwitchReport::image_reachable`] is a bug,
/// and the caller must not continue.
#[must_use]
pub fn drop_identity_map() -> SwitchReport {
    let root = current_root();
    let mut store = KernelTableStore::new();
    let mut dropped = 0;

    for slot in 0..pml4_index(phys::DIRECT_MAP_BASE) {
        let entry = Entry::from_raw(store.entry(root, slot));
        if entry.is_present() {
            store.set_entry(root, slot, 0);
            dropped += 1;
        }
    }

    // Reloading CR3 flushes every translation the old map left in the TLB, including the
    // global ones the kernel mappings carry.
    // SAFETY: `root` is the root this code is already running on; clearing entries in the
    // low half cannot unmap the kernel half.
    unsafe { activate(root) };

    let tables = PageTables::new(root);
    let probe = phys::KERNEL_LOAD_ADDR;
    SwitchReport {
        dropped_entries: dropped,
        identity_still_reachable: tables.translate(&store, probe).is_some(),
        direct_map_reachable: tables
            .translate(&store, phys::DIRECT_MAP_BASE + probe)
            .is_some(),
        image_reachable: tables
            .translate(&store, phys::KERNEL_IMAGE_BASE + probe)
            .is_some(),
    }
}
