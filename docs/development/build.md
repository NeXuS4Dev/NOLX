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
the kernel: the freestanding `no_std` binary targets `x86_64-unknown-none` and has no host
test harness. The object-safe HAL console contract, descriptor layouts, and clock
calculations live in `crates/nolx-hal-core`, which is included in the host tests.


## Make targets

| Target | What it does |
|---|---|
| `make check` | `cargo test` for the host-testable crates (T1) |
| `make kernel` | build `build/nolx.elf` for `x86_64-unknown-none` |
| `make kernel KERNEL_FEATURES=hal-fault-test-pf` | build the dedicated `#PF` test image |
| `scripts/check-hal-faults.sh` | run the dedicated `#PF` and `#GP` QEMU tests |
| `make bootcheck` | assemble + link the boot layer; validate the Multiboot header and 256 ISR entries |
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
| `NOLX: long mode entered` | M1 boot hand-off reached long mode |
| `NOLX: hal online` | M2 single-CPU GDT/TSS/IDT and firmware-selected interrupt/timer setup initialized |
| `hal: active clock source=` | The selected calibrated LAPIC/TSC-deadline or PIT/PIC timer backend |
| `NOLX: timer reached 100 ticks` | T2 confirmed 100 periodic timer interrupts and a monotonic-clock read |
| `wall-clock: CMOS RTC UTC epoch=` | T2 confirmed the CMOS source was decoded and reported during boot |
| `NOLX: mm online` | M3 memory-map bootstrap initialized |
| `NOLX: objmgr online` | The Object Manager registered its types; the executive can create objects |
| `NOLX: obj selftest: 1 object, 2 handles` | M4 reference rules: a handle implies a pointer reference, a widening duplicate is refused, and the object is freed exactly when the last reference goes |
| `NOLX: obj session selftest: sessions 0 and 1 are isolated` | M4 per-session directories below `\Sessions\<id>` resolve an equal name to distinct objects |
| `NOLX: obj private namespace selftest: boundaries enforced, isolated roots` | M4 private namespaces: independent roots, boundary READ/WRITE/DELETE checks, nonempty-destroy refusal, stale-id rejection, exact audit delta |
| `NOLX: obj type callback selftest: create/open/parse/close/delete invoked` | M4 object-type lifecycle callbacks dispatched at their transitions, with create/open rollback |
| `NOLX: srm selftest: 10 access checks (6 granted, 4 denied)` | M4 Security Reference Monitor: DACL order, privileges, integrity labels, and audit totals agree with the cases |
| `NOLX: user mode T2: init spawned (pid 1), entering ring 3` | user mode reached; CI also requires the parent-observed exit status 42 and the ring-3 tmpfs round-trip |

CI's normal QEMU smoke test asserts ACPI, HPET, APIC calibration, HAL, memory, and subsystem
markers and waits for 100 selected-backend timer interrupts. A second boot hides local-APIC
support and verifies that the 8259/PIT fallback still reaches 100 ticks. CI then boots two
fault-test images to verify that `#PF` and `#GP` print register state and halt. Any `PANIC`,
`BUGCHECK`, or unexpected CPU exception in the normal boot log fails the job.

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
