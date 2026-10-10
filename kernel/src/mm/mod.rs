//! Memory management.
//!
//! What is here in M3 is the part that has to exist before anything else can: a map of
//! physical frames built from the firmware's description of memory, with the regions the
//! kernel must never hand out marked unusable. The buddy allocator, the VAD tree, and the
//! kernel heap land in the same milestone; see `docs/architecture/03-memory-manager.md`.
//!
//! # Why the frame map is an array and not a structure
//!
//! `struct page` equivalents are indexed by frame number, so the array must be allocated
//! before the allocator exists. It is carved out of the direct map at a fixed address,
//! which is the same bootstrap problem the object map has, and it is solved the same way:
//! the thing that hands out memory cannot itself be handed out.

use core::fmt::Write;

use crate::hal::console::Console;
use crate::hal::multiboot::BootInfo;

/// Bytes in a physical page.
pub const PAGE_SIZE: usize = 4096;
/// Bits in a page offset.
pub const PAGE_SHIFT: u32 = 12;

/// The state of one physical frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameState {
    /// Not present, or reserved by firmware. Never allocated.
    Reserved = 0,
    /// Free and available to the allocator.
    Free = 1,
    /// Allocated to a user or kernel mapping.
    Allocated = 2,
    /// In use as a page-table page. Never handed to a user mapping.
    PageTable = 3,
    /// Free and already zeroed, so a user mapping can skip the zeroing step.
    Zeroed = 4,
}

/// The physical frame map.
///
/// Sized for up to 64 GiB of RAM at 4 KiB frames, which is 16.7 million frames. At one byte
/// per frame that is 16 MiB of kernel memory -- a real cost, and the reason M3 replaces the
/// byte-per-frame array with a sectioned map that only allocates for populated address
/// ranges. The limit is recorded here rather than discovered at boot.
pub const MAX_FRAMES: usize = 16 * 1024 * 1024;

/// The frame map, as a static array.
///
/// A `static mut` because there is no allocator yet to put it anywhere else, and no second
/// CPU to race with. M3 moves it into the direct map and puts a lock on it.
static mut FRAME_STATES: [FrameState; MAX_FRAMES] = [FrameState::Reserved; MAX_FRAMES];

/// How much of the frame map is populated, in frames.
static mut FRAME_MAP_LEN: usize = 0;

/// Statistics gathered at boot, reported on the console and later through `sysctl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryStats {
    /// Frames the firmware described as usable.
    pub usable_frames: u64,
    /// Frames reserved by firmware.
    pub reserved_frames: u64,
    /// Regions the firmware reported.
    pub regions: usize,
    /// Regions dropped for exceeding the kernel's table, which means the accounting is
    /// incomplete and the kernel says so loudly.
    pub dropped_regions: usize,
    /// Bytes of usable RAM.
    pub usable_bytes: u64,
    /// Highest usable physical address.
    pub highest_address: u64,
}

