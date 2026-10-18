//! Dedicated executable fault probes for the NVMe transport. Refused cleanup, refused
//! allocation, a refused flush and withheld proof are injected; QEMU is not claimed to
//! naturally fail a controller disable or a PCI write. Real queued I/O still runs on both
//! controllers, and every durable write is checked on the media afterwards.
use super::*;
use nolx_df_core::{Error, State as FrameworkState};

/// A vector of this driver's range that no controller owns. The ledger, not the range,
/// decides who owns a vector, so this one reaches no controller at all.
const UNOWNED: u8 = 0xaf;
/// The handle the fixture injects failures into.
static TARGET: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The failure point the next start or stop should hit.
static MODE: AtomicUsize = AtomicUsize::new(3);
/// How many stop refusals are armed.
static REFUSALS: AtomicUsize = AtomicUsize::new(0);
/// Whether the next stop withholds its PCI exclusion proof.
static PCI_PROOF: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// Claim the first controller the driver attaches.
pub(super) fn claim(dev: usize) {
    let _ = TARGET.compare_exchange(usize::MAX, dev, Ordering::AcqRel, Ordering::Acquire);
}

/// Fail the target once, at the named point. The points that leave the controller running
/// also arm the stop refusals, so the rollback after a failed start keeps the owners.
pub(super) fn fail(dev: usize, point: usize) -> bool {
    if dev != TARGET.load(Ordering::Acquire)
        || MODE
            .compare_exchange(point, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return false;
    }
    if point == 2 || point == 3 || point == 5 {
        REFUSALS.store(2, Ordering::Release);
    }
    true
}

/// Refuse the next stop of the target.
pub(super) fn refuse_stop(dev: usize) -> bool {
    dev == TARGET.load(Ordering::Acquire)
        && REFUSALS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
}

/// Withhold the PCI exclusion proof of the next stop of the target, even though the
/// hardware really is excluded.
pub(super) fn withhold_pci_proof(dev: usize) -> bool {
    dev == TARGET.load(Ordering::Acquire) && PCI_PROOF.swap(false, Ordering::AcqRel)
}

/// The public path must refuse a controller that is failed, stopped or quarantined.
fn refused(dev: usize) {
    let id = blockdev::device(dev).unwrap();
    assert_eq!(
        blockdev::read_sectors(id, 0, &mut [0; 512]),
        Err(nolxfs::DevError::Io(19))
    );
    assert_eq!(
        blockdev::write_sectors(id, 1770, &[0; 512]),
        Err(nolxfs::DevError::Io(19))
    );
    assert_eq!(blockdev::flush(id), Err(nolxfs::DevError::Io(19)));
}

/// A stopped or failed controller must not be acknowledged by the interrupt handler, and
/// a vector nobody owns must not advance any counter.
fn detached(dev: usize, vector: u8) {
    assert!(!LIVE[dev].load(Ordering::Acquire));
    assert!(super::super::irq_vectors::owner(UNOWNED).is_none());
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    // The vector this controller used to own, the highest vector of this driver's range
    // that no controller owns, and a vector of another driver: none of them may count.
    on_completion_interrupt(vector);
    on_completion_interrupt(UNOWNED);
    on_completion_interrupt(0x20);
    assert_eq!(COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire), before);
}

/// The target is failed, its owners are still held, and the peer is untouched.
fn pinned(dev: usize, peer: usize, region: Region, mappings: usize, master: bool) {
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Failed));
    assert_eq!(driver_start::state(peer), Ok(FrameworkState::Started));
    // A failed controller may still raise its interrupt, so it keeps its vector.
    let vector = STATE[dev].lock().vector.unwrap();
    assert_eq!(
        super::super::irq_vectors::owner(vector),
        Some((super::super::irq_vectors::VectorKind::Nvme, dev))
    );
    // The target holds six pages and two windows; the peer holds the same.
    assert_eq!(super::super::dma::live_pages(), 12);
    assert_eq!(mmio::live_mappings(), mappings);
    assert!(!empty_dma(dev));
    assert!(region.read32(0).is_ok());
    let command = ConfigAccess.read_u32(STATE[dev].lock().bdf.unwrap(), 4);
    assert_eq!(command & 4 != 0, master);
    assert_ne!(command & (1 << 10), 0);
    assert!(super::super::pci_exclusion::messages_disabled(
        STATE[dev].lock().bdf.unwrap()
    ));
    detached(dev, vector);
    refused(dev);
    assert_eq!(driver_start::start(dev), Err(Error::BadState));
    assert_eq!(driver_start::remove(dev), Err(Error::Busy));
}

