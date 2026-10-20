//! The e1000 driver, run through the driver framework (`nolx-df`). The probe finds the controller
//! by its vendor and device ID and registers a driver for it. The framework binds the device and
//! starts it; starting resets the controller, reads the MAC address from the EEPROM, programs the
//! receive address, sets up one receive ring and one transmit ring of eight legacy descriptors,
//! enables both engines, transmits a 60-byte frame and requests an ARP reply from QEMU's
//! user-network gateway. The level-triggered INTx handler acknowledges ICR, copies completed
//! packets into a bounded FIFO, and recycles receive descriptors. Polling has bounded budgets.
//!
//! The register offsets, descriptor layouts, and ring arithmetic live in `nolx-e1000-core`, where
//! they are host-tested. The framework's rules live in `nolx-df-core`. This module supplies register
//! access, memory for the rings, and the driver's dispatch table.
//!
//! Framework callbacks select a retained controller by handle; each handle owns separate
//! IRQ-visible receive state, TX state, identity and diagnostics. Receive buffers remain owned until
//! checked engine reset and PCI DMA/INTx exclusion succeed. Partial startup retains every owner;
//! stop detaches only this IRQ source, never masks a peer's shared IOAPIC route.
//! RX buffers are retained in a fixed array,
//! with no heap allocation in initialization or interrupt delivery.
//!
//! CI runs QEMU with an e1000 on a user-mode network backend.

#[cfg(feature = "hal-e1000-recovery-test")]
mod recovery;

use core::sync::atomic::{fence, AtomicU8, AtomicUsize, Ordering};
use nolx_net_core::{arp, queue::FrameQueue};

use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_e1000_core::{self as e1000, DEVICE_82540EM, VENDOR_INTEL};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_power_core::routing::Interrupt;
use nolx_pci_core::{Bdf, ConfigSpace};

use super::pci::ConfigAccess;
use super::{dma::Dma, driver_start};
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;

static RX_VECTOR: [AtomicU8; driver_start::DEVICES] =
    [const { AtomicU8::new(0) }; driver_start::DEVICES];
