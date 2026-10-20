//! Shared local-APIC vector allocation rules for native device interrupts.
//!
//! All native PCI interrupt sources draw from one bounded vector domain. Each driver kind
//! has a preferred vector for its first source; after that, every kind shares the same
//! fallback range. The kernel's atomic ownership table prevents two different kinds from
//! taking the same fallback vector. Preferred vectors are reserved for their own kind.
//!
//! A vector is released only after a checked stop proves that the source can no longer raise
//! it. For a shared e1000 INTx route, the owner is the GSI route rather than one adapter, and
//! the kernel keeps that owner until the final adapter releases its route reference.

/// The kinds of native controller or shared PCI route that may own a vector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The virtio block driver.
    VirtioBlk,
    /// The AHCI driver.
    Ahci,
    /// The NVMe driver.
    Nvme,
    /// The xHCI driver.
    Xhci,
    /// The receive queue of a virtio-net controller.
    VirtioNetRx,
    /// The transmit queue of a virtio-net controller.
    VirtioNetTx,
    /// A shared e1000 legacy INTx route, identified by its GSI.
    E1000Intx,
}

/// The preferred local-APIC vector of the kind, reserved so another kind cannot consume it.
const fn preferred(kind: Kind) -> u8 {
    match kind {
        Kind::VirtioBlk => 0x51,
        Kind::Ahci => 0x52,
        Kind::Nvme => 0x50,
        Kind::Xhci => 0xd0,
        Kind::VirtioNetRx => 0x53,
        Kind::VirtioNetTx => 0x54,
        Kind::E1000Intx => 0x55,
    }
}

/// The identifier of each kind, as it is stored in a ledger entry.
const fn kind_id(kind: Kind) -> u64 {
    match kind {
        Kind::VirtioBlk => 1,
        Kind::Ahci => 2,
        Kind::Nvme => 3,
        Kind::Xhci => 4,
        Kind::VirtioNetRx => 5,
        Kind::VirtioNetTx => 6,
        Kind::E1000Intx => 7,
    }
}

/// The kind an identifier names.
const fn kind_from_id(id: u64) -> Option<Kind> {
    match id {
        1 => Some(Kind::VirtioBlk),
        2 => Some(Kind::Ahci),
        3 => Some(Kind::Nvme),
        4 => Some(Kind::Xhci),
        5 => Some(Kind::VirtioNetRx),
        6 => Some(Kind::VirtioNetTx),
        7 => Some(Kind::E1000Intx),
        _ => None,
    }
}

/// First fallback vector in the bounded pool shared by every native PCI source.
pub const FIRST_FALLBACK_VECTOR: u8 = 0x60;
/// Last fallback vector in the bounded pool shared by every native PCI source.
pub const LAST_FALLBACK_VECTOR: u8 = 0xdf;

/// Whether a vector belongs to the native allocator's domain, including reserved preferred
/// slots. A stale interrupt in this domain can be EOI'd safely without calling a driver.
#[must_use]
pub const fn is_managed_vector(vector: u8) -> bool {
    vector >= 0x50 && vector <= LAST_FALLBACK_VECTOR
}

/// The vectors one kind may claim: its preferred vector, followed by the shared fallback range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// The preferred vector reserved for this kind's first source.
    pub preferred: u8,
    /// The first vector of the shared fallback range.
    pub first: u8,
    /// The last vector of the shared fallback range.
    pub last: u8,
}

impl Pool {
    /// The candidates in preference order. Other kinds' preferred vectors are never fallback
    /// candidates, even though some of them lie inside the shared range.
    pub fn candidates(&self) -> impl Iterator<Item = u8> + '_ {
        core::iter::once(self.preferred).chain(
            (self.first..=self.last)
                .filter(move |vector| *vector != self.preferred && !is_preferred_vector(*vector)),
        )
    }
}

const fn is_preferred_vector(vector: u8) -> bool {
    matches!(vector, 0x50..=0x55 | 0xd0)
}

/// The pool of the given kind. Every kind shares the same bounded fallback range.
pub const fn pool(kind: Kind) -> Pool {
    Pool {
        preferred: preferred(kind),
        first: FIRST_FALLBACK_VECTOR,
        last: LAST_FALLBACK_VECTOR,
    }
}

