//! Long-mode GDT system-segment descriptor encoding.

/// Encode the two 64-bit words of an available x86-64 TSS descriptor.
///
/// The descriptor's limit must fit the architectural 20-bit limit field. NOLX uses a
/// 104-byte TSS, so the current limit is `103` and the granularity flag is clear.
#[must_use]
pub const fn tss_descriptor(base: u64, limit: u32) -> [u64; 2] {
    let limit = limit as u64;
    let low = (limit & 0xFFFF)
        | ((base & 0xFFFF) << 16)
        | (((base >> 16) & 0xFF) << 32)
        | (0x89 << 40) // present, DPL 0, available 64-bit TSS
        | (((limit >> 16) & 0x0F) << 48)
        | (((base >> 24) & 0xFF) << 56);
    [low, base >> 32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_tss_descriptor_encodes_base_limit_and_access_byte() {
        let base = 0xFEDC_BA98_7654_3210;
        let limit = 0xABCDE;
        let [low, high] = tss_descriptor(base, limit);

        assert_eq!(low & 0xFFFF, u64::from(limit & 0xFFFF));
        assert_eq!((low >> 16) & 0xFFFF, base & 0xFFFF);
        assert_eq!((low >> 32) & 0xFF, (base >> 16) & 0xFF);
        assert_eq!((low >> 40) & 0xFF, 0x89);
        assert_eq!((low >> 48) & 0x0F, u64::from((limit >> 16) & 0x0F));
        assert_eq!((low >> 56) & 0xFF, (base >> 24) & 0xFF);
        assert_eq!(high, base >> 32);
    }
}
