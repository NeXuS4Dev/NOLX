//! Host tests. A GPT image is built in memory with the layout the specification gives: primary
//! header in LBA 1, table from LBA 2, backup table, and backup header in the last LBA. Each test
//! breaks one rule and checks that exactly that rule is reported.

use super::*;

const DISK_SECTORS: u64 = 2048;
const TABLE_BYTES: usize = 128 * 128;
const TABLE_SECTORS: u64 = 32;
const FIRST_USABLE: u64 = 34;
const BACKUP_TABLE_LBA: u64 = DISK_SECTORS - 1 - TABLE_SECTORS;
const LAST_USABLE: u64 = BACKUP_TABLE_LBA - 1;
const DISK_GUID: [u8; 16] = [0xD1; 16];
const TYPE: [u8; 16] = [0x11; 16];
const TYPE_TWO: [u8; 16] = [0x22; 16];

/// Recomputes a header's CRC-32 after its bytes were edited, as a writer would.
fn seal(sector: &mut [u8]) {
    sector[16..20].fill(0);
    let crc = crc32(&sector[..92]);
    sector[16..20].copy_from_slice(&crc.to_le_bytes());
}

fn header(my: u64, alternate: u64, entries_lba: u64, table_crc: u32) -> Vec<u8> {
    let mut h = vec![0u8; SECTOR_SIZE];
    h[..8].copy_from_slice(&SIGNATURE);
    h[8..12].copy_from_slice(&REVISION_1_0.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&my.to_le_bytes());
    h[32..40].copy_from_slice(&alternate.to_le_bytes());
    h[40..48].copy_from_slice(&FIRST_USABLE.to_le_bytes());
    h[48..56].copy_from_slice(&LAST_USABLE.to_le_bytes());
    h[56..72].copy_from_slice(&DISK_GUID);
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&table_crc.to_le_bytes());
    seal(&mut h);
    h
}

