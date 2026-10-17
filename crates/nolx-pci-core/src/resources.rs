//! Bounded boot-time resource planning for a numbered segment-zero PCI tree.
//!
//! Existing BARs and forwarding windows are reservations, not free space. Missing memory
//! BARs (including prefetchable/64-bit BARs) may use non-prefetchable 32-bit forwarding.
//! Platform apertures must be supplied by the caller; this module never invents RAM holes.
//! Planning restores sizing probes. Programming verifies readback and rolls back every
//! touched register on refusal, with decoding/bus mastering off during the transaction.

extern crate alloc;
use crate::{read_bars, read_bridge_bars, Bar, Bdf, ConfigSpace, Inventory};
use alloc::vec;
use alloc::vec::Vec;

/// Half-open physical memory or I/O port interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    /// First address, inclusive.
    pub start: u64,
    /// First address outside the interval.
    pub end: u64,
}
impl Range {
    /// Construct a nonempty, nonwrapping interval.
    pub fn new(start: u64, size: u64) -> Result<Self, Error> {
        let end = start.checked_add(size).ok_or(Error::InvalidRange)?;
        if size == 0 {
            return Err(Error::InvalidRange);
        }
        Ok(Self { start, end })
    }
    /// Whether two intervals share an address.
    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
    fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// A rejected plan or a configuration write that did not stick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Overflow, zero size, malformed mask or invalid alignment.
    InvalidRange,
    /// No aligned interval remains in the supplied aperture.
    NoSpace,
    /// Incomplete, ambiguous, cyclic or unnumbered topology.
    Topology,
    /// Existing endpoint BARs alias each other.
    Overlap,
    /// An assigned BAR is not forwarded through its ancestor bridges.
    Unrouted,
    /// Readback refused a write; rollback was verified.
    Readback,
    /// Rollback could not restore the configuration; the caller must quarantine or halt.
    Rollback,
}

/// A first-fit aperture with explicit reservations. Reservations outside it have no effect.
#[derive(Clone, Debug)]
pub struct Pool {
    free: Vec<Range>,
}
impl Pool {
    /// Start with one validated free interval.
    pub fn new(range: Range) -> Result<Self, Error> {
        if range.start >= range.end {
            return Err(Error::InvalidRange);
        }
        Ok(Self { free: vec![range] })
    }
    /// Remove an interval, including any overlap with earlier reservations.
    pub fn reserve(&mut self, reserved: Range) -> Result<(), Error> {
        if reserved.start >= reserved.end {
            return Err(Error::InvalidRange);
        }
        let mut next = Vec::new();
        for old in &self.free {
            if !old.overlaps(reserved) {
                next.push(*old);
                continue;
            }
            if old.start < reserved.start {
                next.push(Range {
                    start: old.start,
                    end: reserved.start,
                });
            }
            if reserved.end < old.end {
                next.push(Range {
                    start: reserved.end,
                    end: old.end,
                });
            }
        }
        self.free = next;
        Ok(())
    }
    /// Allocate a power-of-two-sized, equally aligned region without modifying state on failure.
    pub fn allocate(&mut self, size: u64) -> Result<Range, Error> {
        if !size.is_power_of_two() {
            return Err(Error::InvalidRange);
        }
        for range in &self.free {
            let start = range
                .start
                .checked_add(size - 1)
                .ok_or(Error::InvalidRange)?
                & !(size - 1);
            if let Ok(candidate) = Range::new(start, size) {
                if candidate.end <= range.end {
                    self.reserve(candidate)?;
                    return Ok(candidate);
                }
            }
        }
        Err(Error::NoSpace)
    }
}

/// One newly assigned BAR, retained for diagnostics and independent verification.
#[derive(Clone, Copy, Debug)]
pub struct Assignment {
    /// Controller identity in segment zero.
    pub bdf: Bdf,
    /// Lower BAR slot, never the upper half of a 64-bit pair.
    pub slot: usize,
    /// Address and size selected by the planner.
    pub range: Range,
    /// Whether this allocation is in I/O port space rather than memory space.
    pub io: bool,
}
#[derive(Clone, Copy)]
struct Write {
    bdf: Bdf,
    offset: u8,
    before: u32,
    after: u32,
    mask: u32,
}
#[derive(Clone, Copy)]
struct Command {
    bdf: Bdf,
    before: u32,
    after: u32,
}
struct Node {
    bus: u8,
    bridge: Option<Bdf>,
    parent: usize,
    memory: Option<Range>,
    prefetch: Option<Range>,
    io: Option<Range>,
    demand: [u64; 2],
}
struct Leaf {
    bdf: Bdf,
    slot: usize,
    node: usize,
    range: Option<Range>,
    size: u64,
    io: bool,
    prefetch: bool,
    wide: bool,
}

