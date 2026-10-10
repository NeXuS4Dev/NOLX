//! Formatting a NOLXFS container, and reading one back.
//!
//! `mkfs.nolxfs` and the kernel's mount path share this module, so the tool and the
//! filesystem cannot disagree about what a formatted volume looks like.
//!
//! # The bootstrap problem, and how it is solved
//!
//! An object map resolves virtual object ids to physical blocks -- but the object map is
//! itself an object, and its B-tree is made of blocks. Something has to be pinned by
//! address rather than by lookup. NOLXFS pins exactly two things:
//!
//! 1. `nolx_nx_superblock_t.omap_oid` is the **physical block** of the container's
//!    `nolx_omap_phys_t`. The container map cannot resolve itself through itself.
//! 2. `nolx_omap_phys_t.tree_oid` is the **physical block** of that map's B-tree root.
//!
//! Both are safe because a B-tree root block never moves: when the tree grows a level,
//! [`crate::btree::BTree`] writes the new root *at the old root's address* and moves the
//! old root to a fresh block. Everything else in the filesystem is virtual and resolved
//! through a map, including every volume superblock.
//!
//! # Layout
//!
//! ```text
//! block 0                 container superblock
//! block 1                 container superblock copy (reserved, zeroed)
//! [xp_desc_base ..)       checkpoint descriptor ring
//! [xp_data_base ..)       checkpoint data ring
//! container omap_phys     physical, pinned by nx_superblock_t.omap_oid
//!   container omap B-tree root, physical, pinned by omap_phys_t.tree_oid
//! spaceman                ephemeral, pinned by the checkpoint map
//! reaper                  ephemeral, pinned by the checkpoint map
//! volume superblock       virtual, resolved through the container omap
//!   volume omap_phys      virtual, resolved through the container omap
//!     volume omap B-tree root, physical, pinned by its omap_phys_t.tree_oid
//!   fs-tree B-tree root   virtual, resolved through the volume omap
//!   extent-ref B-tree     virtual, resolved through the volume omap
//!   snapshot metadata     virtual, resolved through the volume omap
//! [first_data_block ..)   file data and metadata
//! ```

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;

use crate::blockdev::BlockDevice;
use crate::btree::{BTree, BlockAllocator, KeyKind, Node};
use crate::omap::ObjectMap;
use crate::on_disk::{self, FsError, Result};
use crate::on_disk_gen as gen;
use crate::Pod;

/// Blocks reserved for the checkpoint descriptor ring.
pub const CHECKPOINT_DESC_BLOCKS: u32 = 32;
/// Blocks reserved for the checkpoint data ring.
pub const CHECKPOINT_DATA_BLOCKS: u32 = 32;

/// Fixed object ids assigned at format time.
pub mod oid {
    /// The container superblock. It is never resolved through a map -- it lives at block
    /// 0 by definition -- but it still needs an id for its object header.
    pub const CONTAINER_SUPERBLOCK: u64 = 0;
    /// The container's object map object.
    pub const CONTAINER_OMAP: u64 = 1;
    /// The space manager.
    pub const SPACEMAN: u64 = 2;
    /// The reaper.
    pub const REAPER: u64 = 3;
    /// The first volume's superblock.
    pub const VOLUME_SUPERBLOCK: u64 = 4;
    /// The first volume's object map object.
    pub const VOLUME_OMAP: u64 = 5;
    /// The first volume's fs-tree root.
    pub const FS_TREE: u64 = 6;
    /// The first volume's extent reference tree root.
    pub const EXTENT_REF_TREE: u64 = 7;
    /// The first volume's snapshot metadata tree root.
    pub const SNAP_META_TREE: u64 = 8;
    /// The first object id handed out after formatting.
    pub const NEXT: u64 = 16;
}

