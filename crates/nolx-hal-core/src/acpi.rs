//! Allocation-free parsers for the ACPI records needed by the early x86-64 HAL.
//!
//! The kernel owns the physical-memory reads. This module only validates byte slices, so
//! firmware-table layouts and malformed-input handling can be exercised by host tests.

const SDT_HEADER_LEN: usize = 36;
const RSDP_V1_LEN: usize = 20;
const RSDP_V2_MIN_LEN: usize = 36;
const MADT_FIXED_LEN: usize = 44;
const HPET_MIN_LEN: usize = 56;

/// Maximum local-processor records retained from a MADT.
pub const MAX_LOCAL_APICS: usize = 256;
/// Maximum local-APIC NMI-routing records retained from a MADT.
pub const MAX_LOCAL_APIC_NMIS: usize = 32;
/// Maximum I/O APIC non-maskable interrupt sources retained from a MADT.
pub const MAX_IO_APIC_NMIS: usize = 16;
/// Maximum I/O APIC records retained from a MADT.
pub const MAX_IO_APICS: usize = 16;
/// Maximum interrupt-source overrides retained from a MADT.
pub const MAX_INTERRUPT_OVERRIDES: usize = 64;

/// Failure encountered while validating or decoding an ACPI structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiError {
    /// The supplied slice ended before a required field or declared structure length.
    Truncated,
    /// The requested or expected ACPI signature did not match.
    BadSignature,
    /// An ACPI checksum did not sum to zero modulo 256.
    BadChecksum,
    /// A length was too small, too large, or inconsistent with its entry width.
    InvalidLength,
    /// A variable-length MADT entry was malformed.
    InvalidEntry,
    /// A parsed MADT exceeded a fixed-capacity record array.
    TooManyEntries,
    /// The supplied SDT was not an RSDT or XSDT.
    NotRootTable,
}

/// ACPI Root System Description Pointer, with both legacy and extended root addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    /// ACPI revision from the RSDP.
    pub revision: u8,
    /// Physical address of the RSDT, when present.
    pub rsdt_address: u32,
    /// Physical address of the XSDT; zero for revision 0/1 or when absent.
    pub xsdt_address: u64,
    /// Validated byte length of the RSDP (20 for revision 0/1).
    pub length: u32,
}

impl Rsdp {
    /// Parse and checksum an RSDP from a byte slice beginning at its signature.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        if bytes.len() < RSDP_V1_LEN {
            return Err(AcpiError::Truncated);
        }
        if bytes.get(..8) != Some(&b"RSD PTR "[..]) {
            return Err(AcpiError::BadSignature);
        }
        if checksum(&bytes[..RSDP_V1_LEN]) != 0 {
            return Err(AcpiError::BadChecksum);
        }

        let revision = bytes[15];
        let rsdt_address = read_u32(bytes, 16)?;
        if revision < 2 {
            return Ok(Self {
                revision,
                rsdt_address,
                xsdt_address: 0,
                length: RSDP_V1_LEN as u32,
            });
        }

        if bytes.len() < RSDP_V2_MIN_LEN {
            return Err(AcpiError::Truncated);
        }
        let length = read_u32(bytes, 20)? as usize;
        if !(RSDP_V2_MIN_LEN..=bytes.len()).contains(&length) {
            return Err(AcpiError::InvalidLength);
        }
        if checksum(&bytes[..length]) != 0 {
            return Err(AcpiError::BadChecksum);
        }

        Ok(Self {
            revision,
            rsdt_address,
            xsdt_address: read_u64(bytes, 24)?,
            length: length as u32,
        })
    }
}

