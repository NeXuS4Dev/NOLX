//! VFS driver for **NOLXFS** (`crates/nolxfs`).
//!
//! This is a thin adapter: the filesystem itself — checkpoint ring, copy-on-write
//! B-trees, space manager, snapshots — lives in [`nolxfs::volume::Volume`], and
//! this module translates its typed [`FsError`] results into the VFS layer's
//! kernel-errno convention and presents the exact method set `vfs.rs` dispatches
//! on (`mkdir`/`create_file`/`read`/`write`/`stat` plus the M8 additions
//! `rmdir`/`link`/`symlink`/`readlink`/`unlink`/`rename`/`truncate`/`sync`).
//!
//! Consistency with the host tools is structural: `mkfs.nolxfs`, `fsck.nolxfs`,
//! `nolxfs-dump`, this driver and the kernel selftest all sit on the same
//! `nolxfs` crate, so an image written by one is read identically by the others.

use core::fmt;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::errno;
use nolx_types::scalars::{is_dir, is_reg, Gid, Mode, Stat, Uid, NAME_MAX};
use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::mkfs::{self, FormatOptions, VolumeInfo};
use nolxfs::on_disk::FsError;
use nolxfs::on_disk_gen as gen;
use nolxfs::volume::Volume;

/// Root directory inode number in NOLXFS (`gen::ROOT_INO`).
pub const NOLXFS_ROOT_INO: u64 = gen::ROOT_INO;

/// Sparse in-memory [`BlockDevice`] that allocates storage only for blocks that have been
/// written, returning zeroes for untouched blocks.
///
/// This keeps a mounted in-memory NOLXFS container's footprint proportional to the live
/// metadata and file blocks rather than the entire checkpoint ring + volume capacity.
#[derive(Clone, Debug)]
pub struct SparseBlockDevice {
    block_size: usize,
    block_count: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl SparseBlockDevice {
    /// Construct a sparse block device of `block_count` blocks of `block_size` bytes.
    #[must_use]
    pub const fn new(block_size: usize, block_count: u64) -> Self {
        Self {
            block_size,
            block_count,
            blocks: BTreeMap::new(),
        }
    }

    /// Number of non-zero blocks currently materialized in memory.
    #[must_use]
    pub fn materialized_blocks(&self) -> usize {
        self.blocks.len()
    }
}

impl BlockDevice for SparseBlockDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError> {
        if index >= self.block_count {
            return Err(DevError::OutOfRange { block: index });
        }
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        if let Some(stored) = self.blocks.get(&index) {
            buf.copy_from_slice(stored);
        } else {
            buf.fill(0);
        }
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError> {
        if index >= self.block_count {
            return Err(DevError::OutOfRange { block: index });
        }
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        self.blocks.insert(index, buf.to_vec());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DevError> {
        Ok(())
    }
}

/// A mounted NOLXFS volume backed by a [`BlockDevice`].
///
/// Wraps [`nolxfs::volume::Volume`]; most operations are one-line translations.
/// Mutations are buffered by the volume until [`NolxfsVolume::sync`] commits
/// them as one checkpoint, which is how the VFS gets atomic multi-operation
/// transactions without knowing about xids.
pub struct NolxfsVolume<D: BlockDevice = SparseBlockDevice> {
    inner: Box<Volume<D>>,
    info: Box<VolumeInfo>,
}

impl<D: BlockDevice> fmt::Debug for NolxfsVolume<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NolxfsVolume")
            .field("name", &self.info.name)
            .field("fs_tree_root", &self.inner.fs_root())
            .field("xid", &self.inner.xid())
            .finish()
    }
}

impl NolxfsVolume<SparseBlockDevice> {
    /// Format a fresh sparse in-memory NOLXFS container of `block_count` blocks of
    /// `block_size` bytes and mount it immediately.
    pub fn format_and_mount(block_size: usize, block_count: u64, label: &str) -> Result<Self, i32> {
        let dev = SparseBlockDevice::new(block_size, block_count);
        let opts = FormatOptions {
            block_size: block_size as u32,
            volume_name: String::from(label),
            ..FormatOptions::default()
        };
        let inner = Volume::format(dev, &opts).map_err(fs_err_to_errno)?;
        let info = Box::new(VolumeInfo {
            name: String::from(inner.name()),
            layout: *inner.layout(),
            superblock: inner.volume_superblock(),
        });
        Ok(Self {
            inner: Box::new(inner),
            info,
        })
    }
}

impl<D: BlockDevice> NolxfsVolume<D> {
    /// Probe and mount an existing NOLXFS container on `dev`.
    ///
    /// Resumes from the newest valid checkpoint; a torn newest checkpoint falls
    /// back to the previous one rather than failing the mount.
    pub fn mount(mut dev: D) -> Result<Self, i32> {
        let info = mkfs::probe(&mut dev).map_err(fs_err_to_errno)?;
        let inner = Volume::mount(dev).map_err(fs_err_to_errno)?;
        Ok(Self {
            inner: Box::new(inner),
            info: Box::new(info),
        })
    }

