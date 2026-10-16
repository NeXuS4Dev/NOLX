//! Memory management.
//!
//! Three things live here, in the order they depend on each other:
//!
//! * the **frame map** -- one state per physical frame, built from the firmware's
//!   description of memory, with everything the kernel itself occupies marked unusable;
//! * the **frame allocator** -- the buddy allocator from `nolx-mm-core` over the frames the
//!   map calls free, with the boot processor's magazine in front of it, and its free-list
//!   links written into the free frames themselves through [`store::DirectMapStore`];
//! * the **self test** -- the stress run that proves the allocator still agrees with the
//!   frame map after tens of thousands of allocations taken while interrupts are live.
//!
//! The **kernel heap** sits on top of the frame allocator: `nolx_mm_core::heap` is a
//! boundary-tagged allocator over one region, and [`heap`] gives it regions, the nonpaged-pool
//! charges for them, and the `GlobalAlloc` implementation the Rust runtime uses. Above that
//! sits [`space`]: a per-process address space that owns a page-table root, a VAD tree, and a
//! commit charge, and that resolves the four page faults
//! `docs/architecture/03-memory-manager.md` names. The algorithms behind the allocator
//! -- splitting, coalescing, the magazines, the commit accounting -- live in
//! `crates/nolx-mm-core`, where they are tested on the host without a machine to boot.
//!
//! # Why the frame map is an array and not a structure
//!
//! `struct page` equivalents are indexed by frame number, so the array must be allocated
//! before the allocator exists. It is carved out of the direct map at a fixed address,
//! which is the same bootstrap problem the object map has, and it is solved the same way:
//! the thing that hands out memory cannot itself be handed out.

pub mod heap;
pub mod mmio;
pub mod numa;
pub mod phys;
pub mod pool;
pub mod selftest;
pub mod space;
pub mod store;
pub mod tables;

use crate::hal::multiboot::BootInfo;
use crate::sync::SyncCell;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::buddy::{Buddy, BuddyStats, Frame, FreeError, MAX_ORDER};
use nolx_mm_core::magazine::{Allocated, FrameCache, FrameCacheStats};
use store::{DirectMapStore, MAX_MANAGED_FRAMES};

pub use phys::{direct, physical, DIRECT_MAP_BASE, KERNEL_IMAGE_BASE};

/// The boot processor's index into the per-processor structures.
pub const BOOT_CPU: usize = 0;

/// The buddy allocator over physical frames.
static ALLOCATOR: SyncCell<Buddy> = SyncCell::new(Buddy::new());

/// The boot processor's frame cache: a magazine of single frames in front of the allocator.
///
/// Sized for one processor today. The array is per-processor by construction, so bringing up
/// a second processor is a matter of a real lock, not of rediscovering this structure.
static FRAME_CACHE: SyncCell<FrameCache<1, 64>> = SyncCell::new(FrameCache::new());

extern "C" {
    /// The end of the static heap arena the linker script reserves after the image.
    /// Defined in `kernel/linker.ld`; taking its address is how the kernel learns where it
    /// itself ends without parsing its own ELF headers at runtime.
    static _kernel_heap_end: u8;
}

/// Bytes in a physical page.
pub const PAGE_SIZE: usize = 4096;
/// Bits in a page offset.
pub const PAGE_SHIFT: u32 = 12;

/// The state of one physical frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameState {
    /// Not present, or reserved by firmware. Never allocated.
    Reserved = 0,
    /// Free and available to the allocator.
    Free = 1,
    /// Allocated to a user or kernel mapping.
    Allocated = 2,
    /// In use as a page-table page. Never handed to a user mapping.
    PageTable = 3,
    /// Free and already zeroed, so a user mapping can skip the zeroing step.
    Zeroed = 4,
}

/// The physical frame map.
///
/// Sized for up to 64 GiB of RAM at 4 KiB frames, which is 16.7 million frames. At one byte
/// per frame that is 16 MiB of kernel memory -- a real cost, and the reason M3 replaces the
/// byte-per-frame array with a sectioned map that only allocates for populated address
/// ranges. The limit is recorded here rather than discovered at boot.
pub const MAX_FRAMES: usize = 16 * 1024 * 1024;

/// The frame map, as a static array.
///
/// A static array because there is no allocator yet to put it anywhere else, and no second
/// CPU to race with. M3 moves it into the direct map and puts a lock on it.
static FRAME_STATES: SyncCell<[FrameState; MAX_FRAMES]> =
    SyncCell::new([FrameState::Reserved; MAX_FRAMES]);

