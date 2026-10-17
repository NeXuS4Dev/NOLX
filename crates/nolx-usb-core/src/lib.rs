//! USB host rules for NOLX, decided over bytes: xHCI registers and ports, the transfer request
//! blocks the controller reads and writes, and the HID boot keyboard report.
//!
//! The kernel maps the controller and moves the rings; these rules say what the registers and
//! TRBs mean and turn two keyboard reports into key presses and releases. Key events reuse
//! [`nolx_input_core::KeyEvent`], so a USB keyboard and a PS/2 keyboard produce the same events.
//!
//! Scope: the xHCI operations this crate names (run/stop, reset, port reset, enable slot), the
//! link, normal, and event TRBs, and the boot protocol keyboard report. Hubs, isochronous and
//! interrupt transfers, and class drivers other than the keyboard are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use nolx_input_core::KeyEvent;
pub mod device;

// Capability registers, from the controller's base.

/// Capability length: one byte, the offset of the operational registers.
pub const CAP_CAPLENGTH: u64 = 0x00;
/// Structural parameters 1: maximum device slots (bits 7:0), interrupters (18:8), and ports (31:24).
pub const CAP_HCSPARAMS1: u64 = 0x04;
/// Doorbell array offset (bits 31:2 are the offset).
pub const CAP_DBOFF: u64 = 0x14;

// Operational registers, from the operational base (the capability length).

/// USB command.
pub const OP_USBCMD: u64 = 0x00;
/// USB status.
pub const OP_USBSTS: u64 = 0x04;
/// Command ring control (64-bit).
pub const OP_CRCR: u64 = 0x18;
/// Device context base address array pointer (64-bit).
pub const OP_DCBAAP: u64 = 0x30;
/// Configure: the number of device slots enabled (bits 7:0).
pub const OP_CONFIG: u64 = 0x38;

/// USBCMD bit 0: run the controller.
pub const USBCMD_RUN: u32 = 1 << 0;
/// USBCMD bit 1: reset the controller.
pub const USBCMD_HCRST: u32 = 1 << 1;
/// USBSTS bit 0: the controller is halted.
pub const USBSTS_HCHALTED: u32 = 1 << 0;
/// USBSTS bit 11: the controller is not ready after a reset.
pub const USBSTS_CNR: u32 = 1 << 11;

/// Port register sets start this far past the operational base, and each is this large.
pub const PORT_SET_BASE: u64 = 0x400;
/// Bytes in one port register set.
pub const PORT_SET_STRIDE: u64 = 0x10;

/// PORTSC bit 0: a device is connected.
pub const PORTSC_CONNECTED: u32 = 1 << 0;
/// PORTSC bit 1: the port is enabled. Writing 1 disables it.
pub const PORTSC_ENABLED: u32 = 1 << 1;
/// PORTSC bit 4: port reset. Writing 1 starts a reset.
pub const PORTSC_RESET: u32 = 1 << 4;
/// PORTSC bit 9: port power.
pub const PORTSC_POWER: u32 = 1 << 9;
/// PORTSC bit 17: connect status changed. Write 1 to clear.
pub const PORTSC_CONNECT_CHANGE: u32 = 1 << 17;
/// PORTSC bit 21: port reset completed. Write 1 to clear.
pub const PORTSC_RESET_CHANGE: u32 = 1 << 21;
/// The PORTSC bits that clear when a 1 is written to them. Writing back a register that keeps
/// one of these set would clear it, so these are masked out of a write unless meant.
pub const PORTSC_WRITE_ONE_TO_CLEAR: u32 =
    (1 << 1) | (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);
/// The change bits: each set one is an event the driver must acknowledge.
pub const PORTSC_CHANGE_BITS: u32 =
    (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);

/// The offset, from the controller's base, of port `port`'s register set. Ports count from 1.
#[must_use]
pub fn port_register_offset(caplength: u8, port: u8) -> Option<u64> {
    if port == 0 {
        return None;
    }
    Some(u64::from(caplength) + PORT_SET_BASE + PORT_SET_STRIDE * u64::from(port - 1))
}