    /// Mount the named snapshot of the container on `dev` as a read-only volume.
    pub fn mount_snapshot(dev: D, name: &str) -> Result<Self, i32> {
        let mut probe_dev = dev;
        let info = mkfs::probe(&mut probe_dev).map_err(fs_err_to_errno)?;
        let inner = Volume::mount_snapshot(probe_dev, name).map_err(fs_err_to_errno)?;
        Ok(Self {
            inner: Box::new(inner),
            info: Box::new(info),
        })
    }

    /// Consume the mounted volume and return the underlying [`BlockDevice`].
    #[must_use]
    pub fn into_device(self) -> D {
        self.inner.into_device()
    }

    /// Volume metadata read at mount time.
    #[must_use]
    pub const fn volume_info(&self) -> &VolumeInfo {
        &self.info
    }

    /// The underlying filesystem volume (snapshot creation, xattrs, cloning).
    #[must_use]
    pub fn volume(&self) -> &Volume<D> {
        &self.inner
    }

    /// Mutable access to the underlying filesystem volume.
    pub fn volume_mut(&mut self) -> &mut Volume<D> {
        &mut self.inner
    }

    /// Commit every pending mutation as one checkpoint (`fsync`/`sync`).
    pub fn sync(&mut self) -> Result<(), i32> {
        self.inner.commit().map_err(fs_err_to_errno)
    }

    /// Read and decode `nolx_inode_t` for `ino`.
    pub fn read_inode(&self, ino: u64) -> Result<gen::nolx_inode_t, i32> {
        self.inner.read_inode(ino).map_err(fs_err_to_errno)
    }

    /// Parent directory inode for `ino`.
    #[must_use]
    pub fn parent_of(&self, ino: u64) -> Option<u64> {
        if ino == NOLXFS_ROOT_INO {
            return Some(NOLXFS_ROOT_INO);
        }
        let inode = self.read_inode(ino).ok()?;
        Some(inode.parent_id)
    }