/// How much of the frame map is populated, in frames.
static FRAME_MAP_LEN: SyncCell<usize> = SyncCell::new(0);

/// Statistics gathered at boot, reported on the console and later through `sysctl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryStats {
    /// Frames the firmware described as usable.
    pub usable_frames: u64,
    /// Frames reserved by firmware.
    pub reserved_frames: u64,
    /// Regions the firmware reported.
    pub regions: usize,
    /// Regions dropped for exceeding the kernel's table, which means the accounting is
    /// incomplete and the kernel says so loudly.
    pub dropped_regions: usize,
    /// Bytes of usable RAM.
    pub usable_bytes: u64,
    /// Highest usable physical address.
    pub highest_address: u64,
}

/// Build the frame map from the firmware's memory map.
pub fn init(console: &mut dyn HalConsole, info: &BootInfo) {
    // SAFETY: `info` was produced by the boot hand-off, which validated the Multiboot
    // magic. `memory_map` bounds-checks every read against the map's own length.
    let map = unsafe { info.memory_map() };

    if map.regions().is_empty() {
        init_without_firmware(console);
        return;
    }

    let mut stats = MemoryStats {
        regions: map.regions().len(),
        dropped_regions: map.truncated(),
        usable_bytes: map.usable_bytes(),
        highest_address: map.highest_usable(),
        ..MemoryStats::default()
    };

    // SAFETY: single boot processor, no interrupts enabled yet, and `FRAME_STATES` is
    // written only here during boot before anything else reads it.
    unsafe {
        let len = ((stats.highest_address as usize) / PAGE_SIZE).min(MAX_FRAMES);
        for state in (*FRAME_STATES.get()).iter_mut().take(len) {
            *state = FrameState::Reserved;
        }
        for region in map.regions() {
            let first = (region.base as usize) / PAGE_SIZE;
            let last = (region.end() as usize).div_ceil(PAGE_SIZE);
            let state = if region.is_usable() {
                FrameState::Free
            } else {
                FrameState::Reserved
            };
            for index in first.min(len)..last.min(len) {
                (*FRAME_STATES.get())[index] = state;
            }
            if region.is_usable() {
                stats.usable_frames += (last - first) as u64;
            } else {
                stats.reserved_frames += (last - first) as u64;
            }
        }
        *FRAME_MAP_LEN.get() = len;
    }

    // The nodes exist before the allocator can hand out a frame: an allocation that ran with
    // an empty node list would have no node to come from.
    numa::init();
    finish_boot(console);

    let _ = writeln!(
        console,
        "mm: {} regions, {} MiB usable, {} MiB reserved, highest usable {:#x}",
        stats.regions,
        stats.usable_bytes / (1024 * 1024),
        (stats.reserved_frames * PAGE_SIZE as u64) / (1024 * 1024),
        stats.highest_address,
    );
    if stats.dropped_regions > 0 {
        let _ = writeln!(
            console,
            "mm: WARNING {} firmware regions exceeded the frame map and were dropped; \
             memory accounting is incomplete",
            stats.dropped_regions
        );
    }
    let _ = writeln!(console, "NOLX: mm online");
}

/// Build a conservative frame map when the loader gave us no memory map.
///
/// Uses `mem_upper`, which undercounts because it stops at the first hole above 1 MiB. The
/// kernel boots, with less memory than the machine has, and says so.
pub fn init_without_firmware(console: &mut dyn HalConsole) {
    let _ = writeln!(
        console,
        "mm: no firmware memory map; assuming a conservative 64 MiB. \
         Everything above that is unusable."
    );
    // SAFETY: as in [`init`].
    unsafe {
        let len = (64 * 1024 * 1024) / PAGE_SIZE;
        for state in (*FRAME_STATES.get()).iter_mut().take(len) {
            *state = FrameState::Free;
        }
        // The first megabyte holds the real-mode structures, the BIOS data area, and the
        // kernel image itself. Never hand it out.
        for state in (*FRAME_STATES.get())
            .iter_mut()
            .take((1024 * 1024) / PAGE_SIZE)
        {
            *state = FrameState::Reserved;
        }
        *FRAME_MAP_LEN.get() = len;
    }
    numa::init();
    finish_boot(console);
    let _ = writeln!(console, "NOLX: mm online");
}

