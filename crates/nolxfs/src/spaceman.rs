//! The space manager: chunk-info blocks, allocation bitmaps, and the reaper.
//!
//! The container's data region (`Layout::first_data_block..block_count`) is divided
//! into chunks of at most 1024 blocks. Each chunk has one allocation bitmap block;
//! chunk descriptors (`nolx_chunk_info_t`) are packed into chunk-info blocks (CIBs),
//! and one chunk-info address block (CAB) points at the CIBs:
//!
//! ```text
//! spaceman (ephemeral object, pinned by the checkpoint map)
//!   -> CAB  (object header + count + CIB block addresses)
//!      -> CIBs (object header + count + nolx_chunk_info_t entries)
//!         -> bitmap blocks (object header + raw bits, bit set = allocated)
//! ```
//!
//! The bitmap/CIB/CAB blocks live at the start of the data region, so their addresses
//! are a deterministic function of the layout. The in-memory free-space queue —
//! coalesced `(start, len)` ranges derived from the bitmaps — is the allocation fast
//! path: allocation is first-fit over the queue, so freed ranges are reused before
//! untouched space, and freeing coalesces with neighbours. The bitmaps on disk are
//! the truth; the queue is a cache of them, and the tests assert they never disagree.
//!
//! The **reaper** holds blocks a transaction made garbage (replaced tree clones,
//! overwritten extents, unlinked files) tagged with the transaction that orphaned
//! them. A block returns to the bitmaps only once no live snapshot can still reach
//! it: an entry orphaned at `g` is freed when `g <= oldest live snapshot xid`, or
//! immediately when there are no snapshots.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::BlockDevice;
use crate::btree::BlockAllocator;
use crate::mkfs::{oid, Layout};
use crate::on_disk::{self, FsError, Result};
use crate::on_disk_gen as gen;
use crate::Pod;

/// Largest number of blocks described by one chunk; the value `mkfs` writes into
/// the spaceman object, so old and new readers agree on the geometry.
pub const MAX_BLOCKS_PER_CHUNK: u32 = 1024;

/// Where the space manager's own blocks live inside the data region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceLayout {
    /// First block of the data region the bitmaps describe.
    pub data_start: u64,
    /// Number of blocks in the data region.
    pub data_blocks: u64,
    /// Blocks covered by one chunk (and one bitmap block).
    pub blocks_per_chunk: u32,
    /// Number of chunks (and bitmap blocks).
    pub chunk_count: u64,
    /// First bitmap block; bitmap `i` is at `bitmap_base + i`.
    pub bitmap_base: u64,
    /// First chunk-info block.
    pub cib_base: u64,
    /// Number of chunk-info blocks.
    pub cib_count: u64,
    /// The chunk-info address block.
    pub cab_block: u64,
    /// First block the allocator may hand out (after bitmaps, CIBs, and the CAB).
    pub alloc_start: u64,
}