    /// Look up `name` inside directory `dir_ino`.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64, i32> {
        self.inner.lookup(dir_ino, name).map_err(fs_err_to_errno)
    }

    /// Enumerate `(name, child_ino)` entries in `dir_ino`.
    pub fn readdir(&mut self, dir_ino: u64) -> Result<Vec<(String, u64)>, i32> {
        self.inner.readdir(dir_ino).map_err(fs_err_to_errno)
    }

    /// Create a subdirectory named `name` inside `dir_ino`.
    pub fn mkdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
    ) -> Result<u64, i32> {
        validate_name(name)?;
        self.inner
            .mkdir(dir_ino, name, mode, uid, gid)
            .map_err(fs_err_to_errno)
    }

    /// Remove the empty directory `name` inside `dir_ino` (`rmdir(2)`).
    pub fn rmdir(&mut self, dir_ino: u64, name: &str) -> Result<(), i32> {
        validate_name(name)?;
        self.inner.rmdir(dir_ino, name).map_err(fs_err_to_errno)
    }

    /// Create or look up a regular file named `name` inside `dir_ino`.
    ///
    /// Returns `(ino, created)`; with `exclusive = true` an existing name is
    /// `EEXIST`, exactly like `O_CREAT|O_EXCL`.
    pub fn create_file(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
        exclusive: bool,
    ) -> Result<(u64, bool), i32> {
        validate_name(name)?;
        match self.inner.lookup(dir_ino, name) {
            Ok(existing) => {
                if exclusive {
                    return Err(errno::EEXIST);
                }
                let target = self.read_inode(existing)?;
                if is_dir(u32::from(target.mode)) {
                    return Err(errno::EISDIR);
                }
                Ok((existing, false))
            }
            Err(FsError::NotFound) => {
                let ino = self
                    .inner
                    .create_file(dir_ino, name, mode, uid, gid)
                    .map_err(fs_err_to_errno)?;
                Ok((ino, true))
            }
            Err(e) => Err(fs_err_to_errno(e)),
        }
    }

    /// Create a hard link to `target_ino` as `name` inside `dir_ino` (`link(2)`).
    pub fn link(&mut self, target_ino: u64, dir_ino: u64, name: &str) -> Result<(), i32> {
        validate_name(name)?;
        self.inner
            .link(target_ino, dir_ino, name)
            .map_err(fs_err_to_errno)
    }

    /// Create a symbolic link `name` inside `dir_ino` holding `target` (`symlink(2)`).
    pub fn symlink(
        &mut self,
        dir_ino: u64,
        name: &str,
        target: &[u8],
        uid: Uid,
        gid: Gid,
    ) -> Result<u64, i32> {
        validate_name(name)?;
        self.inner
            .symlink(dir_ino, name, target, uid, gid)
            .map_err(fs_err_to_errno)
    }

    /// Read the target stored in symlink `ino` (`readlink(2)`).
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, i32> {
        self.inner.readlink(ino).map_err(fs_err_to_errno)
    }

    /// Remove the non-directory entry `name` from `dir_ino` (`unlink(2)`).
    pub fn unlink(&mut self, dir_ino: u64, name: &str) -> Result<(), i32> {
        validate_name(name)?;
        self.inner.unlink(dir_ino, name).map_err(fs_err_to_errno)
    }

    /// Rename `old_name` in `old_dir` to `new_name` in `new_dir` (`rename(2)`).
    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<(), i32> {
        validate_name(old_name)?;
        validate_name(new_name)?;
        self.inner
            .rename(old_dir, old_name, new_dir, new_name)
            .map_err(fs_err_to_errno)
    }

    /// Truncate or extend regular file `ino` to `new_size` bytes.
    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<(), i32> {
        self.inner.truncate(ino, new_size).map_err(fs_err_to_errno)
    }

    /// Write `data` to regular file `ino` starting at `offset`.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(errno::EISDIR);
        }
        if !is_reg(u32::from(inode.mode)) {
            return Err(errno::EINVAL);
        }
        self.inner
            .write_data(ino, offset, data)
            .map_err(fs_err_to_errno)
    }

    /// Read up to `buf.len()` bytes from regular file `ino` starting at `offset`.
    pub fn read(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(errno::EISDIR);
        }
        if !is_reg(u32::from(inode.mode)) {
            return Err(errno::EINVAL);
        }
        self.inner
            .read_data(ino, offset, buf)
            .map_err(fs_err_to_errno)
    }

    /// Query POSIX [`Stat`] metadata for `ino`.
    pub fn stat(&self, dev_id: u64, ino: u64) -> Result<Stat, i32> {
        let inode = self.read_inode(ino)?;
        let mode = u32::from(inode.mode);
        let size = Volume::<D>::file_size(&inode) as i64;
        Ok(Stat {
            st_dev: dev_id,
            st_ino: ino,
            st_mode: mode,
            st_nlink: inode.nlink.max(1),
            st_uid: inode.owner,
            st_gid: inode.group,
            st_size: size,
            st_blksize: self.info.layout.block_size as i64,
            st_blocks: (size + 511) / 512,
            ..Stat::default()
        })
    }

    /// Names of the volume's snapshots, oldest first.
    #[must_use]
    pub fn snapshot_names(&self) -> Vec<String> {
        self.inner
            .snapshots()
            .iter()
            .map(|s| String::from(s.name.as_str()))
            .collect()
    }

    /// Create a snapshot of the volume's current committed state.
    pub fn create_snapshot(&mut self, name: &str) -> Result<u64, i32> {
        self.inner.create_snapshot(name).map_err(fs_err_to_errno)
    }

    /// Delete a snapshot, releasing the blocks only it referenced.
    pub fn delete_snapshot(&mut self, name: &str) -> Result<(), i32> {
        self.inner.delete_snapshot(name).map_err(fs_err_to_errno)
    }
}

fn validate_name(name: &str) -> Result<(), i32> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(errno::EINVAL);
    }
    if name.len() > NAME_MAX {
        return Err(errno::ENAMETOOLONG);
    }
    Ok(())
}

