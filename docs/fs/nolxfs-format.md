# NOLXFS On-Disk Format, version 1

NOLXFS is the native filesystem. Its architecture is deliberately shaped like APFS —
container → checkpoint ring → live superblock → object map → volume → typed B-tree — because
that shape solves the right problems: crash consistency without a journal replay of every
metadata write, cheap snapshots, per-object checksums, and sparse files. The format is
**not** byte-compatible with APFS: every magic, type code, and layout here is NOLX's own,
so no tool can mistake one for the other.

**Single source of truth:** `tools/nolxfs-codegen/nolxfs_format.py`. The Rust structures in
`crates/nolxfs/src/on_disk_gen.rs` and the C header `include/nolxfs/nolxfs_format.h` are
generated from it. `make fscheck` regenerates and fails if the checked-in files differ.
Never hand-edit a generated file.

Conventions: all integers little-endian. `u8[N]` is a fixed byte array. Every structure
below is `#[repr(C)]`, naturally aligned, with no implicit padding — the generator asserts
this by computing offsets itself.

---

## 1. The container

A **container** owns a range of physical blocks and hosts one or more **volumes**. Volumes
share free space; they do not have fixed sizes.

```
block 0        container superblock (magic NCSB)      <- the only fixed location
block 1        container superblock copy (reserved)
blocks N..M    checkpoint descriptor ring
blocks M..P    checkpoint data ring
...            everything else is addressed through the object map
```

### Mount sequence

1. Read block 0. Verify magic and the object checksum.
2. Walk the **checkpoint descriptor ring** to find the newest valid checkpoint map.
3. The checkpoint map resolves the *ephemeral* objects (container superblock copy,
   spaceman, reaper) to current physical addresses.
4. Pick the container superblock with the highest valid `xid`.
5. Read the container **object map** and resolve the volume superblocks listed in
   `fs_oid[]`.
6. For each volume, read its own object map, then its fs-tree root.

A torn write during checkpointing leaves older checkpoint descriptors intact, so step 2
naturally falls back to the previous consistent state. That is the whole crash-recovery
story: **there is no journal replay**, only selection of the newest valid checkpoint.

### Atomic write batches

Metadata writes belonging to one transaction are ordered so that:

1. new object versions are written to fresh blocks;
2. the object map is updated (new omap nodes written COW);
3. the checkpoint descriptor is written **last**, as the commit point.

Steps 1–2 are invisible until step 3 lands, so a crash anywhere before the commit leaves
the previous checkpoint authoritative. NOLXFS additionally supports an atomic-write device
hint; where the device cannot guarantee it, the descriptor block is small enough
(≤ 512 B) to be atomic by convention, and `mkfs` places one descriptor per block.

## 2. Objects

Every on-disk object begins with a 32-byte header.

```c
typedef struct {
    uint64_t checksum;   /* Fletcher-64 over the object with this field = 0 */
    uint64_t oid;        /* object id: virtual, resolved via an object map  */
    uint64_t xid;        /* transaction id in which this version was created */
    uint32_t type;       /* NOLX_OBJ_*                                      */
    uint16_t subtype;    /* NOLX_SUB_*                                      */
    uint16_t flags;      /* NOLX_OBJ_FLAG_*                                 */
} nolx_obj_phys_t;       /* 32 bytes */
```

Objects are addressed three ways:

| Kind | How it is found |
|---|---|
| **physical** | stored at a fixed physical block address (e.g. the container object map) |
| **virtual** | stored anywhere; resolved through an object map by `(oid, xid)` |
| **ephemeral** | virtual, and changes during mount (spaceman, reaper); pinned by the checkpoint map |

The checksum is **Fletcher-64**, computed over the object's bytes in 4-byte words with the
`checksum` field zeroed. A block whose checksum fails is treated as unreadable and, if it
is part of a checkpoint candidate, makes that candidate invalid — which is what makes the
"pick the newest valid checkpoint" rule sound.

### Object types

| Code | Name | Structure |
|---|---|---|
| 0x01 | `CONTAINER_SUPERBLOCK` | `nolx_nx_superblock_t` |
| 0x02 | `BTREE_ROOT` | `nolx_btree_node_phys_t` |
| 0x03 | `BTREE_NODE` | `nolx_btree_node_phys_t` |
| 0x04 | `SPACEMAN` | `nolx_spaceman_phys_t` |
| 0x05 | `SPACEMAN_CAB` | chunk-info address block |
| 0x06 | `SPACEMAN_CIB` | chunk-info block |
| 0x07 | `SPACEMAN_BITMAP` | raw bitmap block |
| 0x08 | `OMAP` | `nolx_omap_phys_t` |
| 0x09 | `CHECKPOINT_MAP` | `nolx_checkpoint_map_phys_t` |
| 0x0a | `VOLUME_SUPERBLOCK` | `nolx_fs_superblock_t` |
| 0x0b | `SNAP_METADATA` | `nolx_snap_metadata_val_t` |
| 0x0c | `REAPER` | `nolx_reaper_phys_t` |
| 0x0d | `REAP_LIST` | `nolx_reap_list_phys_t` |
| 0x0e | `EFI_JUMPSTART` | boot information |
| 0x0f | `GBITMAP` / 0x10 `GBITMAP_BLOCK` | general bitmaps |
| 0x11 | `ER_STATE` | rolling-encryption state |