/// The shared tail of both frame-map paths: reserve what the kernel occupies, then hand the
/// rest to the allocator, then say what happened.
fn finish_boot(console: &mut dyn HalConsole) {
    let reserved = reserve_kernel_memory();
    let len = frame_map_len();
    let managed = (MAX_MANAGED_FRAMES as usize).min(len);
    let unmanageable = len - managed;
    let added = build_allocator();

    let stats = allocator_stats();
    let _ = writeln!(
        console,
        "mm: frame map: {len} frames, {reserved} taken back for the kernel image and low \
         memory, {added} handed to the allocator"
    );
    if unmanageable > 0 {
        let _ = writeln!(
            console,
            "mm: WARNING {unmanageable} frames above {:#x} are outside the boot identity \
             map and stay reserved",
            store::MAX_MANAGED_BYTES
        );
    }
    let _ = writeln!(
        console,
        "mm: buddy allocator: {} frames managed, {} free, largest free order {}",
        stats.managed_frames,
        free_frames(),
        stats.largest_free_order().unwrap_or(0),
    );
    let _ = writeln!(console, "NOLX: frame allocator online");
    numa::report(console);

    // The heap is the allocator's first client: a region is a run of frames, and the frames
    // have to exist before anything can allocate a byte. Its failure is not fatal -- nothing in
    // the boot path allocates from it yet -- so it reports and carries on.
    heap::init(console);
}

/// Mark the frames the kernel itself occupies, and everything the loader used, unusable.
///
/// Returns how many frames that took back from the free pool.
fn reserve_kernel_memory() -> usize {
    let free_before = count_frames(FrameState::Free);

    // The first megabyte holds the real-mode structures, the BIOS data area, and -- under
    // the loaders this kernel is tested with -- the Multiboot information the frame map was
    // just built from. None of it is ever allocatable.
    let low_reserved = (1 << 20) / PAGE_SIZE;
    reserve(0, low_reserved, FrameState::Reserved);

    // The kernel image and its BSS, which holds this frame map and the allocator's front
    // matter, plus the static heap arena the linker script reserves after `_end`. Writing a
    // free-list link into any of that would overwrite the kernel.
    let image_end = (kernel_image_end() as usize).div_ceil(PAGE_SIZE);
    reserve(low_reserved, image_end, FrameState::Reserved);

    // Frames the direct map does not cover cannot hold free-list links, so
    // they stay reserved rather than being described as free and then quietly skipped.
    let len = frame_map_len();
    reserve(
        (MAX_MANAGED_FRAMES as usize).min(len),
        len,
        FrameState::Reserved,
    );

    free_before.saturating_sub(count_frames(FrameState::Free))
}

/// The address at which the kernel image, and the static heap arena after it, ends.
///
/// The linker symbol is a virtual address in the image window; the frame map is indexed by
/// physical frame, so the window offset comes off here. This is the boundary between the
/// frames the kernel owns and the frames it can hand out.
fn kernel_image_end() -> u64 {
    let virtual_address = core::ptr::addr_of!(_kernel_heap_end) as u64;
    phys::physical(virtual_address).unwrap_or(virtual_address)
}

/// Put frames `[first, last)` into `state`, clamped to the populated part of the map.
fn reserve(first: usize, last: usize, state: FrameState) {
    // SAFETY: called during single-processor boot bring-up, in the window where the frame
    // map exists but no frame has been handed out, so no other reader can observe the map
    // while it is being written here.
    unsafe {
        let len = *FRAME_MAP_LEN.get();
        for index in first.min(len)..last.min(len) {
            (*FRAME_STATES.get())[index] = state;
        }
    }
}

/// Hand every free run of frames to the buddy allocator.
///
/// This is the last thing that reads the firmware's structures: the allocator writes its
/// free-list links into the free frames themselves, and the loader's information may live in
/// frames the firmware called usable. Building one run at a time from the completed frame
/// map, rather than from the firmware regions again, means the two views cannot disagree
/// about what is available.
fn build_allocator() -> u64 {
    let mut store = DirectMapStore::new();
    let len = frame_map_len();
    let mut first_free: Option<usize> = None;
    let mut added = 0u64;

    for index in 0..len {
        match (frame_state(index), first_free) {
            (FrameState::Free, None) => first_free = Some(index),
            (FrameState::Free, Some(_)) => {}
            (_, Some(start)) => {
                added += add_frames(&mut store, start, index);
                first_free = None;
            }
            (_, None) => {}
        }
    }
    if let Some(start) = first_free {
        added += add_frames(&mut store, start, len);
    }
    added
}

