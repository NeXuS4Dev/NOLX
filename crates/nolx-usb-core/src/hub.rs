//! USB 2.0 hub rules: the hub descriptor, port status and change bits, the class requests, the
//! xHCI route string and the transaction-translator fields a device behind a hub needs.
//!
//! Everything here is decided over bytes and numbers, so the kernel's enumeration loop can be
//! written as "ask these questions, act on these answers" and every question is host-tested.
//! The three pieces that are easy to get subtly wrong are kept together on purpose:
//!
//! 1. **The port status word.** Power is bit 8 and the speed bits are 9 (low) and 10 (high) —
//!    an off-by-one here reads "powered" as "low-speed device attached" and enumerates a device
//!    at the wrong speed.
//! 2. **The change bits** (16..20) are write-one-to-clear *through a feature request*, not by
//!    writing the status word back. [`port_changes`] names them so a driver acknowledges exactly
//!    what it saw.
//! 3. **The route string**: four bits per tier, tier one in the low nibble, and it lists the hub
//!    ports *below the root port*, not the root port itself. A device on hub port 3 of a hub at
//!    root port 1 has route `0x3`, not `0x13`.
//!
//! Two more things are easy to get wrong and are kept here as well: the hub descriptor is a
//! *class* descriptor, so it must be asked for with a class request (`bmRequestType` `0xa0`,
//! type `0x29`) — a standard-typed request stalls — and a hub tells the host that a port changed
//! through an interrupt IN endpoint, which is the only thing that wakes a controller when a
//! device is plugged into a hub, because hub ports raise no controller event of their own.
//!
//! Scope: single-TT hubs behind a high-speed hub are described well enough to be addressed,
//! because that is what the xHCI slot context asks for. Isochronous traffic and hub class
//! requests beyond port power, reset, enable and the change acknowledgements are not here.

/// Class code a hub reports in its *device* descriptor (`bDeviceClass`).
pub const CLASS_HUB: u8 = 9;
/// Class-specific descriptor type of a hub descriptor.
pub const DESCRIPTOR_HUB: u8 = 0x29;
/// The longest hub descriptor this crate accepts: seven fixed bytes, two `DeviceRemovable`
/// bytes and two `PortPwrCtrlMask` bytes, which is what a fifteen-port hub sends.
pub const MAX_DESCRIPTOR_BYTES: usize = 11;
/// Ports a single hub may report: the removable bitmap is two bytes.
pub const MAX_PORTS: u8 = 15;

// Port status word, USB 2.0 table 11-21.
/// Bit 0: a device is attached.
pub const PORT_CONNECTED: u16 = 1 << 0;
/// Bit 1: the port is enabled and the device's reset completed.
pub const PORT_ENABLED: u16 = 1 << 1;
/// Bit 2: the port is suspended.
pub const PORT_SUSPENDED: u16 = 1 << 2;
/// Bit 3: the port detected over-current.
pub const PORT_OVER_CURRENT: u16 = 1 << 3;
/// Bit 4: a reset is in progress on the port.
pub const PORT_RESETTING: u16 = 1 << 4;
/// Bit 8: the port is powered. Not bit 7: bit 7 is reserved.
pub const PORT_POWERED: u16 = 1 << 8;
/// Bit 9: a low-speed device is attached. Only meaningful while [`PORT_CONNECTED`] is set.
pub const PORT_LOW_SPEED: u16 = 1 << 9;
/// Bit 10: a high-speed device is attached.
pub const PORT_HIGH_SPEED: u16 = 1 << 10;
/// Bit 11: the port is in test mode.
pub const PORT_TEST: u16 = 1 << 11;
/// Bit 12: the port indicator is on.
pub const PORT_INDICATOR: u16 = 1 << 12;

// Change bits: the *second* word of `GetPortStatus`, which is why they are numbered from zero.
// A driver that looks for them above bit 15 of the status word, as an xHCI `PORTSC` does, reads
// "nothing changed" on every hub in existence.
/// Change word bit 0: the connection state changed.
pub const C_PORT_CONNECTED: u16 = 1u16 << 0;
/// Change word bit 1: the enable state changed.
pub const C_PORT_ENABLED: u16 = 1u16 << 1;
/// Change word bit 2: the suspend state changed.
pub const C_PORT_SUSPENDED: u16 = 1u16 << 2;
/// Change word bit 3: over-current changed.
pub const C_PORT_OVER_CURRENT: u16 = 1u16 << 3;
/// Change word bit 4: a reset completed.
pub const C_PORT_RESET: u16 = 1u16 << 4;

/// Every change bit a driver must acknowledge.
pub const CHANGE_BITS: u16 =
    C_PORT_CONNECTED | C_PORT_ENABLED | C_PORT_SUSPENDED | C_PORT_OVER_CURRENT | C_PORT_RESET;

/// A port status word and its change word, as `GetPortStatus` returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// `wPortStatus`: connection, enable, power, speed and test state.
    pub status: u16,
    /// `wPortChange`: the changes since the last acknowledgement.
    pub change: u16,
}

