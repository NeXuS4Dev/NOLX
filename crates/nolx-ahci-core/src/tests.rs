//! Host tests. Each rule is checked against the byte layout the AHCI specification gives.

use super::*;

#[test]
fn a_port_register_sits_in_its_own_block() {
    assert_eq!(port_register(0, PORT_CLB), Some(0x100));
    assert_eq!(port_register(1, PORT_CI), Some(0x1B8));
    assert_eq!(port_register(31, PORT_CI), Some(0x100 + 31 * 0x80 + 0x38));
    assert_eq!(port_register(32, PORT_CLB), None);
    assert_eq!(HBA_REGION_BYTES, 0x1100);
}

#[test]
fn the_capability_field_gives_one_more_than_its_zero_based_value() {
    assert_eq!(command_slots(0x0000_0000), 1);
    assert_eq!(command_slots(0x0000_1F00), 32);
    assert_eq!(command_slots(0x0000_0700), 8);
}

#[test]
fn only_the_ports_the_bitmap_names_are_implemented() {
    assert!(port_implemented(0b101, 0));
    assert!(!port_implemented(0b101, 1));
    assert!(port_implemented(0b101, 2));
    assert!(!port_implemented(u32::MAX, 32));
}

#[test]
fn a_sata_disk_needs_a_present_link_and_a_sata_signature() {
    assert!(is_sata_disk(0x0000_0123, SIG_SATA));
    assert!(!is_sata_disk(0x0000_0121, SIG_SATA), "link not established");
    assert!(!is_sata_disk(0x0000_0123, 0xEB14_0101), "an ATAPI device");
}

#[test]
fn the_task_file_reports_errors_and_busy_separately() {
    assert!(tfd_error(0x41));
    assert!(!tfd_error(0x80));
    assert!(tfd_busy(0x80));
    assert!(tfd_busy(0x08));
    assert!(!tfd_busy(0x40));
}

#[test]
fn read_dma_ext_fis_carries_the_48_bit_address_and_count() {
    let fis = h2d_read_dma_ext(0x0000_0102_0304_0506, 8);
    let expected: [u8; FIS_LEN] = [
        0x27, 0x80, 0x25, 0x00, 0x06, 0x05, 0x04, 0x40, 0x03, 0x02, 0x01, 0x00, 0x08, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert_eq!(fis, expected);
}

#[test]
fn a_command_header_names_its_fis_length_direction_and_table() {
    let read = CommandHeader {
        fis_dwords: 5,
        write: false,
        prdt_entries: 1,
        table: 0x1_0000_0800,
    };
    let bytes = encode_command_header(&read).unwrap();
    assert_eq!(&bytes[0..4], &[5, 0, 1, 0], "CFL 5, no write bit, one PRDT entry");
    assert_eq!(&bytes[8..12], &0x0000_0800u32.to_le_bytes());
    assert_eq!(&bytes[12..16], &1u32.to_le_bytes());

    let write = CommandHeader { write: true, ..read };
    assert_eq!(encode_command_header(&write).unwrap()[0], 0x45, "bit 6 is the write bit");
}

#[test]
fn a_command_header_is_refused_for_a_misaligned_table_or_a_bad_fis_length() {
    let mut header = CommandHeader {
        fis_dwords: 5,
        write: false,
        prdt_entries: 1,
        table: 0x801,
    };
    assert_eq!(encode_command_header(&header), None, "table not 128-byte aligned");
    header.table = 0x800;
    header.fis_dwords = 1;
    assert_eq!(encode_command_header(&header), None, "FIS shorter than two dwords");
    header.fis_dwords = 17;
    assert_eq!(encode_command_header(&header), None, "FIS longer than the field allows");
}

#[test]
fn a_region_descriptor_stores_its_count_one_less_than_the_bytes() {
    let entry = prdt_entry(0x1234_5678_9ABC_DEF0, 512).unwrap();
    assert_eq!(&entry[0..4], &0x9ABC_DEF0u32.to_le_bytes());
    assert_eq!(&entry[4..8], &0x1234_5678u32.to_le_bytes());
    assert_eq!(&entry[12..16], &511u32.to_le_bytes());
}

#[test]
fn a_region_descriptor_refuses_empty_odd_misaligned_and_oversized_regions() {
    assert_eq!(prdt_entry(0x1000, 0), None);
    assert_eq!(prdt_entry(0x1000, 3), None);
    assert_eq!(prdt_entry(0x1001, 512), None);
    assert_eq!(prdt_entry(0x1000, PRDT_MAX_BYTES + 2), None);
    assert!(prdt_entry(0x1000, PRDT_MAX_BYTES).is_some(), "4 MiB exactly is allowed");
}
