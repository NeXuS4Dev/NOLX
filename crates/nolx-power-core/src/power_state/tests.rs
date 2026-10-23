use super::*;
use crate::aml::{MethodEffects, RegionField, RegionSpace, Value};
use alloc::vec;
use alloc::vec::Vec;

fn path(text: &str) -> Path {
    Path::parse(text).unwrap()
}

fn package(elements: &[Value]) -> Value {
    Value::Package(elements.to_vec())
}

/// One AML package: opcode bytes, a PkgLength that includes itself, then the body.
fn pkg(op: &[u8], body: Vec<u8>) -> Vec<u8> {
    let mut out = op.to_vec();
    let size = body.len() + 1;
    if size < 64 {
        out.push(size as u8);
    } else {
        let size = body.len() + 2;
        out.extend_from_slice(&[0x40 | (size & 15) as u8, (size >> 4) as u8]);
    }
    out.extend(body);
    out
}

fn method(name: &[u8; 4], args: u8, body: &[u8]) -> Vec<u8> {
    let mut b = name.to_vec();
    b.push(args);
    b.extend_from_slice(body);
    pkg(&[0x14], b)
}

fn device(name: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
    let mut b = name.to_vec();
    b.extend(body);
    pkg(&[0x5b, 0x82], b)
}

fn scope(name: &[u8], body: Vec<u8>) -> Vec<u8> {
    let mut b = name.to_vec();
    b.extend(body);
    pkg(&[0x10], b)
}

#[test]
fn a_power_state_decodes_only_the_four_acpi_states() {
    assert_eq!(DState::parse(0), Ok(DState::D0));
    assert_eq!(DState::parse(3), Ok(DState::D3Hot));
    for value in [4, 5, u64::MAX] {
        assert_eq!(DState::parse(value), Err(Error::Type));
    }
    assert!(!DState::D0.is_low_power());
    assert!(DState::D3Hot.is_low_power());
    assert_eq!(DState::D0.enter_member(), None);
    assert_eq!(DState::D1.enter_member(), Some(*b"_PS3"));
    assert_eq!(DState::D3Hot.name(), "D3hot");
}

#[test]
fn a_wake_source_is_the_two_element_gpe_sleep_state_package() {
    let wake = parse_prw(&package(&[Value::Integer(0x0b), Value::Integer(3)])).unwrap();
    assert_eq!(
        wake,
        WakeSource {
            gpe: 11,
            lowest_sleep_state: 3
        }
    );
    // A source that names S3 wakes from S1..S3 but never from S0 or S4.
    assert!(wake.wakes_from(3));
    assert!(wake.wakes_from(1));
    assert!(!wake.wakes_from(4));
    assert!(!wake.wakes_from(0));
    // A source that names only S0 cannot wake a sleeping system.
    let shallow = parse_prw(&package(&[Value::Integer(1), Value::Integer(0)])).unwrap();
    assert!(!shallow.wakes_from(1));

    // Every malformed shape is refused rather than read as a guess.
    assert_eq!(parse_prw(&Value::Integer(3)), Err(Error::Type));
    assert_eq!(parse_prw(&package(&[Value::Integer(1)])), Err(Error::Type));
    assert_eq!(
        parse_prw(&package(&[
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3)
        ])),
        Err(Error::Type)
    );
    // The extended form names a GPE block device; reading its package as a bit index would
    // invent a wake source, so it is refused.
    assert_eq!(
        parse_prw(&package(&[
            package(&[Value::String(b"\\_SB.PCI0".to_vec()), Value::Integer(4)]),
            Value::Integer(3)
        ])),
        Err(Error::Type)
    );
    // A sleep state above S4 is not a wake depth.
    assert_eq!(
        parse_prw(&package(&[Value::Integer(0), Value::Integer(5)])),
        Err(Error::Type)
    );
    // A negative (unrepresentable) or oversized GPE index is refused.
    assert_eq!(
        parse_prw(&package(&[
            Value::Integer(u64::from(u32::MAX) + 1),
            Value::Integer(3)
        ])),
        Err(Error::Type)
    );
}

