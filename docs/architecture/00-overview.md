# NOLX architecture overview

Seven documents, one system. Read them in order; each one assumes the previous.

| Document | What it settles |
|---|---|
| [01 — Kernel model](01-kernel-model.md) | The layered structure: HAL, Executive, Kernel core. Why POSIX lives in a subsystem and not in the kernel core. |
| [02 — POSIX subsystem](02-posix-subsystem.md) | How a POSIX process, signal, file, and directory are represented on top of the executive objects, and what the syscall boundary actually looks like. |
| [03 — Memory manager](03-memory-manager.md) | Physical frames, virtual address descriptors, the working set, and where copy-on-write is implemented. |
| [04 — I/O manager and drivers](04-io-manager-drivers.md) | Device object stacks, the I/O Request Block, driver loading, and the caching manager. |
| [05 — IPC and ports](05-ipc-ports.md) | Message ports, the port namespace, and how a user-mode service such as the window server is addressed. |
| [06 — Scheduler](06-scheduler.md) | Priority levels, quantum, decay, and how POSIX scheduling policies map onto them. |
| [07 — GUI and window server](07-gui-window-server.md) | The compositor, the window server protocol, and why the GUI is a user-mode service rather than a kernel component. |

## The one-paragraph version

NOLX is a hybrid kernel. A small kernel core provides dispatching, trap dispatch,
interrupt priority levels, and the synchronisation primitives. On top of it sits the
Executive: an Object Manager, a Memory Manager, an I/O Manager with driver stacks, a
Cache Manager, a Security Reference Monitor, and IPC ports. That split is the one
described in Microsoft's public kernel-mode driver documentation, and NOLX adopts it
because it puts the parts that drivers need to touch behind a stable interface.

POSIX is not in the kernel core. It is a subsystem in `kernel/src/psx` that translates the
executive's object and message primitives into what *The Open Group Base Specifications,
Issue 8* requires, and translates POSIX errors back out. Every observable behaviour a
POSIX program depends on is produced there, which is what lets the conformance record point
at one directory.

The filesystem, NOLXFS, is a filesystem driver in the I/O Manager's stack. Its on-disk
format is specified in `docs/fs/nolxfs-format.md` and generated into both Rust and C from
`tools/nolxfs-codegen/nolxfs_format.py`.

The GUI is a user-mode service talking over IPC ports, exactly as a POSIX server subsystem
does. Nothing about it is privileged.

## Reading the code

```
boot/            the Multiboot trampoline: protected mode to long mode to the kernel
kernel/
  src/hal/       ports, MSRs, CPUID, the console, Multiboot parsing
  src/mm/        frame map, and later paging and the VAD tree
  src/obj/       object headers and the type table
  src/exec/      processes, threads, and the priority model
  src/psx/       the POSIX subsystem and the syscall dispatch table
  src/io/        the I/O manager, device stacks, and the IORB
crates/
  nolx-types/    errno, POSIX scalars, the syscall table: shared by kernel and user space
  nolx-syscall/  the user/kernel ABI
  nolxfs/        NOLXFS on-disk structures, B-tree, object map, mkfs
  nolx-vfs/      pathname resolution, the mount table, descriptor tables
tools/
  nolxfs-codegen/  the single source of truth for the on-disk layout
  mkfs-nolxfs/     mkfs.nolxfs
```
