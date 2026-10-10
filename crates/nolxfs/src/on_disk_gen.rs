// GENERATED FILE -- DO NOT EDIT.
// Source of truth: tools/nolxfs-codegen/nolxfs_format.py
// Regenerate with: make fscheck
//

#![allow(clippy::identity_op, clippy::erasing_op)]
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]
#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use crate::Pod;

/// NOLXFS on-disk format version.
pub const FORMAT_VERSION: u32 = 1;

// ------------------------------------------------------------------ magics
/// NOLX container superblock: 'NCSB'
pub const MAGIC_CONTAINER_SUPERBLOCK: u32 = 0x4253434E;
/// NOLX volume superblock: 'NVSB'
pub const MAGIC_VOLUME_SUPERBLOCK: u32 = 0x4253564E;
/// object map: 'NOMP'
pub const MAGIC_OMAP: u32 = 0x504D4F4E;
/// checkpoint map: 'NCKP'
pub const MAGIC_CHECKPOINT_MAP: u32 = 0x504B434E;
/// space manager: 'NSPM'
pub const MAGIC_SPACEMAN: u32 = 0x4D50534E;

// ------------------------------------------------------------------ object types (nolx_obj_phys_t.type)
pub const OBJ_CONTAINER_SUPERBLOCK: u32 = 0x1;
pub const OBJ_BTREE_ROOT: u32 = 0x2;
pub const OBJ_BTREE_NODE: u32 = 0x3;
pub const OBJ_SPACEMAN: u32 = 0x4;
pub const OBJ_SPACEMAN_CAB: u32 = 0x5;
pub const OBJ_SPACEMAN_CIB: u32 = 0x6;
pub const OBJ_SPACEMAN_BITMAP: u32 = 0x7;
pub const OBJ_OMAP: u32 = 0x8;
pub const OBJ_CHECKPOINT_MAP: u32 = 0x9;
pub const OBJ_VOLUME_SUPERBLOCK: u32 = 0xA;
pub const OBJ_SNAP_METADATA: u32 = 0xB;
pub const OBJ_REAPER: u32 = 0xC;
pub const OBJ_REAP_LIST: u32 = 0xD;
pub const OBJ_EFI_JUMPSTART: u32 = 0xE;
pub const OBJ_GBITMAP: u32 = 0xF;
pub const OBJ_GBITMAP_BLOCK: u32 = 0x10;
pub const OBJ_ER_STATE: u32 = 0x11;

// ------------------------------------------------------------------ object subtypes (nolx_obj_phys_t.subtype)
pub const SUB_NONE: u32 = 0x0;
pub const SUB_OMAP: u32 = 0x1;
pub const SUB_FSTREE: u32 = 0x2;
pub const SUB_EXTENT_REF_TREE: u32 = 0x3;
pub const SUB_SNAP_META_TREE: u32 = 0x4;
pub const SUB_FEXT_TREE: u32 = 0x5;

// ------------------------------------------------------------------ fs-tree record types (high 4 bits of a j-key)
pub const DREC_TYPE_SNAP_METADATA: u32 = 0x1;
pub const DREC_TYPE_PHYS_EXTENT: u32 = 0x2;
pub const DREC_TYPE_INODE: u32 = 0x3;
pub const DREC_TYPE_XATTR: u32 = 0x4;
pub const DREC_TYPE_SIBLING_LINK: u32 = 0x5;
pub const DREC_TYPE_DSTREAM_ID: u32 = 0x6;
pub const DREC_TYPE_CRYPTO_STATE: u32 = 0x7;
pub const DREC_TYPE_FILE_EXTENT: u32 = 0x8;
pub const DREC_TYPE_DIR_REC: u32 = 0x9;
pub const DREC_TYPE_DIR_STATS: u32 = 0xA;
pub const DREC_TYPE_SIBLING_MAP: u32 = 0xB;
pub const DREC_TYPE_FILE_INFO: u32 = 0xC;

// ------------------------------------------------------------------ object header flags
pub const OBJ_FLAG_NO_CKSUM: u32 = 0x1; // object body is not covered by the checksum (unused in v1)
pub const OBJ_FLAG_ENCRYPTED: u32 = 0x2; // object body is encrypted at rest
pub const OBJ_FLAG_COMPRESSED: u32 = 0x4; // object body is compressed
pub const OBJ_FLAG_EPHEMERAL: u32 = 0x8; // object changes during mount; pinned by the checkpoint map

// ------------------------------------------------------------------ object map value flags
pub const OMAP_VAL_FLAG_SAVED: u32 = 0x1; // reachable from a snapshot; do not overwrite
pub const OMAP_VAL_FLAG_MISSING: u32 = 0x2; // known absent; a lookup must fail rather than fall back
pub const OMAP_VAL_FLAG_NO_SNAPSHOT: u32 = 0x4; // never visible in a point-in-time view

// ------------------------------------------------------------------ B-tree node flags
pub const BTNODE_ROOT: u32 = 0x1; // this node is the root of its tree
pub const BTNODE_LEAF: u32 = 0x2; // this node holds values rather than child pointers
pub const BTNODE_FIXED_KV_SIZE: u32 = 0x4; // all keys and all values have one fixed size
pub const BTNODE_NO_FREE: u32 = 0x8; // the node free lists are empty; do not scan them
pub const BTNODE_INDEX: u32 = 0x10; // middle node in an index tree

// ------------------------------------------------------------------ container feature bits: required
pub const NX_FEATURE_REQ_V1_BASE: u32 = 0x1; // the version 1 base format

