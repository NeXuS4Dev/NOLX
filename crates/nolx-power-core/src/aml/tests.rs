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
    let aml = pkg(&[0x5b, 0x82], device);
    namespace.load(&aml).unwrap();
    let dev = path("\\DEV0");
    assert_eq!(namespace.device_method_arity(&dev, *b"_SRS"), Ok(Some(1)));
    assert_eq!(namespace.device_method_arity(&dev, *b"_STA"), Ok(None));
    assert_eq!(
        namespace.device_method_arity(&dev, *b"_CRS"),
        Err(Error::Type)
    );
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

#[derive(Default)]
struct TestPciWriteBackend {
    values: Vec<(PciField, u8)>,
    writes: Vec<(PciField, u8)>,
    reads: usize,
    fail_before_write: Option<usize>,
    fail_after_write: Option<usize>,
    override_write: Option<(usize, u8)>,
    mutate_on_read: Option<(usize, PciField, u8)>,
}
impl PciFieldWriteBackend for TestPciWriteBackend {
    type Error = &'static str;

    fn read_byte(&mut self, field: PciField) -> Result<u8, Self::Error> {
        self.reads += 1;
        if let Some((at, changed_field, value)) = self.mutate_on_read {
            if at == self.reads {
                let (_, current) = self
                    .values
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == changed_field)
                    .ok_or("missing field")?;
                *current = value;
            }
        }
        self.values
            .iter()
            .find(|(candidate, _)| *candidate == field)
            .map(|(_, value)| *value)
            .ok_or("missing field")
    }

    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), Self::Error> {
        let attempt = self.writes.len() + 1;
        self.writes.push((field, value));
        if self.fail_before_write == Some(attempt) {
            return Err("write refused");
        }
        let stored = self
            .override_write
            .filter(|(at, _)| *at == attempt)
            .map_or(value, |(_, stored)| stored);
        let (_, current) = self
            .values
            .iter_mut()
            .find(|(candidate, _)| *candidate == field)
            .ok_or("missing field")?;
        *current = stored;
        if self.fail_after_write == Some(attempt) {
            return Err("write uncertain");
        }
        Ok(())
    }
}

fn transaction_field(offset: u16) -> PciField {
    PciField {
        address: 0x0001_0000,
        offset,
        bytes: 1,
    }
}

fn transaction_effects(reads: &[(PciField, u8)], writes: &[(PciField, u8)]) -> MethodEffects {
    MethodEffects {
        result: None,
        reads: reads
            .iter()
            .map(|(field, value)| PciFieldRead {
                field: *field,
                value: u64::from(*value),
            })
            .collect(),
        writes: writes
            .iter()
            .map(|(field, value)| PciFieldWrite {
                field: *field,
                value: u64::from(*value),
            })
            .collect(),
        region_reads: Vec::new(),
        region_writes: Vec::new(),
        device_writes: Vec::new(),
    }
}

fn transaction_permission(field: PciField, values: &[u8]) -> PciFieldWritePermission {
    PciFieldWritePermission::from_values(field, values).unwrap()
}

#[test]
fn pci_write_transaction_checks_snapshots_and_commits_ordered_readback_verified_writes() {
    let pirqa = transaction_field(0x60);
    let pirqb = transaction_field(0x61);
    let effects = transaction_effects(&[(pirqa, 1)], &[(pirqa, 2), (pirqa, 3), (pirqb, 4)]);
    let permissions = [
        transaction_permission(pirqa, &[2, 3]),
        transaction_permission(pirqb, &[4]),
    ];
    let mut backend = TestPciWriteBackend {
        values: vec![(pirqa, 1), (pirqb, 0)],
        ..TestPciWriteBackend::default()
    };
    let mut postcondition_ran = false;
    let result = apply_pci_field_writes(&effects, &permissions, &mut backend, || {
        postcondition_ran = true;
        Ok::<(), &'static str>(())
    });
    assert_eq!(result, Ok(()));
    assert!(postcondition_ran);
    assert_eq!(backend.values, vec![(pirqa, 3), (pirqb, 4)]);
    assert_eq!(backend.writes, vec![(pirqa, 2), (pirqa, 3), (pirqb, 4)]);
}

#[test]
fn pci_write_transaction_refuses_stale_snapshot_before_any_write() {
    let field = transaction_field(0x60);
    let effects = transaction_effects(&[(field, 1)], &[(field, 2)]);
    let permission = transaction_permission(field, &[2]);
    let mut backend = TestPciWriteBackend {
        values: vec![(field, 9)],
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut backend, || {
            Ok::<(), &'static str>(())
        }),
        Err(PciFieldWriteTransactionError::StaleSnapshot)
    );
    assert!(backend.writes.is_empty());
    assert_eq!(backend.values, vec![(field, 9)]);
}

