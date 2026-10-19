//! Two real PCI controllers: keyboard on one, pointer on its independently serviced peer.
use super::*;
use crate::hal::{dma, input};
use input::{Class, Info, Location, Read, ReaderId};
use nolx_df_core::{Error, State};
use nolx_input_core::events::Kind;

#[cfg(not(feature = "hal-xhci-start-fault-test"))]
struct Test {
    keyboard: Info,
    reader: ReaderId,
    mouse: Info,
    mouse_reader: ReaderId,
    handle: usize,
    peer: usize,
    cycle: u8,
    stopped: bool,
    old_region: Region,
    pages: usize,
    mappings: usize,
    done: bool,
}
#[cfg(not(feature = "hal-xhci-start-fault-test"))]
static TEST: IrqMutex<Option<Test>> = IrqMutex::new(None);
fn devices(class: Class) -> Info {
    let mut infos = [None; 16];
    input::enumerate(&mut infos).unwrap();
    infos
        .into_iter()
        .flatten()
        .find(|i| i.connected && i.class == class && matches!(i.location, Location::Usb { .. }))
        .expect("missing USB test producer")
}
fn handle(info: Info) -> usize {
    let Location::Usb { controller, .. } = info.location else {
        panic!("not USB");
    };
    BINDINGS
        .iter()
        .position(|b| {
            b.lock()
                .bdf
                .map(|b| u32::from(b.bus) << 16 | u32::from(b.device) << 8 | u32::from(b.function))
                == Some(controller)
        })
        .unwrap()
}
fn refused(dev: usize, console: &mut dyn HalConsole) {
    assert!(matches!(poll_on(dev, console), Err(Failure::NoController)));
}
#[cfg(not(feature = "hal-xhci-start-fault-test"))]
fn terminal(id: input::DeviceId, reader: ReaderId, usage: u8) {
    let mut neutral = false;
    loop {
        match input::read(reader).unwrap() {
            Read::Event(record) => {
                assert_eq!(record.device, id);
                if let Kind::Key(key) = record.event.kind {
                    assert!(!key.pressed);
                    neutral |= key.usage == usage;
                }
            }
            Read::Disconnected => break,
            other => panic!("unexpected terminal reader result: {other:?}"),
        }
    }
    assert!(neutral, "held key did not receive neutral release");
    let old = input::info(id).unwrap();
    assert!(!old.connected && old.snapshot.keys == [0; 4] && old.snapshot.buttons == 0);
    assert_eq!(input::open(id), Err(input::Error::Disconnected));
    assert_eq!(
        input::publish(
            id,
            Kind::Key(nolx_input_core::KeyEvent {
                usage,
                pressed: true
            })
        ),
        Err(input::Error::Disconnected)
    );
    input::close(reader).unwrap();
    assert_eq!(input::read(reader), Err(input::Error::Stale));
}

