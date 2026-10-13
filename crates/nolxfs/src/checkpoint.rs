//! Checkpointing: the container's crash-recovery and atomic-commit mechanism.
//!
//! There is no journal replay in NOLXFS. A transaction becomes visible at exactly
//! one moment: when its **checkpoint descriptor** lands in the descriptor ring.
//! Everything the transaction changed — B-tree clones, data extents, space-manager
//! bitmaps, superblock copies — is written to fresh or ring-slotted blocks first
//! and flushed; the descriptor, one block in the ring, is written last. A power
//! cut before it leaves the previous descriptor authoritative; a torn descriptor
//! fails its Fletcher-64 checksum and is skipped. Mount is therefore a scan:
//!
//! ```text
//! block 0 -> checkpoint descriptor ring -> newest descriptor whose checksum,
//!            superblock copies, and payload all verify -> that transaction's state
//! ```
//!
//! The descriptor is a `nolx_checkpoint_map_phys_t` pinning the ephemeral objects
//! (spaceman, reaper) and the checkpoint's own superblock copies, which live in
//! the checkpoint data ring in per-transaction slots so committing transaction
//! `x+1` never overwrites the copies transaction `x`'s descriptor still needs.
//! The volume's transactional state (tree roots, counters, snapshots, reaper
//! backlog) is encoded into the volume superblock copy's reserved tail, spilling
//! into space-manager-allocated payload blocks when it outgrows one block.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::BlockDevice;
use crate::mkfs::{oid, Layout};
use crate::on_disk::{self, FsError, Result};
use crate::on_disk_gen as gen;
use crate::spaceman::ReapEntry;
use crate::Pod;

/// Magic at the start of the encoded checkpoint state (`"N8CP"`).
pub const STATE_MAGIC: u32 = 0x5043_384E;
/// Version of the checkpoint state encoding.
pub const STATE_VERSION: u16 = 1;
/// Bytes of state header before the snapshot and reaper entries.
pub const STATE_HEADER_LEN: usize = 128;
/// Offset of the volume superblock's reserved tail inside its block, where the
/// checkpoint state stream begins. The generated superblock packs to 608 bytes;
/// the rest of its 4096-byte block is reserved.
pub const VSB_RESERVED_OFF: usize = gen::nolx_fs_superblock_t::OFF_RESERVED;
/// Where the state stream itself starts: the first eight reserved bytes hold
/// the spill-chain head block (0 when the stream fits inline).
pub const STATE_OFF: usize = VSB_RESERVED_OFF + 8;

/// One snapshot, as recorded in the checkpoint state and mirrored into the
/// snapshot metadata tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// Transaction the snapshot pins; the point-in-time view is the filesystem
    /// as committed by this transaction.
    pub xid: u64,
    /// User-visible snapshot name.
    pub name: String,
    /// Physical fs-tree root at `xid`.
    pub fs_root: u64,
    /// Physical extent-reference tree root at `xid`.
    pub extentref_root: u64,
    /// Physical snapshot metadata tree root at `xid`.
    pub snapmeta_root: u64,
    /// Physical fast-extent tree root at `xid`, or 0 when absent.
    pub fext_root: u64,
    /// Files counted in the snapshot's tree at creation time.
    pub num_files: u64,
    /// Directories counted at creation time.
    pub num_dirs: u64,
    /// Symlinks counted at creation time.
    pub num_symlinks: u64,
    /// Other objects counted at creation time.
    pub num_other: u64,
    /// Creation time (the volume's logical clock: the creating xid).
    pub create_time: i64,
}