/// Translate a filesystem error into the kernel's internal errno convention.
pub const fn fs_err_to_errno(err: FsError) -> i32 {
    match err {
        FsError::NotFound => errno::ENOENT,
        FsError::AlreadyExists => errno::EEXIST,
        FsError::NotDirectory => errno::ENOTDIR,
        FsError::IsDirectory => errno::EISDIR,
        FsError::NotEmpty => errno::ENOTEMPTY,
        FsError::NameTooLong => errno::ENAMETOOLONG,
        FsError::ReadOnly => errno::EROFS,
        FsError::InvalidField(_) => errno::EINVAL,
        FsError::BlockOutOfRange(_) => errno::ENOSPC,
        _ => errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mount_create_write_unmount_and_remount_persists_on_disk() {
        let mut vol = NolxfsVolume::format_and_mount(4096, 96, "root").unwrap();
        let etc = vol.mkdir(NOLXFS_ROOT_INO, "etc", 0o755, 0, 0).unwrap();
        let (f_ino, created) = vol.create_file(etc, "hostname", 0o644, 0, 0, true).unwrap();
        assert!(created);
        assert_eq!(vol.write(f_ino, 0, b"nolx-host\n"), Ok(10));
        vol.sync().unwrap();

        // Unmount back to raw block device, then mount fresh and verify contents.
        let dev = vol.into_device();
        let remounted = NolxfsVolume::mount(dev).unwrap();
        let etc2 = remounted.lookup(NOLXFS_ROOT_INO, "etc").unwrap();
        let f2 = remounted.lookup(etc2, "hostname").unwrap();
        assert_eq!(f2, f_ino);

        let mut buf = [0u8; 16];
        let n = remounted.read(f2, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"nolx-host\n");
    }

    #[test]
    fn links_symlinks_rename_and_snapshots_through_the_adapter() {
        let mut vol = NolxfsVolume::format_and_mount(4096, 256, "root").unwrap();
        let docs = vol.mkdir(NOLXFS_ROOT_INO, "docs", 0o755, 0, 0).unwrap();
        let (f, _) = vol.create_file(docs, "a.txt", 0o644, 0, 0, true).unwrap();
        vol.write(f, 0, b"adapter-data").unwrap();
        vol.link(f, NOLXFS_ROOT_INO, "b.txt").unwrap();
        let lnk = vol
            .symlink(NOLXFS_ROOT_INO, "lnk", b"/docs/a.txt", 0, 0)
            .unwrap();
        assert_eq!(vol.readlink(lnk).unwrap(), b"/docs/a.txt");
        vol.rename(docs, "a.txt", NOLXFS_ROOT_INO, "c.txt").unwrap();
        assert_eq!(vol.lookup(NOLXFS_ROOT_INO, "c.txt").unwrap(), f);
        vol.sync().unwrap();
        vol.create_snapshot("adapter-snap").unwrap();
        assert_eq!(vol.snapshot_names(), vec![String::from("adapter-snap")]);
        vol.unlink(NOLXFS_ROOT_INO, "b.txt").unwrap();
        vol.sync().unwrap();

        let dev = vol.into_device();
        let snap = NolxfsVolume::mount_snapshot(dev.clone(), "adapter-snap").unwrap();
        let sf = snap.lookup(NOLXFS_ROOT_INO, "b.txt").unwrap();
        let mut buf = [0u8; 16];
        let n = snap.read(sf, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"adapter-data");

        let mut live = NolxfsVolume::mount(dev).unwrap();
        assert!(live.lookup(NOLXFS_ROOT_INO, "b.txt").is_err());
        live.delete_snapshot("adapter-snap").unwrap();
        assert!(live.snapshot_names().is_empty());
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use nolxfs::blockdev::MemBlockDevice;
    use nolxfs::volume::Volume;

    #[test]
    fn diag_sparse_remount_lookup() {
        let mut vol = NolxfsVolume::format_and_mount(4096, 96, "root").unwrap();
        let etc = vol.mkdir(NOLXFS_ROOT_INO, "etc", 0o755, 0, 0).unwrap();
        assert!(
            vol.lookup(NOLXFS_ROOT_INO, "etc").is_ok(),
            "pre-commit lookup failed; readdir={:?}",
            vol.readdir(NOLXFS_ROOT_INO)
        );
        let (f, _) = vol.create_file(etc, "hostname", 0o644, 0, 0, true).unwrap();
        vol.write(f, 0, b"nolx-host\n").unwrap();
        vol.sync().unwrap();
        assert!(
            vol.lookup(NOLXFS_ROOT_INO, "etc").is_ok(),
            "post-commit lookup failed; readdir={:?}",
            vol.readdir(NOLXFS_ROOT_INO)
        );
        let dev = vol.into_device();
        let mut re = NolxfsVolume::mount(dev).unwrap();
        assert!(
            re.lookup(NOLXFS_ROOT_INO, "etc").is_ok(),
            "post-remount lookup failed; xid={} readdir={:?}",
            re.volume().xid(),
            re.readdir(NOLXFS_ROOT_INO)
        );
    }

    #[test]
    fn diag_mem_remount_lookup() {
        let dev = MemBlockDevice::new(4096, 96);
        let vol = Volume::format(
            dev,
            &FormatOptions {
                volume_name: String::from("root"),
                ..FormatOptions::default()
            },
        )
        .unwrap();
        let dev = vol.into_device();
        let mut vol = NolxfsVolume::mount(dev).unwrap();
        let etc = vol.mkdir(NOLXFS_ROOT_INO, "etc", 0o755, 0, 0).unwrap();
        let (f, _) = vol.create_file(etc, "hostname", 0o644, 0, 0, true).unwrap();
        vol.write(f, 0, b"nolx-host\n").unwrap();
        vol.sync().unwrap();
        let dev = vol.into_device();
        let mut re = NolxfsVolume::mount(dev).unwrap();
        assert!(
            re.lookup(NOLXFS_ROOT_INO, "etc").is_ok(),
            "mem post-remount lookup failed; xid={} readdir={:?}",
            re.volume().xid(),
            re.readdir(NOLXFS_ROOT_INO)
        );
        let _ = f;
    }
}