#[test]
fn a_pci_address_splits_into_device_and_function() {
    assert_eq!(adr_parts(0x0003_0000), (3, 0));
    assert_eq!(adr_parts(0x001f_0002), (0x1f, 2));
    assert_eq!(adr_parts(0), (0, 0));
}

/// A namespace with one device that declares the whole power-method set.
fn power_device() -> Namespace<'static> {
    let body = [
        // Name (_ADR, 0x00030000)
        vec![0x08, b'_', b'A', b'D', b'R', 0x0c, 0x00, 0x00, 0x03, 0x00],
        // Name (_PSC, 3)
        vec![0x08, b'_', b'P', b'S', b'C', 0x0a, 0x03],
        // Name (_PRW, Package { 0x0b, 3 })
        vec![
            0x08, b'_', b'P', b'R', b'W', 0x12, 0x06, 0x02, 0x0a, 0x0b, 0x0a, 0x03,
        ],
        method(b"_PS0", 0, &[]),
        method(b"_PS3", 0, &[]),
    ]
    .concat();
    let mut namespace = Namespace::new();
    namespace
        .load(Vec::leak(scope(b"\\_SB_", device(b"PWR0", body))))
        .unwrap();
    namespace
}

#[test]
fn a_device_declares_which_power_methods_it_has() {
    let namespace = power_device();
    let node = path("\\_SB_.PWR0");
    let methods = power_methods(&namespace, &node);
    assert_eq!(
        methods,
        PowerMethods {
            ps0: true,
            ps3: true,
            psc: true,
            prw: true,
        }
    );
    assert!(methods.can_suspend());
    assert!(methods.any());
    // A device with none of them is reported as having nothing to do, not as an error.
    let bare = path("\\_SB_");
    assert_eq!(power_methods(&namespace, &bare), PowerMethods::default());
    assert!(!power_methods(&namespace, &bare).any());
}

#[test]
fn a_devices_power_state_address_and_wake_source_are_read_through_the_evaluator() {
    let namespace = power_device();
    let node = path("\\_SB_.PWR0");
    assert_eq!(current_state(&namespace, &node), Ok(Some(DState::D3Hot)));
    assert_eq!(device_address(&namespace, &node), Ok(Some(0x0003_0000)));
    assert_eq!(
        wake_source(&namespace, &node),
        Ok(Some(WakeSource {
            gpe: 11,
            lowest_sleep_state: 3
        }))
    );
    // Absent members are distinguishable from failed ones.
    let absent = path("\\_SB_");
    assert_eq!(current_state(&namespace, &absent), Err(Error::Type));
}

#[test]
fn the_transition_plan_lists_only_devices_that_declare_a_power_method() {
    let namespace = power_device();
    let targets = power_targets(&namespace);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].path, path("\\_SB_.PWR0"));
    assert_eq!(targets[0].current, Some(DState::D3Hot));
    assert_eq!(targets[0].wake.map(|w| w.gpe), Some(11));
    assert!(targets[0].methods.can_suspend());
}

#[test]
fn a_power_transition_journals_any_value_firmware_writes() {
    // `\_SB.PWR0._PS3` writes 0x2a into a SystemMemory field at 0x2000: a value this kernel
    // cannot know in advance, which is exactly why the exact-value permission cannot serve it.
    let field = RegionField {
        space: RegionSpace::SystemMemory,
        address: 0x2000,
        bits: 8,
        shift: 0,
        access: 1,
    };
    // OperationRegion (PRGN, SystemMemory, 0x2000, 0x10): no PkgLength, unlike a package.
    let mut region = vec![0x5b, 0x80];
    region.extend(b"PRGN");
    region.extend([0x00, 0x0c, 0x00, 0x20, 0x00, 0x00, 0x0a, 0x10]);
    // Field (PRGN, ByteAcc, NoLock, Preserve) { FLDA, 8 }
    let mut field_decl = b"PRGN".to_vec();
    field_decl.push(0x01);
    field_decl.extend(b"FLDA");
    field_decl.push(0x08);
    // Method (_PS3, 0) { Store (0x2a, FLDA) }
    let ps3 = method(b"_PS3", 0, &[0x70, 0x0a, 0x2a, b'F', b'L', b'D', b'A']);
    let body = [region, pkg(&[0x5b, 0x81], field_decl), ps3].concat();
    let bytes = scope(b"\\_SB_", device(b"PWR1", body));

    let mut namespace = Namespace::new();
    namespace.load(Vec::leak(bytes)).unwrap();
    let node = path("\\_SB_.PWR1");
    let effects = namespace
        .plan_device_method_authorized(&node, *b"_PS3", &[])
        .unwrap()
        .expect("the device declares _PS3");
    assert_eq!(
        effects.region_writes,
        alloc::vec![crate::aml::RegionFieldWrite { field, value: 0x2a }]
    );
    // An absent member is reported as absent, not as an empty effect list.
    assert_eq!(
        namespace.plan_device_method_authorized(&node, *b"_PS0", &[]),
        Ok(None)
    );
    // A non-device node is a type error, so a policy cannot transition a scope.
    assert_eq!(
        namespace.plan_device_method_authorized(&path("\\_SB_"), *b"_PS3", &[]),
        Err(Error::Type)
    );
}

