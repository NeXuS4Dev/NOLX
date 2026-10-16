//! The e1000 driver, run through the driver framework (`nolx-df`). The probe finds the controller
//! by its vendor and device ID and registers a driver for it. The framework binds the device and
//! starts it; starting resets the controller, reads the MAC address from the EEPROM, programs the
//! receive address, sets up one receive ring and one transmit ring of eight legacy descriptors,
//! enables both engines, and transmits one 60-byte frame. Everything is polled, with bounds.
//!
//! The register offsets, descriptor layouts, and ring arithmetic live in `nolx-e1000-core`, where
//! they are host-tested. The framework's rules live in `nolx-df-core`. This module supplies register
//! access, memory for the rings, and the driver's dispatch table.
//!
//! The framework dispatch table takes plain function pointers and no context, so the state the
//! driver keeps between calls is in a static [`IrqMutex`]. The receive buffers are never reclaimed:
//! the kernel has no frame free path, and the device owns them for as long as it is running.
//!
//! CI runs QEMU with an e1000 on a user-mode network backend.

use core::sync::atomic::{fence, Ordering};

use nolx_df_core::{DeviceId, Dispatch, Driver, Error as DfError, Match, Registry};
use nolx_e1000_core::{self as e1000, DEVICE_82540EM, VENDOR_INTEL};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{Bar, Bdf, ConfigSpace};

use super::dma::Dma;
use super::pci::ConfigAccess;
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;

/// PCI command register, with the memory space and bus master bits.
const PCI_COMMAND: u8 = 0x04;
const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
const COMMAND_BUS_MASTER: u32 = 1 << 2;
/// The base address register that holds the controller registers. The e1000 puts them in BAR 0.
const BAR0_SLOT: usize = 0;
/// The register space of the controller: 128 KiB, as the e1000 BAR 0 reports it.
const REGISTERS_BYTES: u64 = 0x2_0000;
/// Descriptors in each ring. Eight is enough for one frame in flight at a time.
const RING: u32 = 8;
/// The frame sent at boot: the smallest legal Ethernet frame, without the CRC the controller adds.
const FRAME_BYTES: usize = 60;
/// The EtherType of the test frame: the IEEE local experimental value.
const FRAME_TYPE: [u8; 2] = [0x88, 0xB5];
const FRAME_PAYLOAD: &[u8] = b"NOLX-E1000 transmit check";
/// How many times a bounded wait looks before it gives up.
const POLL_LIMIT: u32 = 1_000_000;

/// The rule the framework matches devices against: this vendor and device.
static MATCHES: &[Match] = &[Match::by_ids(VENDOR_INTEL, DEVICE_82540EM)];

/// Why the driver stopped.
#[derive(Debug)]
pub enum Failure {
    /// No e1000 is on the PCI bus.
    NoController,
    /// BAR 0 is not a memory BAR, so the registers cannot be reached.
    BarNotMemory,
    /// The MMIO window refused a mapping.
    Window(crate::mm::mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// A bounded wait ran out, with the step named.
    Timeout(&'static str),
    /// The EEPROM gave no usable MAC address.
    Eeprom,
    /// No frame was available for the rings or the transmit buffer.
    Memory,
    /// The ring or the test frame was refused by the e1000 rules.
    Frame,
}

impl From<MmioError> for Failure {
    fn from(error: MmioError) -> Self {
        Self::Access(error)
    }
}

impl From<crate::mm::mmio::Failure> for Failure {
    fn from(error: crate::mm::mmio::Failure) -> Self {
        Self::Window(error)
    }
}

/// The rings and the transmit buffer. The device keeps them for as long as it runs, so they are
/// held here and never read again.
#[allow(dead_code)]
struct Rings {
    rx: Dma,
    tx: Dma,
    frame: Dma,
}

/// What the bring-up established, for the boot report.
struct Report {
    bdf: Bdf,
    base: u64,
    mac: [u8; 6],
    link_up: bool,
    region: Region,
}

/// State shared between the driver's dispatch calls and the probe that reports them.
struct State {
    #[allow(dead_code)]
    rings: Option<Rings>,
    report: Option<Report>,
    error: Option<Failure>,
}

static STATE: IrqMutex<State> = IrqMutex::new(State {
    rings: None,
    report: None,
    error: None,
});

/// The driver's dispatch table. Probe accepts any device the match rule selects; start and stop
/// act on the one controller the probe found.
const DISPATCH: Dispatch = Dispatch {
    probe: accept,
    start: start_device,
    stop: stop_device,
};

/// Probe for an e1000, register the driver with the framework, bind and start the device, and
/// report the result. Failure is reported on the console and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    let Some((_, id)) = find_controller() else {
        let _ = writeln!(console, "e1000: no controller on the PCI bus");
        return;
    };
    let mut registry: Registry<1, 1> = Registry::new();
    let driver = Driver {
        name: "e1000",
        matches: MATCHES,
        ops: DISPATCH,
    };
    let attached = attach(&mut registry, driver, id);
    if let Err(error) = attached {
        let _ = writeln!(console, "e1000: framework refused the device: {error:?}");
    }

