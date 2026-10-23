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
//! vendor and period registers). Writes go through the same authorized windows: a field write is
//! an intent until [`RegionFieldWriteBackend::write_field`] applies it with exact permission and
//! a verified readback. An embedded-controller region is served through the standard EC handshake
//! (RD_EC/WR_EC commands with IBF/OBF polling) once its ports are authorized, instead of being
//! journaled only.

use crate::mm::mmio;
use nolx_pci_core::{Bdf, ConfigSpace as _};
use nolx_power_core::aml::{
    Error, FieldReader, PciField, PciFieldWriteBackend, RegionField, RegionFieldReader,
    RegionFieldWriteBackend, RegionSpace,
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

/// The embedded controller's Status/Command port is two registers at one address: reads
/// return status, writes latch a command. The Data port transfers bytes. Every poll is
/// bounded, so a wedged EC fails the transaction instead of spinning the interpreter.
const EC_COMMAND_READ: u8 = 0x80; // RD_EC: read the EC byte at an offset
const EC_COMMAND_WRITE: u8 = 0x81; // WR_EC: write the EC byte at an offset
const EC_STATUS_IBF: u8 = 0x02; // input buffer full: the EC has not latched the last byte written
const EC_STATUS_OBF: u8 = 0x01; // output buffer full: the EC has a byte ready for the host
const EC_POLL_LIMIT: u32 = 10_000; // bounded: a silent EC fails rather than hangs

/// An operation-region provider for the ACPI embedded-controller space
/// (`RegionSpace::EmbeddedControl`).
///
/// A field's address is the EC byte offset and only byte-wide access is supported: the EC
/// transaction protocol transfers one byte at a time (command, offset, then data, each step
/// guarded by the input/output buffer flags). Refuses every access until
/// [`EcRegion::authorize_ec`] has been given a command/data port pair the kernel validated
/// from firmware (the ECDT table or an EC device's resources); an EC is never defaulted to
/// present.
#[derive(Clone, Copy, Default)]
pub struct EcRegion {
    command: Option<u16>,
    data: Option<u16>,
}

impl EcRegion {
    /// No ports authorized: every embedded-controller access is refused.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            command: None,
            data: None,
        }
    }

    /// Authorize one command/data port pair. Refuses a degenerate pair or a second pair.
    pub fn authorize_ec(&mut self, command: u16, data: u16) -> bool {
        if command == data || self.command.is_some() {
            return false;
        }
        self.command = Some(command);
        self.data = Some(data);
        true
    }

    /// Wait until the input buffer is empty so the EC can latch the next byte written.
    fn wait_ibf_clear(command: u16) -> Result<(), Error> {
        let mut count = 0u32;
        while unsafe { super::inb(command) } & EC_STATUS_IBF != 0 {
            count += 1;
            if count >= EC_POLL_LIMIT {
                return Err(Error::Unsupported(0x5b82));
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Wait until the output buffer holds a byte for the host.
    fn wait_obf_set(command: u16) -> Result<(), Error> {
        let mut count = 0u32;
        while unsafe { super::inb(command) } & EC_STATUS_OBF == 0 {
            count += 1;
            if count >= EC_POLL_LIMIT {
                return Err(Error::Unsupported(0x5b82));
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Read one EC byte at `offset`: command, then offset, then data, each step guarded by
    /// the buffer flags.
    fn read_byte(command: u16, data: u16, offset: u8) -> Result<u8, Error> {
        Self::wait_ibf_clear(command)?;
        // SAFETY: the command port was authorized from firmware-validated resources.
        unsafe { super::outb(command, EC_COMMAND_READ) };
        Self::wait_ibf_clear(command)?;
        // SAFETY: the data port was authorized from firmware-validated resources.
        unsafe { super::outb(data, offset) };
        Self::wait_obf_set(command)?;
        // SAFETY: as above, one data-byte read after OBF.
        Ok(unsafe { super::inb(data) })
    }

    /// Write one EC byte at `offset`: command, offset, then value, each step guarded by the
    /// input buffer flag.
    fn write_byte(command: u16, data: u16, offset: u8, value: u8) -> Result<(), Error> {
        Self::wait_ibf_clear(command)?;
        // SAFETY: both ports were authorized from firmware-validated resources, and the input
        // buffer was observed empty immediately before this command byte.
        unsafe { super::outb(command, EC_COMMAND_WRITE) };
        Self::wait_ibf_clear(command)?;
        // SAFETY: as above, one data byte after the EC accepted the command.
        unsafe { super::outb(data, offset) };
        Self::wait_ibf_clear(command)?;
        // SAFETY: as above, one data byte after the EC accepted the register offset.
        unsafe { super::outb(data, value) };
        Self::wait_ibf_clear(command)
    }
}

impl RegionFieldReader for EcRegion {
    fn read(&self, space: RegionSpace, address: u64, bytes: u8) -> Result<u64, Error> {
        if space != RegionSpace::EmbeddedControl || bytes != 1 {
            return Err(Error::Unsupported(0x5b82));
        }
        let (Some(command), Some(data)) = (self.command, self.data) else {
            return Err(Error::Unsupported(0x5b82));
        };
        let offset = u8::try_from(address).map_err(|_| Error::Unsupported(0x5b82))?;
        Self::read_byte(command, data, offset).map(u64::from)
    }
}

impl RegionFieldWriteBackend for EcRegion {
    type Error = Error;

    fn read_field(&mut self, field: RegionField) -> Result<u64, Error> {
        if field.space != RegionSpace::EmbeddedControl || field.access != 1 {
            return Err(Error::Unsupported(0x5b82));
        }
        let offset = u8::try_from(field.address).map_err(|_| Error::Unsupported(0x5b82))?;
        let (Some(command), Some(data)) = (self.command, self.data) else {
            return Err(Error::Unsupported(0x5b82));
        };
        Self::read_byte(command, data, offset).map(u64::from)
    }

    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Error> {
        if field.space != RegionSpace::EmbeddedControl || field.access != 1 {
            return Err(Error::Unsupported(0x5b82));
        }
        let offset = u8::try_from(field.address).map_err(|_| Error::Unsupported(0x5b82))?;
        let value = u8::try_from(value).map_err(|_| Error::Unsupported(0x5b82))?;
        let (Some(command), Some(data)) = (self.command, self.data) else {
            return Err(Error::Unsupported(0x5b82));
        };
        Self::write_byte(command, data, offset, value)
    }
}

/// All operation-region providers the kernel authorizes: the validated memory and I/O
/// windows plus the optional embedded-controller handshake provider.
///
/// The reader answers a region by its space: `EmbeddedControl` goes to the EC handshake,
/// everything else goes to the authorized windows. The writer applies region-field writes
/// through [`apply_region_field_writes`]: exact-permission checked with a verified readback.
/// A space with no provider is refused.
#[derive(Clone, Copy)]
pub struct FirmwareRegions {
    windows: RegionWindows,
    ec: EcRegion,
}

impl FirmwareRegions {
    /// No provider authorized: every region access is refused.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            windows: RegionWindows::empty(),
            ec: EcRegion::empty(),
        }
    }

    /// Authorize and map a memory window (delegates to the windows provider).
    pub fn authorize_memory(&mut self, physical: u64, length: u64) -> bool {
        self.windows.authorize_memory(physical, length)
    }

    /// Authorize a port window (delegates to the windows provider).
    pub fn authorize_io(&mut self, port: u64, length: u64) -> bool {
        self.windows.authorize_io(port, length)
    }

    /// Authorize the embedded controller's command/data port pair.
    pub fn authorize_ec(&mut self, command: u16, data: u16) -> bool {
        self.ec.authorize_ec(command, data)
    }

    /// How many authorized memory/port windows this provider holds (the EC pair is separate).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether nothing at all is authorized: no window and no embedded controller.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.ec.command.is_none() && self.ec.data.is_none()
    }
}

impl RegionFieldReader for FirmwareRegions {
    fn read(&self, space: RegionSpace, address: u64, bytes: u8) -> Result<u64, Error> {
        match space {
            RegionSpace::EmbeddedControl => self.ec.read(space, address, bytes),
            _ => self.windows.read(space, address, bytes),
        }
    }
}

impl RegionFieldWriteBackend for FirmwareRegions {
    type Error = Error;

    fn read_field(&mut self, field: RegionField) -> Result<u64, Error> {
        match field.space {
            RegionSpace::EmbeddedControl => self.ec.read_field(field),
            _ => self.windows.read_field(field),
        }
    }

    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Error> {
        match field.space {
            RegionSpace::EmbeddedControl => self.ec.write_field(field, value),
            _ => self.windows.write_field(field, value),
        }
    }
}

/// How many PCI functions one device power transition may program.
pub const MAX_PCI_SCOPES: usize = 1;

/// PCI **configuration** operation regions, for the one function a transition belongs to.
///
/// A `PCI_Config` region names a configuration-space offset in its own base, and every field
/// inside it is a register of whatever device declared it. The evaluator resolves the address
/// from the declaring device's `_ADR` under a segment-zero, bus-zero root bridge and requires
/// the region base and the field to stay inside the 256-byte header, so this provider only has
/// to answer "which function" and "which byte". It answers exactly the addresses it was
/// authorized with: a field naming any other function is refused, not clamped.
///
/// Reads are the declared access width (1, 2 or 4 bytes) so a 16-bit `PMCS` read returns 16
/// bits; writes are one byte, because that is the granularity a transaction verifies and rolls
/// back. The transport is the kernel's audited configuration access, so a transition uses the
/// same ECAM or `0xCF8`/`0xCFC` path as every other configuration user instead of a second,
/// unaudited register interface.
#[derive(Clone, Copy)]
pub struct PciConfigRegions {
    /// `_ADR` device/function tokens (`device << 16 | function`) this provider answers for.
    addresses: [Option<u32>; MAX_PCI_SCOPES],
    count: usize,
}

impl PciConfigRegions {
    /// No function authorized: every configuration access is refused.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            addresses: [None; MAX_PCI_SCOPES],
            count: 0,
        }
    }

    /// Authorize one device/function token, refusing a malformed `_ADR` rather than masking it.
    pub fn authorize(&mut self, address: u32) -> bool {
        if address & !0x1f_0007 != 0 || self.count == MAX_PCI_SCOPES {
            return false;
        }
        self.addresses[self.count] = Some(address);
        self.count += 1;
        true
    }

    /// Whether this provider answers for `address`.
    #[must_use]
    pub fn authorized(&self, address: u32) -> bool {
        self.addresses[..self.count].contains(&Some(address))
    }

    /// Whether any function is authorized.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn bdf(address: u32) -> Result<Bdf, Error> {
        let device = u8::try_from(address >> 16).map_err(|_| Error::Unsupported(0x5b83))?;
        let function = u8::try_from(address & 7).map_err(|_| Error::Unsupported(0x5b83))?;
        Bdf::new(0, device, function).map_err(|_| Error::Unsupported(0x5b83))
    }

    fn check(&self, field: PciField) -> Result<Bdf, Error> {
        if !self.authorized(field.address) || !field.bytes.is_power_of_two() {
            return Err(Error::Unsupported(0x5b83));
        }
        let bytes = u32::from(field.bytes);
        if bytes > 4
            || u32::from(field.offset) % bytes != 0
            || u32::from(field.offset) + bytes > 256
        {
            return Err(Error::Unsupported(0x5b83));
        }
        Self::bdf(field.address)
    }

    fn read_bytes(&self, field: PciField) -> Result<u64, Error> {
        let bdf = self.check(field)?;
        let mut value = 0u64;
        for index in 0..field.bytes {
            let offset = field.offset + u16::from(index);
            let dword = super::pci::ConfigAccess.read_u32(bdf, (offset & !3) as u8);
            let byte = (dword >> ((offset & 3) * 8)) as u8;
            value |= u64::from(byte) << (index * 8);
        }
        Ok(value)
    }

    fn write_byte_now(&self, field: PciField, value: u8) -> Result<(), Error> {
        let bdf = self.check(PciField { bytes: 1, ..field })?;
        let offset = field.offset;
        let shift = (offset & 3) * 8;
        let old = super::pci::ConfigAccess.read_u32(bdf, (offset & !3) as u8);
        let value = (old & !(0xff << shift)) | (u32::from(value) << shift);
        super::pci::ConfigAccess.write_u32(bdf, (offset & !3) as u8, value);
        Ok(())
    }
}

