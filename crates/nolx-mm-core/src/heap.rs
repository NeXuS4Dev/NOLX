//! A boundary-tagged heap with size-class free lists: what the kernel heap is built from.
//!
//! Every block carries a sixteen-byte header giving its own size, the size of the block
//! before it, whether it is free, and -- when free -- the next free block in its bin. The
//! `prev_size` field is what makes freeing `O(1)`: a freed block can merge with its left
//! neighbour without a search, and the merge keeps a long-running kernel from grinding its
//! heap into an unusable dust of small fragments.
//!
//! Free blocks hang off bins indexed by size class, so an allocation looks in the smallest
//! class that could hold it rather than at the head of one enormous list.
//!
//! The region arrives as a plain `&mut [u8]`. That is the point: the same code runs on the
//! host against a `Vec<u8>` in the tests and in the kernel against a slice of the direct
//! map, with no `unsafe` in either case, because the borrow checker -- not a comment -- is
//! what keeps a heap operation from reading the region while it is being written.
//!
//! Offsets and sizes are 32-bit, so a region is at most 4 GiB. A kernel heap region is not
//! the whole address space, and the smaller type makes every metadata field and every
//! length check cheaper.

/// The header in front of every block: size, previous size, magic, next free.
const HEADER: u32 = 16;

/// The smallest block: header plus sixteen usable bytes.
pub const MIN_BLOCK: u32 = 32;

/// The alignment of every payload, and of every block offset.
pub const ALIGNMENT: u32 = 16;

/// The largest region this heap can manage, from the 32-bit offsets.
pub const MAX_REGION: u64 = u32::MAX as u64;

/// The number of size-class bins. Bin `b` holds blocks from `MIN_BLOCK << b` up to, but not
/// including, `MIN_BLOCK << (b + 1)`; the last bin holds everything larger.
pub const BINS: usize = 27;

/// Marks a free block's header.
const FREE_MAGIC: u32 = 0x4E4F_4C58; // "NOLX"

/// The `next_free` value for a block that is not on a free list.
const NOT_FREE: u32 = u32::MAX;

/// What the heap has done, for the boot log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    /// Successful allocations.
    pub allocations: u64,
    /// Frees that were accepted.
    pub frees: u64,
    /// Allocation attempts that found no block large enough.
    pub failures: u64,
    /// Blocks split because the chosen block was larger than the request.
    pub splits: u64,
    /// Pairs of adjacent free blocks merged while freeing.
    pub merges: u64,
    /// Usable bytes in allocated blocks.
    pub in_use: u64,
    /// Usable bytes in free blocks.
    pub free: u64,
    /// Free blocks plus allocated blocks.
    pub blocks: u64,
}

/// Why a heap operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    /// The region is smaller than one block, longer than 4 GiB, or not aligned.
    RegionTooSmall,
    /// The heap's metadata is inconsistent: a bug in the heap, or a caller that wrote
    /// outside its payload. There is no safe way to continue, so the heap refuses to touch
    /// the region and names what it found.
    Corrupt(&'static str),
}

/// A heap over one contiguous region.
pub struct BoundaryHeap<'a> {
    region: &'a mut [u8],
    heads: [u32; BINS],
    stats: HeapStats,
}

impl<'a> BoundaryHeap<'a> {
    /// Build a heap over `region`, which becomes one free block.
    ///
    /// The region must be at least [`MIN_BLOCK`] bytes, a multiple of [`ALIGNMENT`], and at
    /// most 4 GiB.
    pub fn new(region: &'a mut [u8]) -> Result<Self, HeapError> {
        let len = region.len();
        if len < MIN_BLOCK as usize || len % ALIGNMENT as usize != 0 || len as u64 > MAX_REGION {
            return Err(HeapError::RegionTooSmall);
        }
        let mut heap = Self {
            region,
            heads: [NOT_FREE; BINS],
            stats: HeapStats {
                free: len as u64 - u64::from(HEADER),
                blocks: 1,
                ..HeapStats::default()
            },
        };
        heap.write_free_block(0, len as u32, 0)?;
        Ok(heap)
    }

    /// The size of the region.
    #[must_use]
    pub fn region_len(&self) -> usize {
        self.region.len()
    }

