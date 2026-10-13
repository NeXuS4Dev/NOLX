# Assertions: virtio transport and block device rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, virtio-blk) and `crates/nolx-virtio-core`.
This file covers the host-testable rules of the virtio 1.x PCI transport and the block device's
request format. VIO-001 to VIO-009 are `tested-T1`: the host tests in `nolx-virtio-core` are the
evidence. No device has been driven yet, so no row is claimed as `tested-T2`. A boot-level row
(a probe that finds a `-device virtio-blk-pci` disk in QEMU) will be added as `pending-T2` once
the kernel probe and its CI disk exist.

Rows are pipe-delimited: `ID | specification reference | status | test`.

VIO-001 | NOLX M9 Capability discovery: the vendor capabilities of the PCI capability list are read by `cfg_type`, the common, notify, ISR, and device regions are each recorded once, and vendor capabilities of other types are ignored | tested-T1 | nolx-virtio-core tests::discovery_finds_each_region_by_type, tests::a_device_without_a_capability_list_has_no_regions, tests::the_first_valid_capability_of_a_type_wins
VIO-002 | NOLX M9 Capability validation: a virtio capability shorter than 16 bytes, one that runs past the 256-byte space, one naming a BAR above 5, or a notify capability shorter than 20 bytes is skipped, and does not hide the capabilities after it | tested-T1 | nolx-virtio-core tests::malformed_capabilities_are_skipped_not_trusted, tests::a_notify_region_needs_its_multiplier_field, tests::a_capability_past_the_end_of_configuration_space_is_refused
VIO-003 | NOLX M9 Notify address: a queue's notification address is the notify region's offset plus the queue's notify offset times the capability's multiplier | tested-T1 | nolx-virtio-core tests::notify_address_scales_the_queue_offset
VIO-004 | NOLX M9 Feature negotiation: a device that does not offer VIRTIO_F_VERSION_1 (bit 32) is refused, and the driver accepts exactly that bit | tested-T1 | nolx-virtio-core tests::negotiation_requires_version_1_and_accepts_only_it
VIO-005 | NOLX M9 FEATURES_OK readback: a device that clears FEATURES_OK has rejected the feature set; a device reporting NEEDS_RESET or FAILED is reported as failed | tested-T1 | nolx-virtio-core tests::features_ok_readback_is_checked
VIO-006 | NOLX M9 Initialization order: the driver steps through ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK, each step adding bits to the one before | tested-T1 | nolx-virtio-core tests::the_init_sequence_only_adds_bits_and_ends_with_driver_ok
VIO-007 | NOLX M9 Split virtqueue layout: for a queue of n entries the available ring starts after 16n bytes of descriptors, the used ring starts at the next 4-byte boundary, and only power-of-two sizes up to 32768 are accepted | tested-T1 | nolx-virtio-core tests::split_layout_matches_the_specification_sizes, tests::split_layout_refuses_sizes_the_device_cannot_use
VIO-008 | NOLX M9 Descriptor encoding: a descriptor is 16 bytes, little-endian, with the address, length, flags, and next index at their specified offsets | tested-T1 | nolx-virtio-core tests::a_descriptor_encodes_little_endian_in_its_specified_fields
VIO-009 | NOLX M9 Block request format: the 16-byte header holds the request type at offset 0, a zero reserved word, and the sector at offset 8; capacity is the sector count times 512, and an overflowing product is reported | tested-T1 | nolx-virtio-core tests::a_block_request_header_names_the_type_and_sector, tests::capacity_is_sectors_times_512_and_overflow_is_reported
