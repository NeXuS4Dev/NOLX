//! Portable parsing and construction of simple ACPI link-device IRQ resources.
//!
//! This module only evaluates `_PRS`/`_CRS` buffers and constructs a candidate `_SRS` argument. It
//! deliberately does not invoke `_SRS` or access hardware: AML side effects and chipset/IOAPIC
//! programming require a separate, audited transaction provider.

extern crate alloc;
use alloc::vec::Vec;

use crate::aml::{MethodEffects, Namespace, Path, PciFieldWritePermission, Value};

use super::{Error, Interrupt};

/// Maximum resource-template bytes accepted from one AML value.
const MAX_TEMPLATE_BYTES: usize = 16_384;
/// Maximum distinct interrupt choices retained from one simple resource descriptor.
const MAX_IRQ_OPTIONS: usize = 256;
/// Small IRQ bits 1:2 are ignored; bits 0, 3, 4, and 5 are defined (bit 5 is Wake Capability).
const SMALL_IRQ_FLAGS_ALLOWED: u8 = 0x3f;
const SMALL_IRQ_WAKE_CAPABLE: u8 = 1 << 5;

/// The namespace in which an IRQ number in a link resource is expressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkIrqNumber {
    /// An ISA IRQ number from a Small IRQ descriptor. The platform must map this through MADT
    /// interrupt-source overrides before programming an I/O APIC.
    Isa(u8),
    /// A global system interrupt from an Extended IRQ descriptor.
    Gsi(u32),
}

/// One electrically described choice from a simple link-device `_PRS` IRQ descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkIrqOption {
    /// IRQ or GSI number, with its resource-descriptor namespace retained.
    pub number: LinkIrqNumber,
    /// Active-low polarity; false means active high.
    pub active_low: bool,
    /// Level-triggered; false means edge-triggered.
    pub level: bool,
    /// The resource descriptor permits sharing.
    pub shared: bool,
    /// The Small IRQ or Extended IRQ descriptor marks this choice as wake-capable.
    pub wake_capable: bool,
}

#[derive(Clone)]
struct IrqDescriptor {
    tag: u8,
    start: usize,
    flags: u8,
    options: Vec<LinkIrqOption>,
}

struct Template {
    irq: Option<IrqDescriptor>,
    end_tag: usize,
    checksum: u8,
}

/// Evaluate a link device's `_PRS` and return its bounded simple IRQ choices.
///
/// `_PRS` must evaluate to a resource-template buffer containing exactly one IRQ descriptor;
/// dependent-function alternatives, multiple IRQ descriptors, and malformed templates are
/// refused rather than combined into a fictitious set of choices.
pub fn link_possible_interrupts(
    namespace: &Namespace<'_>,
    link: &Path,
) -> Result<Vec<LinkIrqOption>, Error> {
    let value = namespace
        .device_value(link, *b"_PRS")?
        .ok_or(Error::Invalid)?;
    let Value::Buffer(bytes) = value else {
        return Err(Error::Invalid);
    };
    possible_interrupts(&bytes)
}

/// Parse the IRQ choices from one simple `_PRS` resource template.
pub fn possible_interrupts(bytes: &[u8]) -> Result<Vec<LinkIrqOption>, Error> {
    let template = parse_template(bytes)?;
    template.irq.map(|irq| irq.options).ok_or(Error::Invalid)
}

