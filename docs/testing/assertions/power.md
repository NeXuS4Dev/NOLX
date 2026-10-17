# Assertions: ACPI device status, resources, and sleep-state rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, ACPI tables, `_STA`/`_CRS`, and S3/S5 power
policy) and `crates/nolx-power-core`. The rules cover the `_STA` status bits, the `_CRS` resource
descriptors the PC's devices use, the `_Sx_` sleep-type lookup in the AML, and the PM1 control word
that enters a sleep state. The table parsing that finds the RSDP, the root table, the MADT, and HPET
is in `nolx-hal-core` and is not repeated here. The bounded pure `nolx-power-core::aml`
interpreter executes supported firmware methods; the catalog is not a complete hardware inventory.

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

PWR-001 to PWR-005 and PWR-006 to PWR-007 are `tested-T1`, and PWR-008 is `tested-T2`: the host tests and the boot-smoke markers passed in CI (workflow 38730057305).
PWR-006 | NOLX M9 FADT fields: a Fixed ACPI Description Table with a valid checksum gives the DSDT address (the 64-bit field when a revision-2 table sets it, else the 32-bit field) and the PM1a and PM1b control ports | tested-T1 | nolx-power-core tests::a_fadt_gives_the_dsdt_and_the_pm1_control_ports, tests::a_revision_two_fadt_takes_the_64_bit_dsdt_when_it_is_set
PWR-007 | NOLX M9 FADT refusal: a FADT with a bad checksum, the wrong signature, a cut-short length, no DSDT, or no 16-bit PM1a port is refused with its reason | tested-T1 | nolx-power-core tests::a_fadt_that_is_damaged_or_incomplete_is_refused_with_its_reason
PWR-008 | NOLX M9 Sleep types in the kernel: the kernel reads the FADT and the DSDT at boot, reports the PM1a control port, and reads the S5 soft-off type from the DSDT's AML; it reports the S5 word it would write and enters no state | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `acpi: PM1a control block at 0x...` and `acpi: S5 sleep type N, soft-off word would be 0x..., not entered`; kernel/src/hal/acpi.rs

Bounded pure `_STA` and `_CRS` evaluation is now wired before PCI startup. Hardware fields,
operation regions, general root/link resource semantics and evaluated PCI routing remain unwired. S3 resume and system-wide device quiescence remain unwired. The ordinary boot does not enter sleep; the explicit S5 primitive and hardware-test feature below can actually power off a quiesced test VM.


PWR-009 | NOLX M9 S5 register contract: preserve all PM1 bits outside SLP_TYP/SLP_EN; decode ACPI enable port/command and PM1 width without truncating an out-of-range SMI port | tested-T1 | nolx-power-core tests::sleep_control_preserves_every_unrelated_bit and tests::fadt_records_acpi_enable_and_control_width_without_truncating_ports; passed locally with Rust 1.88.0
PWR-010 | NOLX M9 Actual S5 hardware entry: after complete memory tests in a no-active-DMA VM, enable ACPI mode and issue SLP_EN; QMP must report guest-shutdown and QEMU must exit zero on pc and Q35 | tested-T2 | scripts/check-power-s5.py and hal-power-test-s5 feature; CI run [38962036000](https://github.com/NeXuS4Dev/NOLX/actions/runs/38962036000) on 9f8c704 passed all eleven jobs; positive Q35 fixture disables SATA to satisfy the no-active-DMA precondition
PWR-011 | NOLX M9 Conservative S5 test precondition: an active virtio-net PCI bus master prevents the test from issuing S5, and no guest SHUTDOWN event occurs | tested-T2 | scripts/check-power-s5.py active-DMA negative boot; CI run [38962036000](https://github.com/NeXuS4Dev/NOLX/actions/runs/38962036000) passed all eleven jobs; production global quiescence policy remains separate and unimplemented


PWR-012 | NOLX M9 Pure AML namespace and evaluation: scoped names, aliases and deferred references preserve identity; actual method execution observes integer arguments/locals, arithmetic and branches rather than byte scans | tested-T1 | nolx-power-core aml::tests; local 47 power-core tests passed; CI run [39022540188](https://github.com/NeXuS4Dev/NOLX/actions/runs/39022540188) on `353f5d0` passed all sixteen jobs
PWR-013 | NOLX M9 AML refusal boundaries: table loads roll back all objects and allocation accounting; unsupported hardware/global/compound stores, malformed names/packages, recursion, step and aggregate data exhaustion cannot become successful method results | tested-T1 | nolx-power-core aml::tests; local malformed-prefix/opcode/limits tests passed; CI run [39022540188](https://github.com/NeXuS4Dev/NOLX/actions/runs/39022540188) on `353f5d0` passed all sixteen jobs
PWR-014 | NOLX M9 Resource authority bounds: descriptor-local lengths, valid nonoverflowing ranges, optional nonzero checksum and zero-only trailing padding are required before publishing resources | tested-T1 | nolx-power-core tests::resource_descriptor_bounds_cannot_borrow_bytes_from_the_next_descriptor, tests::resource_ranges_and_nonzero_checksums_are_validated, tests::every_truncation_of_a_resource_template_is_refused; CI run [39022540188](https://github.com/NeXuS4Dev/NOLX/actions/runs/39022540188) on `353f5d0` passed all sixteen jobs
PWR-015 | NOLX M9 Native firmware AML catalog: actual PC/Q35 SeaBIOS keyboard/serial methods plus SSDT computed status/resources execute before PCI startup; absent status alone defaults, disabled resources skip, failed status/malformed resources refuse, loops/recursion terminate, bad tables roll back and no-ACPI boots survive without retaining interpreter heap | tested-T2 | scripts/check-aml.py seven local QEMU fixtures passed; catalog does not replace PCI apertures or routing; CI run [39022540188](https://github.com/NeXuS4Dev/NOLX/actions/runs/39022540188) on `353f5d0` passed all sixteen jobs