static PCI_ROUTE: [IrqMutex<Option<(Interrupt, u8)>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
const INTX_KIND: super::irq_vectors::VectorKind = super::irq_vectors::VectorKind::E1000Intx;
static RX_INTERRUPTS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static RECEIVE: [IrqMutex<Option<Receive>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];

struct Receive {
    regs: Region,
    descriptors: Dma,
    buffers: [Option<Dma>; RING as usize],
    next: u32,
    frames: FrameQueue<8>,
}

/// Whether this dynamically owned vector names a shared e1000 INTx GSI route.
pub fn handles_vector(vector: u8) -> bool {
    matches!(super::irq_vectors::owner(vector), Some((kind, _)) if kind == INTX_KIND)
}

/// Acknowledge interrupt causes and recycle completed receive descriptors into a bounded FIFO.
/// This path never allocates, prints, or acquires the framework state lock.
pub fn on_interrupt(vector: u8) {
    if !handles_vector(vector) {
        return;
    }
    // A GSI route may be shared: service every active adapter on that vector before one EOI.
    for (dev, route) in RX_VECTOR.iter().enumerate() {
        if route.load(Ordering::Acquire) == vector {
            service_interrupt(dev);
        }
    }
}
fn service_interrupt(dev: usize) {
    let mut receive = RECEIVE[dev].lock();
    let Some(receive) = receive.as_mut() else {
        return;
    };
    let causes = receive.regs.read32(e1000::REG_ICR).unwrap_or(0);
    if causes & e1000::ICR_RXT0 == 0 {
        return;
    }
    RX_INTERRUPTS[dev].fetch_add(1, Ordering::Release);
    for _ in 0..RING {
        let offset = u64::from(receive.next) * 16;
        let status = receive.descriptors.read_array::<2>(offset + 12);
        if status[0] & e1000::DESC_DD == 0 {
            break;
        }
        fence(Ordering::Acquire);
        let len = usize::from(receive.descriptors.read16(offset + 8));
        if status[0] & e1000::RX_STATUS_EOP != 0
            && status[1] == 0
            && len <= nolx_net_core::FRAME_MAX_DEFAULT
        {
            let mut bytes = [0; nolx_net_core::FRAME_MAX_DEFAULT];
            if let Some(buffer) = &receive.buffers[receive.next as usize] {
                buffer.read(0, &mut bytes[..len]);
                receive.frames.push(&bytes[..len]);
            }
        }
        receive.descriptors.write(offset + 8, &[0; 8]);
        fence(Ordering::Release);
        let _ = receive.regs.write32(e1000::REG_RDT, receive.next);
        receive.next = (receive.next + 1) % RING;
    }
}

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
    /// Firmware INTx assignment is unavailable or its interrupt was not delivered.
    Interrupt,
    /// No matching ARP reply was received from the QEMU network peer.
    Receive,
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
    /// Another caller owns the transport or framework callback.
    Busy,
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

/// The TX ring and buffer, retained until a successful engine disable/reset.
struct Rings {
    tx: Dma,
    frame: Dma,
    next: u32,
}

/// What the bring-up established, for the boot report.
struct Report {
    bdf: Bdf,
    base: u64,
    mac: [u8; 6],
    link_up: bool,
    peer_mac: [u8; 6],
    region: Region,
    interrupts: usize,
}

/// State shared between the driver's dispatch calls and the probe that reports them.
struct State {
    bdf: Option<Bdf>,
    region: Option<Region>,
    rings: Option<Rings>,
    report: Option<Report>,
    error: Option<Failure>,
}

static STATE: [IrqMutex<State>; driver_start::DEVICES] = [const {
    IrqMutex::new(State {
        bdf: None,
        region: None,
        rings: None,
        report: None,
        error: None,
    })
}; driver_start::DEVICES];

/// The driver's dispatch table. Probe accepts any device the match rule selects; start and stop
/// act on the controller selected by the framework handle.
const DISPATCH: Dispatch = Dispatch {
    probe: accept,
    start: start_device,
    stop: stop_device,
};

/// Probe for an e1000, register the driver with the framework, bind and start the device, and
/// report the result. Failure is reported on the console and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = super::pci::enumerate_native();
    let mut found = false;
    for info in inventory
        .as_slice()
        .iter()
        .filter(|info| info.vendor_id == VENDOR_INTEL && info.device_id == DEVICE_82540EM)
    {
        found = true;
        if info.bdf.segment != 0 {
            let _ = writeln!(
                console,
                "e1000: segment {} skipped; native INTx routing is segment-zero only",
                info.bdf.segment
            );
            continue;
        }
        if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
            let _ = writeln!(console, "e1000: duplicate PCI binding refused");
            continue;
        }
        let id = DeviceId {
            vendor: info.vendor_id,
            device: info.device_id,
            class: info.class,
            subclass: info.subclass,
            prog_if: info.prog_if,
        };
        let driver = Driver {
            name: "e1000",
            matches: MATCHES,
            ops: DISPATCH,
        };
        let attached = driver_start::attach_prepared(driver, id, |dev| {
            if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
                return Err(nolx_df_core::Error::Busy);
            }
            STATE[dev].lock().bdf = Some(info.bdf);
            Ok(())
        });
        if let Err(error) = attached {
            let _ = writeln!(console, "e1000: framework refused the device: {error:?}");
        }

        for (dev, slot) in STATE.iter().enumerate() {
            let state = slot.lock();
            if state.bdf != Some(info.bdf) {
                continue;
            }
            match (&state.report, &state.error) {
                (Some(report), _) => {
                    #[cfg(feature = "hal-pci-intx-poison-test")]
                    let _ = writeln!(
                        console,
                        "pci: INTx line poisoned to ff at {:?}, pin {} preserved",
                        report.bdf,
                        (ConfigAccess.read_u32(report.bdf, 0x3c) >> 8) & 255
                    );

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
                    let _ = writeln!(console, "e1000: received ARP reply from 10.0.2.2, peer {:02x?}, descriptor recycled, INTx vector {:#x}, interrupts {}", report.peer_mac, RX_VECTOR[dev].load(Ordering::Acquire), report.interrupts);
                    let _ = writeln!(console, "e1000: bound and started through nolx-df");
                    let (irq, vector) = { *PCI_ROUTE[dev].lock() }
                        .expect("started e1000 owns its shared INTx route");
                    let _ = writeln!(
                        console,
                        "e1000: instance {dev}, PCI {:02x}:{:02x}.{}, mac {:02x?}, GSI {}, vector {:#x}, segment {}",
                        report.bdf.bus,
                        report.bdf.device,
                        report.bdf.function,
                        report.mac,
                        irq.gsi,
                        vector,
                        report.bdf.segment
                    );
                }
                (None, Some(error)) => {
                    let _ = writeln!(console, "e1000: not brought up: {error:?}");
                }
                (None, None) => {
                    let _ = writeln!(console, "e1000: not started");
                }
            }
        }
    }
    if !found {
        let _ = writeln!(console, "e1000: no controller on the PCI bus");
    }
}

