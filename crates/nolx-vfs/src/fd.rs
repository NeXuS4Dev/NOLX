//! The per-process file descriptor table.
//!
//! A descriptor is an index into this table; the entry points at an open file. Two
//! descriptors produced by `dup` share one open file and therefore one offset, while two
//! descriptors from separate `open` calls do not -- which is why the table stores a file
//! identifier rather than the file itself.

use alloc::vec::Vec;

use nolx_types::errno;
use nolx_types::scalars;

/// An open descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// The open file this descriptor refers to. Descriptors that share a file share its
    /// offset and its status flags.
    pub file: u64,
    /// `FD_CLOEXEC`: close this descriptor on `exec`.
    pub close_on_exec: bool,
}

impl Descriptor {
    /// A descriptor with default flags.
    #[must_use]
    pub const fn new(file: u64) -> Self {
        Self {
            file,
            close_on_exec: false,
        }
    }
}

/// A process's descriptor table.
#[derive(Clone, Debug)]
pub struct DescriptorTable {
    slots: Vec<Option<Descriptor>>,
    /// `RLIMIT_NOFILE`, the soft limit. Growing past it is `EMFILE`.
    limit: usize,
    /// Count of successful allocations, for `getrusage` and diagnostics.
    allocations: u64,
}

impl Default for DescriptorTable {
    fn default() -> Self {
        Self::with_limit(scalars::OPEN_MAX)
    }
}

