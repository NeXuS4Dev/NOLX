//! The kernel heap: a `GlobalAlloc` over regions taken from the frame allocator.
//!
//! Until this module existed the kernel had no way to allocate anything smaller than a frame,
//! which is why the object manager, the namespace, and the process table were still fixed
//! arrays. The heap is what makes the rest of the executive possible: a kernel object is
//! allocated, charged, and freed, and its dictionary, its name, or its token lives on the heap
//! rather than in a table carved out at boot.
//!
//! # Why the heap is a list of regions
//!
//! `nolx_mm_core::heap` is a boundary-tagged allocator over **one contiguous region**: block
//! headers and footers live in the region itself, so a free block can be found, split, and
//! merged without any metadata of its own. That is the right shape for a kernel heap, but a
//! kernel cannot hand out one large contiguous run of physical memory at boot -- the frames it
//! can spare are scattered. So the region is the unit of growth: the heap holds a short list of
//! regions, each one a run of frames from the buddy allocator, and a request that no existing
//! region can serve adds another.
//!
//! Regions are reached through the **direct map**. Every frame the allocator manages lies below
//! `mm::phys::DIRECT_MAP_BYTES`, so the heap needs no window of its own and no page tables of
//! its own: a frame's kernel address is `direct(address_of(frame))`. That also means heap
//! memory can never be paged out, which is why every region is charged to the nonpaged pool.
//!
//! # Sub-frame alignment
//!
//! A boundary heap's payloads are 16-byte aligned, which covers every type whose alignment is
//! at most 16 -- all of them except `repr(align(N))` wrappers and page-granular buffers. A
//! request for more than that is served by over-allocating by the alignment and recording, in
//! the words in front of the returned pointer, which region and offset it came from. [`serve`]
//! is the only place that knows this and [`deallocate_bytes`] the only place that undoes it.
//!
//! # Locking
//!
//! One lock, held for the duration of an allocation, with interrupts disabled while it is held.
//! A timer tick or a page fault that allocated memory while the heap lock was held would
//! deadlock on a single processor; running the heap's critical sections with IF clear makes
//! that impossible. This is the discipline a queued spinlock at `DISPATCH` level will implement
//! in M5 -- until then it is the simplest correct thing.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::fmt::Write;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::commit::PoolKind;
use nolx_mm_core::heap::{BoundaryHeap, HeapError, ALIGNMENT};

use crate::hal::console::{raw_put_str, Console};
use crate::mm::{self, phys, PAGE_SIZE};
use crate::sync::{IrqLock, SyncCell};

/// How many regions the heap can hold before it stops growing.
///
/// Sixteen regions of up to 4 MiB is 64 MiB of heap; a kernel that needs more has either leaked
/// or needs a better story for paged pool. When the list is full, a request that does not fit
/// fails, and the failure is counted and reported rather than hidden.
pub const MAX_REGIONS: usize = 16;

/// Order of the first region: 64 frames, 256 KiB.
const FIRST_REGION_ORDER: usize = 6;

/// Order of the largest region the heap takes in one piece: 1024 frames, 4 MiB.
const LAST_REGION_ORDER: usize = 10;

/// Bytes of bookkeeping in front of an over-aligned payload: the region index and the offset
/// the block was allocated from.
const ALIGNED_HEADER: usize = 8;

/// One region of the heap.
struct Region {
    /// The boundary heap over the region's frames. Every access is serialised by [`LOCK`].
    heap: UnsafeCell<BoundaryHeap<'static>>,
    /// The region's length in bytes, which is also what it charges the nonpaged pool.
    bytes: usize,
    /// First physical byte of the region, for the direct-map translation.
    physical: u64,
}

/// The heap's regions, in the order they were added.
static REGIONS: SyncCell<[Option<Region>; MAX_REGIONS]> =
    SyncCell::new([const { None }; MAX_REGIONS]);

/// The heap's lock, held across every mutation of a region with interrupts off.
static LOCK: IrqLock = IrqLock::new();

/// Successful allocations since boot.
static ALLOCATIONS: SyncCell<u64> = SyncCell::new(0);
/// Requests the heap could not serve.
static FAILURES: SyncCell<u64> = SyncCell::new(0);
/// Regions added since boot, including the first.
static GROWTHS: SyncCell<u64> = SyncCell::new(0);
/// Bytes of region added since boot.
static BYTES_MAPPED: SyncCell<u64> = SyncCell::new(0);
/// High-water mark of bytes in use.
static PEAK_IN_USE: SyncCell<u64> = SyncCell::new(0);

