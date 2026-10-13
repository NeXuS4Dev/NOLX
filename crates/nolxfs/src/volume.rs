//! A mounted, transactional NOLXFS volume.
//!
//! [`Volume`] is the M8 filesystem proper. It layers transactions, copy-on-write,
//! snapshots, and the space manager over the primitives that were already here
//! (object maps, B-trees, Fletcher-64 objects, `mkfs`):
//!
//! * **Transactions.** Every mutating operation runs inside a transaction. The
//!   first mutation clones the fs-tree, extent-reference tree, snapshot tree, and
//!   fast-extent tree into fresh blocks from the space manager; the old trees stay
//!   untouched, owned by the last checkpoint (and by any snapshot that pins them).
//!   Data writes always allocate fresh extent blocks. Nothing reachable from the
//!   last committed checkpoint is ever overwritten.
//! * **Commit (`fsync`).** [`Volume::commit`] defers the replaced trees and
//!   extents to the reaper, reaps what no snapshot can still reach, records the
//!   new tree roots in the volume object map at the new xid, flushes the
//!   space-manager bitmaps, and finally commits a checkpoint (see
//!   [`crate::checkpoint`]) whose descriptor is the single atomic commit point.
//!   A crash at any write boundary leaves either the old or the new state fully
//!   visible — the crash-injection tests cut power at every write of a
//!   transaction and assert exactly that.
//! * **Snapshots.** Creating one is metadata-only: the current tree roots are
//!   recorded in the snapshot metadata tree and the checkpoint state, marked
//!   `SAVED` in the object map, and pinned. Later transactions clone before they
//!   write, and the reaper's horizon is the oldest live snapshot, so a snapshot
//!   view is immutable no matter what the live volume does. Mounting a snapshot
//!   ([`Volume::mount_snapshot`]) is a read-only view resolved from its recorded
//!   roots.
//! * **Records.** The fs-tree carries `INODE`, `DIR_REC`, `FILE_EXTENT` (keyed by
//!   logical byte offset, one record per contiguous run), `XATTR`,
//!   `SIBLING_LINK`/`SIBLING_MAP` (one per hard link, so `nlink` is auditable),
//!   `DSTREAM_ID`, `DIR_STATS`, and `FILE_INFO` records. The extent-reference
//!   tree holds one `PHYS_EXTENT` record per physical block with its reference
//!   count, which is what makes clones (`clone_file`) and snapshot-shared
//!   extents safe to free. The fast-extent tree mirrors each file's first
//!   extent for fusion-style devices; it is created by [`Volume::format`] and
//!   absent on volumes formatted by plain `mkfs`.
//!
//! Timestamps use the transaction id as a logical clock: deterministic, monotone
//! across commits, and honest about the fact that a filesystem core has no wall
//! clock of its own (the kernel passes real time through the VFS layer above).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use nolx_types::scalars::{is_dir, is_lnk, Gid, Mode, Uid, S_IFDIR, S_IFLNK, S_IFREG};

use crate::blockdev::BlockDevice;
use crate::btree::{self, BTree, KeyKind};
use crate::checkpoint::{self, CheckpointState, FoundCheckpoint, SnapshotRecord};
use crate::mkfs::{self, FormatOptions, VolumeInfo};
use crate::omap::ObjectMap;
use crate::on_disk::{
    extent_len, jkey_oid, jkey_type, pack_jkey, pack_len_and_flags, FsError, Result,
};
use crate::on_disk_gen as gen;
use crate::spaceman::{Reaper, SpaceAllocator, SpaceMan};
use crate::Pod;

/// Which metadata tree an operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeKind {
    Fs,
    ExtentRef,
    SnapMeta,
    Fext,
}

/// A contiguous run of file data: `len` bytes of capacity at `phys`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Logical byte offset of the run inside the file.
    pub logical: u64,
    /// Byte capacity of the run (a whole number of blocks).
    pub len: u64,
    /// First physical block of the run.
    pub phys: u64,
}

/// A mounted NOLXFS volume over a block device `D`.
pub struct Volume<D: BlockDevice> {
    dev: D,
    layout: mkfs::Layout,
    name: String,
    csb: gen::nolx_nx_superblock_t,
    vsb: gen::nolx_fs_superblock_t,
    spaceman: Option<SpaceMan>,
    reaper: Reaper,
    state: CheckpointState,
    casefold: bool,
    read_only: bool,
    snapshot_view: Option<u64>,
    dirty: bool,
    pending_xid: u64,
    old_tree_blocks: Vec<u64>,
    pending_frees: Vec<(u64, u64)>,
    spill_blocks: Vec<u64>,
}

impl<D: BlockDevice> core::fmt::Debug for Volume<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Volume")
            .field("name", &self.name)
            .field("xid", &self.state.xid)
            .field("read_only", &self.read_only)
            .field("snapshots", &self.state.snapshots.len())
            .finish()
    }
}

impl<D: BlockDevice> Volume<D> {
    // ------------------------------------------------------------ lifecycle

