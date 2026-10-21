# NOLX — Getting Started (QEMU + Rust)

New to this repo? Start here. Everything you need to build the kernel and boot
it under QEMU is in this workspace — no external package managers, no
`apt`, no network-dependent Rust toolchain installs.

## One-liner

```bash
source scripts/nolx-env.sh        # activate QEMU + Rust environment
nolx-kernel-build               # build kernel ELF
nolx-kernel-copy                # copy to build/nolx.elf
nolx-kernel-verify              # verify Multiboot header (21 checks)
nolx-kernel-run                 # boot under QEMU with serial console
```

## What's prebuilt

| Artifact | Location | What it is |
|----------|----------|------------|
| QEMU 11.1.50 | `.nolx-deps/qemu/bin/qemu-system-x86_64` | TCG-only x86_64 emulator, no KVM needed |
| BIOS + blobs | `.nolx-deps/qemu/share/qemu/` | SeaBIOS, VGA, etc. (resolved automatically) |
| Link stubs | `.nolx-deps/stubs/kernel_stubs.o` | `rust_eh_personality`, `memcmp`, `memmove`, etc. |
| Boot objects | `.nolx-deps/stubs/{boot,interrupts,syscall}.o` | Assembled boot trampoline |

## If artifacts are missing

```bash
scripts/bootstrap-qemu.sh   # builds QEMU from codeload.github.com (~5-10 min)
scripts/bootstrap-rust.sh   # installs Rust 1.97.0 from pypi.org
```

Both work through restricted networks (only GitHub/PyPI needed).

## Kernel build target

The kernel builds for `x86_64-unknown-linux-gnu` (not `x86_64-unknown-none`), because
the prebuilt `x86_64-unknown-none` stdlib rmeta files embed the wrong target triple
and corrupt rustc's metadata decoder when patched. The Linux-gnu target compiles all
crates; linking requires the stub objects in `.nolx-deps/stubs/` and `-Wl,--relax`
to convert `R_X86_64_32` relocations at the high kernel address. All of this is
encapsulated in `nolx-env.sh`.

## Verification

| Check | Command | Expected |
|-------|---------|----------|
| Build | `nolx-kernel-build` | exit 0 |
| Tests | `cargo test --lib --bins` | 102 passed, 0 failed |
| Format | `cargo fmt --all -- --check` | clean |
| Clippy | `cargo clippy --all-targets -D warnings` | clean |
| Conformance | `python3 tools/conformance-report.py --check` | 406 assertions, 0 problems |
| Multiboot | `nolx-kernel-verify` | PASS (21 checks) |
| QEMU boot | `nolx-kernel-run` | serial console shows "NOLX 0.1.0 starting" |

## M9 status

M9 (Devices and drivers) is in progress. The kernel compiles and boots under QEMU
with serial console. Remaining open gates are listed in
`docs/development/m9-completion.md` — none require the bootstrap tools above.