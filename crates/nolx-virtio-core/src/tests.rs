//! Host tests. A modelled configuration space carries virtio capabilities, and the rules are
//! checked against the byte layouts the specification gives.

use super::*;
use nolx_pci_core::{Bdf, ConfigSpace};

/// 64 dwords of configuration space, read directly.
struct Cfg {
    regs: [u32; 64],
}

impl ConfigSpace for Cfg {
    fn read_u32(&mut self, _bdf: Bdf, offset: u8) -> u32 {
        self.regs[usize::from(offset) / 4]
    }

    fn write_u32(&mut self, _bdf: Bdf, _offset: u8, _value: u32) {}
}

/// One vendor capability to place in the model.
struct Cap {
    at: u8,
    next: u8,
    cap_len: u8,
    cfg_type: u8,
    bar: u8,
    offset: u32,
    length: u32,
    multiplier: u32,
}

/// A space with the capability list enabled and pointing at `first`.
fn space(first: u8) -> [u32; 64] {
    let mut regs = [0u32; 64];
    regs[1] = 1 << 20;
    regs[13] = u32::from(first);
    regs
}

/// Write `cap` into the model. The multiplier is only written for a capability long enough to
/// hold it, so a short capability never overwrites the header of the next one.
fn put(regs: &mut [u32; 64], cap: &Cap) {
    let i = usize::from(cap.at) / 4;
    let header = 0x09 | (u32::from(cap.next) << 8);
    regs[i] = header | (u32::from(cap.cap_len) << 16) | (u32::from(cap.cfg_type) << 24);
    regs[i + 1] = u32::from(cap.bar);
    regs[i + 2] = cap.offset;
    regs[i + 3] = cap.length;
    if cap.cap_len >= 20 {
        regs[i + 4] = cap.multiplier;
    }
}

/// A common-style capability with the default region, placed at `at`.
fn cap(at: u8, next: u8, cap_len: u8, cfg_type: u8, bar: u8) -> Cap {
    Cap {
        at,
        next,
        cap_len,
        cfg_type,
        bar,
        offset: 0x1000,
        length: 0x38,
        multiplier: 0,
    }
}

fn bdf() -> Bdf {
    Bdf::new(0, 3, 0).expect("valid")
}

/// A device with the four virtio regions and a vendor capability of a type virtio does not
/// define, placed between them. Each capability starts where the previous one ends.
fn device() -> Cfg {
    let mut regs = space(0x40);
    put(
        &mut regs,
        &Cap {
            at: 0x40,
            next: 0x50,
            cap_len: 16,
            cfg_type: CFG_COMMON,
            bar: 4,
            offset: 0x1000,
            length: 0x38,
            multiplier: 0,
        },
    );
    put(
        &mut regs,
        &Cap {
            at: 0x50,
            next: 0x64,
            cap_len: 20,
            cfg_type: CFG_NOTIFY,
            bar: 4,
            offset: 0x2000,
            length: 0x20,
            multiplier: 4,
        },
    );
    put(
        &mut regs,
        &Cap {
            at: 0x64,
            next: 0x74,
            cap_len: 16,
            cfg_type: CFG_ISR,
            bar: 4,
            offset: 0x3000,
            length: 1,
            multiplier: 0,
        },
    );
    put(
        &mut regs,
        &Cap {
            at: 0x74,
            next: 0x84,
            cap_len: 16,
            cfg_type: 0x80,
            bar: 4,
            offset: 0x7000,
            length: 0x10,
            multiplier: 0,
        },
    );
    put(
        &mut regs,
        &Cap {
            at: 0x84,
            next: 0,
            cap_len: 16,
            cfg_type: CFG_DEVICE,
            bar: 4,
            offset: 0x4000,
            length: 0x40,
            multiplier: 0,
        },
    );
    Cfg { regs }
}

#[test]
fn discovery_finds_each_region_by_type() {
    let caps = discover(&mut device(), bdf());
    let common = VirtioCap {
        cfg_type: CFG_COMMON,
        bar: 4,
        offset: 0x1000,
        length: 0x38,
        notify_multiplier: 0,
    };
    assert_eq!(caps.common, Some(common));
    let notify = caps.notify.expect("notify region");
    assert_eq!(notify.offset, 0x2000);
    assert_eq!(notify.notify_multiplier, 4);
    assert_eq!(caps.isr.map(|cap| cap.length), Some(1));
    assert_eq!(caps.device.map(|cap| cap.offset), Some(0x4000));
}