#[test]
fn pci_write_transaction_rolls_back_if_a_planned_field_changes_mid_apply() {
    let first = transaction_field(0x60);
    let second = transaction_field(0x61);
    let effects = transaction_effects(&[(second, 1)], &[(first, 2), (second, 3)]);
    let permissions = [
        transaction_permission(first, &[2]),
        transaction_permission(second, &[3]),
    ];
    let mut backend = TestPciWriteBackend {
        values: vec![(first, 1), (second, 1)],
        // Snapshot read, first write pre-read, first readback, second pre-read.
        mutate_on_read: Some((4, second, 9)),
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &permissions, &mut backend, || {
            Ok::<(), &'static str>(())
        }),
        Err(PciFieldWriteTransactionError::ChangedDuringApply)
    );
    assert_eq!(backend.values, vec![(first, 1), (second, 9)]);
    assert_eq!(backend.writes, vec![(first, 2), (first, 1)]);
}

#[test]
fn pci_write_transaction_prevalidates_all_write_permissions_without_io() {
    let allowed = transaction_field(0x60);
    let denied = transaction_field(0x61);
    let effects = transaction_effects(&[], &[(allowed, 2), (denied, 3)]);
    let permission = transaction_permission(allowed, &[2]);
    let mut backend = TestPciWriteBackend {
        values: vec![(allowed, 1), (denied, 1)],
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut backend, || {
            Ok::<(), &'static str>(())
        }),
        Err(PciFieldWriteTransactionError::PermissionDenied)
    );
    assert_eq!(backend.reads, 0);
    assert!(backend.writes.is_empty());
}

#[test]
fn pci_write_transaction_rolls_back_an_uncertain_failed_write() {
    let field = transaction_field(0x60);
    let effects = transaction_effects(&[], &[(field, 2)]);
    let permission = transaction_permission(field, &[2]);
    let mut backend = TestPciWriteBackend {
        values: vec![(field, 1)],
        fail_after_write: Some(1),
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut backend, || {
            Ok::<(), &'static str>(())
        }),
        Err(PciFieldWriteTransactionError::Write("write uncertain"))
    );
    assert_eq!(backend.values, vec![(field, 1)]);
    assert_eq!(backend.writes, vec![(field, 2), (field, 1)]);
}

#[test]
fn pci_write_transaction_rolls_back_readback_and_postcondition_failures() {
    let field = transaction_field(0x60);
    let effects = transaction_effects(&[], &[(field, 2)]);
    let permission = transaction_permission(field, &[2]);
    let mut bad_readback = TestPciWriteBackend {
        values: vec![(field, 1)],
        override_write: Some((1, 99)),
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut bad_readback, || {
            Ok::<(), &'static str>(())
        }),
        Err(PciFieldWriteTransactionError::ReadbackMismatch)
    );
    assert_eq!(bad_readback.values, vec![(field, 1)]);

    let mut bad_postcondition = TestPciWriteBackend {
        values: vec![(field, 1)],
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut bad_postcondition, || {
            Err::<(), &'static str>("_CRS mismatch")
        }),
        Err(PciFieldWriteTransactionError::Postcondition(
            "_CRS mismatch"
        ))
    );
    assert_eq!(bad_postcondition.values, vec![(field, 1)]);
    assert_eq!(bad_postcondition.writes, vec![(field, 2), (field, 1)]);
}

#[test]
fn pci_write_transaction_reports_unverified_rollback() {
    let field = transaction_field(0x60);
    let effects = transaction_effects(&[], &[(field, 2)]);
    let permission = transaction_permission(field, &[2]);
    let mut backend = TestPciWriteBackend {
        values: vec![(field, 1)],
        fail_before_write: Some(2),
        ..TestPciWriteBackend::default()
    };
    assert_eq!(
        apply_pci_field_writes(&effects, &[permission], &mut backend, || {
            Err::<(), &'static str>("_CRS mismatch")
        }),
        Err(PciFieldWriteTransactionError::RollbackFailed)
    );
    assert_eq!(backend.values, vec![(field, 2)]);
}

