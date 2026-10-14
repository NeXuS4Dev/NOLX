# Assertions: network device rules (virtio-net and Ethernet)

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, Networking) and `crates/nolx-net-core`. The
rules cover the virtio-net device identity, feature bits, configuration space, queue layout, and the
12-byte net header; and the Ethernet frame size limits, header fields, and MAC address rules. The
kernel moves frames through the virtqueues; that driver is a later slice.

The virtio-net device ID (1), the modern PCI device ID base (0x1040), the feature bit positions, the
configuration offsets, the 12-byte header layout, the Ethernet minimum and maximum frame lengths, and
the ethertype values are from the virtio 1.x and IEEE 802.3 specifications as recalled, not yet
checked against the documents. Verify them before the rows are promoted.

Rows are pipe-delimited: `ID | specification reference | status | test`.

NET-001 | NOLX M9 Device identity and features: the virtio-net device ID is 1, its modern PCI ID is 0x1041, the MAC, MTU, status, and mergeable-buffer feature bits sit at their positions, and the receive and transmit queues are 0 and 1 | tested-T1 | nolx-net-core tests::the_device_identity_and_features_are_the_virtio_net_values
NET-002 | NOLX M9 Configuration space: the MAC address, link status, and MTU are at their offsets, and the link-up bit is bit 0 | tested-T1 | nolx-net-core tests::the_config_space_places_mac_status_and_mtu_at_their_offsets
NET-003 | NOLX M9 Net header: the 12-byte header encodes its fields little-endian with the buffer count last, decodes back to the same fields, and a buffer shorter than 12 bytes is refused; a plain header asks for no offload and one buffer | tested-T1 | nolx-net-core tests::a_net_header_is_twelve_bytes_and_round_trips, tests::a_plain_header_asks_for_no_offload_and_one_buffer
NET-004 | NOLX M9 MAC addresses: unicast is decided by the group bit, an all-zero address is not a valid source, and an address prints as lower-case hex pairs | tested-T1 | nolx-net-core tests::unicast_and_valid_source_addresses_are_told_apart, tests::a_mac_address_prints_as_lower_case_hex_pairs
NET-005 | NOLX M9 Frame length: a frame shorter than 60 bytes is padded to 60, a frame up to the MTU plus the 14-byte Ethernet header is sent as it is, a larger one is refused, and a frame without a full header is refused | tested-T1 | nolx-net-core tests::frames_are_padded_to_the_minimum_and_refused_over_the_mtu
NET-006 | NOLX M9 Ethernet header: the destination, source, and big-endian ethertype are read from their fields, and a frame shorter than 14 bytes gives no header | tested-T1 | nolx-net-core tests::an_ethernet_header_gives_its_addresses_and_ethertype

All six rows are `tested-T1`: the host tests passed in CI (workflow 38603598789, host-test job).