    /// Everything the heap has counted.
    #[must_use]
    pub const fn stats(&self) -> HeapStats {
        self.stats
    }

    /// The usable size of the largest free block: what one allocation can still ask for.
    ///
    /// This scans the highest non-empty bin only, because a block in a higher bin is always
    /// larger than one in a lower one. It is cheap enough to call from a boot log and too
    /// expensive to call on every allocation.
    #[must_use]
    pub fn largest_free(&self) -> u64 {
        for bin in (0..BINS).rev() {
            let mut current = self.heads[bin];
            let mut largest = 0;
            while current != NOT_FREE {
                largest = largest.max(u64::from(self.header_size(current) - HEADER));
                current = self.next_free(current);
            }
            if largest != 0 {
                return largest;
            }
        }
        0
    }

    /// Allocate `size` bytes and return the offset of the payload in the region.
    ///
    /// The offset is a multiple of [`ALIGNMENT`]. `None` means no block was large enough;
    /// what to do about that is the caller's decision, and in a kernel it is usually "do
    /// without" rather than "panic".
    pub fn allocate(&mut self, size: usize) -> Option<u32> {
        if size == 0 || size as u64 > MAX_REGION {
            self.stats.failures += 1;
            return None;
        }
        let needed = block_size_for(size as u32)?;
        let mut prepared = None;
        for bin in bin_for(needed)..BINS {
            let mut previous = NOT_FREE;
            let mut current = self.heads[bin];
            while current != NOT_FREE {
                let block_size = self.header_size(current);
                if block_size >= needed {
                    prepared = Some((bin, current, block_size, previous));
                    break;
                }
                previous = current;
                current = self.next_free(current);
            }
            if prepared.is_some() {
                break;
            }
        }

        let (bin, block, block_size, previous) = match prepared {
            Some(found) => found,
            None => {
                self.stats.failures += 1;
                return None;
            }
        };

        // Unlink the block from its bin, then split it if the remainder can stand alone.
        let next = self.next_free(block);
        if previous == NOT_FREE {
            self.heads[bin] = next;
        } else {
            self.write_next_free(previous, next);
        }

        let leftover = block_size - needed;
        if leftover >= MIN_BLOCK {
            self.write_allocated_block(block, needed);
            // The remainder is a new block: its back pointer is the block just allocated,
            // and the block after it gains a new back pointer of its own.
            self.write_free_block(block + needed, leftover, needed)
                .expect("the remainder is inside the region");
            self.stats.blocks += 1;
            self.stats.splits += 1;
            self.stats.in_use += u64::from(needed - HEADER);
            self.stats.free -= u64::from(needed);
        } else {
            self.write_allocated_block(block, block_size);
            self.stats.in_use += u64::from(block_size - HEADER);
            self.stats.free -= u64::from(block_size - HEADER);
        }
        self.stats.allocations += 1;
        Some(block + HEADER)
    }

    /// Return a payload to the heap, merging it with free neighbours.
    ///
    /// `offset` must come from [`BoundaryHeap::allocate`] on this heap and not have been
    /// freed yet. The heap detects what it can -- an offset that is not a block, a double
    /// free, a header that no longer makes sense -- and reports it rather than corrupting
    /// the region for a caller that has already walked past the bug.
    pub fn deallocate(&mut self, offset: u32) -> Result<(), HeapError> {
        let block = offset.checked_sub(HEADER).ok_or(HeapError::Corrupt(
            "the payload offset is not inside the region",
        ))?;
        if !block.is_multiple_of(ALIGNMENT)
            || (block as usize) + MIN_BLOCK as usize > self.region.len()
        {
            return Err(HeapError::Corrupt(
                "the payload offset is not a block start",
            ));
        }
        let mut size = self.header_size(block);
        if !self.header_plausible(block, size) {
            return Err(HeapError::Corrupt("the block header is not plausible"));
        }
        if self.magic(block) == FREE_MAGIC {
            return Err(HeapError::Corrupt("the block is already free"));
        }

        // The block becomes free. Merging removes one block per merge, which returns the
        // header of the absorbed block to the payload pool, so the free total grows by it.
        self.stats.in_use -= u64::from(size - HEADER);
        self.stats.free += u64::from(size - HEADER);
        self.stats.frees += 1;

        let mut head = block;
        let previous_size = self.prev_size_of(block);
        if previous_size >= MIN_BLOCK && previous_size <= block {
            let previous = block - previous_size;
            if self.header_plausible(previous, previous_size) && self.magic(previous) == FREE_MAGIC
            {
                self.unlink(bin_for(previous_size), previous)?;
                head = previous;
                size += previous_size;
                self.stats.blocks -= 1;
                self.stats.merges += 1;
                self.stats.free += u64::from(HEADER);
            }
        }

        let next = head + size;
        if (next as usize) + HEADER as usize <= self.region.len() {
            let next_size = self.header_size(next);
            if self.header_plausible(next, next_size) && self.magic(next) == FREE_MAGIC {
                self.unlink(bin_for(next_size), next)?;
                size += next_size;
                self.stats.blocks -= 1;
                self.stats.merges += 1;
                self.stats.free += u64::from(HEADER);
            }
        }

        let previous_size = self.prev_size_of(head);
        self.write_free_block(head, size, previous_size)
    }

