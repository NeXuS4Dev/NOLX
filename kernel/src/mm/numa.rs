//! The machine's NUMA nodes, and the zonelist every allocation walks.
//!
//! On the machines this kernel runs on today there is exactly one node: all of physical
//! memory is equally close to the only processor. That is a reason to build the *structure*
//! now rather than a reason to skip it. The frame allocator already asks this module which
//! node an allocation may come from, so the per-node allocators that arrive with ACPI SRAT
//! parsing plug into a decision that is already being made, instead of requiring every
//! allocation path to be revisited.
//!
//! The list is built from what the frame map knows: one node covering the frames the map
//! describes, at [`LOCAL_DISTANCE`] from itself. Nothing here parses firmware tables yet --
//! SRAT is M9's ACPI work -- and a machine that reports more than [`MAX_NODES`] nodes would be
//! reported rather than guessed at.
//!
//! A node with no allocator behind it refuses rather than borrows: `allocate_from` returns
//! `None` for any node that is not the local one, so a second node's frames are never handed
//! out before that node has its own allocator. An allocation that cannot be served locally
//! therefore fails loudly at the call site rather than silently taking memory across an
//! interconnect that is not there.

use nolx_hal_core::console::HalConsole;
use nolx_mm_core::numa::{Node, NodeList, NumaError, Zonelist, LOCAL_DISTANCE, MAX_NODES};

use crate::mm;
use crate::sync::SyncCell;

/// The machine's nodes.
static NODES: SyncCell<NodeList<MAX_NODES>> = SyncCell::new(NodeList::new());

/// Build the node list from the frame map: one node, all of the frames the map describes.
///
/// Runs once, as soon as the frame map is sized and before the allocator can hand out anything:
/// an allocation that ran with an empty node list would have no node to come from, which is why
/// this is not left until the first report.
pub fn init() {
    let end = mm::frame_map_len() as u64;
    // SAFETY: single boot processor, interrupts masked, and nothing has been allocated yet.
    let list = unsafe { &mut *NODES.get() };
    *list = NodeList::new();
    if end == 0 {
        return;
    }
    if list
        .add(Node {
            id: 0,
            start: 0,
            end,
            online: true,
        })
        .is_err()
    {
        return;
    }
    let _ = list.set_local(0);
}

/// Report the node list, once the allocator can say how much of it is free.
pub fn report(console: &mut dyn HalConsole) {
    // SAFETY: as [`init`]: a read-only walk on the boot processor.
    let list = unsafe { &*NODES.get() };
    let local = list.local().unwrap_or(0);
    let zones = list.zonelist(local);
    let _ = write!(
        console,
        "mm: numa: {} node(s), node(s) 0..{end}, local node {local}, {free} free frames, \
         zonelist [",
        list.len(),
        end = list.get(0).map_or(0, |node| node.end),
        free = mm::free_frames(),
    );
    for (index, (id, usable)) in zones.iter().enumerate() {
        if index > 0 {
            let _ = write!(console, ", ");
        }
        let _ = write!(console, "{id}{}", if usable { "" } else { " (offline)" });
    }
    let _ = writeln!(console, "], distance matrix local={}", local_distance());
}

/// A failure while describing the machine, printed rather than propagated.
pub fn check_or_report(console: &mut dyn HalConsole) {
    if let Err(error) = check() {
        let _ = writeln!(
            console,
            "mm: WARNING the node list is inconsistent: {error:?}"
        );
    }
}

/// The nodes an allocation on the boot processor may be served from, in preference order.
#[must_use]
pub fn zones() -> Zonelist<MAX_NODES> {
    // SAFETY: the list is written once during boot and read afterwards, on the boot processor.
    let list = unsafe { &*NODES.get() };
    let local = list.local().unwrap_or(0);
    list.zonelist(local)
}

/// The node the boot processor is on.
#[must_use]
pub fn local() -> u16 {
    // SAFETY: as [`zones`].
    unsafe { (*NODES.get()).local().unwrap_or(0) }
}

/// How many nodes the machine has.
#[must_use]
pub fn count() -> usize {
    // SAFETY: as [`zones`].
    unsafe { (*NODES.get()).len() }
}

/// The node that owns `frame`.
#[must_use]
pub fn owner(frame: usize) -> Option<u16> {
    // SAFETY: as [`zones`].
    unsafe { (*NODES.get()).owner(frame as u64).ok() }
}

/// Verify the node list against its own invariants.
pub fn check() -> Result<(), NumaError> {
    // SAFETY: as [`zones`].
    unsafe { (*NODES.get()).check() }
}

/// The distance between two nodes, in the units the node list uses.
#[must_use]
pub fn distance(from: u16, to: u16) -> Option<u8> {
    // SAFETY: as [`zones`].
    unsafe { (*NODES.get()).distance(from, to) }
}

/// Allocate `2^order` contiguous frames from `node`.
///
/// `None` means the node cannot serve the request -- and for a node that has no allocator,
/// that is the honest answer rather than a fallback to whatever memory is closest. Only the
/// local node has an allocator today; SRAT parsing gives the others their own, and this
/// function is where that happens.
pub fn allocate_from(node: u16, order: usize) -> Option<usize> {
    if node != local() {
        return None;
    }
    mm::allocate_frames_from_local(order)
}

/// The distance the list records between a node and itself, for tests and reports.
#[must_use]
pub const fn local_distance() -> u8 {
    LOCAL_DISTANCE
}
