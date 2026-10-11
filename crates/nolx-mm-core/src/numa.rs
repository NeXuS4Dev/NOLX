//! NUMA nodes, and the zonelists that order them.
//!
//! A machine with more than one node has memory that is not equally close to every processor,
//! and an allocator that ignores that hands a processor memory across an interconnect it did
//! not have to cross. The fix is not a better allocator: it is a *zonelist* -- the list of
//! nodes an allocation may be served from, in preference order -- computed once and walked in
//! order.
//!
//! This module is the bookkeeping and the ordering rule, with no allocator in it:
//!
//! * [`NodeList`] describes the machine: each node's identifier and the physical frames it
//!   owns, plus the ACPI distance matrix between nodes.
//! * [`NodeList::zonelist`] builds the preference order for a node: itself, then the others by
//!   ascending distance, with ties broken by identifier so the order is deterministic.
//! * [`Node::contains`] answers which node owns a frame, which is what a free path needs when
//!   it has a frame and no node.
//!
//! Everything is a fixed-capacity array: the node list is built during early boot, before the
//! heap exists, and a machine with more than [`MAX_NODES`] nodes is a configuration this
//! project does not support yet -- the boot code reports that rather than guessing.

use crate::buddy::Frame;

/// The most NUMA nodes a [`NodeList`] can describe.
pub const MAX_NODES: usize = 8;

/// The distance a node has to itself in the ACPI SLIT, in the units that table uses.
pub const LOCAL_DISTANCE: u8 = 10;

/// The largest distance that still means "reachable at all". ACPI uses 255 for "no path".
pub const UNREACHABLE_DISTANCE: u8 = 255;

/// One physical memory node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    /// The node's identifier, as the firmware reports it. Small and dense on every machine
    /// this kernel is expected to run on, but not required to be.
    pub id: u16,
    /// The first frame the node owns.
    pub start: Frame,
    /// One past the last frame the node owns.
    pub end: Frame,
    /// Whether the node has usable memory at all. A node with no frames is still listed, so
    /// that distances to it stay meaningful, but nothing allocates from it.
    pub online: bool,
}

impl Node {
    /// The number of frames the node owns.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether `frame` belongs to this node.
    #[must_use]
    pub const fn contains(&self, frame: Frame) -> bool {
        frame >= self.start && frame < self.end
    }

    /// Whether the node can serve an allocation.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.online && self.end > self.start
    }
}

/// Why a node list is not a description of a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumaError {
    /// The list is full: the machine has more nodes than the boot code can describe.
    TooManyNodes,
    /// The identifier is already in the list. Two nodes with one identifier is a firmware bug
    /// this code refuses to paper over.
    DuplicateId(u16),
    /// A node owns no frames, which cannot be true of a node the firmware reported as usable.
    EmptyNode(u16),
    /// Two nodes claim the same frame.
    Overlap {
        /// The frame both nodes claim.
        frame: Frame,
    },
    /// A distance is missing or inconsistent: the matrix must be symmetric, and every node must
    /// be at [`LOCAL_DISTANCE`] from itself.
    BadDistance {
        /// One end of the pair.
        from: u16,
        /// The other end.
        to: u16,
    },
    /// A frame was looked up in a list that does not own it.
    NoSuchFrame(Frame),
    /// The node identifier is not in the list.
    NoSuchNode(u16),
}

/// The nodes of one machine, with the distances between them.
#[derive(Clone, Copy, Debug)]
pub struct NodeList<const NODES: usize> {
    nodes: [Option<Node>; NODES],
    /// `distances[i][j]` is the distance between the nodes recorded at those slots. Slot
    /// order, not identifier order, so the table stays dense whatever the firmware's ids are.
    distances: [[u8; MAX_NODES]; MAX_NODES],
    count: usize,
    /// The node the boot processor is on, once it is known.
    local: Option<u16>,
}

