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
    assert!(matches!(mac(dev), Err(Failure::NoController)));
    assert!(matches!(
        send_frame_on(dev, &[0; 60]),
        Err(Failure::NoController)
    ));
    assert!(matches!(receive_frame_on(dev), Err(Failure::NoController)));
}
fn detached(dev: usize, vector: u8) {
    assert_eq!(RX_VECTOR[dev].load(Ordering::Acquire), 0);
    let count = RX_INTERRUPTS[dev].load(Ordering::Acquire);
    on_interrupt(vector); // Shared peer may be serviced; retired target must not be touched.
    assert_eq!(RX_INTERRUPTS[dev].load(Ordering::Acquire), count);
}
fn pinned(dev: usize, peer: usize, region: Region, mappings: usize, vector: u8, master: bool) {
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Failed));
    assert_eq!(driver_start::state(peer), Ok(FrameworkState::Started));
    assert_eq!(super::super::dma::live_pages(), 22);
    assert_eq!(mmio::live_mappings(), mappings);
    assert!(STATE[dev].lock().rings.is_some());
    assert!(RECEIVE[dev].lock().is_some());
    assert_eq!(RX_VECTOR[dev].load(Ordering::Acquire), 0);
    let Some((route, held_vector)) = *PCI_ROUTE[dev].lock() else {
        panic!("failed e1000 retained no shared INTx route reference");
    };
    assert_eq!(held_vector, vector);
    assert_eq!(
        super::super::irq_vectors::owner(vector),
        Some((INTX_KIND, route.gsi as usize))
    );
    assert!(region.read32(e1000::REG_CTRL).is_ok());
    let bdf = STATE[dev].lock().bdf.unwrap();
    let command = ConfigAccess.read_u32(bdf, 4);
    assert_eq!(command & 4 != 0, master);
    assert_ne!(command & (1 << 10), 0);
    assert_eq!(region.read32(e1000::REG_IMS).unwrap(), 0);
    detached(dev, vector);
    refused(dev);
    assert_eq!(driver_start::start(dev), Err(Error::BadState));
    assert_eq!(driver_start::remove(dev), Err(Error::Busy));
}
fn recovered(
    dev: usize,
    region: Region,
    mappings: usize,
    route: Interrupt,
    vector: u8,
) {
    driver_start::recover(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Stopped));
    assert!(removable(dev));
    assert!(PCI_ROUTE[dev].lock().is_none());
    assert_eq!(super::super::ioapic::pci_route_references(route.gsi), 1);
    assert_eq!(
        super::super::irq_vectors::owner(vector),
        Some((INTX_KIND, route.gsi as usize)),
        "shared route vector was released while its peer is still active"
    );
    assert_eq!(super::super::dma::live_pages(), 11);
    assert_eq!(mmio::live_mappings() + 1, mappings);
    assert!(region.read32(0).is_err());
    detached(dev, vector);
    refused(dev);
}
fn route_owned(
    dev: usize,
    irq: Interrupt,
    vector: u8,
    references: usize,
    phase: &'static str,
) {
    assert_eq!(*PCI_ROUTE[dev].lock(), Some((irq, vector)));
    assert_eq!(
        RX_VECTOR[dev].load(Ordering::Acquire),
        vector,
        "{phase}: handle {dev} lost vector {vector:#x} for GSI {} (state {:?}, BDF {:?}, route {:?}, owner {:?})",
        irq.gsi,
        driver_start::state(dev),
        STATE[dev].lock().bdf,
        *PCI_ROUTE[dev].lock(),
        super::super::irq_vectors::owner(vector)
    );
    assert_eq!(
        super::super::irq_vectors::owner(vector),
        Some((INTX_KIND, irq.gsi as usize))
    );
    assert_eq!(
        super::super::ioapic::pci_route_references(irq.gsi),
        references
    );
}
fn exchange(dev: usize, sequence: u8, expected: [u8; 6], console: &mut dyn HalConsole) {
    let mac = mac(dev).unwrap();
    assert_eq!(mac, expected);
    let local = [10, 0, 2, 64 + sequence];
    let gateway = [10, 0, 2, 2];
    let before = interrupt_count(dev);
    send_frame_on(dev, &arp::request(mac, local, gateway)).unwrap();
    poll(
        || {
            if let Some(frame) = receive_frame_on(dev)? {
                assert_eq!(&frame.bytes()[..6], &mac, "peer FIFO crossed owners");
                return Ok(arp::reply(frame.bytes(), mac, local, gateway).is_some());
            }
            Ok(false)
        },
        "recovery fresh public ARP",
    )
    .unwrap();
    assert!(super::super::msix::wait_for_interrupts(&RX_INTERRUPTS[dev], before + 1) > before);
    let _=writeln!(console,"e1000-recovery: exchange={sequence} handle={dev} mac={mac:02x?} local={} selected IRQ progressed",local[3]);
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
        .find(|(_, n)| *n == "e1000")
        .unwrap()
        .0;
    assert_ne!(dev, peer);
    let peer_mac = mac(peer).unwrap();
    let target_mac = [2, 0, 0, 0, 0, 0x20];
    assert_eq!(peer_mac, [2, 0, 0, 0, 0, 0x21]);
    let mappings = mmio::live_mappings();
    let mut region = STATE[dev].lock().region.unwrap();
    let (target_irq, target_vector) = PCI_ROUTE[dev].lock().unwrap();
    let (peer_irq, vector) = PCI_ROUTE[peer].lock().unwrap();
    assert_eq!(target_irq, peer_irq, "fixture must share one GSI route");
    assert_eq!(target_vector, vector);
    assert_eq!(
        super::super::irq_vectors::owner(vector),
        Some((INTX_KIND, target_irq.gsi as usize))
    );
    assert_eq!(super::super::ioapic::pci_route_references(target_irq.gsi), 2);
    route_owned(dev, target_irq, vector, 2, "initial target");
    route_owned(peer, peer_irq, vector, 2, "initial peer");
    pinned(dev, peer, region, mappings, vector, true);
    assert_eq!(driver_start::recover(peer), Err(Error::BadState));
    assert_eq!(
        driver_start::recover(driver_start::DEVICES),
        Err(Error::UnknownDevice)
    );
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, vector, true);
    probe(console); // A retained failed BDF must not bind a second owner.
    pinned(dev, peer, region, mappings, vector, true);
    exchange(peer, 0, peer_mac, console);
    recovered(dev, region, mappings, target_irq, vector);
    driver_start::start(dev).unwrap();
    route_owned(dev, target_irq, vector, 2, "post-startup-quarantine recovery");
    exchange(dev, 1, target_mac, console);
    exchange(peer, 2, peer_mac, console);
    let _=writeln!(console,"e1000-recovery: post-ARP startup quarantine recovered; DMA 22 -> 11 -> 22, shared INTx live");

    region = STATE[dev].lock().region.unwrap();
    REFUSALS.store(2, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, vector, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, vector, true);
    exchange(peer, 3, peer_mac, console);
    recovered(dev, region, mappings, target_irq, vector);
    driver_start::start(dev).unwrap();
    route_owned(dev, target_irq, vector, 2, "failed-stop recovery");
    exchange(dev, 4, target_mac, console);
    exchange(peer, 5, peer_mac, console);
    let _=writeln!(console,"e1000-recovery: failed stop/repeated refusal retained 22 pages, stale MMIO and IRQ source retired after recovery");

    driver_start::stop(dev).unwrap();
    route_owned(peer, peer_irq, vector, 1, "peer after target stop");
    MODE.store(1, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert_eq!(
        super::super::dma::live_pages(),
        11,
        "three-page allocation rollback leaked"
    );
    assert_eq!(mmio::live_mappings() + 1, mappings);
    assert!(removable(dev));
    driver_start::recover(dev).unwrap();
    driver_start::start(dev).unwrap();
    route_owned(dev, target_irq, vector, 2, "allocation-failure recovery");
    exchange(dev, 6, target_mac, console);
    let _ = writeln!(
        console,
        "e1000-recovery: partial three-page allocation rollback reclaimed before exposure"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(2, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = STATE[dev].lock().region.unwrap();
    pinned(dev, peer, region, mappings, vector, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, vector, true);
    exchange(peer, 7, peer_mac, console);
    recovered(dev, region, mappings, target_irq, vector);
    driver_start::start(dev).unwrap();
    route_owned(dev, target_irq, vector, 2, "RX-only recovery");
    exchange(dev, 8, target_mac, console);
    let _ = writeln!(
        console,
        "e1000-recovery: RX-only partial startup retained all eleven pages until checked reset"
    );

    region = STATE[dev].lock().region.unwrap();
    PCI_PROOF.store(true, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, vector, false);
    exchange(peer, 9, peer_mac, console);
    recovered(dev, region, mappings, target_irq, vector);
    driver_start::start(dev).unwrap();
    route_owned(dev, target_irq, vector, 2, "withheld-PCI-proof recovery");
    exchange(dev, 10, target_mac, console);
    let _=writeln!(console,"e1000-recovery: deliberately withheld PCI exclusion proof pinned DMA despite actual bus-master disable");
    driver_start::stop(dev).unwrap();
    assert_eq!(super::super::ioapic::pci_route_references(target_irq.gsi), 1);
    driver_start::stop(peer).unwrap();
    assert_eq!(super::super::ioapic::pci_route_references(target_irq.gsi), 0);
    assert_eq!(super::super::irq_vectors::owner(vector), None);
    let _ = writeln!(
        console,
        "e1000-recovery: shared GSI {} vector {:#x} released only after last checked stop",
        target_irq.gsi,
        vector
    );
    detached(dev, vector);
    detached(peer, vector);
    assert_eq!(super::super::dma::live_pages(), 0);
    assert_eq!(mmio::live_mappings() + 2, mappings);

    // Force a programming readback failure followed by an inconclusive remask readback.
    // The route manager must retain the claimed vector and refuse a second claim for this GSI.
    let vectors_before_quarantine = super::super::irq_vectors::outstanding();
    super::super::ioapic::inject_pci_route_remask_readback_failure();
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    driver_start::recover(dev).unwrap();
    assert_eq!(super::super::ioapic::pci_route_references(target_irq.gsi), 0);
    assert!(PCI_ROUTE[dev].lock().is_none());
    assert!(removable(dev));
    let quarantined_vector = super::super::ioapic::pci_route_quarantined_vector(target_irq.gsi)
        .expect("unverified route mask did not quarantine its vector");
    assert!(super::super::irq_vectors::is_managed_vector(quarantined_vector));
    assert_eq!(
        super::super::irq_vectors::owner(quarantined_vector),
        Some((INTX_KIND, target_irq.gsi as usize))
    );
    assert_eq!(
        super::super::irq_vectors::outstanding(),
        vectors_before_quarantine + 1
    );
    detached(dev, quarantined_vector);

    // A retry is refused from the per-GSI quarantine record, not given a fresh vector.
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    driver_start::recover(dev).unwrap();
    assert_eq!(
        super::super::ioapic::pci_route_quarantined_vector(target_irq.gsi),
        Some(quarantined_vector)
    );
    assert_eq!(
        super::super::irq_vectors::owner(quarantined_vector),
        Some((INTX_KIND, target_irq.gsi as usize))
    );
    assert_eq!(
        super::super::irq_vectors::outstanding(),
        vectors_before_quarantine + 1
    );
    assert_eq!(super::super::ioapic::pci_route_references(target_irq.gsi), 0);
    assert!(removable(dev));
    let _ = writeln!(
        console,
        "e1000-recovery: inconclusive remask readback quarantined GSI {} vector {:#x}; retry made no duplicate claim",
        target_irq.gsi,
        quarantined_vector
    );

    driver_start::remove(dev).unwrap();
    driver_start::remove(peer).unwrap();
    crate::mm::audit().unwrap();
    assert_eq!(driver_start::quiesce(console), Ok(0));
    assert_eq!(driver_start::recover(dev), Err(Error::Busy));
    assert!(matches!(mac(peer), Err(Failure::Busy)));
    let _=writeln!(console,"e1000-recovery: PASS eleven fresh wire exchanges, checked failure recovery, shared INTx peer, stale mappings, freeze refusal, final DMA 0");
}
