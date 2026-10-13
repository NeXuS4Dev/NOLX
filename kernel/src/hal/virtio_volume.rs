//! A NOLXFS volume on a partition of the virtio disk.
//!
//! The probe hands over the disk and the first partition in use. This module presents that
//! partition to NOLXFS as a block device of 4 KiB blocks, mounts the volume on it (formatting the
//! partition first if it holds no volume), writes a file, commits, mounts the volume again from
//! the same device, and checks the file read back against what was written. The second mount is
//! what separates "the commit reached the disk" from "the bytes are still in memory".
//!
//! The file is written through the same requests the rest of the driver uses, so each block is two
//! 2 KiB transfers and every transfer is polled to completion.

use alloc::{string::String, vec};
use core::cell::RefCell;
use nolx_block_core::SECTOR_SIZE;
use nolx_hal_core::console::HalConsole;
use nolx_virtio_core as virtio;
use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::mkfs::{self, FormatOptions};
use nolxfs::on_disk_gen::ROOT_INO;
use nolxfs::volume::Volume;

use super::virtio_blk::{fill_pattern, Disk, Failure};

/// The NOLXFS block size. The kernel's own volume uses the same size.
const BLOCK_BYTES: usize = 4096;
/// Sectors in one NOLXFS block.
const SECTORS_PER_BLOCK: u64 = (BLOCK_BYTES / SECTOR_SIZE) as u64;
/// The name the kernel gives the volume it formats.
const VOLUME_NAME: &str = "nolx-data";
/// The file the kernel writes in the root directory, and its length: two blocks.
const FILE_NAME: &str = "checksum.bin";
const FILE_BYTES: usize = 2 * BLOCK_BYTES;
/// The I/O error code a failed transfer reports, as the POSIX layer's `EIO`.
const EIO: u32 = 5;

/// The disk, presented as the blocks of one partition. Block `n` is sectors
/// `first_sector + n * 8` to `first_sector + n * 8 + 7`.
pub(crate) struct PartitionDisk {
    // NOLXFS reads through `&self`, and a read needs `&mut` access to the queue. The cell gives
    // that access without changing the trait; the driver is single-threaded, so the borrow never
    // overlaps.
    disk: RefCell<Disk>,
    first_sector: u64,
    sectors: u64,
}

impl PartitionDisk {
    /// Refuse a block that is the wrong size or past the end of the partition.
    fn check(&self, block: u64, len: usize) -> Result<(), DevError> {
        if len != BLOCK_BYTES {
            return Err(DevError::BadBufferLen {
                got: len,
                want: BLOCK_BYTES,
            });
        }
        if block >= self.block_count() {
            return Err(DevError::OutOfRange { block });
        }
        Ok(())
    }

    fn first_sector_of(&self, block: u64) -> u64 {
        self.first_sector + block * SECTORS_PER_BLOCK
    }
}

impl BlockDevice for PartitionDisk {
    fn block_size(&self) -> usize {
        BLOCK_BYTES
    }

    fn block_count(&self) -> u64 {
        self.sectors / SECTORS_PER_BLOCK
    }

    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError> {
        self.check(index, buf.len())?;
        let first = self.first_sector_of(index);
        self.disk
            .borrow_mut()
            .read_sectors(first, buf)
            .map_err(|_| DevError::Io(EIO))
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError> {
        self.check(index, buf.len())?;
        let first = self.first_sector_of(index);
        self.disk
            .get_mut()
            .write_sectors(first, buf)
            .map_err(|_| DevError::Io(EIO))
    }
}

/// Put a volume on the partition, write the file, commit, mount again, and read the file back.
/// Prints the checksum of what was read back, which CI compares with the pattern it expects.
pub(crate) fn run(
    disk: Disk,
    first_sector: u64,
    sectors: u64,
    console: &mut dyn HalConsole,
) -> Result<(), Failure> {
    let mut dev = PartitionDisk {
        disk: RefCell::new(disk),
        first_sector,
        sectors,
    };
    // An existing volume is mounted as it is. A partition with no volume is formatted first.
    let mut vol = if mkfs::probe(&mut dev).is_ok() {
        Volume::mount(dev)?
    } else {
        let opts = FormatOptions {
            volume_name: String::from(VOLUME_NAME),
            ..FormatOptions::default()
        };
        Volume::format(dev, &opts)?
    };

    let file = vol.create_file(ROOT_INO, FILE_NAME, 0o644, 0, 0)?;
    let mut written = vec![0u8; FILE_BYTES];
    fill_pattern(&mut written);
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
        "virtio-blk: nolxfs file {FILE_NAME}: {FILE_BYTES} bytes written, committed, mounted \
         again, read back, crc32 {checksum:#010x} matches the written data"
    );
    Ok(())
}
