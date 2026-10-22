//! The xHCI controller's bring-up: find the controller by its class code, enable it, map its
//! registers through an [`mmio`] window, halt and reset it, and report the state of each root port.
//! Startup/control commands are bounded and polled; live input uses MSI/MSI-X when available.
//!
//! The register offsets, the status bits, and the port arithmetic live in `nolx-usb-core`, where
//! they are host-tested. This module supplies register access and bounded polling.
//!
//! The live transport retains bounded rings and direct-root USB2 boot-HID devices. It drains
//! completions in IRQ-scheduled idle context; disconnect disables the slot before its DMA pages are freed.
//! Each framework handle owns its controller/MMIO/DMA; deferred service excludes lifecycle callbacks.
//! USB3 EP0/class refusal is hardware-checked; single-burst SS HID has portable rules only.
//! Hubs, verified SuperSpeed HID input, general HID and S3/Executive integration remain open.

#[cfg(feature = "hal-xhci-exhaust-test")]
mod exhaust;
#[cfg(feature = "hal-xhci-transfer-fault-test")]
mod faults;
mod irq;
mod live;
#[cfg(feature = "hal-xhci-recovery-test")]
mod recovery;
#[cfg(feature = "hal-xhci-lifecycle-test")]
mod tests;
use super::driver_start;
use crate::sync::IrqMutex;
use core::fmt::Write;
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
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
    /// Framework checkout or a retained hardware owner refuses this operation.
    Busy,
    /// A command outcome was missing, stopped, aborted or invalid; full controller reset required.
    CommandUnavailable,
    /// MSI/MSI-X setup, delivery proof or interrupt masking/readback failed.
    Interrupt,
    /// Every vector in the shared native interrupt domain is owned by another source.
    NoVector,
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

struct Binding {
    bdf: Option<Bdf>,
    region: Option<Region>,
    log: driver_start::Log,
}
static BINDINGS: [IrqMutex<Binding>; driver_start::DEVICES] = [const {
    IrqMutex::new(Binding {
        bdf: None,
        region: None,
        log: driver_start::Log::new(),
    })
}; driver_start::DEVICES];
static MATCHES: &[Match] = &[Match::by_class(
    XHCI_CLASS,
    XHCI_SUBCLASS,
    Some(XHCI_PROG_IF),
)];

/// Bind every discovered controller once; a failed/quarantined identity is never rebound.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = super::pci::enumerate_native();
    let mut found = false;
    for info in inventory.as_slice().iter().filter(|info| {
        info.class == XHCI_CLASS && info.subclass == XHCI_SUBCLASS && info.prog_if == XHCI_PROG_IF
    }) {
        found = true;
        if BINDINGS
            .iter()
            .any(|binding| binding.lock().bdf == Some(info.bdf))
        {
            let _ = writeln!(console, "xhci: duplicate PCI binding refused");
            continue;
        }
        let driver = Driver {
            name: "xhci",
            matches: MATCHES,
            ops: Dispatch {
                probe: |_| true,
                start: start_device,
                stop: stop_device,
            },
        };
        let result = driver_start::attach_prepared(
            driver,
            DeviceId {
                vendor: info.vendor_id,
                device: info.device_id,
                class: info.class,
                subclass: info.subclass,
                prog_if: info.prog_if,
            },
            |dev| {
                if BINDINGS
                    .iter()
                    .any(|binding| binding.lock().bdf == Some(info.bdf))
                {
                    return Err(nolx_df_core::Error::Busy);
                }
                BINDINGS[dev].lock().bdf = Some(info.bdf);
                Ok(())
            },
        );
        for (dev, binding) in BINDINGS.iter().enumerate() {
            let binding = binding.lock();
            if binding.bdf == Some(info.bdf) {
                binding.log.replay(console);
                let _ = writeln!(
                    console,
                    "xhci: instance {dev} PCI {:?}, framework {result:?}",
                    info.bdf
                );
            }
        }
    }
    if !found {
        let _ = writeln!(console, "xhci: no controller on the PCI bus");
    }
    #[cfg(feature = "hal-xhci-exhaust-test")]
    exhaust::run(console);
}