    let state = STATE.lock();
    match (&state.report, &state.error) {
        (Some(report), _) => {
            let _ = writeln!(
                console,
                "e1000: {:02x}:{:02x}.{} bar0 {:#x}, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}",
                report.bdf.bus,
                report.bdf.device,
                report.bdf.function,
                report.base,
                report.mac[0],
                report.mac[1],
                report.mac[2],
                report.mac[3],
                report.mac[4],
                report.mac[5],
                if report.link_up { "up" } else { "down" },
            );
            let _ = writeln!(
                console,
                "e1000: transmitted a {FRAME_BYTES}-byte frame, descriptor done"
            );
            let _ = writeln!(console, "e1000: bound and started through nolx-df");
        }
        (None, Some(error)) => {
            let _ = writeln!(console, "e1000: not brought up: {error:?}");
        }
        (None, None) => {
            let _ = writeln!(console, "e1000: not started");
        }
    }
}

/// Register the driver, add the device, bind it, and start it. Returns the device handle.
fn attach(registry: &mut Registry<1, 1>, driver: Driver, id: DeviceId) -> Result<usize, DfError> {
    registry.add_driver(driver)?;
    let dev = registry.add_device(id)?;
    registry.bind(dev)?;
    registry.start(dev)?;
    Ok(dev)
}

fn accept(_: &DeviceId) -> bool {
    true
}

/// The framework's start operation: the whole bring-up. The result goes to [`STATE`].
fn start_device(_dev: usize) -> bool {
    let result = bring_up();
    let mut state = STATE.lock();
    match result {
        Ok((rings, report)) => {
            state.rings = Some(rings);
            state.report = Some(report);
            state.error = None;
            true
        }
        Err(failure) => {
            state.error = Some(failure);
            false
        }
    }
}

/// The framework's stop operation: clear both engines' enable bits. The rings stay in place.
fn stop_device(_dev: usize) -> bool {
    let state = STATE.lock();
    state.report.as_ref().is_some_and(|report| {
        report.region.write32(e1000::REG_RCTL, 0).is_ok()
            && report.region.write32(e1000::REG_TCTL, 0).is_ok()
    })
}

