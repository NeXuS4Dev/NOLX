//! Message-signalled interrupt rules: the MSI and MSI-X capability layouts, the message address
//! and data a function is programmed with, the MSI-X table entry, and whether a table fits in its
//! BAR. These are decisions over bytes. The kernel writes the registers and the table.
//!
//! The capability offsets are relative to the capability header found by the walk in
//! [`crate::capabilities`]. Vectors below 32 belong to the processor's exceptions and are refused.

/// Capability ID of MSI.
pub const CAP_MSI: u8 = 0x05;
/// Capability ID of MSI-X.
pub const CAP_MSI_X: u8 = 0x11;

/// The base of the local APIC message window. A message to this range with a destination in
/// bits 19:12 is delivered to that local APIC.
pub const MSI_ADDRESS_BASE: u32 = 0xFEE0_0000;
/// The lowest vector a message may carry.
pub const VECTOR_MIN: u8 = 32;

/// MSI Message Control: enable (bit 0), multiple-message capable (bits 3:1), multiple-message
/// enable (bits 6:4), 64-bit address capable (bit 7), per-vector masking capable (bit 8).
pub const MSI_CTRL_ENABLE: u16 = 1 << 0;
const MSI_CTRL_MMC_SHIFT: u16 = 1;
const MSI_CTRL_MME_SHIFT: u16 = 4;
const MSI_CTRL_MME_MASK: u16 = 0b111 << MSI_CTRL_MME_SHIFT;
/// Bit 7 of MSI Message Control: the function takes a 64-bit message address.
pub const MSI_CTRL_64BIT: u16 = 1 << 7;
/// Bit 8 of MSI Message Control: the function has per-vector mask bits.
pub const MSI_CTRL_PER_VECTOR_MASK: u16 = 1 << 8;

/// MSI-X Message Control: table size minus one (bits 10:0), function mask (bit 14), enable
/// (bit 15).
const MSIX_CTRL_TABLE_MASK: u16 = 0x7FF;
/// Bit 14 of MSI-X Message Control: mask every vector of the function.
pub const MSIX_CTRL_FUNCTION_MASK: u16 = 1 << 14;
/// Bit 15 of MSI-X Message Control: MSI-X is enabled.
pub const MSIX_CTRL_ENABLE: u16 = 1 << 15;
/// Bytes in one MSI-X table entry: address low, address high, data, vector control.
pub const MSIX_ENTRY_BYTES: u64 = 16;
/// Bit 0 of the vector control word: this vector is masked.
pub const MSIX_VECTOR_MASKED: u32 = 1;

/// What the MSI capability says about the function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiCapability {
    /// The function takes a 64-bit message address.
    pub is_64bit: bool,
    /// The function has per-vector mask bits.
    pub per_vector_mask: bool,
    /// The most vectors the function can ask for: 1 to 32, or 0 for a reserved encoding.
    pub max_vectors: u8,
}

/// Decode MSI Message Control. The multiple-message-capable field has encodings 0 to 5; 6 and 7
/// are reserved and report no vectors at all.
#[must_use]
pub fn parse_msi(control: u16) -> MsiCapability {
    let mmc = (control >> MSI_CTRL_MMC_SHIFT) & 0b111;
    MsiCapability {
        is_64bit: control & MSI_CTRL_64BIT != 0,
        per_vector_mask: control & MSI_CTRL_PER_VECTOR_MASK != 0,
        max_vectors: if mmc <= 5 { 1 << mmc } else { 0 },
    }
}

/// The offsets of the MSI registers from the capability header. The message data sits after
/// the upper address word when the function is 64-bit, and the mask bits after the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiLayout {
    /// Offset of the upper address dword, present only for a 64-bit function.
    pub address_high: Option<u8>,
    /// Offset of the message data word.
    pub data: u8,
    /// Offset of the mask bits, present only with per-vector masking.
    pub mask: Option<u8>,
}

/// The register layout a function with this Message Control word has.
#[must_use]
pub fn msi_layout(control: u16) -> MsiLayout {
    let cap = parse_msi(control);
    let (data, mask) = if cap.is_64bit { (12, 16) } else { (8, 12) };
    MsiLayout {
        address_high: cap.is_64bit.then_some(8),
        data,
        mask: cap.per_vector_mask.then_some(mask),
    }
}

