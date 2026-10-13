# Assertions: PCI configuration space and device discovery

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers) and `crates/nolx-pci-core`. This file
covers the first M9 slice: legacy configuration-space access, function discovery, BAR decoding
and sizing, capability walking, and bridge-aware bus enumeration. The kernel side (port access
through `0xCF8`/`0xCFC` and a boot-time inventory marker) is not yet wired, so no row here is
claimed as `tested-T2`.

Rows are pipe-delimited: `ID | specification reference | status | test`.

PCI-001 | NOLX M9 Configuration address: the `0xCF8` value packs enable, bus, device, function, and the dword index, and discards the low two offset bits | implemented | nolx-pci-core tests::config_address_packs_bus_device_function_and_dword
PCI-002 | NOLX M9 Addressing limits: a device number of 32 or more and a function number of 8 or more are refused before any access | implemented | nolx-pci-core tests::addresses_the_hardware_cannot_express_are_refused
PCI-003 | NOLX M9 Function identity: a vendor of 0xFFFF or 0 is absent; a present function reports vendor, device, class, subclass, prog-if, revision, header type, and the multifunction bit | implemented | nolx-pci-core tests::an_absent_function_is_not_recorded, tests::a_present_function_reports_its_identity_and_multifunction_bit
PCI-004 | NOLX M9 BAR decoding: I/O, 32-bit and 64-bit memory windows, and reserved types are decoded from the low dword and, for 64-bit windows, the high dword; a 64-bit window occupies two slots | implemented | nolx-pci-core tests::io_bars_decode_to_their_port_and_zero_is_unused, tests::memory_bars_decode_width_and_prefetch
PCI-005 | NOLX M9 BAR sizing: the window size is the power of two given by the probe mask; a slot that implements no bits has no size; every probed register is restored to its original value | implemented | nolx-pci-core tests::bar_sizes_are_powers_of_two_from_the_probe_mask, tests::bars_are_sized_decoded_and_restored
PCI-006 | NOLX M9 Capability walk: the list is read only when the status bit says it exists, pointers into the header are refused, and a looped list is bounded and reported as truncated | implemented | nolx-pci-core tests::a_function_without_a_capability_bit_has_no_list, tests::the_capability_list_is_walked_in_link_order, tests::a_pointer_into_the_header_is_refused_and_a_loop_is_bounded
PCI-007 | NOLX M9 Bus enumeration: every bus is scanned once; multifunction devices expose functions 0 to 7; buses beyond 0 are reached only through a PCI-to-PCI bridge whose secondary bus is above its own; a bridge that points back is refused and the scan terminates | implemented | nolx-pci-core tests::enumeration_follows_multifunction_devices_and_secondary_buses, tests::a_bridge_that_points_back_is_refused_and_enumeration_terminates
PCI-008 | NOLX M9 Inventory capacity: an inventory that is full refuses the next function and records that the list is incomplete | implemented | nolx-pci-core tests::an_inventory_refuses_the_function_after_its_last_slot
