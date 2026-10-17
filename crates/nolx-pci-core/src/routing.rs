//! PCI bus mapping and standard INTx bridge swizzling, independent of firmware bytecode.
use crate::{Bdf, ConfigSpace, Inventory};
/// A routing row on a mapped PCI bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Bus described by this routing scope.
    pub bus: u8,
    /// Device number.
    pub device: u8,
    /// Exact function or wildcard.
    pub function: Option<u8>,
    /// Zero-based INTA..INTD pin.
    pub pin: u8,
    /// Caller-owned index of the validated interrupt description.
    pub route: usize,
}
/// Missing/ambiguous topology or routing data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid pin, address, bus numbering or incomplete inventory.
    Topology,
    /// No routing entry exists; never infer a line-register route.
    Missing,
    /// Multiple matching parents or entries.
    Ambiguous,
}
fn complete(inventory: &Inventory) -> Result<(), Error> {
    if inventory.overflowed || inventory.refused_bridges != 0 {
        Err(Error::Topology)
    } else {
        Ok(())
    }
}
/// Map a root-relative firmware bridge ADR chain onto numbered PCI buses.
pub fn scope_bus<C: ConfigSpace>(
    cfg: &mut C,
    inventory: &Inventory,
    bridges: &[u32],
) -> Result<u8, Error> {
    complete(inventory)?;
    if bridges.len() > 32 {
        return Err(Error::Topology);
    }
    let mut bus = 0;
    let mut end = 255;
    let mut seen = [false; 256];
    seen[0] = true;
    for address in bridges {
        if address & !0x1f0007 != 0 {
            return Err(Error::Topology);
        }
        let bdf = Bdf {
            bus,
            device: (address >> 16) as u8,
            function: (address & 7) as u8,
        };
        if !inventory
            .as_slice()
            .iter()
            .any(|i| i.bdf == bdf && i.header_type == 1 && i.class == 6 && i.subclass == 4)
        {
            return Err(Error::Topology);
        }
        let buses = cfg.read_u32(bdf, 0x18);
        let secondary = (buses >> 8) as u8;
        let subordinate = (buses >> 16) as u8;
        if buses as u8 != bus
            || secondary == 0
            || secondary > subordinate
            || subordinate > end
            || seen[secondary as usize]
        {
            return Err(Error::Topology);
        }
        seen[secondary as usize] = true;
        bus = secondary;
        end = subordinate;
    }
    Ok(bus)
}
/// Look up an exact/wildcard row. An undescribed child bus swizzles through its unique
/// parent bridge; a described table with a missing row is refused, never silently bypassed.
/// PCI config pin is 1..4; firmware rows use 0..3. Interrupt Line is never read.
pub fn lookup<C: ConfigSpace>(
    cfg: &mut C,
    inventory: &Inventory,
    rows: &[Row],
    mut bdf: Bdf,
    pin: u8,
) -> Result<usize, Error> {
    complete(inventory)?;
    if !(1..=4).contains(&pin) || bdf.device >= 32 || bdf.function >= 8 {
        return Err(Error::Topology);
    }
    let mut pin = pin - 1;
    let mut seen = [false; 256];
    for _ in 0..32 {
        if seen[bdf.bus as usize] {
            return Err(Error::Topology);
        }
        seen[bdf.bus as usize] = true;
        let mut found = None;
        for row in rows.iter().filter(|r| {
            r.bus == bdf.bus
                && r.device == bdf.device
                && r.pin == pin
                && r.function.is_none_or(|f| f == bdf.function)
        }) {
            if found.is_some() {
                return Err(Error::Ambiguous);
            }
            found = Some(row.route);
        }
        if let Some(route) = found {
            return Ok(route);
        }
        if bdf.bus == 0 || rows.iter().any(|r| r.bus == bdf.bus) {
            return Err(Error::Missing);
        }
        let mut parent = None;
        for bridge in inventory
            .as_slice()
            .iter()
            .filter(|i| i.header_type == 1 && i.class == 6 && i.subclass == 4)
        {
            let buses = cfg.read_u32(bridge.bdf, 0x18);
            if (buses >> 8) as u8 == bdf.bus {
                if buses as u8 != bridge.bdf.bus || ((buses >> 16) as u8) < bdf.bus {
                    return Err(Error::Topology);
                }
                if parent.is_some() {
                    return Err(Error::Ambiguous);
                }
                parent = Some(bridge.bdf);
            }
        }
        pin = (pin + bdf.device) % 4;
        bdf = parent.ok_or(Error::Missing)?;
    }
    Err(Error::Topology)
}
#[cfg(test)]
mod tests;