/// Everything a mount needs to resume a volume: the state committed by one
/// transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointState {
    /// The transaction that committed this state.
    pub xid: u64,
    /// Physical fs-tree root block.
    pub fs_root: u64,
    /// Physical extent-reference tree root block.
    pub extentref_root: u64,
    /// Physical snapshot metadata tree root block.
    pub snapmeta_root: u64,
    /// Physical fast-extent tree root block, or 0 when the volume has none.
    pub fext_root: u64,
    /// Next inode number to hand out.
    pub next_ino: u64,
    /// Next object id to hand out.
    pub next_oid: u64,
    /// Next document id for `FILE_INFO` records.
    pub next_doc_id: u64,
    /// Live regular files.
    pub num_files: u64,
    /// Live directories.
    pub num_dirs: u64,
    /// Live symlinks.
    pub num_symlinks: u64,
    /// Live other objects (fifos, sockets, devices).
    pub num_other: u64,
    /// Cumulative blocks allocated since format.
    pub total_blocks_alloced: u64,
    /// Cumulative blocks freed since format.
    pub total_blocks_freed: u64,
    /// Live snapshots, oldest first.
    pub snapshots: Vec<SnapshotRecord>,
    /// Reaper backlog at commit time.
    pub reap: Vec<ReapEntry>,
}

impl CheckpointState {
    /// The xid of the oldest live snapshot, which is the reaper's horizon.
    #[must_use]
    pub fn oldest_snapshot_xid(&self) -> Option<u64> {
        self.snapshots.iter().map(|s| s.xid).min()
    }

    /// Look up a snapshot by name.
    #[must_use]
    pub fn snapshot_named(&self, name: &str) -> Option<&SnapshotRecord> {
        self.snapshots.iter().find(|s| s.name == name)
    }
}

/// A checkpoint found in the ring, with the blocks that carry it.
#[derive(Clone, Debug)]
pub struct FoundCheckpoint {
    /// The decoded state.
    pub state: CheckpointState,
    /// Descriptor ring block holding this checkpoint.
    pub desc_block: u64,
    /// Checkpoint data block holding the container superblock copy.
    pub csb_block: u64,
    /// Checkpoint data block holding the volume superblock copy.
    pub vsb_block: u64,
}

// --------------------------------------------------------------------- encoding

/// Serialise `state`. The encoding is a fixed 128-byte header followed by
/// snapshot records (fixed part plus name) and reaper entries; every length is
/// checked again on decode, so a torn payload cannot be misread as a longer one.
///
/// # Errors
///
/// Never fails for a well-formed state; the result is a byte vector.
pub fn encode_state(state: &CheckpointState) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(STATE_HEADER_LEN + state.snapshots.len() * 96 + state.reap.len() * 24);
    out.extend_from_slice(&STATE_MAGIC.to_le_bytes());
    out.extend_from_slice(&STATE_VERSION.to_le_bytes());
    out.extend_from_slice(&(STATE_HEADER_LEN as u16).to_le_bytes());
    out.extend_from_slice(&state.xid.to_le_bytes());
    for v in [
        state.fs_root,
        state.extentref_root,
        state.snapmeta_root,
        state.fext_root,
        state.next_ino,
        state.next_oid,
        state.next_doc_id,
        state.num_files,
        state.num_dirs,
        state.num_symlinks,
        state.num_other,
        state.total_blocks_alloced,
        state.total_blocks_freed,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(state.snapshots.len() as u32).to_le_bytes());
    out.extend_from_slice(&(state.reap.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), STATE_HEADER_LEN);
    for snap in &state.snapshots {
        for v in [
            snap.xid,
            snap.fs_root,
            snap.extentref_root,
            snap.snapmeta_root,
            snap.fext_root,
            snap.num_files,
            snap.num_dirs,
            snap.num_symlinks,
            snap.num_other,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&snap.create_time.to_le_bytes());
        out.extend_from_slice(&(snap.name.len() as u16).to_le_bytes());
        out.extend_from_slice(snap.name.as_bytes());
        while out.len() % 8 != 0 {
            out.push(0);
        }
    }
    for entry in &state.reap {
        out.extend_from_slice(&entry.block.to_le_bytes());
        out.extend_from_slice(&entry.len.to_le_bytes());
        out.extend_from_slice(&entry.garbage_xid.to_le_bytes());
    }
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes(
            b.try_into()
                .map_err(|_| FsError::CorruptNode("state truncated"))?,
        ))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(
            b.try_into()
                .map_err(|_| FsError::CorruptNode("state truncated"))?,
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(
            b.try_into()
                .map_err(|_| FsError::CorruptNode("state truncated"))?,
        ))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(FsError::CorruptNode("state length overflows"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(FsError::CorruptNode("state truncated"))?;
        self.pos = end;
        Ok(slice)
    }
    fn align8(&mut self) -> Result<()> {
        let skip = (8 - (self.pos % 8)) % 8;
        self.take(skip)?;
        Ok(())
    }
}

