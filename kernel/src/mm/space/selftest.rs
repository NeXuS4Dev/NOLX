//! The address-space power-on self test.
//!
//! A page fault is the one exception a kernel can survive, and the machinery that survives it
//! is the machinery that hands a process memory it has not touched yet. That machinery is also
//! the easiest place in the kernel to create a *silent* corruption: a page mapped without
//! zeroing, a copy-on-write page shared instead of copied, a frame released twice. So the boot
//! path does not trust it -- it builds two address spaces, faults on purpose, writes into the
//! pages, and then insists that every frame, every page table, and every commit byte came back
//! exactly where it started.
//!
//! What is exercised, in order:
//!
//! * a new address space: an empty user half, the kernel's own half shared (the direct map and
//!   the image window translate in the new root before it is ever loaded into CR3);
//! * demand zero: a committed private page, a read-only page, and the zero fill;
//! * the refusals: an address in no VAD, an address in the kernel half, a write to a page the
//!   VAD calls read-only, and an explicit guard page;
//! * stack growth: a fault below a grow-down region extends it and coalesces back into the
//!   stack's own descriptor, and a fault below the space's floor is refused;
//! * copy on write: sharing a page into a second address space, writing in each, and checking
//!   that the data on the two sides moved independently -- the single thing `fork` needs to be
//!   correct, and the single thing a bug in this code would get wrong;
//! * teardown: both spaces unmapped, every table reclaimed, the free-frame count and the
//!   live-page-table count back where they started, and the frame-map audit clean.

use crate::mm::space::{self, Fault, Space, ERROR_WRITE};
use crate::mm::tables;
use crate::mm::{self, phys};
use crate::sync::SyncCell;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::commit::PoolKind;
use nolx_mm_core::paging::Protection;
use nolx_mm_core::vad::{VadFlags, VadKind, VadRequest};

/// A private read-write range: one megabyte, all of it demand-zero until touched.
const PRIVATE_START: u64 = 0x40_0000;
const PRIVATE_END: u64 = 0x50_0000;
/// A read-only range: the write refusal subject.
const RO_START: u64 = 0x60_0000;
const RO_END: u64 = 0x61_0000;
/// An explicit guard page: a fault here is a program error, not a mapping hint.
const GUARD_START: u64 = 0x70_0000;
/// A grow-down stack, and the floor it may not grow past.
const STACK_START: u64 = 0x80_0000;
const STACK_TOP: u64 = 0x81_0000;
const STACK_FLOOR: u64 = 0x7E_0000;
/// The one page below the stack that the fault phase grows it down by. The grown page is
/// committed, so the first space carries one page more than its descriptors were written with.
const GROWN_START: u64 = STACK_START - 0x1000;
const GROWN_BYTES: u64 = 0x1000;
/// A hole between two mappings: nothing owns this address, and nothing may map it.
const HOLE: u64 = 0x55_0000;
/// The byte written into pages whose contents must survive a copy-on-write share.
const MARKER: u8 = 0x5A;
/// The byte written after a copy-on-write, to show the two sides move independently.
const SECOND_MARKER: u8 = 0xA5;
/// The user pages the first address space maps: two private, one read-only, one stack page.
const FIRST_SPACE_PAGES: u64 = 4;
/// The user pages the second address space maps: the two it shares.
const SECOND_SPACE_PAGES: u64 = 2;
/// Bytes of the first space's private range that are shared with the second.
const SHARED_BYTES: u64 = SECOND_SPACE_PAGES * 0x1000;

/// The address space the test drives.
static SPACE_A: SyncCell<Option<Space>> = SyncCell::new(None);
/// The address space it shares pages with.
static SPACE_B: SyncCell<Option<Space>> = SyncCell::new(None);

/// What the self test did, for the boot log.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpaceReport {
    /// Address spaces created and torn down.
    pub spaces: u64,
    /// Live descriptors described across both spaces.
    pub vads: u64,
    /// Faults resolved by demand zero.
    pub demand_zero: u64,
    /// Faults resolved by copying a shared page.
    pub copy_on_write: u64,
    /// Faults resolved by growing a grow-down region.
    pub stack_growth: u64,
    /// Faults refused, with the reason checked.
    pub refusals: u64,
    /// Pages mapped in total, across both spaces.
    pub pages_mapped: u64,
    /// Free frames before and after.
    pub frames: (u64, u64),
    /// Page tables allocated and released, before and after: the counters are
    /// `(allocated before, released before, allocated after, released after)`.
    pub tables: (u64, u64, u64, u64),
    /// Highest charge the system PTE pool reached, in bytes.
    pub pool_peak: u64,
}

