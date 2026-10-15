//! The virtio block driver's probe: find the disk, bring it up through its PCI capabilities,
//! and read sector 0 through a split virtqueue.
//!
//! The rules -- which devices are block devices, the status sequence, feature negotiation, the
//! queue layout, the request format -- live in `nolx-virtio-core`, where they are host-tested.
//! This module supplies what the rules cannot: register access through [`mmio`] windows, queue
//! memory from the frame allocator and reached through the direct map, and the polling.
//!
//! The probe is synchronous and polled. There are no interrupts yet, so each request waits for
//! the used ring to move before its result is read. Every wait is bounded, so a device that
//! never answers is reported as a timeout instead of hanging boot.
//!
//! After the sector-0 read, the probe reads the GUID Partition Table: the primary header in LBA 1
//! and the table it names, checked with the `nolx-block-core` rules, then prints the partitions in
//! use. Then it writes a 2 KiB pattern to sectors 1600 to 1603, in the gap after the last
//! partition, reads it back, and compares CRC-32 checksums. CI computes the same checksum over the
//! image file after QEMU exits, so the write has to have reached the disk and not only the device's
//! cache. Last, it puts a NOLXFS volume on the first partition in use; see [`super::virtio_volume`].

use alloc::vec;
use nolx_block_core::{self as gpt, GptError, Header};
use super::blockdev;
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolxfs::on_disk::FsError;

use super::dma::Dma;
use crate::hal::pci::LegacyPorts;
use crate::mm::mmio::{self, Region};
use core::sync::atomic::{fence, Ordering};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{Bar, BarSet, Bdf};
use nolx_virtio_core::{self as virtio, common, VirtioCap, VirtioError};

/// The queue size the driver asks for, or the device's maximum if that is smaller.
const QUEUE_SIZE_WANTED: u16 = 8;
/// How many times a bounded wait looks before it gives up.
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
pub fn probe(console: &mut dyn HalConsole) {
    if let Err(failure) = bring_up(console) {
        let _ = writeln!(console, "virtio-blk: not brought up: {failure:?}");
    }
}

fn bring_up(console: &mut dyn HalConsole) -> Result<(), Failure> {
    let bdf = find_device().ok_or(Failure::NoDevice)?;
    let bars = nolx_pci_core::read_bars(&mut LegacyPorts, bdf);
    let caps = virtio::discover(&mut LegacyPorts, bdf);
    let common_cap = caps.common.ok_or(Failure::NoCapability("common"))?;
    let notify_cap = caps.notify.ok_or(Failure::NoCapability("notify"))?;
    let device_cap = caps.device.ok_or(Failure::NoCapability("device"))?;
    let common = map_cap(&bars, common_cap)?;
    let notify = map_cap(&bars, notify_cap)?;
    let config = map_cap(&bars, device_cap)?;

    // Reset, then the status steps in the order the rules give.
    common.write8(at(common::DEVICE_STATUS), 0)?;
    poll(
        || Ok(common.read8(at(common::DEVICE_STATUS))? == 0),
        "reset",
    )?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[0])?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[1])?;
    let accepted = virtio::negotiate(read_features(&common)?)?;
    write_driver_features(&common, accepted)?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[2])?;
    virtio::check_features_ok(common.read8(at(common::DEVICE_STATUS))?)?;

    let queue = setup_queue(&common)?;
    common.write8(at(common::DEVICE_STATUS), virtio::INIT_STATUS_SEQUENCE[3])?;

    let sectors = virtio::capacity_sectors(config.read32(0)?, config.read32(4)?);
    let bytes = virtio::capacity_bytes(sectors).ok_or(Failure::Capacity)?;
    let _ = writeln!(
        console,
        "virtio-blk: {:02x}:{:02x}.{} capacity {sectors} sectors ({bytes} bytes), \
         queue size {}, driver ok",
        bdf.bus, bdf.device, bdf.function, queue.size
    );

    let notify_offset =
        virtio::notify_address(&notify_cap, queue.notify_offset) - u64::from(notify_cap.offset);
    let mut disk = Disk {
        queue,
        notify,
        notify_offset,
        request: Dma::new().ok_or(Failure::Memory)?,
    };

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

    let volume = read_partitions(&mut disk, sectors, console)?;
    round_trip(&mut disk, console)?;
    match volume {
        Some((first, last)) => super::virtio_volume::run(disk, first, last - first + 1, console),
        None => {
            let _ = writeln!(
                console,
                "virtio-blk: gpt has no partitions in use; no volume"
            );
            Ok(())
        }
    }
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