/// Build a candidate `_SRS` buffer from the link's current `_CRS` and a choice from `_PRS`.
///
/// The candidate is based on `_CRS`, so every descriptor stays in the same order and keeps its
/// original type, length, and overall buffer size as required for `_SRS`. Only the current IRQ
/// number and representable electrical flags are changed. An optional nonzero EndTag checksum
/// is recomputed. The returned buffer is not written to hardware and does not execute `_SRS`.
pub fn build_srs_template(
    current: &[u8],
    possible: &[u8],
    selected: LinkIrqOption,
) -> Result<Vec<u8>, Error> {
    let current_template = parse_template(current)?;
    let possible_template = parse_template(possible)?;
    let current_irq = current_template.irq.ok_or(Error::Invalid)?;
    let possible_irq = possible_template.irq.ok_or(Error::Invalid)?;
    if current_irq.options.len() != 1 || !possible_irq.options.contains(&selected) {
        return Err(Error::Invalid);
    }

    let current_is_small = matches!(current_irq.tag, 0x22 | 0x23);
    let possible_is_small = matches!(possible_irq.tag, 0x22 | 0x23);
    if current_is_small != possible_is_small
        || (!current_is_small && (current_irq.flags & 1) != (possible_irq.flags & 1))
    {
        return Err(Error::Invalid);
    }

    let mut output = current.to_vec();
    match current_irq.tag {
        0x22 | 0x23 => {
            let LinkIrqNumber::Isa(number) = selected.number else {
                return Err(Error::Invalid);
            };
            if number >= 16 {
                return Err(Error::Invalid);
            }
            let mask = 1u16 << number;
            output[current_irq.start + 1..current_irq.start + 3]
                .copy_from_slice(&mask.to_le_bytes());
            let flags = small_irq_flags(selected);
            if current_irq.tag == 0x22 {
                // IRQNoFlags has implicit edge/high/exclusive/non-wake flags.
                if flags != 1 {
                    return Err(Error::Invalid);
                }
            } else {
                // Bits 1:2 are ignored by ACPI; preserve the current _CRS bits in _SRS.
                output[current_irq.start + 3] = flags | (current_irq.flags & 0x06);
            }
        }
        0x89 => {
            let LinkIrqNumber::Gsi(number) = selected.number else {
                return Err(Error::Invalid);
            };
            output[current_irq.start + 3] = (current_irq.flags & 1) | extended_irq_flags(selected);
            output[current_irq.start + 5..current_irq.start + 9]
                .copy_from_slice(&number.to_le_bytes());
        }
        _ => return Err(Error::Invalid),
    }

    if current_template.checksum != 0 {
        let checksum_at = current_template.end_tag + 1;
        output[checksum_at] = 0;
        let sum = output[..=checksum_at]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        output[checksum_at] = 0u8.wrapping_sub(sum);
    }

    let verified = parse_template(&output)?;
    let verified_irq = verified.irq.ok_or(Error::Invalid)?;
    if output.len() != current.len()
        || verified_irq.options.len() != 1
        || verified_irq.options[0] != selected
    {
        return Err(Error::Invalid);
    }
    Ok(output)
}

/// Evaluate a link's `_PRS` and `_CRS` and build a candidate `_SRS` resource buffer.
pub fn link_srs_template(
    namespace: &Namespace<'_>,
    link: &Path,
    selected: LinkIrqOption,
) -> Result<Vec<u8>, Error> {
    let value = namespace
        .device_value(link, *b"_PRS")?
        .ok_or(Error::Invalid)?;
    let Value::Buffer(possible) = value else {
        return Err(Error::Invalid);
    };
    let current = namespace.device_resources(link)?.ok_or(Error::Invalid)?;
    build_srs_template(&current, &possible, selected)
}

/// A bounded link-route selection and, when needed, the candidate `_SRS` argument.
///
/// This is a planning result only. It does not invoke `_SRS`, alter AML state, or program a
/// controller. The caller must map ISA IRQs through the platform's MADT overrides and use
/// `available` to reject reserved, unsupported, or already-conflicting GSIs. A returned
/// `srs_argument` must not be published as the active route until `_SRS` succeeds and `_CRS`
/// re-evaluation confirms the selected resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRouteSelection {
    /// The exact advertised resource option selected for this link.
    pub option: LinkIrqOption,
    /// The selected route after the caller's ISA-to-GSI mapping.
    pub interrupt: Interrupt,
    /// Candidate `_SRS` buffer when the current `_CRS` route is unavailable; `None` means the
    /// existing `_CRS` route can be retained without asking firmware to change it.
    pub srs_argument: Option<Vec<u8>>,
}

