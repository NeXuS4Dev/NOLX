# Assertions: NVMe controller and namespace rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, NVMe) and `crates/nolx-nvme-core`, with the
boot probe in `kernel/src/hal/nvme.rs`. This file covers the controller register layout, the
enable sequence, the queue and command encodings, the completion decoding, and the namespace block
size, all host-tested in `nolx-nvme-core`; and the boot probe, which identifies namespace 1, reads
sector 0 through an I/O queue, and round-trips sectors 1600 to 1603 with a CRC-32 check.

The register offsets, the CC, CSTS, and AQA fields, the doorbell stride, the 64-byte submission and
16-byte completion layouts, the admin and I/O opcodes, and the Identify namespace layout are from
the NVMe base specification as recalled, not yet checked against the document. NVME-001 to NVME-008
are `tested-T1`: the host tests passed in CI (workflow 38558796880, host-test job). NVME-009 to
NVME-011 are `tested-T2`: the boot smoke test asserted their markers in the same run.

Rows are pipe-delimited: `ID | specification reference | status | test`.

NVME-001 | NOLX M9 Controller capabilities: the queue size limit, doorbell stride, and ready timeout are decoded from their CAP fields, with the timeout in 500 ms units | tested-T1 | nolx-nvme-core tests::capability_fields_decode_queue_limit_stride_and_timeout
NVME-002 | NOLX M9 Doorbells: each queue's submission and completion doorbells sit at 0x1000 plus (2 times the queue plus 0 or 1) times (4 shifted by the stride) | tested-T1 | nolx-nvme-core tests::doorbells_are_spaced_by_the_stride
NVME-003 | NOLX M9 Enable and admin queue: CC enables the controller with 64-byte submission and 16-byte completion entries, CSTS.RDY reports ready, and AQA stores each admin queue size one less than the count, refusing sizes outside 1 to 4096 | tested-T1 | nolx-nvme-core tests::enable_selects_64_byte_commands_and_16_byte_completions, tests::admin_queue_sizes_are_stored_one_less_and_bounded
NVME-004 | NOLX M9 Submission entry: the opcode and command identifier sit in dword 0, the namespace in dword 1, the data pointer in bytes 24 to 39, and the command dwords from byte 40; Identify carries its CNS value in dword 10 | tested-T1 | nolx-nvme-core tests::a_submission_entry_places_each_field_at_its_offset, tests::identify_asks_for_the_structure_named_by_cns
NVME-005 | NOLX M9 I/O queue creation: Create I/O Completion Queue and Create I/O Submission Queue store the entry count one less, mark the queue physically contiguous, bind the submission queue to its completion queue, and refuse an empty or oversized queue | tested-T1 | nolx-nvme-core tests::io_queues_are_stored_one_entry_less_and_refused_when_empty_or_too_big
NVME-006 | NOLX M9 Read and write commands: the 64-bit LBA is split across dwords 10 and 11, the block count is stored one less in dword 12, and a zero-block transfer is refused | tested-T1 | nolx-nvme-core tests::a_read_or_write_carries_the_split_lba_and_a_zero_based_count
NVME-007 | NOLX M9 Completion entry: the command identifier, phase tag, status field, and submission queue head are read from their dwords, and the phase tag is not part of the status | tested-T1 | nolx-nvme-core tests::a_completion_gives_its_identifier_phase_and_status
NVME-008 | NOLX M9 Namespace: the block count comes from NSZE, and the block size comes from the format selected by FLBAS, refusing an exponent below 9 | tested-T1 | nolx-nvme-core tests::the_namespace_reports_its_block_count_and_block_size
NVME-009 | NOLX M9 Controller discovery: the probe finds the class 01:08:02 device, enables memory decoding and bus mastering, maps BAR 0, disables and re-enables the controller around the admin queue setup, identifies namespace 1, and reports its sector count and 512-byte block size | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `nvme: BB:DD.F bar0 0x..., namespace 1: N sectors of 512 bytes`; kernel/src/hal/nvme.rs bring_up and enable_device; QEMU run with `-device nvme,drive=nvme0,serial=nolx0` Workflow [38558796880](https://github.com/NeXuS4Dev/NOLX/actions/runs/38558796880) passed all six jobs.
NVME-010 | NOLX M9 Polled sector read: sector 0 is read through I/O queue 1, the completion carries the command identifier with status zero, and the data matches the disk image's sector 0 | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `nvme: sector 0 read, status ok, first 16 bytes match the disk image`; kernel/src/hal/nvme.rs execute Workflow [38558796880](https://github.com/NeXuS4Dev/NOLX/actions/runs/38558796880) passed all six jobs.
NVME-011 | NOLX M9 Polled round trip: a 2 KiB pattern is written to sectors 1600 to 1603 through I/O queue 1, the buffer is cleared, the sectors are read back, and the bytes are checked by CRC-32; CI checksums the same sectors in the NVMe disk copy after QEMU exits | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserts `nvme: wrote 4 sectors at 1600, read back, crc32 0x... matches the written data` and the step "The NVMe written sectors are on the NVMe disk copy" checks the copy; kernel/src/hal/nvme.rs bring_up Workflow [38558796880](https://github.com/NeXuS4Dev/NOLX/actions/runs/38558796880) passed all six jobs.
NVME-012 | NOLX M9 Completion queue with interrupts: Create I/O Completion Queue with interrupts on sets bit 1 and the MSI-X table entry in bits 31:16, and refuses a vector past the 2048-entry table or an empty queue | tested-T1 | nolx-nvme-core tests::a_completion_queue_with_interrupts_names_its_msi_x_entry
NVME-013 | NOLX M9 MSI-X completions: the kernel enables MSI-X on the controller, programs the I/O queue's table entry to the local APIC with vector 0x50, and every I/O command raises one completion interrupt that the handler counts | implemented | `.github/workflows/ci.yml` boot-smoke asserts `nvme: msi-x completion interrupts: N of N I/O commands, vector 0x50`; kernel/src/hal/nvme.rs; kernel/src/hal/interrupts.rs

