//! Boot-time interrupt-vector exhaustion fixture for the xHCI transport.
//!
//! The shared native vector pool has a bounded number of candidates per kind. The rules are
//! host-tested in `nolx-hal-core`, but a host test cannot show what a *real* controller start
//! does when the pool it needs is already owned by somebody else. This fixture does exactly
//! that, against the controller QEMU enumerated:
//!
//! 1. stop the controller and prove its interrupt source and DMA are retired;
//! 2. take every candidate of the xHCI pool for a synthetic handle;
//! 3. start the stopped controller and require the refusal to name `NoVector`, leave the
//!    framework in `Failed`, leak no DMA page or MMIO window, and steal no held vector;
//! 4. recover, release the synthetic claims, and require the controller to start and own a
//!    vector again;
//! 5. stop and remove it, and report the exact DMA pages that came back.
//!
//! Every assertion is an executable one; a failure stops the machine rather than printing a
//! warning. The feature is test-only and is never part of a production image.

use core::sync::atomic::{AtomicBool, Ordering};

use nolx_df_core::{Error, State};

use super::super::irq_vectors::{self, VectorKind};
use super::super::{dma, driver_start};
use super::*;
use crate::mm::mmio;

/// The synthetic handle that holds the pool. It is one no real controller can have: the
/// framework's device table is bounded well below it.
const SYNTHETIC: usize = 0xfe;

static DONE: AtomicBool = AtomicBool::new(false);

pub(super) fn run(console: &mut dyn HalConsole) {
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(dev) = (0..driver_start::DEVICES).find(|&dev| {
        BINDINGS[dev].lock().bdf.is_some() && driver_start::state(dev) == Ok(State::Started)
    }) else {
        let _ = writeln!(
            console,
            "xhci-exhaust: no started controller, fixture skipped"
        );
        return;
    };
    let pages = dma::live_pages();
    let mappings = mmio::live_mappings();
    let _ = writeln!(
        console,
        "xhci-exhaust: handle {dev}, baseline DMA {pages}, MMIO {mappings}"
    );

    // A stopped controller is the state a start is allowed to run from. Its checked stop also
    // proves that the source that used to own a vector is gone before the pool is taken.
    driver_start::stop(dev).unwrap();
    super::irq::detached_test(dev, false);
    assert_eq!(driver_start::state(dev), Ok(State::Stopped));
    assert_eq!(dma::live_pages() + 8, pages, "checked stop retained DMA");
    assert_eq!(
        mmio::live_mappings() + 1,
        mappings,
        "checked stop retained the MMIO window"
    );
    let stopped_pages = dma::live_pages();
    let stopped_mappings = mmio::live_mappings();

    let held = irq_vectors::occupy(VectorKind::Xhci, SYNTHETIC);
    let held_count = held.len();
    assert_eq!(
        held_count,
        irq_vectors::pool(VectorKind::Xhci).candidates().count(),
        "the synthetic owner did not take every candidate in the xHCI pool"
    );
    let _ = writeln!(
        console,
        "xhci-exhaust: synthetic handle took {held_count} vector(s), pool exhausted"
    );

    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert!(
        BINDINGS[dev].lock().log.contains("NoVector"),
        "an exhausted pool was not reported as NoVector:\n{}",
        BINDINGS[dev].lock().log.text()
    );
    assert_eq!(driver_start::state(dev), Ok(State::Failed));
    assert_eq!(dma::live_pages(), stopped_pages, "refused start leaked DMA");
    assert_eq!(
        mmio::live_mappings(),
        stopped_mappings,
        "refused start leaked an MMIO window"
    );
    for vector in &held {
        assert_eq!(
            irq_vectors::owner(*vector),
            Some((VectorKind::Xhci, SYNTHETIC)),
            "a refused start stole a vector from its holder"
        );
    }
    let _ = writeln!(
        console,
        "xhci-exhaust: start refused as NoVector with no DMA/MMIO/ownership leak"
    );

    driver_start::recover(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(State::Stopped));
    for vector in held {
        assert!(irq_vectors::release(vector, VectorKind::Xhci, SYNTHETIC));
    }
    assert_eq!(irq_vectors::outstanding(), 0);

    driver_start::start(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(State::Started));
    assert_eq!(
        dma::live_pages(),
        pages,
        "the restarted controller did not take its DMA back"
    );
    assert_eq!(
        mmio::live_mappings(),
        mappings,
        "the restarted controller did not map its registers again"
    );
    let _ = writeln!(
        console,
        "xhci-exhaust: released pool reacquired by the controller, DMA/MMIO restored"
    );

    driver_start::stop(dev).unwrap();
    super::irq::detached_test(dev, false);
    driver_start::remove(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(State::Removed));
    assert_eq!(dma::live_pages() + 8, pages);
    assert_eq!(mmio::live_mappings() + 1, mappings);
    assert_eq!(irq_vectors::outstanding(), 0);
    crate::mm::audit().unwrap();
    let _ = writeln!(
        console,
        "xhci-exhaust: PASS {held_count}-vector pool exhaustion refused and reclaimed, restart and removal exact, final DMA {}",
        dma::live_pages()
    );
}
