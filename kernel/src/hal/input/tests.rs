//! Independent observers of the real producer paths, not fabricated input injections.
use super::*;
use nolx_input_core::{events::Kind, objects::Snapshot};
struct Subscriber {
    id: DeviceId,
    reader: ReaderId,
    source: Source,
    class: Class,
    sequence: u64,
    count: u64,
    state: Snapshot,
    notified: bool,
}
struct Observers {
    subscribers: [Option<Subscriber>; 16],
    slow: Option<(DeviceId, ReaderId)>,
    old: Option<(DeviceId, ReaderId)>,
    armed: bool,
    overflow: bool,
    reconnected: bool,
    progress: bool,
}
static OBS: IrqMutex<Observers> = IrqMutex::new(Observers {
    subscribers: [const { None }; 16],
    slow: None,
    old: None,
    armed: false,
    overflow: false,
    reconnected: false,
    progress: false,
});
pub(super) fn step(console: &mut dyn HalConsole) {
    let mut test = OBS.lock();
    let mut devices = [None; 16];
    enumerate(&mut devices).unwrap();
    for device in devices.into_iter().flatten().filter(|i| i.connected) {
        if test.subscribers.iter().flatten().any(|s| s.id == device.id) {
            continue;
        }
        let index = test
            .subscribers
            .iter()
            .position(Option::is_none)
            .expect("input observer capacity");
        let (reader, initial) = subscribe(device.id).unwrap();
        if initial.source == Source::UsbHid && initial.class == Class::Keyboard {
            if !test.armed {
                test.slow = Some((device.id, open(device.id).unwrap()));
                test.armed = true;
                let _ = writeln!(
                    console,
                    "input: object hardware observers armed device={}:{}",
                    device.id.slot(),
                    device.id.generation()
                );
            } else if !test.reconnected {
                let (old, old_reader) = test.old.expect("old input reader was retired");
                assert_eq!(old.slot(), device.id.slot());
                assert_ne!(old, device.id);
                assert!(device.id.generation() > old.generation());
                assert_eq!(read(old_reader), Err(Error::Stale));
                assert_eq!(info(old), Err(Error::Stale));
                assert_eq!(open(old), Err(Error::Stale));
                assert_eq!(
                    publish(
                        old,
                        Kind::Key(nolx_input_core::KeyEvent {
                            usage: 4,
                            pressed: true
                        })
                    ),
                    Err(Error::Stale)
                );
                test.reconnected = true;
                let _=writeln!(console,"input: object reconnect generation advanced; stale producer/reader/open refused");
            }
        }
        test.subscribers[index] = Some(Subscriber {
            id: device.id,
            reader,
            source: initial.source,
            class: initial.class,
            sequence: initial.sequence,
            count: 0,
            state: initial.snapshot,
            notified: false,
        });
    }
    for index in 0..test.subscribers.len() {
        let Some(mut subscriber) = test.subscribers[index].take() else {
            continue;
        };
        let mut ended = false;
        for _ in 0..65 {
            match read(subscriber.reader).unwrap() {
                Read::Event(record) => {
                    assert_eq!(record.device, subscriber.id);
                    assert_eq!(record.event.source, subscriber.source);
                    assert_eq!(record.sequence, subscriber.sequence + 1);
                    subscriber.sequence = record.sequence;
                    subscriber.count += 1;
                    match record.event.kind {
                        Kind::Key(k) => {
                            assert_eq!(subscriber.class, Class::Keyboard);
                            let mask = 1u64 << (k.usage % 64);
                            let word = &mut subscriber.state.keys[usize::from(k.usage) / 64];
                            if k.pressed {
                                *word |= mask
                            } else {
                                *word &= !mask
                            }
                        }
                        Kind::Pointer(p) => {
                            assert_eq!(subscriber.class, Class::Pointer);
                            subscriber.state.buttons = p.buttons;
                        }
                    }
                }
                Read::Empty => break,
                Read::Resync { .. } => panic!("fast independent input observer lost events"),
                Read::Disconnected => {
                    assert_eq!(subscriber.state.keys, [0; 4]);
                    assert_eq!(subscriber.state.buttons, 0);
                    assert_eq!(info(subscriber.id).unwrap().snapshot, subscriber.state);
                    assert_eq!(
                        publish(
                            subscriber.id,
                            Kind::Key(nolx_input_core::KeyEvent {
                                usage: 4,
                                pressed: true
                            })
                        ),
                        Err(Error::Disconnected)
                    );
                    close(subscriber.reader).unwrap();
                    assert_eq!(read(subscriber.reader), Err(Error::Stale));
                    if subscriber.source == Source::UsbHid && subscriber.class == Class::Keyboard {
                        test.old = Some((subscriber.id, subscriber.reader));
                    }
                    let _=writeln!(console,"input: object independent reader drained neutral state and EOF device={}:{}",subscriber.id.slot(),subscriber.id.generation());
                    ended = true;
                    break;
                }
            }
        }
        if subscriber.source != Source::UsbHid && subscriber.count > 0 && !subscriber.notified {
            subscriber.notified = true;
            let _ = writeln!(
                console,
                "input: object independent reader verified PS/2 class={:?} device={}:{}",
                subscriber.class,
                subscriber.id.slot(),
                subscriber.id.generation()
            );
        }
        if subscriber.source == Source::UsbHid
            && subscriber.class == Class::Keyboard
            && subscriber.count >= 600
            && !test.progress
        {
            test.progress = true;
            let _ = writeln!(
                console,
                "input: object independent reader verified 600 ordered hardware events"
            );
        }
        if !ended {
            test.subscribers[index] = Some(subscriber);
        }
    }
    if let Some((device, reader)) = test.slow {
        let latest = info(device).unwrap();
        if latest.sequence >= 130 {
            match read(reader).unwrap() {
                Read::Resync {
                    device: id,
                    dropped,
                    snapshot,
                    sequence,
                } => {
                    assert_eq!(id, device);
                    assert!(dropped >= 65);
                    assert_eq!(snapshot, latest.snapshot);
                    assert_eq!(sequence, latest.sequence);
                }
                _ => panic!("slow input observer overflow was not explicit"),
            }
            assert_eq!(read(reader), Ok(Read::Empty));
            close(reader).unwrap();
            let replacement = open(device).unwrap();
            assert_ne!(replacement, reader);
            assert_eq!(read(reader), Err(Error::Stale));
            assert_eq!(close(reader), Err(Error::Stale));
            close(replacement).unwrap();
            test.slow = None;
            test.overflow = true;
            let _=writeln!(console,"input: object independent reader overflow repaired; console unaffected, stale reader refused");
        }
    }
}
