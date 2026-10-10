# NOLX Kernel Model

NOLX is a **hybrid kernel**: a microkernel-shaped core wrapped in a monolithic address
space. The shape is deliberately the one Microsoft documented for the Windows kernel,
because that shape is what lets an object-based executive stay fast (no message pass for
hot paths) while keeping the discipline of small, replaceable components.

```
+---------------------------------------------------------------------------+
|  user mode                                                                |
|  +----------------+  +----------------+  +------------------------------+ |
|  | nxsh, coreutils|  | window server  |  | user-mode drivers (optional) | |
|  +----------------+  +----------------+  +------------------------------+ |
|  |                        nolx-libc  (C17 + POSIX Issue 8)               | |
+--|------------------------------------------------------------------------|-+
   |                       syscall / sysret boundary                        |
+--|------------------------------------------------------------------------|-+
|  | POSIX SUBSYSTEM  (kernel/src/psx)                                     | |
|  | fd tables, signals, waitpid, termios, SysV+POSIX IPC, uids/gids       | |
|  |   -- translates POSIX semantics into executive object operations      | |
+--|------------------------------------------------------------------------|-+
|  |                        NOLX EXECUTIVE                                 | |
|  | Object Mgr | Memory Mgr | I/O Mgr | Cache Mgr | Security Ref Monitor  | |
|  | IPC Ports  | Process/Thread Mgr | Config Mgr | Power Mgr | PnP Mgr    | |
+--|------------------------------------------------------------------------|-+
|  |                        NOLX KERNEL (core)                             | |
|  | dispatcher & dispatcher objects | trap dispatch | IPL | DPC | spinlocks|
+--|------------------------------------------------------------------------|-+
|  |                     HARDWARE ABSTRACTION LAYER                        | |
|  | CPU | APIC | timers | PCI config | console | platform firmware         | |
+---------------------------------------------------------------------------+
                              hardware
```

## 1. Why hybrid, and what that buys us

A pure microkernel would put the filesystem, drivers, and POSIX semantics in servers and
pay a context switch per operation. A pure monolith would put everything in one address
space with no enforced boundaries and no replaceable components. NOLX takes the middle
road:

* **One address space for the kernel.** Executive components and in-kernel drivers call
  each other directly. No IPC on the file-read path.
* **Enforced structure instead of enforced isolation.** Every executive service is a
  module with a declared interface. Drivers are *objects* with dispatch tables, not
  patches. A component can be replaced or moved to user mode (M13 offers user-mode
  drivers over IPC ports) without touching its clients, because clients only ever hold
  handles and call dispatch routines.
* **Everything is an object with a lifetime.** Refcounting, handle tables, security
  checks, and auditing are implemented once, in the Object Manager, and inherited by
  files, sockets, processes, threads, ports, timers, and device nodes.

The cost is that a bad in-kernel driver can still take the system down. That is accepted,
measured (M13 adds user-mode drivers), and mitigated by Rust's type system rather than by
process boundaries.

## 2. Interrupt Priority Levels (IPL)

NOLX uses the Windows IRQL concept under a different name, because the POSIX layer needs
it to reason about what may block.

| IPL | Name | Rules |
|---|---|---|
| 0 | `PASSIVE` | Thread context. May block, may touch paged pool, may wait. |
| 1 | `APC` | Asynchronous procedure calls run here. Still may not block. |
| 2 | `DISPATCH` | Scheduler and DPCs. No blocking, no paged pool, no waiting. |
| 3–26 | `DEVICE(n)` | Device interrupts. Higher `n` preempts lower. |
| 31 | `HIGH` | Machine check, NMI, spinlock deadlock detection. |

Rules that are enforced by the type system where possible and by assertions always:

* Code running above `PASSIVE` **must not** block. `KeWaitFor*` at IPL > 0 is a bug check.
* A spinlock raises IPL to the lock's level; nested acquisition must be non-increasing.
* Page faults are only legal at IPL ≤ 1; a fault above that is fatal.
* POSIX signals are delivered at `APC` level, which is what makes signal delivery
  interruptible-but-ordered (see `02-posix-subsystem.md`).

## 3. Executive components

### Object Manager (`kernel/src/obj`)
Object headers, object types with callbacks, handle tables, the object namespace, and
quota charging. Detailed in `02-posix-subsystem.md` §4 and
[`kernel/src/obj/mod.rs`](../../kernel/src/obj/mod.rs).

Every object body starts with a common header:

