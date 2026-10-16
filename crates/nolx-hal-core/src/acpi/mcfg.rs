//! ACPI MCFG allocation decoding (PCI Firmware Specification).
//!
//! Malformed or ambiguous tables are rejected as a whole, not partially retained.

use super::{read_u16, read_u64, AcpiError, Sdt};

/// Maximum allocations retained without a heap.
pub const MAX_ALLOCATIONS: usize = 32;

/// One PCI segment's inclusive bus range and bus-zero-relative ECAM base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allocation {
    /// Physical ECAM base corresponding to bus zero.
    pub base: u64,
    /// PCI segment group number.
    pub segment: u16,
    /// First bus covered by this allocation.
    pub start_bus: u8,
    /// Last bus covered by this allocation.
    pub end_bus: u8,
}

impl Allocation {
    fn first(self) -> u64 {
        self.base + (u64::from(self.start_bus) << 20)
    }

    fn last(self) -> u64 {
        self.base + (u64::from(self.end_bus) << 20) + 0xf_ffff
    }
}

/// Validated, fixed-capacity MCFG table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mcfg {
    allocations: [Allocation; MAX_ALLOCATIONS],
    len: usize,
}

impl Mcfg {
    /// Validate the SDT checksum, fixed header and 16-byte allocation records.
    ///
    /// Rejects reversed ranges, unaligned bases, arithmetic overflow, overlapping
    /// bus ranges within a segment, physical aliases, and capacity exhaustion.
    /// Reserved fields are ignored for compatibility with future firmware revisions.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let table = Sdt::parse(bytes, Some(*b"MCFG"))?;
        let payload = table.payload();
        if payload.len() < 8 || !(payload.len() - 8).is_multiple_of(16) {
            return Err(AcpiError::InvalidLength);
        }
        let count = (payload.len() - 8) / 16;
        if count > MAX_ALLOCATIONS {
            return Err(AcpiError::TooManyEntries);
        }
        let mut result = Self {
            allocations: [Allocation::default(); MAX_ALLOCATIONS],
            len: 0,
        };
        for record in payload[8..].chunks_exact(16) {
            let allocation = Allocation {
                base: read_u64(record, 0)?,
                segment: read_u16(record, 8)?,
                start_bus: record[10],
                end_bus: record[11],
            };
            let last_offset = (u64::from(allocation.end_bus) << 20) | 0xf_ffff;
            if allocation.base & 0xf_ffff != 0
                || allocation.start_bus > allocation.end_bus
                || allocation.base.checked_add(last_offset).is_none()
            {
                return Err(AcpiError::InvalidEntry);
            }
            for previous in result.allocations() {
                let bus_overlap = previous.segment == allocation.segment
                    && previous.start_bus <= allocation.end_bus
                    && allocation.start_bus <= previous.end_bus;
                let physical_overlap = previous.first() <= allocation.last()
                    && allocation.first() <= previous.last();
                if bus_overlap || physical_overlap {
                    return Err(AcpiError::InvalidEntry);
                }
            }
            result.allocations[result.len] = allocation;
            result.len += 1;
        }
        Ok(result)
    }

    /// The complete validated allocation list, in firmware order.
    pub fn allocations(&self) -> &[Allocation] {
        &self.allocations[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::checksum;

    fn repair(bytes: &mut [u8]) {
        bytes[9] = 0;
        bytes[9] = 0u8.wrapping_sub(checksum(bytes));
    }

    fn table(records: &[(u64, u16, u8, u8)]) -> Vec<u8> {
        let mut bytes = vec![0; 44 + 16 * records.len()];
        bytes[..4].copy_from_slice(b"MCFG");
        let len = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&len.to_le_bytes());
        bytes[8] = 1;
        for (record, &(base, segment, start, end)) in bytes[44..].chunks_exact_mut(16).zip(records) {
            record[..8].copy_from_slice(&base.to_le_bytes());
            record[8..10].copy_from_slice(&segment.to_le_bytes());
            record[10] = start;
            record[11] = end;
        }
        repair(&mut bytes);
        bytes
    }

    #[test]
    fn parses_multiple_segments_and_disjoint_bus_ranges() {
        let bytes = table(&[
            (0x8000_0000, 0, 32, 63),
            (0x8000_0000, 0, 64, 127),
            (0x9000_0000, 1, 0, 255),
        ]);
        let mcfg = Mcfg::parse(&bytes).unwrap();
        assert_eq!(mcfg.allocations().len(), 3);
        assert_eq!(mcfg.allocations()[0].first(), 0x8200_0000);
        assert_eq!(mcfg.allocations()[2].segment, 1);
        assert!(Mcfg::parse(&table(&[])).unwrap().allocations().is_empty());
    }

    #[test]
    fn validates_signature_checksum_length_and_truncation() {
        let valid = table(&[(0x8000_0000, 0, 0, 255)]);
        for length in 0..valid.len() {
            assert!(Mcfg::parse(&valid[..length]).is_err());
        }
        let mut bytes = valid.clone();
        bytes[0] = b'X';
        assert_eq!(Mcfg::parse(&bytes), Err(AcpiError::BadSignature));
        let mut bytes = valid.clone();
        bytes[44] ^= 1;
        assert_eq!(Mcfg::parse(&bytes), Err(AcpiError::BadChecksum));
        for length in [36, 43, 45, 59] {
            let mut bytes = valid[..length].to_vec();
            bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            repair(&mut bytes);
            assert_eq!(Mcfg::parse(&bytes), Err(AcpiError::InvalidLength));
        }
        let mut trailing = valid.clone();
        trailing.extend_from_slice(&[0xff; 16]);
        assert_eq!(Mcfg::parse(&trailing), Mcfg::parse(&valid));
    }

    #[test]
    fn rejects_bad_allocations_and_ambiguous_windows() {
        for record in [
            (1, 0, 0, 0),
            (0x8000_0000, 0, 2, 1),
            (0xffff_ffff_fff0_0000, 0, 1, 1),
        ] {
            assert_eq!(Mcfg::parse(&table(&[record])), Err(AcpiError::InvalidEntry));
        }
        for second in [
            (0x9000_0000, 0, 32, 80), // same segment, overlapping buses
            (0x8000_0000, 1, 32, 80), // distinct segment, physical alias
            (0x8000_0000, 0, 0, 63), // duplicate
        ] {
            assert_eq!(
                Mcfg::parse(&table(&[(0x8000_0000, 0, 0, 63), second])),
                Err(AcpiError::InvalidEntry)
            );
        }
    }

    #[test]
    fn capacity_is_bounded_and_reserved_fields_are_ignored() {
        let records: Vec<_> = (0..MAX_ALLOCATIONS)
            .map(|i| (0x8000_0000, 0, i as u8, i as u8))
            .collect();
        assert_eq!(
            Mcfg::parse(&table(&records)).unwrap().allocations().len(),
            MAX_ALLOCATIONS
        );
        let mut too_many = records;
        too_many.push((0x8000_0000, 0, 32, 32));
        assert_eq!(Mcfg::parse(&table(&too_many)), Err(AcpiError::TooManyEntries));
        let mut bytes = table(&[(0x8000_0000, 0, 0, 0)]);
        bytes[36..44].fill(0xff);
        bytes[56..60].fill(0xff);
        repair(&mut bytes);
        assert!(Mcfg::parse(&bytes).is_ok());
    }
}