/// Parse a state stream produced by [`encode_state`].
///
/// # Errors
///
/// [`FsError::CorruptNode`] for bad magic/version or any truncated field; a
/// checkpoint candidate that fails here is skipped by the ring scan, not fatal.
pub fn decode_state(bytes: &[u8]) -> Result<CheckpointState> {
    let mut cur = Cursor { bytes, pos: 0 };
    if cur.u32()? != STATE_MAGIC {
        return Err(FsError::CorruptNode("checkpoint state magic mismatch"));
    }
    if cur.u16()? != STATE_VERSION {
        return Err(FsError::CorruptNode("checkpoint state version mismatch"));
    }
    let _header_len = cur.u16()?;
    let xid = cur.u64()?;
    let fs_root = cur.u64()?;
    let extentref_root = cur.u64()?;
    let snapmeta_root = cur.u64()?;
    let fext_root = cur.u64()?;
    let next_ino = cur.u64()?;
    let next_oid = cur.u64()?;
    let next_doc_id = cur.u64()?;
    let num_files = cur.u64()?;
    let num_dirs = cur.u64()?;
    let num_symlinks = cur.u64()?;
    let num_other = cur.u64()?;
    let total_blocks_alloced = cur.u64()?;
    let total_blocks_freed = cur.u64()?;
    let snap_count = cur.u32()? as usize;
    let reap_count = cur.u32()? as usize;
    if snap_count > 100_000 || reap_count > 1_000_000 {
        return Err(FsError::CorruptNode(
            "checkpoint state counts are implausible",
        ));
    }
    let mut snapshots = Vec::with_capacity(snap_count);
    for _ in 0..snap_count {
        let s_xid = cur.u64()?;
        let s_fs = cur.u64()?;
        let s_ext = cur.u64()?;
        let s_snap = cur.u64()?;
        let s_fext = cur.u64()?;
        let n_files = cur.u64()?;
        let n_dirs = cur.u64()?;
        let n_links = cur.u64()?;
        let n_other = cur.u64()?;
        let create_time = cur.i64()?;
        let name_len = cur.u16()? as usize;
        if name_len > gen::MAX_VOLUME_NAME_LEN as usize + 255 {
            return Err(FsError::CorruptNode("snapshot name is implausibly long"));
        }
        let name_bytes = cur.take(name_len)?;
        let name = String::from(
            core::str::from_utf8(name_bytes)
                .map_err(|_| FsError::CorruptNode("snapshot name is not UTF-8"))?,
        );
        cur.align8()?;
        snapshots.push(SnapshotRecord {
            xid: s_xid,
            name,
            fs_root: s_fs,
            extentref_root: s_ext,
            snapmeta_root: s_snap,
            fext_root: s_fext,
            num_files: n_files,
            num_dirs: n_dirs,
            num_symlinks: n_links,
            num_other: n_other,
            create_time,
        });
    }
    let mut reap = Vec::with_capacity(reap_count);
    for _ in 0..reap_count {
        reap.push(ReapEntry {
            block: cur.u64()?,
            len: cur.u64()?,
            garbage_xid: cur.u64()?,
        });
    }
    Ok(CheckpointState {
        xid,
        fs_root,
        extentref_root,
        snapmeta_root,
        fext_root,
        next_ino,
        next_oid,
        next_doc_id,
        num_files,
        num_dirs,
        num_symlinks,
        num_other,
        total_blocks_alloced,
        total_blocks_freed,
        snapshots,
        reap,
    })
}

