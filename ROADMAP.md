# NOLX Roadmap

Milestones are ordered by dependency, not by excitement. A milestone is **done** only when
(a) the code exists, (b) the tests listed under it pass in CI, and (c) the specification
assertions it claims are recorded in `docs/testing/assertions/`.

Verification tiers used below:

* **T1 host** — `cargo test` on the build machine (pure logic, `no_std`-compatible crates).
* **T2 boot** — QEMU boots the kernel and the serial log contains an expected marker.
* **T3 suite** — an external conformance/functional suite runs *on* NOLX (Open POSIX Test
  Suite, LTP-derived cases, our own `nolx-posix-tests`).

---

## M0 — Foundations (done)

Freeze the decisions that are expensive to change later.

- Kernel model: HAL / Executive / Kernel, IPL levels, driver+device objects,
  POSIX subsystem. → `docs/architecture/01-kernel-model.md`
- User↔kernel ABI: `syscall` instruction, register mapping, error convention, TLS.
  → `crates/nolx-syscall`, `docs/architecture/02-posix-subsystem.md`
- Syscall numbering scheme with a permanent reservation policy (numbers are never reused).
  → `crates/nolx-types/src/syscall.rs`
- NOLXFS on-disk format v1 frozen and generated from one source of truth.
  → `tools/nolxfs-codegen`, `docs/fs/nolxfs-format.md`
- Tests: T1 layout/offset invariants; boot layer assembles and links.

## M1 — Boot layer

Get from firmware to `nolx_kernel_main` in 64-bit long mode.

- Multiboot 1 header (QEMU `-kernel` fast path) + ELF loading.
- 32-bit entry → identity-map first 2 GiB with 2 MiB pages → PAE → `EFER.LME` → paging →
  far jump to 64-bit → 64-bit GDT → stack → call Rust.
- Multiboot info parsing: memory map (`mmap`), module list (initrd), command line.
- **M1b:** Multiboot 2 + native UEFI (EFI stub, GOP framebuffer, UEFI memory map, RT
  services hand-off, Secure Boot–compatible PE layout).
- Tests: T2 `NOLX: boot layer reached long mode`; T1 multiboot header validation.

## M2 — HAL: console, tables, interrupts, time (done)

- Serial (16550) + VGA text console behind one `HalConsole` trait; boot log from the first
  instruction that can print.
- 64-bit GDT (kernel/user code+data, TSS), IDT with 256 gates, ISR stubs preserving an
  `InterruptFrame`, exception dispatch with panic reporting.
- 8259 PIC remap, then APIC/x2APIC; EOI discipline tied to IPL.
- Timer: PIT calibration → LAPIC/HPET/deadline timer; monotonic and wall-clock sources.
- CPU feature detection (CPUID), `MSR` accessors, per-CPU data area (`gs` base).
- Tests: T2 timer ticks logged at expected rate; T2 `#PF`/`#GP` handler prints a register
  dump and halts cleanly.

### M2 implementation checkpoint — 2026-10-10

The single-CPU HAL now discovers ACPI RSDP/root tables, MADT topology, and HPET metadata; it
initializes the HPET counter when its register block is in the early uncached mapping; and it
programs the local APIC in xAPIC or x2APIC mode plus validated I/O APIC routes. MADT NMI
routes and ISA overrides are decoded, legacy redirections start masked, and the local APIC
TPR follows software IPL while APIC EOI is issued before IPL restoration. The APIC timer is
calibrated against HPET; an invariant-TSC deadline source is selected when available, with a
periodic LAPIC timer otherwise. The 8259/PIT remains a fallback when firmware, APIC, or
calibration setup is unavailable. HPET also backs monotonic time (including extension of
32-bit counter wraps); the RTC wall-clock anchor stores its matching monotonic baseline.