impl PortStatus {
    /// Decode the four bytes of a `GetPortStatus` reply, little-endian, status word first.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 4]) -> Self {
        Self {
            status: u16::from_le_bytes([bytes[0], bytes[1]]),
            change: u16::from_le_bytes([bytes[2], bytes[3]]),
        }
    }

    /// Whether a device is attached.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.status & PORT_CONNECTED != 0
    }

    /// Whether the port is enabled, which is what a completed reset leaves behind.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.status & PORT_ENABLED != 0
    }

    /// Whether the port is powered.
    #[must_use]
    pub fn powered(&self) -> bool {
        self.status & PORT_POWERED != 0
    }

    /// The xHCI speed ID of the attached device, or `None` when the bits contradict each other.
    #[must_use]
    pub fn speed(&self) -> Option<u8> {
        self.status_speed()
    }

    fn status_speed(&self) -> Option<u8> {
        port_speed(self.status)
    }
}

// Feature selectors, USB 2.0 table 11-17.
/// `C_HUB_LOCAL_POWER`, a hub-level change bit.
pub const C_HUB_LOCAL_POWER: u16 = 0;
/// `C_HUB_OVER_CURRENT`, a hub-level change bit.
pub const C_HUB_OVER_CURRENT: u16 = 1;
/// `PORT_POWER`: set to power the port.
pub const FEATURE_PORT_POWER: u16 = 8;
/// `PORT_RESET`: set to start a reset.
pub const FEATURE_PORT_RESET: u16 = 4;
/// `C_PORT_CONNECTION`: clear to acknowledge a connection change.
pub const FEATURE_C_PORT_CONNECTION: u16 = 16;
/// `C_PORT_ENABLE`: clear to acknowledge an enable change.
pub const FEATURE_C_PORT_ENABLE: u16 = 17;
/// `C_PORT_SUSPEND`: clear to acknowledge a suspend change.
pub const FEATURE_C_PORT_SUSPEND: u16 = 18;
/// `C_PORT_OVER_CURRENT`: clear to acknowledge an over-current change.
pub const FEATURE_C_PORT_OVER_CURRENT: u16 = 19;
/// `C_PORT_RESET`: clear to acknowledge a completed reset.
pub const FEATURE_C_PORT_RESET: u16 = 20;

// Class request codes, USB 2.0 table 11-16.
/// `Get Status`, with the "other" recipient, reads a port's status word.
pub const REQUEST_GET_STATUS: u8 = 0;
/// `bRequest` of `GetDescriptor`.
pub const REQUEST_GET_DESCRIPTOR: u8 = 6;
/// `Clear Feature` acknowledges a change bit.
pub const REQUEST_CLEAR_FEATURE: u8 = 1;
/// `Set Feature` powers or resets a port.
pub const REQUEST_SET_FEATURE: u8 = 3;

/// `bmRequestType` of a port `GetStatus`: class, "other" recipient, device-to-host.
///
/// The recipient is "other" (3), not "device" (0): `0xa0` asks the *hub device* for its status
/// and is stalled by every real hub, which is exactly what happened the first time this ran.
pub const GET_STATUS_REQUEST_TYPE: u8 = 0xa3;
/// `bmRequestType` of a port `ClearFeature` or `SetFeature`: class, "other", host-to-device.
pub const SET_REQUEST_TYPE: u8 = 0x23;
/// `bmRequestType` of a `GetDescriptor` for the hub descriptor: **class**, device, device-to-host.
///
/// The hub descriptor is a class descriptor, not a standard one: a request with the standard
/// type (`0x80`) asks every layer below for a descriptor type it does not know, and the hub is
/// within its rights to stall it. The first version of this driver made exactly that mistake.
pub const GET_HUB_DESCRIPTOR_REQUEST_TYPE: u8 = 0xa0;

/// The `GetDescriptor` request that returns the hub descriptor.
#[must_use]
pub fn get_hub_descriptor(length: u8) -> [u8; 8] {
    [
        GET_HUB_DESCRIPTOR_REQUEST_TYPE,
        REQUEST_GET_DESCRIPTOR,
        0,
        DESCRIPTOR_HUB,
        0,
        0,
        length,
        0,
    ]
}

/// A parsed hub descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hub {
    /// Ports the hub exposes (`bNbrPorts`), 1..=[`MAX_PORTS`].
    pub ports: u8,
    /// `wHubCharacteristics`, little-endian as received.
    pub characteristics: u16,
    /// `bPwrOn2PwrGood`: seconds-to-power is this times two milliseconds.
    pub power_on_delay: u8,
    /// `bHubContrCurrent`: current the hub itself draws, in milliamperes.
    pub current: u8,
    /// `DeviceRemovable` bitmap, little-endian, padded to the two bytes a 15-port hub needs.
    pub removable: [u8; 2],
}