/// Options for [`format`].
#[derive(Clone, Debug)]
pub struct FormatOptions {
    /// Block size. Must satisfy [`on_disk::is_valid_block_size`].
    pub block_size: u32,
    /// Volume name, at most [`gen::MAX_VOLUME_NAME_LEN`] bytes of UTF-8.
    pub volume_name: String,
    /// Initial permission bits for the root directory, without the file type.
    pub root_mode: u16,
    /// Owning uid for the root directory.
    pub root_uid: u32,
    /// Owning gid for the root directory.
    pub root_gid: u32,
    /// Enable case-folding for names on this volume.
    pub casefold: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            block_size: gen::DEFAULT_BLOCK_SIZE as u32,
            volume_name: String::from("nolx"),
            root_mode: 0o755,
            root_uid: 0,
            root_gid: 0,
            casefold: false,
        }
    }
}

/// The block layout of a formatted container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Bytes per block.
    pub block_size: usize,
    /// Total blocks.
    pub block_count: u64,
    /// First block of the checkpoint descriptor ring.
    pub xp_desc_base: u64,
    /// Blocks in the checkpoint descriptor ring.
    pub xp_desc_blocks: u32,
    /// First block of the checkpoint data ring.
    pub xp_data_base: u64,
    /// Blocks in the checkpoint data ring.
    pub xp_data_blocks: u32,
    /// Physical block of the container's `nolx_omap_phys_t`.
    pub container_omap: u64,
    /// Physical block of the container omap's B-tree root.
    pub container_omap_tree: u64,
    /// Block holding the space manager.
    pub spaceman: u64,
    /// Block holding the reaper.
    pub reaper: u64,
    /// Block holding the volume superblock.
    pub volume_sb: u64,
    /// Block holding the volume's `nolx_omap_phys_t`.
    pub volume_omap: u64,
    /// Physical block of the volume omap's B-tree root.
    pub volume_omap_tree: u64,
    /// Block holding the fs-tree root.
    pub fs_tree_root: u64,
    /// Block holding the extent reference tree root.
    pub extentref_tree_root: u64,
    /// Block holding the snapshot metadata tree root.
    pub snap_meta_tree_root: u64,
    /// First block available for file data.
    pub first_data_block: u64,
}

/// Plan a layout without touching the device.
///
/// # Errors
///
/// [`FsError::InvalidField`] for a bad block size, [`FsError::BlockOutOfRange`] if the
/// device is too small to hold the metadata.
pub fn plan(block_size: u32, block_count: u64) -> Result<Layout> {
    if !on_disk::is_valid_block_size(block_size) {
        return Err(FsError::InvalidField("block_size"));
    }
    let xp_desc_base = 2;
    let xp_data_base = xp_desc_base + u64::from(CHECKPOINT_DESC_BLOCKS);
    let mut next = xp_data_base + u64::from(CHECKPOINT_DATA_BLOCKS);

    let take = |l: &mut u64| {
        let b = *l;
        *l += 1;
        b
    };
    let container_omap = take(&mut next);
    let container_omap_tree = take(&mut next);
    let spaceman = take(&mut next);
    let reaper = take(&mut next);
    let volume_sb = take(&mut next);
    let volume_omap = take(&mut next);
    let volume_omap_tree = take(&mut next);
    let fs_tree_root = take(&mut next);
    let extentref_tree_root = take(&mut next);
    let snap_meta_tree_root = take(&mut next);

    if next >= block_count {
        return Err(FsError::BlockOutOfRange(block_count));
    }

    Ok(Layout {
        block_size: block_size as usize,
        block_count,
        xp_desc_base,
        xp_desc_blocks: CHECKPOINT_DESC_BLOCKS,
        xp_data_base,
        xp_data_blocks: CHECKPOINT_DATA_BLOCKS,
        container_omap,
        container_omap_tree,
        spaceman,
        reaper,
        volume_sb,
        volume_omap,
        volume_omap_tree,
        fs_tree_root,
        extentref_tree_root,
        snap_meta_tree_root,
        first_data_block: next,
    })
}

/// A bump allocator over the data region, used while formatting.
///
/// The real allocator is the space manager; during `mkfs` there is nothing to reuse yet.
struct BumpAllocator {
    next: u64,
    end: u64,
}

impl BlockAllocator for BumpAllocator {
    fn alloc(&mut self) -> Result<u64> {
        if self.next >= self.end {
            return Err(FsError::InvalidField("out of space while formatting"));
        }
        let b = self.next;
        self.next += 1;
        Ok(b)
    }
}

