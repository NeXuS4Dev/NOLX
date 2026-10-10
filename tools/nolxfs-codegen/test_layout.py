#!/usr/bin/env python3
"""Layout invariants for the NOLXFS on-disk format.

These run on any machine with Python -- no Rust toolchain, no QEMU. They are the T1 check
that the format declaration in `nolxfs_format.py` is self-consistent and that the fields
other code depends on are where the documentation says they are.

    python3 tools/nolxfs-codegen/test_layout.py
    python3 -m unittest discover -s tools/nolxfs-codegen -p 'test_*.py'
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import nolxfs_format as F  # noqa: E402


class TestPrimitives(unittest.TestCase):
    def test_all_fields_use_known_primitives(self):
        for s in F.STRUCTS:
            for f in s.fields:
                self.assertIn(f.ctype, F.INT_SIZES, f"{s.name}.{f.name}")

    def test_struct_names_are_unique(self):
        names = [s.name for s in F.STRUCTS]
        self.assertEqual(len(names), len(set(names)))


class TestAlignment(unittest.TestCase):
    def test_no_implicit_padding(self):
        """layout() raises if a field would need implicit padding, so simply computing
        the layout of every struct proves the format has none."""
        for s in F.STRUCTS:
            fields, packed = s.layout()
            prev_end = 0
            for f, off in fields:
                self.assertGreaterEqual(off, prev_end, f"{s.name}.{f.name} overlaps")
                self.assertEqual(off % f.align, 0, f"{s.name}.{f.name} misaligned")
                prev_end = off + f.size
            self.assertEqual(packed, prev_end)

    def test_fixed_size_structs_fit(self):
        for s in F.STRUCTS:
            if s.total_size is None:
                continue
            self.assertLessEqual(s.layout()[1], s.total_size, s.name)
            self.assertEqual(s.padded_size(), s.total_size, s.name)

    def test_trailing_padding_never_negative(self):
        for s in F.STRUCTS:
            self.assertGreaterEqual(s.tail_padding(), 0, s.name)


class TestCriticalOffsets(unittest.TestCase):
    """Pinned offsets. If someone reorders a field, one of these fails, which is the
    point: the on-disk format must not move by accident."""

    def test_object_header(self):
        s = F.STRUCT_BY_NAME["nolx_obj_phys_t"]
        self.assertEqual(s.padded_size(), 32)
        self.assertEqual(s.offset_of("checksum"), 0)
        self.assertEqual(s.offset_of("oid"), 8)
        self.assertEqual(s.offset_of("xid"), 16)
        self.assertEqual(s.offset_of("type"), 24)
        self.assertEqual(s.offset_of("subtype"), 28)
        self.assertEqual(s.offset_of("flags"), 30)

    def test_btree_node_header_is_56_bytes(self):
        s = F.STRUCT_BY_NAME["nolx_btree_node_phys_t"]
        self.assertEqual(s.layout()[1], 56)
        self.assertEqual(s.offset_of("key_count"), 36)
        self.assertEqual(s.offset_of("val_free_list_len"), 52)

    def test_btree_toc_entry(self):
        s = F.STRUCT_BY_NAME["nolx_btree_node_toc_entry_t"]
        self.assertEqual(s.layout()[1], 8)
        self.assertEqual(s.offset_of("val_len"), 6)

    def test_container_superblock(self):
        s = F.STRUCT_BY_NAME["nolx_nx_superblock_t"]
        self.assertEqual(s.padded_size(), 4096)
        self.assertEqual(s.offset_of("o"), 0)
        self.assertEqual(s.offset_of("magic"), 32)
        self.assertEqual(s.offset_of("block_size"), 36)
        self.assertEqual(s.offset_of("block_count"), 40)
        self.assertEqual(s.offset_of("fs_oid"), 160)
        self.assertEqual(s.offset_of("counters"), 224)
        self.assertEqual(s.offset_of("omap_oid"), 480)
        self.assertEqual(s.offset_of("test_oid"), 600)

    def test_volume_superblock(self):
        s = F.STRUCT_BY_NAME["nolx_fs_superblock_t"]
        self.assertEqual(s.padded_size(), 4096)
        self.assertEqual(s.offset_of("magic"), 32)
        self.assertEqual(s.offset_of("omap_oid"), 152)
        self.assertEqual(s.offset_of("root_tree_oid"), 160)
        self.assertEqual(s.offset_of("extentref_tree_oid"), 168)
        self.assertEqual(s.offset_of("snap_meta_tree_oid"), 176)
        self.assertEqual(s.offset_of("volume_name"), 328)
        self.assertEqual(F.STRUCT_BY_NAME["nolx_fs_superblock_t"].fields[
            [f.name for f in F.STRUCT_BY_NAME["nolx_fs_superblock_t"].fields]
            .index("volume_name")].count, 256)

    def test_checkpoint_map(self):
        hdr = F.STRUCT_BY_NAME["nolx_checkpoint_map_phys_t"]
        ent = F.STRUCT_BY_NAME["nolx_checkpoint_map_entry_t"]
        self.assertEqual(hdr.layout()[1], 48)
        self.assertEqual(hdr.offset_of("num_entries"), 40)
        # type, subtype, block_size, pad (4x u32) then oid and paddr.
        self.assertEqual(ent.layout()[1], 32)
        self.assertEqual(ent.offset_of("oid"), 16)
        self.assertEqual(ent.offset_of("paddr"), 24)
        self.assertEqual(F.container_superblock_capacity(), (4096 - 48) // 32)
        self.assertGreater(F.container_superblock_capacity(), 0)

    def test_omap(self):
        self.assertEqual(F.STRUCT_BY_NAME["nolx_omap_key_t"].layout()[1], 16)
        v = F.STRUCT_BY_NAME["nolx_omap_val_t"]
        self.assertEqual(v.layout()[1], 16)
        self.assertEqual(v.offset_of("paddr"), 8)
        o = F.STRUCT_BY_NAME["nolx_omap_phys_t"]
        self.assertEqual(o.padded_size(), 4096)
        self.assertEqual(o.offset_of("tree_oid"), 56)

    def test_inode_carries_posix_mode_and_ids(self):
        s = F.STRUCT_BY_NAME["nolx_inode_t"]
        self.assertEqual(s.offset_of("nlink"), 56)
        self.assertEqual(s.offset_of("mode"), 60)
        self.assertEqual(s.offset_of("owner"), 64)
        self.assertEqual(s.offset_of("group"), 68)
        self.assertEqual(s.offset_of("generation"), 72)
        self.assertEqual(s.offset_of("data_stream"), 80)

    def test_embedded_data_stream_matches_its_slot(self):
        """nolx_inode_t embeds the data stream as a byte array; its length must equal
        the real struct's size or the inode layout silently lies."""
        inode = F.STRUCT_BY_NAME["nolx_inode_t"]
        slot = next(f for f in inode.fields if f.name == "data_stream")
        ds = F.STRUCT_BY_NAME["nolx_data_stream_t"]
        self.assertEqual(slot.ctype, "u8")
        self.assertEqual(slot.count, ds.layout()[1])

    def test_embedded_jhdr_matches_its_slot(self):
        ds = F.STRUCT_BY_NAME["nolx_data_stream_t"]
        slot = next(f for f in ds.fields if f.name == "xattr_jhdr")
        self.assertEqual(slot.count, F.STRUCT_BY_NAME["nolx_j_hdr_t"].layout()[1])

    def test_embedded_obj_headers_match(self):
        """Every fixed-size object embeds the 32-byte header as u8[32]."""
        obj = F.STRUCT_BY_NAME["nolx_obj_phys_t"].layout()[1]
        for name in ("nolx_nx_superblock_t", "nolx_fs_superblock_t",
                     "nolx_omap_phys_t", "nolx_checkpoint_map_phys_t",
                     "nolx_btree_node_phys_t", "nolx_spaceman_phys_t",
                     "nolx_reaper_phys_t", "nolx_reap_list_phys_t"):
            s = F.STRUCT_BY_NAME[name]
            self.assertEqual(s.fields[0].name, "o", name)
            self.assertEqual(s.fields[0].count, obj, name)

    def test_extent_values(self):
        for name in ("nolx_phys_ext_val_t", "nolx_file_extent_val_t"):
            self.assertEqual(F.STRUCT_BY_NAME[name].layout()[1], 24, name)
        self.assertEqual(F.STRUCT_BY_NAME["nolx_file_extent_key_t"].layout()[1], 16)


