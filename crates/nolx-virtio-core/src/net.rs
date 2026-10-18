//! Modern virtio-net feature and packet-header contracts (no checksum/segmentation offload).

use crate::{negotiate, VirtioError};

/// Device-supplied MAC address feature.
pub const FEATURE_MAC: u64 = 1 << 5;
/// Modern virtio-net packet header, including the reserved/mergeable-buffer count field.
pub const HEADER_BYTES: usize = 12;
/// Largest supported Ethernet frame, excluding its FCS (the shared bounded frame queue does not enable VLAN offload).
pub const MAX_FRAME_BYTES: usize = 1514;

/// Accept only modern transport and a device MAC; never silently enable offload or multiqueue.
pub fn negotiate_features(offered: u64) -> Result<u64, VirtioError> {
    let modern = negotiate(offered)?;
    if offered & FEATURE_MAC == 0 {
        return Err(VirtioError::NoMacAddress);
    }
    Ok(modern | FEATURE_MAC)
}

/// A packet without checksum/GSO offload has a zeroed modern header.
#[must_use]
pub const fn transmit_header() -> [u8; HEADER_BYTES] {
    [0; HEADER_BYTES]
}

/// Validate a single-buffer received packet and return its Ethernet payload.
/// Unsupported offload flags, truncated packets and excessive lengths are refused.
#[must_use]
pub fn received_frame(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < HEADER_BYTES + 14 || packet.len() > HEADER_BYTES + MAX_FRAME_BYTES {
        return None;
    }
    if packet[..10].iter().any(|&byte| byte != 0) {
        return None;
    }
    let buffers = u16::from_le_bytes([packet[10], packet[11]]);
    if buffers > 1 {
        return None;
    }
    Some(&packet[HEADER_BYTES..])
}

/// Shared reset and PCI exclusion proof, retained here for compatibility.
pub use crate::transport::stop_proven;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FEATURE_VERSION_1;
    #[test]
    fn negotiation_does_not_accept_offload_or_legacy_only_transport() {
        assert_eq!(
            negotiate_features(u64::MAX),
            Ok(FEATURE_VERSION_1 | FEATURE_MAC)
        );
        assert!(negotiate_features(FEATURE_VERSION_1).is_err());
        assert!(negotiate_features(FEATURE_MAC).is_err());
    }
    #[test]
    fn packets_need_a_bounded_ethernet_frame_and_no_offload() {
        let mut packet = [0; HEADER_BYTES + 60];
        packet[HEADER_BYTES..].fill(0x55);
        assert_eq!(received_frame(&packet), Some(&packet[HEADER_BYTES..]));
        packet[10] = 1;
        assert!(received_frame(&packet).is_some());
        packet[10] = 2;
        assert_eq!(received_frame(&packet), None);
        packet[10] = 0;
        for index in 0..10 {
            packet[index] = 1;
            assert_eq!(received_frame(&packet), None);
            packet[index] = 0;
        }
        for len in 0..HEADER_BYTES + 14 {
            assert_eq!(received_frame(&packet[..len]), None);
        }
        assert_eq!(
            received_frame(&[0; HEADER_BYTES + MAX_FRAME_BYTES + 1]),
            None
        );
        assert_eq!(transmit_header(), [0; HEADER_BYTES]);
    }
}
