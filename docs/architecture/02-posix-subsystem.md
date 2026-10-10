# The POSIX Subsystem

This document is the most important one in the tree, because it is where the project's two
goals meet: a Windows-shaped kernel on the inside, a UNIX system on the outside.

**Source of truth for behaviour:** *The Open Group Base Specifications, Issue 8*
(IEEE Std 1003.1-2024, published 2024-06-14). Where this document and the specification
disagree, the specification wins and this document is a bug.

## 1. The architectural move

NT delivered POSIX by putting POSIX semantics in a *subsystem* rather than in the kernel's
core. NOLX does the same, with one change: the subsystem lives in the kernel address space
(`kernel/src/psx/`) so a `read(2)` costs a `syscall` and not two context switches.

```
libc: read(fd, buf, n)
   -> syscall(SYS_read)
      -> psx::dispatch()            # POSIX subsystem: fd table, blocking policy, EINTR
         -> IoReadFile(file_object) # executive: IORB down the device stack
            -> NOLXFS driver        # or tmpfs, or a socket, or a pty
```

The subsystem owns everything POSIX defines that the executive does not naturally have:

| POSIX concept | Executive substrate |
|---|---|
| file descriptor | index into a per-process descriptor table pointing at a *file object* |
| process | process object + POSIX personality block (uids, pgid, sid, signal state) |
| thread | thread object + pthread control block |
| signal | queued APC with a signal record; delivery at IPL 1 |
| `waitpid` | wait on a process object's termination event + reap queue |
| pipe / FIFO / socket | file object over an executive stream object |
| SysV / POSIX IPC | named objects in the object namespace + waitable state |
| terminal line discipline | a filter driver stacked above the console/pty device |
| `mmap` of a file | section object mapped into the process VAD tree |
| credentials | security token, projected from POSIX uid/gid/mode |

The rule of thumb: **the executive provides mechanism, the subsystem provides POSIX
semantics.** If a behaviour is specified in Issue 8, its code lives in `psx/`.

## 2. The ABI

* Instruction: `syscall`. Return: `sysret`. `int 0x80` is *not* supported.
* Numbering: `rax` = syscall number (see `crates/nolx-types/src/syscall.rs`).
* Arguments: `rdi, rsi, rdx, r10, r8, r9` (max 6). `rcx`/`r11` are clobbered by the
  instruction itself and must not carry arguments.
* Return: `rax`. Non-negative is success. Negative is `-errno`. There is no carry-flag
  convention; libc turns the negative into `errno` and returns `-1`.
* `errno` lives in the thread's TLS block; libc owns it, the kernel never touches it.
* Restart: a syscall interrupted by a signal returns `-EINTR` unless the call is in the
  restartable set and `SA_RESTART` was set, in which case the kernel restarts it by
  rewinding `rip` to the `syscall` instruction. This is decided in `psx/`, because
  `SA_RESTART` is a POSIX concept.
* Red zone: **not** available in the kernel. Kernel code is built with
  `-disable-redzone`; user code may use it.
* User pointers: every user pointer argument is validated once and copied, or accessed
  through `UserRef<T>`/`UserSlice` helpers that do SMAP-safe probing. Validation happens
  before any sleep, so a concurrent `munmap` cannot turn a checked pointer into a stale
  one (the classic double-fetch race).

### Why not `vfork` as a syscall

POSIX specifies `vfork` with semantics ("behaviour is undefined if the child modifies
anything but a variable of type `pid_t`") that Issue 8 retained for compatibility. NOLX
implements it on top of the same COW primitive as `fork`, which makes the undefined
behaviour well-defined and harmless. Recorded in `docs/testing/deviations.md` as a
*strengthening*, not a deviation.

## 3. Signals

Signals are the hardest part of POSIX to get exactly right, so they get their own rules.

* A signal is a record queued to a *process* (process-directed) or a *thread*
  (thread-directed). Pending signals are a bitmask plus a queue for `sigqueue` payloads.
* Delivery happens at IPL `APC` on a chosen thread, at a well-defined point: on return
  from a trap, on return from a wait, or by forcing a thread out of a wait.
* Blocking is a per-thread `sigprocmask`. `SIGKILL` and `SIGSTOP` cannot be blocked,
  caught, or ignored.
* `SA_RESTART`, `SA_SIGINFO`, `SA_NOCLDSTOP`, `SA_NOCLDWAIT`, `SA_RESETHAND`,
  `SA_NODEFER`, `SA_ONSTACK` all implemented, including the interaction matrix with
  `waitpid` and job-control stops.
* Async-signal-safety: the safe-function list from Issue 8 is enforced by construction —
  the signal frame builder and the handler trampoline call nothing else.
* Real-time signals (`SIGRTMIN..SIGRTMAX`) queue with payloads and are delivered in
  priority order.
* Standard dispositions and default actions (terminate / core / ignore / stop / continue)
  including the `SIGCHLD` `SIG_IGN` semantics that suppress zombie creation.

## 4. Object Manager (as POSIX needs it)

POSIX names are paths; the executive names are `\Device\...`. The subsystem maintains the
bridge:

* The POSIX root directory is a handle held by the process (`chroot` swaps it).
* Pathname resolution (`crates/nolx-vfs`) walks POSIX paths, resolves symlinks with a
  hard loop limit of 40 (`SYMLOOP_MAX`), honours `O_NOFOLLOW`, `AT_SYMLINK_NOFOLLOW`,
  `AT_EMPTY_PATH`, and `..`-at-mount-point rules.
* File descriptors are indices into a growable table with close-on-exec and
  `O_CLOEXEC`/`FD_CLOEXEC` handling at `exec`.
* `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD*)` share file objects and file offsets exactly as
  specified (shared offset for duplicated descriptors, independent for separate `open`s).

## 5. Credentials and permissions

POSIX has uid/gid/mode/ACL-lite. The SRM has tokens, SIDs, and ACLs. Rather than run two
permission systems, NOLX projects one into the other:

```
uid/gid  ->  SID-like principal  (NOLX_UID(uid), NOLX_GID(gid))
mode     ->  3 ACEs (owner, group, other) + the special bits
setuid/setgid/sticky -> ACE flags on the object
root     ->  SeBackupPrivilege / SeTakeOwnershipPrivilege / bypass on DAC checks
```

`access(2)`, `faccessat(2)`, and the `open(2)` mode checks all call `SeAccessCheck` with a
token *restricted to the real uid/gid* when the effective id differs — which is exactly
what `access(2)` is specified to do. `stat(2)` projects the descriptor back to mode bits.

Consequence: `getfacl`/`setfacl` are not invented, but a future ACL model can be layered
without changing POSIX-visible behaviour.

## 6. Blocking, cancellation, and EINTR

Every POSIX blocking call is implemented as a wait on a dispatcher object with a
cancellation source:

* a signal pending for the thread,
* a `pthread_cancel` request (POSIX cancellation points are exactly the Issue 8 list),
* a timeout (`EAGAIN`/`ETIMEDOUT` as specified per function),
* or an explicit close of the descriptor by another thread (which wakes waiters with
  the behaviour Issue 8 specifies for that case).

`psx::blocking::wait()` is the single choke point, so the EINTR matrix is written once and
tested once.

## 7. Conformance tracking

`docs/testing/assertions/` holds one file per specification area. Each assertion is a
line: `ID | spec reference | status | test`. Statuses: `unimplemented`, `implemented`,
`tested-T1`, `tested-T2`, `tested-T3`. The dashboard in `docs/testing/CONFORMANCE.md` is
generated from those files by `tools/conformance-report.py`, so it cannot drift from the
data.
