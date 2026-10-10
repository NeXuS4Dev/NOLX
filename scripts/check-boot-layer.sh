#!/usr/bin/env bash
#
# Boot layer check: assemble, link, and validate -- no Rust toolchain required.
#
# This is the one part of NOLX that can be machine-verified with nothing but binutils and
# Python, so it is wired into CI as a hard gate. It proves that:
#
#   * boot/boot.S and the 256-vector interrupt stubs assemble cleanly with GNU as;
#   * the ISR object exports exactly 256 stubs and a 256-entry pointer table;
#   * the boot trampoline links against the real kernel/linker.ld, including every symbol
#     the script defines and asserts on;
#   * the result is an ELF64 whose Multiboot header is valid, correctly placed, and whose
#     a.out kludge fields agree with the linker script.
#
# It links against scripts/boot-stub.s, which stands in for the Rust entry point. That is
# the only symbol this check fakes.
#
# Usage: scripts/check-boot-layer.sh [output-dir]

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$REPO/build}"
mkdir -p "$OUT"

AS="${AS:-as}"
LD="${LD:-ld}"
NM="${NM:-nm}"
OBJDUMP="${OBJDUMP:-objdump}"

fail() { echo "FAIL: $*" >&2; exit 1; }

command -v "$AS" >/dev/null 2>&1 || fail "$AS not found (install binutils)"
command -v "$LD" >/dev/null 2>&1 || fail "$LD not found (install binutils)"

echo "== boot layer check"
echo "   $AS  $($AS --version | head -1)"
echo "   $LD  $($LD --version | head -1)"
echo

echo "-- assembling boot/boot.S"
"$AS" --64 --fatal-warnings -o "$OUT/boot.o" "$REPO/boot/boot.S"

echo "-- assembling the x86-64 interrupt stubs"
"$AS" --64 --fatal-warnings -o "$OUT/interrupts.o" "$REPO/kernel/src/hal/interrupts.S"
stub_count=$("$NM" -g --defined-only "$OUT/interrupts.o" \
    | awk '$3 ~ /^nolx_isr_[0-9]+$/ { count++ } END { print count + 0 }')
[[ "$stub_count" -eq 256 ]] || fail "interrupt assembly exports $stub_count stubs, expected 256"
table_size=$("$NM" -S --format=posix "$OUT/interrupts.o" \
    | awk '$1 == "nolx_isr_stub_table" { print $4 }')
[[ "$table_size" == "800" ]] || fail "ISR pointer table is ${table_size:-missing}, expected 0x800 bytes"
echo "  ok   256 interrupt stubs and a 2048-byte pointer table"

echo "-- assembling the test-only kernel entry stub"
"$AS" --64 --fatal-warnings -o "$OUT/boot-stub.o" "$REPO/scripts/boot-stub.s"

echo "-- linking with kernel/linker.ld"
"$LD" -T "$REPO/kernel/linker.ld" \
      --build-id=none \
      -o "$OUT/nolx.bootcheck.elf" \
      "$OUT/boot.o" "$OUT/boot-stub.o"

echo "-- validating the Multiboot header"
python3 "$REPO/scripts/verify-multiboot.py" "$OUT/nolx.bootcheck.elf"

echo
# The entry point is 32-bit code inside a 64-bit ELF, so it has to be disassembled with
# -m i386. Decoding it as x86-64 (objdump's default here) silently misreads instructions
# -- which is a trap worth naming, because the misreading looks plausible.
START32="0x$("$NM" "$OUT/nolx.bootcheck.elf" | awk '/ __start32$/ {print $1}')"
MODE64="0x$("$NM" "$OUT/nolx.bootcheck.elf" | awk '/ long_mode_entry$/ {print $1}')"

echo "-- 32-bit trampoline (__start32 .. long_mode_entry), decoded as i386"
"$OBJDUMP" -d -m i386 --start-address="$START32" --stop-address="$MODE64" \
    "$OUT/nolx.bootcheck.elf" | sed -n '7,40p'

echo
echo "-- 64-bit entry (long_mode_entry), decoded as x86-64"
"$OBJDUMP" -d --start-address="$MODE64" "$OUT/nolx.bootcheck.elf" \
    | sed -n '7,26p'

echo
echo "BOOTCHECK: PASS  ($OUT/nolx.bootcheck.elf)"