/// Add one run of free frames to the allocator.
fn add_frames(store: &mut DirectMapStore, first: usize, end: usize) -> u64 {
    // SAFETY: single boot processor with interrupts still disabled, and nothing else in the
    // kernel can be running. `first` and `end` bound a run of frames the frame map marks
    // free, so the links are written only into memory nobody else owns.
    unsafe { (*ALLOCATOR.get()).add_region(store, first as Frame, end as Frame) }
}

/// Allocate one 4 KiB frame.
pub fn allocate_frame() -> Option<usize> {
    allocate_frames(0)
}

/// Allocate `2^order` contiguous frames, the first of which is aligned to the block.
///
/// Which node the block comes from is the zonelist's decision, made here rather than inside the
/// allocator: the local node first, then the rest in the order [`numa`] computed. This machine
/// has one node, so the walk is one iteration -- and it is a walk, so a machine with more nodes
/// needs a per-node allocator behind [`numa::allocate_from`], not a different allocation path.
pub fn allocate_frames(order: usize) -> Option<usize> {
    for (node, usable) in numa::zones().iter() {
        if !usable {
            continue;
        }
        if let Some(frame) = numa::allocate_from(node, order) {
            return Some(frame);
        }
    }
    None
}

/// Allocate `2^order` contiguous frames from the machine's only allocator.
///
/// The node layer's leaf: one buddy allocator and one magazine in front of it. When a second
/// node gets its own allocator, this becomes the per-node body and the zonelist walk above
/// keeps its shape.
pub fn allocate_frames_from_local(order: usize) -> Option<usize> {
    let mut store = DirectMapStore::new();
    // SAFETY: the frame allocator runs on the boot processor. The timer interrupt is the
    // only other code that can run, and its handler allocates nothing and touches no part of
    // the memory manager. There is no second processor yet; when there is one, this becomes
    // a lock and the magazines become per-processor for real.
    unsafe {
        let cache = &mut *FRAME_CACHE.get();
        let buddy = &mut *ALLOCATOR.get();
        let allocated = if order == 0 {
            cache.allocate(BOOT_CPU, buddy, &mut store)
        } else {
            buddy.allocate(&mut store, order).map(|frame| Allocated {
                frame,
                zeroed: false,
            })
        }?;
        // Every frame of the block, not just its first one: the frame map describes frames,
        // and a block whose interior still reads as free is a block the allocator has
        // already promised to somebody else.
        let base = allocated.frame as usize;
        for index in base..base + (1usize << order) {
            mark_allocated(index);
        }
        Some(base)
    }
}

/// Return one frame to the allocator.
pub fn free_frame(frame: usize) -> Result<(), FreeError> {
    free_block(frame, 0)
}

/// Return a block of `2^order` frames, starting at `frame`.
pub fn free_block(frame: usize, order: usize) -> Result<(), FreeError> {
    if order > MAX_ORDER {
        return Err(FreeError::OrderTooLarge);
    }
    let count = 1usize << order;
    let len = frame_map_len();
    if !frame.is_multiple_of(count) {
        return Err(FreeError::NotAligned);
    }
    if frame + count > len {
        return Err(FreeError::NotManaged);
    }

    // The frame map is the authority on whether a frame may be freed at all: a frame that is
    // reserved, already free, or was never handed out must not reach the allocator, where it
    // would be a double free or an entry outside the managed range.
    for index in frame..frame + count {
        match frame_state(index) {
            FrameState::Allocated | FrameState::PageTable | FrameState::Zeroed => {}
            FrameState::Free => return Err(FreeError::AlreadyFree),
            FrameState::Reserved => return Err(FreeError::NotManaged),
        }
    }

    let mut store = DirectMapStore::new();
    // SAFETY: as `allocate_frames`.
    unsafe {
        if order == 0 {
            (*FRAME_CACHE.get()).free(BOOT_CPU, frame as Frame, &mut *ALLOCATOR.get(), &mut store);
        } else {
            (*ALLOCATOR.get()).free(&mut store, frame as Frame, order)?;
        }
    }

    // Only now: an allocator that refused the block must not leave the frame map claiming
    // the frames are free, or the next allocation could hand out a frame twice.
    for index in frame..frame + count {
        // SAFETY: as `reserve`; the block has just been handed back to the allocator.
        unsafe { (*FRAME_STATES.get())[index] = FrameState::Free };
    }
    Ok(())
}

