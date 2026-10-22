//! Host tests. The driver operations record their calls in statics, one set per test, so tests can
//! run in parallel.

use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};

const NIC: DeviceId = DeviceId {
    vendor: 0x8086,
    device: 0x100E,
    class: 0x02,
    subclass: 0x00,
    prog_if: 0x00,
    segment: 0,
};

const STORAGE: DeviceId = DeviceId {
    vendor: 0x1AF4,
    device: 0x1042,
    class: 0x01,
    subclass: 0x00,
    prog_if: 0x00,
    segment: 0,
};

fn accept(_: &DeviceId) -> bool {
    true
}
fn refuse(_: &DeviceId) -> bool {
    false
}
fn ok(_: usize) -> bool {
    true
}
fn fail(_: usize) -> bool {
    false
}

static DRV_LAST_START: AtomicUsize = AtomicUsize::new(usize::MAX);

fn record_start(dev: usize) -> bool {
    DRV_LAST_START.store(dev, Ordering::SeqCst);
    true
}

const NIC_RULES: &[Match] = &[Match::by_ids(0x8086, 0x100E)];
const NET_CLASS: &[Match] = &[Match::by_class(0x02, 0x00, None)];
const STORAGE_RULES: &[Match] = &[Match::by_ids(0x1AF4, 0x1042)];

fn good_driver(rules: &'static [Match]) -> Driver {
    Driver {
        name: "good",
        matches: rules,
        ops: Dispatch {
            probe: accept,
            start: ok,
            stop: ok,
        },
    }
}

#[test]
fn a_match_rule_compares_only_the_fields_it_sets() {
    assert!(Match::by_ids(0x8086, 0x100E).matches(&NIC));
    assert!(!Match::by_ids(0x8086, 0x1234).matches(&NIC));
    assert!(
        Match::by_class(0x02, 0x00, None).matches(&NIC),
        "any prog-if"
    );
    assert!(!Match::by_class(0x02, 0x00, Some(0x01)).matches(&NIC));
    assert!(!Match::by_class(0x01, 0x00, None).matches(&NIC));
}

#[test]
fn a_device_binds_to_the_first_driver_whose_probe_accepts_it() {
    let mut reg: Registry<4, 4> = Registry::new();
    let refusing = Driver {
        name: "refuses",
        matches: NIC_RULES,
        ops: Dispatch {
            probe: refuse,
            start: ok,
            stop: ok,
        },
    };
    reg.add_driver(refusing).unwrap();
    let accepting = reg.add_driver(good_driver(NET_CLASS)).unwrap();
    let dev = reg.add_device(NIC).unwrap();
    assert_eq!(reg.bind(dev), Ok(accepting), "the class rule accepts it");
    assert_eq!(reg.driver_of(dev), Ok(Some(accepting)));
    assert_eq!(reg.state(dev), Ok(State::Bound));
}

#[test]
fn a_device_with_no_accepting_driver_stays_discovered_and_can_bind_later() {
    let mut reg: Registry<4, 4> = Registry::new();
    let dev = reg.add_device(STORAGE).unwrap();
    assert_eq!(reg.bind(dev), Err(Error::NoDriver));
    assert_eq!(reg.state(dev), Ok(State::Discovered));
    reg.add_driver(good_driver(NET_CLASS)).unwrap();
    assert_eq!(
        reg.bind(dev),
        Err(Error::NoDriver),
        "still not a network device"
    );
    reg.add_driver(good_driver(STORAGE_RULES)).unwrap();
    assert!(reg.bind(dev).is_ok(), "a driver added later binds it");
}

#[test]
fn the_lifecycle_moves_through_start_stop_and_start_again() {
    let mut reg: Registry<2, 2> = Registry::new();
    reg.add_driver(good_driver(NIC_RULES)).unwrap();
    let dev = reg.add_device(NIC).unwrap();
    assert_eq!(reg.start(dev), Err(Error::BadState), "not bound yet");
    reg.bind(dev).unwrap();
    assert_eq!(reg.start(dev), Ok(()));
    assert_eq!(reg.state(dev), Ok(State::Started));
    assert_eq!(
        reg.bind(dev),
        Err(Error::BadState),
        "a bound device is not rebound"
    );
    assert_eq!(reg.stop(dev), Ok(()));
    assert_eq!(reg.state(dev), Ok(State::Stopped));
    assert_eq!(reg.start(dev), Ok(()), "a stopped device starts again");
}

