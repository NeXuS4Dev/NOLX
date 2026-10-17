//! Real two-controller failed-start/failed-stop recovery. Fault hooks are absent in normal images.
use super::*;
use crate::hal::{dma, input};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use input::{Class, Info, Location, Read, ReaderId};
use nolx_df_core::{Error, State};
use nolx_input_core::events::Kind;
static FIRST: AtomicBool = AtomicBool::new(true);
static REFUSALS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
pub(super) fn fail_start(dev: usize) -> bool {
    let inject = FIRST.swap(false, Ordering::AcqRel);
    if inject {
        REFUSALS[dev].store(2, Ordering::Release);
    }
    inject
}
pub(super) fn refuse_stop(dev: usize) -> bool {
    REFUSALS[dev]
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok()
}
struct Test {
    dev: usize,
    peer: usize,
    mouse: Info,
    mouse_reader: ReaderId,
    keyboard: Option<(Info, ReaderId)>,
    old: Option<input::DeviceId>,
    region: Region,
    mappings: usize,
    phase: u8,
}
static TEST: IrqMutex<Option<Test>> = IrqMutex::new(None);
fn usb(class: Class) -> Info {
    let mut infos = [None; 16];
    input::enumerate(&mut infos).unwrap();
    infos
        .into_iter()
        .flatten()
        .find(|i| i.connected && i.class == class && matches!(i.location, Location::Usb { .. }))
        .unwrap()
}
fn owner(info: Info) -> usize {
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
fn quarantined(t: &Test, pages: usize, mappings: usize, console: &mut dyn HalConsole) {
    assert_eq!(driver_start::state(t.dev), Ok(State::Failed));
    irq::detached_test(t.dev);
    assert_eq!(driver_start::state(t.peer), Ok(State::Started));
    assert_eq!(dma::live_pages(), pages);
    assert_eq!(mmio::live_mappings(), mappings);
    assert!(live::active(t.dev));
    assert!(t.region.read8(0).is_ok());
    let bdf = BINDINGS[t.dev].lock().bdf.unwrap();
    assert_ne!(
        ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER,
        0
    );
    assert_eq!(driver_start::start(t.dev), Err(Error::BadState));
    assert_eq!(driver_start::remove(t.dev), Err(Error::Busy));
    assert!(matches!(
        poll_on(t.dev, console),
        Err(Failure::NoController)
    ));
}
fn initialize(console: &mut dyn HalConsole) -> Test {
    let dev = (0..driver_start::DEVICES)
        .find(|&d| driver_start::state(d) == Ok(State::Failed))
        .unwrap();
    let mouse = usb(Class::Pointer);
    let peer = owner(mouse);
    let t = Test {
        dev,
        peer,
        mouse,
        mouse_reader: input::open(mouse.id).unwrap(),
        keyboard: None,
        old: None,
        region: BINDINGS[dev].lock().region.unwrap(),
        mappings: mmio::live_mappings(),
        phase: 0,
    };
    assert_ne!(dev, peer);
    quarantined(&t, 11, t.mappings, console);
    assert_eq!(driver_start::recover(peer), Err(Error::BadState));
    assert_eq!(
        driver_start::recover(driver_start::DEVICES),
        Err(Error::UnknownDevice)
    );
    assert_eq!(driver_start::recover(dev), Err(Error::OpFailed));
    quarantined(&t, 11, t.mappings, console);
    let running = driver_start::started().unwrap();
    probe(console);
    assert_eq!(driver_start::started().unwrap(), running);
    quarantined(&t, 11, t.mappings, console);
    let _ = writeln!(console, "xhci-recovery: armed startup quarantine, repeated failed cleanup retains 11 DMA pages/MMIO; peer input required");
    t
}
fn mouse_event(t: &Test, dx: i16) -> bool {
    let mut found = false;
    loop {
        match input::read(t.mouse_reader).unwrap() {
            Read::Event(r) => {
                assert_eq!(r.device, t.mouse.id);
                if let Kind::Pointer(p) = r.event.kind {
                    found |= p.dx == dx && p.buttons == 1;
                }
            }
            Read::Empty => break,
            other => panic!("live peer lost during recovery: {other:?}"),
        }
    }
    found
}
fn pressed(info: Info, reader: ReaderId, usage: u8) -> bool {
    let mut found = false;
    loop {
        match input::read(reader).unwrap() {
            Read::Event(r) => {
                assert_eq!(r.device, info.id);
                if let Kind::Key(k) = r.event.kind {
                    found |= k.usage == usage && k.pressed;
                }
            }
            Read::Empty => break,
            other => panic!("restarted keyboard not live: {other:?}"),
        }
    }
    found
}
fn terminal(info: Info, reader: ReaderId, usage: Option<u8>) {
    let mut neutral = false;
    loop {
        match input::read(reader).unwrap() {
            Read::Event(r) => {
                assert_eq!(r.device, info.id);
                match r.event.kind {
                    Kind::Key(k) => {
                        assert!(!k.pressed);
                        neutral |= Some(k.usage) == usage;
                    }
                    Kind::Pointer(p) => {
                        assert_eq!(p.buttons, 0);
                        neutral = true;
                    }
                }
            }
            Read::Disconnected => break,
            other => panic!("retired input not terminal: {other:?}"),
        }
    }
    assert!(neutral);
    let retired = input::info(info.id).unwrap();
    assert!(!retired.connected && retired.snapshot.keys == [0; 4] && retired.snapshot.buttons == 0);
    assert_eq!(input::open(info.id), Err(input::Error::Disconnected));
    if let Some(usage) = usage {
        assert_eq!(
            input::publish(
                info.id,
                Kind::Key(nolx_input_core::KeyEvent {
                    usage,
                    pressed: true
                })
            ),
            Err(input::Error::Disconnected)
        );
    }
    input::close(reader).unwrap();
    assert_eq!(input::read(reader), Err(input::Error::Stale));
}
fn recover_restart(t: &mut Test, console: &mut dyn HalConsole) {
    driver_start::recover(t.dev).unwrap();
    irq::detached_test(t.dev);
    assert_eq!(driver_start::state(t.dev), Ok(State::Stopped));
    assert_eq!(driver_start::recover(t.dev), Err(Error::BadState));
    assert_eq!(dma::live_pages(), 8);
    assert_eq!(mmio::live_mappings() + 1, t.mappings);
    assert!(!live::active(t.dev));
    assert!(t.region.read8(0).is_err());
    let bdf = BINDINGS[t.dev].lock().bdf.unwrap();
    assert_eq!(
        ConfigAccess.read_u32(bdf, PCI_COMMAND) & COMMAND_BUS_MASTER,
        0
    );
    driver_start::start(t.dev).unwrap();
    BINDINGS[t.dev].lock().log.replay(console);
    assert_eq!(driver_start::state(t.dev), Ok(State::Started));
    assert_eq!(dma::live_pages(), 16);
    assert_eq!(mmio::live_mappings(), t.mappings);
    assert!(t.region.read8(0).is_err(), "old MMIO generation revived");
    let keyboard = usb(Class::Keyboard);
    assert_eq!(owner(keyboard), t.dev);
    if let Some(old) = t.old {
        assert_ne!(old, keyboard.id);
        assert_eq!(input::info(old), Err(input::Error::Stale));
        assert_eq!(input::open(old), Err(input::Error::Stale));
        assert_eq!(
            input::publish(
                old,
                Kind::Key(nolx_input_core::KeyEvent {
                    usage: 4,
                    pressed: true
                })
            ),
            Err(input::Error::Stale)
        );
    }
    t.keyboard = Some((keyboard, input::open(keyboard.id).unwrap()));
    crate::mm::audit().unwrap();
}
pub(super) fn step(console: &mut dyn HalConsole) {
    // Never hold the test IRQ mutex over framework callbacks/polling.
    let saved = { TEST.lock().take() };
    let mut t = saved.unwrap_or_else(|| initialize(console));
    advance(&mut t, console);
    *TEST.lock() = Some(t);
}
fn advance(t: &mut Test, console: &mut dyn HalConsole) {
    match t.phase {
        0 if mouse_event(t, 7) => {
            recover_restart(t, console);
            t.phase = 1;
            let _ = writeln!(console, "xhci-recovery: armed first restarted keyboard, startup owner reclaimed only after quiescence; fresh key required");
        }
        1 => {
            let (keyboard, reader) = t.keyboard.unwrap();
            if !pressed(keyboard, reader, 4) {
                return;
            }
            assert!(input::info(keyboard.id).unwrap().snapshot.held(4));
            t.region = BINDINGS[t.dev].lock().region.unwrap();
            REFUSALS[t.dev].store(2, Ordering::Release);
            assert_eq!(driver_start::stop(t.dev), Err(Error::OpFailed));
            quarantined(t, 16, t.mappings, console);
            assert_eq!(driver_start::recover(t.dev), Err(Error::OpFailed));
            quarantined(t, 16, t.mappings, console);
            terminal(keyboard, reader, Some(4));
            t.old = Some(keyboard.id);
            t.keyboard = None;
            t.phase = 2;
            let _ = writeln!(console, "xhci-recovery: armed stop quarantine, neutral EOF and repeated cleanup failure retain 16 DMA pages/MMIO; peer input required");
        }
        2 if mouse_event(t, 8) => {
            recover_restart(t, console);
            t.phase = 3;
            let _ = writeln!(console, "xhci-recovery: armed second restarted keyboard, old producer/reader/open/MMIO retired; fresh key required");
        }
        3 => {
            let (keyboard, reader) = t.keyboard.unwrap();
            if !pressed(keyboard, reader, 5) {
                return;
            }
            assert!(input::info(keyboard.id).unwrap().snapshot.held(5));
            driver_start::stop(t.dev).unwrap();
            terminal(keyboard, reader, Some(5));
            driver_start::remove(t.dev).unwrap();
            assert_eq!(driver_start::recover(t.dev), Err(Error::BadState));
            driver_start::stop(t.peer).unwrap();
            terminal(t.mouse, t.mouse_reader, None);
            driver_start::remove(t.peer).unwrap();
            assert_eq!(dma::live_pages(), 0);
            assert_eq!(mmio::live_mappings() + 2, t.mappings);
            for dev in [t.dev, t.peer] {
                assert_eq!(driver_start::state(dev), Ok(State::Removed));
            }
            assert_eq!(driver_start::quiesce(console), Ok(0));
            for dev in [t.dev, t.peer] {
                assert_eq!(driver_start::recover(dev), Err(Error::Busy));
            }
            crate::mm::audit().unwrap();
            t.phase = 4;
            let _ = writeln!(console, "xhci-recovery: PASS failed start/stop, repeated pinning, live peer and fresh keys after both recoveries; neutral EOF, stale generations, removal and shutdown freeze; DMA 0");
        }
        _ => {}
    }
}
