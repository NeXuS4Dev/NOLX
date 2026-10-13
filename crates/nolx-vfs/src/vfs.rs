//! The unified VFS layer: file objects, process VFS context (`umask`, `cwd`, `chroot`), and
//! multi-filesystem dispatch (`tmpfs`, `devfs`, `initrd`, `nolxfs`).
//!
//! In NOLX (`docs/architecture/02-posix-subsystem.md` and `04-io-manager-drivers.md`), an open
//! file description is an executive **FileObject** referenced by one or more slots in a
//! process's [`DescriptorTable`]. Duplicating a descriptor (`dup`, `dup2`, `fcntl(F_DUPFD)`)
//! creates a new descriptor slot pointing to the *same* [`FileObject`], sharing its byte
//! offset and open status flags while keeping `FD_CLOEXEC` per-descriptor.

use core::cell::Cell;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::abi::{fcntl_cmd, open_flags, seek};
use nolx_types::errno;
use nolx_types::scalars::{
    is_dir, is_lnk, is_reg, Gid, Mode, Off, Stat, Uid, NAME_MAX, PATH_MAX, S_IXGRP, S_IXOTH,
    S_IXUSR,
};

use crate::devfs::{Devfs, DEVFS_ROOT_INO};
use crate::fd::{Descriptor, DescriptorTable};
use crate::initrd::{InitrdFs, INITRD_ROOT_INO};
use crate::mount::{MountTable, NodeId};
use crate::nolxfs_vfs::{NolxfsVolume, NOLXFS_ROOT_INO};
use crate::path::{self, pathconf_value, Final, Resolution, Resolver};
use crate::tmpfs::{Tmpfs, TMPFS_ROOT_INO};

/// Concrete filesystem instance mounted in a [`VirtualFileSystem`].
#[derive(Debug)]
pub enum FsInstance {
    /// Bounded in-memory POSIX filesystem (`tmpfs`).
    Tmpfs(Tmpfs),
    /// Device node filesystem (`devfs`).
    Devfs(Devfs),
    /// Read-only initial ramdisk (`cpio` or `tar`).
    Initrd(InitrdFs),
    /// Native NOLXFS volume over a block device.
    Nolxfs(NolxfsVolume),
}

impl FsInstance {
    /// Root inode number of this filesystem instance.
    #[must_use]
    pub const fn root_ino(&self) -> u64 {
        match self {
            Self::Tmpfs(_) => TMPFS_ROOT_INO,
            Self::Devfs(_) => DEVFS_ROOT_INO,
            Self::Initrd(_) => INITRD_ROOT_INO,
            Self::Nolxfs(_) => NOLXFS_ROOT_INO,
        }
    }

    /// Short filesystem type name (`"tmpfs"`, `"devfs"`, `"initrd"`, `"nolxfs"`).
    #[must_use]
    pub const fn fstype(&self) -> &'static str {
        match self {
            Self::Tmpfs(_) => "tmpfs",
            Self::Devfs(_) => "devfs",
            Self::Initrd(_) => "initrd",
            Self::Nolxfs(_) => "nolxfs",
        }
    }
}

/// Executive `FileObject`: the open file description shared across `dup`/`fork`.
#[derive(Clone, Debug)]
pub struct FileObject {
    /// Unique identifier in [`VirtualFileSystem`].
    pub id: u64,
    /// Underlying `(mount, ino)` identity.
    pub node: NodeId,
    /// Open status and access mode flags (`open_flags::*`).
    pub flags: u32,
    /// Current byte offset for sequential `read` / `write` / `lseek`.
    pub offset: u64,
    /// Number of open descriptors referencing this `FileObject`.
    pub ref_count: u32,
}

/// Per-process VFS state: root directory (`chroot`), current working directory, `umask`,
/// credentials, and file descriptor table.
#[derive(Clone, Debug)]
pub struct ProcessVfsContext {
    /// Root directory visible to this process (`chroot` boundary).
    pub root: NodeId,
    /// Current working directory (`chdir`).
    pub cwd: NodeId,
    /// File mode creation mask (`umask`, default `0o022`).
    pub umask: Mode,
    /// Effective user id.
    pub uid: Uid,
    /// Effective group id.
    pub gid: Gid,
    /// Open file descriptor table.
    pub fds: DescriptorTable,
}

impl ProcessVfsContext {
    /// Create a process VFS context rooted at `root` with default `umask = 0o022`.
    #[must_use]
    pub fn new(root: NodeId) -> Self {
        Self {
            root,
            cwd: root,
            umask: 0o022,
            uid: 0,
            gid: 0,
            fds: DescriptorTable::default(),
        }
    }

    /// Set the file creation mask (`umask(2)`) and return the previous mask.
    pub fn set_umask(&mut self, mask: Mode) -> Mode {
        let prev = self.umask;
        self.umask = mask & 0o777;
        prev
    }
}

/// The unified Virtual File System.
#[derive(Default)]
pub struct VirtualFileSystem {
    mounts: MountTable,
    filesystems: BTreeMap<u32, FsInstance>,
    open_files: BTreeMap<u64, FileObject>,
    next_mount_num: u32,
    next_file_id: u64,
}

