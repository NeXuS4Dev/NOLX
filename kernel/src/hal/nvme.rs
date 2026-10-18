//! The NVMe driver's probe: find the controller, enable it, create one admin queue pair and one
//! I/O queue pair, identify namespace 1, read sector 0, then write a pattern to sectors 1600 to
//! 1603, read them back, and compare CRC-32 checksums. Everything is polled.
//!
//! The rules -- the register offsets, the enable sequence, the entry layouts, the completion
//! status -- live in `nolx-nvme-core`, where they are host-tested. This module supplies register
//! access through an [`mmio`] window, queue and data memory from [`Dma`], and bounded polling.
//!
//! Each queue is one page of submission entries and one page of completion entries, both from
//! [`Dma`]. The controller is given the physical addresses. CI runs QEMU with a copy of the disk
//! image attached as an NVMe namespace, so the bytes read come from the disk, and CI checks the
//! written sectors in that copy.
//!
//! Ownership is explicit. A controller owns six DMA pages -- two for the admin queue, two for
//! the I/O queue, one for the identify structure and one for the data buffer -- and one
//! register window, plus the MSI-X table window the handle owns. It keeps them until a checked
//! stop has proved that the controller is disabled and no longer ready, that bus mastering is
//! off and that message delivery is disabled. A failed start keeps every owner it took, and a
//! failed command quarantines the transport, so no flush and no buffer reuse follows it.

use super::blockdev;
use super::driver_start::{self, Log};
use super::msix;
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_nvme_core::{self as nvme, Sqe};
use nolx_pci_core::{Bdf, ConfigSpace};
use nolx_virtio_core::crc32;

use super::dma::Dma;
use super::pci::ConfigAccess;
use super::virtio_blk::fill_pattern;
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicUsize, Ordering};

#[cfg(feature = "hal-ahci-nvme-recovery-test")]
mod recovery;

/// NVMe class code: mass storage, non-volatile memory, NVM Express interface.
const NVME_CLASS: u8 = 0x01;
const NVME_SUBCLASS: u8 = 0x08;
const NVME_PROG_IF: u8 = 0x02;
/// The base address register that holds the controller registers. NVMe puts them in BAR 0.
const BAR0_SLOT: usize = 0;
/// The controller registers and their doorbells, mapped as one window. The doorbells for the two
/// queues sit well inside it for any stride the driver accepts.
const REGISTERS_BYTES: u64 = 0x4000;
/// The largest doorbell stride the driver accepts. Larger strides would put the doorbells beyond
/// the window.
const STRIDE_MAX: u32 = 4;
/// Entries in each queue. Eight is enough for one command at a time.
const QUEUE_ENTRIES: u16 = 8;
/// The queue pair the driver creates for I/O, and the namespace it uses.
const IO_QUEUE: u16 = 1;
const NSID: u32 = 1;
/// The block size the driver's sector arithmetic assumes.
const SECTOR_BYTES: usize = 512;
/// The first bytes of sector 0 of the disk image, which CI writes before the boot.
const SECTOR_PATTERN: &[u8] = b"NOLX-BLK-SECTOR0";
/// The round trip: its first sector and how many sectors. They sit in the gap after the last
/// partition the CI image defines, as the other drivers' round trips do.
const ROUND_TRIP_SECTOR: u64 = 1600;
const ROUND_TRIP_SECTORS: u16 = 4;
const ROUND_TRIP_BYTES: usize = ROUND_TRIP_SECTORS as usize * SECTOR_BYTES;
/// How many times a bounded wait looks before it gives up.
const POLL_LIMIT: u32 = 1_000_000;

/// The interrupt vector the I/O queue's completions arrive on. It is delivered through MSI-X table
/// entry [`MSIX_ENTRY`].
pub const COMPLETION_VECTOR: u8 = 0x50;
/// The MSI-X table entry for the I/O completion queue. Entry 0 stays masked: the admin queue also
/// signals on it, and the driver counts only I/O completions.
const MSIX_ENTRY: u16 = 1;

/// Completion interrupts received since boot. Written by the interrupt handler.
static COMPLETION_INTERRUPTS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
/// I/O commands submitted through [`execute_io`]. Each one should produce one interrupt.
static IO_SUBMITTED: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
/// Whether the transport may be used. A failed command clears it, and only a checked stop
/// and a fresh start set it again.
static LIVE: [AtomicBool; driver_start::DEVICES] =
    [const { AtomicBool::new(false) }; driver_start::DEVICES];
/// The controller configuration register as the last checked stop left it. The recovery
/// fixture reads it to prove the controller was disabled before the pages were released.
static STOPPED_CONTROL: [AtomicU32; driver_start::DEVICES] =
    [const { AtomicU32::new(0) }; driver_start::DEVICES];