class TestConstants(unittest.TestCase):
    def test_magics_are_four_ascii_bytes(self):
        for name, (spell, _doc) in F.MAGICS.items():
            self.assertEqual(len(spell), 4, name)
            self.assertTrue(spell.isascii(), name)
            self.assertTrue(all(32 <= ord(ch) < 127 for ch in spell), name)

    def test_magics_are_distinct(self):
        values = [F.magic_u32(s) for s, _ in F.MAGICS.values()]
        self.assertEqual(len(values), len(set(values)))

    def test_magic_encoding_is_little_endian_ascii(self):
        # 'NCSB' as bytes read as a little-endian u32.
        self.assertEqual(F.magic_u32("NCSB"), 0x4253434E)
        self.assertEqual(F.magic_u32("NVSB"), 0x4253564E)

    def test_object_type_codes_unique_and_nonzero(self):
        codes = [c for c, _ in F.OBJECT_TYPES]
        self.assertEqual(len(codes), len(set(codes)))
        self.assertTrue(all(c != 0 for c in codes))

    def test_subtype_codes_unique(self):
        codes = [c for c, _ in F.SUBTYPES]
        self.assertEqual(len(codes), len(set(codes)))

    def test_record_types_unique_and_fit_in_four_bits(self):
        codes = [c for c, _ in F.RECORD_TYPES]
        self.assertEqual(len(codes), len(set(codes)))
        self.assertTrue(all(0 < c < 16 for c in codes),
                        "record types occupy the high 4 bits of a j-key")

    def test_flag_bits_are_single_bits_and_unique(self):
        for table in (F.OBJ_FLAGS, F.OMAP_VAL_FLAGS, F.NX_FEATURES_REQUIRED,
                      F.NX_FEATURES_RO_COMPAT, F.NX_FEATURES_SUPPORTED, F.INODE_FLAGS):
            values = [v for v, *_ in table]
            self.assertEqual(len(values), len(set(values)))
            for v in values:
                self.assertTrue(v & (v - 1) == 0, f"{v:#x} is not a single bit")

    def test_btnode_flags_are_distinct_from_each_other(self):
        values = [v for v, *_ in F.BTNODE_FLAGS]
        self.assertEqual(len(values), len(set(values)))
        for v in values:
            self.assertTrue(v & (v - 1) == 0, f"{v:#x} is not a single bit")

    def test_limits_are_sane(self):
        limits = dict((n, v) for n, v, _ in F.LIMITS)
        self.assertEqual(limits["MAX_NAME_LEN"], 255)
        self.assertEqual(limits["MAX_PATH_LEN"], 1024)
        self.assertEqual(limits["SYMLOOP_MAX"], 40)
        self.assertGreater(limits["MAX_PATH_LEN"], limits["MAX_NAME_LEN"])
        self.assertLessEqual(limits["DEFAULT_BLOCK_SIZE"], limits["MAX_BLOCK_SIZE"])
        self.assertGreaterEqual(limits["DEFAULT_BLOCK_SIZE"], limits["MIN_BLOCK_SIZE"])
        self.assertEqual(limits["OBJ_ID_BITS"], 60)
        self.assertEqual(limits["MAX_EXTENT_LEN"], (1 << 56) - 1)
        self.assertLess(limits["ROOT_INO"], limits["FIRST_USER_INO"])
        self.assertLess(limits["SNAPSHOT_DIR_INO"], limits["FIRST_USER_INO"])

    def test_object_id_field_is_wide_enough(self):
        """A j-key packs a record type into the top 4 bits, so inode numbers must fit
        in OBJ_ID_BITS."""
        limits = dict((n, v) for n, v, _ in F.LIMITS)
        self.assertLess(limits["FIRST_USER_INO"], 1 << limits["OBJ_ID_BITS"])