/// Prefer an available current `_CRS` route; otherwise select an advertised `_PRS` alternative.
///
/// Only level-triggered, shareable choices can be selected for PCI INTx. ISA choices are mapped
/// by `isa_to_gsi`; returning `None` refuses that option. `_PRS` is consulted only when the
/// current route cannot be retained. The generated buffer preserves the `_CRS` descriptor shape,
/// but this function does not execute `_SRS` or authorize hardware writes.
pub fn select_link_route(
    namespace: &Namespace<'_>,
    link: &Path,
    isa_to_gsi: impl Fn(u8) -> Option<u32>,
    mut available: impl FnMut(Interrupt) -> bool,
) -> Result<LinkRouteSelection, Error> {
    let (status, _) = namespace.device_status(link)?;
    if !crate::is_usable(status) {
        return Err(Error::Invalid);
    }
    let current = namespace.device_resources(link)?.ok_or(Error::Invalid)?;
    let current_irq = parse_template(&current)?.irq.ok_or(Error::Invalid)?;
    if current_irq.options.len() != 1 {
        return Err(Error::Ambiguous);
    }
    let current_option = current_irq.options[0];
    if let Some(interrupt) = mapped_pci_interrupt(current_option, &isa_to_gsi) {
        if available(interrupt) {
            return Ok(LinkRouteSelection {
                option: current_option,
                interrupt,
                srs_argument: None,
            });
        }
    }

    let value = namespace
        .device_value(link, *b"_PRS")?
        .ok_or(Error::Invalid)?;
    let Value::Buffer(possible) = value else {
        return Err(Error::Invalid);
    };
    for option in possible_interrupts(&possible)? {
        let Some(interrupt) = mapped_pci_interrupt(option, &isa_to_gsi) else {
            continue;
        };
        if !available(interrupt) {
            continue;
        }
        if namespace.device_method_arity(link, *b"_SRS")? != Some(1) {
            return Err(Error::Invalid);
        }
        let srs_argument = build_srs_template(&current, &possible, option)?;
        return Ok(LinkRouteSelection {
            option,
            interrupt,
            srs_argument: Some(srs_argument),
        });
    }
    Err(Error::NoRoute)
}

/// Execute a selected link's `_SRS` bytecode in the bounded interpreter and return an authorized
/// PCI-config write journal. The journal is only a plan: this function never writes hardware,
/// commits AML global state, or publishes the selected route.
pub fn plan_link_srs(
    namespace: &Namespace<'_>,
    link: &Path,
    selection: &LinkRouteSelection,
    permissions: &[PciFieldWritePermission],
) -> Result<MethodEffects, Error> {
    let srs_argument = selection.srs_argument.as_deref().ok_or(Error::Invalid)?;
    let options = possible_interrupts(srs_argument)?;
    if options.len() != 1
        || options[0] != selection.option
        || !selection.option.level
        || !selection.option.shared
        || selection.interrupt.active_low != selection.option.active_low
        || selection.interrupt.level != selection.option.level
        || selection.interrupt.shared != selection.option.shared
        || matches!(selection.option.number, LinkIrqNumber::Gsi(gsi) if gsi != selection.interrupt.gsi)
    {
        return Err(Error::Invalid);
    }
    let (status, _) = namespace.device_status(link)?;
    if !crate::is_usable(status) || namespace.device_method_arity(link, *b"_SRS")? != Some(1) {
        return Err(Error::Invalid);
    }
    namespace
        .plan_device_method(
            link,
            *b"_SRS",
            &[Value::Buffer(srs_argument.to_vec())],
            permissions,
        )
        .map_err(Error::from)
}

fn mapped_pci_interrupt(
    option: LinkIrqOption,
    isa_to_gsi: &impl Fn(u8) -> Option<u32>,
) -> Option<Interrupt> {
    if !option.level || !option.shared {
        return None;
    }
    let gsi = match option.number {
        LinkIrqNumber::Isa(irq) => isa_to_gsi(irq)?,
        LinkIrqNumber::Gsi(gsi) => gsi,
    };
    Some(Interrupt {
        gsi,
        active_low: option.active_low,
        level: option.level,
        shared: option.shared,
    })
}

fn small_irq_flags(option: LinkIrqOption) -> u8 {
    let mut flags = u8::from(!option.level);
    if option.active_low {
        flags |= 1 << 3;
    }
    if option.shared {
        flags |= 1 << 4;
    }
    if option.wake_capable {
        flags |= SMALL_IRQ_WAKE_CAPABLE;
    }
    flags
}

fn extended_irq_flags(option: LinkIrqOption) -> u8 {
    let mut flags = 0u8;
    if !option.level {
        flags |= 1 << 1;
    }
    if option.active_low {
        flags |= 1 << 2;
    }
    if option.shared {
        flags |= 1 << 3;
    }
    if option.wake_capable {
        flags |= 1 << 4;
    }
    flags
}

