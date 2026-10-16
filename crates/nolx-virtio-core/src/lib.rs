//! Virtio 1.x over PCI, as far as it can be decided without touching a device.
//!
//! The virtio PCI transport describes its registers through vendor-specific capabilities in
//! the PCI capability list, each pointing into a BAR. Which capability is which, how the device
//! status must be stepped through, which features a driver may accept, how a split virtqueue is
//! laid out in memory, and what a block request looks like on the wire are all rules with no
//! hardware in them. They live here, host-tested, on top of [`nolx_pci_core`]. The kernel maps
//! the BARs, performs the register reads and writes, and owns the queue memory.
//!
//! Scope: virtio 1.x (`VIRTIO_F_VERSION_1`) with split virtqueues, and the block device's request
//! format. Legacy (transitional, I/O-port) transport, packed rings, and interrupts are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use nolx_pci_core::{capabilities, Bdf, ConfigSpace};

/// The PCI vendor ID every virtio device carries.
pub const VENDOR_VIRTIO: u16 = 0x1AF4;
/// The modern (virtio 1.0+) PCI device ID of a block device: `0x1040 + device type 2`.
pub const DEVICE_BLOCK_MODERN: u16 = 0x1042;
/// The transitional PCI device ID of a block device. QEMU reports this one by default, and
/// it carries the same modern capabilities, so the driver accepts it too.
pub const DEVICE_BLOCK_TRANSITIONAL: u16 = 0x1001;

/// Whether a PCI vendor and device ID name a virtio block device this driver can drive.
#[must_use]
pub const fn is_block_device(vendor: u16, device: u16) -> bool {
    vendor == VENDOR_VIRTIO
        && (device == DEVICE_BLOCK_MODERN || device == DEVICE_BLOCK_TRANSITIONAL)
}
/// The PCI capability ID of a vendor-specific capability; virtio uses it for every region.
const CAP_ID_VENDOR: u8 = 0x09;

/// `cfg_type` of the common configuration structure.
pub const CFG_COMMON: u8 = 1;
/// `cfg_type` of the notification structure.
pub const CFG_NOTIFY: u8 = 2;
/// `cfg_type` of the ISR status byte.
pub const CFG_ISR: u8 = 3;
/// `cfg_type` of the device-specific configuration.
pub const CFG_DEVICE: u8 = 4;

/// The feature bit that makes a device virtio 1.x (bit 32).
pub const FEATURE_VERSION_1: u64 = 1 << 32;

/// Device status register bits.
pub mod status {
    /// The guest has found the device.
    pub const ACKNOWLEDGE: u8 = 1;
    /// The guest knows how to drive the device.
    pub const DRIVER: u8 = 2;
    /// The driver is set up and the device may be used.
    pub const DRIVER_OK: u8 = 4;
    /// The driver's feature selection is accepted.
    pub const FEATURES_OK: u8 = 8;
    /// The device has failed and needs a reset.
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    /// The driver gave up on the device.
    pub const FAILED: u8 = 128;
}

/// Byte offsets of the fields of the common configuration structure.
pub mod common {
    /// `device_feature_select`, a 32-bit selector for the feature word read next.
    pub const DEVICE_FEATURE_SELECT: u8 = 0;
    /// `device_feature`, the selected 32-bit word of the device's feature bits.
    pub const DEVICE_FEATURE: u8 = 4;
    /// `driver_feature_select`.
    pub const DRIVER_FEATURE_SELECT: u8 = 8;
    /// `driver_feature`, the selected 32-bit word of the driver's accepted features.
    pub const DRIVER_FEATURE: u8 = 12;
    /// `device_status`, one byte.
    pub const DEVICE_STATUS: u8 = 20;
    /// `num_queues`, a 16-bit count.
    pub const NUM_QUEUES: u8 = 18;
    /// `queue_select`, a 16-bit index chosen before the per-queue fields are accessed.
    pub const QUEUE_SELECT: u8 = 22;
    /// `queue_size` of the selected queue, 16 bits.
    pub const QUEUE_SIZE: u8 = 24;
    /// `queue_msix_vector` of the selected queue, 16 bits: the MSI-X table entry the queue's
    /// interrupts use, or `0xFFFF` for none.
    pub const QUEUE_MSIX_VECTOR: u8 = 26;
    /// `queue_enable` of the selected queue, 16 bits.
    pub const QUEUE_ENABLE: u8 = 28;
    /// `queue_notify_off` of the selected queue, 16 bits.
    pub const QUEUE_NOTIFY_OFF: u8 = 30;
    /// `queue_desc`, the 64-bit physical address of the descriptor table.
    pub const QUEUE_DESC: u8 = 32;
    /// `queue_driver`, the 64-bit physical address of the available ring.
    pub const QUEUE_DRIVER: u8 = 40;
    /// `queue_device`, the 64-bit physical address of the used ring.
    pub const QUEUE_DEVICE: u8 = 48;
}