impl SpaceLayout {
    /// Compute the deterministic space-manager geometry for a formatted container.
    ///
    /// # Errors
    ///
    /// [`FsError::BlockOutOfRange`] if the data region cannot hold its own bitmaps.
    pub fn compute(layout: &Layout) -> Result<Self> {
        let block_size = layout.block_size;
        let data_start = layout.first_data_block;
        let data_blocks = layout
            .block_count
            .checked_sub(data_start)
            .ok_or(FsError::BlockOutOfRange(layout.block_count))?;
        if data_blocks == 0 {
            return Err(FsError::BlockOutOfRange(layout.block_count));
        }
        // One bitmap block must describe a whole chunk: its payload (after the
        // 32-byte object header) holds `(block_size - 32) * 8` block bits.
        let bits_per_bitmap = (block_size - gen::nolx_obj_phys_t::DISK_SIZE) * 8;
        let blocks_per_chunk =
            core::cmp::min(MAX_BLOCKS_PER_CHUNK as usize, bits_per_bitmap) as u32;
        let chunk_count = data_blocks.div_ceil(u64::from(blocks_per_chunk));
        let infos_per_cib =
            (block_size - gen::nolx_obj_phys_t::DISK_SIZE - 4) / gen::nolx_chunk_info_t::DISK_SIZE;
        if infos_per_cib == 0 {
            return Err(FsError::InvalidField(
                "block too small for a chunk-info block",
            ));
        }
        let cib_count = chunk_count.div_ceil(infos_per_cib as u64);
        let cab_capacity = (block_size - gen::nolx_obj_phys_t::DISK_SIZE - 4) / 8;
        if cib_count > cab_capacity as u64 {
            return Err(FsError::InvalidField("too many chunks for one CAB block"));
        }
        let reserved = chunk_count + cib_count + 1;
        let alloc_start = data_start + reserved;
        if alloc_start >= layout.block_count {
            return Err(FsError::BlockOutOfRange(layout.block_count));
        }
        Ok(Self {
            data_start,
            data_blocks,
            blocks_per_chunk,
            chunk_count,
            bitmap_base: data_start,
            cib_base: data_start + chunk_count,
            cib_count,
            cab_block: data_start + chunk_count + cib_count,
            alloc_start,
        })
    }

    /// Number of `nolx_chunk_info_t` entries that fit in one CIB block.
    #[must_use]
    pub fn infos_per_cib(&self, block_size: usize) -> usize {
        (block_size - gen::nolx_obj_phys_t::DISK_SIZE - 4) / gen::nolx_chunk_info_t::DISK_SIZE
    }
}

/// The space manager's live state: allocation bitmaps plus the free-space queue
/// derived from them.
#[derive(Clone, Debug)]
pub struct SpaceMan {
    space: SpaceLayout,
    block_size: usize,
    /// One bit per data-region block, set = allocated. Bits past `data_blocks`
    /// (padding to a whole chunk) are always set so they are never handed out.
    bits: Vec<u8>,
    free_count: u64,
}

impl SpaceMan {
    /// Build a fresh space manager in which only the space-manager blocks
    /// themselves are allocated, then write bitmaps, CIBs, CAB, and the spaceman
    /// object to `dev`.
    ///
    /// # Errors
    ///
    /// Device errors, or [`FsError::BlockOutOfRange`] for a device too small to
    /// describe itself.
    pub fn initialise<D: BlockDevice>(dev: &mut D, layout: &Layout, xid: u64) -> Result<Self> {
        let space = SpaceLayout::compute(layout)?;
        let total_bits = (space.chunk_count * u64::from(space.blocks_per_chunk)) as usize;
        let mut sm = Self {
            space,
            block_size: layout.block_size,
            bits: vec![0u8; total_bits.div_ceil(8)],
            free_count: 0,
        };
        for block in space.data_start..space.alloc_start {
            sm.set_bit(block, true);
        }
        let pad_start = space.data_start + space.data_blocks;
        let pad_end = space.data_start + total_bits as u64;
        for block in pad_start..pad_end {
            sm.set_bit(block, true);
        }
        sm.recount();
        sm.flush(dev, layout.spaceman, xid)?;
        Ok(sm)
    }

