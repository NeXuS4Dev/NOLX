//! Actual stalled HID transfer plus injected cleanup refusal/lost-proof tests. No normal-image hooks.
use super::*;
use crate::hal::{dma, input};
use core::sync::atomic::{AtomicUsize, Ordering};
use input::{Class, Info, Location, Read, ReaderId};
use nolx_df_core::State;
use nolx_input_core::events::Kind;
static REFUSE: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static WITHHOLD: AtomicUsize = AtomicUsize::new(usize::MAX);
pub(super) fn refuse_disable(dev: usize) -> bool {
    REFUSE[dev]
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok()
}
pub(super) fn command_result(
    dev: usize,
    kind: u8,
    result: Result<u8, Failure>,
) -> Result<u8, Failure> {
    if kind == 10
        && result.is_ok()
        && WITHHOLD
            .compare_exchange(dev, usize::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        // Hardware really completed Disable Slot, but this fault deliberately withholds the
        // observation from the ownership layer. It must pin DMA and poison command reuse.
        return Err(Failure::Timeout("injected lost Disable Slot proof"));
    }
    result
}
struct Test {
    dev: usize,
    peer: usize,
    port: u8,
    keyboard: Info,
    reader: ReaderId,
    mouse: Info,
    mouse_reader: ReaderId,
    phase: u8,
    mappings: usize,
    region: Region,
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
fn pressed(reader: ReaderId, usage: u8) -> bool {
    let mut found = false;
    loop {
        match input::read(reader).unwrap() {
            Read::Event(r) => {
                if let Kind::Key(k) = r.event.kind {
                    found |= k.pressed && k.usage == usage;
                }
            }
            Read::Empty => return found,
            other => panic!("active reader: {other:?}"),
        }
    }
}
fn mouse(t: &Test, dx: i16) -> bool {
    let mut found = false;
    loop {
        match input::read(t.mouse_reader).unwrap() {
            Read::Event(r) => {
                assert_eq!(r.device, t.mouse.id);
                if let Kind::Pointer(p) = r.event.kind {
                    found |= p.dx == dx;
                }
            }
            Read::Empty => return found,
            other => panic!("peer reader: {other:?}"),
        }
    }
}
fn terminal(t: &Test, usage: u8) {
    let mut neutral = false;
    loop {
        match input::read(t.reader).unwrap() {
            Read::Event(r) => {
                assert_eq!(r.device, t.keyboard.id);
                let Kind::Key(k) = r.event.kind else {
                    panic!("not key")
                };
                assert!(!k.pressed);
                assert_eq!(k.usage, usage);
                neutral = true;
            }
            Read::Disconnected => break,
            other => panic!("retired reader: {other:?}"),
        }
    }
    assert!(neutral);
    assert_eq!(input::open(t.keyboard.id), Err(input::Error::Disconnected));
    input::close(t.reader).unwrap();
    assert_eq!(input::read(t.reader), Err(input::Error::Stale));
}
fn baselines(t: &Test, pages: usize) {
    assert_eq!(dma::live_pages(), pages);
    assert_eq!(mmio::live_mappings(), t.mappings);
    assert!(t.region.read8(0).is_ok());
    assert_eq!(driver_start::state(t.dev), Ok(State::Started));
    assert_eq!(driver_start::state(t.peer), Ok(State::Started));
    assert_eq!(input::info(t.mouse.id).unwrap().id, t.mouse.id);
    crate::mm::audit().unwrap();
}
fn fresh(t: &mut Test) {
    let old = t.keyboard.id;
    t.keyboard = usb(Class::Keyboard);
    assert_eq!(owner(t.keyboard), t.dev);
    let Location::Usb { port, .. } = t.keyboard.location else {
        unreachable!()
    };
    t.port = port;
    assert_ne!(t.keyboard.id, old);
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
    t.reader = input::open(t.keyboard.id).unwrap();
}
pub(super) fn step(console: &mut dyn HalConsole) {
    let mut guard = TEST.lock();
    if guard.is_none() {
        let keyboard = usb(Class::Keyboard);
        let pointer = usb(Class::Pointer);
        let dev = owner(keyboard);
        let peer = owner(pointer);
        assert_ne!(dev, peer);
        let Location::Usb { port, .. } = keyboard.location else {
            unreachable!()
        };
        *guard = Some(Test {
            dev,
            peer,
            port,
            keyboard,
            reader: input::open(keyboard.id).unwrap(),
            mouse: pointer,
            mouse_reader: input::open(pointer.id).unwrap(),
            phase: 0,
            mappings: mmio::live_mappings(),
            region: BINDINGS[dev].lock().region.unwrap(),
        });
        let _ = writeln!(console, "xhci-transfer: armed held keyboard; require a");
    }
    let t = guard.as_mut().unwrap();
    match t.phase {
        0 if pressed(t.reader, 4) => {
            assert!(input::info(t.keyboard.id).unwrap().snapshot.held(4));
            while matches!(input::read(t.mouse_reader).unwrap(), Read::Event(_)) {}
            REFUSE[t.dev].store(2, Ordering::Release);
            driver_start::device_operation_handle(
                t.dev,
                "xhci",
                || {
                    live::stall_test(t.dev, t.port)?;
                    irq::finish(t.dev, false)
                },
                |_| Failure::Busy,
            )
            .unwrap();
            t.phase = 1;
        }
        1 if !input::info(t.keyboard.id).unwrap().connected => {
            live::quarantined_test(t.dev, t.port, true, true);
            baselines(t, 17);
            terminal(t, 4);
            let Location::Usb {
                port: peer_port, ..
            } = t.mouse.location
            else {
                unreachable!()
            };
            assert!(matches!(
                repair_port_on(t.peer, peer_port, console),
                Err(Failure::Busy)
            ));
            assert!(matches!(
                repair_port_on(t.dev, 0, console),
                Err(Failure::Unsupported)
            ));
            assert!(matches!(
                repair_port_on(t.dev, t.port, console),
                Err(Failure::Completion(5))
            ));
            live::quarantined_test(t.dev, t.port, true, true);
            baselines(t, 17);
            assert!(matches!(
                repair_port_on(driver_start::DEVICES, t.port, console),
                Err(Failure::NoController)
            ));
            t.phase = 2;
            let _=writeln!(console,"xhci-transfer: actual STALL code=6 and Halted endpoint, neutral EOF; repeated cleanup refusal retains 17 pages; require peer dx=7");
        }
        2 if mouse(t, 7) => {
            let before = irq::count(t.dev);
            repair_port_on(t.dev, t.port, console).unwrap();
            baselines(t, 16);
            fresh(t);
            assert!(irq::count(t.dev) >= before);
            assert!(matches!(
                repair_port_on(t.dev, t.port, console),
                Err(Failure::Busy)
            ));
            t.phase = 3;
            let _=writeln!(console,"xhci-transfer: slot repair completed, DMA/MMIO baselines and peer identity preserved; require c");
        }
        3 if pressed(t.reader, 6) => {
            assert!(input::info(t.keyboard.id).unwrap().snapshot.held(6));
            while matches!(input::read(t.mouse_reader).unwrap(), Read::Event(_)) {}
            WITHHOLD.store(t.dev, Ordering::Release);
            t.phase = 4;
            let _ = writeln!(
                console,
                "xhci-transfer: armed lost disconnect cleanup proof; remove keyboard"
            );
        }
        4 if !input::info(t.keyboard.id).unwrap().connected => {
            live::quarantined_test(t.dev, t.port, false, false);
            baselines(t, 16);
            terminal(t, 6);
            for _ in 0..2 {
                assert!(matches!(
                    repair_port_on(t.dev, t.port, console),
                    Err(Failure::CommandUnavailable)
                ));
            }
            baselines(t, 16);
            t.phase = 5;
            let _=writeln!(console,"xhci-transfer: lost Disable Slot proof pins 16 pages; command reuse/repair refused, neutral EOF; require peer dx=8");
        }
        5 if mouse(t, 8) => {
            driver_start::stop(t.dev).unwrap();
            irq::detached_test(t.dev);
            assert_eq!(dma::live_pages(), 8);
            assert!(t.region.read8(0).is_err());
            driver_start::start(t.dev).unwrap();
            BINDINGS[t.dev].lock().log.replay(console);
            t.region = BINDINGS[t.dev].lock().region.unwrap();
            baselines(t, 11);
            t.phase = 6;
            let _=writeln!(console,"xhci-transfer: controller reset cleared poisoned ring, DMA 16 -> 8 -> 11; reconnect keyboard");
        }
        6 => {
            let mut infos = [None; 16];
            input::enumerate(&mut infos).unwrap();
            if infos.into_iter().flatten().any(|i| {
                i.connected
                    && i.class == Class::Keyboard
                    && matches!(i.location, Location::Usb { .. })
            }) {
                fresh(t);
                baselines(t, 16);
                t.phase = 7;
                let _ = writeln!(
                    console,
                    "xhci-transfer: reconnected fresh generation; require d"
                );
            }
        }
        7 if pressed(t.reader, 7) => {
            while matches!(input::read(t.mouse_reader).unwrap(), Read::Event(_)) {}
            driver_start::device_operation_handle(
                t.dev,
                "xhci",
                || {
                    live::stall_test(t.dev, t.port)?;
                    irq::finish(t.dev, false)
                },
                |_| Failure::Busy,
            )
            .unwrap();
            t.phase = 8;
        }
        8 if !input::info(t.keyboard.id).unwrap().connected => {
            live::retired_test(t.dev, t.port);
            baselines(t, 11);
            terminal(t, 7);
            t.phase = 9;
            let _=writeln!(console,"xhci-transfer: automatic successful slot cleanup returned to 11 pages; require peer dx=9");
        }
        9 if mouse(t, 9) => {
            repair_port_on(t.dev, t.port, console).unwrap();
            fresh(t);
            baselines(t, 16);
            t.phase = 10;
            let _=writeln!(console,"xhci-transfer: faulted empty port explicitly repaired without controller reset; require e");
        }
        10 if pressed(t.reader, 8) => {
            driver_start::stop(t.dev).unwrap();
            irq::detached_test(t.dev);
            driver_start::stop(t.peer).unwrap();
            irq::detached_test(t.peer);
            assert_eq!(dma::live_pages(), 0);
            assert_eq!(mmio::live_mappings() + 2, t.mappings);
            terminal(t, 8);
            while matches!(input::read(t.mouse_reader).unwrap(), Read::Event(_)) {}
            assert_eq!(input::read(t.mouse_reader), Ok(Read::Disconnected));
            input::close(t.mouse_reader).unwrap();
            driver_start::remove(t.dev).unwrap();
            driver_start::remove(t.peer).unwrap();
            crate::mm::audit().unwrap();
            t.phase = 11;
            let _=writeln!(console,"xhci-transfer: PASS actual stalled transfer, slot repair, lost command proof, live peer, fresh input, stale identities and final DMA 0");
        }
        _ => {}
    }
}
