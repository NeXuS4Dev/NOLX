//! Intel e1000 network controller rules, decided without touching the device.
//!
//! The controller's registers sit in its memory BAR. The driver reads the MAC address out of the
//! EEPROM (or the receive address registers), sets up a receive and a transmit ring of legacy
//! descriptors, enables the two engines, and then moves frames by writing descriptors and tail
//! pointers. This crate holds what those values mean: the register offsets, the bit fields of
//! the control registers, the descriptor layouts, and the arithmetic of the rings. The kernel
//! maps the registers, allocates the rings, and polls.
//!
//! Scope: the legacy descriptor format and one receive and one transmit queue with polled
//! completions. Extended and advanced descriptors, checksum offload, VLAN tags, interrupt
//! moderation, and multiple queues are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Intel's PCI vendor ID.
pub const VENDOR_INTEL: u16 = 0x8086;
/// The 82540EM, the device QEMU presents as `e1000`.
pub const DEVICE_82540EM: u16 = 0x100E;

/// Device control.
pub const REG_CTRL: u64 = 0x0000;
/// Device status.
pub const REG_STATUS: u64 = 0x0008;
/// EEPROM read.
pub const REG_EERD: u64 = 0x0014;
/// Interrupt cause read.
pub const REG_ICR: u64 = 0x00C0;
/// Interrupt mask set.
pub const REG_IMS: u64 = 0x00D0;
/// Receive control.
pub const REG_RCTL: u64 = 0x0100;
/// Transmit control.
pub const REG_TCTL: u64 = 0x0400;
/// Receive descriptor base, low and high dwords.
pub const REG_RDBAL: u64 = 0x2800;
/// Receive descriptor base, high dword.
pub const REG_RDBAH: u64 = 0x2804;
/// Receive descriptor ring length in bytes.
pub const REG_RDLEN: u64 = 0x2808;
/// Receive descriptor head, owned by the device.
pub const REG_RDH: u64 = 0x2810;
/// Receive descriptor tail, owned by the driver.
pub const REG_RDT: u64 = 0x2818;
/// Transmit descriptor base, low dword.
pub const REG_TDBAL: u64 = 0x3800;
/// Transmit descriptor base, high dword.
pub const REG_TDBAH: u64 = 0x3804;
/// Transmit descriptor ring length in bytes.
pub const REG_TDLEN: u64 = 0x3808;
/// Transmit descriptor head, owned by the device.
pub const REG_TDH: u64 = 0x3810;
/// Transmit descriptor tail, owned by the driver.
pub const REG_TDT: u64 = 0x3818;
/// Receive address low: the first four bytes of the MAC address.
pub const REG_RAL0: u64 = 0x5400;
/// Receive address high: the last two bytes, and the valid bit.
pub const REG_RAH0: u64 = 0x5404;

/// CTRL bit 6: set link up.
pub const CTRL_SLU: u32 = 1 << 6;
/// CTRL bit 26: device reset. It clears itself when done.
pub const CTRL_RST: u32 = 1 << 26;
/// STATUS bit 1: the link is up.
pub const STATUS_LINK_UP: u32 = 1 << 1;

/// EERD bit 0: start a read.
pub const EERD_START: u32 = 1 << 0;
/// EERD bit 4: the read is done, and the data is in bits 31:16.
pub const EERD_DONE: u32 = 1 << 4;

/// RCTL bit 1: receiver enable.
pub const RCTL_EN: u32 = 1 << 1;
/// RCTL bit 15: accept broadcast frames.
pub const RCTL_BAM: u32 = 1 << 15;
/// RCTL bit 26: strip the Ethernet CRC before the driver sees the frame.
pub const RCTL_SECRC: u32 = 1 << 26;
/// TCTL bit 1: transmitter enable.
pub const TCTL_EN: u32 = 1 << 1;
/// TCTL bit 3: pad short frames.
pub const TCTL_PSP: u32 = 1 << 3;
/// TCTL bits 11:4: the collision threshold the specification recommends, 15.
pub const TCTL_CT: u32 = 0x0F << 4;
/// TCTL bits 21:12: the collision distance for full duplex, 64.
pub const TCTL_COLD: u32 = 0x40 << 12;

/// ICR bit 0: a transmit descriptor was written back.
pub const ICR_TXDW: u32 = 1 << 0;
/// ICR bit 2: the link status changed.
pub const ICR_LSC: u32 = 1 << 2;
/// ICR bit 7: a packet was received.
pub const ICR_RXT0: u32 = 1 << 7;

/// RAH bit 31: the address in this register pair is valid.
pub const RAH_VALID: u32 = 1 << 31;

/// Bytes in one legacy receive or transmit descriptor.
pub const DESCRIPTOR_BYTES: usize = 16;
/// The smallest and largest ring the driver accepts, in descriptors. A ring is a multiple of 8
/// descriptors, so its length in bytes is a multiple of the 128-byte alignment the hardware needs.
pub const RING_MIN: u32 = 8;
/// See [`RING_MIN`].
pub const RING_MAX: u32 = 4096;
/// The largest buffer a legacy transmit descriptor may name.
pub const TX_LENGTH_MAX: u16 = 16288;

/// Legacy transmit command: end of packet.
pub const TX_CMD_EOP: u8 = 1 << 0;
/// Legacy transmit command: insert the Ethernet CRC.
pub const TX_CMD_IFCS: u8 = 1 << 1;
/// Legacy transmit command: report status when done.
pub const TX_CMD_RS: u8 = 1 << 3;
/// Descriptor status bit 0: the device is done with the descriptor.
pub const DESC_DD: u8 = 1 << 0;
/// Legacy receive status bit 1: this descriptor ends a packet.
pub const RX_STATUS_EOP: u8 = 1 << 1;

