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
| M3 — Physical/virtual memory, kernel heap | **implemented** — frame map with kernel reservations, buddy allocator with per-CPU magazines, boot-time stress test under live interrupts, the higher-half switch (image in the top 2 GiB, identity map dropped and verified at boot), a kernel heap registered as `GlobalAlloc` over frame-backed regions, per-process address spaces (VAD tree + page tables over `KernelTableStore` + commit charge), demand paging and copy-on-write resolved by the page-fault path, pool accounting reported through the object manager, and a one-node NUMA list whose zonelist every allocation walks; the remaining step is the CI run that promotes the `pending-T2` rows to `tested-T2` |
| M4 — Object Manager and handles | **in progress** — handle tables with generation-checked handles, `DuplicateHandle` and inheritance rules in `crates/nolx-obj-core` (not yet compiled or run; see ROADMAP M4 checkpoint 1); object headers, namespace, Security Reference Monitor, and the T2 self test still to come |
| M5+ | planned |

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
  (`crates/nolx-mm-core`) passes 86 host tests -- buddy split/coalesce and randomized traffic,
  magazine conservation, paging arithmetic and the page-table walker, VAD split/coalesce and
  randomized model comparison, commit and pool accounting, heap integrity checks, and the
  NUMA node list with its zonelist ordering. The kernel's own boot path was rehearsed against
  the same crate on the host: a simulated 512 MiB machine, the same store layout, the same
  reservations, and 200,000 mixed-order allocations and frees with an audit every 4,096
  operations, ending with the free-frame count exactly where it started.
* **Machine-verified in the development sandbox (2026-10-10, updated 2026-10-11):** a Rust
  1.88 stable toolchain (rustc, cargo, clippy, rustfmt, and the `x86_64-unknown-none` standard
  library, assembled from the Rust distribution packages published to this sandbox's package
  registries) runs `make check` — host tests for every crate — plus `make bootcheck fscheck`,
  `make kernel` for the freestanding `x86_64-unknown-none` image, `scripts/verify-multiboot.py`,
  `cargo fmt --check`, `cargo clippy -D warnings`, and `scripts/check-unsafe.sh` clean.
* **Machine-verified in the development sandbox (2026-10-11):** QEMU does run here. The
  emulator is the npm-distributed `qemu-portable-linux-x64` build (QEMU 11.0.2), launched
  through a small glibc-2.38 compatibility shim because the sandbox's libc is older than the
  build's; the kernel boots under it and the boot smoke test's marker list and regexes pass
  locally, including the new heap, address-space, pool, and NUMA assertions. That is what
  makes the claims below local rather than CI-only -- and it is why a failure in this area can
  be reproduced in the sandbox before it is ever pushed.
* **Machine-verified in the development sandbox (2026-10-11):** the kernel heap (183
  allocations through the language allocator, a second and third region taken from the frame
  allocator, every region's metadata intact and the in-use bytes back to zero), and the
  address-space self test (two address spaces created and torn down; demand-zero faults;
  copy-on-write proven byte-for-byte across two spaces; a write to an `mprotect(PROT_READ)`
  page refused rather than copied; guard-page, file-backed, and out-of-address-space refusals;
  stack growth that coalesces into the stack's own descriptor; and the free-frame and
  page-table counts returning to exactly where they started -- reported as
  `NOLX: address space selftest: 2 space(s), 4 VAD(s), 6 fault cause(s) resolved (3
  demand-zero, 2 copy-on-write, 1 stack growth), 5 refusal(s); 6 page(s) mapped; free frames
  121253 -> 121253; page tables allocated 0 -> 7, released 0 -> 7; system PTE peak 36 KiB;
  audit ok`). The pools are reported through
  the object manager, and the machine's one NUMA node and its zonelist are printed at boot.
  Running the address-space self test under a stack-pointer probe found the space itself
  costing more stack than the kernel has: `AddressSpace<256>` is 26 KB, and moving one
  through a local, an `Option`, and a `teardown(self)` reserved 108,640 bytes of a 64 KiB
  kernel stack, writing past it into `.bss` silently. The space is now built in place and
  torn down through a borrow, which puts the same self test at 14,768 bytes of stack.
* **CI-verified (2026-10-10):** workflow run
  [38086516018](https://github.com/NeXuS4Dev/NOLX/actions/runs/38086516018) on revision
  `dc2d8d1` passed every job: boot layer, NOLXFS layout, host tests, fmt + clippy + unsafe
  audit, kernel build, and the QEMU boot smoke test. That boot test boots the kernel,
  asserts every boot marker, checks the wall clock advanced from its RTC anchor, boots the
  `#PF`/`#GP` fault images, and exercises the PIC/PIT fallback with the local APIC hidden.
  The eight HAL assertions that were `pending-T2` are now `tested-T2`.
* **CI-verified (2026-10-11):** workflow run
  [38125065287](https://github.com/NeXuS4Dev/NOLX/actions/runs/38125065287) on revision
  `2e7279b` passed every job. Its boot smoke test asserts the address-space self test's
  report -- two spaces, four described ranges, six faults resolved across the four causes,
  five refusals, six pages mapped, the free-frame and page-table counters back where they
  started, and the system PTE pool charged for every table -- and that run promotes
  MM-028/029/031/032/033/034 from `pending-T2` to `tested-T2`. Getting there is what the
  stack fix above is: the first CI run of that revision died on a `#GP` with the stack
  pointer 54 KiB deep, and QEMU's own exception log is what named the phase.
* **CI-verified (2026-10-10):** workflow run
  [38088471560](https://github.com/NeXuS4Dev/NOLX/actions/runs/38088471560) on revision
  `a025a42`, all six jobs green. Its boot smoke test asserts the frame allocator's markers
  and, separately, requires the timer tick count to have advanced during the boot-time
  allocation stress, which is what makes the stress a test *under interrupt load* rather
  than one that happened to run before the clock started. In that run the kernel described
  131040 frames, reserved 8916 for its own image and low memory, handed 122027 to the buddy
  allocator, and then took and returned 52851 blocks across four ticks, auditing as it went,
  with the free-frame count back at 122027. The four allocator assertions that were
  `pending-T2` are now `tested-T2`.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