### Subtypes (B-tree roles)

| Code | Name | Key | Value |
|---|---|---|---|
| 0x01 | `OMAP` | `nolx_omap_key_t` | `nolx_omap_val_t` |
| 0x02 | `FSTREE` | `nolx_j_key_t` and friends | record-dependent |
| 0x03 | `EXTENT_REF_TREE` | `nolx_phys_ext_key_t` | `nolx_phys_ext_val_t` |
| 0x04 | `SNAP_META_TREE` | `uint64_t` xid | `nolx_snap_metadata_val_t` |
| 0x05 | `FEXT_TREE` | `nolx_fext_tree_key_t` | `nolx_fext_tree_val_t` |

## 3. Object maps

An object map is a B-tree keyed by `(oid, xid)` returning the physical block address of
the object version that is live at that `xid`. Lookups are "largest key ≤ (oid, requested
xid)", which is what makes point-in-time views and snapshots a lookup rather than a copy.

```c
typedef struct { uint64_t oid; uint64_t xid; } nolx_omap_key_t;

typedef struct {
    uint32_t flags;   /* NOLX_OMAP_VAL_FLAG_* : saved, missing, no_snapshot */
    uint32_t pad;
    uint64_t paddr;
} nolx_omap_val_t;
```

There are two object maps: a **container** map for container-scope objects, and a
**per-volume** map for volume-scope objects. Keeping them separate means a volume
transaction does not touch container metadata, which is what makes concurrent volume
activity cheap.

## 4. B-trees

```c
typedef struct {
    nolx_obj_phys_t o;
    uint16_t flags;             /* NOLX_BTNODE_* : root, leaf, fixed_kv_size, no_free */
    uint16_t level;             /* 0 = leaf */
    uint32_t key_count;
    uint32_t key_free_list_off;
    uint32_t key_free_list_len;
    uint32_t val_free_list_off;
    uint32_t val_free_list_len;
} nolx_btree_node_phys_t;       /* 56 bytes; table of contents follows immediately */
```

Node layout inside one block:

```
+0     node header (56 bytes)
+56    table of contents: key_count entries of (key_off, key_len, val_off, val_len)
       keys packed forward from the end of the ToC
       ... free space ...
       values packed backward from the end of the block
```

* `key_off` and `val_off` are both the **start** of what they describe, in bytes from the
  end of the node header. Keys grow forward and values backward, so the offsets run toward
  each other and the free space is the gap between them. Recording a key's *end* instead
  shifts every key read back by one entry, which is silent on write and fatal on read.
* Leaf nodes may be **index** nodes or **fixed-key-size** nodes; the flag tells the reader
  which.
* Middle nodes store `(key, child_block_address)`.
* Free lists inside a node let a node be reused after deletion without a rewrite, so a
  node is only split or merged when it crosses the fill thresholds (split > 90 %, merge
  < 50 % with a sibling that can absorb it).
* Nodes are copy-on-write: modifying a node allocates a new block, updates the parent,
  and the old version stays valid for older `xid`s until the reaper frees it.

## 5. Volumes and the fs-tree

```c
/* abbreviated; the generated definition is authoritative */
typedef struct {
    nolx_obj_phys_t o;
    uint32_t magic;                 /* 'NVSB' */
    ...
    uint64_t omap_oid;              /* volume object map */
    uint64_t root_tree_oid;         /* fs-tree root */
    uint64_t extentref_tree_oid;    /* extent reference tree */
    uint64_t snap_meta_tree_oid;    /* snapshot metadata tree */
    ...
    uint8_t  volume_name[256];      /* UTF-8, NUL padded */
} nolx_fs_superblock_t;
```

The **fs-tree** is keyed by `(parent_inode, record_type, name)`; the key's high 4 bits are
the record type and the low 60 bits the inode number:

```c
/* obj_id_and_type = (type << 60) | (inode & 0x0fff_ffff_ffff_ffff) */
typedef struct { uint64_t obj_id_and_type; } nolx_j_key_t;
```

