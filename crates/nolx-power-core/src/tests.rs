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
    /// A QWord Address Space descriptor: tag 0x8A, forty-three payload bytes, laid out as
    /// type, general flags, type flags, then granularity, minimum, maximum, translation and
    /// length, each eight bytes wide.
    fn qword(resource_type: u8, min: u64, max: u64, length: u64) -> Vec<u8> {
        let mut out = vec![0x8A, 43, 0x00, resource_type, 0x0c, 0x03];
        out.extend_from_slice(&0u64.to_le_bytes()); // granularity
        out.extend_from_slice(&min.to_le_bytes());
        out.extend_from_slice(&max.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // translation
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[0x79, 0x00]);
        out
    }

    // The window above 4 GiB that the host bridge publishes.
    assert_eq!(
        parse_resources(&qword(0, 0x1_0000_0000, 0x1_7FFF_FFFF, 0x8000_0000))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Memory {
            base: 0x1_0000_0000,
            length: 0x8000_0000
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
    // Small-descriptor name 6 is reserved: tag 0x32, two bytes.
    assert_eq!(
        parse_resources(&[0x32, 0x01, 0x00, 0x79, 0x00]),
        Err(ResourceError::Unsupported(0x32))
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

/// A FADT of `length` bytes with the given fields, and its checksum set so the table sums to zero.
fn fadt(length: usize, revision: u8, dsdt32: u32, x_dsdt: u64, pm1a: u32, pm1b: u32) -> [u8; 152] {
    let mut bytes = [0u8; 152];
    bytes[..4].copy_from_slice(b"FACP");
    bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[8] = revision;
    bytes[40..44].copy_from_slice(&dsdt32.to_le_bytes());
    bytes[64..68].copy_from_slice(&pm1a.to_le_bytes());
    bytes[68..72].copy_from_slice(&pm1b.to_le_bytes());
    bytes[140..148].copy_from_slice(&x_dsdt.to_le_bytes());
    let sum = bytes[..length]
        .iter()
        .fold(0u8, |sum, &byte| sum.wrapping_add(byte));
    bytes[9] = 0u8.wrapping_sub(sum);
    bytes
}

#[test]
fn a_fadt_gives_the_dsdt_and_the_pm1_control_ports() {
    let bytes = fadt(116, 1, 0x000E_0000, 0, 0x604, 0);
    let table = Fadt::parse(&bytes[..116]).expect("a valid revision-1 FADT");
    assert_eq!(
        table.dsdt, 0x000E_0000,
        "a revision-1 table uses the 32-bit DSDT"
    );
    assert_eq!(table.pm1a_control, 0x604);
    assert_eq!(
        table.pm1b_control, None,
        "a zero PM1b port means no second block"
    );
    let with_b = fadt(116, 1, 0x1000, 0, 0x604, 0x608);
    assert_eq!(
        Fadt::parse(&with_b[..116]).unwrap().pm1b_control,
        Some(0x608)
    );
}

#[test]
fn a_revision_two_fadt_takes_the_64_bit_dsdt_when_it_is_set() {
    let bytes = fadt(148, 2, 0x1000, 0x2000, 0x604, 0);
    assert_eq!(Fadt::parse(&bytes[..148]).unwrap().dsdt, 0x2000);
    let unset = fadt(148, 2, 0x1000, 0, 0x604, 0);
    assert_eq!(
        Fadt::parse(&unset[..148]).unwrap().dsdt,
        0x1000,
        "a zero 64-bit field falls back to the 32-bit one"
    );
}

#[test]
fn a_fadt_that_is_damaged_or_incomplete_is_refused_with_its_reason() {
    let good = fadt(116, 1, 0x1000, 0, 0x604, 0);
    let mut bad_sum = good;
    bad_sum[20] ^= 0x01;
    assert_eq!(Fadt::parse(&bad_sum[..116]), Err(FadtError::BadChecksum));
    let mut wrong = good;
    wrong[..4].copy_from_slice(b"RSDT");
    assert_eq!(Fadt::parse(&wrong[..116]), Err(FadtError::WrongSignature));
    assert_eq!(
        Fadt::parse(&good[..100]),
        Err(FadtError::TooShort),
        "cut before the declared length"
    );
    let no_dsdt = fadt(116, 1, 0, 0, 0x604, 0);
    assert_eq!(Fadt::parse(&no_dsdt[..116]), Err(FadtError::NoDsdt));
    let no_pm1a = fadt(116, 1, 0x1000, 0, 0, 0);
    assert_eq!(Fadt::parse(&no_pm1a[..116]), Err(FadtError::NoPm1aControl));
    let wide_pm1a = fadt(116, 1, 0x1000, 0, 0x1_0000, 0);
    assert_eq!(
        Fadt::parse(&wide_pm1a[..116]),
        Err(FadtError::NoPm1aControl),
        "a port above 0xFFFF is not an I/O port"
    );
}

#[test]
fn sleep_control_preserves_every_unrelated_bit() {
    let mask = (7 << 10) | (1 << 13);
    for current in [0, 1, 0xffff, 0x1555, 0xaaaa] {
        for typ in 0..8 {
            for enable in [false, true] {
                let word = pm1_sleep_control(current, typ, enable).unwrap();
                assert_eq!(word & !mask, current & !mask);
                assert_eq!(word & mask, pm1_sleep_value(typ, enable).unwrap());
            }
        }
    }
    assert_eq!(pm1_sleep_control(0xffff, 8, true), None);
}

#[test]
fn fadt_records_acpi_enable_and_control_width_without_truncating_ports() {
    let mut bytes = fadt(116, 1, 0x1000, 0, 0x604, 0);
    bytes[48..52].copy_from_slice(&0xb2u32.to_le_bytes());
    bytes[52] = 0xf1;
    bytes[89] = 2;
    bytes[9] = 0;
    bytes[9] = 0u8.wrapping_sub(bytes[..116].iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    let parsed = Fadt::parse(&bytes[..116]).unwrap();
    assert_eq!(parsed.smi_command, Some(0xb2));
    assert_eq!(parsed.acpi_enable, 0xf1);
    assert_eq!(parsed.pm1_control_bytes, 2);
    bytes[48..52].copy_from_slice(&0x100b2u32.to_le_bytes());
    bytes[9] = 0;
    bytes[9] = 0u8.wrapping_sub(bytes[..116].iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    assert_eq!(Fadt::parse(&bytes[..116]).unwrap().smi_command, None);
}

#[test]
fn resource_descriptor_bounds_cannot_borrow_bytes_from_the_next_descriptor() {
    for bytes in [
        &[0x4a, 0xf8, 3, 0x79, 0][..],
        &[0x86, 1, 0, 0, 0x79, 0],
        &[0x79],
        &[0x8a, 43, 0, 0],
    ] {
        assert!(parse_resources(bytes).is_err());
    }
    assert!(parse_resources(&[0x79, 0, 0x22, 1, 0]).is_err());
}
#[test]
fn resource_ranges_and_nonzero_checksums_are_validated() {
    assert_eq!(
        parse_resources(&[0x4b, 0xff, 0xff, 2, 0x79, 0]),
        Err(ResourceError::InvalidRange)
    );
    assert_eq!(
        parse_resources(&[0x4b, 0, 0, 0, 0x79, 0]),
        Err(ResourceError::InvalidRange)
    );
    assert_eq!(parse_resources(&[0x79, 1]), Err(ResourceError::Checksum));
    assert!(parse_resources(&[0x79, 0x87]).unwrap().is_empty());
    assert!(parse_resources(&[0x79, 0, 0, 0]).unwrap().is_empty());
}
#[test]
fn every_truncation_of_a_resource_template_is_refused() {
    let bytes = [0x4b, 0xf8, 3, 8, 0x22, 16, 0, 0x79, 0];
    for end in 0..bytes.len() {
        assert!(parse_resources(&bytes[..end]).is_err());
    }
    assert_eq!(parse_resources(&bytes).unwrap().len(), 2);
}

#[test]
fn a_word_address_space_descriptor_is_decoded_for_memory_io_and_bus_numbers() {
    /// A Word Address Space descriptor: tag 0x88, thirteen payload bytes — resource type,
    /// general flags, type flags, then five two-byte values (granularity, minimum, maximum,
    /// translation, length). This is the layout QEMU's DSDT uses for the PCI root's bus range
    /// and its two I/O windows; see `a_real_host_bridge_resource_template_decodes`.
    fn word(resource_type: u8, flags: u8, min: u16, max: u16, length: u16) -> Vec<u8> {
        let mut out = vec![0x88, 13, 0x00, resource_type, 0x0c, flags];
        out.extend_from_slice(&0u16.to_le_bytes()); // granularity
        out.extend_from_slice(&min.to_le_bytes());
        out.extend_from_slice(&max.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // translation
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[0x79, 0x00]);
        out
    }

    assert_eq!(
        parse_resources(&word(0, 0x03, 0xE000, 0xE0FF, 0x100))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Memory {
            base: 0xE000,
            length: 0x100
        }]
    );
    assert_eq!(
        parse_resources(&word(1, 0x03, 0x0300, 0x03FF, 0x100))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Io {
            base: 0x0300,
            length: 0x100
        }]
    );
    // A host bridge publishes its secondary bus range this way: type 2, no type flags, and a
    // length of 256 for the 0..255 inclusive range.
    assert_eq!(
        parse_resources(&word(2, 0x00, 0x00, 0xFF, 0x100))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::BusNumber {
            start: 0x00,
            end: 0xFF
        }]
    );
    // Subtractive decode is not modelled, so it is refused rather than flattened into a range.
    let mut subtractive = word(1, 0x03, 0x0300, 0x03FF, 0x100);
    subtractive[4] = 0x0e;
    assert_eq!(
        parse_resources(&subtractive),
        Err(ResourceError::Unsupported(0x88))
    );
    // Neither is a translated window: minimum and maximum would no longer be addresses here.
    let mut translated = word(1, 0x03, 0x0300, 0x03FF, 0x100);
    translated[12] = 0x01;
    assert_eq!(
        parse_resources(&translated),
        Err(ResourceError::Unsupported(0x88))
    );
}

#[test]
fn a_dword_address_space_descriptor_takes_the_whole_io_window() {
    /// A DWord Address Space descriptor: tag 0x87, twenty-three payload bytes.
    fn dword(resource_type: u8, flags: u8, min: u32, max: u32, length: u32) -> Vec<u8> {
        let mut out = vec![0x87, 23, 0x00, resource_type, 0x0c, flags];
        out.extend_from_slice(&0u32.to_le_bytes()); // granularity
        out.extend_from_slice(&min.to_le_bytes());
        out.extend_from_slice(&max.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // translation
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[0x79, 0x00]);
        out
    }

    // The I/O window a host bridge really publishes is wider than a byte can count: this is the
    // case an 8-bit length would silently truncate.
    assert_eq!(
        parse_resources(&dword(1, 0x03, 0x0000, 0xFFFF, 0x1_0000))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Io {
            base: 0x0000,
            length: 0x1_0000
        }]
    );
    assert_eq!(
        parse_resources(&dword(0, 0x03, 0x8000_0000, 0x8FFF_FFFF, 0x1000_0000))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Memory {
            base: 0x8000_0000,
            length: 0x1000_0000
        }]
    );
}