    /// Load the space manager written by [`SpaceMan::initialise`].
    ///
    /// # Errors
    ///
    /// [`FsError::InvalidField`] if the spaceman object was never initialised (a
    /// container straight from `mkfs`, before its first M8 mount), plus device and
    /// checksum errors for torn bitmap/CIB/CAB blocks.
    pub fn open<D: BlockDevice>(dev: &D, layout: &Layout) -> Result<Self> {
        let space = SpaceLayout::compute(layout)?;
        let phys: gen::nolx_spaceman_phys_t = dev.read_object(layout.spaceman)?;
        if phys.magic != gen::MAGIC_SPACEMAN {
            return Err(FsError::BadMagic {
                found: phys.magic,
                expected: gen::MAGIC_SPACEMAN,
            });
        }
        if phys.cab_addr_offset == 0 {
            return Err(FsError::InvalidField("space manager is not initialised"));
        }
        if phys.cab_addr_offset != space.cab_block || phys.cib_addr_offset != space.cib_base {
            return Err(FsError::InvalidField(
                "space manager geometry disagrees with the layout",
            ));
        }
        let cab = read_counted_block(dev, space.cab_block, gen::OBJ_SPACEMAN_CAB)?;
        let mut cib_blocks = Vec::new();
        for i in 0..(cab.0 as usize) {
            cib_blocks.push(
                read_u64(&cab.1, 36 + i * 8)
                    .ok_or(FsError::CorruptNode("CAB entry truncated"))?,
            );
        }
        if cib_blocks.len() as u64 != space.cib_count {
            return Err(FsError::InvalidField(
                "CAB lists the wrong number of CIBs",
            ));
        }
        let bytes_per_chunk = (space.blocks_per_chunk as usize).div_ceil(8);
        let mut sm = Self {
            space,
            block_size: layout.block_size,
            bits: vec![0u8; ((space.chunk_count as usize) * bytes_per_chunk)],
            free_count: 0,
        };
        let infos_per_cib = space.infos_per_cib(layout.block_size);
        for (cib_index, cib_block) in cib_blocks.iter().enumerate() {
            let (count, raw) = read_counted_block(dev, *cib_block, gen::OBJ_SPACEMAN_CIB)?;
            for j in 0..(count as usize) {
                let chunk = cib_index * infos_per_cib + j;
                if chunk as u64 >= space.chunk_count {
                    break;
                }
                let off = 36 + j * gen::nolx_chunk_info_t::DISK_SIZE;
                let info = gen::nolx_chunk_info_t::read_from(&raw[off..])
                    .ok_or(FsError::CorruptNode("chunk-info entry truncated"))?;
                if info.bitmap_addr != space.bitmap_base + chunk as u64 {
                    return Err(FsError::InvalidField(
                        "chunk-info bitmap address disagrees with the layout",
                    ));
                }
                let bitmap_raw =
                    read_verified_block(dev, info.bitmap_addr, gen::OBJ_SPACEMAN_BITMAP)?;
                let bitmap_body = &bitmap_raw[gen::nolx_obj_phys_t::DISK_SIZE..];
                if bytes_per_chunk > bitmap_body.len() {
                    return Err(FsError::InvalidField(
                        "chunk does not fit in its bitmap block",
                    ));
                }
                let dst = chunk * bytes_per_chunk;
                sm.bits[dst..dst + bytes_per_chunk]
                    .copy_from_slice(&bitmap_body[..bytes_per_chunk]);
            }
        }
        sm.recount();
        if sm.free_count != phys.free_count {
            return Err(FsError::InvalidField(
                "spaceman free_count disagrees with its bitmaps",
            ));
        }
        Ok(sm)
    }

    /// Whether `dev` already carries an initialised space manager.
    ///
    /// # Errors
    ///
    /// Device or checksum errors reading the spaceman object.
    pub fn is_initialised<D: BlockDevice>(dev: &D, layout: &Layout) -> Result<bool> {
        let phys: gen::nolx_spaceman_phys_t = dev.read_object(layout.spaceman)?;
        Ok(phys.cab_addr_offset != 0)
    }

    /// The deterministic geometry this manager was built with.
    #[must_use]
    pub const fn space_layout(&self) -> &SpaceLayout {
        &self.space
    }

    /// Blocks the allocator may still hand out.
    #[must_use]
    pub const fn free_count(&self) -> u64 {
        self.free_count
    }

    /// Total blocks described by the bitmaps.
    #[must_use]
    pub const fn total_count(&self) -> u64 {
        self.space.data_blocks
    }

