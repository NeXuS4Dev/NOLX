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
};

const STORAGE: DeviceId = DeviceId {
    vendor: 0x1AF4,
    device: 0x1042,
    class: 0x01,
    subclass: 0x00,
    prog_if: 0x00,
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
fn a_failed_start_marks_the_device_failed_and_only_allows_removal() {
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