pub(super) fn step(console: &mut dyn HalConsole) {
    #[cfg(feature = "hal-xhci-start-fault-test")]
    {
        start_fault_step(console);
    }
    #[cfg(not(feature = "hal-xhci-start-fault-test"))]
    run_positive(console);
}
#[cfg(not(feature = "hal-xhci-start-fault-test"))]
fn run_positive(console: &mut dyn HalConsole) {
    let saved = { TEST.lock().take() };
    let mut test = saved.unwrap_or_else(|| initialize(console));
    advance(&mut test, console);
    *TEST.lock() = Some(test);
}
#[cfg(not(feature = "hal-xhci-start-fault-test"))]
fn initialize(console: &mut dyn HalConsole) -> Test {
    let keyboard = devices(Class::Keyboard);
    let mouse = devices(Class::Pointer);
    let handle = handle(keyboard);
    let peer = self::handle(mouse);
    assert_ne!(handle, peer, "fixture must use two physical controllers");
    let running = driver_start::started().unwrap();
    let pages = dma::live_pages();
    let mappings = mmio::live_mappings();
    probe(console);
    assert_eq!(driver_start::started().unwrap(), running);
    assert_eq!(dma::live_pages(), pages);
    assert_eq!(mmio::live_mappings(), mappings);
    assert_eq!(driver_start::remove(handle), Err(Error::Busy));
    assert_eq!(driver_start::remove(peer), Err(Error::Busy));
    refused(driver_start::DEVICES, console);
    let test = Test {
        keyboard,
        reader: input::open(keyboard.id).unwrap(),
        mouse,
        mouse_reader: input::open(mouse.id).unwrap(),
        handle,
        peer,
        cycle: 0,
        stopped: false,
        old_region: BINDINGS[handle].lock().region.unwrap(),
        pages,
        mappings,
        done: false,
    };
    let _ = writeln!(
        console,
        "xhci-test: armed cycle=0 keyboard={handle} peer={peer}, DMA={pages}"
    );
    test
}
#[cfg(not(feature = "hal-xhci-start-fault-test"))]
fn advance(test: &mut Test, console: &mut dyn HalConsole) {
    if test.done {
        return;
    }
    let usage = 4 + test.cycle;
    if !test.stopped {
        let mut pressed = false;
        loop {
            match input::read(test.reader).unwrap() {
                Read::Event(record) => {
                    assert_eq!(record.device, test.keyboard.id);
                    if let Kind::Key(key) = record.event.kind {
                        pressed |= key.pressed && key.usage == usage;
                    }
                }
                Read::Empty => break,
                other => panic!("unexpected active reader result: {other:?}"),
            }
        }
        if !pressed {
            return;
        }
        assert!(input::info(test.keyboard.id).unwrap().snapshot.held(usage));
        // Drain older pointer reports so only genuinely new peer input can resume this cycle.
        while matches!(input::read(test.mouse_reader).unwrap(), Read::Event(_)) {}
        test.old_region = BINDINGS[test.handle].lock().region.unwrap();
        driver_start::stop(test.handle).unwrap();
        irq::detached_test(test.handle);
        assert_eq!(driver_start::state(test.handle), Ok(State::Stopped));
        assert_eq!(driver_start::state(test.peer), Ok(State::Started));
        crate::mm::audit().unwrap();
        assert_eq!(dma::live_pages() + 8, test.pages);
        assert_eq!(mmio::live_mappings() + 1, test.mappings);
        let bdf = BINDINGS[test.handle].lock().bdf.unwrap();
        assert_eq!(
            ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER,
            0
        );
        assert!(
            test.old_region.read8(0).is_err(),
            "stale MMIO handle survived stop"
        );
        refused(test.handle, console);
        assert_eq!(driver_start::stop(test.handle), Err(Error::BadState));
        terminal(test.keyboard.id, test.reader, usage);
        test.stopped = true;
        let _ = writeln!(console, "xhci-test: stopped cycle={} neutral EOF/stale reader/producer/MMIO verified; peer input required", test.cycle);
        return;
    }
    let mut peer_input = false;
    loop {
        match input::read(test.mouse_reader).unwrap() {
            Read::Event(record) => {
                assert_eq!(record.device, test.mouse.id);
                if let Kind::Pointer(pointer) = record.event.kind {
                    peer_input |= pointer.dx == 7 + i16::from(test.cycle);
                }
            }
            Read::Empty => break,
            other => panic!("peer lost across controller stop: {other:?}"),
        }
    }
    if !peer_input {
        return;
    }
    assert_eq!(input::info(test.mouse.id).unwrap().id, test.mouse.id);
    if test.cycle < 3 {
        let old = test.keyboard.id;
        driver_start::start(test.handle).unwrap();
        BINDINGS[test.handle].lock().log.replay(console);
        assert_eq!(driver_start::state(test.handle), Ok(State::Started));
        crate::mm::audit().unwrap();
        assert_eq!(dma::live_pages(), test.pages);
        assert_eq!(mmio::live_mappings(), test.mappings);
        assert!(
            test.old_region.read8(0).is_err(),
            "stale MMIO generation revived on restart"
        );
        test.keyboard = devices(Class::Keyboard);
        assert_eq!(handle(test.keyboard), test.handle);
        assert_ne!(test.keyboard.id, old);
        assert_eq!(input::info(old), Err(input::Error::Stale));
        assert_eq!(
            input::publish(
                old,
                Kind::Key(nolx_input_core::KeyEvent {
                    usage,
                    pressed: true
                })
            ),
            Err(input::Error::Stale)
        );
        test.reader = input::open(test.keyboard.id).unwrap();
        test.cycle += 1;
        test.stopped = false;
        let _ = writeln!(console, "xhci-test: armed cycle={} restart DMA/MMIO baselines restored; peer identity preserved", test.cycle);
    } else {
        driver_start::remove(test.handle).unwrap();
        assert_eq!(driver_start::state(test.handle), Ok(State::Removed));
        assert_eq!(driver_start::start(test.handle), Err(Error::BadState));
        refused(test.handle, console);
        #[cfg(feature = "hal-xhci-stop-fault-test")]
        {
            STOP_FAULT.store(test.peer, core::sync::atomic::Ordering::Release);
            let pages = dma::live_pages();
            let mappings = mmio::live_mappings();
            assert_eq!(driver_start::stop(test.peer), Err(Error::OpFailed));
            assert_eq!(driver_start::state(test.peer), Ok(State::Failed));
            irq::detached_test(test.peer);
            assert_eq!(dma::live_pages(), pages);
            assert_eq!(mmio::live_mappings(), mappings);
            assert!(live::active(test.peer));
            assert!(BINDINGS[test.peer].lock().region.unwrap().read8(0).is_ok());
            let bdf = BINDINGS[test.peer].lock().bdf.unwrap();
            assert_ne!(
                ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER,
                0
            );
            assert_eq!(driver_start::remove(test.peer), Err(Error::Busy));
            assert_eq!(driver_start::start(test.peer), Err(Error::BadState));
            refused(test.peer, console);
            let info = input::info(test.mouse.id).unwrap();
            assert!(!info.connected && info.snapshot.buttons == 0);
            while matches!(input::read(test.mouse_reader).unwrap(), Read::Event(_)) {}
            assert_eq!(input::read(test.mouse_reader), Ok(Read::Disconnected));
            input::close(test.mouse_reader).unwrap();
            test.done = true;
            let _ = writeln!(console, "xhci-test: PASS quarantine: failed quiescence retains {pages} DMA pages and MMIO, bus master active; neutral EOF, polling/start/removal refused");
        }
        #[cfg(not(feature = "hal-xhci-stop-fault-test"))]
        finish_peer(test, console);
    }
}
#[cfg(not(any(
    feature = "hal-xhci-start-fault-test",
    feature = "hal-xhci-stop-fault-test"
)))]
fn finish_peer(test: &mut Test, console: &mut dyn HalConsole) {
    driver_start::stop(test.peer).unwrap();
    irq::detached_test(test.peer);
    // Pointer reader must receive neutral button state then EOF too.
    loop {
        match input::read(test.mouse_reader).unwrap() {
            Read::Event(record) => {
                if let Kind::Pointer(p) = record.event.kind {
                    assert_eq!(p.buttons, 0);
                }
            }
            Read::Disconnected => break,
            other => panic!("unexpected pointer retirement: {other:?}"),
        }
    }
    input::close(test.mouse_reader).unwrap();
    driver_start::remove(test.peer).unwrap();
    assert_eq!(driver_start::state(test.peer), Ok(State::Removed));
    assert_eq!(driver_start::start(test.peer), Err(Error::BadState));
    refused(test.peer, console);
    crate::mm::audit().unwrap();
    assert_eq!(dma::live_pages() + 16, test.pages);
    assert_eq!(mmio::live_mappings() + 2, test.mappings);
    probe(console);
    crate::mm::audit().unwrap();
    assert_eq!(dma::live_pages() + 16, test.pages);
    assert_eq!(mmio::live_mappings() + 2, test.mappings);
    test.done = true;
    let _ = writeln!(console, "xhci-test: PASS two controllers, three restarts with fresh keyboard input, live peer isolation, neutral EOF, stale identities/MMIO, final removal; DMA {} -> {}", test.pages, dma::live_pages());
}