impl core::fmt::Display for SpaceReport {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{} space(s), {} VAD(s), {} fault cause(s) resolved ({} demand-zero, {} copy-on-write, \
             {} stack growth), {} refusal(s); {} page(s) mapped; free frames {} -> {}; page tables \
             allocated {} -> {}, released {} -> {}; system PTE peak {} KiB; audit ok",
            self.spaces,
            self.vads,
            self.demand_zero + self.copy_on_write + self.stack_growth,
            self.demand_zero,
            self.copy_on_write,
            self.stack_growth,
            self.refusals,
            self.pages_mapped,
            self.frames.0,
            self.frames.1,
            self.tables.0,
            self.tables.2,
            self.tables.1,
            self.tables.3,
            self.pool_peak / 1024,
        )
    }
}

/// Run the self test. A failure is reported and then panics: a kernel whose address spaces do
/// not account for their frames cannot be trusted with a process.
pub fn run(console: &mut dyn HalConsole) -> Result<SpaceReport, &'static str> {
    let (allocated, released) = tables::totals();
    let mut report = SpaceReport {
        frames: (mm::free_frames(), 0),
        tables: (allocated, released, 0, 0),
        ..SpaceReport::default()
    };

    create_first(console, &mut report)?;
    describe_ranges(&mut report)?;
    drive_faults(&mut report)?;
    drive_copy_on_write(&mut report)?;
    teardown(&mut report)?;

    if report.frames.1 != report.frames.0 {
        return Err("the address spaces did not give every frame back");
    }
    // The table counters cover the whole boot, so what the self test took and gave back is the
    // *difference* across it: a table the boot layer built is not the test's to release.
    let allocated = report.tables.2 - report.tables.0;
    let released = report.tables.3 - report.tables.1;
    if allocated == 0 {
        return Err("the address spaces did not allocate a page table at all");
    }
    if allocated != released {
        return Err("the address spaces did not give every page table back");
    }
    report.pool_peak = mm::pool::snapshot()
        .iter()
        .find(|snapshot| snapshot.kind == PoolKind::SystemPte)
        .map_or(0, |snapshot| snapshot.peak);
    if report.pool_peak < allocated * 4096 {
        return Err("the system PTE pool was not charged for the page tables the test allocated");
    }
    let _ = writeln!(console, "NOLX: address space selftest: {report}");
    Ok(report)
}

/// The first address space: an empty user half and the kernel's half shared.
fn create_first(
    console: &mut dyn HalConsole,
    report: &mut SpaceReport,
) -> Result<(), &'static str> {
    let kernel_root = tables::current_root();
    let space = Space::create().ok_or("could not create an address space")?;
    if space.root() == kernel_root {
        return Err("a new address space reused the kernel's page-table root");
    }
    if space.commit_used() != 0 || space.pages_mapped() != 0 || space.live_tables() != 0 {
        return Err("a new address space was not empty");
    }
    if space.translate(PRIVATE_START).is_some() {
        return Err("a new address space already maps a user address");
    }
    // The kernel half has to be reachable *before* the root is ever loaded: a switch to a root
    // whose direct map is missing is not a fault, it is a triple fault.
    if space
        .translate(phys::DIRECT_MAP_BASE + phys::KERNEL_LOAD_ADDR)
        .is_none()
    {
        return Err("a new address space does not share the direct map");
    }
    if space
        .translate(phys::KERNEL_IMAGE_BASE + phys::KERNEL_LOAD_ADDR)
        .is_none()
    {
        return Err("a new address space does not share the image window");
    }
    let _ = writeln!(
        console,
        "NOLX: address space online: PML4 {:#x}, user half empty, direct map and image window \
         shared with {} live table(s) under it",
        space.root() << 12,
        space.live_tables()
    );

    // SAFETY: single boot processor; the self test is the only writer of this cell, and no
    // other reference to the space exists between here and teardown.
    unsafe {
        *SPACE_A.get() = Some(space);
    }
    report.spaces += 1;
    Ok(())
}

