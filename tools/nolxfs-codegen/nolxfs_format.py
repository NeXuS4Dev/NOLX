"""NOLXFS on-disk format: single source of truth.

This module declares every on-disk structure in NOLXFS version 1 together with its exact
layout, and computes offsets itself so that implicit padding can never appear.

It is the *only* place the format is declared. `gen.py` emits
`crates/nolxfs/src/on_disk_gen.rs` and `include/nolxfs/nolxfs_format.h` from it, and
`test_layout.py` pins the offsets that matter.

Nothing here knows about Rust or C; it only knows sizes and offsets. The emitters in
`gen.py` do the translation.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# --------------------------------------------------------------------------- primitives

INT_SIZES = {
    "u8": 1, "u16": 2, "u32": 4, "u64": 8,
    "i8": 1, "i16": 2, "i32": 4, "i64": 8,
}

INT_ALIGN = INT_SIZES


@dataclass(frozen=True)
class Field:
    name: str
    ctype: str
    count: int = 1
    doc: str = ""

    def __post_init__(self) -> None:
        if self.ctype not in INT_SIZES:
            raise ValueError(f"{self.name}: unknown primitive {self.ctype!r}")
        if self.count < 1:
            raise ValueError(f"{self.name}: count must be >= 1")

    @property
    def align(self) -> int:
        return INT_ALIGN[self.ctype]

    @property
    def size(self) -> int:
        return INT_SIZES[self.ctype] * self.count

    @property
    def is_array(self) -> bool:
        return self.count > 1


@dataclass
class Struct:
    name: str
    fields: list[Field] = field(default_factory=list)
    total_size: int | None = None
    doc: str = ""
    # Emitted as a fixed-size object (padded to total_size) or as a variable-length
    # header/record with no trailing padding.
    variable_length: bool = False

    def layout(self) -> tuple[list[tuple[Field, int]], int]:
        """Return [(field, offset), ...] and the packed size, asserting no overlap."""
        offset = 0
        out: list[tuple[Field, int]] = []
        for f in self.fields:
            misalign = offset % f.align
            if misalign:
                raise ValueError(
                    f"{self.name}.{f.name}: offset {offset} is not {f.align}-byte aligned; "
                    "add explicit padding so the layout is unambiguous"
                )
            out.append((f, offset))
            offset += f.size
        return out, offset

    def padded_size(self) -> int:
        _, packed = self.layout()
        if self.variable_length or self.total_size is None:
            return packed
        if packed > self.total_size:
            raise ValueError(
                f"{self.name}: fields occupy {packed} bytes, exceeds total_size "
                f"{self.total_size}"
            )
        return self.total_size

    def tail_padding(self) -> int:
        return self.padded_size() - self.layout()[1]

    def offset_of(self, name: str) -> int:
        fields, _packed = self.layout()
        for f, off in fields:
            if f.name == name:
                return off
        raise KeyError(f"{self.name}: no field {name!r}")


# --------------------------------------------------------------------------- constants

FORMAT_VERSION = 1

# Four-byte magics, stored little-endian as u32 and also spelled as bytes here so the
# generated code can carry both the integer and a human-readable form.
MAGICS = {
    "CONTAINER_SUPERBLOCK": ("NCSB", "NOLX container superblock"),
    "VOLUME_SUPERBLOCK": ("NVSB", "NOLX volume superblock"),
    "OMAP": ("NOMP", "object map"),
    "CHECKPOINT_MAP": ("NCKP", "checkpoint map"),
    "SPACEMAN": ("NSPM", "space manager"),
}

# nolx_obj_phys_t.type
OBJECT_TYPES = [
    (0x01, "CONTAINER_SUPERBLOCK"),
    (0x02, "BTREE_ROOT"),
    (0x03, "BTREE_NODE"),
    (0x04, "SPACEMAN"),
    (0x05, "SPACEMAN_CAB"),
    (0x06, "SPACEMAN_CIB"),
    (0x07, "SPACEMAN_BITMAP"),
    (0x08, "OMAP"),
    (0x09, "CHECKPOINT_MAP"),
    (0x0A, "VOLUME_SUPERBLOCK"),
    (0x0B, "SNAP_METADATA"),
    (0x0C, "REAPER"),
    (0x0D, "REAP_LIST"),
    (0x0E, "EFI_JUMPSTART"),
    (0x0F, "GBITMAP"),
    (0x10, "GBITMAP_BLOCK"),
    (0x11, "ER_STATE"),
]

# nolx_obj_phys_t.subtype -- the role of a B-tree.
SUBTYPES = [
    (0x00, "NONE"),
    (0x01, "OMAP"),
    (0x02, "FSTREE"),
    (0x03, "EXTENT_REF_TREE"),
    (0x04, "SNAP_META_TREE"),
    (0x05, "FEXT_TREE"),
]

# High 4 bits of nolx_j_key_t.obj_id_and_type.
RECORD_TYPES = [
    (1, "SNAP_METADATA"),
    (2, "PHYS_EXTENT"),
    (3, "INODE"),
    (4, "XATTR"),
    (5, "SIBLING_LINK"),
    (6, "DSTREAM_ID"),
    (7, "CRYPTO_STATE"),
    (8, "FILE_EXTENT"),
    (9, "DIR_REC"),
    (10, "DIR_STATS"),
    (11, "SIBLING_MAP"),
    (12, "FILE_INFO"),
]

OBJ_FLAGS = [
    (0x0001, "NO_CKSUM", "object body is not covered by the checksum (unused in v1)"),
    (0x0002, "ENCRYPTED", "object body is encrypted at rest"),
    (0x0004, "COMPRESSED", "object body is compressed"),
    (0x0008, "EPHEMERAL", "object changes during mount; pinned by the checkpoint map"),
]

OMAP_VAL_FLAGS = [
    (0x0000_0001, "SAVED", "reachable from a snapshot; do not overwrite"),
    (0x0000_0002, "MISSING", "known absent; a lookup must fail rather than fall back"),
    (0x0000_0004, "NO_SNAPSHOT", "never visible in a point-in-time view"),
]

BTNODE_FLAGS = [
    (0x0001, "ROOT", "this node is the root of its tree"),
    (0x0002, "LEAF", "this node holds values rather than child pointers"),
    (0x0004, "FIXED_KV_SIZE", "all keys and all values have one fixed size"),
    (0x0008, "NO_FREE", "the node free lists are empty; do not scan them"),
    (0x0010, "INDEX", "middle node in an index tree"),
]

NX_FEATURES_REQUIRED = [
    (1 << 0, "V1_BASE", "the version 1 base format"),
]
NX_FEATURES_RO_COMPAT = [
    (1 << 0, "SEALED_VOLUME", "verified read-only system volume"),
]
NX_FEATURES_SUPPORTED = [
    (1 << 0, "CASEFOLD", "case-insensitive, case-preserving names"),
    (1 << 1, "NORMALIZATION_INSENSITIVE", "names compared without Unicode normalization"),
    (1 << 2, "ENCRYPTION", "per-file encryption with a volume keybag"),
    (1 << 3, "FEXT", "fast-dev extent tree present"),
    (1 << 4, "SNAPSHOTS", "snapshot metadata tree present"),
    (1 << 5, "INTEGRITY_META", "Merkle integrity metadata present"),
]

INODE_FLAGS = [
    (1 << 0, "SUID_SAVED", "setuid bit was cleared on write by a non-owner"),
    (1 << 1, "SGID_SAVED", "setgid bit was cleared on write by a non-owner"),
    (1 << 2, "CLONED", "file has one or more clones"),
    (1 << 3, "SPARSE", "data stream contains holes"),
    (1 << 4, "COMPRESSED", "data stream is compressed"),
    (1 << 5, "HIDDEN", "hidden from directory listings"),
    (1 << 6, "FAST_DOC_ID", "document id is allocated"),
]

LIMITS = [
    ("MAX_VOLUMES_PER_CONTAINER", 8, "fixed size of fs_oid[] in the container superblock"),
    ("MAX_NAME_LEN", 255, "POSIX NAME_MAX"),
    ("MAX_PATH_LEN", 1024, "POSIX PATH_MAX"),
    ("MAX_VOLUME_NAME_LEN", 255, "longest volume name"),
    ("SYMLOOP_MAX", 40, "symlink resolution depth, POSIX minimum is 8"),
    ("MAX_EXTENT_LEN", (1 << 56) - 1, "extent lengths are 56-bit byte counts"),
    ("DEFAULT_BLOCK_SIZE", 4096, "block size chosen by mkfs unless told otherwise"),
    ("MIN_BLOCK_SIZE", 1024, "smallest permitted block size"),
    ("MAX_BLOCK_SIZE", 16384, "largest permitted block size"),
    ("MAX_CHECKPOINT_DESCRIPTORS", 128, "descriptors in the checkpoint ring"),
    ("ROOT_INO", 1, "volume root directory"),
    ("PRIVATE_DIR_INO", 2, "private directory / orphan holding pen"),
    ("SNAPSHOT_DIR_INO", 3, "read-only view of the volume's snapshots"),
    ("FIRST_USER_INO", 1024, "first inode number available to users"),
    ("OBJ_ID_BITS", 60, "inode numbers occupy the low 60 bits of a j-key"),
    ("BTREE_SPLIT_FILL_PCT", 90, "split a node above this fill"),
    ("BTREE_MERGE_FILL_PCT", 50, "merge a node below this fill"),
]


def magic_u32(spell: str) -> int:
    """The u32 you get by reading a 4-byte magic stored as ASCII bytes, little-endian."""
    b = spell.encode("ascii")
    if len(b) != 4:
        raise ValueError(f"magic must be exactly 4 ASCII bytes, got {spell!r}")
    return b[0] | (b[1] << 8) | (b[2] << 16) | (b[3] << 24)


# --------------------------------------------------------------------------- structures

def _structs() -> list[Struct]:
    S: list[Struct] = []

    # -- object header ------------------------------------------------------
    S.append(Struct(
        "nolx_obj_phys_t",
        doc="Header of every on-disk object. 32 bytes, no padding.",
        fields=[
            Field("checksum", "u64", doc="Fletcher-64 over the object with this field = 0"),
            Field("oid", "u64", doc="object id, resolved through an object map"),
            Field("xid", "u64", doc="transaction id of the version that created this"),
            Field("type", "u32", doc="NOLX_OBJ_*"),
            Field("subtype", "u16", doc="NOLX_SUB_*"),
            Field("flags", "u16", doc="NOLX_OBJ_FLAG_*"),
        ],
    ))

    # -- container superblock ----------------------------------------------
    S.append(Struct(
        "nolx_nx_superblock_t",
        doc="Container superblock (magic NCSB). One block.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("magic", "u32", doc="'NCSB'"),
            Field("block_size", "u32"),
            Field("block_count", "u64"),
            Field("features_supported", "u64"),
            Field("features_required", "u64"),
            Field("features_ro_compat", "u64"),
            Field("uuid", "u8", 16),
            Field("next_oid", "u64"),
            Field("next_xid", "u64"),
            Field("xp_desc_blocks", "u32", doc="blocks reserved for checkpoint descriptors"),
            Field("xp_data_blocks", "u32", doc="blocks reserved for checkpoint data"),
            Field("xp_desc_base", "u64"),
            Field("xp_data_base", "u64"),
            Field("xp_desc_index", "u32", doc="ring index of the newest valid descriptor"),
            Field("xp_desc_len", "u32"),
            Field("xp_desc_next", "u32"),
            Field("xp_data_index", "u32"),
            Field("xp_data_len", "u32"),
            Field("xp_data_next", "u32"),
            Field("fs_oid_count", "u32"),
            Field("pad0", "u32"),
            Field("fs_oid", "u64", 8, doc="object ids of the volumes in this container"),
            Field("counters", "u64", 32, doc="NOLX_COUNTER_* statistics"),
            Field("omap_oid", "u64", doc="physical object id of the container object map"),
            Field("reaper_oid", "u64", doc="ephemeral reaper object"),
            Field("flags", "u64"),
            Field("efi_jumpstart_oid", "u64"),
            Field("fusion_uuid", "u8", 16),
            Field("blocked_out_start", "u64", doc="first block of a bad region"),
            Field("blocked_out_count", "u64"),
            Field("keybag_uuid", "u8", 16),
            Field("keybag_oid", "u64"),
            Field("ephemeral_info", "u64", 4, doc="paddr/len of the ephemeral info block"),
            Field("test_oid", "u64", doc="object used by fsck for write testing"),
        ],
    ))

    # -- checkpoint map -----------------------------------------------------
    S.append(Struct(
        "nolx_checkpoint_map_entry_t",
        doc="One entry of a checkpoint map: where an ephemeral object lives now.",
        variable_length=True,
        fields=[
            Field("type", "u32"),
            Field("subtype", "u32"),
            Field("block_size", "u32"),
            Field("pad", "u32"),
            Field("oid", "u64"),
            Field("paddr", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_checkpoint_map_phys_t",
        doc="Checkpoint map header. Entries follow immediately.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("magic", "u32", doc="'NCKP'"),
            Field("pad", "u32"),
            Field("num_entries", "u32"),
            Field("pad2", "u32"),
        ],
    ))

    # -- object map ---------------------------------------------------------
    S.append(Struct(
        "nolx_omap_key_t",
        doc="Object map key: resolve (oid, xid) to a physical address.",
        variable_length=True,
        fields=[Field("oid", "u64"), Field("xid", "u64")],
    ))
    S.append(Struct(
        "nolx_omap_val_t",
        doc="Object map value.",
        variable_length=True,
        fields=[
            Field("flags", "u32", doc="NOLX_OMAP_VAL_FLAG_*"),
            Field("pad", "u32"),
            Field("paddr", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_omap_phys_t",
        doc="Object map header; points at the root of the omap B-tree.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("magic", "u32", doc="'NOMP'"),
            Field("flags", "u32"),
            Field("num_snapshots", "u32"),
            Field("tree_type", "u32"),
            Field("tree_subtype", "u32"),
            Field("pad", "u32"),
            Field("tree_oid", "u64"),
            Field("snapshot_tree_oid", "u64"),
            Field("most_recent_snap_xid", "u64"),
        ],
    ))

    # -- volume superblock --------------------------------------------------
    S.append(Struct(
        "nolx_fs_superblock_t",
        doc="Volume superblock (magic NVSB). One block.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("magic", "u32", doc="'NVSB'"),
            Field("block_size", "u32", doc="copied from the container, validated on mount"),
            Field("features_supported", "u64"),
            Field("features_required", "u64"),
            Field("features_ro_compat", "u64"),
            Field("reserve_block_count", "u64"),
            Field("quota_block_count", "u64"),
            Field("alloc_count", "u64"),
            Field("meta_crypto_key", "u64", 5),
            Field("root_tree_type", "u32"),
            Field("root_tree_subtype", "u32"),
            Field("extentref_tree_type", "u32"),
            Field("extentref_tree_subtype", "u32"),
            Field("snap_meta_tree_type", "u32"),
            Field("snap_meta_tree_subtype", "u32"),
            Field("omap_oid", "u64"),
            Field("root_tree_oid", "u64"),
            Field("extentref_tree_oid", "u64"),
            Field("snap_meta_tree_oid", "u64"),
            Field("revert_to_xid", "u64"),
            Field("revert_to_sb_oid", "u64"),
            Field("create_time", "i64"),
            Field("modify_time", "i64"),
            Field("change_time", "i64"),
            Field("backup_time", "i64"),
            Field("volume_uuid", "u8", 16),
            Field("next_ident", "u64"),
            Field("num_files", "u64"),
            Field("num_directories", "u64"),
            Field("num_symlinks", "u64"),
            Field("num_other_fsobjects", "u64"),
            Field("num_snapshots", "u64"),
            Field("total_blocks_alloced", "u64"),
            Field("total_blocks_freed", "u64"),
            Field("keybag_oid", "u64"),
            Field("flags", "u64"),
            Field("volume_name", "u8", 256, doc="UTF-8, NUL padded"),
            Field("next_doc_id", "u32"),
            Field("role", "u16"),
            Field("pad", "u16"),
            Field("fs_reserve_block_count", "u64"),
            Field("used_by_owner", "u64"),
        ],
    ))

    # -- B-tree -------------------------------------------------------------
    S.append(Struct(
        "nolx_btree_node_phys_t",
        doc="B-tree node header. The table of contents follows immediately.",
        variable_length=True,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("flags", "u16", doc="NOLX_BTNODE_*"),
            Field("level", "u16", doc="0 = leaf"),
            Field("key_count", "u32"),
            Field("key_free_list_off", "u32"),
            Field("key_free_list_len", "u32"),
            Field("val_free_list_off", "u32"),
            Field("val_free_list_len", "u32"),
        ],
    ))
    S.append(Struct(
        "nolx_btree_node_toc_entry_t",
        doc="Table-of-contents entry: offsets are relative to the end of this header.",
        variable_length=True,
        fields=[
            Field("key_off", "u16"),
            Field("key_len", "u16"),
            Field("val_off", "u16"),
            Field("val_len", "u16"),
        ],
    ))

    # -- space manager ------------------------------------------------------
    S.append(Struct(
        "nolx_chunk_info_t",
        doc="Per-chunk allocation state.",
        variable_length=True,
        fields=[
            Field("xid", "u64"),
            Field("addr", "u64"),
            Field("free_count", "u32"),
            Field("bitmap_len", "u32"),
            Field("bitmap_addr", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_spaceman_phys_t",
        doc="Space manager (ephemeral object).",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("magic", "u32", doc="'NSPM'"),
            Field("block_size", "u32"),
            Field("blocks_per_chunk", "u32"),
            Field("chunks_per_cib", "u32"),
            Field("cib_count", "u32"),
            Field("pad", "u32"),
            Field("bm_size_per_check", "u64"),
            Field("bm_block_count", "u64"),
            Field("blocks_reserved", "u64"),
            Field("chunks_reserved", "u64"),
            Field("free_count", "u64"),
            Field("flags", "u32"),
            Field("pad2", "u32"),
            Field("ip_block_count", "u64"),
            Field("ip_bm_size_per_check", "u64"),
            Field("ip_bm_block_count", "u64"),
            Field("ip_bm_free_head", "u64"),
            Field("ip_bm_free_tail", "u64"),
            Field("ip_bm_xid_offset", "u64"),
            Field("ip_bm_block_count_offset", "u64"),
            Field("cib_addr_offset", "u64", doc="byte offset of the chunk-info address array"),
            Field("cab_addr_offset", "u64"),
            Field("free_queue_oid", "u64", doc="B-tree of free ranges, keyed by start block"),
        ],
    ))

    # -- reaper -------------------------------------------------------------
    S.append(Struct(
        "nolx_reaper_phys_t",
        doc="Reaper: deferred free list for COW'd and unlinked objects.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("flags", "u32"),
            Field("pad", "u32"),
            Field("oid", "u64"),
            Field("xid", "u64"),
            Field("nr", "u64", doc="next reap-list id"),
            Field("first", "u64"),
            Field("last", "u64"),
            Field("max_record_count", "u64"),
            Field("record_count", "u64"),
            Field("head", "u64"),
            Field("tail", "u64"),
            Field("fwd", "u64"),
            Field("prev", "u64"),
            Field("last_paddr", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_reap_list_phys_t",
        doc="A node of the reaper list.",
        total_size=4096,
        fields=[
            Field("o", "u8", 32, doc="nolx_obj_phys_t"),
            Field("flags", "u32"),
            Field("pad", "u32"),
            Field("max_records", "u32"),
            Field("record_count", "u32"),
            Field("first", "u64"),
            Field("last", "u64"),
            Field("free_head", "u64"),
            Field("free_tail", "u64"),
            Field("block_count", "u64"),
        ],
    ))

    # -- fs-tree keys and values -------------------------------------------
    S.append(Struct(
        "nolx_j_key_t",
        doc="Generic fs-tree key. High 4 bits are the record type, low 60 the inode.",
        variable_length=True,
        fields=[Field("obj_id_and_type", "u64")],
    ))
    S.append(Struct(
        "nolx_j_hdr_t",
        doc="Header of an embedded per-inode B-tree of xattrs.",
        variable_length=True,
        fields=[
            Field("num_keys", "u32"),
            Field("key_size", "u32"),
            Field("val_size", "u32"),
            Field("free_space", "u32"),
        ],
    ))
    S.append(Struct(
        "nolx_data_stream_t",
        doc="Inline description of a file's data, embedded in the inode record.",
        variable_length=True,
        fields=[
            Field("xattr_jhdr", "u8", 16, doc="nolx_j_hdr_t"),
            Field("alloced_size", "u64"),
            Field("default_crypto_id", "u64"),
            Field("total_bytes_written", "u64"),
            Field("total_bytes_read", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_inode_t",
        doc="INODE record value. POSIX semantics live here, untranslated.",
        variable_length=True,
        fields=[
            Field("parent_id", "u64"),
            Field("private_id", "u64"),
            Field("create_time", "i64"),
            Field("modify_time", "i64"),
            Field("change_time", "i64"),
            Field("access_time", "i64"),
            Field("internal_flags", "u64", doc="NOLX_INODE_FLAG_*"),
            Field("nlink", "u32"),
            Field("mode", "u16", doc="POSIX mode_t: S_IF* and permission bits"),
            Field("pad", "u16"),
            Field("owner", "u32", doc="uid"),
            Field("group", "u32", doc="gid"),
            Field("generation", "u32"),
            Field("pad2", "u32"),
            Field("data_stream", "u8", 48, doc="nolx_data_stream_t"),
        ],
    ))
    S.append(Struct(
        "nolx_dentry_key_t",
        doc="DIR_REC key: inode, then the name.",
        variable_length=True,
        fields=[
            Field("obj_id_and_type", "u64"),
            Field("name_len", "u16"),
            Field("pad", "u16"),
        ],
    ))
    S.append(Struct(
        "nolx_direntry_t",
        doc="DIR_REC value. The name follows the fixed part.",
        variable_length=True,
        fields=[
            Field("ino", "u64"),
            Field("name_len", "u16"),
            Field("flags", "u16", doc="NOLX_DREC_FLAG_*"),
            Field("nlen", "u16", doc="stored name length in the value"),
            Field("pad", "u16"),
        ],
    ))
    S.append(Struct(
        "nolx_sibling_key_t",
        doc="SIBLING_LINK key.",
        variable_length=True,
        fields=[
            Field("obj_id_and_type", "u64"),
            Field("sibling_id", "u64"),
            Field("name_len", "u16"),
            Field("pad", "u16"),
        ],
    ))
    S.append(Struct(
        "nolx_sibling_val_t",
        doc="SIBLING_LINK value: enough to reach the parent by name.",
        variable_length=True,
        fields=[
            Field("parent_id", "u64"),
            Field("nlen", "u16"),
            Field("type", "u16", doc="POSIX file type of the parent"),
        ],
    ))
    S.append(Struct(
        "nolx_xattr_key_t",
        doc="XATTR key.",
        variable_length=True,
        fields=[
            Field("obj_id_and_type", "u64"),
            Field("name_len", "u16"),
            Field("pad", "u16"),
        ],
    ))
    S.append(Struct(
        "nolx_phys_ext_key_t",
        doc="PHYS_EXTENT key. Logical address occupies the high bits above the inode.",
        variable_length=True,
        fields=[Field("obj_id_and_type", "u64")],
    ))
    S.append(Struct(
        "nolx_phys_ext_val_t",
        doc="PHYS_EXTENT value: a range of blocks and who owns it.",
        variable_length=True,
        fields=[
            Field("len_and_kind", "u64", doc="low 56 bits byte length, high 8 the kind"),
            Field("phys_block_num", "u64"),
            Field("crypto_id", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_file_extent_key_t",
        doc="FILE_EXTENT key: inode, then logical byte offset.",
        variable_length=True,
        fields=[
            Field("obj_id_and_type", "u64"),
            Field("logical_addr", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_file_extent_val_t",
        doc="FILE_EXTENT value.",
        variable_length=True,
        fields=[
            Field("len_and_flags", "u64", doc="low 56 bits byte length, high 8 flags"),
            Field("phys_block_num", "u64"),
            Field("crypto_id", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_fext_tree_key_t",
        doc="FEXT_TREE key: inode and the extent number.",
        variable_length=True,
        fields=[
            Field("obj_id_and_type", "u64"),
            Field("fext_id", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_fext_tree_val_t",
        doc="FEXT_TREE value.",
        variable_length=True,
        fields=[
            Field("len_and_flags", "u64"),
            Field("phys_block_num", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_dir_stats_val_t",
        doc="DIR_STATS value: cached directory contents.",
        variable_length=True,
        fields=[
            Field("num_children", "u64"),
            Field("total_size", "u64"),
            Field("chained_key", "u64"),
            Field("gen_count", "u64"),
        ],
    ))
    S.append(Struct(
        "nolx_file_info_val_t",
        doc="FILE_INFO value: per-file metadata that is not in the inode.",
        variable_length=True,
        fields=[
            Field("doc_id", "u64"),
            Field("hidden_bytes", "u64"),
            Field("hidden_xattr_count", "u32"),
            Field("pad", "u32"),
        ],
    ))
    S.append(Struct(
        "nolx_snap_metadata_val_t",
        doc="SNAP_METADATA value in the snapshot metadata tree.",
        variable_length=True,
        fields=[
            Field("extentref_tree_oid", "u64"),
            Field("snap_xid", "u64"),
            Field("num_files", "u64"),
            Field("num_dirs", "u64"),
            Field("num_symlinks", "u64"),
            Field("num_other_fsobjects", "u64"),
            Field("create_time", "i64"),
            Field("change_time", "i64"),
            Field("inum", "u64"),
            Field("extentref_tree_subtype", "u32"),
            Field("flags", "u32"),
            Field("name_len", "u16"),
            Field("pad", "u16"),
        ],
    ))

    return S


STRUCTS: list[Struct] = _structs()
STRUCT_BY_NAME: dict[str, Struct] = {s.name: s for s in STRUCTS}


def container_superblock_capacity() -> int:
    """How many checkpoint-map entries fit in one checkpoint map block.

    Entries follow the header immediately, so the header's *packed* size is what matters
    (its padded size is a whole block).
    """
    hdr = STRUCT_BY_NAME["nolx_checkpoint_map_phys_t"].layout()[1]
    entry = STRUCT_BY_NAME["nolx_checkpoint_map_entry_t"].layout()[1]
    block = STRUCT_BY_NAME["nolx_nx_superblock_t"].padded_size()
    return (block - hdr) // entry
