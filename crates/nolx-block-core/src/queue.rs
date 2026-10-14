//! Request queue rules: block requests of one direction that touch end to end merge into one
//! transfer, up to a size limit, so a driver submits fewer and larger commands. A merge can
//! also bridge two requests already queued, and the queue then coalesces them. Pure logic: the
//! kernel's drivers take what the queue hands back.
//!
//! Requests are kept in arrival order. Merging never reorders them, and a request that cannot
//! merge is queued behind the others.

/// The direction of a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the device into memory.
    Read,
    /// From memory onto the device.
    Write,
}

/// One block transfer: `sectors` sectors starting at `lba`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The direction of the transfer.
    pub direction: Direction,
    /// The first sector.
    pub lba: u64,
    /// How many sectors. A request is never empty.
    pub sectors: u32,
}

/// Why a request was refused by [`Queue::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The request has no sectors, or is larger than the size limit on its own.
    Invalid,
    /// The request cannot merge with a queued one, and the queue has no free slot.
    Full,
}

/// The merged request for `a` and `b`, if they are the same direction and touch end to end in
/// either order, and the result is within `max_sectors`. Sums and sector ends are checked, so
/// an overflow is a refusal, not a wrap.
#[must_use]
pub fn merge(a: Request, b: Request, max_sectors: u32) -> Option<Request> {
    if a.direction != b.direction || a.sectors == 0 || b.sectors == 0 {
        return None;
    }
    let sectors = a.sectors.checked_add(b.sectors)?;
    if sectors > max_sectors {
        return None;
    }
    let a_end = a.lba.checked_add(u64::from(a.sectors))?;
    let b_end = b.lba.checked_add(u64::from(b.sectors))?;
    if a_end == b.lba {
        return Some(Request { direction: a.direction, lba: a.lba, sectors });
    }
    if b_end == a.lba {
        return Some(Request { direction: a.direction, lba: b.lba, sectors });
    }
    None
}

/// A fixed-capacity queue of requests in arrival order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queue<const N: usize> {
    slots: [Option<Request>; N],
    len: usize,
}