#[test]
fn an_address_space_descriptor_with_an_inconsistent_window_is_refused() {
    /// The same DWord shape with a chosen payload and a lie about the tag's length.
    fn dword_with(resource_type: u8, min: u32, max: u32, length: u32, len_byte: u8) -> Vec<u8> {
        let mut out = vec![0x87, len_byte, 0x00, resource_type, 0x0c, 0x03];
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&min.to_le_bytes());
        out.extend_from_slice(&max.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[0x79, 0x00]);
        out
    }

    // A window that claims more than its own maximum is lying about one of the two.
    assert_eq!(
        parse_resources(&dword_with(0, 0x1000, 0x1FFF, 0x2000, 23)),
        Err(ResourceError::InvalidRange)
    );
    // An empty window is not a resource.
    assert_eq!(
        parse_resources(&dword_with(0, 0x1000, 0x1FFF, 0, 23)),
        Err(ResourceError::InvalidRange)
    );
    // Minimum above maximum is backwards.
    assert_eq!(
        parse_resources(&dword_with(2, 0x20, 0x10, 0x100, 23)),
        Err(ResourceError::InvalidRange)
    );
    // Device-specific (0xC0…0xFF) and extended (0x80…) types are refused, not coerced.
    assert_eq!(
        parse_resources(&dword_with(0xC0, 0, 0xFF, 0x100, 23)),
        Err(ResourceError::Unsupported(0xC0))
    );
    // The tag carries a fixed length; a template that disagrees is malformed.
    assert_eq!(
        parse_resources(&dword_with(0, 0x1000, 0x1FFF, 0x1000, 24)),
        Err(ResourceError::InvalidLength)
    );
}

