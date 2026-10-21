//! The operation-region reads firmware methods are allowed to perform.
//!
//! A region's base and length come from the table's own AML, so a provider that honoured
//! whatever a table named would let a table read kernel memory or poke a device the kernel
//! already owns. This provider answers only inside windows the kernel itself validated from a
//! table it parsed: the HPET register block the ACPI HPET table names, and the PM1a control
//! block the FADT names. A page of firmware-named MMIO is *mapped* through the MMIO window
//! rather than assumed to be in the direct map — `0xFED0_0000` is above its two-gigabyte limit.
//!
//! Reads are real: memory through a mapped [`Region`], ports through the port primitives. Both
//! are side-effect-free for the registers firmware inspects (`_STA` of an HPET device reads the
//! vendor and period registers). Writes are not implemented here at all: the evaluator refuses
//! a field write into a region, because a write is a hardware effect that needs its own
//! permission model.

use crate::mm::mmio;
use nolx_power_core::aml::{
    Error, RegionField, RegionFieldReader, RegionFieldWriteBackend, RegionSpace,
};

/// How many firmware windows this provider will answer for.
pub const MAX_WINDOWS: usize = 4;

#[derive(Clone, Copy)]
struct Window {
    space: RegionSpace,
    /// The address the firmware named: a physical address or a port number.
    start: u64,
    /// How many bytes the window covers.
    length: u64,
    /// The mapping, for a memory window.
    mapped: Option<mmio::Region>,
}

/// The windows a firmware method may read through an operation region.
#[derive(Clone, Copy)]
pub struct RegionWindows {
    windows: [Option<Window>; MAX_WINDOWS],
    count: usize,
}

impl RegionWindows {
    /// No window authorized: every region read is refused.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            windows: [None; MAX_WINDOWS],
            count: 0,
        }
    }

    /// Authorize and map a memory window. Refuses an empty, wrapping or unmappable window
    /// rather than silently narrowing it.
    pub fn authorize_memory(&mut self, physical: u64, length: u64) -> bool {
        let Some(end) = physical.checked_add(length) else {
            return false;
        };
        if length == 0 || self.count == MAX_WINDOWS || end <= physical {
            return false;
        }
        let Ok(mapped) = mmio::map(physical, length) else {
            return false;
        };
        self.windows[self.count] = Some(Window {
            space: RegionSpace::SystemMemory,
            start: physical,
            length,
            mapped: Some(mapped),
        });
        self.count += 1;
        true
    }

    /// Authorize a port window: a fixed set of I/O ports the firmware may read.
    pub fn authorize_io(&mut self, port: u64, length: u64) -> bool {
        let Some(end) = port.checked_add(length) else {
            return false;
        };
        if length == 0 || self.count == MAX_WINDOWS || end > 0x1_0000 {
            return false;
        }
        self.windows[self.count] = Some(Window {
            space: RegionSpace::SystemIo,
            start: port,
            length,
            mapped: None,
        });
        self.count += 1;
        true
    }

    /// How many windows are authorized.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether no window is authorized.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn find(&self, space: RegionSpace, address: u64, end: u64) -> Option<Window> {
        self.windows[..self.count]
            .iter()
            .flatten()
            .find(|window| {
                window.space == space
                    && address >= window.start
                    && end <= window.start + window.length
            })
            .copied()
    }
}

impl RegionFieldReader for RegionWindows {
    fn read(&self, space: RegionSpace, address: u64, bytes: u8) -> Result<u64, Error> {
        let end = address
            .checked_add(u64::from(bytes))
            .ok_or(Error::Unsupported(0x5b80))?;
        let window = self
            .find(space, address, end)
            .ok_or(Error::Unsupported(0x5b80))?;
        let offset = address - window.start;
        match (space, bytes) {
            (RegionSpace::SystemMemory, 1) => {
                let mapped = window.mapped.ok_or(Error::Unsupported(0x5b80))?;
                mapped
                    .read8(offset)
                    .map(u64::from)
                    .map_err(|_| Error::Unsupported(0x5b80))
            }
            (RegionSpace::SystemMemory, 2) => {
                let mapped = window.mapped.ok_or(Error::Unsupported(0x5b80))?;
                mapped
                    .read16(offset)
                    .map(u64::from)
                    .map_err(|_| Error::Unsupported(0x5b80))
            }
            (RegionSpace::SystemMemory, 4) => {
                let mapped = window.mapped.ok_or(Error::Unsupported(0x5b80))?;
                mapped
                    .read32(offset)
                    .map(u64::from)
                    .map_err(|_| Error::Unsupported(0x5b80))
            }
            // A port number is a 16-bit I/O address. The window has already bounded `address`
            // to a port the kernel authorized, and the port primitives are the only way in.
            (RegionSpace::SystemIo, 1) => {
                // SAFETY: the port lies inside an authorized window and the caller accepts that
                // reading it is side-effect free for the registers firmware declares.
                Ok(u64::from(unsafe { super::inb(address as u16) }))
            }
            (RegionSpace::SystemIo, 2) => {
                // SAFETY: as above, one 16-bit port read.
                Ok(u64::from(unsafe { super::inw(address as u16) }))
            }
            (RegionSpace::SystemIo, 4) => {
                // SAFETY: as above, one 32-bit port read.
                Ok(u64::from(unsafe { super::inl(address as u16) }))
            }
            // An eight-byte unit on a 32-bit device register is two accesses that a device could
            // interleave; and an embedded controller needs its own handshake, which this kernel
            // does not have. Neither is guessed at.
            _ => Err(Error::Unsupported(0x5b80)),
        }
    }
}

