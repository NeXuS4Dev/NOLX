/* GENERATED FILE -- DO NOT EDIT.
 * Source of truth: tools/nolxfs-codegen/nolxfs_format.py
 * Regenerate with: make fscheck
 *
 */

#ifndef NOLX_FS_FORMAT_H
#define NOLX_FS_FORMAT_H

#include <stdint.h>

#define NOLXFS_FORMAT_VERSION 1u

/* NOLX container superblock */
#define NOLX_MAGIC_CONTAINER_SUPERBLOCK 0x4253434Eu  /* "NCSB" */
/* NOLX volume superblock */
#define NOLX_MAGIC_VOLUME_SUPERBLOCK 0x4253564Eu  /* "NVSB" */
/* object map */
#define NOLX_MAGIC_OMAP 0x504D4F4Eu  /* "NOMP" */
/* checkpoint map */
#define NOLX_MAGIC_CHECKPOINT_MAP 0x504B434Eu  /* "NCKP" */
/* space manager */
#define NOLX_MAGIC_SPACEMAN 0x4D50534Eu  /* "NSPM" */

#define NOLX_OBJ_CONTAINER_SUPERBLOCK 0x1u
#define NOLX_OBJ_BTREE_ROOT 0x2u
#define NOLX_OBJ_BTREE_NODE 0x3u
#define NOLX_OBJ_SPACEMAN 0x4u
#define NOLX_OBJ_SPACEMAN_CAB 0x5u
#define NOLX_OBJ_SPACEMAN_CIB 0x6u
#define NOLX_OBJ_SPACEMAN_BITMAP 0x7u
#define NOLX_OBJ_OMAP 0x8u
#define NOLX_OBJ_CHECKPOINT_MAP 0x9u
#define NOLX_OBJ_VOLUME_SUPERBLOCK 0xAu
#define NOLX_OBJ_SNAP_METADATA 0xBu
#define NOLX_OBJ_REAPER 0xCu
#define NOLX_OBJ_REAP_LIST 0xDu
#define NOLX_OBJ_EFI_JUMPSTART 0xEu
#define NOLX_OBJ_GBITMAP 0xFu
#define NOLX_OBJ_GBITMAP_BLOCK 0x10u
#define NOLX_OBJ_ER_STATE 0x11u

#define NOLX_SUB_NONE 0x0u
#define NOLX_SUB_OMAP 0x1u
#define NOLX_SUB_FSTREE 0x2u
#define NOLX_SUB_EXTENT_REF_TREE 0x3u
#define NOLX_SUB_SNAP_META_TREE 0x4u
#define NOLX_SUB_FEXT_TREE 0x5u

#define NOLX_DREC_TYPE_SNAP_METADATA 0x1u
#define NOLX_DREC_TYPE_PHYS_EXTENT 0x2u
#define NOLX_DREC_TYPE_INODE 0x3u
#define NOLX_DREC_TYPE_XATTR 0x4u
#define NOLX_DREC_TYPE_SIBLING_LINK 0x5u
#define NOLX_DREC_TYPE_DSTREAM_ID 0x6u
#define NOLX_DREC_TYPE_CRYPTO_STATE 0x7u
#define NOLX_DREC_TYPE_FILE_EXTENT 0x8u
#define NOLX_DREC_TYPE_DIR_REC 0x9u
#define NOLX_DREC_TYPE_DIR_STATS 0xAu
#define NOLX_DREC_TYPE_SIBLING_MAP 0xBu
#define NOLX_DREC_TYPE_FILE_INFO 0xCu

#define NOLX_OBJ_FLAG_NO_CKSUM 0x1u
#define NOLX_OBJ_FLAG_ENCRYPTED 0x2u
#define NOLX_OBJ_FLAG_COMPRESSED 0x4u
#define NOLX_OBJ_FLAG_EPHEMERAL 0x8u

#define NOLX_OMAP_VAL_FLAG_SAVED 0x1u
#define NOLX_OMAP_VAL_FLAG_MISSING 0x2u
#define NOLX_OMAP_VAL_FLAG_NO_SNAPSHOT 0x4u

#define NOLX_BTNODE_ROOT 0x1u
#define NOLX_BTNODE_LEAF 0x2u
#define NOLX_BTNODE_FIXED_KV_SIZE 0x4u
#define NOLX_BTNODE_NO_FREE 0x8u
#define NOLX_BTNODE_INDEX 0x10u

#define NOLX_NX_FEATURE_REQ_V1_BASE 0x1u

#define NOLX_NX_FEATURE_RO_COMPAT_SEALED_VOLUME 0x1u

