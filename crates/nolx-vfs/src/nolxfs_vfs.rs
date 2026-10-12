//! VFS driver for **NOLXFS** (`crates/nolxfs`).
//!
//! Bridges the on-disk NOLXFS v1 container, object map, and `fs-tree` B-tree (`KeyKind::JKey`)
//! to the VFS layer:
//!
//! * **Mount ([`NolxfsVolume::mount`])**: runs [`nolxfs::mkfs::probe`] and
//!   [`nolxfs::mkfs::read_root_inode`] to validate the container superblock, container object
//!   map, volume superblock, volume object map, and root directory inode (`ROOT_INO = 2`),
//!   checking every Fletcher-64 checksum before trusting any pointer.
//! * **B-tree records (`nolx_j_key_t`)**: reads and writes `DREC_TYPE_INODE` (`nolx_inode_t`),
//!   `DREC_TYPE_DIR_REC` (`nolx_dentry_key_t` -> `nolx_direntry_t`), and
//!   `DREC_TYPE_FILE_EXTENT` (`nolx_file_extent_key_t` -> `nolx_file_extent_val_t`) records in
//!   the volume's `fs-tree` via [`nolxfs::btree::BTree`].
//! * **Physical extents**: stores regular file payloads in physical data blocks allocated from
//!   the container's data region (`first_data_block..block_count`).

use core::fmt;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use nolx_types::errno;
use nolx_types::scalars::{
    is_dir, is_reg, Gid, Mode, Stat, Uid, NAME_MAX, S_IFDIR, S_IFMT, S_IFREG,
};
use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::btree::{BTree, BlockAllocator, KeyKind, Node};
use nolxfs::mkfs::{self, FormatOptions, VolumeInfo};
use nolxfs::on_disk::{extent_len, jkey_oid, jkey_type, pack_jkey, pack_len_and_flags, FsError};
use nolxfs::{on_disk_gen as gen, Pod};

/// Root directory inode number in NOLXFS (`gen::ROOT_INO == 2`).
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

struct LinearBlockAlloc {
    next_block: u64,
    block_count: u64,
}

impl BlockAllocator for LinearBlockAlloc {
    fn alloc(&mut self) -> nolxfs::on_disk::Result<u64> {
        if self.next_block >= self.block_count {
            return Err(FsError::BlockOutOfRange(self.next_block));
        }
        let b = self.next_block;
        self.next_block += 1;
        Ok(b)
    }
}

/// A mounted NOLXFS volume backed by a [`BlockDevice`].
pub struct NolxfsVolume<D: BlockDevice = SparseBlockDevice> {
    dev: D,
    info: Box<VolumeInfo>,
    next_ino: u64,
    alloc: LinearBlockAlloc,
}

impl<D: BlockDevice> fmt::Debug for NolxfsVolume<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NolxfsVolume")
            .field("name", &self.info.name)
            .field("fs_tree_root", &self.info.layout.fs_tree_root)
            .field("next_ino", &self.next_ino)
            .finish()
    }
}

impl NolxfsVolume<SparseBlockDevice> {
    /// Format a fresh sparse in-memory NOLXFS container of `block_count` blocks of
    /// `block_size` bytes and mount it immediately.
    pub fn format_and_mount(block_size: usize, block_count: u64, label: &str) -> Result<Self, i32> {
        let mut dev = SparseBlockDevice::new(block_size, block_count);
        let opts = FormatOptions {
            block_size: block_size as u32,
            volume_name: String::from(label),
            ..FormatOptions::default()
        };
        mkfs::format(&mut dev, &opts).map_err(fs_err_to_errno)?;
        Self::mount(dev)
    }
}

