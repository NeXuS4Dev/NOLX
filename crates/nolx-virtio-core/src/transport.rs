//! Shared modern virtio queue admission and shutdown proof.
/// Choose the bounded block queue layout before exposing any DMA address. Three
/// descriptors are needed for header/data/status, so one- and two-entry rings refuse.
/// The selected power-of-two ring must fit one 4 KiB owner page.
pub fn block_queue_layout(maximum: u16) -> Result<crate::SplitLayout, crate::VirtioError> {
    let size = maximum.min(8);
    if size < 4 {
        return Err(crate::VirtioError::InvalidQueueSize);
    }
    let layout = crate::split_layout(size)?;
    if layout.total_bytes > 4096 {
        return Err(crate::VirtioError::InvalidQueueSize);
    }
    Ok(layout)
}

/// Final readback filter, not a hardware operation: reset, PCI DMA/INTx exclusion and
/// verified disabled PCI message delivery are all required before exposed pages can be freed.
pub fn stop_proven(status: u8, command: u32, messages_disabled: bool) -> bool {
    status == 0 && command & ((1 << 2) | (1 << 10)) == 1 << 10 && messages_disabled
}
#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn block_queue_admission_rejects_short_or_invalid_descriptor_tables() {
        for maximum in 0..=u16::MAX {
            let size = maximum.min(8);
            let result = block_queue_layout(maximum);
            assert_eq!(result.is_ok(), size >= 4 && size.is_power_of_two());
            if let Ok(layout) = result {
                assert_eq!(layout.size, size);
                assert!(layout.total_bytes <= 4096);
            }
        }
    }
    #[test]
    fn block_queue_has_room_for_header_data_and_status_before_publication() {
        assert!(block_queue_layout(1).is_err());
        assert!(block_queue_layout(2).is_err());
        assert_eq!(block_queue_layout(4).unwrap().size, 4);
        assert_eq!(block_queue_layout(u16::MAX).unwrap().size, 8);
    }
    #[test]
    fn every_nonzero_status_and_missing_exclusion_proof_refuses() {
        for status in 0..=255 {
            for bm in [false, true] {
                for intx in [false, true] {
                    for messages in [false, true] {
                        assert_eq!(
                            stop_proven(
                                status,
                                (u32::from(bm) << 2) | (u32::from(intx) << 10),
                                messages
                            ),
                            status == 0 && !bm && intx && messages
                        );
                    }
                }
            }
        }
        assert!(!stop_proven(255, u32::MAX, true));
    }
    #[test]
    fn unrelated_pci_bits_do_not_authorize_dma_or_interrupt_delivery() {
        assert!(stop_proven(0, !4, true));
        assert!(!stop_proven(0, 0, true));
        assert!(!stop_proven(0, 1 << 10, false));
    }
}