    /// Whether `block` is currently allocated. Blocks outside the data region
    /// count as allocated: they are metadata, never the allocator's to hand out.
    #[must_use]
    pub fn is_allocated(&self, block: u64) -> bool {
        if block < self.space.data_start
            || block >= self.space.data_start + self.space.data_blocks
        {
            return true;
        }
        self.get_bit(block)
    }

    /// The free-space queue: coalesced `(start, len)` free ranges in address order.
    ///
    /// Derived from the bitmaps on demand; the bitmaps stay the source of truth,
    /// which is what lets `fsck` audit the queue against them.
    #[must_use]
    pub fn free_ranges(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut run_start: Option<u64> = None;
        for i in 0..self.space.data_blocks {
            let block = self.space.data_start + i;
            if self.get_bit(block) {
                if let Some(start) = run_start.take() {
                    out.push((start, block - start));
                }
            } else if run_start.is_none() {
                run_start = Some(block);
            }
        }
        if let Some(start) = run_start {
            out.push((start, self.space.data_start + self.space.data_blocks - start));
        }
        out
    }

    /// Allocate `count` contiguous blocks, first-fit over the free-space queue.
    ///
    /// # Errors
    ///
    /// [`FsError::BlockOutOfRange`] when no run is large enough (out of space),
    /// or [`FsError::InvalidField`] for a zero-length allocation.
    pub fn alloc(&mut self, count: u64) -> Result<u64> {
        if count == 0 {
            return Err(FsError::InvalidField("zero-length allocation"));
        }
        for (start, len) in self.free_ranges() {
            if len >= count && start >= self.space.alloc_start {
                for block in start..start + count {
                    self.set_bit(block, true);
                }
                self.free_count -= count;
                return Ok(start);
            }
        }
        Err(FsError::BlockOutOfRange(
            self.space.data_start + self.space.data_blocks,
        ))
    }

    /// Allocate one block.
    ///
    /// # Errors
    ///
    /// As [`SpaceMan::alloc`].
    pub fn alloc_block(&mut self) -> Result<u64> {
        self.alloc(1)
    }

    /// Return `count` blocks starting at `start` to the free-space queue.
    ///
    /// # Errors
    ///
    /// [`FsError::InvalidField`] for blocks outside the allocatable region or a
    /// double free — caller bugs that must not silently corrupt the queue.
    pub fn free(&mut self, start: u64, count: u64) -> Result<()> {
        if count == 0 {
            return Err(FsError::InvalidField("zero-length free"));
        }
        let end = start.checked_add(count).ok_or(FsError::BlockOutOfRange(start))?;
        if start < self.space.alloc_start
            || end > self.space.data_start + self.space.data_blocks
        {
            return Err(FsError::InvalidField(
                "free outside the allocatable region",
            ));
        }
        for block in start..end {
            if !self.get_bit(block) {
                return Err(FsError::InvalidField("double free"));
            }
        }
        for block in start..end {
            self.set_bit(block, false);
        }
        self.free_count += count;
        Ok(())
    }

    /// Mark `start..start+count` allocated. Used by `fsck` repair when it
    /// rebuilds bitmaps from the reachable set.
    ///
    /// # Errors
    ///
    /// [`FsError::InvalidField`] outside the data region.
    pub fn mark_allocated(&mut self, start: u64, count: u64) -> Result<()> {
        let end = start.checked_add(count).ok_or(FsError::BlockOutOfRange(start))?;
        if start < self.space.data_start
            || end > self.space.data_start + self.space.data_blocks
        {
            return Err(FsError::InvalidField("mark outside the data region"));
        }
        for block in start..end {
            if !self.get_bit(block) {
                self.set_bit(block, true);
                self.free_count = self.free_count.saturating_sub(1);
            }
        }
        Ok(())
    }

