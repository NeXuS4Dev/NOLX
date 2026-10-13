//! `nolxfs-dump`: read-only inspection of a container without mounting it.
//!
//! [`dump_to_string`] renders the container, checkpoint, space-manager and
//! directory-tree state as text. It never writes to the device, which makes it
//! safe to run against live images and the starting point for the FUSE-less
//! host mounter shipped as the `nolxfs-dump` tool.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::blockdev::BlockDevice;
use crate::btree;
use crate::checkpoint;
use crate::mkfs;
use crate::on_disk::{extent_len, jkey_oid, jkey_type, Result};
use crate::on_disk_gen as gen;
use crate::spaceman::SpaceMan;
use crate::Pod;

/// Render a full text dump of the container on `dev`.
///
/// Works on checkpointed and genesis (mkfs-only) containers alike; per-file
/// extents and xattrs are listed, and snapshot trees are summarised but not
/// expanded (they share most blocks with the live tree).
///
/// # Errors
///
/// Device or corruption errors that make the container unreadable.
pub fn dump_to_string<D: BlockDevice>(dev: &mut D) -> Result<String> {
    let info = mkfs::probe(dev)?;
    let layout = info.layout;
    let mut out = String::new();
    out.push_str(&format!(
        "container: {} blocks x {} bytes ({} bytes)\n",
        layout.block_count,
        layout.block_size,
        layout.block_count * layout.block_size,
    ));
    out.push_str(&format!(
        "  checkpoint ring: descriptors {} blocks at {}, data {} blocks at {}\n",
        layout.xp_desc_blocks, layout.xp_desc_base, layout.xp_data_blocks, layout.xp_data_base,
    ));
    out.push_str(&format!(
        "  spaceman: block {}, reaper: block {}, volume superblock: block {}\n",
        layout.spaceman, layout.reaper, layout.volume_sb,
    ));
    out.push_str(&format!(
        "  first data block: {}, next xid (container): {}\n",
        layout.first_data_block, info.sb.next_xid,
    ));

    let found = checkpoint::find_latest(dev, &layout)?;
    let (fs_root, state) = match &found {
        Some(f) => {
            out.push_str(&format!(
                "checkpoint: xid {} at descriptor block {} (csb {}, vsb {})\n",
                f.state.xid, f.desc_block, f.csb_block, f.vsb_block,
            ));
            (f.state.fs_root, Some(&f.state))
        }
        None => {
            out.push_str("checkpoint: none (genesis state from mkfs)\n");
            (layout.fs_tree_root, None)
        }
    };

    if let Ok(spaceman) = SpaceMan::open(dev, &layout) {
        let space = spaceman.space_layout();
        out.push_str(&format!(
            "spaceman: {} chunks, free {} / {} data blocks\n",
            space.chunk_count,
            spaceman.free_count(),
            layout.block_count - space.alloc_start,
        ));
    } else {
        out.push_str("spaceman: not initialised\n");
    }

    if let Some(state) = state {
        out.push_str(&format!(
            "volume: {} files, {} directories, {} symlinks; next inode {}\n",
            state.num_files, state.num_dirs, state.num_symlinks, state.next_ino,
        ));
        let backlog: u64 = state.reap.iter().map(|e| e.len).sum();
        out.push_str(&format!("reaper backlog: {backlog} blocks\n"));
        for snap in &state.snapshots {
            out.push_str(&format!(
                "snapshot: {:?} taken at xid {} ({} files, {} dirs)\n",
                snap.name, snap.xid, snap.num_files, snap.num_dirs,
            ));
        }
    }

    out.push_str("tree:\n");
    dump_tree(dev, fs_root, &mut out)?;
    Ok(out)
}