impl Hub {
    /// Whether port `port` (one-based) has a permanently attached device.
    ///
    /// A compound device's fixed ports must not be treated as removable: their devices are part
    /// of the hub and never "hot-plugged" away.
    #[must_use]
    pub fn device_removable(&self, port: u8) -> Option<bool> {
        if port == 0 || port > self.ports {
            return None;
        }
        let bit = usize::from(port); // bit 0 is reserved; port 1 is bit 1
        let word = u16::from_le_bytes(self.removable);
        Some(word & (1 << bit) != 0)
    }

    /// Whether the hub switches port power under software control (`wHubCharacteristics` bit 0
    /// clear, bits 1:0 = `00` or `01`).
    #[must_use]
    pub fn per_port_power_switching(&self) -> bool {
        match self.characteristics & 0b11 {
            0 | 1 => true,
            // Gang-powered hubs switch every port at once, which this driver treats as "the hub
            // is already powered"; 3 is "no power switching" and also already powered.
            _ => false,
        }
    }

    /// Whether the hub reports over-current per port rather than globally.
    #[must_use]
    pub fn per_port_over_current(&self) -> bool {
        self.characteristics & (1 << 3) != 0
    }

    /// The transaction translator think time, in the encoding the xHCI slot context takes
    /// (0 = 8 FS bit times, 1 = 16, 2 = 24, 3 = 32).
    #[must_use]
    pub fn tt_think_time(&self) -> u8 {
        ((self.characteristics >> 5) & 0b11) as u8
    }
}

/// Parse a hub descriptor (`bDescriptorType` 0x29).
///
/// A hub descriptor is seven bytes plus a removable bitmap of `(bNbrPorts + 8) / 8` bytes, and
/// the two bitmap bytes are only present for more than seven ports. Both lengths occur in the
/// wild, so the parser accepts `7 + needed` bytes padded to at least nine and refuses anything
/// shorter than the bitmap requires; a short descriptor would silently mark ports removable.
#[must_use]
pub fn descriptor(bytes: &[u8]) -> Option<Hub> {
    let length = usize::from(*bytes.first()?);
    if length < 7 || length > bytes.len() || length > MAX_DESCRIPTOR_BYTES {
        return None;
    }
    let body = bytes.get(..length)?;
    if body[1] != DESCRIPTOR_HUB {
        return None;
    }
    let ports = body[2] & 0x0f;
    if ports == 0 || ports > MAX_PORTS {
        return None;
    }
    // A hub that declares more ports than its bitmap bytes can hold is malformed: the bitmap has
    // one bit per port plus a reserved bit zero.
    let needed = (usize::from(ports) + 8) / 8;
    if length < 7 + needed {
        return None;
    }
    let mut removable = [0u8; 2];
    removable[..needed].copy_from_slice(&body[7..7 + needed]);
    Some(Hub {
        ports,
        characteristics: u16::from_le_bytes([body[3], body[4]]),
        power_on_delay: body[5],
        current: body[6],
        removable,
    })
}

/// Endpoint descriptor type in a configuration.
const DESCRIPTOR_ENDPOINT: u8 = 5;
/// Transfer type field of `bmAttributes` for an interrupt endpoint.
const TRANSFER_INTERRUPT: u8 = 3;

/// The hub's status-change endpoint: an interrupt IN pipe that reports which ports changed.
///
/// A hub raises this endpoint — and through it the controller's interrupt — when a port's
/// connection or enable state changes. Without it a device plugged into a hub produces no
/// controller event at all, so a driver that only polls ports on interrupts would never see
/// the device. That is why this pipe is armed even though each port is also polled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusPipe {
    /// `bEndpointAddress`, which must be an IN endpoint.
    pub endpoint: u8,
    /// `wMaxPacketSize` without the high-bandwidth bits.
    pub packet: u16,
    /// `bInterval`, in the unit the device's speed defines.
    pub interval: u8,
}

/// Find the hub's interrupt IN endpoint in its configuration descriptor.
///
/// The hub class requires exactly one such endpoint and forbids isochronous or bulk endpoints,
/// so the first interrupt IN endpoint in a valid configuration *is* the status pipe.
#[must_use]
pub fn status_pipe(configuration: &[u8]) -> Option<StatusPipe> {
    if configuration.len() < 9
        || configuration[0] != 9
        || configuration[1] != 2
        || usize::from(u16::from_le_bytes([configuration[2], configuration[3]]))
            != configuration.len()
        || configuration[5] == 0
    {
        return None;
    }
    let mut at = 9;
    let mut pipe = None;
    while at + 2 <= configuration.len() {
        let len = usize::from(configuration[at]);
        if len < 2 || at + len > configuration.len() {
            return None;
        }
        let d = &configuration[at..at + len];
        if d[1] == DESCRIPTOR_ENDPOINT {
            if len != 7 {
                return None;
            }
            let kind = d[3] & 3;
            let inbound = d[2] & 0x80 != 0;
            if kind == TRANSFER_INTERRUPT && inbound {
                let packet = u16::from_le_bytes([d[4], d[5]]) & 0x07ff;
                if packet == 0 || d[6] == 0 || pipe.is_some() {
                    return None;
                }
                pipe = Some(StatusPipe {
                    endpoint: d[2],
                    packet,
                    interval: d[6],
                });
            }
        }
        at += len;
    }
    pipe
}