fn start_device(dev: usize) -> bool {
    let mut log = driver_start::Log::new();
    #[cfg(feature = "hal-xhci-start-fault-test")]
    let baseline = (super::dma::live_pages(), mmio::live_mappings());
    let result = bring_up(dev, &mut log);
    if let Err(error) = &result {
        let _ = writeln!(log, "xhci: not brought up: {error:?}");
        #[cfg(feature = "hal-xhci-start-fault-test")]
        if matches!(error, Failure::Timeout("injected post-run start failure")) {
            assert!(
                live::active(dev),
                "exposed host lost ownership on start failure"
            );
            assert_eq!(
                (super::dma::live_pages(), mmio::live_mappings()),
                (baseline.0 + 3, baseline.1 + 1)
            );
        }
        if stop_device(dev) {
            // SAFETY: rollback proved halt/reset and PCI DMA exclusion, detached all host state;
            // the framework remains checked out throughout the failed-start callback.
            unsafe {
                mmio::release_owned(dev);
            }
            let _ = writeln!(log, "xhci: failed-start rollback reclaimed MMIO/DMA");
            #[cfg(feature = "hal-xhci-start-fault-test")]
            assert_eq!((super::dma::live_pages(), mmio::live_mappings()), baseline);
        } else {
            let _ = writeln!(log, "xhci: failed-start quarantine retains MMIO/DMA");
        }
    }
    BINDINGS[dev].lock().log = log;
    result.is_ok()
}

fn bring_up(dev: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
    if live::active(dev) || BINDINGS[dev].lock().region.is_some() {
        return Err(Failure::Busy);
    }
    let bdf = BINDINGS[dev].lock().bdf.ok_or(Failure::NoController)?;
    enable_device(bdf);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base =
        nolx_pci_core::assigned_memory_base(bars.slots[BAR0_SLOT]).ok_or(Failure::BarNotMemory)?;
    let regs = mmio::map_owned(dev, base, REGISTERS_BYTES)?;
    // Retain the mapping before any subsequent fallible operation.
    BINDINGS[dev].lock().region = Some(regs);
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
        "xhci: {:02x}:{:02x}.{} bar0 {base:#x}, version {version:#x}, {ports} ports, {} slots, segment {}",
        bdf.bus,
        bdf.device,
        bdf.function,
        usb::max_slots(hcsparams1),
        bdf.segment,
    );
    let _ = writeln!(console, "xhci: controller reset, halted");
    report_ports(console, &regs, caplength, ports)?;
    live::start(dev, bdf.packed_id(), regs, op, ports, console)?;
    irq::arm(dev, bdf, regs, op, console)
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

fn stop_device(dev: usize) -> bool {
    let (bdf, region) = {
        let binding = BINDINGS[dev].lock();
        (binding.bdf, binding.region)
    };
    let Some(bdf) = bdf else {
        return false;
    };
    // Retire producers even on failed quiescence, but never free hardware-visible storage then.
    live::retire(dev);
    if irq::disarm(dev, bdf).is_err() {
        return false;
    }
    #[cfg(feature = "hal-xhci-recovery-test")]
    if recovery::refuse_stop(dev) {
        return false;
    }
    #[cfg(feature = "hal-xhci-stop-fault-test")]
    if tests::inject_stop(dev) {
        return false;
    }
    #[cfg(feature = "hal-power-quiesce-stop-fault-test")]
    if region.is_some() {
        return false;
    }
    if let Some(regs) = region {
        let quiesced = (|| {
            let op = u64::from(regs.read8(usb::CAP_CAPLENGTH)?);
            halt(&regs, op)?;
            reset(&regs, op)
        })();
        if quiesced.is_err() {
            return false;
        }
    }
    let command = ConfigAccess.read_u32(bdf, PCI_COMMAND) & 0xffff;
    ConfigAccess.write_u32(bdf, PCI_COMMAND, command & !COMMAND_BUS_MASTER);
    if ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER != 0 {
        return false;
    }
    if !irq::release_vector(dev) {
        return false;
    }
    // SAFETY: halt/reset acknowledged (or no register/DMA exposure ever occurred), PCI bus
    // mastering reads disabled, framework checkout excludes polling/submission, producers retired.
    unsafe {
        live::release_host(dev);
    }
    BINDINGS[dev].lock().region = None;
    true
}