impl<D: BlockDevice> NolxfsVolume<D> {
    /// Probe and mount an existing NOLXFS container on `dev`.
    pub fn mount(mut dev: D) -> Result<Self, i32> {
        let info = mkfs::probe(&mut dev).map_err(fs_err_to_errno)?;
        let _root_inode = mkfs::read_root_inode(&mut dev, &info).map_err(fs_err_to_errno)?;

        let mut max_ino = NOLXFS_ROOT_INO;
        let mut next_block = info.layout.first_data_block;
        let mut temp_alloc = LinearBlockAlloc {
            next_block,
            block_count: info.layout.block_count,
        };
        let entries = {
            let mut tree = BTree::new(
                &mut dev,
                &mut temp_alloc,
                info.layout.fs_tree_root,
                KeyKind::JKey,
                gen::SUB_FSTREE as u16,
                1,
            );
            tree.collect().map_err(fs_err_to_errno)?
        };
        for entry in &entries {
            if entry.key.len() >= 8 {
                let raw_jkey = u64::from_le_bytes(entry.key[..8].try_into().unwrap());
                let oid = jkey_oid(raw_jkey);
                if oid > max_ino {
                    max_ino = oid;
                }
                if jkey_type(raw_jkey) == gen::DREC_TYPE_FILE_EXTENT {
                    if let Some(ext) = gen::nolx_file_extent_val_t::read_from(&entry.val) {
                        let byte_len = extent_len(ext.len_and_flags);
                        let blocks = byte_len.div_ceil(info.layout.block_size as u64);
                        let end_blk = ext.phys_block_num + blocks;
                        if end_blk > next_block {
                            next_block = end_blk;
                        }
                    }
                }
            }
        }

        let block_count = info.layout.block_count;
        Ok(Self {
            dev,
            info: Box::new(info),
            next_ino: max_ino + 1,
            alloc: LinearBlockAlloc {
                next_block,
                block_count,
            },
        })
    }

    /// Consume the mounted volume and return the underlying [`BlockDevice`].
    #[must_use]
    pub fn into_device(self) -> D {
        self.dev
    }

    /// Volume metadata read at mount time.
    #[must_use]
    pub const fn volume_info(&self) -> &VolumeInfo {
        &self.info
    }

    /// Read and decode `nolx_inode_t` for `ino`.
    pub fn read_inode(&self, ino: u64) -> Result<gen::nolx_inode_t, i32> {
        let key = inode_key(ino);
        let bytes = self.btree_search(&key)?.ok_or(errno::ENOENT)?;
        gen::nolx_inode_t::read_from(&bytes).ok_or(errno::EIO)
    }

    /// Parent directory inode for `ino`.
    #[must_use]
    pub fn parent_of(&self, ino: u64) -> Option<u64> {
        let inode = self.read_inode(ino).ok()?;
        if ino == NOLXFS_ROOT_INO {
            Some(NOLXFS_ROOT_INO)
        } else {
            Some(inode.parent_id)
        }
    }

