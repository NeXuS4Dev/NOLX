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
fn device_method_arity_is_inspected_without_invoking_or_coercing_members() {
    let mut device = b"DEV0".to_vec();
    device.extend(method(b"_SRS", 1, &[0xa3]));
    device.extend([8, b'_', b'C', b'R', b'S', 0]);
    let mut namespace = Namespace::new();
    namespace
        .load(&pkg(&[0x5b, 0x82], device))
        .unwrap();
    let dev = path("\\DEV0");
    assert_eq!(namespace.device_method_arity(&dev, *b"_SRS"), Ok(Some(1)));
    assert_eq!(namespace.device_method_arity(&dev, *b"_STA"), Ok(None));
    assert_eq!(namespace.device_method_arity(&dev, *b"_CRS"), Err(Error::Type));
    assert_eq!(
        namespace.device_method_arity(&path("\\MISS"), *b"_SRS"),
        Err(Error::Type)
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
        pkg(
            &[0x5b, 0x82],
            [b"DEV0".to_vec(), child, method.clone()].concat(),
        ),
        pkg(&[0x5b, 0x82], [b"DEV1".to_vec(), method].concat()),
    ]
    .concat();
    let mut n = Namespace::new();
    n.load(&bytes).unwrap();
    assert_eq!(n.device_status(&path("\\DEV0")), Err(Error::Missing));
    assert_eq!(n.device_status(&path("\\DEV1")), Err(Error::Missing));
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

#[test]
fn pic_notification_validates_the_entire_body_before_mutating_only_named_integer() {
    let mut bytes = [vec![8], b"PICF".to_vec(), vec![0]].concat();
    bytes.extend(method(b"_PIC", 1, &[0x70, 0x68, b'P', b'I', b'C', b'F']));
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    assert_eq!(ns.select_interrupt_model(true), Ok(true));
    assert_eq!(ns.evaluate(&path("\\PICF"), &[]), Ok(Value::Integer(1)));
    assert_eq!(ns.select_interrupt_model(false), Ok(true));
    assert_eq!(ns.evaluate(&path("\\PICF"), &[]), Ok(Value::Integer(0)));
    assert!(ns.evaluate(&path("\\_PIC"), &[Value::Integer(1)]).is_err());
    assert_eq!(Namespace::new().select_interrupt_model(true), Ok(false));
    for body in [
        vec![0x70, 0x68, b'P', b'I', b'C', b'F', 0xa3],
        vec![0x70, 1, b'P', b'I', b'C', b'F'],
    ] {
        let bytes = [
            vec![8],
            b"PICF".to_vec(),
            vec![0],
            method(b"_PIC", 1, &body),
        ]
        .concat();
        let mut ns = Namespace::new();
        ns.load(&bytes).unwrap();
        assert!(ns.select_interrupt_model(true).is_err());
        assert_eq!(ns.evaluate(&path("\\PICF"), &[]), Ok(Value::Integer(0)));
    }
}
#[test]
fn private_method_buffer_fields_are_fresh_bounded_and_cannot_write_namespace_buffers() {
    let buffer = pkg(&[0x11], vec![0x0a, 4, 0, 0, 0, 0]);
    let body = [
        vec![8],
        b"PRR0".to_vec(),
        buffer.clone(),
        vec![0x8a],
        b"PRR0".to_vec(),
        vec![0],
        b"PRRI".to_vec(),
        vec![0x70, 0x68],
        b"PRRI".to_vec(),
        vec![0xa4],
        b"PRR0".to_vec(),
    ]
    .concat();
    let bytes = method(b"IQCR", 1, &body);
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    for value in [11, 20, 0] {
        assert_eq!(
            ns.evaluate(&path("\\IQCR"), &[Value::Integer(value)]),
            Ok(Value::Buffer((value as u32).to_le_bytes().to_vec()))
        );
    }
    assert!(ns.evaluate(&path("\\PRR0"), &[]).is_err());
    let bytes = [
        vec![8],
        b"PRR0".to_vec(),
        buffer,
        method(
            b"BAD0",
            0,
            &[
                0x8a, b'P', b'R', b'R', b'0', 0, b'F', b'L', b'D', b'0', 0xa4, 0,
            ],
        ),
    ]
    .concat();
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    assert!(ns.evaluate(&path("\\BAD0"), &[]).is_err());
    assert_eq!(
        ns.evaluate(&path("\\PRR0"), &[]),
        Ok(Value::Buffer(vec![0; 4]))
    );
}
fn pci_field_fixture(adr: Vec<u8>, flags: u8) -> Vec<u8> {
    let region = [
        vec![0x5b, 0x80],
        b"PCFG".to_vec(),
        vec![2, 0x0a, 0x60, 0x0a, 4],
    ]
    .concat();
    let field = pkg(
        &[0x5b, 0x81],
        [b"PCFG".to_vec(), vec![flags], b"PRQ0".to_vec(), vec![8]].concat(),
    );
    let child = pkg(
        &[0x5b, 0x82],
        [b"ISA0".to_vec(), adr, region, field].concat(),
    );
    pkg(
        &[0x5b, 0x82],
        [
            b"PCI0".to_vec(),
            vec![8],
            b"_HID".to_vec(),
            vec![0x0d],
            b"PNP0A03\0".to_vec(),
            child,
        ]
        .concat(),
    )
}
#[test]
fn pci_fields_require_readonly_provider_static_root_metadata_and_byte_access() {
    struct Reader;
    impl FieldReader for Reader {
        fn read(&self, f: PciField) -> Result<u64, Error> {
            assert_eq!(
                f,
                PciField {
                    address: 0x10000,
                    offset: 0x60,
                    bytes: 1
                }
            );
            Ok(11)
        }
    }
    let adr = [vec![8], b"_ADR".to_vec(), vec![0x0c, 0, 0, 1, 0]].concat();
    let bytes = pci_field_fixture(adr.clone(), 1);
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    assert!(ns.evaluate(&path("\\PCI0.ISA0.PRQ0"), &[]).is_err());
    ns.set_field_reader(&Reader);
    assert_eq!(
        ns.evaluate(&path("\\PCI0.ISA0.PRQ0"), &[]),
        Ok(Value::Integer(11))
    );
    for flags in [0, 2, 0x11] {
        let bytes = pci_field_fixture(adr.clone(), flags);
        let mut ns = Namespace::new();
        ns.set_field_reader(&Reader);
        ns.load(&bytes).unwrap();
        assert!(ns.evaluate(&path("\\PCI0.ISA0.PRQ0"), &[]).is_err());
    }
    // Metadata method recursion must be refused, not restart the interpreter budget.
    let bytes = pci_field_fixture(method(b"_ADR", 0, &[0xa4, b'P', b'R', b'Q', b'0']), 1);
    let mut ns = Namespace::new();
    ns.set_field_reader(&Reader);
    ns.load(&bytes).unwrap();
    assert!(ns.evaluate(&path("\\PCI0.ISA0.PRQ0"), &[]).is_err());
}

#[test]
fn legacy_qemu_prt_builds_128_owned_rows_with_local_index_stores_and_increment() {
    let mut names = Vec::new();
    for link in [b"LNKA", b"LNKB", b"LNKC", b"LNKD", b"LNKS"] {
        names.extend(pkg(&[0x5b, 0x82], link.to_vec()));
    }
    fn route(link: &[u8; 4]) -> Vec<u8> {
        pkg(&[0x12], [vec![4, 0, 0], link.to_vec(), vec![0]].concat())
    }
    fn choose(link: &[u8; 4], index: u8) -> Vec<u8> {
        pkg(
            &[0xa0],
            [vec![0x93, 0x63, 0x0a, index, 0x70], route(link), vec![0x64]].concat(),
        )
    }
    let mut loop_body = vec![
        0x95, 0x61, 0x0a, 128, 0x70, 0x7a, 0x61, 0x0a, 2, 0, 0x62, 0x70, 0x7b, 0x72, 0x61, 0x62, 0,
        0x0a, 3, 0, 0x63,
    ];
    loop_body.extend(choose(b"LNKD", 0));
    let special = [
        pkg(
            &[0xa0],
            [vec![0x93, 0x61, 0x0a, 4, 0x70], route(b"LNKS"), vec![0x64]].concat(),
        ),
        pkg(&[0xa1], [vec![0x70], route(b"LNKA"), vec![0x64]].concat()),
    ]
    .concat();
    loop_body.extend(pkg(&[0xa0], [vec![0x93, 0x63, 1], special].concat()));
    loop_body.extend(choose(b"LNKB", 2));
    loop_body.extend(choose(b"LNKC", 3));
    loop_body.extend([
        0x70, 0x7d, 0x79, 0x62, 0x0a, 16, 0, 0x0b, 0xff, 0xff, 0, 0x88, 0x64, 0, 0,
    ]);
    loop_body.extend([0x70, 0x7b, 0x61, 0x0a, 3, 0, 0x88, 0x64, 1, 0]);
    loop_body.extend([0x70, 0x64, 0x88, 0x60, 0x61, 0, 0x75, 0x61]);
    let body = [
        vec![0x70],
        pkg(&[0x12], vec![128]),
        vec![0x60, 0x70, 0, 0x61],
        pkg(&[0xa2], loop_body),
        vec![0xa4, 0x60],
    ]
    .concat();
    let bytes = [names, method(b"_PRT", 0, &body)].concat();
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    let Value::Package(rows) = ns.evaluate(&path("\\_PRT"), &[]).unwrap() else {
        panic!()
    };
    assert_eq!(rows.len(), 128);
    for (i, row) in rows.iter().enumerate() {
        let Value::Package(row) = row else { panic!() };
        assert_eq!(row[0], Value::Integer(((i / 4) as u64) << 16 | 0xffff));
        assert_eq!(row[1], Value::Integer((i % 4) as u64));
        let link = if i == 4 {
            "\\LNKS"
        } else {
            ["\\LNKD", "\\LNKA", "\\LNKB", "\\LNKC"][(i + i / 4) % 4]
        };
        assert_eq!(row[2], Value::Reference(path(link)));
    }
}
#[test]
fn package_index_writes_are_local_bounded_copies_never_namespace_or_buffer_targets() {
    let package = pkg(&[0x12], vec![1, 0]);
    for target in [
        vec![0x88, 0x60, 1, 0],
        vec![0x88, 0x60, 0, 0x61],
        vec![0x88, 0x68, 0, 0],
        vec![0x88, b'P', b'K', b'G', b'0', 0, 0],
    ] {
        let bytes = method(
            b"TEST",
            0,
            &[
                vec![0x70],
                package.clone(),
                vec![0x60, 0x70, 1],
                target,
                vec![0xa4, 0x60],
            ]
            .concat(),
        );
        let mut ns = Namespace::new();
        ns.load(&bytes).unwrap();
        assert!(ns.evaluate(&path("\\TEST"), &[]).is_err());
    }
    // Deep self-copy cannot evade aggregate/depth budgets through Index targets.
    let body = [
        vec![0x70],
        package,
        vec![0x60],
        pkg(&[0xa2], vec![1, 0x70, 0x60, 0x88, 0x60, 0, 0]),
        vec![0xa4, 0x60],
    ]
    .concat();
    let bytes = method(b"GROW", 0, &body);
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    assert_eq!(ns.evaluate(&path("\\GROW"), &[]), Err(Error::Limit));
}