/// Failed callbacks may quarantine DMA. Preserve their record until checked recovery succeeds.
pub(crate) fn removable(dev: usize) -> bool {
    BINDINGS[dev].lock().region.is_none() && !live::active(dev)
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

/// Service IRQ-notified native USB HID completions in idle; explicit polling only without a route.
pub fn poll_events(console: &mut dyn HalConsole) {
    if let Ok(started) = driver_start::started() {
        for (dev, name) in started.into_iter().flatten() {
            if name == "xhci" {
                let _ = poll_on(dev, console);
            }
        }
    }
}

/// Stopped/stale/wrong-driver handles never touch registers.
pub fn poll_on(dev: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
    driver_start::device_operation_handle(
        dev,
        "xhci",
        || {
            let Some(delivered) = irq::work(dev) else {
                live::poll_hubs(dev, console);
                return Ok(());
            };
            let (more, reports) = live::poll_events(dev, console);
            irq::finish(dev, more)?;
            #[cfg(feature = "hal-xhci-irq-test")]
            if reports > 0 {
                let kind = if delivered == 0 { "polled" } else { "irq" };
                let _ = writeln!(console, "xhci: {kind} work handle={dev} vector={:#x} delivered={delivered} reports={reports}", irq::vector(dev));
            }
            #[cfg(not(feature = "hal-xhci-irq-test"))]
            let _ = (delivered, reports);
            Ok(())
        },
        |error| {
            if error == nolx_df_core::Error::Busy {
                Failure::Busy
            } else {
                Failure::NoController
            }
        },
    )
}
#[cfg(feature = "hal-xhci-lifecycle-test")]
/// Exercise real two-controller input and checked lifecycle from the idle test path.
pub fn lifecycle_selftest(console: &mut dyn HalConsole) {
    tests::step(console);
}

/// Exercise repeated failed quiescence and successful recovery with real two-controller input.
#[cfg(feature = "hal-xhci-recovery-test")]
pub fn recovery_selftest(console: &mut dyn HalConsole) {
    recovery::step(console);
}

/// Statically reserved MSI vectors are acknowledged even after their controller detaches.
pub fn handles_vector(vector: u8) -> bool {
    irq::handles(vector)
}
/// Bounded top half: acknowledge/mask and notify idle, with no allocation/ring or framework access.
pub fn on_interrupt(vector: u8) {
    irq::interrupt(vector);
}

/// Explicitly retry a quarantined root slot; only checked Disable Slot permits DMA release.
/// Re-enumerate if still connected. A poisoned command ring requires controller stop/restart.
pub fn repair_port_on(dev: usize, port: u8, console: &mut dyn HalConsole) -> Result<(), Failure> {
    driver_start::device_operation_handle(
        dev,
        "xhci",
        || {
            let result = live::repair_port(dev, port, console);
            irq::finish(dev, false)?;
            result
        },
        |error| {
            if error == nolx_df_core::Error::Busy {
                Failure::Busy
            } else {
                Failure::NoController
            }
        },
    )
}
#[cfg(feature = "hal-xhci-transfer-fault-test")]
/// Verify actual stalled transfer, pinned slots and explicit repair from idle context.
pub fn transfer_selftest(console: &mut dyn HalConsole) {
    faults::step(console);
}