    /// The usable size of the payload at `offset`.
    pub fn usable_size(&self, offset: u32) -> Result<u32, HeapError> {
        let block = offset.checked_sub(HEADER).ok_or(HeapError::Corrupt(
            "the payload offset is not inside the region",
        ))?;
        if (block as usize) + MIN_BLOCK as usize > self.region.len() {
            return Err(HeapError::Corrupt(
                "the payload offset is not a block start",
            ));
        }
        let size = self.header_size(block);
        if !self.header_plausible(block, size) {
            return Err(HeapError::Corrupt("the block header is not plausible"));
        }
        if self.magic(block) == FREE_MAGIC {
            return Err(HeapError::Corrupt("the block is free"));
        }
        Ok(size - HEADER)
    }

    /// The bytes of the payload at `offset`, for a caller that needs to look at what it
    /// allocated. The slice ends exactly where the block does, so writing through it cannot
    /// reach the next block's header.
    pub fn payload(&self, offset: u32) -> Result<&[u8], HeapError> {
        let size = self.usable_size(offset)?;
        Ok(&self.region[offset as usize..offset as usize + size as usize])
    }

    /// The same payload, mutably.
    pub fn payload_mut(&mut self, offset: u32) -> Result<&mut [u8], HeapError> {
        let size = self.usable_size(offset)?;
        Ok(&mut self.region[offset as usize..offset as usize + size as usize])
    }

    /// Walk the whole block chain and verify every invariant the heap relies on.
    ///
    /// `O(blocks)`, deliberately: the alternative is learning about a bad back pointer when
    /// an allocation has already handed overlapping memory to two owners.
    pub fn check(&self) -> Result<(), HeapError> {
        let len = self.region.len() as u32;
        let mut offset = 0u32;
        let mut blocks = 0u64;
        let mut free_blocks = 0u64;

        while offset < len {
            let size = self.header_size(offset);
            if !self.header_plausible(offset, size) {
                return Err(HeapError::Corrupt("a block header is not plausible"));
            }
            let previous_size = self.prev_size_of(offset);
            if offset == 0 {
                if previous_size != 0 {
                    return Err(HeapError::Corrupt("the first block has a back pointer"));
                }
            } else if previous_size < MIN_BLOCK
                || previous_size > offset
                || self.header_size(offset - previous_size) != previous_size
            {
                return Err(HeapError::Corrupt("a block's back pointer is wrong"));
            }
            match self.magic(offset) {
                FREE_MAGIC => {
                    free_blocks += 1;
                    if !self.on_a_free_list(offset) {
                        return Err(HeapError::Corrupt("a free block is not on its bin list"));
                    }
                }
                0 => {}
                _ => {
                    return Err(HeapError::Corrupt(
                        "a block's magic is neither free nor allocated",
                    ))
                }
            }
            blocks += 1;
            if blocks > u64::from(len / MIN_BLOCK) + 1 {
                return Err(HeapError::Corrupt("the block chain does not terminate"));
            }
            offset += size;
        }
        if offset != len {
            return Err(HeapError::Corrupt(
                "the block chain does not cover the region",
            ));
        }

        // Every free-list entry must point at a free block of the right class, and there
        // must be no more entries than free blocks.
        let mut entries = 0u64;
        for bin in 0..BINS {
            let mut current = self.heads[bin];
            let mut steps = 0u64;
            while current != NOT_FREE {
                if (current as usize) + MIN_BLOCK as usize > self.region.len()
                    || !current.is_multiple_of(ALIGNMENT)
                {
                    return Err(HeapError::Corrupt("a free list holds a bad offset"));
                }
                if self.magic(current) != FREE_MAGIC {
                    return Err(HeapError::Corrupt("a free list holds an allocated block"));
                }
                if bin_for(self.header_size(current)) != bin {
                    return Err(HeapError::Corrupt("a free block is in the wrong bin"));
                }
                entries += 1;
                steps += 1;
                if steps > u64::from(len / MIN_BLOCK) + 1 {
                    return Err(HeapError::Corrupt("a free list is a cycle"));
                }
                current = self.next_free(current);
            }
        }
        if entries != free_blocks {
            return Err(HeapError::Corrupt(
                "the free blocks and the free lists disagree",
            ));
        }
        Ok(())
    }

