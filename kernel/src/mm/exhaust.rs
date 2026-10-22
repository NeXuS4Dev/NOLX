//! Boot-time allocator-exhaustion fixture.
//!
//! A kernel that only ever sees allocators with room to spare has not tested them. The two
//! failures that matter here are the ones a host test cannot show:
//!
//! * **the frame allocator** must refuse the next allocation instead of panicking, returning a
//!   frame it already gave away, or corrupting its own free lists while empty; and it must give
//!   every frame back when the pressure ends;
//! * **the kernel heap** must refuse a request it cannot serve -- including a request larger
//!   than any region it is allowed to take -- by returning null, not by aborting or by handing
//!   out a block that overlaps a live one, and the memory it did serve must come back exactly.
//!
//! The fixture runs only under `hal-mm-exhaust-test`. It leaves the machine as it found it: the
//! free-frame count is restored to the exact value it started with, the heap's bytes-in-use
//! return to their starting value, and both audits run again afterwards.
//!
//! # Why the frames carry the list
//!
//! The natural way to remember a hundred thousand frames is a vector -- which would allocate
//! from the very heap and frame allocator the fixture is exhausting, and fail with an
//! allocation error in the middle of the test. Instead the fixture writes the link into the
//! first word of each frame it takes, through the direct map, so the free-frame list lives in
//! the frames themselves and no bookkeeping allocates anything.

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nolx_hal_core::console::HalConsole;

use super::{heap, phys, FrameState, PAGE_SIZE};
use crate::mm;

/// The size of one heap block the fixture allocates: 1 MiB, so the heap's 4 MiB regions are
/// each asked to serve four of them and the 16-region ceiling is reached after roughly 60
/// blocks rather than after 16.
const HEAP_BLOCK: usize = 1024 * 1024;

/// How many heap blocks the fixture is willing to hold at once. More than four regions' worth:
/// enough to reach the region ceiling on any plausible region layout, and a bound that turns
/// "the heap never refused" into a failure instead of a hang.
const MAX_BLOCKS: usize = 96;

/// A request no region may serve: larger than the largest region the heap takes in one piece
/// (`LAST_REGION_ORDER`, 4 MiB).
const IMPOSSIBLE: usize = 8 * 1024 * 1024;

/// End of the frame chain, written into the last frame the fixture takes.
const TERMINATOR: usize = usize::MAX;

static DONE: AtomicBool = AtomicBool::new(false);

/// The first frame of the chain: the last one taken, whose own first word links to the next.
static HEAD: AtomicUsize = AtomicUsize::new(TERMINATOR);

/// Run the fixture once per boot.
pub fn run(console: &mut dyn HalConsole) {
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let free_before = mm::free_frames();
    let managed = mm::managed_frames();
    let cached_before = mm::cached_frames();
    let heap_before = heap::status();
    let _ = writeln!(
        console,
        "mm-exhaust: starting with {free_before} free of {managed} managed frames \
         ({cached_before} cached), heap {} region(s), {} bytes in use",
        heap_before.regions, heap_before.bytes_in_use
    );

    let taken = exhaust_frames(console);
    release_frames(taken);

    let free_after = mm::free_frames();
    let cached_after = mm::cached_frames();
    let _ = writeln!(
        console,
        "mm-exhaust: {taken} frame(s) returned, free frames {free_before} -> {free_after}, \
         cached {cached_before} -> {cached_after}"
    );
    assert_eq!(
        free_after, free_before,
        "the frame allocator did not return to its starting free count after exhaustion"
    );
    let free_in_map = mm::count_frames(FrameState::Free) as u64;
    assert_eq!(
        free_in_map, free_before,
        "the frame map and the allocator disagree about free frames after exhaustion"
    );
    mm::audit().expect("the memory manager audit failed after frame exhaustion");

    let blocks = exhaust_heap(console);
    heap::check().expect("the heap's own metadata check failed after exhaustion");
    mm::audit().expect("the memory manager audit failed after heap exhaustion");

    let after = heap::status();
    let _ = writeln!(
        console,
        "mm-exhaust: PASS {taken} frame(s) exhausted and returned, free frames exact at \
         {free_before}; heap refused after {blocks} block(s) of {} KiB with {} region(s), in use \
         {} -> {} bytes, audit ok",
        HEAP_BLOCK / 1024,
        after.regions,
        heap_before.bytes_in_use,
        after.bytes_in_use,
    );
}

