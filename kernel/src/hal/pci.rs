//! Serialized legacy/ECAM PCI configuration transport and boot-time inventories.
//!
//! The mechanism is the one every x86 PC has: write a dword address to `0xCF8`, then read or
//! write the dword at `0xCFC`. The bit-level rules -- address encoding, header parsing, BAR
//! sizing, capability walking, bridge-aware enumeration -- live in `nolx-pci-core`, where they
//! are host-tested. Drivers use [`ConfigAccess`] for ECAM reads/writes on mapped ranges,
//! with serialized legacy-port fallback restricted to segment zero.
//!
//! The address and data ports are a pair: any access between them would select a different
//! register. Discovery runs on the boot processor before the periodic timer starts, so nothing
//! else touches the pair. [`LEGACY_LOCK`] also serializes and masks interrupts around
//! every address/data transaction, including calls after boot.

use nolx_hal_core::console::HalConsole;
use nolx_pci_core::{Bar, BarSet, Bdf, ConfigSpace};

use super::{inl, outl};
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use nolx_hal_core::acpi::mcfg::MAX_ALLOCATIONS;
use nolx_pci_core::ecam::{AccessWidth, EcamError, EcamWindow, ExtendedConfigSpace, SegmentConfig};

static LEGACY_LOCK: IrqMutex<()> = IrqMutex::new(());
static ECAM_READS: AtomicUsize = AtomicUsize::new(0);
static ECAM_WRITES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct MappedAllocation {
    window: EcamWindow,
    region: Region,
    segment: u16,
    first: u8,
    last: u8,
}

static MAPPINGS: IrqMutex<[Option<MappedAllocation>; MAX_ALLOCATIONS]> =
    IrqMutex::new([None; MAX_ALLOCATIONS]);

/// Native configuration transport: mapped ECAM, or legacy ports for uncovered segment zero.
pub struct ConfigAccess;

impl ExtendedConfigSpace for ConfigAccess {
    fn read_dword(&mut self, segment: u16, bdf: Bdf, offset: u16) -> Result<u32, EcamError> {
        if bdf.segment != segment {
            return Err(EcamError::OutsideWindow);
        }
        let mappings = MAPPINGS.lock();
        for mapping in mappings.iter().flatten() {
            if mapping.segment == segment && (mapping.first..=mapping.last).contains(&bdf.bus) {
                let physical = mapping
                    .window
                    .address(segment, bdf, offset, AccessWidth::Dword)?;
                let (start, _) = mapping.window.physical_range();
                let value = mapping
                    .region
                    .read32(physical - start)
                    .map_err(|_| EcamError::InvalidOffset)?;
                ECAM_READS.fetch_add(1, Ordering::Relaxed);
                return Ok(value);
            }
        }
        if segment == 0 && offset < 256 && offset.is_multiple_of(4) {
            Bdf::new(bdf.bus, bdf.device, bdf.function).map_err(|_| EcamError::InvalidBdf)?;
            Ok(LegacyPorts.read_u32(bdf, offset as u8))
        } else {
            Err(EcamError::OutsideWindow)
        }
    }