/// A disk whose used entries are `(slot, type, first, last)`. Every other entry is unused.
fn disk(entries: &[(usize, [u8; 16], u64, u64)]) -> Vec<u8> {
    let mut table = vec![0u8; TABLE_BYTES];
    for &(slot, type_guid, first, last) in entries {
        let at = slot * 128;
        table[at..at + 16].copy_from_slice(&type_guid);
        table[at + 16..at + 32].copy_from_slice(&[slot as u8 + 1; 16]);
        table[at + 32..at + 40].copy_from_slice(&first.to_le_bytes());
        table[at + 40..at + 48].copy_from_slice(&last.to_le_bytes());
        for (i, unit) in "nolx".encode_utf16().enumerate() {
            let o = at + 56 + 2 * i;
            table[o..o + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let table_crc = crc32(&table);
    let mut image = vec![0u8; DISK_SECTORS as usize * SECTOR_SIZE];
    let primary = header(1, DISK_SECTORS - 1, 2, table_crc);
    image[512..1024].copy_from_slice(&primary);
    image[1024..1024 + TABLE_BYTES].copy_from_slice(&table);
    let backup_table = BACKUP_TABLE_LBA as usize * SECTOR_SIZE;
    image[backup_table..backup_table + TABLE_BYTES].copy_from_slice(&table);
    let backup = header(DISK_SECTORS - 1, 1, BACKUP_TABLE_LBA, table_crc);
    let backup_header = (DISK_SECTORS as usize - 1) * SECTOR_SIZE;
    image[backup_header..backup_header + SECTOR_SIZE].copy_from_slice(&backup);
    image
}

fn primary_bytes(image: &[u8]) -> Vec<u8> {
    image[512..1024].to_vec()
}

fn table(image: &[u8]) -> &[u8] {
    &image[1024..1024 + TABLE_BYTES]
}

fn parse_at(sector: &[u8], lba: u64) -> Result<Header, GptError> {
    Header::parse(sector, lba, DISK_SECTORS)
}

#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn a_valid_primary_header_is_read() {
    let image = disk(&[]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(h.my_lba, 1);
    assert_eq!(h.alternate_lba, DISK_SECTORS - 1);
    assert_eq!(h.first_usable, FIRST_USABLE);
    assert_eq!(h.last_usable, LAST_USABLE);
    assert_eq!(h.entries_lba, 2);
    assert_eq!(h.entry_count, 128);
    assert_eq!(h.entry_size, 128);
    assert_eq!(h.disk_guid, DISK_GUID);
}

#[test]
fn the_backup_header_is_read_at_its_own_lba_and_its_table_verifies() {
    let image = disk(&[]);
    let backup_at = (DISK_SECTORS as usize - 1) * SECTOR_SIZE;
    let backup_bytes = &image[backup_at..backup_at + SECTOR_SIZE];
    let h = parse_at(backup_bytes, DISK_SECTORS - 1).unwrap();
    assert_eq!(h.entries_lba, BACKUP_TABLE_LBA);
    assert!(verify_table(&h, &image[BACKUP_TABLE_LBA as usize * SECTOR_SIZE..]).is_ok());
}

#[test]
fn a_sector_shorter_than_a_header_is_refused() {
    assert_eq!(parse_at(&[0u8; 64], 1), Err(GptError::TooShort));
}

#[test]
fn a_header_without_the_signature_is_refused() {
    let mut image = disk(&[]);
    image[512] = b'X';
    assert_eq!(parse_at(&primary_bytes(&image), 1), Err(GptError::BadSignature));
}

#[test]
fn a_header_of_another_revision_is_refused() {
    let mut image = disk(&[]);
    image[512 + 10] = 0;
    assert_eq!(parse_at(&primary_bytes(&image), 1), Err(GptError::BadRevision));
}

#[test]
fn a_header_size_below_the_minimum_is_refused() {
    let mut image = disk(&[]);
    image[512 + 12..512 + 16].copy_from_slice(&91u32.to_le_bytes());
    assert_eq!(parse_at(&primary_bytes(&image), 1), Err(GptError::BadHeaderSize));
}

#[test]
fn an_edited_header_without_a_new_crc_is_refused() {
    let mut image = disk(&[]);
    image[512 + 60] ^= 0xFF;
    assert_eq!(parse_at(&primary_bytes(&image), 1), Err(GptError::BadHeaderCrc));
}

#[test]
fn a_nonzero_reserved_field_is_refused_even_with_a_valid_crc() {
    let mut h = primary_bytes(&disk(&[]));
    h[20] = 1;
    seal(&mut h);
    assert_eq!(parse_at(&h, 1), Err(GptError::BadReserved));
}

#[test]
fn a_header_read_from_the_wrong_lba_is_refused() {
    let image = disk(&[]);
    assert_eq!(parse_at(&primary_bytes(&image), DISK_SECTORS - 1), Err(GptError::BadLocation));
}

#[test]
fn an_entry_size_that_is_not_a_power_of_two_of_at_least_128_is_refused() {
    for size in [0u32, 64, 96, 100] {
        let mut h = primary_bytes(&disk(&[]));
        h[84..88].copy_from_slice(&size.to_le_bytes());
        seal(&mut h);
        assert_eq!(parse_at(&h, 1), Err(GptError::BadEntrySize), "size {size}");
    }
}

#[test]
fn a_table_larger_than_the_limit_is_refused() {
    let mut h = primary_bytes(&disk(&[]));
    h[80..84].copy_from_slice(&129u32.to_le_bytes());
    seal(&mut h);
    assert_eq!(parse_at(&h, 1), Err(GptError::TooManyEntries));
}

#[test]
fn a_usable_area_that_overlaps_the_table_or_runs_past_the_disk_is_refused() {
    let mut overlaps = primary_bytes(&disk(&[]));
    overlaps[40..48].copy_from_slice(&33u64.to_le_bytes());
    seal(&mut overlaps);
    assert_eq!(parse_at(&overlaps, 1), Err(GptError::BadUsableRange));

    let mut past_end = primary_bytes(&disk(&[]));
    past_end[48..56].copy_from_slice(&DISK_SECTORS.to_le_bytes());
    seal(&mut past_end);
    assert_eq!(parse_at(&past_end, 1), Err(GptError::BadUsableRange));
}

#[test]
fn a_table_with_a_bad_crc_is_refused() {
    let mut image = disk(&[(0, TYPE, 34, 133)]);
    image[1024 + 5] ^= 1;
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(verify_table(&h, table(&image)), Err(GptError::BadEntriesCrc));
}

#[test]
fn a_table_shorter_than_the_header_says_is_refused() {
    let image = disk(&[]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(verify_table(&h, &table(&image)[..100]), Err(GptError::EntriesTooShort));
}

#[test]
fn used_entries_are_read_and_counted() {
    let image = disk(&[(0, TYPE, 34, 133), (3, TYPE_TWO, 200, 299)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    verify_table(&h, table(&image)).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Ok(2));

    let first = entry(&h, table(&image), 0).unwrap();
    assert_eq!(first.type_guid, TYPE);
    assert_eq!(first.first_lba, 34);
    assert_eq!(first.last_lba, 133);
    assert_eq!(first.sectors(), 100);
    let expected: Vec<u16> = "nolx".encode_utf16().collect();
    assert_eq!(&first.name[..4], &expected[..]);

    assert!(entry(&h, table(&image), 1).unwrap().is_unused());
    let second = entry(&h, table(&image), 3).unwrap();
    assert_eq!(second.first_lba, 200);
    assert!(entry(&h, table(&image), 128).is_none());
}

#[test]
fn a_partition_outside_the_usable_area_is_refused() {
    let image = disk(&[(0, TYPE, 10, 50)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Err(GptError::PartitionOutOfRange));
}

#[test]
fn a_partition_that_ends_before_it_starts_is_refused() {
    let image = disk(&[(0, TYPE, 300, 200)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Err(GptError::PartitionInverted));
}

#[test]
fn two_partitions_sharing_a_block_are_refused() {
    let image = disk(&[(0, TYPE, 100, 199), (1, TYPE_TWO, 150, 250)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Err(GptError::PartitionsOverlap));
}

#[test]
fn adjacent_partitions_do_not_overlap() {
    let image = disk(&[(0, TYPE, 100, 199), (1, TYPE_TWO, 200, 299)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Ok(2));
}

#[test]
fn an_unused_entry_is_not_checked_at_all() {
    // Type GUID zero marks the entry unused, so its garbage bounds are ignored.
    let image = disk(&[(0, [0; 16], 5, 1)]);
    let h = parse_at(&primary_bytes(&image), 1).unwrap();
    assert_eq!(check_partitions(&h, table(&image)), Ok(0));
}