/// The status values a driver writes, in order, during initialization.
///
/// Each value is a superset of the one before it, and the last one sets `DRIVER_OK`. The device
/// must be reset (status 0) before the first of these.
pub const INIT_STATUS_SEQUENCE: [u8; 4] = [
    status::ACKNOWLEDGE,
    status::ACKNOWLEDGE | status::DRIVER,
    status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
    status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
];

/// Failure in the virtio rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioError {
    /// The device does not offer `VIRTIO_F_VERSION_1`, so it is not a modern device.
    NoVersion1,
    /// The device cleared `FEATURES_OK` after the driver set it: the feature set was rejected.
    FeaturesRejected,
    /// The device set `DEVICE_NEEDS_RESET` or `FAILED`.
    DeviceFailed,
    /// A queue size is zero, not a power of two, or above [`MAX_QUEUE_SIZE`].
    InvalidQueueSize,
    /// A vendor capability is malformed: too short, pointing past the configuration space, or
    /// naming a BAR that does not exist.
    MalformedCapability,
    /// The device completed a block request with a status other than `BLK_S_OK`.
    RequestFailed(u8),
}

/// One virtio vendor capability: where a register region lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioCap {
    /// Which structure this region holds (one of the `CFG_*` constants).
    pub cfg_type: u8,
    /// The BAR the region lives in, 0 to 5.
    pub bar: u8,
    /// Byte offset of the region within the BAR.
    pub offset: u32,
    /// Length of the region in bytes.
    pub length: u32,
    /// For [`CFG_NOTIFY`] only: the multiplier turning a queue's notify offset into bytes.
    pub notify_multiplier: u32,
}

/// The virtio regions a device exposes, one per structure type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtioCaps {
    /// The common configuration structure.
    pub common: Option<VirtioCap>,
    /// The notification structure.
    pub notify: Option<VirtioCap>,
    /// The ISR status byte.
    pub isr: Option<VirtioCap>,
    /// The device-specific configuration structure.
    pub device: Option<VirtioCap>,
}

/// Walk the PCI capability list of `bdf` and collect its virtio regions.
///
/// Vendor capabilities of other types are ignored. The first valid capability of each type wins;
/// a later duplicate is ignored. A malformed virtio capability is skipped rather than trusted, so
/// one bad entry cannot hide the rest of the list.
pub fn discover<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> VirtioCaps {
    let list = capabilities(cfg, bdf);
    let mut caps = VirtioCaps::default();
    for entry in list.as_slice() {
        if entry.id != CAP_ID_VENDOR {
            continue;
        }
        if let Ok(cap) = read_cap(cfg, bdf, entry.offset) {
            record(&mut caps, cap);
        }
    }
    caps
}

/// Store `cap` in the slot for its structure type, unless that slot is already filled.
fn record(caps: &mut VirtioCaps, cap: VirtioCap) {
    let slot = match cap.cfg_type {
        CFG_COMMON => &mut caps.common,
        CFG_NOTIFY => &mut caps.notify,
        CFG_ISR => &mut caps.isr,
        CFG_DEVICE => &mut caps.device,
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(cap);
    }
}