/// Describe the ranges: nothing is mapped yet, and everything is committed.
fn describe_ranges(report: &mut SpaceReport) -> Result<(), &'static str> {
    let space = a();
    space.set_stack_floor(STACK_FLOOR);
    space
        .reserve(VadRequest::private_anonymous(PRIVATE_START, PRIVATE_END))
        .map_err(|_| "could not describe a private range")?;
    space
        .reserve(VadRequest {
            start: RO_START,
            end: RO_END,
            kind: VadKind::Private,
            // Read-only, and it can never become writable: `maximum` is what `mprotect` is
            // checked against, so a later write is refused by the tree, not by the hardware.
            protection: Protection::from_bits(Protection::READ),
            maximum: Protection::from_bits(Protection::READ),
            flags: VadFlags::empty(),
            backing: None,
        })
        .map_err(|_| "could not describe a read-only range")?;
    space
        .reserve(VadRequest {
            start: GUARD_START,
            end: GUARD_START + 0x1000,
            kind: VadKind::Guard,
            protection: Protection::from_bits(0),
            maximum: Protection::from_bits(0),
            flags: VadFlags::empty(),
            backing: None,
        })
        .map_err(|_| "could not describe a guard page")?;
    space
        .reserve(VadRequest {
            start: STACK_START,
            end: STACK_TOP,
            kind: VadKind::Stack,
            protection: Protection::read_write(),
            maximum: Protection::read_write(),
            flags: VadFlags::from_bits(VadFlags::GROWSDOWN),
            backing: None,
        })
        .map_err(|_| "could not describe a stack")?;

    let committed = (PRIVATE_END - PRIVATE_START) + (RO_END - RO_START) + (STACK_TOP - STACK_START);
    if space.commit_used() != committed {
        return Err("the commit charge does not match the committed ranges");
    }
    if space.live_tables() != 0 {
        return Err("describing ranges allocated a page table");
    }
    space
        .check()
        .map_err(|_| "the VAD tree's invariants do not hold after describing the ranges")?;
    report.vads = 4;
    Ok(())
}

/// The four fault causes, and the refusals that go with them.
fn drive_faults(report: &mut SpaceReport) -> Result<(), &'static str> {
    a().activate();

    // Demand zero: the first touch of a committed private page.
    let Fault::DemandZero { frame } = expect_zero(PRIVATE_START)? else {
        return Err("a committed page was not resolved by demand zero");
    };
    if mm::frame_refcount(frame) != 1 {
        return Err("a demand-zero page is not referenced exactly once");
    }
    // The page must be *zero*: handing a process a frame with the previous owner's bytes in it
    // is the bug this check exists for.
    // SAFETY: the frame was just allocated to this address space and the direct map is the
    // kernel's view of it; nothing else is looking at the page.
    let bytes =
        unsafe { core::slice::from_raw_parts(phys::direct_ptr::<u8>(mm::address_of(frame)), 4096) };
    if bytes.iter().any(|byte| *byte != 0) {
        return Err("a demand-zero page is not zeroed");
    }

    // A second page in the same range, then the writes: a page is written only after the fault
    // that mapped it, and what was written has to still be there.
    let _ = expect_zero(PRIVATE_START + 0x1000)?;
    write_user(PRIVATE_START, MARKER);
    write_user(PRIVATE_START + 0x1000, MARKER);
    if read_user(PRIVATE_START) != MARKER || read_user(PRIVATE_START + 0x1000) != MARKER {
        return Err("a demand-zero page did not keep what was written to it");
    }
    report.demand_zero += 2;

    // The read-only range faults in read-only, and a write to it is refused by the *tree*
    // before any frame is taken.
    let _ = expect_zero(RO_START)?;
    report.demand_zero += 1;
    match space::resolve(RO_START, ERROR_WRITE) {
        Err(space::FaultError::Protection) => report.refusals += 1,
        Err(_) => return Err("a write to a read-only page was refused for another reason"),
        Ok(_) => return Err("a write to a read-only page was resolved"),
    }

    // An address in no VAD, an address in the kernel half, and the guard page: three
    // refusals, each with its own reason.
    match space::resolve(HOLE, ERROR_WRITE) {
        Err(space::FaultError::NotMapped) => report.refusals += 1,
        Err(_) => return Err("an address in no VAD was refused for another reason"),
        Ok(_) => return Err("an address in no VAD was mapped"),
    }
    match space::resolve(phys::DIRECT_MAP_BASE, 0) {
        Err(space::FaultError::KernelAddress) => report.refusals += 1,
        Err(_) => return Err("a fault in the kernel half was refused for another reason"),
        Ok(_) => return Err("a fault in the kernel half was resolved"),
    }
    match space::resolve(GUARD_START, 0) {
        Err(space::FaultError::GuardPage) => report.refusals += 1,
        Err(_) => return Err("a fault on a guard page was refused for another reason"),
        Ok(_) => return Err("a fault on a guard page was mapped"),
    }

    // Stack growth: the fault below the region extends it, and the new descriptor coalesces
    // with the stack's own, so a growing stack stays one VAD.
    let grown = GROWN_START;
    match space::resolve(grown, ERROR_WRITE) {
        Ok(Fault::StackGrowth { from, to }) if from == STACK_START && to == grown => {
            report.stack_growth += 1;
        }
        _ => return Err("a fault below the stack did not grow it by one page"),
    }
    write_user(grown, MARKER);
    let space = a();
    match space.vads().find(grown) {
        Some(vad) if vad.kind == VadKind::Stack && vad.start == grown && vad.end == STACK_TOP => {}
        _ => return Err("stack growth did not coalesce with the stack's descriptor"),
    }
    // Below the floor the same fault is refused: that refusal is what SIGSEGV will be built on.
    match space::resolve(STACK_FLOOR - 0x1000, ERROR_WRITE) {
        Err(space::FaultError::StackLimit) => report.refusals += 1,
        Err(_) => return Err("a stack fault below the floor was refused for another reason"),
        Ok(_) => return Err("a stack fault below the floor was grown anyway"),
    }

    a().check()
        .map_err(|_| "the VAD tree's invariants do not hold after the faults")?;
    if a().pages_mapped() != FIRST_SPACE_PAGES {
        return Err("the first address space did not map exactly its four pages");
    }
    a().deactivate();
    Ok(())
}