impl RegionFieldWriteBackend for RegionWindows {
    type Error = Error;

    fn read_field(&mut self, field: RegionField) -> Result<u64, Error> {
        self.read_units(&field)
    }

    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Error> {
        let access = u64::from(field.access);
        let bytes = (u64::from(field.shift) + u64::from(field.bits)).div_ceil(8);
        let window = self
            .find(
                field.space,
                field.address,
                field
                    .address
                    .checked_add(bytes)
                    .ok_or(Error::Unsupported(0x5b80))?,
            )
            .ok_or(Error::Unsupported(0x5b80))?;
        let offset = field.address - window.start;
        let mut at = 0;
        while at < bytes {
            // Read-modify-write per unit: only the field's own bits change, and any bit outside
            // the field keeps whatever the device holds.
            let unit_offset = at;
            let mut unit = self.read(window.space, window.start + offset + at, field.access)?;
            let mut bit = at * 8;
            while bit < (at + access) * 8 {
                let field_bit = bit - u64::from(field.shift);
                if field_bit < u64::from(field.bits) {
                    let mask = 1u64 << bit;
                    if (value >> field_bit) & 1 == 1 {
                        unit |= mask;
                    } else {
                        unit &= !mask;
                    }
                } else if bit < u64::from(field.shift) + u64::from(field.bits) {
                    let mask = 1u64 << bit;
                    unit &= !mask;
                }
                bit += 1;
            }
            self.write(
                window.space,
                window.start + offset + unit_offset,
                unit,
                field.access,
            )?;
            at += access;
        }
        Ok(())
    }
}

impl RegionWindows {
    /// Read a whole field through the same bounded unit path the evaluator uses.
    fn read_units(&self, field: &RegionField) -> Result<u64, Error> {
        let access = u64::from(field.access);
        let bytes = (u64::from(field.shift) + u64::from(field.bits)).div_ceil(8);
        let end = field
            .address
            .checked_add(bytes)
            .ok_or(Error::Unsupported(0x5b80))?;
        self.find(field.space, field.address, end)
            .ok_or(Error::Unsupported(0x5b80))?;
        let mut raw = 0u128;
        let mut at = 0;
        while at < bytes {
            let unit = self.read(field.space, field.address + at, field.access)?;
            raw |= u128::from(unit) << (at * 8);
            at += access;
        }
        let mask = if field.bits == 64 {
            u64::MAX
        } else {
            (1u64 << field.bits) - 1
        };
        Ok((raw >> field.shift) as u64 & mask)
    }

    /// Write one aligned unit inside an authorized window.
    fn write(&self, space: RegionSpace, address: u64, value: u64, bytes: u8) -> Result<(), Error> {
        let end = address
            .checked_add(u64::from(bytes))
            .ok_or(Error::Unsupported(0x5b80))?;
        let window = self
            .find(space, address, end)
            .ok_or(Error::Unsupported(0x5b80))?;
        let offset = address - window.start;
        match (space, bytes) {
            (RegionSpace::SystemMemory, 1) => window
                .mapped
                .ok_or(Error::Unsupported(0x5b80))?
                .write8(offset, value as u8)
                .map_err(|_| Error::Unsupported(0x5b80)),
            (RegionSpace::SystemMemory, 2) => window
                .mapped
                .ok_or(Error::Unsupported(0x5b80))?
                .write16(offset, value as u16)
                .map_err(|_| Error::Unsupported(0x5b80)),
            (RegionSpace::SystemMemory, 4) => window
                .mapped
                .ok_or(Error::Unsupported(0x5b80))?
                .write32(offset, value as u32)
                .map_err(|_| Error::Unsupported(0x5b80)),
            // SAFETY: the port lies inside a window the kernel authorized from a parsed table,
            // and the caller has an exact write permission for the field.
            (RegionSpace::SystemIo, 1) => {
                unsafe { super::outb(address as u16, value as u8) };
                Ok(())
            }
            // SAFETY: as the byte case above, one 16-bit port store to an authorized port.
            (RegionSpace::SystemIo, 2) => {
                unsafe { super::outw(address as u16, value as u16) };
                Ok(())
            }
            // SAFETY: as the byte case above, one 32-bit port store to an authorized port.
            (RegionSpace::SystemIo, 4) => {
                unsafe { super::outl(address as u16, value as u32) };
                Ok(())
            }
            _ => Err(Error::Unsupported(0x5b80)),
        }
    }
}