/// Decode one vendor capability at `offset`, validating its length and BAR number.
fn read_cap<C: ConfigSpace>(cfg: &mut C, bdf: Bdf, offset: u8) -> Result<VirtioCap, VirtioError> {
    let header = cfg.read_u32(bdf, offset);
    let cap_len = (header >> 16) as u8;
    let cfg_type = (header >> 24) as u8;
    // Layout: cap_vndr, cap_next, cap_len, cfg_type, bar, id, padding, offset, length, and for
    // the notification structure a 32-bit notify_off_multiplier.
    if cap_len < 16 || u16::from(offset) + u16::from(cap_len) > 256 {
        return Err(VirtioError::MalformedCapability);
    }
    let bar = cfg.read_u32(bdf, offset + 4) as u8;
    if bar > 5 {
        return Err(VirtioError::MalformedCapability);
    }
    let region_offset = cfg.read_u32(bdf, offset + 8);
    let length = cfg.read_u32(bdf, offset + 12);
    let notify_multiplier = if cfg_type == CFG_NOTIFY {
        if cap_len < 20 {
            return Err(VirtioError::MalformedCapability);
        }
        cfg.read_u32(bdf, offset + 16)
    } else {
        0
    };
    Ok(VirtioCap {
        cfg_type,
        bar,
        offset: region_offset,
        length,
        notify_multiplier,
    })
}

/// The address of a queue's notification register, in bytes from the start of the notify
/// structure's BAR region. The device gives each queue its own offset in units of the multiplier.
#[must_use]
pub fn notify_address(notify: &VirtioCap, queue_notify_off: u16) -> u64 {
    u64::from(notify.offset) + u64::from(queue_notify_off) * u64::from(notify.notify_multiplier)
}

/// Choose the features this driver accepts from what the device offers.
///
/// The driver requires `VIRTIO_F_VERSION_1` and takes no optional feature, so its accepted set is
/// exactly that bit. A device without it is refused, because the rest of this layout assumes the
/// modern transport.
pub fn negotiate(device_features: u64) -> Result<u64, VirtioError> {
    if device_features & FEATURE_VERSION_1 == 0 {
        return Err(VirtioError::NoVersion1);
    }
    Ok(FEATURE_VERSION_1)
}

/// Check the status the device reads back after the driver wrote `FEATURES_OK`.
///
/// The device clears `FEATURES_OK` to refuse the feature set. A device that reports itself failed
/// or needing reset is reported as such, because nothing after this point is valid.
pub fn check_features_ok(readback: u8) -> Result<(), VirtioError> {
    if readback & (status::DEVICE_NEEDS_RESET | status::FAILED) != 0 {
        return Err(VirtioError::DeviceFailed);
    }
    if readback & status::FEATURES_OK == 0 {
        return Err(VirtioError::FeaturesRejected);
    }
    Ok(())
}

/// The largest queue size a split virtqueue may have.
pub const MAX_QUEUE_SIZE: u16 = 32768;

/// Byte layout of a split virtqueue of `size` entries, relative to the start of its memory.
///
/// The descriptor table comes first, at offset 0. The available ring follows it, and the used
/// ring starts at the next 4-byte boundary after the available ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitLayout {
    /// Number of entries in the queue.
    pub size: u16,
    /// Offset of the available ring.
    pub avail_offset: u64,
    /// Offset of the used ring.
    pub used_offset: u64,
    /// Total bytes the queue occupies.
    pub total_bytes: u64,
}

/// Compute the split virtqueue layout for `size` entries.
///
/// The size must be a power of two no larger than [`MAX_QUEUE_SIZE`]. Sizes follow the
/// specification's structure sizes: 16 bytes per descriptor, `6 + 2n` for the available ring, and
/// `6 + 8n` for the used ring.
pub fn split_layout(size: u16) -> Result<SplitLayout, VirtioError> {
    if size == 0 || size > MAX_QUEUE_SIZE || !size.is_power_of_two() {
        return Err(VirtioError::InvalidQueueSize);
    }
    let n = u64::from(size);
    let descriptors = 16 * n;
    let avail_len = 6 + 2 * n;
    let used_offset = align_up(descriptors + avail_len, 4);
    let used_len = 6 + 8 * n;
    Ok(SplitLayout {
        size,
        avail_offset: descriptors,
        used_offset,
        total_bytes: used_offset + used_len,
    })
}

const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

/// The flag a descriptor sets when `next` links to another descriptor.
pub const DESC_NEXT: u16 = 1;
/// The flag a descriptor sets when the device writes into its buffer.
pub const DESC_WRITE: u16 = 2;

