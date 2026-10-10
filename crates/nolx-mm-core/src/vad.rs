//! The virtual address descriptor tree.
//!
//! A process's address space is described by an **AVL tree of VADs** -- virtual address
//! descriptors ordered by start address -- rather than by a flat list of mappings. The
//! reason is that every POSIX memory operation is a range operation: `mmap` has to find a
//! hole, `munmap` has to remove a range that may start and end in the middle of two
//! mappings, `mprotect` has to split a mapping in two, and `mincore`/`msync` have to walk
//! exactly the mappings a range touches. On a tree, each of those is `O(log n)` plus the
//! nodes it actually changes, and -- more usefully -- the invariants they must maintain are
//! few enough to state and property-test.
//!
//! Holes are explicit: a range that is not mapped is a VAD of kind [`VadKind::Free`]. That
//! choice makes the tree a total description of the address space, so a first-fit hole
//! search and an in-order walk cannot disagree about what is mapped.
//!
//! The invariants, all checked by [`VadTree::check`] and property-tested against a
//! reference model:
//!
//! 1. VADs never overlap.
//! 2. Adjacent VADs with identical attributes are coalesced, so a range is split only
//!    while its attributes differ.
//! 3. The tree is height-balanced: height <= 2*log2(n+1).
//! 4. Every VAD's protection is a subset of its maximum protection.
//! 5. Committed pages never exceed what the caller's quota allows -- the tree reports the
//!    charge ([`VadTree::commit_total`]) and [`crate::commit::CommitCharge`] enforces it.

use crate::paging::{Protection, USER_TOP};
use crate::{page_align_up, PAGE_MASK};

/// What a range of virtual address space is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadKind {
    /// A hole: address space, but not memory.
    Free,
    /// Anonymous private memory: `brk`, `mmap(MAP_PRIVATE|MAP_ANONYMOUS)`, thread stacks.
    Private,
    /// A mapping of a section of a file.
    MappedFile,
    /// Shared anonymous memory.
    Shared,
    /// The main stack. Its guard page is a separate VAD of kind [`VadKind::Guard`].
    Stack,
    /// A guard page: reserved, never backed, never mapped. A fault here is a stack
    /// overflow (or `SIGSEGV`), which is the point of it.
    Guard,
    /// A mapped executable image: the program text, data, and the dynamic loader.
    Image,
}

impl VadKind {
    /// Whether a mapping of this kind counts against the commit charge.
    ///
    /// File-backed mappings do not: their pages can always be re-read from the file, which
    /// is exactly what `MAP_NORESERVE`-style accounting means.
    #[must_use]
    pub const fn is_committed(self) -> bool {
        matches!(self, Self::Private | Self::Shared | Self::Stack)
    }

    /// Whether the range is a hole.
    #[must_use]
    pub const fn is_free(self) -> bool {
        matches!(self, Self::Free)
    }
}

/// Attributes of a mapping that are not its protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VadFlags(u8);

impl VadFlags {
    /// The range is locked in memory (`mlock`): its pages may not be trimmed.
    pub const LOCKED: u8 = 1 << 0;
    /// The range is excluded from core dumps (`MAP_NOCORE`).
    pub const NOCORE: u8 = 1 << 1;
    /// The stack grows downward from `vad.end`, and a fault below `vad.start` extends it.
    pub const GROWSDOWN: u8 = 1 << 2;
    /// The range is a candidate for large pages where alignment allows.
    pub const HUGEPAGE: u8 = 1 << 3;
    /// The range must stay within the process's address-space limit but is not backed
    /// until touched (`MAP_NORESERVE`).
    pub const NO_RESERVE: u8 = 1 << 4;

    /// No flags.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// From raw bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        other.0 & !self.0 == 0
    }
}

/// Where a mapped-file VAD's contents come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backing {
    /// The section object's identifier.
    pub object: u64,
    /// The offset into that object.
    pub offset: u64,
}

/// One virtual address descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vad {
    /// First address of the range, inclusive and page-aligned.
    pub start: u64,
    /// Last address of the range, exclusive and page-aligned.
    pub end: u64,
    /// What the range is for.
    pub kind: VadKind,
    /// The protection the range has now.
    pub protection: Protection,
    /// The most permissive protection the range may ever have.
    pub maximum: Protection,
    /// Attributes other than protection.
    pub flags: VadFlags,
    /// The file the range maps, for file-backed mappings.
    pub backing: Option<Backing>,
}

impl Vad {
    /// The length of the range in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Whether the range is empty. Every VAD the tree accepts is non-empty, so this is
    /// `false` for every descriptor that exists; it is here because a length without the
    /// question next to it is a question the next reader will ask anyway.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// Whether the range is a hole.
    #[must_use]
    pub const fn is_free(&self) -> bool {
        self.kind.is_free()
    }

    /// Whether the address is inside the range.
    #[must_use]
    pub const fn contains(&self, address: u64) -> bool {
        address >= self.start && address < self.end
    }

    /// How much of this range counts against the commit charge.
    #[must_use]
    pub const fn commit_charge(&self) -> u64 {
        if self.kind.is_committed() {
            self.len()
        } else {
            0
        }
    }

    /// Whether two adjacent VADs describe the same thing and must be one VAD.
    #[must_use]
    fn merges_with(&self, other: &Self) -> bool {
        if self.end != other.start
            || self.kind != other.kind
            || self.protection != other.protection
            || self.maximum != other.maximum
            || self.flags != other.flags
        {
            return false;
        }
        match (self.backing, other.backing) {
            (None, None) => true,
            (Some(a), Some(b)) => a.object == b.object && a.offset + self.len() == b.offset,
            _ => false,
        }
    }

    /// The VAD covering both ranges; the caller has already checked [`Vad::merges_with`].
    #[must_use]
    fn merged_with(&self, other: &Self) -> Self {
        Self {
            start: self.start,
            end: other.end,
            ..*self
        }
    }
}

/// A request to map a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VadRequest {
    /// First address, inclusive; must be page-aligned.
    pub start: u64,
    /// Last address, exclusive; must be page-aligned.
    pub end: u64,
    /// What the range is for.
    pub kind: VadKind,
    /// The protection to map it with.
    pub protection: Protection,
    /// The maximum protection it may ever carry.
    pub maximum: Protection,
    /// Attributes other than protection.
    pub flags: VadFlags,
    /// The file backing it, if any.
    pub backing: Option<Backing>,
}