/// A validated ACPI System Description Table view.
#[derive(Clone, Copy, Debug)]
pub struct Sdt<'a> {
    bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    /// Validate the header, optional signature, declared length, and checksum of an SDT.
    pub fn parse(bytes: &'a [u8], expected_signature: Option<[u8; 4]>) -> Result<Self, AcpiError> {
        if bytes.len() < SDT_HEADER_LEN {
            return Err(AcpiError::Truncated);
        }
        let signature = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if expected_signature.is_some_and(|expected| expected != signature) {
            return Err(AcpiError::BadSignature);
        }
        let length = read_u32(bytes, 4)? as usize;
        if !(SDT_HEADER_LEN..=bytes.len()).contains(&length) {
            return Err(AcpiError::InvalidLength);
        }
        let bytes = &bytes[..length];
        if checksum(bytes) != 0 {
            return Err(AcpiError::BadChecksum);
        }
        Ok(Self { bytes })
    }

    /// Four-byte SDT signature, such as `APIC`, `HPET`, `RSDT`, or `XSDT`.
    #[must_use]
    pub fn signature(&self) -> [u8; 4] {
        [self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]]
    }

    /// SDT revision byte.
    #[must_use]
    pub fn revision(&self) -> u8 {
        self.bytes[8]
    }

    /// The validated table bytes, excluding any bytes after the declared table length.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Payload after the fixed ACPI SDT header.
    #[must_use]
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[SDT_HEADER_LEN..]
    }

    /// Iterate 32-bit physical addresses from an RSDT or 64-bit addresses from an XSDT.
    pub fn root_entries(&self) -> Result<RootEntries<'a>, AcpiError> {
        let signature = self.signature();
        let entry_width = if signature == *b"RSDT" {
            4
        } else if signature == *b"XSDT" {
            8
        } else {
            return Err(AcpiError::NotRootTable);
        };
        let entries = self.payload();
        if entries.len() & (entry_width - 1) != 0 {
            return Err(AcpiError::InvalidLength);
        }
        Ok(RootEntries {
            bytes: entries,
            entry_width,
            cursor: 0,
        })
    }
}

/// Iterator over the validated physical addresses in an RSDT or XSDT.
#[derive(Clone, Debug)]
pub struct RootEntries<'a> {
    bytes: &'a [u8],
    entry_width: usize,
    cursor: usize,
}

impl Iterator for RootEntries<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.cursor;
        let end = start.checked_add(self.entry_width)?;
        let bytes = self.bytes.get(start..end)?;
        self.cursor = end;
        Some(if self.entry_width == 4 {
            u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        } else {
            u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.bytes.len().saturating_sub(self.cursor) / self.entry_width;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for RootEntries<'_> {}

/// Processor entry from a MADT, supporting both legacy and x2APIC entry formats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalApic {
    /// ACPI processor UID.
    pub processor_uid: u32,
    /// Local APIC or x2APIC ID.
    pub apic_id: u32,
    /// MADT flags; bit 0 means enabled and bit 1 means online-capable.
    pub flags: u32,
    /// `true` when decoded from the x2APIC MADT entry (type 9).
    pub x2apic: bool,
}

impl LocalApic {
    /// Whether firmware marks this processor enabled.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Whether firmware marks this processor as online-capable.
    #[must_use]
    pub const fn online_capable(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Local-APIC LINT input configured as an NMI by a MADT type-4 or type-10 record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalApicNmi {
    /// Processor UID; `0xFF` (legacy) or `u32::MAX` (x2APIC) applies to all processors.
    pub processor_uid: u32,
    /// ACPI polarity and trigger flags, preserved in their encoded form.
    pub flags: u16,
    /// LINT input number, zero or one.
    pub lint: u8,
    /// `true` when decoded from the x2APIC NMI entry (type 10).
    pub x2apic: bool,
}

impl LocalApicNmi {
    /// Whether this NMI-routing record applies to the requested processor UID.
    #[must_use]
    pub const fn applies_to(&self, processor_uid: u32) -> bool {
        self.processor_uid == processor_uid
            || (!self.x2apic && self.processor_uid == 0xFF)
            || (self.x2apic && self.processor_uid == u32::MAX)
    }
}

/// I/O APIC location and first global-system-interrupt number from a MADT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoApic {
    /// I/O APIC hardware ID.
    pub id: u8,
    /// Physical MMIO base address.
    pub address: u32,
    /// First global system interrupt handled by this I/O APIC.
    pub global_interrupt_base: u32,
}

/// ISA interrupt source override from a MADT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptOverride {
    /// Bus number (zero denotes the legacy ISA bus).
    pub bus: u8,
    /// Source IRQ number on `bus`.
    pub source: u8,
    /// Destination global system interrupt number.
    pub global_interrupt: u32,
    /// ACPI polarity and trigger flags, preserved in their encoded form.
    pub flags: u16,
}

/// I/O APIC global system interrupt line configured for non-maskable delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoApicNmiSource {
    /// Destination global system interrupt number.
    pub global_interrupt: u32,
    /// ACPI polarity and trigger flags, preserved in their encoded form.
    pub flags: u16,
}

