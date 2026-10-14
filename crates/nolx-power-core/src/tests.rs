//! Host tests. Each rule is checked against the bytes the specifications lay out.

use super::*;

#[test]
fn a_device_is_usable_only_when_present_enabled_and_functioning() {
    assert!(is_usable(0x0F), "present, enabled, shown, functional");
    assert!(is_usable(0x1F), "a battery bit does not change that");
    assert!(!is_usable(0x03), "present and enabled, not functioning");
    assert!(!is_usable(0x00), "absent");
    assert!(
        !is_usable(STA_PRESENT | STA_FUNCTIONAL),
        "present but disabled"
    );
}

#[test]
fn an_irq_and_an_io_port_descriptor_are_decoded() {
    // IRQ 4: small tag 0x22 (name 4, two bytes), mask 0x0010.
    let irq = [0x22, 0x10, 0x00, 0x79, 0x00];
    let got = parse_resources(&irq).unwrap();
    assert_eq!(
        got.iter().collect::<Vec<_>>(),
        [Resource::Irq { mask: 0x0010 }]
    );
    // I/O port 0x60 to 0x60, eight ports: tag 0x47 (name 8, seven bytes).
    let io = [0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x08, 0x79, 0x00];
    let got = parse_resources(&io).unwrap();
    assert_eq!(
        got.iter().collect::<Vec<_>>(),
        [Resource::Io {
            base: 0x60,
            length: 8
        }]
    );
}

#[test]
fn a_fixed_io_and_a_fixed_memory_descriptor_are_decoded() {
    // Fixed I/O at 0x03F0, eight ports: tag 0x4B (name 9, three bytes).
    let fixed_io = [0x4B, 0xF0, 0x03, 0x08, 0x79, 0x00];
    assert_eq!(
        parse_resources(&fixed_io)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Io {
            base: 0x03F0,
            length: 8
        }]
    );
    // Memory32Fixed at 0xFEBF0000, 4 KiB: tag 0x86, length 9, information, base, length.
    let mut memory = vec![0x86, 0x09, 0x00, 0x01];
    memory.extend_from_slice(&0xFEBF_0000u32.to_le_bytes());
    memory.extend_from_slice(&0x1000u32.to_le_bytes());
    memory.extend_from_slice(&[0x79, 0x00]);
    assert_eq!(
        parse_resources(&memory).unwrap().iter().collect::<Vec<_>>(),
        [Resource::Memory {
            base: 0xFEBF_0000,
            length: 0x1000
        }]
    );
}

#[test]
fn a_qword_memory_descriptor_takes_its_minimum_and_length() {
    // QWord address space, type 0 (memory): tag 0x8A, length 43.
    let mut qword = vec![0x8A, 43, 0x00, 0x00, 0x00, 0x00];
    qword.extend_from_slice(&0u64.to_le_bytes()); // granularity
    qword.extend_from_slice(&0x1_0000_0000u64.to_le_bytes()); // minimum
    qword.extend_from_slice(&0x1_0000_1FFFu64.to_le_bytes()); // maximum
    qword.extend_from_slice(&0u64.to_le_bytes()); // translation
    qword.extend_from_slice(&0x2000u64.to_le_bytes()); // length
    qword.extend_from_slice(&[0x79, 0x00]);
    assert_eq!(
        parse_resources(&qword).unwrap().iter().collect::<Vec<_>>(),
        [Resource::Memory {
            base: 0x1_0000_0000,
            length: 0x2000
        }]
    );
}

#[test]
fn a_template_without_an_end_tag_or_cut_short_is_refused() {
    assert_eq!(
        parse_resources(&[0x22, 0x10, 0x00]),
        Err(ResourceError::NoEnd)
    );
    assert_eq!(
        parse_resources(&[0x22, 0x10]),
        Err(ResourceError::Truncated)
    );
}

#[test]
fn an_unknown_descriptor_is_refused_and_named() {
    // Name 5 (DMA), two bytes: tag 0x2A.
    assert_eq!(
        parse_resources(&[0x2A, 0x01, 0x00, 0x79, 0x00]),
        Err(ResourceError::Unsupported(0x2A))
    );
}

#[test]
fn a_template_holds_at_most_sixteen_resources() {
    let mut sixteen: Vec<u8> = (0..16).flat_map(|_| [0x22, 0x01, 0x00]).collect();
    sixteen.extend_from_slice(&[0x79, 0x00]);
    assert_eq!(parse_resources(&sixteen).unwrap().len(), 16);
    let mut seventeen: Vec<u8> = (0..17).flat_map(|_| [0x22, 0x01, 0x00]).collect();
    seventeen.extend_from_slice(&[0x79, 0x00]);
    assert_eq!(parse_resources(&seventeen), Err(ResourceError::TooMany));
}

#[test]
fn the_sleep_type_comes_from_the_s5_package_in_the_aml() {
    // Name "_S5_" holding package { 0x07, 0x07 }: NameOp, name, PackageOp, length, count,
    // then BytePrefix 7 twice.
    let aml = [
        0x08, b'_', b'S', b'5', b'_', 0x12, 0x06, 0x02, 0x0A, 0x07, 0x0A, 0x07,
    ];
    assert_eq!(sleep_type(&aml, SleepState::S5.name()), Some(7));
    assert_eq!(
        sleep_type(&aml, SleepState::S3.name()),
        None,
        "no _S3_ here"
    );
}

#[test]
fn a_zero_or_one_sleep_type_is_read_without_a_prefix() {
    let aml = [0x08, b'_', b'S', b'3', b'_', 0x12, 0x04, 0x01, 0x00];
    assert_eq!(sleep_type(&aml, SleepState::S3.name()), Some(0));
}

#[test]
fn a_name_not_opened_by_a_name_opcode_is_not_a_sleep_package() {
    let aml = [
        0x00, b'_', b'S', b'5', b'_', 0x12, 0x06, 0x02, 0x0A, 0x07, 0x0A, 0x07,
    ];
    assert_eq!(sleep_type(&aml, *b"_S5_"), None);
}

#[test]
fn the_pm1_word_places_the_type_and_the_go_bit() {
    assert_eq!(pm1_sleep_value(5, true), Some(0x3400));
    assert_eq!(pm1_sleep_value(7, false), Some(0x1C00));
    assert_eq!(
        pm1_sleep_value(8, true),
        None,
        "a type must fit in three bits"
    );
}
