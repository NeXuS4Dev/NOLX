//! Validated PCI Express Enhanced Configuration Access Mechanism (ECAM) windows.
//!
//! An ACPI MCFG allocation describes a segment and an inclusive bus range. Its base
//! corresponds to bus zero, not to the first bus in that range (PCI Firmware
//! Specification, MCFG allocation structure). This module performs no MMIO: a caller
//! must map the returned physical address uncached and use the requested access width.

use crate::Bdf;

/// A malformed ECAM allocation or configuration access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EcamError {
    /// The allocation's base is not aligned to a 1 MiB bus window.
    UnalignedBase,
    /// The inclusive bus range is reversed.
    InvalidBusRange,
    /// The allocation extends beyond the physical address integer's range.
    AddressOverflow,
    /// The requested segment or bus is outside this allocation.
    OutsideWindow,
    /// The device or function cannot be expressed by PCI addressing.
    InvalidBdf,
    /// The access is misaligned or extends beyond the function's 4 KiB space.
    InvalidOffset,
}

/// The width of a configuration-space transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    /// An 8-bit transaction.
    Byte,
    /// A 16-bit transaction.
    Word,
    /// A 32-bit transaction.
    Dword,
}

impl AccessWidth {
    fn bytes(self) -> u16 {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
        }
    }
}

/// A validated MCFG segment/bus allocation, with no ownership of its MMIO mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcamWindow {
    base: u64,
    segment: u16,
    start_bus: u8,
    end_bus: u8,
}

impl EcamWindow {
    /// Validate an MCFG allocation, including its highest addressable byte.
    pub fn new(base: u64, segment: u16, start_bus: u8, end_bus: u8) -> Result<Self, EcamError> {
        if base & 0xF_FFFF != 0 {
            return Err(EcamError::UnalignedBase);
        }
        if start_bus > end_bus {
            return Err(EcamError::InvalidBusRange);
        }
        let last = (u64::from(end_bus) << 20) | 0xF_FFFF;
        base.checked_add(last).ok_or(EcamError::AddressOverflow)?;
        Ok(Self {
            base,
            segment,
            start_bus,
            end_bus,
        })
    }

    /// Physical start and byte length of the bus range to map, not the bus-zero base.
    pub fn physical_range(self) -> (u64, u64) {
        (
            self.base + (u64::from(self.start_bus) << 20),
            (u64::from(self.end_bus) - u64::from(self.start_bus) + 1) << 20,
        )
    }

    /// Resolve an aligned access in the full 4 KiB function configuration space.
    ///
    /// Unlike legacy configuration ports, low offset bits are not silently discarded.
    /// Segment selection is explicit so equal BDFs in different segments cannot alias.
    pub fn address(
        self,
        segment: u16,
        bdf: Bdf,
        offset: u16,
        width: AccessWidth,
    ) -> Result<u64, EcamError> {
        if segment != self.segment || bdf.bus < self.start_bus || bdf.bus > self.end_bus {
            return Err(EcamError::OutsideWindow);
        }
        // Bdf fields are public, so callers need not have used Bdf::new.
        if bdf.device >= 32 || bdf.function >= 8 {
            return Err(EcamError::InvalidBdf);
        }
        let bytes = width.bytes();
        if offset >= 4096 || !offset.is_multiple_of(bytes) || bytes > 4096 - offset {
            return Err(EcamError::InvalidOffset);
        }
        Ok(self.base
            + (u64::from(bdf.bus) << 20)
            + (u64::from(bdf.device) << 15)
            + (u64::from(bdf.function) << 12)
            + u64::from(offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_start_bus_uses_bus_zero_base() {
        let window = EcamWindow::new(0x8000_0000, 7, 0x40, 0x4f).unwrap();
        assert_eq!(window.physical_range(), (0x8400_0000, 0x0100_0000));
        assert_eq!(
            window.address(7, Bdf::new(0x42, 3, 5).unwrap(), 0xabc, AccessWidth::Dword),
            Ok(0x8421_dabc)
        );
    }

    #[test]
    fn allocations_reject_bad_alignment_ranges_and_overflow() {
        assert_eq!(EcamWindow::new(1, 0, 0, 0), Err(EcamError::UnalignedBase));
        assert_eq!(EcamWindow::new(0, 0, 2, 1), Err(EcamError::InvalidBusRange));
        assert_eq!(
            EcamWindow::new(0xffff_ffff_fff0_0000, 0, 1, 1),
            Err(EcamError::AddressOverflow)
        );
        let top = EcamWindow::new(0xffff_ffff_fff0_0000, 0, 0, 0).unwrap();
        assert_eq!(
            top.address(0, Bdf::new(0, 31, 7).unwrap(), 4095, AccessWidth::Byte),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn segment_bus_and_unchecked_bdf_are_validated() {
        let window = EcamWindow::new(0, 1, 2, 3).unwrap();
        for (segment, bus) in [(0, 2), (1, 1), (1, 4)] {
            assert_eq!(
                window.address(segment, Bdf::new(bus, 0, 0).unwrap(), 0, AccessWidth::Dword),
                Err(EcamError::OutsideWindow)
            );
        }
        for (device, function) in [(32, 0), (0, 8)] {
            assert_eq!(
                window.address(
                    1,
                    Bdf {
                        bus: 2,
                        device,
                        function,
                    },
                    0,
                    AccessWidth::Byte,
                ),
                Err(EcamError::InvalidBdf)
            );
        }
    }

    #[test]
    fn offsets_preserve_width_alignment_and_function_boundaries() {
        let window = EcamWindow::new(0, 0, 0, 255).unwrap();
        let bdf = Bdf::new(255, 31, 7).unwrap();
        for (width, last) in [
            (AccessWidth::Byte, 4095),
            (AccessWidth::Word, 4094),
            (AccessWidth::Dword, 4092),
        ] {
            assert_eq!(
                window.address(0, bdf, last, width),
                Ok(0x0fff_f000 + u64::from(last))
            );
            assert_eq!(
                window.address(0, bdf, 4096, width),
                Err(EcamError::InvalidOffset)
            );
            assert_eq!(
                window.address(0, bdf, u16::MAX, width),
                Err(EcamError::InvalidOffset)
            );
        }
        for (width, offset) in [(AccessWidth::Word, 1), (AccessWidth::Dword, 2)] {
            assert_eq!(
                window.address(0, bdf, offset, width),
                Err(EcamError::InvalidOffset)
            );
        }
    }

    #[test]
    fn all_functions_have_distinct_nonoverlapping_pages() {
        let window = EcamWindow::new(0, 0, 0, 255).unwrap();
        let mut expected = 0;
        for bus in 0..=255 {
            for device in 0..32 {
                for function in 0..8 {
                    let bdf = Bdf::new(bus, device, function).unwrap();
                    assert_eq!(window.address(0, bdf, 0, AccessWidth::Dword), Ok(expected));
                    expected += 4096;
                }
            }
        }
        assert_eq!(expected, 1 << 28);
    }
}
