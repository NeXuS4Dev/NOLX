//! Memory a device reads or writes: one frame of the kernel's own memory, reached through the
//! direct map, with the physical address the device is given.
//!
//! Every access is bounds-checked against the frame, and volatile, because the device changes the
//! bytes behind the kernel's back. The frame is zeroed when it is taken.

use crate::mm::{self, phys, PAGE_SIZE};
use core::sync::atomic::{AtomicUsize, Ordering};
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// DMA pages still owned/pinned by devices (including conservative failed-start retention).
pub fn live_pages() -> usize {
    LIVE.load(Ordering::Acquire)
}

/// Memory one device reads or writes, reached through the direct map. A frame is 4 KiB and is
/// zeroed when it is taken; every access is checked to stay inside that frame.
pub(crate) struct Dma {
    phys: u64,
}

impl Dma {
    pub(crate) fn new() -> Option<Self> {
        let frame = mm::allocate_frame_zeroed()?;
        // The allocator manages only frames inside the direct map, so this address is one the
        // device can be given and the kernel can reach.
        LIVE.fetch_add(1, Ordering::Relaxed);
        Some(Self {
            phys: frame as u64 * PAGE_SIZE as u64,
        })
    }

    /// Release a DMA page only after the device can no longer access it.
    ///
    /// # Safety
    /// The caller must have stopped/reset every engine that can DMA to this page, retired its
    /// outstanding commands and prevented further submissions. There is deliberately no Drop:
    /// failed-start unwinding must pin memory rather than free it under an active controller.
    pub(crate) unsafe fn release(self) {
        mm::free_frame((self.phys / PAGE_SIZE as u64) as usize)
            .expect("DMA frame ownership corrupted");
        assert!(
            LIVE.fetch_sub(1, Ordering::AcqRel) > 0,
            "DMA ownership counter underflow"
        );
    }

    /// The physical address the device uses for `offset` within this page.
    pub(crate) fn address(&self, offset: u64) -> u64 {
        self.phys + offset
    }

    fn pointer(&self, offset: u64, len: usize) -> *mut u8 {
        assert!(
            offset + len as u64 <= PAGE_SIZE as u64,
            "dma access past its page"
        );
        phys::direct_ptr::<u8>(self.phys + offset)
    }

    pub(crate) fn write(&self, offset: u64, bytes: &[u8]) {
        let base = self.pointer(offset, bytes.len());
        for (index, byte) in bytes.iter().enumerate() {
            // SAFETY: `pointer` checked that the range is inside this driver's own frame, which
            // only this driver touches, and volatile because the device reads it.
            unsafe { core::ptr::write_volatile(base.add(index), *byte) };
        }
    }

    pub(crate) fn read(&self, offset: u64, out: &mut [u8]) {
        let base = self.pointer(offset, out.len());
        for (index, byte) in out.iter_mut().enumerate() {
            // SAFETY: as [`Dma::write`]; volatile because the device writes it.
            *byte = unsafe { core::ptr::read_volatile(base.add(index)) };
        }
    }

    pub(crate) fn read_array<const N: usize>(&self, offset: u64) -> [u8; N] {
        let mut bytes = [0u8; N];
        self.read(offset, &mut bytes);
        bytes
    }

    pub(crate) fn write16(&self, offset: u64, value: u16) {
        self.write(offset, &value.to_le_bytes());
    }

    pub(crate) fn read16(&self, offset: u64) -> u16 {
        let mut bytes = [0u8; 2];
        self.read(offset, &mut bytes);
        u16::from_le_bytes(bytes)
    }
}
