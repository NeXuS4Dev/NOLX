//! The I/O Manager.
//!
//! Driver objects, device object stacks, and the **IORB** -- the I/O Request Block that
//! carries a request down a device stack and its completion back up. The design follows
//! `docs/architecture/04-io-manager-drivers.md`, which is itself modelled on Microsoft's
//! published kernel-mode driver architecture.

use core::fmt::Write;

use crate::hal::console::Console;
use crate::obj::ObjectHeader;

/// The highest major function code, so dispatch tables are a fixed size.
pub const MAJOR_FUNCTION_COUNT: usize = 16;

/// Major function codes: the dispatch table indices.
pub mod major {
    /// Open or create. `open`, `creat`, `opendir`, `socket`.
    pub const CREATE: usize = 0x00;
    /// Final close of the last handle.
    pub const CLOSE: usize = 0x01;
    /// First close of a descriptor: POSIX flush semantics live here.
    pub const CLEANUP: usize = 0x02;
    /// `read`, `pread`, `readdir`.
    pub const READ: usize = 0x03;
    /// `write`, `pwrite`.
    pub const WRITE: usize = 0x04;
    /// `fstat`, `lseek(SEEK_CUR)`.
    pub const QUERY_INFORMATION: usize = 0x05;
    /// `truncate`, `ftruncate`, `chmod`, `utimensat`.
    pub const SET_INFORMATION: usize = 0x06;
    /// `getxattr`.
    pub const QUERY_EA: usize = 0x07;
    /// `setxattr`.
    pub const SET_EA: usize = 0x08;
    /// Directory enumeration and change notification.
    pub const DIRECTORY_CONTROL: usize = 0x09;
    /// Filesystem-level control: `ioctl`, `fsync`, mount operations.
    pub const FILE_SYSTEM_CONTROL: usize = 0x0a;
    /// Device-node `ioctl`.
    pub const DEVICE_CONTROL: usize = 0x0b;
    /// `fcntl(F_SETLK)`, `flock`.
    pub const LOCK_CONTROL: usize = 0x0c;
    /// `fsync`, `fdatasync`.
    pub const FLUSH_BUFFERS: usize = 0x0d;
    /// `statvfs`.
    pub const QUERY_VOLUME_INFORMATION: usize = 0x0e;
    /// Plug and play, and power state transitions.
    pub const PNP_POWER: usize = 0x0f;
}

/// IORB flags.
pub mod iorb_flags {
    /// The caller is waiting synchronously on this request.
    pub const SYNCHRONOUS_API: u32 = 1 << 0;
    /// This is paging I/O, so it may not allocate from paged pool.
    pub const PAGING_IO: u32 = 1 << 1;
    /// This request is part of a close; failing it cannot be reported to anyone.
    pub const CLOSE_OPERATION: u32 = 1 << 2;
    /// The request may be cancelled.
    pub const CANCELABLE: u32 = 1 << 3;
    /// Bypass the cache manager and go straight to the device.
    pub const NO_CACHE: u32 = 1 << 4;
}

/// What a dispatch routine reports.
///
/// Internal only. The POSIX subsystem translates these to `errno` at one boundary, and no
/// other code is allowed to invent an `errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoStatus {
    /// The operation completed.
    Success,
    /// The operation is in progress; completion will arrive later.
    Pending,
    /// The requested object does not exist.
    NoSuchFile,
    /// The end of the file or stream was reached.
    EndOfFile,
    /// The device is not ready.
    DeviceNotReady,
    /// The buffer was too small for the result.
    BufferTooSmall,
    /// Access was denied by the security check.
    AccessDenied,
    /// An argument was invalid.
    InvalidParameter,
    /// The device reported an error.
    DeviceError,
    /// The volume is full.
    DiskFull,
    /// The request was cancelled.
    Cancelled,
    /// The volume is mounted read-only.
    MediaWriteProtected,
    /// The operation is not supported by this device.
    NotSupported,
}

/// One layer's description of a request, as it passes down a device stack.
#[derive(Clone, Copy, Debug)]
pub struct IoStackLocation {
    /// The major function code.
    pub major: u8,
    /// The device-object-specific minor code.
    pub minor: u8,
    /// Flags from [`iorb_flags`].
    pub flags: u32,
    /// The file object this request operates on.
    pub file_object: u64,
    /// The device object that will handle it.
    pub device_object: u64,
    /// Byte offset within the file, for `read`/`write`.
    pub offset: u64,
    /// Byte count requested.
    pub length: u64,
}

/// The I/O Request Block.
///
/// Allocated by the I/O Manager so that `stack_size` accounting is always right: a driver
/// that built its own would not know how many layers were below it.
pub struct Iorb {
    /// What happened. `information` carries a request-specific byte count, usually the
    /// number of bytes transferred.
    pub status: IoStatus,
    /// Bytes transferred, or a request-specific value.
    pub information: u64,
    /// The index of the stack location currently being processed.
    pub current_stack: u8,
    /// How many stack locations were reserved, which is the depth of the device stack.
    pub stack_count: u8,
    /// Flags from [`iorb_flags`].
    pub flags: u32,
    /// The per-layer descriptions.
    pub stack: [IoStackLocation; MAJOR_FUNCTION_COUNT],
}

impl Iorb {
    /// A request for `major` against `file_object`, with `stack_count` layers reserved.
    #[must_use]
    pub const fn new(major: u8, file_object: u64, stack_count: u8) -> Self {
        Self {
            status: IoStatus::Pending,
            information: 0,
            current_stack: 0,
            stack_count,
            flags: 0,
            stack: [IoStackLocation {
                major,
                minor: 0,
                flags: 0,
                file_object,
                device_object: 0,
                offset: 0,
                length: 0,
            }; MAJOR_FUNCTION_COUNT],
        }
    }

    /// The stack location for the layer currently processing this request.
    #[must_use]
    pub fn current(&self) -> Option<&IoStackLocation> {
        self.stack.get(usize::from(self.current_stack))
    }

    /// Push the request down to the next layer.
    ///
    /// Returns `false` at the bottom of the stack, which means a driver forwarded a request
    /// it should have handled -- a driver bug, not a runtime condition.
    pub fn push_down(&mut self) -> bool {
        if usize::from(self.current_stack) + 1 >= usize::from(self.stack_count) {
            return false;
        }
        self.current_stack += 1;
        true
    }
}

/// A driver: a dispatch table plus the state its devices share.
pub struct DriverObject {
    /// The object header.
    pub header: ObjectHeader,
    /// Dispatch routines, indexed by [`major`].
    pub dispatch: [Option<fn(&mut Iorb)>; MAJOR_FUNCTION_COUNT],
    /// `true` if the driver may be paged out. A pageable driver must not touch paged pool
    /// from a non-pageable path, which is the rule this flag records.
    pub pageable: bool,
}

/// A device in a stack.
pub struct DeviceObject {
    /// The object header.
    pub header: ObjectHeader,
    /// The driver that owns this device.
    pub driver: u64,
    /// The device below this one, if any.
    pub lower: u64,
    /// How many stack locations an IORB needs to reach the bottom from here.
    pub stack_size: u8,
    /// Alignment the device requires, in bytes.
    pub alignment_requirement: u32,
}

/// Register the I/O manager.
pub fn init(console: &mut Console) {
    let _ = writeln!(
        console,
        "io: I/O manager online, {MAJOR_FUNCTION_COUNT} major function codes, \
         IORB is {} bytes",
        core::mem::size_of::<Iorb>()
    );
}
