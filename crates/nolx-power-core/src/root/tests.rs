use super::*;
use alloc::{vec, vec::Vec};
fn descriptor(space: u8, width: usize, start: u64, end: u64, producer: bool) -> Vec<u8> {
    let mut bytes = vec![
        match width {
            2 => 0x88,
            4 => 0x87,
            _ => 0x8a,
        },
        (3 + 5 * width) as u8,
        0,
        space,
        0x0c | u8::from(!producer),
        if space == 1 {
            3
        } else if space == 0 {
            1
        } else {
            0
        },
    ];
    for value in [0, start, end - 1, 0, end - start] {
        bytes.extend(&value.to_le_bytes()[..width]);
    }
    bytes
}
fn template() -> Vec<u8> {
    [
        descriptor(2, 2, 0, 256, true),
        descriptor(1, 2, 0xc000, 0xf000, true),
        descriptor(0, 4, 0x80000000, 0xf0000000, true),
        vec![0x79, 0],
    ]
    .concat()
}
fn package(op: &[u8], body: Vec<u8>) -> Vec<u8> {
    let mut out = op.to_vec();
    let n = body.len() + 1;
    if n < 64 {
        out.push(n as u8);
    } else {
        let n = body.len() + 2;
        out.extend([0x40 | (n & 15) as u8, (n >> 4) as u8]);
    }
    out.extend(body);
    out
}
fn root(bytes: &[u8], extra: Vec<u8>) -> Vec<u8> {
    let buffer = package(
        &[0x11],
        [
            vec![0x0b],
            (bytes.len() as u16).to_le_bytes().to_vec(),
            bytes.to_vec(),
        ]
        .concat(),
    );
    package(
        &[0x5b, 0x82],
        [
            b"PCI0".to_vec(),
            vec![8],
            b"_HID".to_vec(),
            vec![0x0c, 0x41, 0xd0, 0x0a, 3],
            vec![8],
            b"_CRS".to_vec(),
            buffer,
            extra,
        ]
        .concat(),
    )
}
#[test]
fn address_widths_spaces_and_ownership_survive_decoding() {
    let mut bytes = template();
    bytes.truncate(bytes.len() - 2);
    bytes.extend(descriptor(0, 8, 0x100000000, 0x900000000, true));
    bytes.extend(descriptor(0, 4, 0x81000000, 0x82000000, false));
    bytes.extend([0x79, 0]);
    let parsed = parse(&bytes).unwrap();
    let windows: Vec<_> = parsed.iter().collect();
    assert_eq!(windows.len(), 5);
    assert_eq!(windows[0].space, Space::Bus);
    assert_eq!(windows[1].space, Space::Io);
    assert_eq!(windows[2].start, 0x80000000);
    assert_eq!(windows[3].end, 0x900000000);
    assert!(!windows[4].producer);
}
#[test]
fn fixed_resources_are_reservations_never_produced_apertures() {
    let bytes = [
        0x47, 1, 0xf8, 0xc, 0xf8, 0xc, 1, 8, 0x4b, 0xf8, 3, 8, 0x86, 9, 0, 1, 0, 0, 0, 0xe0, 0,
        0x10, 0, 0, 0x79, 0,
    ];
    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.iter().count(), 3);
    assert!(parsed.iter().all(|w| !w.producer));
}
#[test]
fn translations_variable_bounds_decode_and_reserved_flags_refuse() {
    let good = descriptor(0, 4, 0x80000000, 0x90000000, true);
    for (offset, value) in [(4, 0x0e), (4, 0x08), (4, 0x1c), (5, 0x81), (6, 1), (18, 1)] {
        let mut bytes = good.clone();
        bytes[offset] = value;
        bytes.extend([0x79, 0]);
        assert!(parse(&bytes).is_err(), "{offset}");
    }
}
#[test]
fn extents_ranges_sources_and_overlapping_producers_refuse() {
    let good = template();
    for end in 0..good.len() {
        assert!(parse(&good[..end]).is_err());
    }
    let mut bad = descriptor(0, 4, 0x80000000, 0x90000000, true);
    bad[22] = 0;
    bad[23] = 0;
    bad[24] = 0;
    bad[25] = 0;
    bad.extend([0x79, 0]);
    assert!(parse(&bad).is_err());
    let bad = [
        descriptor(0, 4, 0x80000000, 0x90000000, true),
        descriptor(0, 4, 0x88000000, 0xa0000000, true),
        vec![0x79, 0],
    ]
    .concat();
    assert_eq!(parse(&bad), Err(ResourceError::InvalidRange));
    let mut bad = descriptor(1, 2, 0xc000, 0xf000, true);
    bad[1] += 1;
    bad.push(0);
    bad.extend([0x79, 0]);
    assert_eq!(parse(&bad), Err(ResourceError::InvalidLength));
}
#[test]
fn template_capacity_checksum_and_trailing_bytes_are_checked() {
    assert_eq!(parse(&[0x79, 1]), Err(ResourceError::Checksum));
    assert!(parse(&[0x79, 0x87, 0, 0]).is_ok());
    assert!(parse(&[0x79, 0, 1]).is_err());
    let mut bytes = Vec::new();
    for i in 0..=MAX_WINDOWS {
        bytes.extend(descriptor(
            0,
            4,
            0x80000000 + i as u64 * 4096,
            0x80001000 + i as u64 * 4096,
            true,
        ));
    }
    bytes.extend([0x79, 0]);
    assert_eq!(parse(&bytes), Err(ResourceError::TooMany));
}
#[test]
fn actual_root_methods_and_missing_segment_base_defaults_work() {
    let bytes = root(&template(), Vec::new());
    let mut n = aml::Namespace::new();
    n.load(&bytes).unwrap();
    let bridges = discover(&n).unwrap();
    assert_eq!(bridges.iter().count(), 1);
    assert_eq!(bridges.get(0).unwrap().segment, 0);
    assert_eq!(
        bridges.get(0).unwrap().resources.iter().count(),
        3,
        "the root template keeps all three produced windows"
    );
}
#[test]
fn failing_segment_or_base_methods_do_not_default_to_zero() {
    for member in [b"_SEG", b"_BBN"] {
        let method = package(
            &[0x14],
            [member.to_vec(), vec![0, 0xa4], b"MISS".to_vec()].concat(),
        );
        let bytes = root(&template(), method);
        let mut n = aml::Namespace::new();
        n.load(&bytes).unwrap();
        assert_eq!(discover(&n), Err(Error::Aml(aml::Error::Missing)));
    }
}
#[test]
fn disabled_nonzero_segment_or_base_and_wrong_typed_members_refuse() {
    for (member, value) in [
        (b"_STA", vec![0]),
        (b"_SEG", vec![1]),
        (b"_BBN", vec![1]),
        (b"_SEG", vec![0x0d, b'0', 0]),
    ] {
        let bytes = root(&template(), [vec![8], member.to_vec(), value].concat());
        let mut n = aml::Namespace::new();
        n.load(&bytes).unwrap();
        assert!(discover(&n).is_err());
    }
    let other = package(
        &[0x5b, 0x82],
        [
            b"BAD0".to_vec(),
            vec![8],
            b"_HID".to_vec(),
            package(&[0x11], vec![1, 0]),
        ]
        .concat(),
    );
    let bytes = [root(&template(), Vec::new()), other].concat();
    let mut n = aml::Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(discover(&n), Err(Error::Aml(aml::Error::Type)));
}
#[test]
fn missing_duplicate_or_incomplete_bus_roots_cannot_authorize() {
    assert_eq!(discover(&aml::Namespace::new()), Err(Error::Topology));
    let first = root(&template(), Vec::new());
    let mut second = first.clone();
    let length_bytes = if second[2] & 0xc0 == 0 { 1 } else { 2 };
    second[2 + length_bytes + 3] = b'1';
    let bytes = [first, second].concat();
    let mut n = aml::Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(discover(&n), Err(Error::Topology));
    // A consistent bus window that does not start at the `_BBN` default of zero: the root
    // template is complete enough to parse, but it cannot authorize bus numbering.
    let mut partial = template();
    partial[8] = 0;
    partial[9] = 1; // start = 0x100
    partial[10] = 0xff;
    partial[11] = 1; // maximum = 0x1ff
    let bytes = root(&partial, Vec::new());
    let mut n = aml::Namespace::new();
    n.load(&bytes).unwrap();
    assert!(discover(&n).is_err());
}
#[test]
fn unknown_descriptors_and_every_small_tag_are_bounded() {
    for tag in 0..=255u8 {
        let _ = parse(&[tag, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(
        parse(&[0x2a, 0, 0, 0x79, 0]),
        Err(ResourceError::Unsupported(0x2a))
    );
}