    fn write_dword(
        &mut self,
        segment: u16,
        bdf: Bdf,
        offset: u16,
        value: u32,
    ) -> Result<(), EcamError> {
        if bdf.segment != segment {
            return Err(EcamError::OutsideWindow);
        }
        let mappings = MAPPINGS.lock();
        for mapping in mappings.iter().flatten() {
            if mapping.segment == segment && (mapping.first..=mapping.last).contains(&bdf.bus) {
                let physical = mapping
                    .window
                    .address(segment, bdf, offset, AccessWidth::Dword)?;
                let (start, _) = mapping.window.physical_range();
                mapping
                    .region
                    .write32(physical - start, value)
                    .map_err(|_| EcamError::InvalidOffset)?;
                ECAM_WRITES.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }
        if segment == 0 && offset < 256 && offset.is_multiple_of(4) {
            Bdf::new(bdf.bus, bdf.device, bdf.function).map_err(|_| EcamError::InvalidBdf)?;
            LegacyPorts.write_u32(bdf, offset as u8, value);
            Ok(())
        } else {
            Err(EcamError::OutsideWindow)
        }
    }
}

impl ConfigSpace for ConfigAccess {
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32 {
        self.read_dword(bdf.segment, bdf, u16::from(offset & !3))
            .unwrap_or(u32::MAX)
    }
    fn write_u32(&mut self, bdf: Bdf, offset: u8, value: u32) {
        let _ = self.write_dword(bdf.segment, bdf, u16::from(offset & !3), value);
    }
}

/// Select configuration transport after MMIO initialization, before any driver starts.
/// Mapping failures retain legacy access only for segment zero; other segments never alias it.
pub fn select_transport(console: &mut dyn HalConsole) {
    let Some(mcfg) = super::acpi::discover().and_then(|platform| platform.mcfg) else {
        let _ = writeln!(console, "pci: configuration transport=legacy");
        return;
    };
    let mut mappings = MAPPINGS.lock();
    for (slot, allocation) in mcfg.allocations().iter().enumerate() {
        let Ok(window) = EcamWindow::new(
            allocation.base,
            allocation.segment,
            allocation.start_bus,
            allocation.end_bus,
        ) else {
            continue;
        };
        let (start, bytes) = window.physical_range();
        let Ok(region) = mmio::map(start, bytes) else {
            let _ = writeln!(
                console,
                "pci: ECAM segment {} buses {}..{} mapping refused",
                allocation.segment, allocation.start_bus, allocation.end_bus
            );
            continue;
        };
        if allocation.segment == 0 {
            let bdf = Bdf {
                segment: 0,
                bus: allocation.start_bus,
                device: 0,
                function: 0,
            };
            let expected = LegacyPorts.read_u32(bdf, 0);
            if expected != u32::MAX && region.read32(0) != Ok(expected) {
                let _ = writeln!(
                    console,
                    "pci: ECAM identity mismatch; legacy fallback retained"
                );
                continue;
            }
        }
        mappings[slot] = Some(MappedAllocation {
            window,
            region,
            segment: allocation.segment,
            first: allocation.start_bus,
            last: allocation.end_bus,
        });
        let _ = writeln!(
            console,
            "pci: configuration transport=ECAM segment {} buses {}..{}",
            allocation.segment, allocation.start_bus, allocation.end_bus
        );
    }
}

/// Enumerate segment-zero legacy roots plus every successfully mapped MCFG allocation.
///
/// Each allocation is treated as a declared root-bus range, so equal bus/device/function
/// numbers in different segments remain distinct. Overlapping discovery through a bridge is
/// deduplicated by the segment-qualified BDF.
pub fn enumerate_native() -> nolx_pci_core::Inventory {
    let mappings = *MAPPINGS.lock();

    // Fold all segment-zero MCFG ranges into the legacy root scan. ConfigAccess selects ECAM
    // within mapped ranges and legacy ports elsewhere; one visited-bus map avoids double-counting
    // bridges when an MCFG range overlaps bus 0 or a bus reached through a bridge.
    let mut segment_zero_roots = [(0, 0); MAX_ALLOCATIONS + 1];
    segment_zero_roots[0] = (0, 0);
    let mut segment_zero_count = 1;
    for allocation in mappings.iter().flatten() {
        if allocation.segment == 0 {
            segment_zero_roots[segment_zero_count] = (allocation.first, allocation.last);
            segment_zero_count += 1;
        }
    }
    let mut combined = nolx_pci_core::enumerate_ranges(
        &mut ConfigAccess,
        &segment_zero_roots[..segment_zero_count],
    );

    // Each nonzero segment has its own root set and visited map. Group disjoint mappings before
    // scanning, so cross-allocation bridges and repeated roots are each accounted exactly once.
    for (index, allocation) in mappings.iter().enumerate() {
        let Some(allocation) = allocation.as_ref() else {
            continue;
        };
        if allocation.segment == 0
            || mappings[..index]
                .iter()
                .flatten()
                .any(|previous| previous.segment == allocation.segment)
        {
            continue;
        }
        let mut roots = [(0, 0); MAX_ALLOCATIONS];
        let mut root_count = 0;
        for range in mappings
            .iter()
            .flatten()
            .filter(|range| range.segment == allocation.segment)
        {
            roots[root_count] = (range.first, range.last);
            root_count += 1;
        }
        let mut transport = ConfigAccess;
        let mut config = SegmentConfig {
            transport: &mut transport,
            segment: allocation.segment,
        };
        let found = nolx_pci_core::enumerate_ranges(&mut config, &roots[..root_count]);
        combined.refused_bridges = combined
            .refused_bridges
            .saturating_add(found.refused_bridges);
        combined.overflowed |= found.overflowed;
        for info in found.as_slice() {
            if combined.as_slice().iter().any(|old| old.bdf == info.bdf) {
                continue;
            }
            let _ = combined.push(*info);
        }
    }
    combined
}

/// The legacy configuration mechanism over the two architected ports.
pub struct LegacyPorts;

impl ConfigSpace for LegacyPorts {
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32 {
        if bdf.segment != 0 || Bdf::new(bdf.bus, bdf.device, bdf.function).is_err() {
            return u32::MAX;
        }
        let _guard = LEGACY_LOCK.lock();
        // SAFETY: 0xCF8 and 0xCFC are the configuration ports the PCI specification defines on
        // every x86 PC. A configuration read has no side effect of its own; an absent function
        // reads as all ones, which discovery treats as "no device here".
        unsafe {
            outl(
                nolx_pci_core::CONFIG_ADDRESS_PORT,
                nolx_pci_core::config_address(bdf, offset),
            );
            inl(nolx_pci_core::CONFIG_DATA_PORT)
        }
    }

