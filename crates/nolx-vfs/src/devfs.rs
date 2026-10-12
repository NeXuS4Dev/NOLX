//! `devfs`: the synthetic device filesystem mounted at `/dev`.
//!
//! `devfs` exposes standard POSIX character devices alongside any dynamic device nodes
//! registered by kernel drivers:
//!
//! * `/dev/null` (ino `2`, `1:3`): reads return `0` (EOF); writes succeed and discard all bytes.
//! * `/dev/zero` (ino `3`, `1:5`): reads fill the caller's buffer with `0x00`; writes succeed
//!   and discard all bytes.
//! * `/dev/full` (ino `4`, `1:7`): reads fill the caller's buffer with `0x00`; writes always
//!   fail with [`IoStatus::DiskFull`] (`ENOSPC`).
//! * `/dev/console` (ino `5`, `5:1`): writes append to the console capture sink; reads drain
//!   queued console input bytes.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::scalars::{Mode, Stat, S_IFCHR, S_IFDIR};

use crate::io::IoStatus;

/// Inode number of the `devfs` root directory.
pub const DEVFS_ROOT_INO: u64 = 1;
/// Inode number of `/dev/null`.
pub const DEV_NULL_INO: u64 = 2;
/// Inode number of `/dev/zero`.
pub const DEV_ZERO_INO: u64 = 3;
/// Inode number of `/dev/full`.
pub const DEV_FULL_INO: u64 = 4;
/// Inode number of `/dev/console`.
pub const DEV_CONSOLE_INO: u64 = 5;

/// Built-in or driver-registered device behavior in `devfs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// Directory node (the `/dev` root).
    Directory,
    /// `/dev/null`: EOF on read, sink on write.
    Null,
    /// `/dev/zero`: NUL bytes on read, sink on write.
    Zero,
    /// `/dev/full`: NUL bytes on read, `ENOSPC` (`IoStatus::DiskFull`) on write.
    Full,
    /// `/dev/console`: console input/output stream.
    Console,
    /// Driver-registered character device backed by an I/O Manager `device_object` id.
    CustomChar {
        /// Associated `DeviceObject` id in [`crate::io::IoManager`].
        device_object: u64,
    },
}

/// One node in `devfs`.
#[derive(Clone, Debug)]
pub struct DevNode {
    /// Inode number inside `devfs`.
    pub ino: u64,
    /// Entry name (empty for root).
    pub name: String,
    /// File type and permission bits (`S_IFCHR | 0o666`, etc.).
    pub mode: Mode,
    /// Encoded `(major << 8) | minor` device number (`st_rdev`).
    pub rdev: u64,
    /// Behavioral class.
    pub kind: DeviceKind,
}

/// An instance of the `devfs` filesystem.
#[derive(Clone, Debug)]
pub struct Devfs {
    nodes: BTreeMap<u64, DevNode>,
    by_name: BTreeMap<String, u64>,
    next_ino: u64,
    console_in: Vec<u8>,
    console_out: Vec<u8>,
}

impl Default for Devfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Devfs {
    /// Construct a `devfs` instance populated with `/dev/null`, `/dev/zero`, `/dev/full`,
    /// and `/dev/console`.
    #[must_use]
    pub fn new() -> Self {
        let mut fs = Self {
            nodes: BTreeMap::new(),
            by_name: BTreeMap::new(),
            next_ino: 6,
            console_in: Vec::new(),
            console_out: Vec::new(),
        };
        fs.nodes.insert(
            DEVFS_ROOT_INO,
            DevNode {
                ino: DEVFS_ROOT_INO,
                name: String::new(),
                mode: S_IFDIR | 0o755,
                rdev: 0,
                kind: DeviceKind::Directory,
            },
        );
        fs.insert_builtin(DEV_NULL_INO, "null", 0x0103, DeviceKind::Null);
        fs.insert_builtin(DEV_ZERO_INO, "zero", 0x0105, DeviceKind::Zero);
        fs.insert_builtin(DEV_FULL_INO, "full", 0x0107, DeviceKind::Full);
        fs.insert_builtin(DEV_CONSOLE_INO, "console", 0x0501, DeviceKind::Console);
        fs
    }

    fn insert_builtin(&mut self, ino: u64, name: &str, rdev: u64, kind: DeviceKind) {
        self.nodes.insert(
            ino,
            DevNode {
                ino,
                name: String::from(name),
                mode: S_IFCHR | 0o666,
                rdev,
                kind,
            },
        );
        self.by_name.insert(String::from(name), ino);
    }

    /// Register a custom character device node under `/dev/<name>`.
    ///
    /// # Errors
    ///
    /// [`IoStatus::AlreadyExists`] if `<name>` already exists in `/dev`.
    pub fn register_char_device(
        &mut self,
        name: &str,
        rdev: u64,
        mode: Mode,
        device_object: u64,
    ) -> Result<u64, IoStatus> {
        if name.is_empty() || name.contains('/') {
            return Err(IoStatus::InvalidParameter);
        }
        if self.by_name.contains_key(name) {
            return Err(IoStatus::AlreadyExists);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(
            ino,
            DevNode {
                ino,
                name: String::from(name),
                mode: S_IFCHR | (mode & 0o7777),
                rdev,
                kind: DeviceKind::CustomChar { device_object },
            },
        );
        self.by_name.insert(String::from(name), ino);
        Ok(ino)
    }

    /// Look up a child entry in the `devfs` root directory.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64, IoStatus> {
        if dir_ino != DEVFS_ROOT_INO {
            return Err(IoStatus::NotDirectory);
        }
        self.by_name.get(name).copied().ok_or(IoStatus::NoSuchFile)
    }