/// An `OperationRegion` plus a `Field` over it, with the elements' bit widths chosen by name.
fn region_fixture(
    space: u8,
    base: u32,
    length: u16,
    flags: u8,
    elements: &[(&[u8; 4], u16)],
) -> Vec<u8> {
    let mut out = vec![0x5b, 0x80];
    out.extend_from_slice(b"MEMR");
    out.push(space);
    out.extend_from_slice(&[0x0c]);
    out.extend_from_slice(&base.to_le_bytes());
    out.extend_from_slice(&[0x0b]);
    out.extend_from_slice(&length.to_le_bytes());
    let mut body = b"MEMR".to_vec();
    body.push(flags);
    for (name, bits) in elements {
        body.extend_from_slice(*name);
        if *bits < 0x40 {
            body.push(*bits as u8);
        } else {
            body.extend_from_slice(&[0x40 | (*bits & 15) as u8, (*bits >> 4) as u8]);
        }
    }
    out.extend(pkg(&[0x5b, 0x81], body));
    out
}

/// A provider over a fixed byte image, remembering the spaces it was asked for.
struct Region {
    base: u64,
    bytes: &'static [u8],
    space: std::cell::Cell<Option<RegionSpace>>,
}

impl RegionFieldReader for Region {
    fn read(&self, space: RegionSpace, address: u64, bytes: u8) -> Result<u64, Error> {
        self.space.set(Some(space));
        let offset = address.checked_sub(self.base).ok_or(Error::Limit)? as usize;
        let unit = self
            .bytes
            .get(offset..offset + usize::from(bytes))
            .ok_or(Error::Limit)?;
        Ok(unit
            .iter()
            .rev()
            .fold(0u64, |v, b| (v << 8) | u64::from(*b)))
    }
}

#[test]
fn system_memory_region_fields_read_through_a_provider() {
    // The shape QEMU's HPET device uses: a SystemMemory region over the HPET page, a DWordAcc
    // field, and a _STA method that reads it.
    let mut device = b"HPET".to_vec();
    device.extend(region_fixture(
        0,
        0xFED0_0000,
        0x0400,
        0x03,
        &[(b"VEND", 32), (b"PRD_", 32)],
    ));
    device.extend(method(b"_STA", 0, &[0xa4, b'V', b'E', b'N', b'D']));
    device.extend(method(b"_UID", 0, &[0xa4, b'P', b'R', b'D', b'_']));
    let bytes = pkg(&[0x5b, 0x82], device);
    let provider = Region {
        base: 0xFED0_0000,
        bytes: &[0x34, 0x12, 0x86, 0x80, 0x00, 0x00, 0x04, 0x00],
        space: std::cell::Cell::new(None),
    };
    let mut ns = Namespace::new();
    ns.load(&bytes).unwrap();
    // Without a provider the field is still refused, exactly as before.
    assert_eq!(
        ns.evaluate(&path("\\HPET._STA"), &[]),
        Err(Error::Unsupported(0x5b81))
    );
    ns.set_region_reader(&provider);
    assert_eq!(
        ns.evaluate(&path("\\HPET._STA"), &[]),
        Ok(Value::Integer(0x8086_1234))
    );
    assert_eq!(
        ns.evaluate(&path("\\HPET._UID"), &[]),
        Ok(Value::Integer(0x0004_0000))
    );
    assert_eq!(provider.space.get(), Some(RegionSpace::SystemMemory));
    // The read is journaled with the field the provider saw, so a caller can audit what a
    // method looked at before it trusted the result.
    let effects = ns
        .plan_device_method(&path("\\HPET"), *b"_STA", &[], &[])
        .unwrap();
    assert_eq!(effects.result, Some(Value::Integer(0x8086_1234)));
    assert_eq!(
        effects.region_reads,
        [RegionFieldRead {
            field: RegionField {
                space: RegionSpace::SystemMemory,
                address: 0xFED0_0000,
                bits: 32,
                shift: 0,
                access: 4
            },
            value: 0x8086_1234
        }]
    );
}

