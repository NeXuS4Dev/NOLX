#!/usr/bin/env python3
"""Emit the Rust and C definitions of the NOLXFS on-disk format.

    gen.py                 write the generated files
    gen.py --check         exit non-zero if the checked-in files are stale
    gen.py --stdout rust   print one emitter's output

The generated files are:
    crates/nolxfs/src/on_disk_gen.rs
    include/nolxfs/nolxfs_format.h

Run `make fscheck` (which also runs test_layout.py) rather than this directly.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import nolxfs_format as F  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
OUT_RUST = REPO / "crates" / "nolxfs" / "src" / "on_disk_gen.rs"
OUT_C = REPO / "include" / "nolxfs" / "nolxfs_format.h"

RUST_TYPE = {"u8": "u8", "u16": "u16", "u32": "u32", "u64": "u64",
             "i8": "i8", "i16": "i16", "i32": "i32", "i64": "i64"}
C_TYPE = {"u8": "uint8_t", "u16": "uint16_t", "u32": "uint32_t", "u64": "uint64_t",
          "i8": "int8_t", "i16": "int16_t", "i32": "int32_t", "i64": "int64_t"}

# Rust keywords that appear as on-disk field names. They are emitted raw-identifier
# escaped (`r#type`) so the generated struct is valid Rust.
RUST_KEYWORDS = frozenset("""
as break const continue crate else enum extern false fn for if impl in let loop match mod
move mut pub ref return self Self static struct super trait true type unsafe use where while
async await dyn abstract become box do final macro override priv typeof unsized virtual yield
try union gen
""".split())


def rust_ident(name: str) -> str:
    return f"r#{name}" if name in RUST_KEYWORDS else name


BANNER = ("GENERATED FILE -- DO NOT EDIT.\n"
          "Source of truth: tools/nolxfs-codegen/nolxfs_format.py\n"
          "Regenerate with: make fscheck\n")


def rust_field(f: F.Field) -> str:
    ty = RUST_TYPE[f.ctype]
    if f.is_array:
        ty = f"[{ty}; {f.count}]"
    return f"    pub {rust_ident(f.name)}: {ty},"


def emit_rust() -> str:
    o: list[str] = []
    w = o.append
    w("// " + BANNER.replace("\n", "\n// ").rstrip())
    w("")
    w("#![allow(clippy::identity_op, clippy::erasing_op)]")
    w("#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]")
    # The crate denies missing_docs; generated fields and offset constants are
    # self-describing, and the authoritative documentation is the format spec.
    w("#![allow(missing_docs, clippy::missing_docs_in_private_items)]")
    w("")
    w("use crate::Pod;")
    w("")
    w(f"/// NOLXFS on-disk format version.")
    w(f"pub const FORMAT_VERSION: u32 = {F.FORMAT_VERSION};")
    w("")

    # ---- magics
    w("// ------------------------------------------------------------------ magics")
    for name, (spell, doc) in F.MAGICS.items():
        w(f"/// {doc}: {spell!r}")
        w(f"pub const MAGIC_{name}: u32 = 0x{F.magic_u32(spell):08X};")
    w("")

    # ---- enum-like constant tables
    def table(title: str, prefix: str, rows: list[tuple], docs=None) -> None:
        w(f"// ------------------------------------------------------------------ {title}")
        for row in rows:
            value, name = row[0], row[1]
            # The explanation goes on its own line as a doc comment, never as a trailing
            # comment. A trailing comment is the one thing in this file rustfmt reflows --
            # it re-aligns the spacing before `//` -- which put the generator and `cargo
            # fmt` in permanent disagreement over a file only one of them is allowed to
            # write. Doc comments are stable under rustfmt and they reach rustdoc, which a
            # trailing comment never would.
            if docs and name in docs:
                w(f"/// {docs[name]}")
            w(f"pub const {prefix}{name}: u32 = 0x{value:X};")
        w("")

    table("object types (nolx_obj_phys_t.type)", "OBJ_", F.OBJECT_TYPES)
    table("object subtypes (nolx_obj_phys_t.subtype)", "SUB_", F.SUBTYPES)
    table("fs-tree record types (high 4 bits of a j-key)", "DREC_TYPE_",
          [(v, n) for v, n in F.RECORD_TYPES])
    table("object header flags", "OBJ_FLAG_", F.OBJ_FLAGS,
          {n: d for _, n, d in F.OBJ_FLAGS})
    table("object map value flags", "OMAP_VAL_FLAG_", F.OMAP_VAL_FLAGS,
          {n: d for _, n, d in F.OMAP_VAL_FLAGS})
    table("B-tree node flags", "BTNODE_", F.BTNODE_FLAGS,
          {n: d for _, n, d in F.BTNODE_FLAGS})
    table("container feature bits: required", "NX_FEATURE_REQ_", F.NX_FEATURES_REQUIRED,
          {n: d for _, n, d in F.NX_FEATURES_REQUIRED})
    table("container feature bits: read-only compatible", "NX_FEATURE_RO_COMPAT_",
          F.NX_FEATURES_RO_COMPAT, {n: d for _, n, d in F.NX_FEATURES_RO_COMPAT})
    table("container feature bits: supported", "NX_FEATURE_SUP_", F.NX_FEATURES_SUPPORTED,
          {n: d for _, n, d in F.NX_FEATURES_SUPPORTED})
    table("inode internal flags", "INODE_FLAG_", F.INODE_FLAGS,
          {n: d for _, n, d in F.INODE_FLAGS})

    # ---- limits
    w("// ------------------------------------------------------------------ limits")
    for name, value, doc in F.LIMITS:
        w(f"/// {doc}")
        w(f"pub const {name}: u64 = {value};")
    w("")

    # ---- structures
    w("// ------------------------------------------------------------------ structures")
    for s in F.STRUCTS:
        if s.doc:
            for line in s.doc.splitlines():
                w(f"/// {line}")
        w("#[repr(C)]")
        # NB: no `Default` -- `[T; N]` only implements it for N <= 32 and these objects
        # are up to 4 KiB. Use `Pod::zeroed()` instead.
        w("#[derive(Clone, Copy, Debug, PartialEq, Eq)]")
        w(f"pub struct {s.name} {{")
        for f in s.fields:
            if f.doc:
                w(f"    /// {f.doc}")
            w(rust_field(f))
        pad = s.tail_padding()
        if pad:
            w(f"    /// trailing padding, must be zero on write")
            w(f"    pub _reserved: [u8; {pad}],")
        w("}")
        w("")
        upper = s.name.replace("nolx_", "").replace("_t", "").upper()
        w(f"impl {s.name} {{")
        w(f"    /// Size of `{s.name}` on disk, in bytes.")
        w(f"    pub const DISK_SIZE: usize = {s.padded_size()};")
        w("    /// Offset of every field, so readers can validate without `memoffset`.")
        fields, packed = s.layout()
        for f, off in fields:
            w(f"    pub const OFF_{f.name.upper()}: usize = {off};")
        if pad:
            w(f"    pub const OFF_RESERVED: usize = {packed};")
        w("}")
        w(f"unsafe impl Pod for {s.name} {{}}")
        w(f"pub const {upper}_DISK_SIZE: usize = {s.padded_size()};")
        w("")

    w("// ------------------------------------------------------------------ derived geometry")
    w(f"/// Checkpoint-map entries that fit in one block.")
    w(f"pub const CHECKPOINT_MAP_CAPACITY: usize = {F.container_superblock_capacity()};")
    w("")
    return "\n".join(o) + "\n"


def c_field(f: F.Field) -> str:
    t = C_TYPE[f.ctype]
    return f"    {t} {f.name}[{f.count}];" if f.is_array else f"    {t} {f.name};"


def emit_c() -> str:
    o: list[str] = []
    w = o.append
    w("/* " + BANNER.replace("\n", "\n * ").rstrip() + "\n */")
    w("")
    w("#ifndef NOLX_FS_FORMAT_H")
    w("#define NOLX_FS_FORMAT_H")
    w("")
    w("#include <stdint.h>")
    w("")
    w(f"#define NOLXFS_FORMAT_VERSION {F.FORMAT_VERSION}u")
    w("")
    for name, (spell, doc) in F.MAGICS.items():
        w(f"/* {doc} */")
        w(f"#define NOLX_MAGIC_{name} 0x{F.magic_u32(spell):08X}u  /* \"{spell}\" */")
    w("")
    for prefix, rows in (("NOLX_OBJ_", F.OBJECT_TYPES),
                         ("NOLX_SUB_", F.SUBTYPES),
                         ("NOLX_DREC_TYPE_", F.RECORD_TYPES)):
        for value, name in rows:
            w(f"#define {prefix}{name} 0x{value:X}u")
        w("")
    for prefix, rows in (("NOLX_OBJ_FLAG_", F.OBJ_FLAGS),
                         ("NOLX_OMAP_VAL_FLAG_", F.OMAP_VAL_FLAGS),
                         ("NOLX_BTNODE_", F.BTNODE_FLAGS),
                         ("NOLX_NX_FEATURE_REQ_", F.NX_FEATURES_REQUIRED),
                         ("NOLX_NX_FEATURE_RO_COMPAT_", F.NX_FEATURES_RO_COMPAT),
                         ("NOLX_NX_FEATURE_SUP_", F.NX_FEATURES_SUPPORTED),
                         ("NOLX_INODE_FLAG_", F.INODE_FLAGS)):
        for value, name, *_ in rows:
            w(f"#define {prefix}{name} 0x{value:X}u")
        w("")
    for name, value, doc in F.LIMITS:
        w(f"#define NOLX_{name} {value}ull  /* {doc} */")
    w("")
    for s in F.STRUCTS:
        if s.doc:
            w(f"/* {s.doc} */")
        w("typedef struct {")
        for f in s.fields:
            if f.doc:
                w(f"    /* {f.doc} */")
            w(c_field(f))
        pad = s.tail_padding()
        if pad:
            w(f"    uint8_t _reserved[{pad}];  /* must be zero on write */")
        w(f"}} {s.name};")
        w(f"#define NOLX_{s.name.upper()}_SIZE {s.padded_size()}u")
        for f, off in s.layout()[0]:
            w(f"#define NOLX_{s.name.upper()}_OFF_{f.name.upper()} {off}u")
        w("")
    w(f"#define NOLX_CHECKPOINT_MAP_CAPACITY {F.container_superblock_capacity()}u")
    w("")
    w("#endif /* NOLX_FS_FORMAT_H */")
    return "\n".join(o) + "\n"


EMITTERS = {"rust": (emit_rust, OUT_RUST), "c": (emit_c, OUT_C)}


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--check", action="store_true",
                   help="fail if the generated files are out of date")
    p.add_argument("--stdout", choices=sorted(EMITTERS),
                   help="print to stdout instead of writing")
    a = p.parse_args(argv)

    if a.stdout:
        sys.stdout.write(EMITTERS[a.stdout][0]())
        return 0

    stale: list[str] = []
    for name, (fn, path) in EMITTERS.items():
        # Exactly one trailing newline. rustfmt collapses a run of blank lines at end of
        # file, so an emitter that produces two puts the generator and `cargo fmt` back in
        # disagreement over a file that only the generator is allowed to write -- which is
        # the same argument as the doc-comment change, applied to the file's last byte.
        text = fn().rstrip("\n") + "\n"
        if a.check:
            if not path.exists():
                stale.append(f"{path} (missing)")
            elif path.read_text() != text:
                stale.append(f"{path} (out of date)")
            else:
                print(f"ok  {path.relative_to(REPO)}")
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            print(f"wrote {path.relative_to(REPO)} ({len(text.splitlines())} lines)")

    if stale:
        print("\nstale generated files:", file=sys.stderr)
        for s in stale:
            print(f"  {s}", file=sys.stderr)
        print("\nrun: python3 tools/nolxfs-codegen/gen.py", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
