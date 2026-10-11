//! Host-testable queued synchronization primitives used by dispatcher and executive code.
//!
//! [`QueuedLock`] is a FIFO ticket spin lock suitable for short nonblocking critical sections.
//! [`PushLock`] is a writer-fair reader/writer lock: queued writers close the reader gate so a
//! steady stream of new readers cannot starve an exclusive owner. Neither lock may be held
//! while blocking or while calling unknown code.

use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A FIFO ticket lock for short critical sections.
pub struct QueuedLock {
    next_ticket: AtomicUsize,
    serving: AtomicUsize,
}

impl QueuedLock {
    /// Construct an unlocked ticket lock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_ticket: AtomicUsize::new(0),
            serving: AtomicUsize::new(0),
        }
    }

    /// Acquire the next FIFO ticket.
    pub fn lock(&self) -> QueuedLockGuard<'_> {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
        QueuedLockGuard { lock: self, ticket }
    }

    /// Try to acquire the lock without waiting.
    pub fn try_lock(&self) -> Option<QueuedLockGuard<'_>> {
        let serving = self.serving.load(Ordering::Acquire);
        self.next_ticket
            .compare_exchange(
                serving,
                serving.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()
            .map(|ticket| QueuedLockGuard { lock: self, ticket })
    }

    /// Whether at least one ticket is outstanding.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.next_ticket.load(Ordering::Acquire) != self.serving.load(Ordering::Acquire)
    }
}

impl Default for QueuedLock {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that releases a [`QueuedLock`] ticket.
pub struct QueuedLockGuard<'a> {
    lock: &'a QueuedLock,
    ticket: usize,
}

impl Drop for QueuedLockGuard<'_> {
    fn drop(&mut self) {
        self.lock
            .serving
            .store(self.ticket.wrapping_add(1), Ordering::Release);
    }
}

const WRITER_ACTIVE: usize = usize::MAX;

/// A writer-fair reader/writer push lock.
///
/// Readers may share the lock while no writer is queued. Once a writer takes a ticket, new
/// readers wait behind it; existing readers drain, then writers acquire in FIFO ticket order.
pub struct PushLock {
    state: AtomicUsize,
    next_writer: AtomicUsize,
    serving_writer: AtomicUsize,
}

impl PushLock {
    /// Construct an unlocked push lock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            next_writer: AtomicUsize::new(0),
            serving_writer: AtomicUsize::new(0),
        }
    }

    /// Acquire a shared reader reference.
    pub fn lock_shared(&self) -> SharedPushGuard<'_> {
        loop {
            while self.next_writer.load(Ordering::Acquire)
                != self.serving_writer.load(Ordering::Acquire)
            {
                spin_loop();
            }
            let readers = self.state.load(Ordering::Acquire);
            if readers != WRITER_ACTIVE && readers < WRITER_ACTIVE - 1 {
                if self
                    .state
                    .compare_exchange_weak(
                        readers,
                        readers + 1,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return SharedPushGuard { lock: self };
                }
            } else {
                spin_loop();
            }
        }
    }

    /// Acquire the lock exclusively in writer-ticket order.
    pub fn lock_exclusive(&self) -> ExclusivePushGuard<'_> {
        let ticket = self.next_writer.fetch_add(1, Ordering::Relaxed);
        while self.serving_writer.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
        while self
            .state
            .compare_exchange_weak(0, WRITER_ACTIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        ExclusivePushGuard { lock: self, ticket }
    }

    /// Try to acquire the lock exclusively only when no reader or writer is active or queued.
    pub fn try_lock_exclusive(&self) -> Option<ExclusivePushGuard<'_>> {
        let serving = self.serving_writer.load(Ordering::Acquire);
        if self.next_writer.load(Ordering::Acquire) != serving {
            return None;
        }
        if self
            .next_writer
            .compare_exchange(
                serving,
                serving.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return None;
        }
        if self
            .state
            .compare_exchange(0, WRITER_ACTIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.serving_writer
                .store(serving.wrapping_add(1), Ordering::Release);
            return None;
        }
        Some(ExclusivePushGuard {
            lock: self,
            ticket: serving,
        })
    }

    /// Current reader count; returns zero while an exclusive owner holds the lock.
    #[must_use]
    pub fn reader_count(&self) -> usize {
        let state = self.state.load(Ordering::Acquire);
        if state == WRITER_ACTIVE {
            0
        } else {
            state
        }
    }

    /// Whether an exclusive owner currently holds the lock.
    #[must_use]
    pub fn is_exclusive(&self) -> bool {
        self.state.load(Ordering::Acquire) == WRITER_ACTIVE
    }
}

impl Default for PushLock {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for a shared push-lock acquisition.
pub struct SharedPushGuard<'a> {
    lock: &'a PushLock,
}

impl Drop for SharedPushGuard<'_> {
    fn drop(&mut self) {
        self.lock.state.fetch_sub(1, Ordering::Release);
    }
}

/// RAII guard for an exclusive push-lock acquisition.
pub struct ExclusivePushGuard<'a> {
    lock: &'a PushLock,
    ticket: usize,
}

impl Drop for ExclusivePushGuard<'_> {
    fn drop(&mut self) {
        self.lock.state.store(0, Ordering::Release);
        self.lock
            .serving_writer
            .store(self.ticket.wrapping_add(1), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn ticket_lock_serializes_contended_updates() {
        let lock = Arc::new(QueuedLock::new());
        let value = Arc::new(StdAtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let lock = Arc::clone(&lock);
            let value = Arc::clone(&value);
            workers.push(thread::spawn(move || {
                for _ in 0..250 {
                    let _guard = lock.lock();
                    let previous = value.load(StdOrdering::Relaxed);
                    value.store(previous + 1, StdOrdering::Relaxed);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(value.load(StdOrdering::Relaxed), 1_000);
        assert!(!lock.is_locked());
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn push_lock_allows_shared_readers_then_excludes_them_for_writer() {
        let lock = Arc::new(PushLock::new());
        let barrier = Arc::new(Barrier::new(3));
        let mut readers = Vec::new();
        for _ in 0..2 {
            let lock = Arc::clone(&lock);
            let barrier = Arc::clone(&barrier);
            readers.push(thread::spawn(move || {
                let _guard = lock.lock_shared();
                barrier.wait();
                barrier.wait();
            }));
        }
        barrier.wait();
        assert_eq!(lock.reader_count(), 2);
        assert!(!lock.is_exclusive());
        barrier.wait();
        for reader in readers {
            reader.join().unwrap();
        }
        {
            let _writer = lock.lock_exclusive();
            assert!(lock.is_exclusive());
            assert_eq!(lock.reader_count(), 0);
            assert!(lock.try_lock_exclusive().is_none());
        }
        assert!(!lock.is_exclusive());
        assert!(lock.try_lock_exclusive().is_some());
    }
}
