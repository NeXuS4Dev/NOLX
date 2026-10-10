//! The buddy allocator.
//!
//! Physical memory is managed in power-of-two blocks of 4 KiB frames. Order `n` describes a
//! block of `2^n` frames. Allocation takes the smallest block that fits and splits larger
//! ones down to it; freeing merges a block with its buddy whenever the buddy is free, so
//! fragmentation is bounded by the region's shape rather than by allocation history.
//!
//! # Where the bookkeeping lives
//!
//! The free lists are **intrusive**: each free block stores its own links in its first
//! bytes, which is what lets a kernel with no allocator yet manage memory. Reaching that
//! memory is the one thing this crate cannot do portably, so it goes through
//! [`BlockStore`]. The kernel implements it over the direct map; the tests implement it
//! over a map, and the algorithms -- which are the part that can be silently wrong -- are
//! the same code in both.
//!
//! [`BlockStore::free_order`] is the frame database's contribution: a frame reports the
//! order of the free block it heads, or `None` if it is not the head of a free block. It
//! is what makes coalescing `O(1)`, and it is also what makes a double free detectable
//! instead of silent corruption.

/// A physical frame number: the address of a 4 KiB frame divided by 4096.
pub type Frame = u64;

/// The largest order this allocator manages: 2^10 frames, or a 4 MiB block.
pub const MAX_ORDER: usize = 10;

/// The largest block, in bytes.
pub const MAX_BLOCK_BYTES: u64 = (4096u64) << MAX_ORDER;

/// Storage for the intrusive free-list links and the per-block order.
///
/// Every method is called only for frames that are already known to be free-block heads,
/// except [`BlockStore::free_order`], which is a *question about* a frame and must be
/// answerable for any frame in the managed range. An implementation answers `None` for a
/// frame that is allocated, that is an interior frame of a larger free block, or that the
/// allocator has never seen.
pub trait BlockStore {
    /// The next free block in the same order's list, or `None` at the end.
    #[must_use]
    fn next_free(&self, frame: Frame) -> Option<Frame>;

    /// Set the next link of a free block.
    fn set_next_free(&mut self, frame: Frame, next: Option<Frame>);

    /// The previous free block in the same order's list, or `None` at the head.
    #[must_use]
    fn previous_free(&self, frame: Frame) -> Option<Frame>;

    /// Set the previous link of a free block.
    fn set_previous_free(&mut self, frame: Frame, previous: Option<Frame>);

    /// The order of the free block whose first frame is `frame`, if there is one.
    #[must_use]
    fn free_order(&self, frame: Frame) -> Option<usize>;

    /// Record, or clear, the fact that `frame` heads a free block of `order`.
    fn set_free_order(&mut self, frame: Frame, order: Option<usize>);
}

/// Why a frame could not be returned to the allocator.
///
/// Every one of these is a caller bug, not a runtime condition: the kernel's frame map is
/// what decides whether a frame may be freed at all, and it rejects the cases this type
/// reports before they reach here. They stay as errors rather than assertions because a
/// kernel that panics on a bad free is a kernel that cannot report the bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The order was larger than [`MAX_ORDER`].
    OrderTooLarge,
    /// The frame is not aligned to the block it claims to be.
    NotAligned,
    /// The frame is outside every region that was added.
    NotManaged,
    /// The frame already heads a free block: this is a double free.
    AlreadyFree,
}

/// What the allocator has done, for the boot log and for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuddyStats {
    /// Frames handed to the allocator by [`Buddy::add_region`].
    pub managed_frames: u64,
    /// Frames currently free.
    pub free_frames: u64,
    /// Free blocks at each order; index `n` is order `n`.
    pub free_blocks: [u64; MAX_ORDER + 1],
    /// Successful allocations.
    pub allocations: u64,
    /// Successful frees.
    pub frees: u64,
    /// Blocks created by splitting a larger one.
    pub splits: u64,
    /// Blocks destroyed by merging with a buddy.
    pub merges: u64,
    /// Allocation attempts that found nothing, at any order.
    pub failures: u64,
}