#[test]
fn a_failed_start_requires_checked_recovery_before_restart() {
    let mut reg: Registry<2, 2> = Registry::new();
    let broken = Driver {
        name: "broken",
        matches: NIC_RULES,
        ops: Dispatch {
            probe: accept,
            start: fail,
            stop: ok,
        },
    };
    reg.add_driver(broken).unwrap();
    let dev = reg.add_device(NIC).unwrap();
    reg.bind(dev).unwrap();
    assert_eq!(reg.start(dev), Err(Error::OpFailed));
    assert_eq!(reg.state(dev), Ok(State::Failed));
    assert_eq!(
        reg.start(dev),
        Err(Error::BadState),
        "a failed device does not start"
    );
    assert_eq!(reg.remove(dev), Ok(()), "but it can be removed");
}

#[test]
fn a_started_device_cannot_be_removed_until_it_is_stopped() {
    let mut reg: Registry<2, 2> = Registry::new();
    reg.add_driver(good_driver(NIC_RULES)).unwrap();
    let dev = reg.add_device(NIC).unwrap();
    reg.bind(dev).unwrap();
    reg.start(dev).unwrap();
    assert_eq!(reg.remove(dev), Err(Error::Busy));
    reg.stop(dev).unwrap();
    assert_eq!(reg.remove(dev), Ok(()));
    assert_eq!(reg.state(dev), Ok(State::Removed));
    assert_eq!(reg.remove(dev), Err(Error::BadState), "removal is terminal");
    assert_eq!(reg.start(dev), Err(Error::BadState));
}

#[test]
fn the_registry_refuses_work_beyond_its_capacity_and_unknown_handles() {
    let mut reg: Registry<1, 1> = Registry::new();
    reg.add_driver(good_driver(NIC_RULES)).unwrap();
    assert_eq!(reg.add_driver(good_driver(NIC_RULES)), Err(Error::Full));
    reg.add_device(NIC).unwrap();
    assert_eq!(reg.add_device(STORAGE), Err(Error::Full));
    assert_eq!(reg.state(7), Err(Error::UnknownDevice));
    assert_eq!(reg.bind(7), Err(Error::UnknownDevice));
    assert_eq!(reg.start(7), Err(Error::UnknownDevice));
}

#[test]
fn the_dispatch_table_receives_the_device_handle() {
    let mut reg: Registry<2, 4> = Registry::new();
    let driver = Driver {
        name: "recorder",
        matches: NIC_RULES,
        ops: Dispatch {
            probe: accept,
            start: record_start,
            stop: ok,
        },
    };
    reg.add_driver(driver).unwrap();
    let _first = reg.add_device(STORAGE).unwrap();
    let second = reg.add_device(NIC).unwrap();
    reg.bind(second).unwrap();
    reg.start(second).unwrap();
    assert_eq!(DRV_LAST_START.load(Ordering::SeqCst), second);
}

#[test]
fn driver_identity_survives_stop_restart_and_removal() {
    let mut registry: Registry<2, 3> = Registry::new();
    assert_eq!(registry.find_driver("good"), None);
    let driver = registry.add_driver(good_driver(STORAGE_RULES)).unwrap();
    assert_eq!(registry.find_driver("good"), Some(driver));
    let dev = registry.add_device(STORAGE).unwrap();
    assert_eq!(registry.driver_name(dev), Err(Error::NoDriver));
    registry.bind(dev).unwrap();
    registry.start(dev).unwrap();
    assert_eq!(registry.remove(dev), Err(Error::Busy));
    registry.stop(dev).unwrap();
    assert_eq!(registry.driver_name(dev), Ok("good"));
    registry.start(dev).unwrap();
    registry.stop(dev).unwrap();
    registry.remove(dev).unwrap();
    assert_eq!(registry.driver_name(dev), Ok("good"));
    assert_eq!(registry.driver_name(usize::MAX), Err(Error::UnknownDevice));
    assert_eq!(registry.start(dev), Err(Error::BadState));
}