/// Bytes a status-change transfer must carry for a hub with `ports` ports and `packet` bytes.
///
/// The bitmap has one bit for the hub itself plus one per port, and a hub may advertise a
/// smaller packet than that; asking for more than the endpoint allows would be stalled, so the
/// length is clamped to the endpoint's maximum. Ports beyond that edge are still polled.
#[must_use]
pub fn status_length(ports: u8, packet: u16) -> u16 {
    let needed = (u16::from(ports) + 8) / 8;
    needed.clamp(1, packet.max(1))
}

/// Whether a device descriptor describes a hub rather than an interface-class device.
///
/// Hubs are unusual in declaring their class at the *device* level (`bDeviceClass` 9) instead of
/// leaving it zero and putting it on an interface, which is exactly why a boot-HID configuration
/// reader refuses them.
#[must_use]
pub fn is_hub(device_descriptor: &[u8]) -> bool {
    device_descriptor.len() >= 18
        && device_descriptor[0] == 18
        && device_descriptor[1] == 1
        && device_descriptor[4] == CLASS_HUB
}

/// The `GetPortStatus` request: class, other, device-to-host, two data bytes.
#[must_use]
pub fn get_port_status(port: u8) -> Option<[u8; 8]> {
    if port == 0 {
        return None;
    }
    Some([
        GET_STATUS_REQUEST_TYPE,
        REQUEST_GET_STATUS,
        0,
        0,
        port,
        0,
        2,
        0,
    ])
}

/// A `ClearPortFeature` request acknowledging one change bit.
#[must_use]
pub fn clear_port_feature(port: u8, feature: u16) -> Option<[u8; 8]> {
    if port == 0 || feature < FEATURE_C_PORT_CONNECTION {
        return None;
    }
    Some([
        SET_REQUEST_TYPE,
        REQUEST_CLEAR_FEATURE,
        feature as u8,
        (feature >> 8) as u8,
        port,
        0,
        0,
        0,
    ])
}

/// A `SetPortFeature` request powering or resetting a port.
#[must_use]
pub fn set_port_feature(port: u8, feature: u16) -> Option<[u8; 8]> {
    if port == 0 || !(FEATURE_PORT_RESET..=FEATURE_PORT_POWER).contains(&feature) {
        return None;
    }
    Some([
        SET_REQUEST_TYPE,
        REQUEST_SET_FEATURE,
        feature as u8,
        (feature >> 8) as u8,
        port,
        0,
        0,
        0,
    ])
}

/// The xHCI speed ID of a device attached to a hub port, from its status word.
///
/// The hub reports only "low" and "high" explicitly: a device that sets neither bit is
/// full-speed, and a device that sets both is malformed. The result is an xHCI ID — 1 full,
/// 2 low, 3 high — **not** the hub's own numbering: reading the low-speed bit as ID 1 would
/// address a 1.5 Mb/s device with a full-speed endpoint interval.
#[must_use]
pub fn port_speed(status: u16) -> Option<u8> {
    match (status & PORT_LOW_SPEED != 0, status & PORT_HIGH_SPEED != 0) {
        (true, true) => None,
        (true, false) => Some(2),
        (false, true) => Some(3),
        (false, false) => Some(1),
    }
}

/// The change bits set in a change word, as the feature selectors that acknowledge them.
///
/// Returns `None` for a word with no change bits, so a caller can tell "nothing happened" from
/// "everything happened" without consulting a separate empty flag.
#[must_use]
pub fn port_changes(change: u16) -> Option<[u16; 5]> {
    let bits = change & CHANGE_BITS;
    if bits == 0 {
        return None;
    }
    let mut out = [0u16; 5];
    let mut count = 0usize;
    for (mask, feature) in [
        (C_PORT_CONNECTED, FEATURE_C_PORT_CONNECTION),
        (C_PORT_ENABLED, FEATURE_C_PORT_ENABLE),
        (C_PORT_SUSPENDED, FEATURE_C_PORT_SUSPEND),
        (C_PORT_OVER_CURRENT, FEATURE_C_PORT_OVER_CURRENT),
        (C_PORT_RESET, FEATURE_C_PORT_RESET),
    ] {
        if bits & mask != 0 {
            out[count] = feature;
            count += 1;
        }
    }
    Some(out)
}

/// Tiers a route string can address: four bits per tier, twenty bits in the register.
pub const MAX_TIERS: u8 = 5;

/// Append a hub port to a route string.
///
/// Tier one is the low nibble and each deeper tier is the next nibble up, so a device on port 5
/// of a hub plugged into a root port has the route `0x5`, and a device on port 2 of a hub that is
/// itself on port 5 has `0x25`. Port zero is not a port, and a sixth tier does not fit, so both
/// are refused rather than truncated.
#[must_use]
pub fn route_push(route: u32, port: u8) -> Option<u32> {
    if port == 0 || port > 15 || route > 0x000f_ffff {
        return None;
    }
    let depth = route_depth(route);
    if depth >= MAX_TIERS {
        return None;
    }
    Some(route | (u32::from(port) << (4 * u32::from(depth))))
}