fn accept(_: &DeviceId) -> bool {
    true
}

/// The framework's start operation: the whole bring-up. The result goes to [`STATE`].
fn start_device(dev: usize) -> bool {
    #[cfg(feature = "hal-e1000-recovery-test")]
    recovery::claim(dev);
    match bring_up(dev) {
        Ok(report) => {
            let mut state = STATE[dev].lock();
            state.report = Some(report);
            state.error = None;
            true
        }
        Err(failure) => {
            STATE[dev].lock().error = Some(failure);
            if stop_device(dev) {
                // SAFETY: checked rollback detached this IRQ source, reset engines and
                // verified PCI DMA exclusion; the framework callback still excludes I/O.
                unsafe {
                    mmio::release_owned(dev);
                }
            }
            false
        }
    }
}

/// Mask only this adapter, never the shared IOAPIC line. Retain every owner on failure.
fn stop_device(dev: usize) -> bool {
    let (bdf, regs) = {
        let s = STATE[dev].lock();
        (s.bdf, s.region)
    };
    let Some(bdf) = bdf else {
        return false;
    };
    let reset = (|| -> Result<(), Failure> {
        let command = ConfigAccess.read_u32(bdf, PCI_COMMAND) & 0xffff;
        ConfigAccess.write_u32(bdf, PCI_COMMAND, command | (1 << 10));
        if ConfigAccess.read_u32(bdf, PCI_COMMAND) & (1 << 10) == 0 {
            return Err(Failure::Interrupt);
        }
        if let Some(regs) = regs {
            regs.write32(0x00d8, u32::MAX)?;
            if regs.read32(e1000::REG_IMS)? != 0 {
                return Err(Failure::Interrupt);
            }
            // Keep the source routable until its device mask and PCI INTx-disable bit both
            // read back. An early failure must not strand an asserted shared line.
            RX_VECTOR[dev].store(0, Ordering::Release);
            #[cfg(feature = "hal-e1000-recovery-test")]
            if recovery::refuse_stop(dev) {
                return Err(Failure::Timeout("injected quiescence refusal"));
            }
            regs.write32(e1000::REG_RCTL, 0)?;
            regs.write32(e1000::REG_TCTL, 0)?;
            poll(
                || {
                    Ok(regs.read32(e1000::REG_RCTL)? & 2 == 0
                        && regs.read32(e1000::REG_TCTL)? & 2 == 0)
                },
                "engine disable",
            )?;
            regs.write32(
                e1000::REG_CTRL,
                regs.read32(e1000::REG_CTRL)? | e1000::CTRL_RST,
            )?;
            poll(
                || Ok(regs.read32(e1000::REG_CTRL)? & e1000::CTRL_RST == 0),
                "stop reset",
            )?;
            regs.write32(0x00d8, u32::MAX)?;
            let _ = regs.read32(e1000::REG_ICR)?;
        } else {
            if STATE[dev].lock().rings.is_some() || RECEIVE[dev].lock().is_some() {
                return Err(Failure::Busy);
            }
            RX_VECTOR[dev].store(0, Ordering::Release);
        }
        let command = ConfigAccess.read_u32(bdf, PCI_COMMAND) & 0xffff;
        ConfigAccess.write_u32(
            bdf,
            PCI_COMMAND,
            (command & !COMMAND_BUS_MASTER) | (1 << 10),
        );
        let command = ConfigAccess.read_u32(bdf, PCI_COMMAND);
        if let Some(regs) = regs {
            if !e1000::stop_proven(
                regs.read32(e1000::REG_CTRL)?,
                regs.read32(e1000::REG_RCTL)?,
                regs.read32(e1000::REG_TCTL)?,
                regs.read32(e1000::REG_IMS)?,
                command,
            ) {
                return Err(Failure::Busy);
            }
            #[cfg(feature = "hal-e1000-recovery-test")]
            if recovery::withhold_pci_proof(dev) {
                return Err(Failure::Busy);
            }
        } else if command & (COMMAND_BUS_MASTER | (1 << 10)) != 1 << 10 {
            return Err(Failure::Busy);
        }
        Ok(())
    })();
    if reset.is_err() {
        return false;
    }
    let route = { *PCI_ROUTE[dev].lock() };
    if let Some((irq, vector)) = route {
        if !super::ioapic::release_pci_route(irq, vector) {
            return false;
        }
        *PCI_ROUTE[dev].lock() = None;
    }
    let receive = RECEIVE[dev].lock().take();
    let rings = {
        let mut s = STATE[dev].lock();
        s.report = None;
        s.region = None;
        s.rings.take()
    };
    // SAFETY: engine disable/reset acknowledged, masks read disabled, PCI bus mastering
    // reads disabled and INTx blocked; detached ISR source plus framework checkout exclude users.
    unsafe {
        if let Some(receive) = receive {
            receive.descriptors.release();
            for buffer in receive.buffers.into_iter().flatten() {
                buffer.release();
            }
        }
        if let Some(rings) = rings {
            rings.tx.release();
            rings.frame.release();
        }
    }
    true
}

