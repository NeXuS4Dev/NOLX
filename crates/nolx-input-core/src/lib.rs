//! PS/2 keyboard rules, as far as they can be decided without touching the controller.
//!
//! The keyboard controller is read and written through two ports: data at 0x60, and status
//! (on read) or command (on write) at 0x64. The keyboard sends scancode set 1 through the data
//! port. A make code presses a key, a break code (the make code with bit 7 set) releases it,
//! 0xE0 prefixes the extended keys, and 0xE1 starts the Pause sequence. This crate decodes
//! those bytes into key events whose key is a USB HID usage code, so PS/2 and the later xHCI
//! path share one key vocabulary. The kernel reads and writes the ports and polls them.
//!
//! Scope: scancode set 1 as the controller translates it, the keys a desktop keyboard has
//! outside Pause and Print Screen, and the device replies that matter during initialisation.
//! Pause and Print Screen are consumed and not reported. Mice are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The data port: bytes from the keyboard, and the data written to it.
pub const DATA_PORT: u16 = 0x60;
/// The status port (read) and command port (write).
pub const STATUS_PORT: u16 = 0x64;

/// Status bit 0: a byte is waiting in the data port for the CPU to read.
pub const STATUS_OUTPUT_FULL: u8 = 1 << 0;
/// Status bit 1: the controller has not yet taken the last byte written, so do not write.
pub const STATUS_INPUT_FULL: u8 = 1 << 1;
/// Status bit 6: the device did not answer in time.
pub const STATUS_TIMEOUT: u8 = 1 << 6;
/// Status bit 7: the last byte had a parity error.
pub const STATUS_PARITY: u8 = 1 << 7;

/// Controller command: read the configuration byte.
pub const CMD_READ_CONFIG: u8 = 0x20;
/// Controller command: write the configuration byte (the next data byte).
pub const CMD_WRITE_CONFIG: u8 = 0x60;
/// Controller command: disable the first (keyboard) port.
pub const CMD_DISABLE_KEYBOARD: u8 = 0xAD;
/// Controller command: enable the first (keyboard) port.
pub const CMD_ENABLE_KEYBOARD: u8 = 0xAE;

/// Configuration bit 0: raise the keyboard interrupt when a byte arrives.
pub const CONFIG_KEYBOARD_IRQ: u8 = 1 << 0;
/// Configuration bit 4: the keyboard clock is disabled.
pub const CONFIG_KEYBOARD_CLOCK_OFF: u8 = 1 << 4;
/// Configuration bit 6: the controller translates set 2 to set 1, which is what the decoder reads.
pub const CONFIG_TRANSLATION: u8 = 1 << 6;

/// Device reply: acknowledge.
pub const DEVICE_ACK: u8 = 0xFA;
/// Device reply: self-test passed.
pub const DEVICE_SELF_TEST_OK: u8 = 0xAA;
/// Device reply: resend the last command.
pub const DEVICE_RESEND: u8 = 0xFE;

/// Whether the controller has a byte for the CPU to read.
#[must_use]
pub fn output_full(status: u8) -> bool {
    status & STATUS_OUTPUT_FULL != 0
}

/// Whether the controller is still holding a byte the CPU wrote, so a new write must wait.
#[must_use]
pub fn input_full(status: u8) -> bool {
    status & STATUS_INPUT_FULL != 0
}

/// Whether the last byte was a timeout or a parity error. Such a byte is not to be trusted.
#[must_use]
pub fn transfer_failed(status: u8) -> bool {
    status & (STATUS_TIMEOUT | STATUS_PARITY) != 0
}

/// The configuration byte to write so the keyboard raises its interrupt, the controller
/// translates to set 1, and the keyboard clock runs. Other bits are kept.
#[must_use]
pub fn keyboard_config(config: u8) -> u8 {
    (config | CONFIG_KEYBOARD_IRQ | CONFIG_TRANSLATION) & !CONFIG_KEYBOARD_CLOCK_OFF
}

/// What a byte from the keyboard means during initialisation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceReply {
    /// The keyboard accepted the last command.
    Ack,
    /// The keyboard rejected the last command and wants it sent again.
    Resend,
    /// The keyboard's power-on self-test passed.
    SelfTestOk,
    /// Any other byte: a key, or a reply the initialisation does not wait for.
    Other,
}

/// Classify a byte received while waiting for a device reply.
#[must_use]
pub fn classify_reply(byte: u8) -> DeviceReply {
    match byte {
        DEVICE_ACK => DeviceReply::Ack,
        DEVICE_RESEND => DeviceReply::Resend,
        DEVICE_SELF_TEST_OK => DeviceReply::SelfTestOk,
        _ => DeviceReply::Other,
    }
}

