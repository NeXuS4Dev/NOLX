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

use super::blockdev;
use super::driver_start::{self, Log};
use super::msix;
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolx_df_core::{DeviceId, Dispatch, Driver, Match, Registry};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_nvme_core::{self as nvme, Sqe};
use nolx_pci_core::{Bar, Bdf, ConfigSpace};
use nolx_virtio_core::crc32;

use super::dma::Dma;
use super::pci::ConfigAccess;
use super::virtio_blk::fill_pattern;
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, AtomicUsize, Ordering};

/// NVMe class code: mass storage, non-volatile memory, NVM Express interface.
const NVME_CLASS: u8 = 0x01;
const NVME_SUBCLASS: u8 = 0x08;
const NVME_PROG_IF: u8 = 0x02;
/// The base address register that holds the controller registers. NVMe puts them in BAR 0.
const BAR0_SLOT: usize = 0;
/// PCI command register and its memory space and bus master bits. A device may DMA only with the
/// bus master bit set; memory space lets the controller's registers answer.
const PCI_COMMAND: u8 = 0x04;
const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
const COMMAND_BUS_MASTER: u32 = 1 << 2;
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
static COMPLETION_INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
/// I/O commands submitted through [`execute_io`]. Each one should produce one interrupt.
static IO_SUBMITTED: AtomicUsize = AtomicUsize::new(0);

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
}

static STATE: IrqMutex<State> = IrqMutex::new(State {
    log: Log::new(),
    outcome: None,
});

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
    let Some((_, id)) = find_controller() else {
        let _ = writeln!(console, "nvme: not brought up: {:?}", Failure::NoController);
        return;
    };
    let mut registry: Registry<1, 1> = Registry::new();
    let driver = Driver {
        name: "nvme",
        matches: MATCHES,
        ops: DISPATCH,
    };
    if let Err(error) = driver_start::attach(&mut registry, driver, id) {
        let _ = writeln!(console, "nvme: framework refused the device: {error:?}");
    }
    let state = STATE.lock();
    state.log.replay(console);
    match &state.outcome {
        Some(Ok(())) => {
            let _ = writeln!(console, "nvme: bound and started through nolx-df");
        }
        Some(Err(failure)) => {
            let _ = writeln!(console, "nvme: not brought up: {failure:?}");
        }
        None => {
            let _ = writeln!(console, "nvme: not started");
        }
    }
}

fn accept(_: &DeviceId) -> bool {
    true
}

/// The framework's start operation: the whole bring-up. Its output and result go to [`STATE`].
fn start_device(_dev: usize) -> bool {
    let mut log = Log::new();
    let result = bring_up(&mut log);
    let started = result.is_ok();
    let mut state = STATE.lock();
    state.log = log;
    state.outcome = Some(result);
    started
}

/// Nothing to undo: the queue stays registered for the rest of boot.
fn stop_device(_dev: usize) -> bool {
    true
}

/// One submission and completion queue pair, with the producer and consumer positions the driver
/// keeps and the doorbells the controller watches.
struct Queue {
    sq: Dma,
    cq: Dma,
    entries: u16,
    sq_tail: u16,
    cq_head: u16,
    /// The phase tag a new completion carries. It flips each time the head wraps.
    phase: bool,
    sq_doorbell: u64,
    cq_doorbell: u64,
}

impl Queue {
    fn new(id: u16, sq: Dma, cq: Dma, stride: u32) -> Self {
        Self {
            sq,
            cq,
            entries: QUEUE_ENTRIES,
            sq_tail: 0,
            cq_head: 0,
            phase: true,
            sq_doorbell: nvme::sq_doorbell(id, stride),
            cq_doorbell: nvme::cq_doorbell(id, stride),
        }
    }
}