/// Decoded Multiple APIC Description Table (MADT, signature `APIC`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Madt {
    /// Physical address of the local APIC, including any type-5 override.
    pub local_apic_address: u64,
    /// MADT flags; bit 0 indicates PC/AT-compatible dual 8259 PICs.
    pub flags: u32,
    local_apics: [LocalApic; MAX_LOCAL_APICS],
    local_apic_count: usize,
    local_apic_nmis: [LocalApicNmi; MAX_LOCAL_APIC_NMIS],
    local_apic_nmi_count: usize,
    io_apic_nmi_sources: [IoApicNmiSource; MAX_IO_APIC_NMIS],
    io_apic_nmi_count: usize,
    io_apics: [IoApic; MAX_IO_APICS],
    io_apic_count: usize,
    overrides: [InterruptOverride; MAX_INTERRUPT_OVERRIDES],
    override_count: usize,
}

impl Madt {
    /// Parse MADT records, rejecting malformed entries and capacity overflow.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let table = Sdt::parse(bytes, Some(*b"APIC"))?;
        let bytes = table.bytes();
        if bytes.len() < MADT_FIXED_LEN {
            return Err(AcpiError::InvalidLength);
        }

        let mut result = Self {
            local_apic_address: u64::from(read_u32(bytes, 36)?),
            flags: read_u32(bytes, 40)?,
            local_apics: [LocalApic::default(); MAX_LOCAL_APICS],
            local_apic_count: 0,
            local_apic_nmis: [LocalApicNmi::default(); MAX_LOCAL_APIC_NMIS],
            local_apic_nmi_count: 0,
            io_apic_nmi_sources: [IoApicNmiSource::default(); MAX_IO_APIC_NMIS],
            io_apic_nmi_count: 0,
            io_apics: [IoApic::default(); MAX_IO_APICS],
            io_apic_count: 0,
            overrides: [InterruptOverride::default(); MAX_INTERRUPT_OVERRIDES],
            override_count: 0,
        };