/// The most ports the controller reports, from HCSPARAMS1.
#[must_use]
pub fn max_ports(hcsparams1: u32) -> u8 {
    (hcsparams1 >> 24) as u8
}

/// The most device slots the controller reports, from HCSPARAMS1.
#[must_use]
pub fn max_slots(hcsparams1: u32) -> u8 {
    hcsparams1 as u8
}

/// The offset of a slot's doorbell from the controller's base. Slot 0 is the command ring's.
#[must_use]
pub fn doorbell_offset(dboff: u32, slot: u8) -> u64 {
    u64::from(dboff & !0x3) + 4 * u64::from(slot)
}

/// The value to write to PORTSC to start a reset of a port: the port's state kept, its change
/// bits left alone, and the reset bit set. Write-one-to-clear bits are masked, so a set
/// enable bit is not disabled by the write.
#[must_use]
pub fn portsc_start_reset(portsc: u32) -> u32 {
    (portsc & !PORTSC_WRITE_ONE_TO_CLEAR) | PORTSC_RESET
}

/// The value to write to PORTSC to acknowledge the change bits that are set, and nothing else.
#[must_use]
pub fn portsc_acknowledge(portsc: u32) -> u32 {
    (portsc & !PORTSC_WRITE_ONE_TO_CLEAR) | (portsc & PORTSC_CHANGE_BITS)
}

/// One transfer request block: 16 bytes the driver and controller share through a ring.
pub type Trb = [u8; 16];

/// Bytes in one TRB.
pub const TRB_BYTES: usize = 16;
/// TRB type: normal (a data buffer).
pub const TRB_NORMAL: u8 = 1;
/// TRB type: enable slot (command).
pub const TRB_ENABLE_SLOT: u8 = 9;
/// TRB type: link (points the ring to its next segment).
pub const TRB_LINK: u8 = 6;
/// TRB type: transfer event.
pub const TRB_TRANSFER_EVENT: u8 = 32;
/// TRB type: command completion event.
pub const TRB_COMMAND_COMPLETION: u8 = 33;
/// TRB type: port status change event.
pub const TRB_PORT_STATUS_CHANGE: u8 = 34;
/// Completion code: success.
pub const CC_SUCCESS: u8 = 1;

/// The most a normal TRB's transfer length field can name: 17 bits.
pub const TRB_LENGTH_MAX: u32 = 0x1_FFFF;

fn word(trb: &Trb, at: usize) -> u32 {
    u32::from_le_bytes([trb[at], trb[at + 1], trb[at + 2], trb[at + 3]])
}

