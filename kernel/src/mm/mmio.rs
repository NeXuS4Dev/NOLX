//! Kernel mappings for device registers outside the direct map.
//!
//! A BAR can name physical memory the direct map does not reach: above 2 GiB, or in the PCI
//! hole. The boot layer maps only RAM and the APIC and HPET windows, so such a range is mapped
//! here, uncached, into the MMIO window that `nolx_mm_core::mmio` lays out. The tables are
//! edited through the same walker as every other kernel mapping.
//!
//! The window's top-level table is created by [`init`], before any address space exists. Each
//! address space shares the kernel's PML4 entries by reference, so a table created later would
//! be invisible to the spaces made before it. Unmapping preserves that shared top-level child.
//! Reservations are serialized, generation-checked and reusable after their users retire.

use crate::mm::phys;
use crate::mm::space::kernel_root;
use crate::mm::tables::KernelTableStore;
use crate::mm::PAGE_SHIFT;
use crate::sync::IrqMutex;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::{self, MappingId, MmioError, ReusableMmioArena, Window};
use nolx_mm_core::paging::{Entry, EntryFlags, MapError, PageTables, TableStore};

/// Protects reservations and their page-table edits, including post-boot driver restarts.
static ARENA: IrqMutex<ReusableMmioArena<256>> = IrqMutex::new(ReusableMmioArena::new());

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
    id: MappingId,
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
        ARENA.lock().get(self.id)?;
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

/// Map unowned platform registers; their lifetime extends through boot and driver restart.
pub fn map(physical: u64, length: u64) -> Result<Region, Failure> {
    map_region(None, physical, length)
}

/// Map registers owned by one framework handle. Failed starts conservatively retain them.
pub(crate) fn map_owned(owner: usize, physical: u64, length: u64) -> Result<Region, Failure> {
    map_region(Some(owner), physical, length)
}

fn map_region(owner: Option<usize>, physical: u64, length: u64) -> Result<Region, Failure> {
    map_with_store(owner, physical, length, &mut KernelTableStore::new())
}

fn map_with_store(
    owner: Option<usize>,
    physical: u64,
    length: u64,
    store: &mut impl TableStore,
) -> Result<Region, Failure> {
    let mut arena = ARENA.lock();
    let (id, window) = arena
        .reserve(physical, length, phys::DIRECT_MAP_BYTES, owner)
        .map_err(Failure::Window)?;
    let mut walker = PageTables::new(kernel_root());
    if let Err(error) = walker.map_range(
        store,
        window.virt,
        window.physical,
        window.bytes(),
        mmio::mmio_flags(),
    ) {
        // This reservation was fresh, so every mapped leaf in it belongs to this operation.
        // Roll back partial leaves and empty lower tables before recycling any virtual address.
        clear_window(&mut walker, store, window);
        arena.release(id).expect("MMIO rollback lost reservation");
        return Err(Failure::Tables(error));
    }
    Ok(Region {
        id,
        base: window.requested_base(),
        len: length,
    })
}

fn clear_window(walker: &mut PageTables, store: &mut impl TableStore, window: Window) {
    let mut at = window.virt;
    while at < window.virt + window.bytes() {
        let size = walker.translate(store, at).map_or(4096, |m| m.size);
        match walker.unmap_shared(store, at) {
            Ok(_) | Err(MapError::NotMapped) => {}
            Err(error) => panic!("MMIO teardown failed: {error:?}"),
        }
        // SAFETY: a canonical page in our exclusively locked MMIO reservation was removed.
        // NOLX currently runs only the BSP; no remote TLB can retain this translation.
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) at, options(nostack, preserves_flags));
        }
        at += size;
    }
}

/// Retire all windows for one successfully stopped driver, preserving shared PML4 children.
///
/// # Safety
/// All CPU/IRQ users and raw addresses of these windows must have been retired, and no
/// concurrent access may be in progress. Only the BSP is supported (no remote TLB shootdown).
/// Old Region copies subsequently reject register accesses using their generation identity.
pub(crate) unsafe fn release_owned(owner: usize) {
    let mut arena = ARENA.lock();
    let mut walker = PageTables::new(kernel_root());
    let mut store = KernelTableStore::new();
    while let Some(id) = arena.first_owned(owner) {
        let window = arena.get(id).expect("MMIO owner lost reservation");
        clear_window(&mut walker, &mut store, window);
        arena.release(id).expect("MMIO release lost reservation");
    }
}