// ------------------------------------------------------------------ container feature bits: read-only compatible
pub const NX_FEATURE_RO_COMPAT_SEALED_VOLUME: u32 = 0x1; // verified read-only system volume

// ------------------------------------------------------------------ container feature bits: supported
pub const NX_FEATURE_SUP_CASEFOLD: u32 = 0x1; // case-insensitive, case-preserving names
pub const NX_FEATURE_SUP_NORMALIZATION_INSENSITIVE: u32 = 0x2; // names compared without Unicode normalization
pub const NX_FEATURE_SUP_ENCRYPTION: u32 = 0x4; // per-file encryption with a volume keybag
pub const NX_FEATURE_SUP_FEXT: u32 = 0x8; // fast-dev extent tree present
pub const NX_FEATURE_SUP_SNAPSHOTS: u32 = 0x10; // snapshot metadata tree present
pub const NX_FEATURE_SUP_INTEGRITY_META: u32 = 0x20; // Merkle integrity metadata present

// ------------------------------------------------------------------ inode internal flags
pub const INODE_FLAG_SUID_SAVED: u32 = 0x1; // setuid bit was cleared on write by a non-owner
pub const INODE_FLAG_SGID_SAVED: u32 = 0x2; // setgid bit was cleared on write by a non-owner
pub const INODE_FLAG_CLONED: u32 = 0x4; // file has one or more clones
pub const INODE_FLAG_SPARSE: u32 = 0x8; // data stream contains holes
pub const INODE_FLAG_COMPRESSED: u32 = 0x10; // data stream is compressed
pub const INODE_FLAG_HIDDEN: u32 = 0x20; // hidden from directory listings
pub const INODE_FLAG_FAST_DOC_ID: u32 = 0x40; // document id is allocated

// ------------------------------------------------------------------ limits
/// fixed size of fs_oid[] in the container superblock
pub const MAX_VOLUMES_PER_CONTAINER: u64 = 8;
/// POSIX NAME_MAX
pub const MAX_NAME_LEN: u64 = 255;
/// POSIX PATH_MAX
pub const MAX_PATH_LEN: u64 = 1024;
/// longest volume name
pub const MAX_VOLUME_NAME_LEN: u64 = 255;
/// symlink resolution depth, POSIX minimum is 8
pub const SYMLOOP_MAX: u64 = 40;
/// extent lengths are 56-bit byte counts
pub const MAX_EXTENT_LEN: u64 = 72057594037927935;
/// block size chosen by mkfs unless told otherwise
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;
/// smallest permitted block size
pub const MIN_BLOCK_SIZE: u64 = 1024;
/// largest permitted block size
pub const MAX_BLOCK_SIZE: u64 = 16384;
/// descriptors in the checkpoint ring
pub const MAX_CHECKPOINT_DESCRIPTORS: u64 = 128;
/// volume root directory
pub const ROOT_INO: u64 = 1;
/// private directory / orphan holding pen
pub const PRIVATE_DIR_INO: u64 = 2;
/// read-only view of the volume's snapshots
pub const SNAPSHOT_DIR_INO: u64 = 3;
/// first inode number available to users
pub const FIRST_USER_INO: u64 = 1024;
/// inode numbers occupy the low 60 bits of a j-key
pub const OBJ_ID_BITS: u64 = 60;
/// split a node above this fill
pub const BTREE_SPLIT_FILL_PCT: u64 = 90;
/// merge a node below this fill
pub const BTREE_MERGE_FILL_PCT: u64 = 50;

// ------------------------------------------------------------------ structures
/// Header of every on-disk object. 32 bytes, no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_obj_phys_t {
    /// Fletcher-64 over the object with this field = 0
    pub checksum: u64,
    /// object id, resolved through an object map
    pub oid: u64,
    /// transaction id of the version that created this
    pub xid: u64,
    /// NOLX_OBJ_*
    pub r#type: u32,
    /// NOLX_SUB_*
    pub subtype: u16,
    /// NOLX_OBJ_FLAG_*
    pub flags: u16,
}

impl nolx_obj_phys_t {
    /// Size of `nolx_obj_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 32;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_CHECKSUM: usize = 0;
    pub const OFF_OID: usize = 8;
    pub const OFF_XID: usize = 16;
    pub const OFF_TYPE: usize = 24;
    pub const OFF_SUBTYPE: usize = 28;
    pub const OFF_FLAGS: usize = 30;
}
unsafe impl Pod for nolx_obj_phys_t {}
pub const OBJ_PHYS_DISK_SIZE: usize = 32;

/// Container superblock (magic NCSB). One block.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_nx_superblock_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// 'NCSB'
    pub magic: u32,
    pub block_size: u32,
    pub block_count: u64,
    pub features_supported: u64,
    pub features_required: u64,
    pub features_ro_compat: u64,
    pub uuid: [u8; 16],
    pub next_oid: u64,
    pub next_xid: u64,
    /// blocks reserved for checkpoint descriptors
    pub xp_desc_blocks: u32,
    /// blocks reserved for checkpoint data
    pub xp_data_blocks: u32,
    pub xp_desc_base: u64,
    pub xp_data_base: u64,
    /// ring index of the newest valid descriptor
    pub xp_desc_index: u32,
    pub xp_desc_len: u32,
    pub xp_desc_next: u32,
    pub xp_data_index: u32,
    pub xp_data_len: u32,
    pub xp_data_next: u32,
    pub fs_oid_count: u32,
    pub pad0: u32,
    /// object ids of the volumes in this container
    pub fs_oid: [u64; 8],
    /// NOLX_COUNTER_* statistics
    pub counters: [u64; 32],
    /// physical object id of the container object map
    pub omap_oid: u64,
    /// ephemeral reaper object
    pub reaper_oid: u64,
    pub flags: u64,
    pub efi_jumpstart_oid: u64,
    pub fusion_uuid: [u8; 16],
    /// first block of a bad region
    pub blocked_out_start: u64,
    pub blocked_out_count: u64,
    pub keybag_uuid: [u8; 16],
    pub keybag_oid: u64,
    /// paddr/len of the ephemeral info block
    pub ephemeral_info: [u64; 4],
    /// object used by fsck for write testing
    pub test_oid: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 3488],
}

