//! Pool accounting, as the executive sees it.
//!
//! The memory manager owns the three pools and the code that consumes memory charges them:
//! page tables in [`crate::mm::tables`], heap regions in [`crate::mm::heap`]. This module is the
//! executive's view of the same numbers -- the surface a system-information query or a quota
//! object will read. Object-body charging is deferred until the Object Manager allocates bodies.
//!
//! Reporting it is not decoration. The numbers in the boot log are the evidence that the
//! charging sites reached the accounting at all: a pool that nothing charges is a limit that
//! nothing enforces, and the failure mode -- the kernel running out of memory with a pool
//! showing zero -- is exactly the kind that only shows up under load.

use nolx_hal_core::console::HalConsole;
use nolx_mm_core::commit::PoolKind;

use crate::mm::pool::{self, PoolSnapshot};

/// What the three pools are charging, in report order.
#[must_use]
pub fn usage() -> [PoolSnapshot; 3] {
    pool::snapshot()
}

/// Print the pools, and check their accounting while doing it.
///
/// A failure here does not stop the boot: the pools are counters, and a counter that is wrong
/// is a bug to report rather than a reason to refuse to run. The line says so, and the boot
/// smoke test looks for the version that checks out.
pub fn report(console: &mut dyn HalConsole) {
    let _ = write!(console, "obj: pool usage:");
    for snapshot in usage() {
        let _ = write!(
            console,
            " {} {} KiB of {} MiB (peak {} KiB);",
            snapshot.kind.name(),
            snapshot.current / 1024,
            snapshot.limit / (1024 * 1024),
            snapshot.peak / 1024,
        );
    }
    match pool::check() {
        Ok(()) => {
            let _ = writeln!(console, " nonpaged is illegal above PASSIVE, accounting ok");
        }
        Err(error) => {
            let _ = writeln!(console, " accounting FAILED: {error}");
        }
    }
}

/// Whether every pool's accounting checks out.
#[must_use]
pub fn is_consistent() -> bool {
    pool::check().is_ok()
}

/// The kinds this module reports, so a caller can index [`usage`] without importing
/// `nolx-mm-core`.
#[must_use]
pub const fn kinds() -> [PoolKind; 3] {
    PoolKind::all()
}