    /// Clear every allocation bit except the space-manager blocks themselves.
    /// Used by `fsck` repair before it re-marks the reachable set.
    pub fn reset_allocations(&mut self) {
        for byte in self.bits.iter_mut() {
            *byte = 0;
        }
        for block in self.space.data_start..self.space.alloc_start {
            self.set_bit(block, true);
        }
        let total_bits = self.space.chunk_count * u64::from(self.space.blocks_per_chunk);
        for block in (self.space.data_start + self.space.data_blocks)
            ..(self.space.data_start + total_bits)
        {
            self.set_bit(block, true);
        }
        self.recount();
    }

    /// Write bitmap blocks, CIBs, the CAB, and the spaceman object, in that order.
    ///
    /// The spaceman object is written last among the space manager's own
    /// structures; the checkpoint descriptor (written by the caller afterwards)
    /// is what makes the new bitmaps authoritative, so a crash here only leaks
    /// blocks, which `fsck` reclaims.
    ///
    /// # Errors
    ///
    /// Device errors.
    pub fn flush<D: BlockDevice>(&self, dev: &mut D, spaceman_block: u64, xid: u64) -> Result<()> {
        let block_size = self.block_size;
        let bytes_per_chunk = (self.space.blocks_per_chunk as usize).div_ceil(8);
        for chunk in 0..self.space.chunk_count {
            let mut buf = vec![0u8; block_size];
            let src_off = (chunk as usize) * bytes_per_chunk;
            buf[gen::nolx_obj_phys_t::DISK_SIZE
                ..gen::nolx_obj_phys_t::DISK_SIZE + bytes_per_chunk]
                .copy_from_slice(&self.bits[src_off..src_off + bytes_per_chunk]);
            on_disk::seal(
                &mut buf,
                on_disk::ObjHeader::make(chunk, xid, gen::OBJ_SPACEMAN_BITMAP, 0),
            )?;
            dev.write_block(self.space.bitmap_base + chunk, &buf)?;
        }
        let infos_per_cib = self.space.infos_per_cib(block_size);
        for cib_index in 0..self.space.cib_count {
            let first_chunk = (cib_index as usize) * infos_per_cib;
            let last_chunk = core::cmp::min(
                first_chunk + infos_per_cib,
                self.space.chunk_count as usize,
            );
            let mut buf = vec![0u8; block_size];
            buf[32..36].copy_from_slice(&((last_chunk - first_chunk) as u32).to_le_bytes());
            for (slot, chunk) in (first_chunk..last_chunk).enumerate() {
                let info = gen::nolx_chunk_info_t {
                    xid,
                    addr: self.space.data_start
                        + (chunk as u64) * u64::from(self.space.blocks_per_chunk),
                    free_count: self.chunk_free_count(chunk as u64) as u32,
                    bitmap_len: self.space.blocks_per_chunk,
                    bitmap_addr: self.space.bitmap_base + chunk as u64,
                };
                let off = 32 + 4 + slot * gen::nolx_chunk_info_t::DISK_SIZE;
                buf[off..off + gen::nolx_chunk_info_t::DISK_SIZE]
                    .copy_from_slice(info.as_bytes());
            }
            on_disk::seal(
                &mut buf,
                on_disk::ObjHeader::make(cib_index, xid, gen::OBJ_SPACEMAN_CIB, 0),
            )?;
            dev.write_block(self.space.cib_base + cib_index, &buf)?;
        }
        {
            let mut buf = vec![0u8; block_size];
            buf[32..36].copy_from_slice(&(self.space.cib_count as u32).to_le_bytes());
            for i in 0..self.space.cib_count {
                let off = 32 + 4 + (i as usize) * 8;
                buf[off..off + 8].copy_from_slice(&(self.space.cib_base + i).to_le_bytes());
            }
            on_disk::seal(
                &mut buf,
                on_disk::ObjHeader::make(0, xid, gen::OBJ_SPACEMAN_CAB, 0),
            )?;
            dev.write_block(self.space.cab_block, &buf)?;
        }
        {
            let mut phys = gen::nolx_spaceman_phys_t::zeroed();
            phys.magic = gen::MAGIC_SPACEMAN;
            phys.block_size = block_size as u32;
            phys.blocks_per_chunk = self.space.blocks_per_chunk;
            phys.chunks_per_cib = infos_per_cib as u32;
            phys.cib_count = self.space.cib_count as u32;
            phys.bm_block_count = self.space.chunk_count;
            phys.blocks_reserved = self.space.alloc_start;
            phys.free_count = self.free_count;
            phys.cib_addr_offset = self.space.cib_base;
            phys.cab_addr_offset = self.space.cab_block;
            write_sealed_block(dev, spaceman_block, &phys, oid::SPACEMAN, xid, gen::OBJ_SPACEMAN)?;
        }
        Ok(())
    }