/// The global allocator the Rust runtime uses.
///
/// A zero-sized type: the state is in the statics above, which is what lets it be a
/// `#[global_allocator]` without a constructor.
pub struct KernelHeap;

#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap;

// SAFETY: `KernelHeap` has no fields and keeps no state of its own; every allocation goes
// through the module-level [`LOCK`], an interrupt-safe spin lock held for the whole operation,
// so neither another processor nor an interrupt can be inside the heap at the same time.
unsafe impl Sync for KernelHeap {}

// SAFETY: `alloc` and `dealloc` are the [`allocate_bytes`] and [`deallocate_bytes`] pair, which
// together promise what `GlobalAlloc` requires: a pointer returned by one is served from a
// region the heap owns, is aligned to the requested `Layout`, and is returned to the same
// region by the other. A request the heap cannot serve -- including one whose growth fails --
// returns null rather than a non-null pointer to memory nobody owns.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocate_bytes(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        deallocate_bytes(ptr, layout.align());
    }
}

/// Everything the heap has counted, for the boot log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStatus {
    /// Regions in the list.
    pub regions: usize,
    /// Bytes of region mapped: the sum of the regions' lengths.
    pub bytes_mapped: u64,
    /// Usable bytes inside allocated blocks.
    pub bytes_in_use: u64,
    /// Usable bytes inside free blocks.
    pub bytes_free: u64,
    /// The largest single allocation the heap could still serve.
    pub largest_free: u64,
    /// Successful allocations.
    pub allocations: u64,
    /// Requests that could not be served.
    pub failures: u64,
    /// Regions added, including the first.
    pub growths: u64,
    /// High-water mark of [`HeapStatus::bytes_in_use`].
    pub peak_in_use: u64,
}

/// Create the first region and report what the heap looks like.
///
/// Called from [`mm::init`] once the frame allocator is up. Growing a region needs the
/// allocator, so this is the earliest point at which a heap can exist at all; the allocator
/// itself never allocates from the heap.
pub fn init(console: &mut dyn HalConsole) {
    if !grow() {
        // The kernel can still boot without a heap -- nothing in the boot path allocates from
        // it yet -- so this is reported and the failure surfaces when something asks for
        // memory, as a request that returns null.
        let _ = writeln!(
            console,
            "mm: WARNING the kernel heap could not take its first region; allocations will fail"
        );
        return;
    }
    let status = status();
    let _ = writeln!(
        console,
        "mm: kernel heap: {} region(s), {} KiB from the frame allocator, {} KiB charged to the \
         nonpaged pool",
        status.regions,
        status.bytes_mapped / 1024,
        status.bytes_mapped / 1024,
    );
    let _ = writeln!(console, "NOLX: kernel heap online");
}

/// The heap's counters, taken under the lock so they cannot be read half-updated.
#[must_use]
pub fn status() -> HeapStatus {
    let _guard = LOCK.lock();
    status_locked()
}

/// Verify every region's own metadata.
///
/// The boundary heap detects a corrupted header, footer, or free list instead of handing out
/// overlapping blocks; this is the boot-time caller of that check.
pub fn check() -> Result<(), &'static str> {
    let _guard = LOCK.lock();
    // SAFETY: the lock is held for the whole check, so no region can be changed while its
    // metadata is verified.
    unsafe {
        for index in 0..region_count() {
            let heap = region_heap(index);
            heap.check().map_err(check_message)?;
            heap.check_stats().map_err(check_message)?;
        }
    }
    Ok(())
}

/// The message for a failed region check. It names *what* the heap found rather than that
/// something is wrong, because the answer is printed and the machine stops.
fn check_message(error: HeapError) -> &'static str {
    match error {
        HeapError::Corrupt(what) => what,
        HeapError::RegionTooSmall => "a heap region is smaller than one block",
    }
}

/// Number of populated region slots. Caller holds [`LOCK`].
///
/// # Safety
///
/// The caller must hold [`LOCK`]; the walk assumes the list cannot change under it.
unsafe fn region_count() -> usize {
    // SAFETY: the caller holds the heap lock.
    unsafe {
        let mut count = 0;
        while count < MAX_REGIONS && (*REGIONS.get())[count].is_some() {
            count += 1;
        }
        count
    }
}

