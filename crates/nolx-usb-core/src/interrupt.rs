//! xHCI primary-interrupter acknowledgement and bounded event-ring dequeue rules.
/// USBCMD interrupt enable (INTE).
pub const COMMAND_ENABLE: u32 = 1 << 2;
/// USBSTS Event Interrupt (EINT), write one to clear.
pub const STATUS_EVENT: u32 = 1 << 3;
/// IMAN interrupt pending (IP), write one to clear; MSI may clear it automatically.
pub const PENDING: u32 = 1;
/// IMAN interrupt enable (IE).
pub const ENABLE: u32 = 1 << 1;
/// ERDP Event Handler Busy (EHB), write one to clear and recheck queued events.
pub const BUSY: u64 = 1 << 3;
/// Claim only the event-interrupt status bit, never acknowledging unrelated W1C status.
/// IP alone is not required: message-signalled delivery may already have cleared it.
#[must_use]
pub const fn acknowledge(status: u32) -> Option<u32> {
    if status & STATUS_EVENT != 0 {
        Some(STATUS_EVENT)
    } else {
        None
    }
}
/// A 256-entry, one-page event ring has aligned 16-byte entries. Busy clears only at batch end.
#[must_use]
pub const fn dequeue(page: u64, index: u16, complete: bool) -> Option<u64> {
    if page & 63 != 0 || index >= 256 {
        return None;
    }
    match page.checked_add(index as u64 * 16) {
        Some(pointer) => Some(pointer | if complete { BUSY } else { 0 }),
        None => None,
    }
}
/// Restrict MSI-X table/PBA to disjoint tail ranges beyond the standard controller register blocks.
/// Both must fit the existing owned register window; no unchecked alias or overlapping MMIO.
#[must_use]
pub const fn message_table_window(
    register_end: u64,
    table: u64,
    table_bytes: u64,
    pba: u64,
    pba_bytes: u64,
    window: u64,
) -> bool {
    if table < register_end
        || pba < register_end
        || table_bytes == 0
        || pba_bytes == 0
        || table & 7 != 0
        || pba & 7 != 0
    {
        return false;
    }
    let Some(table_end) = table.checked_add(table_bytes) else {
        return false;
    };
    let Some(pba_end) = pba.checked_add(pba_bytes) else {
        return false;
    };
    table_end <= window && pba_end <= window && (table_end <= pba || pba_end <= table)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_ack_never_clears_other_status_and_does_not_require_pending_ip() {
        assert_eq!(acknowledge(0), None);
        assert_eq!(acknowledge(!STATUS_EVENT), None);
        assert_eq!(acknowledge(u32::MAX), Some(STATUS_EVENT));
        assert_eq!(acknowledge(STATUS_EVENT), Some(STATUS_EVENT));
        assert_eq!(PENDING & ENABLE, 0);
    }
    #[test]
    fn busy_is_cleared_only_when_the_consumer_finishes_its_batch() {
        for index in 0..256 {
            assert_eq!(
                dequeue(0x1000, index, false),
                Some(0x1000 + u64::from(index) * 16)
            );
            assert_eq!(
                dequeue(0x1000, index, true),
                Some(0x1008 + u64::from(index) * 16)
            );
        }
    }
    #[test]
    fn invalid_ring_offsets_cannot_escape_the_dma_page_or_wrap() {
        assert_eq!(dequeue(0x1001, 0, true), None);
        assert_eq!(dequeue(0x1000, 256, true), None);
        assert_eq!(dequeue(u64::MAX & !63, 4, true), None);
    }
    #[test]
    fn message_tables_cannot_alias_registers_each_other_or_escape_the_owned_window() {
        assert!(message_table_window(0x2404, 0x3000, 256, 0x3800, 8, 0x4000));
        assert!(message_table_window(0x2404, 0x3f00, 256, 0x3800, 8, 0x4000));
        assert!(!message_table_window(
            0x2404, 0x2000, 256, 0x3800, 8, 0x4000
        ));
        assert!(!message_table_window(
            0x2404, 0x3000, 256, 0x3008, 8, 0x4000
        ));
        assert!(!message_table_window(
            0x2404, 0x3f08, 256, 0x3800, 8, 0x4000
        ));
        assert!(!message_table_window(
            0x2404,
            u64::MAX & !7,
            256,
            0x3800,
            8,
            u64::MAX
        ));
        assert!(!message_table_window(
            0x2404, 0x3001, 256, 0x3800, 8, 0x4000
        ));
        assert!(!message_table_window(
            0x2404, 0x3000, 256, 0x3800, 0, 0x4000
        ));
    }
}