    /// Format a fresh container on `dev` and mount the resulting volume.
    ///
    /// Runs `mkfs`, initialises the space manager, creates the reserved
    /// private/snapshot directory inodes and the fast-extent tree, and commits
    /// the genesis checkpoint (xid 2) so every later mount takes the checkpoint
    /// path.
    ///
    /// # Errors
    ///
    /// Whatever formatting, space-manager initialisation, or the genesis
    /// commit reports.
    pub fn format(mut dev: D, opts: &FormatOptions) -> Result<Self> {
        let layout = mkfs::format(&mut dev, opts)?;
        let info = mkfs::probe(&mut dev)?;
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0)?;
        let mut spaceman = SpaceMan::initialise(&mut dev, &layout, 2)?;
        let mut state = CheckpointState {
            xid: 1,
            fs_root: layout.fs_tree_root,
            extentref_root: layout.extentref_tree_root,
            snapmeta_root: layout.snap_meta_tree_root,
            fext_root: 0,
            next_ino: gen::FIRST_USER_INO,
            next_oid: mkfs::oid::NEXT,
            next_doc_id: 1,
            num_files: 0,
            num_dirs: 1,
            num_symlinks: 0,
            num_other: 0,
            total_blocks_alloced: 0,
            total_blocks_freed: 0,
            snapshots: Vec::new(),
            reap: Vec::new(),
        };
        // Reserved inodes: the private directory (orphan holding pen) and the
        // snapshot directory. Neither is linked into the root; they exist so
        // the reserved numbers are never handed out as user inodes.
        {
            let mut deferred = Vec::new();
            let mut alloc = SpaceAllocator {
                spaceman: &mut spaceman,
                deferred: &mut deferred,
            };
            let mut tree = BTree::new(
                &mut dev,
                &mut alloc,
                layout.fs_tree_root,
                KeyKind::JKey,
                gen::SUB_FSTREE as u16,
                1,
            );
            for (ino, mode) in [
                (gen::PRIVATE_DIR_INO, 0o700u16),
                (gen::SNAPSHOT_DIR_INO, 0o555),
            ] {
                let inode = new_inode(
                    gen::ROOT_INO,
                    ino,
                    (S_IFDIR | u32::from(mode)) as u16,
                    2,
                    0,
                    0,
                    1,
                );
                tree.insert(&inode_key(ino), inode.as_bytes())?;
            }
            let stats = gen::nolx_dir_stats_val_t {
                num_children: 0,
                total_size: 0,
                chained_key: 0,
                gen_count: 1,
            };
            tree.insert(&dir_stats_key(gen::ROOT_INO), stats.as_bytes())?;
        }
        // The fast-extent tree: one fresh block, an empty B-tree root there.
        let fext_root = spaceman.alloc_block()?;
        {
            let mut deferred = Vec::new();
            let mut alloc = SpaceAllocator {
                spaceman: &mut spaceman,
                deferred: &mut deferred,
            };
            BTree::create(
                &mut dev,
                &mut alloc,
                fext_root,
                KeyKind::JKey,
                gen::SUB_FEXT_TREE as u16,
                1,
            )?;
        }
        state.fext_root = fext_root;
        let mut vsb = info.superblock;
        vsb.features_supported |=
            u64::from(gen::NX_FEATURE_SUP_SNAPSHOTS) | u64::from(gen::NX_FEATURE_SUP_FEXT);
        let mut vol = Self {
            dev,
            layout,
            name: info.name.clone(),
            csb,
            vsb,
            spaceman: Some(spaceman),
            reaper: Reaper::new(),
            state,
            casefold: opts.casefold,
            read_only: false,
            snapshot_view: None,
            dirty: true,
            pending_xid: 2,
            old_tree_blocks: Vec::new(),
            pending_frees: Vec::new(),
            spill_blocks: Vec::new(),
        };
        vol.commit()?;
        Ok(vol)
    }

    /// Mount the live volume on `dev`, resuming from the newest valid
    /// checkpoint when one exists and importing the `mkfs` genesis state
    /// (plus a genesis checkpoint commit) when none does.
    ///
    /// # Errors
    ///
    /// Probe, checkpoint, or space-manager errors. A torn newest checkpoint
    /// is not an error: the previous valid one is resumed instead.
    pub fn mount(mut dev: D) -> Result<Self> {
        let info = mkfs::probe(&mut dev)?;
        let layout = info.layout;
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0)?;
        let casefold =
            info.superblock.features_supported & u64::from(gen::NX_FEATURE_SUP_CASEFOLD) != 0;
        match checkpoint::find_latest(&dev, &layout)? {
            Some(found) => {
                let spill = checkpoint::spill_blocks(&dev, found.vsb_block)?;
                let vsb: gen::nolx_fs_superblock_t = dev.read_object(found.vsb_block)?;
                let spaceman = SpaceMan::open(&dev, &layout)?;
                let reaper = Reaper::from_entries(found.state.reap.clone());
                Ok(Self {
                    dev,
                    layout,
                    name: info.name,
                    csb,
                    vsb,
                    spaceman: Some(spaceman),
                    reaper,
                    state: found.state,
                    casefold,
                    read_only: false,
                    snapshot_view: None,
                    dirty: false,
                    pending_xid: 0,
                    old_tree_blocks: Vec::new(),
                    pending_frees: Vec::new(),
                    spill_blocks: spill,
                })
            }
            None => {
                // Genesis import: a container straight from mkfs. Count what
                // is actually in the fs-tree so the counters start honest.
                let mut state = CheckpointState {
                    xid: 1,
                    fs_root: layout.fs_tree_root,
                    extentref_root: layout.extentref_tree_root,
                    snapmeta_root: layout.snap_meta_tree_root,
                    fext_root: 0,
                    next_ino: gen::FIRST_USER_INO,
                    next_oid: csb.next_oid,
                    next_doc_id: 1,
                    num_files: 0,
                    num_dirs: 0,
                    num_symlinks: 0,
                    num_other: 0,
                    total_blocks_alloced: 0,
                    total_blocks_freed: 0,
                    snapshots: Vec::new(),
                    reap: Vec::new(),
                };
                let entries = btree::walk::collect(&dev, layout.fs_tree_root)?;
                for entry in &entries {
                    if entry.key.len() < 8 {
                        continue;
                    }
                    let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
                    if jkey_type(raw) == gen::DREC_TYPE_INODE {
                        let ino = jkey_oid(raw);
                        if ino >= state.next_ino {
                            state.next_ino = ino + 1;
                        }
                        if let Some(inode) = gen::nolx_inode_t::read_from(&entry.val) {
                            match u32::from(inode.mode) & 0o170_000 {
                                x if x == S_IFDIR => state.num_dirs += 1,
                                x if x == S_IFREG => state.num_files += 1,
                                x if x == S_IFLNK => state.num_symlinks += 1,
                                _ => state.num_other += 1,
                            }
                        }
                    }
                }
                if state.num_dirs == 0 {
                    state.num_dirs = 1;
                }
                let spaceman = if SpaceMan::is_initialised(&dev, &layout)? {
                    SpaceMan::open(&dev, &layout)?
                } else {
                    SpaceMan::initialise(&mut dev, &layout, csb.next_xid)?
                };
                let mut vol = Self {
                    dev,
                    layout,
                    name: info.name,
                    csb,
                    vsb: info.superblock,
                    spaceman: Some(spaceman),
                    reaper: Reaper::new(),
                    state,
                    casefold,
                    read_only: false,
                    snapshot_view: None,
                    dirty: true,
                    pending_xid: csb.next_xid.max(2),
                    old_tree_blocks: Vec::new(),
                    pending_frees: Vec::new(),
                    spill_blocks: Vec::new(),
                };
                vol.commit()?;
                Ok(vol)
            }
        }
    }

    /// Mount the snapshot called `name` as a read-only point-in-time view.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`] if no live snapshot has that name, plus probe and
    /// checkpoint errors.
    pub fn mount_snapshot(mut dev: D, name: &str) -> Result<Self> {
        let info = mkfs::probe(&mut dev)?;
        let layout = info.layout;
        let found = checkpoint::find_latest(&dev, &layout)?.ok_or(FsError::NotFound)?;
        let record = found
            .state
            .snapshot_named(name)
            .cloned()
            .ok_or(FsError::NotFound)?;
        Self::view(dev, info, found, record)
    }

    /// Mount the snapshot pinned at `xid` as a read-only point-in-time view.
    ///
    /// # Errors
    ///
    /// As [`Volume::mount_snapshot`].
    pub fn mount_snapshot_xid(mut dev: D, xid: u64) -> Result<Self> {
        let info = mkfs::probe(&mut dev)?;
        let layout = info.layout;
        let found = checkpoint::find_latest(&dev, &layout)?.ok_or(FsError::NotFound)?;
        let record = found
            .state
            .snapshots
            .iter()
            .find(|s| s.xid == xid)
            .cloned()
            .ok_or(FsError::NotFound)?;
        Self::view(dev, info, found, record)
    }

    fn view(
        dev: D,
        info: VolumeInfo,
        found: FoundCheckpoint,
        record: SnapshotRecord,
    ) -> Result<Self> {
        let layout = info.layout;
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0)?;
        let vsb: gen::nolx_fs_superblock_t = dev.read_object(found.vsb_block)?;
        let casefold =
            info.superblock.features_supported & u64::from(gen::NX_FEATURE_SUP_CASEFOLD) != 0;
        let mut state = found.state;
        state.fs_root = record.fs_root;
        state.extentref_root = record.extentref_root;
        state.snapmeta_root = record.snapmeta_root;
        state.fext_root = record.fext_root;
        state.xid = record.xid;
        Ok(Self {
            dev,
            layout,
            name: info.name,
            csb,
            vsb,
            spaceman: None,
            reaper: Reaper::new(),
            state,
            casefold,
            read_only: true,
            snapshot_view: Some(record.xid),
            dirty: false,
            pending_xid: 0,
            old_tree_blocks: Vec::new(),
            pending_frees: Vec::new(),
            spill_blocks: Vec::new(),
        })
    }

    /// Consume the volume and return the underlying device.
    #[must_use]
    pub fn into_device(self) -> D {
        self.dev
    }

    /// Borrow the underlying device (checkpoint ring inspection for tools).
    #[must_use]
    pub const fn device(&self) -> &D {
        &self.dev
    }

    /// The volume label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current transaction id (the xid of the last commit).
    #[must_use]
    pub const fn xid(&self) -> u64 {
        self.state.xid
    }

    /// Physical block of the live fs-tree root.
    #[must_use]
    pub const fn fs_root(&self) -> u64 {
        self.state.fs_root
    }

    /// The volume superblock as last written.
    #[must_use]
    pub const fn volume_superblock(&self) -> gen::nolx_fs_superblock_t {
        self.vsb
    }

    /// Volume metadata: layout plus the superblock as last committed.
    #[must_use]
    pub const fn layout(&self) -> &mkfs::Layout {
        &self.layout
    }

    /// Whether this handle is a read-only (snapshot) view.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The xid this view is pinned at, when it is a snapshot view.
    #[must_use]
    pub const fn snapshot_xid(&self) -> Option<u64> {
        self.snapshot_view
    }

    /// Whether uncommitted mutations are waiting for [`Volume::commit`].
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Free blocks according to the space manager (live volumes only).
    #[must_use]
    pub fn free_blocks(&self) -> Option<u64> {
        self.spaceman.as_ref().map(SpaceMan::free_count)
    }

    /// Live snapshots recorded in the newest checkpoint.
    #[must_use]
    pub fn snapshots(&self) -> &[SnapshotRecord] {
        &self.state.snapshots
    }

    // ------------------------------------------------------------ transactions

    fn ensure_txn(&mut self) -> Result<()> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        if self.dirty {
            return Ok(());
        }
        if self.spaceman.is_none() {
            return Err(FsError::ReadOnly);
        }
        self.pending_xid = self.state.xid + 1;
        let xid = self.pending_xid;
        // The replaced trees become reaper garbage — except the pinned
        // `mkfs` layout blocks (tree roots below `first_data_block`), which
        // live outside the bitmap-managed region and must never be freed.
        let mut old_blocks = Vec::new();
        for root in [
            self.state.fs_root,
            self.state.extentref_root,
            self.state.snapmeta_root,
            self.state.fext_root,
        ] {
            if root != 0 {
                old_blocks.extend(
                    btree::walk::blocks(&self.dev, root)?
                        .into_iter()
                        .filter(|&b| b >= self.layout.first_data_block),
                );
            }
        }
        let fs_root = self.clone_tree(TreeKind::Fs, xid)?;
        let extentref_root = self.clone_tree(TreeKind::ExtentRef, xid)?;
        let snapmeta_root = self.clone_tree(TreeKind::SnapMeta, xid)?;
        let fext_root = if self.state.fext_root != 0 {
            self.clone_tree(TreeKind::Fext, xid)?
        } else {
            0
        };
        self.state.fs_root = fs_root;
        self.state.extentref_root = extentref_root;
        self.state.snapmeta_root = snapmeta_root;
        self.state.fext_root = fext_root;
        self.old_tree_blocks = old_blocks;
        self.dirty = true;
        Ok(())
    }

    fn clone_tree(&mut self, kind: TreeKind, xid: u64) -> Result<u64> {
        let (root, subtype) = match kind {
            TreeKind::Fs => (self.state.fs_root, gen::SUB_FSTREE),
            TreeKind::ExtentRef => (self.state.extentref_root, gen::SUB_EXTENT_REF_TREE),
            TreeKind::SnapMeta => (self.state.snapmeta_root, gen::SUB_SNAP_META_TREE),
            TreeKind::Fext => (self.state.fext_root, gen::SUB_FEXT_TREE),
        };
        let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
        let mut alloc = SpaceAllocator {
            spaceman,
            deferred: &mut self.pending_frees,
        };
        btree::clone_subtree(
            &mut self.dev,
            &mut alloc,
            root,
            KeyKind::JKey,
            subtype as u16,
            xid,
        )
    }

    /// Commit the pending transaction: the `fsync` of this filesystem.
    ///
    /// With nothing pending this is a no-op. On error the volume must be
    /// dropped and remounted; the on-disk state is still the last committed
    /// checkpoint (see the module docs for the write order that proves it).
    ///
    /// # Errors
    ///
    /// Device errors, out-of-space, or corruption found while walking trees.
    pub fn commit(&mut self) -> Result<()> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        if !self.dirty {
            return Ok(());
        }
        let xid = self.pending_xid;
        // 1. Everything this transaction replaced becomes reaper garbage at xid.
        let old_blocks = core::mem::take(&mut self.old_tree_blocks);
        for block in old_blocks {
            self.reaper.defer(block, 1, xid);
        }
        let pending = core::mem::take(&mut self.pending_frees);
        for (block, len) in pending {
            self.reaper.defer(block, len, xid);
        }
        for block in core::mem::take(&mut self.spill_blocks) {
            self.reaper.defer(block, 1, xid);
        }
        // 2. Reap what no snapshot can still reach, then persist the backlog.
        let horizon = self.state.oldest_snapshot_xid();
        {
            let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
            let freed = self.reaper.reap(spaceman, horizon)?;
            self.state.total_blocks_freed += freed;
        }
        self.state.reap = self.reaper.entries().to_vec();
        self.state.xid = xid;
        // 3. Record the new tree roots in the volume object map at xid.
        {
            let vomap_tree = self.layout.volume_omap_tree;
            let mut deferred = Vec::new();
            let (fs_root, ext_root, snap_root) = (
                self.state.fs_root,
                self.state.extentref_root,
                self.state.snapmeta_root,
            );
            let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
            let mut alloc = SpaceAllocator {
                spaceman,
                deferred: &mut deferred,
            };
            let mut vomap = ObjectMap::open(&mut self.dev, &mut alloc, vomap_tree, xid);
            vomap.set(mkfs::oid::FS_TREE, xid, fs_root, 0)?;
            vomap.set(mkfs::oid::EXTENT_REF_TREE, xid, ext_root, 0)?;
            vomap.set(mkfs::oid::SNAP_META_TREE, xid, snap_root, 0)?;
            for (block, len) in deferred {
                self.reaper.defer(block, len, xid);
            }
        }
        // 4. Allocate checkpoint spill blocks, flush bitmaps, commit checkpoint.
        let spill_needed = checkpoint::spill_blocks_needed(&self.state, self.dev.block_size());
        let mut spill = Vec::with_capacity(spill_needed);
        {
            let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
            for _ in 0..spill_needed {
                spill.push(spaceman.alloc_block()?);
            }
            spaceman.flush(&mut self.dev, self.layout.spaceman, xid)?;
        }
        checkpoint::commit(
            &mut self.dev,
            &self.layout,
            &self.csb,
            &self.vsb,
            &self.state,
            &spill,
        )?;
        self.spill_blocks = spill;
        self.dirty = false;
        self.pending_xid = 0;
        Ok(())
    }

    /// Alias for [`Volume::commit`], named for the POSIX operation it backs.
    ///
    /// # Errors
    ///
    /// As [`Volume::commit`].
    pub fn sync(&mut self) -> Result<()> {
        self.commit()
    }

    // ------------------------------------------------------------ tree access

    fn tree_root(&self, kind: TreeKind) -> u64 {
        match kind {
            TreeKind::Fs => self.state.fs_root,
            TreeKind::ExtentRef => self.state.extentref_root,
            TreeKind::SnapMeta => self.state.snapmeta_root,
            TreeKind::Fext => self.state.fext_root,
        }
    }

    fn tree_subtype(kind: TreeKind) -> u32 {
        match kind {
            TreeKind::Fs => gen::SUB_FSTREE,
            TreeKind::ExtentRef => gen::SUB_EXTENT_REF_TREE,
            TreeKind::SnapMeta => gen::SUB_SNAP_META_TREE,
            TreeKind::Fext => gen::SUB_FEXT_TREE,
        }
    }

    fn tree_search(&self, kind: TreeKind, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::walk::search(&self.dev, self.tree_root(kind), KeyKind::JKey, key)
    }

    fn tree_collect(&self, kind: TreeKind) -> Result<Vec<btree::Entry>> {
        let root = self.tree_root(kind);
        if root == 0 {
            return Ok(Vec::new());
        }
        btree::walk::collect(&self.dev, root)
    }

    fn tree_insert(&mut self, kind: TreeKind, key: &[u8], val: &[u8]) -> Result<()> {
        let root = self.tree_root(kind);
        let xid = if self.dirty {
            self.pending_xid
        } else {
            self.state.xid
        };
        let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
        let mut alloc = SpaceAllocator {
            spaceman,
            deferred: &mut self.pending_frees,
        };
        let mut tree = BTree::new(
            &mut self.dev,
            &mut alloc,
            root,
            KeyKind::JKey,
            Self::tree_subtype(kind) as u16,
            xid,
        );
        tree.insert(key, val)
    }

    fn tree_remove(&mut self, kind: TreeKind, key: &[u8]) -> Result<bool> {
        let root = self.tree_root(kind);
        let xid = if self.dirty {
            self.pending_xid
        } else {
            self.state.xid
        };
        let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
        let mut alloc = SpaceAllocator {
            spaceman,
            deferred: &mut self.pending_frees,
        };
        let mut tree = BTree::new(
            &mut self.dev,
            &mut alloc,
            root,
            KeyKind::JKey,
            Self::tree_subtype(kind) as u16,
            xid,
        );
        tree.remove(key)
    }

    // ------------------------------------------------------------ inodes

    /// Read the inode record for `ino`.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`] when no inode record exists.
    pub fn read_inode(&self, ino: u64) -> Result<gen::nolx_inode_t> {
        let bytes = self
            .tree_search(TreeKind::Fs, &inode_key(ino))?
            .ok_or(FsError::NotFound)?;
        gen::nolx_inode_t::read_from(&bytes).ok_or(FsError::TooShort {
            have: bytes.len(),
            need: gen::nolx_inode_t::DISK_SIZE,
        })
    }

    /// All extent runs of `ino`, in logical order.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn extents_of(&self, ino: u64) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        for entry in self.tree_collect(TreeKind::Fs)? {
            if entry.key.len() < 16 {
                continue;
            }
            let raw = u64::from_le_bytes(
                entry.key[..8]
                    .try_into()
                    .map_err(|_| FsError::CorruptNode("extent key truncated"))?,
            );
            if jkey_type(raw) != gen::DREC_TYPE_FILE_EXTENT || jkey_oid(raw) != ino {
                continue;
            }
            let logical = u64::from_le_bytes(
                entry.key[8..16]
                    .try_into()
                    .map_err(|_| FsError::CorruptNode("extent key truncated"))?,
            );
            let val = gen::nolx_file_extent_val_t::read_from(&entry.val)
                .ok_or(FsError::CorruptNode("file extent value truncated"))?;
            out.push(Extent {
                logical,
                len: extent_len(val.len_and_flags),
                phys: val.phys_block_num,
            });
        }
        out.sort_by_key(|e| e.logical);
        Ok(out)
    }

    /// Logical size of a file or symlink, from its data-stream record.
    #[must_use]
    pub fn file_size(inode: &gen::nolx_inode_t) -> u64 {
        gen::nolx_data_stream_t::read_from(&inode.data_stream)
            .map_or(0, |ds| ds.total_bytes_written)
    }

    fn write_inode(&mut self, ino: u64, inode: &gen::nolx_inode_t) -> Result<()> {
        self.tree_insert(TreeKind::Fs, &inode_key(ino), inode.as_bytes())
    }

    fn touch(inode: &mut gen::nolx_inode_t, xid: u64, modify: bool) {
        let now = xid as i64;
        if inode.create_time == 0 {
            inode.create_time = now;
        }
        if modify {
            inode.modify_time = now;
        }
        inode.change_time = now;
    }

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.state.next_ino;
        self.state.next_ino += 1;
        ino
    }

    // ------------------------------------------------------------ directories

    /// Look up `name` inside directory `dir_ino`, returning the child inode.
    ///
    /// With a case-folding volume, an exact miss falls back to an
    /// ASCII-case-insensitive scan, preserving the stored spelling.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`], [`FsError::NotDirectory`], or device errors.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64> {
        let dir = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(dir.mode)) {
            return Err(FsError::NotDirectory);
        }
        if let Some(val) = self.tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))? {
            let dentry = gen::nolx_direntry_t::read_from(&val)
                .ok_or(FsError::CorruptNode("dentry value truncated"))?;
            return Ok(dentry.ino);
        }
        if self.casefold {
            for (stored, ino) in self.readdir(dir_ino)? {
                if stored.eq_ignore_ascii_case(name) {
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /// Enumerate `(name, child_ino)` entries of `dir_ino`, in B-tree (name) order.
    ///
    /// # Errors
    ///
    /// [`FsError::NotDirectory`] or device/corruption errors.
    pub fn readdir(&self, dir_ino: u64) -> Result<Vec<(String, u64)>> {
        let dir = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(dir.mode)) {
            return Err(FsError::NotDirectory);
        }
        let prefix = pack_jkey(gen::DREC_TYPE_DIR_REC, dir_ino);
        let mut out = Vec::new();
        for entry in self.tree_collect(TreeKind::Fs)? {
            if entry.key.len() < 8 {
                continue;
            }
            let raw = u64::from_le_bytes(
                entry.key[..8]
                    .try_into()
                    .map_err(|_| FsError::CorruptNode("dentry key truncated"))?,
            );
            if raw != prefix {
                continue;
            }
            let dentry = gen::nolx_direntry_t::read_from(&entry.val)
                .ok_or(FsError::CorruptNode("dentry value truncated"))?;
            let name_len = usize::from(dentry.name_len);
            let body = entry
                .val
                .get(gen::nolx_direntry_t::DISK_SIZE..gen::nolx_direntry_t::DISK_SIZE + name_len)
                .ok_or(FsError::CorruptNode("dentry name truncated"))?;
            let name = core::str::from_utf8(body)
                .map_err(|_| FsError::CorruptNode("dentry name is not UTF-8"))?;
            out.push((String::from(name), dentry.ino));
        }
        Ok(out)
    }

    fn add_dentry(
        &mut self,
        dir_ino: u64,
        name: &str,
        child: u64,
        inode: &gen::nolx_inode_t,
    ) -> Result<()> {
        let file_type = ((u32::from(inode.mode) & 0o170_000) >> 12) as u16;
        self.tree_insert(
            TreeKind::Fs,
            &dentry_key(dir_ino, name),
            &dentry_val(child, name, file_type),
        )?;
        // One sibling record per directory entry makes nlink auditable by fsck.
        self.tree_insert(
            TreeKind::Fs,
            &sibling_key(child, dir_ino, name),
            &sibling_val(dir_ino, name, file_type),
        )?;
        self.tree_insert(
            TreeKind::Fs,
            &sibling_map_key(child, dir_ino),
            &dir_ino.to_le_bytes(),
        )?;
        self.bump_dir_stats(dir_ino, 1)?;
        Ok(())
    }

    fn remove_dentry(&mut self, dir_ino: u64, name: &str, child: u64) -> Result<()> {
        self.tree_remove(TreeKind::Fs, &dentry_key(dir_ino, name))?;
        self.tree_remove(TreeKind::Fs, &sibling_key(child, dir_ino, name))?;
        self.tree_remove(TreeKind::Fs, &sibling_map_key(child, dir_ino))?;
        self.bump_dir_stats(dir_ino, -1)?;
        Ok(())
    }

    fn bump_dir_stats(&mut self, dir_ino: u64, delta: i64) -> Result<()> {
        let key = dir_stats_key(dir_ino);
        let mut stats = self
            .tree_search(TreeKind::Fs, &key)?
            .and_then(|v| gen::nolx_dir_stats_val_t::read_from(&v))
            .unwrap_or(gen::nolx_dir_stats_val_t {
                num_children: 0,
                total_size: 0,
                chained_key: 0,
                gen_count: 0,
            });
        if delta >= 0 {
            stats.num_children += delta as u64;
        } else {
            stats.num_children = stats.num_children.saturating_sub(delta.unsigned_abs());
        }
        stats.gen_count += 1;
        self.tree_insert(TreeKind::Fs, &key, stats.as_bytes())
    }

    /// Create a subdirectory `name` in `dir_ino`; returns its inode number.
    ///
    /// # Errors
    ///
    /// [`FsError::AlreadyExists`], [`FsError::NotDirectory`],
    /// [`FsError::NameTooLong`], [`FsError::ReadOnly`], or device errors.
    pub fn mkdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
    ) -> Result<u64> {
        validate_name(name)?;
        let mut parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(FsError::NotDirectory);
        }
        if self
            .tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))?
            .is_some()
        {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_txn()?;
        let ino = self.alloc_ino();
        let xid = self.pending_xid;
        let mut child = new_inode(
            dir_ino,
            ino,
            (S_IFDIR | (mode & 0o7777)) as u16,
            2,
            uid,
            gid,
            xid,
        );
        Self::touch(&mut child, xid, true);
        parent.nlink = parent.nlink.saturating_add(1);
        Self::touch(&mut parent, xid, false);
        self.write_inode(dir_ino, &parent)?;
        self.write_inode(ino, &child)?;
        self.tree_insert(TreeKind::Fs, &dstream_key(ino), &ino.to_le_bytes())?;
        self.add_dentry(dir_ino, name, ino, &child)?;
        self.state.num_dirs += 1;
        Ok(ino)
    }

    /// Create an empty regular file `name` in `dir_ino`; returns its inode.
    ///
    /// # Errors
    ///
    /// As [`Volume::mkdir`].
    pub fn create_file(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
    ) -> Result<u64> {
        validate_name(name)?;
        let parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(FsError::NotDirectory);
        }
        if self
            .tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))?
            .is_some()
        {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_txn()?;
        let ino = self.alloc_ino();
        let xid = self.pending_xid;
        let mut inode = new_inode(
            dir_ino,
            ino,
            (S_IFREG | (mode & 0o7777)) as u16,
            1,
            uid,
            gid,
            xid,
        );
        Self::touch(&mut inode, xid, true);
        self.write_inode(ino, &inode)?;
        self.tree_insert(TreeKind::Fs, &dstream_key(ino), &ino.to_le_bytes())?;
        let doc_id = self.state.next_doc_id;
        self.state.next_doc_id += 1;
        let info = gen::nolx_file_info_val_t {
            doc_id,
            hidden_bytes: 0,
            hidden_xattr_count: 0,
            pad: 0,
        };
        self.tree_insert(TreeKind::Fs, &file_info_key(ino), info.as_bytes())?;
        self.add_dentry(dir_ino, name, ino, &inode)?;
        self.state.num_files += 1;
        Ok(ino)
    }

    /// Create a symbolic link `name` in `dir_ino` pointing at `target`.
    ///
    /// # Errors
    ///
    /// As [`Volume::mkdir`].
    pub fn symlink(
        &mut self,
        dir_ino: u64,
        name: &str,
        target: &[u8],
        uid: Uid,
        gid: Gid,
    ) -> Result<u64> {
        validate_name(name)?;
        let parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(FsError::NotDirectory);
        }
        if target.len() > 4096 {
            return Err(FsError::NameTooLong);
        }
        if self
            .tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))?
            .is_some()
        {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_txn()?;
        let ino = self.alloc_ino();
        let xid = self.pending_xid;
        let mut inode = new_inode(dir_ino, ino, (S_IFLNK | 0o777) as u16, 1, uid, gid, xid);
        Self::touch(&mut inode, xid, true);
        self.write_inode(ino, &inode)?;
        self.tree_insert(TreeKind::Fs, &dstream_key(ino), &ino.to_le_bytes())?;
        self.add_dentry(dir_ino, name, ino, &inode)?;
        self.state.num_symlinks += 1;
        // The target is the link's data, stored through the extent machinery.
        self.write_data(ino, 0, target)?;
        Ok(ino)
    }

    /// Read a symlink's target bytes.
    ///
    /// # Errors
    ///
    /// [`FsError::InvalidField`] for a non-symlink, or device errors.
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if !is_lnk(u32::from(inode.mode)) {
            return Err(FsError::InvalidField("not a symbolic link"));
        }
        let size = Self::file_size(&inode) as usize;
        let mut buf = vec![0u8; size];
        let n = self.read_data(ino, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Create a hard link `name` in `dir_ino` to the existing inode `target`.
    ///
    /// # Errors
    ///
    /// [`FsError::IsDirectory`] for directories (directories cannot be hard
    /// linked), [`FsError::AlreadyExists`], or device errors.
    pub fn link(&mut self, target: u64, dir_ino: u64, name: &str) -> Result<()> {
        validate_name(name)?;
        let mut inode = self.read_inode(target)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(FsError::IsDirectory);
        }
        let parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(FsError::NotDirectory);
        }
        if self
            .tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))?
            .is_some()
        {
            return Err(FsError::AlreadyExists);
        }
        self.ensure_txn()?;
        inode.nlink = inode.nlink.saturating_add(1);
        Self::touch(&mut inode, self.pending_xid, false);
        self.write_inode(target, &inode)?;
        self.add_dentry(dir_ino, name, target, &inode)?;
        Ok(())
    }

    /// Remove the directory entry `name` from `dir_ino` (files, symlinks, and
    /// other non-directories). The inode and its extents are destroyed when
    /// the last link goes away; shared extent blocks survive through their
    /// reference counts.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`], [`FsError::IsDirectory`] (use
    /// [`Volume::rmdir`]), or device errors.
    pub fn unlink(&mut self, dir_ino: u64, name: &str) -> Result<()> {
        let child = self.lookup(dir_ino, name)?;
        let mut inode = self.read_inode(child)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(FsError::IsDirectory);
        }
        self.ensure_txn()?;
        self.remove_dentry(dir_ino, name, child)?;
        inode.nlink = inode.nlink.saturating_sub(1);
        if inode.nlink > 0 {
            Self::touch(&mut inode, self.pending_xid, false);
            self.write_inode(child, &inode)?;
            return Ok(());
        }
        self.destroy_inode(child, &inode)?;
        Ok(())
    }

    /// Remove the empty subdirectory `name` from `dir_ino`.
    ///
    /// # Errors
    ///
    /// [`FsError::NotEmpty`], [`FsError::NotDirectory`], or device errors.
    pub fn rmdir(&mut self, dir_ino: u64, name: &str) -> Result<()> {
        let child = self.lookup(dir_ino, name)?;
        let inode = self.read_inode(child)?;
        if !is_dir(u32::from(inode.mode)) {
            return Err(FsError::NotDirectory);
        }
        if child == gen::ROOT_INO {
            return Err(FsError::InvalidField("cannot remove the root directory"));
        }
        if !self.readdir(child)?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        self.ensure_txn()?;
        self.remove_dentry(dir_ino, name, child)?;
        let mut parent = self.read_inode(dir_ino)?;
        parent.nlink = parent.nlink.saturating_sub(1).max(2);
        Self::touch(&mut parent, self.pending_xid, false);
        self.write_inode(dir_ino, &parent)?;
        self.destroy_inode(child, &inode)?;
        Ok(())
    }

    /// Rename `old_name` in `old_dir` to `new_name` in `new_dir`, replacing an
    /// existing non-directory (or empty directory) target as POSIX requires.
    ///
    /// # Errors
    ///
    /// [`FsError::InvalidField`] when a directory would move into its own
    /// descendant, [`FsError::NotEmpty`] for a non-empty directory target, or
    /// device errors.
    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        validate_name(new_name)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
        let child = self.lookup(old_dir, old_name)?;
        let inode = self.read_inode(child)?;
        let moving_dir = is_dir(u32::from(inode.mode));
        if moving_dir {
            // Reject moving a directory into its own descendant.
            let mut cursor = new_dir;
            loop {
                if cursor == child {
                    return Err(FsError::InvalidField(
                        "cannot move a directory into its own descendant",
                    ));
                }
                if cursor == gen::ROOT_INO {
                    break;
                }
                cursor = self.read_inode(cursor)?.parent_id;
            }
        }
        if let Ok(existing) = self.lookup(new_dir, new_name) {
            if existing == child {
                return Ok(());
            }
            let existing_inode = self.read_inode(existing)?;
            if is_dir(u32::from(existing_inode.mode)) {
                if !moving_dir {
                    return Err(FsError::IsDirectory);
                }
                if !self.readdir(existing)?.is_empty() {
                    return Err(FsError::NotEmpty);
                }
                self.rmdir(new_dir, new_name)?;
            } else {
                if moving_dir {
                    return Err(FsError::NotDirectory);
                }
                self.unlink(new_dir, new_name)?;
            }
        }
        self.ensure_txn()?;
        self.remove_dentry(old_dir, old_name, child)?;
        self.add_dentry(new_dir, new_name, child, &inode)?;
        let mut moved = inode;
        moved.parent_id = new_dir;
        Self::touch(&mut moved, self.pending_xid, false);
        self.write_inode(child, &moved)?;
        if moving_dir && old_dir != new_dir {
            let mut old_parent = self.read_inode(old_dir)?;
            old_parent.nlink = old_parent.nlink.saturating_sub(1).max(2);
            self.write_inode(old_dir, &old_parent)?;
            let mut new_parent = self.read_inode(new_dir)?;
            new_parent.nlink = new_parent.nlink.saturating_add(1);
            self.write_inode(new_dir, &new_parent)?;
        }
        Ok(())
    }

    fn destroy_inode(&mut self, ino: u64, inode: &gen::nolx_inode_t) -> Result<()> {
        // Extents first: decrement every block reference; blocks whose count
        // reaches zero are deferred to the reaper by `release_extent_block`.
        let extents = self.extents_of(ino)?;
        let bs = self.dev.block_size() as u64;
        for extent in &extents {
            let blocks = extent.len.div_ceil(bs);
            for i in 0..blocks {
                self.release_extent_block(extent.phys + i)?;
            }
            self.tree_remove(TreeKind::Fs, &extent_key(ino, extent.logical))?;
            self.tree_remove(TreeKind::Fext, &fext_key(ino, extent.logical))?;
        }
        // Xattrs.
        let prefix = pack_jkey(gen::DREC_TYPE_XATTR, ino);
        let mut xattr_keys = Vec::new();
        for entry in self.tree_collect(TreeKind::Fs)? {
            if entry.key.len() >= 8 {
                let raw = u64::from_le_bytes(
                    entry.key[..8]
                        .try_into()
                        .map_err(|_| FsError::CorruptNode("xattr key truncated"))?,
                );
                if raw == prefix {
                    xattr_keys.push(entry.key.clone());
                }
            }
        }
        for key in xattr_keys {
            self.tree_remove(TreeKind::Fs, &key)?;
        }
        self.tree_remove(TreeKind::Fs, &dstream_key(ino))?;
        self.tree_remove(TreeKind::Fs, &file_info_key(ino))?;
        self.tree_remove(TreeKind::Fs, &inode_key(ino))?;
        match u32::from(inode.mode) & 0o170_000 {
            x if x == S_IFDIR => self.state.num_dirs = self.state.num_dirs.saturating_sub(1),
            x if x == S_IFREG => self.state.num_files = self.state.num_files.saturating_sub(1),
            x if x == S_IFLNK => {
                self.state.num_symlinks = self.state.num_symlinks.saturating_sub(1)
            }
            _ => self.state.num_other = self.state.num_other.saturating_sub(1),
        }
        Ok(())
    }

    // ------------------------------------------------------------ data

    /// Read up to `buf.len()` bytes of file/symlink `ino` at `offset`; holes
    /// read as zeroes. Returns the byte count (short at end of file).
    ///
    /// # Errors
    ///
    /// [`FsError::IsDirectory`] for directories, or device errors.
    pub fn read_data(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(FsError::IsDirectory);
        }
        let size = Self::file_size(&inode);
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let want = ((size - offset) as usize).min(buf.len());
        buf[..want].fill(0);
        let bs = self.dev.block_size();
        let extents = self.extents_of(ino)?;
        let mut block_buf = vec![0u8; bs];
        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let block_index = pos / bs as u64;
            let in_block = (pos % bs as u64) as usize;
            let chunk = core::cmp::min(bs - in_block, want - done);
            if let Some(extent) = extents.iter().find(|e| {
                let start = e.logical / bs as u64;
                let count = e.len.div_ceil(bs as u64);
                block_index >= start && block_index < start + count
            }) {
                let start = extent.logical / bs as u64;
                self.dev
                    .read_block(extent.phys + (block_index - start), &mut block_buf)?;
                buf[done..done + chunk].copy_from_slice(&block_buf[in_block..in_block + chunk]);
            }
            done += chunk;
        }
        Ok(want)
    }

    /// Write `data` to file/symlink `ino` at `offset`, allocating fresh extent
    /// blocks (copy-on-write: overwritten blocks are reference-decremented,
    /// never modified in place). Returns `data.len()`.
    ///
    /// # Errors
    ///
    /// [`FsError::IsDirectory`], [`FsError::ReadOnly`], out-of-space, or
    /// device errors.
    pub fn write_data(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(FsError::IsDirectory);
        }
        if data.is_empty() {
            return Ok(0);
        }
        // Preserve the bytes around the written range inside the edge blocks.
        let bs = self.dev.block_size() as u64;
        let range_start = offset / bs * bs;
        let range_end = (offset + data.len() as u64).div_ceil(bs) * bs;
        let mut window = vec![0u8; (range_end - range_start) as usize];
        let old_size = Self::file_size(&inode);
        if old_size > range_start {
            let readable = core::cmp::min(old_size, range_end);
            let n = self.read_data(
                ino,
                range_start,
                &mut window[..(readable - range_start) as usize],
            )?;
            let _ = n;
        }
        self.ensure_txn()?;
        let mut inode = self.read_inode(ino)?;
        let blocks_needed = (range_end - range_start) / bs;
        let run = {
            let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
            spaceman.alloc(blocks_needed)?
        };
        self.state.total_blocks_alloced += blocks_needed;
        window[(offset - range_start) as usize..(offset - range_start) as usize + data.len()]
            .copy_from_slice(data);
        for i in 0..blocks_needed {
            let start = (i * bs) as usize;
            self.dev
                .write_block(run + i, &window[start..start + bs as usize])?;
            self.claim_extent_block(run + i)?;
        }
        // Rewrite the file's extent set: preserved segments of old extents
        // plus the new run.
        let old_extents = self.extents_of(ino)?;
        let mut new_extents: Vec<Extent> = Vec::new();
        for extent in &old_extents {
            let ext_end = extent.logical + extent.len;
            if extent.logical < range_start {
                let keep_len = core::cmp::min(ext_end, range_start) - extent.logical;
                new_extents.push(Extent {
                    logical: extent.logical,
                    len: keep_len,
                    phys: extent.phys,
                });
            }
            if ext_end > range_end {
                let skip = range_end - extent.logical;
                new_extents.push(Extent {
                    logical: range_end,
                    len: ext_end - range_end,
                    phys: extent.phys + skip / bs,
                });
            }
        }
        new_extents.push(Extent {
            logical: range_start,
            len: range_end - range_start,
            phys: run,
        });
        new_extents.sort_by_key(|e| e.logical);
        self.rewrite_extents(ino, &old_extents, &new_extents)?;
        let new_size = core::cmp::max(old_size, offset + data.len() as u64);
        set_file_size(
            &mut inode,
            new_size,
            new_extents.iter().map(|e| e.len).sum(),
        );
        Self::touch(&mut inode, self.pending_xid, true);
        self.write_inode(ino, &inode)?;
        Ok(data.len())
    }

    /// Truncate file `ino` to `new_size`; extending creates a sparse hole.
    ///
    /// # Errors
    ///
    /// [`FsError::IsDirectory`] or device errors.
    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<()> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(FsError::IsDirectory);
        }
        let old_size = Self::file_size(&inode);
        if new_size == old_size {
            return Ok(());
        }
        self.ensure_txn()?;
        let bs = self.dev.block_size() as u64;
        let old_extents = self.extents_of(ino)?;
        let keep_end = new_size.div_ceil(bs) * bs;
        let mut new_extents = Vec::new();
        for extent in &old_extents {
            if extent.logical < keep_end {
                let len = core::cmp::min(extent.len, keep_end - extent.logical);
                new_extents.push(Extent {
                    logical: extent.logical,
                    len,
                    phys: extent.phys,
                });
            }
        }
        self.rewrite_extents(ino, &old_extents, &new_extents)?;
        let mut inode = self.read_inode(ino)?;
        set_file_size(
            &mut inode,
            new_size,
            new_extents.iter().map(|e| e.len).sum(),
        );
        Self::touch(&mut inode, self.pending_xid, true);
        self.write_inode(ino, &inode)?;
        Ok(())
    }

    /// Clone file `src` as `name` in `dir_ino` in O(extents): the new inode
    /// shares every physical block, with extent reference counts incremented,
    /// and is flagged `CLONED`. Returns the new inode.
    ///
    /// # Errors
    ///
    /// [`FsError::IsDirectory`] for directories, or device errors.
    pub fn clone_file(&mut self, src: u64, dir_ino: u64, name: &str) -> Result<u64> {
        validate_name(name)?;
        let source = self.read_inode(src)?;
        if is_dir(u32::from(source.mode)) {
            return Err(FsError::IsDirectory);
        }
        let parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(FsError::NotDirectory);
        }
        if self
            .tree_search(TreeKind::Fs, &dentry_key(dir_ino, name))?
            .is_some()
        {
            return Err(FsError::AlreadyExists);
        }
        let extents = self.extents_of(src)?;
        self.ensure_txn()?;
        let ino = self.alloc_ino();
        let xid = self.pending_xid;
        let mut inode = source;
        inode.parent_id = dir_ino;
        inode.private_id = ino;
        inode.nlink = 1;
        inode.generation = inode.generation.wrapping_add(1);
        inode.internal_flags |= u64::from(gen::INODE_FLAG_CLONED);
        Self::touch(&mut inode, xid, true);
        inode.create_time = xid as i64;
        self.write_inode(ino, &inode)?;
        self.tree_insert(TreeKind::Fs, &dstream_key(ino), &ino.to_le_bytes())?;
        for extent in &extents {
            self.tree_insert(
                TreeKind::Fs,
                &extent_key(ino, extent.logical),
                extent_val(extent).as_bytes(),
            )?;
            if self.state.fext_root != 0 {
                self.tree_insert(
                    TreeKind::Fext,
                    &fext_key(ino, extent.logical),
                    fext_val(extent).as_bytes(),
                )?;
            }
            let bs = self.dev.block_size() as u64;
            for i in 0..extent.len.div_ceil(bs) {
                self.claim_extent_block(extent.phys + i)?;
            }
        }
        self.add_dentry(dir_ino, name, ino, &inode)?;
        if is_lnk(u32::from(inode.mode)) {
            self.state.num_symlinks += 1;
        } else {
            self.state.num_files += 1;
        }
        Ok(ino)
    }

    fn rewrite_extents(&mut self, ino: u64, old: &[Extent], new: &[Extent]) -> Result<()> {
        let bs = self.dev.block_size() as u64;
        // Release blocks that are not part of the new set, claim nothing here
        // (new-run blocks are claimed by the writer; preserved segments keep
        // the references they already hold).
        let mut new_blocks: Vec<u64> = Vec::new();
        for extent in new {
            for i in 0..extent.len.div_ceil(bs) {
                new_blocks.push(extent.phys + i);
            }
        }
        for extent in old {
            for i in 0..extent.len.div_ceil(bs) {
                let block = extent.phys + i;
                if !new_blocks.contains(&block) {
                    self.release_extent_block(block)?;
                }
            }
        }
        for extent in old {
            self.tree_remove(TreeKind::Fs, &extent_key(ino, extent.logical))?;
            self.tree_remove(TreeKind::Fext, &fext_key(ino, extent.logical))?;
        }
        for extent in new {
            self.tree_insert(
                TreeKind::Fs,
                &extent_key(ino, extent.logical),
                extent_val(extent).as_bytes(),
            )?;
            if self.state.fext_root != 0 {
                self.tree_insert(
                    TreeKind::Fext,
                    &fext_key(ino, extent.logical),
                    fext_val(extent).as_bytes(),
                )?;
            }
        }
        Ok(())
    }

    /// Increment the reference count of one physical extent block.
    fn claim_extent_block(&mut self, block: u64) -> Result<()> {
        let key = phys_ext_key(block);
        let current = self
            .tree_search(TreeKind::ExtentRef, &key)?
            .and_then(|v| decode_refcount(&v))
            .unwrap_or(0);
        self.tree_insert(
            TreeKind::ExtentRef,
            &key,
            &encode_refcount(block, current + 1),
        )?;
        Ok(())
    }

    /// Decrement a reference count; at zero the block is deferred to the
    /// reaper as garbage of the pending transaction.
    fn release_extent_block(&mut self, block: u64) -> Result<()> {
        let key = phys_ext_key(block);
        let current = self
            .tree_search(TreeKind::ExtentRef, &key)?
            .and_then(|v| decode_refcount(&v))
            .unwrap_or(0);
        if current <= 1 {
            self.tree_remove(TreeKind::ExtentRef, &key)?;
            let xid = if self.dirty {
                self.pending_xid
            } else {
                self.state.xid
            };
            self.reaper.defer(block, 1, xid);
        } else {
            self.tree_insert(
                TreeKind::ExtentRef,
                &key,
                &encode_refcount(block, current - 1),
            )?;
        }
        Ok(())
    }

    /// The reference count recorded for a physical block (0 = unreferenced).
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn extent_refcount(&self, block: u64) -> Result<u64> {
        Ok(self
            .tree_search(TreeKind::ExtentRef, &phys_ext_key(block))?
            .and_then(|v| decode_refcount(&v))
            .unwrap_or(0))
    }

    // ------------------------------------------------------------ xattrs

    /// Set extended attribute `name` on `ino` to `value`, replacing any old one.
    ///
    /// # Errors
    ///
    /// [`FsError::NameTooLong`] for names over `NAME_MAX`, or device errors.
    pub fn set_xattr(&mut self, ino: u64, name: &str, value: &[u8]) -> Result<()> {
        if name.is_empty() || name.len() > gen::MAX_NAME_LEN as usize {
            return Err(FsError::NameTooLong);
        }
        self.read_inode(ino)?;
        self.ensure_txn()?;
        self.tree_insert(TreeKind::Fs, &xattr_key(ino, name), value)?;
        Ok(())
    }

    /// Read extended attribute `name` from `ino`.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`] when the attribute is absent.
    pub fn get_xattr(&self, ino: u64, name: &str) -> Result<Vec<u8>> {
        self.read_inode(ino)?;
        self.tree_search(TreeKind::Fs, &xattr_key(ino, name))?
            .ok_or(FsError::NotFound)
    }

    /// Remove extended attribute `name` from `ino`.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`] when the attribute is absent.
    pub fn remove_xattr(&mut self, ino: u64, name: &str) -> Result<()> {
        self.read_inode(ino)?;
        self.ensure_txn()?;
        if !self.tree_remove(TreeKind::Fs, &xattr_key(ino, name))? {
            return Err(FsError::NotFound);
        }
        Ok(())
    }

    /// List extended attribute names on `ino`, in B-tree order.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn list_xattrs(&self, ino: u64) -> Result<Vec<String>> {
        self.read_inode(ino)?;
        let prefix = pack_jkey(gen::DREC_TYPE_XATTR, ino);
        let mut out = Vec::new();
        for entry in self.tree_collect(TreeKind::Fs)? {
            if entry.key.len() < 8 {
                continue;
            }
            let raw = u64::from_le_bytes(
                entry.key[..8]
                    .try_into()
                    .map_err(|_| FsError::CorruptNode("xattr key truncated"))?,
            );
            if raw != prefix {
                continue;
            }
            // Key layout: 12-byte xattr header (jkey + name_len) then the name.
            if entry.key.len() < gen::nolx_xattr_key_t::DISK_SIZE {
                continue;
            }
            let name = core::str::from_utf8(&entry.key[gen::nolx_xattr_key_t::DISK_SIZE..])
                .map_err(|_| FsError::CorruptNode("xattr name is not UTF-8"))?;
            out.push(String::from(name));
        }
        Ok(out)
    }

    // ------------------------------------------------------------ snapshots

    /// Create a snapshot called `name`, pinning the currently committed state.
    ///
    /// Commits any pending transaction first. Creation is metadata-only: the
    /// snapshot's tree roots are the live roots, marked `SAVED` in the volume
    /// object map; every later transaction clones before writing, so the
    /// pinned trees are never modified.
    ///
    /// # Errors
    ///
    /// [`FsError::AlreadyExists`] for a duplicate name, or device errors.
    pub fn create_snapshot(&mut self, name: &str) -> Result<u64> {
        if name.is_empty() || name.len() > 255 {
            return Err(FsError::NameTooLong);
        }
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        if self.state.snapshot_named(name).is_some() {
            return Err(FsError::AlreadyExists);
        }
        self.commit()?;
        let snap_xid = self.state.xid;
        let (fs_root, extentref_root, snapmeta_root, fext_root) = (
            self.state.fs_root,
            self.state.extentref_root,
            self.state.snapmeta_root,
            self.state.fext_root,
        );
        // Counts from a walk of the pinned tree.
        let (mut files, mut dirs, mut links, mut other) = (0u64, 0u64, 0u64, 0u64);
        for entry in self.tree_collect(TreeKind::Fs)? {
            if entry.key.len() < 8 {
                continue;
            }
            let raw = u64::from_le_bytes(
                entry.key[..8]
                    .try_into()
                    .map_err(|_| FsError::CorruptNode("inode key truncated"))?,
            );
            if jkey_type(raw) == gen::DREC_TYPE_INODE {
                if let Some(inode) = gen::nolx_inode_t::read_from(&entry.val) {
                    match u32::from(inode.mode) & 0o170_000 {
                        x if x == S_IFDIR => dirs += 1,
                        x if x == S_IFREG => files += 1,
                        x if x == S_IFLNK => links += 1,
                        _ => other += 1,
                    }
                }
            }
        }
        let record = SnapshotRecord {
            xid: snap_xid,
            name: String::from(name),
            fs_root,
            extentref_root,
            snapmeta_root,
            fext_root,
            num_files: files,
            num_dirs: dirs,
            num_symlinks: links,
            num_other: other,
            create_time: snap_xid as i64,
        };
        self.ensure_txn()?;
        // The cloned snapshot tree gets the new metadata record. The pinned
        // trees are deferred like any replaced tree, but the reaper horizon
        // (this snapshot's xid) keeps them alive while the snapshot exists.
        let meta = snap_metadata_val(&record);
        self.tree_insert(TreeKind::SnapMeta, &snap_xid.to_le_bytes(), &meta)?;
        self.state.snapshots.push(record);
        // Mark the pinned roots SAVED in the volume object map.
        {
            let mut deferred = Vec::new();
            let spaceman = self.spaceman.as_mut().ok_or(FsError::ReadOnly)?;
            let mut alloc = SpaceAllocator {
                spaceman,
                deferred: &mut deferred,
            };
            let mut vomap = ObjectMap::open(
                &mut self.dev,
                &mut alloc,
                self.layout.volume_omap_tree,
                self.pending_xid,
            );
            vomap.mark_saved(mkfs::oid::FS_TREE, snap_xid, fs_root)?;
        }
        self.commit()?;
        Ok(snap_xid)
    }

    /// Delete snapshot `name`; its uniquely held tree blocks and extent
    /// references are released to the reaper at the commit horizon.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`], or device errors.
    pub fn delete_snapshot(&mut self, name: &str) -> Result<()> {
        let record = self
            .state
            .snapshot_named(name)
            .cloned()
            .ok_or(FsError::NotFound)?;
        self.commit()?;
        // Release the snapshot's extent references through the live tree.
        let snap_extents = {
            let mut out: Vec<Extent> = Vec::new();
            for entry in btree::walk::collect(&self.dev, record.fs_root)? {
                if entry.key.len() < 16 {
                    continue;
                }
                let raw = u64::from_le_bytes(
                    entry.key[..8]
                        .try_into()
                        .map_err(|_| FsError::CorruptNode("extent key truncated"))?,
                );
                if jkey_type(raw) != gen::DREC_TYPE_FILE_EXTENT {
                    continue;
                }
                let logical = u64::from_le_bytes(
                    entry.key[8..16]
                        .try_into()
                        .map_err(|_| FsError::CorruptNode("extent key truncated"))?,
                );
                if let Some(val) = gen::nolx_file_extent_val_t::read_from(&entry.val) {
                    out.push(Extent {
                        logical,
                        len: extent_len(val.len_and_flags),
                        phys: val.phys_block_num,
                    });
                }
            }
            out
        };
        let snap_tree_blocks = {
            let mut blocks = btree::walk::blocks(&self.dev, record.fs_root)?;
            blocks.extend(btree::walk::blocks(&self.dev, record.extentref_root)?);
            blocks
        };
        self.ensure_txn()?;
        let bs = self.dev.block_size() as u64;
        // Extent blocks the snapshot references are protected by the reaper
        // horizon, not by the live refcount: leave every block the live tree
        // still references alone, and never defer a block twice (the space
        // manager rejects double frees). The snapshot's tree blocks were
        // already deferred when the snapshot was created; they simply become
        // reapable now that the horizon can advance past them.
        let mut backlogged: alloc::collections::BTreeSet<u64> = self
            .reaper
            .entries()
            .iter()
            .flat_map(|e| (e.block..e.block + e.len))
            .collect();
        for extent in &snap_extents {
            for i in 0..extent.len.div_ceil(bs) {
                let block = extent.phys + i;
                if self.extent_refcount(block)? == 0
                    && block >= self.layout.first_data_block
                    && backlogged.insert(block)
                {
                    self.reaper.defer(block, 1, self.pending_xid);
                }
            }
        }
        for block in snap_tree_blocks {
            if block >= self.layout.first_data_block && backlogged.insert(block) {
                self.reaper.defer(block, 1, self.pending_xid);
            }
        }
        self.tree_remove(TreeKind::SnapMeta, &record.xid.to_le_bytes())?;
        self.state.snapshots.retain(|s| s.name != name);
        self.commit()?;
        Ok(())
    }

    /// Rename snapshot `old` to `new`, updating the metadata tree record.
    ///
    /// # Errors
    ///
    /// [`FsError::NotFound`] or [`FsError::AlreadyExists`], or device errors.
    pub fn rename_snapshot(&mut self, old: &str, new: &str) -> Result<()> {
        if self.state.snapshot_named(old).is_none() {
            return Err(FsError::NotFound);
        }
        if self.state.snapshot_named(new).is_some() {
            return Err(FsError::AlreadyExists);
        }
        self.commit()?;
        self.ensure_txn()?;
        if let Some(record) = self.state.snapshots.iter_mut().find(|s| s.name == old) {
            record.name = String::from(new);
            let meta = snap_metadata_val(record);
            let key = record.xid.to_le_bytes();
            self.tree_insert(TreeKind::SnapMeta, &key, &meta)?;
        }
        self.commit()?;
        Ok(())
    }
}