/// Allocate one frame whose contents are known to be zero.
///
/// This is what page tables and user mappings need: an entry that is not cleared is an entry
/// that maps memory nobody asked for, and a page handed to a user process must not contain
/// the previous owner's bytes. The magazine keeps a pool of frames already known to be zero
/// -- they came back from a mapping that was zeroed before it was mapped -- so in the common
/// case this costs nothing but the lookup.
pub fn allocate_frame_zeroed() -> Option<usize> {
    let frame = allocate_frame()?;
    clear_frame(frame);
    Some(frame)
}

/// Mark a frame as holding a page table, so the frame map, the allocator's accounting, and
/// the boot audit agree that it is neither free nor usable as a mapping.
pub fn mark_page_table(frame: usize) {
    set_frame_state(frame, FrameState::PageTable);
}

/// Put one frame into `state`, if it is inside the populated map.
pub fn set_frame_state(frame: usize, state: FrameState) {
    // SAFETY: as `reserve`: called by the boot processor, on frames the caller owns.
    unsafe {
        if frame < *FRAME_MAP_LEN.get() {
            (*FRAME_STATES.get())[frame] = state;
        }
    }
}

/// Overwrite a frame with zeros, through the direct map.
pub fn clear_frame(frame: usize) {
    let base = phys::direct(address_of(frame));
    // SAFETY: `frame` is allocated, so the whole page is mapped and owned by the caller; the
    // write is a plain memset of `PAGE_SIZE` bytes at an aligned address.
    unsafe {
        core::ptr::write_bytes(base as *mut u8, 0, PAGE_SIZE);
    }
}

/// How many mappings share each frame.
///
/// A mapping is a reference: `fork` gives two address spaces one frame, and the frame goes
/// back to the allocator only when the last mapping of it is gone. The count covers the
/// frames the allocator manages -- everything above that line is reserved memory that is
/// never shared.
static FRAME_REFS: SyncCell<[u16; MAX_MANAGED_FRAMES as usize]> =
    SyncCell::new([0; MAX_MANAGED_FRAMES as usize]);

/// Which frames are mapped read-only in more than one place, so a write fault must copy.
///
/// One bit per frame. A frame can be shared without being a copy-on-write candidate: two
/// address spaces mapping the same *shared* page never copy it. The bit is what tells the
/// fault handler which of the two it is looking at.
static FRAME_COW: SyncCell<[u64; MAX_MANAGED_FRAMES as usize / 64]> =
    SyncCell::new([0; MAX_MANAGED_FRAMES as usize / 64]);

/// Take a reference to `frame`.
///
/// Returns `false` for a frame outside the managed range, which cannot be shared because
/// nothing outside the range is ever mapped into an address space.
pub fn retain_frame(frame: usize) -> bool {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return false;
    }
    // SAFETY: single boot processor; the counts are only touched by mapping and unmapping
    // code, which does not run in interrupt context.
    unsafe {
        let count = &mut (*FRAME_REFS.get())[frame];
        *count = count.saturating_add(1);
    }
    true
}

/// Give up a reference to `frame`, returning what the count became.
pub fn release_frame(frame: usize) -> u16 {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return 0;
    }
    // SAFETY: as [`retain_frame`].
    unsafe {
        let count = &mut (*FRAME_REFS.get())[frame];
        *count = count.saturating_sub(1);
        *count
    }
}

/// How many mappings share `frame`.
#[must_use]
pub fn frame_refcount(frame: usize) -> u16 {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return 0;
    }
    // SAFETY: as [`retain_frame`]; a plain read.
    unsafe { (*FRAME_REFS.get())[frame] }
}

/// Record that `frame` is shared and must be copied on write.
pub fn mark_copy_on_write(frame: usize) {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return;
    }
    // SAFETY: as [`retain_frame`].
    unsafe {
        let word = &mut (*FRAME_COW.get())[frame / 64];
        *word |= 1u64 << (frame % 64);
    }
}

/// Stop treating `frame` as a copy-on-write candidate: it has exactly one owner now.
pub fn clear_copy_on_write(frame: usize) {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return;
    }
    // SAFETY: as [`retain_frame`].
    unsafe {
        let word = &mut (*FRAME_COW.get())[frame / 64];
        *word &= !(1u64 << (frame % 64));
    }
}

/// Whether `frame` is mapped read-only in more than one place.
#[must_use]
pub fn is_copy_on_write(frame: usize) -> bool {
    if frame >= MAX_MANAGED_FRAMES as usize {
        return false;
    }
    // SAFETY: as [`retain_frame`]; a plain read.
    unsafe { (*FRAME_COW.get())[frame / 64] & (1u64 << (frame % 64)) != 0 }
}

