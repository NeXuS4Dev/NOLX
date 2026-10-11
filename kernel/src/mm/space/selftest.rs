//! The address-space self test: page tables, faults, and the frame budget.
//!
//! Every claim [`super`] makes about an address space is exercised here at boot, on the machine
//! the kernel is actually running on, with the timer live. It is the same code path the
//! interrupt dispatcher will use for vector 14 -- [`super::resolve`] on the active space -- so a
//! regression that would become a bug check in a running system becomes a failed boot instead.
//!
//! The faults are driven by hand because there is no way to make ring 0 take a recoverable page
//! fault: a CPL 0 fault is not something the kernel can `iretq` out of. So the test asks the
//! resolver the question the hardware would ask, with the same addresses and the same error
//! codes, and checks the answers:
//!
//! * a first read of a committed page maps a zeroed frame (demand zero);
//! * a write to a page two address spaces share copies it, byte for byte (copy on write);
//! * a write to a page the VAD calls read-only is refused (protection);
//! * an access inside a mapped-file range is refused for now (the Cache Manager is M7);
//! * a hit on a guard page, and a stack fault that stays a stack, are told apart;
//! * a stack fault beyond the growth limit, or below the floor, is refused.
//!
//! It ends the way [`crate::mm::selftest`] ends: with the counters back where they started.
//! Two address spaces are created and torn down, every descriptor removed, every page unmapped,
//! and the frame allocator's free count and the page-table totals must be exactly what they were
//! before the first `create`. A kernel that cannot account for its own frames has no business
//! running a process.

use core::fmt;

use nolx_hal_core::console::HalConsole;
use nolx_mm_core::paging::Protection;
use nolx_mm_core::vad::{Backing, VadFlags, VadKind, VadRequest, USER_SPACE_LOW};

use crate::mm::space::{resolve, Fault, FaultError, Space, SpaceError, ERROR_WRITE, SPACE_NODES};
use crate::mm::tables;
use crate::mm::{self, phys};
use crate::sync::SyncCell;

/// A private anonymous mapping: 256 KiB, sixty-four pages, all of them demand-zero.
const PRIVATE: u64 = 0x10_0000;
const PRIVATE_END: u64 = 0x14_0000;
/// The first page of [`PRIVATE`]: the demand-zero and copy-on-write subject.
const TEST_PAGE: u64 = PRIVATE;
/// The second page: a second frame, allocated in the same pass.
const SECOND_PAGE: u64 = PRIVATE + 0x1000;
/// The third page: the `mprotect(PROT_READ)` subject.
const RO_PAGE: u64 = PRIVATE + 0x2000;
/// A guard page in the middle of the address space.
const GUARD_PAGE: u64 = 0x20_0000;
/// A file-backed range: no Cache Manager yet, so a fault inside it must be refused.
const FILE_PAGE: u64 = 0x21_0000;
const FILE_END: u64 = 0x22_0000;
/// The stack, growing down from 0x31_0000.
const STACK_TOP: u64 = 0x31_0000;
const STACK_START: u64 = 0x30_0000;
/// Below this the stack must not grow: the test's stand-in for a process's `RLIMIT_STACK`.
const STACK_FLOOR: u64 = 0x2F_C000;
/// An address no mapping and no stack can reach: four gigabytes above the private range.
const WILD: u64 = 0x1_0010_0000;
/// The byte the test writes into a page before sharing it, so the copy can be proven a copy.
const MARKER: u8 = 0xA5;

/// The address space the test drives.
static SPACE_A: SyncCell<Option<Space>> = SyncCell::new(None);
/// The address space it shares pages with.
static SPACE_B: SyncCell<Option<Space>> = SyncCell::new(None);

/// What the test did, for the boot log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceReport {
    /// Address spaces created and torn down.
    pub spaces: u64,
    /// Pages demand-paged in, across both spaces.
    pub pages: u64,
    /// Faults resolved, across both spaces.
    pub faults: u64,
    /// Accesses the resolver refused on purpose.
    pub refusals: u64,
    /// Pages copied by the copy-on-write path.
    pub copies: u64,
    /// Page tables the two address spaces allocated and freed.
    pub tables: u64,
}

