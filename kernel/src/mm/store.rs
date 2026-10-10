//! The buddy allocator's storage, implemented over the direct map.
//!
//! `nolx-mm-core`'s buddy allocator keeps its free lists *inside* the free blocks: each free
//! block stores the next and previous block at its order, and its order, in its own first
//! bytes. That is what lets a kernel manage memory before it has an allocator to manage
//! metadata with. Reaching that memory is the one thing the portable crate cannot do, so it
//! asks through [`BlockStore`]; this module is the kernel's answer.
//!
//! The blocks are written through the identity map the boot layer installed, so the managed
//! range stops at [`MAX_MANAGED_BYTES`] -- the same 2 GiB the boot layer mapped. Frames
//! above that line are not handed to the allocator at all; the frame map marks them
//! reserved, and the boot log says how many there are. Guessing would be worse: the
//! allocator would write a link into an unmapped address and the machine would triple-fault
//! during a boot that had otherwise already succeeded.
//!
//! One bit per frame records whether that frame heads a free block. It is what makes
//! [`BlockStore::free_order`] answerable for *any* frame -- allocated, interior to a larger
//! free block, or free -- in constant time, and it is what turns a double free into a
//! reported error instead of two owners of one frame.

use crate::mm::{MAX_FRAMES, PAGE_SHIFT};
use crate::sync::SyncCell;
use nolx_mm_core::buddy::{BlockStore, Frame, MAX_ORDER};

/// The value stored for "no link". No frame number can reach it: the frame map is smaller
/// than 4 GiB of frames.
const NONE: u64 = u64::MAX;

/// Which 64-bit word of a free block's first page holds what.
const NEXT_WORD: u64 = 0;
const PREVIOUS_WORD: u64 = 1;
const ORDER_WORD: u64 = 2;

/// The largest physical address the allocator can write its links to: the boot layer's
/// identity map covers the first 2 GiB.
pub const MAX_MANAGED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The first frame the allocator cannot manage, because the identity map does not cover it.
pub const MAX_MANAGED_FRAMES: u64 = MAX_MANAGED_BYTES >> PAGE_SHIFT;

/// The frames that head a free block.
static FREE_HEADS: SyncCell<[u64; MAX_FRAMES / 64]> = SyncCell::new([0; MAX_FRAMES / 64]);

/// The buddy allocator's storage, backed by physical memory through the identity map.
pub struct DirectMapStore;

impl Default for DirectMapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectMapStore {
    /// A store that writes to the frames themselves.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The address through which a word of a frame's first page is reached.
    fn address(frame: Frame, word: u64) -> *mut u64 {
        ((frame << PAGE_SHIFT) + word * 8) as usize as *mut u64
    }

    /// Read a word of a free block.
    ///
    /// The caller is the buddy allocator, which only passes frames that were handed to it by
    /// [`crate::mm::init`] from the firmware's usable regions. All of them lie below
    /// [`MAX_MANAGED_BYTES`], so they are mapped by the boot layer's identity map. The read
    /// is volatile because this memory is otherwise opaque to the kernel: it holds free-list
    /// links, not Rust values.
    fn read_word(frame: Frame, word: u64) -> u64 {
        // SAFETY: the frame is mapped, and it is free -- the allocator owns it outright, and
        // no other kernel code has a reference to it.
        unsafe { core::ptr::read_volatile(Self::address(frame, word)) }
    }

    /// Write a word of a free block.
    ///
    /// As [`DirectMapStore::read_word`], with the same ownership argument: the frame is free,
    /// so nothing else is reading it or holding a reference to it.
    fn write_word(frame: Frame, word: u64, value: u64) {
        // SAFETY: as `read_word`; this is the only writer of a free block's first bytes.
        unsafe { core::ptr::write_volatile(Self::address(frame, word), value) }
    }

    /// Read or write one bit of the free-head map.
    fn head_bit(frame: Frame) -> Option<(usize, u64)> {
        if frame >= MAX_FRAMES as u64 {
            return None;
        }
        let index = frame as usize;
        Some((index / 64, 1u64 << (index % 64)))
    }

    /// Whether a frame heads a free block.
    #[must_use]
    pub fn is_free_head(frame: Frame) -> bool {
        match Self::head_bit(frame) {
            // SAFETY: single boot processor. The map is written only by the allocator,
            // which never runs in interrupt context, and read here from the same processor.
            Some((index, bit)) => unsafe { (*FREE_HEADS.get())[index] & bit != 0 },
            None => false,
        }
    }

    /// Record whether a frame heads a free block.
    fn set_free_head(frame: Frame, present: bool) {
        if let Some((index, bit)) = Self::head_bit(frame) {
            // SAFETY: as [`DirectMapStore::is_free_head`]. Marking is how the map is kept
            // in step with the allocator instead of being a copy of it.
            unsafe {
                if present {
                    (*FREE_HEADS.get())[index] |= bit;
                } else {
                    (*FREE_HEADS.get())[index] &= !bit;
                }
            }
        }
    }
}

impl BlockStore for DirectMapStore {
    fn next_free(&self, frame: Frame) -> Option<Frame> {
        match Self::read_word(frame, NEXT_WORD) {
            NONE => None,
            frame => Some(frame),
        }
    }

    fn set_next_free(&mut self, frame: Frame, next: Option<Frame>) {
        Self::write_word(frame, NEXT_WORD, next.unwrap_or(NONE));
    }

    fn previous_free(&self, frame: Frame) -> Option<Frame> {
        match Self::read_word(frame, PREVIOUS_WORD) {
            NONE => None,
            frame => Some(frame),
        }
    }

    fn set_previous_free(&mut self, frame: Frame, previous: Option<Frame>) {
        Self::write_word(frame, PREVIOUS_WORD, previous.unwrap_or(NONE));
    }

    fn free_order(&self, frame: Frame) -> Option<usize> {
        if !Self::is_free_head(frame) {
            return None;
        }
        let order = Self::read_word(frame, ORDER_WORD);
        if order <= MAX_ORDER as u64 {
            Some(order as usize)
        } else {
            None
        }
    }

    fn set_free_order(&mut self, frame: Frame, order: Option<usize>) {
        match order {
            Some(order) => {
                Self::write_word(frame, ORDER_WORD, order as u64);
                Self::set_free_head(frame, true);
            }
            None => Self::set_free_head(frame, false),
        }
    }
}