/// Message Control to write when the driver wants `vectors` MSI vectors and the function enabled.
/// `vectors` must be a power of two no larger than the function's capability. Returns `None`
/// otherwise. Other control bits are kept as they were.
#[must_use]
pub fn msi_enable_control(control: u16, vectors: u8) -> Option<u16> {
    let cap = parse_msi(control);
    if vectors == 0 || !vectors.is_power_of_two() || vectors > cap.max_vectors {
        return None;
    }
    let mme = u16::from(vectors.trailing_zeros() as u8);
    let kept = control & !(MSI_CTRL_MME_MASK | MSI_CTRL_ENABLE);
    Some(kept | (mme << MSI_CTRL_MME_SHIFT) | MSI_CTRL_ENABLE)
}

/// The message address for fixed delivery to the local APIC `apic_id`.
#[must_use]
pub fn message_address(apic_id: u8) -> u32 {
    MSI_ADDRESS_BASE | (u32::from(apic_id) << 12)
}

/// The message data for fixed, edge-triggered delivery of `vector`. Returns `None` for a vector
/// below [`VECTOR_MIN`].
#[must_use]
pub fn message_data(vector: u8) -> Option<u16> {
    (vector >= VECTOR_MIN).then_some(u16::from(vector))
}

/// What the MSI-X capability says about the function: the table and pending-bit array, each
/// given as the BAR it sits in and its byte offset inside that BAR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixCapability {
    /// The number of table entries, 1 to 2048.
    pub table_size: u16,
    /// The BAR that holds the table, 0 to 5.
    pub table_bar: u8,
    /// The table's byte offset inside that BAR, 8-byte aligned.
    pub table_offset: u32,
    /// The BAR that holds the pending-bit array, 0 to 5.
    pub pba_bar: u8,
    /// The pending-bit array's byte offset inside that BAR, 8-byte aligned.
    pub pba_offset: u32,
}

/// Decode the MSI-X capability from its Message Control word and its two offset dwords.
#[must_use]
pub fn parse_msix(control: u16, table: u32, pba: u32) -> MsixCapability {
    MsixCapability {
        table_size: (control & MSIX_CTRL_TABLE_MASK) + 1,
        table_bar: (table & 0b111) as u8,
        table_offset: table & !0b111,
        pba_bar: (pba & 0b111) as u8,
        pba_offset: pba & !0b111,
    }
}

/// Bytes of pending-bit array for a table of `table_size` entries: one bit per entry, rounded up
/// to whole qwords.
#[must_use]
pub fn pba_bytes(table_size: u16) -> u64 {
    u64::from(table_size).div_ceil(64) * 8
}

/// Whether the table and the pending-bit array both fit inside a BAR of `bar_len` bytes, and
/// both BAR numbers are real BARs. The kernel checks the BAR's own size with this.
#[must_use]
pub fn msix_fits(cap: &MsixCapability, bar_len: u64) -> bool {
    let table_end = u64::from(cap.table_offset) + u64::from(cap.table_size) * MSIX_ENTRY_BYTES;
    let pba_end = u64::from(cap.pba_offset) + pba_bytes(cap.table_size);
    cap.table_bar < 6 && cap.pba_bar < 6 && table_end <= bar_len && pba_end <= bar_len
}

/// Message Control to write when the driver enables MSI-X and unmasks the function. The table
/// size bits are kept.
#[must_use]
pub fn msix_enable(control: u16) -> u16 {
    (control | MSIX_CTRL_ENABLE) & !MSIX_CTRL_FUNCTION_MASK
}

