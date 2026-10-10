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
| M1 — Boot layer: Multiboot → long mode → kernel entry | **code complete**, boot layer machine-verified |
| M2 — HAL: console, GDT, IDT, PIC/APIC, timer | **in progress** — ACPI/HPET discovery, xAPIC/x2APIC, I/O APIC routing, TPR/EOI, and calibrated LAPIC/TSC-deadline paths are implemented with PIC/PIT fallback; current-revision CI/QEMU verification is pending |
| M3 — Physical/virtual memory, kernel heap | **in progress** |
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
* **Not executed in this sandbox:** Rust tooling and QEMU are unavailable, so no host Rust
  tests, Clippy, freestanding kernel build, or QEMU boot is claimed for this revision. CI is
  configured to verify the APIC/HPET timer path, a separate PIC/PIT fallback, the CMOS
  wall-clock marker, and dedicated `#PF`/`#GP` exception reports; those checks remain pending
  until a workflow run for this revision passes.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
