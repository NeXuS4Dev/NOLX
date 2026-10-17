use super::*;
use alloc::vec;
fn row(address: u64, pin: u64, source: Value, index: u64) -> Value {
    Value::Package(vec![
        Value::Integer(address),
        Value::Integer(pin),
        source,
        Value::Integer(index),
    ])
}
#[test]
fn current_irq_descriptors_preserve_flags_and_validate_lengths_checksums() {
    let extended = [0x89, 6, 0, 9, 1, 20, 0, 0, 0, 0x79, 0];
    assert_eq!(
        interrupt(&extended),
        Ok(Interrupt {
            gsi: 20,
            active_low: false,
            level: true,
            shared: true
        })
    );
    let small = [0x23, 0, 8, 0x18, 0x79, 0];
    assert_eq!(
        interrupt(&small).unwrap(),
        Interrupt {
            gsi: 11,
            active_low: true,
            level: true,
            shared: true
        }
    );
    for len in 0..extended.len() {
        assert!(interrupt(&extended[..len]).is_err());
    }
    let mut checksum = extended;
    checksum[10] = 0u8.wrapping_sub(checksum[..10].iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    assert!(interrupt(&checksum).is_ok());
    checksum[10] ^= 1;
    assert!(interrupt(&checksum).is_err());
    for (index, value) in [(1, 7), (3, 0), (3, 0x89), (4, 2), (9, 0)] {
        let mut bad = extended;
        bad[index] = value;
        assert!(interrupt(&bad).is_err());
    }
    assert!(interrupt(&[0x22, 3, 0, 0x79, 0]).is_err());
    assert!(interrupt(&[extended.as_slice(), &[1]].concat()).is_err());
}
#[test]
fn direct_routes_validate_pin_address_function_and_ambiguity() {
    let ns = Namespace::new();
    let valid = row(3 << 16 | 0xffff, 0, Value::Integer(0), 21);
    assert_eq!(
        entries(&ns, Value::Package(vec![valid.clone()])).unwrap()[0].interrupt,
        Interrupt {
            gsi: 21,
            active_low: true,
            level: true,
            shared: true
        }
    );
    assert_eq!(
        entries(
            &ns,
            Value::Package(vec![valid.clone(), row(3 << 16, 0, Value::Integer(0), 22)])
        ),
        Err(Error::Ambiguous)
    );
    assert_eq!(
        entries(&ns, Value::Package(vec![valid.clone(), valid])),
        Err(Error::Ambiguous)
    );
    for bad in [
        row(32 << 16, 0, Value::Integer(0), 1),
        row(8, 0, Value::Integer(0), 1),
        row(0, 4, Value::Integer(0), 1),
        row(0, 0, Value::Integer(1), 1),
        row(0, 0, Value::Integer(0), u64::MAX),
    ] {
        assert!(entries(&ns, Value::Package(vec![bad])).is_err());
    }
}
#[test]
fn link_routes_require_current_usable_single_shared_level_resource() {
    // Device LNK0, Name(_CRS, Buffer(){ ExtendedIRQ, EndTag }).
    let bytes = [
        0x5b, 0x82, 25, b'L', b'N', b'K', b'0', 8, b'_', b'C', b'R', b'S', 0x11, 14, 0x0a, 11,
        0x89, 6, 0, 9, 1, 20, 0, 0, 0, 0x79, 0,
    ];
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    let link = Value::Reference(Path::parse("\\LNK0").unwrap());
    assert_eq!(
        entries(&ns, Value::Package(vec![row(0xffff, 0, link.clone(), 0)])).unwrap()[0]
            .interrupt
            .gsi,
        20
    );
    assert!(entries(&ns, Value::Package(vec![row(0xffff, 0, link, 1)])).is_err());
    let mut edge = bytes;
    edge[19] = 11;
    let mut ns = Namespace::new();
    ns.load(&edge).unwrap();
    assert!(entries(
        &ns,
        Value::Package(vec![row(
            0xffff,
            0,
            Value::Reference(Path::parse("\\LNK0").unwrap()),
            0
        )])
    )
    .is_err());
}
