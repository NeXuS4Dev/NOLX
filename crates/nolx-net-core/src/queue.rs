//! Bounded, allocation-free packet delivery between a device ISR and its consumer.
//! Synchronization is supplied by the kernel; overflow never overwrites unread packets.

use crate::FRAME_MAX_DEFAULT;

/// A complete Ethernet frame copied out of a device-owned receive buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    bytes: [u8; FRAME_MAX_DEFAULT],
    len: usize,
}

impl Frame {
    /// The initialized frame bytes, excluding unused capacity.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// A FIFO with fixed packet capacity and a saturating drop counter.
pub struct FrameQueue<const N: usize> {
    frames: [Option<Frame>; N],
    head: usize,
    len: usize,
    dropped: u64,
}

impl<const N: usize> Default for FrameQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FrameQueue<N> {
    /// An empty receive queue; zero capacity is valid and drops every packet.
    pub const fn new() -> Self {
        Self {
            frames: [None; N],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    /// Copy a valid Ethernet frame into the FIFO; return false on bad size or overflow.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if self.len == N
            || !(crate::ETHERNET_HEADER_BYTES..=FRAME_MAX_DEFAULT).contains(&bytes.len())
        {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        let mut frame = Frame {
            bytes: [0; FRAME_MAX_DEFAULT],
            len: bytes.len(),
        };
        frame.bytes[..bytes.len()].copy_from_slice(bytes);
        self.frames[(self.head + self.len) % N] = Some(frame);
        self.len += 1;
        true
    }

    /// Remove the oldest queued frame, returning its owned copy.
    pub fn pop(&mut self) -> Option<Frame> {
        if self.len == 0 {
            return None;
        }
        let frame = self.frames[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        frame
    }

    /// Packets refused due to invalid size or exhausted capacity.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_wraps_without_overwriting_unread_frames() {
        let mut q = FrameQueue::<2>::new();
        assert!(q.push(&[1; 60]));
        assert!(q.push(&[2; 60]));
        assert!(!q.push(&[3; 60]));
        assert_eq!(q.pop().unwrap().bytes(), &[1; 60]);
        assert!(q.push(&[4; 60]));
        assert_eq!(q.pop().unwrap().bytes(), &[2; 60]);
        assert_eq!(q.pop().unwrap().bytes(), &[4; 60]);
        assert!(q.pop().is_none());
        assert_eq!(q.dropped(), 1);
    }

    #[test]
    fn validates_lengths_and_zero_capacity_without_panicking() {
        let mut empty = FrameQueue::<0>::new();
        assert!(!empty.push(&[0; 60]));
        assert!(empty.pop().is_none());
        let mut q = FrameQueue::<1>::new();
        for size in [0, 13, FRAME_MAX_DEFAULT + 1] {
            assert!(!q.push(&vec![0; size]));
        }
        assert_eq!(q.dropped(), 3);
        for size in [14, FRAME_MAX_DEFAULT] {
            assert!(q.push(&vec![1; size]));
            assert_eq!(q.pop().unwrap().bytes().len(), size);
        }
    }
}
