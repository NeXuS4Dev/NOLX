use super::*;
fn key(pressed: bool) -> Kind {
    Kind::Key(KeyEvent { usage: 4, pressed })
}
fn usb(port: u8) -> Location {
    Location::Usb {
        controller: 3 << 8,
        port,
    }
}
fn attach<const D: usize, const R: usize, const Q: usize>(
    r: &mut Registry<D, R, Q>,
    port: u8,
) -> DeviceId {
    r.attach(Source::UsbHid, Class::Keyboard, usb(port))
        .unwrap()
}
#[test]
fn device_identity_is_not_source_class_or_physical_port() {
    let mut r = Registry::<2, 3, 8>::new();
    let a = attach(&mut r, 1);
    let b = attach(&mut r, 2);
    let ar = r.open(a).unwrap();
    let br = r.open(b).unwrap();
    r.publish(a, key(true)).unwrap();
    assert_eq!(r.read(br), Ok(Read::Empty));
    let Read::Event(record) = r.read(ar).unwrap() else {
        panic!()
    };
    assert_eq!(record.device, a);
    assert_eq!(record.event.source, Source::UsbHid);
    assert_eq!(record.sequence, 1);
    assert!(r.info(a).unwrap().snapshot.held(4));
    assert!(!r.info(b).unwrap().snapshot.held(4));
}
#[test]
fn subscriptions_do_not_steal_each_others_events() {
    let mut r = Registry::<1, 2, 2>::new();
    let d = attach(&mut r, 1);
    let a = r.open(d).unwrap();
    let b = r.open(d).unwrap();
    assert_eq!(r.open(d), Err(Error::Capacity));
    r.publish(d, key(true)).unwrap();
    let first = r.read(a);
    assert_eq!(r.read(a), Ok(Read::Empty));
    assert_eq!(r.read(b), first);
    r.close(a).unwrap();
    let new = r.open(d).unwrap();
    assert_ne!(a, new);
    assert_eq!(r.read(a), Err(Error::Stale));
    assert_eq!(r.close(a), Err(Error::Stale));
    assert_eq!(r.read(new), Ok(Read::Empty));
}
#[test]
fn disconnect_retains_readers_and_neutralizes_only_this_device() {
    let mut r = Registry::<2, 3, 8>::new();
    let a = attach(&mut r, 1);
    let b = attach(&mut r, 2);
    let ar = r.open(a).unwrap();
    let br = r.open(b).unwrap();
    r.publish(a, key(true)).unwrap();
    r.publish(b, key(true)).unwrap();
    r.disconnect(a).unwrap();
    assert_eq!(r.open(a), Err(Error::Disconnected));
    assert_eq!(r.publish(a, key(true)), Err(Error::Disconnected));
    assert!(r.info(b).unwrap().snapshot.held(4));
    for pressed in [true, false] {
        let Read::Event(record) = r.read(ar).unwrap() else {
            panic!()
        };
        assert_eq!(record.device, a);
        assert_eq!(record.event.kind, key(pressed));
    }
    assert_eq!(r.read(ar), Ok(Read::Disconnected));
    assert_eq!(r.read(ar), Ok(Read::Disconnected));
    assert!(matches!(r.read(br), Ok(Read::Event(_))));
    assert_eq!(r.read(br), Ok(Read::Empty));
    r.close(ar).unwrap();
    assert_eq!(r.info(a), Err(Error::Stale));
}
#[test]
fn reconnect_while_old_reader_is_open_never_aliases_the_old_object() {
    let mut r = Registry::<2, 2, 8>::new();
    let old = attach(&mut r, 1);
    let reader = r.open(old).unwrap();
    r.disconnect(old).unwrap();
    let new = attach(&mut r, 1);
    assert_ne!(old, new);
    let nr = r.open(new).unwrap();
    r.publish(new, key(true)).unwrap();
    assert_eq!(r.read(reader), Ok(Read::Disconnected));
    assert!(matches!(r.read(nr), Ok(Read::Event(_))));
    r.close(reader).unwrap();
    assert_eq!(r.publish(old, key(true)), Err(Error::Stale));
    r.disconnect(new).unwrap();
    r.close(nr).unwrap();
    let again = attach(&mut r, 1);
    assert_eq!(again.slot(), old.slot());
    assert_ne!(again.generation(), old.generation());
}
#[test]
fn overflow_is_reader_local_and_repairs_latest_state_before_future_events() {
    let mut r = Registry::<1, 2, 2>::new();
    let d = attach(&mut r, 1);
    let slow = r.open(d).unwrap();
    let fast = r.open(d).unwrap();
    for pressed in [true, false, true, false] {
        r.publish(d, key(pressed)).unwrap();
        assert!(matches!(r.read(fast), Ok(Read::Event(_))));
    }
    assert_eq!(
        r.read(slow),
        Ok(Read::Resync {
            device: d,
            dropped: 4,
            snapshot: Snapshot::EMPTY,
            sequence: 4
        })
    );
    assert_eq!(r.read(slow), Ok(Read::Empty));
    r.publish(d, key(true)).unwrap();
    let result = r.read(fast);
    assert_eq!(r.read(slow), result);
}
#[test]
fn disconnect_after_overflow_returns_neutral_snapshot_then_terminal() {
    let mut r = Registry::<1, 1, 1>::new();
    let d = attach(&mut r, 1);
    let rd = r.open(d).unwrap();
    r.publish(d, key(true)).unwrap();
    r.publish(
        d,
        Kind::Key(KeyEvent {
            usage: 0xe1,
            pressed: true,
        }),
    )
    .unwrap();
    r.disconnect(d).unwrap();
    assert_eq!(
        r.read(rd),
        Ok(Read::Resync {
            device: d,
            dropped: 4,
            snapshot: Snapshot::EMPTY,
            sequence: 4
        })
    );
    assert_eq!(r.read(rd), Ok(Read::Disconnected));
    r.close(rd).unwrap();
    assert!(r.devices().next().is_none());
}
#[test]
fn pointer_class_state_and_neutralization_are_source_owned() {
    let mut r = Registry::<1, 1, 3>::new();
    let d = r
        .attach(Source::Ps2Mouse, Class::Pointer, Location::Ps2Mouse)
        .unwrap();
    let rd = r.open(d).unwrap();
    assert_eq!(r.publish(d, key(true)), Err(Error::Invalid));
    let p = Kind::Pointer(PointerEvent {
        dx: -256,
        dy: 255,
        buttons: 5,
    });
    r.publish(d, p).unwrap();
    assert_eq!(r.info(d).unwrap().snapshot.buttons, 5);
    assert_eq!(
        r.publish(
            d,
            Kind::Pointer(PointerEvent {
                dx: 0,
                dy: 0,
                buttons: 8
            })
        ),
        Err(Error::Invalid)
    );
    r.disconnect(d).unwrap();
    for buttons in [5, 0] {
        let Read::Event(Record {
            event: Event {
                kind: Kind::Pointer(p),
                ..
            },
            ..
        }) = r.read(rd).unwrap()
        else {
            panic!()
        };
        assert_eq!(p.buttons, buttons);
    }
    assert_eq!(r.read(rd), Ok(Read::Disconnected));
}
#[test]
fn duplicates_invalid_profiles_and_capacity_fail_without_partial_objects() {
    let mut r = Registry::<1, 1, 1>::new();
    for (source, class, location) in [
        (Source::Ps2Mouse, Class::Keyboard, Location::Ps2Mouse),
        (Source::UsbHid, Class::Keyboard, Location::Ps2Keyboard),
        (Source::UsbHid, Class::Keyboard, usb(0)),
        (
            Source::UsbHid,
            Class::Keyboard,
            Location::Usb {
                controller: 32 << 8,
                port: 1,
            },
        ),
        (
            Source::UsbHid,
            Class::Keyboard,
            Location::Usb {
                controller: 8,
                port: 1,
            },
        ),
    ] {
        assert_eq!(r.attach(source, class, location), Err(Error::Invalid));
        assert_eq!(r.devices().count(), 0);
    }
    let d = attach(&mut r, 1);
    assert_eq!(
        r.attach(Source::UsbHid, Class::Pointer, usb(1)),
        Err(Error::Duplicate)
    );
    assert_eq!(
        r.attach(Source::UsbHid, Class::Keyboard, usb(2)),
        Err(Error::Capacity)
    );
    r.disconnect(d).unwrap();
    assert!(r.devices().next().is_none());
}
#[test]
fn zero_capacities_are_safe_and_zero_queue_reports_resync() {
    assert_eq!(
        Registry::<0, 0, 0>::new().attach(
            Source::Ps2Keyboard,
            Class::Keyboard,
            Location::Ps2Keyboard
        ),
        Err(Error::Capacity)
    );
    let mut r = Registry::<1, 1, 0>::new();
    let d = attach(&mut r, 1);
    let rd = r.open(d).unwrap();
    r.publish(d, key(true)).unwrap();
    assert!(matches!(r.read(rd), Ok(Read::Resync { dropped: 1, .. })));
    r.disconnect(d).unwrap();
    assert!(matches!(
        r.read(rd),
        Ok(Read::Resync {
            snapshot: Snapshot {
                keys: [0, 0, 0, 0],
                buttons: 0
            },
            ..
        })
    ));
    assert_eq!(r.read(rd), Ok(Read::Disconnected));
}
#[test]
fn generation_exhaustion_retires_slots_without_wrapping() {
    let mut r = Registry::<1, 1, 1>::new();
    r.devices[0].generation = u64::MAX;
    assert_eq!(
        r.attach(Source::UsbHid, Class::Keyboard, usb(1)),
        Err(Error::Capacity)
    );
    r.devices[0].generation = u64::MAX - 1;
    let d = attach(&mut r, 1);
    assert_eq!(d.generation(), u64::MAX);
    r.readers[0].generation = u64::MAX;
    assert_eq!(r.open(d), Err(Error::Capacity));
    r.disconnect(d).unwrap();
    assert_eq!(
        r.attach(Source::UsbHid, Class::Keyboard, usb(1)),
        Err(Error::Capacity)
    );
}

