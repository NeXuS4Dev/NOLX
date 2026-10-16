//! Legacy PCI configuration access, boot-time inventory, and a read-only ECAM probe.
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

/// The legacy configuration mechanism over the two architected ports.
pub struct LegacyPorts;

impl ConfigSpace for LegacyPorts {
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32 {
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
    let inventory = nolx_pci_core::enumerate(&mut LegacyPorts);
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
    probe_ecam(console, &inventory);
    for info in inventory.as_slice() {
        if info.header_type == 0 {
            let bars = nolx_pci_core::read_bars(&mut LegacyPorts, info.bdf);
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
    use crate::mm::mmio;
    use nolx_pci_core::ecam::{AccessWidth, EcamWindow};

    let Some(mcfg) = super::acpi::discover().and_then(|platform| platform.mcfg) else {
        let _ = writeln!(console, "pci: no usable MCFG; legacy configuration retained");
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
        // Map one function, not the potentially 256 MiB allocation. The MMIO
        // allocator rejects RAM aliases and unsupported physical addresses.
        let region = match mmio::map(physical, 4096) {
            Ok(region) => region,
            Err(_) => {
                refused += 1;
                continue;
            }
        };
        let expected = u32::from(info.vendor_id) | (u32::from(info.device_id) << 16);
        if region.read32(0) != Ok(expected) {
            refused += 1;
            continue;
        }
        // The extended capability header is read-only. Zero/all-ones is valid
        // when no extended capabilities exist; do not mistake it for a fault.
        if let Ok(header) = region.read32(0x100) {
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
