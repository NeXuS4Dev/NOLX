//! Network device rules, decided without touching a device.
//!
//! Two layers. The virtio-net layer: the device identity, the feature bits the driver negotiates,
//! the configuration space (MAC, link status, MTU), the queue layout, and the 12-byte header each
//! frame carries through a virtqueue. The Ethernet layer: the frame size limits a driver must
//! respect, the header fields it parses, and the MAC address rules. The kernel moves the bytes
//! through the virtqueues; these rules say what the bytes mean and whether they may be sent.
//!
//! Scope: virtio-net as a modern (1.x) device on one queue pair, without multiqueue, checksum
//! offload, or segmentation offload. e1000 and the `nolx-df` framework are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod arp;
pub mod queue;

/// The virtio device ID of a network device.
pub const VIRTIO_NET_DEVICE_ID: u16 = 1;
/// The PCI device ID of a modern virtio device is 0x1040 plus its virtio device ID.
pub const MODERN_PCI_DEVICE_BASE: u16 = 0x1040;

/// Feature bit 0: the device handles packets with partial checksums.
pub const FEATURE_CSUM: u64 = 1 << 0;
/// Feature bit 5: the device has a MAC address in its configuration space.
pub const FEATURE_MAC: u64 = 1 << 5;
/// Feature bit 3: the device has an MTU in its configuration space.
pub const FEATURE_MTU: u64 = 1 << 3;
/// Feature bit 15: the device may merge receive buffers, so a frame can span several.
pub const FEATURE_MRG_RXBUF: u64 = 1 << 15;
/// Feature bit 16: the device has a link status in its configuration space.
pub const FEATURE_STATUS: u64 = 1 << 16;

/// Configuration space offsets: the MAC address (6 bytes), link status (2), the number of queue
/// pairs (2), and the MTU (2).
pub const CONFIG_MAC: usize = 0;
/// The link status word's offset.
pub const CONFIG_STATUS: usize = 6;
/// The maximum number of queue pairs the device supports.
pub const CONFIG_MAX_QUEUE_PAIRS: usize = 8;
/// The MTU word's offset.
pub const CONFIG_MTU: usize = 10;
/// Link status bit 0: the link is up.
pub const LINK_UP: u16 = 1 << 0;

/// The receive queue of the first (and, here, only) queue pair.
pub const QUEUE_RX: u16 = 0;
/// The transmit queue of the first (and, here, only) queue pair.
pub const QUEUE_TX: u16 = 1;

/// Bytes in the virtio-net header that precedes every frame in a virtqueue.
pub const NET_HEADER_BYTES: usize = 12;

/// The default maximum transmission unit: the payload of a standard Ethernet frame.
pub const MTU_DEFAULT: usize = 1500;
/// Bytes of an Ethernet header: destination, source, and ethertype.
pub const ETHERNET_HEADER_BYTES: usize = 14;
/// The shortest frame that may go on the wire without its frame check sequence.
pub const FRAME_MIN: usize = 60;
/// The largest frame at the default MTU, without its frame check sequence.
pub const FRAME_MAX_DEFAULT: usize = ETHERNET_HEADER_BYTES + MTU_DEFAULT;

/// Ethertype: IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// Ethertype: ARP.
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// Ethertype: IPv6.
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

/// The virtio-net header that precedes each frame. It is 12 bytes in a modern device: the
/// offload fields and the number of buffers a merged receive used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetHeader {
    /// Flags: bit 0 is NEEDS_CSUM, bit 1 is DATA_VALID.
    pub flags: u8,
    /// The segmentation offload type, 0 for none.
    pub gso_type: u8,
    /// The length of the headers the offload needs.
    pub hdr_len: u16,
    /// The segment size for segmentation offload.
    pub gso_size: u16,
    /// Where the checksum starts, when NEEDS_CSUM is set.
    pub csum_start: u16,
    /// Where the checksum is stored, relative to the start.
    pub csum_offset: u16,
    /// How many receive buffers the frame used when buffers are merged.
    pub num_buffers: u16,
}

impl NetHeader {
    /// The header of a frame that asks for no offload: every field zero, one buffer.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            num_buffers: 1,
            ..Self::default()
        }
    }

    /// The 12 bytes the device reads, in little-endian order.
    #[must_use]
    pub fn encode(&self) -> [u8; NET_HEADER_BYTES] {
        let mut out = [0u8; NET_HEADER_BYTES];
        out[0] = self.flags;
        out[1] = self.gso_type;
        out[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        out[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        out[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        out[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        out[10..12].copy_from_slice(&self.num_buffers.to_le_bytes());
        out
    }

    /// Read a header from the first 12 bytes of a received buffer. Returns `None` when the
    /// buffer is shorter than the header.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let head = bytes.get(..NET_HEADER_BYTES)?;
        let word = |at: usize| u16::from_le_bytes([head[at], head[at + 1]]);
        Some(Self {
            flags: head[0],
            gso_type: head[1],
            hdr_len: word(2),
            gso_size: word(4),
            csum_start: word(6),
            csum_offset: word(8),
            num_buffers: word(10),
        })
    }
}

/// A MAC address.
pub type Mac = [u8; 6];

/// Whether a MAC address is unicast: its first byte's low bit (the group bit) is clear.
#[must_use]
pub fn is_unicast(mac: &Mac) -> bool {
    mac[0] & 0x01 == 0
}

/// Whether a MAC address may be a source address: unicast, and not all zero.
#[must_use]
pub fn is_valid_source(mac: &Mac) -> bool {
    is_unicast(mac) && *mac != [0u8; 6]
}

/// The broadcast address.
pub const BROADCAST: Mac = [0xFF; 6];

/// A MAC address as text, `xx:xx:xx:xx:xx:xx` in lower-case hex, as the kernel prints it.
#[must_use]
pub fn mac_text(mac: &Mac) -> [u8; 17] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [b':'; 17];
    for (index, byte) in mac.iter().enumerate() {
        out[index * 3] = DIGITS[usize::from(byte >> 4)];
        out[index * 3 + 1] = DIGITS[usize::from(byte & 0x0F)];
    }
    out
}

/// Why a frame may not be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame is larger than the MTU allows.
    TooLarge,
    /// The frame is shorter than an Ethernet header, so it has no destination.
    TooShort,
}

/// The length to transmit for a frame of `length` bytes at the given MTU: frames shorter than
/// the minimum are padded to it, and a frame longer than the MTU plus the Ethernet header is
/// refused.
///
/// # Errors
///
/// [`FrameError::TooShort`] for a frame without a complete Ethernet header;
/// [`FrameError::TooLarge`] for a frame over the MTU.
pub fn transmit_length(length: usize, mtu: usize) -> Result<usize, FrameError> {
    if length < ETHERNET_HEADER_BYTES {
        return Err(FrameError::TooShort);
    }
    if length > ETHERNET_HEADER_BYTES + mtu {
        return Err(FrameError::TooLarge);
    }
    Ok(length.max(FRAME_MIN))
}

/// The destination, source, and ethertype of an Ethernet frame. Returns `None` for a frame
/// shorter than the header.
#[must_use]
pub fn parse_ethernet(frame: &[u8]) -> Option<(Mac, Mac, u16)> {
    let head = frame.get(..ETHERNET_HEADER_BYTES)?;
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&head[0..6]);
    src.copy_from_slice(&head[6..12]);
    Some((dst, src, u16::from_be_bytes([head[12], head[13]])))
}

#[cfg(test)]
mod tests;
