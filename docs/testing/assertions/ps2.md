# Assertions: PS/2 keyboard controller and scancode set 1 rules

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, Input) and `crates/nolx-input-core`. The
rules cover the controller's status and configuration bits, the device replies that matter during
initialisation, and the scancode set 1 decoder that turns keyboard bytes into USB HID usage codes.
The kernel reads and writes the ports and polls them; that wiring is a later slice.

The port numbers (0x60 and 0x64), the status and command bits, the configuration bits, the
scancode set 1 make and break codes, the E0 and E1 prefixes, the Pause sequence, and the HID usage
values are from the IBM PC AT keyboard interface and the USB HID usage tables, as recalled and not
yet checked against the documents. Verify them before the rows are promoted.

Rows are pipe-delimited: `ID | specification reference | status | test`.

PS2-001 | NOLX M9 Controller status: output-full, input-full, timeout, and parity are read from their status bits, and a timeout or parity error marks a byte as untrustworthy | implemented | nolx-input-core tests::controller_status_bits_say_what_the_cpu_may_do
PS2-002 | NOLX M9 Keyboard configuration: the configuration byte enables the keyboard interrupt and set 1 translation, keeps the keyboard clock running, and keeps the other bits | implemented | nolx-input-core tests::keyboard_config_enables_the_irq_and_translation_and_keeps_the_clock_on
PS2-003 | NOLX M9 Device replies: acknowledge (0xFA), resend (0xFE), and self-test passed (0xAA) are classified; any other byte is a key or an unawaited reply | implemented | nolx-input-core tests::device_replies_are_classified
PS2-004 | NOLX M9 Make and break codes: a make code presses its key, and the same code with bit 7 set releases it, with the same HID usage | implemented | nolx-input-core tests::a_make_code_presses_and_its_break_code_releases_the_same_usage
PS2-005 | NOLX M9 Key table: letters, digits, the standard controls, the modifiers, and the function keys map to their HID usage codes | implemented | nolx-input-core tests::letters_digits_and_controls_map_to_hid_usages
PS2-006 | NOLX M9 Extended keys: a key after the E0 prefix uses the extended table, so the arrows, Delete, and the right-hand Ctrl and Alt are distinct from their unprefixed codes | implemented | nolx-input-core tests::extended_keys_need_the_prefix_and_map_to_their_own_usages
PS2-007 | NOLX M9 Pause and Print Screen: the Pause sequence is consumed in full and decoding resumes after it; the Print Screen fake-shift sequences report no key, and an unknown code reports nothing | implemented | nolx-input-core tests::pause_is_consumed_and_decoding_resumes_after_it, tests::print_screen_fake_shifts_and_unknown_codes_report_nothing