#[test]
fn a_device_without_a_capability_list_has_no_regions() {
    let mut cfg = Cfg { regs: [0; 64] };
    assert_eq!(discover(&mut cfg, bdf()), VirtioCaps::default());
}

#[test]
fn the_first_valid_capability_of_a_type_wins() {
    let mut regs = space(0x40);
    put(&mut regs, &cap(0x40, 0x50, 16, CFG_COMMON, 0));
    put(&mut regs, &cap(0x50, 0, 16, CFG_COMMON, 1));
    let caps = discover(&mut Cfg { regs }, bdf());
    assert_eq!(caps.common.map(|cap| cap.bar), Some(0));
}

#[test]
fn malformed_capabilities_are_skipped_not_trusted() {
    // A common region too short to hold its fields, then a BAR number that does not exist,
    // then a valid notify region that must still be found.
    let mut regs = space(0x40);
    put(&mut regs, &cap(0x40, 0x50, 8, CFG_COMMON, 0));
    put(&mut regs, &cap(0x50, 0x60, 16, CFG_ISR, 7));
    put(&mut regs, &cap(0x60, 0, 20, CFG_NOTIFY, 0));
    let caps = discover(&mut Cfg { regs }, bdf());
    assert_eq!(caps.common, None);
    assert_eq!(caps.isr, None);
    assert!(caps.notify.is_some());
}

#[test]
fn a_notify_region_needs_its_multiplier_field() {
    let mut regs = space(0x40);
    put(&mut regs, &cap(0x40, 0, 16, CFG_NOTIFY, 0));
    assert_eq!(discover(&mut Cfg { regs }, bdf()).notify, None);
}

#[test]
fn a_capability_past_the_end_of_configuration_space_is_refused() {
    let mut regs = space(0x40);
    put(&mut regs, &cap(0x40, 0, 0xF8, CFG_COMMON, 0));
    assert_eq!(discover(&mut Cfg { regs }, bdf()).common, None);
}

#[test]
fn notify_address_scales_the_queue_offset() {
    let notify = VirtioCap {
        cfg_type: CFG_NOTIFY,
        bar: 4,
        offset: 0x2000,
        length: 0x20,
        notify_multiplier: 4,
    };
    assert_eq!(notify_address(&notify, 0), 0x2000);
    assert_eq!(notify_address(&notify, 3), 0x200C);
}

#[test]
fn negotiation_requires_version_1_and_accepts_only_it() {
    assert_eq!(negotiate(0), Err(VirtioError::NoVersion1));
    assert_eq!(negotiate(0xFFFF_FFFF), Err(VirtioError::NoVersion1));
    assert_eq!(negotiate(FEATURE_VERSION_1 | 0x20), Ok(FEATURE_VERSION_1));
}

#[test]
fn features_ok_readback_is_checked() {
    let ok = status::FEATURES_OK | status::DRIVER;
    assert_eq!(check_features_ok(ok), Ok(()));
    assert_eq!(
        check_features_ok(status::DRIVER),
        Err(VirtioError::FeaturesRejected)
    );
    let failed = status::FEATURES_OK | status::FAILED;
    assert_eq!(check_features_ok(failed), Err(VirtioError::DeviceFailed));
    assert_eq!(
        check_features_ok(status::DEVICE_NEEDS_RESET),
        Err(VirtioError::DeviceFailed)
    );
}

#[test]
fn the_init_sequence_only_adds_bits_and_ends_with_driver_ok() {
    let mut previous = 0u8;
    for step in INIT_STATUS_SEQUENCE {
        assert_eq!(
            step & previous,
            previous,
            "each step keeps the bits before it"
        );
        assert_ne!(step, previous, "each step adds a bit");
        previous = step;
    }
    assert_eq!(previous & status::DRIVER_OK, status::DRIVER_OK);
    assert_eq!(INIT_STATUS_SEQUENCE[1] & status::FEATURES_OK, 0);
    assert_eq!(
        INIT_STATUS_SEQUENCE[2] & status::FEATURES_OK,
        status::FEATURES_OK
    );
}

#[test]
fn split_layout_matches_the_specification_sizes() {
    // Queue of 8: 8 * 16 bytes of descriptors, then an available ring of 6 + 2 * 8 = 22 bytes.
    // The used ring starts at the next 4-byte boundary, 150 rounded up to 152, and is
    // 6 + 8 * 8 = 70 bytes long.
    let layout = split_layout(8).expect("valid");
    assert_eq!(layout.avail_offset, 128);
    assert_eq!(layout.used_offset, 152);
    assert_eq!(layout.total_bytes, 222);
    assert_eq!(layout.size, 8);
}

