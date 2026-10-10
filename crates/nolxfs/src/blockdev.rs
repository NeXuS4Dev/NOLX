//! The device abstraction NOLXFS is written against.
//!
//! In the kernel this is implemented by the I/O manager over a device stack; on the host
//! it is [`MemBlockDevice`], which is how the whole filesystem gets tested without
//! booting anything. The trait is deliberately tiny: everything else (caching, merging,
//! retry) belongs below it.

use alloc::vec;
use alloc::vec::Vec;

use crate::on_disk_gen;
use crate::{fletcher, Pod};

/// A device-level failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevError {
    /// The block index is past the end of the device.
    OutOfRange {
        /// The index that was requested.
        block: u64,
    },
    /// The device is read-only, or was mounted read-only because of its feature flags.
    ReadOnly,
    /// The device reported an I/O error. The payload is the device's own status code;
    /// the POSIX subsystem turns it into `EIO`.
    Io(u32),
    /// A buffer of the wrong length was passed. Always a caller bug.
    BadBufferLen {
        /// What the caller gave us.
        got: usize,
        /// What the device needs.
        want: usize,
    },
    /// The block was delivered intact but does not match its own Fletcher-64 checksum.
    ///
    /// This is a statement about the stored bytes, not about the device: the read
    /// succeeded. It lives here because [`BlockDevice::read_object`] is the one place
    /// every object read goes through, and a checksum that is only checked by some
    /// callers is a checksum that is not checked.
    BadChecksum {
        /// The object id from the header, which is what a repair tool needs.
        oid: u64,
    },
}

/// A block-addressable device.
///
/// Implementations must be honest about `block_size`: NOLXFS reads and writes whole
/// blocks and relies on the device's atomicity guarantee for a single block.
pub trait BlockDevice {
    /// Bytes per block.
    fn block_size(&self) -> usize;

    /// Number of blocks on the device.
    fn block_count(&self) -> u64;

    /// Read one block into `buf`, which must be exactly `block_size()` bytes long.
    ///
    /// # Errors
    ///
    /// Returns [`DevError::OutOfRange`] past the end of the device,
    /// [`DevError::BadBufferLen`] for a wrongly sized buffer, or [`DevError::Io`].
    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError>;

    /// Write one block from `buf`.
    ///
    /// # Errors
    ///
    /// As [`BlockDevice::read_block`], plus [`DevError::ReadOnly`] on a read-only device.
    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError>;

    /// Push any buffered writes to stable storage.
    ///
    /// `fsync(2)` and the checkpoint commit both depend on this actually ordering writes.
    ///
    /// # Errors
    ///
    /// Returns [`DevError::Io`] if the device could not guarantee durability.
    fn flush(&mut self) -> Result<(), DevError> {
        Ok(())
    }

    /// Read a block and interpret it as a `T`.
    ///
    /// # Errors
    ///
    /// As [`BlockDevice::read_block`], plus [`DevError::BadBufferLen`] if `T` is larger
    /// than a block, which means the caller asked for something the format forbids.
    fn read_object<T: Pod>(&self, index: u64) -> Result<T, DevError> {
        let size = self.block_size();
        if core::mem::size_of::<T>() > size {
            return Err(DevError::BadBufferLen {
                got: size,
                want: core::mem::size_of::<T>(),
            });
        }
        let mut buf = vec![0u8; size];
        self.read_block(index, &mut buf)?;

        // Every NOLXFS object starts with `nolx_obj_phys_t`, whose first eight bytes are a
        // Fletcher-64 checksum over the rest of the block. Checking it here, rather than
        // leaving it to whichever caller remembers, is the only way the guarantee means
        // anything: a mount that reads a torn superblock must refuse to mount, and a
        // filesystem that reads a torn B-tree node must not go on indexing with it.
        let header =
            on_disk_gen::nolx_obj_phys_t::read_from(&buf).ok_or(DevError::BadBufferLen {
                got: buf.len(),
                want: on_disk_gen::nolx_obj_phys_t::DISK_SIZE,
            })?;
        if !fletcher::verify(&buf, header.checksum) {
            return Err(DevError::BadChecksum { oid: header.oid });
        }

        // `read_from` copies out, so an unaligned scratch buffer is fine.
        T::read_from(&buf).ok_or(DevError::BadBufferLen {
            got: buf.len(),
            want: core::mem::size_of::<T>(),
        })
    }