#[test]
fn region_fields_are_bounded_aligned_and_never_written() {
    // A field that runs past the region's end is refused before a provider sees an address.
    let mut device = b"OVRN".to_vec();
    device.extend(region_fixture(
        0,
        0x1000,
        4,
        0x03,
        &[(b"RSVD", 32), (b"NEXT", 32)],
    ));
    device.extend(method(b"_STA", 0, &[0xa4, b'N', b'E', b'X', b'T']));
    let bytes = pkg(&[0x5b, 0x82], device);
    let provider = Region {
        base: 0x1000,
        bytes: &[0; 8],
        space: std::cell::Cell::new(None),
    };
    let mut ns = Namespace::new();
    ns.set_region_reader(&provider);
    ns.load(&bytes).unwrap();
    assert_eq!(ns.evaluate(&path("\\OVRN._STA"), &[]), Err(Error::Limit));

    // A DWord field that does not start on a DWord boundary is not something to read as one.
    let mut device = b"ALGN".to_vec();
    device.extend(region_fixture(0, 0x1002, 0x10, 0x03, &[(b"WORD", 32)]));
    device.extend(method(b"_STA", 0, &[0xa4, b'W', b'O', b'R', b'D']));
    let bytes = pkg(&[0x5b, 0x82], device);
    let mut ns = Namespace::new();
    ns.set_region_reader(&provider);
    ns.load(&bytes).unwrap();
    assert_eq!(
        ns.evaluate(&path("\\ALGN._STA"), &[]),
        Err(Error::Unsupported(0x5b81))
    );
}

#[test]
fn byte_access_composes_units_and_extracts_a_shifted_field() {
    // AnyAcc with a field that starts four bits in: two byte reads, then shift and mask.
    let mut device = b"NIBL".to_vec();
    device.extend(region_fixture(
        0,
        0x2000,
        0x10,
        0x00,
        &[(b"RSVD", 4), (b"LOW_", 8)],
    ));
    device.extend(method(b"_STA", 0, &[0xa4, b'L', b'O', b'W', b'_']));
    let bytes = pkg(&[0x5b, 0x82], device);
    let provider = Region {
        base: 0x2000,
        bytes: &[0xAB, 0xCD, 0xEF, 0x01],
        space: std::cell::Cell::new(None),
    };
    let mut ns = Namespace::new();
    ns.set_region_reader(&provider);
    ns.load(&bytes).unwrap();
    // Raw little-endian 0xCDAB >> 4 = 0xCDA, masked to eight bits = 0xDA.
    assert_eq!(
        ns.evaluate(&path("\\NIBL._STA"), &[]),
        Ok(Value::Integer(0xDA))
    );

    // The same field through a Device method plan, so the journal shows the byte unit width.
    let effects = ns
        .plan_device_method(&path("\\NIBL"), *b"_STA", &[], &[])
        .unwrap();
    assert_eq!(effects.region_reads.len(), 1);
    assert_eq!(effects.region_reads[0].field.access, 1);
    assert_eq!(effects.region_reads[0].field.shift, 4);
}

#[test]
fn writing_a_region_field_needs_an_exact_permission_and_is_journaled() {
    let mut device = b"WRIT".to_vec();
    device.extend(region_fixture(0, 0x3000, 0x10, 0x03, &[(b"VEND", 32)]));
    device.extend(method(
        b"_SRS",
        0,
        &[0x70, 0x0a, 0x01, b'V', b'E', b'N', b'D'],
    ));
    let bytes = pkg(&[0x5b, 0x82], device);
    let provider = Region {
        base: 0x3000,
        bytes: &[0; 4],
        space: std::cell::Cell::new(None),
    };
    let mut ns = Namespace::new();
    ns.set_region_reader(&provider);
    ns.load(&bytes).unwrap();
    // Without a permission the write is refused, not silently dropped.
    assert_eq!(
        ns.plan_device_method(&path("\\WRIT"), *b"_SRS", &[], &[]),
        Err(Error::WriteDenied)
    );
    // A permission for a different value does not authorize this one.
    let field = RegionField {
        space: RegionSpace::SystemMemory,
        address: 0x3000,
        bits: 32,
        shift: 0,
        access: 4,
    };
    let wrong = RegionFieldWritePermission::from_values(field, &[7]).unwrap();
    assert_eq!(
        ns.plan_device_method_with_regions(&path("\\WRIT"), *b"_SRS", &[], &[], &[wrong]),
        Err(Error::WriteDenied)
    );
    // The exact value is journaled as an intent; hardware is still untouched.
    let allowed = RegionFieldWritePermission::from_values(field, &[1]).unwrap();
    let effects = ns
        .plan_device_method_with_regions(&path("\\WRIT"), *b"_SRS", &[], &[], &[allowed])
        .unwrap();
    assert_eq!(
        effects.region_writes,
        [RegionFieldWrite { field, value: 1 }]
    );

    // An embedded-controller region reaches the provider as its own space; the kernel has no
    // controller, so the provider's refusal is what the method reports.
    let mut device = b"ECTL".to_vec();
    device.extend(region_fixture(3, 0x0062, 0x01, 0x01, &[(b"ECFT", 8)]));
    device.extend(method(b"_STA", 0, &[0xa4, b'E', b'C', b'F', b'T']));
    let bytes = pkg(&[0x5b, 0x82], device);
    struct Refuse;
    impl RegionFieldReader for Refuse {
        fn read(&self, space: RegionSpace, _: u64, _: u8) -> Result<u64, Error> {
            assert_eq!(space, RegionSpace::EmbeddedControl);
            Err(Error::Unsupported(0x5b80))
        }
    }
    let mut ns = Namespace::new();
    ns.set_region_reader(&Refuse);
    ns.load(&bytes).unwrap();
    assert_eq!(
        ns.evaluate(&path("\\ECTL._STA"), &[]),
        Err(Error::Unsupported(0x5b80))
    );
}

