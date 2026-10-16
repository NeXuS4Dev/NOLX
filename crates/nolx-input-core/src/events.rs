//! Shared bounded input delivery and the standard three-byte PS/2 mouse protocol.

use crate::KeyEvent;

/// Origin of an input event, preserved when PS/2 and USB share one delivery queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// i8042 keyboard port.
    Ps2Keyboard,
    /// i8042 auxiliary port.
    Ps2Mouse,
    /// USB HID input.
    UsbHid,
}

/// Relative pointer motion in screen coordinates and the current button mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerEvent {
    /// Horizontal displacement, positive to the right.
    pub dx: i16,
    /// Vertical displacement, positive downwards.
    pub dy: i16,
    /// Left, right and middle buttons in bits 0, 1 and 2.
    pub buttons: u8,
}

/// Payload delivered by an input device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Key press or release using the common USB HID usage vocabulary.
    Key(KeyEvent),
    /// Relative motion and button state.
    Pointer(PointerEvent),
}

/// One source-tagged input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Producing device class.
    pub source: Source,
    /// Key or pointer payload.
    pub kind: Kind,
}

/// Fixed-capacity input FIFO, synchronized externally by the kernel.
pub struct EventQueue<const N: usize> {
    events: [Option<Event>; N],
    head: usize,
    len: usize,
    dropped: u64,
}

impl<const N: usize> EventQueue<N> {
    /// Empty queue; zero capacity safely drops all events.
    pub const fn new() -> Self {
        Self {
            events: [None; N],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }
    /// Enqueue without overwriting an unread event. Overflow increments the drop counter.
    pub fn push(&mut self, event: Event) -> bool {
        if self.len == N {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.events[(self.head + self.len) % N] = Some(event);
        self.len += 1;
        true
    }
    /// Remove the oldest event.
    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.events[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        event
    }
    /// Events rejected because the consumer did not keep up.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl<const N: usize> Default for EventQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Three-byte PS/2 mouse packet decoder, with first-byte resynchronization.
pub struct MouseDecoder {
    bytes: [u8; 3],
    len: usize,
}

impl MouseDecoder {
    /// Decoder waiting for a first byte whose synchronization bit is set.
    pub const fn new() -> Self {
        Self {
            bytes: [0; 3],
            len: 0,
        }
    }
    /// Discard a partial packet after a controller transfer error.
    pub fn reset(&mut self) {
        self.len = 0;
    }
    /// Consume a byte; reject overflowed packets rather than inventing truncated motion.
    pub fn feed(&mut self, byte: u8) -> Option<PointerEvent> {
        if self.len == 0 && byte & 8 == 0 {
            return None;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        if self.len < 3 {
            return None;
        }
        self.len = 0;
        let [flags, x, y] = self.bytes;
        if flags & 0xc0 != 0 {
            return None;
        }
        let dx = i16::from(x) - if flags & 0x10 != 0 { 256 } else { 0 };
        let dy = i16::from(y) - if flags & 0x20 != 0 { 256 } else { 0 };
        Some(PointerEvent {
            dx,
            dy: -dy,
            buttons: flags & 7,
        })
    }
}

impl Default for MouseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_resynchronizes_and_decodes_nine_bit_signed_motion() {
        let mut decoder = MouseDecoder::new();
        assert!(decoder.feed(0).is_none());
        assert!(decoder.feed(0x39).is_none());
        assert!(decoder.feed(0xff).is_none());
        assert_eq!(
            decoder.feed(0xfe),
            Some(PointerEvent {
                dx: -1,
                dy: 2,
                buttons: 1
            })
        );
        for byte in [0xc8, 1, 2] {
            assert!(decoder.feed(byte).is_none());
        }
        assert!(decoder.feed(8).is_none());
        decoder.reset();
        for byte in [0x38, 0] {
            assert!(decoder.feed(byte).is_none());
        }
        assert_eq!(
            decoder.feed(0),
            Some(PointerEvent {
                dx: -256,
                dy: 256,
                buttons: 0
            })
        );
    }
    #[test]
    fn queue_preserves_sources_and_order_on_wrap_without_overwriting() {
        let keyboard = Event {
            source: Source::Ps2Keyboard,
            kind: Kind::Key(KeyEvent {
                usage: 4,
                pressed: true,
            }),
        };
        let usb = Event {
            source: Source::UsbHid,
            ..keyboard
        };
        let mut queue = EventQueue::<2>::new();
        assert!(queue.push(keyboard));
        assert!(queue.push(usb));
        assert!(!queue.push(keyboard));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop(), Some(keyboard));
        assert!(queue.push(keyboard));
        assert_eq!(queue.pop(), Some(usb));
        assert_eq!(queue.pop(), Some(keyboard));
        assert_eq!(queue.pop(), None);
        let mut zero = EventQueue::<0>::new();
        assert!(!zero.push(usb));
        assert_eq!(zero.pop(), None);
    }
}