// --------------------------------------------------------------------- keys

fn new_inode(
    parent: u64,
    ino: u64,
    mode: u16,
    nlink: u32,
    uid: Uid,
    gid: Gid,
    xid: u64,
) -> gen::nolx_inode_t {
    let mut inode = gen::nolx_inode_t::zeroed();
    inode.parent_id = parent;
    inode.private_id = ino;
    inode.create_time = xid as i64;
    inode.modify_time = xid as i64;
    inode.change_time = xid as i64;
    inode.nlink = nlink;
    inode.mode = mode;
    inode.owner = uid;
    inode.group = gid;
    inode.generation = 1;
    inode
}

fn set_file_size(inode: &mut gen::nolx_inode_t, logical: u64, alloced: u64) {
    let ds = gen::nolx_data_stream_t {
        xattr_jhdr: [0u8; 16],
        alloced_size: alloced,
        default_crypto_id: 0,
        total_bytes_written: logical,
        total_bytes_read: 0,
    };
    inode.data_stream.copy_from_slice(ds.as_bytes());
}

fn inode_key(ino: u64) -> Vec<u8> {
    pack_jkey(gen::DREC_TYPE_INODE, ino).to_le_bytes().to_vec()
}

fn dstream_key(ino: u64) -> Vec<u8> {
    pack_jkey(gen::DREC_TYPE_DSTREAM_ID, ino)
        .to_le_bytes()
        .to_vec()
}