impl<const NODES: usize> Default for NodeList<NODES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const NODES: usize> NodeList<NODES> {
    /// An empty list.
    ///
    /// # Panics
    ///
    /// Panics when `NODES` is zero or exceeds [`MAX_NODES`]: there is no machine for which
    /// either is a useful configuration.
    #[must_use]
    pub const fn new() -> Self {
        assert!(NODES > 0 && NODES <= MAX_NODES);
        Self {
            nodes: [None; NODES],
            distances: [[UNREACHABLE_DISTANCE; MAX_NODES]; MAX_NODES],
            count: 0,
            local: None,
        }
    }

    /// How many nodes the list describes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the machine has more than one node.
    #[must_use]
    pub const fn is_numa(&self) -> bool {
        self.count > 1
    }

    /// The nodes, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter_map(Option::as_ref)
    }

    /// The slot a node occupies, by identifier.
    fn slot(&self, id: u16) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| matches!(node, Some(node) if node.id == id))
    }

    /// The node with this identifier, if it is in the list.
    #[must_use]
    pub fn get(&self, id: u16) -> Option<&Node> {
        self.nodes.iter().flatten().find(|node| node.id == id)
    }

    /// Add a node. Fails without changing anything if the list is full, the identifier is
    /// taken, or the node's frames overlap a node already in the list.
    ///
    /// A node is added online or offline: firmware may describe a node it has no memory for,
    /// and dropping it would lose the distance row that makes the others interpretable.
    pub fn add(&mut self, node: Node) -> Result<(), NumaError> {
        if self.slot(node.id).is_some() {
            return Err(NumaError::DuplicateId(node.id));
        }
        if node.online && node.end <= node.start {
            return Err(NumaError::EmptyNode(node.id));
        }
        for other in self.iter() {
            if node.end > other.start && node.start < other.end {
                return Err(NumaError::Overlap {
                    frame: node.start.max(other.start),
                });
            }
        }
        let slot = self.count;
        if slot >= NODES {
            return Err(NumaError::TooManyNodes);
        }
        self.nodes[slot] = Some(node);
        self.count += 1;
        // Every node is at least local distance from itself, which is what makes an unset
        // matrix still describe a usable machine.
        self.distances[slot][slot] = LOCAL_DISTANCE;
        Ok(())
    }

    /// Record the distance between two nodes. Both must already be in the list.
    pub fn set_distance(&mut self, from: u16, to: u16, distance: u8) -> Result<(), NumaError> {
        let a = self.slot(from).ok_or(NumaError::NoSuchNode(from))?;
        let b = self.slot(to).ok_or(NumaError::NoSuchNode(to))?;
        self.distances[a][b] = distance;
        self.distances[b][a] = distance;
        Ok(())
    }

    /// The distance between two nodes, or `None` when either is not in the list.
    #[must_use]
    pub fn distance(&self, from: u16, to: u16) -> Option<u8> {
        let a = self.slot(from)?;
        let b = self.slot(to)?;
        Some(self.distances[a][b])
    }

    /// Declare the node the boot processor is on.
    pub fn set_local(&mut self, id: u16) -> Result<(), NumaError> {
        if self.slot(id).is_none() {
            return Err(NumaError::NoSuchNode(id));
        }
        self.local = Some(id);
        Ok(())
    }

    /// The node the boot processor is on: the one that was declared, or the only node there
    /// is, or the lowest identifier. `None` only for an empty list.
    #[must_use]
    pub fn local(&self) -> Option<u16> {
        if let Some(id) = self.local {
            return Some(id);
        }
        self.iter().map(|node| node.id).min()
    }

    /// The node that owns `frame`.
    #[must_use]
    pub fn find(&self, frame: Frame) -> Option<&Node> {
        self.iter().find(|node| node.contains(frame))
    }

    /// The node that owns `frame`, or the error that says the list does not describe it.
    pub fn owner(&self, frame: Frame) -> Result<u16, NumaError> {
        self.find(frame)
            .map(|node| node.id)
            .ok_or(NumaError::NoSuchFrame(frame))
    }

    /// Total frames the list owns, online or not.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.iter().map(Node::frames).sum()
    }

    /// The nodes an allocation for `id` may be served from, in preference order.
    ///
    /// The local node comes first; then the rest by ascending distance, unreachable nodes
    /// last, and ties broken by identifier. A deterministic order matters: an allocation that
    /// prefers a node by chance is a performance bug that reproduces once a week.
    #[must_use]
    pub fn zonelist(&self, id: u16) -> Zonelist<NODES> {
        let mut list = Zonelist::new();
        let Some(local_slot) = self.slot(id) else {
            return list;
        };
        // Collect (distance, identifier, usable) and sort by insertion: a fixed-capacity
        // selection avoids a heap and keeps the order deterministic.
        let mut taken = [false; MAX_NODES];
        loop {
            let mut best: Option<(u8, u16, bool)> = None;
            for (slot, is_taken) in taken.iter().enumerate().take(self.count) {
                if *is_taken {
                    continue;
                }
                let Some(node) = self.nodes[slot] else {
                    continue;
                };
                let distance = self.distances[local_slot][slot];
                let candidate = (distance, node.id, node.is_usable());
                let better = match best {
                    None => true,
                    Some((distance, id, _)) => (candidate.0, candidate.1) < (distance, id),
                };
                if better {
                    best = Some(candidate);
                }
            }
            let Some((_, id, usable)) = best else {
                break;
            };
            let Some(slot) = self.slot(id) else {
                break;
            };
            taken[slot] = true;
            list.push(id, usable);
        }
        list
    }

    /// Check every invariant the list claims.
    ///
    /// Run by the boot code after the list is built: a node list that overlaps itself would
    /// make two allocators hand out the same frame, and the failure would surface as
    /// corruption rather than as an allocation error.
    pub fn check(&self) -> Result<(), NumaError> {
        for slot in 0..self.count {
            let Some(node) = self.nodes[slot] else {
                return Err(NumaError::EmptyNode(0));
            };
            if self.distances[slot][slot] != LOCAL_DISTANCE {
                return Err(NumaError::BadDistance {
                    from: node.id,
                    to: node.id,
                });
            }
            for peer in slot + 1..self.count {
                let Some(other) = self.nodes[peer] else {
                    return Err(NumaError::EmptyNode(0));
                };
                if self.distances[slot][peer] != self.distances[peer][slot] {
                    return Err(NumaError::BadDistance {
                        from: node.id,
                        to: other.id,
                    });
                }
                if node.end > other.start && node.start < other.end {
                    return Err(NumaError::Overlap {
                        frame: node.start.max(other.start),
                    });
                }
            }
        }
        Ok(())
    }
}

