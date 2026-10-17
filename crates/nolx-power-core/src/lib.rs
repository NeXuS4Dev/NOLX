//! ACPI device status, resources, and sleep-state rules, decided over bytes.
//!
//! Three things the firmware tells the kernel, and what they mean. A device's `_STA` method
//! returns a status word: whether the device is present, enabled, shown, and working. Its `_CRS`
//! method returns a resource template: the interrupts, I/O ranges, and memory ranges the device
//! uses, as a list of small and large resource descriptors ending in an end tag. The sleep types
//! for each sleep state (`_S3_`, `_S5_`) are named in the DSDT's AML as small packages, and the
//! PM1 control register takes the chosen type and a go bit to enter that state.
//!
//! The bounded, pure [`aml`] namespace/evaluator executes supported `_STA` and `_CRS` methods.
//! Hardware fields, global mutation and synchronization are explicitly refused. Scope: a subset
//! of the resource descriptors a PC's devices use, not every descriptor the specification defines; and
//! the sleep lookup finds a `_Sx_` name that is a literal package, not one computed at runtime.
//! The FADT names the DSDT that holds the `_Sx_` packages and the PM1a control port the sleep type
//! is written to; [`Fadt::parse`] decodes those fields.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod aml;
pub mod root;

/// `_STA` bit 0: the device is present.
pub const STA_PRESENT: u32 = 1 << 0;
/// `_STA` bit 1: the device is enabled.
pub const STA_ENABLED: u32 = 1 << 1;
/// `_STA` bit 2: the device should be shown in the user interface.
pub const STA_SHOWN: u32 = 1 << 2;
/// `_STA` bit 3: the device is functioning.
pub const STA_FUNCTIONAL: u32 = 1 << 3;
/// `_STA` bit 4: the device's battery is present.
pub const STA_BATTERY: u32 = 1 << 4;

/// Whether a device with this `_STA` value may be used: present, enabled, and functioning.
#[must_use]
pub fn is_usable(sta: u32) -> bool {
    let needed = STA_PRESENT | STA_ENABLED | STA_FUNCTIONAL;
    sta & needed == needed
}

/// The most resource descriptors a template may hold. A template with more is refused, so the
/// list needs no allocator.
pub const MAX_RESOURCES: usize = 16;

/// One resource a device uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// Interrupt lines, one bit each: bit `n` is IRQ `n`.
    Irq {
        /// The IRQ bitmask.
        mask: u16,
    },
    /// A range of I/O ports.
    Io {
        /// The first port.
        base: u16,
        /// How many ports.
        length: u8,
    },
    /// A range of physical memory.
    Memory {
        /// The first address.
        base: u64,
        /// How many bytes.
        length: u64,
    },
}

/// Why a resource template was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceError {
    /// The template ends before its last descriptor or its end tag.
    Truncated,
    /// The template has no end tag before its bytes run out.
    NoEnd,
    /// A descriptor this crate does not decode. Holds its tag byte.
    Unsupported(u8),
    /// The template holds more descriptors than [`MAX_RESOURCES`].
    TooMany,
    /// A known descriptor has the wrong payload length or trailing nonzero bytes.
    InvalidLength,
    /// A range is empty, wraps, or exceeds its address space.
    InvalidRange,
    /// A supplied nonzero EndTag checksum does not validate the template.
    Checksum,
}

/// The decoded resources of one template, in the order the firmware listed them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    items: [Option<Resource>; MAX_RESOURCES],
    len: usize,
}

impl Resources {
    /// The resources, in template order.
    pub fn iter(&self) -> impl Iterator<Item = Resource> + '_ {
        self.items[..self.len].iter().flatten().copied()
    }

    /// How many resources the template holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the template holds no resources.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, resource: Resource) -> Result<(), ResourceError> {
        if self.len == MAX_RESOURCES {
            return Err(ResourceError::TooMany);
        }
        self.items[self.len] = Some(resource);
        self.len += 1;
        Ok(())
    }
}

/// The little-endian value of `len` bytes at `at`, or `Truncated` when the bytes are not there.
fn le(data: &[u8], at: usize, len: usize) -> Result<u64, ResourceError> {
    let bytes = data.get(at..at + len).ok_or(ResourceError::Truncated)?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0u64, |value, &byte| (value << 8) | u64::from(byte)))
}

