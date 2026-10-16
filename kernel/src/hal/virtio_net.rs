//! Modern virtio-net split RX/TX queues, MSI-X completions and bounded frame delivery.
//! The gateway ARP exchange is a boot self-test peer, not a general IP configuration.

use super::{
    dma::Dma,
    driver_start::{self, Log},
    msix,
    pci::ConfigAccess,
};
use crate::{
    mm::mmio::{self, Region},
    sync::IrqMutex,
};
use core::sync::atomic::{fence, AtomicUsize, Ordering};
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_net_core::{
    arp,
    queue::{Frame, FrameQueue},
};
use nolx_pci_core::{Bar, BarSet, Bdf, ConfigSpace};
use nolx_virtio_core::{self as virtio, common, net, Descriptor, VirtioCap};

/// MSI-X receive vector (table entry zero).
pub const RX_VECTOR: u8 = 0x53;
/// MSI-X transmit vector (table entry one).
pub const TX_VECTOR: u8 = 0x54;
static RX_INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
static TX_INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
static DEVICE: IrqMutex<Option<Nic>> = IrqMutex::new(None);
static START: IrqMutex<Option<(Log, Result<(), Failure>)>> = IrqMutex::new(None);

/// A refused device, transfer, packet or capability.
#[derive(Debug)]
pub enum Failure {
    /// No supported network controller.
    NoDevice,
    /// Required modern capability, MAC, queue or MSI-X entry is missing.
    Configuration,
    /// Physical memory allocation failed.
    Memory,
    /// An MMIO register was inaccessible.
    Access(MmioError),
    /// MMIO mapping failed.
    Window(mmio::Failure),
    /// Feature negotiation or queue layout was refused.
    Virtio(virtio::VirtioError),
    /// MSI-X programming failed.
    Msix(msix::Error),
    /// A bounded command wait expired.
    Timeout,
    /// A used-ring entry or received packet was invalid.
    Packet,
    /// A queue completed without its interrupt arriving.
    Interrupt,
    /// Another caller owns the synchronous transport.
    Busy,
}
impl From<MmioError> for Failure {
    fn from(e: MmioError) -> Self {
        Self::Access(e)
    }
}
impl From<mmio::Failure> for Failure {
    fn from(e: mmio::Failure) -> Self {
        Self::Window(e)
    }
}
impl From<virtio::VirtioError> for Failure {
    fn from(e: virtio::VirtioError) -> Self {
        Self::Virtio(e)
    }
}
impl From<msix::Error> for Failure {
    fn from(e: msix::Error) -> Self {
        Self::Msix(e)
    }
}

static MATCHES: &[Match] = &[Match::by_ids(0x1af4, 0x1000), Match::by_ids(0x1af4, 0x1041)];

/// Find and start one supported interface through the driver framework.
pub fn probe(console: &mut dyn HalConsole) {
    let Some((_, id)) = find() else {
        let _ = writeln!(console, "virtio-net: not brought up: NoDevice");
        return;
    };
    let driver = Driver {
        name: "virtio-net",
        matches: MATCHES,
        ops: Dispatch {
            probe: |_| true,
            start,
            stop,
        },
    };
    if let Err(error) = driver_start::attach(driver, id) {
        let _ = writeln!(console, "virtio-net: framework refused device: {error:?}");
    }
    if let Some((log, outcome)) = START.lock().as_ref() {
        log.replay(console);
        match outcome {
            Ok(()) => {
                let _ = writeln!(console, "virtio-net: bound and started through nolx-df");
            }
            Err(error) => {
                let _ = writeln!(console, "virtio-net: not brought up: {error:?}");
            }
        }
    }
}
fn start(_: usize) -> bool {
    let mut log = Log::new();
    let outcome = bring_up(&mut log).map(|nic| {
        *DEVICE.lock() = Some(nic);
    });
    let good = outcome.is_ok();
    *START.lock() = Some((log, outcome));
    good
}
fn stop(_: usize) -> bool {
    // Reset can wait for the device. Never hold the IRQ state lock across that wait.
    let Some(nic) = DEVICE.lock().take() else {
        return true;
    };
    let result = nic
        .common
        .write8(u64::from(common::DEVICE_STATUS), 0)
        .map_err(Failure::Access)
        .and_then(|()| poll(|| Ok(nic.common.read8(u64::from(common::DEVICE_STATUS))? == 0)));
    if result.is_err() {
        *DEVICE.lock() = Some(nic);
        return false;
    }
    let command = ConfigAccess.read_u32(nic.bdf, 4) & 0xffff;
    ConfigAccess.write_u32(nic.bdf, 4, command & !4);
    // SAFETY: virtio reset completed and PCI bus mastering is disabled; IRQ state owns no rings.
    unsafe {
        nic.rx.ring.release();
        nic.rx.data.release();
        nic.tx.ring.release();
        nic.tx.data.release();
    }

    true
}