impl SpaceReport {
    /// The system PTE pool's peak, in KiB, from the memory manager's own accounting.
    #[must_use]
    pub fn system_pte_peak(&self) -> u64 {
        let snapshot = crate::mm::pool::snapshot();
        let entry = snapshot
            .iter()
            .find(|pool| pool.kind == nolx_mm_core::commit::PoolKind::SystemPte);
        entry.map_or(0, |pool| pool.peak / 1024)
    }
}

/// Run the self test, reporting what it did.
///
/// # Errors
///
/// Returns the check that failed. The caller prints it and panics: an address space that
/// mismanages its frames is not a condition the kernel can continue from.
pub fn run(console: &mut dyn HalConsole) -> Result<SpaceReport, Failure> {
    let frames_before = mm::free_frames();
    let (tables_before, released_before) = tables::totals();
    let kernel_root = tables::current_root();
    let mut report = SpaceReport::default();

    // --- An address space starts empty in the user half, shared in the kernel half. -------
    {
        let space = Space::create().ok_or(Failure::new("could not create an address space"))?;
        if space.root() == kernel_root {
            return Err(Failure::new(
                "a new address space reused the kernel's page-table root",
            ));
        }
        if space.commit_used() != 0 || space.live_tables() != 0 {
            return Err(Failure::new("a new address space was not empty"));
        }
        if space.translate(PRIVATE).is_some() {
            return Err(Failure::new(
                "a new address space already mapped a user address",
            ));
        }
        // The direct map and the image window must be there before a line of the kernel runs
        // on this root, or activating it is a triple fault.
        let probe = phys::KERNEL_LOAD_ADDR;
        if space.translate(phys::DIRECT_MAP_BASE + probe).is_none() {
            return Err(Failure::new(
                "a new address space does not share the direct map",
            ));
        }
        if space.translate(phys::KERNEL_IMAGE_BASE + probe).is_none() {
            return Err(Failure::new(
                "a new address space does not share the kernel image",
            ));
        }
        // SAFETY: single boot processor, and the self test is the only reader or writer of
        // this cell for the whole of boot.
        unsafe {
            *SPACE_A.get() = Some(space);
        }
        report.spaces += 1;
    }

    // --- Describe the ranges; no frame is taken until something touches them. -------------
    a().set_stack_floor(STACK_FLOOR);
    a().reserve(VadRequest::private_anonymous(PRIVATE, PRIVATE_END))
        .map_err(|error| failure("could not describe the private range", error))?;
    a().reserve(VadRequest {
        start: GUARD_PAGE,
        end: GUARD_PAGE + 0x1000,
        kind: VadKind::Guard,
        protection: Protection::from_bits(0),
        maximum: Protection::from_bits(0),
        flags: VadFlags::empty(),
        backing: None,
    })
    .map_err(|error| failure("could not describe the guard page", error))?;
    a().reserve(VadRequest {
        start: FILE_PAGE,
        end: FILE_END,
        kind: VadKind::MappedFile,
        protection: Protection::from_bits(Protection::READ),
        maximum: Protection::from_bits(Protection::READ),
        flags: VadFlags::empty(),
        backing: Some(Backing {
            object: 1,
            offset: 0,
        }),
    })
    .map_err(|error| failure("could not describe the file-backed range", error))?;
    a().reserve(VadRequest {
        start: STACK_START,
        end: STACK_TOP,
        kind: VadKind::Stack,
        protection: Protection::read_write(),
        maximum: Protection::read_write(),
        flags: VadFlags::from_bits(VadFlags::GROWSDOWN),
        backing: None,
    })
    .map_err(|error| failure("could not describe the stack", error))?;

    let committed = (PRIVATE_END - PRIVATE) + (STACK_TOP - STACK_START);
    if a().commit_used() != committed {
        return Err(Failure::count(
            "the commit charge",
            committed,
            a().commit_used(),
        ));
    }
    a().check()
        .map_err(|detail| Failure::invariant("describing the ranges broke the VAD tree", detail))?;
    let described = live_descriptors();

    // --- Activate, and resolve faults the way the dispatcher will. ------------------------
    a().activate();

    // 1. The first touch of a committed page: a zeroed frame appears.
    let Fault::DemandZero { frame } =
        expect("demand zero of the first page", resolve(TEST_PAGE, 0))?
    else {
        return Err(Failure::new(
            "the first touch of a committed page was not demand-zero",
        ));
    };
    report.pages += 1;
    report.faults += 1;
    if a().translate(TEST_PAGE).is_none() {
        return Err(Failure::new(
            "demand zero returned without mapping the page",
        ));
    }
    if mm::frame_refcount(frame) != 1 {
        return Err(Failure::count(
            "the references to a demand-zero page",
            1,
            u64::from(mm::frame_refcount(frame)),
        ));
    }
    // The frame was just allocated to this address space; the direct map is the kernel's view
    // of it, and nothing else is looking at the page.
    let pointer = phys::direct_ptr::<u8>(mm::address_of(frame));
    // SAFETY: as above: one byte, inside a page this code owns.
    unsafe {
        if core::ptr::read(pointer) != 0 {
            return Err(Failure::new("a demand-zero page was not zero"));
        }
        core::ptr::write(pointer, MARKER);
    }

    // 2. A second page, so that copy on write has a neighbour that must not move.
    let Fault::DemandZero { frame: second } =
        expect("demand zero of the second page", resolve(SECOND_PAGE, 0))?
    else {
        return Err(Failure::new(
            "the second committed page was not demand-zero",
        ));
    };
    report.pages += 1;
    report.faults += 1;

    // 3. The accesses the address space must refuse.
    expect_error("a wild address", resolve(WILD, 0), FaultError::NotMapped)?;
    report.refusals += 1;
    expect_error(
        "a hit on the guard page",
        resolve(GUARD_PAGE, 0),
        FaultError::GuardPage,
    )?;
    report.refusals += 1;
    expect_error(
        "an access inside a file-backed range",
        resolve(FILE_PAGE, ERROR_WRITE),
        FaultError::Unsupported,
    )?;
    report.refusals += 1;
    expect_error(
        "an address below the user range",
        resolve(USER_SPACE_LOW - 1, 0),
        FaultError::KernelAddress,
    )?;
    report.refusals += 1;
    a().deactivate();

    // --- mprotect: the tree decides first, and the page tables follow. ---------------------
    a().protect(
        RO_PAGE,
        RO_PAGE + 0x1000,
        Protection::from_bits(Protection::READ),
    )
    .map_err(|error| failure("could not make one page read-only", error))?;
    if live_descriptors() <= described {
        return Err(Failure::new(
            "mprotect did not split the descriptor at the page boundary",
        ));
    }
    a().activate();
    if !matches!(
        expect("a read of the read-only page", resolve(RO_PAGE, 0))?,
        Fault::DemandZero { .. }
    ) {
        return Err(Failure::new(
            "a read-only page was not faulted in as a demand-zero page",
        ));
    }
    report.pages += 1;
    report.faults += 1;
    expect_error(
        "a write to a read-only page",
        resolve(RO_PAGE, ERROR_WRITE),
        FaultError::Protection,
    )?;
    report.refusals += 1;
    a().deactivate();
    a().protect(RO_PAGE, RO_PAGE + 0x1000, Protection::read_write())
        .map_err(|error| failure("could not make the page writable again", error))?;
    if live_descriptors() != described {
        return Err(Failure::new(
            "identical neighbours did not coalesce back into one descriptor",
        ));
    }

    // --- Stack growth: a fault below the stack is a stack, until it is not. ---------------
    a().activate();
    expect_error(
        "a fault far below the stack",
        resolve(STACK_START - 0x101_000, 0),
        FaultError::NotMapped,
    )?;
    report.refusals += 1;
    match expect("the first stack fault", resolve(STACK_START - 0x1000, 0))? {
        Fault::StackGrowth { from, to } => {
            if from != STACK_START || to != STACK_START - 0x1000 {
                return Err(Failure::value(
                    "where the stack grew from",
                    STACK_START,
                    from,
                ));
            }
        }
        _ => return Err(Failure::new("a fault below the stack did not grow it")),
    }
    report.pages += 1;
    report.faults += 1;
    match expect("the second stack fault", resolve(STACK_START - 0x3000, 0))? {
        Fault::StackGrowth { from, to } => {
            if from != STACK_START - 0x1000 || to != STACK_START - 0x3000 {
                return Err(Failure::value(
                    "where the second stack fault grew from",
                    STACK_START - 0x1000,
                    from,
                ));
            }
        }
        _ => {
            return Err(Failure::new(
                "the second stack fault did not grow the stack",
            ))
        }
    }
    report.pages += 1;
    report.faults += 1;
    expect_error(
        "a stack fault below the floor",
        resolve(STACK_START - 0x5000, 0),
        FaultError::StackLimit,
    )?;
    report.refusals += 1;
    // The grown pages coalesced back into the stack's own descriptor: a stack is one VAD, not
    // one per page.
    {
        let grown = *a()
            .vads()
            .find(STACK_START - 0x3000)
            .ok_or(Failure::new("the grown stack page has no descriptor"))?;
        if grown.kind != VadKind::Stack
            || grown.start != STACK_START - 0x3000
            || grown.end != STACK_TOP
        {
            return Err(Failure::new(
                "stack growth did not coalesce into the stack's descriptor",
            ));
        }
    }
    a().check()
        .map_err(|detail| Failure::invariant("stack growth broke the VAD tree", detail))?;
    a().deactivate();

    // --- Copy on write: share two pages with a second address space. ----------------------
    let space_b = Space::create().ok_or(Failure::new("could not create a second address space"))?;
    // SAFETY: as above; the second cell, written once by this boot-time test.
    unsafe {
        *SPACE_B.get() = Some(space_b);
    }
    report.spaces += 1;

    a().activate();
    let shared = a()
        .share_into(b(), PRIVATE, PRIVATE + 0x2000)
        .map_err(|error| failure("sharing two pages with the second space", error))?;
    a().deactivate();
    if shared != 2 {
        return Err(Failure::count("the pages shared", 2, shared));
    }
    if mm::frame_refcount(frame) != 2 {
        return Err(Failure::count(
            "the references to a shared page",
            2,
            u64::from(mm::frame_refcount(frame)),
        ));
    }
    if !mm::is_copy_on_write(frame) {
        return Err(Failure::new(
            "a shared read-only page is not marked copy-on-write",
        ));
    }
    if b().translate(TEST_PAGE).is_none() {
        return Err(Failure::new(
            "the second address space cannot see the shared page",
        ));
    }
    if b().commit_used() != 0x2000 {
        return Err(Failure::count(
            "the second address space's commit charge",
            0x2000,
            b().commit_used(),
        ));
    }

    // Write in B: the page must be copied, and the copy must contain what A wrote.
    b().activate();
    let copy = expect("a write to a shared page", resolve(TEST_PAGE, ERROR_WRITE))?;
    b().deactivate();
    let Fault::CopyOnWrite {
        source,
        destination,
    } = copy
    else {
        return Err(Failure::new("a write to a shared page did not copy it"));
    };
    if source == destination {
        return Err(Failure::new(
            "a shared page was made writable instead of being copied",
        ));
    }
    // The frame was just allocated to B, and the direct map is the kernel's view of it.
    // SAFETY: one byte, inside a page this code owns.
    let copied = unsafe { core::ptr::read(phys::direct_ptr::<u8>(mm::address_of(destination))) };
    if copied != MARKER {
        return Err(Failure::value(
            "the byte in the copy-on-write copy",
            u64::from(MARKER),
            u64::from(copied),
        ));
    }
    if mm::frame_refcount(source) != 1 || mm::frame_refcount(destination) != 1 {
        return Err(Failure::new(
            "the pages after a copy-on-write do not have one owner each",
        ));
    }
    report.copies += 1;
    report.faults += 1;

    // Write in A: its frame now has one owner, so the entry goes writable without a copy.
    a().activate();
    match expect(
        "a write in the last owner's space",
        resolve(TEST_PAGE, ERROR_WRITE),
    )? {
        Fault::CopyOnWrite {
            source,
            destination,
        } if source == frame && destination == frame => {}
        _ => {
            return Err(Failure::new(
                "the last owner of a shared page did not reuse its own frame",
            ))
        }
    }
    a().deactivate();
    report.faults += 1;
    if a().translate(TEST_PAGE).is_none() || b().translate(TEST_PAGE).is_none() {
        return Err(Failure::new(
            "an address space lost a page to copy on write",
        ));
    }
    // SAFETY: both frames belong to address spaces the test owns, reached through the direct
    // map; nothing else is looking at them.
    unsafe {
        if core::ptr::read(phys::direct_ptr::<u8>(mm::address_of(frame))) != MARKER {
            return Err(Failure::new(
                "the original page lost its contents to copy on write",
            ));
        }
        if core::ptr::read(phys::direct_ptr::<u8>(mm::address_of(second))) != 0 {
            return Err(Failure::new(
                "an untouched page lost its contents to copy on write",
            ));
        }
    }

    // --- Tear both spaces down, and check the budget. -------------------------------------
    let a_pages = a().pages_mapped();
    let b_pages = b().pages_mapped();
    let live_a = live_descriptors();
    let live_b = live_descriptors_of(b());
    report.pages = a().pages_mapped() + b().pages_mapped();
    report.faults = a().faults_resolved() + b().faults_resolved();

    let torn_a = take_a().teardown().map_err(teardown_failure)?;
    let torn_b = take_b().teardown().map_err(teardown_failure)?;
    if torn_a.pages_unmapped != a_pages {
        return Err(Failure::count(
            "the pages A's teardown unmapped",
            a_pages,
            torn_a.pages_unmapped,
        ));
    }
    if torn_b.pages_unmapped != b_pages {
        return Err(Failure::count(
            "the pages B's teardown unmapped",
            b_pages,
            torn_b.pages_unmapped,
        ));
    }
    if torn_a.vads_removed != live_a || torn_b.vads_removed != live_b {
        return Err(Failure::new("teardown did not remove every descriptor"));
    }
    report.tables = torn_a.tables_freed + torn_b.tables_freed;

    let frames_after = mm::free_frames();
    if frames_after != frames_before {
        return Err(Failure::value(
            "the free frames after the address spaces were torn down",
            frames_before,
            frames_after,
        ));
    }
    let (tables_after, released_after) = tables::totals();
    if tables_after - released_after != tables_before - released_before {
        return Err(Failure::value(
            "the live page tables after teardown",
            tables_before - released_before,
            tables_after - released_after,
        ));
    }
    let _ = released_before;

    // The system PTE pool's peak is the evidence that page-table allocation charged it: the
    // tables are all freed again by teardown, so the current charge is back to zero and only
    // the peak remembers that the pool was used at all.
    let peak = report.system_pte_peak();
    let _ = writeln!(
        console,
        "NOLX: address space selftest: {} space(s), {} page(s), {} fault(s), {} refusal(s), \
         {} page table(s), {} KiB peak in the system PTE pool, integrity ok",
        report.spaces, report.pages, report.faults, report.refusals, report.tables, peak
    );
    Ok(report)
}

