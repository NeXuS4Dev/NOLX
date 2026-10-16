//! Ethernet/IPv4 ARP requests and strictly matched replies for link-level reachability.
//! This is not an IP or socket stack; drivers use it to verify externally received bytes.

/// Construct a minimum-sized Ethernet ARP request for an IPv4 peer.
pub fn request(mac: [u8; 6], local: [u8; 4], peer: [u8; 4]) -> [u8; 60] {
    let mut bytes = [0; 60];
    bytes[..6].fill(0xff);
    bytes[6..12].copy_from_slice(&mac);
    bytes[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
    bytes[22..28].copy_from_slice(&mac);
    bytes[28..32].copy_from_slice(&local);
    bytes[38..42].copy_from_slice(&peer);
    bytes
}

/// Accept only a well-formed reply from the requested IPv4 peer to this local MAC/IP.
/// The Ethernet source must agree with the ARP sender MAC, which must be unicast/nonzero.
/// Trailing Ethernet padding is permitted; truncated headers and unrelated replies are not.
pub fn reply(bytes: &[u8], mac: [u8; 6], local: [u8; 4], peer: [u8; 4]) -> Option<[u8; 6]> {
    if bytes.len() < 42
        || bytes[..6] != mac
        || bytes[12..22] != [8, 6, 0, 1, 8, 0, 6, 4, 0, 2]
        || bytes[6..12] != bytes[22..28]
        || bytes[28..32] != peer
        || bytes[32..38] != mac
        || bytes[38..42] != local
        || bytes[22] & 1 != 0
        || bytes[22..28] == [0; 6]
    {
        return None;
    }
    let mut sender = [0; 6];
    sender.copy_from_slice(&bytes[22..28]);
    Some(sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const PEER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
    const LOCAL: [u8; 4] = [10, 0, 2, 15];
    const PEER: [u8; 4] = [10, 0, 2, 2];

    fn response() -> [u8; 60] {
        let mut bytes = request(PEER_MAC, PEER, LOCAL);
        bytes[..6].copy_from_slice(&MAC);
        bytes[21] = 2;
        bytes[32..38].copy_from_slice(&MAC);
        bytes
    }

    #[test]
    fn request_has_network_byte_order_and_minimum_frame_padding() {
        let bytes = request(MAC, LOCAL, PEER);
        assert_eq!(&bytes[..6], &[0xff; 6]);
        assert_eq!(&bytes[12..22], &[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        assert_eq!(&bytes[22..28], &MAC);
        assert_eq!(&bytes[28..32], &LOCAL);
        assert_eq!(&bytes[32..38], &[0; 6]);
        assert_eq!(&bytes[38..42], &PEER);
        assert!(bytes[42..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn valid_reply_matches_all_addresses_with_or_without_padding() {
        let bytes = response();
        assert_eq!(reply(&bytes, MAC, LOCAL, PEER), Some(PEER_MAC));
        assert_eq!(reply(&bytes[..42], MAC, LOCAL, PEER), Some(PEER_MAC));
        for end in 0..42 {
            assert_eq!(reply(&bytes[..end], MAC, LOCAL, PEER), None);
        }
    }

    #[test]
    fn rejects_wrong_protocol_opcodes_and_unrelated_or_spoofed_addresses() {
        for offset in 0..42 {
            let mut bytes = response();
            bytes[offset] ^= 0x80;
            assert_eq!(reply(&bytes, MAC, LOCAL, PEER), None, "offset {offset}");
        }
        for sender in [[0; 6], [0xff; 6], [1, 2, 3, 4, 5, 6]] {
            let mut bytes = response();
            bytes[6..12].copy_from_slice(&sender);
            bytes[22..28].copy_from_slice(&sender);
            assert_eq!(reply(&bytes, MAC, LOCAL, PEER), None);
        }
    }
}