### Record types

| Type | Name | Key | Value |
|---|---|---|---|
| 1 | `SNAP_METADATA` | `nolx_j_key_t` | `nolx_snap_metadata_val_t` |
| 2 | `PHYS_EXTENT` | `nolx_phys_ext_key_t` | `nolx_phys_ext_val_t` |
| 3 | `INODE` | `nolx_j_key_t` | `nolx_inode_t` |
| 4 | `XATTR` | `nolx_xattr_key_t` | xattr value |
| 5 | `SIBLING_LINK` | `nolx_sibling_key_t` | `nolx_sibling_val_t` |
| 6 | `DSTREAM_ID` | `nolx_j_key_t` | `uint64_t` |
| 8 | `FILE_EXTENT` | `nolx_file_extent_key_t` | `nolx_file_extent_val_t` |
| 9 | `DIR_REC` | `nolx_dentry_key_t` | `nolx_direntry_t` |
| 10 | `DIR_STATS` | `nolx_j_key_t` | `nolx_dir_stats_val_t` |
| 11 | `SIBLING_MAP` | `nolx_j_key_t` | `uint64_t` parent id |
| 12 | `FILE_INFO` | `nolx_j_key_t` | `nolx_file_info_val_t` |

Because directory entries sort by `(parent, name)` and the name comparison is byte order
(or case-folded, if the volume was created with `casefold=on`), `readdir` is a range scan
and lookup is one seek.

### Inodes

```c
typedef struct {
    uint64_t parent_id;
    uint64_t private_id;
    int64_t  create_time, modify_time, change_time, access_time;
    uint64_t internal_flags;
    uint32_t nlink;
    uint16_t mode;          /* POSIX mode_t: file type + permission bits */
    uint16_t pad;
    uint32_t owner;         /* uid */
    uint32_t group;         /* gid */
    uint32_t generation;    /* bumped on reuse; defeats stale NFS-style handles */
    uint32_t pad2;
    nolx_data_stream_t data_stream;
} nolx_inode_t;
```

POSIX stores its file type and permission bits in `mode`, so NOLXFS stores `mode` as POSIX
defines it: `S_IFREG`, `S_IFDIR`, `S_IFLNK`, `S_IFIFO`, `S_IFSOCK`, `S_IFBLK`, `S_IFCHR`
in the high nibble, `setuid`/`setgid`/`sticky` and the rwx bits below. No translation layer.

### Data streams and extents

```c
typedef struct {
    nolx_j_hdr_t xattr_jhdr;
    uint64_t alloced_size;
    uint64_t default_crypto_id;
    uint64_t total_bytes_written;
    uint64_t total_bytes_read;
} nolx_data_stream_t;

typedef struct {
    uint64_t len_and_flags;    /* low 56 bits byte length, high 8 bits flags */
    uint64_t phys_block_num;
    uint64_t crypto_id;
} nolx_file_extent_val_t;
```

* Files are described by `FILE_EXTENT` records keyed by logical offset, so a sparse file
  costs nothing and an append is one record.
* **Clones** are multiple `FILE_EXTENT` records pointing at the same physical blocks, with
  the **extent reference tree** tracking how many owners each extent has. The reaper only
  frees an extent whose reference count reaches zero. That is what makes `cp -c`-style
  cloning O(1) and snapshot creation O(metadata).
* **Fast-dev extents** (`FEXT_TREE`) hold frequently-read blocks in a separate region for
  fusion-style devices; optional and absent on plain disks.

## 6. Space manager

```
spaceman (ephemeral object)
  -> chunk-info address blocks (CABs)
     -> chunk-info blocks (CIBs), one per chunk of blocks_per_chunk blocks
        -> allocation bitmap blocks
  -> free-space queue (a B-tree of free ranges, keyed by start block)
```

* Allocation comes from the free-space queue first (so freed ranges are reused and stay
  mergeable), then from bitmap chunks.
* `blocks_per_chunk` is chosen at `mkfs` time from device size; chunk granularity is what
  keeps the bitmap bounded for very large volumes.
* **The reaper** frees blocks lazily. When a transaction drops objects or extents, they go
  on a reap list tagged with the `xid` at which they became garbage; nothing is freed while
  any live snapshot or open point-in-time view can still reach it. This is the mechanism
  that makes COW and snapshots safe to delete.

## 7. Snapshots

* A snapshot pins an `xid`. Creating one writes a `SNAP_METADATA` record into the
  snapshot metadata tree and marks the omap entries reachable at that `xid` with
  `NOLX_OMAP_VAL_FLAG_SAVED`, so the omap keeps older versions instead of overwriting them.