impl VadRequest {
    /// A private anonymous read-write mapping: what `mmap(MAP_PRIVATE|MAP_ANONYMOUS)` and
    /// a growing `brk` produce.
    #[must_use]
    pub const fn private_anonymous(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            kind: VadKind::Private,
            protection: Protection::read_write(),
            maximum: Protection::read_write(),
            flags: VadFlags::empty(),
            backing: None,
        }
    }
}

/// Why a VAD operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadError {
    /// The tree's node arena is full. The caller must split or remove something first.
    OutOfNodes,
    /// An address was not page-aligned.
    NotAligned,
    /// The range was empty, inverted, or outside the address space.
    InvalidRange,
    /// The range overlaps a live mapping.
    Overlap,
    /// The range is not fully covered, or is a hole where a mapping was required.
    NotMapped,
    /// The requested protection is not a subset of the range's maximum protection.
    Protection,
}

/// What the tree has done, for the boot log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VadStats {
    /// VADs inserted.
    pub inserts: u64,
    /// VADs removed.
    pub removals: u64,
    /// Protection changes.
    pub protections: u64,
    /// Splits forced by a range boundary landing inside a VAD.
    pub splits: u64,
    /// Coalesces: adjacent identical VADs merged back into one.
    pub merges: u64,
    /// Operations that failed.
    pub failures: u64,
    /// Nodes in use.
    pub nodes_used: u64,
    /// Height of the tree.
    pub height: u64,
}

struct Node {
    vad: Vad,
    left: Option<usize>,
    right: Option<usize>,
    parent: Option<usize>,
    height: u32,
}

/// An AVL tree of virtual address descriptors.
///
/// `NODES` is the arena size: a fixed, compile-time number of descriptors, because the
/// structure that describes a process's address space must not itself need to allocate
/// memory out of that address space. A `fork` needs a handful of nodes; a program that maps
/// and unmaps hundreds of thousands of times still needs only the nodes its live mappings
/// occupy, because removed nodes go back on the free list.
pub struct VadTree<const NODES: usize> {
    nodes: [Option<Node>; NODES],
    root: Option<usize>,
    free: Option<usize>,
    used: usize,
    extent_start: u64,
    extent_end: u64,
    stats: VadStats,
}

impl<const NODES: usize> VadTree<NODES> {
    /// A tree describing one free range: the process's address space before anything is
    /// mapped.
    ///
    /// `start` should leave the null page unmapped, so that a dereferenced null pointer
    /// faults instead of finding real memory. The kernel passes `0x10000` for that reason.
    ///
    /// # Panics
    ///
    /// Panics when `NODES` is zero or the range is empty or unaligned: a misconfigured
    /// address space is a programming error, not a runtime condition.
    #[must_use]
    pub fn new(start: u64, end: u64) -> Self {
        assert!(NODES > 0, "a VAD tree needs at least one node");
        assert!(start < end && start & PAGE_MASK == 0 && end & PAGE_MASK == 0);
        let mut tree = Self {
            nodes: core::array::from_fn(|_| None),
            root: None,
            free: None,
            used: 0,
            extent_start: start,
            extent_end: end,
            stats: VadStats::default(),
        };
        // Nodes 1..NODES form the free list, chained through `left`.
        for index in 1..NODES {
            tree.nodes[index] = Some(Node {
                vad: Vad {
                    start: 0,
                    end: 0,
                    kind: VadKind::Free,
                    protection: Protection::from_bits(0),
                    maximum: Protection::from_bits(0),
                    flags: VadFlags::empty(),
                    backing: None,
                },
                left: if index + 1 < NODES {
                    Some(index + 1)
                } else {
                    None
                },
                right: None,
                parent: None,
                height: 0,
            });
        }
        tree.free = if NODES > 1 { Some(1) } else { None };
        // The initial free VAD.
        tree.nodes[0] = Some(Node {
            vad: Vad {
                start,
                end,
                kind: VadKind::Free,
                protection: Protection::from_bits(0),
                maximum: Protection::from_bits(0),
                flags: VadFlags::empty(),
                backing: None,
            },
            left: None,
            right: None,
            parent: None,
            height: 1,
        });
        tree.root = Some(0);
        tree.used = 1;
        tree
    }

    /// The address space this tree describes.
    #[must_use]
    pub const fn extent(&self) -> (u64, u64) {
        (self.extent_start, self.extent_end)
    }