/// The boundary heap over region `index`. Caller holds [`LOCK`].
///
/// # Safety
///
/// The caller must hold [`LOCK`] and pass an index below [`region_count`].
unsafe fn region_heap(index: usize) -> &'static mut BoundaryHeap<'static> {
    // SAFETY: the slot is populated, and the lock makes this the only live reference to the
    // heap.
    unsafe {
        match (*REGIONS.get())[index].as_ref() {
            Some(region) => &mut *region.heap.get(),
            // The caller promised the index is populated; leaving this unreachable rather
            // than panicking keeps the allocator's fast path free of a branch that cannot
            // be taken.
            None => core::hint::unreachable_unchecked(),
        }
    }
}

/// The kernel's address of region `index`'s first byte, through the direct map.
///
/// # Safety
///
/// The caller must hold [`LOCK`].
unsafe fn region_base(index: usize) -> Option<*mut u8> {
    // SAFETY: the caller holds the lock; a live region's frames came from the allocator, which
    // only manages the first 2 GiB, so they are inside the direct map by construction.
    unsafe {
        (*REGIONS.get())[index]
            .as_ref()
            .map(|region| phys::direct(region.physical) as *mut u8)
    }
}

/// Allocate `size` bytes aligned to `align`, growing the heap if no region can serve it.
///
/// Returning null means the heap is out of regions or the frame allocator is out of frames:
/// a condition the kernel reports and handles, not a reason to stop the machine.
fn allocate_bytes(size: usize, align: usize) -> *mut u8 {
    if size == 0 {
        // A zero-sized allocation never touches memory. An aligned, non-null dangling pointer
        // is what the contract allows for one, and it keeps `deallocate_bytes` honest (which
        // ignores anything below the first page).
        return align.max(1) as *mut u8;
    }
    let _guard = LOCK.lock();
    let request = if align <= ALIGNMENT as usize {
        size
    } else {
        // Room for the payload, the round-up to `align`, and the two header words that
        // `serve` writes in front of the aligned start. The header has to be part of the
        // reservation: without it the caller's last `ALIGNED_HEADER` bytes land on the next
        // block's header, and the damage only shows up in a later free-list walk -- which is
        // exactly how the exhaustion fixture found it.
        size + align + ALIGNED_HEADER
    };

    for _ in 0..=MAX_REGIONS {
        if let Some(pointer) = serve(request, align) {
            // SAFETY: the lock is held, so these counters have no other writer.
            unsafe {
                *ALLOCATIONS.get() += 1;
            }
            record_high_water();
            return pointer;
        }
        if !grow_locked() {
            break;
        }
    }

    // SAFETY: the lock is held; see above.
    unsafe {
        *FAILURES.get() += 1;
    }
    core::ptr::null_mut()
}

/// Try to serve a request of `request` bytes from an existing region, returning the pointer the
/// caller receives.
///
/// Caller holds [`LOCK`].
fn serve(request: usize, align: usize) -> Option<*mut u8> {
    // SAFETY: the caller holds the lock, so the region list and every region's free lists are
    // stable for the duration of this search.
    unsafe {
        for index in 0..region_count() {
            // A request larger than the region itself can never be served from it, whatever
            // the region's own bookkeeping says. Checking the length first means a damaged
            // free-list entry cannot make an impossible request look servable, and the
            // refusals a caller sees stay honest.
            let region_bytes = (*REGIONS.get())[index]
                .as_ref()
                .map_or(0, |region| region.bytes);
            if request > region_bytes {
                continue;
            }
            let heap = region_heap(index);
            let Some(offset) = heap.allocate(request) else {
                continue;
            };
            let Some(base) = region_base(index) else {
                continue;
            };
            let payload = base.add(offset as usize);
            if align <= ALIGNMENT as usize {
                return Some(payload);
            }
            // The caller's pointer must be `align`-aligned, and the two header words must sit
            // immediately in front of it: `deallocate_bytes` finds the region and the block by
            // reading them at `pointer - ALIGNED_HEADER`. Writing them *at* the returned
            // pointer instead puts them inside the caller's own payload, where the caller's
            // first write clobbers them -- the block then cannot be freed, and the region
            // index read back is whatever the payload happened to contain.
            //
            // `request` includes the payload, a full `align` of round-up slack, and
            // `ALIGNED_HEADER` for these words, so the aligned pointer and everything the
            // caller writes after it stay inside the block.
            let aligned = (payload as usize + ALIGNED_HEADER).next_multiple_of(align);
            let header = (aligned - ALIGNED_HEADER) as *mut u32;
            header.write(index as u32);
            header.add(1).write(offset);
            return Some(aligned as *mut u8);
        }
    }
    None
}