    /// Serialise a `T` into a block, seal it, and write it.
    ///
    /// The remainder of the block is zeroed, so a rewritten block never leaks the previous
    /// occupant's bytes. The caller fills in the object header -- id, transaction, type --
    /// and this fills in the checksum, which makes it the exact counterpart of
    /// [`BlockDevice::read_object`]: nothing can be written here that cannot be read back.
    ///
    /// # Errors
    ///
    /// As [`BlockDevice::write_block`], plus [`DevError::BadBufferLen`] if `T` does not
    /// fit in a block or the block is not a whole number of checksum words.
    fn write_object<T: Pod>(&mut self, index: u64, value: &T) -> Result<(), DevError> {
        let size = self.block_size();
        if core::mem::size_of::<T>() > size {
            return Err(DevError::BadBufferLen {
                got: size,
                want: core::mem::size_of::<T>(),
            });
        }
        let mut buf = vec![0u8; size];
        buf[..core::mem::size_of::<T>()].copy_from_slice(value.as_bytes());
        // Zero the checksum field, checksum the block, store the result. `compute` skips
        // the first two words, so the zeroing is belt and braces rather than load-bearing,
        // but writing it explicitly means this cannot drift from `on_disk::seal`.
        buf[..8].copy_from_slice(&0u64.to_le_bytes());
        let sum = fletcher::compute(&buf).ok_or(DevError::BadBufferLen { got: size, want: 4 })?;
        buf[..8].copy_from_slice(&sum.to_le_bytes());
        self.write_block(index, &buf)
    }
}

/// An in-memory device, used by the host tools and every NOLXFS test.
///
/// It can be told to start failing after N writes, which is how the crash-recovery tests
/// simulate a power cut at an arbitrary point in a transaction.
pub struct MemBlockDevice {
    data: Vec<u8>,
    block_size: usize,
    /// Writes performed so far.
    writes: u64,
    /// If set, the write with this sequence number and every write after it fail.
    fail_from_write: Option<u64>,
    read_only: bool,
}

impl MemBlockDevice {
    /// Create a zero-filled device of `block_count` blocks.
    ///
    /// # Panics
    ///
    /// Panics if `block_size` is zero or not a multiple of 512, which every real device
    /// is.
    #[must_use]
    pub fn new(block_size: usize, block_count: u64) -> Self {
        assert!(
            block_size > 0 && block_size.is_multiple_of(512),
            "block_size must be a multiple of 512"
        );
        let total = block_size
            .checked_mul(block_count as usize)
            .expect("device size overflowed usize");
        Self {
            data: vec![0u8; total],
            block_size,
            writes: 0,
            fail_from_write: None,
            read_only: false,
        }
    }

    /// Create a device from existing contents, as when mounting an image file.
    ///
    /// # Panics
    ///
    /// Panics if `data.len()` is not a whole number of `block_size` blocks.
    #[must_use]
    pub fn from_contents(block_size: usize, data: Vec<u8>) -> Self {
        assert!(
            block_size > 0 && data.len().is_multiple_of(block_size),
            "data is not block aligned"
        );
        let writes = 0;
        Self {
            data,
            block_size,
            writes,
            fail_from_write: None,
            read_only: false,
        }
    }

    /// Make every write from the `n`th write onward fail with [`DevError::Io`].
    ///
    /// Pass `None` to clear. This is the hook the crash-recovery tests use.
    pub fn fail_writes_from(&mut self, n: Option<u64>) {
        self.fail_from_write = n;
    }

    /// Number of successful writes so far.
    #[must_use]
    pub const fn write_count(&self) -> u64 {
        self.writes
    }

    /// Make the device refuse writes.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// A copy of the device's contents, for comparing images or writing one to disk.
    #[must_use]
    pub fn contents(&self) -> &[u8] {
        &self.data
    }

    /// `true` if the whole device reads as zero, which is what an unformatted device is.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.data.iter().all(|&b| b == 0)
    }
}

