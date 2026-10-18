//! The virtio block driver's probe: find the disk, bring it up through its PCI capabilities,
//! and read sector 0 through a split virtqueue.
//!
//! The rules -- which devices are block devices, the status sequence, feature negotiation, the
//! queue layout, the request format -- live in `nolx-virtio-core`, where they are host-tested.
//! This module supplies what the rules cannot: register access through [`mmio`] windows, queue
//! memory from the frame allocator and reached through the direct map, and the polling.
//!
//! The probe is synchronous: it polls the used ring, then verifies MSI-X completion delivery.
//! With HPET available, polling allows two seconds for host disk I/O instead of relying on a
//! CPU-speed-dependent spin count. Both clocked and fallback waits have iteration ceilings,
//! so a device that never answers is reported as a timeout instead of hanging boot.
//!
//! After the sector-0 read, the probe reads the GUID Partition Table: the primary header in LBA 1
//! and the table it names, checked with the `nolx-block-core` rules, then prints the partitions in
//! use. Then it writes a 2 KiB pattern to sectors 1600 to 1603, in the gap after the last
//! partition, reads it back, and compares CRC-32 checksums. CI computes the same checksum over the
//! image file after QEMU exits, so the write has to have reached the disk and not only the device's
//! cache. Last, it puts a NOLXFS volume on the first partition in use; see [`super::storage_volume`].

use super::blockdev;
use super::driver_start::{self, Log};
use super::msix;
use alloc::vec;
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolx_block_core::{self as gpt, GptError, Header};
use nolxfs::on_disk::FsError;

use super::dma::Dma;
use crate::hal::pci::ConfigAccess;
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "hal-virtio-blk-recovery-test")]
mod recovery;
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{BarSet, Bdf, ConfigSpace};
use nolx_virtio_core::{self as virtio, common, VirtioCap, VirtioError};

/// Fallback polling ceiling without HPET; the clocked path has a 100x hard ceiling.
const POLL_LIMIT: u32 = 1_000_000;
/// Where the request header, status byte, and data buffer sit in the request page.
const REQUEST_HEADER: u64 = 0;
const REQUEST_STATUS: u64 = 16;
const REQUEST_DATA: u64 = 512;
/// The first bytes of sector 0 of the disk image, which CI writes before the boot. The probe
/// checks them so that it proves the bytes came from the disk, not from memory it zeroed.
const SECTOR_PATTERN: &[u8] = b"NOLX-BLK-SECTOR0";
/// The first sector of the round-trip area, and how many sectors it spans. The area is in the
/// usable part of the disk and between the partitions the CI image defines, so the write does not
/// touch the partition table or a partition's data.
const ROUND_TRIP_SECTOR: u64 = 1600;
const ROUND_TRIP_SECTORS: u32 = 4;
/// The most bytes one request carries. The request page has room for more, but a bounded size
/// keeps every transfer the same shape.
const READ_CHUNK_BYTES: usize = 2048;
/// Bytes in the round-trip area. The request page holds it after the header and status.
const ROUND_TRIP_BYTES: usize = ROUND_TRIP_SECTORS as usize * virtio::SECTOR_BYTES as usize;