fn put_word(trb: &mut Trb, at: usize, value: u32) {
    trb[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The TRB type, from bits 15:10 of the control dword.
#[must_use]
pub fn trb_type(trb: &Trb) -> u8 {
    ((word(trb, 12) >> 10) & 0x3F) as u8
}

/// The cycle bit, which the producer flips each time the ring wraps.
#[must_use]
pub fn cycle_bit(trb: &Trb) -> bool {
    word(trb, 12) & 1 != 0
}

/// A link TRB pointing the ring at `target`, which must be 16-byte aligned. With `toggle_cycle`
/// the consumer flips its cycle sense when it follows the link.
#[must_use]
pub fn link_trb(target: u64, toggle_cycle: bool, cycle: bool) -> Option<Trb> {
    if target & 0xF != 0 {
        return None;
    }
    let mut trb = [0u8; TRB_BYTES];
    trb[0..8].copy_from_slice(&target.to_le_bytes());
    let control = (u32::from(TRB_LINK) << 10) | (u32::from(toggle_cycle) << 1) | u32::from(cycle);
    put_word(&mut trb, 12, control);
    Some(trb)
}

/// A normal TRB moving `length` bytes between memory at `buffer` and the device. Returns `None`
/// for a length the field cannot name. With `interrupt_on_completion` the controller posts an
/// event when it is done.
#[must_use]
pub fn normal_trb(
    buffer: u64,
    length: u32,
    interrupt_on_completion: bool,
    cycle: bool,
) -> Option<Trb> {
    if length > TRB_LENGTH_MAX {
        return None;
    }
    let mut trb = [0u8; TRB_BYTES];
    trb[0..8].copy_from_slice(&buffer.to_le_bytes());
    put_word(&mut trb, 8, length);
    let control = (u32::from(TRB_NORMAL) << 10)
        | (u32::from(interrupt_on_completion) << 5)
        | u32::from(cycle);
    put_word(&mut trb, 12, control);
    Some(trb)
}

/// An enable-slot command TRB.
#[must_use]
pub fn enable_slot_trb(cycle: bool) -> Trb {
    let mut trb = [0u8; TRB_BYTES];
    put_word(
        &mut trb,
        12,
        (u32::from(TRB_ENABLE_SLOT) << 10) | u32::from(cycle),
    );
    trb
}

/// The completion code of an event TRB, from bits 31:24 of its status dword.
#[must_use]
pub fn completion_code(trb: &Trb) -> u8 {
    (word(trb, 8) >> 24) as u8
}

/// The slot an event TRB names, from bits 31:24 of its control dword.
#[must_use]
pub fn event_slot(trb: &Trb) -> u8 {
    (word(trb, 12) >> 24) as u8
}

/// The port a port status change event names, from bits 31:24 of its first dword.
#[must_use]
pub fn event_port(trb: &Trb) -> u8 {
    (word(trb, 0) >> 24) as u8
}

/// Bytes in a boot protocol keyboard report: modifiers, a reserved byte, and six key codes.
pub type BootReport = [u8; 8];
/// The report value that means more keys were pressed than the report can hold.
pub const ERROR_ROLL_OVER: u8 = 1;
/// The most key events one report pair can produce: eight modifier bits, six releases, six presses.
pub const MAX_REPORT_EVENTS: usize = 20;

/// The key events one report produced, in the order they happened: modifiers, releases, presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportEvents {
    items: [Option<KeyEvent>; MAX_REPORT_EVENTS],
    len: usize,
}

impl Default for ReportEvents {
    fn default() -> Self {
        Self {
            items: [None; MAX_REPORT_EVENTS],
            len: 0,
        }
    }
}

impl ReportEvents {
    /// The events, in order.
    pub fn iter(&self) -> impl Iterator<Item = KeyEvent> + '_ {
        self.items[..self.len].iter().flatten().copied()
    }

    /// How many events there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, event: KeyEvent) {
        // Each pass below can add at most the bound, so the slot is always there.
        self.items[self.len] = Some(event);
        self.len += 1;
    }
}

/// The key events between two keyboard reports. The modifier byte gives one usage per bit
/// (0xE0 to 0xE7); the key codes give the usages of the keys held. Usages 0 to 3 are not keys
/// (none, roll-over, POST failure, undefined), so they produce nothing.
///
/// Returns `None` when the new report says too many keys were held. Such a report is not to be
/// acted on, and the previous state stays in force.
#[must_use]
pub fn report_events(previous: &BootReport, next: &BootReport) -> Option<ReportEvents> {
    if next[2..8].contains(&ERROR_ROLL_OVER) {
        return None;
    }
    let mut out = ReportEvents::default();
    let changed = previous[0] ^ next[0];
    for bit in 0..8u8 {
        let mask = 1u8 << bit;
        if changed & mask != 0 {
            out.push(KeyEvent {
                usage: 0xE0 + bit,
                pressed: next[0] & mask != 0,
            });
        }
    }
    let is_key = |usage: u8| usage >= 0x04;
    for &usage in previous[2..8].iter().filter(|&&u| is_key(u)) {
        if !next[2..8].contains(&usage) {
            out.push(KeyEvent {
                usage,
                pressed: false,
            });
        }
    }
    for &usage in next[2..8].iter().filter(|&&u| is_key(u)) {
        if !previous[2..8].contains(&usage) {
            out.push(KeyEvent {
                usage,
                pressed: true,
            });
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