/// Decode a `_CRS` resource template up to and including its end tag.
///
/// Small descriptors carry their length in the tag: IRQ (name 0x04), fixed I/O (0x09), and I/O
/// port (0x08). Large descriptors carry a 16-bit length: Memory32Fixed (0x06) and QWord address
/// space (0x0A), for memory resources only. Anything else is [`ResourceError::Unsupported`].
///
/// # Errors
///
/// See [`ResourceError`].
pub fn parse_resources(bytes: &[u8]) -> Result<Resources, ResourceError> {
    let mut out = Resources::default();
    let mut at = 0usize;
    loop {
        let tag = *bytes.get(at).ok_or(ResourceError::NoEnd)?;
        let (body, len) = if tag & 0x80 == 0 {
            (at + 1, usize::from(tag & 7))
        } else {
            (at + 3, le(bytes, at + 1, 2)? as usize)
        };
        let end = body.checked_add(len).ok_or(ResourceError::Truncated)?;
        let data = bytes.get(body..end).ok_or(ResourceError::Truncated)?;
        let valid_length = match tag {
            0x22 | 0x23 => len == 2 || len == 3,
            0x47 => len == 7,
            0x4b => len == 3,
            0x79 => len == 1,
            0x86 => len == 9,
            0x8a => len == 43,
            _ => return Err(ResourceError::Unsupported(tag)),
        };
        if !valid_length {
            return Err(ResourceError::InvalidLength);
        }
        match tag {
            0x79 => {
                if data[0] != 0 && bytes[..end].iter().fold(0u8, |s, b| s.wrapping_add(*b)) != 0 {
                    return Err(ResourceError::Checksum);
                }
                if bytes[end..].iter().any(|b| *b != 0) {
                    return Err(ResourceError::InvalidLength);
                }
                return Ok(out);
            }
            0x22 | 0x23 => out.push(Resource::Irq {
                mask: le(data, 0, 2)? as u16,
            })?,
            0x47 | 0x4b => {
                let offset = if tag == 0x47 { 1 } else { 0 };
                let base = le(data, offset, 2)?;
                let length = *data.last().ok_or(ResourceError::Truncated)?;
                if length == 0 || base + u64::from(length) > 65536 {
                    return Err(ResourceError::InvalidRange);
                }
                if tag == 0x47 && base > le(data, 3, 2)? {
                    return Err(ResourceError::InvalidRange);
                }
                out.push(Resource::Io {
                    base: base as u16,
                    length,
                })?;
            }
            0x86 | 0x8a => {
                let (base, length) = if tag == 0x86 {
                    (le(data, 1, 4)?, le(data, 5, 4)?)
                } else {
                    if data[0] != 0 || le(data, 27, 8)? != 0 {
                        return Err(ResourceError::Unsupported(tag));
                    }
                    let base = le(data, 11, 8)?;
                    let length = le(data, 35, 8)?;
                    if length == 0
                        || base
                            .checked_add(length - 1)
                            .is_none_or(|end| end > le(data, 19, 8).unwrap_or(0))
                    {
                        return Err(ResourceError::InvalidRange);
                    }
                    (base, length)
                };
                if length == 0
                    || base.checked_add(length).is_none()
                    || (tag == 0x86 && base + length > 0x1_0000_0000)
                {
                    return Err(ResourceError::InvalidRange);
                }
                out.push(Resource::Memory { base, length })?;
            }
            _ => return Err(ResourceError::Unsupported(tag)),
        }
        at = end;
    }
}

/// The byte that opens a name in AML.
const NAME_OP: u8 = 0x08;
/// The byte that opens a package in AML.
const PACKAGE_OP: u8 = 0x12;
/// The byte that prefixes a one-byte constant in AML.
const BYTE_PREFIX: u8 = 0x0A;

/// A sleep state the kernel may enter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepState {
    /// Suspend to RAM.
    S3,
    /// Soft off.
    S5,
}

impl SleepState {
    /// The AML name the DSDT gives this state's sleep types.
    #[must_use]
    pub const fn name(self) -> [u8; 4] {
        match self {
            Self::S3 => *b"_S3_",
            Self::S5 => *b"_S5_",
        }
    }
}

/// The first sleep type (SLP_TYPa) of the `_Sx_` package named `name`, found as a literal in
/// the AML. The name must be preceded by a name opcode and followed by a package whose first
/// element is a constant: a zero or one, or a one-byte constant. Returns `None` when no such
/// package is found.
#[must_use]
pub fn sleep_type(aml: &[u8], name: [u8; 4]) -> Option<u8> {
    for at in 1..aml.len() {
        if aml.get(at..at + 4) != Some(&name[..]) || aml.get(at - 1) != Some(&NAME_OP) {
            continue;
        }
        let package = at + 4;
        if aml.get(package) != Some(&PACKAGE_OP) {
            continue;
        }
        // The package length: its top two bits count the bytes that follow the first.
        let lead = *aml.get(package + 1)?;
        let count_at = package + 2 + usize::from(lead >> 6);
        if *aml.get(count_at)? == 0 {
            return None;
        }
        let first = count_at + 1;
        return match *aml.get(first)? {
            0x00 => Some(0),
            0x01 => Some(1),
            BYTE_PREFIX => aml.get(first + 1).copied(),
            _ => None,
        };
    }
    None
}