    /// Verify that the heap's own counts still describe the region.
    pub fn check_stats(&self) -> Result<(), HeapError> {
        let len = self.region.len() as u32;
        let mut offset = 0u32;
        let mut blocks = 0u64;
        let mut free = 0u64;
        let mut in_use = 0u64;
        while offset < len {
            let size = self.header_size(offset);
            if !self.header_plausible(offset, size) {
                return Err(HeapError::Corrupt("a block header is not plausible"));
            }
            blocks += 1;
            let usable = u64::from(size - HEADER);
            if self.magic(offset) == FREE_MAGIC {
                free += usable;
            } else {
                in_use += usable;
            }
            offset += size;
        }
        if free != self.stats.free {
            return Err(HeapError::Corrupt("the free byte count is wrong"));
        }
        if in_use != self.stats.in_use {
            return Err(HeapError::Corrupt("the in-use byte count is wrong"));
        }
        if blocks != self.stats.blocks {
            return Err(HeapError::Corrupt("the block count is wrong"));
        }
        // Splits add a block and merges remove one, and nothing else does either.
        if blocks != 1 + self.stats.splits - self.stats.merges {
            return Err(HeapError::Corrupt(
                "the split and merge counts do not add up",
            ));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- block metadata

    fn read_u32(&self, offset: u32) -> u32 {
        let offset = offset as usize;
        u32::from_le_bytes([
            self.region[offset],
            self.region[offset + 1],
            self.region[offset + 2],
            self.region[offset + 3],
        ])
    }

    fn write_u32(&mut self, offset: u32, value: u32) {
        let offset = offset as usize;
        self.region[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn header_size(&self, block: u32) -> u32 {
        self.read_u32(block)
    }

    fn prev_size_of(&self, block: u32) -> u32 {
        self.read_u32(block + 4)
    }

    fn magic(&self, block: u32) -> u32 {
        self.read_u32(block + 8)
    }

    fn next_free(&self, block: u32) -> u32 {
        self.read_u32(block + 12)
    }

    fn write_next_free(&mut self, block: u32, next: u32) {
        self.write_u32(block + 12, next);
    }

    fn header_plausible(&self, block: u32, size: u32) -> bool {
        size >= MIN_BLOCK
            && size.is_multiple_of(ALIGNMENT)
            && (block as usize) + (size as usize) <= self.region.len()
    }

    /// Write a free block: its header, its bin link, and the back pointer of the block that
    /// follows it, which is what makes the following block's free `O(1)` as well.
    fn write_free_block(
        &mut self,
        block: u32,
        size: u32,
        previous_size: u32,
    ) -> Result<(), HeapError> {
        if !self.header_plausible(block, size) {
            return Err(HeapError::Corrupt("a free block would not fit the region"));
        }
        self.write_u32(block, size);
        self.write_u32(block + 4, previous_size);
        self.write_u32(block + 8, FREE_MAGIC);
        let bin = bin_for(size);
        let head = self.heads[bin];
        self.write_u32(block + 12, head);
        self.heads[bin] = block;
        let next = block + size;
        if (next as usize) + HEADER as usize <= self.region.len() {
            self.write_u32(next + 4, size);
        }
        Ok(())
    }

    /// Write an allocated block. The size and the back pointer of the next block do not
    /// change when a block is handed out, so only the magic and the link need clearing.
    fn write_allocated_block(&mut self, block: u32, size: u32) {
        self.write_u32(block, size);
        self.write_u32(block + 8, 0);
        self.write_u32(block + 12, NOT_FREE);
    }

    /// Remove a block from its bin's list.
    fn unlink(&mut self, bin: usize, block: u32) -> Result<(), HeapError> {
        let mut previous = NOT_FREE;
        let mut current = self.heads[bin];
        while current != NOT_FREE {
            if current == block {
                let next = self.next_free(current);
                if previous == NOT_FREE {
                    self.heads[bin] = next;
                } else {
                    self.write_next_free(previous, next);
                }
                return Ok(());
            }
            previous = current;
            current = self.next_free(current);
        }
        Err(HeapError::Corrupt("a free block is not on its bin list"))
    }

    /// Whether a free block is reachable from its bin.
    fn on_a_free_list(&self, block: u32) -> bool {
        let mut current = self.heads[bin_for(self.header_size(block))];
        while current != NOT_FREE {
            if current == block {
                return true;
            }
            current = self.next_free(current);
        }
        false
    }
}

/// The block size needed to hold a payload of `size` bytes, or `None` if it cannot be
/// represented.
#[must_use]
pub fn block_size_for(size: u32) -> Option<u32> {
    let total = u64::from(size) + u64::from(HEADER);
    let rounded = total.next_multiple_of(u64::from(ALIGNMENT));
    if rounded > u64::from(u32::MAX) {
        return None;
    }
    Some((rounded as u32).max(MIN_BLOCK))
}

/// The bin a block of `size` bytes belongs in.
#[must_use]
pub fn bin_for(size: u32) -> usize {
    // The comparison runs in 64 bits: the bin for the top size class would shift a 32-bit
    // `MIN_BLOCK` past its own width.
    let mut bin = 0;
    while bin + 1 < BINS && (u64::from(MIN_BLOCK) << (bin + 1)) <= u64::from(size) {
        bin += 1;
    }
    bin
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    fn heap(region: &mut [u8]) -> BoundaryHeap<'_> {
        BoundaryHeap::new(region).expect("a valid region")
    }

    #[test]
    fn a_new_heap_is_one_free_block_covering_the_region() {
        let mut region = vec![0u8; 4096];
        let heap = heap(&mut region);
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
        assert_eq!(heap.stats().blocks, 1);
        assert_eq!(heap.stats().allocations, 0);
        assert_eq!(heap.stats().free, 4096 - 16);
        assert_eq!(heap.largest_free(), 4096 - 16);
        assert_eq!(bin_for(MIN_BLOCK), 0);
        assert_eq!(bin_for(4096), 7, "4096 is 2^7 blocks of the smallest size");
    }

    #[test]
    fn allocation_returns_an_aligned_payload_from_a_split_block() {
        let mut region = vec![0u8; 4096];
        let mut heap = heap(&mut region);
        let payload = heap.allocate(100).expect("allocated");
        assert_eq!(payload % ALIGNMENT, 0, "payloads are aligned");
        assert_eq!(block_size_for(100), Some(128));
        assert_eq!(heap.usable_size(payload), Ok(112));
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
        assert_eq!(
            heap.stats().blocks,
            2,
            "the remainder is a block of its own"
        );
        assert_eq!(heap.stats().splits, 1);
        assert_eq!(heap.stats().in_use, 112);
        assert_eq!(heap.largest_free(), 4096 - 128 - 16);
    }

    #[test]
    fn a_tiny_allocation_still_gets_the_smallest_block() {
        let mut region = vec![0u8; 4096];
        let mut heap = heap(&mut region);
        assert_eq!(block_size_for(1), Some(MIN_BLOCK));
        let payload = heap.allocate(1).expect("allocated");
        assert_eq!(heap.usable_size(payload), Ok(MIN_BLOCK - HEADER));
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
    }

    #[test]
    fn allocations_come_from_the_smallest_bin_that_can_hold_them() {
        let mut region = vec![0u8; 8192];
        let mut heap = heap(&mut region);
        let first = heap.allocate(100).expect("allocated");
        let second = heap.allocate(100).expect("allocated");
        assert_ne!(first, second);
        heap.deallocate(first).expect("freed");
        // The freed 128-byte block is in a smaller bin than the large remainder, so the
        // next identical request must reuse it instead of carving into the remainder again.
        let third = heap.allocate(100).expect("allocated");
        assert_eq!(third, first, "the small free block was reused");
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
        assert_eq!(heap.usable_size(second), Ok(112));
    }

    #[test]
    fn payloads_do_not_overlap_and_keep_their_contents() {
        let mut region = vec![0u8; 8192];
        let mut heap = heap(&mut region);
        let mut taken: Vec<(u32, u8)> = Vec::new();
        for index in 0..16u8 {
            let payload = heap.allocate(64).expect("allocated");
            heap.payload_mut(payload).expect("payload").fill(index);
            taken.push((payload, index));
        }
        for (payload, index) in &taken {
            assert!(
                heap.payload(*payload)
                    .expect("payload")
                    .iter()
                    .all(|byte| byte == index),
                "payload overwritten"
            );
        }
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
    }

    #[test]
    fn freeing_everything_coalesces_back_into_one_block() {
        let mut region = vec![0u8; 8192];
        let mut heap = heap(&mut region);
        let mut payloads = Vec::new();
        for _ in 0..8 {
            payloads.push(heap.allocate(100).expect("allocated"));
        }
        for payload in payloads {
            heap.deallocate(payload).expect("freed");
        }
        assert_eq!(heap.stats().blocks, 1, "the region is one free block again");
        assert_eq!(heap.stats().in_use, 0);
        assert_eq!(heap.largest_free(), 8192 - 16);
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
        let everything = heap
            .allocate(8192 - 16)
            .expect("the whole region minus one header");
        assert_eq!(everything, HEADER, "the payload starts after the header");
        assert!(heap.allocate(1).is_none(), "and then it really is empty");
    }

    #[test]
    fn a_double_free_is_reported_rather_than_silently_accepted() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        let payload = heap.allocate(32).expect("allocated");
        heap.deallocate(payload).expect("freed");
        assert_eq!(
            heap.deallocate(payload),
            Err(HeapError::Corrupt("the block is already free"))
        );
        assert_eq!(
            heap.usable_size(payload),
            Err(HeapError::Corrupt("the block is free"))
        );
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
    }

    #[test]
    fn an_offset_that_is_not_a_payload_is_rejected() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        assert_eq!(
            heap.deallocate(0),
            Err(HeapError::Corrupt(
                "the payload offset is not inside the region"
            ))
        );
        assert_eq!(
            heap.deallocate(24),
            Err(HeapError::Corrupt(
                "the payload offset is not a block start"
            )),
            "8 is not a multiple of the alignment"
        );
        assert_eq!(
            heap.usable_size(1024),
            Err(HeapError::Corrupt(
                "the payload offset is not a block start"
            ))
        );
        assert_eq!(
            heap.usable_size(0),
            Err(HeapError::Corrupt(
                "the payload offset is not inside the region"
            ))
        );
    }

    #[test]
    fn exhaustion_is_reported_and_leaves_the_heap_usable() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        let mut payloads = Vec::new();
        while let Some(payload) = heap.allocate(64) {
            payloads.push(payload);
        }
        assert!(!payloads.is_empty());
        assert_eq!(heap.stats().failures, 1);
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
        // Freeing one block makes room for exactly one more allocation of the same size.
        let freed = payloads.pop().expect("a payload");
        heap.deallocate(freed).expect("freed");
        assert!(heap.allocate(64).is_some());
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
    }