/// A backend over one byte, recording what was written and able to refuse or lie.
struct Byte {
    value: u8,
    writes: usize,
    refuse: bool,
    dishonest: bool,
}

impl crate::aml::RegionFieldWriteBackend for Byte {
    type Error = ();

    fn read_field(&mut self, _: RegionField) -> Result<u64, ()> {
        Ok(u64::from(self.value))
    }

    fn write_field(&mut self, _: RegionField, value: u64) -> Result<(), ()> {
        if self.refuse {
            return Err(());
        }
        self.writes += 1;
        if !self.dishonest {
            self.value = value as u8;
        }
        Ok(())
    }
}

fn field() -> RegionField {
    RegionField {
        space: RegionSpace::SystemMemory,
        address: 0x2000,
        bits: 8,
        shift: 0,
        access: 1,
    }
}

fn effects(values: &[u64]) -> MethodEffects {
    MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: values
            .iter()
            .map(|&value| crate::aml::RegionFieldWrite {
                field: field(),
                value,
            })
            .collect(),
    }
}

#[test]
fn an_authorized_transition_applies_verifies_and_rolls_back() {
    let mut byte = Byte {
        value: 0x11,
        writes: 0,
        refuse: false,
        dishonest: false,
    };
    let applied =
        crate::aml::apply_region_field_writes_authorized(&effects(&[0x2a]), &mut byte).unwrap();
    assert_eq!(applied.len(), 1);
    assert_eq!((byte.value, byte.writes), (0x2a, 1));

    // A backend that refuses the write has the earlier value restored.
    let mut byte = Byte {
        value: 0x11,
        writes: 0,
        refuse: true,
        dishonest: false,
    };
    assert_eq!(
        crate::aml::apply_region_field_writes_authorized(&effects(&[0x2a]), &mut byte),
        Err(crate::aml::RegionWriteTransactionError::Write(()))
    );

    // A backend that accepts the write but reports the old value back is a readback mismatch,
    // and the attempt is rolled back and reported: a silent lie is not a transition.
    let mut byte = Byte {
        value: 0x11,
        writes: 0,
        refuse: false,
        dishonest: true,
    };
    assert_eq!(
        crate::aml::apply_region_field_writes_authorized(&effects(&[0x2a]), &mut byte),
        Err(crate::aml::RegionWriteTransactionError::ReadbackMismatch)
    );

    // The same field twice can never be rolled back well, so it is refused before any access.
    let mut byte = Byte {
        value: 0x11,
        writes: 0,
        refuse: false,
        dishonest: true,
    };
    assert_eq!(
        crate::aml::apply_region_field_writes_authorized(&effects(&[0x2a, 0x2b]), &mut byte),
        Err(crate::aml::RegionWriteTransactionError::DuplicateField)
    );
    assert_eq!(byte.writes, 0);

    // A malformed field is refused before the backend is touched either.
    let mut bad = effects(&[1]);
    bad.region_writes[0].field.access = 3;
    let mut byte = Byte {
        value: 0,
        writes: 0,
        refuse: false,
        dishonest: false,
    };
    assert_eq!(
        crate::aml::apply_region_field_writes_authorized(&bad, &mut byte),
        Err(crate::aml::RegionWriteTransactionError::InvalidField)
    );
    assert_eq!(byte.writes, 0);
}
