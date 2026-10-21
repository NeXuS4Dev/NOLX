//! `initrd`: SVR4 ASCII `cpio` (`070701`/`070702`) and POSIX `ustar` `tar` archive reader.
//!
//! At boot, an initial ramdisk image may be supplied either as a SVR4 "newc" `cpio` archive
//! or as a 512-byte-blocked POSIX `ustar` `tar` archive. This module:
//!
//! * auto-detects and parses both formats without unsafe code;
//! * exposes [`InitrdFs`], a read-only filesystem over an archive's entries (`EROFS` on write);
//! * provides [`InitrdFs::unpack_into_tmpfs`] to populate a writable [`Tmpfs`] root; and
//! * provides deterministic archive builders ([`build_cpio_newc`] and [`build_tar_ustar`])
//!   used by host tests and the kernel boot self-test.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::errno;
use nolx_types::scalars::{
    is_dir, is_lnk, is_reg, Mode, Stat, Timespec, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};

use crate::tmpfs::{Tmpfs, TMPFS_ROOT_INO};

/// Root inode number of an [`InitrdFs`] mount.
pub const INITRD_ROOT_INO: u64 = 1;

/// Size of a SVR4 ASCII `cpio` header (`"070701"` or `"070702"`).
pub const CPIO_NEWC_HEADER_LEN: usize = 110;

/// Sentinel filename marking the end of a `cpio` archive.
pub const CPIO_TRAILER_NAME: &str = "TRAILER!!!";

/// Size of a POSIX `tar` block.
pub const TAR_BLOCK_LEN: usize = 512;

/// Archive format detected by [`parse_archive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// SVR4 portable ASCII `cpio` (`070701` newc or `070702` crc).
    CpioNewc,
    /// POSIX.1-1988 `ustar` `tar`.
    TarUstar,
}

/// Errors returned when parsing an `initrd` archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitrdError {
    /// Archive does not start with a recognized `cpio` or `tar` magic signature.
    UnrecognizedFormat,
    /// Archive ended inside a header, filename, or file payload.
    TruncatedArchive,
    /// Header numeric field contained invalid hex (`cpio`) or octal (`tar`) digits.
    InvalidNumber,
    /// Header checksum (`tar`) failed verification.
    BadChecksum,
    /// Pathname was not valid UTF-8 or contained an unsafe `..` traversal segment.
    InvalidPath,
}

/// One parsed entry from a `cpio` or `tar` archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitrdEntry {
    /// Normalized relative path without leading or trailing slashes (e.g. `"etc/motd"`).
    pub path: String,
    /// POSIX mode including file-type bits (`S_IFREG`, `S_IFDIR`, `S_IFLNK`).
    pub mode: Mode,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Modification timestamp (`st_mtime`).
    pub mtime: u64,
    /// File contents (for `S_IFREG`) or symlink target bytes (for `S_IFLNK`).
    pub data: Vec<u8>,
}

/// Auto-detect and parse either a SVR4 `cpio` (`070701`/`070702`) or a `ustar` `tar` archive.
pub fn parse_archive(bytes: &[u8]) -> Result<(ArchiveFormat, Vec<InitrdEntry>), InitrdError> {
    if bytes.len() >= 6 && (&bytes[..6] == b"070701" || &bytes[..6] == b"070702") {
        let entries = parse_cpio_newc(bytes)?;
        return Ok((ArchiveFormat::CpioNewc, entries));
    }
    if bytes.len() >= TAR_BLOCK_LEN && &bytes[257..262] == b"ustar" {
        let entries = parse_tar_ustar(bytes)?;
        return Ok((ArchiveFormat::TarUstar, entries));
    }
    Err(InitrdError::UnrecognizedFormat)
}

