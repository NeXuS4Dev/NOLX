//! The I/O Manager: IORBs, driver objects, device stacks, completion ports, and cancellation.
//!
//! NOLX's I/O model follows `docs/architecture/04-io-manager-drivers.md`:
//!
//! * A **driver object** holds a dispatch table indexed by major function code plus an
//!   optional unload hook.
//! * A **device object** is one node in a vertical device stack. Attaching a filter or
//!   filesystem device above a lower device updates `stack_size` up the chain so the I/O
//!   Manager always reserves enough [`IoStackLocation`] slots in every [`Iorb`].
//! * An **IORB** (I/O Request Block) travels *down* the device stack via [`Iorb::push_down`]
//!   and completes *up* the stack via [`Iorb::complete`], invoking each layer's registered
//!   completion routine when its control flags match. Any layer may halt upward unwinding
//!   by returning [`CompletionDisposition::MoreProcessingRequired`].
//! * **Cancellation** (`IoCancelIorb`) atomically marks an in-flight request cancelled,
//!   takes and runs its cancel routine once, and completes the packet with
//!   [`IoStatus::Cancelled`].
//! * **Completion ports** collect [`CompletionPacket`]s from asynchronous IORBs associated
//!   with a file object, bounded by the port's capacity.
//! * **Driver unload** is reference-tracked: a driver cannot unload while any of its
//!   devices has an upper device attached, an open handle reference, or an in-flight IORB.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::errno;

/// Total number of major function codes in every driver's dispatch table.
pub const MAJOR_FUNCTION_COUNT: usize = 16;

/// Major function codes (dispatch table indices).
pub mod major {
    /// Open or create (`open`, `creat`, `opendir`, `socket`).
    pub const CREATE: usize = 0x00;
    /// Final close of the last reference to a file object.
    pub const CLOSE: usize = 0x01;
    /// First close of a descriptor (POSIX flush/lock-release semantics).
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
    /// Filesystem control (`ioctl`, `fsync`, mount operations).
    pub const FILE_SYSTEM_CONTROL: usize = 0x0a;
    /// Device-node `ioctl`.
    pub const DEVICE_CONTROL: usize = 0x0b;
    /// Byte-range and file locks (`fcntl(F_SETLK)`, `flock`).
    pub const LOCK_CONTROL: usize = 0x0c;
    /// `fsync`, `fdatasync`.
    pub const FLUSH_BUFFERS: usize = 0x0d;
    /// `statvfs`.
    pub const QUERY_VOLUME_INFORMATION: usize = 0x0e;
    /// Plug and play and power state transitions.
    pub const PNP_POWER: usize = 0x0f;
}

/// Minor function codes qualifying a major code.
pub mod minor {
    /// Default minor code when none applies.
    pub const NONE: u8 = 0x00;
    /// `DIRECTORY_CONTROL`: enumerate directory entries.
    pub const QUERY_DIRECTORY: u8 = 0x01;
    /// `DIRECTORY_CONTROL`: watch for directory changes.
    pub const NOTIFY_CHANGE_DIRECTORY: u8 = 0x02;
    /// `FILE_SYSTEM_CONTROL`: mount a volume on a storage device.
    pub const MOUNT_VOLUME: u8 = 0x03;
    /// `FILE_SYSTEM_CONTROL`: verify a mounted volume after media change.
    pub const VERIFY_VOLUME: u8 = 0x04;
    /// `PNP_POWER`: start device hardware.
    pub const START_DEVICE: u8 = 0x05;
    /// `PNP_POWER`: stop device hardware.
    pub const STOP_DEVICE: u8 = 0x06;
    /// `PNP_POWER`: remove device from the stack.
    pub const REMOVE_DEVICE: u8 = 0x07;
}

/// Flags carried on [`Iorb::flags`] and [`IoStackLocation::flags`].
pub mod iorb_flags {
    /// The caller waits synchronously for completion.
    pub const SYNCHRONOUS_API: u32 = 1 << 0;
    /// Paging I/O: may not touch paged pool or re-enter the cache map.
    pub const PAGING_IO: u32 = 1 << 1;
    /// Part of a close path; errors cannot be reported to user space.
    pub const CLOSE_OPERATION: u32 = 1 << 2;
    /// The request may be cancelled while queued.
    pub const CANCELABLE: u32 = 1 << 3;
    /// Bypass the Cache Manager and go directly to the underlying device.
    pub const NO_CACHE: u32 = 1 << 4;
    /// A dispatch routine marked the request pending and will complete it asynchronously.
    pub const PENDING_RETURNED: u32 = 1 << 5;
}

/// Per-stack-location completion control bits.
pub mod sl_control {
    /// Invoke the completion routine when the IORB completes with [`IoStatus::Success`].
    pub const SL_INVOKE_ON_SUCCESS: u8 = 1 << 0;
    /// Invoke the completion routine when the IORB completes with an error status.
    pub const SL_INVOKE_ON_ERROR: u8 = 1 << 1;
    /// Invoke the completion routine when the IORB was cancelled.
    pub const SL_INVOKE_ON_CANCEL: u8 = 1 << 2;
    /// Invoke the completion routine on every outcome.
    pub const SL_INVOKE_ALWAYS: u8 =
        SL_INVOKE_ON_SUCCESS | SL_INVOKE_ON_ERROR | SL_INVOKE_ON_CANCEL;
}

