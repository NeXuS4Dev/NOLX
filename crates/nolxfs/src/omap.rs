//! The object map: `(oid, xid)` to a physical block address.
//!
//! Two object maps exist -- one per container, one per volume -- and both are B-trees with
//! [`KeyKind::OMap`](crate::btree::KeyKind::OMap) ordering. A lookup is "the newest
//! version of this object at or before transaction `xid`", which is what makes
//! point-in-time views and snapshots a lookup rather than a copy.

use crate::blockdev::BlockDevice;
use crate::btree::{BTree, BlockAllocator, KeyKind};
use crate::on_disk::{FsError, Result};
use crate::on_disk_gen as gen;
use crate::Pod;

/// Encode an object-map key.
#[must_use]
pub fn encode_key(oid: u64, xid: u64) -> [u8; 16] {
    let key = gen::nolx_omap_key_t { oid, xid };
    let mut out = [0u8; 16];
    out.copy_from_slice(key.as_bytes());
    out
}

/// Decode an object-map key.
#[must_use]
pub fn decode_key(bytes: &[u8]) -> Option<(u64, u64)> {
    let key = gen::nolx_omap_key_t::read_from(bytes)?;
    Some((key.oid, key.xid))
}

/// Encode an object-map value.
#[must_use]
pub fn encode_val(paddr: u64, flags: u32) -> [u8; 16] {
    let val = gen::nolx_omap_val_t {
        flags,
        pad: 0,
        paddr,
    };
    let mut out = [0u8; 16];
    out.copy_from_slice(val.as_bytes());
    out
}

/// Decode an object-map value.
#[must_use]
pub fn decode_val(bytes: &[u8]) -> Option<(u64, u32)> {
    let val = gen::nolx_omap_val_t::read_from(bytes)?;
    Some((val.paddr, val.flags))
}

/// An object map over a block device.
pub struct ObjectMap<'d, D: BlockDevice, A: BlockAllocator> {
    tree: BTree<'d, D, A>,
}

impl<'d, D: BlockDevice, A: BlockAllocator> ObjectMap<'d, D, A> {
    /// Open the object map whose B-tree root is at `tree_root`.
    pub fn open(dev: &'d mut D, alloc: &'d mut A, tree_root: u64, xid: u64) -> Self {
        Self {
            tree: BTree::new(
                dev,
                alloc,
                tree_root,
                KeyKind::OMap,
                gen::SUB_OMAP as u16,
                xid,
            ),
        }
    }

    /// The B-tree root, which changes if the map grows a level.
    #[must_use]
    pub const fn tree_root(&self) -> u64 {
        self.tree.root_block()
    }

    /// Resolve `oid` as it was at `xid`.
    ///
    /// # Errors
    ///
    /// Device or corruption errors. A missing object is `Ok(None)`, not an error: the
    /// caller decides whether that is a problem.
    pub fn resolve(&mut self, oid: u64, xid: u64) -> Result<Option<u64>> {
        match self.tree.search_floor(&encode_key(oid, xid))? {
            None => Ok(None),
            Some((found_key, val)) => {
                // `search_floor` can land on a different object entirely if `oid` has no
                // entry at or before `xid`. Verify the oid matches before trusting it.
                let (found_oid, _) =
                    decode_key(&found_key).ok_or(FsError::CorruptNode("bad omap key length"))?;
                if found_oid != oid {
                    return Ok(None);
                }
                let (paddr, flags) =
                    decode_val(&val).ok_or(FsError::CorruptNode("bad omap value length"))?;
                if flags & gen::OMAP_VAL_FLAG_MISSING != 0 {
                    return Ok(None);
                }
                Ok(Some(paddr))
            }
        }
    }

    /// Record that `oid` lives at `paddr` as of `xid`.
    ///
    /// # Errors
    ///
    /// Device errors or out-of-space.
    pub fn set(&mut self, oid: u64, xid: u64, paddr: u64, flags: u32) -> Result<()> {
        self.tree
            .insert(&encode_key(oid, xid), &encode_val(paddr, flags))
    }

    /// Mark `oid` as absent at `xid`, so a lookup fails instead of falling back to an
    /// older version. Used when an object is deleted but a snapshot may still be reading.
    ///
    /// # Errors
    ///
    /// Device errors or out-of-space.
    pub fn mark_missing(&mut self, oid: u64, xid: u64) -> Result<()> {
        self.set(oid, xid, 0, gen::OMAP_VAL_FLAG_MISSING)
    }