impl nolx_nx_superblock_t {
    /// Size of `nolx_nx_superblock_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_MAGIC: usize = 32;
    pub const OFF_BLOCK_SIZE: usize = 36;
    pub const OFF_BLOCK_COUNT: usize = 40;
    pub const OFF_FEATURES_SUPPORTED: usize = 48;
    pub const OFF_FEATURES_REQUIRED: usize = 56;
    pub const OFF_FEATURES_RO_COMPAT: usize = 64;
    pub const OFF_UUID: usize = 72;
    pub const OFF_NEXT_OID: usize = 88;
    pub const OFF_NEXT_XID: usize = 96;
    pub const OFF_XP_DESC_BLOCKS: usize = 104;
    pub const OFF_XP_DATA_BLOCKS: usize = 108;
    pub const OFF_XP_DESC_BASE: usize = 112;
    pub const OFF_XP_DATA_BASE: usize = 120;
    pub const OFF_XP_DESC_INDEX: usize = 128;
    pub const OFF_XP_DESC_LEN: usize = 132;
    pub const OFF_XP_DESC_NEXT: usize = 136;
    pub const OFF_XP_DATA_INDEX: usize = 140;
    pub const OFF_XP_DATA_LEN: usize = 144;
    pub const OFF_XP_DATA_NEXT: usize = 148;
    pub const OFF_FS_OID_COUNT: usize = 152;
    pub const OFF_PAD0: usize = 156;
    pub const OFF_FS_OID: usize = 160;
    pub const OFF_COUNTERS: usize = 224;
    pub const OFF_OMAP_OID: usize = 480;
    pub const OFF_REAPER_OID: usize = 488;
    pub const OFF_FLAGS: usize = 496;
    pub const OFF_EFI_JUMPSTART_OID: usize = 504;
    pub const OFF_FUSION_UUID: usize = 512;
    pub const OFF_BLOCKED_OUT_START: usize = 528;
    pub const OFF_BLOCKED_OUT_COUNT: usize = 536;
    pub const OFF_KEYBAG_UUID: usize = 544;
    pub const OFF_KEYBAG_OID: usize = 560;
    pub const OFF_EPHEMERAL_INFO: usize = 568;
    pub const OFF_TEST_OID: usize = 600;
    pub const OFF_RESERVED: usize = 608;
}
unsafe impl Pod for nolx_nx_superblock_t {}
pub const NX_SUPERBLOCK_DISK_SIZE: usize = 4096;

/// One entry of a checkpoint map: where an ephemeral object lives now.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_checkpoint_map_entry_t {
    pub r#type: u32,
    pub subtype: u32,
    pub block_size: u32,
    pub pad: u32,
    pub oid: u64,
    pub paddr: u64,
}

impl nolx_checkpoint_map_entry_t {
    /// Size of `nolx_checkpoint_map_entry_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 32;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_TYPE: usize = 0;
    pub const OFF_SUBTYPE: usize = 4;
    pub const OFF_BLOCK_SIZE: usize = 8;
    pub const OFF_PAD: usize = 12;
    pub const OFF_OID: usize = 16;
    pub const OFF_PADDR: usize = 24;
}
unsafe impl Pod for nolx_checkpoint_map_entry_t {}
pub const CHECKPOINT_MAP_ENTRY_DISK_SIZE: usize = 32;

/// Checkpoint map header. Entries follow immediately.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_checkpoint_map_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// 'NCKP'
    pub magic: u32,
    pub pad: u32,
    pub num_entries: u32,
    pub pad2: u32,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 4048],
}

impl nolx_checkpoint_map_phys_t {
    /// Size of `nolx_checkpoint_map_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_MAGIC: usize = 32;
    pub const OFF_PAD: usize = 36;
    pub const OFF_NUM_ENTRIES: usize = 40;
    pub const OFF_PAD2: usize = 44;
    pub const OFF_RESERVED: usize = 48;
}
unsafe impl Pod for nolx_checkpoint_map_phys_t {}
pub const CHECKPOINT_MAP_PHYS_DISK_SIZE: usize = 4096;

/// Object map key: resolve (oid, xid) to a physical address.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_omap_key_t {
    pub oid: u64,
    pub xid: u64,
}

impl nolx_omap_key_t {
    /// Size of `nolx_omap_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OID: usize = 0;
    pub const OFF_XID: usize = 8;
}
unsafe impl Pod for nolx_omap_key_t {}
pub const OMAP_KEY_DISK_SIZE: usize = 16;

/// Object map value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_omap_val_t {
    /// NOLX_OMAP_VAL_FLAG_*
    pub flags: u32,
    pub pad: u32,
    pub paddr: u64,
}