/// Take every free frame, chaining the list through the frames themselves.
///
/// Returns how many frames were taken. The allocator must reach empty and refuse cleanly; a
/// frame count that stops short of the free count the map reported is a failure too, which is
/// what the empty-count assertion below catches.
fn exhaust_frames(console: &mut dyn HalConsole) -> u64 {
    let mut head: usize = TERMINATOR;
    let mut taken: u64 = 0;
    while let Some(frame) = mm::allocate_frame() {
        let slot = phys::direct((frame * PAGE_SIZE) as u64) as *mut usize;
        // SAFETY: the frame allocator just handed this frame over, so nothing else in the
        // kernel refers to it and the direct map has a writable view of it. Only the first
        // word is written, to chain the frames together; the frame is handed back before this
        // function returns and nothing reads the word afterwards.
        unsafe { slot.write_volatile(head) };
        head = frame;
        taken += 1;
        assert!(
            taken <= mm::managed_frames(),
            "the frame allocator handed out more frames than it manages"
        );
    }
    HEAD.store(head, Ordering::Release);
    assert_eq!(
        mm::free_frames(),
        0,
        "the allocator refused a frame while its free count was still nonzero"
    );
    assert!(
        mm::allocate_frames(1).is_none(),
        "an order-1 block was served from an empty allocator"
    );
    // A frame the fixture never took is not the allocator's to free: the refusal must be an
    // error, not a silent second owner of the kernel's first frame.
    assert!(
        mm::free_frame(0).is_err(),
        "freeing a reserved frame was accepted"
    );
    let _ = writeln!(
        console,
        "mm-exhaust: {taken} frame(s) taken to exhaustion, order-1 request and reserved-frame \
         free both refused"
    );
    taken
}

/// Walk the chain and give every frame back in the exact reverse order it was taken.
fn release_frames(taken: u64) {
    let mut head = HEAD.load(Ordering::Acquire);
    let mut released: u64 = 0;
    while head != TERMINATOR {
        let frame = head;
        let slot = phys::direct((frame * PAGE_SIZE) as u64) as *mut usize;
        // SAFETY: as in `exhaust_frames`: the frame is owned by the fixture until it is freed
        // here, and the chain word was written before the frame was linked in.
        let next = unsafe { slot.read_volatile() };
        match mm::free_frame(frame) {
            Ok(()) => {}
            Err(error) => panic!("the allocator refused a frame it handed out: {error:?}"),
        }
        released += 1;
        head = next;
    }
    HEAD.store(TERMINATOR, Ordering::Release);
    assert_eq!(
        released, taken,
        "the fixture released a different number of frames than it took"
    );
}