/// Why the probe stopped.
#[derive(Debug)]
pub enum Failure {
    /// Filesystem verification on this controller failed.
    Filesystem(super::storage_volume::Failure),
    /// The block queue refused the request, or did not hand it back.
    Queue(QueueError),
    /// No NVMe controller is on the PCI bus.
    NoController,
    /// BAR 0 is not a memory BAR, so the controller registers cannot be reached.
    BarNotMemory,
    /// The controller's doorbell stride is too large for the mapped window.
    Stride(u32),
    /// The NVMe rules refused a queue size, command, or field.
    Encoding,
    /// No frame was available for queue or data memory.
    Memory,
    /// The MMIO window refused a mapping.
    Window(mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// A bounded wait ran out, with the step named.
    Timeout(&'static str),
    /// The completion answered a different command than the one submitted.
    Identifier,
    /// The controller completed a command with a non-zero status field.
    Status(u16),
    /// The namespace is empty, or its block size is not 512 bytes.
    Namespace,
    /// The data read back is not the pattern the disk image holds.
    Pattern,
    /// The round trip read back different bytes from the ones written.
    RoundTrip,
    /// MSI-X could not be set up for the controller.
    Msix(msix::Error),
    /// Fewer completion interrupts arrived than I/O commands were submitted.
    Interrupts {
        /// Completion interrupts that arrived.
        delivered: usize,
        /// I/O commands submitted.
        expected: usize,
    },
    /// The controller did not prove that it can no longer reach the driver's memory.
    NotExcluded,
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

/// Probe for an NVMe controller, bring it up, and run the sector read and round trip. Failure is
/// reported on the console and does not stop boot.
/// What the framework's start operation leaves for the probe: the bring-up's output and result.
struct State {
    log: Log,
    outcome: Option<Result<(), Failure>>,
    disk: Option<Disk>,
    bdf: Option<Bdf>,
    /// The controller register window. The handle owns the mapping, so the framework releases
    /// it after a successful stop.
    regs: Option<Region>,
    /// The admin queue pair a partial start still owns.
    admin: Option<Queue>,
    /// The identify buffer a partial start still owns.
    identify: Option<Dma>,
    /// The I/O queue pair a partial start still owns.
    io: Option<Queue>,
    /// The data buffer a partial start still owns.
    data: Option<Dma>,
}

static STATE: [IrqMutex<State>; driver_start::DEVICES] = [const {
    IrqMutex::new(State {
        log: Log::new(),
        outcome: None,
        disk: None,
        bdf: None,
        regs: None,
        admin: None,
        identify: None,
        io: None,
        data: None,
    })
}; driver_start::DEVICES];

/// The owners a bring-up holds before the disk is complete. A failed start leaves them with
/// the handle, so a checked stop can release them.
#[derive(Default)]
struct Owners {
    regs: Option<Region>,
    admin: Option<Queue>,
    identify: Option<Dma>,
    io: Option<Queue>,
    data: Option<Dma>,
    disk: Option<Disk>,
}

/// The match rule: the controller's class, subclass, and programming interface.
static MATCHES: &[Match] = &[Match::by_class(
    NVME_CLASS,
    NVME_SUBCLASS,
    Some(NVME_PROG_IF),
)];

/// The driver's dispatch table. Probe accepts any device the match rule selects.
const DISPATCH: Dispatch = Dispatch {
    probe: accept,
    start: start_device,
    stop: stop_device,
};

/// Probe for the controller, register the driver with the framework, and bind and start it. The
/// bring-up's output is replayed on the console. Failure is reported and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let mut found = false;
    for info in inventory.as_slice().iter().filter(|info| {
        info.class == NVME_CLASS && info.subclass == NVME_SUBCLASS && info.prog_if == NVME_PROG_IF
    }) {
        found = true;
        if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
            let _ = writeln!(
                console,
                "nvme: PCI {:02x}:{:02x}.{} already registered, duplicate attach refused",
                info.bdf.bus, info.bdf.device, info.bdf.function
            );
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
            name: "nvme",
            matches: MATCHES,
            ops: DISPATCH,
        };
        let attached = driver_start::attach_prepared(driver, id, |dev| {
            // Recheck under framework exclusion: the outer enumeration check is only a fast path.
            if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
                return Err(nolx_df_core::Error::Busy);
            }
            STATE[dev].lock().bdf = Some(info.bdf);
            blockdev::bind_storage(dev, blockdev::NVME).map_err(|_| nolx_df_core::Error::Full)?;
            #[cfg(feature = "hal-ahci-nvme-recovery-test")]
            recovery::claim(dev);
            Ok(())
        });
        match attached {
            Ok(dev) => {
                let state = STATE[dev].lock();
                state.log.replay(console);
                let _ = writeln!(console, "nvme: bound and started through nolx-df");
                let _ = writeln!(
                    console,
                    "nvme: instance {dev}, block {}, vector {:#x}, PCI {:02x}:{:02x}.{}",
                    blockdev::device(dev).unwrap(),
                    completion_vector(dev),
                    info.bdf.bus,
                    info.bdf.device,
                    info.bdf.function
                );
            }
            Err(error) => {
                let _ = writeln!(console, "nvme: framework refused the device: {error:?}");
                // Rejected starts retain diagnostic state and pinned resources. Identify by BDF.
                for state in &STATE {
                    let state = state.lock();
                    if state.bdf == Some(info.bdf) {
                        state.log.replay(console);
                        if let Some(Err(failure)) = &state.outcome {
                            let _ = writeln!(console, "nvme: not brought up: {failure:?}");
                        }
                    }
                }
            }
        }
    }
    if !found {
        let _ = writeln!(console, "nvme: not brought up: {:?}", Failure::NoController);
    }
}