/// Executive I/O status reported by drivers and the I/O Manager.
///
/// Drivers never return POSIX `errno` values directly; the POSIX subsystem translates
/// [`IoStatus`] at the syscall boundary via [`IoStatus::to_errno`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoStatus {
    /// The operation completed successfully.
    Success,
    /// The operation was queued and will complete asynchronously.
    Pending,
    /// The requested file or object does not exist.
    NoSuchFile,
    /// End of file or stream reached.
    EndOfFile,
    /// The device is not ready.
    DeviceNotReady,
    /// The supplied buffer is too small for the result.
    BufferTooSmall,
    /// Access was denied by the Security Reference Monitor or file mode check.
    AccessDenied,
    /// An argument was invalid.
    InvalidParameter,
    /// The device reported a hardware or medium error.
    DeviceError,
    /// The volume or device has no free space.
    DiskFull,
    /// The request was cancelled via `IoCancelIorb`.
    Cancelled,
    /// The volume is mounted read-only.
    MediaWriteProtected,
    /// The operation is not supported by this driver or filesystem.
    NotSupported,
    /// The target path or object already exists.
    AlreadyExists,
    /// A path component or target is not a directory.
    NotDirectory,
    /// An operation requiring a non-directory targeted a directory.
    IsDirectory,
    /// A directory removal targeted a non-empty directory.
    DirectoryNotEmpty,
    /// The device, mount, or driver is currently busy.
    Busy,
    /// Hard link or rename attempted across distinct mounts/devices.
    CrossDeviceLink,
    /// Symbolic link resolution exceeded `SYMLOOP_MAX`.
    SymlinkLoop,
    /// Pathname or component exceeded `PATH_MAX` or `NAME_MAX`.
    NameTooLong,
}

impl IoStatus {
    /// Translate an internal [`IoStatus`] into a POSIX `errno` at the `psx` boundary.
    #[must_use]
    pub const fn to_errno(self) -> i32 {
        match self {
            Self::Success | Self::EndOfFile => errno::EOK,
            Self::Pending => errno::EAGAIN,
            Self::NoSuchFile => errno::ENOENT,
            Self::DeviceNotReady => errno::ENXIO,
            Self::BufferTooSmall => errno::ERANGE,
            Self::AccessDenied => errno::EACCES,
            Self::InvalidParameter => errno::EINVAL,
            Self::DeviceError => errno::EIO,
            Self::DiskFull => errno::ENOSPC,
            Self::Cancelled => errno::ECANCELED,
            Self::MediaWriteProtected => errno::EROFS,
            Self::NotSupported => errno::ENOTSUP,
            Self::AlreadyExists => errno::EEXIST,
            Self::NotDirectory => errno::ENOTDIR,
            Self::IsDirectory => errno::EISDIR,
            Self::DirectoryNotEmpty => errno::ENOTEMPTY,
            Self::Busy => errno::EBUSY,
            Self::CrossDeviceLink => errno::EXDEV,
            Self::SymlinkLoop => errno::ELOOP,
            Self::NameTooLong => errno::ENAMETOOLONG,
        }
    }

    /// `true` if `self` represents an error outcome (neither `Success` nor `Pending`).
    #[must_use]
    pub const fn is_error(self) -> bool {
        !matches!(self, Self::Success | Self::Pending | Self::Cancelled)
    }
}

/// What a layer's completion routine returns as the IORB unwinds up the device stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionDisposition {
    /// Continue unwinding completion up to the next higher device layer.
    ContinueCompletion,
    /// Stop unwinding now; the driver that returned this owns the IORB and will resume or
    /// finish completion later.
    MoreProcessingRequired,
}

/// Signature of a per-stack-location completion routine.
pub type CompletionRoutine = fn(&mut Iorb, u8, u64) -> CompletionDisposition;

/// Signature of an IORB cancellation routine.
pub type CancelRoutine = fn(&mut Iorb);

/// Signature of a driver dispatch routine.
pub type DispatchFn = fn(&mut Iorb) -> IoStatus;

/// Signature of a driver unload callback.
pub type UnloadFn = fn(u64);

/// One layer's parameters inside an [`Iorb`].
#[derive(Clone, Copy, Debug)]
pub struct IoStackLocation {
    /// Major function code (`major::*`).
    pub major: u8,
    /// Minor function code (`minor::*`).
    pub minor: u8,
    /// Per-location flags (`iorb_flags::*`).
    pub flags: u32,
    /// Completion invocation mask (`sl_control::*`).
    pub control: u8,
    /// Open file object identifier.
    pub file_object: u64,
    /// Device object identifier at this stack depth.
    pub device_object: u64,
    /// Byte offset for `READ` / `WRITE` / `LOCK_CONTROL`.
    pub offset: u64,
    /// Byte length requested.
    pub length: u64,
    /// Optional completion routine registered by this layer before forwarding down.
    pub completion_routine: Option<CompletionRoutine>,
    /// Driver-supplied context word passed to `completion_routine`.
    pub completion_context: u64,
}

impl IoStackLocation {
    /// An empty stack location for `major` on `file_object`.
    #[must_use]
    pub const fn new(major: u8, file_object: u64) -> Self {
        Self {
            major,
            minor: minor::NONE,
            flags: 0,
            control: 0,
            file_object,
            device_object: 0,
            offset: 0,
            length: 0,
            completion_routine: None,
            completion_context: 0,
        }
    }
}

/// Summary of what happened when [`Iorb::complete`] ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionOutcome {
    /// Final status after completion unwinding.
    pub status: IoStatus,
    /// Final `information` byte count or result word.
    pub information: u64,
    /// Number of completion routines that executed during unwinding.
    pub routines_invoked: u32,
    /// `true` if a layer halted unwinding with [`CompletionDisposition::MoreProcessingRequired`].
    pub suspended_by_driver: bool,
}