/// A backend over one four-byte register that refuses a chosen write.
struct Register {
    value: u32,
    refuse: bool,
    writes: usize,
}

impl RegionFieldWriteBackend for Register {
    type Error = ();

    fn read_field(&mut self, _: RegionField) -> Result<u64, ()> {
        Ok(u64::from(self.value))
    }

    fn write_field(&mut self, _: RegionField, value: u64) -> Result<(), ()> {
        if self.refuse {
            return Err(());
        }
        self.writes += 1;
        self.value = value as u32;
        Ok(())
    }
}

#[test]
fn a_region_write_transaction_applies_verifies_and_rolls_back() {
    let field = RegionField {
        space: RegionSpace::SystemMemory,
        address: 0x3000,
        bits: 32,
        shift: 0,
        access: 4,
    };
    let permission = RegionFieldWritePermission::from_values(field, &[1]).unwrap();
    let effects = MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: vec![RegionFieldWrite { field, value: 1 }],
        device_writes: Vec::new(),
    };
    let mut register = Register {
        value: 0,
        refuse: false,
        writes: 0,
    };
    assert_eq!(
        apply_region_field_writes(&effects, &[permission], &mut register),
        Ok(())
    );
    assert_eq!((register.value, register.writes), (1, 1));

    // Without a matching permission nothing is written at all.
    let denied = RegionFieldWritePermission::from_values(field, &[9]).unwrap();
    let mut register = Register {
        value: 0,
        refuse: false,
        writes: 0,
    };
    assert_eq!(
        apply_region_field_writes(&effects, &[denied], &mut register),
        Err(RegionWriteTransactionError::PermissionDenied)
    );
    assert_eq!((register.value, register.writes), (0, 0));

    // A refused write is rolled back and reported, and a duplicate journal entry is refused
    // before the first backend operation.
    let mut register = Register {
        value: 0,
        refuse: true,
        writes: 0,
    };
    assert_eq!(
        apply_region_field_writes(&effects, &[permission], &mut register),
        Err(RegionWriteTransactionError::Write(()))
    );
    let mut twice = effects.clone();
    twice
        .region_writes
        .push(RegionFieldWrite { field, value: 1 });
    let mut register = Register {
        value: 0,
        refuse: false,
        writes: 0,
    };
    assert_eq!(
        apply_region_field_writes(&twice, &[permission], &mut register),
        Err(RegionWriteTransactionError::DuplicateField)
    );
    assert_eq!(register.writes, 0);
}

// ---------------------------------------------------------------- PCI configuration regions
//
// `_PS0`/`_PS3` program a device's own power register, which lives in its PCI configuration
// space. These tests are the portable half of that path: the field resolution, the width, the
// scoping to the one function the device's `_ADR` names, and the ordered journal the kernel
// applies to hardware.

/// A PCI root bridge holding one function device, its `PCI_Config` region and field, and a
/// pair of power methods built by `body`.
fn power_function_fixture(adr: Option<u32>, flags: u8, width: u8, body: &[u8]) -> Vec<u8> {
    // OperationRegion (PCFG, PCI_Config, 0, 0x100)
    // PCI_Config region at configuration offset 0x40, 0x20 bytes long. The field's own offset
    // is relative to the region base, which is how firmware spells "this device's PM register".
    let region = [
        vec![0x5b, 0x80],
        b"PCFG".to_vec(),
        vec![2, 0x0a, 0x40, 0x0a, 0x20],
    ]
    .concat();
    let mut entries = b"PMCS".to_vec();
    entries.push(width);
    let field = pkg(
        &[0x5b, 0x81],
        [b"PCFG".to_vec(), vec![flags], entries].concat(),
    );
    let mut device = b"DEV0".to_vec();
    if let Some(adr) = adr {
        let mut adr_bytes = vec![8];
        adr_bytes.extend(b"_ADR");
        adr_bytes.push(0x0c);
        adr_bytes.extend_from_slice(&adr.to_le_bytes());
        device.extend(adr_bytes);
    }
    device.extend(region);
    device.extend(field);
    device.extend(method(b"_PS3", 0, body));
    device.extend(method(b"_PS0", 0, body));
    let child = pkg(&[0x5b, 0x82], device);
    let mut root = b"PCI0".to_vec();
    root.extend_from_slice(&[8]);
    root.extend(b"_HID");
    root.extend_from_slice(&[0x0d]);
    root.extend_from_slice(b"PNP0A03\0");
    root.extend(child);
    pkg(&[0x5b, 0x82], root)
}