    #[test]
    fn a_zero_byte_request_is_refused() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        assert_eq!(heap.allocate(0), None);
        assert_eq!(heap.stats().failures, 1);
    }

    #[test]
    fn a_region_that_is_too_small_or_misaligned_is_refused() {
        let mut small = vec![0u8; 16];
        assert_eq!(
            BoundaryHeap::new(&mut small).err(),
            Some(HeapError::RegionTooSmall)
        );
        let mut odd = vec![0u8; 100];
        assert_eq!(
            BoundaryHeap::new(&mut odd).err(),
            Some(HeapError::RegionTooSmall)
        );
    }

    #[test]
    fn check_catches_a_broken_back_pointer() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        let payload = heap.allocate(100).expect("allocated");
        // The second block starts where the first one ends: payload + usable size.
        let second = payload + heap.usable_size(payload).expect("size");
        heap.write_u32(second + 4, 96);
        assert_eq!(
            heap.check(),
            Err(HeapError::Corrupt("a block's back pointer is wrong"))
        );
    }

    #[test]
    fn check_catches_a_free_block_that_is_missing_from_its_bin() {
        let mut region = vec![0u8; 1024];
        let mut heap = heap(&mut region);
        let _ = heap.allocate(100).expect("allocated");
        let remainder = heap.heads[bin_for(928)];
        assert_ne!(remainder, NOT_FREE);
        heap.heads[bin_for(928)] = NOT_FREE;
        assert_eq!(
            heap.check(),
            Err(HeapError::Corrupt("a free block is not on its bin list"))
        );
    }

    #[test]
    fn free_blocks_sit_in_the_bin_their_size_belongs_to() {
        let mut region = vec![0u8; 1 << 20];
        let mut heap = heap(&mut region);
        let payload = heap.allocate(100).expect("allocated");
        heap.deallocate(payload).expect("freed");
        let mut seen = false;
        for bin in 0..BINS {
            let mut current = heap.heads[bin];
            while current != NOT_FREE {
                assert_eq!(bin_for(heap.header_size(current)), bin);
                if current == 0 {
                    seen = true;
                }
                current = heap.next_free(current);
            }
        }
        assert!(seen, "the freed first block must be back on a list");
        assert_eq!(heap.check(), Ok(()));
        assert_eq!(heap.check_stats(), Ok(()));
    }

    #[test]
    fn random_allocations_and_frees_agree_with_a_model() {
        let mut rng = Rng::new(0x4E0F_2026);
        let mut region = vec![0u8; 64 * 1024];
        let mut heap = heap(&mut region);
        // The model: live payloads, with the byte written into each and its length.
        let mut live: Vec<(u32, u32, u8)> = Vec::new();

        for step in 0..4000 {
            if rng.chance(55) || live.is_empty() {
                let size = 1 + rng.below(1024) as usize;
                if let Some(payload) = heap.allocate(size) {
                    let usable = heap.usable_size(payload).expect("size");
                    assert!(usable as usize >= size);
                    assert_eq!(payload % ALIGNMENT, 0);
                    let marker = (step % 251) as u8;
                    heap.payload_mut(payload).expect("payload").fill(marker);
                    for (start, length, _) in &live {
                        let overlaps = payload < start + length && *start < payload + usable;
                        assert!(!overlaps, "allocation overlaps a live one at step {step}");
                    }
                    live.push((payload, usable, marker));
                }
            } else {
                let index = rng.below(live.len() as u64) as usize;
                let (payload, usable, marker) = live.swap_remove(index);
                assert!(
                    heap.payload(payload)
                        .expect("payload")
                        .iter()
                        .all(|byte| *byte == marker),
                    "a live payload changed at step {step}"
                );
                assert_eq!(heap.usable_size(payload), Ok(usable));
                heap.deallocate(payload).expect("freed");
            }
            assert_eq!(heap.check(), Ok(()), "invariants broken at step {step}");
            assert_eq!(heap.check_stats(), Ok(()), "counts wrong at step {step}");
        }

        for (payload, _, _) in live {
            heap.deallocate(payload).expect("freed");
        }
        assert_eq!(heap.stats().blocks, 1);
        assert_eq!(heap.stats().in_use, 0);
        assert_eq!(heap.check(), Ok(()));
    }
}
