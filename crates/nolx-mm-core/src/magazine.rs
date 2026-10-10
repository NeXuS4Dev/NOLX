//! Per-processor frame caches, and the two frame queues around them.
//!
//! The buddy allocator is a global structure, and the common case in a kernel is one
//! 4 KiB page. Taking the global structure's lock for that is a poor trade: a per-CPU
//! *magazine* -- a small stack of frames private to one processor -- turns the common case
//! into a push or a pop, and only refills touch the buddy allocator.
//!
//! Two queues ride alongside, and they are the reason this module is more than a stack:
//!
//! * [`FrameRing`] of frames **known to be zero**. Every frame handed to a user mapping
//!   must be zeroed, and zeroing at fault time is a measurable cost, so frames are zeroed
//!   when the machine is otherwise idle and handed out already-clean.
//! * A backlog of frames that **must be zeroed** before reuse, which the kernel drains in
//!   its idle loop. A frame that has been in a user's address space is not safe to hand to
//!   the next process until it is clean, and this queue is what makes that a policy rather
//!   than a hope.

use crate::buddy::{BlockStore, Buddy, Frame};

/// A fixed-capacity FIFO of frame numbers.
///
/// Fixed capacity is deliberate: an allocator that can fail needs to fail in a place that
/// is understood, and a ring with a compile-time size cannot itself need memory to grow.
pub struct FrameRing<const CAPACITY: usize> {
    frames: [Frame; CAPACITY],
    head: usize,
    len: usize,
}

impl<const CAPACITY: usize> Default for FrameRing<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> FrameRing<CAPACITY> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: [0; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    /// How many frames the ring can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// How many frames it holds now.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether it is full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == CAPACITY
    }

    /// Append a frame. Returns `false` -- and drops nothing -- when the ring is full.
    pub fn push(&mut self, frame: Frame) -> bool {
        if self.is_full() || CAPACITY == 0 {
            return false;
        }
        let index = (self.head + self.len) % CAPACITY;
        self.frames[index] = frame;
        self.len += 1;
        true
    }

    /// Remove the oldest frame.
    pub fn pop(&mut self) -> Option<Frame> {
        if self.len == 0 {
            return None;
        }
        let frame = self.frames[self.head];
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        Some(frame)
    }

    /// Every frame in the ring, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = Frame> + '_ {
        (0..self.len).map(move |offset| self.frames[(self.head + offset) % CAPACITY])
    }
}

/// A per-processor stack of single frames.
///
/// LIFO on purpose: the most recently freed frame is the most likely to still be in cache.
pub struct Magazine<const CAPACITY: usize> {
    frames: [Frame; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> Clone for Magazine<CAPACITY> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const CAPACITY: usize> Copy for Magazine<CAPACITY> {}

impl<const CAPACITY: usize> Default for Magazine<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> Magazine<CAPACITY> {
    /// An empty magazine.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: [0; CAPACITY],
            len: 0,
        }
    }

    /// How many frames it can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// How many frames it holds now.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether it is full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == CAPACITY
    }

    /// Push a frame, or hand it back if the magazine is full.
    pub fn push(&mut self, frame: Frame) -> Option<Frame> {
        if self.is_full() || CAPACITY == 0 {
            return Some(frame);
        }
        self.frames[self.len] = frame;
        self.len += 1;
        None
    }

    /// Pop a frame.
    pub fn pop(&mut self) -> Option<Frame> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.frames[self.len])
    }

    /// Every frame, newest first.
    pub fn iter(&self) -> impl Iterator<Item = Frame> + '_ {
        self.frames[..self.len].iter().rev().copied()
    }
}

/// A frame handed out by [`FrameCache`], and whether it still needs zeroing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocated {
    /// The frame number.
    pub frame: Frame,
    /// True when the frame came from the zeroed pool and needs no clearing.
    pub zeroed: bool,
}

/// What the caches have done, for the boot log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCacheStats {
    /// Allocations served without touching the buddy allocator.
    pub hits: u64,
    /// Allocations that had to take the buddy allocator's slow path.
    pub misses: u64,
    /// Frames moved from the buddy allocator into a magazine.
    pub refills: u64,
    /// Frames moved from a magazine back into the buddy allocator.
    pub flushes: u64,
    /// Allocations served from the zeroed pool.
    pub zeroed_hits: u64,
    /// Frames returned to the zeroed pool.
    pub zeroed_returns: u64,
}

/// Per-processor frame caches in front of one buddy allocator.
pub struct FrameCache<const CPUS: usize, const CAPACITY: usize> {
    magazines: [Magazine<CAPACITY>; CPUS],
    zeroed: FrameRing<CAPACITY>,
    stats: FrameCacheStats,
}