/// The first address space.
fn a() -> &'static mut Space {
    // SAFETY: the cell holds a `Space` this test created and has not torn down; single boot
    // processor, and the borrow ends with the statement at every call site.
    unsafe { (*SPACE_A.get()).as_mut().expect("no address space A") }
}

/// The second address space.
fn b() -> &'static mut Space {
    // SAFETY: as `a`.
    unsafe { (*SPACE_B.get()).as_mut().expect("no address space B") }
}

/// Take the first address space out of its cell, for teardown.
fn take_a() -> Space {
    // SAFETY: as `a`; the cell is left empty, so nothing can alias the space afterwards.
    unsafe { (*SPACE_A.get()).take().expect("no address space A") }
}

/// Take the second address space out of its cell, for teardown.
fn take_b() -> Space {
    // SAFETY: as `b`.
    unsafe { (*SPACE_B.get()).take().expect("no address space B") }
}

/// Live (non-hole) descriptors in the active address space.
fn live_descriptors() -> u64 {
    live_descriptors_of(a())
}

/// Live (non-hole) descriptors in `space`.
fn live_descriptors_of(space: &Space) -> u64 {
    let mut live = 0;
    space.vads().visit(|vad| {
        if !vad.is_free() {
            live += 1;
        }
    });
    live
}