#define NOLX_NX_FEATURE_SUP_CASEFOLD 0x1u
#define NOLX_NX_FEATURE_SUP_NORMALIZATION_INSENSITIVE 0x2u
#define NOLX_NX_FEATURE_SUP_ENCRYPTION 0x4u
#define NOLX_NX_FEATURE_SUP_FEXT 0x8u
#define NOLX_NX_FEATURE_SUP_SNAPSHOTS 0x10u
#define NOLX_NX_FEATURE_SUP_INTEGRITY_META 0x20u

#define NOLX_INODE_FLAG_SUID_SAVED 0x1u
#define NOLX_INODE_FLAG_SGID_SAVED 0x2u
#define NOLX_INODE_FLAG_CLONED 0x4u
#define NOLX_INODE_FLAG_SPARSE 0x8u
#define NOLX_INODE_FLAG_COMPRESSED 0x10u
#define NOLX_INODE_FLAG_HIDDEN 0x20u
#define NOLX_INODE_FLAG_FAST_DOC_ID 0x40u

#define NOLX_MAX_VOLUMES_PER_CONTAINER 8ull  /* fixed size of fs_oid[] in the container superblock */
#define NOLX_MAX_NAME_LEN 255ull  /* POSIX NAME_MAX */
#define NOLX_MAX_PATH_LEN 1024ull  /* POSIX PATH_MAX */
#define NOLX_MAX_VOLUME_NAME_LEN 255ull  /* longest volume name */
#define NOLX_SYMLOOP_MAX 40ull  /* symlink resolution depth, POSIX minimum is 8 */
#define NOLX_MAX_EXTENT_LEN 72057594037927935ull  /* extent lengths are 56-bit byte counts */
#define NOLX_DEFAULT_BLOCK_SIZE 4096ull  /* block size chosen by mkfs unless told otherwise */
#define NOLX_MIN_BLOCK_SIZE 1024ull  /* smallest permitted block size */
#define NOLX_MAX_BLOCK_SIZE 16384ull  /* largest permitted block size */
#define NOLX_MAX_CHECKPOINT_DESCRIPTORS 128ull  /* descriptors in the checkpoint ring */
#define NOLX_ROOT_INO 1ull  /* volume root directory */
#define NOLX_PRIVATE_DIR_INO 2ull  /* private directory / orphan holding pen */
#define NOLX_SNAPSHOT_DIR_INO 3ull  /* read-only view of the volume's snapshots */
#define NOLX_FIRST_USER_INO 1024ull  /* first inode number available to users */
#define NOLX_OBJ_ID_BITS 60ull  /* inode numbers occupy the low 60 bits of a j-key */
#define NOLX_BTREE_SPLIT_FILL_PCT 90ull  /* split a node above this fill */
#define NOLX_BTREE_MERGE_FILL_PCT 50ull  /* merge a node below this fill */

/* Header of every on-disk object. 32 bytes, no padding. */
typedef struct {
    /* Fletcher-64 over the object with this field = 0 */
    uint64_t checksum;
    /* object id, resolved through an object map */
    uint64_t oid;
    /* transaction id of the version that created this */
    uint64_t xid;
    /* NOLX_OBJ_* */
    uint32_t type;
    /* NOLX_SUB_* */
    uint16_t subtype;
    /* NOLX_OBJ_FLAG_* */
    uint16_t flags;
} nolx_obj_phys_t;
#define NOLX_NOLX_OBJ_PHYS_T_SIZE 32u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_CHECKSUM 0u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_OID 8u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_XID 16u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_TYPE 24u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_SUBTYPE 28u
#define NOLX_NOLX_OBJ_PHYS_T_OFF_FLAGS 30u