impl nolx_omap_val_t {
    /// Size of `nolx_omap_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_FLAGS: usize = 0;
    pub const OFF_PAD: usize = 4;
    pub const OFF_PADDR: usize = 8;
}
unsafe impl Pod for nolx_omap_val_t {}
pub const OMAP_VAL_DISK_SIZE: usize = 16;

/// Object map header; points at the root of the omap B-tree.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_omap_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// 'NOMP'
    pub magic: u32,
    pub flags: u32,
    pub num_snapshots: u32,
    pub tree_type: u32,
    pub tree_subtype: u32,
    pub pad: u32,
    pub tree_oid: u64,
    pub snapshot_tree_oid: u64,
    pub most_recent_snap_xid: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 4016],
}

impl nolx_omap_phys_t {
    /// Size of `nolx_omap_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_MAGIC: usize = 32;
    pub const OFF_FLAGS: usize = 36;
    pub const OFF_NUM_SNAPSHOTS: usize = 40;
    pub const OFF_TREE_TYPE: usize = 44;
    pub const OFF_TREE_SUBTYPE: usize = 48;
    pub const OFF_PAD: usize = 52;
    pub const OFF_TREE_OID: usize = 56;
    pub const OFF_SNAPSHOT_TREE_OID: usize = 64;
    pub const OFF_MOST_RECENT_SNAP_XID: usize = 72;
    pub const OFF_RESERVED: usize = 80;
}
unsafe impl Pod for nolx_omap_phys_t {}
pub const OMAP_PHYS_DISK_SIZE: usize = 4096;

/// Volume superblock (magic NVSB). One block.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_fs_superblock_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// 'NVSB'
    pub magic: u32,
    /// copied from the container, validated on mount
    pub block_size: u32,
    pub features_supported: u64,
    pub features_required: u64,
    pub features_ro_compat: u64,
    pub reserve_block_count: u64,
    pub quota_block_count: u64,
    pub alloc_count: u64,
    pub meta_crypto_key: [u64; 5],
    pub root_tree_type: u32,
    pub root_tree_subtype: u32,
    pub extentref_tree_type: u32,
    pub extentref_tree_subtype: u32,
    pub snap_meta_tree_type: u32,
    pub snap_meta_tree_subtype: u32,
    pub omap_oid: u64,
    pub root_tree_oid: u64,
    pub extentref_tree_oid: u64,
    pub snap_meta_tree_oid: u64,
    pub revert_to_xid: u64,
    pub revert_to_sb_oid: u64,
    pub create_time: i64,
    pub modify_time: i64,
    pub change_time: i64,
    pub backup_time: i64,
    pub volume_uuid: [u8; 16],
    pub next_ident: u64,
    pub num_files: u64,
    pub num_directories: u64,
    pub num_symlinks: u64,
    pub num_other_fsobjects: u64,
    pub num_snapshots: u64,
    pub total_blocks_alloced: u64,
    pub total_blocks_freed: u64,
    pub keybag_oid: u64,
    pub flags: u64,
    /// UTF-8, NUL padded
    pub volume_name: [u8; 256],
    pub next_doc_id: u32,
    pub role: u16,
    pub pad: u16,
    pub fs_reserve_block_count: u64,
    pub used_by_owner: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 3488],
}

impl nolx_fs_superblock_t {
    /// Size of `nolx_fs_superblock_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_MAGIC: usize = 32;
    pub const OFF_BLOCK_SIZE: usize = 36;
    pub const OFF_FEATURES_SUPPORTED: usize = 40;
    pub const OFF_FEATURES_REQUIRED: usize = 48;
    pub const OFF_FEATURES_RO_COMPAT: usize = 56;
    pub const OFF_RESERVE_BLOCK_COUNT: usize = 64;
    pub const OFF_QUOTA_BLOCK_COUNT: usize = 72;
    pub const OFF_ALLOC_COUNT: usize = 80;
    pub const OFF_META_CRYPTO_KEY: usize = 88;
    pub const OFF_ROOT_TREE_TYPE: usize = 128;
    pub const OFF_ROOT_TREE_SUBTYPE: usize = 132;
    pub const OFF_EXTENTREF_TREE_TYPE: usize = 136;
    pub const OFF_EXTENTREF_TREE_SUBTYPE: usize = 140;
    pub const OFF_SNAP_META_TREE_TYPE: usize = 144;
    pub const OFF_SNAP_META_TREE_SUBTYPE: usize = 148;
    pub const OFF_OMAP_OID: usize = 152;
    pub const OFF_ROOT_TREE_OID: usize = 160;
    pub const OFF_EXTENTREF_TREE_OID: usize = 168;
    pub const OFF_SNAP_META_TREE_OID: usize = 176;
    pub const OFF_REVERT_TO_XID: usize = 184;
    pub const OFF_REVERT_TO_SB_OID: usize = 192;
    pub const OFF_CREATE_TIME: usize = 200;
    pub const OFF_MODIFY_TIME: usize = 208;
    pub const OFF_CHANGE_TIME: usize = 216;
    pub const OFF_BACKUP_TIME: usize = 224;
    pub const OFF_VOLUME_UUID: usize = 232;
    pub const OFF_NEXT_IDENT: usize = 248;
    pub const OFF_NUM_FILES: usize = 256;
    pub const OFF_NUM_DIRECTORIES: usize = 264;
    pub const OFF_NUM_SYMLINKS: usize = 272;
    pub const OFF_NUM_OTHER_FSOBJECTS: usize = 280;
    pub const OFF_NUM_SNAPSHOTS: usize = 288;
    pub const OFF_TOTAL_BLOCKS_ALLOCED: usize = 296;
    pub const OFF_TOTAL_BLOCKS_FREED: usize = 304;
    pub const OFF_KEYBAG_OID: usize = 312;
    pub const OFF_FLAGS: usize = 320;
    pub const OFF_VOLUME_NAME: usize = 328;
    pub const OFF_NEXT_DOC_ID: usize = 584;
    pub const OFF_ROLE: usize = 588;
    pub const OFF_PAD: usize = 590;
    pub const OFF_FS_RESERVE_BLOCK_COUNT: usize = 592;
    pub const OFF_USED_BY_OWNER: usize = 600;
    pub const OFF_RESERVED: usize = 608;
}
unsafe impl Pod for nolx_fs_superblock_t {}
pub const FS_SUPERBLOCK_DISK_SIZE: usize = 4096;