#[test]
fn an_extended_interrupt_descriptor_becomes_an_irq_mask() {
    /// An extended interrupt descriptor: flags, count, then 32-bit interrupt numbers.
    fn extended(flags: u8, lines: &[u32]) -> Vec<u8> {
        let length = 2 + lines.len() * 4;
        let mut out = vec![0x89, length as u8, 0x00, flags, lines.len() as u8];
        for line in lines {
            out.extend_from_slice(&line.to_le_bytes());
        }
        out.extend_from_slice(&[0x79, 0x00]);
        out
    }

    // What QEMU's interrupt-link devices return from `_CRS`: one shared, consumer interrupt.
    assert_eq!(
        parse_resources(&extended(0x09, &[5]))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Irq { mask: 1 << 5 }]
    );
    // And the three-line form the same devices carry in `_PRS` (5, 10, 11).
    assert_eq!(
        parse_resources(&extended(0x09, &[5, 10, 11]))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Irq {
            mask: (1 << 5) | (1 << 10) | (1 << 11)
        }]
    );
    // A count that disagrees with the length is malformed, and a line this crate cannot
    // represent in its 16-bit mask is refused rather than dropped.
    assert_eq!(
        parse_resources(&[0x89, 0x07, 0x00, 0x09, 0x02, 5, 0, 0, 0, 0x79, 0x00]),
        Err(ResourceError::InvalidLength)
    );
    // A Q35 root port's GSI link device names a line above the 16 a small IRQ descriptor can
    // reach, so the mask is wider than that descriptor's.
    assert_eq!(
        parse_resources(&extended(0x09, &[16]))
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Irq { mask: 1 << 16 }]
    );
    assert_eq!(
        parse_resources(&extended(0x09, &[64])),
        Err(ResourceError::Unsupported(0x89))
    );
}