/// One entry of the descriptor table, as the device reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    /// Physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Combination of [`DESC_NEXT`] and [`DESC_WRITE`].
    pub flags: u16,
    /// Index of the next descriptor when [`DESC_NEXT`] is set.
    pub next: u16,
}

impl Descriptor {
    /// The 16-byte little-endian encoding the device reads from the descriptor table.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..12].copy_from_slice(&self.len.to_le_bytes());
        out[12..14].copy_from_slice(&self.flags.to_le_bytes());
        out[14..16].copy_from_slice(&self.next.to_le_bytes());
        out
    }
}

/// Block request type: read sectors from the device.
pub const BLK_T_IN: u32 = 0;
/// Block request type: write sectors to the device.
pub const BLK_T_OUT: u32 = 1;
/// Block request type: flush the device's write cache.
pub const BLK_T_FLUSH: u32 = 4;
/// Block request status: the request completed.
pub const BLK_S_OK: u8 = 0;
/// Block request status: the device failed the request.
pub const BLK_S_IOERR: u8 = 1;
/// Block request status: the device does not support the request.
pub const BLK_S_UNSUPP: u8 = 2;
/// The unit a block request addresses sectors in. It is fixed at 512 bytes whatever the device's
/// physical sector size.
pub const SECTOR_BYTES: u64 = 512;

/// The descriptor chain for one read of `len` bytes: the request header (device-readable), the
/// data buffer (device-writable), and the status byte (device-writable). The buffers are given
/// by physical address. The chain is three descriptors, head at index 0 and the next two
/// linked after it, so the caller places them at consecutive table slots.
#[must_use]
pub fn block_read_chain(header: u64, data: u64, status: u64, len: u32) -> [Descriptor; 3] {
    [
        Descriptor {
            addr: header,
            len: 16,
            flags: DESC_NEXT,
            next: 1,
        },
        Descriptor {
            addr: data,
            len,
            flags: DESC_NEXT | DESC_WRITE,
            next: 2,
        },
        Descriptor {
            addr: status,
            len: 1,
            flags: DESC_WRITE,
            next: 0,
        },
    ]
}

/// The descriptor chain for one write of `len` bytes: the header and the data are both
/// device-readable, and the status byte is device-writable. Same layout as
/// [`block_read_chain`].
#[must_use]
pub fn block_write_chain(header: u64, data: u64, status: u64, len: u32) -> [Descriptor; 3] {
    let mut chain = block_read_chain(header, data, status, len);
    chain[1].flags = DESC_NEXT;
    chain
}

/// CRC-32 (IEEE 802.3, the zlib and Ethernet polynomial) of `bytes`. The probe uses it to
/// check what it wrote against what it read back, and CI checks the same value against the disk
/// image. It is a checksum for that comparison, not a virtio rule.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Interpret the status byte the device wrote for a block request.
pub const fn block_status(byte: u8) -> Result<(), VirtioError> {
    if byte == BLK_S_OK {
        Ok(())
    } else {
        Err(VirtioError::RequestFailed(byte))
    }
}

/// Whether the device has completed a request since the driver last looked: the used ring's
/// running index has moved. Indices wrap at 2^16, so the comparison is exact equality.
#[must_use]
pub const fn has_completion(used_index: u16, last_seen: u16) -> bool {
    used_index != last_seen
}

/// The device's capacity in sectors, from the 64-bit little-endian field at the start of the
/// block device configuration, given as its two halves.
#[must_use]
pub const fn capacity_sectors(low: u32, high: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// The 16-byte header that begins every block request: type, reserved, then the sector.
#[must_use]
pub fn block_request_header(kind: u32, sector: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&kind.to_le_bytes());
    out[8..16].copy_from_slice(&sector.to_le_bytes());
    out
}

/// The capacity of a block device in bytes, from the sector count its configuration reports.
/// `None` if the product overflows, which a conforming device cannot cause.
#[must_use]
pub fn capacity_bytes(capacity_sectors: u64) -> Option<u64> {
    capacity_sectors.checked_mul(SECTOR_BYTES)
}

#[cfg(test)]
mod tests;