* Mounting at a snapshot is a lookup: resolve omap entries at `xid = snap_xid`, then read
  the fs-tree from there. The volume is read-only in that mode.
* Deleting a snapshot clears the `SAVED` marks that no other snapshot needs; the reaper
  then reclaims the blocks.
* Per-snapshot metadata records carry file/dir/symlink counts and timestamps so
  `ls`-style tooling can describe a snapshot without walking it.

## 8. Encryption and integrity (design, implemented from M8c)

* Per-file encryption keys wrapped in a per-volume **keybag** object; `crypto_id` on each
  extent selects the key and the tweak.
* Rolling-encryption state object tracks in-place re-encryption progress.
* Fletcher-64 detects accidental corruption. For detection of *malicious* modification,
  M13 adds an integrity-metadata object with Merkle hashes over metadata blocks, which is
  also what makes sealed/verified system volumes possible.

## 9. Limits and invariants

| Item | Value |
|---|---|
| block size | 4096 (mkfs may choose 1024/2048/4096/8192/16384, must be a multiple of the device sector size) |
| max file size | 2^63 − 1 bytes (extent lengths are 56-bit byte counts, offsets 64-bit) |
| max filename | 255 UTF-8 bytes (`NAME_MAX`), no embedded NUL or `/` |
| max path | 1024 bytes (`PATH_MAX`) |
| max volume name | 255 UTF-8 bytes |
| max volumes per container | 8 (`fs_oid[]` is fixed at 8) |
| max hard links per file | 2^32 − 1 (`nlink` is `uint32`) |
| inode number space | 60 bits; inode 1 is the volume root, 2 the private directory, 3 the snapshot directory |
| `SYMLOOP_MAX` | 40 |
| object checksum | Fletcher-64, mandatory on every object |

Invariants `fsck.nolxfs` enforces:

1. Every object's checksum is valid and its `type`/`subtype` match what referenced it.
2. The live checkpoint resolves, and every object it references is reachable and valid.
3. `nlink` equals the number of `DIR_REC`s plus sibling links pointing at the inode.
4. Every extent has a nonzero reference count in the extent reference tree; no extent is
   claimed by two unrelated files without a clone relationship.
5. No block is both allocated in a bitmap and present in the free-space queue.
6. Directory `.`/`..` relationships are consistent with sibling links.
7. Timestamps are monotonic: `ctime >= mtime` never required, but `ctime` must not precede
   `create_time`.
8. The volume superblock counters agree with a full walk (reported as fixable drift).

## 10. Reserved inode numbers

| Inode | Role |
|---|---|
| 1 | volume root directory |
| 2 | private directory (per-file private data, orphaned inodes during crash) |
| 3 | snapshot directory (read-only view of snapshots as names) |
| 4–1023 | reserved for future internal use |

## 11. Versioning

`features_supported` / `features_required` / `features_ro_compat` follow the ext4
three-field discipline:

* **required** — the reader must understand these or refuse to mount;
* **ro_compat** — the reader may mount read-only if it does not understand these;
* **supported** — the reader understands these but they need not be present.

`mkfs` refuses to enable a feature the kernel does not advertise, so a downgraded kernel
never sees a volume it cannot handle. The on-disk format version is in the container
superblock's `xid`-independent `features_*` set plus the magic; a v2 format gets a new
magic, not a version field, so misinterpretation is impossible rather than merely
detectable.

## 12. Implementation notes (M8, as built)

Two places where the implementation deliberately differs from the letter of
the sections above; both are internal to the fs-tree encoding and invisible to
readers that walk records generically:

* **Dentry keys.** The generated `nolx_dentry_key_t` orders directory entries
  by `(parent inode, name)` directly. There is no separate name-hash field in
  the on-disk key; the "hash" the ordering was meant to provide is the record
  order itself plus exact-name comparison on lookup. (Case-folded volumes do a
  linear fallback scan when the exact lookup misses.)
* **Extent reference counts.** The generated `nolx_phys_ext_val_t` is 24 bytes
  and carries no count. The extent-reference tree stores the per-physical-block
  reference count as an extra little-endian `u64` appended immediately after
  those 24 bytes in the record value. References are tracked per 4 KiB block,
  not per run, so partial overwrites of a shared run adjust counts exactly.

Mounts are transactional: mutations accumulate against copy-on-write clones of
the trees and become visible to a later mount only at `commit`, which writes
the new checkpoint descriptor last. A volume dropped without committing must
simply be remounted — the uncommitted clone blocks are unreachable garbage
that the reaper (below the oldest snapshot's xid horizon) eventually returns
to the space manager. Snapshot metadata itself lives in the checkpoint state
stream, mirrored into the snapshot-metadata B-tree on every commit.
