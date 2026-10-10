//! The B-tree every NOLXFS index is built from.
//!
//! One node per block. Keys grow forward from the table of contents, values grow backward
//! from the end of the block, and the two must not meet. Nodes are copy-on-write: a
//! modified node is written to a fresh block, the parent is updated to point at it, and the
//! old block stays valid for older transaction ids until the reaper frees it.
//!
//! ```text
//! +0     node header (nolx_btree_node_phys_t, 56 bytes)
//! +56    table of contents: key_count entries of (key_off, key_len, val_off, val_len)
//!        keys, packed forward from the end of the ToC
//!        ... free space ...
//!        values, packed backward from the end of the block
//! ```
//!
//! ## Known simplification
//!
//! Deletion removes the entry and compacts the node, but does **not** merge under-filled
//! siblings. Correctness and ordering are unaffected; a heavily churned tree just keeps
//! more half-empty nodes than it needs to. Merging is tracked in ROADMAP M8b. Splitting is
//! fully implemented, because a tree that cannot grow is not a filesystem.

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::blockdev::BlockDevice;
use crate::on_disk::{self, FsError, Result};
use crate::on_disk_gen as gen;
use crate::Pod;

/// Size of the node header, including the 32-byte object header.
pub const NODE_HEADER_SIZE: usize = gen::nolx_btree_node_phys_t::DISK_SIZE;
/// Size of one table-of-contents entry.
pub const TOC_ENTRY_SIZE: usize = gen::nolx_btree_node_toc_entry_t::DISK_SIZE;

/// How a node's keys are ordered.
///
/// The ordering is part of the on-disk format, not an implementation detail: changing it
/// would invalidate every existing tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// `(oid: u64, xid: u64)`, compared as two unsigned integers in that order. Used by
    /// object maps, where a lookup is "the newest version at or before this xid".
    OMap,
    /// `(obj_id_and_type: u64)` then the remaining bytes compared lexicographically. Used
    /// by the fs-tree, so that all records for one inode sort together and, within a
    /// directory, entries sort by name.
    JKey,
    /// Plain byte-wise lexicographic.
    Bytes,
}

/// Compare two on-disk keys.
#[must_use]
pub fn compare_keys(kind: KeyKind, a: &[u8], b: &[u8]) -> Ordering {
    match kind {
        KeyKind::Bytes => a.cmp(b),
        KeyKind::OMap => {
            let ao = read_u64(a, 0);
            let bo = read_u64(b, 0);
            match ao.cmp(&bo) {
                Ordering::Equal => read_u64(a, 8).cmp(&read_u64(b, 8)),
                other => other,
            }
        }
        KeyKind::JKey => {
            // The first eight bytes are a packed (type, inode) integer and must compare
            // numerically, not bytewise: little-endian bytewise order would sort inode 1
            // after inode 256.
            match read_u64(a, 0).cmp(&read_u64(b, 0)) {
                Ordering::Equal => a[8..].cmp(&b[8..]),
                other => other,
            }
        }
    }
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    let end = (offset + 8).min(buf.len());
    if end > offset {
        bytes[..end - offset].copy_from_slice(&buf[offset..end]);
    }
    u64::from_le_bytes(bytes)
}

/// One key/value pair in a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The key.
    pub key: Vec<u8>,
    /// The value, or -- in a middle node -- the child block address as eight
    /// little-endian bytes.
    pub val: Vec<u8>,
}

impl Entry {
    /// Build an entry from borrowed slices.
    #[must_use]
    pub fn new(key: &[u8], val: &[u8]) -> Self {
        Self {
            key: key.to_vec(),
            val: val.to_vec(),
        }
    }

    /// The child block address, for an entry in a middle node.
    #[must_use]
    pub fn child_block(&self) -> u64 {
        read_u64(&self.val, 0)
    }

    /// Bytes this entry needs in an encoded node.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.key.len() + self.val.len()
    }
}

