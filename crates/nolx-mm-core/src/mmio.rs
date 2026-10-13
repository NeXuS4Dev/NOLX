//! Uncached windows onto device registers that the direct map does not cover.
//!
//! The direct map reaches the first 2 GiB of RAM, and the boot layer adds only the APIC and HPET
//! windows. A PCI device's BARs can sit anywhere else in the physical address space, so the
//! kernel needs a second, dedicated region of its address space for them. This module decides
//! where such a window goes and which mapping bits it carries; the kernel does the table edits
//! through [`crate::paging::PageTables`], and the register accesses itself.
//!
//! The window lives in its own PML4 slot, directly above the direct map. It is never a cached
//! view of RAM: a physical range that overlaps the direct map is refused, because two mappings
//! of one page with different cache attributes are undefined on x86.

use crate::paging::{EntryFlags, MAX_PHYSICAL};

/// Size of a page in bytes.
const PAGE: u64 = 4096;

/// The PML4 slot of the MMIO window: slot 257, the one above the direct map at slot 256.
pub const MMIO_SLOT: usize = 257;

/// The first virtual address of the MMIO window, the start of PML4 slot 257.
pub const MMIO_BASE: u64 = 0xffff_8080_0000_0000;

/// How much address space the MMIO window holds: one PDPT entry, one gibibyte.
pub const MMIO_BYTES: u64 = 1 << 30;

/// The mapping bits every MMIO page carries: present, writable, never executable, and not
/// cached. With the architectural default PAT, cache-disable together with write-through
/// selects strong uncacheable (UC) memory, the type device registers need.
#[must_use]
pub const fn mmio_flags() -> EntryFlags {
    EntryFlags::from_bits(
        EntryFlags::PRESENT
            | EntryFlags::WRITABLE
            | EntryFlags::NO_EXECUTE
            | EntryFlags::CACHE_DISABLE
            | EntryFlags::WRITE_THROUGH,
    )
}

/// Why an MMIO request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmioError {
    /// A zero-length range has nothing to map.
    Empty,
    /// The range runs past the physical address space, or past the end of the window.
    OutOfRange,
    /// The range starts inside the direct map, which is cacheable RAM and must not be mapped
    /// a second time as uncached.
    Overlaps,
    /// The window has no room left for another range.
    Exhausted,
    /// An access is not aligned to its width.
    Misaligned,
    /// An access width is not 1, 2, or 4 bytes.
    BadWidth,
}

/// A planned window: the page-aligned virtual and physical ranges to map, and where the caller's
/// first byte falls inside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The first virtual page of the window.
    pub virt: u64,
    /// The first physical page the window maps.
    pub physical: u64,
    /// How many pages the window spans.
    pub pages: u64,
    /// The byte offset, within the first page, of the physical address the caller asked for.
    pub offset: u64,
}

impl Window {
    /// The window's size in bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.pages * PAGE
    }

    /// The virtual address of the physical address the caller asked for.
    #[must_use]
    pub const fn requested_base(&self) -> u64 {
        self.virt + self.offset
    }
}

/// The allocator for MMIO windows. It hands out address space upward and never reuses it: a
/// window is not unmapped in this slice, so its addresses are not recycled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioArena {
    next: u64,
}

impl MmioArena {
    /// An arena with the whole MMIO window free.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: MMIO_BASE }
    }

    /// Plan a window covering `length` bytes at physical address `physical`.
    ///
    /// `direct_map_bytes` is the amount of physical memory the direct map covers; any range
    /// starting below it is refused. Each window is followed by one unmapped guard page, so a
    /// stray access past the end of a window faults rather than reaching the next one.
    pub fn reserve(
        &mut self,
        physical: u64,
        length: u64,
        direct_map_bytes: u64,
    ) -> Result<Window, MmioError> {
        if length == 0 {
            return Err(MmioError::Empty);
        }
        if physical < direct_map_bytes {
            return Err(MmioError::Overlaps);
        }
        let offset = physical % PAGE;
        let first = physical - offset;
        let total = offset.checked_add(length).ok_or(MmioError::OutOfRange)?;
        let pages = total.div_ceil(PAGE);
        let bytes = pages.checked_mul(PAGE).ok_or(MmioError::OutOfRange)?;
        let end = first.checked_add(bytes).ok_or(MmioError::OutOfRange)?;
        if end > MAX_PHYSICAL {
            return Err(MmioError::OutOfRange);
        }
        let window_end = MMIO_BASE + MMIO_BYTES;
        let remaining = window_end.saturating_sub(self.next);
        if bytes > remaining {
            return Err(MmioError::Exhausted);
        }
        let window = Window {
            virt: self.next,
            physical: first,
            pages,
            offset,
        };
        self.next = window.virt + bytes + PAGE;
        Ok(window)
    }
}

impl Default for MmioArena {
    fn default() -> Self {
        Self::new()
    }
}

