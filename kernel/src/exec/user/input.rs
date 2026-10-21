//! Input devices as POSIX descriptors: `open(2)` on the Executive name, `read(2)` on the fd.
//!
//! The Executive binding publishes each attached device as an object named `\Device\InputN`.
//! That name is the identity a program uses, and it is what lets the two layers meet without
//! either one lying about what it is:
//!
//! 1. `open(2)` resolves the path as an **Executive** path. The Object Manager's DACL decides,
//!    through the same `open` the spawn path uses, and only then is a reader created. A refused
//!    token leaves no descriptor behind. The path is deliberately *not* a VFS mount point:
//!    `\Device\Input0` is not a file, and a synthetic inode would make `stat`, `readdir` and
//!    `unlink` answer questions nobody implemented.
//! 2. The device incarnation, not the name, is what the descriptor reads: a device that
//!    disconnects and is replaced by a later `\Device\Input0` drains its own queue and then
//!    returns EOF, rather than resolving to whatever reused the name.
//! 3. `read(2)` returns the eight-byte record [`nolx_input_core::wire`] defines, one per call.
//!    The descriptor is non-blocking: an empty queue is `EAGAIN`, which is the honest answer
//!    until a scheduler-blocking wait is wired to the object's auto-reset waitable. That is the
//!    remaining piece of the input interface and it is recorded as open, not implied here.
//!
//! The descriptor table is fixed-size and keyed by process slot and fd: two processes that open
//! the same device get independent queues, and closing one never disturbs the other.

use nolx_input_core::events::Source;
use nolx_input_core::objects::{DeviceId, ReaderId};
use nolx_input_core::wire;
use nolx_types::errno;

use crate::hal::input;
use crate::obj::boot::{self, ObjectId};

/// Device descriptors this table holds across every process.
pub const MAX_DESCRIPTORS: usize = 8;

/// One open descriptor. Plain data, so the table needs no allocator and no constructor.
#[derive(Clone, Copy)]
struct Descriptor {
    /// Process slot that opened it.
    owner: usize,
    /// The fd number in that process's descriptor table.
    fd: usize,
    /// The device incarnation, which is what the reader is bound to.
    device: DeviceId,
    /// This descriptor's own queue.
    reader: ReaderId,
    /// The last source seen, so a resynchronisation record can name a device class.
    source: Option<Source>,
}

static DESCRIPTORS: crate::sync::IrqMutex<[Option<Descriptor>; MAX_DESCRIPTORS]> =
    crate::sync::IrqMutex::new([const { None }; MAX_DESCRIPTORS]);

/// How many device descriptors are open, for the boot evidence.
pub fn open_count() -> usize {
    DESCRIPTORS.lock().iter().flatten().count()
}

/// The `InputN` stem of an Executive device path, if the path names one.
///
/// Both `\Device\InputN` and `/Device/InputN` are accepted: a program written against slashes
/// should not have to know the Executive namespace uses a backslash.
fn path_stem(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix("\\Device\\")
        .or_else(|| path.strip_prefix("/Device/"))?;
    let digits = rest.strip_prefix("Input")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if rest.contains(['/', '\\']) {
        return None;
    }
    Some(rest)
}

/// Whether `path` names a device object this module serves.
#[must_use]
pub fn is_device_path(path: &str) -> bool {
    path_stem(path).is_some()
}

/// Open `path` for process `slot` as device descriptor number `fd`.
///
/// The access check happens here, through the Object Manager, so a token the descriptor's DACL
/// refuses never reaches a reader.
///
/// # Errors
///
/// `ENODEV` when no published object has that name or it is not an input device, `EACCES` when
/// the DACL refuses the token, and `EMFILE` when this table is full.
pub fn open(slot: usize, path: &str, fd: usize) -> Result<(), i32> {
    let stem = path_stem(path).ok_or(errno::ENODEV)?;
    let executive_path = crate::hal::input::executive::path(stem);
    let object = boot::lookup(executive_path).map_err(|_| errno::ENODEV)?;
    // Drain the check without keeping the handle: the process already holds one from its spawn,
    // and a second would be a reference the descriptor does not need in order to read.
    let mut checked = nolx_obj_core::handle::HandleTable::<ObjectId, 1>::new();
    let handle = boot::open(
        executive_path,
        &crate::hal::input::executive::process_token(),
        &mut checked,
        crate::hal::input::executive::INPUT_ACCESS,
    )
    .map_err(|error| match error {
        boot::ObjError::AccessDenied { .. } | boot::ObjError::UnsupportedAccess { .. } => {
            errno::EACCES
        }
        _ => errno::ENODEV,
    })?;
    let _ = boot::close(&mut checked, handle);
    let device = crate::hal::input::executive::device_of(object).ok_or(errno::ENODEV)?;
    let reader = input::open(device).map_err(|_| errno::ENODEV)?;
    let mut table = DESCRIPTORS.lock();
    let Some(index) = table.iter().position(Option::is_none) else {
        drop(table);
        let _ = input::close(reader);
        return Err(errno::EMFILE);
    };
    table[index] = Some(Descriptor {
        owner: slot,
        fd,
        device,
        reader,
        source: None,
    });
    Ok(())
}