struct WideFieldReader;
impl FieldReader for WideFieldReader {
    fn read(&self, field: PciField) -> Result<u64, Error> {
        match field {
            PciField {
                address: 0x0003_0000,
                offset: 0x40,
                bytes: 2,
            } => Ok(0x8086),
            PciField {
                address: 0x0004_0000,
                offset: 0x40,
                bytes: 2,
            } => Ok(0x1234),
            _ => Err(Error::Unsupported(0x5b81)),
        }
    }
}

/// `Store (PMCS, Local0); Store (Local0, PMCS)`: a real read-modify-write of the field.
const RMW_BODY: &[u8] = &[
    0x70, b'P', b'M', b'C', b'S', 0x60, 0x70, 0x60, b'P', b'M', b'C', b'S',
];

#[test]
fn a_wide_pci_configuration_field_is_read_whole_and_written_byte_by_byte() {
    let fixture = power_function_fixture(Some(0x0003_0000), 2, 16, RMW_BODY);
    let mut ns = Namespace::new();
    ns.set_field_reader(&WideFieldReader);
    ns.load(&fixture).unwrap();
    let device = path("\\PCI0.DEV0");
    assert_eq!(
        ns.device_pci_scope(&device),
        Ok(Some(PciConfigScope {
            address: 0x0003_0000
        }))
    );
    let effects = ns
        .plan_device_method_authorized(&device, *b"_PS3", &[])
        .unwrap()
        .unwrap();
    // The provider saw one 16-bit access, not two byte accesses.
    assert_eq!(
        effects.reads,
        vec![PciFieldRead {
            field: PciField {
                address: 0x0003_0000,
                offset: 0x40,
                bytes: 2,
            },
            value: 0x8086,
        }]
    );
    // The store is split into the two configuration bytes a transaction can verify.
    assert_eq!(
        effects.writes,
        vec![
            PciFieldWrite {
                field: PciField {
                    address: 0x0003_0000,
                    offset: 0x40,
                    bytes: 1,
                },
                value: 0x86,
            },
            PciFieldWrite {
                field: PciField {
                    address: 0x0003_0000,
                    offset: 0x41,
                    bytes: 1,
                },
                value: 0x80,
            },
        ]
    );
    assert_eq!(
        effects.device_writes,
        effects
            .writes
            .iter()
            .map(|write| DeviceWrite::Pci(*write))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_power_method_may_not_write_another_functions_configuration_space() {
    // SIB0 is a different function. DEV0's `_PS3` names SIB0's field by absolute path, which is
    // exactly the shape a hostile or broken table would use to power down somebody else's
    // device during a system sleep.
    let mut device = b"DEV0".to_vec();
    device.extend_from_slice(&[8]);
    device.extend(b"_ADR");
    device.push(0x0c);
    device.extend_from_slice(&0x0003_0000u32.to_le_bytes());
    device.extend_from_slice(&method(
        b"_PS3",
        0,
        &[
            // Store (3, \PCI0.SIB0.PMCS): root prefix, multi-name prefix, three segments.
            0x70, 0x0a, 0x03, b'\\', 0x2f, 0x03, b'P', b'C', b'I', b'0', b'S', b'I', b'B', b'0',
            b'P', b'M', b'C', b'S',
        ],
    ));
    let mut sibling = b"SIB0".to_vec();
    sibling.extend_from_slice(&[8]);
    sibling.extend(b"_ADR");
    sibling.push(0x0c);
    sibling.extend_from_slice(&0x0004_0000u32.to_le_bytes());
    sibling.extend_from_slice(
        &[
            vec![0x5b, 0x80],
            b"PCFG".to_vec(),
            vec![2, 0x0a, 0x40, 0x0a, 0x20],
        ]
        .concat(),
    );
    let mut entries = b"PMCS".to_vec();
    entries.push(16);
    sibling.extend(pkg(
        &[0x5b, 0x81],
        [b"PCFG".to_vec(), vec![2], entries].concat(),
    ));
    let mut root = b"PCI0".to_vec();
    root.extend_from_slice(&[8]);
    root.extend(b"_HID");
    root.extend_from_slice(&[0x0d]);
    root.extend_from_slice(b"PNP0A03\0");
    root.extend(pkg(&[0x5b, 0x82], device));
    root.extend(pkg(&[0x5b, 0x82], sibling));
    let table = pkg(&[0x5b, 0x82], root);
    let mut ns = Namespace::new();
    ns.set_field_reader(&WideFieldReader);
    ns.load(&table).unwrap();
    assert_eq!(
        ns.plan_device_method_authorized(&path("\\PCI0.DEV0"), *b"_PS3", &[]),
        Err(Error::WriteDenied)
    );
}

#[test]
fn a_device_without_adr_has_no_configuration_scope_and_cannot_resolve_the_field() {
    let fixture = power_function_fixture(None, 2, 16, RMW_BODY);
    let mut ns = Namespace::new();
    ns.set_field_reader(&WideFieldReader);
    ns.load(&fixture).unwrap();
    let device = path("\\PCI0.DEV0");
    assert_eq!(ns.device_pci_scope(&device), Ok(None));
    // The field's owner declares no `_ADR`, so the address is unknown rather than zero.
    assert_eq!(
        ns.plan_device_method_authorized(&device, *b"_PS3", &[]),
        Err(Error::Missing)
    );
}

#[test]
fn a_configuration_field_wider_than_its_access_rule_is_refused() {
    for (flags, width) in [(2u8, 8u8), (3, 16), (1, 16), (0, 8), (0x11, 8)] {
        let fixture = power_function_fixture(Some(0x0003_0000), flags, width, RMW_BODY);
        let mut ns = Namespace::new();
        ns.set_field_reader(&WideFieldReader);
        ns.load(&fixture).unwrap();
        assert_eq!(
            ns.plan_device_method_authorized(&path("\\PCI0.DEV0"), *b"_PS3", &[]),
            Err(Error::Unsupported(0x5b81)),
            "flags {flags:#x} width {width}"
        );
    }
}

/// A backend that answers both halves of a device power journal and records the order it saw.
struct PowerBackend {
    region: u64,
    second_region: u64,
    pci: [u8; 256],
    log: Vec<&'static str>,
    fail_pci_write: bool,
    fail_pci_readback: bool,
}

impl PowerBackend {
    fn new(region: u64, pci: [u8; 256]) -> Self {
        Self {
            region,
            second_region: 0,
            pci,
            log: Vec::new(),
            fail_pci_write: false,
            fail_pci_readback: false,
        }
    }
}

impl RegionFieldWriteBackend for PowerBackend {
    type Error = Error;
    fn read_field(&mut self, field: RegionField) -> Result<u64, Error> {
        self.log.push("region read");
        if field.address == 0x30 {
            Ok(self.region)
        } else {
            Ok(self.second_region)
        }
    }
    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Error> {
        self.log.push("region write");
        if field.address == 0x30 {
            self.region = value;
        } else {
            self.second_region = value;
        }
        Ok(())
    }
}

impl PciFieldWriteBackend for PowerBackend {
    type Error = Error;
    fn read_byte(&mut self, field: PciField) -> Result<u8, Error> {
        self.log.push("pci read");
        Ok(self.pci[usize::from(field.offset)])
    }
    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), Error> {
        self.log.push("pci write");
        if self.fail_pci_write {
            return Err(Error::Unsupported(0x5b84));
        }
        if !self.fail_pci_readback {
            self.pci[usize::from(field.offset)] = value;
        }
        Ok(())
    }
}

fn power_region_write(address: u64, value: u64) -> DeviceWrite {
    DeviceWrite::Region(RegionFieldWrite {
        field: RegionField {
            space: RegionSpace::SystemMemory,
            address,
            bits: 32,
            shift: 0,
            access: 4,
        },
        value,
    })
}

fn power_pci_write(offset: u16, value: u64) -> DeviceWrite {
    DeviceWrite::Pci(PciFieldWrite {
        field: PciField {
            address: 0x0003_0000,
            offset,
            bytes: 1,
        },
        value,
    })
}

#[test]
fn the_device_power_journal_replays_region_and_configuration_writes_in_program_order() {
    let scope = PciConfigScope {
        address: 0x0003_0000,
    };
    let effects = MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: Vec::new(),
        device_writes: vec![
            power_region_write(0x30, 0x1122_3344),
            power_pci_write(0x40, 0x86),
            power_pci_write(0x41, 0x80),
            power_region_write(0x38, 0x5566_7788),
        ],
    };
    let mut backend = PowerBackend::new(0, [0; 256]);
    assert_eq!(
        apply_device_power_writes(&effects, Some(scope), &mut backend),
        Ok(())
    );
    // Each entry is read before it is written and read back after: the verification is what
    // makes a resumed device provable rather than assumed.
    assert_eq!(
        backend.log,
        vec![
            "region read",
            "region write",
            "region read",
            "pci read",
            "pci write",
            "pci read",
            "pci read",
            "pci write",
            "pci read",
            "region read",
            "region write",
            "region read",
        ]
    );
    assert_eq!(backend.region, 0x1122_3344);
    assert_eq!(backend.second_region, 0x5566_7788);
    assert_eq!(backend.pci[0x40], 0x86);
    assert_eq!(backend.pci[0x41], 0x80);
}

