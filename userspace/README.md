# NOLX user space

Everything here runs in ring 3 and talks to the kernel through the ABI defined in
`crates/nolx-syscall`. Nothing in this directory is kernel code, and nothing here may assume
it can reach into one.

## Planned layout

| Directory | What it is | Milestone |
|---|---|---|
| `nolx-libc/` | C17 plus the POSIX Issue 8 headers, `crt0`, the dynamic loader, pthreads on native threads, locale, `dlopen` | M6 |
| `nolx-init/` | The first process: mounts, device nodes, service start, supervision | M6 |
| `nxsh/` | The shell. Conforming to the Shell & Utilities volume: expansions, quoting, job control, `set -e/-u/-o pipefail`, functions, traps, here-documents | M11 |
| `nolx-coreutils/` | The utilities the Shell & Utilities volume requires, with the documented options and exit statuses | M11 |
| `nolx-utils/` | `mkfs.nolxfs`'s companions: `fsck.nolxfs`, `mount`, `umount`, `fsstat`, `nolxfs-dump` | M8 |
| `nxd/` | The compositor | M12 |
| `nxwm/` | The window server | M12 |
| `nolx-ui/` | The toolkit: widget tree, text shaping, theming | M12 |
| `apps/` | Terminal, file manager, settings, text editor | M12 |

## Rules

**The ABI is the only interface.** User space reaches the kernel through `syscall` and
nothing else. No shared kernel headers beyond `crates/nolx-types` and `crates/nolx-syscall`,
which exist precisely so that both sides compile against the same definitions.

**libc owns `errno`.** The kernel returns `-errno` in `rax` and never touches thread-local
state. libc negates, stores, and returns `-1`. If kernel code ever writes `errno`, that is a
bug, because it would be writing into a thread's C runtime from outside it.

**Headers are generated where they can be.** The syscall table in
`crates/nolx-types/src/syscall.rs` is the source of truth; the C prototypes that can be
derived from it are generated, and `make check` fails if the two disagree. Hand-written
declarations are limited to the ones whose signatures POSIX specifies in ways a table cannot
express.

**The shell is a conformance target, not a convenience.** `nxsh` is written against the
Shell & Utilities volume and tested against it. A shell that is 95% compatible is a shell
whose missing 5% turns out to be whatever `configure` scripts use.

**Portability is source-level.** NOLX does not run binaries built for another system, and
does not try to. What POSIX buys is that a program written against the standard can be
*recompiled* for NOLX. That is the promise, and it is a useful one; pretending to more would
mean emulating someone else's ABI, which is how a system ends up inheriting another
system's bugs.

## Building user space

User-space crates are ordinary host-buildable Rust and C, cross-compiled for NOLX once the
kernel can load an ELF (M6). Until then the libc and the tools that do not need a kernel --
`mkfs.nolxfs` and friends -- build and test on the host, which is why `mkfs.nolxfs` lives in
`tools/` rather than here.