fn accept(_: &DeviceId) -> bool {
    true
}

/// Start the selected controller; BDF, diagnostics and transport ownership stay per handle.
fn start_device(dev: usize) -> bool {
    if !removable(dev) {
        return false;
    }
    let mut log = Log::new();
    let result = bring_up(dev, &mut log);
    let started = result.is_ok();
    if !started {
        // A failed start leaves the transport unusable: no flush and no buffer reuse may
        // follow until a checked stop has proved the controller disabled.
        LIVE[dev].store(false, Ordering::Release);
    }
    if !started && stop_device(dev) {
        // SAFETY: the checked stop proved the controller disabled, PCI DMA and message
        // delivery disabled, the software queue detached and every owner released.
        unsafe {
            mmio::release_owned(dev);
        }
    }
    let mut state = STATE[dev].lock();
    state.log = log;
    state.outcome = Some(result);
    started
}

/// Complete and partial owners survive every refusal. A failed flush quarantines the
/// controller; explicit recovery stops it without retrying potentially unsafe I/O.
fn stop_device(dev: usize) -> bool {
    let flushed = if LIVE[dev].load(Ordering::Acquire) {
        let disk = STATE[dev].lock().disk.take();
        if let Some(mut disk) = disk {
            #[cfg(feature = "hal-ahci-nvme-recovery-test")]
            let refused = recovery::fail(dev, 4);
            #[cfg(not(feature = "hal-ahci-nvme-recovery-test"))]
            let refused = false;
            let result = !refused && nolxfs::sector::SectorDevice::flush(&mut disk).is_ok();
            STATE[dev].lock().disk = Some(disk);
            result
        } else {
            false
        }
    } else {
        true
    };
    LIVE[dev].store(false, Ordering::Release);
    let (bdf, regs) = {
        let state = STATE[dev].lock();
        (state.bdf, state.regs)
    };
    let Some(bdf) = bdf else {
        return false;
    };
    // Block the legacy interrupt line and stop message delivery before the controller is
    // disabled, so no completion can arrive while the teardown runs.
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    ConfigAccess.write_u32(bdf, 4, command | (1 << 10));
    if ConfigAccess.read_u32(bdf, 4) & (1 << 10) == 0
        || !super::pci_exclusion::disable_messages(bdf)
        || !flushed
    {
        return false;
    }
    #[cfg(feature = "hal-ahci-nvme-recovery-test")]
    if recovery::refuse_stop(dev) {
        return false;
    }
    let stopped = (|| -> Result<nvme::StopState, Failure> {
        // Disable the controller: it processes no command and reaches no queue.
        if let Some(regs) = regs {
            regs.write32(nvme::REG_CC, 0)?;
            poll(
                || Ok(!nvme::csts_ready(regs.read32(nvme::REG_CSTS)?)),
                "controller disable",
            )?;
        } else if !empty_dma(dev) {
            // Nothing was ever mapped, so nothing can be proven: an owner would stay pinned.
            return Err(Failure::Memory);
        }
        // Bus mastering off, with the readback that proves it.
        if !super::pci_exclusion::pci_access(bdf, false) {
            return Err(Failure::NotExcluded);
        }
        let readback = match regs {
            Some(regs) => nvme::StopState {
                control: regs.read32(nvme::REG_CC)?,
                status: regs.read32(nvme::REG_CSTS)?,
            },
            None => nvme::StopState {
                control: 0,
                status: 0,
            },
        };
        if !nvme::stop_proven(
            readback,
            ConfigAccess.read_u32(bdf, 4) as u16,
            super::pci_exclusion::messages_disabled(bdf),
        ) {
            return Err(Failure::NotExcluded);
        }
        #[cfg(feature = "hal-ahci-nvme-recovery-test")]
        if recovery::withhold_pci_proof(dev) {
            return Err(Failure::NotExcluded);
        }
        // The software queue is detached only after the hardware is proven stopped.
        if let Some(device) = blockdev::device(dev) {
            match blockdev::detach(device) {
                Ok(()) | Err(QueueError::UnknownDevice) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(readback)
    })();
    let Ok(readback) = stopped else {
        return false;
    };
    STOPPED_CONTROL[dev].store(readback.control, Ordering::Release);
    let mut state = STATE[dev].lock();
    let admin = state.admin.take();
    let identify = state.identify.take();
    let io = state.io.take();
    let data = state.data.take();
    let disk = state.disk.take();
    // SAFETY: the controller read disabled and not ready, bus mastering and message delivery
    // read disabled, the software queue is detached, and the callback checkout excludes
    // concurrent users. Every unique owner is released only after the final proof.
    unsafe {
        if let Some(admin) = admin {
            admin.sq.release();
            admin.cq.release();
        }
        if let Some(identify) = identify {
            identify.release();
        }
        if let Some(io) = io {
            io.sq.release();
            io.cq.release();
        }
        if let Some(data) = data {
            data.release();
        }
        if let Some(disk) = disk {
            disk.admin.sq.release();
            disk.admin.cq.release();
            disk.identify.release();
            disk.io.sq.release();
            disk.io.cq.release();
            disk.data.release();
        }
    }
    state.regs = None;
    true
}

/// Whether the handle still owns a DMA page.
fn empty_dma(dev: usize) -> bool {
    let state = STATE[dev].lock();
    state.disk.is_none()
        && state.admin.is_none()
        && state.identify.is_none()
        && state.io.is_none()
        && state.data.is_none()
}

/// Whether the handle owns nothing: no window, no page, and no live transport.
pub(crate) fn removable(dev: usize) -> bool {
    let unmapped = STATE[dev].lock().regs.is_none();
    unmapped && !LIVE[dev].load(Ordering::Acquire) && empty_dma(dev)
}

pub(crate) fn with_disk<T>(
    dev: usize,
    op: impl FnOnce(&mut dyn nolxfs::sector::SectorDevice) -> Result<T, nolxfs::DevError>,
) -> Result<T, nolxfs::DevError> {
    if !LIVE[dev].load(Ordering::Acquire) {
        return Err(nolxfs::DevError::Io(19));
    }
    let mut disk = STATE[dev]
        .lock()
        .disk
        .take()
        .ok_or(nolxfs::DevError::Io(19))?;
    let result = op(&mut disk);
    STATE[dev].lock().disk = Some(disk);
    result
}

/// One submission and completion queue pair, with the producer and consumer positions the driver
/// keeps and the doorbells the controller watches.
struct Queue {
    owner: usize,
    /// The queue's identifier: zero for the admin queue, one for the I/O queue.
    id: u16,
    /// The controller's doorbell stride, which places this queue's doorbells.
    stride: u32,
    sq: Dma,
    cq: Dma,
    entries: u16,
    sq_tail: u16,
    cq_head: u16,
    /// The phase tag a new completion carries. It flips each time the head wraps.
    phase: bool,
}

impl Queue {
    fn new(owner: usize, id: u16, sq: Dma, cq: Dma, stride: u32) -> Self {
        Self {
            owner,
            id,
            stride,
            sq,
            cq,
            entries: QUEUE_ENTRIES,
            sq_tail: 0,
            cq_head: 0,
            phase: true,
        }
    }
    /// The register the submission queue's tail is written to.
    fn sq_doorbell(&self) -> u64 {
        nvme::sq_doorbell(self.id, self.stride)
    }
    /// The register the completion queue's head is written to.
    fn cq_doorbell(&self) -> u64 {
        nvme::cq_doorbell(self.id, self.stride)
    }
}

/// Bring the controller up, keeping every owner the bring-up took with the handle: a failed
/// start must not free a page a live controller could still reach.
fn bring_up(dev: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
    let mut owners = Owners::default();
    let result = build(dev, console, &mut owners);
    let mut state = STATE[dev].lock();
    state.regs = owners.regs;
    state.admin = owners.admin;
    state.identify = owners.identify;
    state.io = owners.io;
    state.data = owners.data;
    state.disk = owners.disk;
    drop(state);
    result
}

fn build(dev: usize, console: &mut dyn HalConsole, owners: &mut Owners) -> Result<(), Failure> {
    let bdf = STATE[dev].lock().bdf.ok_or(Failure::NoController)?;
    let vector = completion_vector(dev);
    COMPLETION_INTERRUPTS[dev].store(0, Ordering::Release);
    IO_SUBMITTED[dev].store(0, Ordering::Release);
    // Memory decoding on, bus mastering off and no message delivery: the controller may
    // not reach memory until its queues are ready.
    if !super::pci_exclusion::pci_access(bdf, false) || !super::pci_exclusion::disable_messages(bdf)
    {
        return Err(Failure::NoController);
    }
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base =
        nolx_pci_core::assigned_memory_base(bars.slots[BAR0_SLOT]).ok_or(Failure::BarNotMemory)?;
    let regs = mmio::map_owned(dev, base, REGISTERS_BYTES)?;
    owners.regs = Some(regs);
    let cap = read64(&regs, nvme::REG_CAP)?;
    let stride = nvme::cap_doorbell_stride(cap);
    if stride > STRIDE_MAX {
        return Err(Failure::Stride(stride));
    }

    // Disable the controller, then configure the admin queue before enabling it again.
    regs.write32(nvme::REG_CC, 0)?;
    poll(
        || Ok(!nvme::csts_ready(regs.read32(nvme::REG_CSTS)?)),
        "controller disable",
    )?;
    let aqa = nvme::aqa(QUEUE_ENTRIES, QUEUE_ENTRIES).ok_or(Failure::Encoding)?;
    // The queue pages are taken before the controller is pointed at them, so a failure
    // between here and the disk leaves them with the handle.
    let admin_sq = Dma::new().ok_or(Failure::Memory)?;
    let admin_cq = Dma::new().ok_or(Failure::Memory)?;
    owners.admin = Some(Queue::new(dev, 0, admin_sq, admin_cq, stride));
    #[cfg(feature = "hal-ahci-nvme-recovery-test")]
    if recovery::fail(dev, 1) {
        // The admin queue's pages are already owned and the controller names no queue yet:
        // the rollback must reclaim them instead of pinning them.
        return Err(Failure::Memory);
    }
    regs.write32(nvme::REG_AQA, aqa)?;
    write64(
        &regs,
        nvme::REG_ASQ,
        owners.admin.as_ref().ok_or(Failure::Memory)?.sq.address(0),
    )?;
    write64(
        &regs,
        nvme::REG_ACQ,
        owners.admin.as_ref().ok_or(Failure::Memory)?.cq.address(0),
    )?;
    regs.write32(nvme::REG_CC, nvme::cc_enable())?;
    poll(
        || Ok(nvme::csts_ready(regs.read32(nvme::REG_CSTS)?)),
        "controller ready",
    )?;
    // The controller may reach memory only now: the admin queue is programmed, the pages
    // are owned and the driver is ready to take its first command.
    if !super::pci_exclusion::pci_access(bdf, true) {
        return Err(Failure::NoController);
    }

    // Identify namespace 1. The structure is 4 KiB; the fields the driver reads are in its first
    // 192 bytes, so only that much is copied out.
    let mut cid = 0u16;
    owners.identify = Some(Dma::new().ok_or(Failure::Memory)?);
    let command = nvme::identify(
        next_cid(&mut cid),
        NSID,
        nvme::CNS_NAMESPACE,
        owners.identify.as_ref().ok_or(Failure::Memory)?.address(0),
    );
    admin_execute(owners, &regs, &command)?;
    let mut fields = [0u8; 192];
    owners
        .identify
        .as_ref()
        .ok_or(Failure::Memory)?
        .read(0, &mut fields);
    let blocks = nvme::namespace_blocks(&fields);
    if blocks == 0 || nvme::namespace_block_bytes(&fields) != Some(SECTOR_BYTES as u32) {
        return Err(Failure::Namespace);
    }
    let _ = writeln!(
        console,
        "nvme: {:02x}:{:02x}.{} bar0 {base:#x}, namespace {NSID}: {blocks} sectors of {SECTOR_BYTES} bytes",
        bdf.bus, bdf.device, bdf.function,
    );

    // One I/O queue pair: the completion queue first, since the submission queue names it.
    let io_cq = Dma::new().ok_or(Failure::Memory)?;
    let io_sq = Dma::new().ok_or(Failure::Memory)?;
    owners.io = Some(Queue::new(dev, IO_QUEUE, io_sq, io_cq, stride));
    msix::enable(dev, bdf, &bars, MSIX_ENTRY, vector)?;
    let command = nvme::create_io_cq_msix(
        next_cid(&mut cid),
        IO_QUEUE,
        QUEUE_ENTRIES,
        owners.io.as_ref().ok_or(Failure::Memory)?.cq.address(0),
        MSIX_ENTRY,
    )
    .ok_or(Failure::Encoding)?;
    admin_execute(owners, &regs, &command)?;
    let command = nvme::create_io_sq(
        next_cid(&mut cid),
        IO_QUEUE,
        QUEUE_ENTRIES,
        IO_QUEUE,
        owners.io.as_ref().ok_or(Failure::Memory)?.sq.address(0),
    )
    .ok_or(Failure::Encoding)?;
    admin_execute(owners, &regs, &command)?;

    // The data buffer, the last page the controller will be given.
    owners.data = Some(Dma::new().ok_or(Failure::Memory)?);
    #[cfg(feature = "hal-ahci-nvme-recovery-test")]
    if recovery::fail(dev, 2) {
        // The controller runs and names its queues: a partial start.
        return Err(Failure::Timeout("enabled controller"));
    }
    // The controller's completions are counted from now on, so the transport may be used.
    // A failed command clears this again and quarantines the controller.
    LIVE[dev].store(true, Ordering::Release);

    // Sector 0 first: its first bytes are the pattern CI wrote into the image.
    let data_address = owners.data.as_ref().ok_or(Failure::Memory)?.address(0);
    owners
        .data
        .as_ref()
        .ok_or(Failure::Memory)?
        .write(0, &[0u8; SECTOR_BYTES]);
    let command = nvme::read_write(false, next_cid(&mut cid), NSID, 0, 1, data_address)
        .ok_or(Failure::Encoding)?;
    let read = Request {
        direction: Direction::Read,
        lba: 0,
        sectors: 1,
    };
    io_execute(owners, &regs, &command, read)?;
    let mut sector = [0u8; SECTOR_BYTES];
    owners
        .data
        .as_ref()
        .ok_or(Failure::Memory)?
        .read(0, &mut sector);
    if sector[..SECTOR_PATTERN.len()] != *SECTOR_PATTERN {
        return Err(Failure::Pattern);
    }
    let _ = writeln!(
        console,
        "nvme: sector 0 read, status ok, first {} bytes match the disk image",
        SECTOR_PATTERN.len(),
    );

    // The round trip: write a pattern, clear the buffer, read the sectors back, and compare.
    let mut written = [0u8; ROUND_TRIP_BYTES];
    fill_pattern(&mut written);
    owners
        .data
        .as_ref()
        .ok_or(Failure::Memory)?
        .write(0, &written);
    let command = nvme::read_write(
        true,
        next_cid(&mut cid),
        NSID,
        ROUND_TRIP_SECTOR,
        ROUND_TRIP_SECTORS,
        data_address,
    )
    .ok_or(Failure::Encoding)?;
    let write = Request {
        direction: Direction::Write,
        lba: ROUND_TRIP_SECTOR,
        sectors: u32::from(ROUND_TRIP_SECTORS),
    };
    io_execute(owners, &regs, &command, write)?;
    owners
        .data
        .as_ref()
        .ok_or(Failure::Memory)?
        .write(0, &[0u8; ROUND_TRIP_BYTES]);
    let command = nvme::read_write(
        false,
        next_cid(&mut cid),
        NSID,
        ROUND_TRIP_SECTOR,
        ROUND_TRIP_SECTORS,
        data_address,
    )
    .ok_or(Failure::Encoding)?;
    let read_back = Request {
        direction: Direction::Read,
        lba: ROUND_TRIP_SECTOR,
        sectors: u32::from(ROUND_TRIP_SECTORS),
    };
    io_execute(owners, &regs, &command, read_back)?;
    let mut back = [0u8; ROUND_TRIP_BYTES];
    owners
        .data
        .as_ref()
        .ok_or(Failure::Memory)?
        .read(0, &mut back);
    if back != written {
        return Err(Failure::RoundTrip);
    }
    let checksum = crc32(&back);
    let _ = writeln!(
        console,
        "nvme: wrote {ROUND_TRIP_SECTORS} sectors at {ROUND_TRIP_SECTOR}, read back, crc32 {checksum:#010x} matches the written data"
    );

    // Every I/O command's interrupt was taken in turn, so the two counts are equal.
    let expected = IO_SUBMITTED[dev].load(Ordering::Relaxed);
    let delivered = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    let _ = writeln!(
        console,
        "nvme: msi-x completion interrupts: {delivered} of {expected} I/O commands, vector {vector:#x}"
    );
    if delivered != expected {
        return Err(Failure::Interrupts {
            delivered,
            expected,
        });
    }
    let disk = Disk {
        owner: dev,
        capacity: blocks,
        admin: owners.admin.take().ok_or(Failure::Memory)?,
        identify: owners.identify.take().ok_or(Failure::Memory)?,
        regs,
        io: owners.io.take().ok_or(Failure::Memory)?,
        data: owners.data.take().ok_or(Failure::Memory)?,
        cid,
    };
    owners.disk = Some(disk);
    let disk = owners.disk.as_mut().ok_or(Failure::Memory)?;
    let (first, sectors) =
        super::storage_volume::discover(disk, blocks).map_err(Failure::Filesystem)?;
    super::storage_volume::run(disk, first, sectors, console, "nvme")
        .map_err(Failure::Filesystem)?;
    #[cfg(feature = "hal-ahci-nvme-recovery-test")]
    if recovery::fail(dev, 3) {
        return Err(Failure::Timeout("validated disk"));
    }
    Ok(())
}

/// Run one admin command on the admin queue the owners hold.
fn admin_execute(owners: &mut Owners, regs: &Region, command: &Sqe) -> Result<(), Failure> {
    let admin = owners.admin.as_mut().ok_or(Failure::Memory)?;
    execute(regs, admin, command)
}

/// Run one I/O command on the I/O queue the owners hold.
fn io_execute(
    owners: &mut Owners,
    regs: &Region,
    command: &Sqe,
    request: Request,
) -> Result<(), Failure> {
    let io = owners.io.as_mut().ok_or(Failure::Memory)?;
    execute_io(regs, io, command, request)
}

/// Count one completion interrupt. Called from the interrupt handler on [`COMPLETION_VECTOR`].
fn completion_vector(dev: usize) -> u8 {
    if blockdev::device(dev) == Some(blockdev::NVME) {
        COMPLETION_VECTOR
    } else {
        0xa0 + dev as u8
    }
}
/// Whether the vector belongs to this driver's bounded per-controller reservation.
pub fn handles_vector(vector: u8) -> bool {
    vector == COMPLETION_VECTOR || (0xa0..0xb0).contains(&vector)
}
/// Attribute a vector to exactly one controller before acknowledging its completion.
pub fn on_completion_interrupt(vector: u8) {
    let dev = if vector == COMPLETION_VECTOR {
        blockdev::primary_handle(blockdev::NVME)
    } else if (0xa0..0xb0).contains(&vector) {
        Some(usize::from(vector - 0xa0))
    } else {
        None
    };
    let Some(dev) = dev.filter(|&dev| dev < driver_start::DEVICES) else {
        return;
    };
    // Only a live controller that owns this vector may be counted. A stopped or quarantined
    // one has no window to read and no command to count.
    if !LIVE[dev].load(Ordering::Acquire) || vector != completion_vector(dev) {
        return;
    }
    COMPLETION_INTERRUPTS[dev].fetch_add(1, Ordering::Release);
}

/// Submit `command` on `queue`, ring its doorbell, and wait for its completion. The command's
/// identifier must come back, and its status field must be zero.
fn execute(regs: &Region, queue: &mut Queue, command: &Sqe) -> Result<(), Failure> {
    let cid = u16::from_le_bytes([command[2], command[3]]);
    let slot = u64::from(queue.sq_tail) * nvme::SQE_BYTES as u64;
    queue.sq.write(slot, command);
    // The controller reads the entry over the bus, so it must be written before the doorbell.
    fence(Ordering::SeqCst);
    queue.sq_tail = (queue.sq_tail + 1) % queue.entries;
    regs.write32(queue.sq_doorbell(), u32::from(queue.sq_tail))?;
    #[cfg(feature = "hal-ahci-nvme-recovery-test")]
    if queue.id == IO_QUEUE && recovery::fail(queue.owner, 5) {
        // The command is published; the controller may still own the buffer.
        return Err(Failure::Timeout("published command"));
    }

    let entry_at = u64::from(queue.cq_head) * nvme::CQE_BYTES as u64;
    let mut done = [0u8; nvme::CQE_BYTES];
    poll(
        || {
            let Some(bytes) = nvme::read_completion(queue.phase, |offset, out| {
                queue.cq.read(entry_at + offset as u64, out);
            }) else {
                return Ok(false);
            };
            done = bytes;
            Ok(true)
        },
        "completion",
    )?;
    fence(Ordering::SeqCst);

    queue.cq_head = (queue.cq_head + 1) % queue.entries;
    if queue.cq_head == 0 {
        queue.phase = !queue.phase;
    }
    regs.write32(queue.cq_doorbell(), u32::from(queue.cq_head))?;

    let dw3 = u32::from_le_bytes([done[12], done[13], done[14], done[15]]);
    if nvme::completion_cid(dw3) != cid {
        return Err(Failure::Identifier);
    }
    match nvme::completion_status(dw3) {
        0 => Ok(()),
        status => Err(Failure::Status(status)),
    }
}

/// Run one I/O command on the I/O queue, through the block queue.
fn execute_io(
    regs: &Region,
    queue: &mut Queue,
    command: &Sqe,
    request: Request,
) -> Result<(), Failure> {
    let submitted = IO_SUBMITTED[queue.owner].fetch_add(1, Ordering::Relaxed) + 1;
    let result = blockdev::run(
        blockdev::device(queue.owner).ok_or(QueueError::UnknownDevice)?,
        request,
        || {
            execute(regs, queue, command)?;
            // Take this command's interrupt before the next command is submitted. The local APIC
            // keeps one pending bit per vector, so interrupts that queue up unserviced merge.
            let delivered =
                msix::wait_for_interrupts(&COMPLETION_INTERRUPTS[queue.owner], submitted);
            if delivered < submitted {
                return Err(Failure::Interrupts {
                    delivered,
                    expected: submitted,
                });
            }
            Ok(())
        },
    );
    if result.is_err() {
        // A published command may still own the queue's buffer. Do not submit a flush or
        // reuse the buffer after a failure: only a checked stop can re-open this transport.
        LIVE[queue.owner].store(false, Ordering::Release);
    }
    result
}

/// The next command identifier. The driver has one command outstanding, so identifiers only need
/// to differ from the last one.
fn next_cid(counter: &mut u16) -> u16 {
    let cid = *counter;
    *counter = counter.wrapping_add(1);
    cid
}

fn read64(regs: &Region, offset: u64) -> Result<u64, Failure> {
    let low = u64::from(regs.read32(offset)?);
    let high = u64::from(regs.read32(offset + 4)?);
    Ok(low | (high << 32))
}

fn write64(regs: &Region, offset: u64, value: u64) -> Result<(), Failure> {
    regs.write32(offset, value as u32)?;
    regs.write32(offset + 4, (value >> 32) as u32)?;
    Ok(())
}

/// Poll against a two-second HPET deadline, with a finite iteration ceiling and clockless fallback.
/// Completion DMA can be asynchronous to the vCPU; a CPU-speed-only loop is not a timeout.
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

impl From<QueueError> for Failure {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

impl From<msix::Error> for Failure {
    fn from(error: msix::Error) -> Self {
        Self::Msix(error)
    }
}

/// The namespace's queue and page-aligned PRP buffer, shared by raw and filesystem I/O.
struct Disk {
    owner: usize,
    capacity: u64,
    admin: Queue,
    identify: Dma,
    regs: Region,
    io: Queue,
    data: Dma,
    cid: u16,
}
/// Preserve useful controller failure classes across the filesystem's device-error boundary.
fn filesystem_io_error(error: Failure) -> nolxfs::DevError {
    let status = match error {
        Failure::Timeout(_) => 110,
        Failure::Interrupts { .. } => 4,
        Failure::Identifier => 71,
        Failure::Status(status) => 0x10000 | u32::from(status),
        Failure::Queue(_) => 16,
        _ => 5,
    };
    nolxfs::DevError::Io(status)
}
impl Disk {
    fn transfer(&mut self, write: bool, lba: u64, len: usize) -> Result<(), nolxfs::DevError> {
        let sectors = (len / 512) as u16;
        let command = nvme::read_write(
            write,
            next_cid(&mut self.cid),
            NSID,
            lba,
            sectors,
            self.data.address(0),
        )
        .ok_or(nolxfs::DevError::Io(5))?;
        let request = Request {
            direction: if write {
                Direction::Write
            } else {
                Direction::Read
            },
            lba,
            sectors: u32::from(sectors),
        };
        execute_io(&self.regs, &mut self.io, &command, request).map_err(filesystem_io_error)
    }
}
impl super::storage_volume::SectorDisk for Disk {
    fn sector_count(&self) -> u64 {
        self.capacity
    }
    fn flush(&mut self) -> Result<(), nolxfs::DevError> {
        let command = nvme::flush(next_cid(&mut self.cid), NSID);
        let expected = IO_SUBMITTED[self.owner].fetch_add(1, Ordering::Relaxed) + 1;
        if let Err(error) = execute(&self.regs, &mut self.io, &command) {
            // A published command may still own the queue's buffer: quarantine the transport.
            LIVE[self.owner].store(false, Ordering::Release);
            return Err(filesystem_io_error(error));
        }
        if msix::wait_for_interrupts(&COMPLETION_INTERRUPTS[self.owner], expected) < expected {
            LIVE[self.owner].store(false, Ordering::Release);
            return Err(nolxfs::DevError::Io(4));
        }
        Ok(())
    }
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, out.len())?;
        for (i, chunk) in out.chunks_mut(4096).enumerate() {
            self.transfer(false, first + (i * 8) as u64, chunk.len())?;
            self.data.read(0, chunk);
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, bytes: &[u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, bytes.len())?;
        for (i, chunk) in bytes.chunks(4096).enumerate() {
            self.data.write(0, chunk);
            self.transfer(true, first + (i * 8) as u64, chunk.len())?;
        }
        Ok(())
    }
}

/// Exercise an actual false predicate through the same deadline as controller completions.
#[cfg(feature = "hal-driver-lifecycle-test")]
pub(crate) fn deadline_selftest(console: &mut dyn HalConsole) {
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    assert!(poll(|| Ok(true), "immediate-ready selftest").is_ok());
    assert!(matches!(
        poll(|| Ok(false), "deadline selftest"),
        Err(Failure::Timeout("deadline selftest"))
    ));
    if let Some((start, hz)) = clock {
        let end = super::hpet::counter().expect("HPET disappeared during deadline test");
        assert!(
            super::hpet::elapsed_ticks(start, end) >= hz.saturating_mul(2),
            "poll expired on CPU iteration speed before its deadline"
        );
        let _ = writeln!(console, "nvme: poll deadline selftest: ready predicate accepted, false predicate timed out after two HPET seconds");
    }
}

/// Exercise checked partial-start/failed-stop recovery against two real controllers.
#[cfg(feature = "hal-ahci-nvme-recovery-test")]
pub fn recovery_selftest(console: &mut dyn HalConsole) {
    recovery::run(console);
}