/// Build the frame map from the firmware's memory map.
pub fn init(console: &mut Console, info: &BootInfo) {
    // SAFETY: `info` was produced by the boot hand-off, which validated the Multiboot
    // magic. `memory_map` bounds-checks every read against the map's own length.
    let map = unsafe { info.memory_map() };

    if map.regions().is_empty() {
        init_without_firmware(console);
        return;
    }

    let mut stats = MemoryStats {
        regions: map.regions().len(),
        dropped_regions: map.truncated(),
        usable_bytes: map.usable_bytes(),
        highest_address: map.highest_usable(),
        ..MemoryStats::default()
    };

    // SAFETY: single boot processor, no interrupts enabled yet, and `FRAME_STATES` is
    // written only here during boot before anything else reads it.
    unsafe {
        let len = ((stats.highest_address as usize) / PAGE_SIZE).min(MAX_FRAMES);
        for state in FRAME_STATES.iter_mut().take(len) {
            *state = FrameState::Reserved;
        }
        for region in map.regions() {
            let first = (region.base as usize) / PAGE_SIZE;
            let last = (region.end() as usize).div_ceil(PAGE_SIZE);
            let state = if region.is_usable() { FrameState::Free } else { FrameState::Reserved };
            for index in first.min(len)..last.min(len) {
                FRAME_STATES[index] = state;
            }
            if region.is_usable() {
                stats.usable_frames += (last - first) as u64;
            } else {
                stats.reserved_frames += (last - first) as u64;
            }
        }
        FRAME_MAP_LEN = len;
    }

    let _ = writeln!(
        console,
        "mm: {} regions, {} MiB usable, {} MiB reserved, highest usable {:#x}",
        stats.regions,
        stats.usable_bytes / (1024 * 1024),
        (stats.reserved_frames * PAGE_SIZE as u64) / (1024 * 1024),
        stats.highest_address,
    );
    if stats.dropped_regions > 0 {
        let _ = writeln!(
            console,
            "mm: WARNING {} firmware regions exceeded the frame map and were dropped; \
             memory accounting is incomplete",
            stats.dropped_regions
        );
    }
    let _ = writeln!(console, "NOLX: mm online");
}

/// Build a conservative frame map when the loader gave us no memory map.
///
/// Uses `mem_upper`, which undercounts because it stops at the first hole above 1 MiB. The
/// kernel boots, with less memory than the machine has, and says so.
pub fn init_without_firmware(console: &mut Console) {
    let _ = writeln!(
        console,
        "mm: no firmware memory map; assuming a conservative 64 MiB. \
         Everything above that is unusable."
    );
    // SAFETY: as in [`init`].
    unsafe {
        let len = (64 * 1024 * 1024) / PAGE_SIZE;
        for state in FRAME_STATES.iter_mut().take(len) {
            *state = FrameState::Free;
        }
        // The first megabyte holds the real-mode structures, the BIOS data area, and the
        // kernel image itself. Never hand it out.
        for state in FRAME_STATES.iter_mut().take((1024 * 1024) / PAGE_SIZE) {
            *state = FrameState::Reserved;
        }
        FRAME_MAP_LEN = len;
    }
    let _ = writeln!(console, "NOLX: mm online");
}

/// The state of one frame, by frame number.
///
/// Returns [`FrameState::Reserved`] for a frame number outside the populated map, which is
/// the safe answer: an out-of-range frame must never look allocatable.
#[must_use]
pub fn frame_state(frame: usize) -> FrameState {
    // SAFETY: reading one byte of a static that only this module writes, after `init` has
    // run. Reads cannot tear on a `u8`-backed enum.
    unsafe {
        if frame >= FRAME_MAP_LEN {
            FrameState::Reserved
        } else {
            FRAME_STATES[frame]
        }
    }
}

/// Count the frames in a given state.
///
/// Linear over the map, so it is a diagnostic, not a hot path. The allocator keeps its own
/// counters.
#[must_use]
pub fn count_frames(state: FrameState) -> usize {
    // SAFETY: as [`frame_state`].
    unsafe { FRAME_STATES.iter().take(FRAME_MAP_LEN).filter(|s| **s == state).count() }
}

/// Convert a physical address to a frame number.
#[must_use]
pub const fn frame_of(address: u64) -> usize {
    (address as usize) >> PAGE_SHIFT
}

/// Convert a frame number to the address of its first byte.
#[must_use]
pub const fn address_of(frame: usize) -> u64 {
    (frame as u64) << PAGE_SHIFT
}

/// Round an address up to a page boundary.
#[must_use]
pub const fn page_align_up(address: u64) -> u64 {
    (address + (PAGE_SIZE as u64 - 1)) & !(PAGE_SIZE as u64 - 1)
}

/// Round an address down to a page boundary.
#[must_use]
pub const fn page_align_down(address: u64) -> u64 {
    address & !(PAGE_SIZE as u64 - 1)
}
