//! Adapt a validated disk partition, addressed in 512-byte sectors, to filesystem blocks.
//!
//! This layer validates partition bounds against transport capacity, but does not discover
//! partition tables or issue device-specific commands.

use crate::blockdev::{BlockDevice, DevError};
use core::cell::RefCell;

/// A synchronous 512-byte sector transport, with an explicit durable-write barrier.
///
/// Implementations must reject unaligned buffers and return only after the transfer completes.
pub trait SectorDevice {
    /// Capacity in 512-byte sectors, validated from the device identification/configuration.
    fn sector_count(&self) -> u64;
    /// Read whole sectors, starting at `first`, into `out`.
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), DevError>;
    /// Write whole sectors, starting at `first`, from `data`.
    fn write_sectors(&mut self, first: u64, data: &[u8]) -> Result<(), DevError>;
    /// Complete and order all prior writes onto stable storage. Never a no-op cache flush.
    fn flush(&mut self) -> Result<(), DevError>;
}

// Borrowed adapters let the filesystem mount/commit/remount without destroying driver ownership.
impl<D: SectorDevice + ?Sized> SectorDevice for &mut D {
    fn sector_count(&self) -> u64 {
        (**self).sector_count()
    }
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), DevError> {
        (**self).read_sectors(first, out)
    }
    fn write_sectors(&mut self, first: u64, data: &[u8]) -> Result<(), DevError> {
        (**self).write_sectors(first, data)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        (**self).flush()
    }
}

/// A partition exposed as 4096-byte filesystem blocks; a trailing partial block is inaccessible.
pub struct PartitionDevice<D: SectorDevice> {
    disk: RefCell<D>,
    first_sector: u64,
    sectors: u64,
}

impl<D: SectorDevice> PartitionDevice<D> {
    /// Construct an adapter after partition-table validation.
    ///
    /// Refuses empty partitions, overflowing sector endpoints and partitions smaller than one block.
    /// The exclusive endpoint must fit in the transport's physical sector capacity.
    pub fn new(disk: D, first_sector: u64, sectors: u64) -> Result<Self, DevError> {
        if sectors < 8
            || first_sector
                .checked_add(sectors)
                .is_none_or(|end| end > disk.sector_count())
        {
            return Err(DevError::OutOfRange {
                block: first_sector,
            });
        }
        Ok(Self {
            disk: RefCell::new(disk),
            first_sector,
            sectors,
        })
    }

    fn check(&self, block: u64, len: usize) -> Result<u64, DevError> {
        if len != 4096 {
            return Err(DevError::BadBufferLen {
                got: len,
                want: 4096,
            });
        }
        if block >= self.block_count() {
            return Err(DevError::OutOfRange { block });
        }
        // Construction checked the exclusive partition endpoint, and block_count rounds down.
        Ok(self.first_sector + block * 8)
    }
}