// --------------------------------------------------------------------- the ring

/// Bytes of payload one spill block carries (after its object header and its
/// 16-byte spill header).
fn spill_payload(block_size: usize) -> usize {
    block_size - gen::nolx_obj_phys_t::DISK_SIZE - 16
}

/// How many space-manager blocks `commit` needs for the part of `state` that
/// does not fit in the volume superblock copy's reserved tail.
#[must_use]
pub fn spill_blocks_needed(state: &CheckpointState, block_size: usize) -> usize {
    let total = encode_state(state).len();
    let inline = block_size - STATE_OFF;
    if total <= inline {
        0
    } else {
        (total - inline).div_ceil(spill_payload(block_size))
    }
}

/// Commit `state` as transaction `state.xid`: superblock copies and spill
/// payload first, descriptor last. This is the atomic write batch — the single
/// descriptor write is the commit point, and both flushes order it after the
/// data it makes visible.
///
/// `spill_blocks` must hold at least [`spill_blocks_needed`] freshly allocated
/// blocks; they become part of the checkpoint and are reclaimed by the reaper
/// discipline of later transactions (the volume defers the previous
/// checkpoint's spill blocks when it commits the next one).
///
/// # Errors
///
/// Device errors. On error the volume must be considered unmounted: some
/// pre-commit blocks may be written, but no descriptor points at them.
#[allow(clippy::too_many_arguments)]
pub fn commit<D: BlockDevice>(
    dev: &mut D,
    layout: &Layout,
    csb_template: &gen::nolx_nx_superblock_t,
    vsb_template: &gen::nolx_fs_superblock_t,
    state: &CheckpointState,
    spill_blocks: &[u64],
) -> Result<()> {
    let block_size = dev.block_size();
    if block_size < gen::nolx_nx_superblock_t::DISK_SIZE {
        return Err(FsError::TooShort {
            have: block_size,
            need: gen::nolx_nx_superblock_t::DISK_SIZE,
        });
    }
    let xid = state.xid;
    let stream = encode_state(state);
    let inline_cap = block_size - STATE_OFF;
    let need_spill = spill_blocks_needed(state, block_size);
    if spill_blocks.len() < need_spill {
        return Err(FsError::InvalidField(
            "not enough spill blocks for the checkpoint state",
        ));
    }

    // 1. Spill payload blocks (checkpoint data in allocated space).
    if need_spill > 0 {
        let rest = &stream[inline_cap..];
        for (seq, chunk) in rest.chunks(spill_payload(block_size)).enumerate() {
            let mut buf = vec![0u8; block_size];
            buf[32..36].copy_from_slice(&STATE_MAGIC.to_le_bytes());
            buf[36..40].copy_from_slice(&(seq as u32).to_le_bytes());
            let next = spill_blocks.get(seq + 1).copied().unwrap_or(0);
            buf[40..48].copy_from_slice(&next.to_le_bytes());
            buf[48..48 + chunk.len()].copy_from_slice(chunk);
            on_disk::seal(
                &mut buf,
                on_disk::ObjHeader::make(xid, xid, gen::OBJ_CHECKPOINT_MAP, 1),
            )?;
            dev.write_block(spill_blocks[seq], &buf)?;
        }
    }

    // 2. Superblock copies in this transaction's data-ring slot.
    let pairs = core::cmp::max(1, layout.xp_data_blocks / 2);
    let pair = xid % u64::from(pairs);
    let csb_block = layout.xp_data_base + pair * 2;
    let vsb_block = csb_block + 1;

    let mut vsb = *vsb_template;
    vsb.num_files = state.num_files;
    vsb.num_directories = state.num_dirs;
    vsb.num_symlinks = state.num_symlinks;
    vsb.num_other_fsobjects = state.num_other;
    vsb.num_snapshots = state.snapshots.len() as u64;
    vsb.total_blocks_alloced = state.total_blocks_alloced;
    vsb.total_blocks_freed = state.total_blocks_freed;
    vsb.next_ident = state.next_ino;
    {
        let mut buf = vec![0u8; block_size];
        buf[..core::mem::size_of::<gen::nolx_fs_superblock_t>()].copy_from_slice(vsb.as_bytes());
        // State header records where the spill chain starts (0 = fully inline).
        let spill_first = spill_blocks.first().copied().unwrap_or(0);
        buf[VSB_RESERVED_OFF..VSB_RESERVED_OFF + 8].copy_from_slice(&spill_first.to_le_bytes());
        let inline_len = core::cmp::min(stream.len(), inline_cap);
        buf[STATE_OFF..STATE_OFF + inline_len].copy_from_slice(&stream[..inline_len]);
        on_disk::seal(
            &mut buf,
            on_disk::ObjHeader::make(oid::VOLUME_SUPERBLOCK, xid, gen::OBJ_VOLUME_SUPERBLOCK, 0),
        )?;
        dev.write_block(vsb_block, &buf)?;
    }

    let desc_index = (xid % u64::from(layout.xp_desc_blocks)) as u32;
    let mut csb = *csb_template;
    csb.next_xid = xid + 1;
    csb.next_oid = state.next_oid;
    csb.xp_desc_index = desc_index;
    csb.xp_desc_next = (desc_index + 1) % layout.xp_desc_blocks;
    {
        let mut buf = vec![0u8; block_size];
        buf[..core::mem::size_of::<gen::nolx_nx_superblock_t>()].copy_from_slice(csb.as_bytes());
        on_disk::seal(
            &mut buf,
            on_disk::ObjHeader::make(
                oid::CONTAINER_SUPERBLOCK,
                xid,
                gen::OBJ_CONTAINER_SUPERBLOCK,
                0,
            ),
        )?;
        dev.write_block(csb_block, &buf)?;
    }

    // 2b. Reaper mirror: the backlog count, readable without decoding state.
    {
        let mut reaper = gen::nolx_reaper_phys_t::zeroed();
        reaper.record_count = state.reap.len() as u64;
        reaper.max_record_count = state.reap.len() as u64;
        reaper.xid = xid;
        let mut buf = vec![0u8; block_size];
        buf[..core::mem::size_of::<gen::nolx_reaper_phys_t>()].copy_from_slice(reaper.as_bytes());
        on_disk::seal(
            &mut buf,
            on_disk::ObjHeader::make(oid::REAPER, xid, gen::OBJ_REAPER, 0),
        )?;
        dev.write_block(layout.reaper, &buf)?;
    }

    dev.flush()?;

    // 3. The descriptor: the commit point.
    let desc_block = layout.xp_desc_base + u64::from(desc_index);
    {
        let mut buf = vec![0u8; block_size];
        let mut map = gen::nolx_checkpoint_map_phys_t::zeroed();
        map.magic = gen::MAGIC_CHECKPOINT_MAP;
        map.num_entries = 4;
        buf[..core::mem::size_of::<gen::nolx_checkpoint_map_phys_t>()]
            .copy_from_slice(map.as_bytes());
        let entries: [(u32, u64, u64); 4] = [
            (
                gen::OBJ_CONTAINER_SUPERBLOCK,
                oid::CONTAINER_SUPERBLOCK,
                csb_block,
            ),
            (
                gen::OBJ_VOLUME_SUPERBLOCK,
                oid::VOLUME_SUPERBLOCK,
                vsb_block,
            ),
            (gen::OBJ_SPACEMAN, oid::SPACEMAN, layout.spaceman),
            (gen::OBJ_REAPER, oid::REAPER, layout.reaper),
        ];
        for (i, (obj_type, entry_oid, paddr)) in entries.iter().enumerate() {
            let entry = gen::nolx_checkpoint_map_entry_t {
                r#type: *obj_type,
                subtype: 0,
                block_size: block_size as u32,
                pad: 0,
                oid: *entry_oid,
                paddr: *paddr,
            };
            let off = gen::nolx_checkpoint_map_phys_t::OFF_RESERVED
                + i * gen::nolx_checkpoint_map_entry_t::DISK_SIZE;
            buf[off..off + gen::nolx_checkpoint_map_entry_t::DISK_SIZE]
                .copy_from_slice(entry.as_bytes());
        }
        on_disk::seal(
            &mut buf,
            on_disk::ObjHeader::make(xid, xid, gen::OBJ_CHECKPOINT_MAP, 0),
        )?;
        dev.write_block(desc_block, &buf)?;
    }
    dev.flush()?;
    Ok(())
}