/// B-tree node header. The table of contents follows immediately.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_btree_node_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// NOLX_BTNODE_*
    pub flags: u16,
    /// 0 = leaf
    pub level: u16,
    pub key_count: u32,
    pub key_free_list_off: u32,
    pub key_free_list_len: u32,
    pub val_free_list_off: u32,
    pub val_free_list_len: u32,
}

impl nolx_btree_node_phys_t {
    /// Size of `nolx_btree_node_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 56;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_FLAGS: usize = 32;
    pub const OFF_LEVEL: usize = 34;
    pub const OFF_KEY_COUNT: usize = 36;
    pub const OFF_KEY_FREE_LIST_OFF: usize = 40;
    pub const OFF_KEY_FREE_LIST_LEN: usize = 44;
    pub const OFF_VAL_FREE_LIST_OFF: usize = 48;
    pub const OFF_VAL_FREE_LIST_LEN: usize = 52;
}
unsafe impl Pod for nolx_btree_node_phys_t {}
pub const BTREE_NODE_PHYS_DISK_SIZE: usize = 56;

/// Table-of-contents entry: offsets are relative to the end of this header.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_btree_node_toc_entry_t {
    pub key_off: u16,
    pub key_len: u16,
    pub val_off: u16,
    pub val_len: u16,
}

impl nolx_btree_node_toc_entry_t {
    /// Size of `nolx_btree_node_toc_entry_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 8;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_KEY_OFF: usize = 0;
    pub const OFF_KEY_LEN: usize = 2;
    pub const OFF_VAL_OFF: usize = 4;
    pub const OFF_VAL_LEN: usize = 6;
}
unsafe impl Pod for nolx_btree_node_toc_entry_t {}
pub const BTREE_NODEOC_ENTRY_DISK_SIZE: usize = 8;

/// Per-chunk allocation state.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_chunk_info_t {
    pub xid: u64,
    pub addr: u64,
    pub free_count: u32,
    pub bitmap_len: u32,
    pub bitmap_addr: u64,
}

impl nolx_chunk_info_t {
    /// Size of `nolx_chunk_info_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 32;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_XID: usize = 0;
    pub const OFF_ADDR: usize = 8;
    pub const OFF_FREE_COUNT: usize = 16;
    pub const OFF_BITMAP_LEN: usize = 20;
    pub const OFF_BITMAP_ADDR: usize = 24;
}
unsafe impl Pod for nolx_chunk_info_t {}
pub const CHUNK_INFO_DISK_SIZE: usize = 32;

/// Space manager (ephemeral object).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_spaceman_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    /// 'NSPM'
    pub magic: u32,
    pub block_size: u32,
    pub blocks_per_chunk: u32,
    pub chunks_per_cib: u32,
    pub cib_count: u32,
    pub pad: u32,
    pub bm_size_per_check: u64,
    pub bm_block_count: u64,
    pub blocks_reserved: u64,
    pub chunks_reserved: u64,
    pub free_count: u64,
    pub flags: u32,
    pub pad2: u32,
    pub ip_block_count: u64,
    pub ip_bm_size_per_check: u64,
    pub ip_bm_block_count: u64,
    pub ip_bm_free_head: u64,
    pub ip_bm_free_tail: u64,
    pub ip_bm_xid_offset: u64,
    pub ip_bm_block_count_offset: u64,
    /// byte offset of the chunk-info address array
    pub cib_addr_offset: u64,
    pub cab_addr_offset: u64,
    /// B-tree of free ranges, keyed by start block
    pub free_queue_oid: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 3912],
}

impl nolx_spaceman_phys_t {
    /// Size of `nolx_spaceman_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_MAGIC: usize = 32;
    pub const OFF_BLOCK_SIZE: usize = 36;
    pub const OFF_BLOCKS_PER_CHUNK: usize = 40;
    pub const OFF_CHUNKS_PER_CIB: usize = 44;
    pub const OFF_CIB_COUNT: usize = 48;
    pub const OFF_PAD: usize = 52;
    pub const OFF_BM_SIZE_PER_CHECK: usize = 56;
    pub const OFF_BM_BLOCK_COUNT: usize = 64;
    pub const OFF_BLOCKS_RESERVED: usize = 72;
    pub const OFF_CHUNKS_RESERVED: usize = 80;
    pub const OFF_FREE_COUNT: usize = 88;
    pub const OFF_FLAGS: usize = 96;
    pub const OFF_PAD2: usize = 100;
    pub const OFF_IP_BLOCK_COUNT: usize = 104;
    pub const OFF_IP_BM_SIZE_PER_CHECK: usize = 112;
    pub const OFF_IP_BM_BLOCK_COUNT: usize = 120;
    pub const OFF_IP_BM_FREE_HEAD: usize = 128;
    pub const OFF_IP_BM_FREE_TAIL: usize = 136;
    pub const OFF_IP_BM_XID_OFFSET: usize = 144;
    pub const OFF_IP_BM_BLOCK_COUNT_OFFSET: usize = 152;
    pub const OFF_CIB_ADDR_OFFSET: usize = 160;
    pub const OFF_CAB_ADDR_OFFSET: usize = 168;
    pub const OFF_FREE_QUEUE_OID: usize = 176;
    pub const OFF_RESERVED: usize = 184;
}
unsafe impl Pod for nolx_spaceman_phys_t {}
pub const SPACEMAN_PHYS_DISK_SIZE: usize = 4096;

