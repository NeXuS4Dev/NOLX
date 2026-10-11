//! Bounded deferred procedure and system-worker queues.
//!
//! DPC callbacks run at dispatch-level semantics: they must be short, nonblocking, and must
//! not allocate from paged memory. System-work callbacks use a separate passive-level queue so
//! deferred work that may block is never executed in an interrupt/DPC path.

/// A routine and opaque value captured for later execution.
#[derive(Clone, Copy)]
pub struct DeferredCall {
    /// The routine invoked by the queue's consumer.
    pub routine: fn(usize),
    /// Opaque routine argument.
    pub context: usize,
}

impl core::fmt::Debug for DeferredCall {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeferredCall")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl PartialEq for DeferredCall {
    fn eq(&self, other: &Self) -> bool {
        self.routine as usize == other.routine as usize && self.context == other.context
    }
}

impl Eq for DeferredCall {}

/// Why a deferred routine could not be queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The fixed-capacity queue has no free entry.
    Full,
}

/// Fixed-capacity FIFO for work that will execute later at a defined IPL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredQueue<const CAPACITY: usize> {
    entries: [Option<DeferredCall>; CAPACITY],
    head: usize,
    length: usize,
}

impl<const CAPACITY: usize> DeferredQueue<CAPACITY> {
    /// Construct an empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [None; CAPACITY],
            head: 0,
            length: 0,
        }
    }

    /// Append one routine at the tail, returning an error rather than dropping work.
    pub fn queue(&mut self, call: DeferredCall) -> Result<(), QueueError> {
        if self.length == CAPACITY {
            return Err(QueueError::Full);
        }
        let tail = (self.head + self.length) % CAPACITY;
        self.entries[tail] = Some(call);
        self.length += 1;
        Ok(())
    }

    /// Remove and return the oldest routine without executing it.
    pub fn pop(&mut self) -> Option<DeferredCall> {
        if self.length == 0 {
            return None;
        }
        let call = self.entries[self.head].take();
        self.head = if CAPACITY == 0 {
            0
        } else {
            (self.head + 1) % CAPACITY
        };
        self.length -= 1;
        call
    }

    /// Run at most `limit` queued routines and return the number dispatched.
    pub fn drain(&mut self, limit: usize) -> usize {
        let mut dispatched = 0;
        while dispatched < limit {
            let Some(call) = self.pop() else { break };
            (call.routine)(call.context);
            dispatched += 1;
        }
        dispatched
    }

    /// Number of calls waiting in the queue.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether the queue is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Whether the queue has reached its fixed capacity.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.length == CAPACITY
    }
}

impl<const CAPACITY: usize> Default for DeferredQueue<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-processor deferred procedure queue, drained at dispatch IPL.
pub type DpcQueue<const CAPACITY: usize> = DeferredQueue<CAPACITY>;

/// Passive-level system-worker queue for routines that may block.
pub type SystemWorkQueue<const CAPACITY: usize> = DeferredQueue<CAPACITY>;

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    // One accumulator per test: a single shared counter would make the two tests race when
    // the harness runs them in parallel, which is a test bug rather than a queue bug.
    static DPC_SUM: AtomicUsize = AtomicUsize::new(0);
    static WORKER_SUM: AtomicUsize = AtomicUsize::new(0);

    fn add_dpc(value: usize) {
        DPC_SUM.fetch_add(value, Ordering::Relaxed);
    }

    fn add_worker(value: usize) {
        WORKER_SUM.fetch_add(value, Ordering::Relaxed);
    }

    #[test]
    fn dpcs_are_fifo_bounded_and_drained_in_dispatch_sized_batches() {
        DPC_SUM.store(0, Ordering::Relaxed);
        let mut dpcs = DpcQueue::<3>::new();
        for value in 1..=3 {
            dpcs.queue(DeferredCall {
                routine: add_dpc,
                context: value,
            })
            .unwrap();
        }
        assert_eq!(
            dpcs.queue(DeferredCall {
                routine: add_dpc,
                context: 4
            }),
            Err(QueueError::Full)
        );
        assert_eq!(dpcs.drain(2), 2);
        assert_eq!(DPC_SUM.load(Ordering::Relaxed), 3);
        assert_eq!(dpcs.len(), 1);
        assert_eq!(dpcs.drain(8), 1);
        assert_eq!(DPC_SUM.load(Ordering::Relaxed), 6);
        assert!(dpcs.is_empty());
    }

    #[test]
    fn worker_queue_has_independent_capacity_and_execution_context() {
        WORKER_SUM.store(0, Ordering::Relaxed);
        let mut workers = SystemWorkQueue::<2>::new();
        workers
            .queue(DeferredCall {
                routine: add_worker,
                context: 10,
            })
            .unwrap();
        workers
            .queue(DeferredCall {
                routine: add_worker,
                context: 20,
            })
            .unwrap();
        assert_eq!(workers.drain(1), 1);
        assert_eq!(WORKER_SUM.load(Ordering::Relaxed), 10);
        assert_eq!(workers.pop().unwrap().context, 20);
        assert!(workers.is_empty());
    }
}