/// A key press or release, with the key as a USB HID keyboard usage code (usage page 0x07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The HID usage code: 0x04 to 0xA4 for ordinary keys, 0xE0 to 0xE7 for the modifiers.
    pub usage: u8,
    /// True for a press, false for a release.
    pub pressed: bool,
}

/// The Pause sequence is E1 1D 45 E1 9D C5: after the E1 byte, five more bytes follow.
const PAUSE_TAIL: u8 = 5;

/// Where the decoder is in a byte sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Between keys.
    Normal,
    /// After 0xE0: the next byte is an extended key.
    Extended,
    /// Inside a Pause sequence, with this many bytes still to skip.
    Pause(u8),
}

/// The scancode set 1 decoder. Feed it each byte the controller delivers, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoder {
    state: State,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder between keys.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: State::Normal,
        }
    }

    /// Consume one byte. Returns the key event it completes, if any. Prefix bytes, the Pause
    /// sequence, and unknown codes return `None`.
    pub fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        match self.state {
            State::Pause(remaining) => {
                self.state = if remaining <= 1 {
                    State::Normal
                } else {
                    State::Pause(remaining - 1)
                };
                None
            }
            State::Extended => {
                self.state = State::Normal;
                key(byte, true)
            }
            State::Normal => match byte {
                0xE0 => {
                    self.state = State::Extended;
                    None
                }
                0xE1 => {
                    self.state = State::Pause(PAUSE_TAIL);
                    None
                }
                _ => key(byte, false),
            },
        }
    }
}

/// The key a byte names, after its prefix. Bit 7 marks a release.
fn key(byte: u8, extended: bool) -> Option<KeyEvent> {
    let pressed = byte & 0x80 == 0;
    let usage = usage(byte & 0x7F, extended)?;
    Some(KeyEvent { usage, pressed })
}

/// The HID usage code for a make code of scancode set 1, or `None` for a code this table does
/// not name. `extended` selects the table of keys that follow 0xE0.
#[must_use]
pub fn usage(code: u8, extended: bool) -> Option<u8> {
    if extended {
        return match code {
            0x1C => Some(0x58),
            0x1D => Some(0xE4),
            0x35 => Some(0x54),
            0x38 => Some(0xE6),
            0x47 => Some(0x4A),
            0x48 => Some(0x52),
            0x49 => Some(0x4B),
            0x4B => Some(0x50),
            0x4D => Some(0x4F),
            0x4F => Some(0x4D),
            0x50 => Some(0x51),
            0x51 => Some(0x4E),
            0x52 => Some(0x49),
            0x53 => Some(0x4C),
            0x5B => Some(0xE3),
            0x5C => Some(0xE7),
            _ => None,
        };
    }
    Some(match code {
        0x01 => 0x29,
        0x02..=0x0A => code - 0x02 + 0x1E,
        0x0B => 0x27,
        0x0C => 0x2D,
        0x0D => 0x2E,
        0x0E => 0x2A,
        0x0F => 0x2B,
        0x10 => 0x14,
        0x11 => 0x1A,
        0x12 => 0x08,
        0x13 => 0x15,
        0x14 => 0x17,
        0x15 => 0x1C,
        0x16 => 0x18,
        0x17 => 0x0C,
        0x18 => 0x12,
        0x19 => 0x13,
        0x1A => 0x2F,
        0x1B => 0x30,
        0x1C => 0x28,
        0x1D => 0xE0,
        0x1E => 0x04,
        0x1F => 0x16,
        0x20 => 0x07,
        0x21 => 0x09,
        0x22 => 0x0A,
        0x23 => 0x0B,
        0x24 => 0x0D,
        0x25 => 0x0E,
        0x26 => 0x0F,
        0x27 => 0x33,
        0x28 => 0x34,
        0x29 => 0x35,
        0x2A => 0xE1,
        0x2B => 0x31,
        0x2C => 0x1D,
        0x2D => 0x1B,
        0x2E => 0x06,
        0x2F => 0x19,
        0x30 => 0x05,
        0x31 => 0x11,
        0x32 => 0x10,
        0x33 => 0x36,
        0x34 => 0x37,
        0x35 => 0x38,
        0x36 => 0xE5,
        0x38 => 0xE2,
        0x39 => 0x2C,
        0x3A => 0x39,
        0x3B..=0x44 => code - 0x3B + 0x3A,
        0x46 => 0x47,
        0x57 => 0x44,
        0x58 => 0x45,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
