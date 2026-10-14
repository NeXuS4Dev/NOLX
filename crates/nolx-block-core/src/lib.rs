//! GUID Partition Table rules, as far as they can be decided from the bytes on disk.
//!
//! A GPT disk has a primary header in LBA 1, a table of partition entries after it, and a backup
//! header and table at the end of the disk. Whether a header is genuine, whether its checksums
//! agree, which entries are in use, and whether those entries sit inside the usable area and do
//! not overlap are all rules over bytes, so they live here, host-tested. The kernel reads the
//! sectors and hands them in; nothing in this crate touches a device.
//!
//! Scope: revision 1.0 headers with 512-byte logical blocks. Protective MBRs, the
//! hybrid MBR, and writing a table are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

/// Bytes per logical block. The kernel's block devices are 512-byte sectors.
pub const SECTOR_SIZE: usize = 512;
/// The eight bytes that open every GPT header.
pub const SIGNATURE: [u8; 8] = *b"EFI PART";
/// The header revision this crate reads: 1.0, stored little-endian in the upper half.
pub const REVISION_1_0: u32 = 0x0001_0000;
/// The smallest header size the specification allows.
const HEADER_MIN_SIZE: usize = 92;
/// The smallest partition entry size the specification allows.
const ENTRY_MIN_SIZE: usize = 128;
/// The largest partition table this crate will accept, in bytes. 16 KiB is the table the
/// specification's defaults produce (128 entries of 128 bytes), and it bounds the overlap check.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;
/// The first LBA a partition table may occupy: LBA 0 is the protective MBR, LBA 1 the header.
const FIRST_TABLE_LBA: u64 = 2;

/// Why a header, table, or partition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptError {
    /// The sector given is shorter than a header.
    TooShort,
    /// The header does not open with `EFI PART`.
    BadSignature,
    /// The header revision is not 1.0.
    BadRevision,
    /// The header size is below 92 bytes or larger than the sector.
    BadHeaderSize,
    /// The header CRC-32 does not match the header bytes.
    BadHeaderCrc,
    /// A field the specification requires to be zero is not.
    BadReserved,
    /// The header says it lives somewhere other than where it was read from.
    BadLocation,
    /// The partition entry size is not a power of two of at least 128 bytes.
    BadEntrySize,
    /// The table would be larger than [`MAX_ENTRY_BYTES`].
    TooManyEntries,
    /// The bytes given for the table are shorter than the header says it is.
    EntriesTooShort,
    /// The partition table CRC-32 does not match the table bytes.
    BadEntriesCrc,
    /// The usable area is empty, inverted, runs past the disk, or overlaps the table.
    BadUsableRange,
    /// A used partition's last LBA is before its first.
    PartitionInverted,
    /// A used partition extends outside the usable area.
    PartitionOutOfRange,
    /// Two used partitions share a block.
    PartitionsOverlap,
}

/// Feeds bytes into a CRC-32 register. The register starts at `0xFFFF_FFFF` and is inverted
/// once at the end; splitting the input across calls gives the same result as one call.
fn crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// CRC-32 in the IEEE 802.3 form GPT uses: reflected, polynomial `0xEDB88320`, initial and
/// final XOR `0xFFFFFFFF`. The check value of `123456789` is `0xCBF43926`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    !crc_update(0xFFFF_FFFF, bytes)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(out)
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(out)
}

/// A validated GPT header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The LBA this header describes itself as living at.
    pub my_lba: u64,
    /// The LBA of the other header: the backup for a primary, the primary for a backup.
    pub alternate_lba: u64,
    /// The first LBA that may hold partition data.
    pub first_usable: u64,
    /// The last LBA that may hold partition data.
    pub last_usable: u64,
    /// The disk's GUID, as stored.
    pub disk_guid: [u8; 16],
    /// The first LBA of this header's partition table.
    pub entries_lba: u64,
    /// How many entries the table holds, used or not.
    pub entry_count: u32,
    /// Bytes per entry.
    pub entry_size: u32,
    /// The CRC-32 of the whole table, as stored.
    pub entries_crc: u32,
}

impl Header {
    /// Validates the header in `sector`, which was read from `expected_lba` on a disk of
    /// `disk_sectors` blocks. The header CRC is checked with its own field zeroed, as the
    /// specification requires, and every check runs before any field is trusted for a read.
    ///
    /// # Errors
    ///
    /// Returns the first rule the header breaks, in the order the checks are listed on
    /// [`GptError`].
    pub fn parse(sector: &[u8], expected_lba: u64, disk_sectors: u64) -> Result<Self, GptError> {
        if sector.len() < HEADER_MIN_SIZE {
            return Err(GptError::TooShort);
        }
        if sector[..8] != SIGNATURE {
            return Err(GptError::BadSignature);
        }
        if le32(sector, 8) != REVISION_1_0 {
            return Err(GptError::BadRevision);
        }
        let size = le32(sector, 12) as usize;
        if !(HEADER_MIN_SIZE..=sector.len()).contains(&size) {
            return Err(GptError::BadHeaderSize);
        }
        // The CRC covers the header with its own four bytes read as zero.
        let computed = !crc_update(
            crc_update(crc_update(0xFFFF_FFFF, &sector[..16]), &[0; 4]),
            &sector[20..size],
        );
        if computed != le32(sector, 16) {
            return Err(GptError::BadHeaderCrc);
        }
        if le32(sector, 20) != 0 {
            return Err(GptError::BadReserved);
        }
        let my_lba = le64(sector, 24);
        if my_lba != expected_lba {
            return Err(GptError::BadLocation);
        }
        let entry_count = le32(sector, 80);
        let entry_size = le32(sector, 84);
        if (entry_size as usize) < ENTRY_MIN_SIZE || !entry_size.is_power_of_two() {
            return Err(GptError::BadEntrySize);
        }
        let table_bytes = u64::from(entry_count) * u64::from(entry_size);
        if table_bytes > MAX_ENTRY_BYTES as u64 {
            return Err(GptError::TooManyEntries);
        }
        let table_sectors = table_bytes.div_ceil(SECTOR_SIZE as u64);
        let entries_lba = le64(sector, 72);
        let first_usable = le64(sector, 40);
        let last_usable = le64(sector, 48);
        let alternate_lba = le64(sector, 32);
        // A primary table lies before the usable area and a backup table after it. Either way
        // the table must be on the disk, and the usable area must be non-empty and on the disk.
        let table_end = entries_lba.saturating_add(table_sectors);
        let before_usable = table_end <= first_usable;
        let after_usable = entries_lba > last_usable;
        if entries_lba < FIRST_TABLE_LBA
            || !(before_usable || after_usable)
            || table_end > disk_sectors
            || first_usable > last_usable
            || last_usable >= disk_sectors
            || alternate_lba >= disk_sectors
        {
            return Err(GptError::BadUsableRange);
        }
        let mut disk_guid = [0u8; 16];
        disk_guid.copy_from_slice(&sector[56..72]);
        Ok(Self {
            my_lba,
            alternate_lba,
            first_usable,
            last_usable,
            disk_guid,
            entries_lba,
            entry_count,
            entry_size,
            entries_crc: le32(sector, 88),
        })
    }