/// Parse a SVR4 ASCII `cpio` (`070701` or `070702`) archive.
pub fn parse_cpio_newc(bytes: &[u8]) -> Result<Vec<InitrdEntry>, InitrdError> {
    let mut pos = 0usize;
    let mut entries = Vec::new();

    while pos < bytes.len() {
        if pos + CPIO_NEWC_HEADER_LEN > bytes.len() {
            return Err(InitrdError::TruncatedArchive);
        }
        let hdr = &bytes[pos..pos + CPIO_NEWC_HEADER_LEN];
        if &hdr[..6] != b"070701" && &hdr[..6] != b"070702" {
            return Err(InitrdError::UnrecognizedFormat);
        }

        let mode = parse_hex_u32(&hdr[14..22])?;
        let uid = parse_hex_u32(&hdr[22..30])?;
        let gid = parse_hex_u32(&hdr[30..38])?;
        let mtime = u64::from(parse_hex_u32(&hdr[46..54])?);
        let filesize = parse_hex_u32(&hdr[54..62])? as usize;
        let namesize = parse_hex_u32(&hdr[94..102])? as usize;
        if namesize == 0 {
            return Err(InitrdError::InvalidPath);
        }

        pos += CPIO_NEWC_HEADER_LEN;
        if pos + namesize > bytes.len() {
            return Err(InitrdError::TruncatedArchive);
        }
        let raw_name = &bytes[pos..pos + namesize];
        let trimmed_name = raw_name.strip_suffix(&[0]).unwrap_or(raw_name);
        let name_str = core::str::from_utf8(trimmed_name).map_err(|_| InitrdError::InvalidPath)?;
        pos = align4(pos + namesize);

        if name_str == CPIO_TRAILER_NAME {
            break;
        }

        if pos + filesize > bytes.len() {
            return Err(InitrdError::TruncatedArchive);
        }
        let data = bytes[pos..pos + filesize].to_vec();
        pos = align4(pos + filesize);

        let norm = normalize_archive_path(name_str)?;
        if !norm.is_empty() {
            entries.push(InitrdEntry {
                path: norm,
                mode,
                uid,
                gid,
                mtime,
                data,
            });
        }
    }
    Ok(entries)
}

/// Parse a POSIX `ustar` `tar` archive.
pub fn parse_tar_ustar(bytes: &[u8]) -> Result<Vec<InitrdEntry>, InitrdError> {
    let mut pos = 0usize;
    let mut entries = Vec::new();

    while pos + TAR_BLOCK_LEN <= bytes.len() {
        let blk = &bytes[pos..pos + TAR_BLOCK_LEN];
        if blk.iter().all(|&b| b == 0) {
            break;
        }
        if &blk[257..262] != b"ustar" {
            return Err(InitrdError::UnrecognizedFormat);
        }

        // Verify header checksum (unsigned byte sum with bytes 148..156 treated as spaces).
        let stored_sum = parse_octal_u64(&blk[148..156])? as u32;
        let computed_sum: u32 = blk
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32u32
                } else {
                    u32::from(b)
                }
            })
            .sum();
        if stored_sum != computed_sum {
            return Err(InitrdError::BadChecksum);
        }

        let name_part = trim_nul_str(&blk[0..100])?;
        let prefix_part = trim_nul_str(&blk[345..500])?;
        let full_name = if prefix_part.is_empty() {
            String::from(name_part)
        } else {
            alloc::format!("{prefix_part}/{name_part}")
        };

        let perm = (parse_octal_u64(&blk[100..108])? as u32) & 0o7777;
        let uid = parse_octal_u64(&blk[108..116])? as u32;
        let gid = parse_octal_u64(&blk[116..124])? as u32;
        let size = parse_octal_u64(&blk[124..136])? as usize;
        let mtime = parse_octal_u64(&blk[136..148])?;
        let typeflag = blk[156];

        pos += TAR_BLOCK_LEN;

        let (mode, data) = match typeflag {
            b'5' => (S_IFDIR | perm, Vec::new()),
            b'2' => {
                let linkname = trim_nul_str(&blk[157..257])?;
                (S_IFLNK | 0o777, linkname.as_bytes().to_vec())
            }
            0 | b'0' => {
                if pos + size > bytes.len() {
                    return Err(InitrdError::TruncatedArchive);
                }
                let payload = bytes[pos..pos + size].to_vec();
                let blocks = size.div_ceil(TAR_BLOCK_LEN);
                pos += blocks * TAR_BLOCK_LEN;
                (S_IFREG | perm, payload)
            }
            _ => {
                let blocks = size.div_ceil(TAR_BLOCK_LEN);
                pos += blocks * TAR_BLOCK_LEN;
                continue;
            }
        };

        let norm = normalize_archive_path(&full_name)?;
        if !norm.is_empty() {
            entries.push(InitrdEntry {
                path: norm,
                mode,
                uid,
                gid,
                mtime,
                data,
            });
        }
    }
    Ok(entries)
}

