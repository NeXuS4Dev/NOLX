//! The eight-byte record a `read(2)` on an input device returns.
//!
//! A POSIX `read(2)` returns bytes, so an input device needs a byte encoding before a program
//! can see its events at all. This is that encoding, and it is deliberately the smallest one
//! that carries everything the kernel already tracks:
//!
//! ```text
//! byte 0      class: 1 keyboard, 2 pointer, 3 resync (records were dropped)
//! byte 1      source: 1 PS/2 keyboard, 2 PS/2 mouse, 3 USB HID
//! bytes 2..4  code, little-endian: HID usage for a key, button mask for a pointer
//! bytes 4..8  value, little-endian:
//!               key     1 pressed, 0 released
//!               pointer dx in the low half, dy in the high half, both signed 16-bit
//!               resync  records dropped since the reader last drained
//! ```
//!
//! Every field is fixed-width and little-endian, so a program can read the record straight into
//! a struct or compare it byte for byte. A reader that falls behind receives a `Resync` record
//! *before* any later event: the kernel's registry guarantees that ordering, and this encoding
//! keeps it visible by giving resync its own class rather than smuggling it into an event.

use crate::events::{Kind, Source};
use crate::objects::Record;

/// Bytes in one encoded record.
pub const RECORD_BYTES: usize = 8;

/// Class byte: a key press or release.
pub const CLASS_KEYBOARD: u8 = 1;
/// Class byte: relative pointer motion and button state.
pub const CLASS_POINTER: u8 = 2;
/// Class byte: the reader lost records and was resynchronised.
pub const CLASS_RESYNC: u8 = 3;

/// Source byte: the i8042 keyboard port.
pub const SOURCE_PS2_KEYBOARD: u8 = 1;
/// Source byte: the i8042 auxiliary port.
pub const SOURCE_PS2_MOUSE: u8 = 2;
/// Source byte: a USB HID interface.
pub const SOURCE_USB_HID: u8 = 3;

/// The source byte for a producer class.
#[must_use]
pub const fn source_byte(source: Source) -> u8 {
    match source {
        Source::Ps2Keyboard => SOURCE_PS2_KEYBOARD,
        Source::Ps2Mouse => SOURCE_PS2_MOUSE,
        Source::UsbHid => SOURCE_USB_HID,
    }
}

/// The class byte for an event payload.
#[must_use]
pub const fn class_byte(kind: &Kind) -> u8 {
    match kind {
        Kind::Key(_) => CLASS_KEYBOARD,
        Kind::Pointer(_) => CLASS_POINTER,
    }
}

/// Encode one delivered record.
#[must_use]
pub fn encode(record: &Record) -> [u8; RECORD_BYTES] {
    let (code, value) = match record.event.kind {
        Kind::Key(key) => (u16::from(key.usage), u32::from(u8::from(key.pressed))),
        Kind::Pointer(p) => (
            u16::from(p.buttons),
            u32::from(p.dx as u16) | (u32::from(p.dy as u16) << 16),
        ),
    };
    let mut out = [0u8; RECORD_BYTES];
    out[0] = class_byte(&record.event.kind);
    out[1] = source_byte(record.event.source);
    out[2..4].copy_from_slice(&code.to_le_bytes());
    out[4..8].copy_from_slice(&value.to_le_bytes());
    out
}

/// Encode a resynchronisation: the reader fell behind by `dropped` records.
#[must_use]
pub fn encode_resync(source: Source, dropped: u64) -> [u8; RECORD_BYTES] {
    let mut out = [0u8; RECORD_BYTES];
    out[0] = CLASS_RESYNC;
    out[1] = source_byte(source);
    out[4..8].copy_from_slice(&(dropped.min(u64::from(u32::MAX)) as u32).to_le_bytes());
    out
}

/// Encode the end of a disconnected device: zero bytes, which `read(2)` also signals.
///
/// There is nothing to encode -- EOF is a zero-length read -- so the value exists only to make
/// that decision explicit at the call site rather than as an unexplained `Ok(0)`.
#[must_use]
pub const fn eof() -> usize {
    0
}
