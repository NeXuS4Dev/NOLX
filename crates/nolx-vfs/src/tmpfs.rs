//! `tmpfs`: bounded in-memory POSIX filesystem.
//!
//! `tmpfs` stores directories, regular files, and symbolic links in memory with configurable
//! inode and byte quotas (`max_inodes`, `max_bytes`). It implements full POSIX directory and
//! link lifetime semantics:
//!
//! * **Deferred reclamation on unlink (`UN-001`)**: `unlink` removes the directory entry and
//!   decrements `nlink` immediately, so subsequent path lookups return `ENOENT`, while the
//!   inode's data remains readable and writable through any open file object (`open_refs > 0`)
//!   until the final [`Tmpfs::close_ref`] drops `open_refs` to zero.
//! * **No hard links to directories (`UN-002`)**: `link` refuses directory targets with
//!   `EPERM`.
//! * **Loop-free rename (`UN-003`)**: `rename` walks the destination parent chain and rejects
//!   moving a directory into itself or any of its descendants with `EINVAL`.
//! * **Non-empty `rmdir` rejection (`UN-004`)**: `rmdir` refuses a directory that still holds
//!   children with `ENOTEMPTY`.
//! * **Exclusive creation (`UN-005`)**: `mkdir`, `symlink`, `link`, and `create(..., excl=true)`
//!   fail with `EEXIST` when the target entry already exists.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::errno;
use nolx_types::scalars::{
    is_dir, is_lnk, is_reg, Gid, Mode, Stat, Uid, NAME_MAX, S_IFDIR, S_IFLNK, S_IFREG,
};

use crate::cache::CacheBacking;
use crate::io::IoStatus;
use crate::mount::NodeId;

/// Inode number of the root directory in every `Tmpfs` instance.
pub const TMPFS_ROOT_INO: u64 = 1;

/// Default maximum inodes for a `Tmpfs` volume.
pub const DEFAULT_MAX_INODES: usize = 4096;

/// Default maximum data bytes across all regular files and symlinks in a `Tmpfs` volume (16 MiB).
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Payload of a `Tmpfs` inode.
#[derive(Clone, Debug)]
pub enum TmpfsPayload {
    /// Regular file byte vector.
    Regular(Vec<u8>),
    /// Directory map of entry name -> child inode number.
    Directory {
        /// Parent directory inode (`TMPFS_ROOT_INO` for root).
        parent: u64,
        /// Child entries in lexicographical order.
        entries: BTreeMap<String, u64>,
    },
    /// Symbolic link target bytes.
    Symlink(Vec<u8>),
}

/// One inode in `Tmpfs`.
#[derive(Clone, Debug)]
pub struct TmpfsInode {
    /// Inode number.
    pub ino: u64,
    /// File type + permission bits.
    pub mode: Mode,
    /// Hard link count (`st_nlink`).
    pub nlink: u64,
    /// Open file-object reference count (`UN-001` lifetime pin).
    pub open_refs: u32,
    /// Owner user id.
    pub uid: Uid,
    /// Owner group id.
    pub gid: Gid,
    /// Payload (regular bytes, directory entries, or symlink target).
    pub payload: TmpfsPayload,
}

impl TmpfsInode {
    /// Size in bytes reported by `stat`.
    #[must_use]
    pub fn size(&self) -> u64 {
        match &self.payload {
            TmpfsPayload::Regular(bytes) | TmpfsPayload::Symlink(bytes) => bytes.len() as u64,
            TmpfsPayload::Directory { entries, .. } => entries.len() as u64,
        }
    }
}

/// An in-memory POSIX filesystem instance.
#[derive(Clone, Debug)]
pub struct Tmpfs {
    inodes: BTreeMap<u64, TmpfsInode>,
    next_ino: u64,
    max_inodes: usize,
    max_bytes: usize,
    used_bytes: usize,
}