/// The checked stop released every owner of the target, and the peer still works.
fn recovered(dev: usize, region: Region, mappings: usize) {
    let vector = STATE[dev].lock().vector.unwrap();
    driver_start::recover(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Stopped));
    assert!(removable(dev));
    assert_eq!(super::super::dma::live_pages(), 6);
    assert_eq!(mmio::live_mappings() + 2, mappings);
    assert!(region.read32(0).is_err());
    // The controller can no longer raise its vector, so the vector is free again and the
    // next controller of this kind may take it.
    assert_eq!(STATE[dev].lock().vector, None);
    assert_eq!(super::super::irq_vectors::owner(vector), None);
    detached(dev, vector);
    refused(dev);
}

/// The media tag the host seeded, read through the controller's own disk.
fn media_tag(dev: usize) -> u8 {
    let id = blockdev::device(dev).unwrap();
    let mut bytes = [0u8; 512];
    blockdev::read_sectors(id, 1740, &mut bytes).unwrap();
    assert!(
        bytes.iter().all(|&byte| byte == bytes[0]),
        "the media tag sector is not uniform"
    );
    bytes[0]
}

/// One durable exchange: write, flush and read back a sector, and prove that the other
/// controller's media is untouched.
fn exchange(dev: usize, other: usize, sequence: u8, console: &mut dyn HalConsole) {
    let id = blockdev::device(dev).unwrap();
    let peer = blockdev::device(other).unwrap();
    assert_ne!(id, peer);
    let tag = media_tag(dev);
    let mut bytes = [0u8; 512];
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    let data = [tag ^ 0x80 ^ sequence; 512];
    let lba = 1770 + u64::from(sequence);
    blockdev::write_sectors(id, lba, &data).unwrap();
    blockdev::flush(id).unwrap();
    blockdev::read_sectors(id, lba, &mut bytes).unwrap();
    assert_eq!(bytes, data);
    assert!(
        COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire) >= before + 3,
        "the selected controller's completion interrupt did not progress"
    );
    if driver_start::state(other) == Ok(FrameworkState::Started) {
        assert_ne!(
            media_tag(other),
            tag,
            "two controllers selected the same media"
        );
        blockdev::read_sectors(peer, lba, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 512], "writes crossed controller/media ownership");
    }
    let _ = writeln!(
        console,
        "nvme-recovery: exchange={sequence} handle={dev} block={id} tag={tag} lba={lba} write/flush/read selected IRQ progressed"
    );
}

/// The controller configuration as the last checked stop left it.
fn controller_disabled(dev: usize) -> bool {
    STOPPED_CONTROL[dev].load(Ordering::Acquire) & nvme::CC_EN == 0
}

