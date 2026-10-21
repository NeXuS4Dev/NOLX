# NOLX Dependency Cache

Prebuilt artifacts and bootstrap scripts for QEMU + Rust toolchain.

## What's here

- `qemu/qemu-system-x86_64` — QEMU 11.1.50 built for `x86_64-softmmu` TCG mode (no KVM needed)
- `stubs/kernel_stubs.S` — the freestanding runtime helpers (`memcpy`, `memmove`, `memset`,
  `memcmp`, `bcmp`, the Rust unwinding personality) the host-target fallback link needs
- `stubs/*.o` — the same helpers and the boot/interrupt/syscall stubs, assembled. They are
  regenerated from source by `scripts/nolx-env.sh` on every kernel build; the checked-in
  objects are only a convenience and must never be newer than their sources.

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

- Real implementations of `rust_eh_personality`, `memcmp`, `memmove`, `memcpy`,
  `memset`, `bcmp` and `_Unwind_Resume` (`stubs/kernel_stubs.S`). **They must do the
  work.** An earlier version defined them as bare `ret` instructions: the link succeeded,
  and LLVM turned the 16 MiB frame-map fill into a `memset` call that silently wrote
  nothing, so the machine reported zero usable frames.
- `-C link-arg=--relax` to convert `R_X86_64_32` to `R_X86_64_32S` at high
  kernel addresses
- `-static -no-pie`, so the image is not a dynamically linked executable whose `.interp`
  and relocations no bootloader will resolve
- Manually assembled boot trampoline files (`boot.S`, `interrupts.S`,
  `syscall.S`) linked in

The kernel itself never uses floating point, but the host target's `core` library is
compiled with SSE and is linked into the image. The boot trampoline therefore enables SSE
(`CR4.OSFXSR|CR4.OSXMMEXCPT`, CR0.EM/TS clear, default MXCSR) before it enters long mode.
Without that, the first `movups` the core library emitted raised #UD.

See `scripts/nolx-env.sh` for the exact `RUSTFLAGS`.