#[test]
fn subscription_initial_state_is_captured_before_its_first_future_record() {
    let mut r = Registry::<1, 1, 4>::new();
    let d = attach(&mut r, 1);
    r.publish(d, key(true)).unwrap();
    let (reader, initial) = r.subscribe(d).unwrap();
    assert_eq!(initial.sequence, 1);
    assert!(initial.snapshot.held(4));
    assert_eq!(r.read(reader), Ok(Read::Empty));
    r.publish(d, key(false)).unwrap();
    let Read::Event(record) = r.read(reader).unwrap() else {
        panic!()
    };
    assert_eq!(record.sequence, initial.sequence + 1);
    assert_eq!(record.event.kind, key(false));
}
#[test]
fn repeated_queue_and_object_slot_wrap_never_replays_old_records() {
    let mut r = Registry::<1, 1, 4>::new();
    let mut previous = None;
    for generation in 1..=1024 {
        let d = attach(&mut r, 1);
        assert_eq!(d.generation(), generation);
        let rd = r.open(d).unwrap();
        if let Some((old, old_reader)) = previous {
            assert_eq!(r.publish(old, key(true)), Err(Error::Stale));
            assert_eq!(r.read(old_reader), Err(Error::Stale));
        }
        for seq in 1..=17 {
            r.publish(d, key(seq % 2 == 1)).unwrap();
            let Read::Event(record) = r.read(rd).unwrap() else {
                panic!()
            };
            assert_eq!(record.sequence, seq);
            assert_eq!(record.device, d);
        }
        r.disconnect(d).unwrap();
        assert!(matches!(
            r.read(rd),
            Ok(Read::Event(Record {
                event: Event {
                    kind: Kind::Key(KeyEvent { pressed: false, .. }),
                    ..
                },
                ..
            }))
        ));
        assert_eq!(r.read(rd), Ok(Read::Disconnected));
        r.close(rd).unwrap();
        previous = Some((d, rd));
    }
}