/// The I/O Request Block (IORB).
///
/// Allocated with `stack_count` matching the target device stack's depth so every layer has
/// its own [`IoStackLocation`].
#[derive(Clone, Debug)]
pub struct Iorb {
    /// Unique identifier when managed by [`IoManager`], or `0` for standalone packets.
    pub id: u64,
    /// Current or final completion status.
    pub status: IoStatus,
    /// Bytes transferred or request-specific result.
    pub information: u64,
    /// Index of the active stack location (`0..stack_count`).
    pub current_stack: u8,
    /// Number of valid stack locations reserved for this request.
    pub stack_count: u8,
    /// Packet-wide flags (`iorb_flags::*`).
    pub flags: u32,
    /// Initiating thread identifier (for synchronous wakeup).
    pub thread_id: u64,
    /// Optional event object signalled on completion.
    pub user_event: Option<u64>,
    /// Optional completion port that receives a [`CompletionPacket`] when this IORB finishes.
    pub completion_port: Option<u64>,
    /// Completion key associated with `file_object` on `completion_port`.
    pub completion_key: u64,
    /// `true` once `IoCancelIorb` has been called on this packet.
    pub cancelled: bool,
    /// Cancel callback set by whichever driver currently holds the request in a queue.
    pub cancel_routine: Option<CancelRoutine>,
    /// Count of times a cancel routine was invoked on this packet.
    pub cancel_invocations: u32,
    /// Per-layer stack locations, indexed from top (`0`) to bottom (`stack_count - 1`).
    pub stack: [IoStackLocation; MAJOR_FUNCTION_COUNT],
}

impl Iorb {
    /// Build an IORB for `major` against `file_object` with `stack_count` layers reserved.
    #[must_use]
    pub const fn new(major: u8, file_object: u64, stack_count: u8) -> Self {
        let clamped = if stack_count == 0 {
            1
        } else if stack_count as usize > MAJOR_FUNCTION_COUNT {
            MAJOR_FUNCTION_COUNT as u8
        } else {
            stack_count
        };
        Self {
            id: 0,
            status: IoStatus::Pending,
            information: 0,
            current_stack: 0,
            stack_count: clamped,
            flags: 0,
            thread_id: 0,
            user_event: None,
            completion_port: None,
            completion_key: 0,
            cancelled: false,
            cancel_routine: None,
            cancel_invocations: 0,
            stack: [IoStackLocation::new(major, file_object); MAJOR_FUNCTION_COUNT],
        }
    }

    /// Immutable view of the current layer's stack location.
    #[must_use]
    pub fn current(&self) -> Option<&IoStackLocation> {
        let idx = usize::from(self.current_stack);
        if idx < usize::from(self.stack_count) {
            self.stack.get(idx)
        } else {
            None
        }
    }

    /// Mutable view of the current layer's stack location.
    pub fn current_mut(&mut self) -> Option<&mut IoStackLocation> {
        let idx = usize::from(self.current_stack);
        let count = usize::from(self.stack_count);
        if idx < count {
            self.stack.get_mut(idx)
        } else {
            None
        }
    }

    /// Copy the current stack location's request parameters to the next lower location,
    /// preserving the lower location's `device_object` and clearing its completion routine
    /// (`IoCopyCurrentIrpStackLocationToNext`).
    pub fn copy_current_to_next(&mut self) -> bool {
        let cur = usize::from(self.current_stack);
        let next = cur + 1;
        if next >= usize::from(self.stack_count) {
            return false;
        }
        let target_dev = self.stack[next].device_object;
        let mut copied = self.stack[cur];
        copied.device_object = target_dev;
        copied.completion_routine = None;
        copied.completion_context = 0;
        copied.control = 0;
        self.stack[next] = copied;
        true
    }

    /// Register a completion routine on the current stack location to run when the lower
    /// driver completes the request (`IoSetCompletionRoutine`).
    pub fn set_completion_routine(
        &mut self,
        routine: CompletionRoutine,
        context: u64,
        control: u8,
    ) -> bool {
        let Some(loc) = self.current_mut() else {
            return false;
        };
        loc.completion_routine = Some(routine);
        loc.completion_context = context;
        loc.control = control;
        true
    }

    /// Advance the active stack location to the next lower device in the stack.
    ///
    /// Returns `false` if the request is already at the bottom layer.
    pub fn push_down(&mut self) -> bool {
        if usize::from(self.current_stack) + 1 >= usize::from(self.stack_count) {
            return false;
        }
        self.current_stack += 1;
        true
    }

    /// Move one layer up the stack during completion unwinding.
    pub fn pop_up(&mut self) -> bool {
        if self.current_stack == 0 {
            return false;
        }
        self.current_stack -= 1;
        true
    }

    /// Mark the current stack location pending (`IoMarkIrpPending`).
    pub fn mark_pending(&mut self) {
        self.status = IoStatus::Pending;
        self.flags |= iorb_flags::PENDING_RETURNED;
    }

    /// Install or clear the packet's cancel routine (`IoSetCancelRoutine`).
    pub fn set_cancel_routine(&mut self, routine: Option<CancelRoutine>) -> Option<CancelRoutine> {
        core::mem::replace(&mut self.cancel_routine, routine)
    }

    /// Cancel this IORB (`IoCancelIorb`).
    ///
    /// Atomically sets `cancelled = true` and takes `cancel_routine` so a cancel callback
    /// can never run twice. Returns `true` if a cancel routine was installed and executed.
    pub fn cancel(&mut self) -> bool {
        self.cancelled = true;
        if let Some(routine) = self.cancel_routine.take() {
            self.cancel_invocations = self.cancel_invocations.saturating_add(1);
            routine(self);
            if self.status == IoStatus::Pending {
                self.status = IoStatus::Cancelled;
            }
            true
        } else {
            false
        }
    }

