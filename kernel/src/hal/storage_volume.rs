//! Backend-neutral NOLXFS volumes on queued sector devices.
//!
//! The probe hands over the disk and the first partition in use. This module presents that
//! partition to NOLXFS as a block device of 4 KiB blocks, mounts the volume on it (formatting the
//! partition first only when it is blank), writes a file, commits, mounts the volume again from
//! the same device, and checks the file read back against what was written. The second mount is
//! what separates "the commit reached the disk" from "the bytes are still in memory".
//!
//! All transfers use the driver's queued transport. Commit barriers issue actual device-cache
//! flush commands before the checkpoint is exposed as durable.

use alloc::{string::String, vec};
use nolx_block_core::SECTOR_SIZE;
use nolx_hal_core::console::HalConsole;
use nolx_virtio_core as virtio;
use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::mkfs::{self, FormatOptions};
use nolxfs::on_disk_gen::ROOT_INO;
use nolxfs::sector::PartitionDevice;
pub(crate) use nolxfs::sector::SectorDevice as SectorDisk;
use nolxfs::volume::Volume;

use super::virtio_blk::fill_pattern;
use nolx_block_core::{self as gpt, Header};
use nolxfs::on_disk::FsError;

/// A volume failure is independent of the controller which transported its blocks.
#[derive(Debug)]
pub enum Failure {
    /// The filesystem rejected on-disk metadata or an operation.
    Filesystem(FsError),
    /// Partition metadata failed validation.
    Gpt(gpt::GptError),
    /// The transport failed.
    Io(DevError),
    /// No partition is in use.
    NoPartition,
    /// The remounted file differs from the bytes committed.
    FileMismatch,
}
impl From<FsError> for Failure {
    fn from(error: FsError) -> Self {
        Self::Filesystem(error)
    }
}
impl From<gpt::GptError> for Failure {
    fn from(error: gpt::GptError) -> Self {
        Self::Gpt(error)
    }
}
impl From<DevError> for Failure {
    fn from(error: DevError) -> Self {
        Self::Io(error)
    }
}

/// Validate both GPT checksums, geometry and overlaps before exposing the first partition.
pub(crate) fn discover<D: SectorDisk>(disk: &mut D, capacity: u64) -> Result<(u64, u64), Failure> {
    let mut sector = [0; SECTOR_SIZE];
    disk.read_sectors(1, &mut sector)?;
    let header = Header::parse(&sector, 1, capacity)?;
    let mut table = vec![0; header.table_bytes().div_ceil(SECTOR_SIZE) * SECTOR_SIZE];
    disk.read_sectors(header.entries_lba, &mut table)?;
    gpt::verify_table(&header, &table)?;
    gpt::check_partitions(&header, &table)?;
    for index in 0..header.entry_count {
        if let Some(part) = gpt::entry(&header, &table, index) {
            if !part.is_unused() {
                return Ok((part.first_lba, part.sectors()));
            }
        }
    }
    Err(Failure::NoPartition)
}

/// The NOLXFS block size. The kernel's own volume uses the same size.
const BLOCK_BYTES: usize = 4096;
/// The name the kernel gives the volume it formats.
const VOLUME_NAME: &str = "nolx-data";
/// The file the kernel writes in the root directory, and its length: two blocks.
const FILE_NAME: &str = "checksum.bin";
const FILE_BYTES: usize = 2 * BLOCK_BYTES;

/// Put a volume on the partition, write the file, commit, mount again, and read the file back.
/// Prints the checksum of what was read back, which CI compares with the pattern it expects.
pub(crate) fn run<D: SectorDisk>(
    disk: D,
    first_sector: u64,
    sectors: u64,
    console: &mut dyn HalConsole,
    backend: &str,
) -> Result<(), Failure> {
    let mut dev = PartitionDevice::new(disk, first_sector, sectors)?;
    // Only an entirely blank partition authorizes formatting. I/O failure or a damaged
    // existing container must not silently destroy the partition.
    let mut first_block = [0; BLOCK_BYTES];
    dev.read_block(0, &mut first_block)?;
    let mut vol = if first_block.iter().all(|&byte| byte == 0) {
        for block in 1..dev.block_count() {
            dev.read_block(block, &mut first_block)?;
            if first_block.iter().any(|&byte| byte != 0) {
                return Err(
                    FsError::InvalidField("nonblank partition has no valid container").into(),
                );
            }
        }
        let opts = FormatOptions {
            volume_name: String::from(VOLUME_NAME),
            ..FormatOptions::default()
        };
        Volume::format(dev, &opts)?
    } else {
        mkfs::probe(&mut dev)?;
        Volume::mount(dev)?
    };
    let mut written = vec![0u8; FILE_BYTES];
    fill_pattern(&mut written);
    let file = match vol.lookup(ROOT_INO, FILE_NAME) {
        Ok(file) => {
            let mut previous = vec![0; FILE_BYTES];
            if vol.read_data(file, 0, &mut previous)? != FILE_BYTES || previous != written {
                return Err(Failure::FileMismatch);
            }
            let _ = writeln!(
                console,
                "{backend}: existing committed file verified before write"
            );
            file
        }
        Err(FsError::NotFound) => vol.create_file(ROOT_INO, FILE_NAME, 0o644, 0, 0)?,
        Err(error) => return Err(error.into()),
    };
    if vol.write_data(file, 0, &written)? != FILE_BYTES {
        return Err(Failure::FileMismatch);
    }
    vol.commit()?;

    // Mount again from the same device, so the bytes come back from the disk through the
    // checkpoint the commit wrote, not from the first volume's memory.
    let dev = vol.into_device();
    let vol = Volume::mount(dev)?;
    let file = vol.lookup(ROOT_INO, FILE_NAME)?;
    let mut back = vec![0u8; FILE_BYTES];
    let count = vol.read_data(file, 0, &mut back)?;
    if count != FILE_BYTES || back != written {
        return Err(Failure::FileMismatch);
    }
    let checksum = virtio::crc32(&back);
    let _ = writeln!(
        console,
        "{backend}: nolxfs file {FILE_NAME}: {FILE_BYTES} bytes written, committed, mounted \
         again, read back, crc32 {checksum:#010x} matches the written data"
    );
    Ok(())
}