/// Scan the descriptor ring for the newest checkpoint whose descriptor,
/// superblock copies, and state payload all verify.
///
/// Individual invalid candidates (torn descriptors, torn copies, out-of-range
/// pins) are skipped; this is the crash-recovery rule from the format document:
/// *selection of the newest valid checkpoint*, never replay.
///
/// # Errors
///
/// Device errors only; "no valid checkpoint" is `Ok(None)`.
pub fn find_latest<D: BlockDevice>(dev: &D, layout: &Layout) -> Result<Option<FoundCheckpoint>> {
    let block_size = dev.block_size();
    let mut best: Option<FoundCheckpoint> = None;
    for slot in 0..u64::from(layout.xp_desc_blocks) {
        let desc_block = layout.xp_desc_base + slot;
        let mut raw = vec![0u8; block_size];
        dev.read_block(desc_block, &mut raw)?;
        if raw.iter().all(|&b| b == 0) {
            continue;
        }
        let parsed = match on_disk::ObjRef::parse(&raw, gen::OBJ_CHECKPOINT_MAP) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let xid = parsed.header().xid();
        if let Some(current) = &best {
            if xid <= current.state.xid {
                continue;
            }
        }
        let map = match gen::nolx_checkpoint_map_phys_t::read_from(&raw) {
            Some(m) => m,
            None => continue,
        };
        if map.magic != gen::MAGIC_CHECKPOINT_MAP {
            continue;
        }
        let entries_off = gen::nolx_checkpoint_map_phys_t::OFF_RESERVED;
        let mut csb_block = None;
        let mut vsb_block = None;
        let mut valid = true;
        for i in 0..(map.num_entries as usize) {
            let off = entries_off + i * gen::nolx_checkpoint_map_entry_t::DISK_SIZE;
            let entry = match raw
                .get(off..off + gen::nolx_checkpoint_map_entry_t::DISK_SIZE)
                .and_then(gen::nolx_checkpoint_map_entry_t::read_from)
            {
                Some(e) => e,
                None => {
                    valid = false;
                    break;
                }
            };
            if entry.paddr >= layout.block_count {
                valid = false;
                break;
            }
            match entry.r#type {
                gen::OBJ_CONTAINER_SUPERBLOCK => csb_block = Some(entry.paddr),
                gen::OBJ_VOLUME_SUPERBLOCK => vsb_block = Some(entry.paddr),
                _ => {}
            }
        }
        if !valid {
            continue;
        }
        let (Some(csb_block), Some(vsb_block)) = (csb_block, vsb_block) else {
            continue;
        };
        // Both superblock copies must verify at this xid.
        let csb: gen::nolx_nx_superblock_t = match dev.read_object(csb_block) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if csb.next_xid != xid + 1 {
            continue;
        }
        let mut vsb_raw = vec![0u8; block_size];
        if dev.read_block(vsb_block, &mut vsb_raw).is_err() {
            continue;
        }
        if on_disk::ObjRef::parse(&vsb_raw, gen::OBJ_VOLUME_SUPERBLOCK).is_err() {
            continue;
        }
        let state = match read_state(dev, &vsb_raw) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if state.xid != xid {
            continue;
        }
        best = Some(FoundCheckpoint {
            state,
            desc_block,
            csb_block,
            vsb_block,
        });
    }
    Ok(best)
}