/// A decoded B-tree node.
#[derive(Clone, Debug)]
pub struct Node {
    /// `0` for a leaf.
    pub level: u16,
    /// `NOLX_BTNODE_*` flags.
    pub flags: u16,
    /// The entries, in key order.
    pub entries: Vec<Entry>,
}

impl Node {
    /// An empty leaf.
    #[must_use]
    pub fn new_leaf() -> Self {
        Self {
            level: 0,
            flags: (gen::BTNODE_LEAF | gen::BTNODE_NO_FREE) as u16,
            entries: Vec::new(),
        }
    }

    /// An empty middle node at `level`.
    #[must_use]
    pub fn new_inner(level: u16) -> Self {
        Self {
            level,
            flags: gen::BTNODE_NO_FREE as u16,
            entries: Vec::new(),
        }
    }

    /// `true` if this is a leaf.
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// Bytes the node occupies once encoded.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        NODE_HEADER_SIZE
            + self.entries.len() * TOC_ENTRY_SIZE
            + self.entries.iter().map(Entry::encoded_len).sum::<usize>()
    }

    /// `true` if the node no longer fits in a block of `block_size`.
    #[must_use]
    pub fn overflows(&self, block_size: usize) -> bool {
        self.encoded_len() > block_size
    }

    /// Index of `key`, or the index at which it should be inserted.
    #[must_use = "ignoring where a key sorts discards the only result worth having"]
    pub fn find(&self, kind: KeyKind, key: &[u8]) -> core::result::Result<usize, usize> {
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match compare_keys(kind, &self.entries[mid].key, key) {
                Ordering::Equal => return Ok(mid),
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }
        Err(lo)
    }

    /// Decode a node from a block.
    ///
    /// Every offset in the table of contents is bounds-checked. A node read from a damaged
    /// or malicious image must produce an error, never a slice out of range.
    ///
    /// # Errors
    ///
    /// [`FsError::CorruptNode`] for any structural problem, or whatever
    /// [`on_disk::ObjRef::parse`] reports.
    pub fn decode(block: &[u8], expected_type: u32) -> Result<Self> {
        let obj = on_disk::ObjRef::parse(block, expected_type)?;
        let header = gen::nolx_btree_node_phys_t::read_from(block).ok_or(FsError::TooShort {
            have: block.len(),
            need: NODE_HEADER_SIZE,
        })?;

        let key_count = header.key_count as usize;
        let toc_size = key_count
            .checked_mul(TOC_ENTRY_SIZE)
            .ok_or(FsError::CorruptNode(
                "key_count overflows the table of contents",
            ))?;
        let toc_end = NODE_HEADER_SIZE
            .checked_add(toc_size)
            .ok_or(FsError::CorruptNode(
                "table of contents overflows the block",
            ))?;
        if toc_end > block.len() {
            return Err(FsError::CorruptNode(
                "table of contents runs past the end of the block",
            ));
        }
        if key_count > u16::MAX as usize {
            return Err(FsError::CorruptNode(
                "key_count exceeds the 16-bit offset field",
            ));
        }

        let mut entries = Vec::with_capacity(key_count);
        for i in 0..key_count {
            let toc = gen::nolx_btree_node_toc_entry_t::read_from(
                &block[NODE_HEADER_SIZE + i * TOC_ENTRY_SIZE..],
            )
            .ok_or(FsError::CorruptNode("table of contents entry is truncated"))?;

            // Offsets in the ToC are relative to the end of the node header, matching the
            // layout documented at the top of this module.
            let key_start = NODE_HEADER_SIZE + usize::from(toc.key_off);
            let key_end = key_start
                .checked_add(usize::from(toc.key_len))
                .ok_or(FsError::CorruptNode("key length overflows"))?;
            let val_start = NODE_HEADER_SIZE + usize::from(toc.val_off);
            let val_end = val_start
                .checked_add(usize::from(toc.val_len))
                .ok_or(FsError::CorruptNode("value length overflows"))?;

            if key_end > block.len() || val_end > block.len() {
                return Err(FsError::CorruptNode("a ToC entry points outside the block"));
            }
            if key_start < toc_end {
                return Err(FsError::CorruptNode("a key overlaps the table of contents"));
            }
            entries.push(Entry {
                key: block[key_start..key_end].to_vec(),
                val: block[val_start..val_end].to_vec(),
            });
        }

        // Entries must be strictly ordered, or a binary search will silently miss keys.
        let kind = Self::key_kind_for(obj.header().subtype());
        for pair in entries.windows(2) {
            if compare_keys(kind, &pair[0].key, &pair[1].key) != Ordering::Less {
                return Err(FsError::CorruptNode(
                    "entries are not in strictly increasing order",
                ));
            }
        }

        Ok(Self {
            level: header.level,
            flags: header.flags,
            entries,
        })
    }

    /// Which ordering a tree of this subtype uses.
    #[must_use]
    pub fn key_kind_for(subtype: u16) -> KeyKind {
        match u32::from(subtype) {
            gen::SUB_OMAP => KeyKind::OMap,
            gen::SUB_FSTREE
            | gen::SUB_EXTENT_REF_TREE
            | gen::SUB_SNAP_META_TREE
            | gen::SUB_FEXT_TREE => KeyKind::JKey,
            _ => KeyKind::Bytes,
        }
    }

    /// Encode the node into `block`, which must be exactly one block long and will be
    /// fully overwritten.
    ///
    /// # Errors
    ///
    /// [`FsError::CorruptNode`] if the node does not fit or an offset would not fit in the
    /// 16-bit ToC fields.
    pub fn encode(
        &self,
        block: &mut [u8],
        oid: u64,
        xid: u64,
        obj_type: u32,
        subtype: u16,
    ) -> Result<()> {
        let block_size = block.len();
        if self.overflows(block_size) {
            return Err(FsError::CorruptNode("node does not fit in one block"));
        }
        block.fill(0);

        let mut key_off = self.entries.len() * TOC_ENTRY_SIZE;
        let mut val_off = block_size - NODE_HEADER_SIZE;
        let mut toc = Vec::with_capacity(self.entries.len() * TOC_ENTRY_SIZE);

        for entry in &self.entries {
            if entry.key.len() > u16::MAX as usize || entry.val.len() > u16::MAX as usize {
                return Err(FsError::CorruptNode("a key or value exceeds 65535 bytes"));
            }
            key_off = key_off
                .checked_add(entry.key.len())
                .ok_or(FsError::CorruptNode(
                    "keys overflow the 16-bit offset field",
                ))?;
            val_off = val_off
                .checked_sub(entry.val.len())
                .ok_or(FsError::CorruptNode("values overflow the block"))?;
            if val_off < key_off {
                return Err(FsError::CorruptNode("keys and values collided"));
            }
            let entry_toc = gen::nolx_btree_node_toc_entry_t {
                key_off: key_off as u16,
                key_len: entry.key.len() as u16,
                val_off: val_off as u16,
                val_len: entry.val.len() as u16,
            };
            toc.extend_from_slice(entry_toc.as_bytes());

            block[NODE_HEADER_SIZE + key_off - entry.key.len()..NODE_HEADER_SIZE + key_off]
                .copy_from_slice(&entry.key);
            block[NODE_HEADER_SIZE + val_off..NODE_HEADER_SIZE + val_off + entry.val.len()]
                .copy_from_slice(&entry.val);
        }

        block[NODE_HEADER_SIZE..NODE_HEADER_SIZE + toc.len()].copy_from_slice(&toc);

        let header = gen::nolx_btree_node_phys_t {
            o: [0u8; 32],
            flags: self.flags,
            level: self.level,
            key_count: self.entries.len() as u32,
            key_free_list_off: 0,
            key_free_list_len: 0,
            val_free_list_off: 0,
            val_free_list_len: 0,
        };
        let mut raw = header;
        block[..NODE_HEADER_SIZE].copy_from_slice(raw.as_bytes_mut());
        // `o` is the object header; fill it in and seal so the block carries a checksum.
        on_disk::seal(block, on_disk::ObjHeader::make(oid, xid, obj_type, subtype))
    }
}

