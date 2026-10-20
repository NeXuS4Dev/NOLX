use super::alloc::vec;
use super::alloc::vec::Vec;
use super::{
    build_srs_template, link_possible_interrupts, link_srs_template, possible_interrupts,
    select_link_route, Error, LinkIrqNumber, LinkIrqOption, Namespace, Path, MAX_TEMPLATE_BYTES,
};

fn template(mut descriptors: Vec<u8>, checksum: bool) -> Vec<u8> {
    descriptors.push(0x79);
    descriptors.push(0);
    if checksum {
        let last = descriptors.len() - 1;
        let sum = descriptors
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        descriptors[last] = 0u8.wrapping_sub(sum);
    }
    descriptors.extend_from_slice(&[0, 0]);
    descriptors
}

fn name_buffer(name: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x08];
    out.extend_from_slice(name);
    out.extend_from_slice(&[0x11, (bytes.len() + 3) as u8, 0x0a, bytes.len() as u8]);
    out.extend_from_slice(bytes);
    out
}

fn link_aml(prs: &[u8], crs: &[u8], status: Option<u8>) -> Vec<u8> {
    link_aml_with_srs_arity(prs, crs, status, 1)
}

fn link_aml_with_srs_arity(prs: &[u8], crs: &[u8], status: Option<u8>, srs_arity: u8) -> Vec<u8> {
    let mut body = b"LNK0".to_vec();
    body.extend(name_buffer(b"_PRS", prs));
    body.extend(name_buffer(b"_CRS", crs));
    if let Some(status) = status {
        body.extend_from_slice(&[0x08, b'_', b'S', b'T', b'A', 0x0a, status]);
    }
    body.extend_from_slice(&[0x14, 7, b'_', b'S', b'R', b'S', srs_arity, 0xa3]);
    let mut aml = vec![0x5b, 0x82];
    let package_length = body.len() + 1;
    if package_length < 64 {
        aml.push(package_length as u8);
    } else {
        let package_length = body.len() + 2;
        aml.extend_from_slice(&[
            0x40 | (package_length & 0x0f) as u8,
            (package_length >> 4) as u8,
        ]);
    }
    aml.extend(body);
    aml
}

fn small_link_template() -> Vec<u8> {
    // I/O port descriptor followed by possible ISA IRQ3/IRQ5 (level/low/shared),
    // a checksummed EndTag, and legal zero padding.
    template(
        vec![0x47, 1, 0x60, 0, 0x70, 0, 0x10, 8, 0x23, 0x28, 0, 0x18],
        true,
    )
}

fn small_current_template() -> Vec<u8> {
    template(
        vec![0x47, 1, 0x70, 0, 0x70, 0, 1, 8, 0x23, 8, 0, 0x18],
        true,
    )
}

fn extended_link_template() -> Vec<u8> {
    // Producer, level-triggered, active-low, shared, wake-capable; possible GSIs 20 and 21.
    template(vec![0x89, 10, 0, 0x1c, 2, 20, 0, 0, 0, 21, 0, 0, 0], true)
}

fn extended_current_template() -> Vec<u8> {
    // Producer, level-triggered, active-high, shared, not wake-capable (the new choice differs).
    template(vec![0x89, 6, 0, 0x08, 1, 20, 0, 0, 0], true)
}

#[test]
fn small_irq_choices_preserve_isa_namespace_flags_and_io_resources() {
    let bytes = small_link_template();
    let options = possible_interrupts(&bytes).unwrap();
    assert_eq!(
        options,
        vec![
            LinkIrqOption {
                number: LinkIrqNumber::Isa(3),
                active_low: true,
                level: true,
                shared: true,
                wake_capable: false,
            },
            LinkIrqOption {
                number: LinkIrqNumber::Isa(5),
                active_low: true,
                level: true,
                shared: true,
                wake_capable: false,
            },
        ]
    );

    let current = small_current_template();
    let configured = build_srs_template(&current, &bytes, options[1]).unwrap();
    assert_eq!(configured.len(), current.len());
    assert_eq!(&configured[..8], &current[..8]);
    assert_ne!(&configured[..8], &bytes[..8]);
    assert_eq!(&configured[8..12], &[0x23, 0x20, 0, 0x18]);
    assert_eq!(possible_interrupts(&configured).unwrap(), vec![options[1]]);
    let end_tag = configured.iter().position(|byte| *byte == 0x79).unwrap();
    assert_eq!(
        configured[..=end_tag + 1]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        0
    );
    assert_eq!(&configured[end_tag + 2..], &[0, 0]);
}

#[test]
fn small_irq_wake_capability_bit_is_preserved() {
    // In Small IRQ Information, bit 5 is Wake Capability; IRQNoFlags defaults it off.
    let wake = template(vec![0x23, 1, 0, 0x28], true);
    let choices = possible_interrupts(&wake).unwrap();
    assert_eq!(choices.len(), 1);
    assert!(choices[0].wake_capable);
    let current = template(vec![0x23, 1, 0, 0x01], true);
    let configured = build_srs_template(&current, &wake, choices[0]).unwrap();
    assert_eq!(configured[3], 0x28);
    assert_eq!(possible_interrupts(&configured).unwrap(), choices);

    let no_flags = template(vec![0x22, 1, 0], true);
    assert!(!possible_interrupts(&no_flags).unwrap()[0].wake_capable);
}