/// An allocator that always fails, for read-only paths that must never allocate.
struct NoAlloc;

impl BlockAllocator for NoAlloc {
    fn alloc(&mut self) -> Result<u64> {
        Err(FsError::ReadOnly)
    }
}

/// Format a container with a single volume.
///
/// # Errors
///
/// [`FsError::InvalidField`] for bad options, [`FsError::BlockOutOfRange`] for a device
/// that is too small, or a device error.
pub fn format<D: BlockDevice>(dev: &mut D, opts: &FormatOptions) -> Result<Layout> {
    if !on_disk::is_valid_block_size(opts.block_size) {
        return Err(FsError::InvalidField("block_size"));
    }
    let name_bytes = opts.volume_name.as_bytes();
    if name_bytes.len() > gen::MAX_VOLUME_NAME_LEN as usize {
        return Err(FsError::InvalidField(
            "volume name exceeds MAX_VOLUME_NAME_LEN",
        ));
    }
    if name_bytes.contains(&0) {
        return Err(FsError::InvalidField("volume name contains a NUL"));
    }

    let block_count = dev.block_count();
    let layout = plan(opts.block_size, block_count)?;
    if dev.block_size() != layout.block_size {
        return Err(FsError::InvalidField(
            "the device's block size does not match the requested one",
        ));
    }

    // ---- empty B-tree roots ------------------------------------------------
    // Each root is a single empty leaf. They are written before anything references them
    // so a crash mid-format leaves objects that are individually valid.
    write_empty_tree(dev, layout.container_omap_tree, gen::SUB_OMAP as u16)?;
    write_empty_tree(dev, layout.volume_omap_tree, gen::SUB_OMAP as u16)?;
    write_empty_tree(dev, layout.fs_tree_root, gen::SUB_FSTREE as u16)?;
    write_empty_tree(
        dev,
        layout.extentref_tree_root,
        gen::SUB_EXTENT_REF_TREE as u16,
    )?;
    write_empty_tree(
        dev,
        layout.snap_meta_tree_root,
        gen::SUB_SNAP_META_TREE as u16,
    )?;

    // ---- the root inode, so the fs-tree is never empty ---------------------
    let mut inode = gen::nolx_inode_t::zeroed();
    inode.parent_id = gen::ROOT_INO;
    inode.mode = (nolx_types::scalars::S_IFDIR as u16) | opts.root_mode;
    inode.nlink = 2; // `.` and its parent's entry
    inode.owner = opts.root_uid;
    inode.group = opts.root_gid;
    let inode_bytes = inode.as_bytes();

    {
        let mut alloc = BumpAllocator {
            next: layout.first_data_block,
            end: block_count,
        };
        let mut fs_tree = BTree::new(
            dev,
            &mut alloc,
            layout.fs_tree_root,
            KeyKind::JKey,
            gen::SUB_FSTREE as u16,
            1,
        );
        let key = on_disk::pack_jkey(gen::DREC_TYPE_INODE, gen::ROOT_INO).to_le_bytes();
        fs_tree.insert(&key, inode_bytes)?;
    }

    // ---- volume object map: fs-tree root and friends -----------------------
    {
        let mut alloc = BumpAllocator {
            next: layout.first_data_block,
            end: block_count,
        };
        let mut vomap = ObjectMap::open(dev, &mut alloc, layout.volume_omap_tree, 1);
        vomap.set(oid::FS_TREE, 1, layout.fs_tree_root, 0)?;
        vomap.set(oid::EXTENT_REF_TREE, 1, layout.extentref_tree_root, 0)?;
        vomap.set(oid::SNAP_META_TREE, 1, layout.snap_meta_tree_root, 0)?;
    }

    // ---- container object map: the volume's virtual objects ----------------
    {
        let mut alloc = BumpAllocator {
            next: layout.first_data_block,
            end: block_count,
        };
        let mut comap = ObjectMap::open(dev, &mut alloc, layout.container_omap_tree, 1);
        comap.set(oid::VOLUME_SUPERBLOCK, 1, layout.volume_sb, 0)?;
        comap.set(oid::VOLUME_OMAP, 1, layout.volume_omap, 0)?;
    }

    // ---- the volume's omap_phys_t ------------------------------------------
    write_omap_phys(
        dev,
        layout.volume_omap,
        oid::VOLUME_OMAP,
        layout.volume_omap_tree,
    )?;

    // ---- the container's omap_phys_t ---------------------------------------
    write_omap_phys(
        dev,
        layout.container_omap,
        oid::CONTAINER_OMAP,
        layout.container_omap_tree,
    )?;

    // ---- space manager and reaper ------------------------------------------
    write_spaceman(dev, layout.spaceman, &layout)?;
    write_reaper(dev, layout.reaper)?;

    // ---- volume superblock --------------------------------------------------
    let mut vsb = gen::nolx_fs_superblock_t::zeroed();
    vsb.magic = gen::MAGIC_VOLUME_SUPERBLOCK;
    vsb.block_size = opts.block_size;
    vsb.features_required = u64::from(gen::NX_FEATURE_REQ_V1_BASE);
    vsb.features_supported = if opts.casefold {
        u64::from(gen::NX_FEATURE_SUP_CASEFOLD)
    } else {
        0
    };
    vsb.omap_oid = oid::VOLUME_OMAP;
    vsb.root_tree_oid = oid::FS_TREE;
    vsb.extentref_tree_oid = oid::EXTENT_REF_TREE;
    vsb.snap_meta_tree_oid = oid::SNAP_META_TREE;
    vsb.root_tree_type = gen::OBJ_BTREE_ROOT;
    vsb.root_tree_subtype = gen::SUB_FSTREE;
    vsb.extentref_tree_type = gen::OBJ_BTREE_ROOT;
    vsb.extentref_tree_subtype = gen::SUB_EXTENT_REF_TREE;
    vsb.snap_meta_tree_type = gen::OBJ_BTREE_ROOT;
    vsb.snap_meta_tree_subtype = gen::SUB_SNAP_META_TREE;
    vsb.num_directories = 1;
    vsb.volume_name[..name_bytes.len()].copy_from_slice(name_bytes);
    write_sealed(
        dev,
        layout.volume_sb,
        &vsb,
        oid::VOLUME_SUPERBLOCK,
        gen::OBJ_VOLUME_SUPERBLOCK,
    )?;

    // ---- container superblock, written last as the commit point ------------
    let mut csb = gen::nolx_nx_superblock_t::zeroed();
    csb.magic = gen::MAGIC_CONTAINER_SUPERBLOCK;
    csb.block_size = opts.block_size;
    csb.block_count = block_count;
    csb.features_required = u64::from(gen::NX_FEATURE_REQ_V1_BASE);
    csb.features_supported = vsb.features_supported;
    csb.next_oid = oid::NEXT;
    csb.next_xid = 2;
    csb.xp_desc_base = layout.xp_desc_base;
    csb.xp_desc_blocks = layout.xp_desc_blocks;
    csb.xp_data_base = layout.xp_data_base;
    csb.xp_data_blocks = layout.xp_data_blocks;
    csb.xp_desc_len = 1;
    csb.xp_data_len = 1;
    csb.fs_oid_count = 1;
    csb.fs_oid[0] = oid::VOLUME_SUPERBLOCK;
    csb.omap_oid = layout.container_omap;
    csb.reaper_oid = oid::REAPER;
    write_sealed(
        dev,
        0,
        &csb,
        oid::CONTAINER_SUPERBLOCK,
        gen::OBJ_CONTAINER_SUPERBLOCK,
    )?;

    dev.flush()?;
    Ok(layout)
}