/// Unwrap a resolution the test expected to succeed.
fn expect(what: &'static str, outcome: Result<Fault, FaultError>) -> Result<Fault, Failure> {
    outcome.map_err(|refused| Failure::refused(what, refused))
}

/// Unwrap a resolution the test expected to refuse, with a particular reason.
fn expect_error(
    what: &'static str,
    outcome: Result<Fault, FaultError>,
    wanted: FaultError,
) -> Result<(), Failure> {
    match outcome {
        Err(refused) if refused == wanted => Ok(()),
        Err(refused) => Err(Failure::refused(what, refused)),
        Ok(_) => Err(Failure::new(
            "an access the address space does not permit was resolved anyway",
        )),
    }
}

/// A failure of a range operation.
fn failure(what: &'static str, error: SpaceError) -> Failure {
    Failure {
        what,
        detail: Detail::Space(error),
    }
}

/// A failure while tearing an address space down.
fn teardown_failure(error: SpaceError) -> Failure {
    Failure {
        what: "an address space did not tear down cleanly",
        detail: Detail::Space(error),
    }
}

/// Why the self test failed: which check, and what it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The check that failed, phrased so it completes "the address-space self test ...".
    pub what: &'static str,
    /// What the check saw.
    pub detail: Detail,
}

impl Failure {
    /// A check that failed with nothing more to say.
    fn new(what: &'static str) -> Self {
        Self {
            what,
            detail: Detail::None,
        }
    }