fn shutdown_registry(stop: fn(usize) -> bool) -> Registry<1, 3> {
    let mut reg = Registry::new();
    reg.add_driver(Driver {
        name: "shutdown",
        matches: NIC_RULES,
        ops: Dispatch {
            probe: accept,
            start: ok,
            stop,
        },
    })
    .unwrap();
    for _ in 0..3 {
        let d = reg.add_device(NIC).unwrap();
        reg.bind(d).unwrap();
        reg.start(d).unwrap();
    }
    reg
}
#[test]
fn shutdown_flushes_every_device_before_reverse_stop_and_freezes_all_admission() {
    use std::cell::RefCell;
    let calls = RefCell::new(std::vec::Vec::new());
    let mut reg = shutdown_registry(ok);
    assert_eq!(
        reg.quiesce(
            |d, n| {
                assert_eq!(n, "shutdown");
                calls.borrow_mut().push((0, d));
                true
            },
            |d| calls.borrow_mut().push((1, d))
        ),
        Ok(3)
    );
    assert_eq!(
        *calls.borrow(),
        [(0, 0), (0, 1), (0, 2), (1, 2), (1, 1), (1, 0)]
    );
    assert!(reg.frozen());
    assert_eq!(reg.start(0), Err(Error::Busy));
    assert_eq!(reg.stop(0), Err(Error::Busy));
    assert_eq!(reg.remove(0), Err(Error::Busy));
    assert_eq!(reg.bind(0), Err(Error::Busy));
    assert_eq!(reg.add_device(NIC), Err(Error::Busy));
    assert_eq!(reg.add_driver(good_driver(NIC_RULES)), Err(Error::Busy));
    assert_eq!(reg.quiesce(|_, _| panic!(), |_| panic!()), Err(Error::Busy));
    for d in 0..3 {
        assert_eq!(reg.state(d), Ok(State::Stopped));
    }
}
#[test]
fn shutdown_barrier_failure_stops_nothing_and_stays_frozen() {
    let mut reg = shutdown_registry(ok);
    assert_eq!(
        reg.quiesce(|d, _| d != 1, |_| panic!()),
        Err(Error::OpFailed)
    );
    assert!(reg.frozen());
    for d in 0..3 {
        assert_eq!(reg.state(d), Ok(State::Started));
        assert_eq!(reg.start(d), Err(Error::Busy));
    }
}
#[test]
fn shutdown_stop_failure_never_releases_failed_or_unvisited_devices() {
    let mut reg = shutdown_registry(|d| d != 1);
    let mut released = std::vec::Vec::new();
    assert_eq!(
        reg.quiesce(|_, _| true, |d| released.push(d)),
        Err(Error::OpFailed)
    );
    assert_eq!(released, [2]);
    assert_eq!(reg.state(2), Ok(State::Stopped));
    assert_eq!(reg.state(1), Ok(State::Failed));
    assert_eq!(reg.state(0), Ok(State::Started));
    assert_eq!(reg.remove(1), Err(Error::Busy));
}
#[test]
fn shutdown_failed_record_refuses_before_any_barrier_or_stop() {
    let mut reg = shutdown_registry(fail);
    assert_eq!(reg.stop(1), Err(Error::OpFailed));
    assert_eq!(
        reg.quiesce(|_, _| panic!(), |_| panic!()),
        Err(Error::BadState)
    );
    assert!(reg.frozen());
}
#[test]
fn empty_shutdown_is_valid_but_not_reusable() {
    let mut reg: Registry<0, 0> = Registry::new();
    assert_eq!(reg.quiesce(|_, _| panic!(), |_| panic!()), Ok(0));
    assert_eq!(reg.quiesce(|_, _| panic!(), |_| panic!()), Err(Error::Busy));
}

