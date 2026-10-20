//! Boot-evaluated ACPI INTx routes and narrowly whitelisted, read-only PIRQ snapshots.
use super::pci::ConfigAccess;
use crate::sync::IrqMutex;
use nolx_hal_core::console::HalConsole;
use nolx_pci_core::{
    routing::{self, Row},
    Bdf, ConfigSpace,
};
use nolx_power_core::{
    aml::{self, FieldReader, Namespace, PciField},
    routing::{Interrupt, Table},
};
const LIMIT: usize = 1024;
const EMPTY_ROW: Row = Row {
    bus: 0,
    device: 0,
    function: None,
    pin: 0,
    route: 0,
};
const EMPTY_IRQ: Interrupt = Interrupt {
    gsi: 0,
    active_low: false,
    level: false,
    shared: false,
};
struct Cache {
    rows: [Row; LIMIT],
    irqs: [Interrupt; LIMIT],
    count: usize,
    ready: bool,
}
static CACHE: IrqMutex<Cache> = IrqMutex::new(Cache {
    rows: [EMPTY_ROW; LIMIT],
    irqs: [EMPTY_IRQ; LIMIT],
    count: 0,
    ready: false,
});
/// A one-time snapshot, never an AML hardware write capability.
pub struct Snapshot {
    address: u32,
    offsets: [u16; 8],
    values: [u8; 8],
    count: usize,
}
impl Snapshot {
    /// Capture only PIIX4/ICH9 PIRQ routing bytes on the verified segment-zero host profile.
    pub fn capture() -> Self {
        let mut result = Self {
            address: 0,
            offsets: [0; 8],
            values: [0; 8],
            count: 0,
        };
        let host = ConfigAccess.read_u32(
            Bdf {
                segment: 0,
                bus: 0,
                device: 0,
                function: 0,
            },
            0,
        );
        let (device, id, count) = match host {
            0x12378086 => (1, 0x70008086, 4),
            0x29c08086 => (31, 0x29188086, 8),
            _ => return result,
        };
        let bdf = Bdf {
            segment: 0,
            bus: 0,
            device,
            function: 0,
        };
        if ConfigAccess.read_u32(bdf, 0) != id {
            return result;
        }
        result.address = u32::from(device) << 16;
        result.count = count;
        for i in 0..count {
            let offset = if i < 4 { 0x60 + i } else { 0x68 + i - 4 };
            let value = ConfigAccess.read_u32(bdf, (offset & !3) as u8);
            result.offsets[i] = offset as u16;
            result.values[i] = (value >> ((offset & 3) * 8)) as u8;
        }
        result
    }
}
impl FieldReader for Snapshot {
    fn read(&self, field: PciField) -> Result<u64, aml::Error> {
        if field.address != self.address || field.bytes != 1 {
            return Err(aml::Error::Unsupported(0x5b81));
        }
        let index = self.offsets[..self.count]
            .iter()
            .position(|o| *o == field.offset)
            .ok_or(aml::Error::Unsupported(0x5b81))?;
        Ok(u64::from(self.values[index]))
    }
}
/// Revoke all routes before discovery or after truncated/incomplete firmware metadata.
pub fn revoke() {
    let mut cache = CACHE.lock();
    cache.ready = false;
    cache.count = 0;
}
/// Map evaluated bridge scopes against the existing numbered segment-zero topology.
/// No route is usable until every table and flag-sharing relationship has validated.
pub fn publish(tables: &[Table], console: &mut dyn HalConsole) -> Result<(), routing::Error> {
    revoke();
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let mut cache = CACHE.lock();
    for table in tables {
        let bus = routing::scope_bus(&mut ConfigAccess, &inventory, &table.bridges)?;
        if cache.rows[..cache.count].iter().any(|r| r.bus == bus) {
            return Err(routing::Error::Ambiguous);
        }
        for entry in &table.entries {
            if cache.count == LIMIT {
                return Err(routing::Error::Topology);
            }
            if cache.irqs[..cache.count]
                .iter()
                .any(|irq| irq.gsi == entry.interrupt.gsi && *irq != entry.interrupt)
            {
                return Err(routing::Error::Ambiguous);
            }
            let n = cache.count;
            cache.rows[n] = Row {
                bus,
                device: entry.device,
                function: entry.function,
                pin: entry.pin,
                route: n,
            };
            cache.irqs[n] = entry.interrupt;
            cache.count += 1;
        }
    }
    cache.ready = true;
    let _ = writeln!(
        console,
        "pci: evaluated ACPI INTx routing authorized: {} rows, {} scopes",
        cache.count,
        tables.len()
    );
    Ok(())
}
/// Resolve a PCI pin through actual `_PRT` data, using swizzling only on undescribed buses.
/// An available-but-refused ACPI namespace never downgrades to the Interrupt Line byte.
pub fn lookup(bdf: Bdf) -> Option<Interrupt> {
    // The published `_PRT`/PIRQ snapshot is segment-zero only; never route an identical BDF
    // from another segment through that cache or through legacy Interrupt Line fallback.
    if bdf.segment != 0 {
        return None;
    }
    let pin = ((ConfigAccess.read_u32(bdf, 0x3c) >> 8) & 255) as u8;
    let cache = CACHE.lock();
    if cache.ready {
        let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
        let index = routing::lookup(
            &mut ConfigAccess,
            &inventory,
            &cache.rows[..cache.count],
            bdf,
            pin,
        )
        .ok()?;
        return cache.irqs.get(index).copied();
    }
    if matches!(
        super::acpi::root_resources(),
        super::acpi::RootResourceState::Absent
    ) {
        let irq = (ConfigAccess.read_u32(bdf, 0x3c) & 255) as u8;
        if (3..16).contains(&irq) && irq != 12 {
            return Some(Interrupt {
                gsi: u32::from(irq),
                active_low: true,
                level: true,
                shared: true,
            });
        }
    }
    None
}
/// Evaluate the root/bridge `_PRT` tables after notifying the supported `_PIC` method.
pub fn evaluate(namespace: &Namespace<'_>, console: &mut dyn HalConsole) {
    let result = (|| {
        nolx_power_core::routing::discover(
            namespace,
            &nolx_power_core::root::root_path(namespace)
                .map_err(|_| nolx_power_core::routing::Error::Invalid)?,
        )
    })();
    match result {
        Ok(tables) => {
            if let Err(error) = publish(&tables, console) {
                revoke();
                let _ = writeln!(console, "pci: ACPI INTx scope mapping refused: {error:?}");
            }
        }
        Err(error) => {
            revoke();
            let _ = writeln!(console, "pci: ACPI INTx evaluation refused: {error:?}");
        }
    }
}