fn bring_up(console: &mut dyn HalConsole) -> Result<(), Failure> {
    let (bdf, _) = find_controller().ok_or(Failure::NoController)?;
    enable_device(bdf);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let base = match bars.slots[BAR0_SLOT] {
        Bar::Memory32 { base, .. } => u64::from(base),
        Bar::Memory64 { base, .. } => base,
        _ => return Err(Failure::BarNotMemory),
    };
    let regs = mmio::map(base, REGISTERS_BYTES)?;
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
    let admin_sq = Dma::new().ok_or(Failure::Memory)?;
    let admin_cq = Dma::new().ok_or(Failure::Memory)?;
    let mut admin = Queue::new(0, admin_sq, admin_cq, stride);
    regs.write32(nvme::REG_AQA, aqa)?;
    write64(&regs, nvme::REG_ASQ, admin.sq.address(0))?;
    write64(&regs, nvme::REG_ACQ, admin.cq.address(0))?;
    regs.write32(nvme::REG_CC, nvme::cc_enable())?;
    poll(
        || Ok(nvme::csts_ready(regs.read32(nvme::REG_CSTS)?)),
        "controller ready",
    )?;

    // Identify namespace 1. The structure is 4 KiB; the fields the driver reads are in its first
    // 192 bytes, so only that much is copied out.
    let mut cid = 0u16;
    let identify = Dma::new().ok_or(Failure::Memory)?;
    let command = nvme::identify(
        next_cid(&mut cid),
        NSID,
        nvme::CNS_NAMESPACE,
        identify.address(0),
    );
    execute(&regs, &mut admin, &command)?;
    let mut fields = [0u8; 192];
    identify.read(0, &mut fields);
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
    msix::enable(bdf, &bars, MSIX_ENTRY, COMPLETION_VECTOR)?;
    let command = nvme::create_io_cq_msix(
        next_cid(&mut cid),
        IO_QUEUE,
        QUEUE_ENTRIES,
        io_cq.address(0),
        MSIX_ENTRY,
    )
    .ok_or(Failure::Encoding)?;
    execute(&regs, &mut admin, &command)?;
    let command = nvme::create_io_sq(
        next_cid(&mut cid),
        IO_QUEUE,
        QUEUE_ENTRIES,
        IO_QUEUE,
        io_sq.address(0),
    )
    .ok_or(Failure::Encoding)?;
    execute(&regs, &mut admin, &command)?;
    let mut io = Queue::new(IO_QUEUE, io_sq, io_cq, stride);

    // Sector 0 first: its first bytes are the pattern CI wrote into the image.
    let data = Dma::new().ok_or(Failure::Memory)?;
    data.write(0, &[0u8; SECTOR_BYTES]);
    let command = nvme::read_write(false, next_cid(&mut cid), NSID, 0, 1, data.address(0))
        .ok_or(Failure::Encoding)?;
    let read = Request {
        direction: Direction::Read,
        lba: 0,
        sectors: 1,
    };
    execute_io(&regs, &mut io, &command, read)?;
    let mut sector = [0u8; SECTOR_BYTES];
    data.read(0, &mut sector);
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
    data.write(0, &written);
    let command = nvme::read_write(
        true,
        next_cid(&mut cid),
        NSID,
        ROUND_TRIP_SECTOR,
        ROUND_TRIP_SECTORS,
        data.address(0),
    )
    .ok_or(Failure::Encoding)?;
    let write = Request {
        direction: Direction::Write,
        lba: ROUND_TRIP_SECTOR,
        sectors: u32::from(ROUND_TRIP_SECTORS),
    };
    execute_io(&regs, &mut io, &command, write)?;
    data.write(0, &[0u8; ROUND_TRIP_BYTES]);
    let command = nvme::read_write(
        false,
        next_cid(&mut cid),
        NSID,
        ROUND_TRIP_SECTOR,
        ROUND_TRIP_SECTORS,
        data.address(0),
    )
    .ok_or(Failure::Encoding)?;
    let read_back = Request {
        direction: Direction::Read,
        lba: ROUND_TRIP_SECTOR,
        sectors: u32::from(ROUND_TRIP_SECTORS),
    };
    execute_io(&regs, &mut io, &command, read_back)?;
    let mut back = [0u8; ROUND_TRIP_BYTES];
    data.read(0, &mut back);
    if back != written {
        return Err(Failure::RoundTrip);
    }
    let checksum = crc32(&back);
    let _ = writeln!(
        console,
        "nvme: wrote {ROUND_TRIP_SECTORS} sectors at {ROUND_TRIP_SECTOR}, read back, crc32 {checksum:#010x} matches the written data"
    );

    // Every I/O command's interrupt was taken in turn, so the two counts are equal.
    let expected = IO_SUBMITTED.load(Ordering::Relaxed);
    let delivered = COMPLETION_INTERRUPTS.load(Ordering::Acquire);
    let _ = writeln!(
        console,
        "nvme: msi-x completion interrupts: {delivered} of {expected} I/O commands, vector {COMPLETION_VECTOR:#x}"
    );
    if delivered != expected {
        return Err(Failure::Interrupts {
            delivered,
            expected,
        });
    }
    let mut disk = Disk {
        regs,
        io,
        data,
        cid,
    };
    let (first, sectors) =
        super::storage_volume::discover(&mut disk, blocks).map_err(Failure::Filesystem)?;
    super::storage_volume::run(disk, first, sectors, console, "nvme").map_err(Failure::Filesystem)
}

