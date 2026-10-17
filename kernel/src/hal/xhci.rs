//! The xHCI controller's bring-up: find the controller by its class code, enable it, map its
//! registers through an [`mmio`] window, halt and reset it, and report the state of each root port.
//! Everything is polled, and failure is reported without stopping boot.
//!
//! The register offsets, the status bits, and the port arithmetic live in `nolx-usb-core`, where
//! they are host-tested. This module supplies register access and bounded polling.
//!
//! The live transport retains bounded rings and direct-root USB2 boot-HID devices. It polls
//! completions in idle context; disconnect disables the slot before its DMA pages are freed.
//! Hubs, SuperSpeed devices, multiple host controllers and framework/power integration remain open.

mod live;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{Bdf, ConfigSpace};
use nolx_usb_core as usb;

use super::pci::ConfigAccess;
use crate::mm::mmio::{self, Region};

/// xHCI class code: serial bus controller, USB, xHCI programming interface.
const XHCI_CLASS: u8 = 0x0C;
const XHCI_SUBCLASS: u8 = 0x03;
const XHCI_PROG_IF: u8 = 0x30;
/// The base address register that holds the controller registers. xHCI puts them in BAR 0.
const BAR0_SLOT: usize = 0;
/// PCI command register, with the memory space and bus master bits. The controller answers its
/// registers only with memory space set, and DMAs only with bus master set.
const PCI_COMMAND: u8 = 0x04;
const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
const COMMAND_BUS_MASTER: u32 = 1 << 2;
/// The capability register that holds the interface version, after CAPLENGTH.
const CAP_HCIVERSION: u64 = 0x02;
/// The controller's register space as mapped. It covers the capability and operational registers
/// and the root port register sets: for 255 ports the last one ends below 0x1500.
const REGISTERS_BYTES: u64 = 0x4000;
/// How many times a bounded wait looks before it gives up.
const POLL_LIMIT: u32 = 1_000_000;

/// Why the bring-up stopped.
#[derive(Debug)]
pub enum Failure {
    /// No xHCI controller is on the PCI bus.
    NoController,
    /// A DMA allocation could not be obtained.
    Memory,
    /// Controller/device features exceed the supported direct-root boot-HID profile.
    Unsupported,
    /// The controller returned an unsuccessful command or transfer completion.
    Completion(u8),
    /// BAR 0 is not a memory BAR, so the controller registers cannot be reached.
    BarNotMemory,
    /// The MMIO window refused a mapping.
    Window(mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// A bounded wait ran out, with the step named.
    Timeout(&'static str),
}

impl From<MmioError> for Failure {
    fn from(error: MmioError) -> Self {
        Self::Access(error)
    }
}

impl From<mmio::Failure> for Failure {
    fn from(error: mmio::Failure) -> Self {
        Self::Window(error)
    }
}

/// Probe for an xHCI controller, bring it to a halted, reset state, and report its root ports.
/// Failure is reported on the console and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    if live::active() {
        let _ = writeln!(console, "xhci: duplicate active probe refused");
        return;
    }
    if let Err(failure) = bring_up(console) {
        let _ = writeln!(console, "xhci: not brought up: {failure:?}");
    }
}

