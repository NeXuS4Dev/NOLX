//! Boot-evaluated ACPI INTx routes and narrowly whitelisted PIRQ snapshots, plus the
//! audited kernel backend that applies planned ACPI link `_SRS` writes to those bytes.
use super::pci::ConfigAccess;
use crate::sync::IrqMutex;
use nolx_hal_core::{acpi::Madt, console::HalConsole};
use nolx_pci_core::{
    routing::{self, Row},
    Bdf, ConfigSpace,
};
use nolx_power_core::{
    aml::{self, FieldReader, Namespace, PciField, PciFieldWriteBackend, PciFieldWritePermission},
    routing::{Interrupt, LinkIrqNumber, LinkReprogramming, Table},
};
const LIMIT: usize = 1024;
const MAX_SEGMENTS: usize = 8;
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
#[derive(Clone, Copy)]
struct SegmentCache {
    rows: [Row; LIMIT],
    irqs: [Interrupt; LIMIT],
    count: usize,
    ready: bool,
}
impl Default for SegmentCache {
    fn default() -> Self {
        Self {
            rows: [EMPTY_ROW; LIMIT],
            irqs: [EMPTY_IRQ; LIMIT],
            count: 0,
            ready: false,
        }
    }
}
struct Cache {
    segments: [Option<SegmentCache>; MAX_SEGMENTS],
}
impl Cache {
    const fn new() -> Self {
        Self {
            segments: [None; MAX_SEGMENTS],
        }
    }
    fn get_mut(&mut self, segment: u16) -> Option<&mut SegmentCache> {
        if segment >= MAX_SEGMENTS as u16 {
            return None;
        }
        if self.segments[segment as usize].is_none() {
            self.segments[segment as usize] = Some(SegmentCache::default());
        }
        self.segments[segment as usize].as_mut()
    }
    fn get(&self, segment: u16) -> Option<&SegmentCache> {
        if segment >= MAX_SEGMENTS as u16 {
            return None;
        }
        self.segments[segment as usize].as_ref()
    }
}
static CACHE: IrqMutex<Cache> = IrqMutex::new(Cache::new());
struct SnapshotData {
    address: u32,
    offsets: [u16; 8],
    values: [u8; 8],
    count: usize,
}
/// Capture only PIIX4/ICH9 PIRQ routing bytes on the verified segment-zero host profile.
fn capture_data() -> SnapshotData {
    let mut result = SnapshotData {
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
        result.offsets[i] = offset as u16;
        result.values[i] = read_config_byte(bdf, offset as u16);
    }
    result
}
fn read_config_byte(bdf: Bdf, offset: u16) -> u8 {
    let value = ConfigAccess.read_u32(bdf, (offset & !3) as u8);
    (value >> ((offset & 3) * 8)) as u8
}
fn write_config_byte(bdf: Bdf, offset: u16, byte: u8) {
    let shift = (offset & 3) * 8;
    let old = ConfigAccess.read_u32(bdf, (offset & !3) as u8);
    let value = (old & !(0xff << shift)) | (u32::from(byte) << shift);
    ConfigAccess.write_u32(bdf, (offset & !3) as u8, value);
}
/// A snapshot of the verified board's PIRQ bytes, never an AML hardware write capability.
///
/// AML evaluation sees one consistent snapshot. The only refresh is the explicit one an
/// audited `_SRS` transaction performs after its writes have read back, so a re-evaluated
/// `_CRS` observes post-write hardware without giving ordinary evaluation a live view
/// that could change underneath a method halfway through its execution.
pub struct Snapshot {
    data: IrqMutex<SnapshotData>,
}
impl Snapshot {
    /// Capture the verified board's PIRQ bytes; any other profile yields an empty snapshot.
    pub fn capture() -> Self {
        Self {
            data: IrqMutex::new(capture_data()),
        }
    }
    /// Re-read the PIRQ bytes. Called only as an `_SRS` transaction postcondition, after
    /// the transaction's own byte readbacks have already matched.
    pub fn refresh(&self) {
        let fresh = capture_data();
        *self.data.lock() = fresh;
    }
    fn fields(&self) -> ([PciField; 8], usize) {
        let data = self.data.lock();
        let mut fields = [PciField {
            address: 0,
            offset: 0,
            bytes: 1,
        }; 8];
        for (index, field) in fields.iter_mut().take(data.count).enumerate() {
            *field = PciField {
                address: data.address,
                offset: data.offsets[index],
                bytes: 1,
            };
        }
        (fields, data.count)
    }
}
impl FieldReader for Snapshot {
    fn read(&self, field: PciField) -> Result<u64, aml::Error> {
        let data = self.data.lock();
        if field.address != data.address || field.bytes != 1 {
            return Err(aml::Error::Unsupported(0x5b81));
        }
        let index = data.offsets[..data.count]
            .iter()
            .position(|o| *o == field.offset)
            .ok_or(aml::Error::Unsupported(0x5b81))?;
        Ok(u64::from(data.values[index]))
    }
}
/// PCI byte access for one audited `_SRS` transaction.
///
/// The backend refuses every field outside the captured snapshot's verified PIRQ byte
/// set, so not even a caller error in the permission list can widen it into general
/// configuration access. A byte write preserves the other bytes of its dword; the
/// transaction's readback, not this backend, proves the device accepted the value.
/// Configuration access is serialized by `ConfigAccess`, and route evaluation runs on the
/// boot processor before any driver starts, so no concurrent configuration user exists.
struct ConfigBackend<'a> {
    snapshot: &'a Snapshot,
}
impl ConfigBackend<'_> {
    fn bdf(&self, field: PciField) -> Result<Bdf, ()> {
        let (fields, count) = self.snapshot.fields();
        if !fields[..count].contains(&field) {
            return Err(());
        }
        Ok(Bdf {
            segment: 0,
            bus: 0,
            device: u8::try_from(field.address >> 16).map_err(|_| ())?,
            function: u8::try_from(field.address & 7).map_err(|_| ())?,
        })
    }
}
impl PciFieldWriteBackend for ConfigBackend<'_> {
    type Error = ();

    fn read_byte(&mut self, field: PciField) -> Result<u8, ()> {
        Ok(read_config_byte(self.bdf(field)?, field.offset))
    }

    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), ()> {
        write_config_byte(self.bdf(field)?, field.offset, value);
        Ok(())
    }
}
#[derive(Debug)]
enum SrsError {
    Route(nolx_power_core::routing::Error),
    Permission(aml::Error),
    Refused(&'static str),
    Transaction(aml::PciFieldWriteTransactionError<(), nolx_power_core::routing::Error>),
}
impl core::fmt::Display for SrsError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Route(error) => write!(formatter, "route selection or planning: {error:?}"),
            Self::Permission(error) => write!(formatter, "write permission: {error:?}"),
            Self::Refused(reason) => write!(formatter, "{reason}"),
            Self::Transaction(error) => write!(formatter, "write transaction: {error:?}"),
        }
    }
}
/// Map an ISA IRQ from a link `_PRS` to its GSI through the MADT source overrides.
///
/// Current-route decoding has always treated the descriptor number itself as the GSI, so
/// reprogramming accepts only the mappings that model already relies on: identity, or an
/// override that keeps the same number. A renumbering override would let `_PRS` steer a
/// link at a different IOAPIC input than the one current `_CRS` decoding reports, so it
/// refuses the option instead of silently widening the supported firmware model.
fn isa_to_gsi(madt: &Madt, irq: u8) -> Option<u32> {
    match madt
        .interrupt_overrides()
        .iter()
        .find(|entry| entry.bus == 0 && entry.source == irq)
    {
        Some(entry) if entry.global_interrupt == u32::from(irq) => Some(entry.global_interrupt),
        Some(_) => None,
        None => Some(u32::from(irq)),
    }
}
/// Plan one link's `_SRS`, apply its byte-write journal, and confirm the selection by
/// re-evaluating `_CRS` against refreshed hardware state. Permissions cover exactly the
/// snapshot's PIRQ bytes and exactly the selected route's encoded value: the firmware
/// method may choose which link byte to steer, but never what to write into it.
fn apply_link_srs(
    namespace: &Namespace<'_>,
    snapshot: &Snapshot,
    item: &LinkReprogramming,
    console: &mut dyn HalConsole,
) -> Result<(), SrsError> {
    let value = match item.selection.option.number {
        LinkIrqNumber::Isa(irq) => irq,
        LinkIrqNumber::Gsi(gsi) => u8::try_from(gsi)
            .map_err(|_| SrsError::Refused("selected GSI does not fit a PIRQ byte"))?,
    };
    let (fields, count) = snapshot.fields();
    if count == 0 {
        return Err(SrsError::Refused("no verified PIRQ fields on this board"));
    }
    let mut permissions = alloc::vec::Vec::with_capacity(count);
    for field in &fields[..count] {
        permissions.push(
            PciFieldWritePermission::from_values(*field, &[value]).map_err(SrsError::Permission)?,
        );
    }
    let effects = nolx_power_core::routing::plan_link_srs(
        namespace,
        &item.link,
        &item.selection,
        &permissions,
    )
    .map_err(SrsError::Route)?;
    if effects.writes.is_empty() {
        return Err(SrsError::Refused("_SRS planned no writes"));
    }
    let writes = effects.writes.len();
    let mut backend = ConfigBackend { snapshot };
    aml::apply_pci_field_writes(
        &effects,
        &permissions,
        &mut backend,
        || -> Result<(), nolx_power_core::routing::Error> {
            snapshot.refresh();
            let bytes = namespace
                .device_resources(&item.link)?
                .ok_or(nolx_power_core::routing::Error::Invalid)?;
            if nolx_power_core::routing::interrupt(&bytes)? == item.selection.interrupt {
                Ok(())
            } else {
                Err(nolx_power_core::routing::Error::Invalid)
            }
        },
    )
    .map_err(SrsError::Transaction)?;
    let _ = writeln!(
        console,
        "pci: ACPI link {} reprogrammed via _SRS: GSI {} confirmed by re-evaluated _CRS, {} byte write(s)",
        item.link, item.selection.interrupt.gsi, writes
    );
    Ok(())
}
/// Test-only: disable every verified PIRQ route before AML evaluation, so route
/// publication can only succeed by recovering through `_SRS`. It proves the current
/// routes were genuinely unavailable instead of silently retained.
#[cfg(feature = "hal-pci-link-srs-test")]
pub fn disable_links_for_test(console: &mut dyn HalConsole) {
    let data = capture_data();
    assert!(
        data.count > 0,
        "link _SRS fixture requires a verified PIRQ board profile"
    );
    let bdf = Bdf {
        segment: 0,
        bus: 0,
        device: (data.address >> 16) as u8,
        function: 0,
    };
    for index in 0..data.count {
        write_config_byte(bdf, data.offsets[index], 0x80);
        assert_eq!(read_config_byte(bdf, data.offsets[index]), 0x80);
    }
    let _ = writeln!(
        console,
        "pci: link test: disabled {} PIRQ route(s) before AML evaluation; _SRS recovery required",
        data.count
    );
}
/// Revoke all routes before discovery or after truncated/incomplete firmware metadata.
pub fn revoke() {
    let mut cache = CACHE.lock();
    for segment_cache in cache.segments.iter_mut().flatten() {
        segment_cache.ready = false;
        segment_cache.count = 0;
    }
}
/// Map evaluated bridge scopes against the existing numbered topology for a segment.
/// No route is usable until every table and flag-sharing relationship has validated.
pub fn publish(segment: u16, tables: &[Table], console: &mut dyn HalConsole) -> Result<(), routing::Error> {
    revoke();
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let mut cache = CACHE.lock();
    let segment_cache = cache.get_mut(segment).ok_or(routing::Error::Topology)?;
    for table in tables {
        let bus = routing::scope_bus(&mut ConfigAccess, &inventory, &table.bridges)?;
        if segment_cache.rows[..segment_cache.count].iter().any(|r| r.bus == bus) {
            return Err(routing::Error::Ambiguous);
        }
        for entry in &table.entries {
            if segment_cache.count == LIMIT {
                return Err(routing::Error::Topology);
            }
            if segment_cache.irqs[..segment_cache.count]
                .iter()
                .any(|irq| irq.gsi == entry.interrupt.gsi && *irq != entry.interrupt)
            {
                return Err(routing::Error::Ambiguous);
            }
            let n = segment_cache.count;
            segment_cache.rows[n] = Row {
                bus,
                device: entry.device,
                function: entry.function,
                pin: entry.pin,
                route: n,
            };
            segment_cache.irqs[n] = entry.interrupt;
            segment_cache.count += 1;
        }
    }
    segment_cache.ready = true;
    let _ = writeln!(
        console,
        "pci: segment {:04x}: evaluated ACPI INTx routing authorized: {} rows, {} scopes",
        segment,
        segment_cache.count,
        tables.len()
    );
    Ok(())
}
/// Resolve a PCI pin through actual `_PRT` data, using swizzling only on undescribed buses.
/// An available-but-refused ACPI namespace never downgrades to the Interrupt Line byte.
pub fn lookup(bdf: Bdf) -> Option<Interrupt> {
    let cache = CACHE.lock();
    let segment_cache = cache.get(bdf.segment)?;
    if !segment_cache.ready {
        // Fallback to legacy Interrupt Line byte only for segment 0 without ACPI
        if bdf.segment == 0
            && matches!(
                super::acpi::root_resources(),
                super::acpi::RootResourceState::Absent
            )
        {
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
        return None;
    }
    let pin = ((ConfigAccess.read_u32(bdf, 0x3c) >> 8) & 255) as u8;
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let index = routing::lookup(
        &mut ConfigAccess,
        &inventory,
        &segment_cache.rows[..segment_cache.count],
        bdf,
        pin,
    )
    .ok()?;
    segment_cache.irqs.get(index).copied()
}
/// Evaluate the root/bridge `_PRT` tables after notifying the supported `_PIC` method.
///
/// Links whose current route this platform cannot program are reprogrammed first through
/// the audited `_SRS` transaction; publication then comes from a fresh current-route
/// discovery, never from the plan. Any refusal revokes every route instead of downgrading
/// to the firmware Interrupt Line byte. If an earlier link was already reprogrammed when
/// a later one refuses, firmware steering stays changed but unusable, which is the
/// fail-closed outcome the earlier current-route refusal already produced. The preflight
/// is APIC-only: without a local APIC there is no IOAPIC route table to judge current
/// routes against, and INTx consumers refuse separately on that profile.
pub fn evaluate(
    namespace: &Namespace<'_>,
    snapshot: &Snapshot,
    madt: &Madt,
    console: &mut dyn HalConsole,
) {
    // Discover all root bridges (multi-segment support)
    let root_bridges = match nolx_power_core::root::discover(namespace) {
        Ok(bridges) => bridges,
        Err(_) => {
            revoke();
            let _ = writeln!(
                console,
                "pci: ACPI INTx evaluation refused: {:?}",
                nolx_power_core::routing::Error::Invalid
            );
            return;
        }
    };
    if super::apic::is_enabled() {
        // Plan link reprogramming for all segments
        let segment_plans = match nolx_power_core::routing::plan_link_reprogramming_all(
            namespace,
            &root_bridges,
            |irq| isa_to_gsi(madt, irq),
            |interrupt| super::ioapic::pci_route_supported(interrupt.gsi),
        ) {
            Ok(plans) => plans,
            Err(error) => {
                revoke();
                let _ = writeln!(console, "pci: ACPI INTx evaluation refused: {error:?}");
                return;
            }
        };
        // Log preflight summary for all segments
        for seg_plan in &segment_plans {
            let _ = writeln!(
                console,
                "pci: segment {:04x}: ACPI link preflight: {} link(s), {} require _SRS",
                seg_plan.segment,
                seg_plan.plan.evaluated,
                seg_plan.plan.reprogramming.len()
            );
        }
        #[cfg(feature = "hal-pci-link-srs-test")]
        {
            for seg_plan in &segment_plans {
                for (link, interrupt) in &seg_plan.plan.retained {
                    let _ = writeln!(
                        console,
                        "pci: segment {:04x}: link test: retained {} at GSI {}",
                        seg_plan.segment, link, interrupt.gsi
                    );
                }
            }
        }
        // Apply _SRS for each segment's reprogramming items
        for seg_plan in &segment_plans {
            for item in &seg_plan.plan.reprogramming {
                if let Err(error) = apply_link_srs(namespace, snapshot, item, console) {
                    revoke();
                    let _ = writeln!(
                        console,
                        "pci: segment {:04x}: ACPI INTx evaluation refused: link _SRS: {error}",
                        seg_plan.segment
                    );
                    return;
                }
            }
        }
    }
    // Discover and publish routing tables for each segment
    let all_tables = match nolx_power_core::routing::discover_all(namespace, &root_bridges) {
        Ok(tables) => tables,
        Err(error) => {
            revoke();
            let _ = writeln!(console, "pci: ACPI INTx evaluation refused: {error:?}");
            return;
        }
    };
    for (segment, tables) in all_tables {
        if let Err(error) = publish(segment, &tables, console) {
            revoke();
            let _ = writeln!(
                console,
                "pci: segment {:04x}: ACPI INTx scope mapping refused: {error:?}",
                segment
            );
            return;
        }
    }
}