    /// Mark `oid` as reachable from a snapshot, so a later transaction must copy rather
    /// than overwrite it.
    ///
    /// # Errors
    ///
    /// Device errors or out-of-space.
    pub fn mark_saved(&mut self, oid: u64, xid: u64, paddr: u64) -> Result<()> {
        self.set(oid, xid, paddr, gen::OMAP_VAL_FLAG_SAVED)
    }

    /// Every mapping in the map, in `(oid, xid)` order. Used by `fsck`.
    ///
    /// # Errors
    ///
    /// Device or corruption errors.
    pub fn all(&mut self) -> Result<alloc::vec::Vec<((u64, u64), (u64, u32))>> {
        let mut out = alloc::vec::Vec::new();
        for entry in self.tree.collect()? {
            let key = decode_key(&entry.key).ok_or(FsError::CorruptNode("bad omap key length"))?;
            let val = decode_val(&entry.val).ok_or(FsError::CorruptNode("bad omap val length"))?;
            out.push((key, val));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::MemBlockDevice;

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

    #[test]
    fn keys_and_values_round_trip() {
        let k = encode_key(0x1122_3344_5566_7788, 99);
        assert_eq!(decode_key(&k), Some((0x1122_3344_5566_7788, 99)));
        let v = encode_val(4096, gen::OMAP_VAL_FLAG_SAVED);
        assert_eq!(decode_val(&v), Some((4096, gen::OMAP_VAL_FLAG_SAVED)));
    }

    #[test]
    fn keys_and_values_reject_short_input() {
        assert_eq!(decode_key(&[0u8; 4]), None);
        assert_eq!(decode_val(&[0u8; 4]), None);
    }

    #[test]
    fn resolve_picks_the_newest_version_at_or_before_the_xid() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            omap.set(7, 1, 100, 0).unwrap();
            omap.set(7, 5, 200, 0).unwrap();
            omap.set(7, 9, 300, 0).unwrap();
        }
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            assert_eq!(
                omap.resolve(7, 0).unwrap(),
                None,
                "before the first version"
            );
            assert_eq!(omap.resolve(7, 1).unwrap(), Some(100));
            assert_eq!(omap.resolve(7, 4).unwrap(), Some(100), "still the v1 block");
            assert_eq!(omap.resolve(7, 5).unwrap(), Some(200));
            assert_eq!(omap.resolve(7, 8).unwrap(), Some(200));
            assert_eq!(omap.resolve(7, 9).unwrap(), Some(300));
            assert_eq!(
                omap.resolve(7, 10_000).unwrap(),
                Some(300),
                "newest version"
            );
        }
    }

    #[test]
    fn resolve_does_not_confuse_two_object_ids() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            omap.set(5, 1, 111, 0).unwrap();
        }
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            assert_eq!(omap.resolve(6, 99).unwrap(), None, "oid 6 has no versions");
            assert_eq!(omap.resolve(4, 99).unwrap(), None, "oid 4 has no versions");
        }
    }

    #[test]
    fn a_missing_mark_hides_older_versions() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            omap.set(3, 1, 42, 0).unwrap();
            omap.mark_missing(3, 2).unwrap();
        }
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            assert_eq!(
                omap.resolve(3, 1).unwrap(),
                Some(42),
                "v1 is still visible at xid 1"
            );
            assert_eq!(
                omap.resolve(3, 2).unwrap(),
                None,
                "hidden from xid 2 onward"
            );
        }
    }

    #[test]
    fn saved_flag_survives_a_round_trip() {
        let mut dev = MemBlockDevice::new(4096, 256);
        let mut alloc = Bump { next: 1, end: 256 };
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            omap.mark_saved(9, 4, 77).unwrap();
        }
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            let all = omap.all().unwrap();
            assert_eq!(all, vec![((9, 4), (77, gen::OMAP_VAL_FLAG_SAVED))]);
        }
    }

    #[test]
    fn many_objects_keep_the_map_consistent() {
        let mut dev = MemBlockDevice::new(4096, 4096);
        let mut alloc = Bump { next: 1, end: 4096 };
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            for oid in 1..500u64 {
                omap.set(oid, 1, oid * 10, 0).unwrap();
                omap.set(oid, 2, oid * 20, 0).unwrap();
            }
        }
        {
            let mut omap = ObjectMap::open(&mut dev, &mut alloc, 0, 1);
            for oid in 1..500u64 {
                assert_eq!(
                    omap.resolve(oid, 1).unwrap(),
                    Some(oid * 10),
                    "oid {oid} at xid 1"
                );
                assert_eq!(
                    omap.resolve(oid, 2).unwrap(),
                    Some(oid * 20),
                    "oid {oid} at xid 2"
                );
            }
            assert_eq!(omap.all().unwrap().len(), 998);
        }
    }
}