fn write_empty_tree<D: BlockDevice>(dev: &mut D, block: u64, subtype: u16) -> Result<()> {
    let node = Node::new_leaf();
    let mut buf = vec![0u8; dev.block_size()];
    node.encode(&mut buf, 0, 1, gen::OBJ_BTREE_ROOT, subtype)?;
    dev.write_block(block, &buf)?;
    Ok(())
}

fn write_omap_phys<D: BlockDevice>(
    dev: &mut D,
    block: u64,
    oid: u64,
    tree_root_block: u64,
) -> Result<()> {
    let mut omap = gen::nolx_omap_phys_t::zeroed();
    omap.magic = gen::MAGIC_OMAP;
    omap.tree_type = gen::OBJ_BTREE_ROOT;
    omap.tree_subtype = gen::SUB_OMAP;
    // See the module docs: this is a physical block address, not an oid.
    omap.tree_oid = tree_root_block;
    write_sealed(dev, block, &omap, oid, gen::OBJ_OMAP)
}

fn write_spaceman<D: BlockDevice>(dev: &mut D, block: u64, layout: &Layout) -> Result<()> {
    let mut sp = gen::nolx_spaceman_phys_t::zeroed();
    sp.magic = gen::MAGIC_SPACEMAN;
    sp.block_size = layout.block_size as u32;
    sp.blocks_per_chunk = 1024;
    sp.chunks_per_cib = 1024;
    sp.free_count = layout.block_count - layout.first_data_block;
    write_sealed(dev, block, &sp, oid::SPACEMAN, gen::OBJ_SPACEMAN)
}