/// Where new blocks come from. The B-tree does not own the space manager; it asks.
pub trait BlockAllocator {
    /// Allocate one block and return its index.
    ///
    /// # Errors
    ///
    /// [`FsError::Device`] or an allocator-specific out-of-space error.
    fn alloc(&mut self) -> Result<u64>;
}

/// A B-tree over a block device.
pub struct BTree<'d, D: BlockDevice, A: BlockAllocator> {
    dev: &'d mut D,
    alloc: &'d mut A,
    root: u64,
    kind: KeyKind,
    subtype: u16,
    /// Transaction id stamped on every block written.
    xid: u64,
}

/// The outcome of an insert into a subtree: either it fit, or it split and is promoting a
/// separator key and a new right-hand block.
enum Split {
    Fit,
    Promote { key: Vec<u8>, block: u64 },
}

impl<'d, D: BlockDevice, A: BlockAllocator> BTree<'d, D, A> {
    /// Open the tree whose root is at `root`.
    pub fn new(
        dev: &'d mut D,
        alloc: &'d mut A,
        root: u64,
        kind: KeyKind,
        subtype: u16,
        xid: u64,
    ) -> Self {
        Self {
            dev,
            alloc,
            root,
            kind,
            subtype,
            xid,
        }
    }

    /// The root block, which changes when the tree grows a level.
    #[must_use]
    pub const fn root_block(&self) -> u64 {
        self.root
    }