/// Deterministically build a SVR4 ASCII `cpio` (`070701`) archive from `(path, mode, data)` tuples.
#[must_use]
pub fn build_cpio_newc(items: &[(&str, Mode, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (idx, &(path, mode, data)) in items.iter().enumerate() {
        let ino = (idx + 2) as u32;
        let payload = if is_dir(mode) { &[][..] } else { data };
        append_cpio_record(&mut out, ino, mode, path, payload);
    }
    append_cpio_record(&mut out, 0, 0, CPIO_TRAILER_NAME, &[]);
    out
}

fn append_cpio_record(out: &mut Vec<u8>, ino: u32, mode: Mode, name: &str, data: &[u8]) {
    let namesize = name.len() + 1;
    let nlink: u32 = if is_dir(mode) { 2 } else { 1 };
    let header = alloc::format!(
        "070701\
         {ino:08X}\
         {mode:08X}\
         {:08X}\
         {:08X}\
         {nlink:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {namesize:08X}\
         {:08X}",
        0u32,
        0u32,
        0u32,
        data.len() as u32,
        0u32,
        0u32,
        0u32,
        0u32,
        0u32,
    );
    debug_assert_eq!(header.len(), CPIO_NEWC_HEADER_LEN);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Deterministically build a POSIX `ustar` `tar` archive from `(path, mode, data)` tuples.
#[must_use]
pub fn build_tar_ustar(items: &[(&str, Mode, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(path, mode, data) in items {
        let mut blk = [0u8; TAR_BLOCK_LEN];
        let name_bytes = path.as_bytes();
        let copy_len = name_bytes.len().min(100);
        blk[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        write_octal(&mut blk[100..108], u64::from(mode & 0o7777));
        write_octal(&mut blk[108..116], 0);
        write_octal(&mut blk[116..124], 0);

        let payload_len = if is_reg(mode) { data.len() } else { 0 };
        write_octal(&mut blk[124..136], payload_len as u64);
        write_octal(&mut blk[136..148], 0);

        blk[148..156].fill(b' ');
        blk[156] = match mode & S_IFMT {
            S_IFDIR => b'5',
            S_IFLNK => b'2',
            _ => b'0',
        };
        if is_lnk(mode) {
            let link_len = data.len().min(100);
            blk[157..157 + link_len].copy_from_slice(&data[..link_len]);
        }
        blk[257..263].copy_from_slice(b"ustar\0");
        blk[263..265].copy_from_slice(b"00");

        let sum: u32 = blk.iter().map(|&b| u32::from(b)).sum();
        write_octal(&mut blk[148..155], u64::from(sum));
        blk[155] = b' ';

        out.extend_from_slice(&blk);
        if is_reg(mode) && !data.is_empty() {
            out.extend_from_slice(data);
            let rem = data.len() % TAR_BLOCK_LEN;
            if rem != 0 {
                out.extend(core::iter::repeat_n(0u8, TAR_BLOCK_LEN - rem));
            }
        }
    }
    // Two zero-filled 512-byte end-of-archive blocks.
    out.extend(core::iter::repeat_n(0u8, TAR_BLOCK_LEN * 2));
    out
}

/// Read-only mounted `initrd` filesystem (`InitrdFs`).
#[derive(Clone, Debug)]
pub struct InitrdFs {
    format: ArchiveFormat,
    inodes: BTreeMap<u64, InitrdNode>,
}

#[derive(Clone, Debug)]
struct InitrdNode {
    ino: u64,
    parent: u64,
    mode: Mode,
    uid: u32,
    gid: u32,
    mtime: u64,
    data: Vec<u8>,
    children: BTreeMap<String, u64>,
}

impl InitrdFs {
    /// Mount an `InitrdFs` from raw `cpio` or `tar` archive bytes.
    pub fn from_archive(bytes: &[u8]) -> Result<Self, InitrdError> {
        let (format, entries) = parse_archive(bytes)?;
        let mut inodes = BTreeMap::new();
        inodes.insert(
            INITRD_ROOT_INO,
            InitrdNode {
                ino: INITRD_ROOT_INO,
                parent: INITRD_ROOT_INO,
                mode: S_IFDIR | 0o755,
                uid: 0,
                gid: 0,
                mtime: 0,
                data: Vec::new(),
                children: BTreeMap::new(),
            },
        );
        let mut next_ino = 2u64;

        for entry in entries {
            let parts: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                continue;
            }
            let mut cur_dir = INITRD_ROOT_INO;
            for &dir_name in &parts[..parts.len() - 1] {
                let existing = inodes[&cur_dir].children.get(dir_name).copied();
                cur_dir = if let Some(id) = existing {
                    id
                } else {
                    let id = next_ino;
                    next_ino += 1;
                    inodes
                        .get_mut(&cur_dir)
                        .unwrap()
                        .children
                        .insert(String::from(dir_name), id);
                    inodes.insert(
                        id,
                        InitrdNode {
                            ino: id,
                            parent: cur_dir,
                            mode: S_IFDIR | 0o755,
                            uid: 0,
                            gid: 0,
                            mtime: 0,
                            data: Vec::new(),
                            children: BTreeMap::new(),
                        },
                    );
                    id
                };
            }

            let leaf = parts[parts.len() - 1];
            if let Some(&existing_id) = inodes[&cur_dir].children.get(leaf) {
                if let Some(node) = inodes.get_mut(&existing_id) {
                    node.mode = entry.mode;
                    node.uid = entry.uid;
                    node.gid = entry.gid;
                    node.mtime = entry.mtime;
                    node.data = entry.data;
                }
            } else {
                let id = next_ino;
                next_ino += 1;
                inodes
                    .get_mut(&cur_dir)
                    .unwrap()
                    .children
                    .insert(String::from(leaf), id);
                inodes.insert(
                    id,
                    InitrdNode {
                        ino: id,
                        parent: cur_dir,
                        mode: entry.mode,
                        uid: entry.uid,
                        gid: entry.gid,
                        mtime: entry.mtime,
                        data: entry.data,
                        children: BTreeMap::new(),
                    },
                );
            }
        }

        Ok(Self { format, inodes })
    }

    /// Detected archive format (`CpioNewc` or `TarUstar`).
    #[must_use]
    pub const fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// Look up `name` in `dir_ino`.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64, i32> {
        let node = self.inodes.get(&dir_ino).ok_or(errno::ENOENT)?;
        if !is_dir(node.mode) {
            return Err(errno::ENOTDIR);
        }
        node.children.get(name).copied().ok_or(errno::ENOENT)
    }

    /// Parent directory inode of `ino`.
    #[must_use]
    pub fn parent_of(&self, ino: u64) -> Option<u64> {
        self.inodes.get(&ino).map(|n| n.parent)
    }

    /// Read bytes from regular file `ino` starting at `offset`.
    pub fn read(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        if is_dir(node.mode) {
            return Err(errno::EISDIR);
        }
        if !is_reg(node.mode) {
            return Err(errno::EINVAL);
        }
        let off = usize::try_from(offset).map_err(|_| errno::EINVAL)?;
        if off >= node.data.len() || buf.is_empty() {
            return Ok(0);
        }
        let n = (node.data.len() - off).min(buf.len());
        buf[..n].copy_from_slice(&node.data[off..off + n]);
        Ok(n)
    }

    /// Read the target of symbolic link `ino`.
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        if !is_lnk(node.mode) {
            return Err(errno::EINVAL);
        }
        Ok(node.data.clone())
    }

    /// Query POSIX [`Stat`] for `ino`.
    pub fn stat(&self, dev_id: u64, ino: u64) -> Result<Stat, i32> {
        let node = self.inodes.get(&ino).ok_or(errno::ENOENT)?;
        let size = if is_dir(node.mode) {
            node.children.len() as i64
        } else {
            node.data.len() as i64
        };
        Ok(Stat {
            st_dev: dev_id,
            st_ino: node.ino,
            st_mode: node.mode,
            st_nlink: if is_dir(node.mode) { 2 } else { 1 },
            st_uid: node.uid,
            st_gid: node.gid,
            st_size: size,
            st_blksize: 4096,
            st_blocks: (size + 511) / 512,
            st_mtim: Timespec {
                tv_sec: node.mtime as i64,
                tv_nsec: 0,
            },
            ..Stat::default()
        })
    }

    /// Enumerate `(name, ino)` pairs inside directory `dir_ino`.
    pub fn readdir(&self, dir_ino: u64) -> Result<Vec<(String, u64)>, i32> {
        let node = self.inodes.get(&dir_ino).ok_or(errno::ENOENT)?;
        if !is_dir(node.mode) {
            return Err(errno::ENOTDIR);
        }
        Ok(node.children.iter().map(|(k, &v)| (k.clone(), v)).collect())
    }

    /// Unpack every directory, regular file, and symlink in this `InitrdFs` into `tmpfs`.
    pub fn unpack_into_tmpfs(&self, tmpfs: &mut Tmpfs) -> Result<usize, i32> {
        let mut count = 0usize;
        self.unpack_dir_recursive(INITRD_ROOT_INO, TMPFS_ROOT_INO, tmpfs, &mut count)?;
        Ok(count)
    }

    fn unpack_dir_recursive(
        &self,
        src_dir: u64,
        dst_dir: u64,
        tmpfs: &mut Tmpfs,
        count: &mut usize,
    ) -> Result<(), i32> {
        let Some(dir_node) = self.inodes.get(&src_dir) else {
            return Ok(());
        };
        for (name, &child_ino) in &dir_node.children {
            let child = &self.inodes[&child_ino];
            if is_dir(child.mode) {
                let sub = match tmpfs.lookup(dst_dir, name) {
                    Ok(existing) => existing,
                    Err(_) => {
                        tmpfs.mkdir(dst_dir, name, child.mode & 0o7777, child.uid, child.gid)?
                    }
                };
                *count += 1;
                self.unpack_dir_recursive(child_ino, sub, tmpfs, count)?;
            } else if is_lnk(child.mode) {
                tmpfs.symlink(dst_dir, name, &child.data, child.uid, child.gid)?;
                *count += 1;
            } else if is_reg(child.mode) {
                let (f_ino, _) = tmpfs.create_file(
                    dst_dir,
                    name,
                    child.mode & 0o7777,
                    child.uid,
                    child.gid,
                    false,
                )?;
                tmpfs.truncate(f_ino, 0)?;
                tmpfs.write(f_ino, 0, &child.data)?;
                *count += 1;
            }
        }
        Ok(())
    }
}

const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn parse_hex_u32(bytes: &[u8]) -> Result<u32, InitrdError> {
    let s = core::str::from_utf8(bytes).map_err(|_| InitrdError::InvalidNumber)?;
    u32::from_str_radix(s, 16).map_err(|_| InitrdError::InvalidNumber)
}

fn parse_octal_u64(bytes: &[u8]) -> Result<u64, InitrdError> {
    let mut val = 0u64;
    let mut seen_digit = false;
    for &b in bytes {
        if b == 0 || b == b' ' {
            if seen_digit {
                break;
            }
            continue;
        }
        if !(b'0'..=b'7').contains(&b) {
            return Err(InitrdError::InvalidNumber);
        }
        seen_digit = true;
        val = val
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
            .ok_or(InitrdError::InvalidNumber)?;
    }
    Ok(val)
}

fn write_octal(field: &mut [u8], mut value: u64) {
    let len = field.len();
    if len == 0 {
        return;
    }
    field[len - 1] = 0;
    for slot in field[..len - 1].iter_mut().rev() {
        *slot = b'0' + ((value & 7) as u8);
        value >>= 3;
    }
}

fn trim_nul_str(bytes: &[u8]) -> Result<&str, InitrdError> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).map_err(|_| InitrdError::InvalidPath)
}

fn normalize_archive_path(raw: &str) -> Result<String, InitrdError> {
    let mut parts = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(InitrdError::InvalidPath);
        }
        parts.push(seg);
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpio_newc_archive_round_trips_and_unpacks_into_tmpfs() {
        let items: &[(&str, Mode, &[u8])] = &[
            ("etc", S_IFDIR | 0o755, b""),
            ("etc/motd", S_IFREG | 0o644, b"NOLX initrd ready\n"),
            ("bin", S_IFDIR | 0o755, b""),
            ("bin/sh", S_IFLNK | 0o777, b"/bin/init"),
        ];
        let archive = build_cpio_newc(items);
        let fs = InitrdFs::from_archive(&archive).unwrap();
        assert_eq!(fs.format(), ArchiveFormat::CpioNewc);

        let etc = fs.lookup(INITRD_ROOT_INO, "etc").unwrap();
        let motd = fs.lookup(etc, "motd").unwrap();
        let mut buf = [0u8; 32];
        let n = fs.read(motd, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"NOLX initrd ready\n");

        let bin = fs.lookup(INITRD_ROOT_INO, "bin").unwrap();
        let sh = fs.lookup(bin, "sh").unwrap();
        assert_eq!(fs.readlink(sh).unwrap(), b"/bin/init");

        let mut tmpfs = Tmpfs::new();
        let unpacked = fs.unpack_into_tmpfs(&mut tmpfs).unwrap();
        assert_eq!(unpacked, 4);
        let t_etc = tmpfs.lookup(TMPFS_ROOT_INO, "etc").unwrap();
        let t_motd = tmpfs.lookup(t_etc, "motd").unwrap();
        let mut out = [0u8; 18];
        assert_eq!(tmpfs.read(t_motd, 0, &mut out), Ok(18));
        assert_eq!(&out, b"NOLX initrd ready\n");
    }

    #[test]
    fn tar_ustar_archive_round_trips_files_dirs_and_symlinks() {
        let items: &[(&str, Mode, &[u8])] = &[
            ("usr", S_IFDIR | 0o755, b""),
            ("usr/readme.txt", S_IFREG | 0o644, b"ustar payload"),
            ("usr/latest", S_IFLNK | 0o777, b"readme.txt"),
        ];
        let archive = build_tar_ustar(items);
        let fs = InitrdFs::from_archive(&archive).unwrap();
        assert_eq!(fs.format(), ArchiveFormat::TarUstar);

        let usr = fs.lookup(INITRD_ROOT_INO, "usr").unwrap();
        let readme = fs.lookup(usr, "readme.txt").unwrap();
        let latest = fs.lookup(usr, "latest").unwrap();

        let mut buf = [0u8; 32];
        let n = fs.read(readme, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ustar payload");
        assert_eq!(fs.readlink(latest).unwrap(), b"readme.txt");
    }
}