impl BuddyStats {
    /// The largest order with a free block, or `None` when the allocator is exhausted.
    #[must_use]
    pub fn largest_free_order(&self) -> Option<usize> {
        (0..=MAX_ORDER).rev().find(|&o| self.free_blocks[o] > 0)
    }

    /// Free frames that could not be handed out as a single block of `order`.
    ///
    /// This is the honest way to describe fragmentation: not a percentage, but "if you
    /// needed a block this size right now, how much of the free memory is useless".
    #[must_use]
    pub fn free_frames_below_order(&self, order: usize) -> u64 {
        let mut total = 0;
        for o in 0..order.min(MAX_ORDER + 1) {
            total += self.free_blocks[o] << o;
        }
        total
    }
}

impl Default for BuddyStats {
    fn default() -> Self {
        Self {
            managed_frames: 0,
            free_frames: 0,
            free_blocks: [0; MAX_ORDER + 1],
            allocations: 0,
            frees: 0,
            splits: 0,
            merges: 0,
            failures: 0,
        }
    }
}

/// A buddy allocator over one or more physical regions.
pub struct Buddy {
    heads: [Option<Frame>; MAX_ORDER + 1],
    counts: [u64; MAX_ORDER + 1],
    managed: u64,
    free: u64,
    limit: Frame,
    allocations: u64,
    frees: u64,
    splits: u64,
    merges: u64,
    failures: u64,
}

impl Default for Buddy {
    fn default() -> Self {
        Self::new()
    }
}

