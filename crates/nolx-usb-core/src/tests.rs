//! Host tests. Each rule is checked against the layout the xHCI and HID specifications give.

use super::*;

#[test]
fn port_register_sets_sit_sixteen_bytes_apart_past_the_operational_base() {
    assert_eq!(port_register_offset(0x20, 1), Some(0x420));
    assert_eq!(port_register_offset(0x20, 3), Some(0x440));
    assert_eq!(port_register_offset(0x20, 0), None, "ports count from 1");
}

#[test]
fn hcsparams1_gives_the_port_and_slot_counts() {
    let hcs = (4u32 << 24) | (8u32 << 8) | 32;
    assert_eq!(max_ports(hcs), 4);
    assert_eq!(max_slots(hcs), 32);
}

#[test]
fn doorbells_are_four_bytes_apart_and_the_offset_drops_its_low_bits() {
    assert_eq!(doorbell_offset(0x2000, 0), 0x2000);
    assert_eq!(doorbell_offset(0x2003, 2), 0x2008, "bits 1:0 are reserved");
}

#[test]
fn a_port_reset_keeps_the_port_and_clears_nothing_by_accident() {
    let portsc = PORTSC_CONNECTED | PORTSC_ENABLED | PORTSC_POWER | PORTSC_CONNECT_CHANGE;
    let written = portsc_start_reset(portsc);
    assert_eq!(written & PORTSC_RESET, PORTSC_RESET);
    assert_eq!(written & PORTSC_ENABLED, 0, "writing 1 to enabled would disable the port");
    assert_eq!(written & PORTSC_CONNECT_CHANGE, 0, "the change bit is not re-written as set");
    assert_eq!(written & PORTSC_POWER, PORTSC_POWER);
}

#[test]
fn acknowledging_writes_back_only_the_change_bits_that_are_set() {
    let portsc = PORTSC_CONNECTED | PORTSC_POWER | PORTSC_CONNECT_CHANGE;
    let written = portsc_acknowledge(portsc);
    assert_eq!(written & PORTSC_CHANGE_BITS, PORTSC_CONNECT_CHANGE, "only the set change bit");
    assert_eq!(written & PORTSC_ENABLED, 0, "the enable bit is not written as a 1");
    assert_eq!(written & PORTSC_POWER, PORTSC_POWER, "power is kept");
    assert_eq!(portsc_acknowledge(PORTSC_CONNECTED) & PORTSC_CHANGE_BITS, 0, "no change to clear");
}

#[test]
fn a_link_trb_names_its_target_and_keeps_the_cycle_and_toggle_bits() {
    let trb = link_trb(0x1_0000, true, true).unwrap();
    assert_eq!(trb_type(&trb), TRB_LINK);
    assert!(cycle_bit(&trb));
    assert_eq!(&trb[0..8], &0x1_0000u64.to_le_bytes());
    assert_eq!(word(&trb, 12) & 0b10, 0b10, "toggle cycle is bit 1");
    assert!(link_trb(0x1_0008, false, false).is_none(), "targets are 16-byte aligned");
}

#[test]
fn a_normal_trb_carries_its_buffer_length_and_completion_flag() {
    let trb = normal_trb(0x4000, 512, true, false).unwrap();
    assert_eq!(trb_type(&trb), TRB_NORMAL);
    assert_eq!(&trb[0..8], &0x4000u64.to_le_bytes());
    assert_eq!(&trb[8..12], &512u32.to_le_bytes());
    assert_eq!(word(&trb, 12) & (1 << 5), 1 << 5, "interrupt on completion");
    assert!(!cycle_bit(&trb));
    assert!(normal_trb(0x4000, TRB_LENGTH_MAX + 1, false, true).is_none());
}

#[test]
fn an_enable_slot_command_has_its_type_and_cycle() {
    let trb = enable_slot_trb(true);
    assert_eq!(trb_type(&trb), TRB_ENABLE_SLOT);
    assert!(cycle_bit(&trb));
}

#[test]
fn an_event_gives_its_completion_code_slot_and_port() {
    // Command completion: status dword holds the code, control dword the slot.
    let mut event = [0u8; 16];
    event[8..12].copy_from_slice(&((u32::from(CC_SUCCESS)) << 24).to_le_bytes());
    event[12..16].copy_from_slice(&((u32::from(TRB_COMMAND_COMPLETION) << 10) | (3 << 24) | 1).to_le_bytes());
    assert_eq!(trb_type(&event), TRB_COMMAND_COMPLETION);
    assert_eq!(completion_code(&event), CC_SUCCESS);
    assert_eq!(event_slot(&event), 3);
    // Port status change: the port is in the first dword.
    let mut port = [0u8; 16];
    port[0..4].copy_from_slice(&(3u32 << 24).to_le_bytes());
    port[12..16].copy_from_slice(&(u32::from(TRB_PORT_STATUS_CHANGE) << 10).to_le_bytes());
    assert_eq!(event_port(&port), 3);
    assert_eq!(trb_type(&port), TRB_PORT_STATUS_CHANGE);
}

/// A keyboard report with the given modifier byte and held keys.
fn report(modifiers: u8, keys: &[u8]) -> BootReport {
    let mut out = [0u8; 8];
    out[0] = modifiers;
    out[2..2 + keys.len()].copy_from_slice(keys);
    out
}

#[test]
fn an_unchanged_report_produces_no_events() {
    let held = report(0, &[0x04]);
    assert!(report_events(&held, &held).unwrap().is_empty());
}

#[test]
fn a_new_key_presses_and_a_dropped_key_releases() {
    let got = report_events(&report(0, &[]), &report(0, &[0x04])).unwrap();
    assert_eq!(got.iter().collect::<Vec<_>>(), [KeyEvent { usage: 0x04, pressed: true }]);
    let got = report_events(&report(0, &[0x04]), &report(0, &[])).unwrap();
    assert_eq!(got.iter().collect::<Vec<_>>(), [KeyEvent { usage: 0x04, pressed: false }]);
}

#[test]
fn a_modifier_bit_gives_its_usage_in_the_e_range() {
    let got = report_events(&report(0, &[]), &report(0x02, &[])).unwrap();
    assert_eq!(got.iter().collect::<Vec<_>>(), [KeyEvent { usage: 0xE1, pressed: true }], "left Shift");
    let got = report_events(&report(0x80, &[]), &report(0, &[])).unwrap();
    assert_eq!(got.iter().collect::<Vec<_>>(), [KeyEvent { usage: 0xE7, pressed: false }], "right GUI");
}

#[test]
fn releases_come_before_presses_and_modifiers_first_of_all() {
    let before = report(0x00, &[0x04]);
    let after = report(0x01, &[0x05]);
    let got: Vec<KeyEvent> = report_events(&before, &after).unwrap().iter().collect();
    assert_eq!(
        got,
        [
            KeyEvent { usage: 0xE0, pressed: true },
            KeyEvent { usage: 0x04, pressed: false },
            KeyEvent { usage: 0x05, pressed: true },
        ]
    );
}

#[test]
fn a_roll_over_report_is_not_acted_on() {
    let before = report(0, &[0x04]);
    let roll_over = report(0, &[ERROR_ROLL_OVER]);
    assert!(report_events(&before, &roll_over).is_none());
}

#[test]
fn the_reserved_codes_below_four_are_not_keys() {
    let got = report_events(&report(0, &[]), &report(0, &[0x00, 0x02, 0x03])).unwrap();
    assert!(got.is_empty(), "codes 0 to 3 are not keys");
}