fn parse_template(bytes: &[u8]) -> Result<Template, Error> {
    if bytes.is_empty() || bytes.len() > MAX_TEMPLATE_BYTES {
        return Err(if bytes.is_empty() {
            Error::Invalid
        } else {
            Error::Limit
        });
    }

    let mut irq = None;
    let mut at = 0usize;
    loop {
        let tag = *bytes.get(at).ok_or(Error::Invalid)?;
        if tag == 0x79 {
            let checksum = *bytes.get(at + 1).ok_or(Error::Invalid)?;
            let end = at.checked_add(2).ok_or(Error::Limit)?;
            if bytes[end..].iter().any(|byte| *byte != 0)
                || (checksum != 0
                    && bytes[..end]
                        .iter()
                        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
                        != 0)
            {
                return Err(Error::Invalid);
            }
            return Ok(Template {
                irq,
                end_tag: at,
                checksum,
            });
        }

        let (body, length) = if tag & 0x80 == 0 {
            (at.checked_add(1).ok_or(Error::Limit)?, usize::from(tag & 7))
        } else {
            let length_bytes = bytes.get(at + 1..at + 3).ok_or(Error::Invalid)?;
            (
                at.checked_add(3).ok_or(Error::Limit)?,
                usize::from(u16::from_le_bytes([length_bytes[0], length_bytes[1]])),
            )
        };
        let end = body.checked_add(length).ok_or(Error::Limit)?;
        let data = bytes.get(body..end).ok_or(Error::Invalid)?;

        // Dependent-function descriptors express alternatives across several resources. A
        // flat union of their IRQ values would authorize combinations firmware never offered.
        if matches!(tag, 0x30 | 0x31 | 0x38) {
            return Err(Error::Invalid);
        }

        let parsed = match tag {
            0x22 | 0x23 => Some(parse_small_irq(tag, at, data)?),
            0x89 => Some(parse_extended_irq(at, data)?),
            _ => None,
        };
        if let Some(parsed) = parsed {
            if irq.is_some() {
                return Err(Error::Ambiguous);
            }
            irq = Some(parsed);
        }
        at = end;
    }
}

fn parse_small_irq(tag: u8, start: usize, data: &[u8]) -> Result<IrqDescriptor, Error> {
    if data.len() != usize::from(tag & 7) || !(data.len() == 2 || data.len() == 3) {
        return Err(Error::Invalid);
    }
    let mask = u16::from_le_bytes([data[0], data[1]]);
    if mask == 0 {
        return Err(Error::Invalid);
    }
    let flags = if data.len() == 3 { data[2] } else { 1 };
    if flags & !SMALL_IRQ_FLAGS_ALLOWED != 0 || (flags & 1 == 0) != (flags & 8 != 0) {
        return Err(Error::Invalid);
    }
    let mut options = Vec::new();
    for irq in 0u8..16 {
        if mask & (1u16 << irq) == 0 {
            continue;
        }
        if options.len() == MAX_IRQ_OPTIONS {
            return Err(Error::Limit);
        }
        options.push(LinkIrqOption {
            number: LinkIrqNumber::Isa(irq),
            active_low: flags & 8 != 0,
            level: flags & 1 == 0,
            shared: flags & 16 != 0,
            wake_capable: flags & SMALL_IRQ_WAKE_CAPABLE != 0,
        });
    }
    Ok(IrqDescriptor {
        tag,
        start,
        flags,
        options,
    })
}

fn parse_extended_irq(start: usize, data: &[u8]) -> Result<IrqDescriptor, Error> {
    if data.len() < 2 || data[0] & !0x1f != 0 || data[1] == 0 {
        return Err(Error::Invalid);
    }
    let count = usize::from(data[1]);
    let expected = 2usize
        .checked_add(count.checked_mul(4).ok_or(Error::Limit)?)
        .ok_or(Error::Limit)?;
    if data.len() != expected || count > MAX_IRQ_OPTIONS {
        return Err(Error::Invalid);
    }
    let flags = data[0];
    let mut options = Vec::with_capacity(count);
    for vector in data[2..].chunks_exact(4) {
        let number = u32::from_le_bytes(vector.try_into().map_err(|_| Error::Invalid)?);
        let option = LinkIrqOption {
            number: LinkIrqNumber::Gsi(number),
            active_low: flags & 4 != 0,
            level: flags & 2 == 0,
            shared: flags & 8 != 0,
            wake_capable: flags & 16 != 0,
        };
        if options.contains(&option) {
            return Err(Error::Ambiguous);
        }
        options.push(option);
    }
    Ok(IrqDescriptor {
        tag: 0x89,
        start,
        flags,
        options,
    })
}

#[cfg(test)]
mod tests;