/// Give a block back to the region it came from.
fn deallocate_bytes(pointer: *mut u8, align: usize) {
    if pointer.is_null() || (pointer as usize) < PAGE_SIZE {
        // A zero-sized layout's dangling pointer, or a null the caller should not have passed.
        // Neither owns a block, and both are cheap to recognise here.
        return;
    }
    let _guard = LOCK.lock();
    // SAFETY: the lock is held for the whole deallocation, so the region list and the region's
    // free lists have no other reader or writer.
    unsafe {
        if align > ALIGNMENT as usize {
            let header = pointer.sub(ALIGNED_HEADER) as *const u32;
            let index = header.read() as usize;
            let offset = header.add(1).read();
            if index < MAX_REGIONS && (*REGIONS.get())[index].is_some() {
                release(index, offset);
            }
            return;
        }
        // Region bases are not ordered: the allocator hands out whatever run is available, so
        // a later region can sit below an earlier one. A pointer that is not inside this
        // region is skipped by the subtraction's own answer rather than by an ordering
        // assumption -- and `checked_sub` is what keeps that skip from being an overflow.
        for index in 0..region_count() {
            let Some(base) = region_base(index) else {
                continue;
            };
            let Some(offset) = (pointer as usize).checked_sub(base as usize) else {
                continue;
            };
            let bytes = (*REGIONS.get())[index]
                .as_ref()
                .map_or(0, |region| region.bytes);
            if offset < bytes {
                release(index, offset as u32);
                return;
            }
        }
        debug_assert!(false, "freed a pointer that no heap region contains");
    }
}

/// Hand `offset` back to region `index`, reporting a heap-integrity failure as a bug.
///
/// # Safety
///
/// The caller must hold [`LOCK`] and pass a populated region index.
unsafe fn release(index: usize, offset: u32) {
    // SAFETY: as documented.
    let heap = unsafe { region_heap(index) };
    match heap.deallocate(offset) {
        Ok(()) => {}
        Err(HeapError::Corrupt(what)) => {
            // A free that corrupts the heap is a bug in the kernel, not a condition to
            // handle: every later allocation could hand out memory that is still in use.
            // Report it on the raw console, which cannot allocate, and stop.
            raw_put_str("HEAP CORRUPTION: ");
            raw_put_str(what);
            raw_put_str("\n");
            panic!("kernel heap: {what}");
        }
        Err(error) => {
            let _ = writeln!(Console::new(), "heap: rejected a free: {error:?}");
        }
    }
}

/// Add one region to the heap, charging it to the nonpaged pool.
///
/// Region sizes grow with the number of regions -- 256 KiB, then 512 KiB, up to 4 MiB -- because
/// the first region should be cheap on a small machine, and a kernel that has outgrown its
/// first region has already said it is willing to spend real memory.
fn grow() -> bool {
    let _guard = LOCK.lock();
    grow_locked()
}

/// [`grow`] with the lock already held.
fn grow_locked() -> bool {
    // SAFETY: the lock is held for the whole of this function, so the list and the counters
    // below have no other writer.
    unsafe {
        let index = region_count();
        if index >= MAX_REGIONS {
            return false;
        }
        let order = (FIRST_REGION_ORDER + index).min(LAST_REGION_ORDER);
        // A contiguous run: a region is one boundary heap, and its metadata addresses the
        // whole of it as a single slice.
        let Some(frame) = mm::allocate_frames(order) else {
            return false;
        };
        let bytes = (1usize << order) * PAGE_SIZE;
        if mm::pool::charge(PoolKind::NonPaged, bytes as u64).is_err() {
            // Take-back: a region the pool refused is a region the heap must not keep, or the
            // accounting and the machine disagree about what is in use.
            let _ = mm::free_block(frame, order);
            return false;
        }
        let base = phys::direct(mm::address_of(frame)) as *mut u8;
        // SAFETY: the frames belong to the heap from here on -- allocated, never freed, and
        // reachable only through this region. The slice is the region's whole extent, page
        // aligned, and inside the direct map's coverage.
        let region = core::slice::from_raw_parts_mut(base, bytes);
        let Ok(heap) = BoundaryHeap::new(region) else {
            // Not reachable with the sizes above, but a region that cannot hold one block must
            // not be kept: give the frames and the charge back and report the failure.
            let _ = mm::pool::release(PoolKind::NonPaged, bytes as u64);
            let _ = mm::free_block(frame, order);
            return false;
        };
        (*REGIONS.get())[index] = Some(Region {
            heap: UnsafeCell::new(heap),
            bytes,
            physical: mm::address_of(frame),
        });
        *BYTES_MAPPED.get() += bytes as u64;
        *GROWTHS.get() += 1;
    }
    true
}