#[test]
fn the_device_power_journal_refuses_out_of_scope_duplicate_and_malformed_writes() {
    let scope = PciConfigScope {
        address: 0x0003_0000,
    };
    let base = MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: Vec::new(),
        device_writes: vec![power_pci_write(0x40, 0x86)],
    };
    let mut backend = PowerBackend::new(0, [0; 256]);
    // The scope the caller applies with is not the one the journal was built for.
    assert_eq!(
        apply_device_power_writes(
            &base,
            Some(PciConfigScope { address: 0x40000 }),
            &mut backend
        ),
        Err(DevicePowerWriteError::OutOfScope)
    );
    // No scope at all: a device that is not a PCI function cannot write configuration space.
    assert_eq!(
        apply_device_power_writes(&base, None, &mut backend),
        Err(DevicePowerWriteError::OutOfScope)
    );
    let mut duplicate = base.clone();
    duplicate.device_writes.push(power_pci_write(0x40, 0x00));
    assert_eq!(
        apply_device_power_writes(&duplicate, Some(scope), &mut backend),
        Err(DevicePowerWriteError::DuplicateField)
    );
    let mut wide = base.clone();
    wide.device_writes[0] = DeviceWrite::Pci(PciFieldWrite {
        field: PciField {
            address: 0x0003_0000,
            offset: 0x40,
            bytes: 2,
        },
        value: 0x8086,
    });
    assert_eq!(
        apply_device_power_writes(&wide, Some(scope), &mut backend),
        Err(DevicePowerWriteError::InvalidField)
    );
    // Nothing above reached the device.
    assert_eq!(backend.log, Vec::<&'static str>::new());
}

