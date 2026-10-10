# Building NOLX

## Prerequisites

| Tool | Why | Minimum |
|---|---|---|
| `rustup` | toolchain management | stable; see `rust-toolchain.toml` |
| `rustc`/`cargo` | builds everything | the version pinned in `rust-toolchain.toml` |
| GNU `binutils` (`as`, `ld`, `objcopy`) | boot layer assembly, image post-processing | 2.38 |
| `qemu-system-x86_64` | running the kernel | 8.0 |
| `python3` | NOLXFS layout codegen, image verification | 3.10 |
| `grub-mkrescue`, `xorriso` | ISO images (optional; QEMU `-kernel` needs neither) | — |

```sh
rustup target add x86_64-unknown-none
```

NOLX builds on **stable** Rust. Everything the kernel needs -- `no_std`,
`core::arch::asm!`, `core::arch::global_asm!`, `MaybeUninit` -- is stable, and a kernel that
only builds on one specific nightly stops building three months later.

The kernel targets `x86_64-unknown-none`, a built-in target with precompiled `core` and
`compiler_builtins`, so `-Z build-std` is not needed and neither is a custom target JSON.
That is a deliberate choice: one fewer thing that can break when the compiler changes.

## Targets

The kernel builds for `x86_64-unknown-none`, whose defaults are already what a kernel
wants: no operating system, `panic-strategy: "abort"`, red zone disabled, software floating
point, and no vector extensions -- which is the important one, because the kernel must not
touch SSE or AVX state it has not decided to save.

The target is *not* set as a global default in `.cargo/config.toml`, because the host-side
crates (`crates/*`, `tools/*`) must build and test for the host. `make kernel` passes
`--target` explicitly, along with the linker script:

```sh
RUSTFLAGS="-C link-arg=-T$PWD/kernel/linker.ld" \
  cargo build --release --target x86_64-unknown-none -p nolx-kernel
```

`cargo test` with no arguments builds only the workspace's `default-members`, which excludes
the kernel: a `no_std` staticlib cannot build a test harness, and its testable logic lives in
the crates it depends on. That exclusion is in the root `Cargo.toml` with a comment saying
why.


## Make targets

| Target | What it does |
|---|---|
| `make check` | `cargo test` for the host-testable crates (T1) |
| `make kernel` | build `build/nolx.elf` for `x86_64-unknown-none` |
| `make bootcheck` | assemble + link the boot layer and validate the Multiboot header |
| `make fscheck` | regenerate NOLXFS layouts and run the layout invariant tests |
| `make iso` | `build/nolx.iso` via `grub-mkrescue` |
| `make run` | QEMU with `-kernel build/nolx.elf -serial stdio` |
| `make run-iso` | QEMU booting the ISO through GRUB |
| `make mkfs` | build `tools/mkfs-nolxfs` for the host |
| `make fmt` / `make lint` | `cargo fmt --check`, `cargo clippy -D warnings` |

`make bootcheck` and `make fscheck` need **no Rust toolchain** — they are the checks that
run anywhere, including in a container with only binutils and Python.

## Running

```sh
make run                       # serial console on stdio, 512 MiB RAM
make run QEMU_ARGS="-smp 4"    # extra QEMU arguments are appended
```

Boot log markers used by CI:

| Marker | Meaning |
|---|---|
| `NOLX: long mode entered` | M1 complete |
| `NOLX: hal online` | M2 complete |
| `NOLX: mm online` | M3 complete |
| `NOLX: objmgr online` | M4 complete |
| `NOLX: init started` | user mode reached |

CI's smoke test boots QEMU, waits for `NOLX: mm online`, and fails on any `PANIC` or
`BUGCHECK` in the log.

## Layout conventions

* `kernel/` is a single `no_std` binary crate. Modules, not crates, so the kernel has one
  link step and no cross-crate monomorphisation surprises.
* `crates/` are libraries that are `no_std` in the kernel and `std` under `cfg(test)`:

  ```rust
  #![cfg_attr(not(test), no_std)]
  ```

  This is the pattern that makes kernel logic testable on a laptop without QEMU. Every
  crate in `crates/` follows it, and every one of them has real unit tests.
* `tools/` are host binaries (`mkfs.nolxfs`, `fsck.nolxfs`). They share `crates/nolxfs`,
  so the format cannot drift between the kernel and the tools.
* `include/nolxfs/nolxfs_format.h` is **generated**. Never edit it; edit
  `tools/nolxfs-codegen/nolxfs_format.py` and run `make fscheck`.

## Reproducibility

`build/nolx.elf` is built with `--remap-path-prefix` so the build path is not embedded,
and the build records `rustc --version --verbose` in `build/buildinfo.txt`. Release
images are signed (M13).
