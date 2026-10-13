# Assertions: GUID Partition Table rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, block layer with partitions) and
`crates/nolx-block-core`. The rules are taken from the UEFI specification's GUID Partition Table
chapter (header layout, header and table CRC-32, entry layout, usable area). Cited from the
specification's published structure, not from a fetched copy. The crate is host-tested only; no
kernel path reads a GPT yet. All rows are `tested-T1`: the host tests passed in CI (run [38444462222](https://github.com/NeXuS4Dev/NOLX/actions/runs/38444462222), host-test job).

Rows are pipe-delimited: `ID | specification reference | status | test`.

BLK-001 | NOLX M9 GPT: CRC-32 is the IEEE form whose check value for `123456789` is `0xCBF43926` | tested-T1 | nolx-block-core tests::crc32_matches_the_standard_check_value
BLK-002 | NOLX M9 GPT: A primary header in LBA 1 with the signature `EFI PART`, revision 1.0, a valid header CRC over its bytes with the CRC field zeroed, and a matching own-LBA field is read, and its fields are reported as stored | tested-T1 | nolx-block-core tests::a_valid_primary_header_is_read
BLK-003 | NOLX M9 GPT: The backup header at the last LBA is read against its own LBA, and the table it names passes its CRC | tested-T1 | nolx-block-core tests::the_backup_header_is_read_at_its_own_lba_and_its_table_verifies
BLK-004 | NOLX M9 GPT: A sector shorter than a header is refused | tested-T1 | nolx-block-core tests::a_sector_shorter_than_a_header_is_refused
BLK-005 | NOLX M9 GPT: A header without the `EFI PART` signature is refused | tested-T1 | nolx-block-core tests::a_header_without_the_signature_is_refused
BLK-006 | NOLX M9 GPT: A header whose revision is not 1.0 is refused | tested-T1 | nolx-block-core tests::a_header_of_another_revision_is_refused
BLK-007 | NOLX M9 GPT: A header size below 92 bytes is refused | tested-T1 | nolx-block-core tests::a_header_size_below_the_minimum_is_refused
BLK-008 | NOLX M9 GPT: A header whose bytes change without a new CRC is refused | tested-T1 | nolx-block-core tests::an_edited_header_without_a_new_crc_is_refused
BLK-009 | NOLX M9 GPT: A nonzero reserved header field is refused even when the CRC is valid | tested-T1 | nolx-block-core tests::a_nonzero_reserved_field_is_refused_even_with_a_valid_crc
BLK-010 | NOLX M9 GPT: A header read from an LBA other than the one it names as its own is refused | tested-T1 | nolx-block-core tests::a_header_read_from_the_wrong_lba_is_refused
BLK-011 | NOLX M9 GPT: The entry size must be a power of two of at least 128 bytes | tested-T1 | nolx-block-core tests::an_entry_size_that_is_not_a_power_of_two_of_at_least_128_is_refused
BLK-012 | NOLX M9 GPT: A table larger than 16 KiB is refused | tested-T1 | nolx-block-core tests::a_table_larger_than_the_limit_is_refused
BLK-013 | NOLX M9 GPT: A usable area that is empty, inverted, overlaps the table, or runs past the disk is refused | tested-T1 | nolx-block-core tests::a_usable_area_that_overlaps_the_table_or_runs_past_the_disk_is_refused
BLK-014 | NOLX M9 GPT: A table whose CRC-32 disagrees with the header is refused | tested-T1 | nolx-block-core tests::a_table_with_a_bad_crc_is_refused
BLK-015 | NOLX M9 GPT: A buffer shorter than the table the header describes is refused | tested-T1 | nolx-block-core tests::a_table_shorter_than_the_header_says_is_refused
BLK-016 | NOLX M9 GPT: Used entries (non-zero type GUID) are read with their LBA range, name, and sector count, and counted; unused entries are read as unused; indices past the table are absent | tested-T1 | nolx-block-core tests::used_entries_are_read_and_counted
BLK-017 | NOLX M9 GPT: A used partition that extends outside the usable area is refused | tested-T1 | nolx-block-core tests::a_partition_outside_the_usable_area_is_refused
BLK-018 | NOLX M9 GPT: A used partition whose last LBA precedes its first is refused | tested-T1 | nolx-block-core tests::a_partition_that_ends_before_it_starts_is_refused
BLK-019 | NOLX M9 GPT: Two used partitions that share a block are refused | tested-T1 | nolx-block-core tests::two_partitions_sharing_a_block_are_refused
BLK-020 | NOLX M9 GPT: Partitions that touch without sharing a block are accepted | tested-T1 | nolx-block-core tests::adjacent_partitions_do_not_overlap
BLK-021 | NOLX M9 GPT: An unused entry is not range-checked, whatever its bounds hold | tested-T1 | nolx-block-core tests::an_unused_entry_is_not_checked_at_all