/// The value to write to EERD to read EEPROM word `word`.
#[must_use]
pub fn eerd_read(word: u8) -> u32 {
    (u32::from(word) << 8) | EERD_START
}

/// The EEPROM word an EERD value holds once its read is done, or `None` while it is still busy.
#[must_use]
pub fn eerd_result(eerd: u32) -> Option<u16> {
    (eerd & EERD_DONE != 0).then_some((eerd >> 16) as u16)
}

/// The MAC address held in EEPROM words 0 to 2, each word little-endian.
#[must_use]
pub fn mac_from_eeprom(words: [u16; 3]) -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (index, word) in words.iter().enumerate() {
        mac[index * 2..index * 2 + 2].copy_from_slice(&word.to_le_bytes());
    }
    mac
}

/// The values for RAL0 and RAH0 that make `mac` the receive address, with the valid bit set.
#[must_use]
pub fn receive_address(mac: [u8; 6]) -> (u32, u32) {
    let low = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let high = u32::from(u16::from_le_bytes([mac[4], mac[5]])) | RAH_VALID;
    (low, high)
}

/// The byte length of a ring of `descriptors` entries, or `None` for a count the driver does not
/// accept: fewer than [`RING_MIN`], more than [`RING_MAX`], or not a multiple of 8.
#[must_use]
pub fn ring_bytes(descriptors: u32) -> Option<u32> {
    if !(RING_MIN..=RING_MAX).contains(&descriptors) || !descriptors.is_multiple_of(8) {
        return None;
    }
    Some(descriptors * DESCRIPTOR_BYTES as u32)
}

/// The receive control value that enables the receiver with 2048-byte buffers, optionally
/// accepting broadcast, and stripping the CRC.
#[must_use]
pub fn rctl_enable(broadcast: bool) -> u32 {
    RCTL_EN | RCTL_SECRC | if broadcast { RCTL_BAM } else { 0 }
}

/// The transmit control value that enables the transmitter, pads short frames, and uses the
/// recommended collision threshold and full-duplex distance.
#[must_use]
pub fn tctl_enable() -> u32 {
    TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD
}

/// A legacy receive descriptor for a buffer at `buffer`, with the device owning it: the address
/// in bytes 0 to 7, every other field zero.
#[must_use]
pub fn rx_descriptor(buffer: u64) -> [u8; DESCRIPTOR_BYTES] {
    let mut out = [0u8; DESCRIPTOR_BYTES];
    out[0..8].copy_from_slice(&buffer.to_le_bytes());
    out
}

/// Whether a receive descriptor's status says the device has filled it and it ends a packet.
#[must_use]
pub fn rx_complete(status: u8) -> bool {
    status & DESC_DD != 0 && status & RX_STATUS_EOP != 0
}

/// A legacy transmit descriptor sending `length` bytes from `buffer`. The command asks for the
/// CRC to be inserted and for a status report, and marks the end of the packet when `end` is set.
/// Returns `None` for an empty or oversized buffer.
#[must_use]
pub fn tx_descriptor(buffer: u64, length: u16, end: bool) -> Option<[u8; DESCRIPTOR_BYTES]> {
    if length == 0 || length > TX_LENGTH_MAX {
        return None;
    }
    let mut out = [0u8; DESCRIPTOR_BYTES];
    out[0..8].copy_from_slice(&buffer.to_le_bytes());
    out[8..10].copy_from_slice(&length.to_le_bytes());
    let cmd = TX_CMD_IFCS | TX_CMD_RS | if end { TX_CMD_EOP } else { 0 };
    out[11] = cmd;
    Some(out)
}

/// Whether a transmit descriptor's status says the device has sent it.
#[must_use]
pub fn tx_done(status: u8) -> bool {
    status & DESC_DD != 0
}

/// How many descriptors the driver may still fill in a transmit ring of `count` entries, with
/// the device's head and the driver's tail. One slot is kept empty so a full ring and an empty
/// one can be told apart.
#[must_use]
pub fn tx_free(head: u32, tail: u32, count: u32) -> u32 {
    let used = (tail + count - head) % count;
    count - 1 - used
}

#[cfg(test)]
mod tests;

/// Validate final readbacks after reset and PCI interrupt/DMA exclusion. This does not
/// stop hardware itself: the caller must detach software users and obtain these readbacks
/// before reclaiming any exposed descriptor, packet buffer or register mapping.
pub fn stop_proven(ctrl: u32, receive: u32, transmit: u32, mask: u32, command: u32) -> bool {
    ctrl & CTRL_RST == 0
        && receive & 2 == 0
        && transmit & 2 == 0
        && mask == 0
        && command & ((1 << 2) | (1 << 10)) == 1 << 10
}
#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn no_partial_quiescence_proof_authorizes_reclamation() {
        for reset in [false, true] {
            for rx in [false, true] {
                for tx in [false, true] {
                    for irq in [false, true] {
                        for bm in [false, true] {
                            for intx in [false, true] {
                                assert_eq!(
                                    stop_proven(
                                        if reset { CTRL_RST } else { 0 },
                                        if rx { 2 } else { 0 },
                                        if tx { 2 } else { 0 },
                                        u32::from(irq),
                                        (u32::from(bm) << 2) | (u32::from(intx) << 10)
                                    ),
                                    !reset && !rx && !tx && !irq && !bm && intx
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(!stop_proven(
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX
        ));
    }
    #[test]
    fn unrelated_control_bits_do_not_substitute_for_or_invalidate_stop_proof() {
        assert!(stop_proven(!CTRL_RST, !2, !2, 0, !4));
        for bit in 0..32 {
            assert!(!stop_proven(0, 0, 0, 1 << bit, 1 << 10));
        }
    }
}