/// Reaper: deferred free list for COW'd and unlinked objects.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_reaper_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    pub flags: u32,
    pub pad: u32,
    pub oid: u64,
    pub xid: u64,
    /// next reap-list id
    pub nr: u64,
    pub first: u64,
    pub last: u64,
    pub max_record_count: u64,
    pub record_count: u64,
    pub head: u64,
    pub tail: u64,
    pub fwd: u64,
    pub prev: u64,
    pub last_paddr: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 3960],
}

impl nolx_reaper_phys_t {
    /// Size of `nolx_reaper_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_FLAGS: usize = 32;
    pub const OFF_PAD: usize = 36;
    pub const OFF_OID: usize = 40;
    pub const OFF_XID: usize = 48;
    pub const OFF_NR: usize = 56;
    pub const OFF_FIRST: usize = 64;
    pub const OFF_LAST: usize = 72;
    pub const OFF_MAX_RECORD_COUNT: usize = 80;
    pub const OFF_RECORD_COUNT: usize = 88;
    pub const OFF_HEAD: usize = 96;
    pub const OFF_TAIL: usize = 104;
    pub const OFF_FWD: usize = 112;
    pub const OFF_PREV: usize = 120;
    pub const OFF_LAST_PADDR: usize = 128;
    pub const OFF_RESERVED: usize = 136;
}
unsafe impl Pod for nolx_reaper_phys_t {}
pub const REAPER_PHYS_DISK_SIZE: usize = 4096;

/// A node of the reaper list.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_reap_list_phys_t {
    /// nolx_obj_phys_t
    pub o: [u8; 32],
    pub flags: u32,
    pub pad: u32,
    pub max_records: u32,
    pub record_count: u32,
    pub first: u64,
    pub last: u64,
    pub free_head: u64,
    pub free_tail: u64,
    pub block_count: u64,
    /// trailing padding, must be zero on write
    pub _reserved: [u8; 4008],
}

impl nolx_reap_list_phys_t {
    /// Size of `nolx_reap_list_phys_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 4096;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_O: usize = 0;
    pub const OFF_FLAGS: usize = 32;
    pub const OFF_PAD: usize = 36;
    pub const OFF_MAX_RECORDS: usize = 40;
    pub const OFF_RECORD_COUNT: usize = 44;
    pub const OFF_FIRST: usize = 48;
    pub const OFF_LAST: usize = 56;
    pub const OFF_FREE_HEAD: usize = 64;
    pub const OFF_FREE_TAIL: usize = 72;
    pub const OFF_BLOCK_COUNT: usize = 80;
    pub const OFF_RESERVED: usize = 88;
}
unsafe impl Pod for nolx_reap_list_phys_t {}
pub const REAP_LIST_PHYS_DISK_SIZE: usize = 4096;

/// Generic fs-tree key. High 4 bits are the record type, low 60 the inode.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_j_key_t {
    pub obj_id_and_type: u64,
}

impl nolx_j_key_t {
    /// Size of `nolx_j_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 8;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
}
unsafe impl Pod for nolx_j_key_t {}
pub const J_KEY_DISK_SIZE: usize = 8;

/// Header of an embedded per-inode B-tree of xattrs.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_j_hdr_t {
    pub num_keys: u32,
    pub key_size: u32,
    pub val_size: u32,
    pub free_space: u32,
}

impl nolx_j_hdr_t {
    /// Size of `nolx_j_hdr_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_NUM_KEYS: usize = 0;
    pub const OFF_KEY_SIZE: usize = 4;
    pub const OFF_VAL_SIZE: usize = 8;
    pub const OFF_FREE_SPACE: usize = 12;
}
unsafe impl Pod for nolx_j_hdr_t {}
pub const J_HDR_DISK_SIZE: usize = 16;

/// Inline description of a file's data, embedded in the inode record.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_data_stream_t {
    /// nolx_j_hdr_t
    pub xattr_jhdr: [u8; 16],
    pub alloced_size: u64,
    pub default_crypto_id: u64,
    pub total_bytes_written: u64,
    pub total_bytes_read: u64,
}

impl nolx_data_stream_t {
    /// Size of `nolx_data_stream_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 48;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_XATTR_JHDR: usize = 0;
    pub const OFF_ALLOCED_SIZE: usize = 16;
    pub const OFF_DEFAULT_CRYPTO_ID: usize = 24;
    pub const OFF_TOTAL_BYTES_WRITTEN: usize = 32;
    pub const OFF_TOTAL_BYTES_READ: usize = 40;
}
unsafe impl Pod for nolx_data_stream_t {}
pub const DATA_STREAM_DISK_SIZE: usize = 48;

