//! Bounded evaluated ACPI PCI interrupt routing and preparatory link-resource selection.
//! Current routing resolution performs no `_SRS`, hardware writes or route allocation.
extern crate alloc;
use crate::aml::{self, Namespace, Path, Value};
use alloc::vec::Vec;

mod link;
pub use link::{
    build_srs_template, link_possible_interrupts, link_srs_template, possible_interrupts,
    LinkIrqNumber, LinkIrqOption,
};

/// Fully resolved interrupt electrical characteristics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupt {
    /// Global system interrupt number.
    pub gsi: u32,
    /// Active-low polarity (false means active high).
    pub active_low: bool,
    /// Level trigger (false means edge).
    pub level: bool,
    /// Firmware permits sharing this interrupt.
    pub shared: bool,
}
/// A routing package or link could not safely authorize an interrupt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Actual AML evaluation failed.
    Aml(aml::Error),
    /// Invalid package, address, source, descriptor or electrical flags.
    Invalid,
    /// Ambiguous or duplicate routing entries.
    Ambiguous,
    /// Resource, table or topology capacity exceeded.
    Limit,
}
impl From<aml::Error> for Error {
    fn from(value: aml::Error) -> Self {
        Self::Aml(value)
    }
}
/// One resolved `_PRT` row. Pin is zero-based as required by ACPI, not PCI config space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Device number on this scope's PCI bus.
    pub device: u8,
    /// Exact function, or None for the ACPI 0xffff wildcard.
    pub function: Option<u8>,
    /// Interrupt pin 0..3 (INTA..INTD).
    pub pin: u8,
    /// Resolved link or direct GSI characteristics.
    pub interrupt: Interrupt,
}
/// One namespace bridge's routing table, mapped to a physical bus by its ADR ancestry.
#[derive(Clone, Debug)]
pub struct Table {
    /// Root-relative ordered bridge device/function ADRs, empty for the root bus.
    pub bridges: Vec<u32>,
    /// Completely validated rows, never a partially decoded prefix.
    pub entries: Vec<Entry>,
}
/// Decode one current IRQ resource; ambiguity is refused rather than choosing a bit/entry.
/// Supports Small IRQ and Extended IRQ, strict lengths, optional checksum and zero padding.
pub fn interrupt(bytes: &[u8]) -> Result<Interrupt, Error> {
    let tag = *bytes.first().ok_or(Error::Invalid)?;
    let (end, irq) = match tag {
        0x22 | 0x23 => {
            let len = usize::from(tag & 7);
            let data = bytes.get(1..1 + len).ok_or(Error::Invalid)?;
            let mask = u16::from_le_bytes([data[0], data[1]]);
            if mask.count_ones() != 1 {
                return Err(Error::Invalid);
            }
            let flags = if len == 3 { data[2] } else { 1 };
            if flags & !0x3f != 0 || (flags & 1 == 0) != (flags & 8 != 0) {
                return Err(Error::Invalid);
            }
            (
                1 + len,
                Interrupt {
                    gsi: mask.trailing_zeros(),
                    level: flags & 1 == 0,
                    active_low: flags & 8 != 0,
                    shared: flags & 16 != 0,
                },
            )
        }
        0x89 => {
            if bytes.get(1..3) != Some(&[6, 0]) {
                return Err(Error::Invalid);
            }
            let data = bytes.get(3..9).ok_or(Error::Invalid)?;
            if data[0] & !0x1f != 0 || data[1] != 1 {
                return Err(Error::Invalid);
            }
            (
                9,
                Interrupt {
                    gsi: u32::from_le_bytes(data[2..6].try_into().map_err(|_| Error::Invalid)?),
                    level: data[0] & 2 == 0,
                    active_low: data[0] & 4 != 0,
                    shared: data[0] & 8 != 0,
                },
            )
        }
        _ => return Err(Error::Invalid),
    };
    if bytes.get(end) != Some(&0x79) {
        return Err(Error::Invalid);
    }
    let checksum = *bytes.get(end + 1).ok_or(Error::Invalid)?;
    if (checksum != 0 && bytes[..end + 2].iter().fold(0u8, |v, b| v.wrapping_add(*b)) != 0)
        || bytes[end + 2..].iter().any(|b| *b != 0)
    {
        return Err(Error::Invalid);
    }
    Ok(irq)
}
/// Resolve a `_PRT` package and the current resources of every named link device.
/// Only SourceIndex zero and a single current link interrupt are supported; no choices are
/// guessed from `_PRS`. Conflicting exact/wildcard entries are rejected transactionally.
pub fn entries(namespace: &Namespace<'_>, value: Value) -> Result<Vec<Entry>, Error> {
    let Value::Package(rows) = value else {
        return Err(Error::Invalid);
    };
    if rows.is_empty() {
        return Err(Error::Invalid);
    }
    if rows.len() > 256 {
        return Err(Error::Limit);
    }
    let mut entries: Vec<Entry> = Vec::new();
    for row in rows {
        let Value::Package(row) = row else {
            return Err(Error::Invalid);
        };
        if row.len() != 4 {
            return Err(Error::Invalid);
        }
        let address = row[0].integer()?;
        let pin = row[1].integer()?;
        let source_index = row[3].integer()?;
        let function = address & 0xffff;
        if address >> 16 > 31 || pin > 3 || (function > 7 && function != 0xffff) {
            return Err(Error::Invalid);
        }
        let interrupt = match &row[2] {
            Value::Integer(0) => Interrupt {
                gsi: u32::try_from(source_index).map_err(|_| Error::Invalid)?,
                active_low: true,
                level: true,
                shared: true,
            },
            Value::Reference(path) => {
                if source_index != 0 || !crate::is_usable(namespace.device_status(path)?.0) {
                    return Err(Error::Invalid);
                }
                let bytes = namespace.device_resources(path)?.ok_or(Error::Invalid)?;
                interrupt(&bytes)?
            }
            _ => return Err(Error::Invalid),
        };
        if !interrupt.level || !interrupt.shared {
            return Err(Error::Invalid);
        }
        let entry = Entry {
            device: (address >> 16) as u8,
            function: if function == 0xffff {
                None
            } else {
                Some(function as u8)
            },
            pin: pin as u8,
            interrupt,
        };
        if entries.iter().any(|old| {
            old.device == entry.device
                && old.pin == entry.pin
                && (old.function == entry.function
                    || old.function.is_none()
                    || entry.function.is_none())
        }) {
            return Err(Error::Ambiguous);
        }
        entries.push(entry);
    }
    Ok(entries)
}
/// Evaluate every `_PRT` under a specified root. Bridge ADRs must be valid PCI addresses.
/// At most eight tables/1024 entries are retained; a failed table refuses the entire result.
pub fn discover(namespace: &Namespace<'_>, root: &Path) -> Result<Vec<Table>, Error> {
    let mut tables = Vec::new();
    let mut count = 0;
    for path in namespace.devices() {
        let Some(value) = namespace.device_value(path, *b"_PRT")? else {
            continue;
        };
        let mut current = path.clone();
        let mut bridges = Vec::new();
        while &current != root {
            if current == Path::root() {
                return Err(Error::Invalid);
            }
            let adr = namespace
                .device_value(&current, *b"_ADR")?
                .ok_or(Error::Invalid)?
                .integer()?;
            if adr & !0x1f0007 != 0 {
                return Err(Error::Invalid);
            }
            bridges.push(adr as u32);
            current = current.parent();
        }
        bridges.reverse();
        let entries = entries(namespace, value)?;
        count += entries.len();
        if tables.len() == 8 || count > 1024 {
            return Err(Error::Limit);
        }
        tables.push(Table { bridges, entries });
    }
    if tables.iter().filter(|t| t.bridges.is_empty()).count() != 1 {
        return Err(Error::Invalid);
    }
    Ok(tables)
}

#[cfg(test)]
mod tests;
