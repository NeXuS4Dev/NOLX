//! The table of interrupt-vector owners.
//!
//! Every live native source has one globally unique local-APIC vector. Drivers claim before
//! exposing a vector to hardware and release only after a checked stop proves that source
//! quiet. Shared e1000 adapters use one owner per GSI; the I/O APIC route manager retains it
//! until the final adapter reference is released.
//!
//! The table is a plain array of atomics: interrupt dispatch reads it without a lock, and a
//! claim or release is one compare-and-swap. Preferred slots and a common bounded fallback
//! pool live in [`nolx_hal_core::irq_vectors`], where the rules are host-tested.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use nolx_hal_core::irq_vectors::{self as rules, Kind};

pub use nolx_hal_core::irq_vectors::{pool, Kind as VectorKind};

/// Whether an unowned vector is in the bounded native-device domain and can be safely
/// acknowledged as a stale or retired interrupt.
pub const fn is_managed_vector(vector: u8) -> bool {
    rules::is_managed_vector(vector)
}

/// The number of vectors the table covers.
const VECTORS: usize = 256;

/// The owner of each vector: free, or the kind and handle that own it.
static OWNERS: [AtomicU64; VECTORS] = [const { AtomicU64::new(rules::FREE) }; VECTORS];

/// Claim a vector for a native source: a controller or a shared GSI route.
///
/// The kind's reserved preferred vector is taken first, then the lowest free candidate in
/// the common fallback pool. The compare-and-swap decides the winner across all kinds, so
/// two live sources can never own the same vector. An exhausted pool is refused.
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

/// Release a vector after the owner has proved its source quiet.
///
/// A vector that is free, or that another source owns, is refused: a release that does not
/// match the owner would hand a live source's vector to somebody else.
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

/// The controller or shared route that currently owns a vector.
pub fn owner(vector: u8) -> Option<(Kind, usize)> {
    let index = usize::from(vector);
    if index >= VECTORS {
        return None;
    }
    rules::unpack(OWNERS[index].load(Ordering::Acquire))
}

/// Take every currently free candidate of a kind for a synthetic source that does not exist.
///
/// The handle is one that no real controller or route uses, so the claims stand for other
/// sources that would take the vectors. The caller releases them again, and a further claim
/// of that kind is refused while they are held.
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
