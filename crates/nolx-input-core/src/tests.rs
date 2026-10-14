//! Host tests. Each rule is checked against the byte values the controller and keyboard use.

use super::*;

/// Decode a byte sequence in order and collect the key events it completes.
fn events(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut decoder = Decoder::new();
    bytes
        .iter()
        .filter_map(|&byte| decoder.feed(byte))
        .collect()
}

#[test]
fn controller_status_bits_say_what_the_cpu_may_do() {
    assert!(output_full(STATUS_OUTPUT_FULL));
    assert!(!output_full(STATUS_INPUT_FULL));
    assert!(input_full(STATUS_INPUT_FULL));
    assert!(!input_full(STATUS_OUTPUT_FULL));
    assert!(transfer_failed(STATUS_TIMEOUT));
    assert!(transfer_failed(STATUS_PARITY));
    assert!(!transfer_failed(STATUS_OUTPUT_FULL | STATUS_INPUT_FULL));
}

#[test]
fn keyboard_config_enables_the_irq_and_translation_and_keeps_the_clock_on() {
    assert_eq!(keyboard_config(0x00), 0x41);
    assert_eq!(keyboard_config(CONFIG_KEYBOARD_CLOCK_OFF), 0x41);
    assert_eq!(keyboard_config(0x02), 0x43, "the mouse IRQ bit is kept");
}

#[test]
fn device_replies_are_classified() {
    assert_eq!(classify_reply(0xFA), DeviceReply::Ack);
    assert_eq!(classify_reply(0xFE), DeviceReply::Resend);
    assert_eq!(classify_reply(0xAA), DeviceReply::SelfTestOk);
    assert_eq!(classify_reply(0x1E), DeviceReply::Other);
}

#[test]
fn a_make_code_presses_and_its_break_code_releases_the_same_usage() {
    let got = events(&[0x1E, 0x9E]);
    assert_eq!(
        got,
        [
            KeyEvent {
                usage: 0x04,
                pressed: true
            },
            KeyEvent {
                usage: 0x04,
                pressed: false
            },
        ]
    );
}

#[test]
fn letters_digits_and_controls_map_to_hid_usages() {
    assert_eq!(usage(0x1E, false), Some(0x04), "A");
    assert_eq!(usage(0x10, false), Some(0x14), "Q");
    assert_eq!(usage(0x02, false), Some(0x1E), "1");
    assert_eq!(usage(0x0B, false), Some(0x27), "0");
    assert_eq!(usage(0x1C, false), Some(0x28), "Enter");
    assert_eq!(usage(0x01, false), Some(0x29), "Escape");
    assert_eq!(usage(0x39, false), Some(0x2C), "Space");
    assert_eq!(usage(0x2A, false), Some(0xE1), "left Shift");
    assert_eq!(usage(0x3B, false), Some(0x3A), "F1");
    assert_eq!(usage(0x58, false), Some(0x45), "F12");
}

#[test]
fn extended_keys_need_the_prefix_and_map_to_their_own_usages() {
    let got = events(&[0xE0, 0x48, 0xE0, 0xC8]);
    assert_eq!(
        got,
        [
            KeyEvent {
                usage: 0x52,
                pressed: true
            },
            KeyEvent {
                usage: 0x52,
                pressed: false
            },
        ],
        "Up, pressed and released"
    );
    assert_eq!(usage(0x1D, true), Some(0xE4), "right Ctrl");
    assert_eq!(
        usage(0x1D, false),
        Some(0xE0),
        "left Ctrl without the prefix"
    );
    assert_eq!(usage(0x53, true), Some(0x4C), "Delete");
}

#[test]
fn pause_is_consumed_and_decoding_resumes_after_it() {
    let got = events(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E]);
    assert_eq!(
        got,
        [KeyEvent {
            usage: 0x04,
            pressed: true
        }],
        "Pause reports nothing, and the A after it still does"
    );
}

#[test]
fn print_screen_fake_shifts_and_unknown_codes_report_nothing() {
    // Print Screen: E0 2A E0 37 to press, E0 B7 E0 AA to release.
    let got = events(&[0xE0, 0x2A, 0xE0, 0x37, 0xE0, 0xB7, 0xE0, 0xAA, 0x1E]);
    assert_eq!(
        got,
        [KeyEvent {
            usage: 0x04,
            pressed: true
        }],
        "only the A after the sequence is reported"
    );
    assert_eq!(usage(0x7F, false), None);
    assert!(
        events(&[0xFA, 0xFE]).is_empty(),
        "acknowledge and resend are not keys"
    );
}