#[test]
fn small_irq_ignored_bits_are_accepted_and_preserved_from_current_crs() {
    let possible = template(vec![0x23, 8, 0, 0x18], false);
    let current = template(vec![0x23, 8, 0, 0x1e], false);
    let selected = possible_interrupts(&possible).unwrap()[0];
    let configured = build_srs_template(&current, &possible, selected).unwrap();
    assert_eq!(configured[3], 0x1e);
    assert_eq!(possible_interrupts(&configured).unwrap(), vec![selected]);
}

#[test]
fn extended_irq_choice_preserves_current_descriptor_shape_and_checksum() {
    let bytes = extended_link_template();
    let options = possible_interrupts(&bytes).unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[1].number, LinkIrqNumber::Gsi(21));
    assert!(options[1].active_low && options[1].level && options[1].shared);
    assert!(options[1].wake_capable);

    let current = extended_current_template();
    let configured = build_srs_template(&current, &bytes, options[1]).unwrap();
    assert_eq!(configured.len(), current.len());
    assert_eq!(&configured[..9], &[0x89, 6, 0, 0x1c, 1, 21, 0, 0, 0]);
    assert_eq!(possible_interrupts(&configured).unwrap(), vec![options[1]]);
    let end_tag = configured.iter().position(|byte| *byte == 0x79).unwrap();
    assert_eq!(end_tag, 9);
    assert_eq!(
        configured[..=end_tag + 1]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        0
    );
    assert_eq!(&configured[end_tag + 2..], &[0, 0]);
}

#[test]
fn extended_irq_parser_accepts_both_resource_usage_flags() {
    let producer = template(vec![0x89, 6, 0, 0x1c, 1, 20, 0, 0, 0], false);
    let consumer = template(vec![0x89, 6, 0, 0x1d, 1, 20, 0, 0, 0], false);
    assert_eq!(possible_interrupts(&producer).unwrap().len(), 1);
    assert_eq!(possible_interrupts(&consumer).unwrap().len(), 1);
}

#[test]
fn namespace_helpers_evaluate_prs_and_build_from_current_crs() {
    // Device(LNK0) with literal _PRS and _CRS buffers; IRQ3 is current, IRQ3/IRQ5 are possible.
    let aml = [
        0x5b, 0x82, 35, b'L', b'N', b'K', b'0', 8, b'_', b'P', b'R', b'S', 0x11, 9, 0x0a, 6, 0x23,
        40, 0, 0x18, 0x79, 0, 8, b'_', b'C', b'R', b'S', 0x11, 9, 0x0a, 6, 0x23, 8, 0, 0x18, 0x79,
        0,
    ];
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();
    let choices = link_possible_interrupts(&namespace, &link).unwrap();
    assert_eq!(choices.len(), 2);
    let candidate = link_srs_template(&namespace, &link, choices[1]).unwrap();
    assert_eq!(possible_interrupts(&candidate).unwrap(), vec![choices[1]]);
}

#[test]
fn link_selector_keeps_an_available_current_route_without_building_srs() {
    let possible = small_link_template();
    let current = small_current_template();
    let aml = link_aml(&possible, &current, None);
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();

    let selected = select_link_route(
        &namespace,
        &link,
        |irq| Some(u32::from(irq) + 16),
        |interrupt| interrupt.gsi == 19,
    )
    .unwrap();
    assert_eq!(selected.option.number, LinkIrqNumber::Isa(3));
    assert_eq!(
        selected.interrupt,
        super::super::Interrupt {
            gsi: 19,
            active_low: true,
            level: true,
            shared: true,
        }
    );
    assert!(selected.srs_argument.is_none());
}

#[test]
fn link_selector_chooses_an_available_advertised_alternative_and_builds_srs() {
    let possible = small_link_template();
    let current = small_current_template();
    let aml = link_aml(&possible, &current, None);
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();

    let selected = select_link_route(
        &namespace,
        &link,
        |irq| Some(u32::from(irq) + 16),
        |interrupt| interrupt.gsi == 21,
    )
    .unwrap();
    assert_eq!(selected.option.number, LinkIrqNumber::Isa(5));
    assert_eq!(selected.interrupt.gsi, 21);
    let srs = selected.srs_argument.unwrap();
    assert_eq!(possible_interrupts(&srs).unwrap(), vec![selected.option]);
}