/// INODE record value. POSIX semantics live here, untranslated.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_inode_t {
    pub parent_id: u64,
    pub private_id: u64,
    pub create_time: i64,
    pub modify_time: i64,
    pub change_time: i64,
    pub access_time: i64,
    /// NOLX_INODE_FLAG_*
    pub internal_flags: u64,
    pub nlink: u32,
    /// POSIX mode_t: S_IF* and permission bits
    pub mode: u16,
    pub pad: u16,
    /// uid
    pub owner: u32,
    /// gid
    pub group: u32,
    pub generation: u32,
    pub pad2: u32,
    /// nolx_data_stream_t
    pub data_stream: [u8; 48],
}

impl nolx_inode_t {
    /// Size of `nolx_inode_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 128;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_PARENT_ID: usize = 0;
    pub const OFF_PRIVATE_ID: usize = 8;
    pub const OFF_CREATE_TIME: usize = 16;
    pub const OFF_MODIFY_TIME: usize = 24;
    pub const OFF_CHANGE_TIME: usize = 32;
    pub const OFF_ACCESS_TIME: usize = 40;
    pub const OFF_INTERNAL_FLAGS: usize = 48;
    pub const OFF_NLINK: usize = 56;
    pub const OFF_MODE: usize = 60;
    pub const OFF_PAD: usize = 62;
    pub const OFF_OWNER: usize = 64;
    pub const OFF_GROUP: usize = 68;
    pub const OFF_GENERATION: usize = 72;
    pub const OFF_PAD2: usize = 76;
    pub const OFF_DATA_STREAM: usize = 80;
}
unsafe impl Pod for nolx_inode_t {}
pub const INODE_DISK_SIZE: usize = 128;

/// DIR_REC key: inode, then the name.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_dentry_key_t {
    pub obj_id_and_type: u64,
    pub name_len: u16,
    pub pad: u16,
}

impl nolx_dentry_key_t {
    /// Size of `nolx_dentry_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 12;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
    pub const OFF_NAME_LEN: usize = 8;
    pub const OFF_PAD: usize = 10;
}
unsafe impl Pod for nolx_dentry_key_t {}
pub const DENTRY_KEY_DISK_SIZE: usize = 12;

/// DIR_REC value. The name follows the fixed part.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_direntry_t {
    pub ino: u64,
    pub name_len: u16,
    /// NOLX_DREC_FLAG_*
    pub flags: u16,
    /// stored name length in the value
    pub nlen: u16,
    pub pad: u16,
}

impl nolx_direntry_t {
    /// Size of `nolx_direntry_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_INO: usize = 0;
    pub const OFF_NAME_LEN: usize = 8;
    pub const OFF_FLAGS: usize = 10;
    pub const OFF_NLEN: usize = 12;
    pub const OFF_PAD: usize = 14;
}
unsafe impl Pod for nolx_direntry_t {}
pub const DIRENTRY_DISK_SIZE: usize = 16;

/// SIBLING_LINK key.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_sibling_key_t {
    pub obj_id_and_type: u64,
    pub sibling_id: u64,
    pub name_len: u16,
    pub pad: u16,
}

impl nolx_sibling_key_t {
    /// Size of `nolx_sibling_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 20;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
    pub const OFF_SIBLING_ID: usize = 8;
    pub const OFF_NAME_LEN: usize = 16;
    pub const OFF_PAD: usize = 18;
}
unsafe impl Pod for nolx_sibling_key_t {}
pub const SIBLING_KEY_DISK_SIZE: usize = 20;

/// SIBLING_LINK value: enough to reach the parent by name.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_sibling_val_t {
    pub parent_id: u64,
    pub nlen: u16,
    /// POSIX file type of the parent
    pub r#type: u16,
}

impl nolx_sibling_val_t {
    /// Size of `nolx_sibling_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 12;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_PARENT_ID: usize = 0;
    pub const OFF_NLEN: usize = 8;
    pub const OFF_TYPE: usize = 10;
}
unsafe impl Pod for nolx_sibling_val_t {}
pub const SIBLING_VAL_DISK_SIZE: usize = 12;

/// XATTR key.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_xattr_key_t {
    pub obj_id_and_type: u64,
    pub name_len: u16,
    pub pad: u16,
}

impl nolx_xattr_key_t {
    /// Size of `nolx_xattr_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 12;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
    pub const OFF_NAME_LEN: usize = 8;
    pub const OFF_PAD: usize = 10;
}
unsafe impl Pod for nolx_xattr_key_t {}
pub const XATTR_KEY_DISK_SIZE: usize = 12;

/// PHYS_EXTENT key. Logical address occupies the high bits above the inode.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_phys_ext_key_t {
    pub obj_id_and_type: u64,
}

impl nolx_phys_ext_key_t {
    /// Size of `nolx_phys_ext_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 8;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
}
unsafe impl Pod for nolx_phys_ext_key_t {}
pub const PHYS_EXT_KEY_DISK_SIZE: usize = 8;

/// PHYS_EXTENT value: a range of blocks and who owns it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_phys_ext_val_t {
    /// low 56 bits byte length, high 8 the kind
    pub len_and_kind: u64,
    pub phys_block_num: u64,
    pub crypto_id: u64,
}

impl nolx_phys_ext_val_t {
    /// Size of `nolx_phys_ext_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 24;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_LEN_AND_KIND: usize = 0;
    pub const OFF_PHYS_BLOCK_NUM: usize = 8;
    pub const OFF_CRYPTO_ID: usize = 16;
}
unsafe impl Pod for nolx_phys_ext_val_t {}
pub const PHYS_EXT_VAL_DISK_SIZE: usize = 24;