    /// The table's length in bytes.
    #[must_use]
    pub fn table_bytes(&self) -> usize {
        self.entry_count as usize * self.entry_size as usize
    }
}

/// A partition entry in use: the fields a reader needs to find its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// The partition type GUID. All zeros marks an unused entry.
    pub type_guid: [u8; 16],
    /// The partition's own GUID.
    pub unique_guid: [u8; 16],
    /// The first LBA of the partition.
    pub first_lba: u64,
    /// The last LBA of the partition, inclusive.
    pub last_lba: u64,
    /// The attribute flags, as stored.
    pub attributes: u64,
    /// The name, UTF-16 code units, zero-padded.
    pub name: [u16; 36],
}

impl Partition {
    /// Reads the first 128 bytes of an entry. Larger entries carry extra bytes this crate
    /// does not interpret, so only the leading 128 are read.
    #[must_use]
    pub fn parse(entry: &[u8]) -> Option<Self> {
        if entry.len() < ENTRY_MIN_SIZE {
            return None;
        }
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&entry[..16]);
        let mut unique_guid = [0u8; 16];
        unique_guid.copy_from_slice(&entry[16..32]);
        let mut name = [0u16; 36];
        for (unit, pair) in name.iter_mut().zip(entry[56..128].chunks_exact(2)) {
            *unit = u16::from_le_bytes([pair[0], pair[1]]);
        }
        Some(Self {
            type_guid,
            unique_guid,
            first_lba: le64(entry, 32),
            last_lba: le64(entry, 40),
            attributes: le64(entry, 48),
            name,
        })
    }

    /// Whether the entry is unused: its type GUID is all zeros.
    #[must_use]
    pub fn is_unused(&self) -> bool {
        self.type_guid == [0; 16]
    }

    /// The number of blocks the partition covers.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// Checks the table's CRC-32 against the header. `entries` is the table as read from
/// `header.entries_lba`; only its first [`Header::table_bytes`] bytes are used.
///
/// # Errors
///
/// [`GptError::EntriesTooShort`] if `entries` does not hold the whole table, and
/// [`GptError::BadEntriesCrc`] if the CRC disagrees.
pub fn verify_table(header: &Header, entries: &[u8]) -> Result<(), GptError> {
    let bytes = header.table_bytes();
    if entries.len() < bytes {
        return Err(GptError::EntriesTooShort);
    }
    if crc32(&entries[..bytes]) != header.entries_crc {
        return Err(GptError::BadEntriesCrc);
    }
    Ok(())
}

/// The entry at `index`, unused or not, or `None` past the end of the table.
#[must_use]
pub fn entry(header: &Header, entries: &[u8], index: u32) -> Option<Partition> {
    if index >= header.entry_count {
        return None;
    }
    let size = header.entry_size as usize;
    let start = index as usize * size;
    Partition::parse(entries.get(start..start + size)?)
}

/// Checks every used entry: first no later than last, inside the usable area, and not sharing
/// a block with an earlier used entry. Returns how many entries are in use.
///
/// The table must already have passed [`verify_table`].
///
/// # Errors
///
/// The first partition that breaks a rule, as [`GptError::PartitionInverted`],
/// [`GptError::PartitionOutOfRange`], or [`GptError::PartitionsOverlap`].
pub fn check_partitions(header: &Header, entries: &[u8]) -> Result<u32, GptError> {
    let mut used = 0;
    for index in 0..header.entry_count {
        let Some(part) = entry(header, entries, index) else {
            break;
        };
        if part.is_unused() {
            continue;
        }
        used += 1;
        if part.first_lba > part.last_lba {
            return Err(GptError::PartitionInverted);
        }
        if part.first_lba < header.first_usable || part.last_lba > header.last_usable {
            return Err(GptError::PartitionOutOfRange);
        }
        for earlier in 0..index {
            let Some(other) = entry(header, entries, earlier) else {
                break;
            };
            if !other.is_unused()
                && part.first_lba <= other.last_lba
                && other.first_lba <= part.last_lba
            {
                return Err(GptError::PartitionsOverlap);
            }
        }
    }
    Ok(used)
}

pub mod queue;

#[cfg(test)]
mod tests;
