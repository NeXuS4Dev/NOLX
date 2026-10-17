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

NET-007 | NOLX M9 ARP reachability: minimum Ethernet ARP requests use network byte order and valid replies must match hardware/protocol/opcode, both MACs and both IPv4 endpoints; malformed or unrelated packets are refused | tested-T1 | nolx-net-core arp::tests (local Rust 1.88: 3 passed)
NET-008 | NOLX M9 receive queue: bounded FIFO copies frames, preserves order across wrap, refuses overflow without overwriting unread packets, validates frame sizes and handles zero capacity | tested-T1 | nolx-net-core queue::tests (local Rust 1.88: 2 passed)

NET-014 | NOLX M9 Modern virtio-net transport: negotiate VERSION_1 and MAC only; refuse unavailable MAC/modern transport, offload flags, oversized/truncated frames and multi-buffer RX headers | tested-T1 | nolx-virtio-core net::tests, passed locally with Rust 1.88.0 on 2026-10-16
NET-015 | NOLX M9 Native virtio-net queues and MSI-X: bind through nolx-df, retain RX/TX queues and DMA, recycle receive descriptors, and take two RX and two TX completion interrupts on separate vectors | tested-T2 | scripts/check-virtio-net.py; CI run [38960237622](https://github.com/NeXuS4Dev/NOLX/actions/runs/38960237622) on bebe438 passed all ten jobs; stricter complete-memory/zero-heap-leak assertions also passed in the network job on 1cdc218
NET-016 | NOLX M9 Real network bytes: capture two exact transmitted Ethernet ARP requests and two externally supplied gateway replies independently on the host | tested-T2 | scripts/check-virtio-net.py parses QEMU filter-dump pcap and compares source/destination MACs, protocol fields and IPv4 addresses; CI run [38960237622](https://github.com/NeXuS4Dev/NOLX/actions/runs/38960237622) passed the hardware job on pc/Q35, modern/transitional, absent/no-APIC variants

NET-017 | NOLX M9 Exact-adapter native network endpoints: per-handle BDF/MAC, transport and FIFO survive probe return/restart; public TX/RX/MAC refuses stopped, removed, invalid and wrong-driver handles even with Started peers; recursive public I/O returns Busy | tested-T2 | kernel/src/hal/driver_start.rs net_multi_selftest; scripts/check-network-multi.py; CI run [39013817622](https://github.com/NeXuS4Dev/NOLX/actions/runs/39013817622) on 9566b32 passed all fourteen jobs; nine local QEMU 11.0.2 fixtures passed 2026-10-17, including secondary PCI buses and six-storage/four-NIC integration; segment zero only
NET-018 | NOLX M9 Multi-adapter interrupt and wire isolation: shared INTx acknowledges every matching e1000 before EOI, virtio-net RX/TX vectors are distinct per adapter, stopped targets do not break peer wire traffic, queued-all-adapter bursts reach own FIFOs, and independent pcaps verify 214 tagged public requests and gateway replies | tested-T2 | scripts/check-network-multi.py; CI run [39013817622](https://github.com/NeXuS4Dev/NOLX/actions/runs/39013817622) on 9566b32 passed all fourteen jobs; eight network fixtures plus the combined ten-device fixture; kernel requires each exchange to advance the selected receive-interrupt counter and verifies zero pinned DMA after three-cycle lifecycle/removal