fn bring_up(console: &mut dyn HalConsole) -> Result<(), Failure> {
    let bdf = find_controller().ok_or(Failure::NoController)?;
    enable_device(bdf);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base =
        nolx_pci_core::assigned_memory_base(bars.slots[BAR0_SLOT]).ok_or(Failure::BarNotMemory)?;
    let regs = mmio::map(base, REGISTERS_BYTES)?;
    let caplength = regs.read8(usb::CAP_CAPLENGTH)?;
    let version = regs.read16(CAP_HCIVERSION)?;
    let hcsparams1 = regs.read32(usb::CAP_HCSPARAMS1)?;
    // The operational registers start CAPLENGTH bytes into the controller's space.
    let op = u64::from(caplength);

    halt(&regs, op)?;
    reset(&regs, op)?;
    let ports = usb::max_ports(hcsparams1);
    let _ = writeln!(
        console,
        "xhci: {:02x}:{:02x}.{} bar0 {base:#x}, version {version:#x}, {ports} ports, {} slots",
        bdf.bus,
        bdf.device,
        bdf.function,
        usb::max_slots(hcsparams1),
    );
    let _ = writeln!(console, "xhci: controller reset, halted");
    report_ports(console, &regs, caplength, ports)?;
    live::start(
        u32::from(bdf.bus) << 16 | u32::from(bdf.device) << 8 | u32::from(bdf.function),
        regs,
        op,
        ports,
        console,
    )
}

/// Stop the controller's run bit and wait until it reports halted.
fn halt(regs: &Region, op: u64) -> Result<(), Failure> {
    let command = regs.read32(op + usb::OP_USBCMD)?;
    regs.write32(op + usb::OP_USBCMD, command & !usb::USBCMD_RUN)?;
    poll(
        || Ok(regs.read32(op + usb::OP_USBSTS)? & usb::USBSTS_HCHALTED != 0),
        "controller halt",
    )
}

/// Reset the halted controller, wait until the reset bit clears and the controller is ready, and
/// check that it is halted again.
fn reset(regs: &Region, op: u64) -> Result<(), Failure> {
    let command = regs.read32(op + usb::OP_USBCMD)?;
    regs.write32(op + usb::OP_USBCMD, command | usb::USBCMD_HCRST)?;
    poll(
        || {
            let command = regs.read32(op + usb::OP_USBCMD)?;
            let status = regs.read32(op + usb::OP_USBSTS)?;
            Ok(command & usb::USBCMD_HCRST == 0 && status & usb::USBSTS_CNR == 0)
        },
        "controller reset",
    )?;
    if regs.read32(op + usb::OP_USBSTS)? & usb::USBSTS_HCHALTED == 0 {
        return Err(Failure::Timeout("halted after reset"));
    }
    Ok(())
}

/// Report each root port's connection state. A connected port is the first place a device shows.
fn report_ports(
    console: &mut dyn HalConsole,
    regs: &Region,
    caplength: u8,
    ports: u8,
) -> Result<(), Failure> {
    let mut connected = 0u8;
    for port in 1..=ports {
        let Some(offset) = usb::port_register_offset(caplength, port) else {
            continue;
        };
        let portsc = regs.read32(offset)?;
        let state = if portsc & usb::PORTSC_CONNECTED != 0 {
            connected += 1;
            "connected"
        } else {
            "empty"
        };
        let _ = writeln!(console, "xhci: port {port} {state}");
    }
    let _ = writeln!(console, "xhci: {connected} of {ports} ports connected");
    Ok(())
}

fn enable_device(bdf: Bdf) {
    let mut cfg = ConfigAccess;
    let command = cfg.read_u32(bdf, PCI_COMMAND) & 0xFFFF;
    cfg.write_u32(
        bdf,
        PCI_COMMAND,
        command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER,
    );
}

/// The first xHCI controller on the bus, if there is one.
fn find_controller() -> Option<Bdf> {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .find(|info| {
            info.class == XHCI_CLASS
                && info.subclass == XHCI_SUBCLASS
                && info.prog_if == XHCI_PROG_IF
        })
        .map(|info| info.bdf)
}

/// Poll `ready` until it returns true, at most [`POLL_LIMIT`] times.
fn poll(
    mut ready: impl FnMut() -> Result<bool, Failure>,
    step: &'static str,
) -> Result<(), Failure> {
    for _ in 0..POLL_LIMIT {
        if ready()? {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout(step))
}

/// Poll native USB HID completions and consume source-tagged input in idle context.
pub fn poll_events(console: &mut dyn HalConsole) {
    live::poll_events(console);
}
