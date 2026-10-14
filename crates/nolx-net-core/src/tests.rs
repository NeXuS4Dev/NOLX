//! Host tests. Each rule is checked against the layout the virtio and Ethernet specifications give.

use super::*;

#[test]
fn the_device_identity_and_features_are_the_virtio_net_values() {
    assert_eq!(VIRTIO_NET_DEVICE_ID, 1);
    assert_eq!(MODERN_PCI_DEVICE_BASE + VIRTIO_NET_DEVICE_ID, 0x1041);
    assert_eq!(FEATURE_MAC, 1 << 5);
    assert_eq!(FEATURE_STATUS, 1 << 16);
    assert_eq!(FEATURE_MRG_RXBUF, 1 << 15);
    assert_eq!((QUEUE_RX, QUEUE_TX), (0, 1));
}

#[test]
fn the_config_space_places_mac_status_and_mtu_at_their_offsets() {
    assert_eq!(CONFIG_MAC, 0);
    assert_eq!(CONFIG_STATUS, 6);
    assert_eq!(CONFIG_MTU, 10);
    assert_eq!(LINK_UP, 1);
}

#[test]
fn a_net_header_is_twelve_bytes_and_round_trips() {
    let header = NetHeader {
        flags: 1,
        gso_type: 0,
        hdr_len: 0x0102,
        gso_size: 0x0304,
        csum_start: 0x0506,
        csum_offset: 0x0708,
        num_buffers: 2,
    };
    let bytes = header.encode();
    assert_eq!(bytes.len(), NET_HEADER_BYTES);
    assert_eq!(&bytes[2..4], &[0x02, 0x01], "little-endian hdr_len");
    assert_eq!(&bytes[10..12], &[2, 0], "num_buffers is the last field");
    assert_eq!(NetHeader::decode(&bytes), Some(header));
    assert_eq!(NetHeader::decode(&bytes[..11]), None, "short buffers are refused");
}

#[test]
fn a_plain_header_asks_for_no_offload_and_one_buffer() {
    let plain = NetHeader::plain();
    assert_eq!(plain.flags, 0);
    assert_eq!(plain.gso_type, 0);
    assert_eq!(plain.num_buffers, 1);
    assert_eq!(plain.encode()[10..12], [1, 0]);
}

#[test]
fn unicast_and_valid_source_addresses_are_told_apart() {
    let unicast = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    assert!(is_unicast(&unicast));
    assert!(is_valid_source(&unicast));
    assert!(!is_unicast(&BROADCAST), "the group bit is set");
    assert!(!is_unicast(&[0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]), "a multicast address");
    assert!(!is_valid_source(&[0u8; 6]), "all zeros is not a source");
}

#[test]
fn a_mac_address_prints_as_lower_case_hex_pairs() {
    let text = mac_text(&[0x52, 0x54, 0x00, 0x12, 0xAB, 0xCD]);
    assert_eq!(&text, b"52:54:00:12:ab:cd");
}

#[test]
fn frames_are_padded_to_the_minimum_and_refused_over_the_mtu() {
    assert_eq!(transmit_length(20, MTU_DEFAULT), Ok(FRAME_MIN), "short frames pad to 60");
    assert_eq!(transmit_length(100, MTU_DEFAULT), Ok(100));
    assert_eq!(transmit_length(FRAME_MAX_DEFAULT, MTU_DEFAULT), Ok(1514));
    assert_eq!(
        transmit_length(FRAME_MAX_DEFAULT + 1, MTU_DEFAULT),
        Err(FrameError::TooLarge)
    );
    assert_eq!(transmit_length(13, MTU_DEFAULT), Err(FrameError::TooShort));
}

#[test]
fn an_ethernet_header_gives_its_addresses_and_ethertype() {
    let mut frame = [0u8; 14];
    frame[0..6].copy_from_slice(&BROADCAST);
    frame[6..12].copy_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    frame[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    assert_eq!(
        parse_ethernet(&frame),
        Some((BROADCAST, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56], ETHERTYPE_ARP))
    );
    assert_eq!(parse_ethernet(&frame[..13]), None);
    assert_eq!(ETHERTYPE_IPV4, 0x0800);
    assert_eq!(ETHERTYPE_IPV6, 0x86DD);
}