    fn get_bit(&self, block: u64) -> bool {
        let index = (block - self.space.data_start) as usize;
        self.bits[index / 8] & (1u8 << (index % 8)) != 0
    }

    fn set_bit(&mut self, block: u64, allocated: bool) {
        let index = (block - self.space.data_start) as usize;
        if allocated {
            self.bits[index / 8] |= 1u8 << (index % 8);
        } else {
            self.bits[index / 8] &= !(1u8 << (index % 8));
        }
    }

    fn chunk_free_count(&self, chunk: u64) -> u64 {
        let start = self.space.data_start + chunk * u64::from(self.space.blocks_per_chunk);
        let end = core::cmp::min(
            start + u64::from(self.space.blocks_per_chunk),
            self.space.data_start + self.space.data_blocks,
        );
        let mut free = 0u64;
        for block in start..end {
            if !self.get_bit(block) {
                free += 1;
            }
        }
        free
    }

    fn recount(&mut self) {
        let mut free = 0u64;
        for i in 0..self.space.data_blocks {
            if !self.get_bit(self.space.data_start + i) {
                free += 1;
            }
        }
        self.free_count = free;
    }
}

fn write_sealed_block<D: BlockDevice, T: Pod>(
    dev: &mut D,
    block: u64,
    value: &T,
    oid: u64,
    xid: u64,
    obj_type: u32,
) -> Result<()> {
    if core::mem::size_of::<T>() > dev.block_size() {
        return Err(FsError::TooShort {
            have: dev.block_size(),
            need: core::mem::size_of::<T>(),
        });
    }
    let mut buf = vec![0u8; dev.block_size()];
    buf[..core::mem::size_of::<T>()].copy_from_slice(value.as_bytes());
    on_disk::seal(&mut buf, on_disk::ObjHeader::make(oid, xid, obj_type, 0))?;
    dev.write_block(block, &buf)?;
    Ok(())
}

/// Read a block, verify its Fletcher-64 checksum and object type, and return the
/// raw block.
fn read_verified_block<D: BlockDevice>(dev: &D, block: u64, expected: u32) -> Result<Vec<u8>> {
    if block >= dev.block_count() {
        return Err(FsError::BlockOutOfRange(block));
    }
    let mut raw = vec![0u8; dev.block_size()];
    dev.read_block(block, &mut raw)?;
    on_disk::ObjRef::parse(&raw, expected)?;
    Ok(raw)
}