/// The PM1 control word that enters a sleep state: SLP_TYP in bits 12:10 and SLP_EN in bit 13.
/// Returns `None` for a type that does not fit in three bits.
#[must_use]
pub fn pm1_sleep_value(slp_typ: u8, enable: bool) -> Option<u16> {
    if slp_typ > 7 {
        return None;
    }
    let typ = u16::from(slp_typ) << 10;
    Some(typ | if enable { 1 << 13 } else { 0 })
}

/// The FADT's signature: the table that names the DSDT and the PM1 control block.
pub const FADT_SIGNATURE: [u8; 4] = *b"FACP";
/// The offset of the FADT revision byte.
const FADT_REVISION_AT: usize = 8;
/// The offset of the 32-bit DSDT address.
const FADT_DSDT_AT: usize = 40;
/// The offset of the PM1a control block's I/O port.
const FADT_PM1A_CNT_AT: usize = 64;
/// The offset of the PM1b control block's I/O port.
const FADT_PM1B_CNT_AT: usize = 68;
/// The offset of the 64-bit DSDT address, present from revision 2.
const FADT_X_DSDT_AT: usize = 140;
/// The shortest FADT that holds the PM1b field.
const FADT_MIN_LENGTH: usize = 72;
/// The shortest FADT that holds the 64-bit DSDT address.
const FADT_X_DSDT_MIN_LENGTH: usize = 148;

/// The fields of the Fixed ACPI Description Table the kernel needs to enter a sleep state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fadt {
    /// The FADT revision.
    pub revision: u8,
    /// The physical address of the DSDT, the 64-bit field when the table has one.
    pub dsdt: u64,
    /// The I/O port of the PM1a control block, where the sleep type and go bit are written.
    pub pm1a_control: u16,
    /// The I/O port of the PM1b control block, when the platform has one.
    pub pm1b_control: Option<u16>,
    /// Legacy SMI command port used to enable ACPI mode, when it fits the I/O address space.
    pub smi_command: Option<u16>,
    /// Firmware command byte for enabling ACPI mode (zero means unsupported).
    pub acpi_enable: u8,
    /// Width of each PM1 control block, in bytes (zero when absent from an old short table).
    pub pm1_control_bytes: u8,
}

/// Why a FADT was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadtError {
    /// The declared length is shorter than the fields it must hold, or longer than the bytes given.
    TooShort,
    /// The signature is not `FACP`.
    WrongSignature,
    /// The bytes of the declared length do not sum to zero.
    BadChecksum,
    /// The table names no DSDT.
    NoDsdt,
    /// The PM1a control block is missing or is not a 16-bit I/O port.
    NoPm1aControl,
}

impl Fadt {
    /// Decode a FADT. The declared length must fit in `bytes`, the bytes of that length must sum
    /// to zero, and the table must name a DSDT and a 16-bit PM1a control port.
    ///
    /// # Errors
    /// The [`FadtError`] for the first check the table fails.
    pub fn parse(bytes: &[u8]) -> Result<Self, FadtError> {
        let header = bytes.get(..8).ok_or(FadtError::TooShort)?;
        if header[..4] != FADT_SIGNATURE {
            return Err(FadtError::WrongSignature);
        }
        let declared = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if declared < FADT_MIN_LENGTH || declared > bytes.len() {
            return Err(FadtError::TooShort);
        }
        let table = &bytes[..declared];
        if table.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) != 0 {
            return Err(FadtError::BadChecksum);
        }
        let revision = table[FADT_REVISION_AT];
        let dsdt32 = u64::from(le32(table, FADT_DSDT_AT));
        let dsdt64 = if revision >= 2 && declared >= FADT_X_DSDT_MIN_LENGTH {
            le64(table, FADT_X_DSDT_AT)
        } else {
            0
        };
        let dsdt = if dsdt64 != 0 { dsdt64 } else { dsdt32 };
        if dsdt == 0 {
            return Err(FadtError::NoDsdt);
        }
        let pm1a_control = u16::try_from(le32(table, FADT_PM1A_CNT_AT))
            .ok()
            .filter(|&port| port != 0)
            .ok_or(FadtError::NoPm1aControl)?;
        let pm1b_control = u16::try_from(le32(table, FADT_PM1B_CNT_AT))
            .ok()
            .filter(|&port| port != 0);
        Ok(Self {
            revision,
            dsdt,
            pm1a_control,
            pm1b_control,
            smi_command: u16::try_from(le32(table, 48))
                .ok()
                .filter(|&port| port != 0),
            acpi_enable: table[52],
            pm1_control_bytes: table.get(89).copied().unwrap_or(0),
        })
    }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
mod tests;

/// Set only PM1 SLP_TYP/SLP_EN while retaining SCI_EN, bus-master reload and reserved bits.
#[must_use]
pub fn pm1_sleep_control(current: u16, sleep_type: u8, enable: bool) -> Option<u16> {
    Some((current & !((7 << 10) | (1 << 13))) | pm1_sleep_value(sleep_type, enable)?)
}