The host assertion database covers portable ACPI/APIC parsing and timer arithmetic. CI boots
the normal APIC/HPET timer path, asserts 100 timer interrupts, exercises a separate PIC/PIT
fallback boot with local APIC hidden, and boots `#PF`/`#GP` fault images. Those QEMU checks
are no longer pending: workflow run
[38086516018](https://github.com/NeXuS4Dev/NOLX/actions/runs/38086516018) on revision
`dc2d8d1` passed all six jobs -- boot layer, NOLXFS layout, host tests, fmt + clippy + unsafe
audit, kernel build, and the QEMU boot smoke test -- and the eight HAL assertions that were
`pending-T2` are recorded as `tested-T2` in `docs/testing/assertions/hal.md`. The HAL also
retains the earlier verified console contract, TSS/IST stacks, complete IDT,
register-preserving interrupt stubs, and bootstrap GS-based per-CPU block.

M2 is **done**: the code exists, its listed tests pass in CI, and its assertions are recorded
in the database.

### M2 verification checkpoint — 2026-10-10

The revision named above is the first on which every M2 claim has a passing machine record
behind it. The QEMU boot smoke test asserts the boot markers themselves -- not a summary of
them -- and the serial log for each run is archived as a build artifact and posted on the
pull request, so the evidence can be read rather than trusted.

## M3 — Memory management

- Physical frame allocator: boot-map from the multiboot memory map, buddy allocator with
  per-CPU magazines, NUMA-node aware node list.
- 4-level paging: kernel address space, per-process page tables, copy-on-write, demand
  paging, large-page promotion/demotion.
- Kernel heap: size-class segregated pools + a general allocator; `GlobalAlloc` impl.
- **Higher-half kernel mapping** and switch off identity mapping (KASLR-ready layout).
- Address-space description: **VAD tree** (Windows-style virtual address descriptors)
  rather than a flat VMA list — region objects carry protection, type, and backing.
- Pool accounting: nonpaged pool, paged pool, and per-process commit charge with
  quota objects.
- Tests: T1 buddy allocator + VAD tree property tests (split/coalesce invariants,
  fragmentation bounds, overlap rejection); T2 stress allocation under interrupt load.

M3 is **in progress**. The algorithms are done and host-tested; the parts that need a
machine to run on are partly wired up.

### M3 progress checkpoint — 2026-10-10

Landed:

* `crates/nolx-mm-core` -- the buddy allocator, per-CPU frame magazines with a zeroed-frame
  pool, the four-level paging arithmetic and page-table walker, the VAD tree, the commit and
  pool accounting, and the boundary-tagged kernel heap. `no_std`, `forbid(unsafe_code)`, and
  therefore the same code under `cargo test` on the host as in the kernel, reached through
  the `BlockStore` and `TableStore` traits.
* `kernel/src/mm` -- the frame map gains the reservations that make it honest (low memory,
  the kernel image and BSS, the static heap arena, and frames outside the boot identity map),
  the buddy allocator is built from runs of free frames, and the boot processor's magazine
  sits in front of it. `kernel/src/mm/store.rs` is the `BlockStore` over the direct map: the
  free-list links are written into the free frames themselves.
* `kernel/src/mm/selftest.rs` -- the boot-time stress test: single frames in both
  directions, then a deterministic mixed-order walk, audited every few thousand operations,
  running until the timer has ticked so the allocations demonstrably happen under interrupt
  load. It ends by requiring the free-frame count to return to where it started.

The four allocator assertions that were `pending-T2` are now `tested-T2`, recorded against
workflow run 38088471560 on revision `a025a42` -- the same bar M2 was held to: the claim, the
marker that carries it, and the run that shows it happening.

Landed since the checkpoint above (2026-10-11):

MM-030 is `tested-T2`: the switch marker is asserted by the `boot-smoke` job, and run
38114869085 on revision `b9686fb` shows the machine reporting it -- "image window at
0xffffffff80000000, dropped 1 identity-map PML4 entry/entries; identity map gone=true, direct
map reachable=true, image window reachable=true" -- with the frame map (131040 frames, 8935
taken back for the kernel), the buddy allocator (122008 frames), the 100-tick marker, and the
mm self-test all still passing afterwards. The same run re-stated HAL-008, whose #GP test had
been loading an LDT selector and so had been relying on the identity map still being there.