    /// Look up `name` inside directory `dir_ino`.
    pub fn lookup(&self, dir_ino: u64, name: &str) -> Result<u64, i32> {
        let dir_inode = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(dir_inode.mode)) {
            return Err(errno::ENOTDIR);
        }
        let key = dentry_key(dir_ino, name);
        let val = self.btree_search(&key)?.ok_or(errno::ENOENT)?;
        let dentry = gen::nolx_direntry_t::read_from(&val).ok_or(errno::EIO)?;
        Ok(dentry.ino)
    }

    /// Enumerate `(name, child_ino)` entries in `dir_ino`.
    pub fn readdir(&mut self, dir_ino: u64) -> Result<Vec<(String, u64)>, i32> {
        let dir_inode = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(dir_inode.mode)) {
            return Err(errno::ENOTDIR);
        }
        let prefix = pack_jkey(gen::DREC_TYPE_DIR_REC, dir_ino);
        let entries = self.fs_tree().collect().map_err(fs_err_to_errno)?;
        let mut out = Vec::new();
        for e in entries {
            if e.key.len() < 8 {
                continue;
            }
            let hdr = u64::from_le_bytes(e.key[..8].try_into().unwrap());
            if hdr != prefix {
                continue;
            }
            let dentry = gen::nolx_direntry_t::read_from(&e.val).ok_or(errno::EIO)?;
            let name_len = usize::from(dentry.name_len);
            let hdr_size = gen::nolx_direntry_t::DISK_SIZE;
            if e.val.len() < hdr_size + name_len {
                return Err(errno::EIO);
            }
            let name_str = core::str::from_utf8(&e.val[hdr_size..hdr_size + name_len])
                .map_err(|_| errno::EIO)?;
            out.push((String::from(name_str), dentry.ino));
        }
        Ok(out)
    }

    /// Create a subdirectory named `name` inside `dir_ino` and persist it in the `fs-tree`.
    pub fn mkdir(
        &mut self,
        dir_ino: u64,
        name: &str,
        mode: Mode,
        uid: Uid,
        gid: Gid,
    ) -> Result<u64, i32> {
        validate_name(name)?;
        let mut parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(errno::ENOTDIR);
        }
        let dkey = dentry_key(dir_ino, name);
        if self.btree_search(&dkey)?.is_some() {
            return Err(errno::EEXIST);
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        let child_inode = gen::nolx_inode_t {
            parent_id: dir_ino,
            private_id: ino,
            create_time: 0,
            modify_time: 0,
            change_time: 0,
            access_time: 0,
            internal_flags: 0,
            nlink: 2,
            mode: ((S_IFDIR | (mode & 0o7777)) & 0xFFFF) as u16,
            pad: 0,
            owner: uid,
            group: gid,
            generation: 1,
            pad2: 0,
            data_stream: [0; 48],
        };

        parent.nlink = parent.nlink.saturating_add(1);
        let dval = dentry_val(ino, name, (S_IFDIR >> 12) as u16);
        self.fs_tree()
            .insert(&inode_key(dir_ino), parent.as_bytes())
            .map_err(fs_err_to_errno)?;
        self.fs_tree()
            .insert(&inode_key(ino), child_inode.as_bytes())
            .map_err(fs_err_to_errno)?;
        self.fs_tree()
            .insert(&dkey, &dval)
            .map_err(fs_err_to_errno)?;
        Ok(ino)
    }

    /// Create or look up a regular file named `name` inside `dir_ino`.
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
        let parent = self.read_inode(dir_ino)?;
        if !is_dir(u32::from(parent.mode)) {
            return Err(errno::ENOTDIR);
        }
        let dkey = dentry_key(dir_ino, name);
        if let Some(existing_val) = self.btree_search(&dkey)? {
            if exclusive {
                return Err(errno::EEXIST);
            }
            let dentry = gen::nolx_direntry_t::read_from(&existing_val).ok_or(errno::EIO)?;
            let target = self.read_inode(dentry.ino)?;
            if is_dir(u32::from(target.mode)) {
                return Err(errno::EISDIR);
            }
            return Ok((dentry.ino, false));
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        let file_inode = gen::nolx_inode_t {
            parent_id: dir_ino,
            private_id: ino,
            create_time: 0,
            modify_time: 0,
            change_time: 0,
            access_time: 0,
            internal_flags: 0,
            nlink: 1,
            mode: ((S_IFREG | (mode & 0o7777)) & 0xFFFF) as u16,
            pad: 0,
            owner: uid,
            group: gid,
            generation: 1,
            pad2: 0,
            data_stream: [0; 48],
        };

        let dval = dentry_val(ino, name, (S_IFREG >> 12) as u16);
        self.fs_tree()
            .insert(&inode_key(ino), file_inode.as_bytes())
            .map_err(fs_err_to_errno)?;
        self.fs_tree()
            .insert(&dkey, &dval)
            .map_err(fs_err_to_errno)?;
        Ok((ino, true))
    }

    /// Write `data` to regular file `ino` starting at `offset`, allocating physical data blocks
    /// and updating its `FILE_EXTENT` + `INODE` records in the `fs-tree`.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        let mut inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(errno::EISDIR);
        }
        if !is_reg(u32::from(inode.mode)) {
            return Err(errno::EINVAL);
        }

        let current_size =
            usize::try_from(inode_file_size(&inode)).map_err(|_| errno::EOVERFLOW)?;
        let off = usize::try_from(offset).map_err(|_| errno::EINVAL)?;
        let new_end = off.checked_add(data.len()).ok_or(errno::EOVERFLOW)?;
        let mut full_bytes = vec![0u8; current_size.max(new_end)];
        if current_size > 0 {
            let _ = self.read(ino, 0, &mut full_bytes[..current_size])?;
        }
        full_bytes[off..new_end].copy_from_slice(data);

        let bs = self.dev.block_size();
        let blocks_needed = full_bytes.len().div_ceil(bs) as u64;
        let ext_key = extent_key(ino, 0);
        let existing_ext = self
            .btree_search(&ext_key)?
            .and_then(|v| gen::nolx_file_extent_val_t::read_from(&v));

        let phys_start = if let Some(ext) = existing_ext {
            let old_blocks = extent_len(ext.len_and_flags).div_ceil(bs as u64);
            if blocks_needed <= old_blocks
                && ext.phys_block_num >= self.info.layout.first_data_block
            {
                ext.phys_block_num
            } else {
                self.alloc_contiguous_blocks(blocks_needed)?
            }
        } else if blocks_needed > 0 {
            self.alloc_contiguous_blocks(blocks_needed)?
        } else {
            0
        };

        let mut block_buf = vec![0u8; bs];
        for blk_idx in 0..blocks_needed {
            block_buf.fill(0);
            let start = (blk_idx as usize) * bs;
            let end = (start + bs).min(full_bytes.len());
            block_buf[..end - start].copy_from_slice(&full_bytes[start..end]);
            self.dev
                .write_block(phys_start + blk_idx, &block_buf)
                .map_err(|_| errno::EIO)?;
        }

        let aligned_len = blocks_needed * (bs as u64);
        if !full_bytes.is_empty() {
            let ext_val = gen::nolx_file_extent_val_t {
                len_and_flags: pack_len_and_flags(aligned_len, 0),
                phys_block_num: phys_start,
                crypto_id: 0,
            };
            self.fs_tree()
                .insert(&ext_key, ext_val.as_bytes())
                .map_err(fs_err_to_errno)?;
        }

        set_inode_file_size(&mut inode, full_bytes.len() as u64, aligned_len);
        inode.generation = inode.generation.wrapping_add(1);
        self.fs_tree()
            .insert(&inode_key(ino), inode.as_bytes())
            .map_err(fs_err_to_errno)?;
        Ok(data.len())
    }

    /// Read up to `buf.len()` bytes from regular file `ino` starting at `offset` via its
    /// `FILE_EXTENT` physical block mapping.
    pub fn read(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let inode = self.read_inode(ino)?;
        if is_dir(u32::from(inode.mode)) {
            return Err(errno::EISDIR);
        }
        if !is_reg(u32::from(inode.mode)) {
            return Err(errno::EINVAL);
        }
        let file_size = inode_file_size(&inode);
        if offset >= file_size || buf.is_empty() {
            return Ok(0);
        }
        let avail = ((file_size - offset) as usize).min(buf.len());
        let ext_key = extent_key(ino, 0);
        let Some(ext_bytes) = self.btree_search(&ext_key)? else {
            buf[..avail].fill(0);
            return Ok(avail);
        };
        let ext = gen::nolx_file_extent_val_t::read_from(&ext_bytes).ok_or(errno::EIO)?;

        let bs = self.dev.block_size();
        let mut block_buf = vec![0u8; bs];
        let mut copied = 0usize;
        while copied < avail {
            let cur_off = offset as usize + copied;
            let blk_idx = (cur_off / bs) as u64;
            let blk_off = cur_off % bs;
            let chunk = (bs - blk_off).min(avail - copied);
            self.dev
                .read_block(ext.phys_block_num + blk_idx, &mut block_buf)
                .map_err(|_| errno::EIO)?;
            buf[copied..copied + chunk].copy_from_slice(&block_buf[blk_off..blk_off + chunk]);
            copied += chunk;
        }
        Ok(copied)
    }

    /// Query POSIX [`Stat`] metadata for `ino`.
    pub fn stat(&self, dev_id: u64, ino: u64) -> Result<Stat, i32> {
        let inode = self.read_inode(ino)?;
        let mode = u32::from(inode.mode);
        let size = inode_file_size(&inode) as i64;
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

    fn btree_search(&self, key: &[u8]) -> Result<Option<Vec<u8>>, i32> {
        let root = self.info.layout.fs_tree_root;
        let mut block = root;
        let mut buf = vec![0u8; self.dev.block_size()];
        loop {
            self.dev
                .read_block(block, &mut buf)
                .map_err(|_| errno::EIO)?;
            let expected = if block == root {
                gen::OBJ_BTREE_ROOT
            } else {
                gen::OBJ_BTREE_NODE
            };
            let node = Node::decode(&buf, expected).map_err(fs_err_to_errno)?;
            let index = match node.find(KeyKind::JKey, key) {
                Ok(i) => i,
                Err(_) if node.is_leaf() => return Ok(None),
                Err(0) => return Ok(None),
                Err(i) => i - 1,
            };
            if node.is_leaf() {
                return Ok(Some(node.entries[index].val.clone()));
            }
            block = node.entries[index].child_block();
        }
    }

    fn alloc_contiguous_blocks(&mut self, count: u64) -> Result<u64, i32> {
        let start = self.alloc.next_block;
        let end = start.checked_add(count).ok_or(errno::ENOSPC)?;
        if end > self.alloc.block_count {
            return Err(errno::ENOSPC);
        }
        self.alloc.next_block = end;
        Ok(start)
    }

    fn fs_tree(&mut self) -> BTree<'_, D, LinearBlockAlloc> {
        BTree::new(
            &mut self.dev,
            &mut self.alloc,
            self.info.layout.fs_tree_root,
            KeyKind::JKey,
            gen::SUB_FSTREE as u16,
            1,
        )
    }
}