/// A completely validated proposal. No assignment is committed until [`Plan::apply`].
pub struct Plan {
    writes: Vec<Write>,
    commands: Vec<Command>,
    assignments: Vec<Assignment>,
    bridges: usize,
}
impl Plan {
    /// Newly assigned endpoint/bridge BARs.
    pub fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }
    /// Number of bridge memory/I/O windows created by the plan.
    pub fn windows(&self) -> usize {
        self.bridges
    }
    fn write<C: ConfigSpace>(&mut self, cfg: &mut C, bdf: Bdf, offset: u8, after: u32, mask: u32) {
        self.writes.push(Write {
            bdf,
            offset,
            before: cfg.read_u32(bdf, offset) & mask,
            after,
            mask,
        });
    }
    fn command<C: ConfigSpace>(&mut self, cfg: &mut C, bdf: Bdf, enable: u32) {
        if let Some(command) = self.commands.iter_mut().find(|c| c.bdf == bdf) {
            command.after |= enable;
        } else {
            let before = cfg.read_u32(bdf, 4) & 0xffff;
            self.commands.push(Command {
                bdf,
                before,
                after: before | enable,
            });
        }
    }
    /// Apply under exclusive boot-time access, before any driver owns DMA/register pointers.
    /// A failed rollback attempts to disable decode/bus mastering; the caller must halt or
    /// quarantine these functions, since a failed configuration transport cannot guarantee it.
    pub fn apply<C: ConfigSpace>(&self, cfg: &mut C) -> Result<(), Error> {
        for c in &self.commands {
            cfg.write_u32(c.bdf, 4, c.before & !7);
        }
        let disabled = self
            .commands
            .iter()
            .all(|c| cfg.read_u32(c.bdf, 4) & 7 == 0);
        let mut good = disabled;
        if disabled {
            for w in &self.writes {
                cfg.write_u32(w.bdf, w.offset, w.after);
            }
            good = self
                .writes
                .iter()
                .all(|w| cfg.read_u32(w.bdf, w.offset) & w.mask == w.after & w.mask);
        }
        if good {
            for c in &self.commands {
                cfg.write_u32(c.bdf, 4, c.after);
            }
            good = self
                .commands
                .iter()
                .all(|c| cfg.read_u32(c.bdf, 4) & 0xffff == c.after);
            if good {
                return Ok(());
            }
        }
        for c in &self.commands {
            cfg.write_u32(c.bdf, 4, c.before & !7);
        }
        // If decode cannot be disabled, do not rewrite addresses under an active device.
        if self
            .commands
            .iter()
            .any(|c| cfg.read_u32(c.bdf, 4) & 7 != 0)
        {
            return Err(Error::Rollback);
        }
        for w in self.writes.iter().rev() {
            cfg.write_u32(w.bdf, w.offset, w.before);
        }
        if self
            .writes
            .iter()
            .any(|w| cfg.read_u32(w.bdf, w.offset) & w.mask != w.before & w.mask)
        {
            return Err(Error::Rollback);
        }
        for c in &self.commands {
            cfg.write_u32(c.bdf, 4, c.before);
        }
        if self
            .commands
            .iter()
            .any(|c| cfg.read_u32(c.bdf, 4) & 0xffff != c.before)
        {
            for c in &self.commands {
                cfg.write_u32(c.bdf, 4, c.before & !7);
            }
            return Err(Error::Rollback);
        }
        Err(Error::Readback)
    }
}

fn memory_window(value: u32) -> Option<Range> {
    let start = u64::from(value & 0xfff0) << 16;
    let end = (u64::from((value >> 16) & 0xfff0) << 16) + 0x10_0000;
    (start < end).then_some(Range { start, end })
}
fn prefetch_window<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> Option<Range> {
    let value = cfg.read_u32(bdf, 0x24);
    let mut start = u64::from(value & 0xfff0) << 16;
    let mut last = (u64::from((value >> 16) & 0xfff0) << 16) | 0xf_ffff;
    if value & 15 == 1 {
        start |= u64::from(cfg.read_u32(bdf, 0x28)) << 32;
        last |= u64::from(cfg.read_u32(bdf, 0x2c)) << 32;
    }
    let range = Range {
        start,
        end: last.checked_add(1)?,
    };
    (range.start < range.end).then_some(range)
}
fn io_window<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> Option<Range> {
    let value = cfg.read_u32(bdf, 0x1c);
    let upper = cfg.read_u32(bdf, 0x30);
    let start = (u64::from(value & 0xf0) << 8)
        | if value & 15 == 1 {
            u64::from(upper & 0xffff) << 16
        } else {
            0
        };
    let end = ((u64::from((value >> 8) & 0xf0) << 8)
        | if (value >> 8) & 15 == 1 {
            u64::from(upper >> 16) << 16
        } else {
            0
        })
        + 0x1000;
    (start < end).then_some(Range { start, end })
}
fn rounded(size: u64, granularity: u64) -> Result<u64, Error> {
    size.max(granularity)
        .checked_next_power_of_two()
        .ok_or(Error::InvalidRange)
}