fn file_info_key(ino: u64) -> Vec<u8> {
    pack_jkey(gen::DREC_TYPE_FILE_INFO, ino)
        .to_le_bytes()
        .to_vec()
}

fn dir_stats_key(ino: u64) -> Vec<u8> {
    pack_jkey(gen::DREC_TYPE_DIR_STATS, ino)
        .to_le_bytes()
        .to_vec()
}

fn dentry_key(parent: u64, name: &str) -> Vec<u8> {
    let hdr = gen::nolx_dentry_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_DIR_REC, parent),
        name_len: name.len() as u16,
        pad: 0,
    };
    // `as_bytes` covers `size_of`, which pads this 12-byte header to 16; the
    // on-disk key is the DISK_SIZE prefix, and the padding bytes are
    // unspecified — including them would make the key depend on stack garbage.
    let mut key = hdr.as_bytes()[..gen::nolx_dentry_key_t::DISK_SIZE].to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

fn dentry_val(child: u64, name: &str, file_type: u16) -> Vec<u8> {
    let val = gen::nolx_direntry_t {
        ino: child,
        name_len: name.len() as u16,
        flags: file_type,
        nlen: name.len() as u16,
        pad: 0,
    };
    let mut bytes = val.as_bytes().to_vec();
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn sibling_key(child: u64, parent: u64, name: &str) -> Vec<u8> {
    let hdr = gen::nolx_sibling_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_SIBLING_LINK, child),
        sibling_id: parent,
        name_len: name.len() as u16,
        pad: 0,
    };
    let mut key = hdr.as_bytes()[..gen::nolx_sibling_key_t::DISK_SIZE].to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

