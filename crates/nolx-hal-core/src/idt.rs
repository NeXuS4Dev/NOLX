//! Long-mode Interrupt Descriptor Table gate layout.

/// Number of entries in a complete x86-64 IDT.
pub const IDT_ENTRIES: usize = 256;

/// One 16-byte, 64-bit interrupt gate descriptor.
///
/// The descriptor is packed because the architectural layout has fields at byte rather than
/// naturally aligned offsets. Methods read fields by value; callers should not take a
/// reference to an individual field.
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub struct IdtGate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtGate {
    /// A missing gate, suitable only before the live table is installed.
    pub const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    /// Build a present ring-`dpl` interrupt gate with an optional IST stack index.
    ///
    /// `dpl` is restricted to its architectural two bits, and `ist` to its three bits.
    /// The long-mode interrupt-gate type is `0xE`; the present bit is set.
    #[must_use]
    pub const fn interrupt(handler: u64, selector: u16, ist: u8, dpl: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist: ist & 0x07,
            attributes: 0x8E | ((dpl & 0x03) << 5),
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    /// Reconstruct the 64-bit handler address.
    #[must_use]
    pub const fn handler(&self) -> u64 {
        (self.offset_low as u64)
            | ((self.offset_middle as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    /// The code-segment selector used by this gate.
    #[must_use]
    pub const fn selector(&self) -> u16 {
        self.selector
    }

    /// The IST index, in the architectural range `0..=7`.
    #[must_use]
    pub const fn ist(&self) -> u8 {
        self.ist
    }

    /// The gate type, DPL, and present bits.
    #[must_use]
    pub const fn attributes(&self) -> u8 {
        self.attributes
    }
}

const _: () = assert!(core::mem::size_of::<IdtGate>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_layout_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<IdtGate>(), 16);
        assert_eq!(core::mem::align_of::<IdtGate>(), 1);
    }

    #[test]
    fn gate_splits_and_reassembles_a_canonical_handler_address() {
        let address = 0xFFFF_8000_1234_5678;
        let gate = IdtGate::interrupt(address, 0x08, 0, 0);
        assert_eq!(gate.handler(), address);
        assert_eq!(gate.selector(), 0x08);
    }

    #[test]
    fn gate_encodes_the_interrupt_type_privilege_and_ist() {
        let gate = IdtGate::interrupt(0x1234_5678_9ABC_DEF0, 0x08, 0x0A, 0xFF);
        assert_eq!(gate.ist(), 2);
        assert_eq!(gate.attributes(), 0xEE);
    }
}