/// Copy on write: share the private range into a second address space and write in both.
fn drive_copy_on_write(report: &mut SpaceReport) -> Result<(), &'static str> {
    let second = Space::create().ok_or("could not create a second address space")?;
    // SAFETY: as in `create_first`: one writer, and no live reference to the space yet.
    unsafe {
        *SPACE_B.get() = Some(second);
    }
    report.spaces += 1;

    // Sharing happens with the parent active: it is the parent's entries that are made
    // read-only, and only the running address space can have them shot down.
    a().activate();
    let pages = a()
        .share_into(b(), PRIVATE_START, PRIVATE_START + SHARED_BYTES)
        .map_err(|_| "sharing two pages into a second address space failed")?;
    if pages != SECOND_SPACE_PAGES {
        return Err("sharing did not find the two mapped pages");
    }

    // Both sides are described, both are charged for exactly what was shared, and both point
    // at the same frame, read-only.
    if b().commit_used() != SHARED_BYTES {
        return Err("the second address space was not charged for the shared range");
    }
    let shared = a()
        .translate(PRIVATE_START)
        .ok_or("the parent lost its page")?;
    let other = b()
        .translate(PRIVATE_START)
        .ok_or("the child cannot see the shared page")?;
    if shared.entry.frame() != other.entry.frame() {
        return Err("sharing did not point both spaces at the same frame");
    }
    if shared.entry.is_writable() || other.entry.is_writable() {
        return Err("a shared page is still writable");
    }
    if mm::frame_refcount(shared.entry.frame() as usize) != 2 {
        return Err("a shared frame is not referenced exactly twice");
    }
    if !mm::is_copy_on_write(shared.entry.frame() as usize) {
        return Err("a shared frame is not marked copy-on-write");
    }
    if read_user(PRIVATE_START) != MARKER {
        return Err("the child cannot read the shared page");
    }
    a().deactivate();

    // Write in the child: the frame must be *copied*, the copy must carry the parent's bytes,
    // and the parent must not see the child's write.
    // The child stays active across the resolve, the read of the copy, and the write that
    // must not fault again: the fault handler and the user access both need the child's
    // tables to be the ones the processor is running on.
    b().activate();
    let outcome = space::resolve(PRIVATE_START, ERROR_WRITE);
    let (source, destination) = match outcome {
        Ok(Fault::CopyOnWrite {
            source,
            destination,
        }) => (source, destination),
        Ok(_) => return Err("a write to a shared page was resolved without copying"),
        Err(_) => return Err("a write to a shared page was refused"),
    };
    if source == destination {
        return Err("a write to a shared page reused the frame instead of copying it");
    }
    if read_user(PRIVATE_START) != MARKER {
        return Err("the copy-on-write copy does not contain the parent's data");
    }
    write_user(PRIVATE_START, SECOND_MARKER);
    if read_user(PRIVATE_START) != SECOND_MARKER {
        return Err("the copy-on-write copy did not become writable");
    }
    b().deactivate();
    report.copy_on_write += 1;

    // Both sides now have one owner each, and the parent's contents did not move.
    if mm::frame_refcount(source) != 1 || mm::frame_refcount(destination) != 1 {
        return Err("a page after copy-on-write does not have exactly one owner");
    }
    a().activate();
    if read_user(PRIVATE_START) != MARKER {
        return Err("the parent's page changed when the child wrote to its copy");
    }
    // The parent's entry is read-only but its frame is no longer shared: the next write makes
    // it writable again instead of copying, which is what an unshared page should cost.
    match space::resolve(PRIVATE_START, ERROR_WRITE) {
        Ok(Fault::CopyOnWrite {
            source: from,
            destination: to,
        }) if from == source && to == source => report.copy_on_write += 1,
        Ok(_) => return Err("the parent's page was copied although it had no other owner"),
        Err(_) => return Err("a write in the parent after the copy was refused"),
    }
    write_user(PRIVATE_START, MARKER);
    a().deactivate();

    if a().pages_mapped() != FIRST_SPACE_PAGES || b().pages_mapped() != SECOND_SPACE_PAGES {
        return Err("copy on write changed the number of mapped pages");
    }
    Ok(())
}

