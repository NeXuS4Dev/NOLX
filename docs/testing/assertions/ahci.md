# Assertions: AHCI host controller and SATA port rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, AHCI) and `crates/nolx-ahci-core`, with the
boot probe in `kernel/src/hal/ahci.rs`. This file covers the AHCI register layout, the port state
rules, the FIS and command-header encodings, and the physical region descriptors, all host-tested
in `nolx-ahci-core`; and the boot probe, which reads sector 0 from a SATA disk through a command
slot. AHCI-001 to AHCI-007 are `tested-T1`: the host tests passed in CI (workflow 38460754722, host-test
job). AHCI-008 and AHCI-009 are `tested-T2`: the boot smoke test asserted their markers in the same run.

The register offsets, the FIS and command-header layouts, and the SIG_SATA value are from the
AHCI 1.3.1 specification as recalled, not yet checked against the document. Verify them before
the rows are promoted.

Rows are pipe-delimited: `ID | specification reference | status | test`.

AHCI-001 | NOLX M9 HBA and port registers: the HBA registers sit at their specified offsets, port registers repeat every 0x80 bytes from 0x100, and port 32 is refused | tested-T1 | nolx-ahci-core tests::a_port_register_sits_in_its_own_block
AHCI-002 | NOLX M9 Command slots: the slot count is the capability field's 5-bit value plus one | tested-T1 | nolx-ahci-core tests::the_capability_field_gives_one_more_than_its_zero_based_value
AHCI-003 | NOLX M9 Port implementation and device type: only the ports named by the implemented bitmap are used, and a port is a SATA disk only when its link is present and its signature is SATA | tested-T1 | nolx-ahci-core tests::only_the_ports_the_bitmap_names_are_implemented, tests::a_sata_disk_needs_a_present_link_and_a_sata_signature
AHCI-004 | NOLX M9 Task file status: the error bit and the busy and data-request bits are read separately | tested-T1 | nolx-ahci-core tests::the_task_file_reports_errors_and_busy_separately
AHCI-005 | NOLX M9 Read command FIS: a host-to-device FIS for READ DMA EXT carries the 48-bit sector address and sector count in its specified bytes | tested-T1 | nolx-ahci-core tests::read_dma_ext_fis_carries_the_48_bit_address_and_count
AHCI-006 | NOLX M9 Command header: the FIS length, write direction, descriptor count, and 128-byte-aligned table address are encoded in their fields, and a misaligned table or a FIS length outside 2 to 16 dwords is refused | tested-T1 | nolx-ahci-core tests::a_command_header_names_its_fis_length_direction_and_table, tests::a_command_header_is_refused_for_a_misaligned_table_or_a_bad_fis_length
AHCI-007 | NOLX M9 Physical region descriptor: the address is split into its low and high words, the byte count is stored one less than the bytes, and an empty, odd, misaligned, or over-4-MiB region is refused | tested-T1 | nolx-ahci-core tests::a_region_descriptor_stores_its_count_one_less_than_the_bytes, tests::a_region_descriptor_refuses_empty_odd_misaligned_and_oversized_regions
AHCI-008 | NOLX M9 Controller and port discovery: the probe finds the AHCI class device, reads the HBA through BAR 5, enables AHCI mode, and reports the first port with a SATA disk | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `ahci: BB:DD.F abar 0x..., ports 0x..., port N is a SATA disk`; kernel/src/hal/ahci.rs bring_up; QEMU run with `-device ahci` and `-device ide-hd` on a copy of the disk image; workflow [38460754722](https://github.com/NeXuS4Dev/NOLX/actions/runs/38460754722) passed all six jobs.
AHCI-009 | NOLX M9 Polled sector read: sector 0 is read through command slot 0 with a READ DMA EXT, the port completes it without a task-file error, and the data matches the disk image's sector 0 | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `ahci: port N sector 0 read, status ok, first 16 bytes match the disk image`; kernel/src/hal/ahci.rs read_sector_zero; workflow [38460754722](https://github.com/NeXuS4Dev/NOLX/actions/runs/38460754722) passed all six jobs.
AHCI-010 | NOLX M9 Write command FIS: WRITE DMA EXT uses command 0x35 with the same 48-bit address and sector-count layout as READ DMA EXT, and only the command byte differs | tested-T1 | nolx-ahci-core tests::write_dma_ext_fis_names_the_write_command_and_the_same_address_layout
AHCI-011 | NOLX M9 Polled round trip: a 2 KiB pattern is written to sectors 1600 to 1603 through command slot 0, the buffer is cleared, the sectors are read back, and the bytes are checked by CRC-32; CI checksums the same sectors in the SATA disk copy after QEMU exits | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `ahci: wrote 4 sectors at 1600, read back, crc32 0x... matches the written data` and the step "The AHCI written sectors are on the SATA disk copy" checks the copy; kernel/src/hal/ahci.rs bring_up and transfer; workflow [38523473276](https://github.com/NeXuS4Dev/NOLX/actions/runs/38523473276) passed all six jobs.

AHCI-012 | NOLX M9 AHCI MSI completion: port D2H register-FIS causes and HBA causes are acknowledged before APIC EOI; every disk command waits for its completion interrupt | pending-T2 | boot-smoke and storage-backends require `ahci: MSI completion interrupts: 3 of 3 commands, vector 0x52`, plus persistent disk checksums
