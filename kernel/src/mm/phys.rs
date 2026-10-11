//! Physical addresses as kernel pointers.
//!
//! The kernel runs in the higher half, and physical memory is reachable through exactly two
//! windows, both installed by `boot/boot.S` before long mode:
//!
//! * the **direct map** -- physical address `p` at `DIRECT_MAP_BASE + p`, covering the first
//!   2 GiB of RAM and the standard APIC/HPET MMIO windows. Every physical address the kernel
//!   computes (a frame from the allocator, an ACPI table, a memory-mapped register) is turned
//!   into a pointer here and nowhere else;
//! * the **image window** -- the kernel's own frames at `KERNEL_IMAGE_BASE + p`. Kernel code,
//!   data, and statics are addressed through it, because that is where the linker put them.
//!
//! The boot layer's identity map is gone by the time any code in this module runs: the
//! trampoline switches to the image window before it enters Rust, and the kernel clears
//! PML4 entry 0 as soon as its own descriptor tables are live
//! ([`crate::mm::tables::drop_identity_map`]). That is the whole point of the switch -- a
//! physical address and a pointer are no longer the same number, so a wild pointer cannot
//! quietly walk into firmware memory.

/// Base of the direct map: physical address 0 appears here. `boot/boot.S` fills PML4 slot
/// 256; `nolx_mm_core::paging::DIRECT_MAP_BASE` is the same constant as seen by the walker.
pub const DIRECT_MAP_BASE: u64 = nolx_mm_core::paging::DIRECT_MAP_BASE;

/// Base of the kernel image window: the kernel's own frames appear here.
/// `kernel/linker.ld` links the image at this offset and `boot/boot.S` fills PML4 slot 511;
/// the constant -- and why the window is in the top 2 GiB in the first place -- is
/// documented next to the layout arithmetic in `nolx_mm_core::paging`.
pub const KERNEL_IMAGE_BASE: u64 = nolx_mm_core::paging::KERNEL_IMAGE_BASE;

/// Physical address the image is loaded at: the Multiboot `load_addr`.
pub const KERNEL_LOAD_ADDR: u64 = 0x0010_0000;

/// How much physical memory the direct map covers: the boot layer maps the first 2 GiB with
/// 2 MiB pages. `kernel/src/mm/store.rs` refuses to manage frames above it, because writing a
/// free-list link through an unmapped address is a triple fault, not a bug report.
pub const DIRECT_MAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The kernel's pointer to physical address `physical`.
///
/// Caller must know that `physical` is inside the direct map's range; nothing here can check
/// it, because the answer is also a legal address for a `0` argument out of range.
#[must_use]
pub const fn direct(physical: u64) -> usize {
    (DIRECT_MAP_BASE + physical) as usize
}

/// The kernel's pointer to physical address `physical`, typed.
#[must_use]
pub const fn direct_ptr<T>(physical: u64) -> *mut T {
    direct(physical) as *mut T
}

/// The physical address behind a kernel pointer, for either window.
///
/// Returns `None` for an address in neither window: a low address (the identity map is gone),
/// or a hole in the kernel half. Callers that must distinguish should not guess a window --
/// this is also the check that catches a stale physical address being used as a pointer.
#[must_use]
pub const fn physical(address: u64) -> Option<u64> {
    if address >= KERNEL_IMAGE_BASE {
        Some(address - KERNEL_IMAGE_BASE)
    } else if address >= DIRECT_MAP_BASE {
        Some(address - DIRECT_MAP_BASE)
    } else {
        None
    }
}

/// Whether a physical address is inside the range the direct map covers.
#[must_use]
pub const fn is_mapped_physical(physical: u64) -> bool {
    physical < DIRECT_MAP_BYTES
}
