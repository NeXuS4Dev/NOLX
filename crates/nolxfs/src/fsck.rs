//! `fsck.nolxfs`: offline consistency checking and repair.
//!
//! The checker enforces the invariants listed in `docs/fs/nolxfs-format.md` §9:
//! checksums on every reachable object, `nlink` against the directory/sibling
//! record count, extent reference counts against actual `FILE_EXTENT`
//! references, allocation bitmaps against the reachable set (in both
//! directions), directory parent consistency, timestamp ordering, and volume
//! counter agreement. The checkpoint ring is audited too: a torn descriptor is
//! reported (and, in repair mode, cleared so the previous checkpoint becomes
//! authoritative again).
//!
//! Repairs are deliberately limited to what can be fixed without inventing
//! user data: torn checkpoint descriptors are cleared, `nlink` fields are
//! rewritten from the directory records, volume counters are rewritten from a
//! full walk, and bitmaps are rebuilt from the reachable set. Anything else —
//! a corrupt tree node, a dangling dentry, a lost extent — is reported and
//! left alone.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::blockdev::BlockDevice;
use crate::btree::{self, BTree, KeyKind};
use crate::checkpoint;
use crate::mkfs;
use crate::on_disk::{self, extent_len, jkey_oid, jkey_type, FsError, Result};
use crate::on_disk_gen as gen;
use crate::spaceman::{SpaceAllocator, SpaceMan};
use crate::Pod;

/// One consistency problem found by [`check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsckIssue {
    /// An object failed its Fletcher-64 checksum.
    BadChecksum {
        /// Block holding the object.
        block: u64,
        /// Object id from its header.
        oid: u64,
    },
    /// A structure is invalid in a way that is not a checksum failure.
    InvalidObject {
        /// Block holding the object.
        block: u64,
        /// What is wrong with it.
        reason: String,
    },
    /// A checkpoint descriptor ring block is neither empty nor a valid
    /// checkpoint map (a torn commit).
    TornCheckpoint {
        /// Descriptor ring block.
        block: u64,
    },
    /// A directory entry points at an inode that does not exist.
    DanglingDentry {
        /// Parent directory inode.
        parent: u64,
        /// Entry name.
        name: String,
        /// Missing child inode.
        child: u64,
    },
    /// An inode's recorded `nlink` disagrees with its directory/sibling records.
    NlinkMismatch {
        /// Inode concerned.
        ino: u64,
        /// `nlink` stored in the inode.
        recorded: u32,
        /// Links actually found.
        actual: u32,
    },
    /// An inode's `parent_id` disagrees with the directory that holds it.
    ParentMismatch {
        /// Inode concerned.
        ino: u64,
        /// Stored parent.
        recorded: u64,
        /// Directory actually holding the entry.
        actual: u64,
    },
    /// A physical block's extent reference count disagrees with the number of
    /// `FILE_EXTENT` block references to it.
    ExtentRefMismatch {
        /// Physical block.
        block: u64,
        /// Count in the extent-reference tree (0 when absent).
        recorded: u64,
        /// References actually found.
        actual: u64,
    },
    /// A block's bitmap state disagrees with its reachability.
    BitmapMismatch {
        /// Physical block.
        block: u64,
        /// Whether the bitmap marks it allocated.
        allocated: bool,
        /// Whether a live structure references it.
        reachable: bool,
    },
    /// A volume superblock counter disagrees with a full walk.
    CounterDrift {
        /// Counter name.
        field: &'static str,
        /// Value in the superblock/checkpoint state.
        recorded: u64,
        /// Value counted by walking.
        actual: u64,
    },
    /// `change_time` precedes `create_time`.
    TimestampOrder {
        /// Inode concerned.
        ino: u64,
    },
    /// An inode no directory entry references (and is not reserved).
    OrphanInode {
        /// Inode concerned.
        ino: u64,
    },
}