/* Container superblock (magic NCSB). One block. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* 'NCSB' */
    uint32_t magic;
    uint32_t block_size;
    uint64_t block_count;
    uint64_t features_supported;
    uint64_t features_required;
    uint64_t features_ro_compat;
    uint8_t uuid[16];
    uint64_t next_oid;
    uint64_t next_xid;
    /* blocks reserved for checkpoint descriptors */
    uint32_t xp_desc_blocks;
    /* blocks reserved for checkpoint data */
    uint32_t xp_data_blocks;
    uint64_t xp_desc_base;
    uint64_t xp_data_base;
    /* ring index of the newest valid descriptor */
    uint32_t xp_desc_index;
    uint32_t xp_desc_len;
    uint32_t xp_desc_next;
    uint32_t xp_data_index;
    uint32_t xp_data_len;
    uint32_t xp_data_next;
    uint32_t fs_oid_count;
    uint32_t pad0;
    /* object ids of the volumes in this container */
    uint64_t fs_oid[8];
    /* NOLX_COUNTER_* statistics */
    uint64_t counters[32];
    /* physical object id of the container object map */
    uint64_t omap_oid;
    /* ephemeral reaper object */
    uint64_t reaper_oid;
    uint64_t flags;
    uint64_t efi_jumpstart_oid;
    uint8_t fusion_uuid[16];
    /* first block of a bad region */
    uint64_t blocked_out_start;
    uint64_t blocked_out_count;
    uint8_t keybag_uuid[16];
    uint64_t keybag_oid;
    /* paddr/len of the ephemeral info block */
    uint64_t ephemeral_info[4];
    /* object used by fsck for write testing */
    uint64_t test_oid;
    uint8_t _reserved[3488];  /* must be zero on write */
} nolx_nx_superblock_t;
#define NOLX_NOLX_NX_SUPERBLOCK_T_SIZE 4096u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_O 0u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_MAGIC 32u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_BLOCK_SIZE 36u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_BLOCK_COUNT 40u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FEATURES_SUPPORTED 48u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FEATURES_REQUIRED 56u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FEATURES_RO_COMPAT 64u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_UUID 72u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_NEXT_OID 88u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_NEXT_XID 96u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DESC_BLOCKS 104u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DATA_BLOCKS 108u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DESC_BASE 112u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DATA_BASE 120u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DESC_INDEX 128u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DESC_LEN 132u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DESC_NEXT 136u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DATA_INDEX 140u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DATA_LEN 144u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_XP_DATA_NEXT 148u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FS_OID_COUNT 152u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_PAD0 156u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FS_OID 160u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_COUNTERS 224u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_OMAP_OID 480u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_REAPER_OID 488u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FLAGS 496u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_EFI_JUMPSTART_OID 504u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_FUSION_UUID 512u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_BLOCKED_OUT_START 528u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_BLOCKED_OUT_COUNT 536u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_KEYBAG_UUID 544u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_KEYBAG_OID 560u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_EPHEMERAL_INFO 568u
#define NOLX_NOLX_NX_SUPERBLOCK_T_OFF_TEST_OID 600u

/* One entry of a checkpoint map: where an ephemeral object lives now. */
typedef struct {
    uint32_t type;
    uint32_t subtype;
    uint32_t block_size;
    uint32_t pad;
    uint64_t oid;
    uint64_t paddr;
} nolx_checkpoint_map_entry_t;
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_SIZE 32u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_TYPE 0u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_SUBTYPE 4u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_BLOCK_SIZE 8u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_PAD 12u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_OID 16u
#define NOLX_NOLX_CHECKPOINT_MAP_ENTRY_T_OFF_PADDR 24u

/* Checkpoint map header. Entries follow immediately. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* 'NCKP' */
    uint32_t magic;
    uint32_t pad;
    uint32_t num_entries;
    uint32_t pad2;
    uint8_t _reserved[4048];  /* must be zero on write */
} nolx_checkpoint_map_phys_t;
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_SIZE 4096u
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_OFF_O 0u
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_OFF_MAGIC 32u
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_OFF_PAD 36u
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_OFF_NUM_ENTRIES 40u
#define NOLX_NOLX_CHECKPOINT_MAP_PHYS_T_OFF_PAD2 44u

/* Object map key: resolve (oid, xid) to a physical address. */
typedef struct {
    uint64_t oid;
    uint64_t xid;
} nolx_omap_key_t;
#define NOLX_NOLX_OMAP_KEY_T_SIZE 16u
#define NOLX_NOLX_OMAP_KEY_T_OFF_OID 0u
#define NOLX_NOLX_OMAP_KEY_T_OFF_XID 8u

/* Object map value. */
typedef struct {
    /* NOLX_OMAP_VAL_FLAG_* */
    uint32_t flags;
    uint32_t pad;
    uint64_t paddr;
} nolx_omap_val_t;
#define NOLX_NOLX_OMAP_VAL_T_SIZE 16u
#define NOLX_NOLX_OMAP_VAL_T_OFF_FLAGS 0u
#define NOLX_NOLX_OMAP_VAL_T_OFF_PAD 4u
#define NOLX_NOLX_OMAP_VAL_T_OFF_PADDR 8u

/* Object map header; points at the root of the omap B-tree. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* 'NOMP' */
    uint32_t magic;
    uint32_t flags;
    uint32_t num_snapshots;
    uint32_t tree_type;
    uint32_t tree_subtype;
    uint32_t pad;
    uint64_t tree_oid;
    uint64_t snapshot_tree_oid;
    uint64_t most_recent_snap_xid;
    uint8_t _reserved[4016];  /* must be zero on write */
} nolx_omap_phys_t;
#define NOLX_NOLX_OMAP_PHYS_T_SIZE 4096u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_O 0u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_MAGIC 32u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_FLAGS 36u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_NUM_SNAPSHOTS 40u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_TREE_TYPE 44u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_TREE_SUBTYPE 48u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_PAD 52u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_TREE_OID 56u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_SNAPSHOT_TREE_OID 64u
#define NOLX_NOLX_OMAP_PHYS_T_OFF_MOST_RECENT_SNAP_XID 72u