/// Read a counted space-manager block, returning `(count, raw_block)`; the count
/// is the `u32` immediately after the object header.
fn read_counted_block<D: BlockDevice>(
    dev: &D,
    block: u64,
    expected: u32,
) -> Result<(u32, Vec<u8>)> {
    let raw = read_verified_block(dev, block, expected)?;
    if raw.len() < 36 {
        return Err(FsError::TooShort {
            have: raw.len(),
            need: 36,
        });
    }
    let count = u32::from_le_bytes(
        raw[32..36]
            .try_into()
            .map_err(|_| FsError::CorruptNode("count truncated"))?,
    );
    Ok((count, raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// A [`BlockAllocator`] over a [`SpaceMan`] that defers every freed block to a
/// caller-owned list instead of freeing it immediately. The volume turns that
/// list into reaper entries at commit time, so a block freed by a B-tree merge
/// inside a transaction cannot be reused while a snapshot might still
/// reference the tree it came from.
pub struct SpaceAllocator<'a> {
    /// The space manager to allocate from.
    pub spaceman: &'a mut SpaceMan,
    /// Blocks freed through this allocator, as `(block, len)` ranges.
    pub deferred: &'a mut Vec<(u64, u64)>,
}

impl BlockAllocator for SpaceAllocator<'_> {
    fn alloc(&mut self) -> Result<u64> {
        self.spaceman.alloc_block()
    }

    fn free(&mut self, block: u64) {
        self.deferred.push((block, 1));
    }
}

/// One deferred free: `len` blocks at `block`, orphaned by transaction
/// `garbage_xid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReapEntry {
    /// First orphaned block.
    pub block: u64,
    /// Number of orphaned blocks.
    pub len: u64,
    /// Transaction that made the range garbage.
    pub garbage_xid: u64,
}

/// The reaper: deferred frees awaiting the snapshot horizon.
///
/// Entries persist inside the checkpoint state (see `checkpoint`), and the
/// on-disk reaper object's `record_count` mirrors the entry count at every
/// commit so an external reader can see the backlog without decoding a
/// checkpoint.
#[derive(Clone, Debug, Default)]
pub struct Reaper {
    entries: Vec<ReapEntry>,
}

impl Reaper {
    /// An empty reaper.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Rebuild from persisted entries (checkpoint state).
    #[must_use]
    pub fn from_entries(entries: Vec<ReapEntry>) -> Self {
        Self { entries }
    }

    /// The pending entries, in deferral order.
    #[must_use]
    pub fn entries(&self) -> &[ReapEntry] {
        &self.entries
    }

    /// Replace the pending entries (checkpoint state restore).
    pub fn set_entries(&mut self, entries: Vec<ReapEntry>) {
        self.entries = entries;
    }

    /// Number of blocks waiting to be reaped.
    #[must_use]
    pub fn pending_blocks(&self) -> u64 {
        self.entries.iter().map(|e| e.len).sum()
    }

    /// Record `len` blocks at `block` as garbage from transaction `xid`.
    pub fn defer(&mut self, block: u64, len: u64, xid: u64) {
        if len == 0 {
            return;
        }
        self.entries.push(ReapEntry {
            block,
            len,
            garbage_xid: xid,
        });
    }