impl<const N: usize> Default for Queue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Queue<N> {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [None; N],
            len: 0,
        }
    }

    /// How many requests are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no request is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Queue `request`. It merges into a queued request when it can, and the merged request then
    /// absorbs any queued requests it now touches. Otherwise it takes the last free slot.
    ///
    /// # Errors
    ///
    /// [`PushError::Invalid`] for an empty request or one larger than `max_sectors`;
    /// [`PushError::Full`] when it cannot merge and the queue has no free slot.
    pub fn push(&mut self, request: Request, max_sectors: u32) -> Result<(), PushError> {
        if request.sectors == 0 || request.sectors > max_sectors {
            return Err(PushError::Invalid);
        }
        let found = self.slots[..self.len]
            .iter()
            .enumerate()
            .find_map(|(index, slot)| {
                let existing = (*slot)?;
                merge(existing, request, max_sectors).map(|merged| (index, merged))
            });
        if let Some((index, merged)) = found {
            self.slots[index] = Some(merged);
            self.coalesce(index, max_sectors);
            return Ok(());
        }
        if self.len == N {
            return Err(PushError::Full);
        }
        self.slots[self.len] = Some(request);
        self.len += 1;
        Ok(())
    }

    /// The oldest queued request, removed from the queue.
    pub fn pop(&mut self) -> Option<Request> {
        let front = self.slots.first().copied().flatten()?;
        self.remove(0);
        Some(front)
    }

    /// Merge the request at `at` with any other queued request it now touches, until none does.
    fn coalesce(&mut self, mut at: usize, max_sectors: u32) {
        let mut other = 0;
        while other < self.len {
            let candidate = if other == at { None } else { self.slots[other] };
            let merged = candidate
                .zip(self.slots[at])
                .and_then(|(b, a)| merge(a, b, max_sectors));
            match merged {
                Some(merged) => {
                    self.slots[at] = Some(merged);
                    self.remove(other);
                    if other < at {
                        at -= 1;
                    }
                    // The merged request grew, so every other request is looked at again.
                    other = 0;
                }
                None => other += 1,
            }
        }
    }

    /// Remove the request at `index`, keeping the others in order.
    fn remove(&mut self, index: usize) {
        self.slots.copy_within(index + 1..self.len, index);
        self.slots[self.len - 1] = None;
        self.len -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(lba: u64, sectors: u32) -> Request {
        Request { direction: Direction::Read, lba, sectors }
    }

    fn write(lba: u64, sectors: u32) -> Request {
        Request { direction: Direction::Write, lba, sectors }
    }

    #[test]
    fn back_to_back_requests_of_one_direction_merge() {
        let mut queue: Queue<4> = Queue::new();
        queue.push(read(100, 8), 64).unwrap();
        queue.push(read(108, 8), 64).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some(read(100, 16)));
    }

    #[test]
    fn a_request_just_before_a_queued_one_merges_at_the_front() {
        let mut queue: Queue<4> = Queue::new();
        queue.push(read(200, 8), 64).unwrap();
        queue.push(read(192, 8), 64).unwrap();
        assert_eq!(queue.pop(), Some(read(192, 16)));
    }

    #[test]
    fn different_directions_and_gaps_do_not_merge() {
        let mut queue: Queue<4> = Queue::new();
        queue.push(read(100, 8), 64).unwrap();
        queue.push(write(108, 8), 64).unwrap();
        queue.push(read(120, 8), 64).unwrap();
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn a_merge_stops_at_the_size_limit() {
        let mut queue: Queue<4> = Queue::new();
        queue.push(read(100, 8), 16).unwrap();
        queue.push(read(108, 8), 16).unwrap();
        queue.push(read(116, 4), 16).unwrap();
        assert_eq!(queue.len(), 2, "the third request would make 20 sectors");
        assert_eq!(queue.pop(), Some(read(100, 16)));
        assert_eq!(queue.pop(), Some(read(116, 4)));
    }

    #[test]
    fn a_merge_bridges_two_queued_requests() {
        let mut queue: Queue<4> = Queue::new();
        queue.push(read(0, 8), 64).unwrap();
        queue.push(read(16, 8), 64).unwrap();
        assert_eq!(queue.len(), 2);
        queue.push(read(8, 8), 64).unwrap();
        assert_eq!(queue.len(), 1, "0..8, 8..16 and 16..24 coalesce into one");
        assert_eq!(queue.pop(), Some(read(0, 24)));
    }

    #[test]
    fn empty_and_oversized_requests_are_invalid() {
        let mut queue: Queue<4> = Queue::new();
        assert_eq!(queue.push(read(0, 0), 64), Err(PushError::Invalid));
        assert_eq!(queue.push(read(0, 65), 64), Err(PushError::Invalid));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_full_queue_refuses_what_cannot_merge_and_still_takes_merges() {
        let mut queue: Queue<2> = Queue::new();
        queue.push(read(0, 8), 64).unwrap();
        queue.push(read(100, 8), 64).unwrap();
        assert_eq!(queue.push(read(200, 8), 64), Err(PushError::Full));
        assert!(queue.push(read(8, 8), 64).is_ok(), "this one merges into 0..8");
        assert_eq!(queue.pop(), Some(read(0, 16)));
        assert_eq!(queue.pop(), Some(read(100, 8)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn merge_refuses_overflowing_ends_and_sums_instead_of_wrapping() {
        let near_top = read(u64::MAX - 7, 8);
        assert_eq!(merge(near_top, read(0, 8), 64), None);
        let big = read(0, u32::MAX);
        assert_eq!(merge(big, read(u64::from(u32::MAX), 1), u32::MAX), None);
    }
}
