# NOLX — Native Operating Layer eXecutive

A modern UNIX®-like operating system written in **Rust + assembly**, with a **hybrid kernel
architecturally modelled on the Windows kernel** (HAL / Executive / Kernel / driver model),
a real **GUI**, and its own copy-on-write filesystem, **NOLXFS**.

> NOLX is UNIX-*like*. It targets the published interface standard — The Open Group Base
> Specifications / IEEE Std 1003.1 — and does not contain, derive from, or claim any
> trademarked UNIX® code or name.

---

## The two goals, in priority order

1. **UNIX® fidelity (priority #1).** Externally observable behaviour is defined by
   *The Open Group Base Specifications, Issue 8* (IEEE Std 1003.1-2024, published
   2024-06-14). Conformance is measured, not asserted: every milestone ships with the
   set of specification assertions it satisfies and the tests that prove them.
2. **A Windows-style hybrid kernel.** Internally, NOLX is *not* a UNIX kernel. It is an
   object-based executive: a Hardware Abstraction Layer, an Executive (Object Manager,
   Memory Manager, I/O Manager, Cache Manager, Security Reference Monitor, IPC ports),
   a microkernel-ish core (dispatcher, trap dispatch, interrupt priority levels), and a
   driver stack of *driver objects* + *device objects* with dispatch tables.
   POSIX semantics are delivered by a **POSIX subsystem** layered on the executive — the
   same architectural move NT used, and the reason a Windows-shaped kernel can still
   behave exactly like a UNIX system to a shell script.

The tension between those two goals is the interesting part of the project, and it is
resolved in one place only: `kernel/src/psx/` (see
[docs/architecture/02-posix-subsystem.md](docs/architecture/02-posix-subsystem.md)).

## Repository map

| Path | What lives there |
|---|---|
| `boot/` | Machine-code entry: Multiboot header, 32→64-bit transition, bootstrap paging |
| `kernel/` | The kernel: HAL, executive, POSIX subsystem (`no_std` binary) |
| `crates/nolx-types/` | POSIX scalar types, `errno`, syscall numbers — kernel/user shared |
| `crates/nolx-hal-core/` | Host-testable HAL console contract, ACPI/APIC parsing and arithmetic, x86-64 descriptors, PIC vectors, RTC decoding, and clock primitives |
| `crates/nolx-syscall/` | The user↔kernel ABI: register mapping, marshalling, dispatch table |
| `crates/nolxfs/` | NOLXFS on-disk format, Fletcher-64, B-tree, mkfs/fsck logic |
| `crates/nolx-vfs/` | Pathname resolution, mount table, descriptor table |
| `tools/mkfs-nolxfs/` | `mkfs.nolxfs` host tool |
| `tools/nolxfs-codegen/` | Single-source-of-truth generator for NOLXFS on-disk layouts |
| `userspace/` | libc, shell, coreutils, window server (later milestones) |
| `docs/` | Architecture, on-disk formats, conformance strategy, build guide |
| `scripts/` | Build, run, and verification entry points |

## Build

Prerequisites: stable Rust via `rustup` (plus the `x86_64-unknown-none` target), GNU
`binutils`, `qemu-system-x86_64`, and optionally `xorriso` + `grub-mkrescue` for ISO images.

```sh
make check     # host-side unit tests (crates/*) — no kernel needed
make kernel    # builds the kernel ELF for x86_64-unknown-none
make bootcheck # assembles + links the boot layer and validates the Multiboot header
make iso       # build/nolx.iso (GRUB bootable)
make run       # qemu-system-x86_64 -kernel build/nolx.elf -serial stdio
```

See [docs/development/build.md](docs/development/build.md) for the long version.

## Status

Milestone tracking lives in [ROADMAP.md](ROADMAP.md). Current state:

| Milestone | State |
|---|---|
| M0 — Architecture, ABI and on-disk formats frozen | **done** |
| M1 — Boot layer: Multiboot → long mode → kernel entry | **done** — boot layer machine-verified, kernel entry reached under QEMU |
| M2 — HAL: console, GDT, IDT, PIC/APIC, timer | **done** — ACPI/HPET discovery, xAPIC/x2APIC, I/O APIC routing, TPR/EOI, calibrated LAPIC/TSC-deadline paths with PIC/PIT fallback; every HAL T2 assertion now has a passing QEMU run |
| M3 — Physical/virtual memory, kernel heap | **in progress** — frame map with kernel reservations, buddy allocator with per-CPU magazines, boot-time stress test under live interrupts; page tables, kernel heap, and the higher-half switch remain |
| M4+ | planned |

### What has actually been verified

This repository is honest about which artifacts were executed and which were not.

* **Machine-verified here:** `boot/boot.S` and all 256 interrupt stubs assemble with GNU
  `as`; the Multiboot trampoline links with the real `kernel/linker.ld`, and the check
  validates its ELF layout, entry point, header, and the ISR-table size. Reproduce with
  `make bootcheck`.
* **Machine-verified here:** the NOLXFS layout generator and its offset/alignment
  invariants (`tools/nolxfs-codegen/test_layout.py`). Reproduce with `make fscheck`.
* **Machine-verified here (2026-10-10):** `make bootcheck fscheck` passed, including
  assembly/linking of the boot layer and all 256 interrupt stubs, Multiboot validation,
  NOLXFS layout tests, and the conformance-database check.
* **Machine-verified in the development sandbox (2026-10-10):** the memory-management core
  (`crates/nolx-mm-core`) passes 78 host tests -- buddy split/coalesce and randomized traffic,
  magazine conservation, paging arithmetic and the page-table walker, VAD split/coalesce and
  randomized model comparison, commit and pool accounting, and heap integrity checks. The
  kernel's own boot path was rehearsed against the same crate on the host: a simulated
  512 MiB machine, the same store layout, the same reservations, and 200,000 mixed-order
  allocations and frees with an audit every 4,096 operations, ending with the free-frame
  count exactly where it started.
* **Machine-verified in the development sandbox (2026-10-10):** a Rust 1.88 stable toolchain
  (rustc, cargo, clippy, rustfmt, and the `x86_64-unknown-none` standard library, assembled
  from the Rust distribution packages published to this sandbox's package registries) runs
  `make check` — host tests for every crate — plus `make bootcheck fscheck`, `make kernel`
  for the freestanding `x86_64-unknown-none` image, `scripts/verify-multiboot.py`,
  `cargo fmt --check`, `cargo clippy -D warnings`, and `scripts/check-unsafe.sh` clean.
  QEMU is still unavailable here, so boot tests remain CI-only.
* **CI-verified (2026-10-10):** workflow run
  [38086516018](https://github.com/NeXuS4Dev/NOLX/actions/runs/38086516018) on revision
  `dc2d8d1` passed every job: boot layer, NOLXFS layout, host tests, fmt + clippy + unsafe
  audit, kernel build, and the QEMU boot smoke test. That boot test boots the kernel,
  asserts every boot marker, checks the wall clock advanced from its RTC anchor, boots the
  `#PF`/`#GP` fault images, and exercises the PIC/PIT fallback with the local APIC hidden.
  The eight HAL assertions that were `pending-T2` are now `tested-T2`.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