        let mut cursor = MADT_FIXED_LEN;
        while cursor < bytes.len() {
            let header_end = cursor.checked_add(2).ok_or(AcpiError::InvalidEntry)?;
            let entry_header = bytes
                .get(cursor..header_end)
                .ok_or(AcpiError::InvalidEntry)?;
            let entry_type = entry_header[0];
            let entry_length = usize::from(entry_header[1]);
            if entry_length < 2 {
                return Err(AcpiError::InvalidEntry);
            }
            let end = cursor
                .checked_add(entry_length)
                .filter(|&end| end <= bytes.len())
                .ok_or(AcpiError::InvalidEntry)?;
            let entry = &bytes[cursor..end];
            match entry_type {
                0 => {
                    require_entry_length(entry, 8)?;
                    push_local_apic(
                        &mut result,
                        LocalApic {
                            processor_uid: u32::from(entry[2]),
                            apic_id: u32::from(entry[3]),
                            flags: read_u32(entry, 4)?,
                            x2apic: false,
                        },
                    )?;
                }
                1 => {
                    require_entry_length(entry, 12)?;
                    push_io_apic(
                        &mut result,
                        IoApic {
                            id: entry[2],
                            address: read_u32(entry, 4)?,
                            global_interrupt_base: read_u32(entry, 8)?,
                        },
                    )?;
                }
                2 => {
                    require_entry_length(entry, 10)?;
                    push_override(
                        &mut result,
                        InterruptOverride {
                            bus: entry[2],
                            source: entry[3],
                            global_interrupt: read_u32(entry, 4)?,
                            flags: read_u16(entry, 8)?,
                        },
                    )?;
                }
                3 => {
                    require_entry_length(entry, 8)?;
                    push_io_apic_nmi(
                        &mut result,
                        IoApicNmiSource {
                            flags: read_u16(entry, 2)?,
                            global_interrupt: read_u32(entry, 4)?,
                        },
                    )?;
                }
                4 => {
                    require_entry_length(entry, 6)?;
                    push_local_apic_nmi(
                        &mut result,
                        LocalApicNmi {
                            processor_uid: u32::from(entry[2]),
                            flags: read_u16(entry, 3)?,
                            lint: entry[5],
                            x2apic: false,
                        },
                    )?;
                }
                5 => {
                    require_entry_length(entry, 12)?;
                    result.local_apic_address = read_u64(entry, 4)?;
                }
                9 => {
                    require_entry_length(entry, 16)?;
                    push_local_apic(
                        &mut result,
                        LocalApic {
                            processor_uid: read_u32(entry, 12)?,
                            apic_id: read_u32(entry, 4)?,
                            flags: read_u32(entry, 8)?,
                            x2apic: true,
                        },
                    )?;
                }
                10 => {
                    require_entry_length(entry, 12)?;
                    push_local_apic_nmi(
                        &mut result,
                        LocalApicNmi {
                            processor_uid: read_u32(entry, 4)?,
                            flags: read_u16(entry, 2)?,
                            lint: entry[8],
                            x2apic: true,
                        },
                    )?;
                }
                // Unknown MADT records are safely skipped by their validated length.
                _ => {}
            }
            cursor = end;
        }
        Ok(result)
    }

    /// Processor records, including disabled and online-capable processors.
    #[must_use]
    pub fn local_apics(&self) -> &[LocalApic] {
        &self.local_apics[..self.local_apic_count]
    }

    /// Local-APIC NMI LINT settings in table order.
    #[must_use]
    pub fn local_apic_nmis(&self) -> &[LocalApicNmi] {
        &self.local_apic_nmis[..self.local_apic_nmi_count]
    }

    /// I/O APIC NMI sources in table order.
    #[must_use]
    pub fn io_apic_nmi_sources(&self) -> &[IoApicNmiSource] {
        &self.io_apic_nmi_sources[..self.io_apic_nmi_count]
    }

    /// I/O APIC records in table order.
    #[must_use]
    pub fn io_apics(&self) -> &[IoApic] {
        &self.io_apics[..self.io_apic_count]
    }

    /// Legacy bus interrupt-source overrides in table order.
    #[must_use]
    pub fn interrupt_overrides(&self) -> &[InterruptOverride] {
        &self.overrides[..self.override_count]
    }

    /// `true` if the platform advertises PC/AT-compatible 8259 PICs.
    #[must_use]
    pub const fn has_legacy_pic(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Generic Address Structure used by the ACPI HPET table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenericAddress {
    /// Address-space ID (zero is system memory).
    pub address_space_id: u8,
    /// Register width in bits.
    pub bit_width: u8,
    /// Bit offset within the register.
    pub bit_offset: u8,
    /// Access-size encoding from ACPI.
    pub access_size: u8,
    /// Physical address of the register block.
    pub address: u64,
}

/// Parsed ACPI HPET table fields needed to initialize the high-precision timer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HpetInfo {
    /// Hardware revision from the event-timer-block ID.
    pub hardware_revision: u8,
    /// Number of comparators reported by the event-timer-block ID.
    pub comparator_count: u8,
    /// Whether the main counter supports 64-bit mode.
    pub counter_64bit: bool,
    /// Whether legacy replacement routing is supported.
    pub legacy_replacement: bool,
    /// PCI vendor ID from the event-timer-block ID.
    pub vendor_id: u16,
    /// Register-block address description.
    pub base: GenericAddress,
    /// HPET number assigned by firmware.
    pub number: u8,
    /// Minimum clock tick in femtoseconds reported by firmware.
    pub minimum_tick: u16,
    /// Page-protection encoding from the ACPI HPET table.
    pub page_protection: u8,
}

impl HpetInfo {
    /// Parse the ACPI HPET table (signature `HPET`).
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let table = Sdt::parse(bytes, Some(*b"HPET"))?;
        let bytes = table.bytes();
        if bytes.len() < HPET_MIN_LEN {
            return Err(AcpiError::InvalidLength);
        }
        let block_id = read_u32(bytes, 36)?;
        Ok(Self {
            hardware_revision: block_id as u8,
            comparator_count: (((block_id >> 8) & 0x1F) + 1) as u8,
            counter_64bit: block_id & (1 << 13) != 0,
            legacy_replacement: block_id & (1 << 14) != 0,
            vendor_id: (block_id >> 16) as u16,
            base: GenericAddress {
                address_space_id: bytes[40],
                bit_width: bytes[41],
                bit_offset: bytes[42],
                access_size: bytes[43],
                address: read_u64(bytes, 44)?,
            },
            number: bytes[52],
            minimum_tick: read_u16(bytes, 53)?,
            page_protection: bytes[55],
        })
    }
}