fn bring_up() -> Result<(Rings, Report), Failure> {
    let (bdf, _) = find_controller().ok_or(Failure::NoController)?;
    enable_device(bdf);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base = match bars.slots[BAR0_SLOT] {
        Bar::Memory32 { base, .. } => u64::from(base),
        Bar::Memory64 { base, .. } => base,
        _ => return Err(Failure::BarNotMemory),
    };
    let regs = mmio::map(base, REGISTERS_BYTES)?;

    // Reset, then set the link up. The reset bit clears itself when the reset is done.
    regs.write32(
        e1000::REG_CTRL,
        regs.read32(e1000::REG_CTRL)? | e1000::CTRL_RST,
    )?;
    poll(
        || Ok(regs.read32(e1000::REG_CTRL)? & e1000::CTRL_RST == 0),
        "device reset",
    )?;
    regs.write32(
        e1000::REG_CTRL,
        regs.read32(e1000::REG_CTRL)? | e1000::CTRL_SLU,
    )?;

    // The MAC address: EEPROM words 0 to 2, each read through EERD.
    let mac = e1000::mac_from_eeprom([
        eeprom_word(&regs, 0)?,
        eeprom_word(&regs, 1)?,
        eeprom_word(&regs, 2)?,
    ]);
    if mac == [0; 6] || mac == [0xFF; 6] {
        return Err(Failure::Eeprom);
    }
    let (low, high) = e1000::receive_address(mac);
    regs.write32(e1000::REG_RAL0, low)?;
    regs.write32(e1000::REG_RAH0, high)?;

    // Receive ring: each descriptor names its own buffer, and the tail gives the device all of
    // them but the one behind the head.
    let ring_bytes = e1000::ring_bytes(RING).ok_or(Failure::Frame)?;
    let rx = Dma::new().ok_or(Failure::Memory)?;
    for slot in 0..RING {
        let buffer = Dma::new().ok_or(Failure::Memory)?;
        rx.write(
            u64::from(slot) * e1000::DESCRIPTOR_BYTES as u64,
            &e1000::rx_descriptor(buffer.address(0)),
        );
    }
    fence(Ordering::SeqCst);
    write_address(&regs, e1000::REG_RDBAL, rx.address(0))?;
    regs.write32(e1000::REG_RDLEN, ring_bytes)?;
    regs.write32(e1000::REG_RDH, 0)?;
    regs.write32(e1000::REG_RDT, RING - 1)?;
    regs.write32(e1000::REG_RCTL, e1000::rctl_enable(true))?;

    // Transmit ring, empty to begin with.
    let tx = Dma::new().ok_or(Failure::Memory)?;
    write_address(&regs, e1000::REG_TDBAL, tx.address(0))?;
    regs.write32(e1000::REG_TDLEN, ring_bytes)?;
    regs.write32(e1000::REG_TDH, 0)?;
    regs.write32(e1000::REG_TDT, 0)?;
    regs.write32(e1000::REG_TCTL, e1000::tctl_enable())?;

    // One frame: broadcast destination, this MAC as the source, the test EtherType, the payload,
    // zero padding to the minimum length.
    let frame = Dma::new().ok_or(Failure::Memory)?;
    let mut bytes = [0u8; FRAME_BYTES];
    bytes[0..6].fill(0xFF);
    bytes[6..12].copy_from_slice(&mac);
    bytes[12..14].copy_from_slice(&FRAME_TYPE);
    bytes[14..14 + FRAME_PAYLOAD.len()].copy_from_slice(FRAME_PAYLOAD);
    frame.write(0, &bytes);
    let descriptor =
        e1000::tx_descriptor(frame.address(0), FRAME_BYTES as u16, true).ok_or(Failure::Frame)?;
    tx.write(0, &descriptor);
    fence(Ordering::SeqCst);
    regs.write32(e1000::REG_TDT, 1)?;
    poll(
        || {
            let mut status = [0u8; 1];
            tx.read(12, &mut status);
            Ok(e1000::tx_done(status[0]))
        },
        "transmit",
    )?;

    let link_up = regs.read32(e1000::REG_STATUS)? & e1000::STATUS_LINK_UP != 0;
    Ok((
        Rings { rx, tx, frame },
        Report {
            bdf,
            base,
            mac,
            link_up,
            region: regs,
        },
    ))
}

/// Read one EEPROM word through EERD, waiting for the controller to report the read done.
fn eeprom_word(regs: &Region, word: u8) -> Result<u16, Failure> {
    regs.write32(e1000::REG_EERD, e1000::eerd_read(word))?;
    for _ in 0..POLL_LIMIT {
        if let Some(value) = e1000::eerd_result(regs.read32(e1000::REG_EERD)?) {
            return Ok(value);
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout("eeprom read"))
}

/// Write a 64-bit physical address to a low and high register pair.
fn write_address(regs: &Region, low: u64, address: u64) -> Result<(), Failure> {
    regs.write32(low, (address & 0xFFFF_FFFF) as u32)?;
    regs.write32(low + 4, (address >> 32) as u32)?;
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

/// The first e1000 on the bus, with the identity the framework matches on.
fn find_controller() -> Option<(Bdf, DeviceId)> {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .find(|info| info.vendor_id == VENDOR_INTEL && info.device_id == DEVICE_82540EM)
        .map(|info| {
            (
                info.bdf,
                DeviceId {
                    vendor: info.vendor_id,
                    device: info.device_id,
                    class: info.class,
                    subclass: info.subclass,
                    prog_if: info.prog_if,
                },
            )
        })
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
