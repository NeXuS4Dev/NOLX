use super::*;
fn pkg(op: &[u8], body: Vec<u8>) -> Vec<u8> {
    let mut out = op.to_vec();
    let n = body.len() + 1;
    if n < 64 {
        out.push(n as u8);
    } else {
        let n = body.len() + 2;
        out.extend_from_slice(&[0x40 | (n & 15) as u8, (n >> 4) as u8]);
    }
    out.extend(body);
    out
}
fn method(name: &[u8; 4], args: u8, body: &[u8]) -> Vec<u8> {
    let mut b = name.to_vec();
    b.push(args);
    b.extend(body);
    pkg(&[0x14], b)
}
fn path(s: &str) -> Path {
    Path::parse(s).unwrap()
}
#[test]
fn names_and_paths_reject_invalid_segments_and_parent_underflow() {
    assert!(Path::parse("_SB_.PCI0").is_err());
    for s in ["\\bad_", "\\AB", "\\ABCD..EFGH", "\\0ABC"] {
        assert_eq!(Path::parse(s), Err(Error::Name));
    }
    assert_eq!(path("\\_SB_.PCI0").to_string(), "\\_SB_.PCI0");
    let mut n = Namespace::new();
    assert_eq!(
        n.load(&[8, b'^', b'N', b'A', b'M', b'E', 0]),
        Err(Error::Name)
    );
}
#[test]
fn evaluates_real_method_bodies_and_if_else_not_byte_scanned_markers() {
    let mut bytes = vec![8];
    bytes.extend(b"FLAG");
    bytes.extend([0x0a, 3]);
    let branch = pkg(
        &[0xa0],
        [vec![0x93], b"FLAG".to_vec(), vec![0x0a, 3, 0xa4, 0x0a, 15]].concat(),
    );
    let body = [branch, pkg(&[0xa1], vec![0xa4, 0])].concat();
    bytes.extend(method(b"_STA", 0, &body));
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\_STA"), &[]), Ok(Value::Integer(15)));
}
#[test]
fn returns_named_resource_buffers_and_preserves_zero_padding() {
    let buf = pkg(&[0x11], vec![0x0a, 6, 0x79, 0]);
    let bytes = [
        vec![8],
        b"CRSB".to_vec(),
        buf,
        method(b"_CRS", 0, &[0xa4, b'C', b'R', b'S', b'B']),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(
        n.evaluate(&path("\\_CRS"), &[]),
        Ok(Value::Buffer(vec![0x79, 0, 0, 0, 0, 0]))
    );
}
#[test]
fn scopes_devices_parent_search_and_absolute_names_have_distinct_identity() {
    let mut device = b"DEV0".to_vec();
    device.extend(method(b"_STA", 0, &[0xa4, b'^', b'V', b'A', b'L', b'U']));
    let bytes = [
        vec![8],
        b"VALU".to_vec(),
        vec![0x0a, 15],
        pkg(&[0x5b, 0x82], device),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(
        n.devices().map(ToString::to_string).collect::<Vec<_>>(),
        ["\\DEV0"]
    );
    assert_eq!(
        n.evaluate(&path("\\DEV0._STA"), &[]),
        Ok(Value::Integer(15))
    );
}
#[test]
fn table_load_is_atomic_on_unknown_opcode_duplicate_and_truncated_package() {
    let initial = [vec![8], b"GOOD".to_vec(), vec![1]].concat();
    let mut n = Namespace::new();
    n.load(&initial).unwrap();
    let count = n.len();
    for bad in &[
        [vec![8], b"TEMP".to_vec(), vec![1, 0xfe]].concat(),
        initial.clone(),
        vec![0x14, 63, b'X'],
    ] {
        assert!(n.load(bad).is_err());
        assert_eq!(n.len(), count);
        assert_eq!(n.evaluate(&path("\\GOOD"), &[]), Ok(Value::Integer(1)));
        assert_eq!(n.evaluate(&path("\\TEMP"), &[]), Err(Error::Missing));
    }
}
#[test]
fn arguments_locals_arithmetic_and_method_calls_share_correct_scopes() {
    let bytes = [
        method(b"INCR", 1, &[0xa4, 0x72, 0x68, 1, 0]),
        method(
            b"MAIN",
            0,
            &[0x70, 0x0a, 14, 0x60, 0xa4, b'I', b'N', b'C', b'R', 0x60],
        ),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\MAIN"), &[]), Ok(Value::Integer(15)));
    assert_eq!(n.evaluate(&path("\\INCR"), &[]), Err(Error::Type));
}
#[test]
fn infinite_loops_and_recursion_are_bounded() {
    let bytes = [
        method(b"LOOP", 0, &pkg(&[0xa2], vec![1, 0xa3])),
        method(b"RECU", 0, &[0xa4, b'R', b'E', b'C', b'U']),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\LOOP"), &[]), Err(Error::Steps));
    assert_eq!(n.evaluate(&path("\\RECU"), &[]), Err(Error::Limit));
}
#[test]
fn global_writes_and_hardware_objects_are_explicitly_refused() {
    let bytes = [
        vec![8],
        b"FLAG".to_vec(),
        vec![1],
        method(b"WRIT", 0, &[0x70, 0, b'F', b'L', b'A', b'G', 0xa4, 0]),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\WRIT"), &[]), Err(Error::Target));
    assert_eq!(n.evaluate(&path("\\FLAG"), &[]), Ok(Value::Integer(1)));
    let region = [
        vec![0x5b, 0x80],
        b"REGN".to_vec(),
        vec![0, 0, 0x0a, 4],
        method(b"READ", 0, &[0xa4, b'R', b'E', b'G', b'N']),
    ]
    .concat();
    n.load(&region).unwrap();
    assert_eq!(
        n.evaluate(&path("\\READ"), &[]),
        Err(Error::Unsupported(0x5b80))
    );
}
#[test]
fn bytecode_inside_buffer_never_creates_namespace_objects() {
    let bytes = [
        vec![8],
        b"DATA".to_vec(),
        pkg(&[0x11], vec![0x0a, 7, 8, b'F', b'A', b'K', b'E', 1, 0]),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\FAKE"), &[]), Err(Error::Missing));
}
#[test]
fn buffer_package_and_path_limits_are_enforced() {
    let huge = [
        vec![8],
        b"HUGE".to_vec(),
        pkg(&[0x11], vec![0x0c, 0, 0, 1, 0]),
    ]
    .concat();
    assert_eq!(Namespace::new().load(&huge), Err(Error::Limit));
    let package = [vec![8], b"HUGE".to_vec(), pkg(&[0x13], vec![0x0b, 1, 1])].concat();
    assert_eq!(Namespace::new().load(&package), Err(Error::Limit));
    let long = "\\".to_owned() + &vec!["ABCD"; 33].join(".");
    assert_eq!(Path::parse(&long), Err(Error::Limit));
}
#[test]
fn all_prefixes_of_valid_table_fail_safely_or_load_complete_terms() {
    let bytes = method(b"TEST", 0, &[0xa4, 0x0e, 1, 2, 3, 4, 5, 6, 7, 8]);
    for end in 0..bytes.len() {
        let mut n = Namespace::new();
        let _ = n.load(&bytes[..end]);
        assert_eq!(n.evaluate(&path("\\TEST"), &[]), Err(Error::Missing));
    }
}
#[test]
fn packages_retain_reference_paths_and_external_definitions_can_be_completed() {
    let bytes = [
        vec![0x15],
        b"TEST".to_vec(),
        vec![8, 0],
        method(b"TEST", 0, &[0xa4, 1]),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\TEST"), &[]), Ok(Value::Integer(1)));
}

#[test]
fn default_status_is_only_for_an_absent_member_not_a_failing_method() {
    let mut first = b"DEV0".to_vec();
    first.extend(method(b"_STA", 0, &[0xa4, b'N', b'O', b'N', b'E']));
    let bytes = [
        pkg(&[0x5b, 0x82], b"DEV1".to_vec()),
        pkg(&[0x5b, 0x82], first),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.device_status(&path("\\DEV1")), Ok((15, true)));
    assert_eq!(n.device_status(&path("\\DEV0")), Err(Error::Missing));
    assert_eq!(n.device_status(&path("\\NOPE")), Err(Error::Type));
    assert_eq!(n.device_resources(&path("\\DEV1")), Ok(None));
}
#[test]
fn wrong_status_and_resource_types_are_not_coerced_into_authority() {
    let body = [
        b"DEV0".to_vec(),
        method(b"_STA", 0, &[0xa4, 0x0d, b'Y', 0]),
        method(b"_CRS", 0, &[0xa4, 1]),
    ]
    .concat();
    let bytes = pkg(&[0x5b, 0x82], body);
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.device_status(&path("\\DEV0")), Err(Error::Type));
    assert_eq!(n.device_resources(&path("\\DEV0")), Err(Error::Type));
}
#[test]
fn branches_do_not_execute_the_unselected_unsupported_body() {
    let body = [
        pkg(&[0xa0], vec![0, 0xfe]),
        pkg(&[0xa1], vec![0xa4, 0x0a, 15]),
    ]
    .concat();
    let bytes = method(b"TEST", 0, &body);
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\TEST"), &[]), Ok(Value::Integer(15)));
}
#[test]
fn rejected_later_table_restores_external_objects_not_only_vector_length() {
    let external = [vec![0x15], b"TEST".to_vec(), vec![8, 0]].concat();
    let bad = [method(b"TEST", 0, &[0xa4, 1]), vec![0xfe]].concat();
    let mut n = Namespace::new();
    n.load(&external).unwrap();
    assert!(n.load(&bad).is_err());
    assert_eq!(n.evaluate(&path("\\TEST"), &[]), Err(Error::Missing));
}

#[test]
fn namespace_object_exhaustion_is_atomic() {
    let mut bytes = Vec::new();
    for i in 0..MAX_OBJECTS {
        bytes.push(8);
        bytes.extend(format!("N{i:03X}").as_bytes());
        bytes.push(1);
    }
    let mut n = Namespace::new();
    let initial = n.len();
    assert_eq!(n.load(&bytes), Err(Error::Limit));
    assert_eq!(n.len(), initial);
}
#[test]
fn aggregate_data_exhaustion_rolls_back_its_accounting_too() {
    let mut bytes = Vec::new();
    for i in 0..17 {
        bytes.push(8);
        bytes.extend(format!("B{i:03X}").as_bytes());
        bytes.extend(pkg(&[0x11], vec![0x0b, 0, 0x40]));
    }
    let initial = [vec![8], b"GOOD".to_vec(), vec![1]].concat();
    let mut n = Namespace::new();
    n.load(&initial).unwrap();
    let cost = n.value_bytes;
    assert_eq!(n.load(&bytes), Err(Error::Limit));
    assert_eq!(n.value_bytes, cost);
    assert_eq!(n.evaluate(&path("\\GOOD"), &[]), Ok(Value::Integer(1)));
}
#[test]
fn argument_and_compound_store_limits_prevent_frame_amplification() {
    let bytes = [
        method(b"ARGS", 1, &[0xa4, 0x68]),
        method(b"COPY", 0, &[0x70, 0x11, 3, 1, 0, 0x60, 0xa4, 0]),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(
        n.evaluate(
            &path("\\ARGS"),
            &[Value::Buffer(vec![0; MAX_VALUE_BYTES + 1])]
        ),
        Err(Error::Limit)
    );
    assert_eq!(n.evaluate(&path("\\COPY"), &[]), Err(Error::Target));
}
#[test]
fn arbitrary_small_opcode_streams_cannot_panic_or_escape_limits() {
    for byte in 0..=255u8 {
        let bytes = method(b"TEST", 0, &[0xa4, byte, 0, 0, 0]);
        let mut n = Namespace::new();
        n.load(&bytes).unwrap();
        let _ = n.evaluate(&path("\\TEST"), &[]);
        let mut n = Namespace::new();
        let _ = n.load(&[byte, 0, 0, 0, 0]);
    }
}

#[test]
fn scopes_resolve_existing_upward_targets_and_never_invent_containers() {
    let patch = pkg(
        &[0x10],
        [b"ROOT".to_vec(), vec![8], b"FLAG".to_vec(), vec![1]].concat(),
    );
    let child = pkg(&[0x5b, 0x82], [b"CHLD".to_vec(), patch].concat());
    let root = pkg(&[0x5b, 0x82], [b"ROOT".to_vec(), child].concat());
    let mut n = Namespace::new();
    n.load(&root).unwrap();
    assert_eq!(n.evaluate(&path("\\ROOT.FLAG"), &[]), Ok(Value::Integer(1)));
    let bad = pkg(&[0x10], b"MISS".to_vec());
    let count = n.len();
    assert_eq!(n.load(&bad), Err(Error::Missing));
    assert_eq!(n.len(), count);
    let global = pkg(&[0x10], vec![b'\\', 0, 8, b'T', b'O', b'P', b'0', 1]);
    n.load(&global).unwrap();
    assert_eq!(n.evaluate(&path("\\TOP0"), &[]), Ok(Value::Integer(1)));
}
#[test]
fn a_nearer_multisegment_prefix_does_not_fall_through_to_another_subtree() {
    let root = pkg(
        &[0x5b, 0x82],
        [b"BRCH".to_vec(), vec![8], b"VALU".to_vec(), vec![0x0a, 15]].concat(),
    );
    let child = pkg(&[0x5b, 0x82], b"BRCH".to_vec());
    let method = method(
        b"_STA",
        0,
        &[0xa4, 0x2e, b'B', b'R', b'C', b'H', b'V', b'A', b'L', b'U'],
    );
    let bytes = [
        root,
        pkg(&[0x5b, 0x82], [b"DEV0".to_vec(), child, method].concat()),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.device_status(&path("\\DEV0")), Err(Error::Missing));
}
#[test]
fn aliases_preserve_method_arity_and_original_definition_scope() {
    let bytes = [
        vec![8],
        b"BASE".to_vec(),
        vec![1],
        method(b"HELP", 1, &[0xa4, 0x72, 0x68, b'B', b'A', b'S', b'E', 0]),
        vec![6],
        b"HELP".to_vec(),
        b"ALSO".to_vec(),
        method(b"TEST", 0, &[0xa4, b'A', b'L', b'S', b'O', 0x0a, 14]),
        vec![6],
        b"BASE".to_vec(),
        b"NUM1".to_vec(),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\TEST"), &[]), Ok(Value::Integer(15)));
    assert_eq!(
        n.evaluate(&path("\\ALSO"), &[Value::Integer(14)]),
        Ok(Value::Integer(15))
    );
    assert_eq!(n.evaluate(&path("\\NUM1"), &[]), Ok(Value::Integer(1)));
}
#[test]
fn forward_module_and_method_names_retain_their_lexical_search_context() {
    let child = pkg(
        &[0x5b, 0x82],
        [
            b"DEV0".to_vec(),
            vec![8],
            b"COPY".to_vec(),
            b"LATE".to_vec(),
            method(b"_STA", 0, &[0xa4, b'C', b'O', b'P', b'Y']),
        ]
        .concat(),
    );
    let late = [vec![8], b"LATE".to_vec(), vec![0x0a, 15]].concat();
    let mut n = Namespace::new();
    n.load(&child).unwrap();
    assert_eq!(n.device_status(&path("\\DEV0")), Err(Error::Missing));
    n.load(&late).unwrap();
    assert_eq!(n.device_status(&path("\\DEV0")), Ok((15, false)));
    assert_eq!(
        n.evaluate(&path("\\DEV0.COPY"), &[]),
        Ok(Value::Integer(15))
    );
}
#[test]
fn forward_package_references_resolve_to_the_actual_parent_device_not_a_guess() {
    let package = pkg(&[0x12], [vec![1], b"LATE".to_vec()].concat());
    let child = pkg(
        &[0x5b, 0x82],
        [b"DEV0".to_vec(), vec![8], b"PKG0".to_vec(), package].concat(),
    );
    let late = pkg(&[0x5b, 0x82], b"LATE".to_vec());
    let bytes = [child, late].concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(
        n.evaluate(&path("\\DEV0.PKG0"), &[]),
        Ok(Value::Package(vec![Value::Reference(path("\\LATE"))]))
    );
}

#[test]
fn cyclic_forward_data_references_and_aliases_fail_with_a_depth_limit() {
    let bytes = [
        vec![8],
        b"ONE0".to_vec(),
        b"TWO0".to_vec(),
        vec![8],
        b"TWO0".to_vec(),
        b"ONE0".to_vec(),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\ONE0"), &[]), Err(Error::Limit));
    let aliases = [
        vec![0x15],
        b"BASE".to_vec(),
        vec![0, 0, 6],
        b"BASE".to_vec(),
        b"ALSO".to_vec(),
        vec![6],
        b"ALSO".to_vec(),
        b"BASE".to_vec(),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&aliases).unwrap();
    assert_eq!(n.evaluate(&path("\\BASE"), &[]), Err(Error::Limit));
}

#[test]
fn dsdt_revision_controls_wrapping_ones_logical_and_shift_width_for_all_tables() {
    let bytes = [
        method(b"WRAP", 0, &[0xa4, 0x72, 0x0c, 255, 255, 255, 255, 1, 0]),
        method(b"ONES", 0, &[0xa4, 0xff]),
        method(b"TRUE", 0, &[0xa4, 0x92, 0]),
        method(b"SHFT", 0, &[0xa4, 0x7a, 0xff, 0x0a, 32, 0]),
    ]
    .concat();
    for (revision, wrap, ones, shift) in [
        (1, 0, u32::MAX as u64, 0),
        (2, 1u64 << 32, u64::MAX, u32::MAX as u64),
    ] {
        let mut n = Namespace::with_dsdt_revision(revision);
        n.load(&bytes).unwrap();
        for (name, value) in [
            ("\\WRAP", wrap),
            ("\\ONES", ones),
            ("\\TRUE", ones),
            ("\\SHFT", shift),
        ] {
            assert_eq!(n.evaluate(&path(name), &[]), Ok(Value::Integer(value)));
        }
        let ssdt = method(b"ARGS", 1, &[0xa4, 0x68]);
        n.load(&ssdt).unwrap();
        assert_eq!(
            n.evaluate(&path("\\ARGS"), &[Value::Integer(u64::MAX)]),
            Ok(Value::Integer(ones))
        );
    }
}
#[test]
fn revision_one_qword_literals_cannot_invent_high_bits_in_status_or_buffer_sizes() {
    let bytes = [
        vec![8],
        b"WIDE".to_vec(),
        vec![0x0e, 15, 0, 0, 0, 1, 0, 0, 0],
    ]
    .concat();
    let mut n = Namespace::with_dsdt_revision(1);
    n.load(&bytes).unwrap();
    assert_eq!(n.evaluate(&path("\\WIDE"), &[]), Ok(Value::Integer(15)));
}