/// Decode the state stream carried by a volume superblock copy block,
/// following its spill chain when the stream outgrows the reserved tail.
fn read_state<D: BlockDevice>(dev: &D, vsb_raw: &[u8]) -> Result<CheckpointState> {
    let block_size = vsb_raw.len();
    let inline_cap = block_size - STATE_OFF;
    let spill_first = u64::from_le_bytes(
        vsb_raw[VSB_RESERVED_OFF..VSB_RESERVED_OFF + 8]
            .try_into()
            .map_err(|_| FsError::CorruptNode("state spill pointer truncated"))?,
    );
    // The stream length is not stored separately: the header is self-describing
    // once the snapshot/reaper counts are known, so decode from a buffer that
    // starts inline and grows along the spill chain until decoding succeeds or
    // the chain ends.
    let mut stream = vsb_raw[STATE_OFF..STATE_OFF + inline_cap].to_vec();
    if spill_first == 0 {
        return decode_state(&stream);
    }
    // First try inline-only; then append spill blocks one at a time.
    if let Ok(state) = decode_state(&stream) {
        return Ok(state);
    }
    let mut next = spill_first;
    let mut seq = 0u32;
    while next != 0 {
        if next >= dev.block_count() {
            return Err(FsError::BlockOutOfRange(next));
        }
        let mut raw = vec![0u8; block_size];
        dev.read_block(next, &mut raw)?;
        on_disk::ObjRef::parse(&raw, gen::OBJ_CHECKPOINT_MAP)?;
        let magic = u32::from_le_bytes(
            raw[32..36]
                .try_into()
                .map_err(|_| FsError::CorruptNode("spill header truncated"))?,
        );
        if magic != STATE_MAGIC {
            return Err(FsError::CorruptNode("spill block magic mismatch"));
        }
        let found_seq = u32::from_le_bytes(
            raw[36..40]
                .try_into()
                .map_err(|_| FsError::CorruptNode("spill header truncated"))?,
        );
        if found_seq != seq {
            return Err(FsError::CorruptNode("spill block out of sequence"));
        }
        let following = u64::from_le_bytes(
            raw[40..48]
                .try_into()
                .map_err(|_| FsError::CorruptNode("spill header truncated"))?,
        );
        stream.extend_from_slice(&raw[48..]);
        if let Ok(state) = decode_state(&stream) {
            return Ok(state);
        }
        next = following;
        seq += 1;
        if seq > 1_000_000 {
            return Err(FsError::CorruptNode("spill chain does not terminate"));
        }
    }
    decode_state(&stream)
}