fn write_reaper<D: BlockDevice>(dev: &mut D, block: u64) -> Result<()> {
    let reaper = gen::nolx_reaper_phys_t::zeroed();
    write_sealed(dev, block, &reaper, oid::REAPER, gen::OBJ_REAPER)
}

fn write_sealed<D: BlockDevice, T: Pod>(
    dev: &mut D,
    block: u64,
    value: &T,
    oid: u64,
    obj_type: u32,
) -> Result<()> {
    let mut buf = vec![0u8; dev.block_size()];
    buf[..core::mem::size_of::<T>()].copy_from_slice(value.as_bytes());
    on_disk::seal(&mut buf, on_disk::ObjHeader::make(oid, 1, obj_type, 0))?;
    dev.write_block(block, &buf)?;
    Ok(())
}

// --------------------------------------------------------------------- reading back

/// What a mount found. Enough to start answering requests, without holding the device.
#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// The container's layout.
    pub layout: Layout,
    /// The volume name.
    pub name: String,
    /// The volume superblock, as read.
    pub superblock: gen::nolx_fs_superblock_t,
}

/// Walk the bootstrap chain and read the first volume.
///
/// # Errors
///
/// Whatever the first invalid thing found is. A caller should report it and refuse to
/// mount; repairing is `fsck.nolxfs`'s job, not the mount path's.
pub fn probe<D: BlockDevice>(dev: &mut D) -> Result<VolumeInfo> {
    let csb: gen::nolx_nx_superblock_t = dev.read_object(0)?;
    on_disk::validate_container(&csb)?;

    let layout = plan(csb.block_size, csb.block_count)?;
    if layout.xp_desc_base != csb.xp_desc_base || layout.xp_data_base != csb.xp_data_base {
        return Err(FsError::InvalidField(
            "the checkpoint rings are not where mkfs put them",
        ));
    }
    if csb.fs_oid_count == 0 {
        return Err(FsError::InvalidField("the container holds no volumes"));
    }

    // The container omap's physical block is pinned by the superblock.
    let comap_phys: gen::nolx_omap_phys_t = dev.read_object(csb.omap_oid)?;
    if comap_phys.magic != gen::MAGIC_OMAP {
        return Err(FsError::BadMagic {
            found: comap_phys.magic,
            expected: gen::MAGIC_OMAP,
        });
    }

    let volume_sb_block = {
        let mut alloc = NoAlloc;
        let mut comap = ObjectMap::open(dev, &mut alloc, comap_phys.tree_oid, csb.next_xid);
        comap
            .resolve(csb.fs_oid[0], csb.next_xid)?
            .ok_or(FsError::InvalidField(
                "the container omap cannot find the volume",
            ))?
    };

    let vsb: gen::nolx_fs_superblock_t = dev.read_object(volume_sb_block)?;
    on_disk::validate_volume(&vsb)?;

    let volume_omap_block = {
        let mut alloc = NoAlloc;
        let mut comap = ObjectMap::open(dev, &mut alloc, comap_phys.tree_oid, csb.next_xid);
        comap
            .resolve(vsb.omap_oid, csb.next_xid)?
            .ok_or(FsError::InvalidField(
                "the container omap cannot find the volume's map",
            ))?
    };
    let vomap_phys: gen::nolx_omap_phys_t = dev.read_object(volume_omap_block)?;
    if vomap_phys.magic != gen::MAGIC_OMAP {
        return Err(FsError::BadMagic {
            found: vomap_phys.magic,
            expected: gen::MAGIC_OMAP,
        });
    }

    let name_len = vsb
        .volume_name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(vsb.volume_name.len());
    let name = core::str::from_utf8(&vsb.volume_name[..name_len])
        .map_err(|_| FsError::InvalidField("volume name is not valid UTF-8"))?
        .to_string();

    Ok(VolumeInfo {
        layout,
        name,
        superblock: vsb,
    })
}