    /// How many descriptors are in use.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.used
    }

    /// Whether the tree has no descriptors at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// What the tree has done.
    #[must_use]
    pub const fn stats(&self) -> VadStats {
        self.stats
    }

    /// The descriptor covering `address`, hole or mapping.
    #[must_use]
    pub fn find(&self, address: u64) -> Option<&Vad> {
        let mut index = self.root;
        while let Some(current) = index {
            let node = self.node(current);
            if address < node.vad.start {
                index = node.left;
            } else if address >= node.vad.end {
                index = node.right;
            } else {
                return Some(&node.vad);
            }
        }
        None
    }

    /// The first hole of at least `length` bytes at or above `low` and ending by `high`.
    ///
    /// First fit, which is what `mmap(NULL, ...)` means in practice: the lowest address
    /// that fits, so a program's mappings stay compact and the allocator's behaviour is
    /// predictable. `MAP_32BIT`-style hints and top-down allocation are later work.
    #[must_use]
    pub fn find_gap(&self, length: u64, low: u64, high: u64) -> Option<(u64, u64)> {
        if length == 0 {
            return None;
        }
        let mut best: Option<(u64, u64)> = None;
        self.visit(|vad| {
            if best.is_some() || !vad.is_free() {
                return;
            }
            let start = page_align_up(vad.start.max(low));
            let end = vad.end.min(high);
            if start < end && end - start >= length {
                best = Some((start, start + length));
            }
        });
        best
    }

    /// Call `visit` for every descriptor, in ascending address order.
    pub fn visit(&self, mut visit: impl FnMut(&Vad)) {
        self.in_order(self.root, &mut |node| visit(&node.vad));
    }

    /// Call `visit` for every descriptor that overlaps `[start, end)`, in address order.
    pub fn visit_range(&self, start: u64, end: u64, mut visit: impl FnMut(&Vad)) {
        self.in_order(self.root, &mut |node| {
            if node.vad.start < end && node.vad.end > start {
                visit(&node.vad);
            }
        });
    }

    /// The total commit charge of every committed mapping.
    #[must_use]
    pub fn commit_total(&self) -> u64 {
        let mut total = 0;
        self.visit(|vad| total += vad.commit_charge());
        total
    }

    /// Map a range, splitting the hole it lands in.
    ///
    /// Fails with [`VadError::Overlap`] when any part of the range is already mapped, which
    /// is the `MAP_FIXED`-less behaviour: `mmap` without `MAP_FIXED` finds a hole first and
    /// asks for exactly it.
    pub fn insert(&mut self, request: VadRequest) -> Result<(), VadError> {
        if let Err(error) = self.check_request(&request) {
            self.stats.failures += 1;
            return Err(error);
        }
        // Every node the range touches must be a hole.
        let mut conflict = false;
        self.visit_range(request.start, request.end, |vad| {
            if !vad.is_free() {
                conflict = true;
            }
        });
        if conflict {
            self.stats.failures += 1;
            return Err(VadError::Overlap);
        }
        if let Err(error) = self.carve(request.start, request.end, Some(request)) {
            self.stats.failures += 1;
            return Err(error);
        }
        self.stats.inserts += 1;
        self.coalesce_around(request.start);
        Ok(())
    }

    /// Unmap a range: every descriptor it touches becomes a hole.
    ///
    /// The range must lie inside the address space; holes inside it are fine, which is what
    /// `munmap` of a partially mapped range means.
    pub fn remove(&mut self, start: u64, end: u64) -> Result<(), VadError> {
        if let Err(error) = self.check_range(start, end) {
            self.stats.failures += 1;
            return Err(error);
        }
        // The carved range becomes an explicit hole. Holes are descriptors like any other,
        // so a hole that is not in the tree is address space the tree cannot describe -- and
        // the first thing that would go wrong is the next `find_gap`, which would hand out
        // address space that is still mapped.
        let hole = VadRequest {
            start,
            end,
            kind: VadKind::Free,
            protection: Protection::from_bits(0),
            maximum: Protection::from_bits(0),
            flags: VadFlags::empty(),
            backing: None,
        };
        if let Err(error) = self.carve(start, end, Some(hole)) {
            self.stats.failures += 1;
            return Err(error);
        }
        self.stats.removals += 1;
        self.coalesce_around(start);
        Ok(())
    }

    /// Change the protection of a mapped range.
    ///
    /// The whole range must be mapped (no holes), and the new protection must be a subset
    /// of every covered descriptor's maximum protection -- that is what
    /// `mprotect(PROT_EXEC)` failing on a range that was never allowed to be executable
    /// looks like from the inside.
    pub fn protect(
        &mut self,
        start: u64,
        end: u64,
        protection: Protection,
    ) -> Result<(), VadError> {
        if let Err(error) = self.check_range(start, end) {
            self.stats.failures += 1;
            return Err(error);
        }
        let mut hole = false;
        let mut denied = false;
        self.visit_range(start, end, |vad| {
            if vad.is_free() {
                hole = true;
            } else if !vad.maximum.contains(protection) {
                denied = true;
            }
        });
        if hole {
            self.stats.failures += 1;
            return Err(VadError::NotMapped);
        }
        if denied {
            self.stats.failures += 1;
            return Err(VadError::Protection);
        }

        // The range inherits everything but the protection from the descriptor at its
        // start, which is the one attribute a range operation cannot invent.
        let Some(first) = self.find(start) else {
            self.stats.failures += 1;
            return Err(VadError::NotMapped);
        };
        let request = VadRequest {
            start,
            end,
            kind: first.kind,
            protection,
            maximum: first.maximum,
            flags: first.flags,
            backing: first.backing,
        };
        if let Err(error) = self.carve(start, end, Some(request)) {
            self.stats.failures += 1;
            return Err(error);
        }
        self.stats.protections += 1;
        self.coalesce_around(start);
        Ok(())
    }

    /// Check every invariant this module claims.
    ///
    /// Asserted in debug builds by the callers and run explicitly by tests. A failure here
    /// means the tree would eventually answer a range query wrongly, which in a kernel
    /// means mapping memory over something that was already there.
    pub fn check(&self) -> Result<(), &'static str> {
        // The free list must be consistent and must not overlap the tree.
        let mut free_count = 0;
        let mut index = self.free;
        while let Some(current) = index {
            if current >= NODES {
                return Err("free-list index out of range");
            }
            free_count += 1;
            if free_count > NODES {
                return Err("free list is a cycle");
            }
            index = self.node(current).left;
        }
        if free_count + self.used != NODES {
            return Err("free list plus tree does not account for every node");
        }

        // The tree's structure, and the ordering that makes it a search tree.
        let mut count = 0;
        let mut previous: Option<Vad> = None;
        let mut ordered = Ok(());
        self.in_order(self.root, &mut |node| {
            count += 1;
            if count > NODES {
                ordered = Err("tree is a cycle");
                return;
            }
            if let Some(previous) = previous {
                if previous.end > node.vad.start {
                    ordered = Err("VADs overlap or are out of order");
                } else if previous.end == node.vad.start && previous.merges_with(&node.vad) {
                    ordered = Err("two adjacent VADs should have been coalesced");
                }
            }
            if !node.vad.maximum.contains(node.vad.protection) {
                ordered = Err("protection exceeds the maximum protection");
            }
            if node.vad.start & PAGE_MASK != 0 || node.vad.end & PAGE_MASK != 0 {
                ordered = Err("a VAD boundary is not page-aligned");
            }
            if node.vad.start < self.extent_start || node.vad.end > self.extent_end {
                ordered = Err("a VAD lies outside the address space");
            }
            previous = Some(node.vad);
        });
        ordered?;
        if count != self.used {
            return Err("the tree holds a different number of nodes than it reports");
        }

        // Parent links, heights, and the AVL balance condition.
        self.check_structure(self.root, None)?;
        Ok(())
    }

    /// The height of the tree, for diagnostics.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.root.map_or(0, |index| self.node(index).height)
    }

    // ---------------------------------------------------------------- internals

    fn node(&self, index: usize) -> &Node {
        self.nodes[index]
            .as_ref()
            .expect("a live index must hold a node")
    }

    fn node_mut(&mut self, index: usize) -> &mut Node {
        self.nodes[index]
            .as_mut()
            .expect("a live index must hold a node")
    }

    fn check_range(&self, start: u64, end: u64) -> Result<(), VadError> {
        if start & PAGE_MASK != 0 || end & PAGE_MASK != 0 {
            return Err(VadError::NotAligned);
        }
        if start >= end || start < self.extent_start || end > self.extent_end {
            return Err(VadError::InvalidRange);
        }
        Ok(())
    }

    fn check_request(&self, request: &VadRequest) -> Result<(), VadError> {
        self.check_range(request.start, request.end)?;
        if !request.maximum.contains(request.protection) {
            return Err(VadError::Protection);
        }
        if request.kind.is_free() {
            return Err(VadError::InvalidRange);
        }
        Ok(())
    }

    /// Replace whatever covers `[start, end)` with `replacement`.
    ///
    /// Descriptors fully inside the range are removed; descriptors that straddle a boundary
    /// are split, which is where node count is spent. The caller has already decided that
    /// the operation is legal; this function only reshapes the tree.
    fn carve(
        &mut self,
        start: u64,
        end: u64,
        replacement: Option<VadRequest>,
    ) -> Result<(), VadError> {
        // Plan first, modify second. A range that needs more nodes than the arena can give
        // must fail without having trimmed anything: an operation that fails must leave the
        // address space exactly as it found it, or a program that handles `ENOMEM` correctly
        // still ends up with a corrupted one.
        let plan = self.plan_carve(start, end);
        let needed = plan.right_remainders + usize::from(replacement.is_some());
        let available = (NODES - self.used) + plan.removals;
        if needed > available {
            return Err(VadError::OutOfNodes);
        }

        loop {
            let mut overlapping: Option<usize> = None;
            let mut index = self.root;
            while let Some(current) = index {
                let node = self.node(current);
                if node.vad.end <= start {
                    index = node.right;
                } else if node.vad.start >= end {
                    index = node.left;
                } else {
                    overlapping = Some(current);
                    break;
                }
            }

            let Some(current) = overlapping else {
                break;
            };
            let vad = self.node(current).vad;
            let fully_inside = vad.start >= start && vad.end <= end;

            if fully_inside {
                self.detach(current);
                self.release(current);
                continue;
            }

            // Straddles an edge: trim it and continue. The part outside the range keeps the
            // node, which keeps the tree's shape and the node count stable.
            if vad.start < start {
                self.node_mut(current).vad.end = start;
                if vad.end > end {
                    // The right remainder is a new descriptor.
                    self.insert_vad(Vad { start: end, ..vad })?;
                    self.stats.splits += 1;
                }
            } else {
                // The range starts inside the node, so the remainder is on the right.
                self.node_mut(current).vad.start = end;
                self.stats.splits += 1;
            }
            continue;
        }

        if let Some(request) = replacement {
            let vad = Vad {
                start: request.start,
                end: request.end,
                kind: request.kind,
                protection: request.protection,
                maximum: request.maximum,
                flags: request.flags,
                backing: request.backing,
            };
            self.insert_vad(vad)?;
        }
        Ok(())
    }

    /// Count what a carve of `[start, end)` will do before it does any of it.
    ///
    /// Returns how many descriptors it will remove outright and how many right-hand
    /// remainders it will have to create. Those two numbers decide whether the arena has
    /// room; the answer is the difference between failing cleanly and failing after having
    /// already edited the tree.
    fn plan_carve(&self, start: u64, end: u64) -> CarvePlan {
        let mut plan = CarvePlan::default();
        self.visit_range(start, end, |vad| {
            if vad.start >= start && vad.end <= end {
                plan.removals += 1;
            } else if vad.start < start && vad.end > end {
                plan.right_remainders += 1;
            }
        });
        plan
    }

    /// Merge the descriptors around `start`, as far as their attributes allow.
    fn coalesce_around(&mut self, start: u64) {
        let Some(mut current) = self.candidate_for(start) else {
            return;
        };

        // Merge with the successors while the attributes agree.
        while let Some(next) = self.successor(current) {
            if !self.node(current).vad.merges_with(&self.node(next).vad) {
                break;
            }
            let merged = self.node(current).vad.merged_with(&self.node(next).vad);
            self.node_mut(current).vad = merged;
            self.detach(next);
            self.release(next);
            self.stats.merges += 1;
        }

        // And with the predecessors.
        while let Some(previous) = self.predecessor(current) {
            if !self.node(previous).vad.merges_with(&self.node(current).vad) {
                break;
            }
            let merged = self.node(previous).vad.merged_with(&self.node(current).vad);
            self.node_mut(previous).vad = merged;
            self.detach(current);
            self.release(current);
            self.stats.merges += 1;
            current = previous;
        }
    }

    /// The descriptor that contains `address`, or the first one that starts after it.
    ///
    /// This is where coalescing starts after a carve. A node that merely *ends* at the
    /// address is not it: that node is the left neighbour, and the merge has to be decided
    /// between it and whatever now begins at the address.
    fn candidate_for(&self, address: u64) -> Option<usize> {
        let mut index = self.root;
        let mut best = None;
        while let Some(current) = index {
            let node = self.node(current);
            if address < node.vad.start {
                best = Some(current);
                index = node.left;
            } else if address >= node.vad.end {
                index = node.right;
            } else {
                return Some(current);
            }
        }
        best
    }

    fn in_order(&self, from: Option<usize>, visit: &mut impl FnMut(&Node)) {
        if let Some(index) = from {
            self.in_order(self.node(index).left, visit);
            visit(self.node(index));
            self.in_order(self.node(index).right, visit);
        }
    }

    fn predecessor(&self, index: usize) -> Option<usize> {
        let node = self.node(index);
        if let Some(left) = node.left {
            let mut current = left;
            while let Some(right) = self.node(current).right {
                current = right;
            }
            return Some(current);
        }
        let mut current = index;
        let mut parent = node.parent;
        while let Some(above) = parent {
            if self.node(above).right == Some(current) {
                return Some(above);
            }
            current = above;
            parent = self.node(above).parent;
        }
        None
    }

    fn successor(&self, index: usize) -> Option<usize> {
        let node = self.node(index);
        if let Some(right) = node.right {
            let mut current = right;
            while let Some(left) = self.node(current).left {
                current = left;
            }
            return Some(current);
        }
        let mut current = index;
        let mut parent = node.parent;
        while let Some(above) = parent {
            if self.node(above).left == Some(current) {
                return Some(above);
            }
            current = above;
            parent = self.node(above).parent;
        }
        None
    }

    fn alloc_node(&mut self, vad: Vad) -> Result<usize, VadError> {
        let Some(index) = self.free else {
            return Err(VadError::OutOfNodes);
        };
        self.free = self.node(index).left;
        self.nodes[index] = Some(Node {
            vad,
            left: None,
            right: None,
            parent: None,
            height: 1,
        });
        self.used += 1;
        Ok(index)
    }

    fn release(&mut self, index: usize) {
        let free = self.free;
        let node = self.node_mut(index);
        node.left = free;
        node.right = None;
        node.parent = None;
        node.height = 0;
        self.free = Some(index);
        self.used -= 1;
    }

    /// Insert a descriptor by address, balancing on the way back up.
    fn insert_vad(&mut self, vad: Vad) -> Result<(), VadError> {
        let index = self.alloc_node(vad)?;
        self.link(index);
        Ok(())
    }

    /// Put an allocated node into the tree at its key and rebalance.
    fn link(&mut self, index: usize) {
        let key = self.node(index).vad.start;
        let mut parent: Option<usize> = None;
        let mut current = self.root;
        while let Some(node) = current {
            parent = current;
            if key < self.node(node).vad.start {
                current = self.node(node).left;
            } else {
                current = self.node(node).right;
            }
        }
        self.node_mut(index).parent = parent;
        match parent {
            Some(parent) => {
                if key < self.node(parent).vad.start {
                    self.node_mut(parent).left = Some(index);
                } else {
                    self.node_mut(parent).right = Some(index);
                }
            }
            None => self.root = Some(index),
        }
        self.rebalance_from(Some(index));
    }

    /// Remove a node from the tree, keeping it allocated for the caller to release.
    fn detach(&mut self, index: usize) {
        let (left, right, parent) = {
            let node = self.node(index);
            (node.left, node.right, node.parent)
        };

        let mut rebalance_from = parent;
        match (left, right) {
            (None, None) => {
                self.replace_child(parent, index, None);
            }
            (Some(child), None) | (None, Some(child)) => {
                self.node_mut(child).parent = parent;
                self.replace_child(parent, index, Some(child));
            }
            (Some(left_child), Some(right_child)) => {
                // Two children: the in-order successor takes the node's place, which is the
                // only way to keep the search-tree order without rewriting keys.
                let successor = self.successor(index).expect("a right subtree exists");
                let successor_parent = self.node(successor).parent;
                if successor_parent != Some(index) {
                    let successor_right = self.node(successor).right;
                    let up = successor_parent.expect("a successor has a parent");
                    self.replace_child(Some(up), successor, successor_right);
                    if let Some(moved) = successor_right {
                        self.node_mut(moved).parent = Some(up);
                    }
                    self.node_mut(successor).right = Some(right_child);
                    self.node_mut(right_child).parent = Some(successor);
                    rebalance_from = Some(up);
                } else {
                    // The successor is the node's own right child, so it already holds the
                    // right subtree and only needs the left one.
                    rebalance_from = Some(successor);
                }
                self.replace_child(parent, index, Some(successor));
                self.node_mut(successor).parent = parent;
                self.node_mut(successor).left = Some(left_child);
                self.node_mut(left_child).parent = Some(successor);
            }
        }
        self.rebalance_from(rebalance_from);
    }

    fn replace_child(&mut self, parent: Option<usize>, old: usize, new: Option<usize>) {
        match parent {
            Some(parent) => {
                if self.node(parent).left == Some(old) {
                    self.node_mut(parent).left = new;
                } else if self.node(parent).right == Some(old) {
                    self.node_mut(parent).right = new;
                }
            }
            None => {
                if self.root == Some(old) {
                    self.root = new;
                }
            }
        }
    }

    fn update_height(&mut self, index: usize) {
        let left = self.node(index).left;
        let right = self.node(index).right;
        let left_height = left.map_or(0, |node| self.node(node).height);
        let right_height = right.map_or(0, |node| self.node(node).height);
        self.node_mut(index).height = 1 + left_height.max(right_height);
    }

    fn balance_factor(&self, index: usize) -> i32 {
        let left = self
            .node(index)
            .left
            .map_or(0, |node| self.node(node).height) as i32;
        let right = self
            .node(index)
            .right
            .map_or(0, |node| self.node(node).height) as i32;
        left - right
    }

    fn rotate_left(&mut self, index: usize) -> usize {
        let pivot = self
            .node(index)
            .right
            .expect("rotate_left needs a right child");
        let moved = self.node(pivot).left;
        let parent = self.node(index).parent;

        self.node_mut(index).right = moved;
        if let Some(moved) = moved {
            self.node_mut(moved).parent = Some(index);
        }
        self.node_mut(pivot).left = Some(index);
        self.node_mut(index).parent = Some(pivot);
        self.node_mut(pivot).parent = parent;
        self.replace_child(parent, index, Some(pivot));

        self.update_height(index);
        self.update_height(pivot);
        pivot
    }

    fn rotate_right(&mut self, index: usize) -> usize {
        let pivot = self
            .node(index)
            .left
            .expect("rotate_right needs a left child");
        let moved = self.node(pivot).right;
        let parent = self.node(index).parent;

        self.node_mut(index).left = moved;
        if let Some(moved) = moved {
            self.node_mut(moved).parent = Some(index);
        }
        self.node_mut(pivot).right = Some(index);
        self.node_mut(index).parent = Some(pivot);
        self.node_mut(pivot).parent = parent;
        self.replace_child(parent, index, Some(pivot));

        self.update_height(index);
        self.update_height(pivot);
        pivot
    }

    fn rebalance_from(&mut self, mut index: Option<usize>) {
        while let Some(current) = index {
            self.update_height(current);
            let factor = self.balance_factor(current);
            let top = if factor > 1 {
                let left = self
                    .node(current)
                    .left
                    .expect("height implies a left child");
                if self.balance_factor(left) < 0 {
                    self.rotate_left(left);
                }
                self.rotate_right(current)
            } else if factor < -1 {
                let right = self
                    .node(current)
                    .right
                    .expect("height implies a right child");
                if self.balance_factor(right) > 0 {
                    self.rotate_right(right);
                }
                self.rotate_left(current)
            } else {
                current
            };
            index = self.node(top).parent;
        }
    }

    fn check_structure(
        &self,
        index: Option<usize>,
        parent: Option<usize>,
    ) -> Result<u32, &'static str> {
        let Some(current) = index else {
            return Ok(0);
        };
        let node = self.node(current);
        if node.parent != parent {
            return Err("a parent link is wrong");
        }
        let left = self.check_structure(node.left, Some(current))?;
        let right = self.check_structure(node.right, Some(current))?;
        if left.abs_diff(right) > 1 {
            return Err("the tree is not height-balanced");
        }
        let height = 1 + left.max(right);
        if node.height != height {
            return Err("a node's recorded height is wrong");
        }
        if let Some(left) = node.left {
            if self.node(left).vad.start >= node.vad.start {
                return Err("the search-tree order is broken");
            }
        }
        if let Some(right) = node.right {
            if self.node(right).vad.start <= node.vad.start {
                return Err("the search-tree order is broken");
            }
        }
        Ok(height)
    }
}