/// Tear both address spaces down and check what the counters say afterwards.
fn teardown(report: &mut SpaceReport) -> Result<(), &'static str> {
    let first_pages = a().pages_mapped();
    let second_pages = b().pages_mapped();
    report.pages_mapped = first_pages + second_pages;

    // SAFETY: the self test is the only owner of both cells, and both spaces are inactive --
    // `teardown` asserts that itself.
    let (first, second) = unsafe {
        (
            (*SPACE_A.get())
                .take()
                .ok_or("the first address space is missing")?,
            (*SPACE_B.get())
                .take()
                .ok_or("the second address space is missing")?,
        )
    };
    let torn = first
        .teardown()
        .map_err(|_| "the first address space did not tear down")?;
    if torn.pages_unmapped != first_pages {
        return Err("the first address space did not free every page it mapped");
    }
    if torn.vads_removed != report.vads {
        return Err("the first address space did not remove every descriptor");
    }
    // Every committed byte came back: the private range, the read-only range, the stack, and
    // the page stack growth charged for after the descriptors were described.
    let committed = (PRIVATE_END - PRIVATE_START)
        + (RO_END - RO_START)
        + (STACK_TOP - STACK_START)
        + GROWN_BYTES;
    if torn.commit_released != committed {
        return Err("the first address space released the wrong commit charge");
    }
    let torn = second
        .teardown()
        .map_err(|_| "the second address space did not tear down")?;
    if torn.pages_unmapped != second_pages {
        return Err("the second address space did not free every page it mapped");
    }
    if torn.vads_removed != 1 {
        return Err("the second address space removed the wrong number of descriptors");
    }
    mm::audit().map_err(|_| "the frame-map audit failed after teardown")?;
    report.frames.1 = mm::free_frames();
    let (allocated, released) = tables::totals();
    report.tables.2 = allocated;
    report.tables.3 = released;
    Ok(())
}

/// A fault the test expected to be resolved by demand zero.
fn expect_zero(address: u64) -> Result<Fault, &'static str> {
    match space::resolve(address, 0) {
        Ok(fault @ Fault::DemandZero { .. }) => Ok(fault),
        Ok(_) => Err("a committed page was resolved by something other than demand zero"),
        Err(_) => Err("a committed page was refused instead of being faulted in"),
    }
}

/// Write one byte to a user address in the active address space.
fn write_user(address: u64, value: u8) {
    // SAFETY: the address is inside a mapping this test made writable, in the address space
    // that is loaded; the write is one byte to a page this code owns.
    unsafe { core::ptr::write_volatile(address as *mut u8, value) };
}

/// Read one byte from a user address in the active address space.
fn read_user(address: u64) -> u8 {
    // SAFETY: as [`write_user`]: the address is inside a mapping the test made present.
    unsafe { core::ptr::read_volatile(address as *const u8) }
}

/// The first address space.
fn a() -> &'static mut Space {
    // SAFETY: single boot processor; the cell holds a live `Space` from `create_first` until
    // `teardown`, and every use of the returned reference ends before the next one begins.
    unsafe {
        (*SPACE_A.get())
            .as_mut()
            .expect("the first address space is missing")
    }
}

/// The second address space.
fn b() -> &'static mut Space {
    // SAFETY: as `a`.
    unsafe {
        (*SPACE_B.get())
            .as_mut()
            .expect("the second address space is missing")
    }
}