    /// Complete the IORB with `status` and `information` (`IoCompleteRequest`), walking up
    /// from `current_stack` to layer `0` and invoking each layer's completion routine when
    /// its [`sl_control`] mask matches the outcome.
    pub fn complete(&mut self, status: IoStatus, information: u64) -> CompletionOutcome {
        self.status = status;
        self.information = information;
        self.cancel_routine = None;
        let mut routines_invoked = 0u32;

        loop {
            let depth = self.current_stack;
            let idx = usize::from(depth);
            if let Some(loc) = self.stack.get_mut(idx) {
                let routine = loc.completion_routine.take();
                let control = loc.control;
                let context = loc.completion_context;
                if let Some(cb) = routine {
                    let should_call = (self.cancelled
                        && (control & sl_control::SL_INVOKE_ON_CANCEL != 0))
                        || (!self.cancelled
                            && self.status == IoStatus::Success
                            && (control & sl_control::SL_INVOKE_ON_SUCCESS != 0))
                        || (!self.cancelled
                            && self.status.is_error()
                            && (control & sl_control::SL_INVOKE_ON_ERROR != 0));
                    if should_call {
                        routines_invoked += 1;
                        if cb(self, depth, context) == CompletionDisposition::MoreProcessingRequired
                        {
                            return CompletionOutcome {
                                status: self.status,
                                information: self.information,
                                routines_invoked,
                                suspended_by_driver: true,
                            };
                        }
                    }
                }
            }
            if !self.pop_up() {
                break;
            }
        }

        CompletionOutcome {
            status: self.status,
            information: self.information,
            routines_invoked,
            suspended_by_driver: false,
        }
    }
}

/// A completion notification queued to an [`IoCompletionPort`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionPacket {
    /// Per-file completion key registered when the file object was bound to the port.
    pub key: u64,
    /// Identifier of the completed [`Iorb`].
    pub iorb_id: u64,
    /// Final status of the operation.
    pub status: IoStatus,
    /// Bytes transferred or request-specific result.
    pub information: u64,
}

/// An executive I/O completion port (`IoCompletionPort`).
#[derive(Clone, Debug)]
pub struct IoCompletionPort {
    /// Port identifier.
    pub id: u64,
    /// Maximum concurrent threads target (`0` means unbounded).
    pub max_concurrency: u32,
    /// Maximum packets queued before `post` refuses with `IoStatus::BufferTooSmall`.
    pub capacity: usize,
    /// Queued completion packets in FIFO order.
    packets: VecDeque<CompletionPacket>,
    /// File object id -> completion key bindings.
    bindings: BTreeMap<u64, u64>,
}

impl IoCompletionPort {
    /// Create a completion port with `max_concurrency` and `capacity`.
    #[must_use]
    pub fn new(id: u64, max_concurrency: u32, capacity: usize) -> Self {
        Self {
            id,
            max_concurrency,
            capacity: capacity.max(1),
            packets: VecDeque::new(),
            bindings: BTreeMap::new(),
        }
    }

    /// Associate `file_object` with this completion port using `key`.
    ///
    /// # Errors
    ///
    /// [`IoStatus::AlreadyExists`] if `file_object` is already bound to this port.
    pub fn bind_file(&mut self, file_object: u64, key: u64) -> Result<(), IoStatus> {
        if self.bindings.contains_key(&file_object) {
            return Err(IoStatus::AlreadyExists);
        }
        self.bindings.insert(file_object, key);
        Ok(())
    }

    /// Look up the completion key for `file_object`.
    #[must_use]
    pub fn key_for_file(&self, file_object: u64) -> Option<u64> {
        self.bindings.get(&file_object).copied()
    }

    /// Post a completion packet to the port's FIFO queue.
    ///
    /// # Errors
    ///
    /// [`IoStatus::BufferTooSmall`] if the queue is at capacity.
    pub fn post(&mut self, packet: CompletionPacket) -> Result<(), IoStatus> {
        if self.packets.len() >= self.capacity {
            return Err(IoStatus::BufferTooSmall);
        }
        self.packets.push_back(packet);
        Ok(())
    }

    /// Dequeue the oldest completion packet, if any.
    pub fn dequeue(&mut self) -> Option<CompletionPacket> {
        self.packets.pop_front()
    }

    /// Number of queued completion packets waiting to be dequeued.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.packets.len()
    }
}

/// A registered driver object.
#[derive(Clone, Debug)]
pub struct DriverObject {
    /// Driver identifier.
    pub id: u64,
    /// Object namespace path (e.g. `\Driver\Tmpfs` or `\FileSystem\Nolxfs`).
    pub name: String,
    /// Dispatch table indexed by [`major`].
    pub dispatch: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT],
    /// Optional unload callback invoked when [`IoManager::unload_driver`] succeeds.
    pub unload: Option<UnloadFn>,
    /// `true` if the driver may be paged out (and therefore must not run above `PASSIVE`).
    pub pageable: bool,
}

/// A device object in a device stack.
#[derive(Clone, Debug)]
pub struct DeviceObject {
    /// Device identifier.
    pub id: u64,
    /// Optional device name (e.g. `\Device\Null`).
    pub name: String,
    /// Owning driver identifier.
    pub driver_id: u64,
    /// Next lower device in the stack, if this device is attached above another.
    pub lower: Option<u64>,
    /// Next upper device attached directly on top of this device, if any.
    pub upper: Option<u64>,
    /// Number of [`IoStackLocation`] slots required to reach the bottom from this device.
    pub stack_size: u8,
    /// Required buffer alignment in bytes.
    pub alignment_requirement: u32,
    /// Open handle / file-object reference count.
    pub reference_count: u32,
    /// Number of in-flight IORBs currently traversing this device stack.
    pub in_flight_iorbs: u32,
}

/// The executive I/O Manager registry and dispatcher.
#[derive(Default)]
pub struct IoManager {
    drivers: BTreeMap<u64, DriverObject>,
    devices: BTreeMap<u64, DeviceObject>,
    ports: BTreeMap<u64, IoCompletionPort>,
    in_flight: BTreeMap<u64, Iorb>,
    next_id: u64,
}