    /// A count that was not what it should have been.
    fn count(what: &'static str, expected: u64, actual: u64) -> Self {
        Self {
            what,
            detail: Detail::Count { expected, actual },
        }
    }

    /// A value that was not what it should have been.
    fn value(what: &'static str, expected: u64, actual: u64) -> Self {
        Self {
            what,
            detail: Detail::Count { expected, actual },
        }
    }

    /// A fault the resolver refused for the wrong reason.
    fn refused(what: &'static str, refused: FaultError) -> Self {
        Self {
            what,
            detail: Detail::Refused(refused),
        }
    }

    /// A VAD tree that broke one of its invariants.
    fn invariant(what: &'static str, detail: &'static str) -> Self {
        Self {
            what,
            detail: Detail::Invariant(detail),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.what, self.detail)
    }
}

/// What a failed check observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    /// Nothing beyond the check itself.
    None,
    /// The number that was wanted, and the number seen.
    Count {
        /// The number the check expected.
        expected: u64,
        /// The number it saw.
        actual: u64,
    },
    /// How the resolver refused an access.
    Refused(FaultError),
    /// How a range operation failed.
    Space(SpaceError),
    /// The VAD-tree invariant that broke.
    Invariant(&'static str),
}

impl fmt::Display for Detail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => Ok(()),
            Self::Count { expected, actual } => {
                write!(formatter, "expected {expected}, saw {actual}")
            }
            Self::Refused(error) => write!(formatter, "refused as {error:?}"),
            Self::Space(error) => write!(formatter, "{error:?}"),
            Self::Invariant(detail) => write!(formatter, "the VAD tree says: {detail}"),
        }
    }
}

/// The number of descriptors the address space type admits, for the log.
pub const NODES: usize = SPACE_NODES;