#[test]
fn split_layout_refuses_sizes_the_device_cannot_use() {
    assert_eq!(split_layout(0), Err(VirtioError::InvalidQueueSize));
    assert_eq!(split_layout(3), Err(VirtioError::InvalidQueueSize));
    assert_eq!(
        split_layout(MAX_QUEUE_SIZE + 1),
        Err(VirtioError::InvalidQueueSize)
    );
    assert!(split_layout(MAX_QUEUE_SIZE).is_ok());
    assert!(split_layout(1).is_ok());
}

#[test]
fn a_descriptor_encodes_little_endian_in_its_specified_fields() {
    let desc = Descriptor {
        addr: 0x0102_0304_0506_0708,
        len: 0x0A0B_0C0D,
        flags: DESC_NEXT | DESC_WRITE,
        next: 0x0E0F,
    };
    let bytes = desc.to_bytes();
    assert_eq!(bytes[0..8], 0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(bytes[8..12], [0x0D, 0x0C, 0x0B, 0x0A]);
    assert_eq!(bytes[12..14], [3, 0]);
    assert_eq!(bytes[14..16], [0x0F, 0x0E]);
}

#[test]
fn a_block_request_header_names_the_type_and_sector() {
    let header = block_request_header(BLK_T_IN, 0x0123_4567_89AB);
    assert_eq!(header[0..4], [0, 0, 0, 0]);
    assert_eq!(header[4..8], [0, 0, 0, 0], "the reserved word is zero");
    assert_eq!(header[8..16], 0x0123_4567_89ABu64.to_le_bytes());
    assert_eq!(block_request_header(BLK_T_FLUSH, 0)[0], 4);
    assert_eq!(block_request_header(BLK_T_OUT, 0)[0], 1);
}

#[test]
fn capacity_is_sectors_times_512_and_overflow_is_reported() {
    assert_eq!(capacity_bytes(2048), Some(1 << 20));
    assert_eq!(capacity_bytes(u64::MAX), None);
}

#[test]
fn only_the_block_device_ids_are_accepted() {
    assert!(is_block_device(VENDOR_VIRTIO, DEVICE_BLOCK_TRANSITIONAL));
    assert!(is_block_device(VENDOR_VIRTIO, DEVICE_BLOCK_MODERN));
    assert!(!is_block_device(VENDOR_VIRTIO, 0x1041), "the network device is not a block device");
    assert!(!is_block_device(0x8086, DEVICE_BLOCK_MODERN), "the vendor must be virtio");
}

#[test]
fn a_read_chain_has_the_request_data_and_status_in_order() {
    let chain = block_read_chain(0x1000, 0x2000, 0x3000, 512);
    assert_eq!(chain[0].addr, 0x1000);
    assert_eq!(chain[0].len, 16);
    assert_eq!(chain[0].flags, DESC_NEXT, "the header is device-readable");
    assert_eq!(chain[0].next, 1);
    assert_eq!(chain[1].addr, 0x2000);
    assert_eq!(chain[1].len, 512);
    assert_eq!(chain[1].flags, DESC_NEXT | DESC_WRITE, "the data is device-writable");
    assert_eq!(chain[1].next, 2);
    assert_eq!(chain[2].addr, 0x3000);
    assert_eq!(chain[2].len, 1);
    assert_eq!(chain[2].flags, DESC_WRITE, "the status byte is device-writable and ends the chain");
}

#[test]
fn a_block_status_is_ok_only_for_ok() {
    assert_eq!(block_status(BLK_S_OK), Ok(()));
    assert_eq!(block_status(BLK_S_IOERR), Err(VirtioError::RequestFailed(BLK_S_IOERR)));
    assert_eq!(block_status(BLK_S_UNSUPP), Err(VirtioError::RequestFailed(BLK_S_UNSUPP)));
}

#[test]
fn a_completion_is_seen_once_and_survives_wraparound() {
    assert!(!has_completion(0, 0));
    assert!(has_completion(1, 0));
    assert!(has_completion(0, u16::MAX), "the index wrapped past 2^16");
}

#[test]
fn the_capacity_joins_its_halves_little_endian() {
    assert_eq!(capacity_sectors(2048, 0), 2048);
    assert_eq!(capacity_sectors(0, 1), 1 << 32);
    assert_eq!(capacity_sectors(u32::MAX, u32::MAX), u64::MAX);
}
