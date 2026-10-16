//! Kernel mappings for device registers outside the direct map.
//!
//! A BAR can name physical memory the direct map does not reach: above 2 GiB, or in the PCI
//! hole. The boot layer maps only RAM and the APIC and HPET windows, so such a range is mapped
//! here, uncached, into the MMIO window that `nolx_mm_core::mmio` lays out. The tables are
//! edited through the same walker as every other kernel mapping.
//!
//! The window's top-level table is created by [`init`], before any address space exists. Each
//! address space shares the kernel's PML4 entries by reference, so a table created later would
//! be invisible to the spaces made before it. Mappings are not removed yet, so the window only
//! grows, and [`Region`] checks every register access.

use crate::mm::phys;
use crate::mm::space::kernel_root;
use crate::mm::tables::KernelTableStore;
use crate::mm::PAGE_SHIFT;
use crate::sync::SyncCell;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::{self, MmioArena, MmioError};
use nolx_mm_core::paging::{Entry, EntryFlags, MapError, PageTables, TableStore};

/// Where the next window starts. Advanced only by [`map`], on the boot processor.
static ARENA: SyncCell<MmioArena> = SyncCell::new(MmioArena::new());

/// Why a mapping could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The window rules refused the request.
    Window(MmioError),
    /// The page tables could not be edited.
    Tables(MapError),
}

/// A mapped device range. Every access is checked for width, alignment, and bounds, and is
/// volatile, because the device changes these registers behind the processor's back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    base: u64,
    len: u64,
}

impl Region {
    /// The virtual address of the first byte the caller asked for.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// The length of the range in bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.len
    }

    /// The address of a `width`-byte access at `offset`, after checking it.
    fn address(&self, offset: u64, width: u64) -> Result<u64, MmioError> {
        mmio::check_access(self.len, offset, width)?;
        Ok(self.base + offset)
    }

    /// Read one byte at `offset`.
    pub fn read8(&self, offset: u64) -> Result<u8, MmioError> {
        let address = self.address(offset, 1)?;
        // SAFETY: `address` is inside a window `map` mapped uncached, and the access is checked
        // to be in range and aligned.
        Ok(unsafe { core::ptr::read_volatile(address as *const u8) })
    }

    /// Read the 16-bit register at `offset`.
    pub fn read16(&self, offset: u64) -> Result<u16, MmioError> {
        let address = self.address(offset, 2)?;
        // SAFETY: as [`Region::read8`].
        Ok(unsafe { core::ptr::read_volatile(address as *const u16) })
    }

    /// Read the 32-bit register at `offset`.
    pub fn read32(&self, offset: u64) -> Result<u32, MmioError> {
        let address = self.address(offset, 4)?;
        // SAFETY: as [`Region::read8`].
        Ok(unsafe { core::ptr::read_volatile(address as *const u32) })
    }

    /// Write one byte at `offset`.
    pub fn write8(&self, offset: u64, value: u8) -> Result<(), MmioError> {
        let address = self.address(offset, 1)?;
        // SAFETY: as [`Region::read8`].
        unsafe { core::ptr::write_volatile(address as *mut u8, value) };
        Ok(())
    }

    /// Write the 16-bit register at `offset`.
    pub fn write16(&self, offset: u64, value: u16) -> Result<(), MmioError> {
        let address = self.address(offset, 2)?;
        // SAFETY: as [`Region::read8`].
        unsafe { core::ptr::write_volatile(address as *mut u16, value) };
        Ok(())
    }

    /// Write the 32-bit register at `offset`.
    pub fn write32(&self, offset: u64, value: u32) -> Result<(), MmioError> {
        let address = self.address(offset, 4)?;
        // SAFETY: as [`Region::read8`].
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
        Ok(())
    }
}

/// Create the MMIO window's top-level table in the kernel's PML4.
///
/// Call once at boot, before any address space is created. The table is zeroed and charged to
/// the system PTE pool like every other page table.
pub fn init() -> Result<(), Failure> {
    let root = kernel_root();
    let mut store = KernelTableStore::new();
    if Entry::from_raw(store.entry(root, mmio::MMIO_SLOT)).is_present() {
        return Err(Failure::Tables(MapError::AlreadyMapped));
    }
    let table = store
        .allocate_table()
        .ok_or(Failure::Tables(MapError::OutOfMemory))?;
    let flags = EntryFlags::intermediate(mmio::mmio_flags());
    let entry = Entry::new(table << PAGE_SHIFT, flags);
    let entry = entry.ok_or(Failure::Tables(MapError::Misaligned))?;
    store.set_entry(root, mmio::MMIO_SLOT, entry.bits());
    Ok(())
}

/// Map `length` bytes of device memory at physical address `physical`, uncached and writable.
///
/// The window is planned first and then mapped. If the tables run out after planning, the
/// address range is spent: the window is never reused, so the failure costs address space and
/// nothing else.
pub fn map(physical: u64, length: u64) -> Result<Region, Failure> {
    // SAFETY: device setup runs on the boot processor before the periodic timer starts, and
    // nothing else advances the arena.
    let planned = unsafe { (*ARENA.get()).reserve(physical, length, phys::DIRECT_MAP_BYTES) };
    let window = planned.map_err(Failure::Window)?;
    let mut walker = PageTables::new(kernel_root());
    let mut store = KernelTableStore::new();
    let flags = mmio::mmio_flags();
    let mapped = walker.map_range(
        &mut store,
        window.virt,
        window.physical,
        window.bytes(),
        flags,
    );
    mapped.map_err(Failure::Tables)?;
    Ok(Region {
        base: window.requested_base(),
        len: length,
    })
}

/// The local APIC's register page. The boot layer maps it through the direct map as well.
const LAPIC_PHYSICAL: u64 = 0xFEE0_0000;
/// The local APIC's version register, a read-only dword in that page.
const APIC_VERSION: u64 = 0x30;

/// Map the local APIC through the window and check two things: the register reads the same
/// value as through the direct map, and the mapping is uncached. Panics if either fails, as
/// the other boot self tests do: a window that reads wrong would send every driver wrong.
pub fn selfcheck(console: &mut dyn HalConsole) {
    let region = match map(LAPIC_PHYSICAL, 0x1000) {
        Ok(region) => region,
        Err(error) => panic!("mmio: mapping the local APIC failed: {error:?}"),
    };
    let through_window = region
        .read32(APIC_VERSION)
        .expect("the version register is inside the one-page mapping");
    // SAFETY: the direct map covers the APIC page, which the boot layer maps uncached, and the
    // version register is read-only.
    let direct_register = phys::direct_ptr::<u32>(LAPIC_PHYSICAL + APIC_VERSION);
    let direct = unsafe { core::ptr::read_volatile(direct_register) };
    let walker = PageTables::new(kernel_root());
    let mapping = walker.translate(&KernelTableStore::new(), region.base());
    let uncached = mapping.is_some_and(|m| m.entry.flags().bits() & EntryFlags::CACHE_DISABLE != 0);
    if !uncached || through_window != direct {
        panic!(
            "mmio: window check failed: uncached={uncached}, \
             window {through_window:#x}, direct {direct:#x}"
        );
    }
    let _ = writeln!(
        console,
        "mm: mmio: LAPIC {LAPIC_PHYSICAL:#x} at {:#x}: uncached={uncached}, \
         version {through_window:#x} read through the window matches the direct map",
        region.base()
    );
}