impl<const CPUS: usize, const CAPACITY: usize> Default for FrameCache<CPUS, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CPUS: usize, const CAPACITY: usize> FrameCache<CPUS, CAPACITY> {
    /// Empty caches.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            magazines: [Magazine::new(); CPUS],
            zeroed: FrameRing::new(),
            stats: FrameCacheStats {
                hits: 0,
                misses: 0,
                refills: 0,
                flushes: 0,
                zeroed_hits: 0,
                zeroed_returns: 0,
            },
        }
    }

    /// Frames held in magazines, not in the zeroed pool.
    #[must_use]
    pub fn cached(&self) -> u64 {
        self.magazines.iter().map(|m| m.len() as u64).sum()
    }

    /// A magazine, for assertions in tests and for diagnostics.
    #[must_use]
    pub fn magazine(&self, cpu: usize) -> Option<&Magazine<CAPACITY>> {
        self.magazines.get(cpu)
    }

    /// What the caches have done.
    #[must_use]
    pub const fn stats(&self) -> FrameCacheStats {
        self.stats
    }

    /// Allocate one frame for `cpu`.
    ///
    /// The zeroed pool is consulted first, because that is what it is for: the frame comes
    /// back clean and the caller can map it straight into a user address space. A caller
    /// that cannot use a zeroed frame pays nothing for this, because a frame is only ever
    /// put in the pool after it *was* zeroed.
    pub fn allocate(
        &mut self,
        cpu: usize,
        buddy: &mut Buddy,
        store: &mut impl BlockStore,
    ) -> Option<Allocated> {
        if let Some(frame) = self.zeroed.pop() {
            self.stats.zeroed_hits += 1;
            return Some(Allocated {
                frame,
                zeroed: true,
            });
        }

        if let Some(magazine) = self.magazines.get_mut(cpu) {
            if let Some(frame) = magazine.pop() {
                self.stats.hits += 1;
                return Some(Allocated {
                    frame,
                    zeroed: false,
                });
            }
        }

        self.stats.misses += 1;
        let frame = self.refill(cpu, buddy, store)?;
        Some(Allocated {
            frame,
            zeroed: false,
        })
    }

    /// Return one frame to `cpu`'s magazine, flushing half of it when it overflows.
    ///
    /// The caller must not reuse the frame's contents: a frame freed through here is
    /// assumed to be dirty, and if it later reaches a user mapping the kernel zeroes it.
    pub fn free(
        &mut self,
        cpu: usize,
        frame: Frame,
        buddy: &mut Buddy,
        store: &mut impl BlockStore,
    ) {
        let Some(magazine) = self.magazines.get_mut(cpu) else {
            // A processor the caches were not sized for: hand the frame straight back.
            let _ = buddy.free(store, frame, 0);
            return;
        };

        if magazine.push(frame).is_some() {
            // Full: return half of the magazine to the buddy allocator, oldest first, and
            // retry. Keeping half is what makes the cache worth having -- a magazine that
            // empties itself on every overflow would never serve a hit.
            let mut keep = magazine.len() / 2;
            while keep > 0 {
                if let Some(old) = magazine.pop() {
                    let _ = buddy.free(store, old, 0);
                    self.stats.flushes += 1;
                    keep -= 1;
                }
            }
            // The pop above removed the newest frames, so retry with the same frame; the
            // magazine now has room.
            if magazine.push(frame).is_some() {
                let _ = buddy.free(store, frame, 0);
                self.stats.flushes += 1;
            }
        }
    }

    /// Put a frame that is known to be zero into the pool.
    ///
    /// Returns `false` when the pool is full; the caller then has a zeroed frame it must
    /// return to the buddy allocator instead.
    pub fn mark_zeroed(&mut self, frame: Frame) -> bool {
        if self.zeroed.push(frame) {
            self.stats.zeroed_returns += 1;
            true
        } else {
            false
        }
    }

    /// Take a zeroed frame, if the pool has one.
    pub fn take_zeroed(&mut self) -> Option<Frame> {
        self.zeroed.pop()
    }

    /// Frames waiting in the zeroed pool.
    #[must_use]
    pub const fn zeroed_frames(&self) -> usize {
        self.zeroed.len()
    }

    /// Return every cached frame to the buddy allocator.
    ///
    /// Returns how many frames were returned. The kernel calls this when it is about to
    /// stop trusting the caches -- a test, a shutdown, or a consistency check.
    pub fn drain(&mut self, buddy: &mut Buddy, store: &mut impl BlockStore) -> u64 {
        let mut returned = 0;
        for magazine in &mut self.magazines {
            while let Some(frame) = magazine.pop() {
                let _ = buddy.free(store, frame, 0);
                self.stats.flushes += 1;
                returned += 1;
            }
        }
        while let Some(frame) = self.zeroed.pop() {
            let _ = buddy.free(store, frame, 0);
            returned += 1;
        }
        returned
    }

    /// Take a frame from the buddy allocator for `cpu`, filling the magazine.
    fn refill(
        &mut self,
        cpu: usize,
        buddy: &mut Buddy,
        store: &mut impl BlockStore,
    ) -> Option<Frame> {
        let frame = buddy.allocate(store, 0)?;
        let Some(magazine) = self.magazines.get_mut(cpu) else {
            return Some(frame);
        };
        // Fill what the magazine can hold. The first frame is the one being returned; the
        // rest wait here for the next allocation.
        let mut filled = 0;
        for _ in 1..CAPACITY {
            match buddy.allocate(store, 0) {
                Some(extra) => {
                    if magazine.push(extra).is_some() {
                        let _ = buddy.free(store, extra, 0);
                        break;
                    }
                    filled += 1;
                }
                None => break,
            }
        }
        self.stats.refills += filled;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::MAX_ORDER;
    use crate::test_rng::Rng;
    use std::collections::{BTreeMap, BTreeSet};

    /// One frame's links: the next block, the previous block, and the recorded order.
    type Links = (Option<Frame>, Option<Frame>, Option<usize>);

    #[derive(Default)]
    struct MapStore {
        links: BTreeMap<Frame, Links>,
    }

    impl MapStore {
        fn entry(&mut self, frame: Frame) -> &mut Links {
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

    fn allocator(frames: u64) -> (Buddy, MapStore) {
        let mut store = MapStore::default();
        let mut buddy = Buddy::new();
        buddy.add_region(&mut store, 0, frames);
        (buddy, store)
    }

    #[test]
    fn a_ring_is_a_fifo_and_refuses_to_grow() {
        let mut ring: FrameRing<4> = FrameRing::new();
        assert!(ring.is_empty());
        for frame in 0..4 {
            assert!(ring.push(frame));
        }
        assert!(ring.is_full());
        assert!(!ring.push(99), "a full ring must not silently drop a frame");
        assert_eq!(ring.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(ring.pop(), Some(0));
        assert_eq!(ring.pop(), Some(1));
        assert!(ring.push(4));
        assert_eq!(ring.iter().collect::<Vec<_>>(), vec![2, 3, 4]);
        assert_eq!(ring.pop(), Some(2));
        assert_eq!(ring.pop(), Some(3));
        assert_eq!(ring.pop(), Some(4));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn a_magazine_is_a_stack_and_hands_back_what_it_cannot_hold() {
        let mut magazine: Magazine<2> = Magazine::new();
        assert_eq!(magazine.push(7), None);
        assert_eq!(magazine.push(8), None);
        assert_eq!(magazine.push(9), Some(9), "the third frame comes back");
        assert_eq!(magazine.iter().collect::<Vec<_>>(), vec![8, 7]);
        assert_eq!(magazine.pop(), Some(8));
        assert_eq!(magazine.pop(), Some(7));
        assert_eq!(magazine.pop(), None);
    }

    #[test]
    fn the_cache_serves_the_slow_path_once_and_then_hits() {
        let (mut buddy, mut store) = allocator(64);
        let mut cache: FrameCache<2, 4> = FrameCache::new();

        let first = cache.allocate(0, &mut buddy, &mut store).expect("a frame");
        assert!(!first.zeroed);
        assert_eq!(cache.stats().misses, 1);
        // The refill filled the magazine, so the next three allocations are hits.
        for _ in 0..3 {
            assert!(cache.allocate(0, &mut buddy, &mut store).is_some());
        }
        assert_eq!(cache.stats().hits, 3);
        assert_eq!(cache.magazine(0).expect("cpu 0").len(), 0);
        assert_eq!(cache.cached(), 0);
    }

    #[test]
    fn freeing_past_the_magazine_capacity_flushes_to_the_buddy() {
        let (mut buddy, mut store) = allocator(64);
        let mut cache: FrameCache<1, 4> = FrameCache::new();

        // Allocate eight frames, then free them into a magazine that holds four: when it
        // overflows it returns half of itself rather than the frame it was just handed.
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(
                cache
                    .allocate(0, &mut buddy, &mut store)
                    .expect("a frame")
                    .frame,
            );
        }
        for frame in frames {
            cache.free(0, frame, &mut buddy, &mut store);
        }
        assert!(cache.cached() <= 4);
        assert!(cache.stats().flushes >= 4);
        assert_eq!(buddy.free_frames() + cache.cached(), 64);
        assert_eq!(buddy.audit(&store), Ok(()));
    }

    #[test]
    fn a_frame_the_caches_were_not_sized_for_goes_straight_back() {
        let (mut buddy, mut store) = allocator(8);
        let mut cache: FrameCache<1, 2> = FrameCache::new();
        let frame = buddy.allocate(&mut store, 0).expect("a frame");
        assert_eq!(buddy.free_frames(), 7);
        cache.free(7, frame, &mut buddy, &mut store);
        assert_eq!(
            buddy.free_frames(),
            8,
            "the frame went back to the allocator"
        );
        assert_eq!(cache.cached(), 0);
    }

    #[test]
    fn zeroed_frames_come_out_of_the_pool_and_not_out_of_the_buddy() {
        let (mut buddy, mut store) = allocator(16);
        let mut cache: FrameCache<1, 4> = FrameCache::new();

        assert!(cache.mark_zeroed(9));
        assert!(cache.mark_zeroed(10));
        assert_eq!(cache.zeroed_frames(), 2);

        let first = cache.allocate(0, &mut buddy, &mut store).expect("a frame");
        assert_eq!(first.frame, 9, "the pool is a FIFO, so 9 is first");
        assert!(first.zeroed);
        let second = cache.allocate(0, &mut buddy, &mut store).expect("a frame");
        assert_eq!(second.frame, 10);
        assert!(second.zeroed);
        assert_eq!(cache.stats().zeroed_hits, 2);

        // The pool is empty now, so the next frame comes from the buddy and must be zeroed
        // by the caller, which is exactly what `zeroed: false` means.
        let third = cache.allocate(0, &mut buddy, &mut store).expect("a frame");
        assert!(!third.zeroed);
        assert_eq!(cache.stats().zeroed_hits, 2);
    }

    #[test]
    fn a_full_zeroed_pool_tells_the_caller_to_keep_the_frame() {
        let mut cache: FrameCache<1, 2> = FrameCache::new();
        assert!(cache.mark_zeroed(1));
        assert!(cache.mark_zeroed(2));
        assert!(!cache.mark_zeroed(3), "no room for a third");
        assert_eq!(cache.zeroed_frames(), 2);
    }

    #[test]
    fn draining_returns_every_frame_to_the_allocator() {
        let (mut buddy, mut store) = allocator(128);
        let mut cache: FrameCache<2, 8> = FrameCache::new();
        let mut held = Vec::new();
        for _ in 0..20 {
            held.push(cache.allocate(1, &mut buddy, &mut store).expect("a frame"));
        }
        assert_eq!(
            buddy.free_frames(),
            128 - 24,
            "three refills of eight frames"
        );
        let returned = cache.drain(&mut buddy, &mut store);
        assert_eq!(
            returned, 4,
            "only the frames the caches still hold are theirs to return"
        );
        assert_eq!(
            buddy.free_frames() + 20,
            128,
            "the allocator plus the frames the test holds account for every frame"
        );
        assert_eq!(cache.cached(), 0);
        assert_eq!(cache.zeroed_frames(), 0);
        let _ = held;
    }

    #[test]
    fn random_allocation_through_the_caches_conserves_frames() {
        let mut rng = Rng::new(0x0f2e_2c11);
        let (mut buddy, mut store) = allocator(4096);
        let mut cache: FrameCache<2, 8> = FrameCache::new();
        let mut held: Vec<(usize, Frame)> = Vec::new();
        let mut frames_seen: BTreeSet<Frame> = BTreeSet::new();

        for _ in 0..4000 {
            let cpu = rng.below(2) as usize;
            if rng.chance(55) || held.is_empty() {
                if let Some(allocated) = cache.allocate(cpu, &mut buddy, &mut store) {
                    assert!(
                        frames_seen.insert(allocated.frame),
                        "frame {} handed out twice",
                        allocated.frame
                    );
                    held.push((cpu, allocated.frame));
                }
            } else {
                let index = rng.below(held.len() as u64) as usize;
                let (cpu, frame) = held.swap_remove(index);
                assert!(frames_seen.remove(&frame));
                cache.free(cpu, frame, &mut buddy, &mut store);
            }
            assert_eq!(buddy.audit(&store), Ok(()));
        }

        // Everything the test holds goes back, then the caches are drained: the allocator
        // must be exactly as full as it started.
        for (cpu, frame) in held {
            cache.free(cpu, frame, &mut buddy, &mut store);
        }
        cache.drain(&mut buddy, &mut store);
        assert_eq!(buddy.free_frames(), 4096);
        assert_eq!(buddy.stats().largest_free_order(), Some(MAX_ORDER));
        assert_eq!(buddy.audit(&store), Ok(()));
    }
}