fn sibling_val(parent: u64, name: &str, file_type: u16) -> Vec<u8> {
    let val = gen::nolx_sibling_val_t {
        parent_id: parent,
        nlen: name.len() as u16,
        r#type: file_type,
    };
    let mut bytes = val.as_bytes()[..gen::nolx_sibling_val_t::DISK_SIZE].to_vec();
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn sibling_map_key(child: u64, parent: u64) -> Vec<u8> {
    let mut key = pack_jkey(gen::DREC_TYPE_SIBLING_MAP, child)
        .to_le_bytes()
        .to_vec();
    key.extend_from_slice(&parent.to_le_bytes());
    key
}

fn xattr_key(ino: u64, name: &str) -> Vec<u8> {
    let hdr = gen::nolx_xattr_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_XATTR, ino),
        name_len: name.len() as u16,
        pad: 0,
    };
    let mut key = hdr.as_bytes()[..gen::nolx_xattr_key_t::DISK_SIZE].to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

fn extent_key(ino: u64, logical: u64) -> Vec<u8> {
    let key = gen::nolx_file_extent_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_FILE_EXTENT, ino),
        logical_addr: logical,
    };
    key.as_bytes().to_vec()
}

fn fext_key(ino: u64, logical: u64) -> Vec<u8> {
    let key = gen::nolx_fext_tree_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_FILE_EXTENT, ino),
        fext_id: logical,
    };
    key.as_bytes().to_vec()
}