// No address from this batch is exposed before all eleven pages have an owner.
fn allocate_pages(dev: usize) -> Result<[Dma; 11], Failure> {
    let mut pages: [Option<Dma>; 11] = core::array::from_fn(|_| None);
    for i in 0..pages.len() {
        #[cfg(feature = "hal-e1000-recovery-test")]
        let injected = i == 3 && recovery::fail(dev, 1);
        #[cfg(not(feature = "hal-e1000-recovery-test"))]
        let injected = {
            let _ = dev;
            false
        };
        pages[i] = if injected { None } else { Dma::new() };
        if pages[i].is_none() {
            // SAFETY: this batch has not been published to registers, software ring owners,
            // or the ISR. Only this local array can reference the uniquely allocated pages.
            unsafe {
                for page in pages.into_iter().flatten() {
                    page.release();
                }
            }
            return Err(Failure::Memory);
        }
    }
    Ok(pages.map(Option::unwrap))
}

fn bring_up(dev: usize) -> Result<Report, Failure> {
    let initial_interrupts = RX_INTERRUPTS[dev].load(Ordering::Acquire);
    let bdf = STATE[dev].lock().bdf.ok_or(Failure::NoController)?;
    let owned = {
        let state = STATE[dev].lock();
        state.region.is_some() || state.rings.is_some()
    };
    if owned || RECEIVE[dev].lock().is_some() {
        return Err(Failure::Busy);
    }
    pci_access(bdf, false, false)?;
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base =
        nolx_pci_core::assigned_memory_base(bars.slots[BAR0_SLOT]).ok_or(Failure::BarNotMemory)?;
    let regs = mmio::map_owned(dev, base, REGISTERS_BYTES)?;
    STATE[dev].lock().region = Some(regs);

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
    let [rx, b0, b1, b2, b3, b4, b5, b6, b7, tx, frame] = allocate_pages(dev)?;
    let buffers = [b0, b1, b2, b3, b4, b5, b6, b7].map(Some);
    for (slot, buffer) in buffers.iter().enumerate() {
        rx.write(
            slot as u64 * e1000::DESCRIPTOR_BYTES as u64,
            &e1000::rx_descriptor(buffer.as_ref().unwrap().address(0)),
        );
    }
    let rx_address = rx.address(0);
    let tx_address = tx.address(0);
    let frame_address = frame.address(0);
    *RECEIVE[dev].lock() = Some(Receive {
        regs,
        descriptors: rx,
        buffers,
        next: 0,
        frames: FrameQueue::new(),
    });
    STATE[dev].lock().rings = Some(Rings { tx, frame, next: 2 });
    fence(Ordering::SeqCst);
    write_address(&regs, e1000::REG_RDBAL, rx_address)?;
    regs.write32(e1000::REG_RDLEN, ring_bytes)?;
    regs.write32(e1000::REG_RDH, 0)?;
    regs.write32(e1000::REG_RDT, RING - 1)?;
    #[cfg(feature = "hal-pci-intx-poison-test")]
    {
        let old = ConfigAccess.read_u32(bdf, 0x3c);
        ConfigAccess.write_u32(bdf, 0x3c, (old & !255) | 255);
        let readback = ConfigAccess.read_u32(bdf, 0x3c);
        if readback & 0xffff != (old & 0xff00) | 255 {
            return Err(Failure::Interrupt);
        }
    }
    let irq = super::pci_intx::lookup(bdf).ok_or(Failure::Interrupt)?;
    if !super::apic::is_enabled() || irq.gsi >= 24 {
        return Err(Failure::Interrupt);
    }
    // Keep local interrupts masked across route acquisition and publication of the source
    // identity. The GSI may already be shared and unmasked for a peer adapter.
    let _interrupts = super::cpu::InterruptGuard::disable();
    let vector = super::ioapic::acquire_pci_route(irq).ok_or(Failure::Interrupt)?;
    *PCI_ROUTE[dev].lock() = Some((irq, vector));
    RX_VECTOR[dev].store(vector, Ordering::Release);
    // Clear stale causes, enable the NIC source and PCI INTx only after the shared vector is
    // owned and its per-device dispatch identity is visible.
    let _ = regs.read32(e1000::REG_ICR)?;
    regs.write32(e1000::REG_IMS, e1000::ICR_RXT0)?;
    pci_access(bdf, true, true)?;
    regs.write32(e1000::REG_RCTL, e1000::rctl_enable(true))?;
    drop(_interrupts);
    #[cfg(feature = "hal-e1000-recovery-test")]
    if recovery::fail(dev, 2) {
        return Err(Failure::Timeout("injected RX-only start failure"));
    }

    // Transmit ring, empty to begin with.
    write_address(&regs, e1000::REG_TDBAL, tx_address)?;
    regs.write32(e1000::REG_TDLEN, ring_bytes)?;
    regs.write32(e1000::REG_TDH, 0)?;
    regs.write32(e1000::REG_TDT, 0)?;
    regs.write32(e1000::REG_TCTL, e1000::tctl_enable())?;

    // One frame: broadcast destination, this MAC as the source, the test EtherType, the payload,
    // zero padding to the minimum length.
    let mut bytes = [0u8; FRAME_BYTES];
    bytes[0..6].fill(0xFF);
    bytes[6..12].copy_from_slice(&mac);
    bytes[12..14].copy_from_slice(&FRAME_TYPE);
    bytes[14..14 + FRAME_PAYLOAD.len()].copy_from_slice(FRAME_PAYLOAD);
    STATE[dev]
        .lock()
        .rings
        .as_ref()
        .unwrap()
        .frame
        .write(0, &bytes);
    let descriptor =
        e1000::tx_descriptor(frame_address, FRAME_BYTES as u16, true).ok_or(Failure::Frame)?;
    STATE[dev]
        .lock()
        .rings
        .as_ref()
        .unwrap()
        .tx
        .write(0, &descriptor);
    fence(Ordering::SeqCst);
    regs.write32(e1000::REG_TDT, 1)?;
    poll(
        || {
            let mut status = [0u8; 1];
            STATE[dev]
                .lock()
                .rings
                .as_ref()
                .unwrap()
                .tx
                .read(12, &mut status);
            Ok(e1000::tx_done(status[0]))
        },
        "transmit",
    )?;

    // A second TX descriptor asks the QEMU user-network gateway for a real ARP reply.
    // This fixed peer belongs to the boot self-test, not an inferred network configuration.
    let local_ip = [10, 0, 2, 15];
    let peer_ip = [10, 0, 2, 2];
    {
        let s = STATE[dev].lock();
        let rings = s.rings.as_ref().unwrap();
        rings.frame.write(0, &arp::request(mac, local_ip, peer_ip));
        rings.tx.write(16, &descriptor);
    }
    fence(Ordering::SeqCst);
    regs.write32(e1000::REG_TDT, 2)?;
    // At boot IF is masked, so the descriptor signals readiness. On restart IF may already
    // be enabled: the ISR can consume/recycle it before this poll, so accept its fresh counter
    // too. Never wait solely for a DD bit that the IRQ handler has already cleared.
    poll(
        || {
            let receive = RECEIVE[dev].lock();
            let receive = receive.as_ref().ok_or(Failure::Receive)?;
            Ok(
                RX_INTERRUPTS[dev].load(Ordering::Acquire) > initial_interrupts
                    || receive.descriptors.read_array::<1>(12)[0] & e1000::DESC_DD != 0,
            )
        },
        "ARP receive descriptor",
    )?;
    if super::msix::wait_for_interrupts(&RX_INTERRUPTS[dev], initial_interrupts + 1)
        < initial_interrupts + 1
    {
        return Err(Failure::Interrupt);
    }
    let peer_mac = {
        let mut receive = RECEIVE[dev].lock();
        let receive = receive.as_mut().ok_or(Failure::Receive)?;
        let mut peer = None;
        while let Some(frame) = receive.frames.pop() {
            if let Some(mac) = arp::reply(frame.bytes(), mac, local_ip, peer_ip) {
                peer = Some(mac);
                break;
            }
        }
        peer.ok_or(Failure::Receive)?
    };
    let link_up = regs.read32(e1000::REG_STATUS)? & e1000::STATUS_LINK_UP != 0;
    #[cfg(feature = "hal-e1000-recovery-test")]
    if recovery::fail(dev, 3) {
        return Err(Failure::Timeout("injected post-ARP start failure"));
    }
    Ok(Report {
        bdf,
        base,
        mac,
        link_up,
        peer_mac,
        region: regs,
        interrupts: RX_INTERRUPTS[dev].load(Ordering::Acquire) - initial_interrupts,
    })
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

fn pci_access(bdf: Bdf, dma: bool, interrupts: bool) -> Result<(), Failure> {
    let command = ConfigAccess.read_u32(bdf, PCI_COMMAND) & 0xffff;
    let wanted = (command | COMMAND_MEMORY_SPACE) & !(COMMAND_BUS_MASTER | (1 << 10))
        | if dma { COMMAND_BUS_MASTER } else { 0 }
        | if interrupts { 0 } else { 1 << 10 };
    ConfigAccess.write_u32(bdf, PCI_COMMAND, wanted);
    if ConfigAccess.read_u32(bdf, PCI_COMMAND)
        & (COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER | (1 << 10))
        != wanted & (COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER | (1 << 10))
    {
        return Err(Failure::Interrupt);
    }
    Ok(())
}

/// Poll with a two-second HPET budget and hard iteration bound, or a bounded clockless fallback.
fn poll(
    mut ready: impl FnMut() -> Result<bool, Failure>,
    step: &'static str,
) -> Result<(), Failure> {
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    for iteration in 0..POLL_LIMIT * 100 {
        if ready()? {
            return Ok(());
        }
        match (clock, super::hpet::counter()) {
            (Some((start, frequency)), Some(now)) => {
                if super::hpet::elapsed_ticks(start, now) >= frequency.saturating_mul(2) {
                    break;
                }
            }
            _ if iteration >= POLL_LIMIT - 1 => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout(step))
}

#[cfg(feature = "hal-driver-lifecycle-test")]
pub(crate) fn lifecycle_diagnostic(dev: usize, console: &mut dyn HalConsole) {
    let _ = writeln!(
        console,
        "e1000: lifecycle error: {:?}",
        STATE[dev].lock().error
    );
}

fn refused(error: nolx_df_core::Error) -> Failure {
    if error == nolx_df_core::Error::Busy {
        Failure::Busy
    } else {
        Failure::NoController
    }
}
/// Identity of this exact Started controller.
pub fn mac(dev: usize) -> Result<[u8; 6], Failure> {
    driver_start::device_operation_handle(
        dev,
        "e1000",
        || {
            STATE[dev]
                .lock()
                .report
                .as_ref()
                .map(|r| r.mac)
                .ok_or(Failure::NoController)
        },
        refused,
    )
}
/// Synchronous bounded TX on this controller; no state/IRQ lock is held while waiting.
pub fn send_frame_on(dev: usize, bytes: &[u8]) -> Result<(), Failure> {
    driver_start::device_operation_handle(
        dev,
        "e1000",
        || {
            if !(14..=nolx_net_core::FRAME_MAX_DEFAULT).contains(&bytes.len()) {
                return Err(Failure::Frame);
            }
            let (mut rings, regs) = {
                let mut state = STATE[dev].lock();
                let regs = state.report.as_ref().ok_or(Failure::NoController)?.region;
                (state.rings.take().ok_or(Failure::Busy)?, regs)
            };
            let result = (|| {
                rings.frame.write(0, bytes);
                let descriptor =
                    e1000::tx_descriptor(rings.frame.address(0), bytes.len() as u16, true)
                        .ok_or(Failure::Frame)?;
                let offset = u64::from(rings.next) * e1000::DESCRIPTOR_BYTES as u64;
                rings.tx.write(offset, &descriptor);
                fence(Ordering::SeqCst);
                let next = (rings.next + 1) % RING;
                regs.write32(e1000::REG_TDT, next)?;
                poll(
                    || Ok(e1000::tx_done(rings.tx.read_array::<1>(offset + 12)[0])),
                    "public transmit",
                )?;
                rings.next = next;
                Ok(())
            })();
            STATE[dev].lock().rings = Some(rings);
            result
        },
        refused,
    )
}
/// Consume only this controller's receive FIFO; a peer cannot authorize a stopped target.
pub fn receive_frame_on(dev: usize) -> Result<Option<nolx_net_core::queue::Frame>, Failure> {
    driver_start::device_operation_handle(
        dev,
        "e1000",
        || {
            Ok(RECEIVE[dev]
                .lock()
                .as_mut()
                .ok_or(Failure::NoController)?
                .frames
                .pop())
        },
        refused,
    )
}

/// Count only this controller's acknowledged receive interrupt causes.
#[cfg(any(
    feature = "hal-driver-net-multi-test",
    feature = "hal-e1000-recovery-test"
))]
pub(crate) fn interrupt_count(dev: usize) -> usize {
    RX_INTERRUPTS[dev].load(Ordering::Acquire)
}

/// Whether every native software/DMA owner has been safely retired (diagnostic BDF remains).
pub(crate) fn removable(dev: usize) -> bool {
    let s = STATE[dev].lock();
    s.region.is_none()
        && s.rings.is_none()
        && s.report.is_none()
        && RECEIVE[dev].lock().is_none()
        && PCI_ROUTE[dev].lock().is_none()
        && RX_VECTOR[dev].load(Ordering::Acquire) == 0
}
#[cfg(feature = "hal-e1000-recovery-test")]
/// Exercise checked native recovery with captured public network traffic.
pub fn recovery_selftest(console: &mut dyn HalConsole) {
    recovery::run(console);
}