impl Default for Tmpfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Tmpfs {
    /// Create a new `Tmpfs` with default quotas and an empty root directory (`ino == 1`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_INODES, DEFAULT_MAX_BYTES)
    }

    /// Create a new `Tmpfs` with explicit inode and byte quotas.
    #[must_use]
    pub fn with_limits(max_inodes: usize, max_bytes: usize) -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            TMPFS_ROOT_INO,
            TmpfsInode {
                ino: TMPFS_ROOT_INO,
                mode: S_IFDIR | 0o755,
                nlink: 2,
                open_refs: 0,
                uid: 0,
                gid: 0,
                payload: TmpfsPayload::Directory {
                    parent: TMPFS_ROOT_INO,
                    entries: BTreeMap::new(),
                },
            },
        );
        Self {
            inodes,
            next_ino: 2,
            max_inodes: max_inodes.max(1),
            max_bytes,
            used_bytes: 0,
        }
    }

    /// Total bytes currently stored across regular files and symlinks.
    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Number of allocated inodes (including unlinked-but-open inodes).
    #[must_use]
    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    /// Inspect an inode by number.
    #[must_use]
    pub fn inode(&self, ino: u64) -> Option<&TmpfsInode> {
        self.inodes.get(&ino)
    }

    /// Parent directory inode for `dir_ino`.
    #[must_use]
    pub fn parent_of(&self, dir_ino: u64) -> Option<u64> {
        match &self.inodes.get(&dir_ino)?.payload {
            TmpfsPayload::Directory { parent, .. } => Some(*parent),
            _ => None,
        }
    }

    /// Look up `name` inside `dir_ino`.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64, i32> {
        let dir = self.inodes.get(&dir_ino).ok_or(errno::ENOENT)?;
        let TmpfsPayload::Directory { entries, .. } = &dir.payload else {
            return Err(errno::ENOTDIR);
        };
        entries.get(name).copied().ok_or(errno::ENOENT)
    }

    fn alloc_ino(&mut self) -> Result<u64, i32> {
        if self.inodes.len() >= self.max_inodes {
            return Err(errno::ENOSPC);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        Ok(ino)
    }

    /// Increment the open file-object count on `ino` (`UN-001` lifetime pin).
    pub fn open_ref(&mut self, ino: u64) -> Result<(), i32> {
        let node = self.inodes.get_mut(&ino).ok_or(errno::ENOENT)?;
        node.open_refs = node.open_refs.saturating_add(1);
        Ok(())
    }

    /// Decrement the open file-object count on `ino`; if `nlink == 0 && open_refs == 0`,
    /// reclaim the inode and its data bytes (`UN-001`).
    pub fn close_ref(&mut self, ino: u64) -> Result<bool, i32> {
        let should_reclaim = {
            let node = self.inodes.get_mut(&ino).ok_or(errno::ENOENT)?;
            node.open_refs = node.open_refs.saturating_sub(1);
            node.nlink == 0 && node.open_refs == 0
        };
        if should_reclaim {
            self.reclaim_inode(ino);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn reclaim_inode(&mut self, ino: u64) {
        if let Some(removed) = self.inodes.remove(&ino) {
            match removed.payload {
                TmpfsPayload::Regular(b) | TmpfsPayload::Symlink(b) => {
                    self.used_bytes = self.used_bytes.saturating_sub(b.len());
                }
                TmpfsPayload::Directory { .. } => {}
            }
        }
    }

    /// Create or open a regular file named `name` inside `dir_ino`.
    ///
    /// If `name` already exists:
    /// * returns `Err(EEXIST)` when `exclusive` is `true` (`O_CREAT | O_EXCL`);
    /// * returns `Err(EISDIR)` if the existing node is a directory;
    /// * returns `Ok((ino, false))` otherwise.
    pub fn create_file(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
        exclusive: bool,
    ) -> Result<(u64, bool), i32> {
        validate_entry_name(name)?;
        if let Some(&existing) = self.dir_entries(dir_ino)?.get(name) {
            if exclusive {
                return Err(errno::EEXIST);
            }
            let existing_mode = self.inodes.get(&existing).ok_or(errno::ENOENT)?.mode;
            if is_dir(existing_mode) {
                return Err(errno::EISDIR);
            }
            return Ok((existing, false));
        }

        let ino = self.alloc_ino()?;
        self.inodes.insert(
            ino,
            TmpfsInode {
                ino,
                mode: S_IFREG | (mode & 0o7777),
                nlink: 1,
                open_refs: 0,
                uid,
                gid,
                payload: TmpfsPayload::Regular(Vec::new()),
            },
        );
        self.dir_entries_mut(dir_ino)?
            .insert(String::from(name), ino);
        Ok((ino, true))
    }

    /// Create a subdirectory named `name` inside `dir_ino` (`mkdir(2)`).
    ///
    /// Fails with `EEXIST` if `name` already exists (`UN-005`).
    pub fn mkdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
    ) -> Result<u64, i32> {
        validate_entry_name(name)?;
        if self.dir_entries(dir_ino)?.contains_key(name) {
            return Err(errno::EEXIST);
        }
        let ino = self.alloc_ino()?;
        self.inodes.insert(
            ino,
            TmpfsInode {
                ino,
                mode: S_IFDIR | (mode & 0o7777),
                nlink: 2,
                open_refs: 0,
                uid,
                gid,
                payload: TmpfsPayload::Directory {
                    parent: dir_ino,
                    entries: BTreeMap::new(),
                },
            },
        );
        if let Some(parent) = self.inodes.get_mut(&dir_ino) {
            parent.nlink = parent.nlink.saturating_add(1);
            if let TmpfsPayload::Directory { entries, .. } = &mut parent.payload {
                entries.insert(String::from(name), ino);
            }
        }
        Ok(ino)
    }

    /// Remove an empty directory named `name` from `dir_ino` (`rmdir(2)`).
    ///
    /// Fails with `ENOTEMPTY` if the target directory still contains entries (`UN-004`),
    /// `ENOTDIR` if the target is not a directory, and `EINVAL` if `name` is `.` or `..`.
    pub fn rmdir(&mut self, dir_ino: u64, name: &str) -> Result<(), i32> {
        if name == "." || name == ".." {
            return Err(errno::EINVAL);
        }
        validate_entry_name(name)?;
        let child_ino = *self.dir_entries(dir_ino)?.get(name).ok_or(errno::ENOENT)?;
        if child_ino == TMPFS_ROOT_INO {
            return Err(errno::EBUSY);
        }
        let child = self.inodes.get(&child_ino).ok_or(errno::ENOENT)?;
        let TmpfsPayload::Directory { entries, .. } = &child.payload else {
            return Err(errno::ENOTDIR);
        };
        if !entries.is_empty() {
            return Err(errno::ENOTEMPTY);
        }

        self.dir_entries_mut(dir_ino)?.remove(name);
        if let Some(parent) = self.inodes.get_mut(&dir_ino) {
            parent.nlink = parent.nlink.saturating_sub(1);
        }
        if let Some(child_mut) = self.inodes.get_mut(&child_ino) {
            child_mut.nlink = 0;
            if child_mut.open_refs == 0 {
                self.inodes.remove(&child_ino);
            }
        }
        Ok(())
    }

    /// Create a hard link to `target_ino` at `(new_dir_ino, new_name)` (`link(2)`).
    ///
    /// Refuses directories with `EPERM` (`UN-002`) and existing names with `EEXIST`.
    pub fn link(&mut self, target_ino: u64, new_dir_ino: u64, new_name: &str) -> Result<(), i32> {
        validate_entry_name(new_name)?;
        if self.dir_entries(new_dir_ino)?.contains_key(new_name) {
            return Err(errno::EEXIST);
        }
        let target = self.inodes.get(&target_ino).ok_or(errno::ENOENT)?;
        if is_dir(target.mode) {
            return Err(errno::EPERM);
        }
        if target.nlink == 0 {
            return Err(errno::ENOENT);
        }
        self.dir_entries_mut(new_dir_ino)?
            .insert(String::from(new_name), target_ino);
        self.inodes.get_mut(&target_ino).unwrap().nlink += 1;
        Ok(())
    }

    /// Create a symbolic link at `(dir_ino, name)` pointing to `target` (`symlink(2)`).
    pub fn symlink(
        &mut self,
        dir_ino: u64,
        name: &str,
        target: &[u8],
        uid: Uid,
        gid: Gid,
    ) -> Result<u64, i32> {
        validate_entry_name(name)?;
        if target.is_empty() {
            return Err(errno::ENOENT);
        }
        if self.dir_entries(dir_ino)?.contains_key(name) {
            return Err(errno::EEXIST);
        }
        if self.used_bytes + target.len() > self.max_bytes {
            return Err(errno::ENOSPC);
        }
        let ino = self.alloc_ino()?;
        self.used_bytes += target.len();
        self.inodes.insert(
            ino,
            TmpfsInode {
                ino,
                mode: S_IFLNK | 0o777,
                nlink: 1,
                open_refs: 0,
                uid,
                gid,
                payload: TmpfsPayload::Symlink(target.to_vec()),
            },
        );
        self.dir_entries_mut(dir_ino)?
            .insert(String::from(name), ino);
        Ok(ino)
    }

    /// Read the target of symbolic link `ino` (`readlink(2)`).
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        if !is_lnk(node.mode) {
            return Err(errno::EINVAL);
        }
        match &node.payload {
            TmpfsPayload::Symlink(bytes) => Ok(bytes.clone()),
            _ => Err(errno::EINVAL),
        }
    }

    /// Unlink a non-directory entry `name` from `dir_ino` (`unlink(2)`).
    ///
    /// Removes the directory entry and decrements `nlink`. If `nlink == 0` and no open file
    /// object references the inode (`open_refs == 0`), the inode is freed immediately;
    /// otherwise it survives until the last [`Self::close_ref`] (`UN-001`).
    pub fn unlink(&mut self, dir_ino: u64, name: &str) -> Result<u64, i32> {
        validate_entry_name(name)?;
        let child_ino = *self.dir_entries(dir_ino)?.get(name).ok_or(errno::ENOENT)?;
        let child_mode = self.inodes.get(&child_ino).ok_or(errno::ENOENT)?.mode;
        if is_dir(child_mode) {
            return Err(errno::EPERM);
        }
        self.dir_entries_mut(dir_ino)?.remove(name);
        let should_reclaim = {
            let child = self.inodes.get_mut(&child_ino).unwrap();
            child.nlink = child.nlink.saturating_sub(1);
            child.nlink == 0 && child.open_refs == 0
        };
        if should_reclaim {
            self.reclaim_inode(child_ino);
        }
        Ok(child_ino)
    }

    /// Atomically rename `(old_dir_ino, old_name)` to `(new_dir_ino, new_name)` (`rename(2)`).
    ///
    /// Rejects moving a directory onto itself or a descendant of itself with `EINVAL`
    /// (`UN-003`), enforces directory/non-directory compatibility (`EISDIR`/`ENOTDIR`), and
    /// refuses replacing a non-empty directory (`ENOTEMPTY`).
    pub fn rename(
        &mut self,
        old_dir_ino: u64,
        old_name: &str,
        new_dir_ino: u64,
        new_name: &str,
    ) -> Result<(), i32> {
        validate_entry_name(old_name)?;
        validate_entry_name(new_name)?;

        let src_ino = *self
            .dir_entries(old_dir_ino)?
            .get(old_name)
            .ok_or(errno::ENOENT)?;
        let src_is_dir = is_dir(self.inodes.get(&src_ino).ok_or(errno::ENOENT)?.mode);

        // Check that `new_dir_ino` is not `src_ino` or a descendant of `src_ino` (UN-003).
        if src_is_dir {
            let mut cursor = new_dir_ino;
            loop {
                if cursor == src_ino {
                    return Err(errno::EINVAL);
                }
                if cursor == TMPFS_ROOT_INO {
                    break;
                }
                let Some(parent) = self.parent_of(cursor) else {
                    break;
                };
                if parent == cursor {
                    break;
                }
                cursor = parent;
            }
        }

        let dst_existing = self.dir_entries(new_dir_ino)?.get(new_name).copied();
        if let Some(dst_ino) = dst_existing {
            if dst_ino == src_ino {
                // POSIX: renaming a file to a hard link of the same inode is a no-op.
                return Ok(());
            }
            let dst = self.inodes.get(&dst_ino).ok_or(errno::ENOENT)?;
            let dst_is_dir = is_dir(dst.mode);
            if src_is_dir && !dst_is_dir {
                return Err(errno::ENOTDIR);
            }
            if !src_is_dir && dst_is_dir {
                return Err(errno::EISDIR);
            }
            if let TmpfsPayload::Directory { entries, .. } = &dst.payload {
                if !entries.is_empty() {
                    return Err(errno::ENOTEMPTY);
                }
            }
            // Remove displaced destination.
            self.dir_entries_mut(new_dir_ino)?.remove(new_name);
            if dst_is_dir {
                if let Some(new_parent) = self.inodes.get_mut(&new_dir_ino) {
                    new_parent.nlink = new_parent.nlink.saturating_sub(1);
                }
            }
            let reclaim_dst = {
                let d = self.inodes.get_mut(&dst_ino).unwrap();
                d.nlink = d.nlink.saturating_sub(1);
                d.nlink == 0 && d.open_refs == 0
            };
            if reclaim_dst {
                self.reclaim_inode(dst_ino);
            }
        }

        self.dir_entries_mut(old_dir_ino)?.remove(old_name);
        self.dir_entries_mut(new_dir_ino)?
            .insert(String::from(new_name), src_ino);

        if src_is_dir && old_dir_ino != new_dir_ino {
            if let Some(old_parent) = self.inodes.get_mut(&old_dir_ino) {
                old_parent.nlink = old_parent.nlink.saturating_sub(1);
            }
            if let Some(new_parent) = self.inodes.get_mut(&new_dir_ino) {
                new_parent.nlink = new_parent.nlink.saturating_add(1);
            }
            if let Some(src_node) = self.inodes.get_mut(&src_ino) {
                if let TmpfsPayload::Directory { parent, .. } = &mut src_node.payload {
                    *parent = new_dir_ino;
                }
            }
        }
        Ok(())
    }

    /// Read up to `buf.len()` bytes from regular file `ino` starting at `offset`.
    pub fn read(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        if is_dir(node.mode) {
            return Err(errno::EISDIR);
        }
        if !is_reg(node.mode) {
            return Err(errno::EINVAL);
        }
        let TmpfsPayload::Regular(bytes) = &node.payload else {
            return Err(errno::EINVAL);
        };
        let off = usize::try_from(offset).map_err(|_| errno::EINVAL)?;
        if off >= bytes.len() || buf.is_empty() {
            return Ok(0);
        }
        let n = (bytes.len() - off).min(buf.len());
        buf[..n].copy_from_slice(&bytes[off..off + n]);
        Ok(n)
    }

    /// Write `data` to regular file `ino` starting at `offset`, extending with zero-fill if
    /// `offset` lies past the current end of file.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        let off = usize::try_from(offset).map_err(|_| errno::EINVAL)?;
        let end = off.checked_add(data.len()).ok_or(errno::EOVERFLOW)?;
        let current_len = {
            let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
            if is_dir(node.mode) {
                return Err(errno::EISDIR);
            }
            if !is_reg(node.mode) {
                return Err(errno::EINVAL);
            }
            node.size() as usize
        };
        let growth = end.saturating_sub(current_len);
        if self.used_bytes + growth > self.max_bytes {
            return Err(errno::ENOSPC);
        }
        let node = self.inodes.get_mut(&ino).unwrap();
        let TmpfsPayload::Regular(bytes) = &mut node.payload else {
            return Err(errno::EINVAL);
        };
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[off..end].copy_from_slice(data);
        self.used_bytes += growth;
        Ok(data.len())
    }

    /// Truncate or extend regular file `ino` to `new_size` bytes (`truncate` / `ftruncate` / `O_TRUNC`).
    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<(), i32> {
        let target_len = usize::try_from(new_size).map_err(|_| errno::EOVERFLOW)?;
        let current_len = {
            let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
            if is_dir(node.mode) {
                return Err(errno::EISDIR);
            }
            if !is_reg(node.mode) {
                return Err(errno::EINVAL);
            }
            node.size() as usize
        };
        if target_len > current_len {
            let growth = target_len - current_len;
            if self.used_bytes + growth > self.max_bytes {
                return Err(errno::ENOSPC);
            }
            self.used_bytes += growth;
        } else {
            self.used_bytes = self.used_bytes.saturating_sub(current_len - target_len);
        }
        let node = self.inodes.get_mut(&ino).unwrap();
        if let TmpfsPayload::Regular(bytes) = &mut node.payload {
            bytes.resize(target_len, 0);
        }
        Ok(())
    }

    /// Change permission bits on `ino` (`chmod` / `fchmod`).
    pub fn chmod(&mut self, ino: u64, mode: Mode) -> Result<(), i32> {
        let node = self.inodes.get_mut(&ino).ok_or(errno::ENOENT)?;
        let kind = node.mode & nolx_types::scalars::S_IFMT;
        node.mode = kind | (mode & 0o7777);
        Ok(())
    }

    /// Query [`Stat`] for `ino`.
    pub fn stat(&self, dev_id: u64, ino: u64) -> Result<Stat, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        let size = node.size() as i64;
        Ok(Stat {
            st_dev: dev_id,
            st_ino: node.ino,
            st_mode: node.mode,
            st_nlink: node.nlink as u32,
            st_uid: node.uid,
            st_gid: node.gid,
            st_rdev: 0,
            st_size: size,
            st_blksize: 4096,
            st_blocks: (size + 511) / 512,
            ..Stat::default()
        })
    }

    /// Enumerate `(name, ino)` pairs inside directory `dir_ino`.
    pub fn readdir(&self, dir_ino: u64) -> Result<Vec<(String, u64)>, i32> {
        let entries = self.dir_entries(dir_ino)?;
        Ok(entries.iter().map(|(k, &v)| (k.clone(), v)).collect())
    }

    fn dir_entries(&self, dir_ino: u64) -> Result<&BTreeMap<String, u64>, i32> {
        let dir = self.inodes.get(&dir_ino).ok_or(errno::ENOENT)?;
        match &dir.payload {
            TmpfsPayload::Directory { entries, .. } => Ok(entries),
            _ => Err(errno::ENOTDIR),
        }
    }

    fn dir_entries_mut(&mut self, dir_ino: u64) -> Result<&mut BTreeMap<String, u64>, i32> {
        let dir = self.inodes.get_mut(&dir_ino).ok_or(errno::ENOENT)?;
        match &mut dir.payload {
            TmpfsPayload::Directory { entries, .. } => Ok(entries),
            _ => Err(errno::ENOTDIR),
        }
    }
}