/// Live mappings, rather than cumulative restart allocations.
#[cfg(any(
    feature = "hal-driver-lifecycle-test",
    feature = "hal-e1000-recovery-test",
    feature = "hal-virtio-net-recovery-test",
    feature = "hal-xhci-lifecycle-test",
    feature = "hal-xhci-recovery-test",
    feature = "hal-xhci-transfer-fault-test"
))]
pub(crate) fn live_mappings() -> usize {
    ARENA.lock().live()
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

/// Exercise real stale-handle rejection, reuse and partial table-allocation rollback after boot.
#[cfg(feature = "hal-driver-lifecycle-test")]
pub(crate) fn lifecycle_selftest(console: &mut dyn HalConsole) {
    struct Budget {
        inner: KernelTableStore,
        remaining: usize,
    }
    impl TableStore for Budget {
        fn entry(&self, frame: u64, index: usize) -> u64 {
            self.inner.entry(frame, index)
        }
        fn set_entry(&mut self, frame: u64, index: usize, value: u64) {
            self.inner.set_entry(frame, index, value);
        }
        fn allocate_table(&mut self) -> Option<u64> {
            if self.remaining == 0 {
                return None;
            }
            self.remaining -= 1;
            self.inner.allocate_table()
        }
        fn release_table(&mut self, frame: u64) {
            self.inner.release_table(frame);
        }
        fn is_empty_table(&self, frame: u64) -> bool {
            self.inner.is_empty_table(frame)
        }
    }
    let before = live_mappings();
    let root = KernelTableStore::new().entry(kernel_root(), mmio::MMIO_SLOT);
    {
        let budget = 0;
        let frames = crate::mm::free_frames();
        let mut store = Budget {
            inner: KernelTableStore::new(),
            remaining: budget,
        };
        assert_eq!(
            map_with_store(
                Some(usize::MAX),
                LAPIC_PHYSICAL,
                (4 << 20) + 4096,
                &mut store
            ),
            Err(Failure::Tables(MapError::OutOfMemory))
        );
        assert_eq!(live_mappings(), before, "failed map leaked reservation");
        assert_eq!(
            crate::mm::free_frames(),
            frames,
            "failed map leaked page tables"
        );
        assert_eq!(
            KernelTableStore::new().entry(kernel_root(), mmio::MMIO_SLOT),
            root
        );
    }
    let first = map_owned(usize::MAX, LAPIC_PHYSICAL, 4096).unwrap();
    let version = first.read32(APIC_VERSION).unwrap(); // Populate a real translation.
                                                       // SAFETY: only this selftest owns the temporary alias; no IRQ/raw-pointer user references it.
    unsafe {
        release_owned(usize::MAX);
    }
    assert!(PageTables::new(kernel_root())
        .translate(&KernelTableStore::new(), first.base())
        .is_none());
    assert_eq!(first.read32(APIC_VERSION), Err(MmioError::Stale));
    let second = map_owned(usize::MAX, LAPIC_PHYSICAL, 4096).unwrap();
    assert_eq!(
        second.base(),
        first.base(),
        "released window was not reused"
    );
    assert_eq!(second.read32(APIC_VERSION), Ok(version));
    assert_eq!(first.write32(APIC_VERSION, 0), Err(MmioError::Stale));
    // SAFETY: the replacement alias also has no users outside this selftest.
    unsafe {
        release_owned(usize::MAX);
    }
    assert_eq!(live_mappings(), before);
    assert_eq!(
        KernelTableStore::new().entry(kernel_root(), mmio::MMIO_SLOT),
        root
    );
    let _ = writeln!(console, "mm: MMIO lifecycle: stale access refused, VA reused, partial-map OOM rolled back, shared root retained");
}