    fn write_u32(&mut self, bdf: Bdf, offset: u8, value: u32) {
        if bdf.segment != 0 || Bdf::new(bdf.bus, bdf.device, bdf.function).is_err() {
            return;
        }
        let _guard = LEGACY_LOCK.lock();
        // SAFETY: as [`LegacyPorts::read_u32`]. Callers are PCI configuration algorithms
        // (BAR probing, device enable and interrupt setup); the pair stays locked.
        unsafe {
            outl(
                nolx_pci_core::CONFIG_ADDRESS_PORT,
                nolx_pci_core::config_address(bdf, offset),
            );
            outl(nolx_pci_core::CONFIG_DATA_PORT, value);
        }
    }
}

/// Enumerate PCI configuration space across segment-zero roots and mapped MCFG allocations.
pub fn init(console: &mut dyn HalConsole) {
    let inventory = enumerate_native();
    let note = if inventory.overflowed {
        ", inventory incomplete"
    } else {
        ""
    };
    let _ = writeln!(
        console,
        "pci: {} function(s), {} bridge(s) refused{note}",
        inventory.len, inventory.refused_bridges
    );
    for info in inventory.as_slice() {
        if info.bdf.segment == 0 {
            let _ = writeln!(
                console,
                "pci: {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x} if {:02x} rev {:02x} hdr {}",
                info.bdf.bus,
                info.bdf.device,
                info.bdf.function,
                info.vendor_id,
                info.device_id,
                info.class,
                info.subclass,
                info.prog_if,
                info.revision,
                info.header_type
            );
        } else {
            let _ = writeln!(
                console,
                "pci: segment {:04x} {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x} if {:02x} rev {:02x} hdr {}",
                info.bdf.segment,
                info.bdf.bus,
                info.bdf.device,
                info.bdf.function,
                info.vendor_id,
                info.device_id,
                info.class,
                info.subclass,
                info.prog_if,
                info.revision,
                info.header_type
            );
        }
    }
    let allocations = *MAPPINGS.lock();
    for allocation in allocations.iter().flatten() {
        let functions = nolx_pci_core::enumerate_range(
            &mut SegmentConfig {
                transport: &mut ConfigAccess,
                segment: allocation.segment,
            },
            allocation.first,
            allocation.last,
        );
        let _ = writeln!(
            console,
            "pci: ECAM segment {} buses {}..{}: {} function(s), incomplete={}",
            allocation.segment,
            allocation.first,
            allocation.last,
            functions.len,
            functions.overflowed
        );
    }
    let _ = writeln!(
        console,
        "pci: ECAM driver accesses: {} reads, {} writes",
        ECAM_READS.load(Ordering::Relaxed),
        ECAM_WRITES.load(Ordering::Relaxed)
    );
    probe_ecam(console, &inventory);
    // These were sized before any driver owned DMA. Do not disable live device decoding.
    for (bdf, bars) in BAR_CACHE.lock().iter().flatten() {
        report_bars(console, *bdf, bars);
    }
}

/// Print every implemented BAR of one type-0 function, with its decoded base and window size.
fn report_bars(console: &mut dyn HalConsole, bdf: Bdf, bars: &BarSet) {
    let (bus, device, function) = (bdf.bus, bdf.device, bdf.function);
    for (slot, bar) in bars.slots.iter().enumerate() {
        let size = bars.sizes[slot].unwrap_or(0);
        let (kind, base, prefetch) = match *bar {
            Bar::Io { port } => ("io", u64::from(port), false),
            Bar::Memory32 { base, prefetchable } => ("mem32", u64::from(base), prefetchable),
            Bar::Memory64 { base, prefetchable } => ("mem64", base, prefetchable),
            Bar::Unused | Bar::Reserved => ("none", 0, false),
        };
        if size == 0 {
            continue;
        }
        let _ = writeln!(
            console,
            "pci: {bus:02x}:{device:02x}.{function} bar{slot} {kind} {base:#x} size {size:#x} prefetch={prefetch}"
        );
    }
}

/// Independently check segment-zero ECAM identities and extended headers against legacy ports.
/// Called after the memory manager starts and before the periodic timer is enabled.
fn probe_ecam(console: &mut dyn HalConsole, inventory: &nolx_pci_core::Inventory) {
    use nolx_pci_core::ecam::{AccessWidth, EcamWindow};

    let Some(mcfg) = super::acpi::discover().and_then(|platform| platform.mcfg) else {
        let _ = writeln!(
            console,
            "pci: no usable MCFG; legacy configuration retained"
        );
        return;
    };
    let mut matched = 0;
    let mut refused = 0;
    for info in inventory.as_slice() {
        if info.bdf.segment != 0 {
            continue;
        }
        // This legacy-equivalence check applies only to segment zero. Never compare a
        // nonzero-segment identity with legacy ports, even when bus numbers match.
        let Some(allocation) = mcfg.allocations().iter().find(|allocation| {
            allocation.segment == 0
                && allocation.start_bus <= info.bdf.bus
                && info.bdf.bus <= allocation.end_bus
        }) else {
            continue;
        };
        let window = match EcamWindow::new(
            allocation.base,
            allocation.segment,
            allocation.start_bus,
            allocation.end_bus,
        ) {
            Ok(window) => window,
            Err(_) => {
                refused += 1;
                continue;
            }
        };
        let physical = match window.address(0, info.bdf, 0, AccessWidth::Dword) {
            Ok(physical) => physical,
            Err(_) => {
                refused += 1;
                continue;
            }
        };
        let mappings = MAPPINGS.lock();
        let Some(mapping) = mappings.iter().flatten().find(|mapping| {
            mapping.segment == 0 && (mapping.first..=mapping.last).contains(&info.bdf.bus)
        }) else {
            refused += 1;
            continue;
        };
        let region = mapping.region;
        let (base, _) = mapping.window.physical_range();
        let offset = physical - base;
        // Keep the cross-check independent of the newly selected native transport:
        // `inventory` itself now comes from ECAM, so comparing it only to ECAM
        // would be circular and would no longer prove PCI-018's legacy equivalence.
        let expected = LegacyPorts.read_u32(info.bdf, 0);
        let discovered = u32::from(info.vendor_id) | (u32::from(info.device_id) << 16);
        if expected != discovered || region.read32(offset) != Ok(expected) {
            refused += 1;
            continue;
        }
        // The extended capability header is read-only. Zero/all-ones is valid
        // when no extended capabilities exist; do not mistake it for a fault.
        if let Ok(header) = region.read32(offset + 0x100) {
            matched += 1;
            let _ = writeln!(
                console,
                "pci: ECAM {:02x}:{:02x}.{} identity matched, extended header {header:#010x}",
                info.bdf.bus, info.bdf.device, info.bdf.function
            );
        } else {
            refused += 1;
        }
    }
    let _ = writeln!(
        console,
        "pci: ECAM {matched} identity match(es), {refused} refused; legacy fallback available"
    );
}

static RESOURCE_MEMORY_MAP: IrqMutex<super::multiboot::MemoryMap> =
    IrqMutex::new(super::multiboot::MemoryMap::empty());
static RESOURCE_HOLE_CLEAR: AtomicBool = AtomicBool::new(false);
static BAR_CACHE: IrqMutex<[Option<(Bdf, BarSet)>; nolx_pci_core::MAX_FUNCTIONS]> =
    IrqMutex::new([None; nolx_pci_core::MAX_FUNCTIONS]);

/// Validate the bounded PC/Q35 profile's memory aperture against the complete loader map.
/// Any described memory (including reserved firmware regions) blocks this profile.
pub(crate) fn record_boot_memory(map: &super::multiboot::MemoryMap) {
    *RESOURCE_MEMORY_MAP.lock() = *map;
    let clear = !map.regions().is_empty()
        && map.truncated() == 0
        && map.regions().iter().all(|r| {
            r.base
                .checked_add(r.length)
                .is_some_and(|end| end <= 0xc000_0000 || r.base >= 0xf000_0000)
        });
    RESOURCE_HOLE_CLEAR.store(clear, Ordering::Release);
}

#[cfg(feature = "hal-pci-resource-test")]
fn resource_test_target(info: &nolx_pci_core::FunctionInfo) -> bool {
    (info.vendor_id == 0x1af4 && [0x1000, 0x1041, 0x1001, 0x1042].contains(&info.device_id))
        || (info.vendor_id == 0x8086 && info.device_id == 0x100e)
        || (info.class == 1
            && ((info.subclass == 6 && info.prog_if == 1)
                || (info.subclass == 8 && info.prog_if == 2)))
        || (info.class == 0x0c && info.subclass == 3 && info.prog_if == 0x30)
        || info.header_type == 1
}

/// Repair missing boot BARs/windows on the bounded PC/Q35 segment-zero profile, before drivers.
/// Firmware bus numbering remains required. Unknown host bridges/ambiguous memory maps retain
/// firmware resources without guessing an aperture. Valid ACPI roots supply apertures;
/// only absent ACPI permits the previous board fallback. Refused roots never downgrade.
pub fn prepare_resources(console: &mut dyn HalConsole) {
    use nolx_pci_core::resources::{self, Pool, Range};
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let host = ConfigAccess.read_u32(
        Bdf {
            segment: 0,
            bus: 0,
            device: 0,
            function: 0,
        },
        0,
    );
    let boot_map = *RESOURCE_MEMORY_MAP.lock();
    let complete_map = !boot_map.regions().is_empty()
        && boot_map.truncated() == 0
        && boot_map
            .regions()
            .iter()
            .all(|r| r.base.checked_add(r.length).is_some());
    let board = [0x1237_8086, 0x29c0_8086].contains(&host);
    let root = super::acpi::root_resources();
    let pools = if board && complete_map {
        match root {
            super::acpi::RootResourceState::Ready(root) => {
                use nolx_power_core::root::Space;
                let ranges = |space, low, high| {
                    root.iter()
                        .filter(|w| {
                            w.producer
                                && w.space == space
                                && (space != Space::Memory
                                    || (w.memory_flags & 1 != 0 && w.memory_flags & 6 != 6))
                        })
                        .filter_map(|w| {
                            let range = Range {
                                start: w.start.max(low),
                                end: w.end.min(high),
                            };
                            (range.start < range.end).then_some(range)
                        })
                        .collect::<alloc::vec::Vec<_>>()
                };
                // Retain a conservative chipset ceiling/legacy-I/O exclusion policy, but
                // no address outside the evaluated producer windows can be allocated.
                let mut memory = Pool::from_ranges(&ranges(
                    Space::Memory,
                    crate::mm::phys::DIRECT_MAP_BYTES,
                    0xf000_0000,
                ))
                .unwrap();
                let mut io = Pool::from_ranges(&ranges(Space::Io, 0xc000, 0xf000)).unwrap();
                for window in root.iter().filter(|w| !w.producer) {
                    let range = Range {
                        start: window.start,
                        end: window.end,
                    };
                    match window.space {
                        Space::Memory => memory.reserve(range).unwrap(),
                        Space::Io => io.reserve(range).unwrap(),
                        Space::Bus => {}
                    }
                }
                for region in boot_map.regions().iter().filter(|r| r.length != 0) {
                    let _ = writeln!(
                        console,
                        "pci: resource memory exclusion {:#x}..{:#x}",
                        region.base,
                        region.base + region.length
                    );
                    memory
                        .reserve(Range::new(region.base, region.length).unwrap())
                        .unwrap();
                }
                for index in 0..128 {
                    if let Some(device) = super::acpi::firmware_device(index) {
                        if let Some(resources) = device.resources {
                            for resource in resources.iter() {
                                match resource {
                                    nolx_power_core::Resource::Memory { base, length } => {
                                        memory.reserve(Range::new(base, length).unwrap()).unwrap()
                                    }
                                    nolx_power_core::Resource::Io { base, length } => io
                                        .reserve(
                                            Range::new(u64::from(base), u64::from(length)).unwrap(),
                                        )
                                        .unwrap(),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                let _=writeln!(console,"pci: resource policy=AML root _CRS; memory 0x80000000..0xf0000000, I/O 0xc000..0xf000, complete loader map excluded");
                Some((memory, io))
            }
            super::acpi::RootResourceState::Absent
                if RESOURCE_HOLE_CLEAR.load(Ordering::Acquire) =>
            {
                let _ = writeln!(
                    console,
                    "pci: resource policy=no-ACPI bounded board fallback"
                );
                Some((
                    Pool::new(Range::new(0xc000_0000, 0x3000_0000).unwrap()).unwrap(),
                    Pool::new(Range::new(0xc000, 0x3000).unwrap()).unwrap(),
                ))
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some((mut memory, mut io)) = pools {
        for mapping in MAPPINGS.lock().iter().flatten() {
            let (base, bytes) = mapping.window.physical_range();
            memory.reserve(Range::new(base, bytes).unwrap()).unwrap();
        }
        // Chipset PM/SMBus fixed registers are not ordinary PCI BARs.
        for info in inventory
            .as_slice()
            .iter()
            .filter(|i| i.vendor_id == 0x8086)
        {
            if info.device_id == 0x7113 {
                let pm = ConfigAccess.read_u32(info.bdf, 0x40) & 0xffc0;
                let smbus = ConfigAccess.read_u32(info.bdf, 0x90) & 0xfff0;
                if pm != 0 {
                    io.reserve(Range::new(u64::from(pm), 64).unwrap()).unwrap();
                }
                if smbus != 0 {
                    io.reserve(Range::new(u64::from(smbus), 16).unwrap())
                        .unwrap();
                }
            } else if info.device_id == 0x2918 {
                let pm = ConfigAccess.read_u32(info.bdf, 0x40) & 0xff80;
                if pm != 0 {
                    io.reserve(Range::new(u64::from(pm), 128).unwrap()).unwrap();
                }
            }
        }
        #[cfg(feature = "hal-pci-resource-test")]
        {
            let mut cleared = 0;
            for info in inventory
                .as_slice()
                .iter()
                .filter(|i| resource_test_target(i))
            {
                let command = ConfigAccess.read_u32(info.bdf, 4) & 0xffff;
                ConfigAccess.write_u32(info.bdf, 4, command & !7);
                let bars = if info.header_type == 1 {
                    nolx_pci_core::read_bridge_bars(&mut ConfigAccess, info.bdf)
                } else {
                    nolx_pci_core::read_bars(&mut ConfigAccess, info.bdf)
                };
                for (slot, bar) in bars.slots.iter().enumerate() {
                    if matches!(bar, Bar::Unused | Bar::Reserved) {
                        continue;
                    }
                    let offset = 0x10 + slot as u8 * 4;
                    let flags = ConfigAccess.read_u32(info.bdf, offset)
                        & if matches!(bar, Bar::Io { .. }) { 3 } else { 15 };
                    ConfigAccess.write_u32(info.bdf, offset, flags);
                    if matches!(bar, Bar::Memory64 { .. }) {
                        ConfigAccess.write_u32(info.bdf, offset + 4, 0);
                    }
                    cleared += 1;
                }
                if info.header_type == 1 {
                    ConfigAccess.write_u32(info.bdf, 0x20, 0x0000_fff0);
                    ConfigAccess.write_u32(info.bdf, 0x24, 0x0000_fff0);
                    let types = ConfigAccess.read_u32(info.bdf, 0x1c) & 0x0f0f;
                    ConfigAccess.write_u32(info.bdf, 0x1c, types | 0xf0);
                    ConfigAccess.write_u32(info.bdf, 0x30, 0);
                }
            }
            assert!(cleared > 0, "resource fixture needs supported devices");
            let _ = writeln!(console, "pci: resource fault injection: {cleared} BAR addresses and bridge forwarding cleared before native assignment");
        }
        let plan = resources::plan(&mut ConfigAccess, &inventory, memory, io)
            .expect("PCI resource plan refused; no assignments committed");
        plan.apply(&mut ConfigAccess)
            .expect("PCI resource readback/rollback refused");
        for a in plan.assignments() {
            let _ = writeln!(
                console,
                "pci: assigned {:02x}:{:02x}.{} bar{} {} {:#x}..{:#x}",
                a.bdf.bus,
                a.bdf.device,
                a.bdf.function,
                a.slot,
                if a.io { "io" } else { "mem" },
                a.range.start,
                a.range.end
            );
        }
        let _ = writeln!(
            console,
            "pci: native resource assignment: {} BARs, {} bridge windows, readback verified",
            plan.assignments().len(),
            plan.windows()
        );
    } else {
        let _ = writeln!(
            console,
            "pci: native resource profile unavailable; firmware assignments retained"
        );
        #[cfg(feature = "hal-pci-resource-test")]
        panic!(
            "resource fixture requires a supported host and a clear complete firmware memory map"
        );
    }
    let mut cache = BAR_CACHE.lock();
    for (slot, info) in inventory
        .as_slice()
        .iter()
        .filter(|i| i.header_type == 0)
        .enumerate()
    {
        cache[slot] = Some((
            info.bdf,
            nolx_pci_core::read_bars(&mut ConfigAccess, info.bdf),
        ));
    }
}
