#!/usr/bin/env python3
"""Emit the ring-3 user programs the M6 boot test runs.

    gen.py                 write the generated file
    gen.py --check         exit non-zero if the checked-in file is stale
    gen.py --stdout        print the generated Rust to stdout

The generated file is:
    crates/nolx-exec-core/src/programs_gen.rs

Two tiny statically linked x86-64 ET_EXEC images are assembled from hand-written machine
code, byte for byte, in this file. That is deliberate: the images must be *deterministic*.
Compiling them with a host toolchain would make the kernel image depend on whichever gcc
happens to be installed, and a test that passes on one machine and fails on another is not
a test. Every instruction below is commented with its mnemonic; the ELF headers are the
only other content.

    hello  -- write(1, "hello: hello from ring 3\\n"), exit(42)
    init   -- posix_spawn("hello"), waitid, report the observed status, exit(42)

`init` is the parent the M6 milestone's T2 test needs: it observes the child's exit
status through `waitid` and only reports success when the status is exactly 42.

Run `make usercheck` rather than this directly.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUT_RUST = REPO / "crates" / "nolx-exec-core" / "src" / "programs_gen.rs"

BANNER = ("GENERATED FILE -- DO NOT EDIT.\n"
          "Source of truth: tools/user-elf/gen.py\n"
          "Regenerate with: make usercheck\n")

# ---------------------------------------------------------------- ABI constants
# From crates/nolx-types/src/syscall.rs and crates/nolx-types/src/abi.rs. Duplicated
# here (and pinned by the generated tests) because a generator cannot import Rust.
SYS_EXIT = 0
SYS_WAITID = 4
SYS_POSIX_SPAWN = 31
SYS_WRITE = 105
P_PID = 1
WEXITED = 4
SIGINFO_STATUS_OFFSET = 20
EXPECTED_STATUS = 42

PAGE = 0x1000


class Asm:
    """A two-pass x86-64 assembler for exactly the instructions these programs need."""

    def __init__(self, vaddr: int):
        self.vaddr = vaddr
        self.code = bytearray()
        self.labels: dict[str, int] = {}
        self.fixups: list[tuple[int, str, str]] = []  # (offset, label, kind)

    def here(self) -> int:
        return self.vaddr + len(self.code)

    def label(self, name: str) -> None:
        self.labels[name] = self.here()

    def raw(self, data: bytes) -> None:
        self.code += data

    def mov_rax(self, value: int) -> None:
        """mov rax, imm32 (sign-extended)."""
        self.raw(b"\x48\xc7\xc0" + value.to_bytes(4, "little", signed=True))

    def mov_rdi(self, value: int) -> None:
        """mov rdi, imm32 (sign-extended)."""
        self.raw(b"\x48\xc7\xc7" + value.to_bytes(4, "little", signed=True))

    def mov_rdx(self, value: int) -> None:
        """mov rdx, imm32 (sign-extended)."""
        self.raw(b"\x48\xc7\xc2" + value.to_bytes(4, "little", signed=True))

    def mov_r10d(self, value: int) -> None:
        """mov r10d, imm32."""
        self.raw(b"\x41\xba" + value.to_bytes(4, "little", signed=True))

    def xor_r10d(self) -> None:
        """xor r10d, r10d."""
        self.raw(b"\x45\x31\xd2")

    def syscall(self) -> None:
        """syscall."""
        self.raw(b"\x0f\x05")

    def mov_r11_rax(self) -> None:
        """mov r11, rax."""
        self.raw(b"\x49\x89\xc3")

    def mov_rsi_r11(self) -> None:
        """mov rsi, r11."""
        self.raw(b"\x4c\x89\xde")

    def lea(self, reg: str, label: str) -> None:
        """lea reg, [rip + label]."""
        modrm = {"rsi": b"\x35", "rdi": b"\x3d", "rdx": b"\x15"}[reg]
        self.raw(b"\x48\x8d" + modrm + b"\x00\x00\x00\x00")
        self.fixups.append((len(self.code) - 4, label, "rel32"))

    def mov_eax_label(self, label: str) -> None:
        """mov eax, dword [rip + label]."""
        self.raw(b"\x8b\x05\x00\x00\x00\x00")
        self.fixups.append((len(self.code) - 4, label, "rel32"))

    def cmp_eax(self, value: int) -> None:
        """cmp eax, imm8."""
        self.raw(b"\x83\xf8" + value.to_bytes(1, "little", signed=True))

    def jne(self, label: str) -> None:
        """jne rel32."""
        self.raw(b"\x0f\x85\x00\x00\x00\x00")
        self.fixups.append((len(self.code) - 4, label, "rel32"))

    def finish(self) -> bytes:
        for offset, label, _kind in self.fixups:
            target = self.labels[label]
            next_instruction = self.vaddr + offset + 4
            disp = target - next_instruction
            self.code[offset:offset + 4] = disp.to_bytes(4, "little", signed=True)
        return bytes(self.code)


# ---------------------------------------------------------------- ELF assembly
E_TYPE_EXEC = 2
E_MACHINE_X86_64 = 62
PT_LOAD = 1
PT_GNU_STACK = 0x6474E551
PF_X, PF_W, PF_R = 1, 2, 4


def elf_header(entry: int, phoff: int, phnum: int) -> bytes:
    header = bytearray(64)
    header[0:4] = b"\x7fELF"
    header[4] = 2  # ELFCLASS64
    header[5] = 1  # ELFDATA2LSB
    header[6] = 1  # EV_CURRENT
    header[16:18] = E_TYPE_EXEC.to_bytes(2, "little")
    header[18:20] = E_MACHINE_X86_64.to_bytes(2, "little")
    header[20:24] = (1).to_bytes(4, "little")  # e_version
    header[24:32] = entry.to_bytes(8, "little")
    header[32:40] = phoff.to_bytes(8, "little")
    header[52:54] = (64).to_bytes(2, "little")  # e_ehsize
    header[54:56] = (56).to_bytes(2, "little")  # e_phentsize
    header[56:58] = phnum.to_bytes(2, "little")
    return bytes(header)


def program_header(kind: int, flags: int, offset: int, vaddr: int,
                   filesz: int, memsz: int, align: int) -> bytes:
    header = bytearray(56)
    header[0:4] = kind.to_bytes(4, "little")
    header[4:8] = flags.to_bytes(4, "little")
    header[8:16] = offset.to_bytes(8, "little")
    header[16:24] = vaddr.to_bytes(8, "little")
    header[24:32] = vaddr.to_bytes(8, "little")
    header[32:40] = filesz.to_bytes(8, "little")
    header[40:48] = memsz.to_bytes(8, "little")
    header[48:56] = align.to_bytes(8, "little")
    return bytes(header)


HELLO_BASE = 0x0040_0000
HELLO_TEXT = HELLO_BASE + 0x100
HELLO_MESSAGE = b"hello: hello from ring 3\n"
HELLO_PATH = b"hello"


def build_hello() -> bytes:
    asm = Asm(HELLO_TEXT)
    # TEMPORARY T2 forensics: a guaranteed #UD as the very first instruction proves
    # (or disproves) that the ring-3 iretq ever completes. Remove once the resume works.
    asm.raw(b"\x0f\x0b")
    asm.lea("rsi", "msg")
    asm.mov_rdx(len(HELLO_MESSAGE))
    asm.mov_rax(SYS_WRITE)
    asm.mov_rdi(1)
    asm.syscall()
    asm.mov_rax(SYS_EXIT)
    asm.mov_rdi(EXPECTED_STATUS)
    asm.syscall()
    asm.label("msg")
    asm.raw(HELLO_MESSAGE)
    code = asm.finish()

    filesz = 0x100 + len(code)
    image = bytearray(filesz)
    phnum = 2
    image[0:64] = elf_header(HELLO_TEXT, 64, phnum)
    image[64:120] = program_header(PT_LOAD, PF_R | PF_X, 0, HELLO_BASE,
                                  filesz, filesz, PAGE)
    image[120:176] = program_header(PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 0x10)
    image[0x100:] = code
    assert len(image) < PAGE, "hello grew past its first page"
    return bytes(image)


INIT_BASE = 0x0042_0000
INIT_TEXT = INIT_BASE + 0x100
INIT_DATA = 0x0042_1000
INIT_OK_MESSAGE = b"init: child exited with status 42\n"
INIT_BAD_MESSAGE = b"init: child exit status mismatch\n"
INIT_PATH = b"hello"

# Data-segment layout, offsets from INIT_DATA.
DATA_PID = 0x00
DATA_ARGV = 0x08
DATA_PATH = 0x18
DATA_OK = 0x20
DATA_BAD = 0x42
DATA_INFO = 0x68
DATA_END = 0xE8


def build_init() -> bytes:
    asm = Asm(INIT_TEXT)
    # posix_spawn(&pid, "hello", argv, NULL)
    asm.lea("rdi", "pid")
    asm.lea("rsi", "path")
    asm.lea("rdx", "argv")
    asm.xor_r10d()
    asm.mov_rax(SYS_POSIX_SPAWN)
    asm.syscall()
    asm.mov_r11_rax()
    # waitid(P_PID, pid, &info, WEXITED)
    asm.mov_rdi(P_PID)
    asm.mov_rsi_r11()
    asm.lea("rdx", "info")
    asm.mov_r10d(WEXITED)
    asm.mov_rax(SYS_WAITID)
    asm.syscall()
    # if info.si_status != 42: report the mismatch and exit(1)
    asm.mov_eax_label("status")
    asm.cmp_eax(EXPECTED_STATUS)
    asm.jne("mismatch")
    # write(1, ok, len(ok)); exit(42)
    asm.lea("rsi", "ok")
    asm.mov_rdx(len(INIT_OK_MESSAGE))
    asm.mov_rax(SYS_WRITE)
    asm.mov_rdi(1)
    asm.syscall()
    asm.mov_rax(SYS_EXIT)
    asm.mov_rdi(EXPECTED_STATUS)
    asm.syscall()
    asm.label("mismatch")
    asm.lea("rsi", "bad")
    asm.mov_rdx(len(INIT_BAD_MESSAGE))
    asm.mov_rax(SYS_WRITE)
    asm.mov_rdi(1)
    asm.syscall()
    asm.mov_rax(SYS_EXIT)
    asm.mov_rdi(1)
    asm.syscall()
    # Data labels: absolute virtual addresses in the data segment.
    asm.label("pid")
    asm.label("argv")
    asm.label("path")
    asm.label("ok")
    asm.label("bad")
    asm.label("info")
    asm.label("status")
    asm.labels["pid"] = INIT_DATA + DATA_PID
    asm.labels["argv"] = INIT_DATA + DATA_ARGV
    asm.labels["path"] = INIT_DATA + DATA_PATH
    asm.labels["ok"] = INIT_DATA + DATA_OK
    asm.labels["bad"] = INIT_DATA + DATA_BAD
    asm.labels["info"] = INIT_DATA + DATA_INFO
    asm.labels["status"] = INIT_DATA + DATA_INFO + SIGINFO_STATUS_OFFSET
    code = asm.finish()

    assert len(code) <= PAGE - 0x100, "init text grew past its first page"
    assert DATA_OK + len(INIT_OK_MESSAGE) <= DATA_BAD
    assert DATA_BAD + len(INIT_BAD_MESSAGE) <= DATA_INFO
    assert DATA_INFO + 128 == DATA_END

    text_filesz = 0x100 + len(code)
    data = bytearray(DATA_END)
    data[DATA_PATH:DATA_PATH + len(INIT_PATH) + 1] = INIT_PATH + b"\x00"
    data[DATA_ARGV:DATA_ARGV + 8] = (INIT_DATA + DATA_PATH).to_bytes(8, "little")
    data[DATA_ARGV + 8:DATA_ARGV + 16] = (0).to_bytes(8, "little")
    data[DATA_OK:DATA_OK + len(INIT_OK_MESSAGE)] = INIT_OK_MESSAGE
    data[DATA_BAD:DATA_BAD + len(INIT_BAD_MESSAGE)] = INIT_BAD_MESSAGE

    filesz = PAGE + len(data)
    image = bytearray(filesz)
    phnum = 3
    image[0:64] = elf_header(INIT_TEXT, 64, phnum)
    image[64:120] = program_header(PT_LOAD, PF_R | PF_X, 0, INIT_BASE,
                                  text_filesz, text_filesz, PAGE)
    image[120:176] = program_header(PT_LOAD, PF_R | PF_W, PAGE, INIT_DATA,
                                   len(data), len(data), PAGE)
    image[176:232] = program_header(PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 0x10)
    image[0x100:0x100 + len(code)] = code
    image[PAGE:] = data
    return bytes(image)


def rust_bytes(name: str, doc: str, data: bytes) -> list[str]:
    lines = [f"/// {doc}", "#[rustfmt::skip]", f"pub static {name}: &[u8] = &["]
    for start in range(0, len(data), 12):
        chunk = data[start:start + 12]
        lines.append("    " + ", ".join(f"0x{b:02x}" for b in chunk) + ",")
    lines.append("];")
    return lines


def emit_rust() -> str:
    hello = build_hello()
    init = build_init()
    o: list[str] = []
    w = o.append
    w("// " + BANNER.replace("\n", "\n// ").rstrip())
    w("")
    w("//! Embedded ring-3 user programs for the M6 boot test.")
    w("//!")
    w("//! `hello` writes one line through `nxposix_write` and exits with status 42.")
    w("//! `init` spawns `hello` through `nxposix_spawn`, observes the exit status with")
    w("//! `nxposix_waitid`, reports it, and exits 42 only when the status was exactly 42.")
    w("")
    o.extend(rust_bytes("HELLO_ELF", f"The `hello` image: a static x86-64 ET_EXEC ELF64 ({len(hello)} bytes).", hello))
    w("")
    o.extend(rust_bytes("INIT_ELF", f"The `init` image: a static x86-64 ET_EXEC ELF64 ({len(init)} bytes).", init))
    w("")
    w("/// The embedded programs, keyed by the path `nxposix_spawn` resolves.")
    w("pub static PROGRAMS: &[(&str, &[u8])] = &[(\"hello\", HELLO_ELF), (\"init\", INIT_ELF)];")
    return "\n".join(o) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="exit non-zero if the checked-in file is stale")
    parser.add_argument("--stdout", action="store_true",
                        help="print the generated Rust instead of writing it")
    args = parser.parse_args()

    text = emit_rust()
    if args.stdout:
        sys.stdout.write(text)
        return 0
    if args.check:
        current = OUT_RUST.read_text() if OUT_RUST.exists() else ""
        if current != text:
            print(f"{OUT_RUST.relative_to(REPO)} is stale; run: make usercheck",
                  file=sys.stderr)
            return 1
        print(f"{OUT_RUST.relative_to(REPO)} is up to date")
        return 0
    OUT_RUST.write_text(text)
    print(f"wrote {OUT_RUST.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