/// What a carve will cost, decided before it starts.
#[derive(Clone, Copy, Debug, Default)]
struct CarvePlan {
    /// Descriptors the range covers completely, which will be removed.
    removals: usize,
    /// Right-hand remainders that will have to be created.
    right_remainders: usize,
}

/// The user address space: the null page is left unmapped on purpose.
pub const USER_SPACE_LOW: u64 = 0x1_0000;

/// The top of the user address space.
pub const USER_SPACE_HIGH: u64 = USER_TOP + 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    fn tree<const N: usize>() -> VadTree<N> {
        VadTree::new(USER_SPACE_LOW, USER_SPACE_HIGH)
    }

    /// A reference model: the same semantics implemented over a sorted `Vec`.
    ///
    /// This is the point of the property test. Two implementations that share no code
    /// agreeing after every one of thousands of random operations is worth more than any
    /// number of hand-written cases, and it catches exactly the class of bug an AVL
    /// implementation has: a rotation that loses a parent link shows up as a divergence
    /// instead of as a wrong answer three operations later.
    #[derive(Default, Clone)]
    struct Model {
        segments: Vec<Vad>,
    }

    impl Model {
        fn new(start: u64, end: u64) -> Self {
            Self {
                segments: vec![Vad {
                    start,
                    end,
                    kind: VadKind::Free,
                    protection: Protection::from_bits(0),
                    maximum: Protection::from_bits(0),
                    flags: VadFlags::empty(),
                    backing: None,
                }],
            }
        }

        fn find(&self, address: u64) -> Option<&Vad> {
            self.segments.iter().find(|vad| vad.contains(address))
        }

        fn carve(&mut self, start: u64, end: u64, replacement: Option<Vad>) {
            let mut next = Vec::new();
            for vad in &self.segments {
                if vad.end <= start || vad.start >= end {
                    next.push(*vad);
                    continue;
                }
                if vad.start < start {
                    next.push(Vad { end: start, ..*vad });
                }
                if vad.end > end {
                    next.push(Vad { start: end, ..*vad });
                }
            }
            if let Some(vad) = replacement {
                next.push(vad);
            }
            next.sort_by_key(|vad| vad.start);
            self.segments = next;
            self.coalesce();
        }

        fn coalesce(&mut self) {
            let mut merged: Vec<Vad> = Vec::new();
            for vad in self.segments.drain(..) {
                match merged.last_mut() {
                    Some(last) if last.merges_with(&vad) => *last = last.merged_with(&vad),
                    _ => merged.push(vad),
                }
            }
            self.segments = merged;
        }
    }

    fn insert_via_model(model: &mut Model, request: VadRequest) -> Result<(), VadError> {
        let mut start = request.start;
        while start < request.end {
            let vad = model.find(start).expect("the model covers its extent");
            if !vad.is_free() {
                return Err(VadError::Overlap);
            }
            start = vad.end;
        }
        model.carve(
            request.start,
            request.end,
            Some(Vad {
                start: request.start,
                end: request.end,
                kind: request.kind,
                protection: request.protection,
                maximum: request.maximum,
                flags: request.flags,
                backing: request.backing,
            }),
        );
        Ok(())
    }

    #[test]
    fn a_new_tree_is_one_hole_and_nothing_else() {
        let tree: VadTree<16> = tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.height(), 1);
        assert_eq!(tree.check(), Ok(()));
        let vad = tree.find(0x9_0000).expect("the whole space is one VAD");
        assert!(vad.is_free());
        assert_eq!(vad.start, USER_SPACE_LOW);
        assert_eq!(vad.end, USER_SPACE_HIGH);
        assert_eq!(tree.commit_total(), 0);
    }

    #[test]
    fn mapping_inside_a_hole_splits_it_and_the_hole_parts_stay_holes() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.check(), Ok(()));

        assert!(tree.find(0x9_0000).expect("a hole").is_free());
        assert!(!tree.find(0x10_5000).expect("the mapping").is_free());
        assert!(tree.find(0x20_0000).expect("a hole").is_free());
        assert_eq!(tree.commit_total(), 0x1_0000);
        assert_eq!(tree.find(0x10_0000).expect("mapped").kind, VadKind::Private);
    }

    #[test]
    fn mapping_over_a_live_mapping_is_refused() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0x10_8000, 0x10_9000)),
            Err(VadError::Overlap)
        );
        // And a range that only partly overlaps is refused too.
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0x10_F000, 0x11_1000)),
            Err(VadError::Overlap)
        );
        assert_eq!(tree.check(), Ok(()));
    }

    #[test]
    fn unaligned_and_empty_ranges_are_refused() {
        let mut tree: VadTree<16> = tree();
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0x10_0001, 0x11_0000)),
            Err(VadError::NotAligned)
        );
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0x11_0000, 0x11_0000)),
            Err(VadError::InvalidRange)
        );
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0, 0x1000)),
            Err(VadError::InvalidRange),
            "below the address space's low end"
        );
        assert_eq!(
            tree.remove(0x10_0000, USER_SPACE_HIGH + 0x1000),
            Err(VadError::InvalidRange)
        );
    }

    #[test]
    fn adjacent_identical_mappings_coalesce_into_one() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        tree.insert(VadRequest::private_anonymous(0x11_0000, 0x12_0000))
            .expect("mapped");
        assert_eq!(
            tree.len(),
            3,
            "the two mappings are one VAD between two holes"
        );
        let vad = tree.find(0x10_0000).expect("mapped");
        assert_eq!(vad.start, 0x10_0000);
        assert_eq!(vad.end, 0x12_0000);
        assert_eq!(tree.stats().merges, 1);
        assert_eq!(tree.check(), Ok(()));
    }

    #[test]
    fn different_protections_keep_adjacent_mappings_apart() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        let mut read_only = VadRequest::private_anonymous(0x11_0000, 0x12_0000);
        read_only.protection = Protection::from_bits(Protection::READ);
        read_only.maximum = Protection::read_write();
        tree.insert(read_only).expect("mapped");
        assert_eq!(tree.len(), 4, "the two mappings stay separate");
        assert!(!tree.find(0x11_0000).expect("mapped").protection.can_write());
    }

    #[test]
    fn unmapping_creates_a_hole_that_coalesces_with_its_neighbours() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x14_0000))
            .expect("mapped");
        tree.remove(0x11_0000, 0x12_0000).expect("unmapped");
        assert_eq!(
            tree.len(),
            5,
            "the mapping became two mappings with a hole between them"
        );
        assert!(tree.find(0x11_0000).expect("the hole").is_free());

        // Removing the rest of the mapping merges three holes into one hole.
        tree.remove(0x10_0000, 0x11_0000).expect("unmapped");
        tree.remove(0x12_0000, 0x14_0000).expect("unmapped");
        assert_eq!(tree.len(), 1, "everything is one hole again");
        assert!(tree.find(0x10_0000).expect("hole").is_free());
        assert_eq!(tree.commit_total(), 0);
        assert_eq!(tree.check(), Ok(()));
    }

    #[test]
    fn protecting_a_range_splits_the_mapping_it_lands_in() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x14_0000))
            .expect("mapped");
        tree.protect(
            0x11_0000,
            0x12_0000,
            Protection::from_bits(Protection::READ),
        )
        .expect("protected");
        assert_eq!(tree.len(), 5, "one mapping became three");
        assert!(tree.find(0x10_0000).expect("mapped").protection.can_write());
        assert!(!tree.find(0x11_0000).expect("mapped").protection.can_write());
        assert!(tree.find(0x12_0000).expect("mapped").protection.can_write());
        assert_eq!(tree.stats().splits, 2);
        assert_eq!(tree.check(), Ok(()));

        // Putting it back merges the three into one again.
        tree.protect(0x11_0000, 0x12_0000, Protection::read_write())
            .expect("protected");
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.find(0x10_0000).expect("mapped").end, 0x14_0000);
        assert_eq!(tree.check(), Ok(()));
    }

    #[test]
    fn protecting_beyond_the_maximum_or_a_hole_is_refused() {
        let mut tree: VadTree<16> = tree();
        let mut request = VadRequest::private_anonymous(0x10_0000, 0x12_0000);
        request.protection = Protection::from_bits(Protection::READ);
        request.maximum = Protection::from_bits(Protection::READ);
        tree.insert(request).expect("mapped");

        assert_eq!(
            tree.protect(0x10_0000, 0x11_0000, Protection::read_write()),
            Err(VadError::Protection)
        );
        assert_eq!(
            tree.protect(0x9_0000, 0x10_0000, Protection::from_bits(Protection::READ)),
            Err(VadError::NotMapped),
            "the range starts in a hole"
        );
        assert_eq!(tree.check(), Ok(()));
    }

    #[test]
    fn a_gap_search_finds_the_lowest_hole_that_fits() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x20_0000, 0x30_0000))
            .expect("mapped");
        // The lowest hole is the one at the bottom of the address space; it is 0x1F0000
        // bytes long, so a 0x100000-byte request fits in it without reaching the mapping.
        assert_eq!(
            tree.find_gap(0x10_0000, 0, USER_SPACE_HIGH),
            Some((0x1_0000, 0x11_0000))
        );
        assert_eq!(
            tree.find_gap(0x10_0000, 0x20_0000, USER_SPACE_HIGH),
            Some((0x30_0000, 0x40_0000))
        );
        assert_eq!(
            tree.find_gap(0x1000, 0x21_0000, USER_SPACE_HIGH),
            Some((0x30_0000, 0x30_1000))
        );
        assert_eq!(
            tree.find_gap(0x10_0000, 0x10_0000, 0x20_0000),
            Some((0x10_0000, 0x20_0000)),
            "the hole below the mapping ends exactly at `high`, so it fits"
        );
        assert_eq!(
            tree.find_gap(0x10_0000, 0x10_1000, 0x20_0000),
            None,
            "starting inside the hole leaves less than the request"
        );
        assert_eq!(tree.find_gap(0, 0, USER_SPACE_HIGH), None);
    }

    #[test]
    fn visiting_a_range_reports_only_what_it_touches() {
        let mut tree: VadTree<16> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        tree.insert(VadRequest::private_anonymous(0x12_0000, 0x13_0000))
            .expect("mapped");

        let mut visited = Vec::new();
        tree.visit_range(0x10_8000, 0x12_8000, |vad| {
            visited.push((vad.start, vad.end))
        });
        assert_eq!(
            visited.len(),
            3,
            "the mapping, the middle hole, and the second mapping"
        );
        assert_eq!(visited[0], (0x10_0000, 0x11_0000));
        assert_eq!(visited[1].0, 0x11_0000);
        assert_eq!(visited[2], (0x12_0000, 0x13_0000));
    }

    #[test]
    fn the_node_arena_is_a_hard_limit_that_is_reported() {
        // Three nodes: a hole, a mapping, and one split's worth of room.
        let mut tree: VadTree<3> = tree();
        tree.insert(VadRequest::private_anonymous(0x10_0000, 0x11_0000))
            .expect("mapped");
        assert_eq!(
            tree.insert(VadRequest::private_anonymous(0x20_0000, 0x21_0000)),
            Err(VadError::OutOfNodes)
        );
        assert_eq!(tree.check(), Ok(()));
        // The failed insert left the tree exactly as it was.
        assert!(tree.find(0x20_0000).expect("hole").is_free());
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn many_mappings_keep_the_tree_balanced() {
        let mut tree: VadTree<512> = tree();
        for index in 0..100u64 {
            let start = 0x10_0000 + index * 0x20_0000;
            tree.insert(VadRequest::private_anonymous(start, start + 0x1000))
                .expect("mapped");
            assert_eq!(tree.check(), Ok(()));
        }
        assert_eq!(tree.len(), 201);
        let nodes = tree.len() as f64;
        let bound = 2.0 * (nodes + 1.0).log2();
        assert!(
            f64::from(tree.height()) <= bound,
            "height {} exceeds the AVL bound {bound}",
            tree.height()
        );
        assert_eq!(tree.commit_total(), 100 * 0x1000);
    }

    #[test]
    fn random_operations_agree_with_the_reference_model() {
        let mut rng = Rng::new(0x5eed_1234);
        let mut tree: VadTree<256> = tree();
        let mut model = Model::new(USER_SPACE_LOW, USER_SPACE_HIGH);
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut log: Vec<String> = Vec::new();

        for step in 0..3000 {
            let roll = rng.below(100);
            if roll < 45 {
                // Map a range somewhere in a small window, so collisions happen often.
                let start = 0x10_0000 + rng.below(24) * 0x1_0000;
                let length = (1 + rng.below(4)) * 0x1_0000;
                let request = VadRequest::private_anonymous(start, start + length);
                let expected = insert_via_model(&mut model, request);
                let actual = tree.insert(request);
                log.push(format!(
                    "insert {start:#x}..{:#x} -> {actual:?}",
                    start + length
                ));
                assert_eq!(actual, expected, "insert disagreed at step {step}");
                if actual.is_ok() {
                    live.push((start, start + length));
                }
            } else if roll < 70 {
                let start = 0x10_0000 + rng.below(24) * 0x1_0000;
                let length = (1 + rng.below(4)) * 0x1_0000;
                let expected = model_remove_range(&mut model, start, start + length);
                let actual = tree.remove(start, start + length);
                log.push(format!(
                    "remove {start:#x}..{:#x} -> {actual:?}",
                    start + length
                ));
                assert_eq!(actual, expected, "remove disagreed at step {step}");
            } else if roll < 90 {
                let start = 0x10_0000 + rng.below(24) * 0x1_0000;
                let length = (1 + rng.below(4)) * 0x1_0000;
                let protection = if rng.chance(50) {
                    Protection::from_bits(Protection::READ)
                } else {
                    Protection::read_write()
                };
                let expected = model_protect_range(&mut model, start, start + length, protection);
                let actual = tree.protect(start, start + length, protection);
                log.push(format!(
                    "protect {start:#x}..{:#x} {:#x} -> {actual:?}",
                    start + length,
                    protection.bits()
                ));
                assert_eq!(actual, expected, "protect disagreed at step {step}");
            }

            // The model and the tree must agree about sampled addresses in the window.
            for _ in 0..6 {
                let address = 0x10_0000 + rng.below(48) * 0x8000;
                let from_tree = tree
                    .find(address)
                    .map(|vad| (vad.start, vad.end, vad.kind, vad.protection));
                let from_model = model
                    .find(address)
                    .map(|vad| (vad.start, vad.end, vad.kind, vad.protection));
                assert_eq!(
                    from_tree, from_model,
                    "divergence at step {step}, address {address:#x}"
                );
            }
            if let Err(message) = tree.check() {
                let mut listing = String::new();
                tree.visit(|vad| {
                    listing.push_str(&format!(
                        "[{:#x},{:#x}) {:?} {:#x} ",
                        vad.start,
                        vad.end,
                        vad.kind,
                        vad.protection.bits()
                    ));
                });
                let mut history = String::new();
                for line in log.iter().rev().take(8).rev() {
                    history.push_str(line);
                    history.push('\n');
                }
                panic!("step {step}: {message}\n{listing}\n-- last operations --\n{history}");
            }
        }
        let _ = live;
    }

    /// Removing a range: legal inside the extent, refused outside it.
    fn model_remove_range(model: &mut Model, start: u64, end: u64) -> Result<(), VadError> {
        if start >= end || start < USER_SPACE_LOW || end > USER_SPACE_HIGH {
            return Err(VadError::InvalidRange);
        }
        model.carve(
            start,
            end,
            Some(Vad {
                start,
                end,
                kind: VadKind::Free,
                protection: Protection::from_bits(0),
                maximum: Protection::from_bits(0),
                flags: VadFlags::empty(),
                backing: None,
            }),
        );
        Ok(())
    }

    /// Protecting a range: everything under it must be mapped and may allow it.
    fn model_protect_range(
        model: &mut Model,
        start: u64,
        end: u64,
        protection: Protection,
    ) -> Result<(), VadError> {
        if start >= end || start < USER_SPACE_LOW || end > USER_SPACE_HIGH {
            return Err(VadError::InvalidRange);
        }
        let first = *model.find(start).expect("the model covers its extent");
        let mut cursor = start;
        while cursor < end {
            let vad = model.find(cursor).expect("the model covers its extent");
            if vad.is_free() {
                return Err(VadError::NotMapped);
            }
            if !vad.maximum.contains(protection) {
                return Err(VadError::Protection);
            }
            cursor = vad.end;
        }
        model.carve(
            start,
            end,
            Some(Vad {
                start,
                end,
                kind: first.kind,
                protection,
                maximum: first.maximum,
                flags: first.flags,
                backing: first.backing,
            }),
        );
        Ok(())
    }
}