```
+0   type_index      u16      index into the global object-type table
+2   flags           u16      permanent / exclusive / case-insensitive / audit-on-close
+4   refcount        i32      pointer references (kernel pointers)
+8   handle_count    i32      handle references
+12  body_size       u32
+16  security_desc   *SECURITY_DESCRIPTOR
+24  name            *OBJECT_NAME (in the namespace, if named)
+32  quota_charge    *EPROCESS   process charged for this object
+40  body            ...       type-specific data
```

Refcount rules mirror the executive's: a handle reference implies a pointer reference, so
`handle_count <= refcount` is an invariant checked on every release.

### Memory Manager (`kernel/src/mm`)
Physical frames, page tables, VAD trees, pools, commit accounting, working sets, and the
page-fault path. Detailed in `03-memory-manager.md`.

### I/O Manager (`kernel/src/io`)
Driver objects, device object stacks, and the **IORB** — the I/O Request Block, our IRP.
Detailed in `04-io-manager-drivers.md`.

### Cache Manager
Shared cache maps over file objects, lazy writer, read-ahead with a per-stream history,
and `MmFlushSection`-equivalent semantics so `fsync(2)` and `msync(2)` can be honest.
Sits on top of the Memory Manager's section objects.

### Security Reference Monitor
Tokens, SIDs, ACLs, privileges, integrity labels, and `SeAccessCheck`. POSIX
uid/gid/mode bits are *projected into* this model, not bolted beside it: a POSIX mode is
translated to a security descriptor at `open(2)` time and back at `stat(2)` time. See
`02-posix-subsystem.md` §5.

### IPC: Ports (`kernel/src/ipc`)
Synchronous LPC-shaped request/reply with async variants, used for user-mode drivers, the
window server, and the POSIX subsystem's own out-of-kernel parts. Detailed in
`05-ipc-ports.md`.

### Configuration Manager
A hierarchical key/value store backing device configuration, kernel tunables
(`sysctl(8)`-equivalent), and boot configuration. POSIX `sysconf`/`pathconf` answers come
from here where they are tunable.

### Process/Thread Manager (`kernel/src/exec`)
Process and thread objects, the dispatcher, and the scheduler. Detailed in
`06-scheduler.md`.

## 4. The kernel core

* **Dispatcher** — priority-based preemptive scheduler, 32 levels, per-CPU run queues,
  dispatcher objects (event, mutex, semaphore, timer, thread) with a single wait path.
* **Trap dispatch** — one entry stub per vector, a common `InterruptFrame`, and a
  language-level handler table. Exceptions become either a POSIX signal, a structured
  exception record handed to the process, or a bug check.
* **DPC** — deferred procedure calls queued at `DISPATCH` level, the mechanism that keeps
  interrupt service routines short. Timer expiry, I/O completion, and network softirq
  work all run as DPCs.
* **Locks** — queued spinlocks (IPL-raising), pushlocks (sleeping, reader/writer),
  fast mutexes (APC-raising, recursive), and `ERESOURCE` for heavy reader/writer cases.

## 5. What is deliberately *not* like Windows

* **No registry as the system's source of truth.** The Configuration Manager is a
  tunables store; system state lives in files, because POSIX says it does.
* **No case-insensitive-by-default namespace.** NOLXFS is case-sensitive by default with
  per-volume case-folding as an mkfs option.
* **No `\\?\` path syntax.** Paths are POSIX paths, full stop.
* **No subsystem DLL indirection for syscalls.** The POSIX subsystem is in-kernel for
  speed; its user-mode helper processes exist only for things POSIX expects to be
  processes (`init`, `getty`).
* **Errors are POSIX `errno`.** The executive's `NTSTATUS`-shaped status codes exist
  internally but are translated at the subsystem boundary and never escape it.

## 6. Module boundaries and how they are enforced

| Mechanism | Enforces |
|---|---|
| Rust visibility (`pub(crate)`, private modules) | Component interfaces |
| `unsafe` confined to `hal/`, `mm/paging.rs`, and `obj/raw.rs` | Memory-safety budget |
| Trait objects for device dispatch | Driver replaceability |
| Object type callbacks | Uniform lifetime/security for every object |
| IPL assertions | Blocking discipline |
| `#[cfg(test)]` host tests on pure-logic crates | Logic correctness without booting |

The safety budget is a hard rule: `unsafe` blocks must carry a `// SAFETY:` comment
naming the invariant they rely on. CI greps for it.