impl<D: SectorDevice> BlockDevice for PartitionDevice<D> {
    fn block_size(&self) -> usize {
        4096
    }
    fn block_count(&self) -> u64 {
        self.sectors / 8
    }
    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError> {
        let sector = self.check(index, buf.len())?;
        self.disk.borrow_mut().read_sectors(sector, buf)
    }
    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError> {
        let sector = self.check(index, buf.len())?;
        self.disk.get_mut().write_sectors(sector, buf)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        self.disk.get_mut().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{rc::Rc, vec, vec::Vec};
    use core::cell::Cell;

    struct Disk {
        bytes: Vec<u8>,
        flushes: Rc<Cell<usize>>,
        fail: bool,
    }
    impl SectorDevice for Disk {
        fn sector_count(&self) -> u64 {
            (self.bytes.len() / 512) as u64
        }
        fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), DevError> {
            if self.fail {
                return Err(DevError::Io(71));
            }
            let first = first as usize * 512;
            out.copy_from_slice(&self.bytes[first..first + out.len()]);
            Ok(())
        }
        fn write_sectors(&mut self, first: u64, data: &[u8]) -> Result<(), DevError> {
            if self.fail {
                return Err(DevError::Io(71));
            }
            let first = first as usize * 512;
            self.bytes[first..first + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), DevError> {
            if self.fail {
                return Err(DevError::Io(71));
            }
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }
    fn disk() -> Disk {
        Disk {
            bytes: vec![0x55; 64 * 512],
            flushes: Rc::new(Cell::new(0)),
            fail: false,
        }
    }

    #[test]
    fn borrowed_partition_does_not_consume_controller_ownership() {
        let mut disk = disk();
        {
            let mut part = PartitionDevice::new(&mut disk, 5, 16).unwrap();
            part.write_block(0, &[0x77; 4096]).unwrap();
            part.flush().unwrap();
        }
        assert_eq!(disk.flushes.get(), 1);
        assert_eq!(disk.bytes[5 * 512], 0x77);
        assert!(PartitionDevice::new(&mut disk, 63, 8).is_err());
    }
    #[test]
    fn maps_blocks_without_touching_neighbors() {
        let mut dev = PartitionDevice::new(disk(), 5, 16).unwrap();
        dev.write_block(1, &[0xab; 4096]).unwrap();
        let mut back = [0; 4096];
        dev.read_block(1, &mut back).unwrap();
        assert_eq!(back, [0xab; 4096]);
        let disk = dev.disk.into_inner();
        assert!(disk.bytes[..13 * 512].iter().all(|&b| b == 0x55));
        assert!(disk.bytes[21 * 512..].iter().all(|&b| b == 0x55));
    }
    #[test]
    fn refuses_bad_lengths_and_out_of_partition_blocks() {
        let mut dev = PartitionDevice::new(disk(), 5, 17).unwrap();
        assert_eq!(dev.block_count(), 2);
        assert_eq!(
            dev.write_block(2, &[0; 4096]),
            Err(DevError::OutOfRange { block: 2 })
        );
        assert_eq!(
            dev.read_block(u64::MAX, &mut [0; 4096]),
            Err(DevError::OutOfRange { block: u64::MAX })
        );
        assert_eq!(
            dev.read_block(0, &mut [0; 512]),
            Err(DevError::BadBufferLen {
                got: 512,
                want: 4096
            })
        );
        assert_eq!(
            dev.write_block(0, &[]),
            Err(DevError::BadBufferLen { got: 0, want: 4096 })
        );
    }
    #[test]
    fn refuses_overflow_and_too_small_partitions() {
        assert!(PartitionDevice::new(disk(), u64::MAX - 7, 8).is_err());
        assert!(PartitionDevice::new(disk(), 0, 7).is_err());
        assert!(PartitionDevice::new(disk(), 0, 0).is_err());
    }
    #[test]
    fn forwards_flush_and_preserves_transport_errors() {
        let d = disk();
        let flushes = d.flushes.clone();
        let mut dev = PartitionDevice::new(d, 0, 16).unwrap();
        dev.flush().unwrap();
        assert_eq!(flushes.get(), 1);
        dev.disk.get_mut().fail = true;
        assert_eq!(dev.read_block(0, &mut [0; 4096]), Err(DevError::Io(71)));
        assert_eq!(dev.write_block(0, &[0; 4096]), Err(DevError::Io(71)));
        assert_eq!(dev.flush(), Err(DevError::Io(71)));
    }
}

/// Validate a whole-sector operation before any device command or DMA buffer access.
pub fn check_sector_io(capacity: u64, first: u64, bytes: usize) -> Result<(), DevError> {
    if bytes == 0 || !bytes.is_multiple_of(512) {
        return Err(DevError::BadBufferLen {
            got: bytes,
            want: 512,
        });
    }
    if first
        .checked_add((bytes / 512) as u64)
        .is_none_or(|end| end > capacity)
    {
        return Err(DevError::OutOfRange { block: first });
    }
    Ok(())
}

#[cfg(test)]
mod bounds_tests {
    use super::*;
    #[test]
    fn sector_transfers_refuse_empty_unaligned_overflow_and_end_of_disk() {
        assert_eq!(check_sector_io(100, 99, 512), Ok(()));
        assert_eq!(
            check_sector_io(100, 100, 512),
            Err(DevError::OutOfRange { block: 100 })
        );
        assert_eq!(
            check_sector_io(u64::MAX, u64::MAX, 512),
            Err(DevError::OutOfRange { block: u64::MAX })
        );
        assert_eq!(
            check_sector_io(100, 0, 0),
            Err(DevError::BadBufferLen { got: 0, want: 512 })
        );
        assert_eq!(
            check_sector_io(100, 0, 513),
            Err(DevError::BadBufferLen {
                got: 513,
                want: 512
            })
        );
    }
}