/// Why the probe stopped.
#[derive(Debug)]
pub enum Failure {
    /// The block queue refused the request, or did not hand it back.
    Queue(QueueError),
    /// MSI-X could not be set up for the device.
    Msix(msix::Error),
    /// The device did not accept the MSI-X vector for the queue.
    NoVector,
    /// Fewer completion interrupts arrived than requests were submitted.
    Interrupts {
        /// Completion interrupts that arrived.
        delivered: usize,
        /// Requests submitted.
        expected: usize,
    },
    /// No virtio block device is on the PCI bus.
    NoDevice,
    /// The device does not expose a capability the driver needs.
    NoCapability(&'static str),
    /// A capability names a BAR that is not a memory BAR.
    BarNotMemory(u8),
    /// The MMIO window refused a mapping.
    Window(mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// The virtio rules refused the device's answer.
    Virtio(VirtioError),
    /// No frame was available for the queue or the request.
    Memory,
    /// The device offers no queue 0.
    NoQueue,
    /// The device's capacity does not fit a byte count.
    Capacity,
    /// A bounded wait ran out, with the step named.
    Timeout(&'static str),
    /// The data read back is not the pattern the disk image holds.
    Pattern,
    /// The round trip read back different bytes from the ones written.
    RoundTrip,
    /// The partition table did not pass the GPT rules.
    Gpt(GptError),
    /// A read was asked for a length that is not a whole number of sectors.
    Unaligned,
    /// The volume layer refused the partition or the file.
    Volume(FsError),
    /// Backend-neutral filesystem verification failed.
    Filesystem(super::storage_volume::Failure),
    /// The file read back is not the bytes that were written.
    FileMismatch,
}

impl From<MmioError> for Failure {
    fn from(error: MmioError) -> Self {
        Self::Access(error)
    }
}

impl From<VirtioError> for Failure {
    fn from(error: VirtioError) -> Self {
        Self::Virtio(error)
    }
}

impl From<mmio::Failure> for Failure {
    fn from(error: mmio::Failure) -> Self {
        Self::Window(error)
    }
}

impl From<GptError> for Failure {
    fn from(error: GptError) -> Self {
        Self::Gpt(error)
    }
}

impl From<FsError> for Failure {
    fn from(error: FsError) -> Self {
        Self::Volume(error)
    }
}

/// Probe for the virtio block device, bring it up, and read sector 0. Failure is reported on
/// the console and does not stop boot: a machine without the disk still boots.
/// What the framework's start operation leaves for the probe: the bring-up's output and result.
struct State {
    log: Log,
    outcome: Option<Result<(), Failure>>,
    disk: Option<Disk>,
    bdf: Option<Bdf>,
    common: Option<Region>,
    pages: [Option<Dma>; 2],
    queue: Option<Queue>,
}

static STATE: [IrqMutex<State>; driver_start::DEVICES] = [const {
    IrqMutex::new(State {
        log: Log::new(),
        outcome: None,
        disk: None,
        bdf: None,
        common: None,
        pages: [const { None }; 2],
        queue: None,
    })
}; driver_start::DEVICES];

static LIVE: [AtomicBool; driver_start::DEVICES] =
    [const { AtomicBool::new(false) }; driver_start::DEVICES];
static PRIMARY: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The match rules: the transitional and the modern virtio block device IDs.
static MATCHES: &[Match] = &[
    Match::by_ids(virtio::VENDOR_VIRTIO, virtio::DEVICE_BLOCK_TRANSITIONAL),
    Match::by_ids(virtio::VENDOR_VIRTIO, virtio::DEVICE_BLOCK_MODERN),
];

/// The driver's dispatch table. Probe accepts any device the match rules select.
const DISPATCH: Dispatch = Dispatch {
    probe: accept,
    start: start_device,
    stop: stop_device,
};

/// Probe for a virtio block device, register the driver with the framework, and bind and start
/// it. The bring-up's output is replayed on the console. Failure is reported and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let mut found = false;
    for info in inventory
        .as_slice()
        .iter()
        .filter(|info| virtio::is_block_device(info.vendor_id, info.device_id))
    {
        found = true;
        if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
            let _ = writeln!(
                console,
                "virtio-blk: PCI {:02x}:{:02x}.{} already registered, duplicate attach refused",
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
            name: "virtio-blk",
            matches: MATCHES,
            ops: DISPATCH,
        };
        let attached = driver_start::attach_prepared(driver, id, |dev| {
            // Recheck under framework exclusion: the outer enumeration check is only a fast path.
            if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
                return Err(nolx_df_core::Error::Busy);
            }
            STATE[dev].lock().bdf = Some(info.bdf);
            blockdev::bind_storage(dev, blockdev::VIRTIO).map_err(|_| nolx_df_core::Error::Full)?;
            if blockdev::device(dev) == Some(blockdev::VIRTIO) {
                PRIMARY.store(dev, Ordering::Release);
            }
            #[cfg(feature = "hal-virtio-blk-recovery-test")]
            recovery::claim(dev);
            Ok(())
        });
        match attached {
            Ok(dev) => {
                let state = STATE[dev].lock();
                state.log.replay(console);
                let _ = writeln!(console, "virtio-blk: bound and started through nolx-df");
                let _ = writeln!(
                    console,
                    "virtio-blk: instance {dev}, block {}, vector {:#x}, PCI {:02x}:{:02x}.{}",
                    blockdev::device(dev).unwrap(),
                    completion_vector(dev),
                    info.bdf.bus,
                    info.bdf.device,
                    info.bdf.function
                );
            }
            Err(error) => {
                let _ = writeln!(
                    console,
                    "virtio-blk: framework refused the device: {error:?}"
                );
                // Rejected starts retain diagnostic state and pinned resources. Identify by BDF.
                for state in &STATE {
                    let state = state.lock();
                    if state.bdf == Some(info.bdf) {
                        state.log.replay(console);
                        if let Some(Err(failure)) = &state.outcome {
                            let _ = writeln!(console, "virtio-blk: not brought up: {failure:?}");
                        }
                    }
                }
            }
        }
    }
    if !found {
        let _ = writeln!(
            console,
            "virtio-blk: not brought up: {:?}",
            Failure::NoDevice
        );
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
        LIVE[dev].store(false, Ordering::Release);
    }
    if !started && stop_device(dev) {
        // SAFETY: rollback reset acknowledged, PCI DMA/messages disabled, source detached
        // and all complete/partial DMA owners released; registry checkout excludes users.
        unsafe {
            mmio::release_owned(dev);
        }
    }
    let mut state = STATE[dev].lock();
    state.log = log;
    state.outcome = Some(result);
    started
}

/// Complete/partial owners survive every refusal. A failed flush quarantines the source;
/// explicit recovery resets it without retrying potentially unsafe or timed-out I/O.
fn stop_device(dev: usize) -> bool {
    let flushed = if LIVE[dev].load(Ordering::Acquire) {
        let disk = STATE[dev].lock().disk.take();
        if let Some(mut disk) = disk {
            #[cfg(feature = "hal-virtio-blk-recovery-test")]
            let refused = recovery::fail(dev, 4);
            #[cfg(not(feature = "hal-virtio-blk-recovery-test"))]
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
    let (bdf, common) = {
        let state = STATE[dev].lock();
        (state.bdf, state.common)
    };
    let Some(bdf) = bdf else {
        return false;
    };
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    ConfigAccess.write_u32(bdf, 4, command | (1 << 10));
    if ConfigAccess.read_u32(bdf, 4) & (1 << 10) == 0
        || !super::pci_exclusion::disable_messages(bdf)
        || !flushed
    {
        return false;
    }
    #[cfg(feature = "hal-virtio-blk-recovery-test")]
    if recovery::refuse_stop(dev) {
        return false;
    }
    let reset = (|| -> Result<(), Failure> {
        if let Some(common) = common {
            common.write8(at(common::DEVICE_STATUS), 0)?;
            poll(
                || Ok(common.read8(at(common::DEVICE_STATUS))? == 0),
                "stop reset",
            )?;
        } else if !empty_dma(dev) {
            return Err(Failure::Memory);
        }
        if !super::pci_exclusion::pci_access(bdf, false) {
            return Err(Failure::NoDevice);
        }
        let status = if let Some(common) = common {
            common.read8(at(common::DEVICE_STATUS))?
        } else {
            0
        };
        if !virtio::transport::stop_proven(
            status,
            ConfigAccess.read_u32(bdf, 4),
            super::pci_exclusion::messages_disabled(bdf),
        ) {
            return Err(Failure::NoDevice);
        }
        #[cfg(feature = "hal-virtio-blk-recovery-test")]
        if recovery::withhold_pci_proof(dev) {
            return Err(Failure::NoDevice);
        }
        if let Some(device) = blockdev::device(dev) {
            match blockdev::detach(device) {
                Ok(()) | Err(QueueError::UnknownDevice) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    if reset.is_err() {
        return false;
    }
    let mut state = STATE[dev].lock();
    let pages = core::mem::replace(&mut state.pages, [const { None }; 2]);
    let queue = state.queue.take();
    let disk = state.disk.take();
    // SAFETY: reset reads zero, PCI bus mastering/INTx/messages read disabled, LIVE
    // is false, the software queue is detached, and callback checkout excludes users.
    // Every unique loose/partial/complete owner is released only after the final proof.
    unsafe {
        for page in pages.into_iter().flatten() {
            page.release();
        }
        if let Some(queue) = queue {
            queue.ring.release();
        }
        if let Some(disk) = disk {
            disk.queue.ring.release();
            disk.request.release();
        }
    }
    state.common = None;
    true
}
fn empty_dma(dev: usize) -> bool {
    let state = STATE[dev].lock();
    state.disk.is_none() && state.queue.is_none() && state.pages.iter().all(Option::is_none)
}
pub(crate) fn removable(dev: usize) -> bool {
    let unmapped = STATE[dev].lock().common.is_none();
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

fn bring_up(dev: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
    let bdf = STATE[dev].lock().bdf.ok_or(Failure::NoDevice)?;
    let device = blockdev::device(dev).ok_or(Failure::NoDevice)?;
    let vector = completion_vector(dev);
    if !super::pci_exclusion::pci_access(bdf, false) || !super::pci_exclusion::disable_messages(bdf)
    {
        return Err(Failure::NoDevice);
    }
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let caps = virtio::discover(&mut ConfigAccess, bdf);
    let common_cap = caps.common.ok_or(Failure::NoCapability("common"))?;
    let notify_cap = caps.notify.ok_or(Failure::NoCapability("notify"))?;
    let device_cap = caps.device.ok_or(Failure::NoCapability("device"))?;
    let common = map_cap(dev, &bars, common_cap)?;
    STATE[dev].lock().common = Some(common);
    let notify = map_cap(dev, &bars, notify_cap)?;
    let config = map_cap(dev, &bars, device_cap)?;

    // Reset, then the status steps in the order the rules give.
    common.write8(at(common::DEVICE_STATUS), 0)?;
    poll(
        || Ok(common.read8(at(common::DEVICE_STATUS))? == 0),
        "reset",
    )?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[0])?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[1])?;
    let offered = read_features(&common)?;
    let accepted = virtio::negotiate_block(offered)?;
    write_driver_features(&common, accepted)?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[2])?;
    virtio::check_features_ok(common.read8(at(common::DEVICE_STATUS))?)?;

    COMPLETION_INTERRUPTS[dev].store(0, Ordering::Release);
    REQUESTS_SUBMITTED[dev].store(0, Ordering::Release);
    msix::enable(dev, bdf, &bars, MSIX_ENTRY, vector)?;
    for index in 0..2 {
        #[cfg(feature = "hal-virtio-blk-recovery-test")]
        if index == 1 && recovery::fail(dev, 1) {
            return Err(Failure::Memory);
        }
        STATE[dev].lock().pages[index] = Some(Dma::new().ok_or(Failure::Memory)?);
    }
    setup_queue(dev, &common, &notify, notify_cap, MSIX_ENTRY)?;
    #[cfg(feature = "hal-virtio-blk-recovery-test")]
    if recovery::fail(dev, 2) {
        return Err(Failure::Timeout("partial queue"));
    }

    let sectors = virtio::capacity_sectors(config.read32(0)?, config.read32(4)?);
    let bytes = virtio::capacity_bytes(sectors).ok_or(Failure::Capacity)?;

    let mut state = STATE[dev].lock();
    let queue = state.queue.take().unwrap();
    let notify_offset =
        virtio::notify_address(&notify_cap, queue.notify_offset) - u64::from(notify_cap.offset);
    state.disk = Some(Disk {
        owner: dev,
        device,
        capacity: sectors,
        queue,
        notify,
        notify_offset,
        request: state.pages[1].take().unwrap(),
        flush_supported: accepted & virtio::FEATURE_BLK_FLUSH != 0,
    });
    drop(state);
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[3])?;
    if !super::pci_exclusion::pci_access(bdf, true) {
        return Err(Failure::NoDevice);
    }
    LIVE[dev].store(true, Ordering::Release);
    let _ = writeln!(
        console,
        "virtio-blk: {:02x}:{:02x}.{} capacity {sectors} sectors ({bytes} bytes), \
         queue size {}, driver ok",
        bdf.bus,
        bdf.device,
        bdf.function,
        STATE[dev].lock().disk.as_ref().unwrap().queue.size
    );
    let mut disk = STATE[dev].lock().disk.take().unwrap();
    let result = validate(&mut disk, sectors, vector, dev, console);
    STATE[dev].lock().disk = Some(disk);
    result
}

fn validate(
    disk: &mut Disk,
    sectors: u64,
    vector: u8,
    dev: usize,
    console: &mut dyn HalConsole,
) -> Result<(), Failure> {
    disk.transfer(virtio::BLK_T_IN, 0, virtio::SECTOR_BYTES as u32)?;
    let first: [u8; 16] = disk.request.read_array(REQUEST_DATA);
    if first[..SECTOR_PATTERN.len()] != *SECTOR_PATTERN {
        return Err(Failure::Pattern);
    }
    let _ = writeln!(
        console,
        "virtio-blk: sector 0 read, status ok, first {} bytes match the disk image",
        SECTOR_PATTERN.len()
    );

    let volume = read_partitions(disk, sectors, console)?;
    round_trip(disk, console)?;
    // Every request's completion interrupt was taken before the next request, so the counts match.
    let expected = REQUESTS_SUBMITTED[dev].load(Ordering::Relaxed);
    let delivered = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    let _ = writeln!(
        console,
        "virtio-blk: msi-x completion interrupts: {delivered} of {expected} requests, vector {vector:#x}"
    );
    if delivered != expected {
        return Err(Failure::Interrupts {
            delivered,
            expected,
        });
    }
    match volume {
        Some((first, last)) => {
            super::storage_volume::run(disk, first, last - first + 1, console, "virtio-blk")
                .map_err(Failure::Filesystem)?;
        }
        None => {
            let _ = writeln!(
                console,
                "virtio-blk: gpt has no partitions in use; no volume"
            );
        }
    }
    #[cfg(feature = "hal-virtio-blk-recovery-test")]
    if recovery::fail(dev, 3) {
        return Err(Failure::Timeout("validated disk"));
    }
    Ok(())
}

/// Read the primary GPT header from LBA 1 and the table it names, check both against the
/// `nolx-block-core` rules, and print each partition in use. The table is read in requests of
/// [`READ_CHUNK_BYTES`]; the header and table are checked before any entry is trusted. Returns the
/// first and last LBA of the first partition in use, if there is one.
fn read_partitions(
    disk: &mut Disk,
    capacity: u64,
    console: &mut dyn HalConsole,
) -> Result<Option<(u64, u64)>, Failure> {
    let mut sector = [0u8; gpt::SECTOR_SIZE];
    disk.read_sectors(1, &mut sector)?;
    let header = Header::parse(&sector, 1, capacity)?;

    let table_sectors = header.table_bytes().div_ceil(gpt::SECTOR_SIZE);
    let mut table = vec![0u8; table_sectors * gpt::SECTOR_SIZE];
    disk.read_sectors(header.entries_lba, &mut table)?;
    gpt::verify_table(&header, &table)?;
    let used = gpt::check_partitions(&header, &table)?;
    let _ = writeln!(
        console,
        "virtio-blk: gpt: header and table verified, {used} partitions in use, usable LBA {}-{}",
        header.first_usable, header.last_usable
    );

    let mut first_used = None;
    for index in 0..header.entry_count {
        let Some(part) = gpt::entry(&header, &table, index) else {
            break;
        };
        if part.is_unused() {
            continue;
        }
        if first_used.is_none() {
            first_used = Some((part.first_lba, part.last_lba));
        }
        let _ = write!(
            console,
            "virtio-blk: gpt partition {index}: LBA {}-{} ({} sectors), name ",
            part.first_lba,
            part.last_lba,
            part.sectors()
        );
        print_name(console, &part.name);
        let _ = writeln!(console);
    }
    Ok(first_used)
}

/// Print a UTF-16 partition name, stopping at the first zero unit. An unpaired surrogate prints as
/// the replacement character.
fn print_name(console: &mut dyn HalConsole, name: &[u16]) {
    let units = name.iter().copied().take_while(|&unit| unit != 0);
    for decoded in core::char::decode_utf16(units) {
        let ch = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
        let _ = write!(console, "{ch}");
    }
}

/// Write the round-trip pattern, read it back into a cleared buffer, and compare. Prints the
/// checksum CI checks against the disk image.
fn round_trip(disk: &mut Disk, console: &mut dyn HalConsole) -> Result<(), Failure> {
    let mut written = [0u8; ROUND_TRIP_BYTES];
    fill_pattern(&mut written);
    disk.request.write(REQUEST_DATA, &written);
    disk.transfer(
        virtio::BLK_T_OUT,
        ROUND_TRIP_SECTOR,
        ROUND_TRIP_BYTES as u32,
    )?;

    // Clear the buffer first, so that bytes which match only because they were never replaced
    // cannot pass the comparison.
    disk.request.write(REQUEST_DATA, &[0u8; ROUND_TRIP_BYTES]);
    disk.transfer(virtio::BLK_T_IN, ROUND_TRIP_SECTOR, ROUND_TRIP_BYTES as u32)?;
    let read_back: [u8; ROUND_TRIP_BYTES] = disk.request.read_array(REQUEST_DATA);
    if read_back != written {
        return Err(Failure::RoundTrip);
    }
    let checksum = virtio::crc32(&read_back);
    let _ = writeln!(
        console,
        "virtio-blk: wrote {ROUND_TRIP_SECTORS} sectors at {ROUND_TRIP_SECTOR}, read back, \
         crc32 {checksum:#010x} matches the written data"
    );
    Ok(())
}

/// A deterministic pattern that is not a repeating short sequence, so a shifted or stale read
/// does not match it by accident.
pub(crate) fn fill_pattern(out: &mut [u8]) {
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7) ^ ((index >> 8) as u8).wrapping_mul(131) ^ 0xA5;
    }
}

/// Map the region a capability describes: `bar` plus the capability's offset, for its length.
fn map_cap(dev: usize, bars: &BarSet, cap: VirtioCap) -> Result<Region, Failure> {
    let base = bars
        .slots
        .get(usize::from(cap.bar))
        .copied()
        .and_then(nolx_pci_core::assigned_memory_base)
        .ok_or(Failure::BarNotMemory(cap.bar))?;
    let address = base
        .checked_add(u64::from(cap.offset))
        .ok_or(Failure::BarNotMemory(cap.bar))?;
    mmio::map_owned(dev, address, u64::from(cap.length)).map_err(Failure::Window)
}

/// The byte offset of a common-configuration field, widened for the register accessors.
const fn at(offset: u8) -> u64 {
    offset as u64
}

/// Poll with a two-second HPET budget when available, otherwise a bounded spin count.
/// A hard iteration ceiling also bounds the wait if the firmware clock stops advancing.
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

/// Both halves of the device's feature bits, as one 64-bit set.
fn read_features(common: &Region) -> Result<u64, Failure> {
    common.write32(at(common::DEVICE_FEATURE_SELECT), 0)?;
    let low = common.read32(at(common::DEVICE_FEATURE))?;
    common.write32(at(common::DEVICE_FEATURE_SELECT), 1)?;
    let high = common.read32(at(common::DEVICE_FEATURE))?;
    Ok(u64::from(low) | (u64::from(high) << 32))
}

/// Write the accepted feature set back, both halves.
fn write_driver_features(common: &Region, accepted: u64) -> Result<(), Failure> {
    common.write32(at(common::DRIVER_FEATURE_SELECT), 0)?;
    common.write32(at(common::DRIVER_FEATURE), accepted as u32)?;
    common.write32(at(common::DRIVER_FEATURE_SELECT), 1)?;
    common.write32(at(common::DRIVER_FEATURE), (accepted >> 32) as u32)?;
    Ok(())
}

/// Write a 64-bit register as its two halves, low first.
fn write_u64(common: &Region, offset: u8, value: u64) -> Result<(), Failure> {
    common.write32(at(offset), value as u32)?;
    common.write32(at(offset) + 4, (value >> 32) as u32)?;
    Ok(())
}

/// Queue 0, configured and enabled, with its ring memory.
struct Queue {
    size: u16,
    notify_offset: u16,
    ring: Dma,
    layout: virtio::SplitLayout,
    /// Requests published so far. The available index is this count, wrapping.
    submitted: u16,
}

/// The disk once it is running: its queue, the notify register, and the request page.
pub(crate) struct Disk {
    owner: usize,
    device: usize,
    capacity: u64,
    queue: Queue,
    notify: Region,
    notify_offset: u64,
    request: Dma,
    flush_supported: bool,
}

impl Disk {
    /// Read whole sectors starting at `sector` into `out`, one request of at most
    /// [`READ_CHUNK_BYTES`] at a time.
    pub(crate) fn read_sectors(&mut self, sector: u64, out: &mut [u8]) -> Result<(), Failure> {
        if !out.len().is_multiple_of(gpt::SECTOR_SIZE) {
            return Err(Failure::Unaligned);
        }
        let mut done = 0;
        while done < out.len() {
            let len = (out.len() - done).min(READ_CHUNK_BYTES);
            let first = sector + (done / gpt::SECTOR_SIZE) as u64;
            self.transfer(virtio::BLK_T_IN, first, len as u32)?;
            self.request.read(REQUEST_DATA, &mut out[done..done + len]);
            done += len;
        }
        Ok(())
    }

    /// Write whole sectors starting at `sector` from `data`, one request of at most
    /// [`READ_CHUNK_BYTES`] at a time. Each request completes before the next is published.
    pub(crate) fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), Failure> {
        if !data.len().is_multiple_of(gpt::SECTOR_SIZE) {
            return Err(Failure::Unaligned);
        }
        let mut done = 0;
        while done < data.len() {
            let len = (data.len() - done).min(READ_CHUNK_BYTES);
            let first = sector + (done / gpt::SECTOR_SIZE) as u64;
            self.request.write(REQUEST_DATA, &data[done..done + len]);
            self.transfer(virtio::BLK_T_OUT, first, len as u32)?;
            done += len;
        }
        Ok(())
    }

    /// Run one request of `len` bytes at `sector` and wait for the device to complete it. For a
    /// write, the data must already be in the request page; for a read, it is there afterwards.
    fn transfer(&mut self, kind: u32, sector: u64, len: u32) -> Result<(), Failure> {
        let direction = if kind == virtio::BLK_T_OUT {
            Direction::Write
        } else {
            Direction::Read
        };
        let request = Request {
            direction,
            lba: sector,
            sectors: len / virtio::SECTOR_BYTES as u32,
        };
        blockdev::run(self.device, request, || self.issue(kind, sector, len))
    }

    /// Issue one request on the virtio queue and wait for the device. Called through
    /// [`Disk::transfer`], which routes it through the block queue.
    fn issue(&mut self, kind: u32, sector: u64, len: u32) -> Result<(), Failure> {
        if !LIVE[self.owner].load(Ordering::Acquire) {
            return Err(Failure::NoDevice);
        }
        let result = self.issue_inner(kind, sector, len);
        if result.is_err() {
            // A published command may still own descriptor zero. Do not submit a flush or
            // reuse its buffer after failure: only checked reset can re-open this transport.
            LIVE[self.owner].store(false, Ordering::Release);
        }
        result
    }
    fn issue_inner(&mut self, kind: u32, sector: u64, len: u32) -> Result<(), Failure> {
        let submitted = REQUESTS_SUBMITTED[self.owner].fetch_add(1, Ordering::Relaxed) + 1;
        let request = &self.request;
        request.write(REQUEST_HEADER, &virtio::block_request_header(kind, sector));
        // A status byte the device never writes stays at this value, which is not `BLK_S_OK`.
        request.write(REQUEST_STATUS, &[0xFF]);
        let (header, data, status) = (
            request.address(REQUEST_HEADER),
            request.address(REQUEST_DATA),
            request.address(REQUEST_STATUS),
        );
        let data_chain = if kind == virtio::BLK_T_OUT {
            virtio::block_write_chain(header, data, status, len)
        } else {
            virtio::block_read_chain(header, data, status, len)
        };
        let flush_chain = virtio::block_flush_chain(header, status);
        let chain = if kind == virtio::BLK_T_FLUSH {
            &flush_chain[..]
        } else {
            &data_chain[..]
        };
        for (index, descriptor) in chain.iter().enumerate() {
            self.queue
                .ring
                .write(index as u64 * 16, &descriptor.to_bytes());
        }

        // Publish the chain: the head goes in the available ring at this request's slot, then
        // the index moves past it. The fences keep the device from seeing the index before the
        // entry it names.
        let slot = u64::from(self.queue.submitted % self.queue.size);
        self.queue.submitted = self.queue.submitted.wrapping_add(1);
        let avail = self.queue.layout.avail_offset;
        self.queue.ring.write16(avail + 4 + 2 * slot, 0);
        fence(Ordering::SeqCst);
        self.queue.ring.write16(avail + 2, self.queue.submitted);
        fence(Ordering::SeqCst);
        self.notify.write16(self.notify_offset, 0)?;
        #[cfg(feature = "hal-virtio-blk-recovery-test")]
        if recovery::fail(self.owner, 5) {
            return Err(Failure::Timeout("published request"));
        }

        // The used index must have moved past the count before this request.
        let used = self.queue.layout.used_offset;
        let before = self.queue.submitted.wrapping_sub(1);
        let ring = &self.queue.ring;
        poll(
            || Ok(virtio::has_completion(ring.read16(used + 2), before)),
            "block request",
        )?;

        // The request's completion interrupt must be taken before the next request is submitted.
        let delivered = msix::wait_for_interrupts(&COMPLETION_INTERRUPTS[self.owner], submitted);
        if delivered < submitted {
            return Err(Failure::Interrupts {
                delivered,
                expected: submitted,
            });
        }

        let mut status_byte = [0u8; 1];
        self.request.read(REQUEST_STATUS, &mut status_byte);
        virtio::block_status(status_byte[0])?;
        Ok(())
    }
}

/// Select queue 0, size it, give the device its three ring addresses, and enable it.
fn setup_queue(
    dev: usize,
    common: &Region,
    notify: &Region,
    cap: VirtioCap,
    vector: u16,
) -> Result<(), Failure> {
    common.write16(at(common::QUEUE_SELECT), 0)?;
    let max = common.read16(at(common::QUEUE_SIZE))?;
    if max == 0 {
        return Err(Failure::NoQueue);
    }
    let layout = virtio::transport::block_queue_layout(max)?;
    let size = layout.size;
    let notify_offset = common.read16(at(common::QUEUE_NOTIFY_OFF))?;
    let notify_at = virtio::notify_address(&cap, notify_offset)
        .checked_sub(u64::from(cap.offset))
        .ok_or(Failure::NoQueue)?;
    if notify_at & 1 != 0
        || notify_at
            .checked_add(2)
            .is_none_or(|end| end > notify.bytes())
    {
        return Err(Failure::NoQueue);
    }
    let mut state = STATE[dev].lock();
    state.queue = Some(Queue {
        size,
        notify_offset,
        ring: state.pages[0].take().unwrap(),
        layout,
        submitted: 0,
    });
    let queue = state.queue.as_ref().unwrap();
    common.write16(at(common::QUEUE_SIZE), size)?;
    write_u64(common, common::QUEUE_DESC, queue.ring.address(0))?;
    write_u64(
        common,
        common::QUEUE_DRIVER,
        queue.ring.address(layout.avail_offset),
    )?;
    write_u64(
        common,
        common::QUEUE_DEVICE,
        queue.ring.address(layout.used_offset),
    )?;
    common.write16(at(common::QUEUE_MSIX_VECTOR), vector)?;
    if common.read16(at(common::QUEUE_MSIX_VECTOR))? != vector {
        return Err(Failure::NoVector);
    }
    common.write16(at(common::QUEUE_ENABLE), 1)?;
    if common.read16(at(common::QUEUE_ENABLE))? != 1 {
        return Err(Failure::NoQueue);
    }
    Ok(())
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

/// The interrupt vector the queue's completions arrive on, through MSI-X table entry
/// [`MSIX_ENTRY`]. The handler counts them in [`COMPLETION_INTERRUPTS`].
pub const COMPLETION_VECTOR: u8 = 0x51;
/// The MSI-X table entry for the queue. Entry 0 is left for the configuration interrupt, which the
/// driver does not use.
const MSIX_ENTRY: u16 = 1;

/// Completion interrupts for this startup epoch. Written by the interrupt handler.
static COMPLETION_INTERRUPTS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static REQUESTS_SUBMITTED: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];

/// Dedicated vectors for the first instance (legacy 0x51) and additional framework handles.
fn completion_vector(dev: usize) -> u8 {
    if PRIMARY.load(Ordering::Acquire) == dev {
        COMPLETION_VECTOR
    } else {
        0x60 + dev as u8
    }
}
/// Whether the vector belongs to this driver's bounded per-controller reservation.
pub fn handles_vector(vector: u8) -> bool {
    vector == COMPLETION_VECTOR || (0x60..0x70).contains(&vector)
}
/// Attribute a vector to exactly one controller before acknowledging its completion.
pub fn on_completion_interrupt(vector: u8) {
    let dev = if vector == COMPLETION_VECTOR {
        Some(PRIMARY.load(Ordering::Acquire))
    } else if (0x60..0x70).contains(&vector) {
        Some(usize::from(vector - 0x60))
    } else {
        None
    };
    let Some(dev) = dev.filter(|&dev| dev < driver_start::DEVICES) else {
        return;
    };
    if !LIVE[dev].load(Ordering::Acquire) || vector != completion_vector(dev) {
        return;
    }
    COMPLETION_INTERRUPTS[dev].fetch_add(1, Ordering::Release);
}

impl super::storage_volume::SectorDisk for Disk {
    fn sector_count(&self) -> u64 {
        self.capacity
    }
    fn flush(&mut self) -> Result<(), nolxfs::DevError> {
        if !self.flush_supported {
            return Err(nolxfs::DevError::Io(5));
        }
        // Requests are synchronous, so no data command can cross this barrier.
        self.issue(virtio::BLK_T_FLUSH, 0, 0)
            .map_err(|_| nolxfs::DevError::Io(5))
    }
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, out.len())?;
        Disk::read_sectors(self, first, out).map_err(|_| nolxfs::DevError::Io(5))
    }
    fn write_sectors(&mut self, first: u64, data: &[u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, data.len())?;
        Disk::write_sectors(self, first, data).map_err(|_| nolxfs::DevError::Io(5))
    }
}

/// Exercise checked partial-start/failed-stop recovery against two real disks.
#[cfg(feature = "hal-virtio-blk-recovery-test")]
pub fn recovery_selftest(console: &mut dyn HalConsole) {
    recovery::run(console);
}