#[test]
fn failed_start_recovery_calls_only_original_stop_and_never_starts_or_touches_peer() {
    static CLEANED: AtomicUsize = AtomicUsize::new(usize::MAX);
    let mut reg: Registry<2, 2> = Registry::new();
    reg.add_driver(Driver {
        name: "failed-start",
        matches: NIC_RULES,
        ops: Dispatch {
            probe: accept,
            start: fail,
            stop: |d| {
                CLEANED.store(d, Ordering::SeqCst);
                true
            },
        },
    })
    .unwrap();
    reg.add_driver(Driver {
        name: "peer",
        matches: STORAGE_RULES,
        ops: Dispatch {
            probe: accept,
            start: ok,
            stop: |_| panic!("recovery touched peer"),
        },
    })
    .unwrap();
    let dev = reg.add_device(NIC).unwrap();
    let peer = reg.add_device(STORAGE).unwrap();
    reg.bind(dev).unwrap();
    reg.bind(peer).unwrap();
    reg.start(peer).unwrap();
    assert_eq!(reg.start(dev), Err(Error::OpFailed));
    assert_eq!(reg.recover(dev), Ok(()));
    assert_eq!(CLEANED.load(Ordering::SeqCst), dev);
    assert_eq!(reg.state(dev), Ok(State::Stopped));
    assert_eq!(reg.driver_name(dev), Ok("failed-start"));
    assert_eq!(reg.state(peer), Ok(State::Started));
    // No implicit start: this driver's start still fails on explicit retry.
    assert_eq!(reg.start(dev), Err(Error::OpFailed));
}
#[test]
fn repeated_recovery_failure_preserves_failed_until_checked_stop_succeeds() {
    use core::sync::atomic::AtomicBool;
    static REFUSE: AtomicBool = AtomicBool::new(true);
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut reg: Registry<1, 1> = Registry::new();
    let mut driver = good_driver(NIC_RULES);
    driver.ops.stop = |_| {
        CALLS.fetch_add(1, Ordering::SeqCst);
        !REFUSE.load(Ordering::SeqCst)
    };
    reg.add_driver(driver).unwrap();
    let dev = reg.add_device(NIC).unwrap();
    reg.bind(dev).unwrap();
    reg.start(dev).unwrap();
    assert_eq!(reg.stop(dev), Err(Error::OpFailed));
    for _ in 0..3 {
        assert_eq!(reg.recover(dev), Err(Error::OpFailed));
        assert_eq!(reg.state(dev), Ok(State::Failed));
    }
    REFUSE.store(false, Ordering::SeqCst);
    reg.recover(dev).unwrap();
    assert_eq!(CALLS.load(Ordering::SeqCst), 5);
    assert_eq!(reg.state(dev), Ok(State::Stopped));
    reg.start(dev).unwrap();
    assert_eq!(reg.state(dev), Ok(State::Started));
}
#[test]
fn recovery_requires_failed_exact_handle_and_cannot_thaw_failed_shutdown() {
    let mut reg: Registry<1, 1> = Registry::new();
    assert_eq!(reg.recover(usize::MAX), Err(Error::UnknownDevice));
    let dev = reg.add_device(NIC).unwrap();
    assert_eq!(reg.recover(dev), Err(Error::BadState));
    reg.add_driver(good_driver(NIC_RULES)).unwrap();
    reg.bind(dev).unwrap();
    assert_eq!(reg.recover(dev), Err(Error::BadState));
    reg.start(dev).unwrap();
    assert_eq!(reg.recover(dev), Err(Error::BadState));
    reg.stop(dev).unwrap();
    assert_eq!(reg.recover(dev), Err(Error::BadState));
    reg.remove(dev).unwrap();
    assert_eq!(reg.recover(dev), Err(Error::BadState));
    let mut failed = shutdown_registry(fail);
    assert_eq!(failed.stop(1), Err(Error::OpFailed));
    assert_eq!(
        failed.quiesce(|_, _| panic!(), |_| panic!()),
        Err(Error::BadState)
    );
    assert_eq!(failed.recover(1), Err(Error::Busy));
    assert_eq!(failed.state(1), Ok(State::Failed));
}

#[test]
fn suspend_stops_in_reverse_order_without_closing_admission() {
    use std::cell::RefCell;
    let calls = RefCell::new(std::vec::Vec::new());
    let mut reg = shutdown_registry(ok);
    assert_eq!(
        reg.suspend(
            |d, n| {
                assert_eq!(n, "shutdown");
                calls.borrow_mut().push((0, d));
                true
            },
            |d| calls.borrow_mut().push((1, d))
        ),
        Ok(3)
    );
    assert_eq!(
        *calls.borrow(),
        [(0, 0), (0, 1), (0, 2), (1, 2), (1, 1), (1, 0)]
    );
    // Suspension is reversible: admission is still open and the records can start again.
    assert!(!reg.frozen());
    for d in 0..3 {
        assert_eq!(reg.state(d), Ok(State::Stopped));
    }
    assert_eq!(reg.resume(), Ok((3, 0)));
    for d in 0..3 {
        assert_eq!(reg.state(d), Ok(State::Started));
    }
}
#[test]
fn suspend_barrier_failure_stops_nothing_and_leaves_records_started() {
    let mut reg = shutdown_registry(ok);
    assert_eq!(
        reg.suspend(|d, _| d != 1, |_| panic!()),
        Err(Error::OpFailed)
    );
    assert!(!reg.frozen());
    for d in 0..3 {
        assert_eq!(reg.state(d), Ok(State::Started));
    }
}
#[test]
fn suspend_stop_failure_keeps_that_record_failed_and_resume_reports_it() {
    let mut reg = shutdown_registry(|d| d != 1);
    let mut released = std::vec::Vec::new();
    assert_eq!(
        reg.suspend(|_, _| true, |d| released.push(d)),
        Err(Error::OpFailed)
    );
    // Only the devices stopped before the failure were released.
    assert_eq!(released, [2]);
    assert_eq!(reg.state(2), Ok(State::Stopped));
    assert_eq!(reg.state(1), Ok(State::Failed));
    assert_eq!(reg.state(0), Ok(State::Started));
    // The failed record is retried through its stop callback before it can start again.
    assert_eq!(reg.resume(), Ok((1, 1)));
    assert_eq!(reg.state(2), Ok(State::Started));
    assert_eq!(reg.state(1), Ok(State::Failed));
    assert_eq!(reg.state(0), Ok(State::Started));
}
#[test]
fn resume_refuses_after_shutdown_freeze() {
    let mut reg = shutdown_registry(ok);
    reg.quiesce(|_, _| true, |_| {}).unwrap();
    assert_eq!(reg.resume(), Err(Error::Busy));
}
