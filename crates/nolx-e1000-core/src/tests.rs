//! Host tests. Each rule is checked against the register and descriptor layouts the controller uses.

use super::*;

#[test]
fn the_register_offsets_are_the_e1000_map() {
    assert_eq!(REG_RCTL, 0x100);
    assert_eq!(REG_TCTL, 0x400);
    assert_eq!(REG_RDT, REG_RDBAL + 0x18);
    assert_eq!(REG_TDT, REG_TDBAL + 0x18);
    assert_eq!(REG_RAH0, REG_RAL0 + 4);
    assert_eq!((VENDOR_INTEL, DEVICE_82540EM), (0x8086, 0x100E));
}

#[test]
fn an_eeprom_read_names_its_word_and_returns_data_once_done() {
    assert_eq!(eerd_read(0), EERD_START);
    assert_eq!(eerd_read(2), (2 << 8) | EERD_START);
    assert_eq!(eerd_result(0x1234_0000), None, "not done yet");
    assert_eq!(eerd_result(0x1234_0000 | EERD_DONE), Some(0x1234));
}

#[test]
fn the_mac_address_comes_from_three_little_endian_words() {
    assert_eq!(
        mac_from_eeprom([0x5452, 0x1200, 0x5600]),
        [0x52, 0x54, 0x00, 0x12, 0x00, 0x56]
    );
}

#[test]
fn the_receive_address_registers_hold_the_mac_with_the_valid_bit() {
    let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let (low, high) = receive_address(mac);
    assert_eq!(low, 0x1200_5452);
    assert_eq!(high, 0x5634 | RAH_VALID);
}

#[test]
fn a_ring_is_a_multiple_of_eight_descriptors_within_the_limits() {
    assert_eq!(ring_bytes(8), Some(128), "128 bytes is the hardware alignment");
    assert_eq!(ring_bytes(256), Some(4096));
    assert_eq!(ring_bytes(4), None, "below the minimum");
    assert_eq!(ring_bytes(12), None, "not a multiple of eight");
    assert_eq!(ring_bytes(RING_MAX + 8), None, "above the maximum");
}

#[test]
fn the_control_values_enable_the_engines_with_the_expected_bits() {
    assert_eq!(rctl_enable(false) & (RCTL_EN | RCTL_SECRC), RCTL_EN | RCTL_SECRC);
    assert_eq!(rctl_enable(false) & RCTL_BAM, 0);
    assert_eq!(rctl_enable(true) & RCTL_BAM, RCTL_BAM);
    let tctl = tctl_enable();
    assert_eq!(tctl & TCTL_EN, TCTL_EN);
    assert_eq!(tctl & TCTL_PSP, TCTL_PSP);
    assert_eq!((tctl & TCTL_CT) >> 4, 0x0F);
    assert_eq!((tctl & TCTL_COLD) >> 12, 0x40);
}

#[test]
fn a_receive_descriptor_holds_its_buffer_and_completes_on_done_and_end() {
    let rx = rx_descriptor(0x0123_4567_89AB_CDEF);
    assert_eq!(&rx[0..8], &0x0123_4567_89AB_CDEFu64.to_le_bytes());
    assert!(rx[8..].iter().all(|&byte| byte == 0), "the device fills the rest");
    assert!(rx_complete(DESC_DD | RX_STATUS_EOP));
    assert!(!rx_complete(DESC_DD), "a packet must end in this descriptor");
    assert!(!rx_complete(RX_STATUS_EOP), "the device has not written it back");
}

#[test]
fn a_transmit_descriptor_names_its_length_and_command() {
    let tx = tx_descriptor(0x2000, 60, true).unwrap();
    assert_eq!(&tx[0..8], &0x2000u64.to_le_bytes());
    assert_eq!(&tx[8..10], &60u16.to_le_bytes());
    assert_eq!(tx[11], TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
    let middle = tx_descriptor(0x2000, 1500, false).unwrap();
    assert_eq!(middle[11] & TX_CMD_EOP, 0, "a fragment does not end the packet");
    assert!(tx_descriptor(0x2000, 0, true).is_none());
    assert!(tx_descriptor(0x2000, TX_LENGTH_MAX + 1, true).is_none());
}

#[test]
fn a_transmit_descriptor_is_done_when_the_device_sets_done() {
    assert!(tx_done(DESC_DD));
    assert!(!tx_done(0));
}

#[test]
fn an_empty_ring_has_all_but_one_slot_free() {
    assert_eq!(tx_free(0, 0, 8), 7);
    assert_eq!(tx_free(3, 5, 8), 5, "two in flight");
    assert_eq!(tx_free(6, 4, 8), 1, "wrapped: six in flight");
    assert_eq!(tx_free(5, 4, 8), 0, "full: the slot behind the head is kept empty");
}

#[test]
fn the_interrupt_cause_and_status_bits_are_distinct() {
    assert_eq!(ICR_TXDW, 1);
    assert_eq!(ICR_LSC, 4);
    assert_eq!(ICR_RXT0, 128);
    assert_eq!(STATUS_LINK_UP, 2);
    assert_eq!(CTRL_SLU | CTRL_RST, (1 << 6) | (1 << 26));
}