#[test]
fn a_refused_configuration_write_restores_the_region_write_before_it() {
    let scope = PciConfigScope {
        address: 0x0003_0000,
    };
    let effects = MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: Vec::new(),
        device_writes: vec![
            power_region_write(0x30, 0x1122_3344),
            power_pci_write(0x40, 0x86),
            power_pci_write(0x41, 0x80),
        ],
    };
    let mut backend = PowerBackend::new(0xdead_beef, [0; 256]);
    backend.fail_pci_readback = true;
    assert_eq!(
        apply_device_power_writes(&effects, Some(scope), &mut backend),
        Err(DevicePowerWriteError::ReadbackMismatch)
    );
    // The region write was rolled back and the attempted bytes were restored in reverse order.
    assert_eq!(backend.region, 0xdead_beef);
    assert_eq!(backend.pci[0x40], 0);
    assert_eq!(backend.pci[0x41], 0);
    // Rollback is reverse program order, so the last operation is the verification of the
    // restored memory-mapped register -- the first write the transaction attempted.
    assert_eq!(backend.log.last().copied(), Some("region read"));
}

#[test]
fn a_failed_configuration_write_is_reported_without_leaving_the_region_changed() {
    let scope = PciConfigScope {
        address: 0x0003_0000,
    };
    let effects = MethodEffects {
        result: None,
        reads: Vec::new(),
        writes: Vec::new(),
        region_reads: Vec::new(),
        region_writes: Vec::new(),
        device_writes: vec![power_region_write(0x30, 7), power_pci_write(0x40, 0x86)],
    };
    let mut backend = PowerBackend::new(1, [0; 256]);
    backend.fail_pci_write = true;
    assert_eq!(
        apply_device_power_writes(&effects, Some(scope), &mut backend),
        Err(DevicePowerWriteError::Read(Error::Unsupported(0x5b84)))
    );
    assert_eq!(backend.region, 1);
}