#[test]
fn link_selector_handles_extended_gsi_choices_and_builds_srs() {
    let possible = extended_link_template();
    let current = extended_current_template();
    let aml = link_aml(&possible, &current, None);
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();

    let selected = select_link_route(&namespace, &link, |_| None, |irq| irq.gsi == 21).unwrap();
    assert_eq!(selected.option.number, LinkIrqNumber::Gsi(21));
    assert_eq!(selected.interrupt.gsi, 21);
    let srs = selected.srs_argument.unwrap();
    assert_eq!(possible_interrupts(&srs).unwrap(), vec![selected.option]);
}

#[test]
fn link_selector_refuses_a_reprogramming_method_with_wrong_arity() {
    let possible = small_link_template();
    let current = small_current_template();
    let aml = link_aml_with_srs_arity(&possible, &current, None, 0);
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();

    assert_eq!(
        select_link_route(
            &namespace,
            &link,
            |irq| Some(u32::from(irq) + 16),
            |interrupt| interrupt.gsi == 21,
        ),
        Err(Error::Invalid)
    );
}

#[test]
fn link_selector_refuses_unusable_devices_and_when_no_route_is_available() {
    let possible = small_link_template();
    let current = small_current_template();
    let aml = link_aml(&possible, &current, Some(0));
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    let link = Path::parse("\\LNK0").unwrap();
    assert_eq!(
        select_link_route(&namespace, &link, |_| Some(3), |_| true),
        Err(Error::Invalid)
    );

    let aml = link_aml(&possible, &current, None);
    let mut namespace = Namespace::new();
    namespace.load(&aml).unwrap();
    assert_eq!(
        select_link_route(&namespace, &link, |_| Some(3), |_| false),
        Err(Error::NoRoute)
    );
}

#[test]
fn malformed_or_ambiguous_possible_irq_templates_are_refused() {
    let cases = [
        // No IRQ descriptor.
        template(vec![0x47, 1, 0x60, 0, 0x60, 0, 1, 8], false),
        // A zero IRQ mask.
        template(vec![0x22, 0, 0], false),
        // Reserved Small IRQ flag bits are not silently accepted.
        template(vec![0x23, 1, 0, 0x58], false),
        // Small IRQ active-low edge combinations are illegal for PIC-compatible IRQs.
        template(vec![0x23, 1, 0, 0x09], false),
        // Two independent IRQ descriptors cannot be flattened into one link choice.
        template(vec![0x22, 8, 0, 0x22, 32, 0], false),
        // Start/end-dependent alternatives are intentionally unsupported.
        template(vec![0x30, 0x22, 8, 0, 0x38], false),
        // Extended IRQ count and descriptor length disagree.
        template(vec![0x89, 6, 0, 1, 2, 20, 0, 0, 0], false),
        // Extended IRQ resource-source suffix is not silently dropped.
        template(vec![0x89, 7, 0, 1, 1, 20, 0, 0, 0, 0], false),
    ];
    for bytes in cases {
        assert!(possible_interrupts(&bytes).is_err(), "{bytes:02x?}");
    }

    let mut bad_checksum = small_link_template();
    let end_tag = bad_checksum.iter().position(|byte| *byte == 0x79).unwrap();
    bad_checksum[end_tag + 1] ^= 1;
    assert!(possible_interrupts(&bad_checksum).is_err());

    let mut bad_padding = small_link_template();
    *bad_padding.last_mut().unwrap() = 1;
    assert!(possible_interrupts(&bad_padding).is_err());

    assert_eq!(possible_interrupts(&[]), Err(Error::Invalid));
    assert!(possible_interrupts(&vec![0; MAX_TEMPLATE_BYTES + 1]).is_err());
}

#[test]
fn srs_builder_requires_an_exact_advertised_choice_and_valid_end_tag() {
    let possible = small_link_template();
    let current = small_current_template();
    let options = possible_interrupts(&possible).unwrap();
    let mut unadvertised = options[0];
    unadvertised.number = LinkIrqNumber::Isa(7);
    assert_eq!(
        build_srs_template(&current, &possible, unadvertised),
        Err(Error::Invalid)
    );

    let mut changed_electrical_flags = options[0];
    changed_electrical_flags.shared = false;
    assert_eq!(
        build_srs_template(&current, &possible, changed_electrical_flags),
        Err(Error::Invalid)
    );

    let mut short = current;
    let end_tag = short.iter().position(|byte| *byte == 0x79).unwrap();
    short.truncate(end_tag + 1);
    assert!(build_srs_template(&short, &possible, options[0]).is_err());

    let no_flags_current = template(vec![0x22, 8, 0], false);
    let wake_possible = template(vec![0x23, 8, 0, 0x28], false);
    let wake_choice = possible_interrupts(&wake_possible).unwrap()[0];
    assert!(build_srs_template(&no_flags_current, &wake_possible, wake_choice).is_err());

    let extended_possible = extended_link_template();
    let extended_choice = possible_interrupts(&extended_possible).unwrap()[0];
    let consumer_current = template(vec![0x89, 6, 0, 0x09, 1, 20, 0, 0, 0], false);
    assert!(build_srs_template(&consumer_current, &extended_possible, extended_choice).is_err());
}