/// The first virtio block device on the bus, if there is one.
fn find_device() -> Option<Bdf> {
    let inventory = nolx_pci_core::enumerate(&mut LegacyPorts);
    inventory
        .as_slice()
        .iter()
        .find(|info| virtio::is_block_device(info.vendor_id, info.device_id))
        .map(|info| info.bdf)
}

/// Map the region a capability describes: `bar` plus the capability's offset, for its length.
fn map_cap(bars: &BarSet, cap: VirtioCap) -> Result<Region, Failure> {
    let base = match bars.slots[usize::from(cap.bar)] {
        Bar::Memory32 { base, .. } => u64::from(base),
        Bar::Memory64 { base, .. } => base,
        _ => return Err(Failure::BarNotMemory(cap.bar)),
    };
    mmio::map(base + u64::from(cap.offset), u64::from(cap.length)).map_err(Failure::Window)
}

/// The byte offset of a common-configuration field, widened for the register accessors.
const fn at(offset: u8) -> u64 {
    offset as u64
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
    queue: Queue,
    notify: Region,
    notify_offset: u64,
    request: Dma,
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
        blockdev::run(blockdev::VIRTIO, request, || self.issue(kind, sector, len))
    }

    /// Issue one request on the virtio queue and wait for the device. Called through
    /// [`Disk::transfer`], which routes it through the block queue.
    fn issue(&mut self, kind: u32, sector: u64, len: u32) -> Result<(), Failure> {
        let request = &self.request;
        request.write(REQUEST_HEADER, &virtio::block_request_header(kind, sector));
        // A status byte the device never writes stays at this value, which is not `BLK_S_OK`.
        request.write(REQUEST_STATUS, &[0xFF]);
        let (header, data, status) = (
            request.address(REQUEST_HEADER),
            request.address(REQUEST_DATA),
            request.address(REQUEST_STATUS),
        );
        let chain = if kind == virtio::BLK_T_OUT {
            virtio::block_write_chain(header, data, status, len)
        } else {
            virtio::block_read_chain(header, data, status, len)
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

        // The used index must have moved past the count before this request.
        let used = self.queue.layout.used_offset;
        let before = self.queue.submitted.wrapping_sub(1);
        let ring = &self.queue.ring;
        poll(
            || Ok(virtio::has_completion(ring.read16(used + 2), before)),
            "block request",
        )?;

        let mut status_byte = [0u8; 1];
        self.request.read(REQUEST_STATUS, &mut status_byte);
        virtio::block_status(status_byte[0])?;
        Ok(())
    }
}

/// Select queue 0, size it, give the device its three ring addresses, and enable it.
fn setup_queue(common: &Region) -> Result<Queue, Failure> {
    common.write16(at(common::QUEUE_SELECT), 0)?;
    let max = common.read16(at(common::QUEUE_SIZE))?;
    if max == 0 {
        return Err(Failure::NoQueue);
    }
    let size = max.min(QUEUE_SIZE_WANTED);
    let layout = virtio::split_layout(size)?;
    common.write16(at(common::QUEUE_SIZE), size)?;
    let ring = Dma::new().ok_or(Failure::Memory)?;
    write_u64(common, common::QUEUE_DESC, ring.address(0))?;
    write_u64(
        common,
        common::QUEUE_DRIVER,
        ring.address(layout.avail_offset),
    )?;
    write_u64(
        common,
        common::QUEUE_DEVICE,
        ring.address(layout.used_offset),
    )?;
    let notify_offset = common.read16(at(common::QUEUE_NOTIFY_OFF))?;
    common.write16(at(common::QUEUE_ENABLE), 1)?;
    Ok(Queue {
        size,
        notify_offset,
        ring,
        layout,
        submitted: 0,
    })
}

impl From<QueueError> for Failure {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}
