//! The table of interrupt-vector owners.
//!
//! A vector belongs to at most one controller. A driver claims a vector when it starts
//! and releases it only after a checked stop has proved that the controller can no
//! longer raise it. A released vector is free again, so a stop and restart cycle does
//! not consume a new vector for ever, and the next controller of that kind takes the
//! vector that was freed.
//!
//! The table is a plain array of atomics: the interrupt path reads it without taking a
//! lock, and a claim or a release is one compare-and-swap. The rules -- which vectors a
//! kind may own and in which order they are preferred -- live in
//! [`nolx_hal_core::irq_vectors`], where they are host-tested.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use nolx_hal_core::irq_vectors::{self as rules, Kind};

pub use nolx_hal_core::irq_vectors::{pool, Kind as VectorKind};

/// The number of vectors the table covers.
const VECTORS: usize = 256;

/// The owner of each vector: free, or the kind and handle that own it.
static OWNERS: [AtomicU64; VECTORS] = [const { AtomicU64::new(rules::FREE) }; VECTORS];

/// Claim a vector for a controller of a kind.
///
/// The kind's preferred vector is taken first, then the lowest free vector of its
/// range. The compare-and-swap decides the winner, so two controllers can never own the
/// same vector. A kind whose pool is exhausted is refused.
pub fn claim(kind: Kind, handle: usize) -> Option<u8> {
    let pool = rules::pool(kind);
    let entry = rules::pack(kind, handle);
    for vector in pool.candidates() {
        let owner = &OWNERS[usize::from(vector)];
        if owner.load(Ordering::Acquire) != rules::FREE {
            continue;
        }
        if owner
            .compare_exchange(rules::FREE, entry, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(vector);
        }
        // Another controller took it between the read and the swap: try the next one.
    }
    None
}

/// Release a vector that this controller owns.
///
/// A vector that is free, or that another controller owns, is refused: a release that
/// does not match the owner would hand a live controller's vector to somebody else.
pub fn release(vector: u8, kind: Kind, handle: usize) -> bool {
    let index = usize::from(vector);
    if index >= VECTORS {
        return false;
    }
    OWNERS[index]
        .compare_exchange(
            rules::pack(kind, handle),
            rules::FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

/// The controller that owns a vector.
pub fn owner(vector: u8) -> Option<(Kind, usize)> {
    let index = usize::from(vector);
    if index >= VECTORS {
        return None;
    }
    rules::unpack(OWNERS[index].load(Ordering::Acquire))
}

/// Take every free vector of a kind's pool for a controller that does not exist.
///
/// The handle is one that no real controller uses, so the claims stand for the other
/// controllers that would take the vectors. The caller releases them again, and the
/// next claim of that kind is refused while they are held.
pub fn occupy(kind: Kind, handle: usize) -> Vec<u8> {
    let mut held = Vec::new();
    while let Some(vector) = claim(kind, handle) {
        held.push(vector);
    }
    held
}

/// How many vectors are currently owned.
pub fn outstanding() -> usize {
    OWNERS
        .iter()
        .filter(|entry| entry.load(Ordering::Acquire) != rules::FREE)
        .count()
}