#[test]
fn a_dma_descriptor_becomes_a_channel_mask() {
    // The floppy controller publishes DMA channel 2 with no transfer-type bits set.
    assert_eq!(
        parse_resources(&[0x2A, 0x04, 0x00, 0x79, 0x00])
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [Resource::Dma { mask: 0x04 }]
    );
}

#[test]
fn a_real_host_bridge_resource_template_decodes() {
    /// The PCI root's `_CRS` exactly as QEMU's i440fx firmware returns it, dumped from the DSDT
    /// of a booted machine (156 bytes): a bus range, the fixed 0xCF8 configuration window, two
    /// I/O windows, a DWord window over the 640 KiB-1 MiB hole, a DWord window over the PCI
    /// aperture, and the QWord window above 4 GiB.
    const PCI0_CRS: [u8; 156] = [
        0x88, 0x0d, 0x00, 0x02, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x47, 0x01, 0xf8, 0x0c, 0xf8, 0x0c, 0x01, 0x08, 0x88, 0x0d, 0x00, 0x01, 0x0c, 0x03,
        0x00, 0x00, 0x00, 0x00, 0xf7, 0x0c, 0x00, 0x00, 0xf8, 0x0c, 0x88, 0x0d, 0x00, 0x01, 0x0c,
        0x03, 0x00, 0x00, 0x00, 0x0d, 0xff, 0xff, 0x00, 0x00, 0x00, 0xf3, 0x87, 0x17, 0x00, 0x00,
        0x0c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0xff, 0xff, 0x0b, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x87, 0x17, 0x00, 0x00, 0x0c, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0xff, 0xff, 0xbf, 0xfe, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xc0, 0xde, 0x8a, 0x2b, 0x00, 0x00, 0x0c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x7f, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80,
        0x00, 0x00, 0x00, 0x00, 0x79, 0x00,
    ];

    assert_eq!(
        parse_resources(&PCI0_CRS)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [
            Resource::BusNumber {
                start: 0,
                end: 0xFF
            },
            Resource::Io {
                base: 0x0CF8,
                length: 8
            },
            Resource::Io {
                base: 0x0000,
                length: 0x0CF8
            },
            Resource::Io {
                base: 0x0D00,
                length: 0xF300
            },
            Resource::Memory {
                base: 0x000A_0000,
                length: 0x2_0000
            },
            Resource::Memory {
                base: 0x2000_0000,
                length: 0xDEC0_0000
            },
            Resource::Memory {
                base: 0x1_0000_0000,
                length: 0x8000_0000
            },
        ]
    );

    // The strict host-bridge decoder reads the same bytes the same way: both see one bus range
    // and the same fixed, positively-decoded, untranslated windows.
    let root = crate::root::parse(&PCI0_CRS).expect("root decoder accepts the same template");
    let windows: Vec<_> = root.iter().collect();
    assert_eq!(windows.len(), 7);
    assert!(windows
        .iter()
        .any(|w| w.space == crate::root::Space::Bus && w.start == 0 && w.end == 256));
}