/// The nodes one allocation may be served from, in preference order.
#[derive(Clone, Copy, Debug)]
pub struct Zonelist<const NODES: usize> {
    entries: [(u16, bool); NODES],
    count: usize,
}

impl<const NODES: usize> Default for Zonelist<NODES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const NODES: usize> Zonelist<NODES> {
    /// An empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [(0, false); NODES],
            count: 0,
        }
    }

    /// How many nodes the list can offer.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The first node: the one an allocation tries first.
    #[must_use]
    pub fn first(&self) -> Option<u16> {
        (self.count > 0).then(|| self.entries[0].0)
    }

    /// Append a node and whether it can serve an allocation.
    fn push(&mut self, id: u16, usable: bool) {
        if self.count < NODES {
            self.entries[self.count] = (id, usable);
            self.count += 1;
        }
    }

    /// The nodes in preference order, with whether each can serve an allocation.
    pub fn iter(&self) -> impl Iterator<Item = (u16, bool)> + '_ {
        self.entries[..self.count].iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(nodes: &[(u16, Frame, Frame)]) -> NodeList<MAX_NODES> {
        let mut list = NodeList::new();
        for (id, start, end) in nodes {
            list.add(Node {
                id: *id,
                start: *start,
                end: *end,
                online: true,
            })
            .expect("added");
        }
        list
    }

    #[test]
    fn a_frame_is_found_in_the_node_that_describes_it() {
        let list = list(&[(0, 0, 1024), (1, 1024, 2048)]);
        assert_eq!(list.owner(0), Ok(0));
        assert_eq!(list.owner(1023), Ok(0));
        assert_eq!(list.owner(1024), Ok(1));
        assert_eq!(list.owner(2047), Ok(1));
        assert_eq!(list.owner(2048), Err(NumaError::NoSuchFrame(2048)));
        assert_eq!(list.frames(), 2048);
        assert!(list.is_numa());
        assert_eq!(list.check(), Ok(()));
    }

    #[test]
    fn the_local_node_comes_first_and_the_nearest_next() {
        let mut list = list(&[(0, 0, 1024), (1, 1024, 2048), (2, 2048, 3072)]);
        list.set_distance(0, 1, 20).expect("0-1");
        list.set_distance(0, 2, 12).expect("0-2");
        list.set_distance(1, 2, 30).expect("1-2");
        assert_eq!(list.distance(0, 2), Some(12));
        assert_eq!(list.distance(2, 0), Some(12));

        let zones = list.zonelist(0);
        let order: Vec<u16> = zones.iter().map(|(id, _)| id).collect();
        assert_eq!(order, [0, 2, 1]);
        assert_eq!(zones.first(), Some(0));
        assert_eq!(zones.len(), 3);
    }

    #[test]
    fn ties_are_broken_by_identifier_so_the_order_is_deterministic() {
        let mut list = list(&[(3, 0, 1024), (1, 1024, 2048), (2, 2048, 3072)]);
        list.set_distance(3, 1, 20).expect("3-1");
        list.set_distance(3, 2, 20).expect("3-2");
        list.set_local(3).expect("local");
        let order: Vec<u16> = list.zonelist(3).iter().map(|(id, _)| id).collect();
        assert_eq!(order, [3, 1, 2]);
    }

    #[test]
    fn a_single_node_list_always_offers_that_node() {
        let list = list(&[(7, 512, 1536)]);
        assert_eq!(list.local(), Some(7));
        assert_eq!(list.zonelist(7).first(), Some(7));
        assert_eq!(list.zonelist(7).len(), 1);
        assert!(!list.is_numa());
        assert_eq!(list.zonelist(99).len(), 0);
    }

    #[test]
    fn an_offline_node_is_listed_but_never_offered_to_an_allocation() {
        let mut list = list(&[(0, 0, 1024)]);
        list.add(Node {
            id: 1,
            start: 1024,
            end: 1024,
            online: false,
        })
        .expect("added");
        let zones: Vec<(u16, bool)> = list.zonelist(0).iter().collect();
        assert_eq!(zones, [(0, true), (1, false)]);
        assert_eq!(list.check(), Ok(()));
    }

    #[test]
    fn overlapping_and_duplicate_nodes_are_refused_and_change_nothing() {
        let mut list = list(&[(0, 0, 1024), (1, 1024, 2048)]);
        assert_eq!(
            list.add(Node {
                id: 2,
                start: 512,
                end: 1536,
                online: true,
            }),
            Err(NumaError::Overlap { frame: 512 })
        );
        assert_eq!(
            list.add(Node {
                id: 1,
                start: 4096,
                end: 5120,
                online: true,
            }),
            Err(NumaError::DuplicateId(1))
        );
        assert_eq!(
            list.add(Node {
                id: 3,
                start: 8192,
                end: 8192,
                online: true,
            }),
            Err(NumaError::EmptyNode(3))
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list.check(), Ok(()));
    }

    #[test]
    fn a_full_list_refuses_another_node() {
        let mut list = NodeList::<2>::new();
        for id in 0..2u16 {
            list.add(Node {
                id,
                start: u64::from(id) * 1024,
                end: u64::from(id + 1) * 1024,
                online: true,
            })
            .expect("added");
        }
        assert_eq!(
            list.add(Node {
                id: 9,
                start: 16384,
                end: 17408,
                online: true,
            }),
            Err(NumaError::TooManyNodes)
        );
    }

    #[test]
    fn a_distance_to_a_node_outside_the_list_is_refused() {
        let mut list = list(&[(0, 0, 1024), (1, 1024, 2048)]);
        assert_eq!(list.set_distance(0, 5, 20), Err(NumaError::NoSuchNode(5)));
        assert_eq!(list.set_local(5), Err(NumaError::NoSuchNode(5)));
        assert_eq!(list.distance(0, 5), None);
        assert_eq!(list.distance(0, 1), Some(UNREACHABLE_DISTANCE));
    }
}