/// Check that a `width`-byte access at `offset` lies inside a region of `region_bytes` and is
/// aligned to its width.
pub const fn check_access(region_bytes: u64, offset: u64, width: u64) -> Result<(), MmioError> {
    if width != 1 && width != 2 && width != 4 {
        return Err(MmioError::BadWidth);
    }
    if offset % width != 0 {
        return Err(MmioError::Misaligned);
    }
    match offset.checked_add(width) {
        Some(end) if end <= region_bytes => Ok(()),
        _ => Err(MmioError::OutOfRange),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECT_MAP: u64 = 2 << 30;

    #[test]
    fn the_window_covers_the_requested_range_with_its_offset() {
        let mut arena = MmioArena::new();
        let window = arena
            .reserve(0xFEE0_0030, 0x10, DIRECT_MAP)
            .expect("planned");
        assert_eq!(window.virt, MMIO_BASE);
        assert_eq!(window.physical, 0xFEE0_0000);
        assert_eq!(window.pages, 1);
        assert_eq!(window.offset, 0x30);
        assert_eq!(window.requested_base(), MMIO_BASE + 0x30);
    }

    #[test]
    fn a_range_that_crosses_a_page_boundary_takes_both_pages() {
        let mut arena = MmioArena::new();
        let window = arena.reserve(0xFE00_0FFF, 2, DIRECT_MAP).expect("planned");
        assert_eq!(window.pages, 2);
        assert_eq!(window.bytes(), 2 * PAGE);
        assert_eq!(window.offset, 0xFFF);
    }

    #[test]
    fn an_empty_range_is_refused() {
        let mut arena = MmioArena::new();
        assert_eq!(
            arena.reserve(0xFE00_0000, 0, DIRECT_MAP),
            Err(MmioError::Empty)
        );
    }

    #[test]
    fn ranges_past_the_physical_address_space_are_refused() {
        let mut arena = MmioArena::new();
        assert_eq!(
            arena.reserve(u64::MAX - 1, 16, DIRECT_MAP),
            Err(MmioError::OutOfRange)
        );
        assert_eq!(
            arena.reserve(MAX_PHYSICAL - PAGE, 2 * PAGE, DIRECT_MAP),
            Err(MmioError::OutOfRange)
        );
        assert!(arena.reserve(MAX_PHYSICAL - PAGE, PAGE, DIRECT_MAP).is_ok());
    }

    #[test]
    fn a_range_that_starts_inside_the_direct_map_is_refused() {
        let mut arena = MmioArena::new();
        // The last page of the direct map: it is cacheable RAM already.
        assert_eq!(
            arena.reserve(DIRECT_MAP - PAGE, PAGE, DIRECT_MAP),
            Err(MmioError::Overlaps)
        );
        // The first page above it is device space.
        assert!(arena.reserve(DIRECT_MAP, PAGE, DIRECT_MAP).is_ok());
    }

    #[test]
    fn windows_are_disjoint_and_separated_by_a_guard_page() {
        let mut arena = MmioArena::new();
        let first = arena
            .reserve(0xC000_0000, 0x2000, DIRECT_MAP)
            .expect("first");
        let second = arena
            .reserve(0xD000_0000, 0x1000, DIRECT_MAP)
            .expect("second");
        assert_eq!(second.virt, first.virt + first.bytes() + PAGE);
    }

    #[test]
    fn the_window_runs_out_and_then_refuses() {
        let mut arena = MmioArena::new();
        // Exactly the whole window: it fits, and the guard page after it does not.
        let all = arena
            .reserve(0x1_0000_0000, MMIO_BYTES, DIRECT_MAP)
            .expect("whole window");
        assert_eq!(all.bytes(), MMIO_BYTES);
        assert_eq!(
            arena.reserve(0x2_0000_0000, PAGE, DIRECT_MAP),
            Err(MmioError::Exhausted)
        );
    }

    #[test]
    fn a_window_too_large_for_what_is_left_is_refused_without_moving_the_arena() {
        let mut arena = MmioArena::new();
        let before = arena;
        assert_eq!(
            arena.reserve(0x1_0000_0000, MMIO_BYTES + PAGE, DIRECT_MAP),
            Err(MmioError::Exhausted)
        );
        assert_eq!(arena, before);
    }

    #[test]
    fn mmio_pages_are_uncached_writable_and_never_executable() {
        let flags = mmio_flags();
        for bit in [
            EntryFlags::PRESENT,
            EntryFlags::WRITABLE,
            EntryFlags::NO_EXECUTE,
            EntryFlags::CACHE_DISABLE,
            EntryFlags::WRITE_THROUGH,
        ] {
            assert!(
                flags.contains(EntryFlags::from_bits(bit)),
                "missing bit {bit:#x}"
            );
        }
        for bit in [EntryFlags::USER, EntryFlags::GLOBAL, EntryFlags::HUGE] {
            assert_eq!(flags.bits() & bit, 0, "unexpected bit {bit:#x}");
        }
    }

    #[test]
    fn the_window_slot_is_the_one_above_the_direct_map() {
        assert_eq!(crate::paging::pml4_index(MMIO_BASE), MMIO_SLOT);
        assert_eq!(
            crate::paging::pml4_index(crate::paging::DIRECT_MAP_BASE),
            256
        );
        assert_eq!(MMIO_BASE + MMIO_BYTES - 1, 0xffff_8080_3fff_ffff);
    }

    #[test]
    fn accesses_must_be_aligned_and_inside_the_region() {
        assert_eq!(check_access(0x100, 0x10, 4), Ok(()));
        assert_eq!(check_access(0x100, 0xFC, 4), Ok(()));
        assert_eq!(check_access(0x100, 0xFD, 4), Err(MmioError::Misaligned));
        assert_eq!(check_access(0x100, 0xFE, 4), Err(MmioError::Misaligned));
        assert_eq!(check_access(0x100, 0xFE, 2), Ok(()));
        assert_eq!(check_access(0x100, 0x102, 2), Err(MmioError::OutOfRange));
        assert_eq!(check_access(0x100, 0x100, 1), Err(MmioError::OutOfRange));
        assert_eq!(check_access(0x100, 0xFF, 1), Ok(()));
        assert_eq!(
            check_access(0x100, u64::MAX - 1, 4),
            Err(MmioError::Misaligned)
        );
        assert_eq!(
            check_access(0x100, u64::MAX - 3, 4),
            Err(MmioError::OutOfRange)
        );
        assert_eq!(check_access(0x100, 0, 8), Err(MmioError::BadWidth));
        assert_eq!(check_access(0x100, 0, 3), Err(MmioError::BadWidth));
    }
}