fn find() -> Option<(Bdf, DeviceId)> {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .find(|d| d.vendor_id == 0x1af4 && [0x1000, 0x1041].contains(&d.device_id))
        .map(|d| {
            (
                d.bdf,
                DeviceId {
                    vendor: d.vendor_id,
                    device: d.device_id,
                    class: d.class,
                    subclass: d.subclass,
                    prog_if: d.prog_if,
                },
            )
        })
}
fn map(bars: &BarSet, cap: VirtioCap) -> Result<Region, Failure> {
    let base = match bars.slots.get(usize::from(cap.bar)) {
        Some(Bar::Memory32 { base, .. }) => u64::from(*base),
        Some(Bar::Memory64 { base, .. }) => *base,
        _ => return Err(Failure::Configuration),
    };
    mmio::map(
        base.checked_add(u64::from(cap.offset))
            .ok_or(Failure::Configuration)?,
        u64::from(cap.length),
    )
    .map_err(Into::into)
}
fn write64(regs: &Region, at: u8, value: u64) -> Result<(), Failure> {
    regs.write32(u64::from(at), value as u32)?;
    regs.write32(u64::from(at) + 4, (value >> 32) as u32)?;
    Ok(())
}
fn poll(mut ready: impl FnMut() -> Result<bool, Failure>) -> Result<(), Failure> {
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    for index in 0..100_000_000u32 {
        if ready()? {
            return Ok(());
        }
        match (clock, super::hpet::counter()) {
            (Some((start, hz)), Some(now))
                if super::hpet::elapsed_ticks(start, now) >= hz.saturating_mul(2) =>
            {
                break
            }
            (None, _) if index >= 1_000_000 => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout)
}

struct Queue {
    number: u16,
    ring: Dma,
    data: Dma,
    layout: virtio::SplitLayout,
    notify: Region,
    notify_at: u64,
    size: u16,
    available: u16,
    used: u16,
}
impl Queue {
    fn new(common: &Region, notify: Region, cap: VirtioCap, number: u16) -> Result<Self, Failure> {
        common.write16(u64::from(common::QUEUE_SELECT), number)?;
        let size = common.read16(u64::from(common::QUEUE_SIZE))?.min(8);
        if size == 0 {
            return Err(Failure::Configuration);
        }
        let layout = virtio::split_layout(size)?;
        common.write16(u64::from(common::QUEUE_SIZE), size)?;
        let ring = Dma::new().ok_or(Failure::Memory)?;
        let data = Dma::new().ok_or(Failure::Memory)?;
        write64(common, common::QUEUE_DESC, ring.address(0))?;
        write64(
            common,
            common::QUEUE_DRIVER,
            ring.address(layout.avail_offset),
        )?;
        write64(
            common,
            common::QUEUE_DEVICE,
            ring.address(layout.used_offset),
        )?;
        common.write16(u64::from(common::QUEUE_MSIX_VECTOR), number)?;
        if common.read16(u64::from(common::QUEUE_MSIX_VECTOR))? != number {
            return Err(Failure::Configuration);
        }
        let offset = common.read16(u64::from(common::QUEUE_NOTIFY_OFF))?;
        let notify_at = virtio::notify_address(&cap, offset) - u64::from(cap.offset);
        common.write16(u64::from(common::QUEUE_ENABLE), 1)?;
        Ok(Self {
            number,
            ring,
            data,
            layout,
            notify,
            notify_at,
            size,
            available: 0,
            used: 0,
        })
    }
    fn publish(&mut self, len: u32, receive: bool) -> Result<(), Failure> {
        self.ring.write(
            0,
            &Descriptor {
                addr: self.data.address(0),
                len,
                flags: if receive { virtio::DESC_WRITE } else { 0 },
                next: 0,
            }
            .to_bytes(),
        );
        self.ring.write16(
            self.layout.avail_offset + 4 + u64::from(self.available % self.size) * 2,
            0,
        );
        self.available = self.available.wrapping_add(1);
        fence(Ordering::SeqCst);
        self.ring
            .write16(self.layout.avail_offset + 2, self.available);
        fence(Ordering::SeqCst);
        self.notify.write16(self.notify_at, self.number)?;
        Ok(())
    }
    fn completed(&self) -> bool {
        self.ring.read16(self.layout.used_offset + 2) != self.used
    }
    fn take(&mut self) -> Result<usize, Failure> {
        fence(Ordering::SeqCst);
        let entry = self
            .ring
            .read_array::<8>(self.layout.used_offset + 4 + u64::from(self.used % self.size) * 8);
        self.used = self.used.wrapping_add(1);
        let id = u32::from_le_bytes(entry[..4].try_into().map_err(|_| Failure::Packet)?);
        let len = u32::from_le_bytes(entry[4..].try_into().map_err(|_| Failure::Packet)?) as usize;
        if id != 0 || len > 2048 {
            return Err(Failure::Packet);
        }
        Ok(len)
    }
}
struct Nic {
    bdf: Bdf,
    common: Region,
    rx: Queue,
    tx: Queue,
    frames: FrameQueue<8>,
}
impl Nic {
    fn transmit(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if !(14..=net::MAX_FRAME_BYTES).contains(&bytes.len()) {
            return Err(Failure::Packet);
        }
        let expected = TX_INTERRUPTS.load(Ordering::Acquire) + 1;
        self.tx.data.write(0, &net::transmit_header());
        self.tx.data.write(net::HEADER_BYTES as u64, bytes);
        self.tx
            .publish((net::HEADER_BYTES + bytes.len()) as u32, false)?;
        poll(|| Ok(self.tx.completed()))?;
        self.tx.take()?;
        if msix::wait_for_interrupts(&TX_INTERRUPTS, expected) < expected {
            return Err(Failure::Interrupt);
        }
        Ok(())
    }
    fn receive(&mut self) -> Result<(), Failure> {
        if self.rx.completed() {
            let len = self.rx.take()?;
            let mut packet = [0; 2048];
            self.rx.data.read(0, &mut packet[..len]);
            let frame = net::received_frame(&packet[..len]).ok_or(Failure::Packet)?;
            let _ = self.frames.push(frame);
            self.rx.publish(2048, true)?;
        }
        Ok(())
    }
}

fn bring_up(console: &mut dyn HalConsole) -> Result<Nic, Failure> {
    let (bdf, _) = find().ok_or(Failure::NoDevice)?;
    if !super::apic::is_enabled() {
        return Err(Failure::Interrupt);
    }
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    ConfigAccess.write_u32(bdf, 4, command | 6);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let caps = virtio::discover(&mut ConfigAccess, bdf);
    let common = map(&bars, caps.common.ok_or(Failure::Configuration)?)?;
    let cap = caps.notify.ok_or(Failure::Configuration)?;
    let notify = map(&bars, cap)?;
    let config = map(&bars, caps.device.ok_or(Failure::Configuration)?)?;
    common.write8(u64::from(common::DEVICE_STATUS), 0)?;
    poll(|| Ok(common.read8(u64::from(common::DEVICE_STATUS))? == 0))?;
    for status in &virtio::INIT_STATUS_SEQUENCE[..2] {
        common.write8(u64::from(common::DEVICE_STATUS), *status)?;
    }
    common.write32(u64::from(common::DEVICE_FEATURE_SELECT), 0)?;
    let low = u64::from(common.read32(u64::from(common::DEVICE_FEATURE))?);
    common.write32(u64::from(common::DEVICE_FEATURE_SELECT), 1)?;
    let offered = low | (u64::from(common.read32(u64::from(common::DEVICE_FEATURE))?) << 32);
    let accepted = net::negotiate_features(offered)?;
    for half in 0..2 {
        common.write32(u64::from(common::DRIVER_FEATURE_SELECT), half)?;
        common.write32(
            u64::from(common::DRIVER_FEATURE),
            (accepted >> (half * 32)) as u32,
        )?;
    }
    common.write8(
        u64::from(common::DEVICE_STATUS),
        virtio::INIT_STATUS_SEQUENCE[2],
    )?;
    virtio::check_features_ok(common.read8(u64::from(common::DEVICE_STATUS))?)?;
    let mut mac = [0; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = config.read8(i as u64)?;
    }
    if mac == [0; 6] || mac[0] & 1 != 0 {
        return Err(Failure::Configuration);
    }
    msix::enable(bdf, &bars, 0, RX_VECTOR)?;
    msix::enable(bdf, &bars, 1, TX_VECTOR)?;
    let rx = Queue::new(&common, notify, cap, 0)?;
    let tx = Queue::new(&common, notify, cap, 1)?;
    let mut nic = Nic {
        bdf,
        common,
        rx,
        tx,
        frames: FrameQueue::new(),
    };
    common.write8(
        u64::from(common::DEVICE_STATUS),
        virtio::INIT_STATUS_SEQUENCE[3],
    )?;
    nic.rx.publish(2048, true)?;
    for exchange in 1..=2 {
        let expected = RX_INTERRUPTS.load(Ordering::Acquire) + 1;
        nic.transmit(&arp::request(mac, [10, 0, 2, 15], [10, 0, 2, 2]))?;
        poll(|| Ok(nic.rx.completed()))?;
        if msix::wait_for_interrupts(&RX_INTERRUPTS, expected) < expected {
            return Err(Failure::Interrupt);
        }
        nic.receive()?;
        let frame = nic.frames.pop().ok_or(Failure::Packet)?;
        let peer =
            arp::reply(frame.bytes(), mac, [10, 0, 2, 15], [10, 0, 2, 2]).ok_or(Failure::Packet)?;
        let _ = writeln!(console, "virtio-net: received ARP reply from 10.0.2.2, peer {peer:02x?}, exchange {exchange}, descriptor recycled");
    }
    let _ = writeln!(
        console,
        "virtio-net: MSI-X RX {} TX {} completions, vectors {RX_VECTOR:#x}/{TX_VECTOR:#x}",
        RX_INTERRUPTS.load(Ordering::Acquire),
        TX_INTERRUPTS.load(Ordering::Acquire)
    );
    Ok(nic)
}

/// Acknowledge MSI-X by consuming/reposting received descriptors; bounded and allocation-free.
pub fn on_interrupt(vector: u8) {
    if vector == RX_VECTOR {
        RX_INTERRUPTS.fetch_add(1, Ordering::Release);
        if let Some(nic) = DEVICE.lock().as_mut() {
            let _ = nic.receive();
        }
    } else if vector == TX_VECTOR {
        TX_INTERRUPTS.fetch_add(1, Ordering::Release);
    }
}

/// Submit a frame without holding the IRQ state lock while waiting for completion.
pub fn send_frame(bytes: &[u8]) -> Result<(), Failure> {
    if super::cpu::current_ipl() != nolx_hal_core::ipl::Ipl::PASSIVE {
        return Err(Failure::Busy);
    }
    driver_start::device_operation(
        "virtio-net",
        || {
            let mut nic = DEVICE.lock().take().ok_or(Failure::Busy)?;
            let result = nic.transmit(bytes);
            *DEVICE.lock() = Some(nic);
            result
        },
        |error| {
            if error == nolx_df_core::Error::Busy {
                Failure::Busy
            } else {
                Failure::NoDevice
            }
        },
    )
}
/// Consume one externally received frame from the bounded device queue.
pub fn receive_frame() -> Option<Frame> {
    driver_start::device_operation(
        "virtio-net",
        || {
            let mut slot = DEVICE.lock();
            let nic = slot.as_mut().ok_or(Failure::NoDevice)?;
            nic.receive()?;
            Ok(nic.frames.pop())
        },
        |_| Failure::Busy,
    )
    .ok()
    .flatten()
}

/// Receive-vector entry point, before APIC EOI.
pub fn on_rx_interrupt() {
    on_interrupt(RX_VECTOR);
}
/// Transmit-vector entry point, before APIC EOI.
pub fn on_tx_interrupt() {
    on_interrupt(TX_VECTOR);
}
