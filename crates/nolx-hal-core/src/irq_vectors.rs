//! Which interrupt vector a native controller may own, and how that ownership is tracked.
//!
//! Every vector belongs to at most one controller. A driver claims a vector when it
//! starts and releases it only after a checked stop has proved that the controller can
//! no longer raise it. A released vector is free again, so a stop and restart cycle does
//! not consume a new vector for ever, and the next controller of that kind takes the
//! vector that was freed.
//!
//! The rules live here so that they can be checked on a host. The kernel keeps the table
//! of owners and performs the atomic claim and the release.

/// The kinds of native controller that own a vector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The virtio block driver.
    VirtioBlk,
    /// The AHCI driver.
    Ahci,
    /// The NVMe driver.
    Nvme,
}

/// The identifier of each kind, as it is stored in a ledger entry.
const fn kind_id(kind: Kind) -> u64 {
    match kind {
        Kind::VirtioBlk => 1,
        Kind::Ahci => 2,
        Kind::Nvme => 3,
    }
}

/// The kind an identifier names.
const fn kind_from_id(id: u64) -> Option<Kind> {
    match id {
        1 => Some(Kind::VirtioBlk),
        2 => Some(Kind::Ahci),
        3 => Some(Kind::Nvme),
        _ => None,
    }
}

/// The vectors one kind of controller may own.
///
/// The preferred vector is the one the first controller of the kind takes; the range
/// holds the vectors that further controllers take, one after another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// The vector the first controller of the kind takes.
    pub preferred: u8,
    /// The first vector of the kind's range.
    pub first: u8,
    /// The last vector of the kind's range.
    pub last: u8,
}

impl Pool {
    /// The pool's vectors in the order they are preferred: the preferred vector first,
    /// then the range from its lowest.
    pub fn candidates(&self) -> impl Iterator<Item = u8> + '_ {
        let (first, last) = (self.first, self.last);
        core::iter::once(self.preferred).chain(first..=last)
    }
}

/// The pool of the given kind.
pub const fn pool(kind: Kind) -> Pool {
    match kind {
        // The first controller keeps the compatibility vector every CI image expects.
        Kind::VirtioBlk => Pool {
            preferred: 0x51,
            first: 0x60,
            last: 0x6f,
        },
        Kind::Ahci => Pool {
            preferred: 0x52,
            first: 0x90,
            last: 0x9f,
        },
        Kind::Nvme => Pool {
            preferred: 0x50,
            first: 0xa0,
            last: 0xaf,
        },
    }
}

/// The ledger entry that names no owner.
pub const FREE: u64 = 0;

/// The ledger entry that names a controller: its kind and its handle.
///
/// The handle is stored with one added, so that the entry of the handle zero is not the
/// free entry.
pub const fn pack(kind: Kind, handle: usize) -> u64 {
    ((handle as u64 + 1) << 8) | kind_id(kind)
}

/// The controller a ledger entry names, if it names one.
pub const fn unpack(entry: u64) -> Option<(Kind, usize)> {
    if entry == FREE {
        return None;
    }
    // The handle is stored with one added, so an entry without a handle part names no
    // controller at all.
    if entry < 0x100 {
        return None;
    }
    let kind = match kind_from_id(entry & 0xff) {
        Some(kind) => kind,
        None => return None,
    };
    Some((kind, ((entry >> 8) - 1) as usize))
}

/// The vector a new controller of this kind takes: the preferred vector when it is free,
/// otherwise the lowest free vector of the range.
pub fn choose(pool: &Pool, free: impl Fn(u8) -> bool) -> Option<u8> {
    pool.candidates().find(|vector| free(*vector))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_controller_of_a_kind_takes_the_preferred_vector() {
        for kind in [Kind::VirtioBlk, Kind::Ahci, Kind::Nvme] {
            let pool = pool(kind);
            assert_eq!(choose(&pool, |_| true), Some(pool.preferred));
        }
    }

    #[test]
    fn a_taken_preferred_vector_makes_the_range_start_at_its_lowest() {
        for kind in [Kind::VirtioBlk, Kind::Ahci, Kind::Nvme] {
            let pool = pool(kind);
            let free = |vector: u8| vector != pool.preferred;
            assert_eq!(choose(&pool, free), Some(pool.first));
            let free = |vector: u8| vector != pool.preferred && vector != pool.first;
            assert_eq!(choose(&pool, free), Some(pool.first + 1));
        }
    }

    #[test]
    fn an_exhausted_pool_has_no_vector_left() {
        for kind in [Kind::VirtioBlk, Kind::Ahci, Kind::Nvme] {
            let pool = pool(kind);
            assert_eq!(choose(&pool, |_| false), None);
        }
    }

    #[test]
    fn the_pools_of_the_kinds_do_not_overlap() {
        let pools = [pool(Kind::VirtioBlk), pool(Kind::Ahci), pool(Kind::Nvme)];
        for (index, first) in pools.iter().enumerate() {
            for second in &pools[index + 1..] {
                for vector in first.candidates() {
                    assert!(
                        !second.candidates().any(|other| other == vector),
                        "vector {vector:#x} is in two pools"
                    );
                }
            }
        }
    }

    #[test]
    fn the_pools_avoid_the_firmware_and_usb_vector_ranges() {
        // The e1000 shares the firmware-routed INTx vectors and the xHCI owns its own
        // range; a storage controller must never take one of them.
        for kind in [Kind::VirtioBlk, Kind::Ahci, Kind::Nvme] {
            for vector in pool(kind).candidates() {
                assert!(!(0x23..=0x37).contains(&vector), "vector {vector:#x}");
                assert!(!(0xd0..=0xdf).contains(&vector), "vector {vector:#x}");
                assert!(!(0xb0..=0xcf).contains(&vector), "vector {vector:#x}");
            }
        }
    }

    #[test]
    fn a_ledger_entry_names_its_controller_and_nothing_else() {
        for kind in [Kind::VirtioBlk, Kind::Ahci, Kind::Nvme] {
            for handle in [0usize, 1, 2, 7, 64, 4095] {
                let entry = pack(kind, handle);
                assert_eq!(unpack(entry), Some((kind, handle)));
            }
        }
        assert_eq!(unpack(FREE), None);
    }

    #[test]
    fn an_entry_of_an_unknown_kind_names_no_controller() {
        // The kind sits in the low byte; an unknown kind names nothing.
        assert_eq!(unpack(0x0000_0000_0000_00ff), None);
        // An entry without a handle part names nothing either.
        assert_eq!(unpack(0x01), None);
    }
}