fn dump_tree<D: BlockDevice>(dev: &D, fs_root: u64, out: &mut String) -> Result<()> {
    let entries = btree::walk::collect(dev, fs_root)?;
    let mut inodes: BTreeMap<u64, gen::nolx_inode_t> = BTreeMap::new();
    let mut children: BTreeMap<u64, Vec<(String, u64)>> = BTreeMap::new();
    let mut extents: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
    let mut xattrs: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for entry in &entries {
        if entry.key.len() < 8 {
            continue;
        }
        let raw = u64::from_le_bytes(entry.key[..8].try_into().unwrap_or([0u8; 8]));
        match jkey_type(raw) {
            gen::DREC_TYPE_INODE => {
                if let Some(inode) = gen::nolx_inode_t::read_from(&entry.val) {
                    inodes.insert(jkey_oid(raw), inode);
                }
            }
            gen::DREC_TYPE_DIR_REC => {
                if let Some(dentry) = gen::nolx_direntry_t::read_from(&entry.val) {
                    let name_len = usize::from(dentry.name_len);
                    let name = entry
                        .val
                        .get(gen::nolx_direntry_t::DISK_SIZE..gen::nolx_direntry_t::DISK_SIZE + name_len)
                        .and_then(|b| core::str::from_utf8(b).ok())
                        .unwrap_or("")
                        .to_owned();
                    children.entry(jkey_oid(raw)).or_default().push((name, dentry.ino));
                }
            }
            gen::DREC_TYPE_FILE_EXTENT => {
                if let Some(val) = gen::nolx_file_extent_val_t::read_from(&entry.val) {
                    let logical = u64::from_le_bytes(
                        entry.key.get(8..16).and_then(|b| <[u8; 8]>::try_from(b).ok()).unwrap_or([0u8; 8]),
                    );
                    extents
                        .entry(jkey_oid(raw))
                        .or_default()
                        .push((logical, extent_len(val.len_and_flags)));
                }
            }
            gen::DREC_TYPE_XATTR => {
                if let Some(xattr) = gen::nolx_xattr_rec_t::read_from(&entry.val) {
                    let name_len = usize::from(xattr.name_len);
                    let name = entry
                        .val
                        .get(gen::nolx_xattr_rec_t::DISK_SIZE..gen::nolx_xattr_rec_t::DISK_SIZE + name_len)
                        .and_then(|b| core::str::from_utf8(b).ok())
                        .unwrap_or("")
                        .to_owned();
                    xattrs.entry(jkey_oid(raw)).or_default().push(name);
                }
            }
            _ => {}
        }
    }
    let mut visiting = Vec::new();
    dump_dir(gen::ROOT_INO, "/", &inodes, &children, &extents, &xattrs, out, &mut visiting);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dump_dir(
    ino: u64,
    path: &str,
    inodes: &BTreeMap<u64, gen::nolx_inode_t>,
    children: &BTreeMap<u64, Vec<(String, u64)>>,
    extents: &BTreeMap<u64, Vec<(u64, u64)>>,
    xattrs: &BTreeMap<u64, Vec<String>>,
    out: &mut String,
    visiting: &mut Vec<u64>,
) {
    let Some(inode) = inodes.get(&ino) else {
        out.push_str(&format!("{path} <inode {ino} missing>\n"));
        return;
    };
    let mode = u32::from(inode.mode);
    let kind = match mode & 0o170_000 {
        0o040_000 => "dir ",
        0o120_000 => "link",
        _ => "file",
    };
    out.push_str(&format!(
        "{path} [{kind} ino={ino} mode={mode:o} nlink={} size={}]",
        inode.nlink,
        read_size(inode),
    ));
    if let Some(list) = extents.get(&ino) {
        let total: u64 = list.iter().map(|(_, len)| len).sum();
        out.push_str(&format!(" extents={} ({} bytes)", list.len(), total));
    }
    if let Some(list) = xattrs.get(&ino) {
        out.push_str(&format!(" xattrs={:?}", list));
    }
    out.push('\n');
    if mode & 0o170_000 != 0o040_000 || visiting.contains(&ino) {
        return;
    }
    visiting.push(ino);
    if let Some(list) = children.get(&ino) {
        let mut sorted: Vec<&(String, u64)> = list.iter().collect();
        sorted.sort();
        for (name, child) in sorted {
            let child_path = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            dump_dir(*child, &child_path, inodes, children, extents, xattrs, out, visiting);
        }
    }
    visiting.pop();
}

fn read_size(inode: &gen::nolx_inode_t) -> u64 {
    gen::nolx_data_stream_t::read_from(&inode.data_stream)
        .map_or(0, |ds| ds.total_bytes_written)
}
