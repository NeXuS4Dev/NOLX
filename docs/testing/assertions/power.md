# Assertions: ACPI device status, resources, and sleep-state rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, ACPI tables, `_STA`/`_CRS`, and S3/S5 power
policy) and `crates/nolx-power-core`. The rules cover the `_STA` status bits, the `_CRS` resource
descriptors the PC's devices use, the `_Sx_` sleep-type lookup in the AML, and the PM1 control word
that enters a sleep state. The table parsing that finds the RSDP, the root table, the MADT, and HPET
is in `nolx-hal-core` and is not repeated here. The kernel runs no AML; these rules read what the
firmware returns.

The `_STA` bit positions, the small and large resource descriptor tags and their layouts (IRQ 0x22,
I/O 0x47, fixed I/O 0x4B, Memory32Fixed 0x86, QWord address space 0x8A, end tag 0x79), the AML name,
package, and byte-prefix opcodes, and the PM1 SLP_TYP (bits 12:10) and SLP_EN (bit 13) fields are
from the ACPI specification as recalled, not yet checked against the document. Verify them before the
rows are promoted.

Rows are pipe-delimited: `ID | specification reference | status | test`.

PWR-001 | NOLX M9 Device status: a device is usable only when its `_STA` value has the present, enabled, and functioning bits set | tested-T1 | nolx-power-core tests::a_device_is_usable_only_when_present_enabled_and_functioning
PWR-002 | NOLX M9 Resource templates: IRQ, I/O port, fixed I/O, Memory32Fixed, and QWord memory descriptors decode to their ranges, and a template ends at its end tag | tested-T1 | nolx-power-core tests::an_irq_and_an_io_port_descriptor_are_decoded, tests::a_fixed_io_and_a_fixed_memory_descriptor_are_decoded, tests::a_qword_memory_descriptor_takes_its_minimum_and_length
PWR-003 | NOLX M9 Template bounds: a template with no end tag, a descriptor cut short, an unknown descriptor, or more than sixteen resources is refused with its reason | tested-T1 | nolx-power-core tests::a_template_without_an_end_tag_or_cut_short_is_refused, tests::an_unknown_descriptor_is_refused_and_named, tests::a_template_holds_at_most_sixteen_resources
PWR-004 | NOLX M9 Sleep types: the first sleep type of the `_S3_` or `_S5_` package is read from the AML when the name is opened by a name opcode and followed by a package with a constant element; a name without that shape gives nothing | tested-T1 | nolx-power-core tests::the_sleep_type_comes_from_the_s5_package_in_the_aml, tests::a_zero_or_one_sleep_type_is_read_without_a_prefix, tests::a_name_not_opened_by_a_name_opcode_is_not_a_sleep_package
PWR-005 | NOLX M9 PM1 sleep control: the sleep type sits in bits 12 to 10 and the go bit in bit 13, and a type that does not fit in three bits is refused | tested-T1 | nolx-power-core tests::the_pm1_word_places_the_type_and_the_go_bit

All five rows are `tested-T1`: the host tests passed in CI (workflow 38607105889, host-test job).
