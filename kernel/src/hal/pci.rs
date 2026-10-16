//! Serialized legacy/ECAM PCI configuration transport and boot-time inventories.
//!
//! The mechanism is the one every x86 PC has: write a dword address to `0xCF8`, then read or
//! write the dword at `0xCFC`. The bit-level rules -- address encoding, header parsing, BAR
//! sizing, capability walking, bridge-aware enumeration -- live in `nolx-pci-core`, where they
//! are host-tested. This module supplies port access and reports the result; its ECAM probe
//! independently checks identities through uncached MMIO without changing driver writes.
//!
//! The address and data ports are a pair: any access between them would select a different
//! register. Discovery runs on the boot processor before the periodic timer starts, so nothing
//! else touches the pair. Interrupt-driven drivers will need that pair under a lock; the
//! assertion rows record that obligation rather than leaving it implicit.

use nolx_hal_core::console::HalConsole;
use nolx_pci_core::{Bar, BarSet, Bdf, ConfigSpace};

use super::{inl, outl};
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{AtomicUsize, Ordering};
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
        self.read_dword(0, bdf, u16::from(offset & !3))
            .unwrap_or(u32::MAX)
    }
    fn write_u32(&mut self, bdf: Bdf, offset: u8, value: u32) {
        let _ = self.write_dword(0, bdf, u16::from(offset & !3), value);
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

/// The legacy configuration mechanism over the two architected ports.
pub struct LegacyPorts;

impl ConfigSpace for LegacyPorts {
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32 {
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
        let _guard = LEGACY_LOCK.lock();
        // SAFETY: as [`LegacyPorts::read_u32`]. Writes come only from the BAR sizing protocol,
        // which restores each register it changes; discovery itself never writes.
        unsafe {
            outl(
                nolx_pci_core::CONFIG_ADDRESS_PORT,
                nolx_pci_core::config_address(bdf, offset),
            );
            outl(nolx_pci_core::CONFIG_DATA_PORT, value);
        }
    }
}

/// Enumerate PCI configuration space and report every function found.
pub fn init(console: &mut dyn HalConsole) {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
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
    for info in inventory.as_slice() {
        if info.header_type == 0 {
            let bars = nolx_pci_core::read_bars(&mut ConfigAccess, info.bdf);
            report_bars(console, info.bdf, &bars);
        }
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

/// Exercise read-only ECAM through uncached MMIO without changing the drivers' transport.
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
        // Legacy discovery describes only segment zero. Never compare its BDFs to
        // another segment, even when that segment uses the same bus numbers.
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
        let expected = u32::from(info.vendor_id) | (u32::from(info.device_id) << 16);
        if region.read32(offset) != Ok(expected) {
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
        "pci: ECAM {matched} identity match(es), {refused} refused; legacy configuration retained"
    );
}