impl DescriptorTable {
    /// An empty table with `limit` slots available.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            slots: Vec::new(),
            limit: limit.max(1),
            allocations: 0,
        }
    }

    /// Raise or lower the soft limit, as `setrlimit(RLIMIT_NOFILE)` does.
    ///
    /// Lowering below the highest descriptor in use is allowed by POSIX; the descriptors
    /// stay open, they just cannot be duplicated or reused until they are closed.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    /// The current soft limit.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// The highest descriptor in use, plus one. This is what `select(2)` wants as `nfds`,
    /// and caching it would mean it could drift, so it is computed.
    #[must_use]
    pub fn next_after_last(&self) -> usize {
        self.slots
            .iter()
            .rposition(|s| s.is_some())
            .map_or(0, |i| i + 1)
    }

    /// The number of open descriptors.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Allocate the lowest-numbered unused descriptor, as `open` and `dup` must.
    ///
    /// # Errors
    ///
    /// `EMFILE` if the process is at its limit.
    pub fn alloc(&mut self, descriptor: Descriptor) -> Result<usize, i32> {
        self.alloc_at_or_above(0, descriptor)
    }

    /// Allocate the lowest-numbered unused descriptor at or above `min`, as
    /// `fcntl(F_DUPFD)` and `F_DUPFD_CLOEXEC` must.
    ///
    /// # Errors
    ///
    /// `EINVAL` if `min` is negative when interpreted as a descriptor, `EMFILE` if no slot
    /// is available below the limit.
    pub fn alloc_at_or_above(&mut self, min: usize, descriptor: Descriptor) -> Result<usize, i32> {
        if min >= self.limit {
            return Err(errno::EMFILE);
        }
        let slot = (min..self.limit)
            .find(|&i| self.slots.get(i).map_or(true, Option::is_none))
            .ok_or(errno::EMFILE)?;
        self.place(slot, descriptor);
        self.allocations += 1;
        Ok(slot)
    }

    /// Place a descriptor at an exact index, closing whatever was there.
    ///
    /// This is `dup2`/`dup3`: POSIX requires the target to be closed first, atomically with
    /// respect to signals, and requires `dup2(fd, fd)` to be a no-op that leaves `fd` open.
    /// The caller is responsible for the `old == new` check, because only it knows whether
    /// the two arguments were the same descriptor.
    ///
    /// Returns the descriptor that was displaced, if any, so the caller can release it.
    ///
    /// # Errors
    ///
    /// `EBADF` if `slot` is at or above the limit.
    pub fn place_at(
        &mut self,
        slot: usize,
        descriptor: Descriptor,
    ) -> Result<Option<Descriptor>, i32> {
        if slot >= self.limit {
            return Err(errno::EBADF);
        }
        Ok(self.place(slot, descriptor))
    }

    fn place(&mut self, slot: usize, descriptor: Descriptor) -> Option<Descriptor> {
        if self.slots.len() <= slot {
            self.slots.resize(slot + 1, None);
        }
        self.slots[slot].replace(descriptor)
    }

    /// The descriptor at `fd`.
    ///
    /// # Errors
    ///
    /// `EBADF` if `fd` is not open.
    pub fn get(&self, fd: usize) -> Result<Descriptor, i32> {
        self.slots.get(fd).and_then(|s| *s).ok_or(errno::EBADF)
    }

    /// `true` if `fd` is open. Never an error, because `close` and `dup2` both need to ask
    /// without failing.
    #[must_use]
    pub fn is_open(&self, fd: usize) -> bool {
        self.slots.get(fd).map_or(false, Option::is_some)
    }

    /// Close `fd`.
    ///
    /// # Errors
    ///
    /// `EBADF` if `fd` is not open. POSIX makes closing an invalid descriptor an error, and
    /// programs do depend on it.
    pub fn close(&mut self, fd: usize) -> Result<Descriptor, i32> {
        let slot = self.slots.get_mut(fd).ok_or(errno::EBADF)?;
        slot.take().ok_or(errno::EBADF)
    }

    /// Set or clear `FD_CLOEXEC`.
    ///
    /// # Errors
    ///
    /// `EBADF` if `fd` is not open.
    pub fn set_close_on_exec(&mut self, fd: usize, value: bool) -> Result<(), i32> {
        let slot = self.slots.get_mut(fd).ok_or(errno::EBADF)?;
        let descriptor = slot.as_mut().ok_or(errno::EBADF)?;
        descriptor.close_on_exec = value;
        Ok(())
    }

    /// Close every descriptor marked `FD_CLOEXEC`, as `exec` must.
    ///
    /// Returns the closed descriptors so the caller can release the underlying open files.
    pub fn close_on_exec(&mut self) -> Vec<Descriptor> {
        let mut closed = Vec::new();
        for slot in self.slots.iter_mut() {
            if slot.map_or(false, |d| d.close_on_exec) {
                if let Some(d) = slot.take() {
                    closed.push(d);
                }
            }
        }
        closed
    }

    /// Total allocations since the table was created.
    #[must_use]
    pub const fn allocations(&self) -> u64 {
        self.allocations
    }

    /// Build a table with the three standard descriptors already open on `files`, as a new
    /// process gets from its parent or from `posix_spawn` file actions.
    #[must_use]
    pub fn with_stdio(stdin: u64, stdout: u64, stderr: u64) -> Self {
        let mut t = Self::default();
        for file in [stdin, stdout, stderr] {
            // Cannot fail: the default limit is far above 3.
            let _ = t.alloc(Descriptor::new(file));
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_returns_the_lowest_free_slot() {
        let mut t = DescriptorTable::default();
        assert_eq!(t.alloc(Descriptor::new(10)).unwrap(), 0);
        assert_eq!(t.alloc(Descriptor::new(11)).unwrap(), 1);
        assert_eq!(t.alloc(Descriptor::new(12)).unwrap(), 2);
    }

    #[test]
    fn a_closed_slot_is_reused_before_a_higher_one() {
        let mut t = DescriptorTable::default();
        let a = t.alloc(Descriptor::new(10)).unwrap();
        let _b = t.alloc(Descriptor::new(11)).unwrap();
        let _c = t.alloc(Descriptor::new(12)).unwrap();
        t.close(a).unwrap();
        assert_eq!(t.alloc(Descriptor::new(13)).unwrap(), a);
    }

    #[test]
    fn stdio_occupies_the_first_three() {
        let t = DescriptorTable::with_stdio(0, 1, 2);
        assert_eq!(t.get(0).unwrap().file, 0);
        assert_eq!(t.get(1).unwrap().file, 1);
        assert_eq!(t.get(2).unwrap().file, 2);
        assert_eq!(t.next_after_last(), 3);
        assert_eq!(t.open_count(), 3);
    }

    #[test]
    fn alloc_at_or_above_skips_lower_slots() {
        let mut t = DescriptorTable::default();
        assert_eq!(t.alloc_at_or_above(5, Descriptor::new(1)).unwrap(), 5);
        assert_eq!(t.alloc_at_or_above(5, Descriptor::new(2)).unwrap(), 6);
        assert_eq!(t.alloc(Descriptor::new(3)).unwrap(), 0);
    }

    #[test]
    fn the_limit_is_enforced_with_emfile() {
        let mut t = DescriptorTable::with_limit(2);
        assert!(t.alloc(Descriptor::new(1)).is_ok());
        assert!(t.alloc(Descriptor::new(2)).is_ok());
        assert_eq!(t.alloc(Descriptor::new(3)), Err(errno::EMFILE));
        assert_eq!(
            t.alloc_at_or_above(2, Descriptor::new(4)),
            Err(errno::EMFILE)
        );
    }

    #[test]
    fn placing_above_the_limit_is_ebadf() {
        let mut t = DescriptorTable::with_limit(4);
        assert_eq!(t.place_at(4, Descriptor::new(1)), Err(errno::EBADF));
        assert!(t.place_at(3, Descriptor::new(1)).is_ok());
    }

    #[test]
    fn place_at_displaces_and_reports_the_previous_descriptor() {
        let mut t = DescriptorTable::default();
        t.alloc(Descriptor::new(10)).unwrap();
        t.alloc(Descriptor::new(11)).unwrap();
        let displaced = t.place_at(1, Descriptor::new(99)).unwrap();
        assert_eq!(displaced, Some(Descriptor::new(11)));
        assert_eq!(t.get(1).unwrap().file, 99);
    }

    #[test]
    fn closing_an_invalid_descriptor_is_ebadf() {
        let mut t = DescriptorTable::default();
        assert_eq!(t.close(0), Err(errno::EBADF));
        t.alloc(Descriptor::new(1)).unwrap();
        t.close(0).unwrap();
        assert_eq!(t.close(0), Err(errno::EBADF), "closing twice must fail");
    }

    #[test]
    fn get_on_an_invalid_descriptor_is_ebadf() {
        let t = DescriptorTable::default();
        assert_eq!(t.get(0), Err(errno::EBADF));
        assert_eq!(t.get(1000), Err(errno::EBADF));
    }

    #[test]
    fn dup_shares_the_file_and_therefore_the_offset() {
        // The table stores a file id, not the file, so a duplicated descriptor is a
        // distinct entry pointing at the same id. That identity is what makes the shared
        // offset work.
        let mut t = DescriptorTable::default();
        let a = t.alloc(Descriptor::new(42)).unwrap();
        let src = t.get(a).unwrap();
        let b = t.alloc(src).unwrap();
        assert_eq!(t.get(b).unwrap().file, t.get(a).unwrap().file);
        assert_ne!(a, b);
    }

    #[test]
    fn close_on_exec_closes_only_the_marked_descriptors() {
        let mut t = DescriptorTable::with_stdio(0, 1, 2);
        let d = t.alloc(Descriptor::new(7)).unwrap();
        t.set_close_on_exec(d, true).unwrap();
        t.set_close_on_exec(2, true).unwrap();

        let closed = t.close_on_exec();
        assert_eq!(closed.len(), 2);
        assert!(closed.contains(&Descriptor {
            file: 7,
            close_on_exec: true
        }));
        assert!(t.is_open(0) && t.is_open(1));
        assert!(!t.is_open(2));
        assert!(!t.is_open(d));
    }

    #[test]
    fn close_on_exec_flag_round_trips() {
        let mut t = DescriptorTable::default();
        let d = t.alloc(Descriptor::new(1)).unwrap();
        assert!(!t.get(d).unwrap().close_on_exec);
        t.set_close_on_exec(d, true).unwrap();
        assert!(t.get(d).unwrap().close_on_exec);
        t.set_close_on_exec(d, false).unwrap();
        assert!(!t.get(d).unwrap().close_on_exec);
    }

    #[test]
    fn setting_close_on_exec_on_a_closed_descriptor_is_ebadf() {
        let mut t = DescriptorTable::default();
        assert_eq!(t.set_close_on_exec(0, true), Err(errno::EBADF));
    }

    #[test]
    fn next_after_last_tracks_the_table() {
        let mut t = DescriptorTable::default();
        assert_eq!(t.next_after_last(), 0);
        let a = t.alloc(Descriptor::new(1)).unwrap();
        assert_eq!(t.next_after_last(), a + 1);
        t.close(a).unwrap();
        assert_eq!(t.next_after_last(), 0);
    }

    #[test]
    fn allocations_are_counted() {
        let mut t = DescriptorTable::default();
        for i in 0..5u64 {
            t.alloc(Descriptor::new(i)).unwrap();
        }
        assert_eq!(t.allocations(), 5);
        assert_eq!(t.open_count(), 5);
    }
}
