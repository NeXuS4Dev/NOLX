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
- 32-bit entry → identity-map first 1 GiB with 2 MiB pages → PAE → `EFER.LME` → paging →
  far jump to 64-bit → 64-bit GDT → stack → call Rust.
- Multiboot info parsing: memory map (`mmap`), module list (initrd), command line.
- **M1b:** Multiboot 2 + native UEFI (EFI stub, GOP framebuffer, UEFI memory map, RT
  services hand-off, Secure Boot–compatible PE layout).
- Tests: T2 `NOLX: boot layer reached long mode`; T1 multiboot header validation.

## M2 — HAL: console, tables, interrupts, time

- Serial (16550) + VGA text console behind one `HalConsole` trait; boot log from the first
  instruction that can print.
- 64-bit GDT (kernel/user code+data, TSS), IDT with 256 gates, ISR stubs preserving an
  `InterruptFrame`, exception dispatch with panic reporting.
- 8259 PIC remap, then APIC/x2APIC; EOI discipline tied to IPL.
- Timer: PIT calibration → LAPIC/HPET/deadline timer; monotonic and wall-clock sources.
- CPU feature detection (CPUID), `MSR` accessors, per-CPU data area (`gs` base).
- Tests: T2 timer ticks logged at expected rate; T2 `#PF`/`#GP` handler prints a register
  dump and halts cleanly.

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

## Known defect: the kernel entry point does not yet execute

Recorded 2026-10-10, M1.

`boot/boot.S` brings the machine to long mode and calls `nolx_kernel_main`. Nothing comes
back: no output, no panic message, no fault report. The image itself is verified correct --
`readelf` places every section at its intended address, `nm` shows both entry points, and
`objdump` shows the probe containing the port write that would print -- so the failure is in
executing Rust code reached from the trampoline, not in building or loading it.

What has been ruled out, with the evidence that ruled it out:

| Hypothesis | Evidence |
|---|---|
| The Multiboot magic was lost | Fixed: `__start32` zeroed BSS with `xorl %eax,%eax` before saving the magic. The mismatch warning is gone. |
| The boot code was never linked in | Fixed: `kernel/build.rs` now assembles `boot.S` into the kernel. |
| The image is loaded at the wrong address | Ruled out: `readelf -SW` shows every section where the linker script put it. |
| Port I/O was optimized away | Fixed: `options(nomem, preserves_flags)` licensed LLVM to delete an output-less `out`. That was a real bug; it was not this one. |
| The entry symbol is missing | Ruled out: `nm` shows `nolx_probe` and `nolx_kernel_main`. |

Still open: the interrupt descriptor table is never loaded, so any exception between the
`call` and the first instruction is an unrecoverable triple fault rather than a message.
That is the leading candidate and the first thing M4 should address -- a kernel that cannot
report its own faults cannot be debugged from inside.

The QEMU job reports these markers on every run rather than failing silently, so the gap
cannot be mistaken for progress.
