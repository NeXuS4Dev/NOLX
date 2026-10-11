//! The kernel's three charged pools.
//!
//! `nolx_mm_core::commit` is the accounting: a charged byte is a byte the kernel has
//! promised to keep resident (nonpaged), promised to keep pageable (paged), or promised to
//! page tables (system PTEs). Nothing in the allocator needs to know about pools; the code
//! that *consumes* the memory charges them, which is why the charge sites are page-table
//! allocation in [`crate::mm::tables`] and kernel-heap regions in [`crate::mm::heap`].
//!
//! The limits are boot-time policy for now -- fractions of the machine's memory large enough
//! that a self test can exercise the accounting without reaching them. When the object
//! manager owns allocations, the limits move to quota objects and this module keeps only the
//! static ones.

use crate::sync::SyncCell;
use nolx_mm_core::commit::{CommitError, PoolKind, PoolLimits, Pools};

/// Limit of the nonpaged pool: kernel heap regions come from here.
pub const NON_PAGED_LIMIT: u64 = 256 * 1024 * 1024;
/// Limit of the paged pool.
pub const PAGED_LIMIT: u64 = 64 * 1024 * 1024;
/// Limit of the system PTE pool: page tables come from here.
pub const SYSTEM_PTE_LIMIT: u64 = 64 * 1024 * 1024;

/// The three pools, at their boot-time limits.
static POOLS: SyncCell<Pools> = SyncCell::new(Pools::new(PoolLimits {
    non_paged: NON_PAGED_LIMIT,
    paged: PAGED_LIMIT,
    system_pte: SYSTEM_PTE_LIMIT,
}));

/// Charge `bytes` to `kind`, or fail without changing anything.
pub fn charge(kind: PoolKind, bytes: u64) -> Result<u64, CommitError> {
    // SAFETY: single boot processor. The pools are charged by the code that takes memory
    // from them, which never runs in interrupt context, and read by the boot report on the
    // same processor.
    unsafe { (*POOLS.get()).charge(kind, bytes) }
}

/// Return `bytes` to `kind`.
pub fn release(kind: PoolKind, bytes: u64) -> Result<u64, CommitError> {
    // SAFETY: as [`charge`].
    unsafe { (*POOLS.get()).release(kind, bytes) }
}

/// Verify the pools against their own counters.
pub fn check() -> Result<(), CommitError> {
    // SAFETY: as [`charge`].
    unsafe { (*POOLS.get()).check() }
}

/// One pool's line in the boot report.
#[derive(Clone, Copy, Debug)]
pub struct PoolSnapshot {
    /// Which pool.
    pub kind: PoolKind,
    /// Bytes currently charged.
    pub current: u64,
    /// Highest charge seen.
    pub peak: u64,
    /// The limit.
    pub limit: u64,
}

/// What every pool is charging, in report order.
#[must_use]
pub fn snapshot() -> [PoolSnapshot; 3] {
    let kinds = PoolKind::all();
    // SAFETY: as [`charge`]: a read-only walk while no other processor exists.
    let pools = unsafe { &*POOLS.get() };
    let mut out = [PoolSnapshot {
        kind: PoolKind::NonPaged,
        current: 0,
        peak: 0,
        limit: 0,
    }; 3];
    for (slot, kind) in out.iter_mut().zip(kinds) {
        let charge = pools.get(kind);
        *slot = PoolSnapshot {
            kind,
            current: charge.current(),
            peak: charge.peak(),
            limit: charge.limit(),
        };
    }
    out
}