/// Sum of an ACPI checksum-protected byte range, using 8-bit wrapping arithmetic.
#[must_use]
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte))
}

fn require_entry_length(entry: &[u8], minimum: usize) -> Result<(), AcpiError> {
    if entry.len() < minimum {
        Err(AcpiError::InvalidEntry)
    } else {
        Ok(())
    }
}

fn push_local_apic(madt: &mut Madt, entry: LocalApic) -> Result<(), AcpiError> {
    let slot = madt
        .local_apics
        .get_mut(madt.local_apic_count)
        .ok_or(AcpiError::TooManyEntries)?;
    *slot = entry;
    madt.local_apic_count += 1;
    Ok(())
}

fn push_local_apic_nmi(madt: &mut Madt, entry: LocalApicNmi) -> Result<(), AcpiError> {
    if entry.lint > 1 {
        return Err(AcpiError::InvalidEntry);
    }
    let slot = madt
        .local_apic_nmis
        .get_mut(madt.local_apic_nmi_count)
        .ok_or(AcpiError::TooManyEntries)?;
    *slot = entry;
    madt.local_apic_nmi_count += 1;
    Ok(())
}

fn push_io_apic_nmi(madt: &mut Madt, entry: IoApicNmiSource) -> Result<(), AcpiError> {
    let slot = madt
        .io_apic_nmi_sources
        .get_mut(madt.io_apic_nmi_count)
        .ok_or(AcpiError::TooManyEntries)?;
    *slot = entry;
    madt.io_apic_nmi_count += 1;
    Ok(())
}

fn push_io_apic(madt: &mut Madt, entry: IoApic) -> Result<(), AcpiError> {
    let slot = madt
        .io_apics
        .get_mut(madt.io_apic_count)
        .ok_or(AcpiError::TooManyEntries)?;
    *slot = entry;
    madt.io_apic_count += 1;
    Ok(())
}

