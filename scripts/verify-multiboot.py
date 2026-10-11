#!/usr/bin/env python3
"""Validate the Multiboot header of a NOLX kernel image.

The Multiboot 1 specification has a handful of requirements that are easy to get subtly
wrong and that fail at boot time with no diagnostics: the header must be within the first
8192 bytes of the OS image, must be 4-byte aligned, and its checksum must be the twos
complement of the sum of the magic and the flags. When the a.out kludge flag is set, four
more addresses must match what the linker actually produced, or the loader will copy the
wrong number of bytes and zero the wrong range.

This checks all of it against a real linked ELF, plus the linker-script symbols the boot
layer depends on.

    verify-multiboot.py build/nolx.elf
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

MULTIBOOT_MAGIC = 0x1BADB002
FLAG_ALIGN = 1 << 0
FLAG_MEMINFO = 1 << 1
FLAG_VIDEO = 1 << 2
FLAG_AOUT_KLUDGE = 1 << 16
HEADER_SEARCH_LIMIT = 8192

EM_X86_64 = 0x3E
SHT_SYMTAB = 2
SHT_NOBITS = 8

PAGE_SIZE = 4096
HIGHER_HALF_MIN = 0xFFFF800000000000


class Fail(Exception):
    pass


def check(cond: bool, msg: str) -> None:
    if not cond:
        raise Fail(msg)


class Elf:
    def __init__(self, data: bytes):
        check(data[:4] == b"\x7fELF", "not an ELF file")
        check(data[4] == 2, "not a 64-bit ELF")
        check(data[5] == 1, "not little-endian")
        (self.e_type, self.e_machine, self.e_version, self.e_entry, self.e_phoff,
         self.e_shoff, self.e_flags, self.e_ehsize, self.e_phentsize, self.e_phnum,
         self.e_shentsize, self.e_shnum, self.e_shstrndx) = struct.unpack_from(
            "<HHIQQQIHHHHHH", data, 16)
        self.data = data
        self._sections()
        self._symbols()

    def _sections(self) -> None:
        self.sections: list[dict] = []
        for i in range(self.e_shnum):
            off = self.e_shoff + i * self.e_shentsize
            (sh_name, sh_type, sh_flags, sh_addr, sh_offset, sh_size, sh_link,
             sh_info, sh_addralign, sh_entsize) = struct.unpack_from(
                "<IIQQQQIIQQ", self.data, off)
            self.sections.append(dict(name_off=sh_name, type=sh_type, flags=sh_flags,
                                      addr=sh_addr, offset=sh_offset, size=sh_size,
                                      link=sh_link, entsize=sh_entsize,
                                      addralign=sh_addralign))
        strtab = self.sections[self.e_shstrndx]
        raw = self.data[strtab["offset"]:strtab["offset"] + strtab["size"]]
        for s in self.sections:
            end = raw.index(b"\0", s["name_off"])
            s["name"] = raw[s["name_off"]:end].decode("ascii", "replace")

    def _symbols(self) -> None:
        self.symbols: dict[str, int] = {}
        for s in self.sections:
            if s["type"] != SHT_SYMTAB:
                continue
            strtab = self.sections[s["link"]]
            names = self.data[strtab["offset"]:strtab["offset"] + strtab["size"]]
            count = s["size"] // 24
            for i in range(count):
                st_name, st_info, st_other, st_shndx, st_value, st_size = \
                    struct.unpack_from("<IBBHQQ", self.data,
                                       s["offset"] + i * 24)
                if st_name == 0:
                    continue
                end = names.index(b"\0", st_name)
                self.symbols[names[st_name:end].decode("ascii", "replace")] = st_value

    def section(self, name: str) -> dict | None:
        return next((s for s in self.sections if s["name"] == name), None)

    def loads(self) -> list[dict]:
        out = []
        for i in range(self.e_phnum):
            off = self.e_phoff + i * self.e_phentsize
            (p_type, p_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz,
             p_align) = struct.unpack_from("<IIQQQQQQ", self.data, off)
            if p_type == 1:  # PT_LOAD
                out.append(dict(flags=p_flags, offset=p_offset, vaddr=p_vaddr,
                                paddr=p_paddr, filesz=p_filesz, memsz=p_memsz,
                                align=p_align))
        return out


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(argv[1])
    if not path.exists():
        print(f"FAIL: {path} does not exist", file=sys.stderr)
        return 2

    try:
        elf = Elf(path.read_bytes())
        passed: list[str] = []

        def ok(msg: str) -> None:
            passed.append(msg)
            print(f"  ok   {msg}")

        check(elf.e_machine == EM_X86_64,
              f"e_machine is {elf.e_machine:#x}, expected {EM_X86_64:#x} (x86-64)")
        ok("ELF64 x86-64")

        check(elf.e_type in (2, 3), f"e_type {elf.e_type} is not EXEC/DYN")
        ok(f"ELF type {elf.e_type} (EXEC/DYN)")

        mb = elf.section(".multiboot")
        check(mb is not None, "no .multiboot section; linker.ld must place it first")
        assert mb is not None
        check(mb["addr"] % 4 == 0, ".multiboot is not 4-byte aligned")
        check(mb["size"] >= 28, f".multiboot is only {mb['size']} bytes; need >= 28")
        ok(f".multiboot at {mb['addr']:#x}, {mb['size']} bytes, 4-byte aligned")

        loads = elf.loads()
        check(bool(loads), "no PT_LOAD segments")
        image_base = min(l["vaddr"] for l in loads)
        check(mb["addr"] >= image_base,
              ".multiboot is below the first load segment")
        check(mb["addr"] - image_base < HEADER_SEARCH_LIMIT,
              f".multiboot is {mb['addr'] - image_base} bytes into the image; "
              f"a Multiboot loader only searches the first {HEADER_SEARCH_LIMIT}")
        ok(f"header is {mb['addr'] - image_base} bytes into the image (< {HEADER_SEARCH_LIMIT})")

        # Also confirm it is inside a segment's file data, so a loader reading the file
        # (not the section table) will actually see it.
        inside = [l for l in loads
                  if l["offset"] <= mb["offset"] < l["offset"] + l["filesz"]]
        check(bool(inside), ".multiboot is not inside any PT_LOAD segment's file data")
        ok("header is inside a PT_LOAD segment's file data")

        magic, flags, checksum = struct.unpack_from("<III", elf.data, mb["offset"])
        check(magic == MULTIBOOT_MAGIC, f"magic is {magic:#x}, expected {MULTIBOOT_MAGIC:#x}")
        ok(f"magic {magic:#010x}")

        check((magic + flags + checksum) & 0xFFFFFFFF == 0,
              f"checksum {checksum:#x} does not zero out magic+flags "
              f"({(magic + flags) & 0xFFFFFFFF:#x})")
        ok(f"checksum {checksum:#010x} is correct for flags {flags:#x}")

        check(flags & FLAG_ALIGN, "ALIGN flag not set: modules may not be page aligned")
        check(flags & FLAG_MEMINFO,
              "MEMINFO flag not set: the memory map will not be handed to us")
        ok("ALIGN and MEMINFO flags set")
        if flags & FLAG_VIDEO:
            ok("VIDEO flag set (loader supplies mode information)")

        # The a.out kludge fields must agree with the linker script, or a loader that
        # uses them will copy or zero the wrong bytes.
        kludge = struct.unpack_from("<IIIII", elf.data, mb["offset"] + 12)
        header_addr, load_addr, load_end_addr, bss_end_addr, entry_addr = kludge

        if flags & FLAG_AOUT_KLUDGE:
            check(header_addr == mb["addr"],
                  f"header_addr {header_addr:#x} != .multiboot address {mb['addr']:#x}")
            check(load_addr == image_base,
                  f"load_addr {load_addr:#x} != first PT_LOAD vaddr {image_base:#x}")

            edata = elf.symbols.get("_edata")
            end = elf.symbols.get("_end")
            check(edata is not None, "_edata is not defined by the linker script")
            check(end is not None, "_end is not defined by the linker script")
            assert edata is not None and end is not None
            check(load_end_addr == edata,
                  f"load_end_addr {load_end_addr:#x} != _edata {edata:#x}")
            check(bss_end_addr == end,
                  f"bss_end_addr {bss_end_addr:#x} != _end {end:#x}")
            check(end >= edata, f"_end {end:#x} < _edata {edata:#x}")
            ok(f"kludge: load {load_addr:#x}, data ends {edata:#x}, bss ends {end:#x}")

            # A loader that uses these fields copies the image as one flat block: it reads
            # `load_end_addr - load_addr` bytes starting at
            # `multiboot-offset - (header_addr - load_addr)` in the file and drops them at
            # `load_addr` (QEMU's Multiboot loader does exactly this, byte for byte). Every
            # stored byte must therefore sit at
            # `file_offset == copy_base + (paddr - load_addr)`, and its physical address must
            # be below `load_end_addr`; anything else is loaded from, or left as, another
            # section's bytes. This is the check that pins the section order in linker.ld.
            copy_base = mb["offset"] - (header_addr - load_addr)
            stored = [l for l in loads if l["filesz"] > 0]
            check(bool(stored), "no PT_LOAD segment carries file content")
            for l in stored:
                want = copy_base + (l["paddr"] - load_addr)
                check(l["offset"] == want,
                      f"segment at {l['paddr']:#x} is at file offset {l['offset']:#x}, "
                      f"but a flat copy needs {want:#x}")
                check(l["paddr"] >= load_addr and l["paddr"] + l["filesz"] <= load_end_addr,
                      f"stored segment {l['paddr']:#x}..{l['paddr'] + l['filesz']:#x} is "
                      f"outside the copied range {load_addr:#x}..{load_end_addr:#x}")
            ok(f"stored segments are flat from file offset {copy_base:#x}: "
               f"file offset = {copy_base:#x} + (load - {load_addr:#x})")

        check(entry_addr == elf.e_entry,
              f"entry_addr {entry_addr:#x} != ELF entry {elf.e_entry:#x}")
        ok(f"entry {elf.e_entry:#x}")

        start32 = elf.symbols.get("__start32")
        check(start32 is not None, "__start32 is not defined")
        assert start32 is not None
        check(start32 == elf.e_entry,
              f"ELF entry {elf.e_entry:#x} is not __start32 {start32:#x}")
        ok("ELF entry point is __start32")

        for name, align in (("pml4", PAGE_SIZE), ("pdpt", PAGE_SIZE),
                            ("pd0", PAGE_SIZE), ("pd1", PAGE_SIZE), ("pd3", PAGE_SIZE),
                            ("boot_stack_top", 16), ("boot_stack_bottom", 16)):
            value = elf.symbols.get(name)
            check(value is not None, f"{name} is not defined")
            assert value is not None
            check(value % align == 0,
                  f"{name} = {value:#x} is not {align}-byte aligned")
        ok("page tables are 4096-byte aligned; the boot stack is 16-byte aligned")

        for name in ("mb_info_phys", "mb_magic", "gdt64", "gdt64_ptr",
                     "long_mode_entry", "__multiboot_header"):
            check(name in elf.symbols, f"{name} is not defined")
        ok("boot symbols present (gdt64, long_mode_entry, mb_info_phys, mb_magic)")

        gdt = elf.symbols.get("gdt64")
        gdt_end_candidates = [v for k, v in elf.symbols.items() if k == "gdt64_end"]
        check(gdt is not None and bool(gdt_end_candidates), "gdt64/gdt64_end missing")
        assert gdt is not None and gdt_end_candidates
        size = gdt_end_candidates[0] - gdt
        check(size >= 48 and size % 8 == 0,
              f"gdt64 is {size} bytes; expected a multiple of 8 with >= 6 entries")
        ok(f"gdt64 holds {size // 8} descriptors ({size} bytes)")

        # Decode the descriptors. A wrong DPL or a missing L bit here costs hours of
        # triple faults later, so check the bits rather than trusting the source.
        gdt_addr = elf.symbols["gdt64"]
        holder = next((s for s in elf.sections
                       if s["type"] != SHT_NOBITS and s["size"]
                       and s["addr"] <= gdt_addr < s["addr"] + s["size"]), None)
        check(holder is not None, "gdt64 is not inside any stored section")
        assert holder is not None
        gdt_file_off = holder["offset"] + (gdt_addr - holder["addr"])
        descs = struct.unpack_from(f"<{size // 8}Q", elf.data, gdt_file_off)

        def decode(d: int) -> dict:
            return dict(
                present=bool((d >> 47) & 1),
                dpl=(d >> 45) & 3,
                system=bool((d >> 44) & 1),
                type=(d >> 40) & 0xF,
                long_mode=bool((d >> 53) & 1),
                db=bool((d >> 54) & 1),
            )

        # The user entries are data-at-0x18, code-at-0x20: the `sysret` convention
        # (SS = STAR[63:48]+8, CS = STAR[63:48]+16 with STAR[63:48] = 0x10).
        expected = {
            1: ("kernel code", dict(present=True, dpl=0, system=True, type=0xA,
                                    long_mode=True, db=False)),
            2: ("kernel data", dict(present=True, dpl=0, system=True, type=0x2,
                                    long_mode=False, db=True)),
            3: ("user data", dict(present=True, dpl=3, system=True, type=0x2,
                                  long_mode=False, db=True)),
            4: ("user code", dict(present=True, dpl=3, system=True, type=0xA,
                                  long_mode=True, db=False)),
        }
        check(descs[0] == 0, "GDT entry 0 must be the null descriptor")
        for index, (label, want) in expected.items():
            got = decode(descs[index])
            for key, value in want.items():
                check(got[key] == value,
                      f"GDT[{index}] ({label}): {key} is {got[key]!r}, expected {value!r}")
            check(got["long_mode"] == (not got["db"]),
                  f"GDT[{index}] ({label}): L and D/B must be mutually exclusive")
        ok("GDT descriptors: null, kernel code (L, DPL0), kernel data, "
           "user data (sysret SS), user code (L, DPL3, sysret CS)")
        check(descs[5] == 0 and descs[6] == 0,
              "the TSS descriptor slot must be zero at link time; the HAL fills it in")
        ok("TSS slot reserved and zeroed at link time")

        # The trampoline is linked where the loader puts it; the entry point has to be in it.
        # The Rust kernel is on the other side of the higher-half switch, so the entry point
        # must not be reachable only after the switch has already happened.
        boot = elf.section(".boot")
        check(boot is not None, "no .boot section")
        assert boot is not None
        check(boot["addr"] <= elf.e_entry < boot["addr"] + boot["size"],
              "the entry point is not inside .boot")
        ok(f".boot at {boot['addr']:#x}, {boot['size']} bytes, contains the entry point")

        image_end_load = elf.symbols.get("_end")
        check(image_end_load is not None, "_end is not defined by the linker script")
        assert image_end_load is not None

        # Every higher-half segment must live at the window's fixed offset from where it is
        # loaded, because the trampoline maps window page i to physical page i. A segment
        # anywhere else would be reachable only while the identity map still exists.
        higher = [l for l in loads if l["vaddr"] >= HIGHER_HALF_MIN]
        check(bool(higher), "no higher-half segment; the image window is not mapped")
        delta = higher[0]["vaddr"] - higher[0]["paddr"]
        check(delta % 0x200000 == 0,
              f"the image window base {delta:#x} is not 2 MiB aligned")
        for l in higher:
            check(l["vaddr"] - l["paddr"] == delta,
                  f"segment at {l['vaddr']:#x} is not at the image window offset")

        text = elf.section(".text")
        check(text is not None, "no .text section")
        assert text is not None
        check(text["addr"] >= HIGHER_HALF_MIN,
              f".text at {text['addr']:#x} is not in the higher half")
        check(text["addr"] % PAGE_SIZE == 0, ".text is not page aligned")
        ok(f".text at {text['addr']:#x}, {text['size']} bytes, in the image window")

        # The window has to cover the kernel's whole reserved range -- the loaded image plus
        # the static arena that follows it -- because both the linker and the trampoline
        # derive the window from one symbol. `_image_window_pages` counts 2 MiB pages from
        # the load address; `_kernel_region_phys_end` is the load-address-side end of that
        # range. A window that is too small would leave part of the kernel unmapped once the
        # identity map is gone; taking the ceiling is the only way to cover a partial page.
        window_pages = elf.symbols.get("_image_window_pages")
        check(window_pages is not None, "_image_window_pages is not defined")
        assert window_pages is not None
        region_end = elf.symbols.get("_kernel_region_phys_end")
        check(region_end is not None, "_kernel_region_phys_end is not defined")
        assert region_end is not None
        expected_pages = (region_end + 0x1FFFFF) >> 21
        check(window_pages == expected_pages,
              f"_image_window_pages {window_pages} != {expected_pages} "
              f"for region end {region_end:#x}")
        check(region_end >= image_end_load,
              f"_kernel_region_phys_end {region_end:#x} < _end {image_end_load:#x}")
        image_highest = max(l["paddr"] + l["memsz"] for l in higher)
        check(image_highest <= region_end,
              f"a segment ends at {image_highest:#x}, past {region_end:#x}")
        check(((window_pages - 1) << 21) < region_end,
              f"_image_window_pages {window_pages} overshoots {region_end:#x} "
              "by more than one huge page")
        ok(f"the image window covers {region_end:#x} "
           f"in {window_pages} huge page(s)")

        bss = elf.section(".bss")
        check(bss is not None, "no .bss section")
        assert bss is not None
        check(bss["type"] == SHT_NOBITS, ".bss should be SHT_NOBITS (not stored in the file)")
        ok(f".bss at {bss['addr']:#x}, {bss['size']} bytes, SHT_NOBITS")

        print(f"\nPASS: {path} is a valid Multiboot kernel image "
              f"({len(passed)} checks)")
        return 0

    except Fail as e:
        print(f"FAIL: {path}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
