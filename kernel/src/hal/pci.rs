//! Legacy PCI configuration access and the boot-time device inventory.
//!
//! The mechanism is the one every x86 PC has: write a dword address to `0xCF8`, then read or
//! write the dword at `0xCFC`. The bit-level rules -- address encoding, header parsing, BAR
//! sizing, capability walking, bridge-aware enumeration -- live in `nolx-pci-core`, where they
//! are host-tested. This module supplies only the two port accesses and reports the result.
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