/// Allocate 1 MiB blocks until the heap refuses, prove the refusal is honest, and give the
/// blocks back.
///
/// The heap's regions are capped, so this reaches a real ceiling: every region has been added
/// and no free block can serve another MiB. What matters is that the refusal is a null pointer,
/// that an over-sized request is refused too, that the accounting stays consistent, and that
/// bytes-in-use come back to exactly the value the fixture found.
fn exhaust_heap(console: &mut dyn HalConsole) -> usize {
    const NULL: *mut u8 = core::ptr::null_mut();
    let before = heap::status();
    let layout = Layout::from_size_align(HEAP_BLOCK, PAGE_SIZE).expect("1 MiB, 4 KiB aligned");
    let mut blocks = [NULL; MAX_BLOCKS];
    let mut count = 0;
    while count < MAX_BLOCKS {
        // SAFETY: the layout has a nonzero size, so `alloc` may only return a fresh block or
        // null; every stored pointer is released with the same layout below.
        let pointer = unsafe { alloc::alloc::alloc(layout) };
        if pointer.is_null() {
            break;
        }
        // Fill the block: an allocator that handed out overlapping memory would be discovered
        // by the heap's own metadata check after the release, and a null refusal would not.
        // SAFETY: the heap returned this block for `layout`, so all `HEAP_BLOCK` bytes are
        // writable and belong to the fixture until the release below.
        unsafe { core::ptr::write_bytes(pointer, 0xa5, HEAP_BLOCK) };
        blocks[count] = pointer;
        count += 1;
    }
    assert!(
        count < MAX_BLOCKS,
        "the heap served {MAX_BLOCKS} x 1 MiB blocks without refusing; the region cap is not \
         being enforced"
    );
    assert!(
        count > 0,
        "the heap refused the very first 1 MiB block; it never grew past its first region"
    );

    // Every block must still hold the pattern the loop wrote. A heap that handed the same bytes
    // to two callers, or that wrote its own metadata into a live payload, fails here instead of
    // failing later in a free-list walk with no explanation.
    for (index, &pointer) in blocks[..count].iter().enumerate() {
        // SAFETY: the block is live and the loop wrote all `HEAP_BLOCK` bytes of it; the slice
        // is read-only and is dropped before the release below.
        let payload = unsafe { core::slice::from_raw_parts(pointer, HEAP_BLOCK) };
        assert!(
            payload.iter().all(|&byte| byte == 0xa5),
            "live heap block {index} was overwritten while the heap was full"
        );
    }

    // A request larger than the largest region the heap may take must be refused even though
    // live blocks are held: no region may ever be larger than the ceiling.
    let impossible = Layout::from_size_align(IMPOSSIBLE, PAGE_SIZE).expect("8 MiB layout");
    // SAFETY: nonzero size; the result must be null and is never dereferenced.
    let oversized = unsafe { alloc::alloc::alloc(impossible) };
    if !oversized.is_null() {
        let st = heap::status();
        panic!(
            "the heap served a request larger than any region it may take: {:p} after {} live \
             block(s), {} region(s), {} KiB mapped, largest free {}, failures {}",
            oversized,
            count,
            st.regions,
            st.bytes_mapped / 1024,
            st.largest_free,
            st.failures
        );
    }

    // A small request may still be served from a remaining free block. If it is, it must not
    // overlap the live blocks: writing it is what makes that check real.
    let small = Layout::from_size_align(64, 8).expect("64-byte layout");
    // SAFETY: nonzero size; a non-null result is written and released with the same layout.
    let tiny = unsafe { alloc::alloc::alloc(small) };
    if !tiny.is_null() {
        // SAFETY: the heap returned this block for `small`; 64 bytes are writable.
        unsafe { core::ptr::write_bytes(tiny, 0x5a, 64) };
        // SAFETY: released with the layout it was allocated with.
        unsafe { alloc::alloc::dealloc(tiny, small) };
    }

    let full = heap::status();
    let _ = writeln!(
        console,
        "mm-exhaust: heap served {count} block(s) of {} KiB across {} region(s), {} bytes in \
         use, then refused an 8 MiB request",
        HEAP_BLOCK / 1024,
        full.regions,
        full.bytes_in_use
    );

    for &pointer in &blocks[..count] {
        assert!(!pointer.is_null(), "a null block reached the release path");
        // SAFETY: every pointer in `blocks[..count]` came from `alloc::alloc::alloc(layout)`
        // above and is released exactly once with the same layout.
        unsafe { alloc::alloc::dealloc(pointer, layout) };
    }
    let after = heap::status();
    let _ = writeln!(
        console,
        "mm-exhaust: heap released {count} block(s), in use {} -> {} bytes (started at {}), \
         {} region(s) retained",
        full.bytes_in_use, after.bytes_in_use, before.bytes_in_use, after.regions
    );
    assert_eq!(
        after.bytes_in_use, before.bytes_in_use,
        "the heap's bytes-in-use did not return to the value the fixture found"
    );
    assert!(
        after.failures > before.failures,
        "the heap refused a request but did not count the failure"
    );
    count
}