/// FILE_EXTENT key: inode, then logical byte offset.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_file_extent_key_t {
    pub obj_id_and_type: u64,
    pub logical_addr: u64,
}

impl nolx_file_extent_key_t {
    /// Size of `nolx_file_extent_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
    pub const OFF_LOGICAL_ADDR: usize = 8;
}
unsafe impl Pod for nolx_file_extent_key_t {}
pub const FILE_EXTENT_KEY_DISK_SIZE: usize = 16;

/// FILE_EXTENT value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_file_extent_val_t {
    /// low 56 bits byte length, high 8 flags
    pub len_and_flags: u64,
    pub phys_block_num: u64,
    pub crypto_id: u64,
}

impl nolx_file_extent_val_t {
    /// Size of `nolx_file_extent_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 24;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_LEN_AND_FLAGS: usize = 0;
    pub const OFF_PHYS_BLOCK_NUM: usize = 8;
    pub const OFF_CRYPTO_ID: usize = 16;
}
unsafe impl Pod for nolx_file_extent_val_t {}
pub const FILE_EXTENT_VAL_DISK_SIZE: usize = 24;

/// FEXT_TREE key: inode and the extent number.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_fext_tree_key_t {
    pub obj_id_and_type: u64,
    pub fext_id: u64,
}

impl nolx_fext_tree_key_t {
    /// Size of `nolx_fext_tree_key_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_OBJ_ID_AND_TYPE: usize = 0;
    pub const OFF_FEXT_ID: usize = 8;
}
unsafe impl Pod for nolx_fext_tree_key_t {}
pub const FEXTREE_KEY_DISK_SIZE: usize = 16;

/// FEXT_TREE value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_fext_tree_val_t {
    pub len_and_flags: u64,
    pub phys_block_num: u64,
}

impl nolx_fext_tree_val_t {
    /// Size of `nolx_fext_tree_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 16;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_LEN_AND_FLAGS: usize = 0;
    pub const OFF_PHYS_BLOCK_NUM: usize = 8;
}
unsafe impl Pod for nolx_fext_tree_val_t {}
pub const FEXTREE_VAL_DISK_SIZE: usize = 16;

/// DIR_STATS value: cached directory contents.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_dir_stats_val_t {
    pub num_children: u64,
    pub total_size: u64,
    pub chained_key: u64,
    pub gen_count: u64,
}

impl nolx_dir_stats_val_t {
    /// Size of `nolx_dir_stats_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 32;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_NUM_CHILDREN: usize = 0;
    pub const OFF_TOTAL_SIZE: usize = 8;
    pub const OFF_CHAINED_KEY: usize = 16;
    pub const OFF_GEN_COUNT: usize = 24;
}
unsafe impl Pod for nolx_dir_stats_val_t {}
pub const DIR_STATS_VAL_DISK_SIZE: usize = 32;

/// FILE_INFO value: per-file metadata that is not in the inode.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_file_info_val_t {
    pub doc_id: u64,
    pub hidden_bytes: u64,
    pub hidden_xattr_count: u32,
    pub pad: u32,
}

impl nolx_file_info_val_t {
    /// Size of `nolx_file_info_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 24;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_DOC_ID: usize = 0;
    pub const OFF_HIDDEN_BYTES: usize = 8;
    pub const OFF_HIDDEN_XATTR_COUNT: usize = 16;
    pub const OFF_PAD: usize = 20;
}
unsafe impl Pod for nolx_file_info_val_t {}
pub const FILE_INFO_VAL_DISK_SIZE: usize = 24;

/// SNAP_METADATA value in the snapshot metadata tree.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct nolx_snap_metadata_val_t {
    pub extentref_tree_oid: u64,
    pub snap_xid: u64,
    pub num_files: u64,
    pub num_dirs: u64,
    pub num_symlinks: u64,
    pub num_other_fsobjects: u64,
    pub create_time: i64,
    pub change_time: i64,
    pub inum: u64,
    pub extentref_tree_subtype: u32,
    pub flags: u32,
    pub name_len: u16,
    pub pad: u16,
}

impl nolx_snap_metadata_val_t {
    /// Size of `nolx_snap_metadata_val_t` on disk, in bytes.
    pub const DISK_SIZE: usize = 84;
    /// Offset of every field, so readers can validate without `memoffset`.
    pub const OFF_EXTENTREF_TREE_OID: usize = 0;
    pub const OFF_SNAP_XID: usize = 8;
    pub const OFF_NUM_FILES: usize = 16;
    pub const OFF_NUM_DIRS: usize = 24;
    pub const OFF_NUM_SYMLINKS: usize = 32;
    pub const OFF_NUM_OTHER_FSOBJECTS: usize = 40;
    pub const OFF_CREATE_TIME: usize = 48;
    pub const OFF_CHANGE_TIME: usize = 56;
    pub const OFF_INUM: usize = 64;
    pub const OFF_EXTENTREF_TREE_SUBTYPE: usize = 72;
    pub const OFF_FLAGS: usize = 76;
    pub const OFF_NAME_LEN: usize = 80;
    pub const OFF_PAD: usize = 82;
}
unsafe impl Pod for nolx_snap_metadata_val_t {}
pub const SNAP_METADATA_VAL_DISK_SIZE: usize = 84;

// ------------------------------------------------------------------ derived geometry
/// Checkpoint-map entries that fit in one block.
pub const CHECKPOINT_MAP_CAPACITY: usize = 126;