/// Zero one descriptor ring block (fsck repair for a torn descriptor; the
/// previous checkpoint becomes the newest valid one).
///
/// # Errors
///
/// Device errors.
pub fn clear_descriptor<D: BlockDevice>(dev: &mut D, block: u64) -> Result<()> {
    let buf = vec![0u8; dev.block_size()];
    dev.write_block(block, &buf)?;
    Ok(())
}

/// Collect the spill blocks carrying the checkpoint whose volume superblock
/// copy lives at `vsb_block`, in chain order (empty when the state fit inline).
///
/// # Errors
///
/// Device or corruption errors in the spill chain.
pub fn spill_blocks<D: BlockDevice>(dev: &D, vsb_block: u64) -> Result<Vec<u64>> {
    let block_size = dev.block_size();
    let mut vsb_raw = vec![0u8; block_size];
    dev.read_block(vsb_block, &mut vsb_raw)?;
    on_disk::ObjRef::parse(&vsb_raw, gen::OBJ_VOLUME_SUPERBLOCK)?;
    let mut next = u64::from_le_bytes(
        vsb_raw[VSB_RESERVED_OFF..VSB_RESERVED_OFF + 8]
            .try_into()
            .map_err(|_| FsError::CorruptNode("state spill pointer truncated"))?,
    );
    let mut out = Vec::new();
    while next != 0 {
        if next >= dev.block_count() {
            return Err(FsError::BlockOutOfRange(next));
        }
        let mut raw = vec![0u8; block_size];
        dev.read_block(next, &mut raw)?;
        on_disk::ObjRef::parse(&raw, gen::OBJ_CHECKPOINT_MAP)?;
        out.push(next);
        next = u64::from_le_bytes(
            raw[40..48]
                .try_into()
                .map_err(|_| FsError::CorruptNode("spill header truncated"))?,
        );
        if out.len() > 1_000_000 {
            return Err(FsError::CorruptNode("spill chain does not terminate"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;
    use crate::mkfs::{format, FormatOptions};

    fn state(xid: u64) -> CheckpointState {
        CheckpointState {
            xid,
            fs_root: 100,
            extentref_root: 101,
            snapmeta_root: 102,
            fext_root: 103,
            next_ino: 1024,
            next_oid: 16,
            next_doc_id: 1,
            num_files: 3,
            num_dirs: 2,
            num_symlinks: 1,
            num_other: 0,
            total_blocks_alloced: 50,
            total_blocks_freed: 10,
            snapshots: vec![SnapshotRecord {
                xid: 2,
                name: String::from("before-upgrade"),
                fs_root: 90,
                extentref_root: 91,
                snapmeta_root: 92,
                fext_root: 0,
                num_files: 1,
                num_dirs: 1,
                num_symlinks: 0,
                num_other: 0,
                create_time: 2,
            }],
            reap: vec![ReapEntry {
                block: 500,
                len: 3,
                garbage_xid: 4,
            }],
        }
    }

    #[test]
    fn state_encoding_round_trips() {
        let s = state(7);
        let bytes = encode_state(&s);
        assert_eq!(decode_state(&bytes).unwrap(), s);
    }

    #[test]
    fn state_encoding_rejects_truncation_and_bad_magic() {
        let bytes = encode_state(&state(7));
        assert!(decode_state(&bytes[..40]).is_err());
        let mut bad = bytes.clone();
        bad[0] ^= 0xFF;
        assert!(decode_state(&bad).is_err());
    }

    #[test]
    fn commit_then_find_latest_round_trips() {
        let mut dev = MemBlockDevice::new(4096, 2048);
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();
        let info = crate::mkfs::probe(&mut dev).unwrap();
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0).unwrap();
        commit(&mut dev, &layout, &csb, &info.superblock, &state(2), &[]).unwrap();
        commit(&mut dev, &layout, &csb, &info.superblock, &state(3), &[]).unwrap();
        let found = find_latest(&dev, &layout).unwrap().unwrap();
        assert_eq!(found.state, state(3));
    }

    #[test]
    fn a_torn_newest_descriptor_falls_back_to_the_previous_checkpoint() {
        let mut dev = MemBlockDevice::new(4096, 2048);
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();
        let info = crate::mkfs::probe(&mut dev).unwrap();
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0).unwrap();
        commit(&mut dev, &layout, &csb, &info.superblock, &state(2), &[]).unwrap();
        commit(&mut dev, &layout, &csb, &info.superblock, &state(3), &[]).unwrap();
        let newest = find_latest(&dev, &layout).unwrap().unwrap();
        // Tear the newest descriptor the way a power cut would: flip body bytes
        // and leave the stored checksum stale.
        let mut raw = vec![0u8; 4096];
        dev.read_block(newest.desc_block, &mut raw).unwrap();
        raw[100] ^= 0xFF;
        dev.write_block(newest.desc_block, &raw).unwrap();
        let found = find_latest(&dev, &layout).unwrap().unwrap();
        assert_eq!(
            found.state.xid, 2,
            "recovery must select the previous valid checkpoint"
        );
    }

    #[test]
    fn no_checkpoint_is_none_not_an_error() {
        let mut dev = MemBlockDevice::new(4096, 2048);
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();
        assert!(find_latest(&dev, &layout).unwrap().is_none());
    }
}