/* Volume superblock (magic NVSB). One block. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* 'NVSB' */
    uint32_t magic;
    /* copied from the container, validated on mount */
    uint32_t block_size;
    uint64_t features_supported;
    uint64_t features_required;
    uint64_t features_ro_compat;
    uint64_t reserve_block_count;
    uint64_t quota_block_count;
    uint64_t alloc_count;
    uint64_t meta_crypto_key[5];
    uint32_t root_tree_type;
    uint32_t root_tree_subtype;
    uint32_t extentref_tree_type;
    uint32_t extentref_tree_subtype;
    uint32_t snap_meta_tree_type;
    uint32_t snap_meta_tree_subtype;
    uint64_t omap_oid;
    uint64_t root_tree_oid;
    uint64_t extentref_tree_oid;
    uint64_t snap_meta_tree_oid;
    uint64_t revert_to_xid;
    uint64_t revert_to_sb_oid;
    int64_t create_time;
    int64_t modify_time;
    int64_t change_time;
    int64_t backup_time;
    uint8_t volume_uuid[16];
    uint64_t next_ident;
    uint64_t num_files;
    uint64_t num_directories;
    uint64_t num_symlinks;
    uint64_t num_other_fsobjects;
    uint64_t num_snapshots;
    uint64_t total_blocks_alloced;
    uint64_t total_blocks_freed;
    uint64_t keybag_oid;
    uint64_t flags;
    /* UTF-8, NUL padded */
    uint8_t volume_name[256];
    uint32_t next_doc_id;
    uint16_t role;
    uint16_t pad;
    uint64_t fs_reserve_block_count;
    uint64_t used_by_owner;
    uint8_t _reserved[3488];  /* must be zero on write */
} nolx_fs_superblock_t;
#define NOLX_NOLX_FS_SUPERBLOCK_T_SIZE 4096u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_O 0u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_MAGIC 32u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_BLOCK_SIZE 36u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_FEATURES_SUPPORTED 40u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_FEATURES_REQUIRED 48u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_FEATURES_RO_COMPAT 56u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_RESERVE_BLOCK_COUNT 64u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_QUOTA_BLOCK_COUNT 72u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_ALLOC_COUNT 80u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_META_CRYPTO_KEY 88u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_ROOT_TREE_TYPE 128u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_ROOT_TREE_SUBTYPE 132u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_EXTENTREF_TREE_TYPE 136u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_EXTENTREF_TREE_SUBTYPE 140u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_SNAP_META_TREE_TYPE 144u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_SNAP_META_TREE_SUBTYPE 148u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_OMAP_OID 152u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_ROOT_TREE_OID 160u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_EXTENTREF_TREE_OID 168u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_SNAP_META_TREE_OID 176u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_REVERT_TO_XID 184u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_REVERT_TO_SB_OID 192u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_CREATE_TIME 200u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_MODIFY_TIME 208u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_CHANGE_TIME 216u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_BACKUP_TIME 224u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_VOLUME_UUID 232u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NEXT_IDENT 248u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NUM_FILES 256u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NUM_DIRECTORIES 264u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NUM_SYMLINKS 272u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NUM_OTHER_FSOBJECTS 280u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NUM_SNAPSHOTS 288u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_TOTAL_BLOCKS_ALLOCED 296u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_TOTAL_BLOCKS_FREED 304u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_KEYBAG_OID 312u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_FLAGS 320u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_VOLUME_NAME 328u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_NEXT_DOC_ID 584u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_ROLE 588u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_PAD 590u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_FS_RESERVE_BLOCK_COUNT 592u
#define NOLX_NOLX_FS_SUPERBLOCK_T_OFF_USED_BY_OWNER 600u

/* B-tree node header. The table of contents follows immediately. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* NOLX_BTNODE_* */
    uint16_t flags;
    /* 0 = leaf */
    uint16_t level;
    uint32_t key_count;
    uint32_t key_free_list_off;
    uint32_t key_free_list_len;
    uint32_t val_free_list_off;
    uint32_t val_free_list_len;
} nolx_btree_node_phys_t;
#define NOLX_NOLX_BTREE_NODE_PHYS_T_SIZE 56u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_O 0u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_FLAGS 32u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_LEVEL 34u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_KEY_COUNT 36u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_KEY_FREE_LIST_OFF 40u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_KEY_FREE_LIST_LEN 44u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_VAL_FREE_LIST_OFF 48u
#define NOLX_NOLX_BTREE_NODE_PHYS_T_OFF_VAL_FREE_LIST_LEN 52u