/// Whether `slot`'s descriptor `fd` is a device descriptor rather than a VFS one.
#[must_use]
pub fn is_device_descriptor(slot: usize, fd: usize) -> bool {
    DESCRIPTORS
        .lock()
        .iter()
        .flatten()
        .any(|entry| entry.owner == slot && entry.fd == fd)
}

/// One `read(2)` outcome: a record to copy into user memory, or end of file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// An eight-byte record.
    Record([u8; wire::RECORD_BYTES]),
    /// The device is gone and its queue has drained: a zero-length read.
    Eof,
}

/// Read one record from `slot`'s device descriptor `fd`.
///
/// The caller supplies the destination and its length; a buffer shorter than one record is
/// refused rather than truncated, because handing a program half a record would let it read a
/// field from the next event as if it belonged to this one.
///
/// # Errors
///
/// `EBADF` for a descriptor this module does not own, `EAGAIN` when the queue is empty, and
/// `EIO` if a reader ever receives another device's record or the registry refuses the reader.
pub fn read(slot: usize, fd: usize) -> Result<ReadResult, i32> {
    let index = DESCRIPTORS
        .lock()
        .iter()
        .position(|entry| {
            entry
                .as_ref()
                .is_some_and(|entry| entry.owner == slot && entry.fd == fd)
        })
        .ok_or(errno::EBADF)?;
    let (reader, device) = {
        let table = DESCRIPTORS.lock();
        let entry = table[index].as_ref().ok_or(errno::EBADF)?;
        (entry.reader, entry.device)
    };
    match input::read(reader).map_err(|_| errno::EIO)? {
        input::Read::Event(record) => {
            if record.device != device {
                // A reader only ever receives records from the incarnation it subscribed to; if
                // that ever breaks, refuse rather than hand this process another device's input.
                return Err(errno::EIO);
            }
            let bytes = wire::encode(&record);
            if let Some(entry) = DESCRIPTORS.lock()[index].as_mut() {
                entry.source = Some(record.event.source);
            }
            Ok(ReadResult::Record(bytes))
        }
        input::Read::Resync { dropped, .. } => {
            let source = DESCRIPTORS.lock()[index]
                .as_ref()
                .and_then(|entry| entry.source)
                .unwrap_or(Source::Ps2Keyboard);
            Ok(ReadResult::Record(wire::encode_resync(source, dropped)))
        }
        input::Read::Empty => Err(errno::EAGAIN),
        input::Read::Disconnected => Ok(ReadResult::Eof),
    }
}

/// Close `slot`'s device descriptor `fd`, releasing its reader.
///
/// Returns `false` when `fd` is not a device descriptor, so the caller falls through to the VFS.
pub fn close(slot: usize, fd: usize) -> bool {
    let entry = {
        let mut table = DESCRIPTORS.lock();
        let Some(index) = table.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|entry| entry.owner == slot && entry.fd == fd)
        }) else {
            return false;
        };
        table[index].take()
    };
    let Some(entry) = entry else {
        return false;
    };
    // Outside the descriptor lock: closing a reader takes the input registry's lock, which the
    // interrupt path also takes to publish a record.
    let _ = input::close(entry.reader);
    true
}

/// Release every device descriptor a process still holds, before its handles are torn down.
pub fn release_process(slot: usize) {
    loop {
        let fd = {
            let table = DESCRIPTORS.lock();
            table
                .iter()
                .flatten()
                .find(|entry| entry.owner == slot)
                .map(|entry| entry.fd)
        };
        let Some(fd) = fd else {
            break;
        };
        if !close(slot, fd) {
            break;
        }
    }
}