impl VirtualFileSystem {
    /// Construct an empty [`VirtualFileSystem`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mounts: MountTable::new(),
            filesystems: BTreeMap::new(),
            open_files: BTreeMap::new(),
            next_mount_num: 1,
            next_file_id: 1,
        }
    }

    /// Immutable reference to the mount table.
    #[must_use]
    pub const fn mount_table(&self) -> &MountTable {
        &self.mounts
    }

    /// Mutable reference to the mount table.
    pub fn mount_table_mut(&mut self) -> &mut MountTable {
        &mut self.mounts
    }

    /// Mount `fs` as the root filesystem (`/`), returning its root [`NodeId`].
    pub fn mount_root(&mut self, fs: FsInstance, source: &str) -> Result<NodeId, i32> {
        let mount_num = self.next_mount_num;
        self.next_mount_num += 1;
        let root_node = NodeId::new(mount_num, fs.root_ino());
        let fstype = String::from(fs.fstype());
        self.mounts
            .mount_root(root_node, String::from(source), fstype)?;
        self.filesystems.insert(mount_num, fs);
        Ok(root_node)
    }

    /// Mount `fs` at `path` (resolved relative to `ctx`), returning the mounted root [`NodeId`].
    pub fn mount_at(
        &mut self,
        ctx: &ProcessVfsContext,
        path: &[u8],
        fs: FsInstance,
        mflags: u64,
        source: &str,
    ) -> Result<NodeId, i32> {
        let covered = self.resolve_path(ctx, path, Final::Follow)?.node;
        if !self.node_is_dir(covered) {
            return Err(errno::ENOTDIR);
        }
        let mount_num = self.next_mount_num;
        self.next_mount_num += 1;
        let root_node = NodeId::new(mount_num, fs.root_ino());
        let fstype = String::from(fs.fstype());
        self.mounts
            .mount_over(covered, root_node, mflags, String::from(source), fstype)?;
        self.filesystems.insert(mount_num, fs);
        Ok(root_node)
    }

    /// Resolve `path` according to `ctx` (`root`, `cwd`, permissions) and `final_mode`.
    pub fn resolve_path(
        &self,
        ctx: &ProcessVfsContext,
        path: &[u8],
        final_mode: Final,
    ) -> Result<Resolution, i32> {
        let view = VfsResolver {
            vfs: self,
            ctx,
            symlinks: Cell::new(0),
        };
        path::resolve(&view, path, final_mode)
    }

    /// Resolve the parent directory of `path` and return `(parent_dir_node, final_entry_name)`.
    pub fn resolve_parent<'a>(
        &self,
        ctx: &ProcessVfsContext,
        path: &'a [u8],
    ) -> Result<(NodeId, &'a str), i32> {
        if path.is_empty() {
            return Err(errno::ENOENT);
        }
        if path.len() >= PATH_MAX {
            return Err(errno::ENAMETOOLONG);
        }
        let trimmed = strip_trailing_slashes(path);
        if trimmed.is_empty() {
            // Path was all slashes ("/") -> cannot create/unlink/rename root itself.
            return Err(errno::EBUSY);
        }
        let (dir_node, leaf_bytes) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(0) => {
                let r = self.resolve_path(ctx, b"/", Final::Follow)?;
                (r.node, &trimmed[1..])
            }
            Some(pos) => {
                let parent_slice = &trimmed[..pos];
                let r = self.resolve_path(ctx, parent_slice, Final::Follow)?;
                (r.node, &trimmed[pos + 1..])
            }
            None => (ctx.cwd, trimmed),
        };
        if !self.node_is_dir(dir_node) {
            return Err(errno::ENOTDIR);
        }
        self.check_search_permission(ctx, dir_node)?;
        let leaf = core::str::from_utf8(leaf_bytes).map_err(|_| errno::EINVAL)?;
        if leaf.len() > NAME_MAX {
            return Err(errno::ENAMETOOLONG);
        }
        Ok((dir_node, leaf))
    }

    /// Change the process's current working directory (`chdir(2)`).
    pub fn chdir(&self, ctx: &mut ProcessVfsContext, path: &[u8]) -> Result<(), i32> {
        let resolved = self.resolve_path(ctx, path, Final::Follow)?;
        if !self.node_is_dir(resolved.node) {
            return Err(errno::ENOTDIR);
        }
        self.check_search_permission(ctx, resolved.node)?;
        ctx.cwd = resolved.node;
        Ok(())
    }

    /// Change the process's root directory (`chroot(2)`).
    pub fn chroot(&self, ctx: &mut ProcessVfsContext, path: &[u8]) -> Result<(), i32> {
        if ctx.uid != 0 {
            return Err(errno::EPERM);
        }
        let resolved = self.resolve_path(ctx, path, Final::Follow)?;
        if !self.node_is_dir(resolved.node) {
            return Err(errno::ENOTDIR);
        }
        ctx.root = resolved.node;
        ctx.cwd = resolved.node;
        Ok(())
    }

    /// Evaluate `pathconf(path, name)`.
    pub fn pathconf(&self, ctx: &ProcessVfsContext, path: &[u8], name: i32) -> Result<i64, i32> {
        let _resolved = self.resolve_path(ctx, path, Final::Follow)?;
        pathconf_value(name)
    }

    /// Evaluate `fpathconf(fd, name)`.
    pub fn fpathconf(&self, ctx: &ProcessVfsContext, fd: usize, name: i32) -> Result<i64, i32> {
        let _desc = ctx.fds.get(fd)?;
        pathconf_value(name)
    }

    /// `stat(path)` or `lstat(path)` depending on `follow`.
    pub fn stat(&self, ctx: &ProcessVfsContext, path: &[u8], follow: Final) -> Result<Stat, i32> {
        let resolved = self.resolve_path(ctx, path, follow)?;
        self.stat_node(resolved.node)
    }

    /// `fstat(fd)`.
    pub fn fstat(&self, ctx: &ProcessVfsContext, fd: usize) -> Result<Stat, i32> {
        let desc = ctx.fds.get(fd)?;
        let fobj = self.open_files.get(&desc.file).ok_or(errno::EBADF)?;
        self.stat_node(fobj.node)
    }

    /// Query [`Stat`] directly by [`NodeId`].
    pub fn stat_node(&self, node: NodeId) -> Result<Stat, i32> {
        let fs = self.filesystems.get(&node.mount).ok_or(errno::ENOENT)?;
        let dev_id = u64::from(node.mount);
        match fs {
            FsInstance::Tmpfs(t) => t.stat(dev_id, node.ino),
            FsInstance::Devfs(d) => d.stat(dev_id, node.ino).map_err(|s| s.to_errno()),
            FsInstance::Initrd(i) => i.stat(dev_id, node.ino),
            FsInstance::Nolxfs(n) => n.stat(dev_id, node.ino),
        }
    }

    /// Create a directory at `path` (`mkdir(2)`), applying `!ctx.umask`.
    pub fn mkdir(
        &mut self,
        ctx: &ProcessVfsContext,
        path: &[u8],
        mode: Mode,
    ) -> Result<NodeId, i32> {
        let (parent, name) = self.resolve_parent(ctx, path)?;
        let effective_mode = (mode & 0o7777) & !ctx.umask;
        let fs = self
            .filesystems
            .get_mut(&parent.mount)
            .ok_or(errno::ENOENT)?;
        let ino = match fs {
            FsInstance::Tmpfs(t) => t.mkdir(parent.ino, name, effective_mode, ctx.uid, ctx.gid)?,
            FsInstance::Nolxfs(n) => n.mkdir(parent.ino, name, effective_mode, ctx.uid, ctx.gid)?,
            FsInstance::Initrd(_) => return Err(errno::EROFS),
            FsInstance::Devfs(_) => return Err(errno::EPERM),
        };
        Ok(NodeId::new(parent.mount, ino))
    }

    /// Remove an empty directory at `path` (`rmdir(2)`).
    pub fn rmdir(&mut self, ctx: &ProcessVfsContext, path: &[u8]) -> Result<(), i32> {
        let (parent, name) = self.resolve_parent(ctx, path)?;
        // Check if the target directory is currently a mount point.
        if let Ok(child) = self.lookup_in_dir(parent, name.as_bytes()) {
            if self.mounts.is_mount_point(child) || child == ctx.root {
                return Err(errno::EBUSY);
            }
        }
        let fs = self
            .filesystems
            .get_mut(&parent.mount)
            .ok_or(errno::ENOENT)?;
        match fs {
            FsInstance::Tmpfs(t) => t.rmdir(parent.ino, name),
            FsInstance::Nolxfs(n) => n.rmdir(parent.ino, name),
            FsInstance::Initrd(_) => Err(errno::EROFS),
            FsInstance::Devfs(_) => Err(errno::ENOTSUP),
        }
    }

    /// Create a hard link from `old_path` to `new_path` (`link(2)`).
    pub fn link(
        &mut self,
        ctx: &ProcessVfsContext,
        old_path: &[u8],
        new_path: &[u8],
    ) -> Result<(), i32> {
        let target = self.resolve_path(ctx, old_path, Final::NoFollow)?.node;
        let (new_dir, new_name) = self.resolve_parent(ctx, new_path)?;
        if target.mount != new_dir.mount {
            return Err(errno::EXDEV);
        }
        let fs = self
            .filesystems
            .get_mut(&target.mount)
            .ok_or(errno::ENOENT)?;
        match fs {
            FsInstance::Tmpfs(t) => t.link(target.ino, new_dir.ino, new_name),
            FsInstance::Nolxfs(n) => n.link(target.ino, new_dir.ino, new_name),
            FsInstance::Initrd(_) => Err(errno::EROFS),
            FsInstance::Devfs(_) => Err(errno::ENOTSUP),
        }
    }

    /// Create a symbolic link at `link_path` pointing to `target` (`symlink(2)`).
    pub fn symlink(
        &mut self,
        ctx: &ProcessVfsContext,
        target: &[u8],
        link_path: &[u8],
    ) -> Result<NodeId, i32> {
        let (dir, name) = self.resolve_parent(ctx, link_path)?;
        let fs = self.filesystems.get_mut(&dir.mount).ok_or(errno::ENOENT)?;
        let ino = match fs {
            FsInstance::Tmpfs(t) => t.symlink(dir.ino, name, target, ctx.uid, ctx.gid)?,
            FsInstance::Nolxfs(n) => n.symlink(dir.ino, name, target, ctx.uid, ctx.gid)?,
            FsInstance::Initrd(_) => return Err(errno::EROFS),
            FsInstance::Devfs(_) => return Err(errno::ENOTSUP),
        };
        Ok(NodeId::new(dir.mount, ino))
    }

    /// Read the target of a symbolic link at `path` (`readlink(2)`).
    pub fn readlink(&self, ctx: &ProcessVfsContext, path: &[u8]) -> Result<Vec<u8>, i32> {
        let resolved = self.resolve_path(ctx, path, Final::NoFollow)?;
        self.read_symlink_node(resolved.node)
    }

    /// Unlink a non-directory entry at `path` (`unlink(2)`).
    pub fn unlink(&mut self, ctx: &ProcessVfsContext, path: &[u8]) -> Result<(), i32> {
        if path.ends_with(b"/") {
            return Err(errno::EISDIR);
        }
        let (dir, name) = self.resolve_parent(ctx, path)?;
        let fs = self.filesystems.get_mut(&dir.mount).ok_or(errno::ENOENT)?;
        match fs {
            FsInstance::Tmpfs(t) => {
                t.unlink(dir.ino, name)?;
                Ok(())
            }
            FsInstance::Nolxfs(n) => n.unlink(dir.ino, name),
            FsInstance::Initrd(_) => Err(errno::EROFS),
            FsInstance::Devfs(_) => Err(errno::ENOTSUP),
        }
    }

    /// Rename `old_path` to `new_path` (`rename(2)`).
    pub fn rename(
        &mut self,
        ctx: &ProcessVfsContext,
        old_path: &[u8],
        new_path: &[u8],
    ) -> Result<(), i32> {
        let (old_dir, old_name) = self.resolve_parent(ctx, old_path)?;
        let (new_dir, new_name) = self.resolve_parent(ctx, new_path)?;
        if old_dir.mount != new_dir.mount {
            return Err(errno::EXDEV);
        }
        let fs = self
            .filesystems
            .get_mut(&old_dir.mount)
            .ok_or(errno::ENOENT)?;
        match fs {
            FsInstance::Tmpfs(t) => t.rename(old_dir.ino, old_name, new_dir.ino, new_name),
            FsInstance::Nolxfs(n) => n.rename(old_dir.ino, old_name, new_dir.ino, new_name),
            FsInstance::Initrd(_) => Err(errno::EROFS),
            FsInstance::Devfs(_) => Err(errno::ENOTSUP),
        }
    }

    /// Open or create a file at `path` (`open(2)`), allocating a descriptor in `ctx.fds`.
    pub fn open(
        &mut self,
        ctx: &mut ProcessVfsContext,
        path: &[u8],
        flags: u32,
        mode: Mode,
    ) -> Result<usize, i32> {
        let follow = if flags & open_flags::O_NOFOLLOW != 0 {
            Final::NoFollow
        } else {
            Final::Follow
        };
        let acc = flags & open_flags::O_ACCMODE;
        let writable = acc == open_flags::O_WRONLY || acc == open_flags::O_RDWR;

        let node = if flags & open_flags::O_CREAT != 0 {
            let exclusive = flags & open_flags::O_EXCL != 0;
            match self.resolve_path(ctx, path, follow) {
                Ok(found) => {
                    if exclusive {
                        return Err(errno::EEXIST);
                    }
                    found.node
                }
                Err(errno::ENOENT) => {
                    if path.ends_with(b"/") {
                        return Err(errno::EISDIR);
                    }
                    let (dir, name) = self.resolve_parent(ctx, path)?;
                    let eff_mode = (mode & 0o7777) & !ctx.umask;
                    let fs = self.filesystems.get_mut(&dir.mount).ok_or(errno::ENOENT)?;
                    let ino = match fs {
                        FsInstance::Tmpfs(t) => {
                            t.create_file(dir.ino, name, eff_mode, ctx.uid, ctx.gid, exclusive)?
                                .0
                        }
                        FsInstance::Nolxfs(n) => {
                            n.create_file(dir.ino, name, eff_mode, ctx.uid, ctx.gid, exclusive)?
                                .0
                        }
                        FsInstance::Initrd(_) => return Err(errno::EROFS),
                        FsInstance::Devfs(_) => return Err(errno::EPERM),
                    };
                    NodeId::new(dir.mount, ino)
                }
                Err(e) => return Err(e),
            }
        } else {
            self.resolve_path(ctx, path, follow)?.node
        };

        let st = self.stat_node(node)?;
        if is_lnk(st.st_mode) && (flags & open_flags::O_NOFOLLOW != 0) {
            return Err(errno::ELOOP);
        }
        if (flags & open_flags::O_DIRECTORY != 0) && !is_dir(st.st_mode) {
            return Err(errno::ENOTDIR);
        }
        if is_dir(st.st_mode) && (writable || (flags & open_flags::O_TRUNC != 0)) {
            return Err(errno::EISDIR);
        }

        if flags & open_flags::O_TRUNC != 0 {
            if !writable {
                return Err(errno::EINVAL);
            }
            if is_reg(st.st_mode) {
                match self.filesystems.get_mut(&node.mount).ok_or(errno::ENOENT)? {
                    FsInstance::Tmpfs(t) => t.truncate(node.ino, 0)?,
                    FsInstance::Nolxfs(n) => n.truncate(node.ino, 0)?,
                    FsInstance::Initrd(_) => return Err(errno::EROFS),
                    FsInstance::Devfs(_) => {}
                }
            }
        }

        if let Some(FsInstance::Tmpfs(t)) = self.filesystems.get_mut(&node.mount) {
            t.open_ref(node.ino)?;
        }

        let file_id = self.next_file_id;
        self.next_file_id += 1;
        let cloexec = flags & open_flags::O_CLOEXEC != 0;
        let fd = match ctx.fds.alloc(Descriptor {
            file: file_id,
            close_on_exec: cloexec,
        }) {
            Ok(fd) => fd,
            Err(e) => {
                if let Some(FsInstance::Tmpfs(t)) = self.filesystems.get_mut(&node.mount) {
                    let _ = t.close_ref(node.ino);
                }
                return Err(e);
            }
        };

        self.open_files.insert(
            file_id,
            FileObject {
                id: file_id,
                node,
                flags,
                offset: 0,
                ref_count: 1,
            },
        );
        Ok(fd)
    }

    /// Read up to `buf.len()` bytes from open descriptor `fd` (`read(2)`), advancing the
    /// shared [`FileObject`] offset.
    pub fn read(
        &mut self,
        ctx: &ProcessVfsContext,
        fd: usize,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        let desc = ctx.fds.get(fd)?;
        let (node, flags, offset) = {
            let fobj = self.open_files.get(&desc.file).ok_or(errno::EBADF)?;
            ((fobj.node), fobj.flags, fobj.offset)
        };
        if (flags & open_flags::O_ACCMODE) == open_flags::O_WRONLY {
            return Err(errno::EBADF);
        }

        let fs = self.filesystems.get_mut(&node.mount).ok_or(errno::EIO)?;
        let n = match fs {
            FsInstance::Tmpfs(t) => t.read(node.ino, offset, buf)?,
            FsInstance::Devfs(d) => d.read(node.ino, buf).map_err(|s| s.to_errno())?,
            FsInstance::Initrd(i) => i.read(node.ino, offset, buf)?,
            FsInstance::Nolxfs(n) => n.read(node.ino, offset, buf)?,
        };
        if let Some(fobj) = self.open_files.get_mut(&desc.file) {
            fobj.offset = fobj.offset.saturating_add(n as u64);
        }
        Ok(n)
    }

    /// Write `data` to open descriptor `fd` (`write(2)`), honoring `O_APPEND` and advancing
    /// the shared [`FileObject`] offset.
    pub fn write(&mut self, ctx: &ProcessVfsContext, fd: usize, data: &[u8]) -> Result<usize, i32> {
        let desc = ctx.fds.get(fd)?;
        let (node, flags, mut offset) = {
            let fobj = self.open_files.get(&desc.file).ok_or(errno::EBADF)?;
            (fobj.node, fobj.flags, fobj.offset)
        };
        let acc = flags & open_flags::O_ACCMODE;
        if acc != open_flags::O_WRONLY && acc != open_flags::O_RDWR {
            return Err(errno::EBADF);
        }
        if flags & open_flags::O_APPEND != 0 {
            let st = self.stat_node(node)?;
            offset = st.st_size.max(0) as u64;
        }

        let fs = self.filesystems.get_mut(&node.mount).ok_or(errno::EIO)?;
        let n = match fs {
            FsInstance::Tmpfs(t) => t.write(node.ino, offset, data)?,
            FsInstance::Devfs(d) => d.write(node.ino, data).map_err(|s| s.to_errno())?,
            FsInstance::Initrd(_) => return Err(errno::EROFS),
            FsInstance::Nolxfs(n) => n.write(node.ino, offset, data)?,
        };
        if let Some(fobj) = self.open_files.get_mut(&desc.file) {
            fobj.offset = offset.saturating_add(n as u64);
        }
        Ok(n)
    }

    /// Reposition the byte offset of `fd` (`lseek(2)`).
    pub fn lseek(
        &mut self,
        ctx: &ProcessVfsContext,
        fd: usize,
        offset: Off,
        whence: i32,
    ) -> Result<u64, i32> {
        let desc = ctx.fds.get(fd)?;
        let (node, cur_off) = {
            let fobj = self.open_files.get(&desc.file).ok_or(errno::EBADF)?;
            (fobj.node, fobj.offset as i64)
        };
        let st = self.stat_node(node)?;
        let base: i64 = match whence {
            seek::SEEK_SET => 0,
            seek::SEEK_CUR => cur_off,
            seek::SEEK_END => st.st_size,
            _ => return Err(errno::EINVAL),
        };
        let new_off = base.checked_add(offset).ok_or(errno::EOVERFLOW)?;
        if new_off < 0 {
            return Err(errno::EINVAL);
        }
        let fobj = self.open_files.get_mut(&desc.file).ok_or(errno::EBADF)?;
        fobj.offset = new_off as u64;
        Ok(fobj.offset)
    }

    /// Duplicate `old_fd` (`dup(2)`), incrementing the [`FileObject`] reference count.
    pub fn dup(&mut self, ctx: &mut ProcessVfsContext, old_fd: usize) -> Result<usize, i32> {
        let fd = ctx.fds.dup(old_fd)?;
        let file_id = ctx.fds.get(fd)?.file;
        if let Some(fobj) = self.open_files.get_mut(&file_id) {
            fobj.ref_count += 1;
        }
        Ok(fd)
    }

    /// Duplicate `old_fd` onto `new_fd` (`dup2(2)`), properly handling `old_fd == new_fd`
    /// (`FD-005`) and releasing any displaced [`FileObject`].
    pub fn dup2(
        &mut self,
        ctx: &mut ProcessVfsContext,
        old_fd: usize,
        new_fd: usize,
    ) -> Result<usize, i32> {
        let (fd, displaced) = ctx.fds.dup2(old_fd, new_fd)?;
        if old_fd != new_fd {
            let file_id = ctx.fds.get(fd)?.file;
            if let Some(fobj) = self.open_files.get_mut(&file_id) {
                fobj.ref_count += 1;
            }
        }
        if let Some(old_desc) = displaced {
            self.release_file_object(old_desc.file);
        }
        Ok(fd)
    }

    /// Perform `fcntl(fd, cmd, arg)` (`F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`,
    /// `F_GETFL`, `F_SETFL`).
    pub fn fcntl(
        &mut self,
        ctx: &mut ProcessVfsContext,
        fd: usize,
        cmd: i32,
        arg: u64,
    ) -> Result<u64, i32> {
        match cmd {
            fcntl_cmd::F_DUPFD | fcntl_cmd::F_DUPFD_CLOEXEC => {
                let min_fd = usize::try_from(arg).map_err(|_| errno::EINVAL)?;
                let cloexec = cmd == fcntl_cmd::F_DUPFD_CLOEXEC;
                let new_fd = ctx.fds.fcntl_dupfd(fd, min_fd, cloexec)?;
                let file_id = ctx.fds.get(new_fd)?.file;
                if let Some(fobj) = self.open_files.get_mut(&file_id) {
                    fobj.ref_count += 1;
                }
                Ok(new_fd as u64)
            }
            fcntl_cmd::F_GETFD => {
                let d = ctx.fds.get(fd)?;
                Ok(if d.close_on_exec {
                    fcntl_cmd::FD_CLOEXEC as u64
                } else {
                    0
                })
            }
            fcntl_cmd::F_SETFD => {
                let cloexec = (arg as i32 & fcntl_cmd::FD_CLOEXEC) != 0;
                ctx.fds.set_close_on_exec(fd, cloexec)?;
                Ok(0)
            }
            fcntl_cmd::F_GETFL => {
                let d = ctx.fds.get(fd)?;
                let fobj = self.open_files.get(&d.file).ok_or(errno::EBADF)?;
                Ok(u64::from(fobj.flags))
            }
            fcntl_cmd::F_SETFL => {
                let d = ctx.fds.get(fd)?;
                let fobj = self.open_files.get_mut(&d.file).ok_or(errno::EBADF)?;
                let mutable_mask = open_flags::O_APPEND | open_flags::O_NONBLOCK;
                fobj.flags = (fobj.flags & !mutable_mask) | ((arg as u32) & mutable_mask);
                Ok(0)
            }
            _ => Err(errno::EINVAL),
        }
    }

    /// Close descriptor `fd` (`close(2)`), decrementing its [`FileObject`] reference count and
    /// releasing any unlinked `tmpfs` inode when the final reference drops.
    pub fn close(&mut self, ctx: &mut ProcessVfsContext, fd: usize) -> Result<(), i32> {
        let desc = ctx.fds.close(fd)?;
        self.release_file_object(desc.file);
        Ok(())
    }

    fn release_file_object(&mut self, file_id: u64) {
        let drop_node = if let Some(fobj) = self.open_files.get_mut(&file_id) {
            fobj.ref_count = fobj.ref_count.saturating_sub(1);
            if fobj.ref_count == 0 {
                Some(fobj.node)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(node) = drop_node {
            self.open_files.remove(&file_id);
            if let Some(FsInstance::Tmpfs(t)) = self.filesystems.get_mut(&node.mount) {
                let _ = t.close_ref(node.ino);
            }
        }
    }

    fn lookup_in_dir(&self, dir: NodeId, name: &[u8]) -> Result<NodeId, i32> {
        let name_str = core::str::from_utf8(name).map_err(|_| errno::ENOENT)?;
        let fs = self.filesystems.get(&dir.mount).ok_or(errno::ENOENT)?;
        let child_ino = match fs {
            FsInstance::Tmpfs(t) => t.lookup(dir.ino, name_str)?,
            FsInstance::Devfs(d) => d.lookup(dir.ino, name_str).map_err(|s| s.to_errno())?,
            FsInstance::Initrd(i) => i.lookup(dir.ino, name_str)?,
            FsInstance::Nolxfs(n) => n.lookup(dir.ino, name_str)?,
        };
        let raw = NodeId::new(dir.mount, child_ino);
        // Cross down if `raw` is covered by a mounted child filesystem!
        Ok(self.mounts.cross_down(raw).unwrap_or(raw))
    }

    fn parent_of_dir(&self, ctx: &ProcessVfsContext, node: NodeId) -> NodeId {
        // Never walk above the process's `chroot` root.
        if node == ctx.root {
            return ctx.root;
        }
        // If `node` is the root of a non-root mount, cross up to the covered directory first!
        let effective = if let Some(fs) = self.filesystems.get(&node.mount) {
            if node.ino == fs.root_ino() {
                self.mounts.cross_up(node).unwrap_or(node)
            } else {
                node
            }
        } else {
            node
        };
        if effective == ctx.root {
            return ctx.root;
        }
        let parent_ino = match self.filesystems.get(&effective.mount) {
            Some(FsInstance::Tmpfs(t)) => t.parent_of(effective.ino).unwrap_or(effective.ino),
            Some(FsInstance::Devfs(_)) => DEVFS_ROOT_INO,
            Some(FsInstance::Initrd(i)) => i.parent_of(effective.ino).unwrap_or(effective.ino),
            Some(FsInstance::Nolxfs(n)) => n.parent_of(effective.ino).unwrap_or(effective.ino),
            None => effective.ino,
        };
        let raw_parent = NodeId::new(effective.mount, parent_ino);
        self.mounts.cross_down(raw_parent).unwrap_or(raw_parent)
    }

    fn node_is_dir(&self, node: NodeId) -> bool {
        self.stat_node(node).is_ok_and(|st| is_dir(st.st_mode))
    }

    fn node_is_symlink(&self, node: NodeId) -> bool {
        self.stat_node(node).is_ok_and(|st| is_lnk(st.st_mode))
    }

    fn read_symlink_node(&self, node: NodeId) -> Result<Vec<u8>, i32> {
        let fs = self.filesystems.get(&node.mount).ok_or(errno::ENOENT)?;
        match fs {
            FsInstance::Tmpfs(t) => t.readlink(node.ino),
            FsInstance::Initrd(i) => i.readlink(node.ino),
            FsInstance::Nolxfs(n) => n.readlink(node.ino),
            FsInstance::Devfs(_) => Err(errno::EINVAL),
        }
    }

    fn check_search_permission(&self, ctx: &ProcessVfsContext, dir: NodeId) -> Result<(), i32> {
        let st = self.stat_node(dir)?;
        if !is_dir(st.st_mode) {
            return Err(errno::ENOTDIR);
        }
        // POSIX: even uid 0 requires at least one execute bit, or for non-root the matching class bit.
        let exec_mask = if ctx.uid == st.st_uid {
            S_IXUSR
        } else if ctx.gid == st.st_gid {
            S_IXGRP
        } else {
            S_IXOTH
        };
        if (st.st_mode & exec_mask) == 0 {
            return Err(errno::EACCES);
        }
        Ok(())
    }
}

struct VfsResolver<'a> {
    vfs: &'a VirtualFileSystem,
    ctx: &'a ProcessVfsContext,
    symlinks: Cell<u32>,
}