/// One MSI-X table entry: the 64-bit message address, the message data, and the vector control
/// word with its mask bit. Returns `None` when the address is not dword-aligned or the vector
/// is below [`VECTOR_MIN`].
#[must_use]
pub fn msix_entry(address: u64, vector: u8, masked: bool) -> Option<[u8; 16]> {
    if address & 0b11 != 0 {
        return None;
    }
    let data = u32::from(message_data(vector)?);
    let control = if masked { MSIX_VECTOR_MASKED } else { 0 };
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&(address as u32).to_le_bytes());
    out[4..8].copy_from_slice(&((address >> 32) as u32).to_le_bytes());
    out[8..12].copy_from_slice(&data.to_le_bytes());
    out[12..16].copy_from_slice(&control.to_le_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_address_names_the_local_apic_and_data_names_the_vector() {
        assert_eq!(message_address(3), 0xFEE0_3000);
        assert_eq!(message_address(0xFF), 0xFEEF_F000);
        assert_eq!(message_data(0x31), Some(0x31));
        assert_eq!(message_data(31), None, "exceptions are not device vectors");
    }

    #[test]
    fn msi_control_reports_64_bit_masking_and_the_vector_capacity() {
        let plain = parse_msi(0x0000);
        assert!(!plain.is_64bit && !plain.per_vector_mask);
        assert_eq!(plain.max_vectors, 1);
        let full = parse_msi((3 << 1) | MSI_CTRL_64BIT | MSI_CTRL_PER_VECTOR_MASK);
        assert!(full.is_64bit && full.per_vector_mask);
        assert_eq!(full.max_vectors, 8);
        assert_eq!(parse_msi(6 << 1).max_vectors, 0, "encoding 6 is reserved");
    }

    #[test]
    fn msi_enable_keeps_other_bits_and_refuses_bad_counts() {
        // 64-bit, capable of 4 vectors: asking for 2 sets the enable bit and MME = 1.
        let control = 0x0080 | (2 << 1);
        assert_eq!(
            msi_enable_control(control, 2),
            Some(0x0080 | (2 << 1) | 0x10 | 0x01)
        );
        assert_eq!(msi_enable_control(control, 3), None, "not a power of two");
        assert_eq!(
            msi_enable_control(control, 8),
            None,
            "more than the function has"
        );
        assert_eq!(msi_enable_control(control, 0), None);
    }

    #[test]
    fn msi_layout_moves_data_and_mask_with_the_address_width() {
        let narrow = msi_layout(0x0000);
        assert_eq!(
            narrow,
            MsiLayout {
                address_high: None,
                data: 8,
                mask: None
            }
        );
        let wide_masked = msi_layout(MSI_CTRL_64BIT | MSI_CTRL_PER_VECTOR_MASK);
        assert_eq!(
            wide_masked,
            MsiLayout {
                address_high: Some(8),
                data: 12,
                mask: Some(16)
            }
        );
        let narrow_masked = msi_layout(MSI_CTRL_PER_VECTOR_MASK);
        assert_eq!(narrow_masked.mask, Some(12));
    }

    #[test]
    fn msix_control_gives_the_table_size_and_its_bar_offsets() {
        // Table size 8 (encoded 7), enabled. Table at BAR 3 offset 0x2000, PBA at BAR 1 0x3000.
        let cap = parse_msix(0x8007, 0x0000_2003, 0x0000_3001);
        assert_eq!(cap.table_size, 8);
        assert_eq!((cap.table_bar, cap.table_offset), (3, 0x2000));
        assert_eq!((cap.pba_bar, cap.pba_offset), (1, 0x3000));
    }

    #[test]
    fn pba_is_one_bit_per_entry_rounded_to_qwords() {
        assert_eq!(pba_bytes(1), 8);
        assert_eq!(pba_bytes(64), 8);
        assert_eq!(pba_bytes(65), 16);
        assert_eq!(pba_bytes(2048), 256);
    }

    #[test]
    fn a_table_must_fit_inside_its_bar() {
        let cap = parse_msix(0x8007, 0x0000_2003, 0x0000_3001);
        assert!(msix_fits(&cap, 0x4000));
        assert!(
            !msix_fits(&cap, 0x3000),
            "the pending-bit array runs past the BAR"
        );
        let bad_bar = parse_msix(0x8007, 0x0000_2006, 0x0000_3001);
        assert!(!msix_fits(&bad_bar, 0x4000), "BAR 6 does not exist");
    }

    #[test]
    fn msix_enable_sets_enable_and_clears_function_mask() {
        assert_eq!(msix_enable(0x4007), 0x8007);
        assert_eq!(msix_enable(0x0007), 0x8007);
    }

    #[test]
    fn an_msix_entry_holds_address_data_and_mask_in_its_fields() {
        let entry = msix_entry(0xFEE0_1000, 0x41, true).unwrap();
        assert_eq!(&entry[0..4], &0xFEE0_1000u32.to_le_bytes());
        assert_eq!(&entry[4..8], &0u32.to_le_bytes());
        assert_eq!(&entry[8..12], &0x41u32.to_le_bytes());
        assert_eq!(&entry[12..16], &1u32.to_le_bytes());
        let open = msix_entry(0x1_0000_0000 | 0xFEE0_1000, 0x41, false).unwrap();
        assert_eq!(&open[4..8], &1u32.to_le_bytes());
        assert_eq!(&open[12..16], &0u32.to_le_bytes());
    }

    #[test]
    fn an_msix_entry_refuses_unaligned_addresses_and_low_vectors() {
        assert!(msix_entry(0xFEE0_1002, 0x41, false).is_none());
        assert!(msix_entry(0xFEE0_1000, 0x1F, false).is_none());
    }
}