/// How many hub tiers a route string addresses.
///
/// A route whose nibbles are not contiguous from tier one (`0x10`, say) does not describe a
/// physical path; [`route_depth`] reports the depth of the highest set nibble and [`route_valid`]
/// is what a driver should check.
#[must_use]
pub fn route_depth(route: u32) -> u8 {
    let mut depth = 0u8;
    let mut remaining = route;
    while remaining != 0 && depth < MAX_TIERS {
        depth += 1;
        remaining >>= 4;
    }
    depth
}

/// Whether a route string is a contiguous path of one to five hub ports.
#[must_use]
pub fn route_valid(route: u32) -> bool {
    if route == 0 {
        return true;
    }
    if route > 0x000f_ffff {
        return false;
    }
    let depth = route_depth(route);
    // Every nibble below the top one must be nonzero: `0x10` would mean "hub port 0".
    let mut remaining = route;
    for _ in 0..depth {
        if remaining & 0xf == 0 {
            return false;
        }
        remaining >>= 4;
    }
    remaining == 0
}

/// The route of the hub a device's route hangs off, or `None` for a direct-root device.
#[must_use]
pub fn route_parent(route: u32) -> Option<u32> {
    if route == 0 {
        return None;
    }
    let depth = route_depth(route);
    Some(route & ((1 << (4 * u32::from(depth - 1))) - 1))
}

/// Whether `route` is `ancestor` itself or a device below it.
///
/// This is what a disconnect check needs: a hub that goes away takes every device behind it,
/// including devices behind hubs behind it.
#[must_use]
pub fn route_under(route: u32, ancestor: u32) -> bool {
    let depth = route_depth(ancestor);
    if depth == 0 {
        return true;
    }
    if route_depth(route) < depth {
        return false;
    }
    route & ((1 << (u32::from(depth) * 4)) - 1) == ancestor
}

/// The transaction translator a low/full-speed device behind a high-speed hub is addressed
/// through, in the encoding the xHCI slot context takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tt {
    /// Slot ID of the high-speed hub the device hangs off.
    pub hub_slot: u8,
    /// The hub port the device is attached to.
    pub port: u8,
    /// The hub's think time, from [`Hub::tt_think_time`].
    pub think_time: u8,
}

/// Slot context dword 0: route string, speed, hub flag and the context-entry count.
///
/// `entries` counts the contexts *after* the slot context (endpoint zero plus one per endpoint),
/// and `hub_ports` is `Some` only for a hub, which additionally sets bit 26 and puts its port
/// count in dword 1.
#[must_use]
pub fn slot_context0(route: u32, speed: u8, entries: u8, hub_ports: Option<u8>) -> Option<u32> {
    if !route_valid(route) || !(1..=4).contains(&speed) || entries == 0 || entries > 31 {
        return None;
    }
    let hub = u32::from(hub_ports.is_some()) << 26;
    Some(route | (u32::from(speed) << 20) | hub | (u32::from(entries) << 27))
}

/// Slot context dword 1: the root port the device's tree hangs off, and a hub's port count.
#[must_use]
pub fn slot_context1(root_port: u8, hub_ports: Option<u8>) -> Option<u32> {
    if root_port == 0 {
        return None;
    }
    let ports = match hub_ports {
        Some(ports) if (1..=MAX_PORTS).contains(&ports) => u32::from(ports),
        Some(_) => return None,
        None => 0,
    };
    Some((u32::from(root_port) << 16) | (ports << 24))
}