fn inode_file_size(inode: &gen::nolx_inode_t) -> u64 {
    gen::nolx_data_stream_t::read_from(&inode.data_stream).map_or(0, |ds| ds.total_bytes_written)
}

fn set_inode_file_size(inode: &mut gen::nolx_inode_t, logical_size: u64, alloced_size: u64) {
    let ds = gen::nolx_data_stream_t {
        xattr_jhdr: [0; 16],
        alloced_size,
        default_crypto_id: 0,
        total_bytes_written: logical_size,
        total_bytes_read: 0,
    };
    inode.data_stream.copy_from_slice(ds.as_bytes());
}

fn inode_key(ino: u64) -> Vec<u8> {
    let k = gen::nolx_j_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_INODE, ino),
    };
    k.as_bytes().to_vec()
}

fn dentry_key(parent_ino: u64, name: &str) -> Vec<u8> {
    let hdr = gen::nolx_dentry_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_DIR_REC, parent_ino),
        name_len: name.len() as u16,
        pad: 0,
    };
    let mut bytes = Vec::with_capacity(gen::nolx_dentry_key_t::DISK_SIZE + name.len());
    bytes.extend_from_slice(hdr.as_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn dentry_val(child_ino: u64, name: &str, dt_flags: u16) -> Vec<u8> {
    let val = gen::nolx_direntry_t {
        ino: child_ino,
        name_len: name.len() as u16,
        flags: dt_flags,
        nlen: name.len() as u16,
        pad: 0,
    };
    let mut bytes = Vec::with_capacity(gen::nolx_direntry_t::DISK_SIZE + name.len());
    bytes.extend_from_slice(val.as_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn extent_key(ino: u64, logical_addr: u64) -> Vec<u8> {
    let k = gen::nolx_file_extent_key_t {
        obj_id_and_type: pack_jkey(gen::DREC_TYPE_FILE_EXTENT, ino),
        logical_addr,
    };
    k.as_bytes().to_vec()
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

const fn fs_err_to_errno(err: FsError) -> i32 {
    match err {
        FsError::BlockOutOfRange(_) => errno::ENOSPC,
        FsError::InvalidField(_) => errno::EINVAL,
        _ => errno::EIO,
    }
}

const _: Mode = S_IFMT;

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

        // Unmount back to raw block device, then mount fresh and verify Fletcher-64 + contents.
        let dev = vol.into_device();
        let remounted = NolxfsVolume::mount(dev).unwrap();
        let etc2 = remounted.lookup(NOLXFS_ROOT_INO, "etc").unwrap();
        let f2 = remounted.lookup(etc2, "hostname").unwrap();
        assert_eq!(f2, f_ino);

        let mut buf = [0u8; 16];
        let n = remounted.read(f2, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"nolx-host\n");
    }
}