impl BlockDevice for MemBlockDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        (self.data.len() / self.block_size) as u64
    }

    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError> {
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        let start = (index as usize)
            .checked_mul(self.block_size)
            .ok_or(DevError::OutOfRange { block: index })?;
        let end = start
            .checked_add(self.block_size)
            .filter(|&e| e <= self.data.len())
            .ok_or(DevError::OutOfRange { block: index })?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError> {
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        if self.read_only {
            return Err(DevError::ReadOnly);
        }
        if let Some(from) = self.fail_from_write {
            if self.writes >= from {
                return Err(DevError::Io(5));
            }
        }
        let start = (index as usize)
            .checked_mul(self.block_size)
            .ok_or(DevError::OutOfRange { block: index })?;
        let end = start
            .checked_add(self.block_size)
            .filter(|&e| e <= self.data.len())
            .ok_or(DevError::OutOfRange { block: index })?;
        self.data[start..end].copy_from_slice(buf);
        self.writes += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::on_disk_gen;

    #[test]
    fn geometry_is_reported_correctly() {
        let dev = MemBlockDevice::new(4096, 16);
        assert_eq!(dev.block_size(), 4096);
        assert_eq!(dev.block_count(), 16);
        assert!(dev.is_blank());
    }

    #[test]
    #[should_panic(expected = "multiple of 512")]
    fn rejects_a_nonsensical_block_size() {
        let _ = MemBlockDevice::new(4000, 4);
    }

    #[test]
    fn read_write_round_trip() {
        let mut dev = MemBlockDevice::new(4096, 8);
        let block: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        dev.write_block(3, &block).unwrap();
        let mut out = vec![0u8; 4096];
        dev.read_block(3, &mut out).unwrap();
        assert_eq!(out, block);
        assert!(!dev.is_blank());
    }

    #[test]
    fn out_of_range_is_an_error_not_a_panic() {
        let mut dev = MemBlockDevice::new(4096, 4);
        let mut buf = vec![0u8; 4096];
        assert_eq!(
            dev.read_block(4, &mut buf),
            Err(DevError::OutOfRange { block: 4 })
        );
        assert_eq!(
            dev.write_block(u64::MAX, &buf),
            Err(DevError::OutOfRange { block: u64::MAX })
        );
    }

    #[test]
    fn wrong_buffer_length_is_reported_with_both_sizes() {
        let dev = MemBlockDevice::new(4096, 4);
        let mut buf = vec![0u8; 512];
        assert_eq!(
            dev.read_block(0, &mut buf),
            Err(DevError::BadBufferLen {
                got: 512,
                want: 4096
            })
        );
    }

    #[test]
    fn read_only_refuses_writes_but_still_reads() {
        let mut dev = MemBlockDevice::new(4096, 4);
        let block = vec![7u8; 4096];
        dev.write_block(0, &block).unwrap();
        dev.set_read_only(true);
        assert_eq!(dev.write_block(1, &block), Err(DevError::ReadOnly));
        let mut out = vec![0u8; 4096];
        dev.read_block(0, &mut out).unwrap();
        assert_eq!(out, block);
    }

    #[test]
    fn injected_failure_starts_exactly_where_asked() {
        let mut dev = MemBlockDevice::new(4096, 8);
        dev.fail_writes_from(Some(3));
        let block = vec![1u8; 4096];
        assert!(dev.write_block(0, &block).is_ok());
        assert!(dev.write_block(1, &block).is_ok());
        assert!(dev.write_block(2, &block).is_ok());
        assert_eq!(dev.write_block(3, &block), Err(DevError::Io(5)));
        assert_eq!(dev.write_count(), 3);
        dev.fail_writes_from(None);
        assert!(dev.write_block(3, &block).is_ok());
    }

    #[test]
    fn a_corrupted_object_is_refused_on_read() {
        let mut dev = MemBlockDevice::new(4096, 8);
        let mut sb = on_disk_gen::nolx_obj_phys_t::zeroed();
        sb.oid = 7;
        sb.r#type = on_disk_gen::OBJ_CONTAINER_SUPERBLOCK;
        dev.write_object(0, &sb).unwrap();

        // Flip a bit in the body and leave the stored checksum alone: this is what a torn
        // write or a bad sector looks like, and it is the case the checksum exists for.
        let mut raw = vec![0u8; 4096];
        dev.read_block(0, &mut raw).unwrap();
        raw[20] ^= 0xFF;
        dev.write_block(0, &raw).unwrap();

        assert!(
            matches!(
                dev.read_object::<on_disk_gen::nolx_obj_phys_t>(0),
                Err(DevError::BadChecksum { oid: 7 })
            ),
            "a corrupted object must be refused on read"
        );
    }

    #[test]
    fn object_helpers_round_trip_a_real_on_disk_struct() {
        let mut dev = MemBlockDevice::new(4096, 8);
        let mut sb = on_disk_gen::nolx_obj_phys_t::zeroed();
        sb.oid = 0x1234_5678_9abc_def0;
        sb.xid = 42;
        sb.r#type = on_disk_gen::OBJ_CONTAINER_SUPERBLOCK;
        dev.write_object(0, &sb).unwrap();

        let back: on_disk_gen::nolx_obj_phys_t = dev.read_object(0).unwrap();
        // `write_object` seals what it writes, so the one field that differs on the way
        // back is the checksum it filled in.
        assert_eq!(back.oid, sb.oid);
        assert_eq!(back.xid, sb.xid);
        assert_eq!(back.r#type, sb.r#type);
        assert_ne!(back.checksum, 0, "write_object must seal what it writes");
        // The rest of the block must have been zeroed, not left as it was.
        let mut raw = vec![0u8; 4096];
        dev.read_block(0, &mut raw).unwrap();
        assert!(raw[core::mem::size_of::<on_disk_gen::nolx_obj_phys_t>()..]
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn object_helpers_reject_a_struct_larger_than_a_block() {
        let mut dev = MemBlockDevice::new(512, 8);
        let sb = on_disk_gen::nolx_nx_superblock_t::zeroed();
        let err = dev.write_object(0, &sb).unwrap_err();
        assert!(matches!(
            err,
            DevError::BadBufferLen {
                got: 512,
                want: 4096
            }
        ));
    }
}