impl IoManager {
    /// Create an empty I/O Manager.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            drivers: BTreeMap::new(),
            devices: BTreeMap::new(),
            ports: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Register a new driver object.
    pub fn register_driver(
        &mut self,
        name: &str,
        dispatch: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT],
        unload: Option<UnloadFn>,
        pageable: bool,
    ) -> u64 {
        let id = self.alloc_id();
        self.drivers.insert(
            id,
            DriverObject {
                id,
                name: String::from(name),
                dispatch,
                unload,
                pageable,
            },
        );
        id
    }

    /// Create a standalone device object owned by `driver_id`.
    ///
    /// # Errors
    ///
    /// [`IoStatus::InvalidParameter`] if `driver_id` is not registered.
    pub fn create_device(
        &mut self,
        driver_id: u64,
        name: &str,
        alignment_requirement: u32,
    ) -> Result<u64, IoStatus> {
        if !self.drivers.contains_key(&driver_id) {
            return Err(IoStatus::InvalidParameter);
        }
        let id = self.alloc_id();
        self.devices.insert(
            id,
            DeviceObject {
                id,
                name: String::from(name),
                driver_id,
                lower: None,
                upper: None,
                stack_size: 1,
                alignment_requirement: alignment_requirement.max(1),
                reference_count: 0,
                in_flight_iorbs: 0,
            },
        );
        Ok(id)
    }

    /// Attach `upper_device` directly on top of `lower_device` (`IoAttachDeviceToDeviceStack`),
    /// propagating `stack_size` upward through any devices already attached above `upper_device`.
    ///
    /// # Errors
    ///
    /// * [`IoStatus::InvalidParameter`] if either device does not exist, `upper == lower`,
    ///   attaching would create a cycle, or the resulting stack depth would exceed
    ///   [`MAJOR_FUNCTION_COUNT`].
    /// * [`IoStatus::Busy`] if `upper_device` already has a lower device or `lower_device`
    ///   already has an upper device attached.
    pub fn attach_device(&mut self, upper_device: u64, lower_device: u64) -> Result<u8, IoStatus> {
        if upper_device == lower_device {
            return Err(IoStatus::InvalidParameter);
        }
        let upper = self
            .devices
            .get(&upper_device)
            .ok_or(IoStatus::InvalidParameter)?;
        let lower = self
            .devices
            .get(&lower_device)
            .ok_or(IoStatus::InvalidParameter)?;
        if upper.lower.is_some() || lower.upper.is_some() {
            return Err(IoStatus::Busy);
        }

        // Prevent cycles: walk down from `lower_device` and make sure `upper_device` never appears.
        let mut cursor = Some(lower_device);
        while let Some(id) = cursor {
            if id == upper_device {
                return Err(IoStatus::InvalidParameter);
            }
            cursor = self.devices.get(&id).and_then(|d| d.lower);
        }

        let base_depth = usize::from(lower.stack_size);
        // Count how many devices sit at or above `upper_device`.
        let mut above_count = 0usize;
        let mut up_cursor = Some(upper_device);
        while let Some(id) = up_cursor {
            above_count += 1;
            up_cursor = self.devices.get(&id).and_then(|d| d.upper);
        }
        if base_depth + above_count > MAJOR_FUNCTION_COUNT {
            return Err(IoStatus::InvalidParameter);
        }

        self.devices.get_mut(&upper_device).unwrap().lower = Some(lower_device);
        self.devices.get_mut(&lower_device).unwrap().upper = Some(upper_device);
        self.recompute_stack_sizes_from(upper_device);

        Ok(self.devices[&upper_device].stack_size)
    }

    /// Detach `upper_device` from the device immediately below it (`IoDetachDevice`).
    ///
    /// # Errors
    ///
    /// * [`IoStatus::InvalidParameter`] if `upper_device` is not attached to a lower device.
    /// * [`IoStatus::Busy`] if `upper_device` has in-flight IORBs.
    pub fn detach_device(&mut self, upper_device: u64) -> Result<(), IoStatus> {
        let upper = self
            .devices
            .get(&upper_device)
            .ok_or(IoStatus::InvalidParameter)?;
        if upper.in_flight_iorbs > 0 {
            return Err(IoStatus::Busy);
        }
        let lower_id = upper.lower.ok_or(IoStatus::InvalidParameter)?;
        if let Some(lower) = self.devices.get_mut(&lower_id) {
            lower.upper = None;
        }
        if let Some(upper_mut) = self.devices.get_mut(&upper_device) {
            upper_mut.lower = None;
        }
        self.recompute_stack_sizes_from(upper_device);
        Ok(())
    }

    fn recompute_stack_sizes_from(&mut self, start_device: u64) {
        let mut cursor = Some(start_device);
        while let Some(id) = cursor {
            let lower_depth = self
                .devices
                .get(&id)
                .and_then(|d| d.lower)
                .and_then(|l| self.devices.get(&l))
                .map_or(0, |l| l.stack_size);
            let next_upper = if let Some(dev) = self.devices.get_mut(&id) {
                dev.stack_size = lower_depth + 1;
                dev.upper
            } else {
                None
            };
            cursor = next_upper;
        }
    }

    /// Return the highest device attached above `device_id` (`IoGetAttachedDeviceReference`).
    #[must_use]
    pub fn top_of_stack(&self, mut device_id: u64) -> Option<u64> {
        if !self.devices.contains_key(&device_id) {
            return None;
        }
        while let Some(upper) = self.devices.get(&device_id).and_then(|d| d.upper) {
            device_id = upper;
        }
        Some(device_id)
    }

    /// Inspect a device object by id.
    #[must_use]
    pub fn device(&self, device_id: u64) -> Option<&DeviceObject> {
        self.devices.get(&device_id)
    }

    /// Increment a device's open reference count.
    pub fn reference_device(&mut self, device_id: u64) -> Result<u32, IoStatus> {
        let dev = self
            .devices
            .get_mut(&device_id)
            .ok_or(IoStatus::InvalidParameter)?;
        dev.reference_count = dev
            .reference_count
            .checked_add(1)
            .ok_or(IoStatus::InvalidParameter)?;
        Ok(dev.reference_count)
    }

    /// Decrement a device's open reference count.
    pub fn dereference_device(&mut self, device_id: u64) -> Result<u32, IoStatus> {
        let dev = self
            .devices
            .get_mut(&device_id)
            .ok_or(IoStatus::InvalidParameter)?;
        if dev.reference_count == 0 {
            return Err(IoStatus::InvalidParameter);
        }
        dev.reference_count -= 1;
        Ok(dev.reference_count)
    }

    /// Unload a driver (`IoDeleteDriver` / unload path), checking that none of its devices
    /// has an attached upper filter, an open reference, or an in-flight IORB.
    ///
    /// # Errors
    ///
    /// * [`IoStatus::InvalidParameter`] if `driver_id` is unknown.
    /// * [`IoStatus::Busy`] if any device owned by `driver_id` is still referenced, has an
    ///   attached upper device, or has an in-flight IORB.
    pub fn unload_driver(&mut self, driver_id: u64) -> Result<(), IoStatus> {
        if !self.drivers.contains_key(&driver_id) {
            return Err(IoStatus::InvalidParameter);
        }
        let mut owned_devices = Vec::new();
        for (id, dev) in &self.devices {
            if dev.driver_id == driver_id {
                if dev.reference_count > 0
                    || dev.in_flight_iorbs > 0
                    || dev.upper.is_some()
                    || dev.lower.is_some()
                {
                    return Err(IoStatus::Busy);
                }
                owned_devices.push(*id);
            }
        }
        for dev_id in owned_devices {
            self.devices.remove(&dev_id);
        }
        let drv = self.drivers.remove(&driver_id).unwrap();
        if let Some(unload) = drv.unload {
            unload(driver_id);
        }
        Ok(())
    }

    /// Create a completion port (`IoCreateIoCompletion`).
    pub fn create_completion_port(&mut self, max_concurrency: u32, capacity: usize) -> u64 {
        let id = self.alloc_id();
        self.ports
            .insert(id, IoCompletionPort::new(id, max_concurrency, capacity));
        id
    }

    /// Bind `file_object` to `port_id` with `key`.
    pub fn bind_completion_port(
        &mut self,
        port_id: u64,
        file_object: u64,
        key: u64,
    ) -> Result<(), IoStatus> {
        let port = self
            .ports
            .get_mut(&port_id)
            .ok_or(IoStatus::InvalidParameter)?;
        port.bind_file(file_object, key)
    }

    /// Dequeue one packet from `port_id`.
    pub fn dequeue_completion(
        &mut self,
        port_id: u64,
    ) -> Result<Option<CompletionPacket>, IoStatus> {
        let port = self
            .ports
            .get_mut(&port_id)
            .ok_or(IoStatus::InvalidParameter)?;
        Ok(port.dequeue())
    }

    /// Allocate an [`Iorb`] sized for `target_device`'s stack, pre-populating every stack
    /// location's `device_object` down the chain (`IoAllocateIorb`).
    pub fn allocate_iorb(
        &mut self,
        target_device: u64,
        major_code: u8,
        file_object: u64,
    ) -> Result<Iorb, IoStatus> {
        let top = self
            .devices
            .get(&target_device)
            .ok_or(IoStatus::InvalidParameter)?;
        let depth = top.stack_size;
        let mut iorb = Iorb::new(major_code, file_object, depth);
        iorb.id = self.alloc_id();

        let mut cursor = Some(target_device);
        for slot in 0..usize::from(depth) {
            let Some(dev_id) = cursor else {
                return Err(IoStatus::InvalidParameter);
            };
            iorb.stack[slot].device_object = dev_id;
            cursor = self.devices.get(&dev_id).and_then(|d| d.lower);
        }

        // If `file_object` is bound to any completion port, attach it automatically.
        for port in self.ports.values() {
            if let Some(key) = port.key_for_file(file_object) {
                iorb.completion_port = Some(port.id);
                iorb.completion_key = key;
                break;
            }
        }
        Ok(iorb)
    }

    /// Dispatch `iorb` through the device stack (`IoCallDriver`).
    ///
    /// Walks down the device stack as long as each driver advances `current_stack` with
    /// [`Iorb::push_down`]. If the bottom (or handling) driver returns [`IoStatus::Pending`],
    /// the IORB is retained in the manager's in-flight table until [`Self::complete_iorb`]
    /// or [`Self::cancel_iorb`] is called; otherwise completion runs immediately and posts
    /// to any bound completion port.
    pub fn submit_iorb(&mut self, mut iorb: Iorb) -> Result<IoStatus, IoStatus> {
        let depth = usize::from(iorb.stack_count);
        for slot in 0..depth {
            let dev_id = iorb.stack[slot].device_object;
            if let Some(dev) = self.devices.get_mut(&dev_id) {
                dev.in_flight_iorbs += 1;
            }
        }

        loop {
            let cur = usize::from(iorb.current_stack);
            let loc = iorb.stack[cur];
            let dev = self
                .devices
                .get(&loc.device_object)
                .ok_or(IoStatus::InvalidParameter)?;
            let drv = self
                .drivers
                .get(&dev.driver_id)
                .ok_or(IoStatus::InvalidParameter)?;
            let Some(handler) = drv.dispatch.get(usize::from(loc.major)).copied().flatten() else {
                let status = IoStatus::NotSupported;
                self.finish_iorb(&mut iorb, status, 0);
                return Ok(status);
            };

            let before_stack = iorb.current_stack;
            let status = handler(&mut iorb);
            if status == IoStatus::Pending {
                iorb.mark_pending();
                self.in_flight.insert(iorb.id, iorb);
                return Ok(IoStatus::Pending);
            }
            // If the driver pushed the request down to the next device layer, continue loop.
            if iorb.current_stack > before_stack {
                continue;
            }
            let info = iorb.information;
            self.finish_iorb(&mut iorb, status, info);
            return Ok(iorb.status);
        }
    }

    /// Complete a previously pending asynchronous IORB by id.
    pub fn complete_pending_iorb(
        &mut self,
        iorb_id: u64,
        status: IoStatus,
        information: u64,
    ) -> Result<CompletionOutcome, IoStatus> {
        let mut iorb = self
            .in_flight
            .remove(&iorb_id)
            .ok_or(IoStatus::InvalidParameter)?;
        let outcome = self.finish_iorb(&mut iorb, status, information);
        Ok(outcome)
    }

    /// Cancel a pending asynchronous IORB by id (`IoCancelIorb`).
    pub fn cancel_pending_iorb(&mut self, iorb_id: u64) -> Result<bool, IoStatus> {
        let mut iorb = self
            .in_flight
            .remove(&iorb_id)
            .ok_or(IoStatus::InvalidParameter)?;
        let invoked = iorb.cancel();
        let info = iorb.information;
        self.finish_iorb(&mut iorb, IoStatus::Cancelled, info);
        Ok(invoked)
    }

    fn finish_iorb(
        &mut self,
        iorb: &mut Iorb,
        status: IoStatus,
        information: u64,
    ) -> CompletionOutcome {
        let outcome = iorb.complete(status, information);
        let depth = usize::from(iorb.stack_count);
        for slot in 0..depth {
            let dev_id = iorb.stack[slot].device_object;
            if let Some(dev) = self.devices.get_mut(&dev_id) {
                dev.in_flight_iorbs = dev.in_flight_iorbs.saturating_sub(1);
            }
        }
        if !outcome.suspended_by_driver {
            if let Some(port_id) = iorb.completion_port {
                if let Some(port) = self.ports.get_mut(&port_id) {
                    let _ = port.post(CompletionPacket {
                        key: iorb.completion_key,
                        iorb_id: iorb.id,
                        status: outcome.status,
                        information: outcome.information,
                    });
                }
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    static UNLOAD_RECORDED: AtomicU64 = AtomicU64::new(0);

    fn record_unload(driver_id: u64) {
        UNLOAD_RECORDED.store(driver_id, Ordering::SeqCst);
    }

    fn filter_completion(iorb: &mut Iorb, _depth: u8, ctx: u64) -> CompletionDisposition {
        iorb.information = iorb.information.saturating_add(ctx);
        CompletionDisposition::ContinueCompletion
    }

    fn stop_completion(_iorb: &mut Iorb, _depth: u8, _ctx: u64) -> CompletionDisposition {
        CompletionDisposition::MoreProcessingRequired
    }

    fn filter_read_dispatch(iorb: &mut Iorb) -> IoStatus {
        iorb.copy_current_to_next();
        iorb.set_completion_routine(filter_completion, 10, sl_control::SL_INVOKE_ON_SUCCESS);
        let _ = iorb.push_down();
        IoStatus::Success
    }

    fn bottom_sync_read_dispatch(iorb: &mut Iorb) -> IoStatus {
        let len = iorb.current().map_or(0, |l| l.length);
        iorb.information = len;
        IoStatus::Success
    }

    fn bottom_cancel_routine(iorb: &mut Iorb) {
        iorb.information = 0xDEAD;
    }

    fn bottom_async_read_dispatch(iorb: &mut Iorb) -> IoStatus {
        iorb.set_cancel_routine(Some(bottom_cancel_routine));
        IoStatus::Pending
    }

    #[test]
    fn device_stack_attach_updates_stack_size_and_rejects_cycles() {
        let mut mgr = IoManager::new();
        let table = [None; MAJOR_FUNCTION_COUNT];
        let drv = mgr.register_driver("\\Driver\\Disk", table, None, false);
        let d1 = mgr.create_device(drv, "\\Device\\Disk0", 512).unwrap();
        let d2 = mgr.create_device(drv, "\\Device\\Part0", 512).unwrap();
        let d3 = mgr.create_device(drv, "\\Device\\Vol0", 4096).unwrap();

        assert_eq!(mgr.attach_device(d2, d1), Ok(2));
        assert_eq!(mgr.attach_device(d3, d2), Ok(3));
        assert_eq!(mgr.device(d1).unwrap().stack_size, 1);
        assert_eq!(mgr.device(d2).unwrap().stack_size, 2);
        assert_eq!(mgr.device(d3).unwrap().stack_size, 3);
        assert_eq!(mgr.top_of_stack(d1), Some(d3));

        // Attaching d1 above d3 would form a cycle (InvalidParameter); attaching a new device above d1 is Busy.
        assert_eq!(mgr.attach_device(d1, d3), Err(IoStatus::InvalidParameter));
        assert_eq!(mgr.attach_device(d3, d1), Err(IoStatus::Busy));
    }

    #[test]
    fn synchronous_iorb_walks_down_and_unwinds_completion_up() {
        let mut mgr = IoManager::new();
        let mut filter_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] =
            [None; MAJOR_FUNCTION_COUNT];
        filter_tbl[major::READ] = Some(filter_read_dispatch);
        let mut bottom_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] =
            [None; MAJOR_FUNCTION_COUNT];
        bottom_tbl[major::READ] = Some(bottom_sync_read_dispatch);

        let bottom_drv = mgr.register_driver("\\Driver\\Bottom", bottom_tbl, None, false);
        let filter_drv = mgr.register_driver("\\Driver\\Filter", filter_tbl, None, true);
        let bottom_dev = mgr
            .create_device(bottom_drv, "\\Device\\Bottom", 4)
            .unwrap();
        let filter_dev = mgr
            .create_device(filter_drv, "\\Device\\Filter", 4)
            .unwrap();
        mgr.attach_device(filter_dev, bottom_dev).unwrap();

        let port = mgr.create_completion_port(2, 8);
        mgr.bind_completion_port(port, 77, 0xCAFE).unwrap();

        let mut iorb = mgr
            .allocate_iorb(filter_dev, major::READ as u8, 77)
            .unwrap();
        iorb.current_mut().unwrap().length = 64;
        let status = mgr.submit_iorb(iorb).unwrap();
        assert_eq!(status, IoStatus::Success);

        // Filter's completion routine added 10 to the bottom driver's 64-byte transfer.
        let pkt = mgr.dequeue_completion(port).unwrap().unwrap();
        assert_eq!(pkt.key, 0xCAFE);
        assert_eq!(pkt.status, IoStatus::Success);
        assert_eq!(pkt.information, 74);
    }

    #[test]
    fn async_iorb_pends_and_cancels_or_completes_cleanly() {
        let mut mgr = IoManager::new();
        let mut filter_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] =
            [None; MAJOR_FUNCTION_COUNT];
        filter_tbl[major::READ] = Some(filter_read_dispatch);
        let mut bottom_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] =
            [None; MAJOR_FUNCTION_COUNT];
        bottom_tbl[major::READ] = Some(bottom_async_read_dispatch);

        let bottom_drv = mgr.register_driver("\\Driver\\AsyncBottom", bottom_tbl, None, false);
        let filter_drv = mgr.register_driver("\\Driver\\Filter", filter_tbl, None, false);
        let bottom_dev = mgr
            .create_device(bottom_drv, "\\Device\\Async0", 4)
            .unwrap();
        let filter_dev = mgr
            .create_device(filter_drv, "\\Device\\Filter0", 4)
            .unwrap();
        mgr.attach_device(filter_dev, bottom_dev).unwrap();

        let port = mgr.create_completion_port(1, 8);
        mgr.bind_completion_port(port, 99, 0xBEEF).unwrap();

        // First async IORB: complete normally.
        let iorb1 = mgr
            .allocate_iorb(filter_dev, major::READ as u8, 99)
            .unwrap();
        let id1 = iorb1.id;
        assert_eq!(mgr.submit_iorb(iorb1), Ok(IoStatus::Pending));
        assert_eq!(mgr.device(filter_dev).unwrap().in_flight_iorbs, 1);

        let outcome = mgr
            .complete_pending_iorb(id1, IoStatus::Success, 128)
            .unwrap();
        assert_eq!(outcome.status, IoStatus::Success);
        assert_eq!(outcome.information, 138);
        assert_eq!(outcome.routines_invoked, 1);
        assert_eq!(mgr.device(filter_dev).unwrap().in_flight_iorbs, 0);

        let pkt1 = mgr.dequeue_completion(port).unwrap().unwrap();
        assert_eq!(pkt1.iorb_id, id1);
        assert_eq!(pkt1.information, 138);

        // Second async IORB: cancel while pending.
        let iorb2 = mgr
            .allocate_iorb(filter_dev, major::READ as u8, 99)
            .unwrap();
        let id2 = iorb2.id;
        assert_eq!(mgr.submit_iorb(iorb2), Ok(IoStatus::Pending));
        assert_eq!(mgr.cancel_pending_iorb(id2), Ok(true));

        let pkt2 = mgr.dequeue_completion(port).unwrap().unwrap();
        assert_eq!(pkt2.iorb_id, id2);
        assert_eq!(pkt2.status, IoStatus::Cancelled);
        assert_eq!(pkt2.information, 0xDEAD);
    }

    #[test]
    fn more_processing_required_halts_upward_completion() {
        let mut iorb = Iorb::new(major::WRITE as u8, 1, 3);
        iorb.set_completion_routine(filter_completion, 100, sl_control::SL_INVOKE_ALWAYS);
        assert!(iorb.push_down());
        iorb.set_completion_routine(stop_completion, 0, sl_control::SL_INVOKE_ALWAYS);
        assert!(iorb.push_down());

        let outcome = iorb.complete(IoStatus::Success, 50);
        assert!(outcome.suspended_by_driver);
        assert_eq!(outcome.routines_invoked, 1);
        // Layer 0's completion routine did not run because layer 1 returned MoreProcessingRequired.
        assert_eq!(outcome.information, 50);
        assert_eq!(iorb.current_stack, 1);
    }

    #[test]
    fn driver_unload_enforces_reference_and_attachment_tracking() {
        UNLOAD_RECORDED.store(0, Ordering::SeqCst);
        let mut mgr = IoManager::new();
        let table = [None; MAJOR_FUNCTION_COUNT];
        let drv = mgr.register_driver("\\Driver\\Unloadable", table, Some(record_unload), false);
        let filter_drv = mgr.register_driver("\\Driver\\Upper", table, None, false);
        let dev = mgr.create_device(drv, "\\Device\\Test", 1).unwrap();
        let upper = mgr.create_device(filter_drv, "\\Device\\Upper", 1).unwrap();

        mgr.attach_device(upper, dev).unwrap();
        mgr.reference_device(dev).unwrap();

        // Refused while attached and referenced.
        assert_eq!(mgr.unload_driver(drv), Err(IoStatus::Busy));

        mgr.detach_device(upper).unwrap();
        // Still refused while open reference count > 0.
        assert_eq!(mgr.unload_driver(drv), Err(IoStatus::Busy));

        mgr.dereference_device(dev).unwrap();
        assert_eq!(mgr.unload_driver(drv), Ok(()));
        assert_eq!(UNLOAD_RECORDED.load(Ordering::SeqCst), drv);
    }

    #[test]
    fn io_status_translates_to_posix_errno_at_one_boundary() {
        assert_eq!(IoStatus::Success.to_errno(), errno::EOK);
        assert_eq!(IoStatus::NoSuchFile.to_errno(), errno::ENOENT);
        assert_eq!(IoStatus::AccessDenied.to_errno(), errno::EACCES);
        assert_eq!(IoStatus::DiskFull.to_errno(), errno::ENOSPC);
        assert_eq!(IoStatus::Cancelled.to_errno(), errno::ECANCELED);
        assert_eq!(IoStatus::AlreadyExists.to_errno(), errno::EEXIST);
        assert_eq!(IoStatus::CrossDeviceLink.to_errno(), errno::EXDEV);
        assert_eq!(IoStatus::SymlinkLoop.to_errno(), errno::ELOOP);
    }
}
