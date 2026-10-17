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
| `crates/nolx-exec-core/` | Host-testable process context: identity, address-space association, token, and per-process handle table |
| `crates/nolx-syscall/` | The user↔kernel ABI: register mapping, marshalling, dispatch table |
| `crates/nolxfs/` | NOLXFS on-disk format, Fletcher-64, B-tree, mkfs/fsck logic |
| `crates/nolx-vfs/` | I/O Manager (IORB, driver/device stacks, completion ports, cancellation), Cache Manager, VFS (`namei`, mount propagation, `pivot_root`, `fcntl`, `umask`, `chroot`), and filesystems (`devfs`, `tmpfs`, `initrd` cpio/tar, `NOLXFS` VFS) |
| `tools/mkfs-nolxfs/` | `mkfs.nolxfs` host tool |
| `tools/fsck-nolxfs/` | `fsck.nolxfs` host tool (check + safe repair) |
| `tools/nolxfs-dump/` | `nolxfs-dump` host tool (read-only dump/ls/cat/stat, the FUSE-less mounter) |
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
| M3 — Physical/virtual memory, kernel heap | **done** — frame map with kernel reservations, buddy allocator with per-CPU magazines, boot-time stress test under live interrupts, higher-half switch with the identity map dropped and verified, frame-backed `GlobalAlloc` heap, per-process address spaces with demand paging and copy-on-write, pool accounting, and a one-node NUMA list; all 11 boot assertions are `tested-T2` and full CI run [38162486568](https://github.com/NeXuS4Dev/NOLX/actions/runs/38162486568) passed all six jobs |
| M4 — Object Manager and handles | **done** — type lifecycle callbacks, generation-checked handle-table primitives, session directories, boundary-protected private namespaces, Security Reference Monitor, host tests, and expanded T2 boot selftests. All 14 assertions are recorded (9 `tested-T1`, 5 `tested-T2`, none pending), and CI run [38249363821](https://github.com/NeXuS4Dev/NOLX/actions/runs/38249363821) on revision `15aee68` passed all six jobs with every M4 boot marker asserted (see [ROADMAP M4](ROADMAP.md)). Security descriptors and names are out-of-line; per-body quota ownership remains deferred. |
| M5 — Executive core: processes, threads, dispatcher | **done** — host-tested process/thread, scheduler, dispatcher, DPC, and lock contracts; QEMU process/thread self-tests and the preemptive `ABCABCABCABC` trace pass in CI (see [run 38162486568](https://github.com/NeXuS4Dev/NOLX/actions/runs/38162486568)) |
| M6 — User mode, ELF, and syscall ABI | **done** — ring-3 `syscall`/`sysret` with TSS.RSP0 and per-thread GS discipline; ELF64 loader (`ET_EXEC`/`ET_DYN`, ASLR, `R_X86_64_RELATIVE`, auxv/initial stack); `nxposix_spawn`/`waitid`/`write`/`exit`/`getpid`; T2 ring-3 `hello world` exits 42 with a parent-observed status in CI; T1 syscall-table uniqueness and ABI round-trip (see [ROADMAP.md](ROADMAP.md)) |
| M7 — I/O Manager, VFS, and the first filesystems | **done** — IORB with 8-deep stack locations, completion routines, async completion ports, cancellation, and reference-tracked driver unload; Cache Manager (`SharedCacheMap`, read-ahead, lazy writer, `MmFlushSection`); VFS (`FileObject`, `namei`, `pathconf`, mount propagation classes, `pivot_root`, `fcntl`, `umask`, `chroot`); `devfs`, `tmpfs`, `initrd` (cpio/tar), and `NOLXFS` VFS driver; T1 pathname/VFS/I/O/filesystem suites (102 tests in `nolx-vfs`) and T2 boot self-tests + ring-3 `tmpfs` read/write round-trip in CI (see [run 38240528081](https://github.com/NeXuS4Dev/NOLX/actions/runs/38240528081) and [ROADMAP.md](ROADMAP.md)) |
| M8 — NOLXFS | **implemented** — COW container with checkpoint ring and atomic descriptor-last commits, omap, split/merge B-trees, space manager + snapshot-horizon reaper, snapshots with read-only mount-at-snapshot, xattrs, hard links, clone_file; `mkfs.nolxfs`/`fsck.nolxfs`/`nolxfs-dump` host tools; VFS dispatch for rmdir/link/symlink/readlink/unlink/rename/truncate; assertions in `docs/testing/assertions/nxfs.md` (NXFS-001…015). CI run [38359528564](https://github.com/NeXuS4Dev/NOLX/actions/runs/38359528564) on revision `be7730a` passed all six jobs, including the QEMU boot-smoke `nolxfs-m8 ok` marker. |
| M9 — Devices and drivers | **in progress** — native legacy/ECAM PCI transport and segment discovery, with evaluated root `_CRS` apertures for bounded PC/Q35 missing-BAR/nested-bridge allocation; multi-controller framework-bound virtio-blk/AHCI/NVMe with per-controller queued filesystem I/O, durable flushes, persisted-file/reboot tests and MSI/MSI-X; evaluated ACPI `_PRT`/current-link routing with native GSI programming and multi-adapter e1000 shared INTx, plus native virtio-net RX/TX with MSI-X and host-captured ARP exchanges; PS/2 keyboard/mouse IRQs with injected event checks; polled direct-root USB2 boot keyboard/mouse enumeration, ring-wrap and disconnect/reconnect DMA checks, with shared PS/2/USB device-owned kernel input objects, independent readers and overflow/stale-handle verification, plus actual ACPI S5 entry with QMP shutdown verification; bounded pure AML namespace and firmware `_STA`/`_CRS` evaluation with a status-gated catalog. general USB/HID, Executive/user input-object bindings and controller-framework integration, general hardware-field AML/PCI roots and link reprogramming, global power quiescence/S3 resume and nonzero-segment binding and failure-path lifecycle remain open; a persistent registry and healthy stop/restart/DMA reclamation now have executable five-driver tests, with owned MMIO window reuse and stale-access checks. See the [M9 completion gate](docs/development/m9-completion.md). |
| M10+ | planned |

M9 continuation (2026-10-16): native ECAM driver writes, AHCI MSI completions,
e1000 interrupt-driven receive, and injected PS/2 keyboard/mouse events pass in
[CI run 38953420334](https://github.com/NeXuS4Dev/NOLX/actions/runs/38953420334)
on `492d7a5` (all nine jobs). This is a verified integration batch, **not M9 completion**.
The [completion checklist](docs/development/m9-completion.md) records the remaining work.

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
* **CI-verified (2026-10-12):** workflow run
  [38249363821](https://github.com/NeXuS4Dev/NOLX/actions/runs/38249363821) on revision
  `15aee68` -- the tree that carries M4 checkpoint 6 -- passed all six jobs. Its QEMU boot smoke
  test asserted the five M4 T2 markers (`NOLX: obj selftest: 1 object, 2 handles`,
  `NOLX: obj session selftest: sessions 0 and 1 are isolated`,
  `NOLX: obj private namespace selftest: boundaries enforced, isolated roots`,
  `NOLX: obj type callback selftest: create/open/parse/close/delete invoked`, and
  `NOLX: srm selftest: 10 access checks (6 granted, 4 denied)`), and its host-test job ran the
  M4 T1 suite, including both seeded fuzz tests. That run is what promotes OBJ-006, OBJ-010,
  OBJ-011, OBJ-012, and OBJ-013 from `pending-T2` to `tested-T2`; this sandbox has no `rustc`
  and no QEMU, so those two jobs are read from the run rather than re-executed here, while
  `make bootcheck fscheck usercheck` and `scripts/check-unsafe.sh` were run here and passed.
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