#[cfg(feature = "hal-xhci-stop-fault-test")]
static STOP_FAULT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
#[cfg(feature = "hal-xhci-stop-fault-test")]
pub(super) fn inject_stop(dev: usize) -> bool {
    STOP_FAULT
        .compare_exchange(
            dev,
            usize::MAX,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        )
        .is_ok()
}
#[cfg(feature = "hal-xhci-start-fault-test")]
fn start_fault_step(console: &mut dyn HalConsole) {
    static READER: IrqMutex<Option<(usize, usize, ReaderId, usize)>> = IrqMutex::new(None);
    static DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if DONE.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let current = *READER.lock();
    let Some((failed, peer, reader, mappings)) = current else {
        let failed = (0..driver_start::DEVICES)
            .find(|&d| {
                BINDINGS[d].lock().bdf.is_some() && driver_start::state(d) == Ok(State::Failed)
            })
            .unwrap();
        irq::detached_test(failed);
        assert!(removable(failed));
        assert!(!live::active(failed));
        let bdf = BINDINGS[failed].lock().bdf.unwrap();
        assert_eq!(
            ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER,
            0
        );
        refused(failed, console);
        let mouse = devices(Class::Pointer);
        let peer = handle(mouse);
        assert_ne!(failed, peer);
        assert_eq!(
            dma::live_pages(),
            8,
            "failed startup leaked its three exposed host pages"
        );
        *READER.lock() = Some((
            failed,
            peer,
            input::open(mouse.id).unwrap(),
            mmio::live_mappings(),
        ));
        let _ = writeln!(console, "xhci-test: start fault armed peer={peer}; post-run rollback DMA/MMIO baselines restored");
        return;
    };
    let mut received = false;
    while let Read::Event(record) = input::read(reader).unwrap() {
        if let Kind::Pointer(p) = record.event.kind {
            received |= p.dx == 7;
        }
    }
    if !received {
        return;
    }
    driver_start::remove(failed).unwrap();
    assert_eq!(driver_start::state(failed), Ok(State::Removed));
    assert_eq!(driver_start::start(failed), Err(Error::BadState));
    driver_start::stop(peer).unwrap();
    driver_start::remove(peer).unwrap();
    while matches!(input::read(reader).unwrap(), Read::Event(_)) {}
    assert_eq!(input::read(reader), Ok(Read::Disconnected));
    input::close(reader).unwrap();
    crate::mm::audit().unwrap();
    assert_eq!(dma::live_pages(), 0);
    assert_eq!(mmio::live_mappings() + 1, mappings);
    DONE.store(true, core::sync::atomic::Ordering::Release);
    let _ = writeln!(console, "xhci-test: PASS post-run start failure: exposed DMA reclaimed only after halt/reset/bus-master readback, live peer input, failed record safely removed, DMA 0");
}