class TestGeneratorOutput(unittest.TestCase):
    """The generated files are the artifact the kernel and the C tools actually compile,
    so check that the emitters run and produce something coherent."""

    def test_rust_output_covers_every_struct(self):
        import gen
        text = gen.emit_rust()
        for s in F.STRUCTS:
            self.assertIn(f"pub struct {s.name} {{", text, s.name)
            self.assertIn(f"unsafe impl Pod for {s.name} {{}}", text, s.name)
            self.assertIn(f"pub const DISK_SIZE: usize = {s.padded_size()};", text)

    def test_c_output_covers_every_struct(self):
        import gen
        text = gen.emit_c()
        for s in F.STRUCTS:
            self.assertIn(f"}} {s.name};", text, s.name)
            self.assertIn(f"#define NOLX_{s.name.upper()}_SIZE {s.padded_size()}u",
                          text, s.name)

    def test_rust_and_c_agree_on_sizes(self):
        import gen
        rust, c = gen.emit_rust(), gen.emit_c()
        for s in F.STRUCTS:
            self.assertIn(f"pub const DISK_SIZE: usize = {s.padded_size()};", rust)
            self.assertIn(f"#define NOLX_{s.name.upper()}_SIZE {s.padded_size()}u", c)

    def test_generated_files_are_up_to_date(self):
        import gen
        for _name, (fn, path) in gen.EMITTERS.items():
            self.assertTrue(path.exists(), f"{path} is missing; run gen.py")
            self.assertEqual(path.read_text(), fn(),
                             f"{path} is stale; run: python3 tools/nolxfs-codegen/gen.py")


if __name__ == "__main__":
    unittest.main(verbosity=2)
