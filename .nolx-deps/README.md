# NOLX Dependency Cache

Prebuilt artifacts and bootstrap scripts for QEMU + Rust toolchain.

## What's here

- `qemu/qemu-system-x86_64` — QEMU 11.1.50 built for `x86_64-softmmu` TCG mode (no KVM needed)
- `stubs/` — dummy object files for kernel linking on `x86_64-unknown-linux-gnu` fallback target

## Quick start

```bash
# 1. Source the environment
source scripts/nolx-env.sh

# 2. Verify QEMU works
qemu-system-x86_64 --version

# 3. Verify Rust works
cargo --version
rustc --version

# 4. Build the kernel
make kernel

# 5. Boot under QEMU
make run-kernel
```

## If artifacts are missing

Run the bootstrap:

```bash
scripts/bootstrap-qemu.sh    # builds QEMU from codeload.github.com (works through restricted network)
scripts/bootstrap-rust.sh    # installs Rust 1.97.0 via pypi.org (arena-rust-toolchain)
```

## Network restrictions

This environment only allows outbound to: `github.com`, `codeload.github.com`,
`pypi.org`, `files.pythonhosted.org`, `registry.npmjs.org`.

QEMU source is fetched from `codeload.github.com/qemu/qemu`. Rust is installed
from `pypi.org` via the `arena-rust-toolchain` package. Both are known to work.

## Why x86_64-unknown-linux-gnu instead of x86_64-unknown-none?

The `x86_64-unknown-none` target's prebuilt standard library rmeta files embed
`x86_64-unknown-linux-gnu` as the target triple, and patching them breaks rustc's
metadata decoder (string-sentinel corruption). The `x86_64-unknown-linux-gnu`
target compiles all crates successfully; linking requires:

- Stub objects defining `rust_eh_personality`, `memcmp`, `memmove`, `memcpy`,
  `memset`, `bcmp`, `_Unwind_Resume` (in `stubs/`)
- `-C link-arg=--relax` to convert `R_X86_64_32` to `R_X86_64_32S` at high
  kernel addresses
- Manually assembled boot trampoline files (`boot.S`, `interrupts.S`,
  `syscall.S`) linked in

See `scripts/nolx-env.sh` for the exact `RUSTFLAGS`.