/// Count one completion interrupt. Called from the interrupt handler on [`COMPLETION_VECTOR`].
pub fn on_completion_interrupt() {
    COMPLETION_INTERRUPTS.fetch_add(1, Ordering::Release);
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
    regs.write32(queue.sq_doorbell, u32::from(queue.sq_tail))?;

    let entry_at = u64::from(queue.cq_head) * nvme::CQE_BYTES as u64;
    let mut done = [0u8; nvme::CQE_BYTES];
    poll(
        || {
            let mut bytes = [0u8; nvme::CQE_BYTES];
            queue.cq.read(entry_at, &mut bytes);
            let dw3 = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
            if nvme::completion_phase(dw3) != queue.phase {
                return Ok(false);
            }
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
    regs.write32(queue.cq_doorbell, u32::from(queue.cq_head))?;

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
    let submitted = IO_SUBMITTED.fetch_add(1, Ordering::Relaxed) + 1;
    blockdev::run(blockdev::NVME, request, || {
        execute(regs, queue, command)?;
        // Take this command's interrupt before the next command is submitted. The local APIC
        // keeps one pending bit per vector, so interrupts that queue up unserviced merge.
        let delivered = msix::wait_for_interrupts(&COMPLETION_INTERRUPTS, submitted);
        if delivered < submitted {
            return Err(Failure::Interrupts {
                delivered,
                expected: submitted,
            });
        }
        Ok(())
    })
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

/// Turn on memory decoding and bus mastering, keeping every other command bit as it was. The
/// status half of the register is not written, since some of its bits clear when written.
fn enable_device(bdf: Bdf) {
    let mut cfg = ConfigAccess;
    let command = cfg.read_u32(bdf, PCI_COMMAND) & 0xFFFF;
    cfg.write_u32(
        bdf,
        PCI_COMMAND,
        command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER,
    );
}

/// The first NVMe controller on the bus, if there is one.
fn find_controller() -> Option<(Bdf, DeviceId)> {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .find(|info| {
            info.class == NVME_CLASS
                && info.subclass == NVME_SUBCLASS
                && info.prog_if == NVME_PROG_IF
        })
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
    regs: Region,
    io: Queue,
    data: Dma,
    cid: u16,
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
        execute_io(&self.regs, &mut self.io, &command, request).map_err(|_| nolxfs::DevError::Io(5))
    }
}
impl super::storage_volume::SectorDisk for Disk {
    fn flush(&mut self) -> Result<(), nolxfs::DevError> {
        let command = nvme::flush(next_cid(&mut self.cid), NSID);
        let expected = IO_SUBMITTED.fetch_add(1, Ordering::Relaxed) + 1;
        execute(&self.regs, &mut self.io, &command).map_err(|_| nolxfs::DevError::Io(5))?;
        if msix::wait_for_interrupts(&COMPLETION_INTERRUPTS, expected) < expected {
            return Err(nolxfs::DevError::Io(5));
        }
        Ok(())
    }
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), nolxfs::DevError> {
        if out.len() % 512 != 0 {
            return Err(nolxfs::DevError::Io(5));
        }
        for (i, chunk) in out.chunks_mut(4096).enumerate() {
            self.transfer(false, first + (i * 8) as u64, chunk.len())?;
            self.data.read(0, chunk);
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, bytes: &[u8]) -> Result<(), nolxfs::DevError> {
        if bytes.len() % 512 != 0 {
            return Err(nolxfs::DevError::Io(5));
        }
        for (i, chunk) in bytes.chunks(4096).enumerate() {
            self.data.write(0, chunk);
            self.transfer(true, first + (i * 8) as u64, chunk.len())?;
        }
        Ok(())
    }
}