/* Table-of-contents entry: offsets are relative to the end of this header. */
typedef struct {
    uint16_t key_off;
    uint16_t key_len;
    uint16_t val_off;
    uint16_t val_len;
} nolx_btree_node_toc_entry_t;
#define NOLX_NOLX_BTREE_NODE_TOC_ENTRY_T_SIZE 8u
#define NOLX_NOLX_BTREE_NODE_TOC_ENTRY_T_OFF_KEY_OFF 0u
#define NOLX_NOLX_BTREE_NODE_TOC_ENTRY_T_OFF_KEY_LEN 2u
#define NOLX_NOLX_BTREE_NODE_TOC_ENTRY_T_OFF_VAL_OFF 4u
#define NOLX_NOLX_BTREE_NODE_TOC_ENTRY_T_OFF_VAL_LEN 6u

/* Per-chunk allocation state. */
typedef struct {
    uint64_t xid;
    uint64_t addr;
    uint32_t free_count;
    uint32_t bitmap_len;
    uint64_t bitmap_addr;
} nolx_chunk_info_t;
#define NOLX_NOLX_CHUNK_INFO_T_SIZE 32u
#define NOLX_NOLX_CHUNK_INFO_T_OFF_XID 0u
#define NOLX_NOLX_CHUNK_INFO_T_OFF_ADDR 8u
#define NOLX_NOLX_CHUNK_INFO_T_OFF_FREE_COUNT 16u
#define NOLX_NOLX_CHUNK_INFO_T_OFF_BITMAP_LEN 20u
#define NOLX_NOLX_CHUNK_INFO_T_OFF_BITMAP_ADDR 24u

/* Space manager (ephemeral object). */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    /* 'NSPM' */
    uint32_t magic;
    uint32_t block_size;
    uint32_t blocks_per_chunk;
    uint32_t chunks_per_cib;
    uint32_t cib_count;
    uint32_t pad;
    uint64_t bm_size_per_check;
    uint64_t bm_block_count;
    uint64_t blocks_reserved;
    uint64_t chunks_reserved;
    uint64_t free_count;
    uint32_t flags;
    uint32_t pad2;
    uint64_t ip_block_count;
    uint64_t ip_bm_size_per_check;
    uint64_t ip_bm_block_count;
    uint64_t ip_bm_free_head;
    uint64_t ip_bm_free_tail;
    uint64_t ip_bm_xid_offset;
    uint64_t ip_bm_block_count_offset;
    /* byte offset of the chunk-info address array */
    uint64_t cib_addr_offset;
    uint64_t cab_addr_offset;
    /* B-tree of free ranges, keyed by start block */
    uint64_t free_queue_oid;
    uint8_t _reserved[3912];  /* must be zero on write */
} nolx_spaceman_phys_t;
#define NOLX_NOLX_SPACEMAN_PHYS_T_SIZE 4096u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_O 0u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_MAGIC 32u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_BLOCK_SIZE 36u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_BLOCKS_PER_CHUNK 40u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_CHUNKS_PER_CIB 44u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_CIB_COUNT 48u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_PAD 52u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_BM_SIZE_PER_CHECK 56u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_BM_BLOCK_COUNT 64u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_BLOCKS_RESERVED 72u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_CHUNKS_RESERVED 80u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_FREE_COUNT 88u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_FLAGS 96u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_PAD2 100u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BLOCK_COUNT 104u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_SIZE_PER_CHECK 112u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_BLOCK_COUNT 120u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_FREE_HEAD 128u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_FREE_TAIL 136u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_XID_OFFSET 144u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_IP_BM_BLOCK_COUNT_OFFSET 152u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_CIB_ADDR_OFFSET 160u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_CAB_ADDR_OFFSET 168u
#define NOLX_NOLX_SPACEMAN_PHYS_T_OFF_FREE_QUEUE_OID 176u

/* Reaper: deferred free list for COW'd and unlinked objects. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    uint32_t flags;
    uint32_t pad;
    uint64_t oid;
    uint64_t xid;
    /* next reap-list id */
    uint64_t nr;
    uint64_t first;
    uint64_t last;
    uint64_t max_record_count;
    uint64_t record_count;
    uint64_t head;
    uint64_t tail;
    uint64_t fwd;
    uint64_t prev;
    uint64_t last_paddr;
    uint8_t _reserved[3960];  /* must be zero on write */
} nolx_reaper_phys_t;
#define NOLX_NOLX_REAPER_PHYS_T_SIZE 4096u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_O 0u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_FLAGS 32u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_PAD 36u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_OID 40u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_XID 48u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_NR 56u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_FIRST 64u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_LAST 72u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_MAX_RECORD_COUNT 80u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_RECORD_COUNT 88u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_HEAD 96u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_TAIL 104u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_FWD 112u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_PREV 120u
#define NOLX_NOLX_REAPER_PHYS_T_OFF_LAST_PADDR 128u

