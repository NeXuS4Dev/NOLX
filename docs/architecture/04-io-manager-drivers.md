# I/O Manager and Driver Model

NOLX's I/O stack follows the documented Windows kernel-mode design: driver objects, device
object stacks, and a request packet that travels down the stack. Our packet is the
**IORB** (I/O Request Block) — the same role as an IRP, our own layout.

## Objects

**Driver object** — one per loaded driver module.

```
name                 "\FileSystem\Nolxfs"
flags                built-in | loadable | user-mode
dispatch[MAJOR_MAX]  table of dispatch routines
unload               optional unload routine
extensions           driver-private storage
```

**Device object** — a node in a stack. A filesystem device sits on a volume device which
sits on a partition device which sits on a disk device. Each layer may consume, modify,
and forward the request.

```
driver_object        owner
stack_size           number of stack locations an IORB must reserve
lower / attached     the device below
characteristics      removable, read-only, ...
alignment_requirement
queue                device queue for serialised drivers
extension            driver-private storage
```

**File object** — an open instance. This is what a POSIX file descriptor points at.

```
device_object        the top of the stack this handle talks to
flags                read/write/append/sync modes from open(2)
current_offset       POSIX file offset (shared by dup'd descriptors)
fs_context           filesystem-private (inode reference for NOLXFS)
completion_context   for async I/O and POSIX aio
```

## The IORB

```
+0    io_status          { status: Status, information: usize }
+8    flags              SYNCHRONOUS_API | PAGING_IO | CLOSE_OPERATION | ...
+16   thread             initiating thread (for synchronous completion)
+24   user_event         optional event signalled on completion
+32   tail.overlay {
         current_stack_location  *IO_STACK_LOCATION
         stack_count             u8
         device_object           *DEVICE_OBJECT  (where it currently is)
      }
+64   stack[0..n]        one IO_STACK_LOCATION per device layer
+...  cancel_routine     set by whichever layer may need to abort
      mdl_chain          scatter/gather description of the buffer
```

**Major function codes** (dispatch table indices) are the union of what POSIX needs and
what the device stack needs:

| Code | Name | POSIX surface |
|---|---|---|
| 0x00 | `CREATE` | `open`, `creat`, `opendir`, `socket` |
| 0x01 | `CLOSE` | last `close` of a file object |
| 0x02 | `CLEANUP` | first `close` of a descriptor (POSIX flush semantics) |
| 0x03 | `READ` | `read`, `pread`, `readdir` |
| 0x04 | `WRITE` | `write`, `pwrite` |
| 0x05 | `QUERY_INFORMATION` | `fstat`, `lseek(SEEK_CUR)` |
| 0x06 | `SET_INFORMATION` | `truncate`, `ftruncate`, `chmod`, `utimensat` |
| 0x07 | `QUERY_EA` / `SET_EA` | `getxattr`/`setxattr` |
| 0x08 | `DIRECTORY_CONTROL` | `readdir`, change notification |
| 0x09 | `FILE_SYSTEM_CONTROL` | `ioctl`, `fsync`, mount operations |
| 0x0a | `DEVICE_CONTROL` | `ioctl` on device nodes |
| 0x0b | `LOCK_CONTROL` | `fcntl(F_SETLK)`, `flock` |
| 0x0c | `FLUSH_BUFFERS` | `fsync`, `fdatasync` |
| 0x0d | `QUERY_VOLUME_INFORMATION` | `statvfs` |
| 0x0e | `PNP` / `POWER` | device state transitions |

## Completion

* Completion runs **up** the stack: each layer's completion routine may consume, translate,
  or post-process status, then hand the IORB to the layer above.
* A synchronous request blocks the caller on the IORB's event; an asynchronous one queues
  a completion (DPC → work item → POSIX `aio` callback or completion port).
* **Cancellation** is a first-class path: `IoCancelIorb` sets a cancel routine, and every
  layer that queues a request must check for a cancel between enqueue and dequeue. This is
  how `close(2)` on a descriptor with in-flight `aio` behaves sanely.
* Status codes are internal. `psx/` translates them to `errno` at one boundary; nothing
  else may invent an `errno`.

## Cache Manager interaction

Reads and writes on cached file objects go through the Cache Manager, which decides
whether to satisfy from a shared cache map, issue a paging I/O, or read ahead. Filesystems
implement `COPY_READ`/`COPY_WRITE` fast paths so the common `read(2)` never allocates an
IORB at all — it is a memory copy out of a mapped section, plus a lazy-writer dirty mark.

## Driver authoring rules

1. A driver is a driver object plus a device object; it never touches another driver's
   internals.
2. Dispatch routines must be safe to call at the IPL their code declares; blocking is
   only allowed at `PASSIVE`.
3. Drivers allocate IORBs through the I/O Manager so `stack_size` accounting is right.
4. Every driver declares whether it is pageable. Non-pageable drivers may not touch paged
   pool.
5. Long-term plan (M13): the same driver object can live in a user-mode process, with the
   I/O Manager forwarding IORBs over an IPC port. Clients do not change.