fn extent_val(extent: &Extent) -> gen::nolx_file_extent_val_t {
    gen::nolx_file_extent_val_t {
        len_and_flags: pack_len_and_flags(extent.len, 0),
        phys_block_num: extent.phys,
        crypto_id: 0,
    }
}

fn fext_val(extent: &Extent) -> gen::nolx_fext_tree_val_t {
    gen::nolx_fext_tree_val_t {
        len_and_flags: pack_len_and_flags(extent.len, 0),
        phys_block_num: extent.phys,
    }
}

fn phys_ext_key(block: u64) -> Vec<u8> {
    pack_jkey(gen::DREC_TYPE_PHYS_EXTENT, block)
        .to_le_bytes()
        .to_vec()
}

/// Extent-reference value: a `nolx_phys_ext_val_t` describing one block, with
/// the reference count appended as a trailing `u64`.
fn encode_refcount(block: u64, count: u64) -> Vec<u8> {
    let val = gen::nolx_phys_ext_val_t {
        len_and_kind: pack_len_and_flags(4096, 1),
        phys_block_num: block,
        crypto_id: 0,
    };
    let mut bytes = val.as_bytes().to_vec();
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes
}

fn decode_refcount(val: &[u8]) -> Option<u64> {
    let tail =
        val.get(gen::nolx_phys_ext_val_t::DISK_SIZE..gen::nolx_phys_ext_val_t::DISK_SIZE + 8)?;
    Some(u64::from_le_bytes(tail.try_into().ok()?))
}