/* A node of the reaper list. */
typedef struct {
    /* nolx_obj_phys_t */
    uint8_t o[32];
    uint32_t flags;
    uint32_t pad;
    uint32_t max_records;
    uint32_t record_count;
    uint64_t first;
    uint64_t last;
    uint64_t free_head;
    uint64_t free_tail;
    uint64_t block_count;
    uint8_t _reserved[4008];  /* must be zero on write */
} nolx_reap_list_phys_t;
#define NOLX_NOLX_REAP_LIST_PHYS_T_SIZE 4096u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_O 0u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_FLAGS 32u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_PAD 36u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_MAX_RECORDS 40u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_RECORD_COUNT 44u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_FIRST 48u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_LAST 56u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_FREE_HEAD 64u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_FREE_TAIL 72u
#define NOLX_NOLX_REAP_LIST_PHYS_T_OFF_BLOCK_COUNT 80u

/* Generic fs-tree key. High 4 bits are the record type, low 60 the inode. */
typedef struct {
    uint64_t obj_id_and_type;
} nolx_j_key_t;
#define NOLX_NOLX_J_KEY_T_SIZE 8u
#define NOLX_NOLX_J_KEY_T_OFF_OBJ_ID_AND_TYPE 0u

/* Header of an embedded per-inode B-tree of xattrs. */
typedef struct {
    uint32_t num_keys;
    uint32_t key_size;
    uint32_t val_size;
    uint32_t free_space;
} nolx_j_hdr_t;
#define NOLX_NOLX_J_HDR_T_SIZE 16u
#define NOLX_NOLX_J_HDR_T_OFF_NUM_KEYS 0u
#define NOLX_NOLX_J_HDR_T_OFF_KEY_SIZE 4u
#define NOLX_NOLX_J_HDR_T_OFF_VAL_SIZE 8u
#define NOLX_NOLX_J_HDR_T_OFF_FREE_SPACE 12u

/* Inline description of a file's data, embedded in the inode record. */
typedef struct {
    /* nolx_j_hdr_t */
    uint8_t xattr_jhdr[16];
    uint64_t alloced_size;
    uint64_t default_crypto_id;
    uint64_t total_bytes_written;
    uint64_t total_bytes_read;
} nolx_data_stream_t;
#define NOLX_NOLX_DATA_STREAM_T_SIZE 48u
#define NOLX_NOLX_DATA_STREAM_T_OFF_XATTR_JHDR 0u
#define NOLX_NOLX_DATA_STREAM_T_OFF_ALLOCED_SIZE 16u
#define NOLX_NOLX_DATA_STREAM_T_OFF_DEFAULT_CRYPTO_ID 24u
#define NOLX_NOLX_DATA_STREAM_T_OFF_TOTAL_BYTES_WRITTEN 32u
#define NOLX_NOLX_DATA_STREAM_T_OFF_TOTAL_BYTES_READ 40u

/* INODE record value. POSIX semantics live here, untranslated. */
typedef struct {
    uint64_t parent_id;
    uint64_t private_id;
    int64_t create_time;
    int64_t modify_time;
    int64_t change_time;
    int64_t access_time;
    /* NOLX_INODE_FLAG_* */
    uint64_t internal_flags;
    uint32_t nlink;
    /* POSIX mode_t: S_IF* and permission bits */
    uint16_t mode;
    uint16_t pad;
    /* uid */
    uint32_t owner;
    /* gid */
    uint32_t group;
    uint32_t generation;
    uint32_t pad2;
    /* nolx_data_stream_t */
    uint8_t data_stream[48];
} nolx_inode_t;
#define NOLX_NOLX_INODE_T_SIZE 128u
#define NOLX_NOLX_INODE_T_OFF_PARENT_ID 0u
#define NOLX_NOLX_INODE_T_OFF_PRIVATE_ID 8u
#define NOLX_NOLX_INODE_T_OFF_CREATE_TIME 16u
#define NOLX_NOLX_INODE_T_OFF_MODIFY_TIME 24u
#define NOLX_NOLX_INODE_T_OFF_CHANGE_TIME 32u
#define NOLX_NOLX_INODE_T_OFF_ACCESS_TIME 40u
#define NOLX_NOLX_INODE_T_OFF_INTERNAL_FLAGS 48u
#define NOLX_NOLX_INODE_T_OFF_NLINK 56u
#define NOLX_NOLX_INODE_T_OFF_MODE 60u
#define NOLX_NOLX_INODE_T_OFF_PAD 62u
#define NOLX_NOLX_INODE_T_OFF_OWNER 64u
#define NOLX_NOLX_INODE_T_OFF_GROUP 68u
#define NOLX_NOLX_INODE_T_OFF_GENERATION 72u
#define NOLX_NOLX_INODE_T_OFF_PAD2 76u
#define NOLX_NOLX_INODE_T_OFF_DATA_STREAM 80u