/// Plan missing BARs and closed type-1 memory/I/O windows, preserving assigned resources.
/// Bus numbers must already be valid. ROM BARs, CardBus and reassignment of existing BARs
/// are outside this API. Prefetchable new BARs use non-prefetchable forwarding below 4 GiB.
pub fn plan<C: ConfigSpace>(
    cfg: &mut C,
    inventory: &Inventory,
    memory: Pool,
    io: Pool,
) -> Result<Plan, Error> {
    if inventory.overflowed
        || inventory.refused_bridges != 0
        || inventory
            .as_slice()
            .iter()
            .any(|i| i.header_type > 1 || (i.header_type == 1 && (i.class != 6 || i.subclass != 4)))
    {
        return Err(Error::Topology);
    }
    let mut nodes = vec![Node {
        bus: 0,
        bridge: None,
        parent: 0,
        memory: None,
        prefetch: None,
        io: None,
        demand: [0; 2],
    }];
    for info in inventory.as_slice().iter().filter(|i| i.header_type == 1) {
        let io_types = cfg.read_u32(info.bdf, 0x1c);
        let pref_types = cfg.read_u32(info.bdf, 0x24);
        if io_types & 15 > 1
            || (io_types & 15) != ((io_types >> 8) & 15)
            || pref_types & 15 > 1
            || (pref_types & 15) != ((pref_types >> 16) & 15)
        {
            return Err(Error::InvalidRange);
        }
        let buses = cfg.read_u32(info.bdf, 0x18);
        let secondary = (buses >> 8) as u8;
        if secondary == 0 || nodes.iter().any(|n| n.bus == secondary) {
            return Err(Error::Topology);
        }
        nodes.push(Node {
            bus: secondary,
            bridge: Some(info.bdf),
            parent: 0,
            memory: memory_window(cfg.read_u32(info.bdf, 0x20)),
            prefetch: prefetch_window(cfg, info.bdf),
            io: io_window(cfg, info.bdf),
            demand: [0; 2],
        });
    }
    for i in 1..nodes.len() {
        let bus = nodes[i].bridge.unwrap().bus;
        nodes[i].parent = nodes
            .iter()
            .position(|n| n.bus == bus)
            .ok_or(Error::Topology)?;
        let mut current = i;
        for depth in 0..=nodes.len() {
            if current == 0 {
                break;
            }
            if depth == nodes.len() {
                return Err(Error::Topology);
            }
            current = nodes[current].parent;
        }
    }
    for (i, node) in nodes.iter().enumerate().skip(1) {
        if node
            .memory
            .is_some_and(|m| node.prefetch.is_some_and(|p| m.overlaps(p)))
        {
            return Err(Error::Overlap);
        }
        for other in nodes.iter().skip(i + 1).filter(|n| n.parent == node.parent) {
            for a in [node.memory, node.prefetch].into_iter().flatten() {
                if [other.memory, other.prefetch]
                    .into_iter()
                    .flatten()
                    .any(|b| a.overlaps(b))
                {
                    return Err(Error::Overlap);
                }
            }
            if node
                .io
                .is_some_and(|a| other.io.is_some_and(|b| a.overlaps(b)))
            {
                return Err(Error::Overlap);
            }
        }
    }
    let mut leaves: Vec<Leaf> = Vec::new();
    for info in inventory.as_slice().iter().filter(|i| i.header_type <= 1) {
        let bars = if info.header_type == 0 {
            read_bars(cfg, info.bdf)
        } else {
            read_bridge_bars(cfg, info.bdf)
        };
        let node = nodes
            .iter()
            .position(|n| n.bus == info.bdf.bus)
            .ok_or(Error::Topology)?;
        for (slot, bar) in bars.slots.iter().enumerate() {
            let (base, is_io, prefetch, wide) = match *bar {
                Bar::Io { port } => (u64::from(port), true, false, false),
                Bar::Memory32 { base, prefetchable } => {
                    (u64::from(base), false, prefetchable, false)
                }
                Bar::Memory64 { base, prefetchable } => (base, false, prefetchable, true),
                Bar::Unused => continue,
                Bar::Reserved => return Err(Error::InvalidRange),
            };
            let size = bars.sizes[slot].ok_or(Error::InvalidRange)?;
            if !size.is_power_of_two() || (base != 0 && base & (size - 1) != 0) {
                return Err(Error::InvalidRange);
            }
            let range = if base == 0 {
                None
            } else {
                Some(Range::new(base, size)?)
            };
            if let Some(range) = range {
                if leaves
                    .iter()
                    .any(|old| old.io == is_io && old.range.is_some_and(|r| r.overlaps(range)))
                {
                    return Err(Error::Overlap);
                }
            }
            leaves.push(Leaf {
                bdf: info.bdf,
                slot,
                node,
                range,
                size,
                io: is_io,
                prefetch,
                wide,
            });
        }
    }
    // Every assigned child resource must already be routable; never silently abandon it.
    for leaf in leaves.iter().filter(|l| l.range.is_some()) {
        let range = leaf.range.unwrap();
        let mut current = leaf.node;
        while current != 0 {
            let n = &nodes[current];
            let forwarded = if leaf.io {
                n.io.is_some_and(|r| r.contains(range))
            } else {
                n.memory.is_some_and(|r| r.contains(range))
                    || (leaf.prefetch && n.prefetch.is_some_and(|r| r.contains(range)))
            };
            if !forwarded {
                return Err(Error::Unrouted);
            }
            current = n.parent;
        }
    }
    // Depth ordering, independent of inventory order. Demands are powers of two, so sorting
    // allocations largest-first packs an aligned fresh bridge window without hidden padding.
    let mut order: Vec<usize> = (1..nodes.len()).collect();
    fn depth(nodes: &[Node], mut i: usize) -> usize {
        let mut d = 0;
        while i != 0 {
            d += 1;
            i = nodes[i].parent;
        }
        d
    }
    order.sort_by_key(|&i| core::cmp::Reverse(depth(&nodes, i)));
    for leaf in leaves.iter().filter(|l| l.range.is_none()) {
        let d = &mut nodes[leaf.node].demand[usize::from(leaf.io)];
        *d = d.checked_add(leaf.size).ok_or(Error::InvalidRange)?;
    }
    for &i in &order {
        for kind in 0..2 {
            let existing = if kind == 0 {
                nodes[i].memory
            } else {
                nodes[i].io
            };
            if existing.is_none() && nodes[i].demand[kind] > 0 {
                nodes[i].demand[kind] = rounded(
                    nodes[i].demand[kind],
                    if kind == 0 { 0x10_0000 } else { 0x1000 },
                )?;
                let parent = nodes[i].parent;
                nodes[parent].demand[kind] = nodes[parent].demand[kind]
                    .checked_add(nodes[i].demand[kind])
                    .ok_or(Error::InvalidRange)?;
            }
        }
    }
    let mut pools = vec![(Some(memory), Some(io))];
    for node in nodes.iter().skip(1) {
        pools.push((
            node.memory.map(Pool::new).transpose()?,
            node.io.map(Pool::new).transpose()?,
        ));
    }
    // Reserve descendants in their parent pool, and fixed BARs everywhere they intersect a
    // pool. Prefetch windows remain reserved even though new allocations don't use them.
    for n in nodes.iter().skip(1) {
        for range in [n.memory, n.prefetch].into_iter().flatten() {
            if let Some(pool) = &mut pools[n.parent].0 {
                pool.reserve(range)?;
            }
            if let Some(pool) = &mut pools[0].0 {
                pool.reserve(range)?;
            }
        }
        if let (Some(pool), Some(range)) = (&mut pools[n.parent].1, n.io) {
            pool.reserve(range)?;
        }
        if let (Some(pool), Some(range)) = (&mut pools[0].1, n.io) {
            pool.reserve(range)?;
        }
    }
    for leaf in leaves.iter().filter(|l| l.range.is_some()) {
        for (memory, io) in &mut pools {
            if let Some(pool) = if leaf.io { io } else { memory } {
                pool.reserve(leaf.range.unwrap())?;
            }
        }
    }
    let mut plan = Plan {
        writes: Vec::new(),
        commands: Vec::new(),
        assignments: Vec::new(),
        bridges: 0,
    };
    let mut parents: Vec<usize> = (0..nodes.len()).collect();
    parents.sort_by_key(|&i| depth(&nodes, i));
    for parent in parents {
        // An entry is either a missing child window or a missing BAR in this bus.
        let mut requests: Vec<(u64, bool, bool, usize)> = Vec::new();
        for (i, n) in nodes
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, n)| n.parent == parent)
        {
            for kind in 0..2 {
                if (if kind == 0 { n.memory } else { n.io }).is_none() && n.demand[kind] != 0 {
                    requests.push((n.demand[kind], kind == 1, true, i));
                }
            }
        }
        for (i, l) in leaves
            .iter()
            .enumerate()
            .filter(|(_, l)| l.node == parent && l.range.is_none())
        {
            requests.push((l.size, l.io, false, i));
        }
        requests.sort_by_key(|r| core::cmp::Reverse(r.0));
        for (size, is_io, window, index) in requests {
            let pool = if is_io {
                &mut pools[parent].1
            } else {
                &mut pools[parent].0
            };
            let range = pool.as_mut().ok_or(Error::NoSpace)?.allocate(size)?;
            if range.start == 0 {
                return Err(Error::InvalidRange);
            }
            if (!is_io && range.end > 0x1_0000_0000) || (is_io && range.end > 0x1_0000) {
                return Err(Error::InvalidRange);
            }
            if window {
                let bdf = nodes[index].bridge.unwrap();
                if is_io {
                    let types = cfg.read_u32(bdf, 0x1c) & 0x0f0f;
                    let value = types
                        | ((range.start as u32 >> 8) & 0xf0)
                        | (((range.end as u32 - 1) >> 8) & 0xf0) << 8;
                    // The upper half is Secondary Status (W1C), never copied into a write.
                    plan.write(cfg, bdf, 0x1c, value, 0x0000_ffff);
                    plan.write(cfg, bdf, 0x30, 0, u32::MAX);
                    pools[index].1 = Some(Pool::new(range)?);
                    nodes[index].io = Some(range);
                } else {
                    let value = ((range.start as u32 >> 16) & 0xfff0)
                        | ((range.end as u32 - 1) & 0xfff0_0000);
                    plan.write(cfg, bdf, 0x20, value, u32::MAX);
                    pools[index].0 = Some(Pool::new(range)?);
                    nodes[index].memory = Some(range);
                }
                plan.command(cfg, bdf, if is_io { 5 } else { 6 });
                plan.bridges += 1;
            } else {
                let leaf = &mut leaves[index];
                let offset = 0x10 + leaf.slot as u8 * 4;
                let flags = cfg.read_u32(leaf.bdf, offset) & if is_io { 3 } else { 15 };
                plan.write(cfg, leaf.bdf, offset, range.start as u32 | flags, u32::MAX);
                if leaf.wide {
                    plan.write(cfg, leaf.bdf, offset + 4, 0, u32::MAX);
                }
                plan.command(cfg, leaf.bdf, 0);
                let mut ancestor = leaf.node;
                while ancestor != 0 {
                    plan.command(
                        cfg,
                        nodes[ancestor].bridge.unwrap(),
                        if is_io { 5 } else { 6 },
                    );
                    ancestor = nodes[ancestor].parent;
                }
                plan.assignments.push(Assignment {
                    bdf: leaf.bdf,
                    slot: leaf.slot,
                    range,
                    io: is_io,
                });
                leaf.range = Some(range);
            }
        }
    }
    // Verify the completed proposal too: an existing child window may be outside a new
    // parent window. No new non-prefetchable BAR may silently use prefetch-only routing.
    for (i, leaf) in leaves.iter().enumerate() {
        let range = leaf.range.ok_or(Error::NoSpace)?;
        if leaves
            .iter()
            .skip(i + 1)
            .any(|other| other.io == leaf.io && other.range.is_some_and(|r| r.overlaps(range)))
        {
            return Err(Error::Overlap);
        }
        let mut ancestor = leaf.node;
        while ancestor != 0 {
            let n = &nodes[ancestor];
            let forwarded = if leaf.io {
                n.io.is_some_and(|r| r.contains(range))
            } else {
                n.memory.is_some_and(|r| r.contains(range))
                    || (leaf.prefetch && n.prefetch.is_some_and(|r| r.contains(range)))
            };
            if !forwarded {
                return Err(Error::Unrouted);
            }
            ancestor = n.parent;
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests;