impl fmt::Display for FsckIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadChecksum { block, oid } => {
                write!(f, "block {block}: object {oid:#x} failed its checksum")
            }
            Self::InvalidObject { block, reason } => write!(f, "block {block}: {reason}"),
            Self::TornCheckpoint { block } => {
                write!(f, "block {block}: torn checkpoint descriptor")
            }
            Self::DanglingDentry {
                parent,
                name,
                child,
            } => write!(
                f,
                "directory {parent}: entry {name:?} points at missing inode {child}"
            ),
            Self::NlinkMismatch {
                ino,
                recorded,
                actual,
            } => write!(
                f,
                "inode {ino}: nlink is {recorded}, directory records say {actual}"
            ),
            Self::ParentMismatch {
                ino,
                recorded,
                actual,
            } => write!(
                f,
                "inode {ino}: parent_id is {recorded}, held by directory {actual}"
            ),
            Self::ExtentRefMismatch {
                block,
                recorded,
                actual,
            } => write!(
                f,
                "block {block}: extent refcount is {recorded}, references say {actual}"
            ),
            Self::BitmapMismatch {
                block,
                allocated,
                reachable,
            } => write!(
                f,
                "block {block}: bitmap allocated={allocated}, reachable={reachable}"
            ),
            Self::CounterDrift {
                field,
                recorded,
                actual,
            } => write!(f, "counter {field}: recorded {recorded}, walked {actual}"),
            Self::TimestampOrder { ino } => {
                write!(f, "inode {ino}: change_time precedes create_time")
            }
            Self::OrphanInode { ino } => write!(f, "inode {ino}: no directory entry references it"),
        }
    }
}

/// The outcome of a [`check`] run.
#[derive(Clone, Debug, Default)]
pub struct FsckReport {
    /// Newest valid checkpoint xid, or 1 for a genesis (mkfs-only) container.
    pub xid: u64,
    /// Whether a checkpoint was found in the ring.
    pub has_checkpoint: bool,
    /// Metadata blocks whose checksums were verified.
    pub checked_blocks: u64,
    /// Live regular files counted.
    pub num_files: u64,
    /// Live directories counted.
    pub num_dirs: u64,
    /// Live symlinks counted.
    pub num_symlinks: u64,
    /// Blocks in the reaper backlog at check time.
    pub reaper_backlog: u64,
    /// Every problem found, in walk order.
    pub issues: Vec<FsckIssue>,
    /// Repairs applied (only when `check` ran with `repair = true`).
    pub repairs: Vec<String>,
}