    /// Inspect a node by inode number.
    #[must_use]
    pub fn node(&self, ino: u64) -> Option<&DevNode> {
        self.nodes.get(&ino)
    }

    /// Query POSIX [`Stat`] metadata for `ino`.
    pub fn stat(&self, dev_id: u64, ino: u64) -> Result<Stat, IoStatus> {
        let node = self.nodes.get(&ino).ok_or(IoStatus::NoSuchFile)?;
        let nlink = if node.kind == DeviceKind::Directory {
            2
        } else {
            1
        };
        Ok(Stat {
            st_dev: dev_id,
            st_ino: node.ino,
            st_mode: node.mode,
            st_nlink: nlink,
            st_rdev: node.rdev,
            st_blksize: 4096,
            ..Stat::default()
        })
    }

    /// List `(name, ino)` entries in `/dev`.
    pub fn readdir(&self, dir_ino: u64) -> Result<Vec<(String, u64)>, IoStatus> {
        if dir_ino != DEVFS_ROOT_INO {
            return Err(IoStatus::NotDirectory);
        }
        Ok(self.by_name.iter().map(|(k, &v)| (k.clone(), v)).collect())
    }

    /// Read from a `devfs` node.
    pub fn read(&mut self, ino: u64, buf: &mut [u8]) -> Result<usize, IoStatus> {
        let node = self.nodes.get(&ino).ok_or(IoStatus::NoSuchFile)?;
        match node.kind {
            DeviceKind::Directory => Err(IoStatus::IsDirectory),
            DeviceKind::Null => Ok(0),
            DeviceKind::Zero | DeviceKind::Full => {
                buf.fill(0);
                Ok(buf.len())
            }
            DeviceKind::Console => {
                let n = buf.len().min(self.console_in.len());
                buf[..n].copy_from_slice(&self.console_in[..n]);
                self.console_in.drain(..n);
                Ok(n)
            }
            DeviceKind::CustomChar { .. } => Ok(0),
        }
    }

    /// Write to a `devfs` node.
    pub fn write(&mut self, ino: u64, data: &[u8]) -> Result<usize, IoStatus> {
        let node = self.nodes.get(&ino).ok_or(IoStatus::NoSuchFile)?;
        match node.kind {
            DeviceKind::Directory => Err(IoStatus::IsDirectory),
            DeviceKind::Null | DeviceKind::Zero => Ok(data.len()),
            DeviceKind::Full => Err(IoStatus::DiskFull),
            DeviceKind::Console => {
                self.console_out.extend_from_slice(data);
                Ok(data.len())
            }
            DeviceKind::CustomChar { .. } => Ok(data.len()),
        }
    }

    /// Feed input bytes into `/dev/console`.
    pub fn push_console_input(&mut self, bytes: &[u8]) {
        self.console_in.extend_from_slice(bytes);
    }

    /// Bytes written to `/dev/console`.
    #[must_use]
    pub fn console_output(&self) -> &[u8] {
        &self.console_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_zero_full_and_console_behave_per_spec() {
        let mut dev = Devfs::new();
        let null = dev.lookup(DEVFS_ROOT_INO, "null").unwrap();
        let zero = dev.lookup(DEVFS_ROOT_INO, "zero").unwrap();
        let full = dev.lookup(DEVFS_ROOT_INO, "full").unwrap();
        let cons = dev.lookup(DEVFS_ROOT_INO, "console").unwrap();

        let mut buf = [0xFFu8; 16];
        // /dev/null: EOF on read, accepts writes.
        assert_eq!(dev.read(null, &mut buf), Ok(0));
        assert_eq!(dev.write(null, b"discarded"), Ok(9));

        // /dev/zero: zeroes buffer on read, accepts writes.
        assert_eq!(dev.read(zero, &mut buf), Ok(16));
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(dev.write(zero, b"ignored"), Ok(7));

        // /dev/full: zeroes buffer on read, fails writes with DiskFull (ENOSPC).
        buf.fill(0xAA);
        assert_eq!(dev.read(full, &mut buf), Ok(16));
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(dev.write(full, b"x"), Err(IoStatus::DiskFull));

        // /dev/console: round-trips console output and queued input.
        assert_eq!(dev.write(cons, b"hello console"), Ok(13));
        assert_eq!(dev.console_output(), b"hello console");
        dev.push_console_input(b"kbd");
        let mut in_buf = [0u8; 8];
        assert_eq!(dev.read(cons, &mut in_buf), Ok(3));
        assert_eq!(&in_buf[..3], b"kbd");
    }
}