/* DIR_REC key: inode, then the name. */
typedef struct {
    uint64_t obj_id_and_type;
    uint16_t name_len;
    uint16_t pad;
} nolx_dentry_key_t;
#define NOLX_NOLX_DENTRY_KEY_T_SIZE 12u
#define NOLX_NOLX_DENTRY_KEY_T_OFF_OBJ_ID_AND_TYPE 0u
#define NOLX_NOLX_DENTRY_KEY_T_OFF_NAME_LEN 8u
#define NOLX_NOLX_DENTRY_KEY_T_OFF_PAD 10u

/* DIR_REC value. The name follows the fixed part. */
typedef struct {
    uint64_t ino;
    uint16_t name_len;
    /* NOLX_DREC_FLAG_* */
    uint16_t flags;
    /* stored name length in the value */
    uint16_t nlen;
    uint16_t pad;
} nolx_direntry_t;
#define NOLX_NOLX_DIRENTRY_T_SIZE 16u
#define NOLX_NOLX_DIRENTRY_T_OFF_INO 0u
#define NOLX_NOLX_DIRENTRY_T_OFF_NAME_LEN 8u
#define NOLX_NOLX_DIRENTRY_T_OFF_FLAGS 10u
#define NOLX_NOLX_DIRENTRY_T_OFF_NLEN 12u
#define NOLX_NOLX_DIRENTRY_T_OFF_PAD 14u

/* SIBLING_LINK key. */
typedef struct {
    uint64_t obj_id_and_type;
    uint64_t sibling_id;
    uint16_t name_len;
    uint16_t pad;
} nolx_sibling_key_t;
#define NOLX_NOLX_SIBLING_KEY_T_SIZE 20u
#define NOLX_NOLX_SIBLING_KEY_T_OFF_OBJ_ID_AND_TYPE 0u
#define NOLX_NOLX_SIBLING_KEY_T_OFF_SIBLING_ID 8u
#define NOLX_NOLX_SIBLING_KEY_T_OFF_NAME_LEN 16u
#define NOLX_NOLX_SIBLING_KEY_T_OFF_PAD 18u

/* SIBLING_LINK value: enough to reach the parent by name. */
typedef struct {
    uint64_t parent_id;
    uint16_t nlen;
    /* POSIX file type of the parent */
    uint16_t type;
} nolx_sibling_val_t;
#define NOLX_NOLX_SIBLING_VAL_T_SIZE 12u
#define NOLX_NOLX_SIBLING_VAL_T_OFF_PARENT_ID 0u
#define NOLX_NOLX_SIBLING_VAL_T_OFF_NLEN 8u
#define NOLX_NOLX_SIBLING_VAL_T_OFF_TYPE 10u

/* XATTR key. */
typedef struct {
    uint64_t obj_id_and_type;
    uint16_t name_len;
    uint16_t pad;
} nolx_xattr_key_t;
#define NOLX_NOLX_XATTR_KEY_T_SIZE 12u
#define NOLX_NOLX_XATTR_KEY_T_OFF_OBJ_ID_AND_TYPE 0u
#define NOLX_NOLX_XATTR_KEY_T_OFF_NAME_LEN 8u
#define NOLX_NOLX_XATTR_KEY_T_OFF_PAD 10u

/* PHYS_EXTENT key. Logical address occupies the high bits above the inode. */
typedef struct {
    uint64_t obj_id_and_type;
} nolx_phys_ext_key_t;
#define NOLX_NOLX_PHYS_EXT_KEY_T_SIZE 8u
#define NOLX_NOLX_PHYS_EXT_KEY_T_OFF_OBJ_ID_AND_TYPE 0u

/* PHYS_EXTENT value: a range of blocks and who owns it. */
typedef struct {
    /* low 56 bits byte length, high 8 the kind */
    uint64_t len_and_kind;
    uint64_t phys_block_num;
    uint64_t crypto_id;
} nolx_phys_ext_val_t;
#define NOLX_NOLX_PHYS_EXT_VAL_T_SIZE 24u
#define NOLX_NOLX_PHYS_EXT_VAL_T_OFF_LEN_AND_KIND 0u
#define NOLX_NOLX_PHYS_EXT_VAL_T_OFF_PHYS_BLOCK_NUM 8u
#define NOLX_NOLX_PHYS_EXT_VAL_T_OFF_CRYPTO_ID 16u

/* FILE_EXTENT key: inode, then logical byte offset. */
typedef struct {
    uint64_t obj_id_and_type;
    uint64_t logical_addr;
} nolx_file_extent_key_t;
#define NOLX_NOLX_FILE_EXTENT_KEY_T_SIZE 16u
#define NOLX_NOLX_FILE_EXTENT_KEY_T_OFF_OBJ_ID_AND_TYPE 0u
#define NOLX_NOLX_FILE_EXTENT_KEY_T_OFF_LOGICAL_ADDR 8u