pub(super) fn run(console: &mut dyn HalConsole) {
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let dev = TARGET.load(Ordering::Acquire);
    if dev == usize::MAX {
        // No controller of this kind is on the bus: there is nothing to exercise.
        return;
    }
    if driver_start::frozen() {
        // Another transport's fixture already closed the registry: admission stays closed.
        return;
    }
    let peer = driver_start::started()
        .unwrap()
        .into_iter()
        .flatten()
        .find(|(_, name)| *name == "nvme")
        .unwrap()
        .0;
    assert_ne!(dev, peer);
    let mappings = mmio::live_mappings();
    let mut region = STATE[dev].lock().regs.unwrap();
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    probe(console);
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 0, console);
    let reclaimed = STATE[dev].lock().vector.unwrap();
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    // The reclaimed vector is taken again: a stop and restart does not consume a new one.
    assert_eq!(STATE[dev].lock().vector, Some(reclaimed));
    // A vector that no controller owns reaches no controller at all.
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    on_completion_interrupt(UNOWNED);
    assert_eq!(COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire), before);
    exchange(dev, peer, 1, console);
    exchange(peer, dev, 2, console);
    let _ = writeln!(
        console,
        "nvme-recovery: interrupt vector {reclaimed:#x} released by the checked stop and taken again on restart"
    );
    let _ = writeln!(
        console,
        "nvme-recovery: post-filesystem startup quarantine recovered; DMA 12 -> 6 -> 12, independent MSI-X peer live"
    );

    region = STATE[dev].lock().regs.unwrap();
    REFUSALS.store(2, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 3, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 4, console);
    exchange(peer, dev, 5, console);
    let _ = writeln!(
        console,
        "nvme-recovery: failed stop/repeated refusal retained owners; stale window and IRQ retired after recovery"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(1, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert_eq!(super::super::dma::live_pages(), 6);
    assert_eq!(mmio::live_mappings() + 2, mappings);
    assert!(removable(dev));
    driver_start::recover(dev).unwrap();
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 6, console);
    let _ = writeln!(
        console,
        "nvme-recovery: admin-queue allocation rollback reclaimed before the controller named the queue"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(2, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = STATE[dev].lock().regs.unwrap();
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 7, console);
    recovered(dev, region, mappings);
    assert!(controller_disabled(dev));
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 8, console);
    let _ = writeln!(
        console,
        "nvme-recovery: enabled-controller partial startup retained all six pages until the checked disable"
    );

    region = STATE[dev].lock().regs.unwrap();
    PCI_PROOF.store(true, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, false);
    exchange(peer, dev, 9, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 10, console);
    let _ = writeln!(
        console,
        "nvme-recovery: withheld PCI exclusion proof retained all six pages despite actual bus-master disable"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(5, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = STATE[dev].lock().regs.unwrap();
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 11, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 12, console);
    let _ = writeln!(
        console,
        "nvme-recovery: published-command failure quarantined without flush/buffer reuse until the checked disable"
    );

    region = STATE[dev].lock().regs.unwrap();
    MODE.store(4, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 13, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 14, console);
    let _ = writeln!(
        console,
        "nvme-recovery: refused flush quarantined without reset/free; explicit recovery avoided unsafe flush retry"
    );
    let target_vector = STATE[dev].lock().vector.unwrap();
    let peer_vector = STATE[peer].lock().vector.unwrap();
    driver_start::stop(dev).unwrap();
    driver_start::stop(peer).unwrap();
    // Both controllers are stopped, so both vectors are free again.
    let vectors = [STATE[dev].lock().vector, STATE[peer].lock().vector];
    assert_eq!(vectors, [None, None]);
    assert_eq!(super::super::irq_vectors::outstanding(), 0);
    detached(dev, target_vector);
    detached(peer, peer_vector);
    assert_eq!(super::super::dma::live_pages(), 0);
    // The released vectors are taken again, by the other controller: the table hands out
    // the free vector that is lowest, not a fixed one per controller.
    driver_start::start(peer).unwrap();
    assert_eq!(STATE[peer].lock().vector, Some(target_vector));
    driver_start::start(dev).unwrap();
    assert_eq!(STATE[dev].lock().vector, Some(peer_vector));
    driver_start::stop(dev).unwrap();
    driver_start::stop(peer).unwrap();
    assert_eq!(super::super::irq_vectors::outstanding(), 0);
    let _ = writeln!(
        console,
        "nvme-recovery: released vectors {target_vector:#x} and {peer_vector:#x} were taken again by the other controller"
    );
    // The pool of this kind is bounded: when every vector of it is taken, the start is
    // refused instead of sharing a vector with a live controller.
    let kind = super::super::irq_vectors::VectorKind::Nvme;
    let held = super::super::irq_vectors::occupy(kind, 0xfe);
    assert_eq!(
        held.len(),
        super::super::irq_vectors::pool(kind).candidates().count()
    );
    assert!(super::super::irq_vectors::claim(kind, 0xfe).is_none());
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert!(last_failure_was_no_vector(dev));
    assert_eq!(STATE[dev].lock().vector, None);
    let _ = writeln!(
        console,
        "nvme-recovery: the vector pool of {} vectors is exhausted and the start is refused",
        held.len()
    );
    for vector in held {
        assert!(super::super::irq_vectors::release(vector, kind, 0xfe));
    }
    assert_eq!(super::super::irq_vectors::outstanding(), 0);
    // With the vectors free again the controller starts and owns one again.
    driver_start::recover(dev).unwrap();
    driver_start::start(dev).unwrap();
    assert!(STATE[dev].lock().vector.is_some());
    driver_start::stop(dev).unwrap();
    assert_eq!(super::super::irq_vectors::outstanding(), 0);
    assert_eq!(mmio::live_mappings() + 4, mappings);
    driver_start::remove(dev).unwrap();
    driver_start::remove(peer).unwrap();
    crate::mm::audit().unwrap();
    assert_eq!(driver_start::quiesce(console), Ok(0));
    assert_eq!(driver_start::recover(dev), Err(Error::Busy));
    assert_eq!(
        blockdev::flush(blockdev::device(peer).unwrap()),
        Err(nolxfs::DevError::Io(16))
    );
    let _ = writeln!(
        console,
        "nvme-recovery: PASS fifteen durable media exchanges, checked failure recovery, independent MSI-X peer, stale window, freeze refusal, final DMA 0"
    );
}