/// Mark one frame allocated, if it is inside the populated map.
fn mark_allocated(frame: usize) {
    // SAFETY: as `reserve`. The frame came from the allocator, so it is inside the map.
    unsafe {
        if frame < *FRAME_MAP_LEN.get() {
            (*FRAME_STATES.get())[frame] = FrameState::Allocated;
        }
    }
}

/// Verify the allocator, the frame map, and the caches against each other.
///
/// The three are built from different information -- the allocator from the links it wrote
/// into free frames, the frame map from the firmware's description, the magazines from what
/// has been allocated lately -- so agreement between them is evidence rather than a
/// restatement. The one deliberate asymmetry is the cache: a frame sitting in the boot
/// processor's magazine is allocated as far as the allocator is concerned and free as far as
/// the map is concerned, which is why the cached count is added back here.
pub fn audit() -> Result<(), &'static str> {
    let store = DirectMapStore::new();
    // SAFETY: single boot processor; the allocator is only touched from it.
    unsafe {
        (*ALLOCATOR.get()).audit(&store)?;
    }
    let free_in_map = count_frames(FrameState::Free) as u64;
    let free_in_allocator = allocator_stats().free_frames + cached_frames();
    if free_in_map != free_in_allocator {
        return Err("the frame map and the buddy allocator disagree about free frames");
    }
    Ok(())
}

/// The number of frames the frame map describes.
#[must_use]
pub fn frame_map_len() -> usize {
    // SAFETY: `init` writes this once, before any reader exists.
    unsafe { *FRAME_MAP_LEN.get() }
}

/// Frames the allocator manages, free or not.
#[must_use]
pub fn managed_frames() -> u64 {
    allocator_stats().managed_frames
}

/// Frames available to allocate, counting the ones in the boot processor's magazine.
#[must_use]
pub fn free_frames() -> u64 {
    allocator_stats().free_frames + cached_frames()
}

/// Frames sitting in the boot processor's magazine.
#[must_use]
pub fn cached_frames() -> u64 {
    // SAFETY: single boot processor; the cache is only touched from it.
    unsafe { (*FRAME_CACHE.get()).cached() }
}

/// The allocator's counters.
#[must_use]
pub fn allocator_stats() -> BuddyStats {
    // SAFETY: single boot processor; the allocator is only touched from it.
    unsafe { (*ALLOCATOR.get()).stats() }
}

/// The frame cache's counters.
#[must_use]
pub fn cache_stats() -> FrameCacheStats {
    // SAFETY: as `cached_frames`.
    unsafe { (*FRAME_CACHE.get()).stats() }
}

/// The state of one frame, by frame number.
///
/// Returns [`FrameState::Reserved`] for a frame number outside the populated map, which is
/// the safe answer: an out-of-range frame must never look allocatable.
#[must_use]
pub fn frame_state(frame: usize) -> FrameState {
    // SAFETY: reading one byte of a static that only this module writes, after `init` has
    // run. Reads cannot tear on a `u8`-backed enum.
    unsafe {
        if frame >= *FRAME_MAP_LEN.get() {
            FrameState::Reserved
        } else {
            (*FRAME_STATES.get())[frame]
        }
    }
}

/// Count the frames in a given state.
///
/// Linear over the map, so it is a diagnostic, not a hot path. The allocator keeps its own
/// counters.
#[must_use]
pub fn count_frames(state: FrameState) -> usize {
    // SAFETY: as [`frame_state`].
    unsafe {
        (*FRAME_STATES.get())
            .iter()
            .take(*FRAME_MAP_LEN.get())
            .filter(|s| **s == state)
            .count()
    }
}

/// Convert a physical address to a frame number.
#[must_use]
pub const fn frame_of(address: u64) -> usize {
    (address as usize) >> PAGE_SHIFT
}

/// Convert a frame number to the address of its first byte.
#[must_use]
pub const fn address_of(frame: usize) -> u64 {
    (frame as u64) << PAGE_SHIFT
}

/// Round an address up to a page boundary.
#[must_use]
pub const fn page_align_up(address: u64) -> u64 {
    (address + (PAGE_SIZE as u64 - 1)) & !(PAGE_SIZE as u64 - 1)
}

/// Round an address down to a page boundary.
#[must_use]
pub const fn page_align_down(address: u64) -> u64 {
    address & !(PAGE_SIZE as u64 - 1)
}