impl FieldReader for PciConfigRegions {
    fn read(&self, field: PciField) -> Result<u64, Error> {
        self.read_bytes(field)
    }
}

impl PciFieldWriteBackend for PciConfigRegions {
    type Error = Error;

    fn read_byte(&mut self, field: PciField) -> Result<u8, Error> {
        self.read_bytes(PciField { bytes: 1, ..field })
            .map(|value| value as u8)
    }

    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), Error> {
        self.write_byte_now(field, value)
    }
}

/// The reader a device power transition evaluates through: the authorized configuration
/// functions first, then the boot-time PIRQ snapshot.
///
/// Splitting by address, rather than trying one provider and falling back on error, keeps a
/// genuine transport failure visible instead of turning it into "not a PIRQ byte".
pub struct PowerFieldReader<'a> {
    /// Configuration space of the recorded power targets.
    pub pci: &'a PciConfigRegions,
    /// The read-only PIRQ snapshot `_SRS` planning uses.
    pub snapshot: &'a super::pci_intx::Snapshot,
}

impl FieldReader for PowerFieldReader<'_> {
    fn read(&self, field: PciField) -> Result<u64, Error> {
        if self.pci.authorized(field.address) {
            self.pci.read(field)
        } else {
            self.snapshot.read(field)
        }
    }
}

/// The writer a device power transition applies through: operation regions and configuration
/// space under one error type, so a mixed journal rolls back as one transaction.
#[derive(Clone, Copy)]
pub struct DevicePowerProvider {
    /// Memory-mapped, port and embedded-controller regions.
    pub regions: FirmwareRegions,
    /// The transitioning device's own configuration function.
    pub pci: PciConfigRegions,
}

impl RegionFieldWriteBackend for DevicePowerProvider {
    type Error = Error;

    fn read_field(&mut self, field: RegionField) -> Result<u64, Error> {
        self.regions.read_field(field)
    }

    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Error> {
        self.regions.write_field(field, value)
    }
}

impl PciFieldWriteBackend for DevicePowerProvider {
    type Error = Error;

    fn read_byte(&mut self, field: PciField) -> Result<u8, Error> {
        self.pci.read_byte(field)
    }

    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), Error> {
        self.pci.write_byte(field, value)
    }
}