fn snap_metadata_val(record: &SnapshotRecord) -> Vec<u8> {
    let val = gen::nolx_snap_metadata_val_t {
        extentref_tree_oid: record.extentref_root,
        snap_xid: record.xid,
        num_files: record.num_files,
        num_dirs: record.num_dirs,
        num_symlinks: record.num_symlinks,
        num_other_fsobjects: record.num_other,
        create_time: record.create_time,
        change_time: record.create_time,
        inum: gen::SNAPSHOT_DIR_INO,
        extentref_tree_subtype: gen::SUB_EXTENT_REF_TREE,
        flags: 0,
        name_len: record.name.len() as u16,
        pad: 0,
    };
    let mut bytes = val.as_bytes().to_vec();
    bytes.extend_from_slice(record.name.as_bytes());
    bytes
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::InvalidField("invalid file name"));
    }
    if name.len() > gen::MAX_NAME_LEN as usize {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;

    fn formatted(blocks: u64) -> Volume<MemBlockDevice> {
        let dev = MemBlockDevice::new(4096, blocks);
        Volume::format(
            dev,
            &mkfs::FormatOptions {
                volume_name: String::from("test"),
                ..mkfs::FormatOptions::default()
            },
        )
        .unwrap()
    }

    fn remount(vol: Volume<MemBlockDevice>) -> Volume<MemBlockDevice> {
        Volume::mount(vol.into_device()).unwrap()
    }

    fn read_all(vol: &Volume<MemBlockDevice>, ino: u64) -> Vec<u8> {
        let inode = vol.read_inode(ino).unwrap();
        let size = usize::try_from(Volume::<MemBlockDevice>::file_size(&inode)).unwrap();
        let mut buf = vec![0u8; size];
        let n = vol.read_data(ino, 0, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    #[test]
    fn create_write_read_round_trip_survives_remount() {
        let mut vol = formatted(4096);
        let docs = vol.mkdir(gen::ROOT_INO, "docs", 0o755, 0, 0).unwrap();
        let file = vol.create_file(docs, "hello.txt", 0o644, 0, 0).unwrap();
        let data: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        vol.write_data(file, 0, &data).unwrap();
        vol.commit().unwrap();
        assert_eq!(read_all(&vol, file), data);

        let vol = remount(vol);
        let found = vol.lookup(gen::ROOT_INO, "docs").unwrap();
        let file2 = vol.lookup(found, "hello.txt").unwrap();
        assert_eq!(file2, file);
        assert_eq!(read_all(&vol, file2), data);
        let names: Vec<String> = vol
            .readdir(gen::ROOT_INO)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.iter().any(|n| n == "docs"));
    }

    #[test]
    fn crash_at_every_write_boundary_keeps_last_commit() {
        // Build a committed base image.
        let mut vol = formatted(2048);
        let file = vol
            .create_file(gen::ROOT_INO, "stable.txt", 0o644, 0, 0)
            .unwrap();
        vol.write_data(file, 0, b"committed").unwrap();
        vol.commit().unwrap();
        let base = vol.into_device();

        for boundary in 1..28u64 {
            let mut dev = base.clone();
            dev.fail_writes_from(Some(boundary));
            // Whatever happens to this transaction, remounting must succeed
            // and must still see the last committed state.
            if let Ok(mut vol2) = Volume::mount(dev.clone()) {
                let _ = vol2.write_data(file, 0, b"torn-write-attempt");
                let _ = vol2.commit();
            }
            dev.fail_writes_from(None);
            let vol3 = Volume::mount(dev)
                .unwrap_or_else(|e| panic!("boundary {boundary}: remount failed: {e}"));
            assert_eq!(read_all(&vol3, file), b"committed", "boundary {boundary}");
            drop(vol3);
        }
        let _ = base;
    }

    #[test]
    fn snapshot_is_isolated_from_later_writes() {
        let mut vol = formatted(2048);
        let file = vol
            .create_file(gen::ROOT_INO, "data.bin", 0o644, 0, 0)
            .unwrap();
        vol.write_data(file, 0, b"version-one").unwrap();
        let snap_xid = vol.create_snapshot("before").unwrap();
        assert!(snap_xid >= 2);
        vol.write_data(file, 0, b"version-two-longer").unwrap();
        vol.commit().unwrap();
        assert_eq!(read_all(&vol, file), b"version-two-longer");

        let dev = vol.into_device();
        let view = Volume::mount_snapshot(dev.clone(), "before").unwrap();
        let snap_file = view.lookup(gen::ROOT_INO, "data.bin").unwrap();
        assert_eq!(read_all(&view, snap_file), b"version-one");
        assert_eq!(view.snapshots().len(), 1);

        let mut live = Volume::mount(dev).unwrap();
        live.delete_snapshot("before").unwrap();
        assert!(live.snapshots().is_empty());
        let still = live.lookup(gen::ROOT_INO, "data.bin").unwrap();
        assert_eq!(read_all(&live, still), b"version-two-longer");
    }

    #[test]
    fn links_rename_unlink_and_rmdir_rules() {
        let mut vol = formatted(2048);
        let a = vol.mkdir(gen::ROOT_INO, "a", 0o755, 0, 0).unwrap();
        let b = vol.mkdir(gen::ROOT_INO, "b", 0o755, 0, 0).unwrap();
        let file = vol.create_file(a, "f.txt", 0o644, 0, 0).unwrap();
        vol.write_data(file, 0, b"payload").unwrap();
        vol.link(file, b, "g.txt").unwrap();
        assert_eq!(vol.read_inode(file).unwrap().nlink, 2);
        // Non-empty directory cannot be removed.
        assert!(vol.rmdir(gen::ROOT_INO, "a").is_err());
        // Rename moves the entry and updates parentage.
        vol.rename(a, "f.txt", b, "h.txt").unwrap();
        assert!(vol.lookup(a, "f.txt").is_err());
        assert_eq!(vol.lookup(b, "h.txt").unwrap(), file);
        vol.unlink(b, "g.txt").unwrap();
        assert_eq!(vol.read_inode(file).unwrap().nlink, 1);
        assert_eq!(read_all(&vol, file), b"payload");
        vol.commit().unwrap();
        let vol = remount(vol);
        assert_eq!(vol.lookup(b, "h.txt").unwrap(), file);
    }

    #[test]
    fn symlinks_xattrs_truncate_and_holes() {
        let mut vol = formatted(2048);
        let link = vol
            .symlink(gen::ROOT_INO, "lnk", b"/some/target", 0, 0)
            .unwrap();
        assert_eq!(vol.readlink(link).unwrap(), b"/some/target");
        let file = vol.create_file(gen::ROOT_INO, "f", 0o644, 0, 0).unwrap();
        vol.set_xattr(file, "user.note", b"hello").unwrap();
        assert_eq!(vol.get_xattr(file, "user.note").unwrap(), b"hello");
        assert_eq!(
            vol.list_xattrs(file).unwrap(),
            vec![String::from("user.note")]
        );
        // Sparse write: bytes before the offset read back as zeroes.
        vol.write_data(file, 8192, b"tail").unwrap();
        let all = read_all(&vol, file);
        assert_eq!(all.len(), 8196);
        assert!(all[..8192].iter().all(|&b| b == 0));
        assert_eq!(&all[8192..], b"tail");
        vol.truncate(file, 4).unwrap();
        assert_eq!(read_all(&vol, file).len(), 4);
        vol.truncate(file, 100).unwrap();
        assert_eq!(read_all(&vol, file).len(), 100);
        vol.remove_xattr(file, "user.note").unwrap();
        assert!(vol.get_xattr(file, "user.note").is_err());
        vol.commit().unwrap();
        let vol = remount(vol);
        assert_eq!(vol.readlink(link).unwrap(), b"/some/target");
    }

    #[test]
    fn fsck_reports_healthy_volume_clean_and_dump_reads_it() {
        let mut vol = formatted(2048);
        let dir = vol.mkdir(gen::ROOT_INO, "etc", 0o755, 0, 0).unwrap();
        let file = vol.create_file(dir, "motd", 0o644, 0, 0).unwrap();
        vol.write_data(file, 0, b"welcome\n").unwrap();
        vol.commit().unwrap();
        let mut dev = vol.into_device();
        let report = crate::fsck::check(&mut dev, false).unwrap();
        assert!(report.is_clean(), "issues: {:?}", report.issues);
        let text = crate::dump::dump_to_string(&mut dev).unwrap();
        assert!(text.contains("motd"), "dump:\n{text}");
        assert!(text.contains("checkpoint: xid"));
    }

    #[test]
    fn clone_file_shares_then_diverges() {
        let mut vol = formatted(2048);
        let src = vol.create_file(gen::ROOT_INO, "src", 0o644, 0, 0).unwrap();
        let data = vec![7u8; 5000];
        vol.write_data(src, 0, &data).unwrap();
        let copy = vol.clone_file(src, gen::ROOT_INO, "copy").unwrap();
        assert_eq!(read_all(&vol, copy), data);
        // Overwriting the copy must not disturb the source (COW).
        vol.write_data(copy, 0, &[9u8; 16]).unwrap();
        assert_eq!(read_all(&vol, src), data);
        let mut after = read_all(&vol, copy);
        after.truncate(16);
        assert_eq!(after, vec![9u8; 16]);
    }
}

#[cfg(test)]
mod m8_regression_tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;

    #[test]
    fn rename_to_root_after_symlink_on_small_volume() {
        let dev = MemBlockDevice::new(4096, 256);
        let mut vol = Volume::format(
            dev,
            &mkfs::FormatOptions {
                volume_name: String::from("small"),
                ..mkfs::FormatOptions::default()
            },
        )
        .unwrap();
        let docs = vol.mkdir(gen::ROOT_INO, "docs", 0o755, 0, 0).unwrap();
        let f = vol.create_file(docs, "a.txt", 0o644, 0, 0).unwrap();
        vol.write_data(f, 0, b"adapter-data").unwrap();
        assert_eq!(vol.lookup(docs, "a.txt").unwrap(), f);
        vol.link(f, gen::ROOT_INO, "b.txt").unwrap();
        assert_eq!(vol.lookup(gen::ROOT_INO, "b.txt").unwrap(), f);
        let lnk = vol
            .symlink(gen::ROOT_INO, "lnk", b"/docs/a.txt", 0, 0)
            .unwrap();
        assert_eq!(vol.readlink(lnk).unwrap(), b"/docs/a.txt");
        assert_eq!(vol.lookup(docs, "a.txt").unwrap(), f);
        vol.rename(docs, "a.txt", gen::ROOT_INO, "c.txt").unwrap();
        assert_eq!(vol.lookup(gen::ROOT_INO, "c.txt").unwrap(), f);
        vol.commit().unwrap();
    }

    #[test]
    fn link_and_commit_on_kernel_sized_volume() {
        // 96 blocks is what the kernel boot self-test mounts.
        let dev = MemBlockDevice::new(4096, 96);
        let mut vol = Volume::format(
            dev,
            &mkfs::FormatOptions {
                volume_name: String::from("nolx-boot"),
                ..mkfs::FormatOptions::default()
            },
        )
        .unwrap();
        let f = vol
            .create_file(gen::ROOT_INO, "boot.txt", 0o644, 0, 0)
            .unwrap();
        vol.write_data(f, 0, b"nolxfs-v1-ok").unwrap();
        vol.link(f, gen::ROOT_INO, "boot-link.txt").unwrap();
        assert_eq!(vol.read_inode(f).unwrap().nlink, 2);
        vol.commit().unwrap();
        let vol = Volume::mount(vol.into_device()).unwrap();
        let f2 = vol.lookup(gen::ROOT_INO, "boot-link.txt").unwrap();
        assert_eq!(f2, f);
    }
}