/// Update the in-use high-water mark after a successful allocation.
///
/// The walk touches [`MAX_REGIONS`] regions of already-cached metadata and runs only once an
/// allocation has succeeded, which is what makes it affordable on the allocation path.
fn record_high_water() {
    let status = status_locked();
    // SAFETY: every caller holds the lock, so the peak has no other writer.
    unsafe {
        if status.bytes_in_use > *PEAK_IN_USE.get() {
            *PEAK_IN_USE.get() = status.bytes_in_use;
        }
    }
}

/// [`status`] without taking the lock, for callers that already hold it.
fn status_locked() -> HeapStatus {
    // SAFETY: callers hold the lock, which excludes every other reader and writer of the
    // region list and the counters.
    unsafe {
        let count = region_count();
        let mut bytes_in_use = 0;
        let mut bytes_free = 0;
        let mut largest_free = 0;
        for index in 0..count {
            let heap = region_heap(index);
            let stats = heap.stats();
            bytes_in_use += stats.in_use;
            bytes_free += stats.free;
            largest_free = largest_free.max(heap.largest_free());
        }
        HeapStatus {
            regions: count,
            bytes_mapped: *BYTES_MAPPED.get(),
            bytes_in_use,
            bytes_free,
            largest_free,
            allocations: *ALLOCATIONS.get(),
            failures: *FAILURES.get(),
            growths: *GROWTHS.get(),
            peak_in_use: *PEAK_IN_USE.get(),
        }
    }
}

/// Run the heap's boot self test and report what it did.
///
/// The heap is exercised through the *language* allocator -- `Vec`, `BTreeMap` -- and not
/// through its own entry points, because that is what the rest of the kernel will use, and
/// because it makes the test cover the parts a direct call would skip: `Layout` alignment, the
/// trait's reallocation path, and the drop glue that frees blocks in the order the compiler
/// chose.
///
/// The test proves four things:
///
/// * a vector of a few thousand values keeps its contents while reallocating, which is the
///   alloc-copy-free path;
/// * a `BTreeMap` over a thousand entries answers lookups correctly, which exercises many
///   small allocations across the size-class bins and their frees;
/// * a request larger than every existing region forces growth, and the grown region serves it;
/// * every region's metadata still checks out afterwards, and the in-use bytes come back to
///   where they started -- the heap's own answer to "did this leak?".
pub fn selftest(console: &mut dyn HalConsole) -> Result<(), &'static str> {
    let before = status();

    let mut numbers: Vec<u64> = Vec::new();
    for value in 0..4096u64 {
        numbers.push(value.wrapping_mul(2_654_435_761));
    }
    for (index, value) in numbers.iter().enumerate() {
        if *value != (index as u64).wrapping_mul(2_654_435_761) {
            return Err("the kernel heap lost a value in a grown vector");
        }
    }

    let mut map: BTreeMap<u64, u64> = BTreeMap::new();
    for key in 0..1024u64 {
        map.insert(key, key.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    }
    for key in (0..1024u64).step_by(7) {
        if map.get(&key) != Some(&key.wrapping_mul(0x9E37_79B9_7F4A_7C15)) {
            return Err("the kernel heap lost a value in a tree map");
        }
    }
    for key in (0..1024u64).step_by(3) {
        map.remove(&key);
    }

    // Larger than any region the heap has taken so far: this is the path that must grow.
    let mut large: Vec<u8> = Vec::new();
    large.resize(512 * 1024, 0xA5);
    if large.len() != 512 * 1024 || !large.iter().all(|byte| *byte == 0xA5) {
        return Err("an allocation larger than every existing region did not survive");
    }
    drop(large);

    let grown = status();
    drop(map);
    drop(numbers);

    check()?;
    let after = status();
    if after.bytes_in_use != before.bytes_in_use {
        return Err("the kernel heap did not return to its starting size after the test");
    }
    if grown.growths <= before.growths {
        return Err("the heap served an oversized request without growing");
    }
    if after.allocations <= before.allocations {
        return Err("the heap counted no allocations during its own self test");
    }

    let _ = writeln!(
        console,
        "NOLX: kernel heap selftest: {} allocations, +{} region(s) to {} KiB, largest free {} \
         bytes, in use {} -> {} bytes, integrity ok",
        after.allocations - before.allocations,
        after.growths - before.growths,
        after.bytes_mapped / 1024,
        after.largest_free,
        before.bytes_in_use,
        after.bytes_in_use,
    );
    Ok(())
}
