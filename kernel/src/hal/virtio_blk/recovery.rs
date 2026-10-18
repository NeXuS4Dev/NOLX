//! Dedicated executable fault probes. Refused cleanup/proof is injected; QEMU is not
//! claimed to naturally fail reset or PCI writes. Real queued I/O still runs on both disks.
use super::*;
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
    if point == 2 || point == 3 || point == 5 {
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
fn detached(dev: usize) {
    assert!(!LIVE[dev].load(Ordering::Acquire));
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    on_completion_interrupt(completion_vector(dev));
    on_completion_interrupt(0x60 + dev as u8);
    on_completion_interrupt(0x20);
    assert_eq!(COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire), before);
}
fn pinned(dev: usize, peer: usize, region: Region, mappings: usize, master: bool) {
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Failed));
    assert_eq!(driver_start::state(peer), Ok(FrameworkState::Started));
    assert_eq!(super::super::dma::live_pages(), 4);
    assert_eq!(mmio::live_mappings(), mappings);
    assert!(!empty_dma(dev));
    assert!(region.read8(at(common::DEVICE_STATUS)).is_ok());
    let command = ConfigAccess.read_u32(STATE[dev].lock().bdf.unwrap(), 4);
    assert_eq!(command & 4 != 0, master);
    assert_ne!(command & (1 << 10), 0);
    assert!(super::super::virtio_pci::messages_disabled(
        STATE[dev].lock().bdf.unwrap()
    ));
    detached(dev);
    refused(dev);
    assert_eq!(driver_start::start(dev), Err(Error::BadState));
    assert_eq!(driver_start::remove(dev), Err(Error::Busy));
}
fn recovered(dev: usize, region: Region, mappings: usize) {
    driver_start::recover(dev).unwrap();
    assert_eq!(driver_start::state(dev), Ok(FrameworkState::Stopped));
    assert!(removable(dev));
    assert_eq!(super::super::dma::live_pages(), 2);
    assert_eq!(mmio::live_mappings() + 4, mappings);
    assert!(region.read32(0).is_err());
    detached(dev);
    refused(dev);
}
fn exchange(dev: usize, other: usize, sequence: u8, tag: u8, console: &mut dyn HalConsole) {
    let id = blockdev::device(dev).unwrap();
    let peer = blockdev::device(other).unwrap();
    assert_ne!(id, peer);
    let mut bytes = [0; 512];
    blockdev::read_sectors(id, 1740, &mut bytes).unwrap();
    assert_eq!(bytes, [tag; 512]);
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    let data = [tag ^ 0x80 ^ sequence; 512];
    let lba = 1770 + u64::from(sequence);
    blockdev::write_sectors(id, lba, &data).unwrap();
    blockdev::flush(id).unwrap();
    blockdev::read_sectors(id, lba, &mut bytes).unwrap();
    assert_eq!(bytes, data);
    assert!(COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire) >= before + 3);
    if driver_start::state(other) == Ok(FrameworkState::Started) {
        blockdev::read_sectors(peer, lba, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 512], "writes crossed controller/media ownership");
    }
    let _ = writeln!(console, "virtio-blk-recovery: exchange={sequence} handle={dev} block={id} tag={tag} lba={lba} write/flush/read selected IRQ progressed");
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
        .find(|(_, name)| *name == "virtio-blk")
        .unwrap()
        .0;
    assert_ne!(dev, peer);
    let device_id =
        |owner: usize| (ConfigAccess.read_u32(STATE[owner].lock().bdf.unwrap(), 0) >> 16) as u16;
    let target_id = device_id(dev);
    let peer_id = device_id(peer);
    assert!([target_id, peer_id].contains(&virtio::DEVICE_BLOCK_MODERN));
    assert!([target_id, peer_id].contains(&virtio::DEVICE_BLOCK_TRANSITIONAL));
    let transport = |id| {
        if id == virtio::DEVICE_BLOCK_MODERN {
            "modern"
        } else {
            "transitional"
        }
    };
    let _ = writeln!(
        console,
        "virtio-blk-recovery: transport target={} peer={}",
        transport(target_id),
        transport(peer_id)
    );
    let mappings = mmio::live_mappings();
    let mut region = STATE[dev].lock().common.unwrap();
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    probe(console);
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 0, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    let alias = 0x60 + dev as u8;
    assert_ne!(alias, completion_vector(dev));
    let before = COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire);
    on_completion_interrupt(alias);
    assert_eq!(COMPLETION_INTERRUPTS[dev].load(Ordering::Acquire), before);
    exchange(dev, peer, 1, 0x31, console);
    exchange(peer, dev, 2, 0x32, console);
    let _ = writeln!(console, "virtio-blk-recovery: post-filesystem startup quarantine recovered; DMA 4 -> 2 -> 4, independent MSI-X live");

    region = STATE[dev].lock().common.unwrap();
    REFUSALS.store(2, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 3, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 4, 0x31, console);
    exchange(peer, dev, 5, 0x32, console);
    let _ = writeln!(console, "virtio-blk-recovery: failed stop/repeated refusal retained owners; stale MMIO and IRQ retired after recovery");

    driver_start::stop(dev).unwrap();
    MODE.store(1, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    assert_eq!(super::super::dma::live_pages(), 2);
    assert_eq!(mmio::live_mappings() + 4, mappings);
    assert!(removable(dev));
    driver_start::recover(dev).unwrap();
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 6, 0x31, console);
    let _ = writeln!(
        console,
        "virtio-blk-recovery: single-page allocation rollback reclaimed before descriptor exposure"
    );

    driver_start::stop(dev).unwrap();
    MODE.store(2, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = STATE[dev].lock().common.unwrap();
    assert_eq!(region.read16(at(common::QUEUE_ENABLE)).unwrap(), 1);
    assert_eq!(region.read8(at(common::DEVICE_STATUS)).unwrap() & 4, 0);
    pinned(dev, peer, region, mappings, false);
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, false);
    exchange(peer, dev, 7, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 8, 0x31, console);
    let _ = writeln!(console, "virtio-blk-recovery: enabled-queue partial startup retained both pages until checked reset");

    region = STATE[dev].lock().common.unwrap();
    PCI_PROOF.store(true, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, false);
    exchange(peer, dev, 9, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 10, 0x31, console);
    let _ = writeln!(console, "virtio-blk-recovery: withheld PCI exclusion proof retained both pages despite actual bus-master disable");

    driver_start::stop(dev).unwrap();
    MODE.store(5, Ordering::Release);
    assert_eq!(driver_start::start(dev), Err(Error::OpFailed));
    region = STATE[dev].lock().common.unwrap();
    pinned(dev, peer, region, mappings, true);
    assert_eq!(
        REQUESTS_SUBMITTED[dev].load(Ordering::Acquire),
        1,
        "failed request was followed by unsafe I/O"
    );
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 11, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 12, 0x31, console);
    let _ = writeln!(console, "virtio-blk-recovery: published-request failure quarantined without flush/buffer reuse until checked reset");

    region = STATE[dev].lock().common.unwrap();
    MODE.store(4, Ordering::Release);
    assert_eq!(driver_start::stop(dev), Err(Error::OpFailed));
    pinned(dev, peer, region, mappings, true);
    exchange(peer, dev, 13, 0x32, console);
    recovered(dev, region, mappings);
    driver_start::start(dev).unwrap();
    exchange(dev, peer, 14, 0x31, console);
    let _ = writeln!(console, "virtio-blk-recovery: refused flush quarantined without reset/free; explicit recovery avoided unsafe flush retry");
    driver_start::stop(dev).unwrap();
    driver_start::stop(peer).unwrap();
    detached(dev);
    detached(peer);
    assert_eq!(super::super::dma::live_pages(), 0);
    assert_eq!(mmio::live_mappings() + 8, mappings);
    driver_start::remove(dev).unwrap();
    driver_start::remove(peer).unwrap();
    crate::mm::audit().unwrap();
    assert_eq!(driver_start::quiesce(console), Ok(0));
    assert_eq!(driver_start::recover(dev), Err(Error::Busy));
    assert_eq!(
        blockdev::flush(blockdev::device(peer).unwrap()),
        Err(nolxfs::DevError::Io(16))
    );
    let _ = writeln!(console, "virtio-blk-recovery: PASS fifteen durable media exchanges, checked failure recovery, independent MSI-X peer, stale mappings, freeze refusal, final DMA 0");
}
