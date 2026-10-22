#!/usr/bin/env bash
# =============================================================================
# nolx-env.sh — Source this to set up QEMU + Rust for NOLX kernel development
#
# Usage: source scripts/nolx-env.sh
#
# Sets up PATH, RUSTUP_HOME, CARGO_HOME, and the RUSTFLAGS needed to build
# the NOLX kernel. Also documents the exact link command for manual builds.
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---------------------------------------------------------------------------
# 1. QEMU
# ---------------------------------------------------------------------------
QEMU_DIR="$REPO_ROOT/.nolx-deps/qemu"
if [[ -x "$QEMU_DIR/bin/qemu-system-x86_64" ]]; then
  export PATH="$QEMU_DIR/bin:$PATH"
  export NOLX_QEMU="$QEMU_DIR/bin/qemu-system-x86_64"
else
  echo "WARNING: QEMU not found at $QEMU_DIR/bin/qemu-system-x86_64"
  echo "Run: scripts/bootstrap-qemu.sh"
fi

# ---------------------------------------------------------------------------
# 2. Rust (1.97.0 via arena-rust-toolchain)
# ---------------------------------------------------------------------------
RUST_PREFIX="${RUST_PREFIX:-/opt/rust/prefix}"
if [[ -x "$RUST_PREFIX/bin/cargo" ]]; then
  export PATH="$RUST_PREFIX/bin:$PATH"
  export RUSTUP_HOME="$RUST_PREFIX"
  export CARGO_HOME="$RUST_PREFIX"
  export NOLX_RUST_PREFIX="$RUST_PREFIX"
else
  echo "WARNING: Rust not found at $RUST_PREFIX/bin/cargo"
  echo "Run: scripts/bootstrap-rust.sh"
fi

# ---------------------------------------------------------------------------
# 3. Kernel build RUSTFLAGS (x86_64-unknown-linux-gnu fallback target)
#
# The x86_64-unknown-none target's prebuilt stdlib rmeta files embed the
# x86_64-unknown-linux-gnu triple; patching them corrupts rustc's metadata
# decoder. The x86_64-unknown-linux-gnu target compiles all crates but needs:
#
#   - panic=abort + relocation-model=static + code-model=kernel
#   - linker script (kernel/linker.ld), no-pie, entry from linker script
#   - stub objects for rust_eh_personality, memcmp, memmove, memcpy, memset,
#     bcmp, _Unwind_Resume (.nolx-deps/stubs/kernel_stubs.o)
#   - boot trampoline objects: boot.o, interrupts.o, syscall.o
#   - -Wl,--relax to convert R_X86_64_32 -> R_X86_64_32S at high addresses
# ---------------------------------------------------------------------------
STUBS_DIR="$REPO_ROOT/.nolx-deps/stubs"
LINKER_SCRIPT="$REPO_ROOT/kernel/linker.ld"

# The boot trampoline, the interrupt stubs and the freestanding runtime helpers are assembled
# from source on every kernel build. Using the checked-in objects instead would let a stale
# object outlive a change to boot/boot.S; that is exactly how a build once ran without SSE
# enabled and faulted on the first `movups` the core library emitted.
STUBS_BUILD="$REPO_ROOT/build/stubs"
mkdir -p "$STUBS_BUILD"
as --64 --fatal-warnings -o "$STUBS_BUILD/boot.o" "$REPO_ROOT/boot/boot.S"
as --64 --fatal-warnings -o "$STUBS_BUILD/interrupts.o" "$REPO_ROOT/kernel/src/hal/interrupts.S"
as --64 --fatal-warnings -o "$STUBS_BUILD/syscall.o" "$REPO_ROOT/kernel/src/hal/syscall.S"
as --64 --fatal-warnings -o "$STUBS_BUILD/kernel_stubs.o" "$REPO_ROOT/.nolx-deps/stubs/kernel_stubs.S"

export NOLX_KERNEL_RUSTFLAGS="-C panic=abort -C relocation-model=static -C code-model=kernel \
  -C link-arg=-nostartfiles \
  -C link-arg=-T$LINKER_SCRIPT \
  -C link-arg=$STUBS_BUILD/kernel_stubs.o \
  -C link-arg=$STUBS_BUILD/boot.o \
  -C link-arg=$STUBS_BUILD/interrupts.o \
  -C link-arg=$STUBS_BUILD/syscall.o \
  -C link-arg=-Wl,--relax \
  -C link-arg=-static -C link-arg=-no-pie"

# ---------------------------------------------------------------------------
# 4. Convenience functions
# ---------------------------------------------------------------------------

nolx-kernel-build() {
  echo "Building NOLX kernel for x86_64-unknown-linux-gnu..."
  RUSTFLAGS="$NOLX_KERNEL_RUSTFLAGS" \
    cargo build --release --target x86_64-unknown-linux-gnu -p nolx-kernel "$@"
}

nolx-kernel-copy() {
  local elf="${1:-$REPO_ROOT/target/x86_64-unknown-linux-gnu/release/nolx-kernel}"
  local out="${2:-$REPO_ROOT/build/nolx.elf}"
  mkdir -p "$REPO_ROOT/build"
  cp "$elf" "$out"
  echo "Kernel ELF copied to $out"
}

nolx-kernel-run() {
  local elf="${1:-$REPO_ROOT/build/nolx.elf}"
  local mem="${2:-512M}"
  echo "Booting $elf under QEMU ($mem RAM, serial console)..."
  "$NOLX_QEMU" \
    -kernel "$elf" \
    -m "$mem" \
    -nographic \
    -no-reboot \
    "$@"
}

nolx-kernel-verify() {
  local elf="${1:-$REPO_ROOT/build/nolx.elf}"
  echo "Verifying Multiboot header: $elf"
  python3 "$REPO_ROOT/scripts/verify-multiboot.py" "$elf"
}

nolx-kernel-clean() {
  cargo clean --target x86_64-unknown-linux-gnu -p nolx-kernel 2>/dev/null || true
  rm -f "$REPO_ROOT/build/nolx.elf"
  echo "Cleaned kernel build artifacts"
}

nolx-env-info() {
  echo "=== NOLX Environment ==="
  echo "Repo root: $REPO_ROOT"
  echo "QEMU: $NOLX_QEMU"
  echo "Rust: $(cargo --version 2>/dev/null || echo 'not found')"
  echo "Rustc: $(rustc --version 2>/dev/null || echo 'not found')"
  echo "Target: x86_64-unknown-linux-gnu (kernel fallback)"
  echo ""
  echo "Kernel RUSTFLAGS:"
  echo "  $NOLX_KERNEL_RUSTFLAGS"
}

# ---------------------------------------------------------------------------
# 5. Auto-inform on source
# ---------------------------------------------------------------------------
echo "NOLX environment loaded."
echo "  QEMU: $NOLX_QEMU"
echo "  Rust: $(cargo --version 2>/dev/null || echo 'NOT INSTALLED — run scripts/bootstrap-rust.sh')"
echo "  Run 'nolx-env-info' for details, 'nolx-kernel-build' to build, 'nolx-kernel-run' to boot."