impl CacheBacking for Tmpfs {
    fn paging_read(
        &mut self,
        node: NodeId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, IoStatus> {
        self.read(node.ino, offset, buf)
            .map_err(|_| IoStatus::DeviceError)
    }

    fn paging_write(&mut self, node: NodeId, offset: u64, data: &[u8]) -> Result<usize, IoStatus> {
        self.write(node.ino, offset, data).map_err(|e| {
            if e == errno::ENOSPC {
                IoStatus::DiskFull
            } else {
                IoStatus::DeviceError
            }
        })
    }
}

fn validate_entry_name(name: &str) -> Result<(), i32> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(errno::EINVAL);
    }
    if name.len() > NAME_MAX {
        return Err(errno::ENAMETOOLONG);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlinked_open_file_survives_until_close() {
        // UN-001: unlinking an open file removes the directory entry immediately while
        // keeping the inode's data accessible through the open file reference until close.
        let mut fs = Tmpfs::new();
        let (ino, created) = fs
            .create_file(TMPFS_ROOT_INO, "ephemeral", 0o644, 0, 0, true)
            .unwrap();
        assert!(created);
        fs.open_ref(ino).unwrap();
        fs.write(ino, 0, b"still alive after unlink").unwrap();

        // Unlink while open: directory lookup immediately fails with ENOENT.
        assert_eq!(fs.unlink(TMPFS_ROOT_INO, "ephemeral"), Ok(ino));
        assert_eq!(fs.lookup(TMPFS_ROOT_INO, "ephemeral"), Err(errno::ENOENT));

        // Open reference can still read and write the unlinked inode!
        let mut buf = [0u8; 24];
        assert_eq!(fs.read(ino, 0, &mut buf), Ok(24));
        assert_eq!(&buf, b"still alive after unlink");
        assert!(fs.used_bytes() > 0);

        // Closing the final open reference reclaims the inode and its bytes.
        assert_eq!(fs.close_ref(ino), Ok(true));
        assert_eq!(fs.used_bytes(), 0);
        assert_eq!(fs.read(ino, 0, &mut buf), Err(errno::ENOENT));
    }

    #[test]
    fn hard_link_to_directory_is_eperm() {
        // UN-002: link(2) on a directory is rejected with EPERM.
        let mut fs = Tmpfs::new();
        let sub = fs.mkdir(TMPFS_ROOT_INO, "dir", 0o755, 0, 0).unwrap();
        assert_eq!(fs.link(sub, TMPFS_ROOT_INO, "dir_alias"), Err(errno::EPERM));
    }

    #[test]
    fn rename_onto_descendant_is_einval() {
        // UN-003: renaming a directory into itself or a descendant fails with EINVAL.
        let mut fs = Tmpfs::new();
        let a = fs.mkdir(TMPFS_ROOT_INO, "a", 0o755, 0, 0).unwrap();
        let b = fs.mkdir(a, "b", 0o755, 0, 0).unwrap();
        assert_eq!(
            fs.rename(TMPFS_ROOT_INO, "a", b, "into_grandchild"),
            Err(errno::EINVAL)
        );
        assert_eq!(
            fs.rename(TMPFS_ROOT_INO, "a", a, "into_self"),
            Err(errno::EINVAL)
        );
    }

    #[test]
    fn rmdir_nonempty_is_enotempty() {
        // UN-004: rmdir(2) on a non-empty directory fails with ENOTEMPTY.
        let mut fs = Tmpfs::new();
        let dir = fs.mkdir(TMPFS_ROOT_INO, "work", 0o755, 0, 0).unwrap();
        fs.create_file(dir, "child.txt", 0o644, 0, 0, true).unwrap();
        assert_eq!(fs.rmdir(TMPFS_ROOT_INO, "work"), Err(errno::ENOTEMPTY));

        // Once the child is unlinked, rmdir succeeds.
        fs.unlink(dir, "child.txt").unwrap();
        assert_eq!(fs.rmdir(TMPFS_ROOT_INO, "work"), Ok(()));
    }

    #[test]
    fn mkdir_existing_is_eexist() {
        // UN-005: mkdir(2) on an existing entry fails with EEXIST.
        let mut fs = Tmpfs::new();
        fs.mkdir(TMPFS_ROOT_INO, "dup", 0o755, 0, 0).unwrap();
        assert_eq!(
            fs.mkdir(TMPFS_ROOT_INO, "dup", 0o755, 0, 0),
            Err(errno::EEXIST)
        );
        fs.create_file(TMPFS_ROOT_INO, "f", 0o644, 0, 0, true)
            .unwrap();
        assert_eq!(
            fs.mkdir(TMPFS_ROOT_INO, "f", 0o755, 0, 0),
            Err(errno::EEXIST)
        );
    }

    #[test]
    fn hard_link_and_atomic_rename_preserve_data_and_link_counts() {
        let mut fs = Tmpfs::new();
        let (ino, _) = fs
            .create_file(TMPFS_ROOT_INO, "orig", 0o644, 1000, 1000, true)
            .unwrap();
        fs.write(ino, 0, b"payload").unwrap();
        fs.link(ino, TMPFS_ROOT_INO, "hard").unwrap();
        assert_eq!(fs.stat(1, ino).unwrap().st_nlink, 2);

        fs.unlink(TMPFS_ROOT_INO, "orig").unwrap();
        assert_eq!(fs.stat(1, ino).unwrap().st_nlink, 1);

        fs.rename(TMPFS_ROOT_INO, "hard", TMPFS_ROOT_INO, "renamed")
            .unwrap();
        let found = fs.lookup(TMPFS_ROOT_INO, "renamed").unwrap();
        assert_eq!(found, ino);
        let mut out = [0u8; 7];
        assert_eq!(fs.read(found, 0, &mut out), Ok(7));
        assert_eq!(&out, b"payload");
    }
}