fn push_override(madt: &mut Madt, entry: InterruptOverride) -> Result<(), AcpiError> {
    let slot = madt
        .overrides
        .get_mut(madt.override_count)
        .ok_or(AcpiError::TooManyEntries)?;
    *slot = entry;
    madt.override_count += 1;
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, AcpiError> {
    let field = bytes
        .get(offset..offset.checked_add(2).ok_or(AcpiError::Truncated)?)
        .ok_or(AcpiError::Truncated)?;
    Ok(u16::from_le_bytes([field[0], field[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AcpiError> {
    let field = bytes
        .get(offset..offset.checked_add(4).ok_or(AcpiError::Truncated)?)
        .ok_or(AcpiError::Truncated)?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, AcpiError> {
    let field = bytes
        .get(offset..offset.checked_add(8).ok_or(AcpiError::Truncated)?)
        .ok_or(AcpiError::Truncated)?;
    Ok(u64::from_le_bytes([
        field[0], field[1], field[2], field[3], field[4], field[5], field[6], field[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish_sdt(mut bytes: Vec<u8>) -> Vec<u8> {
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes[9] = 0;
        let sum = checksum(&bytes);
        bytes[9] = 0u8.wrapping_sub(sum);
        bytes
    }

    fn append_madt_entry(table: &mut Vec<u8>, entry: &[u8]) {
        table.extend_from_slice(entry);
    }

    #[test]
    fn rsdp_v1_and_v2_signatures_lengths_and_checksums_are_checked() {
        let mut v1 = vec![0u8; RSDP_V1_LEN];
        v1[..8].copy_from_slice(b"RSD PTR ");
        v1[15] = 0;
        v1[16..20].copy_from_slice(&0x1234_5000u32.to_le_bytes());
        let sum = checksum(&v1);
        v1[8] = 0u8.wrapping_sub(sum);
        assert_eq!(
            Rsdp::parse(&v1),
            Ok(Rsdp {
                revision: 0,
                rsdt_address: 0x1234_5000,
                xsdt_address: 0,
                length: 20,
            })
        );

        let mut v2 = vec![0u8; RSDP_V2_MIN_LEN];
        v2[..8].copy_from_slice(b"RSD PTR ");
        v2[15] = 2;
        v2[16..20].copy_from_slice(&0x1234_5000u32.to_le_bytes());
        v2[20..24].copy_from_slice(&(RSDP_V2_MIN_LEN as u32).to_le_bytes());
        v2[24..32].copy_from_slice(&0x1_2345_6789u64.to_le_bytes());
        let base_sum = checksum(&v2[..RSDP_V1_LEN]);
        v2[8] = 0u8.wrapping_sub(base_sum);
        let extended_sum = checksum(&v2);
        v2[32] = 0u8.wrapping_sub(extended_sum);
        assert_eq!(
            Rsdp::parse(&v2),
            Ok(Rsdp {
                revision: 2,
                rsdt_address: 0x1234_5000,
                xsdt_address: 0x1_2345_6789,
                length: 36,
            })
        );

        v2[10] ^= 1;
        assert_eq!(Rsdp::parse(&v2), Err(AcpiError::BadChecksum));
    }

    #[test]
    fn sdt_root_entries_decode_both_widths_and_reject_bad_tables() {
        let mut rsdt = vec![0u8; SDT_HEADER_LEN];
        rsdt[..4].copy_from_slice(b"RSDT");
        rsdt.extend_from_slice(&[0x00, 0x10, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00]);
        let rsdt = finish_sdt(rsdt);
        let parsed = Sdt::parse(&rsdt, Some(*b"RSDT")).unwrap();
        assert_eq!(
            parsed.root_entries().unwrap().collect::<Vec<_>>(),
            vec![0x1000, 0x2000]
        );

        let mut xsdt = vec![0u8; SDT_HEADER_LEN];
        xsdt[..4].copy_from_slice(b"XSDT");
        xsdt.extend_from_slice(&0x1_2345_6789u64.to_le_bytes());
        let xsdt = finish_sdt(xsdt);
        let parsed = Sdt::parse(&xsdt, Some(*b"XSDT")).unwrap();
        assert_eq!(
            parsed.root_entries().unwrap().collect::<Vec<_>>(),
            vec![0x1_2345_6789]
        );

        assert_eq!(
            Sdt::parse(&rsdt, Some(*b"APIC")).err(),
            Some(AcpiError::BadSignature)
        );
        let mut corrupt = rsdt.clone();
        corrupt[SDT_HEADER_LEN] ^= 1;
        assert_eq!(
            Sdt::parse(&corrupt, None).err(),
            Some(AcpiError::BadChecksum)
        );
    }

    #[test]
    fn madt_decodes_legacy_x2apic_ioapic_overrides_and_address_override() {
        let mut table = vec![0u8; MADT_FIXED_LEN];
        table[..4].copy_from_slice(b"APIC");
        table[36..40].copy_from_slice(&0xFEE0_0000u32.to_le_bytes());
        table[40..44].copy_from_slice(&1u32.to_le_bytes());

        append_madt_entry(&mut table, &[0, 8, 7, 9, 1, 0, 0, 0]);
        append_madt_entry(&mut table, &[3, 8, 0, 0, 7, 0, 0, 0]);
        append_madt_entry(&mut table, &[4, 6, 0xFF, 0, 0, 1]);
        let mut io_apic = [0u8; 12];
        io_apic[0..2].copy_from_slice(&[1, 12]);
        io_apic[2] = 2;
        io_apic[4..8].copy_from_slice(&0xFEC0_0000u32.to_le_bytes());
        io_apic[8..12].copy_from_slice(&24u32.to_le_bytes());
        append_madt_entry(&mut table, &io_apic);
        let mut source_override = [0u8; 10];
        source_override[0..2].copy_from_slice(&[2, 10]);
        source_override[2] = 0;
        source_override[3] = 0;
        source_override[4..8].copy_from_slice(&2u32.to_le_bytes());
        source_override[8..10].copy_from_slice(&0x000Fu16.to_le_bytes());
        append_madt_entry(&mut table, &source_override);
        let mut address_override = [0u8; 12];
        address_override[0..2].copy_from_slice(&[5, 12]);
        address_override[4..12].copy_from_slice(&0x1_FEE0_0000u64.to_le_bytes());
        append_madt_entry(&mut table, &address_override);
        let mut x2_apic = [0u8; 16];
        x2_apic[0..2].copy_from_slice(&[9, 16]);
        x2_apic[4..8].copy_from_slice(&0x1234u32.to_le_bytes());
        x2_apic[8..12].copy_from_slice(&3u32.to_le_bytes());
        x2_apic[12..16].copy_from_slice(&0x55u32.to_le_bytes());
        append_madt_entry(&mut table, &x2_apic);
        append_madt_entry(&mut table, &[10, 12, 0x0F, 0, 0x55, 0, 0, 0, 1, 0, 0, 0]);

        let table = finish_sdt(table);
        let madt = Madt::parse(&table).unwrap();
        assert_eq!(madt.local_apic_address, 0x1_FEE0_0000);
        assert!(madt.has_legacy_pic());
        assert_eq!(madt.local_apics().len(), 2);
        assert_eq!(
            madt.local_apics()[0],
            LocalApic {
                processor_uid: 7,
                apic_id: 9,
                flags: 1,
                x2apic: false,
            }
        );
        assert!(madt.local_apics()[0].enabled());
        assert_eq!(madt.local_apics()[1].processor_uid, 0x55);
        assert_eq!(madt.local_apics()[1].apic_id, 0x1234);
        assert!(madt.local_apics()[1].online_capable());
        assert!(madt.local_apics()[1].x2apic);
        assert_eq!(madt.local_apic_nmis().len(), 2);
        assert!(madt.local_apic_nmis()[0].applies_to(0x55));
        assert_eq!(madt.local_apic_nmis()[0].lint, 1);
        assert!(madt.local_apic_nmis()[1].applies_to(0x55));
        assert_eq!(madt.local_apic_nmis()[1].flags, 0x000F);
        assert_eq!(madt.local_apic_nmis()[1].lint, 1);
        assert!(madt.local_apic_nmis()[1].x2apic);
        assert_eq!(
            madt.io_apic_nmi_sources(),
            &[IoApicNmiSource {
                global_interrupt: 7,
                flags: 0,
            }]
        );
        assert_eq!(
            madt.io_apics(),
            &[IoApic {
                id: 2,
                address: 0xFEC0_0000,
                global_interrupt_base: 24,
            }]
        );
        assert_eq!(
            madt.interrupt_overrides(),
            &[InterruptOverride {
                bus: 0,
                source: 0,
                global_interrupt: 2,
                flags: 0x000F,
            }]
        );
    }

    #[test]
    fn malformed_madt_entries_are_rejected_instead_of_read_past_the_table() {
        let mut table = vec![0u8; MADT_FIXED_LEN];
        table[..4].copy_from_slice(b"APIC");
        table.extend_from_slice(&[0, 1]);
        let table = finish_sdt(table);
        assert_eq!(Madt::parse(&table), Err(AcpiError::InvalidEntry));

        let mut table = vec![0u8; MADT_FIXED_LEN];
        table[..4].copy_from_slice(b"APIC");
        table.extend_from_slice(&[0, 8, 0, 0]);
        let table = finish_sdt(table);
        assert_eq!(Madt::parse(&table), Err(AcpiError::InvalidEntry));
    }

    #[test]
    fn hpet_table_decodes_gas_capabilities_and_timing_fields() {
        let mut table = vec![0u8; HPET_MIN_LEN];
        table[..4].copy_from_slice(b"HPET");
        let block_id = 0x8086u32 << 16 | (1 << 14) | (1 << 13) | (2 << 8) | 3;
        table[36..40].copy_from_slice(&block_id.to_le_bytes());
        table[40..44].copy_from_slice(&[0, 64, 0, 0]);
        table[44..52].copy_from_slice(&0xFED0_0000u64.to_le_bytes());
        table[52] = 0;
        table[53..55].copy_from_slice(&0x80u16.to_le_bytes());
        table[55] = 0;
        let table = finish_sdt(table);
        assert_eq!(
            HpetInfo::parse(&table),
            Ok(HpetInfo {
                hardware_revision: 3,
                comparator_count: 3,
                counter_64bit: true,
                legacy_replacement: true,
                vendor_id: 0x8086,
                base: GenericAddress {
                    address_space_id: 0,
                    bit_width: 64,
                    bit_offset: 0,
                    access_size: 0,
                    address: 0xFED0_0000,
                },
                number: 0,
                minimum_tick: 0x80,
                page_protection: 0,
            })
        );
    }
}