    /// Free every entry no live view can still reach.
    ///
    /// `oldest_snapshot_xid` is the smallest xid pinned by a live snapshot, or
    /// `None` when there are no snapshots. An entry orphaned at `g` is still
    /// reachable from a snapshot taken before `g`, so it is freed exactly when
    /// `g <= oldest_snapshot_xid` (or always, with no snapshots). Returns the
    /// number of blocks returned to `spaceman`.
    ///
    /// # Errors
    ///
    /// Whatever [`SpaceMan::free`] reports; entries already freed stay freed
    /// and are removed, the failing entry remains queued.
    pub fn reap(
        &mut self,
        spaceman: &mut SpaceMan,
        oldest_snapshot_xid: Option<u64>,
    ) -> Result<u64> {
        let horizon = oldest_snapshot_xid.unwrap_or(u64::MAX);
        let mut freed = 0u64;
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in core::mem::take(&mut self.entries) {
            if entry.garbage_xid <= horizon {
                spaceman.free(entry.block, entry.len)?;
                freed += entry.len;
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
        Ok(freed)
    }
}

/// Snapshot the free-space queue as a map, for tests and `fsck` audits that
/// compare the queue against an independently walked reachable set.
#[must_use]
pub fn ranges_to_map(ranges: &[(u64, u64)]) -> BTreeMap<u64, u64> {
    let mut map = BTreeMap::new();
    for &(start, len) in ranges {
        map.insert(start, len);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;
    use crate::mkfs::{format, FormatOptions};

    fn formatted() -> (MemBlockDevice, Layout) {
        let mut dev = MemBlockDevice::new(4096, 2048);
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();
        (dev, layout)
    }

    #[test]
    fn layout_is_deterministic_and_reserves_its_own_blocks() {
        let (_, layout) = formatted();
        let space = SpaceLayout::compute(&layout).unwrap();
        assert_eq!(space, SpaceLayout::compute(&layout).unwrap());
        assert_eq!(space.bitmap_base, layout.first_data_block);
        assert!(space.alloc_start > space.cab_block);
        assert!(space.alloc_start < layout.block_count);
    }

    #[test]
    fn initialise_then_open_round_trips() {
        let (mut dev, layout) = formatted();
        assert!(!SpaceMan::is_initialised(&dev, &layout).unwrap());
        let sm = SpaceMan::initialise(&mut dev, &layout, 2).unwrap();
        assert!(SpaceMan::is_initialised(&dev, &layout).unwrap());
        let opened = SpaceMan::open(&dev, &layout).unwrap();
        assert_eq!(opened.free_count(), sm.free_count());
        assert_eq!(opened.free_ranges(), sm.free_ranges());
    }

    #[test]
    fn allocation_is_first_fit_and_freed_ranges_are_reused() {
        let (mut dev, layout) = formatted();
        let mut sm = SpaceMan::initialise(&mut dev, &layout, 2).unwrap();
        let a = sm.alloc(4).unwrap();
        let b = sm.alloc(4).unwrap();
        assert_eq!(b, a + 4);
        sm.free(a, 4).unwrap();
        let c = sm.alloc(2).unwrap();
        assert_eq!(c, a, "freed range must be reused before fresh space");
        let ranges = sm.free_ranges();
        assert!(ranges.iter().all(|&(s, l)| s + l <= layout.block_count));
    }

    #[test]
    fn double_free_and_out_of_region_free_are_errors() {
        let (mut dev, layout) = formatted();
        let mut sm = SpaceMan::initialise(&mut dev, &layout, 2).unwrap();
        let a = sm.alloc_block().unwrap();
        sm.free(a, 1).unwrap();
        assert!(sm.free(a, 1).is_err(), "double free must be caught");
        assert!(sm.free(0, 1).is_err(), "metadata blocks are not freeable");
        assert!(sm.alloc(0).is_err());
    }

    #[test]
    fn out_of_space_is_an_error_not_a_panic() {
        let (mut dev, layout) = formatted();
        let mut sm = SpaceMan::initialise(&mut dev, &layout, 2).unwrap();
        let total = sm.free_count();
        assert!(sm.alloc(total + 1).is_err());
        let all = sm.alloc(total).unwrap();
        assert_eq!(sm.free_count(), 0);
        assert!(sm.alloc_block().is_err());
        sm.free(all, total).unwrap();
        assert_eq!(sm.free_count(), total);
    }

    #[test]
    fn reaper_respects_the_snapshot_horizon() {
        let (mut dev, layout) = formatted();
        let mut sm = SpaceMan::initialise(&mut dev, &layout, 2).unwrap();
        let a = sm.alloc(2).unwrap();
        let b = sm.alloc(2).unwrap();
        let mut reaper = Reaper::new();
        reaper.defer(a, 2, 5);
        reaper.defer(b, 2, 9);
        // The range orphaned at 5 was already garbage when the xid-7 snapshot
        // was taken, so it can go; the range orphaned at 9 is what the
        // snapshot still reads, so it must stay.
        assert_eq!(reaper.reap(&mut sm, Some(7)).unwrap(), 2);
        assert!(sm.is_allocated(b), "post-snapshot garbage must be kept");
        assert!(!sm.is_allocated(a));
        assert_eq!(reaper.reap(&mut sm, None).unwrap(), 2);
        assert!(!sm.is_allocated(b));
    }
}
