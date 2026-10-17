//! Strict host-bridge resource templates for bounded segment-zero PCI allocation.
//!
//! Unlike the legacy device catalog, these records preserve producer/consumer and address
//! space semantics. Only fixed, positive-decode, untranslated windows are accepted. This
//! does not implement IRQ routing, bus numbering, rebalancing or arbitrary root bridges.

use crate::{aml, ResourceError};

/// Address space named by a host-bridge descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// Physical memory.
    Memory,
    /// I/O ports.
    Io,
    /// PCI bus numbers.
    Bus,
}
/// A half-open interval with descriptor ownership retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Address space.
    pub space: Space,
    /// First address.
    pub start: u64,
    /// First address after the range.
    pub end: u64,
    /// True for a host-produced aperture, false for a consumed reservation.
    pub producer: bool,
    /// Memory descriptor type-specific flags; I/O and bus flags are validated separately.
    pub memory_flags: u8,
}
/// Maximum descriptors retained from one root template; overflow refuses the whole template.
pub const MAX_WINDOWS: usize = 16;
/// A completely decoded root template, never a partially accepted prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootResources {
    windows: [Option<Window>; MAX_WINDOWS],
    count: usize,
}
impl RootResources {
    /// All windows and reservations in descriptor order.
    pub fn iter(&self) -> impl Iterator<Item = Window> + '_ {
        self.windows[..self.count].iter().flatten().copied()
    }
    fn push(&mut self, window: Window) -> Result<(), ResourceError> {
        if self.count == MAX_WINDOWS {
            return Err(ResourceError::TooMany);
        }
        if window.producer
            && self.iter().any(|old| {
                old.producer
                    && old.space == window.space
                    && old.start < window.end
                    && window.start < old.end
            })
        {
            return Err(ResourceError::InvalidRange);
        }
        self.windows[self.count] = Some(window);
        self.count += 1;
        Ok(())
    }
}
fn number(bytes: &[u8], offset: usize, width: usize) -> u64 {
    bytes[offset..offset + width]
        .iter()
        .enumerate()
        .fold(0, |v, (i, b)| v | (u64::from(*b) << (i * 8)))
}
fn interval(start: u64, length: u64) -> Result<u64, ResourceError> {
    if length == 0 {
        return Err(ResourceError::InvalidRange);
    }
    start.checked_add(length).ok_or(ResourceError::InvalidRange)
}
/// Decode Word/DWord/QWord address windows and fixed I/O/memory reservations.
/// Translation, subtractive/variable decode, unknown flags/descriptors, optional resource
/// sources and bad checksums/ranges are refused. Only zero padding may follow the EndTag.
pub fn parse(bytes: &[u8]) -> Result<RootResources, ResourceError> {
    let mut result = RootResources {
        windows: [None; MAX_WINDOWS],
        count: 0,
    };
    let mut at = 0;
    loop {
        let tag = *bytes.get(at).ok_or(ResourceError::NoEnd)?;
        let (body, length) = if tag & 0x80 == 0 {
            (at + 1, usize::from(tag & 7))
        } else {
            let size = bytes.get(at + 1..at + 3).ok_or(ResourceError::Truncated)?;
            (at + 3, usize::from(u16::from_le_bytes([size[0], size[1]])))
        };
        let end = body.checked_add(length).ok_or(ResourceError::Truncated)?;
        let data = bytes.get(body..end).ok_or(ResourceError::Truncated)?;
        if tag == 0x79 {
            if length != 1 || bytes[end..].iter().any(|b| *b != 0) {
                return Err(ResourceError::InvalidLength);
            }
            if data[0] != 0 && bytes[..end].iter().fold(0u8, |s, b| s.wrapping_add(*b)) != 0 {
                return Err(ResourceError::Checksum);
            }
            return Ok(result);
        }
        let window = match tag {
            0x88 | 0x87 | 0x8a => {
                let width = match tag {
                    0x88 => 2,
                    0x87 => 4,
                    _ => 8,
                };
                if length != 3 + 5 * width {
                    return Err(ResourceError::InvalidLength);
                }
                let space = match data[0] {
                    0 => Space::Memory,
                    1 => Space::Io,
                    2 => Space::Bus,
                    _ => return Err(ResourceError::Unsupported(tag)),
                };
                // ResourceUsage bit 0: 0 producer, 1 consumer. Both address bounds fixed.
                if data[1] & !1 != 0x0c
                    || number(data, 3, width) != 0
                    || number(data, 3 + 3 * width, width) != 0
                {
                    return Err(ResourceError::Unsupported(tag));
                }
                if (space == Space::Io && data[2] != 3)
                    || (space == Space::Bus && data[2] != 0)
                    || (space == Space::Memory && data[2] & !7 != 0)
                {
                    return Err(ResourceError::Unsupported(tag));
                }
                let start = number(data, 3 + width, width);
                let maximum = number(data, 3 + 2 * width, width);
                let end = interval(start, number(data, 3 + 4 * width, width))?;
                if end - 1 != maximum
                    || (space == Space::Io && end > 65536)
                    || (space == Space::Bus && end > 256)
                {
                    return Err(ResourceError::InvalidRange);
                }
                Window {
                    space,
                    start,
                    end,
                    producer: data[1] & 1 == 0,
                    memory_flags: if space == Space::Memory { data[2] } else { 0 },
                }
            }
            0x47 | 0x4b => {
                if length != if tag == 0x47 { 7 } else { 3 } {
                    return Err(ResourceError::InvalidLength);
                }
                let offset = if tag == 0x47 { 1 } else { 0 };
                let start = number(data, offset, 2);
                if tag == 0x47 && (data[0] != 1 || number(data, 3, 2) != start || data[5] > 1) {
                    return Err(ResourceError::Unsupported(tag));
                }
                let end = interval(start, u64::from(data[length - 1]))?;
                if end > 65536 {
                    return Err(ResourceError::InvalidRange);
                }
                Window {
                    space: Space::Io,
                    start,
                    end,
                    producer: false,
                    memory_flags: 0,
                }
            }
            0x86 => {
                if length != 9 {
                    return Err(ResourceError::InvalidLength);
                }
                if data[0] > 1 {
                    return Err(ResourceError::Unsupported(tag));
                }
                let start = number(data, 1, 4);
                let end = interval(start, number(data, 5, 4))?;
                if end > 0x1_0000_0000 {
                    return Err(ResourceError::InvalidRange);
                }
                Window {
                    space: Space::Memory,
                    start,
                    end,
                    producer: false,
                    memory_flags: data[0],
                }
            }
            _ => return Err(ResourceError::Unsupported(tag)),
        };
        result.push(window)?;
        at = end;
    }
}
/// A rejected root discovery, distinct from an entirely absent ACPI platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A root method failed; this is never an absent-member default.
    Aml(aml::Error),
    /// Malformed or unsupported resource template.
    Resources(ResourceError),
    /// No identifiable root, more than one root, disabled root or unsupported segment/bus.
    Topology,
}
impl From<aml::Error> for Error {
    fn from(error: aml::Error) -> Self {
        Self::Aml(error)
    }
}
fn root_id(value: &aml::Value) -> bool {
    match value {
        aml::Value::Integer(id) => [0x030a_d041, 0x080a_d041].contains(id),
        aml::Value::String(id) => id == b"PNP0A03" || id == b"PNP0A08",
        _ => false,
    }
}
/// Evaluate one PNP0A03/PNP0A08 root's status, segment, base bus and resource method.
/// Missing `_SEG`/`_BBN` alone default to zero; failing methods never default. The supported
/// topology is one segment-zero root with a produced 0..255 bus window. Unknown `_HID`
/// evaluation is refused, not silently assumed to describe a non-root device.
pub fn discover(namespace: &aml::Namespace<'_>) -> Result<RootResources, Error> {
    let mut root = None;
    for path in namespace.devices() {
        let identity = namespace.device_value(path, *b"_HID")?;
        if identity
            .as_ref()
            .is_some_and(|value| !matches!(value, aml::Value::Integer(_) | aml::Value::String(_)))
        {
            return Err(Error::Aml(aml::Error::Type));
        }
        if identity.as_ref().is_some_and(root_id) {
            if root.is_some() {
                return Err(Error::Topology);
            }
            root = Some(path);
        }
    }
    let path = root.ok_or(Error::Topology)?;
    if !crate::is_usable(namespace.device_status(path)?.0) {
        return Err(Error::Topology);
    }
    for member in [*b"_SEG", *b"_BBN"] {
        if let Some(value) = namespace.device_value(path, member)? {
            if value.integer()? != 0 {
                return Err(Error::Topology);
            }
        }
    }
    let bytes = namespace.device_resources(path)?.ok_or(Error::Topology)?;
    let resources = parse(&bytes).map_err(Error::Resources)?;
    let buses: alloc::vec::Vec<_> = resources.iter().filter(|w| w.space == Space::Bus).collect();
    if buses.len() != 1 || !buses[0].producer || buses[0].start != 0 || buses[0].end != 256 {
        return Err(Error::Topology);
    }
    Ok(resources)
}

extern crate alloc;
#[cfg(test)]
mod tests;