/// The ledger entry that names no owner.
pub const FREE: u64 = 0;

/// The ledger entry that names a controller: its kind and its handle.
///
/// The handle is stored with one added, so that the entry of handle zero is not the free entry.
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

/// The candidate a new source of this kind takes: its preferred vector when free, otherwise
/// the lowest free vector in the shared fallback range.
pub fn choose(pool: &Pool, free: impl Fn(u8) -> bool) -> Option<u8> {
    pool.candidates().find(|vector| free(*vector))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 7] = [
        Kind::VirtioBlk,
        Kind::Ahci,
        Kind::Nvme,
        Kind::Xhci,
        Kind::VirtioNetRx,
        Kind::VirtioNetTx,
        Kind::E1000Intx,
    ];

    #[test]
    fn every_kind_gets_its_reserved_preferred_vector_first() {
        let mut preferred_vectors = [false; 256];
        for kind in KINDS {
            let pool = pool(kind);
            assert_eq!(choose(&pool, |_| true), Some(pool.preferred));
            assert!(!preferred_vectors[usize::from(pool.preferred)]);
            preferred_vectors[usize::from(pool.preferred)] = true;
        }
    }

    #[test]
    fn fallback_allocation_starts_at_the_lowest_globally_free_vector() {
        let occupied = [FIRST_FALLBACK_VECTOR, FIRST_FALLBACK_VECTOR + 1];
        for kind in KINDS {
            let pool = pool(kind);
            let free = |vector: u8| vector != pool.preferred && !occupied.contains(&vector);
            assert_eq!(choose(&pool, free), Some(FIRST_FALLBACK_VECTOR + 2));
        }
    }

    #[test]
    fn an_exhausted_shared_fallback_and_preferred_slot_refuses_every_kind() {
        for kind in KINDS {
            let pool = pool(kind);
            assert_eq!(choose(&pool, |_| false), None);
        }
    }

    #[test]
    fn all_kinds_share_one_fallback_range_without_sharing_allocated_vectors() {
        let pools = KINDS.map(pool);
        for (index, first) in pools.iter().enumerate() {
            assert_eq!(first.first, FIRST_FALLBACK_VECTOR);
            assert_eq!(first.last, LAST_FALLBACK_VECTOR);
            for second in &pools[index + 1..] {
                assert_eq!((first.first, first.last), (second.first, second.last));
            }
        }

        // Model the kernel's global owner test: each claim removes its chosen vector from the
        // free set before the next kind searches the overlapping candidate range.
        let mut owned = [false; 256];
        let mut selected = [0u8; KINDS.len()];
        for (index, kind) in KINDS.into_iter().enumerate() {
            let candidate = choose(&pool(kind), |vector| !owned[usize::from(vector)]).unwrap();
            assert!(!owned[usize::from(candidate)]);
            owned[usize::from(candidate)] = true;
            selected[index] = candidate;
        }
        for (index, vector) in selected.iter().enumerate() {
            assert!(!selected[..index].contains(vector));
        }
    }

    #[test]
    fn preferred_slots_are_never_used_as_another_kinds_fallback() {
        for kind in KINDS {
            for other in KINDS {
                if kind == other {
                    continue;
                }
                assert!(
                    !pool(kind)
                        .candidates()
                        .skip(1)
                        .any(|vector| vector == preferred(other)),
                    "{kind:?} can consume {other:?}'s preferred vector"
                );
            }
        }
    }

    #[test]
    fn the_shared_domain_avoids_exception_pic_timer_and_spurious_vectors() {
        for vector in (0..=u8::MAX).filter(|vector| is_managed_vector(*vector)) {
            assert!(vector >= 0x50);
            assert!(vector != 0xff);
            assert!(!(0x20..=0x30).contains(&vector));
        }
        assert!(is_managed_vector(0x50));
        assert!(is_managed_vector(0xdf));
        assert!(!is_managed_vector(0x4f));
        assert!(!is_managed_vector(0xe0));
    }

    #[test]
    fn a_ledger_entry_names_its_controller_or_shared_route() {
        for kind in KINDS {
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
