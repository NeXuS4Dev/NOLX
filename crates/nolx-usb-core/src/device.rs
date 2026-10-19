//! Strict direct-attached USB boot-HID configuration selection and xHCI context encodings.
use crate::Trb;
/// A boot HID interrupt-IN interface selected from a complete configuration descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hid {
    /// Configuration value (not descriptor index).
    pub configuration: u8,
    /// Interface number.
    pub interface: u8,
    /// Boot protocol: one keyboard, two relative mouse.
    pub protocol: u8,
    /// USB endpoint address, including IN direction.
    pub endpoint: u8,
    /// Maximum packet size, without high-bandwidth multipliers.
    pub packet: u16,
    /// Maximum bytes per service interval (companion payload for SuperSpeed).
    pub payload: u16,
    /// USB polling interval.
    pub interval: u8,
}
/// Refuse truncated, composite/alternate, ambiguous or non-boot-HID configurations.
pub fn hid(bytes: &[u8]) -> Option<Hid> {
    hid_at_speed(bytes, 1)
}
/// Select one boot interface for the negotiated default xHCI speed ID (1..=4).
/// SuperSpeed requires an immediately following, single-burst interrupt companion.
/// Multiburst, SuperSpeedPlus/custom speed IDs, alternate and composite interfaces refuse.
pub fn hid_at_speed(bytes: &[u8], speed: u8) -> Option<Hid> {
    if !(1..=4).contains(&speed) {
        return None;
    }
    if bytes.len() < 9
        || bytes[0] != 9
        || bytes[1] != 2
        || usize::from(u16::from_le_bytes([bytes[2], bytes[3]])) != bytes.len()
        || bytes[4] != 1
        || bytes[5] == 0
        || bytes[7] & 0x9f != 0x80
    {
        return None;
    }
    let mut at = 9;
    let mut interface = None;
    let mut endpoint = None;
    let mut hid_seen = false;
    let mut companion = None;
    while at < bytes.len() {
        let len = usize::from(*bytes.get(at)?);
        if len < 2 {
            return None;
        }
        let d = bytes.get(at..at.checked_add(len)?)?;
        match d[1] {
            4 => {
                if len != 9
                    || interface.is_some()
                    || d[3] != 0
                    || d[4] != 1
                    || d[5] != 3
                    || d[6] != 1
                    || !(1..=2).contains(&d[7])
                {
                    return None;
                }
                interface = Some((d[2], d[7]));
            }
            5 => {
                if len != 7
                    || interface.is_none()
                    || !hid_seen
                    || endpoint.is_some()
                    || d[2] & 0xf0 != 0x80
                    || d[2] & 15 == 0
                    || d[3] != 3
                    || d[6] == 0
                {
                    return None;
                }
                let packet = u16::from_le_bytes([d[4], d[5]]);
                let limit = match speed {
                    2 => 8,
                    1 => 64,
                    _ => 1024,
                };
                if !(3..=limit).contains(&packet) || interval(speed, d[6]).is_none() {
                    return None;
                }
                endpoint = Some((d[2], packet, d[6]));
            }
            0x21 => {
                if interface.is_none()
                    || hid_seen
                    || endpoint.is_some()
                    || len != 9
                    || d[5] != 1
                    || d[6] != 0x22
                    || (d[7] == 0 && d[8] == 0)
                {
                    return None;
                }
                hid_seen = true;
            }
            0x30 => {
                let (_, packet, _) = endpoint?;
                if speed != 4 || len != 6 || companion.is_some() || d[2] != 0 || d[3] != 0 {
                    return None;
                }
                let payload = u16::from_le_bytes([d[4], d[5]]);
                if payload == 0 || payload > packet {
                    return None;
                }
                companion = Some(payload);
            }
            _ => return None,
        }
        at += len;
    }
    let (interface, protocol) = interface?;
    let (endpoint, packet, interval) = endpoint?;
    let payload = if speed == 4 { companion? } else { packet };
    if payload < if protocol == 1 { 8 } else { 3 } {
        return None;
    }
    Some(Hid {
        configuration: bytes[5],
        interface,
        protocol,
        endpoint,
        packet,
        payload,
        interval,
    })
}
/// xHCI interrupt-IN device context index. Endpoint zero and reserved address bits refuse.
pub fn dci(endpoint: u8) -> Option<u8> {
    if endpoint & 0xf0 != 0x80 || endpoint & 15 == 0 {
        None
    } else {
        Some((endpoint & 15) * 2 + 1)
    }
}
/// xHCI interval exponent for low/full/high/SuperSpeed interrupt endpoints; other IDs refuse.
pub fn interval(speed: u8, interval: u8) -> Option<u8> {
    if interval == 0 || (speed == 2 && interval < 10) {
        return None;
    }
    match speed {
        1 | 2 => Some((7 - (interval.leading_zeros() as u8)) + 3),
        3 | 4 if interval <= 16 => Some(interval - 1),
        _ => None,
    }
}
/// Initial EP0 maximum packet size for a default speed ID, before reading descriptors.
pub fn initial_ep0_packet(speed: u8) -> Option<u16> {
    match speed {
        1 | 2 => Some(8),
        3 => Some(64),
        4 => Some(512),
        _ => None,
    }
}
/// Validate bMaxPacketSize0 against the negotiated speed; USB3 encodes an exponent.
pub fn ep0_packet(speed: u8, encoded: u8) -> Option<u16> {
    match speed {
        1 if [8, 16, 32, 64].contains(&encoded) => Some(u16::from(encoded)),
        2 if encoded == 8 => Some(8),
        3 if encoded == 64 => Some(64),
        4 if encoded == 9 => Some(512),
        _ => None,
    }
}
/// Encode the parameter/status/control dwords of a TRB with an explicit producer cycle.
pub fn trb(parameter: u64, status: u32, control: u32, cycle: bool) -> Trb {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&parameter.to_le_bytes());
    bytes[8..12].copy_from_slice(&status.to_le_bytes());
    bytes[12..].copy_from_slice(&((control & !1) | u32::from(cycle)).to_le_bytes());
    bytes
}
/// Physical command/transfer pointer carried by a completion event.
pub fn pointer(event: &Trb) -> u64 {
    u64::from_le_bytes(event[..8].try_into().unwrap()) & !15
}
/// Remaining bytes reported by a transfer event.
pub fn residual(event: &Trb) -> u32 {
    u32::from_le_bytes(event[8..12].try_into().unwrap()) & 0xffffff
}
/// Endpoint context index reported by a transfer event.
pub fn endpoint(event: &Trb) -> u8 {
    (event[14]) & 31
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> [u8; 34] {
        [
            9, 2, 34, 0, 1, 7, 0, 0x80, 50, 9, 4, 2, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1,
            0x22, 63, 0, 7, 5, 0x81, 3, 8, 0, 10,
        ]
    }
    #[test]
    fn configuration_uses_actual_values_and_refuses_every_truncation() {
        let b = config();
        let h = hid(&b).unwrap();
        assert_eq!(
            (h.configuration, h.interface, h.endpoint, h.protocol),
            (7, 2, 0x81, 1)
        );
        for n in 0..b.len() {
            assert_eq!(hid(&b[..n]), None);
        }
    }
    #[test]
    fn malformed_and_unsupported_interfaces_are_not_guessed() {
        for (index, value) in [
            (0, 8),
            (2, 33),
            (4, 2),
            (5, 0),
            (9, 0),
            (12, 1),
            (13, 2),
            (14, 8),
            (15, 0),
            (16, 0),
            (28, 0),
            (29, 1),
            (30, 2),
            (31, 0),
            (32, 8),
            (33, 0),
        ] {
            let mut b = config();
            b[index] = value;
            assert_eq!(hid(&b), None, "{index}");
        }
    }
    #[test]
    fn endpoint_and_interval_encodings_are_bounded() {
        assert_eq!(dci(0x81), Some(3));
        assert_eq!(dci(0x8f), Some(31));
        for e in [0, 1, 0x80, 0x91, 255] {
            assert_eq!(dci(e), None);
        }
        assert_eq!(interval(1, 10), Some(6));
        assert_eq!(interval(3, 16), Some(15));
        assert_eq!(interval(3, 17), None);
        assert_eq!(interval(4, 1), Some(0));
        assert_eq!(interval(4, 17), None);
        assert_eq!(interval(2, 9), None);
        assert_eq!(interval(2, 10), Some(6));
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    fn configuration(ss: bool) -> std::vec::Vec<u8> {
        let mut b = std::vec![
            9, 2, 34, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1,
            0x22, 63, 0, 7, 5, 0x81, 3, 8, 0, 10
        ];
        if ss {
            b.extend_from_slice(&[6, 0x30, 0, 0, 8, 0]);
            b[2] = 40;
        }
        b
    }
    #[test]
    fn ep0_encoding_is_speed_specific_and_never_an_unchecked_shift() {
        for speed in 0..=255 {
            assert_eq!(
                initial_ep0_packet(speed).is_some(),
                (1..=4).contains(&speed)
            );
            for encoded in 0..=255 {
                let expected = match speed {
                    1 if [8, 16, 32, 64].contains(&encoded) => Some(u16::from(encoded)),
                    2 if encoded == 8 => Some(8),
                    3 if encoded == 64 => Some(64),
                    4 if encoded == 9 => Some(512),
                    _ => None,
                };
                assert_eq!(ep0_packet(speed, encoded), expected);
            }
        }
    }
    #[test]
    fn superspeed_companion_payload_and_interval_are_not_usb2_encodings() {
        let mut b = configuration(true);
        b[31] = 0;
        b[32] = 4;
        b[33] = 1;
        let h = hid_at_speed(&b, 4).unwrap();
        assert_eq!(
            (h.packet, h.payload, interval(4, h.interval)),
            (1024, 8, Some(0))
        );
        for speed in [0, 1, 2, 3, 5, 255] {
            assert_eq!(hid_at_speed(&b, speed), None);
        }
        for n in 0..b.len() {
            assert_eq!(hid_at_speed(&b[..n], 4), None);
        }
        b.truncate(34);
        b[2] = 34;
        assert_eq!(hid_at_speed(&b, 4), None);
    }
    #[test]
    fn malformed_companions_and_non_single_burst_profiles_refuse() {
        let original = configuration(true);
        for (at, value) in [
            (34, 5),
            (35, 0x31),
            (36, 1),
            (37, 1),
            (37, 2),
            (37, 0x80),
            (38, 0),
            (38, 7),
            (38, 9),
            (39, 1),
            (33, 0),
            (33, 17),
            (32, 8),
        ] {
            let mut b = original.clone();
            b[at] = value;
            assert_eq!(hid_at_speed(&b, 4), None, "{at}={value}");
        }
        let mut b = original.clone();
        b.extend_from_slice(&original[34..]);
        b[2] = 46;
        assert_eq!(hid_at_speed(&b, 4), None);
        let mut b = original;
        let companion = b.split_off(34);
        b.splice(27..27, companion);
        assert_eq!(hid_at_speed(&b, 4), None);
    }
    #[test]
    fn speed_limits_and_mandatory_single_hid_descriptor_are_checked() {
        let original = configuration(false);
        for speed in 1..=3 {
            assert!(hid_at_speed(&original, speed).is_some());
        }
        for (speed, packet, expected) in [
            (2, 9u16, false),
            (1, 65, false),
            (3, 1024, true),
            (3, 1025, false),
            (3, 0x0808, false),
        ] {
            let mut b = original.clone();
            b[31..33].copy_from_slice(&packet.to_le_bytes());
            assert_eq!(hid_at_speed(&b, speed).is_some(), expected);
        }
        let mut b = original.clone();
        b.drain(18..27);
        b[2] = 25;
        assert_eq!(hid_at_speed(&b, 1), None);
        let mut b = original.clone();
        b.splice(18..18, original[18..27].iter().copied());
        b[2] = 43;
        assert_eq!(hid_at_speed(&b, 1), None);
        let mut b = original;
        let descriptor = b.drain(18..27).collect::<std::vec::Vec<_>>();
        b.extend(descriptor);
        assert_eq!(hid_at_speed(&b, 1), None);
    }
    #[test]
    fn warm_and_usb2_reset_writes_cannot_replay_action_or_change_bits() {
        for speed in 0..=255 {
            let expected = match speed {
                1..=3 => Some(crate::PORTSC_POWER | crate::PORTSC_RESET),
                4 => Some(crate::PORTSC_POWER | crate::PORTSC_WARM_RESET),
                _ => None,
            };
            assert_eq!(crate::portsc_reset_for_speed(u32::MAX, speed), expected);
        }
        assert_eq!(
            crate::portsc_reset_for_speed(0, 4),
            Some(crate::PORTSC_WARM_RESET)
        );
    }
}