impl Buddy {
    /// An allocator with nothing to manage.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            heads: [None; MAX_ORDER + 1],
            counts: [0; MAX_ORDER + 1],
            managed: 0,
            free: 0,
            limit: 0,
            allocations: 0,
            frees: 0,
            splits: 0,
            merges: 0,
            failures: 0,
        }
    }

    /// Hand the allocator the frame range `[first, end)`.
    ///
    /// The range is carved into the largest aligned blocks that fit. Regions must not
    /// overlap; the boot code adds the usable ranges from the firmware memory map, which
    /// cannot overlap by construction.
    ///
    /// Returns the number of frames added.
    pub fn add_region(&mut self, store: &mut impl BlockStore, first: Frame, end: Frame) -> u64 {
        if end <= first {
            return 0;
        }
        let mut frame = first;
        let mut added = 0;
        while frame < end {
            let mut order = MAX_ORDER;
            while order > 0 {
                let size = 1u64 << order;
                if frame.is_multiple_of(size) && frame + size <= end {
                    break;
                }
                order -= 1;
            }
            self.push_free(store, frame, order);
            frame += 1u64 << order;
            added += 1u64 << order;
        }
        self.managed += added;
        self.limit = self.limit.max(end);
        added
    }

    /// Allocate one block of `2^order` frames.
    ///
    /// Returns the first frame of the block, which is aligned to the block size, or `None`
    /// when no block that large is free.
    pub fn allocate(&mut self, store: &mut impl BlockStore, order: usize) -> Option<Frame> {
        if order > MAX_ORDER {
            self.failures += 1;
            return None;
        }

        let mut level = order;
        while level <= MAX_ORDER && self.heads[level].is_none() {
            level += 1;
        }
        if level > MAX_ORDER {
            self.failures += 1;
            return None;
        }

        let frame = self.pop_head(store, level);
        while level > order {
            level -= 1;
            // The upper half becomes the buddy; the lower half keeps going down the
            // orders. Which half is the buddy is a free choice, and taking the upper one
            // keeps the returned block's address stable down to the target order.
            self.push_free(store, frame + (1u64 << level), level);
            self.splits += 1;
        }

        self.allocations += 1;
        Some(frame)
    }

    /// Return a block of `2^order` frames starting at `frame`.
    ///
    /// The block is merged with its buddy as many times as the buddy is free.
    pub fn free(
        &mut self,
        store: &mut impl BlockStore,
        frame: Frame,
        order: usize,
    ) -> Result<(), FreeError> {
        if order > MAX_ORDER {
            return Err(FreeError::OrderTooLarge);
        }
        let size = 1u64 << order;
        if !frame.is_multiple_of(size) {
            return Err(FreeError::NotAligned);
        }
        if frame + size > self.limit {
            return Err(FreeError::NotManaged);
        }
        if store.free_order(frame).is_some() {
            return Err(FreeError::AlreadyFree);
        }

        let mut head = frame;
        let mut level = order;
        while level < MAX_ORDER {
            let buddy = head ^ (1u64 << level);
            if store.free_order(buddy) == Some(level) {
                self.remove(store, buddy, level);
                self.merges += 1;
                head = head.min(buddy);
                level += 1;
            } else {
                break;
            }
        }

        self.push_free(store, head, level);
        self.frees += 1;
        Ok(())
    }

    /// Frames currently free.
    #[must_use]
    pub const fn free_frames(&self) -> u64 {
        self.free
    }

    /// Frames the allocator manages, free or not.
    #[must_use]
    pub const fn managed_frames(&self) -> u64 {
        self.managed
    }

    /// Free blocks at `order`.
    #[must_use]
    pub fn free_blocks(&self, order: usize) -> u64 {
        self.counts.get(order).copied().unwrap_or(0)
    }

    /// Everything the allocator has counted.
    #[must_use]
    pub fn stats(&self) -> BuddyStats {
        BuddyStats {
            managed_frames: self.managed,
            free_frames: self.free,
            free_blocks: self.counts,
            allocations: self.allocations,
            frees: self.frees,
            splits: self.splits,
            merges: self.merges,
            failures: self.failures,
        }
    }

    /// Walk every free block, calling `visit(frame, order)`.
    ///
    /// The walk is bounded by the recorded counts, so a cycle introduced by a bug
    /// terminates and is reported by [`Buddy::audit`] rather than hanging the machine.
    pub fn visit_free_blocks(&self, store: &impl BlockStore, mut visit: impl FnMut(Frame, usize)) {
        for order in 0..=MAX_ORDER {
            let mut next = self.heads[order];
            let mut seen = 0;
            while let Some(frame) = next {
                if seen > self.counts[order] {
                    return;
                }
                visit(frame, order);
                seen += 1;
                next = store.next_free(frame);
            }
        }
    }

    /// Check the free lists against the allocator's own bookkeeping.
    ///
    /// This is what the kernel calls after boot-time bring-up to prove the allocator it is
    /// about to rely on is self-consistent. It verifies, for every free block: the frame is
    /// aligned to its order, it lies inside the managed range, the store agrees about its
    /// order, and the per-order list holds exactly the number of blocks the counters claim.
    /// It does not prove that no two blocks overlap -- that needs a bitmap over all frames,
    /// which is what the host tests do.
    pub fn audit(&self, store: &impl BlockStore) -> Result<(), &'static str> {
        if self.free > self.managed {
            return Err("free frames exceed managed frames");
        }
        let mut total = 0u64;
        for order in 0..=MAX_ORDER {
            let mut next = self.heads[order];
            let mut seen = 0u64;
            while let Some(frame) = next {
                if seen >= self.counts[order] {
                    return Err("free list is longer than its recorded length");
                }
                let size = 1u64 << order;
                if !frame.is_multiple_of(size) {
                    return Err("free block is not aligned to its order");
                }
                if frame + size > self.limit {
                    return Err("free block lies outside the managed range");
                }
                if store.free_order(frame) != Some(order) {
                    return Err("free list entry disagrees with the frame database");
                }
                if let Some(previous) = store.previous_free(frame) {
                    if store.next_free(previous) != Some(frame) {
                        return Err("free list is not doubly linked");
                    }
                }
                if let Some(next) = store.next_free(frame) {
                    if store.previous_free(next) != Some(frame) {
                        return Err("free list is not doubly linked");
                    }
                }
                seen += 1;
                total += size;
                next = store.next_free(frame);
            }
            if seen != self.counts[order] {
                return Err("free list is shorter than its recorded length");
            }
        }
        if total != self.free {
            return Err("free block size total disagrees with the free-frame count");
        }
        Ok(())
    }

    /// Push a block onto the head of its order's list.
    fn push_free(&mut self, store: &mut impl BlockStore, frame: Frame, order: usize) {
        let head = self.heads[order];
        store.set_free_order(frame, Some(order));
        store.set_previous_free(frame, None);
        store.set_next_free(frame, head);
        if let Some(head) = head {
            store.set_previous_free(head, Some(frame));
        }
        self.heads[order] = Some(frame);
        self.counts[order] += 1;
        self.free += 1u64 << order;
    }

    /// Remove a block from the middle of its order's list.
    fn remove(&mut self, store: &mut impl BlockStore, frame: Frame, order: usize) {
        let previous = store.previous_free(frame);
        let next = store.next_free(frame);
        match previous {
            Some(previous) => store.set_next_free(previous, next),
            None => self.heads[order] = next,
        }
        if let Some(next) = next {
            store.set_previous_free(next, previous);
        }
        store.set_free_order(frame, None);
        store.set_next_free(frame, None);
        store.set_previous_free(frame, None);
        self.counts[order] -= 1;
        self.free -= 1u64 << order;
    }

    /// Remove and return the head of an order's list.
    fn pop_head(&mut self, store: &mut impl BlockStore, order: usize) -> Frame {
        let frame = self.heads[order].expect("pop_head on an empty list");
        self.remove(store, frame, order);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;
    use std::collections::{BTreeMap, BTreeSet};

    /// One frame's links: the next block, the previous block, and the order it records.
    type Links = (Option<Frame>, Option<Frame>, Option<usize>);

    /// A `BlockStore` backed by a map, standing in for the kernel's direct map.
    #[derive(Default)]
    struct MapStore {
        links: BTreeMap<Frame, Links>,
    }

    impl MapStore {
        fn entry(&mut self, frame: Frame) -> &mut (Option<Frame>, Option<Frame>, Option<usize>) {
            self.links.entry(frame).or_insert((None, None, None))
        }
    }

    impl BlockStore for MapStore {
        fn next_free(&self, frame: Frame) -> Option<Frame> {
            self.links.get(&frame).and_then(|e| e.0)
        }

        fn set_next_free(&mut self, frame: Frame, next: Option<Frame>) {
            self.entry(frame).0 = next;
        }

        fn previous_free(&self, frame: Frame) -> Option<Frame> {
            self.links.get(&frame).and_then(|e| e.1)
        }

        fn set_previous_free(&mut self, frame: Frame, previous: Option<Frame>) {
            self.entry(frame).1 = previous;
        }

        fn free_order(&self, frame: Frame) -> Option<usize> {
            self.links.get(&frame).and_then(|e| e.2)
        }

        fn set_free_order(&mut self, frame: Frame, order: Option<usize>) {
            self.entry(frame).2 = order;
        }
    }

    /// The exhaustive check `Buddy::audit` cannot do: no two free blocks overlap.
    fn assert_no_overlap(buddy: &Buddy, store: &MapStore) {
        let mut seen = BTreeSet::new();
        let mut frames = 0u64;
        buddy.visit_free_blocks(store, |frame, order| {
            let size = 1u64 << order;
            for f in frame..frame + size {
                assert!(seen.insert(f), "frame {f} appears twice in the free lists");
                frames += 1;
            }
            assert_eq!(store.free_order(frame), Some(order));
            for f in frame + 1..frame + size {
                assert_eq!(store.free_order(f), None, "interior frame {f} is marked");
            }
        });
        assert_eq!(frames, buddy.free_frames());
    }

    #[test]
    fn a_single_aligned_region_becomes_one_large_block() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        // 1024 frames starting at 256: the carving takes the largest aligned blocks, so
        // the result depends on alignment, not on the length alone.
        let added = buddy.add_region(&mut store, 256, 256 + 1024);
        assert_eq!(added, 1024);
        assert_eq!(buddy.managed_frames(), 1024);
        assert_eq!(buddy.free_frames(), 1024);
        // 256 frames at frame 256 (order 8), 512 at frame 512 (order 9), 256 at 1024.
        assert_eq!(buddy.free_blocks(8), 2);
        assert_eq!(buddy.free_blocks(9), 1);
        assert_eq!(buddy.stats().largest_free_order(), Some(9));
        assert_eq!(buddy.audit(&store), Ok(()));
        assert_no_overlap(&buddy, &store);
    }

    #[test]
    fn an_unaligned_region_is_carved_into_aligned_blocks() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        // Frames 1..6: frame 1 alone, then {2,3} and {4,5} as order-1 blocks.
        buddy.add_region(&mut store, 1, 6);
        assert_eq!(buddy.free_frames(), 5);
        assert_eq!(buddy.free_blocks(0), 1);
        assert_eq!(buddy.free_blocks(1), 2);
        assert_no_overlap(&buddy, &store);
    }

    #[test]
    fn allocation_splits_and_the_block_is_aligned() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 64);

        let block = buddy.allocate(&mut store, 2).expect("a block");
        assert!(block.is_multiple_of(4));
        assert_eq!(buddy.free_frames(), 60);
        assert_eq!(buddy.stats().splits, 4);
        assert_eq!(buddy.audit(&store), Ok(()));
        assert_no_overlap(&buddy, &store);
    }

    #[test]
    fn freeing_merges_with_the_buddy_all_the_way_up() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 64);
        let a = buddy.allocate(&mut store, 0).expect("a frame");
        let b = buddy.allocate(&mut store, 0).expect("a frame");
        assert_eq!(buddy.free_frames(), 62);

        buddy.free(&mut store, a, 0).expect("free");
        assert_eq!(buddy.free_blocks(0), 1, "the buddy is free, so they merge");
        assert_eq!(buddy.free_blocks(1), 1, "and the merged block is order 1");

        buddy.free(&mut store, b, 0).expect("free");
        assert_eq!(buddy.free_frames(), 64);
        assert_eq!(
            buddy.free_blocks(6),
            1,
            "everything merged back into one block"
        );
        assert_eq!(buddy.audit(&store), Ok(()));
        assert_no_overlap(&buddy, &store);
    }

    #[test]
    fn a_double_free_is_rejected_instead_of_corrupting_the_lists() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 16);
        let frame = buddy.allocate(&mut store, 0).expect("a frame");
        buddy.free(&mut store, frame, 0).expect("free");
        assert_eq!(
            buddy.free(&mut store, frame, 0),
            Err(FreeError::AlreadyFree)
        );
        assert_eq!(buddy.audit(&store), Ok(()));
    }

    #[test]
    fn misaligned_and_out_of_range_frees_are_rejected() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 16);
        assert_eq!(
            buddy.free(&mut store, 3, 1),
            Err(FreeError::NotAligned),
            "frame 3 does not start a two-frame block"
        );
        assert_eq!(
            buddy.free(&mut store, 16, 0),
            Err(FreeError::NotManaged),
            "the frame after the region is not managed"
        );
        assert_eq!(
            buddy.free(&mut store, 8, MAX_ORDER + 1),
            Err(FreeError::OrderTooLarge)
        );
    }

    #[test]
    fn an_exhausted_allocator_fails_rather_than_misbehaving() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 8);
        for _ in 0..8 {
            assert!(buddy.allocate(&mut store, 0).is_some());
        }
        assert_eq!(buddy.allocate(&mut store, 0), None);
        assert_eq!(buddy.stats().failures, 1);
        assert_eq!(buddy.free_frames(), 0);
        assert_eq!(buddy.audit(&store), Ok(()));
    }

    #[test]
    fn a_fragmented_allocator_reports_what_it_cannot_serve() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 64);
        let mut frames = Vec::new();
        for _ in 0..64 {
            frames.push(buddy.allocate(&mut store, 0).expect("a frame"));
        }
        assert_eq!(buddy.free_frames(), 0);

        // Free every other frame: 32 free frames, and not two of them adjacent.
        for (index, frame) in frames.iter().enumerate() {
            if index.is_multiple_of(2) {
                buddy.free(&mut store, *frame, 0).expect("free");
            }
        }
        assert_eq!(buddy.free_frames(), 32);
        assert_eq!(buddy.stats().largest_free_order(), Some(0));
        assert_eq!(buddy.allocate(&mut store, 1), None, "no pair is free");
        assert_eq!(buddy.stats().free_frames_below_order(1), 32);

        for (index, frame) in frames.iter().enumerate() {
            if !index.is_multiple_of(2) {
                buddy.free(&mut store, *frame, 0).expect("free");
            }
        }
        assert_eq!(buddy.free_blocks(6), 1);
        assert_eq!(buddy.stats().largest_free_order(), Some(6));
        assert_eq!(buddy.audit(&store), Ok(()));
        assert_no_overlap(&buddy, &store);
    }

    #[test]
    fn auditing_catches_a_list_that_disagrees_with_the_frame_database() {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, 16);
        assert_eq!(buddy.audit(&store), Ok(()));
        // Simulate the frame database losing track of a free block: the audit must notice.
        store.set_free_order(0, None);
        assert_eq!(
            buddy.audit(&store),
            Err("free list entry disagrees with the frame database")
        );
    }

    #[test]
    fn random_allocations_and_frees_keep_the_invariants() {
        let mut rng = Rng::new(0x1010_2026);
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        // Two regions with a hole between them, like a real memory map.
        buddy.add_region(&mut store, 0, 1024);
        buddy.add_region(&mut store, 2048, 3072);

        let mut allocated: Vec<(Frame, usize)> = Vec::new();
        let mut occupied: BTreeSet<Frame> = BTreeSet::new();

        for _ in 0..2000 {
            let allocate = rng.chance(60) || allocated.is_empty();
            if allocate {
                let order = rng.below(5) as usize;
                if let Some(frame) = buddy.allocate(&mut store, order) {
                    let size = 1u64 << order;
                    assert!(frame.is_multiple_of(size), "misaligned block");
                    for f in frame..frame + size {
                        assert!(occupied.insert(f), "allocated frame {f} twice");
                    }
                    allocated.push((frame, order));
                }
            } else {
                let index = rng.below(allocated.len() as u64) as usize;
                let (frame, order) = allocated.swap_remove(index);
                let size = 1u64 << order;
                for f in frame..frame + size {
                    assert!(occupied.remove(&f), "freed frame {f} was not allocated");
                }
                buddy.free(&mut store, frame, order).expect("free");
            }
            assert_eq!(buddy.audit(&store), Ok(()));
            assert_no_overlap(&buddy, &store);
        }

        // Free everything: the allocator must return to exactly its starting shape.
        for (frame, order) in allocated.drain(..) {
            buddy.free(&mut store, frame, order).expect("free");
        }
        assert_eq!(buddy.free_frames(), buddy.managed_frames());
        // Both regions return to the exact shape they were added in: one order-10 block
        // each, and nothing below that.
        assert_eq!(buddy.free_blocks(10), 2);
        for order in 0..10 {
            assert_eq!(
                buddy.free_blocks(order),
                0,
                "order {order} did not merge away"
            );
        }
        assert_eq!(buddy.audit(&store), Ok(()));
        assert_no_overlap(&buddy, &store);
    }
}
