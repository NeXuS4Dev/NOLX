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
| `crates/nolx-syscall/` | The user↔kernel ABI: register mapping, marshalling, dispatch table |
| `crates/nolxfs/` | NOLXFS on-disk format, Fletcher-64, B-tree, mkfs/fsck logic |
| `crates/nolx-vfs/` | Pathname resolution, mount table, descriptor table |
| `tools/mkfs-nolxfs/` | `mkfs.nolxfs` host tool |
| `tools/nolxfs-codegen/` | Single-source-of-truth generator for NOLXFS on-disk layouts |
| `userspace/` | libc, shell, coreutils, window server (later milestones) |
| `docs/` | Architecture, on-disk formats, conformance strategy, build guide |
| `scripts/` | Build, run, and verification entry points |

## Build

Prerequisites: `rustup` (nightly, with `rust-src`), GNU `binutils`, `qemu-system-x86_64`,
`xorriso` + `grub-mkrescue` for ISO images.

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
| M2 — HAL: console, GDT, IDT, PIC, timer | **code complete**, unverified on hardware |
| M3 — Physical/virtual memory, kernel heap | **in progress** |
| M4+ | planned |

### What has actually been verified

This repository is honest about which artifacts were executed and which were not.

* **Machine-verified here:** `boot/boot.S` assembles with GNU `as`, links with the real
  `kernel/linker.ld`, produces a valid ELF64 with the expected entry point and section
  layout, and carries a Multiboot header whose magic/flags/checksum are validated by
  `scripts/verify-multiboot.py`. Reproduce with `make bootcheck`.
* **Machine-verified here:** the NOLXFS layout generator and its offset/alignment
  invariants (`tools/nolxfs-codegen/test_layout.py`). Reproduce with `make fscheck`.
* **Not verified here:** all Rust sources. The build sandbox for this repository has no
  Rust toolchain and no network route to `static.rust-lang.org` or `static.crates.io`, so
  `cargo` cannot run. `.github/workflows/ci.yml` runs `cargo test`, `cargo build` for the
  kernel target, and a QEMU boot smoke test on every push. Treat "code complete" as
  "written and reviewed", not "compiler-clean", until CI is green.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