/// Slot context dword 2: the transaction translator a low/full-speed device is addressed through.
#[must_use]
pub fn slot_context2(tt: Option<Tt>) -> u32 {
    match tt {
        Some(tt) => {
            u32::from(tt.hub_slot)
                | (u32::from(tt.port) << 8)
                | (u32::from(tt.think_time & 0b11) << 16)
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seven-port hub descriptor: seven fixed bytes plus its one removable-bitmap byte.
    fn short_descriptor() -> [u8; 8] {
        [8, DESCRIPTOR_HUB, 7, 0x00, 0x00, 0x32, 100, 0x04]
    }

    /// An eight-port hub descriptor, which needs both removable bytes; the trailing byte is
    /// `PortPwrCtrlMask`, which this crate deliberately ignores.
    fn wide_descriptor() -> [u8; 11] {
        [
            11,
            DESCRIPTOR_HUB,
            8,
            0xe9,
            0x00,
            0x32,
            100,
            0x04,
            0x00,
            0xff,
            0xff,
        ]
    }

    #[test]
    fn hub_descriptor_requires_its_bitmap() {
        assert!(descriptor(&short_descriptor()).is_some());
        assert_eq!(descriptor(&short_descriptor()).unwrap().ports, 7);
        let wide = descriptor(&wide_descriptor()).expect("nine-byte hub descriptor");
        assert_eq!(wide.ports, 8);
        assert_eq!(wide.removable, [0x04, 0x00]);
        // Eight ports need two bitmap bytes; a descriptor that claims eight ports and carries
        // one is refused, because its second byte would be read as whatever follows it in memory.
        assert_eq!(
            descriptor(&[8, DESCRIPTOR_HUB, 8, 0, 0, 0x32, 100, 0xff]),
            None
        );
        // Nine bytes is a valid eight-port descriptor without the port-power-control mask:
        // real hubs disagree about sending it, so the parser does not require it.
        assert_eq!(
            descriptor(&[9, DESCRIPTOR_HUB, 8, 0xe9, 0, 0x32, 100, 0x04, 0x00]).map(|h| h.ports),
            Some(8)
        );
        assert_eq!(
            descriptor(&[8, DESCRIPTOR_HUB, 7, 0, 0, 0x32, 100, 0xff]).map(|h| h.ports),
            Some(7)
        );
        // A hub with no ports, an impossible length and a wrong descriptor type are all refused.
        assert_eq!(
            descriptor(&[9, DESCRIPTOR_HUB, 0, 0, 0, 0x32, 100, 0, 0]),
            None
        );
        assert_eq!(
            descriptor(&[9, DESCRIPTOR_HUB, 16, 0, 0, 0x32, 100, 0, 0]),
            None
        );
        assert_eq!(descriptor(&[8, 0x2a, 7, 0, 0, 0x32, 100, 0]), None);
        assert_eq!(descriptor(&[6, DESCRIPTOR_HUB, 7, 0, 0, 0x32]), None);
        assert_eq!(descriptor(&[]), None);
    }

    #[test]
    fn hub_characteristics_decode_into_the_three_answers_a_driver_asks_for() {
        // Bits 1:0 = 00 (per-port power switching), bit 3 = 1 (per-port over-current),
        // bits 6:5 = 10b (think time 24 FS bit times).
        let mut bytes = wide_descriptor();
        bytes[3] = 0b0100_1001;
        let hub = descriptor(&bytes).expect("hub");
        assert!(hub.per_port_power_switching());
        assert!(hub.per_port_over_current());
        assert_eq!(hub.tt_think_time(), 0b10);
        // Gang power (01) is still "software switches a port"; no switching (11) is not.
        bytes[3] = 0b0000_0001;
        assert!(descriptor(&bytes).unwrap().per_port_power_switching());
        bytes[3] = 0b0000_0011;
        assert!(!descriptor(&bytes).unwrap().per_port_power_switching());
        // Bit zero of the removable bitmap is reserved; port 1 is bit 1.
        let hub = descriptor(&wide_descriptor()).unwrap();
        assert_eq!(hub.device_removable(0), None);
        assert_eq!(hub.device_removable(1), Some(false));
        assert_eq!(hub.device_removable(2), Some(true));
        assert_eq!(hub.device_removable(8), Some(false));
        assert_eq!(hub.device_removable(9), None);
    }

    #[test]
    fn port_status_bits_are_the_ones_the_specification_names() {
        assert_eq!(PORT_POWERED, 0x100);
        assert_eq!(PORT_LOW_SPEED, 0x200);
        assert_eq!(PORT_HIGH_SPEED, 0x400);
        // The change bits are the second word: bit 0 of the change word is C_PORT_CONNECTION.
        assert_eq!(C_PORT_CONNECTED, 0x1);
        assert_eq!(C_PORT_RESET, 0x10);
        // A powered, connected port with a high-speed device reads as speed 3.
        let high = PortStatus::from_bytes([
            (PORT_POWERED | PORT_CONNECTED | PORT_HIGH_SPEED | PORT_ENABLED) as u8,
            ((PORT_POWERED | PORT_CONNECTED | PORT_HIGH_SPEED | PORT_ENABLED) >> 8) as u8,
            0,
            0,
        ]);
        assert!(high.connected() && high.enabled() && high.powered());
        assert_eq!(high.speed(), Some(3));
        assert_eq!(port_speed(PORT_POWERED | PORT_CONNECTED), Some(1));
        assert_eq!(port_speed(PORT_LOW_SPEED), Some(2));
        assert_eq!(port_speed(PORT_LOW_SPEED | PORT_HIGH_SPEED), None);
        // Power alone is not a speed: an off-by-one that read bit 7 would claim low speed.
        assert_eq!(PORT_POWERED & PORT_LOW_SPEED, 0);
        // Bytes are little-endian status first, change second.
        assert_eq!(
            PortStatus::from_bytes([0x01, 0x05, 0x11, 0x00]),
            PortStatus {
                status: 0x0501,
                change: 0x0011
            }
        );
    }

    #[test]
    fn change_acknowledgements_name_exactly_the_bits_that_are_set() {
        assert_eq!(port_changes(0), None);
        let changes = port_changes(C_PORT_CONNECTED | C_PORT_RESET).expect("two changes");
        assert_eq!(
            &changes[..2],
            &[FEATURE_C_PORT_CONNECTION, FEATURE_C_PORT_RESET]
        );
        let all = port_changes(CHANGE_BITS).expect("every change");
        assert_eq!(all, [16, 17, 18, 19, 20]);
        // A clear that is not a change selector is refused, and so is port zero.
        assert_eq!(clear_port_feature(1, FEATURE_PORT_POWER), None);
        assert_eq!(clear_port_feature(0, FEATURE_C_PORT_CONNECTION), None);
        // Set accepts only power and reset.
        assert!(set_port_feature(3, FEATURE_PORT_POWER).is_some());
        assert!(set_port_feature(3, FEATURE_PORT_RESET).is_some());
        assert_eq!(set_port_feature(3, FEATURE_C_PORT_RESET), None);
        assert_eq!(set_port_feature(0, FEATURE_PORT_POWER), None);
        assert_eq!(get_port_status(0), None);
        assert_eq!(get_port_status(4), Some([0xa3, 0, 0, 0, 4, 0, 2, 0]));
        assert_eq!(
            clear_port_feature(2, FEATURE_C_PORT_CONNECTION),
            Some([0x23, 1, 16, 0, 2, 0, 0, 0])
        );
        assert_eq!(
            set_port_feature(2, FEATURE_PORT_RESET),
            Some([0x23, 3, 4, 0, 2, 0, 0, 0])
        );
    }

    #[test]
    fn route_strings_are_four_bits_per_tier_and_list_ports_below_the_root() {
        assert_eq!(route_push(0, 3), Some(0x3));
        // Deeper tiers go in the higher nibbles: tier one is the port on the first hub.
        assert_eq!(route_push(0x3, 5), Some(0x53));
        assert_eq!(route_depth(0), 0);
        assert_eq!(route_depth(0x3), 1);
        assert_eq!(route_depth(0x35), 2);
        // Five tiers fit; a sixth does not, and a zero port is never a port.
        let mut route = 0;
        for port in 1..=5u8 {
            route = route_push(route, port).expect("tier fits");
        }
        // Port 1 became tier one and port 5 became tier five, so the low nibble is port 1.
        assert_eq!(route, 0x54321);
        assert_eq!(route_depth(route), 5);
        assert_eq!(route_push(route, 1), None);
        assert_eq!(route_push(0, 0), None);
        assert_eq!(route_push(0, 16), None);
        // `0x10` has a zero tier, which no physical path can have.
        assert!(route_valid(0x3));
        assert!(route_valid(0x35));
        assert!(route_valid(0x12345));
        assert!(!route_valid(0x10));
        assert!(!route_valid(0x100000));
        assert_eq!(route_parent(0), None);
        assert_eq!(route_parent(0x35), Some(0x5));
        assert_eq!(route_parent(0x3), Some(0));
    }

    #[test]
    fn a_hub_leaving_takes_every_device_below_it() {
        // A device at 0x35 is under the hub at 0x3 and the hub at 0.
        assert!(route_under(0x35, 0x5));
        assert!(route_under(0x5, 0x5));
        assert!(route_under(0x35, 0));
        // Not the other way round, and not a sibling.
        assert!(!route_under(0x5, 0x35));
        assert!(!route_under(0x25, 0x3));
        // A device two tiers down is still under the first hub it passes through.
        assert!(route_under(0x513, 0x13));
        assert!(!route_under(0x13, 0x513));
    }

    #[test]
    fn slot_context_words_carry_the_xhci_layout() {
        // Direct root full-speed boot keyboard: two contexts, no hub flag, no route.
        assert_eq!(slot_context0(0, 1, 1, None), Some((1 << 20) | (1 << 27)));
        // A hub at speed 3 with eight ports: the hub bit and the count.
        let hub = slot_context0(0, 3, 1, Some(8)).unwrap();
        assert_eq!(hub & (1 << 26), 1 << 26);
        assert_eq!(slot_context1(1, Some(8)), Some((1 << 16) | (8 << 24)));
        // A device behind a hub keeps its own route and its root port.
        assert_eq!(
            slot_context0(0x3, 1, 1, None).map(|v| v & 0xfffff),
            Some(0x3)
        );
        assert_eq!(slot_context1(1, None), Some(1 << 16));
        assert_eq!(slot_context2(None), 0);
        assert_eq!(
            slot_context2(Some(Tt {
                hub_slot: 2,
                port: 5,
                think_time: 2
            })),
            0x2 | 0x500 | (2 << 16)
        );
        // Refusals: no speed 0 or 5, no empty context list, no zero root port, no 16 ports.
        assert_eq!(slot_context0(0, 0, 1, None), None);
        assert_eq!(slot_context0(0, 5, 1, None), None);
        assert_eq!(slot_context0(0, 1, 0, None), None);
        assert_eq!(slot_context0(0x10, 1, 1, None), None);
        assert_eq!(
            slot_context0(0, 1, 1, Some(0)),
            Some((1 << 20) | (1 << 27) | (1 << 26))
        );
        assert_eq!(slot_context1(0, None), None);
        assert_eq!(slot_context1(1, Some(16)), None);
    }

    #[test]
    fn the_status_pipe_is_the_interrupt_in_endpoint() {
        // A four-port hub configuration: header, interface, one interrupt IN endpoint.
        let mut configuration = [0u8; 25];
        configuration[..9].copy_from_slice(&[9, 2, 25, 0, 1, 1, 0, 0xe0, 9]);
        configuration[9..18].copy_from_slice(&[9, 4, 0, 0, 1, 9, 0, 0, 0]);
        configuration[18..].copy_from_slice(&[7, 5, 0x81, 3, 1, 0, 12]);
        assert_eq!(
            status_pipe(&configuration),
            Some(StatusPipe {
                endpoint: 0x81,
                packet: 1,
                interval: 12
            })
        );
        // An OUT endpoint is not a status pipe: the hub reports to the host.
        let mut out = configuration;
        out[20] = 0x01;
        assert_eq!(status_pipe(&out), None);
        // Bulk and isochronous endpoints are not status pipes either.
        let mut bulk = configuration;
        bulk[21] = 2;
        assert_eq!(status_pipe(&bulk), None);
        let mut isochronous = configuration;
        isochronous[21] = 1;
        assert_eq!(status_pipe(&isochronous), None);
        // A zero packet size or a zero interval cannot be programmed.
        let mut empty = configuration;
        empty[22] = 0;
        assert_eq!(status_pipe(&empty), None);
        let mut no_interval = configuration;
        no_interval[24] = 0;
        assert_eq!(status_pipe(&no_interval), None);
        // Truncated, mismatched-length and endpoint-less configurations are refused.
        assert_eq!(status_pipe(&configuration[..20]), None);
        let mut lying = configuration;
        lying[2] = 24;
        assert_eq!(status_pipe(&lying), None);
        assert_eq!(status_pipe(&configuration[..18]), None);
        // Two interrupt endpoints: a hub has exactly one, so this is refused rather than guessed.
        let mut twice = [0u8; 32];
        twice[..9].copy_from_slice(&[9, 2, 32, 0, 1, 1, 0, 0xe0, 9]);
        twice[9..18].copy_from_slice(&[9, 4, 0, 0, 1, 9, 0, 0, 0]);
        twice[18..25].copy_from_slice(&[7, 5, 0x81, 3, 1, 0, 12]);
        twice[25..].copy_from_slice(&[7, 5, 0x82, 3, 1, 0, 12]);
        assert_eq!(status_pipe(&twice), None);
        // High-bandwidth bits in wMaxPacketSize are not part of the packet size.
        let mut wide = configuration;
        wide[23] = 0x18;
        assert_eq!(status_pipe(&wide).map(|p| p.packet), Some(1));
    }

    #[test]
    fn the_status_transfer_covers_every_port_and_never_exceeds_the_endpoint() {
        assert_eq!(status_length(1, 1), 1);
        assert_eq!(status_length(4, 1), 1);
        assert_eq!(status_length(7, 2), 1);
        assert_eq!(status_length(8, 2), 2);
        assert_eq!(status_length(15, 8), 2);
        // A one-byte endpoint cannot be asked for two, even with eight ports.
        assert_eq!(status_length(8, 1), 1);
        assert_eq!(status_length(15, 1), 1);
        // A zero packet can never yield a zero-length transfer.
        assert_eq!(status_length(4, 0), 1);
    }

    #[test]
    fn the_hub_descriptor_is_read_with_a_class_request() {
        // The hub descriptor is a class descriptor. Asking for it with the standard request type
        // makes every layer answer from the standard table, where type 0x29 does not exist; the
        // hub then stalls the transfer, which is how the first live run failed.
        let request = get_hub_descriptor(9);
        assert_eq!(request, [0xa0, 6, 0, DESCRIPTOR_HUB, 0, 0, 9, 0]);
        assert_eq!(GET_HUB_DESCRIPTOR_REQUEST_TYPE, 0xa0);
        assert_eq!(request[1], REQUEST_GET_DESCRIPTOR);
        // Recipient bits 4:0 are zero (the device), 6:5 are class, bit 7 is device-to-host.
        assert_eq!(request[0] & 0x1f, 0);
        assert_eq!(request[0] >> 5 & 3, 1);
        assert_eq!(request[0] >> 7, 1);
        // Port requests are aimed at "other" (a port) and never at the device.
        assert_eq!(get_port_status(1).unwrap()[0], 0xa3);
        assert_eq!(
            clear_port_feature(1, FEATURE_C_PORT_CONNECTION).unwrap()[0],
            0x23
        );
        assert_eq!(set_port_feature(1, FEATURE_PORT_RESET).unwrap()[0], 0x23);
    }

    #[test]
    fn a_hub_is_recognized_by_its_device_class_not_its_interface() {
        let mut descriptor = [0u8; 18];
        descriptor[0] = 18;
        descriptor[1] = 1;
        descriptor[4] = CLASS_HUB;
        assert!(is_hub(&descriptor));
        // A boot keyboard: class zero at the device level, HID on the interface.
        descriptor[4] = 0;
        assert!(!is_hub(&descriptor));
        // Truncated descriptors are not hubs either.
        assert!(!is_hub(&descriptor[..9]));
    }
}