impl FsckReport {
    /// Whether the volume is clean (repairs, when requested, have been applied
    /// and are listed separately; `issues` still reports what was wrong).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Check the container on `dev`, optionally repairing the safe classes of
/// damage listed in the module docs.
///
/// # Errors
///
/// Device errors that prevent reading the container at all. A container so
/// damaged that even its superblock cannot be read produces a report with a
/// single [`FsckIssue::InvalidObject`] instead — `fsck`'s job is to *report*
/// damage, not to fail on it.
pub fn check<D: BlockDevice>(dev: &mut D, repair: bool) -> Result<FsckReport> {
    let mut report = FsckReport {
        xid: 1,
        ..FsckReport::default()
    };
    let info = match mkfs::probe(dev) {
        Ok(info) => info,
        Err(e) => {
            report.issues.push(FsckIssue::InvalidObject {
                block: 0,
                reason: format!("cannot probe container: {e}"),
            });
            return Ok(report);
        }
    };
    let layout = info.layout;
    let found = checkpoint::find_latest(dev, &layout)?;
    report.has_checkpoint = found.is_some();
    let (fs_root, extentref_root, state) = match &found {
        Some(f) => {
            report.xid = f.state.xid;
            (
                f.state.fs_root,
                f.state.extentref_root,
                Some(f.state.clone()),
            )
        }
        None => (layout.fs_tree_root, layout.extentref_tree_root, None),
    };

    // Torn descriptors: ring blocks that are neither empty nor parseable.
    let mut torn: Vec<u64> = Vec::new();
    for slot in 0..u64::from(layout.xp_desc_blocks) {
        let block = layout.xp_desc_base + slot;
        let mut raw = vec![0u8; dev.block_size()];
        dev.read_block(block, &mut raw)?;
        if raw.iter().all(|&b| b == 0) {
            continue;
        }
        if on_disk::ObjRef::parse(&raw, gen::OBJ_CHECKPOINT_MAP).is_err() {
            report.issues.push(FsckIssue::TornCheckpoint { block });
            torn.push(block);
        }
    }
    if repair {
        for block in &torn {
            checkpoint::clear_descriptor(dev, *block)?;
            report.repairs.push(format!(
                "cleared torn checkpoint descriptor at block {block}"
            ));
        }
    }

    // ---- walk the live fs-tree -------------------------------------------
    let mut inodes: BTreeMap<u64, gen::nolx_inode_t> = BTreeMap::new();
    let mut dentry_count: BTreeMap<u64, u32> = BTreeMap::new();
    let mut sibling_count: BTreeMap<u64, u32> = BTreeMap::new();
    let mut held_by: BTreeMap<u64, u64> = BTreeMap::new();
    let mut block_refs: BTreeMap<u64, u64> = BTreeMap::new();
    let mut reachable: BTreeSet<u64> = BTreeSet::new();
    let mut subdir_count: BTreeMap<u64, u64> = BTreeMap::new();

    collect_tree_blocks(dev, fs_root, &mut reachable, &mut report);
    let entries = btree::walk::collect(dev, fs_root).unwrap_or_default();
    for entry in &entries {
        if entry.key.len() < 8 {
            continue;
        }
        let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
        match jkey_type(raw) {
            gen::DREC_TYPE_INODE => {
                if let Some(inode) = gen::nolx_inode_t::read_from(&entry.val) {
                    inodes.insert(jkey_oid(raw), inode);
                }
            }
            gen::DREC_TYPE_DIR_REC => {
                let parent = jkey_oid(raw);
                if let Some(dentry) = gen::nolx_direntry_t::read_from(&entry.val) {
                    *dentry_count.entry(dentry.ino).or_insert(0) += 1;
                    held_by.insert(dentry.ino, parent);
                    let name_len = usize::from(dentry.name_len);
                    let name = entry
                        .val
                        .get(
                            gen::nolx_direntry_t::DISK_SIZE
                                ..gen::nolx_direntry_t::DISK_SIZE + name_len,
                        )
                        .and_then(|b| core::str::from_utf8(b).ok())
                        .map_or_else(String::new, String::from);
                    if !inodes.contains_key(&dentry.ino) {
                        // The inode record may sort after this dentry in a
                        // full walk; resolve after the walk instead.
                        let _ = name;
                    }
                    let _ = (parent, name);
                }
            }
            gen::DREC_TYPE_SIBLING_LINK => {
                *sibling_count.entry(jkey_oid(raw)).or_insert(0) += 1;
            }
            gen::DREC_TYPE_FILE_EXTENT => {
                if let Some(val) = gen::nolx_file_extent_val_t::read_from(&entry.val) {
                    let blocks = extent_len(val.len_and_flags).div_ceil(dev.block_size() as u64);
                    for i in 0..blocks {
                        *block_refs.entry(val.phys_block_num + i).or_insert(0) += 1;
                        reachable.insert(val.phys_block_num + i);
                    }
                }
            }
            _ => {}
        }
    }
    // Second pass over dentries now that every inode is known.
    for entry in &entries {
        if entry.key.len() < 8 {
            continue;
        }
        let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
        if jkey_type(raw) == gen::DREC_TYPE_DIR_REC {
            let parent = jkey_oid(raw);
            if let Some(dentry) = gen::nolx_direntry_t::read_from(&entry.val) {
                let name_len = usize::from(dentry.name_len);
                let name = entry
                    .val
                    .get(
                        gen::nolx_direntry_t::DISK_SIZE..gen::nolx_direntry_t::DISK_SIZE + name_len,
                    )
                    .and_then(|b| core::str::from_utf8(b).ok())
                    .map_or_else(String::new, String::from);
                match inodes.get(&dentry.ino) {
                    None => report.issues.push(FsckIssue::DanglingDentry {
                        parent,
                        name,
                        child: dentry.ino,
                    }),
                    Some(child_inode) => {
                        if is_dir_mode(u32::from(child_inode.mode)) {
                            *subdir_count.entry(parent).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }

    for (&ino, inode) in &inodes {
        let mode = u32::from(inode.mode);
        // The reserved private/snapshot directories are internal plumbing;
        // the volume counters track the root plus user-visible entries.
        if ino != gen::PRIVATE_DIR_INO && ino != gen::SNAPSHOT_DIR_INO {
            match mode & 0o170_000 {
                0o040_000 => report.num_dirs += 1,
                0o100_000 => report.num_files += 1,
                0o120_000 => report.num_symlinks += 1,
                _ => {}
            }
        }
        if inode.change_time < inode.create_time && inode.create_time != 0 {
            report.issues.push(FsckIssue::TimestampOrder { ino });
        }
        let links = dentry_count.get(&ino).copied().unwrap_or(0);
        if ino >= gen::FIRST_USER_INO && links == 0 {
            report.issues.push(FsckIssue::OrphanInode { ino });
        }
        let expected_nlink = if is_dir_mode(mode) {
            // POSIX directory nlink: "." + parent's entry + one per subdir.
            2 + subdir_count.get(&ino).copied().unwrap_or(0) as u32
        } else {
            links
        };
        if ino != gen::PRIVATE_DIR_INO
            && ino != gen::SNAPSHOT_DIR_INO
            && inode.nlink != expected_nlink
        {
            report.issues.push(FsckIssue::NlinkMismatch {
                ino,
                recorded: inode.nlink,
                actual: expected_nlink,
            });
        }
        let siblings = sibling_count.get(&ino).copied().unwrap_or(0);
        if ino >= gen::ROOT_INO && links > 0 && siblings != links {
            report.issues.push(FsckIssue::InvalidObject {
                block: fs_root,
                reason: format!(
                    "inode {ino}: {links} directory entries but {siblings} sibling links"
                ),
            });
        }
        if links == 1 {
            if let Some(&parent) = held_by.get(&ino) {
                if inode.parent_id != parent {
                    report.issues.push(FsckIssue::ParentMismatch {
                        ino,
                        recorded: inode.parent_id,
                        actual: parent,
                    });
                }
            }
        }
    }

    // ---- extent reference counts ------------------------------------------
    collect_tree_blocks(dev, extentref_root, &mut reachable, &mut report);
    let mut recorded_refs: BTreeMap<u64, u64> = BTreeMap::new();
    if let Ok(ref_entries) = btree::walk::collect(dev, extentref_root) {
        for entry in &ref_entries {
            if entry.key.len() < 8 {
                continue;
            }
            let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
            if jkey_type(raw) == gen::DREC_TYPE_PHYS_EXTENT {
                let block = jkey_oid(raw);
                let count = entry
                    .val
                    .get(
                        gen::nolx_phys_ext_val_t::DISK_SIZE
                            ..gen::nolx_phys_ext_val_t::DISK_SIZE + 8,
                    )
                    .and_then(|b| <[u8; 8]>::try_from(b).ok())
                    .map_or(0, u64::from_le_bytes);
                recorded_refs.insert(block, count);
            }
        }
    }
    let mut all_ref_blocks: BTreeSet<u64> = BTreeSet::new();
    all_ref_blocks.extend(block_refs.keys().copied());
    all_ref_blocks.extend(recorded_refs.keys().copied());
    for block in all_ref_blocks {
        let recorded = recorded_refs.get(&block).copied().unwrap_or(0);
        let actual = block_refs.get(&block).copied().unwrap_or(0);
        if recorded != actual {
            report.issues.push(FsckIssue::ExtentRefMismatch {
                block,
                recorded,
                actual,
            });
        }
    }

    // ---- snapshots: their trees and extents must stay allocated ------------
    let mut snapshot_blocks: BTreeSet<u64> = BTreeSet::new();
    if let Some(state) = &state {
        report.reaper_backlog = state.reap.iter().map(|e| e.len).sum();
        for snap in &state.snapshots {
            collect_tree_blocks(dev, snap.fs_root, &mut snapshot_blocks, &mut report);
            collect_tree_blocks(dev, snap.extentref_root, &mut snapshot_blocks, &mut report);
            if let Ok(snap_entries) = btree::walk::collect(dev, snap.fs_root) {
                for entry in &snap_entries {
                    if entry.key.len() < 8 {
                        continue;
                    }
                    let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
                    if jkey_type(raw) == gen::DREC_TYPE_FILE_EXTENT {
                        if let Some(val) = gen::nolx_file_extent_val_t::read_from(&entry.val) {
                            let blocks =
                                extent_len(val.len_and_flags).div_ceil(dev.block_size() as u64);
                            for i in 0..blocks {
                                snapshot_blocks.insert(val.phys_block_num + i);
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- counters -----------------------------------------------------------
    if let Some(state) = &state {
        for (field, recorded, actual) in [
            ("num_files", state.num_files, report.num_files),
            ("num_directories", state.num_dirs, report.num_dirs),
            ("num_symlinks", state.num_symlinks, report.num_symlinks),
        ] {
            if recorded != actual {
                report.issues.push(FsckIssue::CounterDrift {
                    field,
                    recorded,
                    actual,
                });
            }
        }
    }

    // ---- bitmaps vs reachable ------------------------------------------------
    // Fixed metadata and ring blocks are always reachable.
    for block in 0..layout.first_data_block {
        reachable.insert(block);
    }
    for block in layout.xp_desc_base..layout.xp_desc_base + u64::from(layout.xp_desc_blocks) {
        reachable.insert(block);
    }
    for block in layout.xp_data_base..layout.xp_data_base + u64::from(layout.xp_data_blocks) {
        reachable.insert(block);
    }
    collect_tree_blocks(dev, layout.container_omap_tree, &mut reachable, &mut report);
    collect_tree_blocks(dev, layout.volume_omap_tree, &mut reachable, &mut report);
    collect_tree_blocks(dev, layout.snap_meta_tree_root, &mut reachable, &mut report);
    if let Some(state) = &state {
        collect_tree_blocks(dev, state.snapmeta_root, &mut reachable, &mut report);
        if state.fext_root != 0 {
            collect_tree_blocks(dev, state.fext_root, &mut reachable, &mut report);
        }
    }
    if let Some(f) = &found {
        reachable.insert(f.desc_block);
        reachable.insert(f.csb_block);
        reachable.insert(f.vsb_block);
        if let Ok(spill) = checkpoint::spill_blocks(dev, f.vsb_block) {
            for block in spill {
                reachable.insert(block);
            }
        }
        for entry in &f.state.reap {
            for i in 0..entry.len {
                reachable.insert(entry.block + i);
            }
        }
    }
    for block in &snapshot_blocks {
        reachable.insert(*block);
    }

    let spaceman = SpaceMan::open(dev, &layout);
    if let Ok(spaceman) = &spaceman {
        let space = *spaceman.space_layout();
        for block in space.data_start..space.alloc_start {
            reachable.insert(block);
        }
        let mut bitmap_issues = 0usize;
        for block in space.alloc_start..layout.block_count {
            let allocated = spaceman.is_allocated(block);
            let is_reachable = reachable.contains(&block);
            if allocated != is_reachable {
                // Blocks held only by the reaper backlog or a snapshot are
                // reachable by construction above; anything else allocated but
                // unreachable is a leak, and anything reachable but free is a
                // future double-allocation.
                report.issues.push(FsckIssue::BitmapMismatch {
                    block,
                    allocated,
                    reachable: is_reachable,
                });
                bitmap_issues += 1;
            }
        }
        if repair && bitmap_issues > 0 {
            let mut fixed = spaceman.clone();
            fixed.reset_allocations();
            for &block in &reachable {
                if block >= space.data_start && block < layout.block_count {
                    let _ = fixed.mark_allocated(block, 1);
                }
            }
            fixed.flush(dev, layout.spaceman, report.xid)?;
            report.repairs.push(format!(
                "rebuilt allocation bitmaps from the reachable set ({bitmap_issues} mismatches)"
            ));
        }
    }

    // ---- repairs that rewrite records ----------------------------------------
    if repair {
        let nlink_fixes: Vec<u64> = report
            .issues
            .iter()
            .filter_map(|issue| match issue {
                FsckIssue::NlinkMismatch { ino, .. } => Some(*ino),
                _ => None,
            })
            .collect();
        if !nlink_fixes.is_empty() {
            if let Ok(mut spaceman) = SpaceMan::open(dev, &layout) {
                let mut deferred = Vec::new();
                let mut alloc = SpaceAllocator {
                    spaceman: &mut spaceman,
                    deferred: &mut deferred,
                };
                let mut tree = BTree::new(
                    dev,
                    &mut alloc,
                    fs_root,
                    KeyKind::JKey,
                    gen::SUB_FSTREE as u16,
                    report.xid,
                );
                for ino in &nlink_fixes {
                    if let Some(inode) = inodes.get(ino) {
                        let links = dentry_count.get(ino).copied().unwrap_or(0);
                        let expected = if is_dir_mode(u32::from(inode.mode)) {
                            2 + subdir_count.get(ino).copied().unwrap_or(0) as u32
                        } else {
                            links
                        };
                        let mut fixed_inode = *inode;
                        fixed_inode.nlink = expected;
                        let key = pack_inode_key(*ino);
                        tree.insert(&key, fixed_inode.as_bytes())?;
                        report
                            .repairs
                            .push(format!("rewrote nlink of inode {ino} as {expected}"));
                    }
                }
            }
        }
        let drift = report
            .issues
            .iter()
            .any(|i| matches!(i, FsckIssue::CounterDrift { .. }));
        if drift {
            // Patch the counters in the volume superblock copy that mount
            // will read (and the original block, for plain-probe readers).
            let (files, dirs, links) = (report.num_files, report.num_dirs, report.num_symlinks);
            let mut targets = vec![layout.volume_sb];
            if let Some(f) = &found {
                targets.push(f.vsb_block);
            }
            for block in targets {
                let mut vsb: gen::nolx_fs_superblock_t = dev.read_object(block)?;
                vsb.num_files = files;
                vsb.num_directories = dirs;
                vsb.num_symlinks = links;
                let mut buf = vec![0u8; dev.block_size()];
                buf[..core::mem::size_of::<gen::nolx_fs_superblock_t>()]
                    .copy_from_slice(vsb.as_bytes());
                on_disk::seal(
                    &mut buf,
                    on_disk::ObjHeader::make(
                        mkfs::oid::VOLUME_SUPERBLOCK,
                        report.xid,
                        gen::OBJ_VOLUME_SUPERBLOCK,
                        0,
                    ),
                )?;
                dev.write_block(block, &buf)?;
            }
            report
                .repairs
                .push(String::from("rewrote volume counters from the walked tree"));
        }
    }

    report.checked_blocks = reachable.len() as u64;
    Ok(report)
}

fn is_dir_mode(mode: u32) -> bool {
    mode & 0o170_000 == 0o040_000
}

fn pack_inode_key(ino: u64) -> Vec<u8> {
    on_disk::pack_jkey(gen::DREC_TYPE_INODE, ino)
        .to_le_bytes()
        .to_vec()
}

fn collect_tree_blocks<D: BlockDevice>(
    dev: &D,
    root: u64,
    out: &mut BTreeSet<u64>,
    report: &mut FsckReport,
) {
    if root == 0 {
        return;
    }
    match btree::walk::blocks(dev, root) {
        Ok(blocks) => {
            for block in blocks {
                out.insert(block);
            }
        }
        Err(FsError::BadChecksum { oid }) => report
            .issues
            .push(FsckIssue::BadChecksum { block: root, oid }),
        Err(e) => report.issues.push(FsckIssue::InvalidObject {
            block: root,
            reason: format!("cannot walk tree: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;
    use crate::volume::Volume;

    fn healthy_device() -> MemBlockDevice {
        let dev = MemBlockDevice::new(4096, 2048);
        let mut vol = Volume::format(
            dev,
            &mkfs::FormatOptions {
                volume_name: String::from("fsck-test"),
                ..mkfs::FormatOptions::default()
            },
        )
        .unwrap();
        let file = vol.create_file(gen::ROOT_INO, "data", 0o644, 0, 0).unwrap();
        vol.write_data(file, 0, &[42u8; 9000]).unwrap();
        vol.commit().unwrap();
        vol.into_device()
    }

    #[test]
    fn torn_checkpoint_descriptor_is_found_and_cleared() {
        let mut dev = healthy_device();
        let info = mkfs::probe(&mut dev).unwrap();
        let desc = info.layout.xp_desc_base;
        // Scribble over the newest descriptor so it no longer parses.
        let junk = vec![0xA5u8; dev.block_size()];
        dev.write_block(desc, &junk).unwrap();
        let report = check(&mut dev, false).unwrap();
        assert!(
            report
                .issues
                .iter()
                .any(|i| matches!(i, FsckIssue::TornCheckpoint { .. })),
            "issues: {:?}",
            report.issues
        );
        let repaired = check(&mut dev, true).unwrap();
        assert!(!repaired.repairs.is_empty());
        // After clearing, the volume still mounts from the previous checkpoint.
        let _vol = Volume::mount(dev).unwrap();
    }

    #[test]
    fn leaked_bitmap_block_is_found_and_rebuilt() {
        let mut dev = healthy_device();
        let info = mkfs::probe(&mut dev).unwrap();
        {
            let mut spaceman = SpaceMan::open(&dev, &info.layout).unwrap();
            // Allocate a block nothing references: a leak.
            let leaked = spaceman.alloc_block().unwrap();
            spaceman.flush(&mut dev, info.layout.spaceman, 9).unwrap();
            let report = check(&mut dev, false).unwrap();
            assert!(
                report.issues.iter().any(
                    |i| matches!(i, FsckIssue::BitmapMismatch { block, .. } if *block == leaked)
                ),
                "issues: {:?}",
                report.issues
            );
        }
        let repaired = check(&mut dev, true).unwrap();
        assert!(repaired.repairs.iter().any(|r| r.contains("bitmap")));
        let after = check(&mut dev, false).unwrap();
        assert!(
            !after
                .issues
                .iter()
                .any(|i| matches!(i, FsckIssue::BitmapMismatch { .. })),
            "issues after repair: {:?}",
            after.issues
        );
    }
}