/// Read the volume's root inode.
///
/// # Errors
///
/// Device or corruption errors, or [`FsError::InvalidField`] if the volume has no root
/// inode, which means it was never formatted or is badly damaged.
pub fn read_root_inode<D: BlockDevice>(
    dev: &mut D,
    info: &VolumeInfo,
) -> Result<gen::nolx_inode_t> {
    let vomap_block = {
        let mut alloc = NoAlloc;
        let csb: gen::nolx_nx_superblock_t = dev.read_object(0)?;
        let comap_phys: gen::nolx_omap_phys_t = dev.read_object(csb.omap_oid)?;
        let mut comap = ObjectMap::open(dev, &mut alloc, comap_phys.tree_oid, csb.next_xid);
        comap
            .resolve(info.superblock.omap_oid, csb.next_xid)?
            .ok_or(FsError::InvalidField("cannot find the volume object map"))?
    };
    let vomap_phys: gen::nolx_omap_phys_t = dev.read_object(vomap_block)?;

    let fs_tree_block = {
        let mut alloc = NoAlloc;
        let mut vomap = ObjectMap::open(dev, &mut alloc, vomap_phys.tree_oid, u64::MAX);
        vomap
            .resolve(info.superblock.root_tree_oid, u64::MAX)?
            .ok_or(FsError::InvalidField("cannot find the fs-tree root"))?
    };

    let mut alloc = NoAlloc;
    let mut fs_tree = BTree::new(
        dev,
        &mut alloc,
        fs_tree_block,
        KeyKind::JKey,
        gen::SUB_FSTREE as u16,
        1,
    );
    let key = on_disk::pack_jkey(gen::DREC_TYPE_INODE, gen::ROOT_INO).to_le_bytes();
    let bytes = fs_tree
        .search(&key)?
        .ok_or(FsError::InvalidField("the volume has no root inode"))?;
    gen::nolx_inode_t::read_from(&bytes).ok_or(FsError::TooShort {
        have: bytes.len(),
        need: gen::nolx_inode_t::DISK_SIZE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;

    fn device() -> MemBlockDevice {
        MemBlockDevice::new(4096, 4096)
    }

    #[test]
    fn plan_lays_out_metadata_before_data() {
        let l = plan(4096, 1_000_000).unwrap();
        assert_eq!(l.xp_desc_base, 2);
        assert_eq!(l.xp_data_base, 2 + CHECKPOINT_DESC_BLOCKS as u64);
        assert!(l.first_data_block > l.snap_meta_tree_root);
        assert!(l.first_data_block > l.xp_data_base + u64::from(CHECKPOINT_DATA_BLOCKS));
        // Nothing overlaps.
        let blocks = [
            0,
            1,
            l.container_omap,
            l.container_omap_tree,
            l.spaceman,
            l.reaper,
            l.volume_sb,
            l.volume_omap,
            l.volume_omap_tree,
            l.fs_tree_root,
            l.extentref_tree_root,
            l.snap_meta_tree_root,
        ];
        // A fixed array has no `dedup`, and the overlap check is the point of the test, so
        // this moves to a Vec rather than being rewritten as a pairwise comparison.
        let mut sorted = blocks.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "metadata blocks overlap");
    }

    #[test]
    fn plan_rejects_a_bad_block_size() {
        assert!(matches!(
            plan(4000, 1000),
            Err(FsError::InvalidField("block_size"))
        ));
        assert!(matches!(
            plan(512, 1000),
            Err(FsError::InvalidField("block_size"))
        ));
    }

    #[test]
    fn plan_rejects_a_device_too_small_for_the_metadata() {
        assert!(matches!(plan(4096, 16), Err(FsError::BlockOutOfRange(_))));
    }

    #[test]
    fn plan_is_deterministic() {
        assert_eq!(plan(4096, 10_000).unwrap(), plan(4096, 10_000).unwrap());
    }

    #[test]
    fn format_then_probe_round_trips() {
        let mut dev = device();
        let opts = FormatOptions {
            volume_name: String::from("testdata"),
            ..Default::default()
        };
        let layout = format(&mut dev, &opts).unwrap();

        let info = probe(&mut dev).unwrap();
        assert_eq!(info.name, "testdata");
        assert_eq!(info.layout, layout);
        assert_eq!(info.superblock.block_size, 4096);
        assert_eq!(info.superblock.root_tree_oid, oid::FS_TREE);
    }

    #[test]
    fn format_creates_a_root_directory_with_the_requested_permissions() {
        let mut dev = device();
        let opts = FormatOptions {
            root_mode: 0o750,
            root_uid: 1000,
            root_gid: 1000,
            ..Default::default()
        };
        format(&mut dev, &opts).unwrap();
        let info = probe(&mut dev).unwrap();
        let root = read_root_inode(&mut dev, &info).unwrap();

        assert!(nolx_types::scalars::is_dir(u32::from(root.mode)));
        assert_eq!(u32::from(root.mode) & 0o7777, 0o750);
        assert_eq!(root.owner, 1000);
        assert_eq!(root.group, 1000);
        assert_eq!(root.nlink, 2);
    }

    #[test]
    fn format_refuses_an_oversized_volume_name() {
        let mut dev = device();
        let opts = FormatOptions {
            volume_name: "n".repeat(gen::MAX_VOLUME_NAME_LEN as usize + 1),
            ..Default::default()
        };
        assert!(matches!(
            format(&mut dev, &opts),
            Err(FsError::InvalidField(_))
        ));
    }

    #[test]
    fn format_refuses_a_nul_in_the_volume_name() {
        let mut dev = device();
        let opts = FormatOptions {
            volume_name: String::from("a\0b"),
            ..Default::default()
        };
        assert!(matches!(
            format(&mut dev, &opts),
            Err(FsError::InvalidField(_))
        ));
    }

    #[test]
    fn format_refuses_a_device_whose_block_size_disagrees() {
        let mut dev = MemBlockDevice::new(512, 8192);
        let opts = FormatOptions {
            block_size: 4096,
            ..Default::default()
        };
        assert!(matches!(
            format(&mut dev, &opts),
            Err(FsError::InvalidField(_))
        ));
    }

    #[test]
    fn probe_rejects_an_unformatted_device() {
        let mut dev = device();
        let err = probe(&mut dev).unwrap_err();
        assert!(matches!(err, FsError::BadMagic { .. }), "got {err:?}");
    }

    #[test]
    fn probe_rejects_a_corrupted_container_superblock() {
        let mut dev = device();
        format(&mut dev, &FormatOptions::default()).unwrap();

        let mut raw = vec![0u8; 4096];
        dev.read_block(0, &mut raw).unwrap();
        // Flip a bit in the feature flags, leaving the checksum intact: this is a valid
        // object whose contents are nonsense, which is exactly what a mount must reject.
        let offset = gen::nolx_nx_superblock_t::OFF_FEATURES_REQUIRED;
        raw[offset] ^= 0x01;
        // Reseal so the checksum passes and the semantic validation is what fires.
        raw[0..8].copy_from_slice(&0u64.to_le_bytes());
        let sum = crate::fletcher::compute(&raw).unwrap();
        raw[0..8].copy_from_slice(&sum.to_le_bytes());
        dev.write_block(0, &raw).unwrap();

        let err = probe(&mut dev).unwrap_err();
        assert!(
            matches!(
                err,
                FsError::InvalidField(_) | FsError::UnsupportedFeature(_)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn probe_rejects_a_torn_container_superblock() {
        let mut dev = device();
        format(&mut dev, &FormatOptions::default()).unwrap();

        let mut raw = vec![0u8; 4096];
        dev.read_block(0, &mut raw).unwrap();
        raw[64] ^= 0xFF; // corrupt the body, leave the stored checksum alone
        dev.write_block(0, &raw).unwrap();

        assert!(matches!(probe(&mut dev), Err(FsError::BadChecksum { .. })));
    }

    #[test]
    fn every_metadata_object_written_by_mkfs_passes_its_checksum() {
        let mut dev = device();
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();

        let checks = [
            (0u64, gen::OBJ_CONTAINER_SUPERBLOCK),
            (layout.container_omap, gen::OBJ_OMAP),
            (layout.container_omap_tree, gen::OBJ_BTREE_ROOT),
            (layout.spaceman, gen::OBJ_SPACEMAN),
            (layout.reaper, gen::OBJ_REAPER),
            (layout.volume_sb, gen::OBJ_VOLUME_SUPERBLOCK),
            (layout.volume_omap, gen::OBJ_OMAP),
            (layout.volume_omap_tree, gen::OBJ_BTREE_ROOT),
            (layout.fs_tree_root, gen::OBJ_BTREE_ROOT),
            (layout.extentref_tree_root, gen::OBJ_BTREE_ROOT),
            (layout.snap_meta_tree_root, gen::OBJ_BTREE_ROOT),
        ];
        let mut raw = vec![0u8; 4096];
        for (block, expected) in checks {
            dev.read_block(block, &mut raw).unwrap();
            let obj = on_disk::ObjRef::parse(&raw, expected)
                .unwrap_or_else(|e| panic!("block {block}: {e}"));
            assert_eq!(
                obj.header().obj_type(),
                expected,
                "block {block} has the wrong type"
            );
        }
    }

    #[test]
    fn the_checkpoint_rings_are_left_clear_for_the_first_transaction() {
        let mut dev = device();
        let layout = format(&mut dev, &FormatOptions::default()).unwrap();
        let mut raw = vec![0u8; 4096];
        for b in layout.xp_desc_base..layout.xp_desc_base + u64::from(layout.xp_desc_blocks) {
            dev.read_block(b, &mut raw).unwrap();
            assert!(
                raw.iter().all(|&x| x == 0),
                "descriptor block {b} is not clear"
            );
        }
    }

    #[test]
    fn formatting_survives_a_failure_at_every_write_boundary() {
        // A power cut at any point must leave either a blank device or one whose block 0
        // is not a valid container superblock. What must never happen is a device that
        // looks formatted but reads back corrupt.
        let reference = {
            let mut dev = device();
            format(&mut dev, &FormatOptions::default()).unwrap();
            dev.contents().to_vec()
        };

        let total_writes = {
            let mut dev = device();
            format(&mut dev, &FormatOptions::default()).unwrap();
            dev.write_count()
        };

        for cut in 0..total_writes {
            let mut dev = device();
            dev.fail_writes_from(Some(cut));
            // Formatting may fail; that is fine. What matters is the state it leaves.
            let _ = format(&mut dev, &FormatOptions::default());
            dev.fail_writes_from(None);

            let block0 = &dev.contents()[0..4096];
            let looks_formatted =
                u32::from_le_bytes([block0[32], block0[33], block0[34], block0[35]])
                    == gen::MAGIC_CONTAINER_SUPERBLOCK;
            if looks_formatted {
                // The commit point landed, so everything before it must be intact.
                assert_eq!(
                    &dev.contents()[..reference.len()],
                    &reference[..],
                    "a cut at write {cut} produced a device that looks formatted but differs"
                );
            }
        }
    }
}
