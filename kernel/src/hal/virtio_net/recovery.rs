//! Feature-only ownership failures; real RX/TX and IRQs are never simulated.
use super::*;
use core::sync::atomic::AtomicBool;
use nolx_df_core::{Error, State as FrameworkState};
static TARGET: AtomicUsize = AtomicUsize::new(usize::MAX);
static MODE: AtomicUsize = AtomicUsize::new(3);
static REFUSALS: AtomicUsize = AtomicUsize::new(0);
static PCI_PROOF: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
pub(super) fn claim(dev: usize) {
    let _ = TARGET.compare_exchange(usize::MAX, dev, Ordering::AcqRel, Ordering::Acquire);
}
pub(super) fn fail(dev: usize, point: usize) -> bool {
    if dev != TARGET.load(Ordering::Acquire)
        || MODE
            .compare_exchange(point, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return false;
    }
    if point != 1 {
        REFUSALS.store(2, Ordering::Release);
    }
    true
}
pub(super) fn refuse_stop(dev: usize) -> bool {
    dev == TARGET.load(Ordering::Acquire)
        && REFUSALS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
}
pub(super) fn withhold_pci_proof(dev: usize) -> bool {
    dev == TARGET.load(Ordering::Acquire) && PCI_PROOF.swap(false, Ordering::AcqRel)
}
fn refused(dev: usize) {
    assert!(matches!(mac(dev), Err(Failure::NoDevice)));
    assert!(matches!(
        send_frame_on(dev, &[0; 60]),
        Err(Failure::NoDevice)
    ));
    assert!(matches!(receive_frame_on(dev), Err(Failure::NoDevice)));
}
fn detached(dev: usize, vectors: (u8, u8), retained: bool) {
    assert!(!LIVE[dev].load(Ordering::Acquire));
    let before = interrupt_counts(dev);
    for (vector, kind) in [(vectors.0, RX_KIND), (vectors.1, TX_KIND)] {
        assert_eq!(
            irq_vectors::owner(vector),
            if retained { Some((kind, dev)) } else { None },
            "vector ownership did not follow checked-stop proof"
        );
        on_interrupt(vector);
    }
    assert_eq!(interrupt_counts(dev), before);
}
fn pinned(
    dev: usize,
    peer: usize,
    region: Region,
    mappings: usize,
    vector: (u8, u8),
    master: bool,
) {
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Failed));
    assert_eq!(driver_start::state(peer), Ok(FrameworkState::Started));
    assert_eq!(super::super::dma::live_pages(), 8);
    assert_eq!(mmio::live_mappings(), mappings);
    assert!(!empty_dma(dev));
    if DEVICE[dev].lock().is_none() {
        region.write16(u64::from(common::QUEUE_SELECT), 0).unwrap();
        assert_eq!(region.read16(u64::from(common::QUEUE_ENABLE)).unwrap(), 1);
        region.write16(u64::from(common::QUEUE_SELECT), 1).unwrap();
        assert_eq!(region.read16(u64::from(common::QUEUE_ENABLE)).unwrap(), 0);
        assert_eq!(
            region.read8(u64::from(common::DEVICE_STATUS)).unwrap() & 4,
            0
        );
    }
    assert!(region.read8(u64::from(common::DEVICE_STATUS)).is_ok());
    let command = ConfigAccess.read_u32(BDFS[dev].lock().unwrap(), 4);
    assert_eq!(command & 4 != 0, master);
    assert_ne!(command & (1 << 10), 0);
    assert!(messages_disabled(BDFS[dev].lock().unwrap()));
    detached(dev, vector, true);
    refused(dev);
    assert_eq!(driver_start::start(dev), Err(Error::BadState));
    assert_eq!(driver_start::remove(dev), Err(Error::Busy));
}
fn recovered(dev: usize, region: Region, mappings: usize, vector: (u8, u8)) {
    driver_start::recover(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Stopped));
    assert!(removable(dev));
    assert_eq!(super::super::dma::live_pages(), 4);
    assert_eq!(mmio::live_mappings() + 5, mappings);
    assert!(region.read32(0).is_err());
    detached(dev, vector, false);
    refused(dev);
}
fn exchange(dev: usize, sequence: u8, expected: [u8; 6], console: &mut dyn HalConsole) {
    let mac = mac(dev).unwrap();
    assert_eq!(mac, expected);
    let local = [10, 0, 2, 64 + sequence];
    let gateway = [10, 0, 2, 2];
    let before = interrupt_counts(dev).0;
    send_frame_on(dev, &arp::request(mac, local, gateway)).unwrap();
    poll(|| {
        if let Some(frame) = receive_frame_on(dev)? {
            assert_eq!(&frame.bytes()[..6], &mac, "peer FIFO crossed owners");
            return Ok(arp::reply(frame.bytes(), mac, local, gateway).is_some());
        }
        Ok(false)
    })
    .unwrap();
    assert!(super::super::msix::wait_for_interrupts(&RX_INTERRUPTS[dev], before + 1) > before);
    let _=writeln!(console,"virtio-net-recovery: exchange={sequence} handle={dev} mac={mac:02x?} local={} selected IRQ progressed",local[3]);
}
pub(super) fn run(console: &mut dyn HalConsole) {
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let dev = TARGET.load(Ordering::Acquire);
    let peer = driver_start::started()
        .unwrap()
        .into_iter()
        .flatten()
        .find(|(_, n)| *n == "virtio-net")
        .unwrap()
        .0;
    assert_ne!(dev, peer);
    let peer_mac = mac(peer).unwrap();
    let target_mac = [2, 0, 0, 0, 0, 0x20];
    assert_eq!(peer_mac, [2, 0, 0, 0, 0, 0x21]);
    let mappings = mmio::live_mappings();
    let mut region = COMMON[dev].lock().unwrap();
    let target_vectors = vectors(dev).expect("target started with two owned vectors");
    let peer_vectors = vectors(peer).expect("peer started with two owned vectors");
    assert_ne!(target_vectors.0, target_vectors.1);
    assert_ne!(peer_vectors.0, peer_vectors.1);
    assert_ne!(target_vectors.0, peer_vectors.0);
    assert_ne!(target_vectors.0, peer_vectors.1);
    assert_ne!(target_vectors.1, peer_vectors.0);
    assert_ne!(target_vectors.1, peer_vectors.1);
    pinned(dev, peer, region, mappings, target_vectors, true);
    assert_eq!(driver_start::recover(peer), Err(Error::BadState));
    assert_eq!(
        driver_start::recover(driver_start::DEVICES),
        Err(Error::UnknownDevice)
    );
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, target_vectors, true);
    probe(console); // A retained failed BDF must not bind a second owner.
    pinned(dev, peer, region, mappings, target_vectors, true);
    exchange(peer, 0, peer_mac, console);
    recovered(dev, region, mappings, target_vectors);
    driver_start::start(dev).unwrap();
    assert!(LIVE[dev].load(Ordering::Acquire));
    assert_eq!(vectors(dev), Some(target_vectors));
    let _ = writeln!(
        console,
        "virtio-net-recovery: vectors {:#x}/{:#x} released after checked stop and reclaimed on restart",
        target_vectors.0,
        target_vectors.1
    );
    let unowned = (0x50..=0xdf)
        .find(|vector| irq_vectors::owner(*vector).is_none())
        .expect("the bounded vector domain must have an unowned vector");
    let before = interrupt_counts(dev);
    on_interrupt(unowned);
    assert_eq!(
        interrupt_counts(dev),
        before,
        "an unowned vector reached a live queue"
    );
    exchange(dev, 1, target_mac, console);
    exchange(peer, 2, peer_mac, console);
    let _=writeln!(console,"virtio-net-recovery: post-ARP startup quarantine recovered; DMA 8 -> 4 -> 8, independent MSI-X live");

    region = COMMON[dev].lock().unwrap();
    REFUSALS.store(2, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, target_vectors, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, target_vectors, true);
    exchange(peer, 3, peer_mac, console);
    recovered(dev, region, mappings, target_vectors);
    driver_start::start(dev).unwrap();
    exchange(dev, 4, target_mac, console);
    exchange(peer, 5, peer_mac, console);
    let _=writeln!(console,"virtio-net-recovery: failed stop/repeated refusal retained 8 pages, stale MMIO and IRQ source retired after recovery");

    driver_start::stop(dev).unwrap();
    MODE.store(1, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert_eq!(
        super::super::dma::live_pages(),
        4,
        "three-page allocation rollback leaked"
    );
    assert_eq!(mmio::live_mappings() + 5, mappings);
    assert!(removable(dev));
    driver_start::recover(dev).unwrap();
    driver_start::start(dev).unwrap();
    exchange(dev, 6, target_mac, console);
    let _ = writeln!(
        console,
        "virtio-net-recovery: partial three-page allocation rollback reclaimed before exposure"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(2, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = COMMON[dev].lock().unwrap();
    pinned(dev, peer, region, mappings, target_vectors, false);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, target_vectors, false);
    exchange(peer, 7, peer_mac, console);
    recovered(dev, region, mappings, target_vectors);
    driver_start::start(dev).unwrap();
    exchange(dev, 8, target_mac, console);
    let _ = writeln!(
        console,
        "virtio-net-recovery: RX-queue-only partial startup retained all four pages until checked reset"
    );

    region = COMMON[dev].lock().unwrap();
    PCI_PROOF.store(true, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, target_vectors, false);
    exchange(peer, 9, peer_mac, console);
    recovered(dev, region, mappings, target_vectors);
    driver_start::start(dev).unwrap();
    exchange(dev, 10, target_mac, console);
    let _=writeln!(console,"virtio-net-recovery: deliberately withheld PCI exclusion proof pinned DMA despite actual bus-master disable");
    driver_start::stop(dev).unwrap();
    driver_start::stop(peer).unwrap();
    detached(dev, target_vectors, false);
    detached(peer, peer_vectors, false);
    assert_eq!(super::super::dma::live_pages(), 0);
    assert_eq!(mmio::live_mappings() + 10, mappings);
    driver_start::remove(dev).unwrap();
    driver_start::remove(peer).unwrap();
    crate::mm::audit().unwrap();
    assert_eq!(driver_start::quiesce(console), Ok(0));
    assert_eq!(driver_start::recover(dev), Err(Error::Busy));
    assert!(matches!(mac(peer), Err(Failure::Busy)));
    let _=writeln!(console,"virtio-net-recovery: PASS eleven fresh wire exchanges, checked failure recovery, independent MSI-X peer, stale mappings, freeze refusal, final DMA 0");
}