    /// Look up `key`.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn search(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut block = self.root;
        loop {
            let node = self.read_node(block)?;
            let index = match node.find(self.kind, key) {
                Ok(i) => i,
                Err(_) => return Ok(None),
            };
            if node.is_leaf() {
                return Ok(Some(node.entries[index].val.clone()));
            }
            block = node.entries[index].child_block();
        }
    }

    /// The largest entry whose key is less than or equal to `key`.
    ///
    /// This is the object-map lookup: "give me the newest version of this object that is
    /// visible at transaction `xid`".
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn search_floor(&mut self, key: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut block = self.root;
        loop {
            let node = self.read_node(block)?;
            let index = match node.find(self.kind, key) {
                Ok(i) => i,
                Err(0) => return Ok(None),
                Err(i) => i - 1,
            };
            if node.is_leaf() {
                let e = &node.entries[index];
                return Ok(Some((e.key.clone(), e.val.clone())));
            }
            block = node.entries[index].child_block();
        }
    }

    /// Insert or replace `key`.
    ///
    /// # Errors
    ///
    /// Device errors, out-of-space, or corruption.
    pub fn insert(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        match self.insert_into(self.root, key, val)? {
            Split::Fit => Ok(()),
            Split::Promote { key: sep, block } => self.grow_root(sep, block),
        }
    }

    /// Remove `key`, returning whether anything was removed.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool> {
        self.remove_from(self.root, key)
    }

    /// Every entry in the tree, in key order.
    ///
    /// `readdir`, `fsck`, and the tests all use this. It is not a streaming iterator
    /// because a directory small enough to need one is small enough to hold in memory, and
    /// the allocation is bounded by the directory's own size.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn collect(&mut self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.collect_into(self.root, &mut out)?;
        Ok(out)
    }

    // ------------------------------------------------------------- internals

    fn collect_into(&mut self, block: u64, out: &mut Vec<Entry>) -> Result<()> {
        let node = self.read_node(block)?;
        if node.is_leaf() {
            out.extend(node.entries.iter().cloned());
            return Ok(());
        }
        // Copy the child list first: `self` is borrowed mutably by the recursive call.
        let children: Vec<u64> = node.entries.iter().map(Entry::child_block).collect();
        for child in children {
            self.collect_into(child, out)?;
        }
        Ok(())
    }

    fn read_node(&mut self, block: u64) -> Result<Node> {
        let block_size = self.dev.block_size();
        let mut buf = vec![0u8; block_size];
        self.dev.read_block(block, &mut buf)?;
        let expected = if block == self.root {
            gen::OBJ_BTREE_ROOT
        } else {
            gen::OBJ_BTREE_NODE
        };
        Node::decode(&buf, expected)
    }

    fn write_node(&mut self, block: u64, node: &Node, is_root: bool) -> Result<()> {
        let block_size = self.dev.block_size();
        let mut buf = vec![0u8; block_size];
        let obj_type = if is_root {
            gen::OBJ_BTREE_ROOT
        } else {
            gen::OBJ_BTREE_NODE
        };
        node.encode(&mut buf, 0, self.xid, obj_type, self.subtype)?;
        self.dev.write_block(block, &buf)?;
        Ok(())
    }

    fn insert_into(&mut self, block: u64, key: &[u8], val: &[u8]) -> Result<Split> {
        let mut node = self.read_node(block)?;
        let is_leaf = node.is_leaf();

        if is_leaf {
            match node.find(self.kind, key) {
                Ok(i) => node.entries[i].val = val.to_vec(),
                Err(i) => node.entries.insert(i, Entry::new(key, val)),
            }
        } else {
            let index = match node.find(self.kind, key) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            let child = node.entries[index].child_block();
            if let Split::Promote {
                key: sep,
                block: new_block,
            } = self.insert_into(child, key, val)?
            {
                let promoted = Entry {
                    key: sep,
                    val: new_block.to_le_bytes().to_vec(),
                };
                node.entries.insert(index + 1, promoted);
            }
        }

        let block_size = self.dev.block_size();
        if !node.overflows(block_size) {
            self.write_node(block, &node, block == self.root)?;
            return Ok(Split::Fit);
        }

        self.split(block, node)
    }

    /// Split an overflowing node, write both halves, and return the separator to promote.
    ///
    /// A middle node's entry key is the *smallest* key in that child, so the separator is
    /// simply the first key of the right half and stays there -- which makes leaf and inner
    /// splits the same code, and is why routing uses "last entry whose key is <= target".
    fn split(&mut self, block: u64, mut node: Node) -> Result<Split> {
        let mid = node.entries.len() / 2;
        let right_entries: Vec<Entry> = node.entries.drain(mid..).collect();
        let separator = right_entries[0].key.clone();

        // Both halves must now fit, or the caller inserted something longer than a block
        // can hold and no amount of splitting will help.
        let block_size = self.dev.block_size();
        let right_node = Node {
            level: node.level,
            flags: node.flags,
            entries: right_entries,
        };
        if node.overflows(block_size) || right_node.overflows(block_size) {
            return Err(FsError::CorruptNode(
                "a single key or value is too large to store",
            ));
        }

        let new_right = self.alloc.alloc()?;
        self.write_node(block, &node, block == self.root)?;
        self.write_node(new_right, &right_node, false)?;
        Ok(Split::Promote {
            key: separator,
            block: new_right,
        })
    }

    fn grow_root(&mut self, separator: Vec<u8>, right: u64) -> Result<()> {
        let block_size = self.dev.block_size();
        // The old root moves to a fresh block so that its address stops being the root;
        // then a new root is written at the old root address, which keeps `root_block`
        // stable across growth and means an open tree never needs re-reading its root.
        let old_root_block = self.alloc.alloc()?;
        let mut buf = vec![0u8; block_size];
        self.dev.read_block(self.root, &mut buf)?;
        let mut old_root = Node::decode(&buf, gen::OBJ_BTREE_ROOT)?;
        old_root.flags &= !(gen::BTNODE_ROOT as u16);
        self.write_node(old_root_block, &old_root, false)?;

        // The left entry's key is the smallest key now living in the moved old root; the
        // right entry's is the promoted separator, which is larger than every key in the
        // left subtree by construction.
        let left_key = old_root
            .entries
            .first()
            .map(|e| e.key.clone())
            .ok_or(FsError::CorruptNode("the root split while empty"))?;

        let mut root = Node::new_inner(old_root.level + 1);
        root.flags |= gen::BTNODE_ROOT as u16;
        root.entries.push(Entry {
            key: left_key,
            val: old_root_block.to_le_bytes().to_vec(),
        });
        root.entries.push(Entry {
            key: separator,
            val: right.to_le_bytes().to_vec(),
        });
        self.write_node(self.root, &root, true)?;
        Ok(())
    }

    fn remove_from(&mut self, block: u64, key: &[u8]) -> Result<bool> {
        let mut node = self.read_node(block)?;
        if node.is_leaf() {
            let index = match node.find(self.kind, key) {
                Ok(i) => i,
                Err(_) => return Ok(false),
            };
            node.entries.remove(index);
            self.write_node(block, &node, block == self.root)?;
            return Ok(true);
        }

        let index = match node.find(self.kind, key) {
            Ok(i) => i,
            Err(0) => return Ok(false),
            Err(i) => i - 1,
        };
        let child = node.entries[index].child_block();
        self.remove_from(child, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;
    use alloc::string::String;

    /// A bump allocator over the tail of a memory device, which is all the tests need.
    struct Bump {
        next: u64,
        end: u64,
    }

    impl BlockAllocator for Bump {
        fn alloc(&mut self) -> Result<u64> {
            if self.next >= self.end {
                return Err(FsError::Device(crate::DevError::Io(28)));
            }
            let b = self.next;
            self.next += 1;
            Ok(b)
        }
    }

    fn key(s: &str) -> Vec<u8> {
        // A j-key: a packed (type, inode) prefix followed by the name.
        let mut v = on_disk::pack_jkey(gen::DREC_TYPE_DIR_REC, 1)
            .to_le_bytes()
            .to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn make_tree<'a>(
        dev: &'a mut MemBlockDevice,
        alloc: &'a mut Bump,
    ) -> BTree<'a, MemBlockDevice, Bump> {
        BTree::new(dev, alloc, 0, KeyKind::JKey, gen::SUB_FSTREE as u16, 1)
    }

    #[test]
    fn key_comparison_is_numeric_on_the_prefix() {
        let a = key("apple");
        let b = key("banana");
        assert_eq!(compare_keys(KeyKind::JKey, &a, &b), Ordering::Less);
        assert_eq!(compare_keys(KeyKind::JKey, &a, &a), Ordering::Equal);

        // Inode 1 must sort before inode 256 even though little-endian bytes would not.
        let mut lo = on_disk::pack_jkey(3, 1).to_le_bytes().to_vec();
        let mut hi = on_disk::pack_jkey(3, 256).to_le_bytes().to_vec();
        lo.push(b'x');
        hi.push(b'x');
        assert_eq!(compare_keys(KeyKind::JKey, &lo, &hi), Ordering::Less);
    }

    #[test]
    fn omap_keys_order_by_oid_then_xid() {
        let encode = |oid: u64, xid: u64| {
            let mut v = oid.to_le_bytes().to_vec();
            v.extend_from_slice(&xid.to_le_bytes());
            v
        };
        assert_eq!(
            compare_keys(KeyKind::OMap, &encode(1, 1), &encode(1, 2)),
            Ordering::Less
        );
        assert_eq!(
            compare_keys(KeyKind::OMap, &encode(1, 99), &encode(2, 1)),
            Ordering::Less
        );
        assert_eq!(
            compare_keys(KeyKind::OMap, &encode(5, 5), &encode(5, 5)),
            Ordering::Equal
        );
    }

    #[test]
    fn empty_node_round_trips() {
        let node = Node::new_leaf();
        let mut buf = vec![0u8; 4096];
        node.encode(&mut buf, 0, 1, gen::OBJ_BTREE_ROOT, gen::SUB_FSTREE as u16)
            .unwrap();
        let back = Node::decode(&buf, gen::OBJ_BTREE_ROOT).unwrap();
        assert!(back.entries.is_empty());
        assert_eq!(back.level, 0);
    }

    #[test]
    fn node_round_trips_its_entries() {
        let mut node = Node::new_leaf();
        for name in ["alpha", "beta", "gamma", "delta"] {
            let mut entries: Vec<Entry> = node.entries.clone();
            let k = key(name);
            let pos = entries
                .binary_search_by(|e| compare_keys(KeyKind::JKey, &e.key, &k))
                .unwrap_or_else(|i| i);
            entries.insert(pos, Entry::new(&k, name.as_bytes()));
            node.entries = entries;
        }
        let mut buf = vec![0u8; 4096];
        node.encode(&mut buf, 0, 1, gen::OBJ_BTREE_ROOT, gen::SUB_FSTREE as u16)
            .unwrap();
        let back = Node::decode(&buf, gen::OBJ_BTREE_ROOT).unwrap();
        assert_eq!(back.entries.len(), 4);
        assert_eq!(back.entries[0].key, key("alpha"));
        assert_eq!(back.entries[0].val, b"alpha");
    }

    #[test]
    fn encode_refuses_a_node_that_does_not_fit() {
        let mut node = Node::new_leaf();
        for i in 0..2000u32 {
            let k = alloc::format!("key-{i:08}");
            let v = alloc::vec![0xABu8; 64];
            node.entries.push(Entry::new(k.as_bytes(), &v));
        }
        let mut buf = vec![0u8; 512];
        assert!(matches!(
            node.encode(&mut buf, 0, 1, gen::OBJ_BTREE_ROOT, 0),
            Err(FsError::CorruptNode(_))
        ));
    }

    #[test]
    fn decode_rejects_a_node_that_is_not_even_a_valid_object() {
        let garbage = vec![0xFFu8; 4096];
        assert!(Node::decode(&garbage, gen::OBJ_BTREE_ROOT).is_err());
    }

    #[test]
    fn insert_then_search_round_trips() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        {
            let mut tree = make_tree(&mut dev, &mut alloc);
            tree.insert(&key("hello"), b"world").unwrap();
        }
        {
            let mut tree = make_tree(&mut dev, &mut alloc);
            assert_eq!(
                tree.search(&key("hello")).unwrap().as_deref(),
                Some(&b"world"[..])
            );
            assert_eq!(tree.search(&key("missing")).unwrap(), None);
        }
    }

    #[test]
    fn insert_replaces_an_existing_key() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        let mut tree = make_tree(&mut dev, &mut alloc);
        tree.insert(&key("k"), b"first").unwrap();
        tree.insert(&key("k"), b"second").unwrap();
        assert_eq!(
            tree.search(&key("k")).unwrap().as_deref(),
            Some(&b"second"[..])
        );
        assert_eq!(tree.collect().unwrap().len(), 1);
    }

    #[test]
    fn many_inserts_split_the_tree_and_preserve_order() {
        let mut dev = MemBlockDevice::new(4096, 4096);
        let mut alloc = Bump { next: 1, end: 4096 };
        let count = 500;
        let mut names: Vec<String> = (0..count).map(|i| alloc::format!("name-{i:06}")).collect();

        {
            let mut tree = make_tree(&mut dev, &mut alloc);
            for n in &names {
                tree.insert(&key(n), n.as_bytes()).unwrap();
            }
            // The tree must have grown past a single node.
            let all = tree.collect().unwrap();
            assert_eq!(all.len(), count);
        }

        {
            let mut tree = make_tree(&mut dev, &mut alloc);
            let all = tree.collect().unwrap();
            assert_eq!(all.len(), count, "entry count changed after a reload");

            names.sort();
            for (i, n) in names.iter().enumerate() {
                assert_eq!(all[i].key, key(n), "entry {i} is out of order");
                assert_eq!(all[i].val, n.as_bytes());
            }

            for n in &names {
                assert_eq!(
                    tree.search(&key(n)).unwrap().as_deref(),
                    Some(n.as_bytes()),
                    "lookup of {n} failed after splits"
                );
            }
        }
    }

    #[test]
    fn search_floor_returns_the_predecessor() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        let mut tree = make_tree(&mut dev, &mut alloc);
        for n in ["a", "c", "e"] {
            tree.insert(&key(n), n.as_bytes()).unwrap();
        }
        // Exact hit.
        let (k, _) = tree.search_floor(&key("c")).unwrap().unwrap();
        assert_eq!(k, key("c"));
        // Between c and e -> c.
        let (k, _) = tree.search_floor(&key("d")).unwrap().unwrap();
        assert_eq!(k, key("c"));
        // Past the end -> e.
        let (k, _) = tree.search_floor(&key("z")).unwrap().unwrap();
        assert_eq!(k, key("e"));
        // Before everything -> nothing.
        assert!(tree.search_floor(&key("A")).unwrap().is_none());
    }

    #[test]
    fn remove_deletes_only_the_named_key() {
        let mut dev = MemBlockDevice::new(4096, 1024);
        let mut alloc = Bump { next: 1, end: 1024 };
        let mut tree = make_tree(&mut dev, &mut alloc);
        for n in ["a", "b", "c", "d"] {
            tree.insert(&key(n), n.as_bytes()).unwrap();
        }
        assert!(tree.remove(&key("b")).unwrap());
        assert!(
            !tree.remove(&key("b")).unwrap(),
            "removing twice reports success twice"
        );
        assert_eq!(tree.search(&key("b")).unwrap(), None);
        let all = tree.collect().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|e| e.key == key("a")));
        assert!(all.iter().any(|e| e.key == key("c")));
        assert!(all.iter().any(|e| e.key == key("d")));
    }

    #[test]
    fn running_out_of_space_is_an_error_not_a_panic() {
        let mut dev = MemBlockDevice::new(4096, 64);
        // Only one spare block: the first split will exhaust the allocator.
        let mut alloc = Bump { next: 1, end: 2 };
        let mut tree = make_tree(&mut dev, &mut alloc);
        let mut hit_error = false;
        for i in 0..2000u32 {
            let k = key(&alloc::format!("n-{i:08}"));
            let v = alloc::vec![0u8; 128];
            if tree.insert(&k, &v).is_err() {
                hit_error = true;
                break;
            }
        }
        assert!(
            hit_error,
            "exhausting the allocator did not surface as an error"
        );
    }

    #[test]
    fn a_corrupt_node_is_rejected_rather_than_indexed_out_of_bounds() {
        let mut dev = MemBlockDevice::new(4096, 16);
        let mut alloc = Bump { next: 1, end: 16 };
        {
            let mut tree = make_tree(&mut dev, &mut alloc);
            tree.insert(&key("a"), b"1").unwrap();
        }
        // Corrupt the table of contents so an entry points outside the block.
        let mut raw = vec![0u8; 4096];
        dev.read_block(0, &mut raw).unwrap();
        // key_off is the first u16 of the first ToC entry, at NODE_HEADER_SIZE.
        raw[NODE_HEADER_SIZE..NODE_HEADER_SIZE + 2].copy_from_slice(&60000u16.to_le_bytes());
        // Recompute the checksum so the object header itself passes and the ToC check is
        // what fires.
        raw[0..8].copy_from_slice(&0u64.to_le_bytes());
        let sum = crate::fletcher::compute(&raw).unwrap();
        raw[0..8].copy_from_slice(&sum.to_le_bytes());
        dev.write_block(0, &raw).unwrap();

        let mut tree = make_tree(&mut dev, &mut alloc);
        let err = tree.search(&key("a")).unwrap_err();
        assert!(matches!(err, FsError::CorruptNode(_)), "got {err:?}");
    }
}
