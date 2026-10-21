//! Modern virtio-net split RX/TX queues, MSI-X completions and bounded frame delivery.
//! The gateway ARP exchange is a boot self-test peer, not a general IP configuration.

#[cfg(feature = "hal-virtio-net-recovery-test")]
mod recovery;

use super::{
    dma::Dma,
    driver_start::{self, Log},
    irq_vectors, msix,
    pci::ConfigAccess,
};
use crate::{
    mm::mmio::{self, Region},
    sync::IrqMutex,
};
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_net_core::{
    arp,
    queue::{Frame, FrameQueue},
};
use nolx_pci_core::{BarSet, Bdf, ConfigSpace};
use nolx_virtio_core::{self as virtio, common, net, Descriptor, VirtioCap};

/// Preferred MSI-X receive vector (table entry zero) for the first native adapter.
pub const RX_VECTOR: u8 = 0x53;
/// Preferred MSI-X transmit vector (table entry one) for the first native adapter.
pub const TX_VECTOR: u8 = 0x54;
const RX_KIND: irq_vectors::VectorKind = irq_vectors::VectorKind::VirtioNetRx;
const TX_KIND: irq_vectors::VectorKind = irq_vectors::VectorKind::VirtioNetTx;
static VECTORS: [IrqMutex<Option<(u8, u8)>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
static RX_INTERRUPTS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static TX_INTERRUPTS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static DEVICE: [IrqMutex<Option<Nic>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
static LIVE: [AtomicBool; driver_start::DEVICES] =
    [const { AtomicBool::new(false) }; driver_start::DEVICES];
static COMMON: [IrqMutex<Option<Region>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
struct Partial {
    pages: [Option<Dma>; 4],
    rx: Option<Queue>,
    tx: Option<Queue>,
}
static PARTIAL: [IrqMutex<Partial>; driver_start::DEVICES] = [const {
    IrqMutex::new(Partial {
        pages: [const { None }; 4],
        rx: None,
        tx: None,
    })
}; driver_start::DEVICES];
type StartRecord = Option<(Log, Result<(), Failure>)>;
static START: [IrqMutex<StartRecord>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
static BDFS: [IrqMutex<Option<Bdf>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
static PRIMARY: AtomicUsize = AtomicUsize::new(usize::MAX);

fn vectors(dev: usize) -> Option<(u8, u8)> {
    *VECTORS[dev].lock()
}
fn claim_vectors(dev: usize) -> Result<(u8, u8), Failure> {
    let mut assigned = VECTORS[dev].lock();
    if assigned.is_some() {
        return Err(Failure::Busy);
    }
    let rx = irq_vectors::claim(RX_KIND, dev).ok_or(Failure::NoVector)?;
    let Some(tx) = irq_vectors::claim(TX_KIND, dev) else {
        let released = irq_vectors::release(rx, RX_KIND, dev);
        debug_assert!(released, "newly claimed virtio-net RX vector was not owned");
        return Err(Failure::NoVector);
    };
    *assigned = Some((rx, tx));
    Ok((rx, tx))
}
fn release_vectors(dev: usize) -> bool {
    let mut assigned = VECTORS[dev].lock();
    let Some((rx, tx)) = *assigned else {
        return true;
    };
    for (vector, kind) in [(rx, RX_KIND), (tx, TX_KIND)] {
        match irq_vectors::owner(vector) {
            Some(owner) if owner == (kind, dev) => {
                if !irq_vectors::release(vector, kind, dev) {
                    return false;
                }
            }
            // A previous checked-stop attempt can have released the first queue vector
            // before a later queue's ledger compare-and-swap failed. Retry only that
            // partial state; never silently forget a vector owned by another source.
            None => {}
            Some(_) => return false,
        }
    }
    *assigned = None;
    true
}
/// Whether an allocated shared-table vector belongs to one of this driver's two queues.
pub fn handles_vector(vector: u8) -> bool {
    match irq_vectors::owner(vector) {
        Some((kind, _)) => kind == RX_KIND || kind == TX_KIND,
        None => false,
    }
}

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
    /// The shared native interrupt vector domain has no two free vectors for this adapter.
    NoVector,
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

/// Enumerate and bind each supported segment-zero interface exactly once.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = super::pci::enumerate_native();
    let mut found = false;
    for info in inventory
        .as_slice()
        .iter()
        .filter(|d| d.vendor_id == 0x1af4 && [0x1000, 0x1041].contains(&d.device_id))
    {
        found = true;
        if BDFS.iter().any(|bdf| *bdf.lock() == Some(info.bdf)) {
            let _ = writeln!(console, "virtio-net: duplicate PCI binding refused");
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
            name: "virtio-net",
            matches: MATCHES,
            ops: Dispatch {
                probe: |_| true,
                start,
                stop,
            },
        };
        let attached = driver_start::attach_prepared(driver, id, |dev| {
            if BDFS.iter().any(|bdf| *bdf.lock() == Some(info.bdf)) {
                return Err(nolx_df_core::Error::Busy);
            }
            *BDFS[dev].lock() = Some(info.bdf);
            let _ = PRIMARY.compare_exchange(usize::MAX, dev, Ordering::AcqRel, Ordering::Acquire);
            Ok(())
        });
        if let Err(error) = attached {
            let _ = writeln!(console, "virtio-net: framework refused device: {error:?}");
        }
        for (dev, bdf) in BDFS.iter().enumerate() {
            if *bdf.lock() != Some(info.bdf) {
                continue;
            }
            if let Some((log, outcome)) = START[dev].lock().as_ref() {
                log.replay(console);
                match outcome {
                    Ok(()) => {
                        let _ = writeln!(console, "virtio-net: bound and started through nolx-df");
                        let (rx, tx) = vectors(dev).expect("started virtio-net owns both vectors");
                        let mac = DEVICE[dev].lock().as_ref().unwrap().mac;
                        let _ = writeln!(console, "virtio-net: instance {dev}, PCI {:02x}:{:02x}.{}, mac {mac:02x?}, vectors {rx:#x}/{tx:#x}, segment {}", info.bdf.bus, info.bdf.device, info.bdf.function, info.bdf.segment);
                    }
                    Err(error) => {
                        let _ = writeln!(console, "virtio-net: not brought up: {error:?}");
                    }
                }
            }
        }
    }
    if !found {
        let _ = writeln!(console, "virtio-net: not brought up: NoDevice");
    }
}
/// Whether the last start refused because the vector pool was exhausted.
///
/// The refusal reason is kept with the bring-up log so a test can distinguish exhaustion
/// from a configuration or transport failure without parsing console text.
pub fn last_failure_was_no_vector(dev: usize) -> bool {
    matches!(
        START[dev].lock().as_ref(),
        Some((_, Err(Failure::NoVector)))
    )
}

fn start(dev: usize) -> bool {
    #[cfg(feature = "hal-virtio-net-recovery-test")]
    recovery::claim(dev);
    let mut log = Log::new();
    let outcome = bring_up(dev, &mut log);
    let good = outcome.is_ok();
    if !good && stop(dev) {
        // SAFETY: rollback reset acknowledged, PCI DMA/messages disabled and source
        // detached; callback checkout excludes any concurrent transport or lifecycle entry.
        unsafe {
            mmio::release_owned(dev);
        }
    }
    *START[dev].lock() = Some((log, outcome));
    good
}
fn pci_access(bdf: Bdf, dma: bool) -> Result<(), Failure> {
    super::pci_exclusion::pci_access(bdf, dma)
        .then_some(())
        .ok_or(Failure::Configuration)
}
fn disable_messages(bdf: Bdf) -> Result<(), Failure> {
    super::pci_exclusion::disable_messages(bdf)
        .then_some(())
        .ok_or(Failure::Configuration)
}
fn messages_disabled(bdf: Bdf) -> bool {
    super::pci_exclusion::messages_disabled(bdf)
}
fn stop(dev: usize) -> bool {
    LIVE[dev].store(false, Ordering::Release);
    let Some(bdf) = *BDFS[dev].lock() else {
        return false;
    };
    let common = *COMMON[dev].lock();
    let result = (|| -> Result<(), Failure> {
        let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
        ConfigAccess.write_u32(bdf, 4, command | (1 << 10));
        if ConfigAccess.read_u32(bdf, 4) & (1 << 10) == 0 {
            return Err(Failure::Configuration);
        }
        disable_messages(bdf)?;
        #[cfg(feature = "hal-virtio-net-recovery-test")]
        if recovery::refuse_stop(dev) {
            return Err(Failure::Timeout);
        }
        if let Some(common) = common {
            common.write8(u64::from(common::DEVICE_STATUS), 0)?;
            poll(|| Ok(common.read8(u64::from(common::DEVICE_STATUS))? == 0))?;
        } else if !empty_dma(dev) {
            return Err(Failure::Busy);
        }
        pci_access(bdf, false)?;
        let status = if let Some(common) = common {
            common.read8(u64::from(common::DEVICE_STATUS))?
        } else {
            0
        };
        if !net::stop_proven(
            status,
            ConfigAccess.read_u32(bdf, 4),
            messages_disabled(bdf),
        ) {
            return Err(Failure::Configuration);
        }
        #[cfg(feature = "hal-virtio-net-recovery-test")]
        if recovery::withhold_pci_proof(dev) {
            return Err(Failure::Configuration);
        }
        Ok(())
    })();
    if result.is_err() {
        return false;
    }
    if !release_vectors(dev) {
        return false;
    }
    let nic = DEVICE[dev].lock().take();
    let partial = core::mem::replace(
        &mut *PARTIAL[dev].lock(),
        Partial {
            pages: [const { None }; 4],
            rx: None,
            tx: None,
        },
    );
    // SAFETY: virtio reset reads zero, PCI bus mastering and messages read disabled,
    // LIVE is false and callback checkout excludes users. Unique complete/partial owners
    // are moved out only after proof; no descriptor or buffer is released on failure.
    unsafe {
        for page in partial.pages.into_iter().flatten() {
            page.release();
        }
        for queue in [partial.rx, partial.tx].into_iter().flatten() {
            queue.ring.release();
            queue.data.release();
        }
        if let Some(nic) = nic {
            nic.rx.ring.release();
            nic.rx.data.release();
            nic.tx.ring.release();
            nic.tx.data.release();
        }
    }
    *COMMON[dev].lock() = None;
    true
}
fn empty_dma(dev: usize) -> bool {
    let p = PARTIAL[dev].lock();
    p.pages.iter().all(Option::is_none)
        && p.rx.is_none()
        && p.tx.is_none()
        && DEVICE[dev].lock().is_none()
}
pub(crate) fn removable(dev: usize) -> bool {
    COMMON[dev].lock().is_none()
        && !LIVE[dev].load(Ordering::Acquire)
        && empty_dma(dev)
        && vectors(dev).is_none()
}

fn map(dev: usize, bars: &BarSet, cap: VirtioCap) -> Result<Region, Failure> {
    let base = bars
        .slots
        .get(usize::from(cap.bar))
        .copied()
        .and_then(nolx_pci_core::assigned_memory_base)
        .ok_or(Failure::Configuration)?;
    mmio::map_owned(
        dev,
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
    fn new(
        common: &Region,
        notify: Region,
        cap: VirtioCap,
        number: u16,
        pages: &mut [Option<Dma>; 4],
    ) -> Result<Self, Failure> {
        common.write16(u64::from(common::QUEUE_SELECT), number)?;
        let size = common.read16(u64::from(common::QUEUE_SIZE))?.min(8);
        if size == 0 {
            return Err(Failure::Configuration);
        }
        let layout = virtio::split_layout(size)?;
        let offset = common.read16(u64::from(common::QUEUE_NOTIFY_OFF))?;
        let notify_at = virtio::notify_address(&cap, offset)
            .checked_sub(u64::from(cap.offset))
            .ok_or(Failure::Configuration)?;
        if notify_at & 1 != 0
            || notify_at
                .checked_add(2)
                .is_none_or(|end| end > notify.bytes())
        {
            return Err(Failure::Configuration);
        }
        let index = usize::from(number) * 2;
        // All fallible shape/notify accesses precede the infallible ownership move.
        Ok(Self {
            number,
            ring: pages[index].take().unwrap(),
            data: pages[index + 1].take().unwrap(),
            layout,
            notify,
            notify_at,
            size,
            available: 0,
            used: 0,
        })
    }
    fn configure(&self, common: &Region) -> Result<(), Failure> {
        common.write16(u64::from(common::QUEUE_SELECT), self.number)?;
        common.write16(u64::from(common::QUEUE_SIZE), self.size)?;
        write64(common, common::QUEUE_DESC, self.ring.address(0))?;
        write64(
            common,
            common::QUEUE_DRIVER,
            self.ring.address(self.layout.avail_offset),
        )?;
        write64(
            common,
            common::QUEUE_DEVICE,
            self.ring.address(self.layout.used_offset),
        )?;
        common.write16(u64::from(common::QUEUE_MSIX_VECTOR), self.number)?;
        if common.read16(u64::from(common::QUEUE_MSIX_VECTOR))? != self.number {
            return Err(Failure::Configuration);
        }
        common.write16(u64::from(common::QUEUE_ENABLE), 1)?;
        if common.read16(u64::from(common::QUEUE_ENABLE))? != 1 {
            return Err(Failure::Configuration);
        }
        Ok(())
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
    owner: usize,
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    frames: FrameQueue<8>,
}
impl Nic {
    fn transmit(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if !(14..=net::MAX_FRAME_BYTES).contains(&bytes.len()) {
            return Err(Failure::Packet);
        }
        let expected = TX_INTERRUPTS[self.owner].load(Ordering::Acquire) + 1;
        self.tx.data.write(0, &net::transmit_header());
        self.tx.data.write(net::HEADER_BYTES as u64, bytes);
        self.tx
            .publish((net::HEADER_BYTES + bytes.len()) as u32, false)?;
        poll(|| Ok(self.tx.completed()))?;
        self.tx.take()?;
        if msix::wait_for_interrupts(&TX_INTERRUPTS[self.owner], expected) < expected {
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

fn bring_up(dev: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
    let bdf = BDFS[dev].lock().ok_or(Failure::NoDevice)?;
    if !super::apic::is_enabled() {
        return Err(Failure::Interrupt);
    }
    if !removable(dev) {
        return Err(Failure::Busy);
    }
    // Both queue vectors are owned before MSI-X can expose either one to the controller.
    let (rx_vector, tx_vector) = claim_vectors(dev)?;
    pci_access(bdf, false)?;
    disable_messages(bdf)?;
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let caps = virtio::discover(&mut ConfigAccess, bdf);
    let common = map(dev, &bars, caps.common.ok_or(Failure::Configuration)?)?;
    *COMMON[dev].lock() = Some(common);
    let cap = caps.notify.ok_or(Failure::Configuration)?;
    let notify = map(dev, &bars, cap)?;
    let config = map(dev, &bars, caps.device.ok_or(Failure::Configuration)?)?;
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
    msix::enable(dev, bdf, &bars, 0, rx_vector)?;
    msix::enable(dev, bdf, &bars, 1, tx_vector)?;
    for index in 0..4 {
        #[cfg(feature = "hal-virtio-net-recovery-test")]
        if index == 3 && recovery::fail(dev, 1) {
            return Err(Failure::Memory);
        }
        PARTIAL[dev].lock().pages[index] = Some(Dma::new().ok_or(Failure::Memory)?);
    }
    {
        let mut partial = PARTIAL[dev].lock();
        partial.rx = Some(Queue::new(&common, notify, cap, 0, &mut partial.pages)?);
        partial.rx.as_ref().unwrap().configure(&common)?;
    }
    #[cfg(feature = "hal-virtio-net-recovery-test")]
    if recovery::fail(dev, 2) {
        return Err(Failure::Timeout);
    }
    pci_access(bdf, true)?;
    let (rx, tx) = {
        let mut partial = PARTIAL[dev].lock();
        partial.tx = Some(Queue::new(&common, notify, cap, 1, &mut partial.pages)?);
        partial.tx.as_ref().unwrap().configure(&common)?;
        (partial.rx.take().unwrap(), partial.tx.take().unwrap())
    };
    let mut nic = Nic {
        owner: dev,
        mac,
        rx,
        tx,
        frames: FrameQueue::new(),
    };
    LIVE[dev].store(true, Ordering::Release);
    let result = (|| {
        common.write8(
            u64::from(common::DEVICE_STATUS),
            virtio::INIT_STATUS_SEQUENCE[3],
        )?;
        nic.rx.publish(2048, true)?;
        for exchange in 1..=2 {
            let expected = RX_INTERRUPTS[dev].load(Ordering::Acquire) + 1;
            nic.transmit(&arp::request(mac, [10, 0, 2, 15], [10, 0, 2, 2]))?;
            poll(|| Ok(nic.rx.completed()))?;
            if msix::wait_for_interrupts(&RX_INTERRUPTS[dev], expected) < expected {
                return Err(Failure::Interrupt);
            }
            nic.receive()?;
            let frame = nic.frames.pop().ok_or(Failure::Packet)?;
            let peer = arp::reply(frame.bytes(), mac, [10, 0, 2, 15], [10, 0, 2, 2])
                .ok_or(Failure::Packet)?;
            let _ = writeln!(console, "virtio-net: received ARP reply from 10.0.2.2, peer {peer:02x?}, exchange {exchange}, descriptor recycled");
        }
        let _ = writeln!(
            console,
            "virtio-net: MSI-X RX {} TX {} completions, vectors {rx_vector:#x}/{tx_vector:#x}",
            RX_INTERRUPTS[dev].load(Ordering::Acquire),
            TX_INTERRUPTS[dev].load(Ordering::Acquire)
        );
        #[cfg(feature = "hal-virtio-net-recovery-test")]
        if recovery::fail(dev, 3) {
            return Err(Failure::Timeout);
        }
        Ok(())
    })();
    *DEVICE[dev].lock() = Some(nic);
    result
}

/// Attribute each MSI-X completion and receive FIFO to its exact adapter before APIC EOI.
pub fn on_interrupt(vector: u8) {
    let (dev, rx) = match irq_vectors::owner(vector) {
        Some((kind, dev)) if kind == RX_KIND => (dev, true),
        Some((kind, dev)) if kind == TX_KIND => (dev, false),
        _ => return,
    };
    if dev >= driver_start::DEVICES || !LIVE[dev].load(Ordering::Acquire) {
        return;
    }
    let Some(assigned) = vectors(dev) else {
        return;
    };
    if vector != if rx { assigned.0 } else { assigned.1 } {
        return;
    }
    if rx {
        RX_INTERRUPTS[dev].fetch_add(1, Ordering::Release);
        if let Some(nic) = DEVICE[dev].lock().as_mut() {
            let _ = nic.receive();
        }
    } else {
        TX_INTERRUPTS[dev].fetch_add(1, Ordering::Release);
    }
}
fn refused(error: nolx_df_core::Error) -> Failure {
    if error == nolx_df_core::Error::Busy {
        Failure::Busy
    } else {
        Failure::NoDevice
    }
}
/// Compatibility endpoint: always the original first binding, never a replacement peer.
pub fn send_frame(bytes: &[u8]) -> Result<(), Failure> {
    send_frame_on(PRIMARY.load(Ordering::Acquire), bytes)
}
/// Submit through this exact Started adapter without holding an IRQ lock during waits.
pub fn send_frame_on(dev: usize, bytes: &[u8]) -> Result<(), Failure> {
    driver_start::device_operation_handle(
        dev,
        "virtio-net",
        || {
            let mut nic = DEVICE[dev].lock().take().ok_or(Failure::Busy)?;
            let result = nic.transmit(bytes);
            *DEVICE[dev].lock() = Some(nic);
            result
        },
        refused,
    )
}
/// Compatibility receive endpoint for the original first binding.
pub fn receive_frame() -> Option<Frame> {
    receive_frame_on(PRIMARY.load(Ordering::Acquire))
        .ok()
        .flatten()
}
/// Consume a frame only from this Started adapter's bounded FIFO.
pub fn receive_frame_on(dev: usize) -> Result<Option<Frame>, Failure> {
    driver_start::device_operation_handle(
        dev,
        "virtio-net",
        || {
            let mut slot = DEVICE[dev].lock();
            let nic = slot.as_mut().ok_or(Failure::NoDevice)?;
            nic.receive()?;
            Ok(nic.frames.pop())
        },
        refused,
    )
}
/// Return the immutable adapter MAC only while this binding is Started.
pub fn mac(dev: usize) -> Result<[u8; 6], Failure> {
    driver_start::device_operation_handle(
        dev,
        "virtio-net",
        || {
            DEVICE[dev]
                .lock()
                .as_ref()
                .map(|nic| nic.mac)
                .ok_or(Failure::NoDevice)
        },
        refused,
    )
}

/// Per-adapter counters used by the hardware isolation fixture.
#[cfg(any(
    feature = "hal-driver-net-multi-test",
    feature = "hal-virtio-net-recovery-test"
))]
pub(crate) fn interrupt_counts(dev: usize) -> (usize, usize) {
    (
        RX_INTERRUPTS[dev].load(Ordering::Acquire),
        TX_INTERRUPTS[dev].load(Ordering::Acquire),
    )
}

#[cfg(feature = "hal-virtio-net-recovery-test")]
/// Verify checked failure recovery with real network peers.
pub fn recovery_selftest(console: &mut dyn HalConsole) {
    recovery::run(console);
}