* the higher-half switch -- the image is linked in the top 2 GiB at
  `0xffff_ffff_8000_0000` and mapped there by the trampoline in PML4 slot 511, the direct map
  carries the low 2 GiB of RAM and the APIC/HPET windows at `0xffff_8000_0000_0000`, and the
  kernel drops the boot identity map once its own GDT, IDT, and stack are live. The switch
  verifies itself: it probes a direct-map frame, an image-window address, and a low physical
  address that must no longer translate, and the boot stops if any probe answers wrong. The
  kernel is built with `-C code-model=kernel`, which is what makes the top-2-GiB address
  reachable; `nolx_mm_core::paging` pins that with a compile-time assertion, so a layout that
  the code model cannot address fails to build instead of failing to link.
* `kernel/src/mm/tables.rs` -- `KernelTableStore`, the `TableStore` over frames from the
  frame allocator, plus `activate`, `invalidate_page`, and the walker probes. This is the
  mechanism per-process address spaces will be built from.
* the image-window layout as a flat-copy contract: `scripts/verify-multiboot.py` now checks
  that every stored section sits at `file_offset = copy_base + (load - load_base)` and that
  the window covers the whole kernel region, because a loader that copies the image as one
  block (QEMU's Multiboot loader does) will otherwise load the wrong bytes into the boot
  layer's page tables. The boot layer's zero-initialised state is stored in the file rather
  than as a NOBITS hole for the same reason.
* `kernel/src/mm/phys.rs` (the two windows and the physical-to-pointer conversion) and
  `kernel/src/mm/pool.rs` (the charged pools, not yet wired to the object manager).

Still to do in M3:

* per-process address spaces over the frame allocator (MM-028, MM-029): a VAD tree per
  process, page tables built with `KernelTableStore`, and address-space switching;
* the kernel heap registered as a `GlobalAlloc`, over regions taken from the frame allocator;
* demand paging, copy-on-write, and the four fault causes from the memory-manager design;
* NUMA node lists, and the pool accounting wired to the object manager.

## M4 — Object Manager and handles

The centre of the design. Everything is an object; everything reachable is reachable
through a handle.

- Object headers (body, type, refcount, security descriptor, name, quota charge).
- Object types with create/open/parse/close/delete callbacks.
- Handle tables: per-process table, inheritance flags, `DuplicateHandle` semantics,
  handle auditing.
- Object namespace: root directory, `\Device`, `\FileSystem`, `\BaseNamedObjects`,
  `\NOLX`, symlinks, per-session directories, private namespaces.
- Security Reference Monitor: SID/ACE/ACL evaluation, tokens (user, group, privilege,
  integrity), `SeAccessCheck`, mandatory-label support, auditing.
- Tests: T1 refcount lifecycle (no leak/no double free under fuzzed open/close),
  handle-table allocation, ACL evaluation vectors.

## M5 — Executive core: processes, threads, dispatcher

- Process object: address space, handle table, token, job membership, PEB-equivalent.
- Thread object: context, kernel/user stacks, TEB-equivalent, APC queues.
- Dispatcher: ready queues per priority band, 32 priority levels with real-time and
  dynamic bands, quantum and priority-boost rules, per-processor run queues, idle thread.
- Dispatcher objects: event, mutex (with owner + recursion), semaphore, timer, thread
  join — all waitable through one `WaitForMultipleObjects`-shaped primitive.
- IPL model + **DPC** (deferred procedure calls) + system worker threads + queued locks
  and pushlocks.
- Scheduler: SMP with per-CPU affinity, load balancing, group scheduling for job objects.
- Tests: T1 scheduler simulation (priority inversion bounded by boost, FIFO within band,
  no starvation); T2 round-robin of N threads prints interleaved output.

## M6 — User mode, ELF, and the syscall ABI

- Ring-3 transition: `syscall`/`sysret` fast path, `TSS.RSP0`, IST stacks for `#DF`/`#MC`/`#NMI`.
- ELF64 loader: `PT_LOAD`, `PT_INTERP`, `PT_TLS`, `PT_GNU_STACK`, ASLR, auxv.
- `nolx-libc`: C17 + POSIX headers generated from the syscall table, `crt0`, dynamic
  loader, pthreads on native threads, locale, `dlopen`.
- Process creation model: `nxposix_spawn` primitive that the POSIX subsystem builds
  `fork`/`vfork`/`posix_spawn`/`exec*` on top of, including a true COW `fork`.
- Tests: T2 "hello world" ELF runs and exits with a status the parent observes;
  T1 syscall table uniqueness/ABI round-trip.

## M7 — I/O Manager, VFS, and the first filesystems

- **IORB** (I/O Request Block — our IRP): stack locations, major/minor function codes,
  completion ports, cancel routines, synchronous and asynchronous paths, `IoStatus`.
- Driver objects with dispatch tables; device objects forming stacks; attach/detach;
  driver unload with reference tracking.
- Cache Manager: shared cache maps, lazy writer, read-ahead, `MmFlushSection`.
- VFS layer: vnode-equivalent *file objects*, namei/pathname resolution (symlink loop
  limit, `O_NOFOLLOW`, `..` at mount points), mount table with propagation classes,
  descriptor table with `fcntl` duplication semantics, `umask`, `chroot`/pivot-root
  equivalent.
- Filesystems: `devfs`, `tmpfs`, `initrd` (cpio/tar) reader, then **NOLXFS**.
- Tests: T1 pathname-resolution state machine against a scripted filesystem tree
  (every POSIX `pathconf`/`ENOENT`/`ELOOP` case); T2 read/write a file in tmpfs.

## M8 — NOLXFS

APFS-shaped design, our own implementation: container → checkpoint ring → live superblock
→ object map → volume → B-tree of typed records, with copy-on-write, snapshots, and a
space manager. Full spec in `docs/fs/nolxfs-format.md`.

- On-disk objects with 32-byte headers and Fletcher-64 checksums; omap (oid,xid)→paddr.
- B-tree: node split/merge, TOC layout, keyed by `(parent inode, name hash, name)`.
- Record types: inode, dentry, xattr, sibling-link, data stream, physical extent, fext.
- Copy-on-write with transaction IDs; checkpointing; crash recovery by replaying the
  checkpoint ring to the newest valid superblock.
- Space manager: chunk-info blocks + allocation bitmaps; reaper for deferred frees.
- Snapshots: per-volume snapshot metadata tree, mount-at-snapshot, delete/rename.
- Atomic write batches (metadata journal) so a power cut never tears a transaction.
- `mkfs.nolxfs`, `fsck.nolxfs`, `nolxfs-dump`, and a FUSE-less host mounter for testing.
- Tests: T1 mkfs→mount→create→write→fsync→remount round trip on an in-memory device,
  crash injection at every write boundary with recovery asserted; fsck finds and repairs
  injected corruption; snapshot isolation property tests.

## M9 — Devices and drivers

- PCI/PCIe enumeration, BAR assignment, MSI/MSI-X, capability walking.
- Storage: AHCI (SATA), NVMe, virtio-blk; block layer with partitions (GPT), request
  merging, per-device queues.
- Input: PS/2, USB (xHCI) → HID → evdev-equivalent event objects.
- Networking: virtio-net, e1000, then a driver framework (`nolx-df`) so third-party
  drivers are a device object + dispatch table, not kernel patches.
- Power: ACPI table parsing, `_STA`/`_CRS`, S3/S5, device power policy.
- Tests: T2 boot with each storage backend and checksum a written file.

## M10 — POSIX conformance waves

Each wave is a numbered, testable set of System Interfaces functions.

- **W1 file & directory:** `open`/`read`/`write`/`pread`/`pwrite`/`lseek`/`close`/`dup*`,
  `stat`/`fstat`/`lstat`/`fstatat`/`statvfs`, `mkdir`/`rmdir`/`unlink`/`rename`/`link`/
  `symlink`/`readlink`/`chmod`/`chown`/`utimensat`/`truncate`/`access`/`faccessat`,
  `opendir`/`readdir`/`seekdir`/`telldir`, `fcntl`, `ioctl`, `umask`, `chdir`/`chroot`,
  `pathconf`/`sysconf`/`confstr`.
- **W2 process & credentials:** `fork`/`exec*`/`_exit`/`wait`/`waitpid`/`waitid`,
  `getpid`/`getppid`/`getuid`/`setuid`/`getgid`/`setgid`/`getgroups`/`setpgid`/`getsid`/
  `setsid`/`setuid` family with saved-set-user-ID semantics, `nice`, `getrlimit`/
  `setrlimit`, `getrusage`, `times`, `alarm`, `gettimeofday`/`clock_gettime`/
  `clock_nanosleep`/`nanosleep`, `umask`, `exit`/`atexit`, `posix_spawn`.
- **W3 signals:** full `sigaction` semantics (SA_SIGINFO, SA_RESTART, SA_NOCLDSTOP,
  SA_RESETHAND, SA_NODEFER), `sigprocmask`/`sigwait`/`sigsuspend`/`sigpending`/
  `sigqueue`/`sigtimedwait`, standard signal dispositions, `kill`/`killpg`/`raise`,
  async-signal-safety list, signal stack (`sigaltstack`), `pthread_kill`/`pthread_sigmask`.
- **W4 terminal & process groups:** termios (canonical/raw, `V*` indices, baud rates,
  `tcgetpgrp`/`tcsetpgrp`, flow control), controlling terminals, `pty`/`openpty`,
  job-control signals (`SIGTTIN`/`SIGTTOU`/`SIGTSTP`/`SIGCONT`), `ttyname`.
- **W5 IPC:** pipes, FIFOs, System V IPC (`msgget`/`msgsnd`/`msgrcv`/`semget`/`semop`/
  `semctl`/`shmget`/`shmat`/`shmctl`), POSIX IPC (`mq_*`, `sem_*`, `shm_open`/
  `shm_unlink`), `socketpair`, `select`/`pselect`/`poll`/`ppoll`/`epoll`-equivalent
  `nxevent`, `pipe2`, `dup3`.
- **W6 real-time & threads:** pthreads (mutex, cond, rwlock, barrier, spin, TLS,
  cancellation, `pthread_atfork`), `clock_*` clock set, `timer_create`/`timer_settime`/
  overrun counting, `sched_*` policies and `SCHED_FIFO`/`RR`/`OTHER`/`SPORADIC`,
  `mlock*`, `shm_*`, `aio_*`, `mmap`/`munmap`/`mprotect`/`msync`/`madvise`/`mlock`.
- **W7 network:** BSD sockets (`socket`/`bind`/`listen`/`accept`/`connect`/`send*`/
  `recv*`/`shutdown`/`getsockopt`/`setsockopt`), `getaddrinfo`/`getnameinfo`,
  `if_*`, routing socket or netlink-equivalent.
- **W8 dynamic loading & locale:** `dlopen`/`dlsym`/`dlclose`/`dlerror`, `iconv`,
  `setlocale`/`nl_langinfo`, wide chars, regex (`regcomp`/`regexec`), `glob`, `fnmatch`,
  `wordexp`, `fts`, `getopt`.
- Tests: T3 for every wave — Open POSIX Test Suite where it covers the area (it is
  POSIX.1-2001-era and stopped updating in 2013, so it is a floor, not a ceiling), plus
  our own assertion-per-spec-sentence suite for Issue 8 additions.

## M11 — Userspace and the shell

- `nxsh`: a conforming Shell & Utilities volume shell (expansion, quoting, job control,
  `set -e/-u/-o pipefail`, functions, traps, here-docs).
- `nolx-coreutils`: the utilities the Shell & Utilities volume requires, with the
  documented options and exit statuses.
- `nolx-init`, `getty`, service manager with dependency ordering and supervision.
- Package/port system with a reproducible build description.
- Tests: T3 shell passes the Shell & Utilities assertions; coreutils golden-output tests.

## M12 — GUI

- Display stack: KMS-equivalent modesetting, DRM-like buffer objects, virtio-gpu +
  native drivers, damage tracking, page-flip.
- **Compositor** in a user-mode server on IPC ports; clients share buffers via
  `nxshm`/dmabuf-equivalent; no client can scribble on another.
- Window server: window objects in the object namespace, focus/activation policy,
  input routing with grab semantics, HiDPI, accessibility hooks from day one.
- Toolkit (`nolx-ui`): retained-mode widget tree, text layout with shaping, theming.
- Reference apps: terminal, file manager, settings, text editor.
- Tests: T2 compositor boots to a desktop under QEMU with virtio-gpu; T1 compositor
  state machine (focus, stacking, damage regions).

## M13 — Robustness, security, and performance

- Verified syscall argument marshalling with explicit bounds (no TOCTOU on user pointers:
  fetch-once, then validate).
- Hardening: stack protector, CFI on the kernel, `noexec`/SMAP/SMEP/CET, KASLR,
  hardened usercopy, driver sandboxing option (user-mode drivers over ports).
- Lock-down and attestation, signed modules, secure boot chain.
- Performance targets and the benchmarks that define them (syscall round trip, page
  fault, file create/append, process spawn, context switch).
- Tests: T1/T2 fuzzing of the syscall boundary and NOLXFS image parser (crash-free
  requirement: a malformed image must never panic the kernel).

## M14 — Conformance posture

- Public conformance dashboard: per-function status generated from the assertion database.
- Run the Open POSIX Test Suite end to end in CI, publish the pass matrix.
- Document every deliberate deviation with its rationale in
  `docs/testing/deviations.md`.
- Evaluate what formal certification would require, and which gaps remain
  (certification is a legal/commercial process separate from engineering completeness).

---

## Explicitly out of scope

- Binary compatibility with Linux, macOS, or any other system's ABI. NOLX runs NOLX ELF
  binaries. Porting is a source-level exercise, which is what POSIX is for.
- Reading or referencing Linux source. NOLX is designed from the published specifications
  and from the FreeBSD and illumos codebases, plus Microsoft's published kernel-mode
  architecture guidance.

---

## Resolved: the kernel entry point did not execute

Recorded 2026-10-10, M1. Closed the same day.

`boot/boot.S` brought the machine to long mode and called `nolx_kernel_main`. Nothing came
back: no output, no panic, no fault report. The image was verified correct throughout --
`readelf` placed every section where the linker script put it, `nm` showed both entry
points, and `objdump` showed the probe containing the port write that would print -- so the
failure was in executing Rust reached from the trampoline, not in building or loading it.

The cause was one constant. `COM1_LSR` was `3`. Offset 3 from COM1's base port is the FIFO
control register, which is write-only on a 16550A; the line status register, the one with
the "transmit holding register empty" bit, is at offset 5 -- port `0x3FD`, which
`boot/boot.S` had been using correctly all along. So the kernel's first Rust function spun
forever on `while inb(0x3FB) & 0x20 == 0`, waiting for a bit a write-only register would
never report. No fault, no output, and nothing on the serial log to distinguish it from a
machine that had simply stopped.

What made it findable was refusing to guess. Three things were added to the boot layer:

| Step | What it established |
|---|---|
| Load an interrupt descriptor table before the first call into Rust | The silence was *not* a triple fault -- no exception ever fired |
| An empty Rust function, called before the printing probe | The trampoline could enter Rust and return, so the handoff was fine |
| The first sixteen bytes at the probe, printed as hex | `31 f6 eb 1c` -- `xor esi,esi` then a jump -- the transmit loop was compiled exactly as written, so the code was present and the loop was not terminating |

That leaves one possibility, and one port read to check.

The lesson is worth more than the fix: a kernel that cannot report its own faults cannot be
debugged from inside, and a boot log that is not posted where it can be read is the same as
no log. Both are now true in the other direction -- the IDT is loaded in the boot layer, in
assembly, so it works even when calling Rust is the thing that is broken, and the serial log
is posted to the pull request on every run.

The whole init sequence now runs under QEMU:

```
NOLX: nolx_probe reached (Rust is callable)
NOLX: nolx_kernel_main entered
NOLX 0.1.0 starting
multiboot: flags=0x24f mem_lower=639KiB mem_upper=523136KiB modules=0 mmap=168B@0x9000
mm: 7 regions, 511 MiB usable, 12288 MiB reserved, highest usable 0x1ffe0000
obj: registered 15 object types
exec: 32 priority levels, real-time band from 16, SCHED_FIFO priority range 0..15
psx: 229 syscall numbers assigned, 0 implemented, the rest return ENOSYS
io: I/O manager online, 16 major function codes, IORB is 656 bytes
NOLX: init sequence complete, halting the boot processor
```

Every one of those lines is now a hard gate in the boot smoke test.
