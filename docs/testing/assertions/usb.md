# Assertions: USB host controller (xHCI) and HID boot keyboard rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, Input: USB xHCI to HID) and `crates/nolx-usb-core`.
The rules cover the xHCI capability and operational register offsets, the port status bits and their
write-one-to-clear behaviour, the transfer request block layouts the controller reads and writes, and
the HID boot protocol keyboard report, turned into the same key events the PS/2 decoder produces. The
kernel maps the controller and moves the rings; that wiring is a later slice.

The xHCI register offsets and bit positions (CAPLENGTH, HCSPARAMS1, DBOFF, USBCMD, USBSTS, PORTSC and
its change bits, port register sets at 0x400 with 0x10 stride), the TRB type numbers and cycle, toggle,
and interrupt-on-completion bits, the event completion code and slot and port fields, and the HID boot
keyboard report layout and modifier bits are from the xHCI and USB HID specifications as recalled, not
yet checked against the documents. Verify them before the rows are promoted.

Rows are pipe-delimited: `ID | specification reference | status | test`.

USB-001 | NOLX M9 Controller registers: port register sets sit 16 bytes apart from 0x400 past the capability length, the port and slot counts come from HCSPARAMS1, and each doorbell is four bytes apart with its reserved low bits dropped | tested-T1 | nolx-usb-core tests::port_register_sets_sit_sixteen_bytes_apart_past_the_operational_base, tests::hcsparams1_gives_the_port_and_slot_counts, tests::doorbells_are_four_bytes_apart_and_the_offset_drops_its_low_bits
USB-002 | NOLX M9 Port reset and acknowledgement: a reset is started with the enable bit and the other write-one-to-clear bits masked so it cannot disable the port or clear a change; acknowledging writes back only the change bits that are set | tested-T1 | nolx-usb-core tests::a_port_reset_keeps_the_port_and_clears_nothing_by_accident, tests::acknowledging_writes_back_only_the_change_bits_that_are_set
USB-003 | NOLX M9 Link TRB: its target is 16-byte aligned or refused, the type is 6, and the cycle and toggle-cycle bits sit in their fields | tested-T1 | nolx-usb-core tests::a_link_trb_names_its_target_and_keeps_the_cycle_and_toggle_bits
USB-004 | NOLX M9 Normal and enable-slot TRBs: the normal TRB carries its buffer, a 17-bit length, and the interrupt-on-completion flag; an enable-slot command has its type and cycle | tested-T1 | nolx-usb-core tests::a_normal_trb_carries_its_buffer_length_and_completion_flag, tests::an_enable_slot_command_has_its_type_and_cycle
USB-005 | NOLX M9 Event TRBs: the completion code, slot, and port are read from their fields | tested-T1 | nolx-usb-core tests::an_event_gives_its_completion_code_slot_and_port
USB-006 | NOLX M9 Boot keyboard report: the difference between two reports gives modifier events in the E0 range, releases of keys no longer held, and presses of new keys, in that order; a roll-over report is refused; codes 0 to 3 are not keys | tested-T1 | nolx-usb-core tests::an_unchanged_report_produces_no_events, tests::a_new_key_presses_and_a_dropped_key_releases, tests::a_modifier_bit_gives_its_usage_in_the_e_range, tests::releases_come_before_presses_and_modifiers_first_of_all, tests::a_roll_over_report_is_not_acted_on, tests::the_reserved_codes_below_four_are_not_keys

All six rows are `tested-T1`: the host tests passed in CI (workflow 38608775432, host-test job).

USB-007 | NOLX M9 xHCI controller bring-up: the controller is found by its class code, memory space and bus mastering are enabled, BAR 0 is mapped through the MMIO window, and the controller is halted, reset, and halted again, each step polled with a bound | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `xhci: <bdf> bar0 0x... , version 0x..., N ports, N slots` and `xhci: controller reset, halted`; kernel/src/hal/xhci.rs
USB-008 | NOLX M9 xHCI root ports: each root port is reported with its connection state, and the summary counts the connected ports | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `xhci: port N connected|empty` and `xhci: N of N ports connected`; CI attaches `qemu-xhci` with a `usb-kbd`; kernel/src/hal/xhci.rs

USB-007 and USB-008 are `tested-T2`: the boot-smoke markers passed in CI (workflow 38621595620).