impl Resolver for VfsResolver<'_> {
    fn start(&self, absolute: bool) -> NodeId {
        if absolute {
            self.ctx.root
        } else {
            self.ctx.cwd
        }
    }

    fn lookup(&self, dir: NodeId, name: &[u8]) -> Result<NodeId, i32> {
        self.vfs.lookup_in_dir(dir, name)
    }

    fn parent(&self, node: NodeId) -> NodeId {
        self.vfs.parent_of_dir(self.ctx, node)
    }

    fn is_dir(&self, node: NodeId) -> bool {
        self.vfs.node_is_dir(node)
    }

    fn is_symlink(&self, node: NodeId) -> bool {
        self.vfs.node_is_symlink(node)
    }

    fn read_link(&self, node: NodeId) -> Result<Vec<u8>, i32> {
        self.vfs.read_symlink_node(node)
    }

    fn may_search(&self, dir: NodeId) -> Result<(), i32> {
        self.vfs.check_search_permission(self.ctx, dir)
    }

    fn symlink_budget(&self) -> u32 {
        self.symlinks.get()
    }

    fn note_symlink(&self) {
        self.symlinks.set(self.symlinks.get() + 1);
    }
}

fn strip_trailing_slashes(mut path: &[u8]) -> &[u8] {
    while path.last() == Some(&b'/') {
        path = &path[..path.len() - 1];
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::initrd::build_cpio_newc;
    use nolx_types::abi::pathconf_name::*;
    use nolx_types::scalars::{S_IFDIR, S_IFREG};

    #[test]
    fn scripted_multi_mount_tree_exercises_every_pathconf_enoent_eloop_and_mount_crossing() {
        let mut vfs = VirtualFileSystem::new();
        let root = vfs
            .mount_root(FsInstance::Tmpfs(Tmpfs::new()), "tmpfs-root")
            .unwrap();
        let mut ctx = ProcessVfsContext::new(root);

        // Build directory hierarchy on `/`: `/dev`, `/initrd`, `/mnt`, `/mnt/nolxfs`, `/jail/sub`.
        vfs.mkdir(&ctx, b"/dev", 0o755).unwrap();
        vfs.mkdir(&ctx, b"/initrd", 0o755).unwrap();
        vfs.mkdir(&ctx, b"/mnt", 0o755).unwrap();
        vfs.mkdir(&ctx, b"/mnt/nolxfs", 0o755).unwrap();
        vfs.mkdir(&ctx, b"/jail", 0o755).unwrap();
        vfs.mkdir(&ctx, b"/jail/sub", 0o755).unwrap();

        // Mount devfs at `/dev`, initrd at `/initrd`, and NOLXFS at `/mnt/nolxfs`.
        vfs.mount_at(&ctx, b"/dev", FsInstance::Devfs(Devfs::new()), 0, "devfs")
            .unwrap();
        let cpio = build_cpio_newc(&[
            ("etc", S_IFDIR | 0o755, b""),
            ("etc/release", S_IFREG | 0o644, b"NOLX 0.1.0\n"),
        ]);
        vfs.mount_at(
            &ctx,
            b"/initrd",
            FsInstance::Initrd(InitrdFs::from_archive(&cpio).unwrap()),
            0,
            "initrd",
        )
        .unwrap();
        let nolx_vol = NolxfsVolume::format_and_mount(4096, 96, "data").unwrap();
        vfs.mount_at(
            &ctx,
            b"/mnt/nolxfs",
            FsInstance::Nolxfs(nolx_vol),
            0,
            "nolxfs",
        )
        .unwrap();

        // Cross down into `/mnt/nolxfs`, write a file, and walk `..` back out across the mount point!
        let fd_nolx = vfs
            .open(
                &mut ctx,
                b"/mnt/nolxfs/greeting.txt",
                open_flags::O_CREAT | open_flags::O_RDWR,
                0o644,
            )
            .unwrap();
        assert_eq!(vfs.write(&ctx, fd_nolx, b"from nolxfs"), Ok(11));
        vfs.close(&mut ctx, fd_nolx).unwrap();

        // `..` at mount root `/mnt/nolxfs/..` lands on `/mnt`, and `/mnt/nolxfs/../../initrd/etc/release`
        // crosses up out of NOLXFS and down into initrd!
        let fd_initrd = vfs
            .open(
                &mut ctx,
                b"/mnt/nolxfs/../../initrd/etc/release",
                open_flags::O_RDONLY,
                0,
            )
            .unwrap();
        let mut buf = [0u8; 16];
        let n = vfs.read(&ctx, fd_initrd, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"NOLX 0.1.0\n");
        vfs.close(&mut ctx, fd_initrd).unwrap();

        // Symlink loops (ELOOP), dangling symlinks (ENOENT), and O_NOFOLLOW (ELOOP on open).
        vfs.symlink(&ctx, b"/loop_b", b"/loop_a").unwrap();
        vfs.symlink(&ctx, b"/loop_a", b"/loop_b").unwrap();
        assert_eq!(
            vfs.open(&mut ctx, b"/loop_a", open_flags::O_RDONLY, 0),
            Err(errno::ELOOP)
        );
        vfs.symlink(&ctx, b"/does/not/exist", b"/dangling").unwrap();
        assert_eq!(
            vfs.open(&mut ctx, b"/dangling", open_flags::O_RDONLY, 0),
            Err(errno::ENOENT)
        );
        vfs.symlink(&ctx, b"/mnt/nolxfs/greeting.txt", b"/valid_link")
            .unwrap();
        assert_eq!(
            vfs.open(
                &mut ctx,
                b"/valid_link",
                open_flags::O_RDONLY | open_flags::O_NOFOLLOW,
                0
            ),
            Err(errno::ELOOP)
        );

        // Every POSIX pathconf selector on `/mnt/nolxfs/greeting.txt`.
        assert_eq!(
            vfs.pathconf(&ctx, b"/mnt/nolxfs/greeting.txt", PC_NAME_MAX),
            Ok(NAME_MAX as i64)
        );
        assert_eq!(
            vfs.pathconf(&ctx, b"/mnt/nolxfs/greeting.txt", PC_PATH_MAX),
            Ok(PATH_MAX as i64)
        );
        assert_eq!(
            vfs.pathconf(&ctx, b"/mnt/nolxfs/greeting.txt", PC_NO_TRUNC),
            Ok(1)
        );
        assert_eq!(
            vfs.pathconf(&ctx, b"/mnt/nolxfs/greeting.txt", PC_SYMLINK_MAX),
            Ok(PATH_MAX as i64 - 1)
        );

        // `chroot` into `/jail` confines `..` so `/../../..` cannot escape `/jail`.
        vfs.chroot(&mut ctx, b"/jail").unwrap();
        let escaped = vfs
            .resolve_path(&ctx, b"/../../../sub", Final::Follow)
            .unwrap();
        assert_eq!(
            escaped.node,
            vfs.resolve_path(&ctx, b"/sub", Final::Follow).unwrap().node
        );
    }

    #[test]
    fn tmpfs_open_write_dup_lseek_read_and_unlink_while_open_work_end_to_end() {
        let mut vfs = VirtualFileSystem::new();
        let root = vfs
            .mount_root(FsInstance::Tmpfs(Tmpfs::new()), "tmpfs")
            .unwrap();
        let mut ctx = ProcessVfsContext::new(root);
        ctx.set_umask(0o027);

        let fd1 = vfs
            .open(
                &mut ctx,
                b"/note.txt",
                open_flags::O_CREAT | open_flags::O_EXCL | open_flags::O_RDWR,
                0o666,
            )
            .unwrap();
        // Mode respects umask: 0o666 & !0o027 == 0o640.
        assert_eq!(vfs.fstat(&ctx, fd1).unwrap().st_mode & 0o777, 0o640);

        let fd2 = vfs.dup(&mut ctx, fd1).unwrap();
        assert_eq!(vfs.write(&ctx, fd1, b"hello "), Ok(6));
        // `fd2` shares the same FileObject and offset!
        assert_eq!(vfs.write(&ctx, fd2, b"tmpfs!"), Ok(6));

        // Unlink while still open on both fd1 and fd2.
        vfs.unlink(&ctx, b"/note.txt").unwrap();
        assert_eq!(
            vfs.stat(&ctx, b"/note.txt", Final::Follow),
            Err(errno::ENOENT)
        );

        assert_eq!(vfs.lseek(&ctx, fd1, 0, seek::SEEK_SET), Ok(0));
        let mut out = [0u8; 12];
        assert_eq!(vfs.read(&ctx, fd2, &mut out), Ok(12));
        assert_eq!(&out, b"hello tmpfs!");

        vfs.close(&mut ctx, fd1).unwrap();
        vfs.close(&mut ctx, fd2).unwrap();
    }
}