/* FILE_EXTENT value. */
typedef struct {
    /* low 56 bits byte length, high 8 flags */
    uint64_t len_and_flags;
    uint64_t phys_block_num;
    uint64_t crypto_id;
} nolx_file_extent_val_t;
#define NOLX_NOLX_FILE_EXTENT_VAL_T_SIZE 24u
#define NOLX_NOLX_FILE_EXTENT_VAL_T_OFF_LEN_AND_FLAGS 0u
#define NOLX_NOLX_FILE_EXTENT_VAL_T_OFF_PHYS_BLOCK_NUM 8u
#define NOLX_NOLX_FILE_EXTENT_VAL_T_OFF_CRYPTO_ID 16u

/* FEXT_TREE key: inode and the extent number. */
typedef struct {
    uint64_t obj_id_and_type;
    uint64_t fext_id;
} nolx_fext_tree_key_t;
#define NOLX_NOLX_FEXT_TREE_KEY_T_SIZE 16u
#define NOLX_NOLX_FEXT_TREE_KEY_T_OFF_OBJ_ID_AND_TYPE 0u
#define NOLX_NOLX_FEXT_TREE_KEY_T_OFF_FEXT_ID 8u

/* FEXT_TREE value. */
typedef struct {
    uint64_t len_and_flags;
    uint64_t phys_block_num;
} nolx_fext_tree_val_t;
#define NOLX_NOLX_FEXT_TREE_VAL_T_SIZE 16u
#define NOLX_NOLX_FEXT_TREE_VAL_T_OFF_LEN_AND_FLAGS 0u
#define NOLX_NOLX_FEXT_TREE_VAL_T_OFF_PHYS_BLOCK_NUM 8u

/* DIR_STATS value: cached directory contents. */
typedef struct {
    uint64_t num_children;
    uint64_t total_size;
    uint64_t chained_key;
    uint64_t gen_count;
} nolx_dir_stats_val_t;
#define NOLX_NOLX_DIR_STATS_VAL_T_SIZE 32u
#define NOLX_NOLX_DIR_STATS_VAL_T_OFF_NUM_CHILDREN 0u
#define NOLX_NOLX_DIR_STATS_VAL_T_OFF_TOTAL_SIZE 8u
#define NOLX_NOLX_DIR_STATS_VAL_T_OFF_CHAINED_KEY 16u
#define NOLX_NOLX_DIR_STATS_VAL_T_OFF_GEN_COUNT 24u

/* FILE_INFO value: per-file metadata that is not in the inode. */
typedef struct {
    uint64_t doc_id;
    uint64_t hidden_bytes;
    uint32_t hidden_xattr_count;
    uint32_t pad;
} nolx_file_info_val_t;
#define NOLX_NOLX_FILE_INFO_VAL_T_SIZE 24u
#define NOLX_NOLX_FILE_INFO_VAL_T_OFF_DOC_ID 0u
#define NOLX_NOLX_FILE_INFO_VAL_T_OFF_HIDDEN_BYTES 8u
#define NOLX_NOLX_FILE_INFO_VAL_T_OFF_HIDDEN_XATTR_COUNT 16u
#define NOLX_NOLX_FILE_INFO_VAL_T_OFF_PAD 20u

/* SNAP_METADATA value in the snapshot metadata tree. */
typedef struct {
    uint64_t extentref_tree_oid;
    uint64_t snap_xid;
    uint64_t num_files;
    uint64_t num_dirs;
    uint64_t num_symlinks;
    uint64_t num_other_fsobjects;
    int64_t create_time;
    int64_t change_time;
    uint64_t inum;
    uint32_t extentref_tree_subtype;
    uint32_t flags;
    uint16_t name_len;
    uint16_t pad;
} nolx_snap_metadata_val_t;
#define NOLX_NOLX_SNAP_METADATA_VAL_T_SIZE 84u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_EXTENTREF_TREE_OID 0u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_SNAP_XID 8u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_NUM_FILES 16u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_NUM_DIRS 24u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_NUM_SYMLINKS 32u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_NUM_OTHER_FSOBJECTS 40u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_CREATE_TIME 48u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_CHANGE_TIME 56u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_INUM 64u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_EXTENTREF_TREE_SUBTYPE 72u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_FLAGS 76u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_NAME_LEN 80u
#define NOLX_NOLX_SNAP_METADATA_VAL_T_OFF_PAD 82u

#define NOLX_CHECKPOINT_MAP_CAPACITY 126u

#endif /* NOLX_FS_FORMAT_H */
