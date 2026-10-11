//! Host-testable dispatcher scheduler: priority bands, FIFO queues, quanta, affinity,
//! bounded priority donation, dynamic boosts, starvation protection, and load balancing.
//!
//! The scheduler owns no hardware state. A kernel supplies the saved CPU contexts and calls
//! [`Scheduler::on_tick`] from its clock path; host tests drive exactly the same transitions.
//! Ready queues are intrusive and fixed-capacity: there is no allocation on a scheduling path.

use crate::{JobId, ThreadId};

/// Number of dispatcher priority levels.
pub const PRIORITY_LEVELS: u8 = 32;
/// Highest priority in the idle band. Only an idle thread may use it.
pub const IDLE_PRIORITY: u8 = 0;
/// Lowest priority in the dynamic band.
pub const DYNAMIC_PRIORITY_MIN: u8 = 1;
/// Highest priority in the dynamic band.
pub const DYNAMIC_PRIORITY_MAX: u8 = 15;
/// Lowest priority in the real-time band.
pub const REALTIME_PRIORITY_MIN: u8 = 16;
/// Highest priority in the real-time band.
pub const REALTIME_PRIORITY_MAX: u8 = 31;
/// A dynamic thread waiting this many ticks receives a one-level aging boost.
pub const STARVATION_BOOST_INTERVAL: u32 = 16;
/// Default quantum for a time-sliced thread, in scheduler ticks.
pub const DEFAULT_QUANTUM: u32 = 20;
const JOB_RUNTIME_SCALE: u64 = 100_000;

/// A scheduling policy with its POSIX value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// `SCHED_OTHER`, using dynamic priorities and a time quantum.
    Other,
    /// `SCHED_FIFO`, which runs until it blocks or yields.
    Fifo,
    /// `SCHED_RR`, which rotates within an equal-priority real-time band.
    RoundRobin,
    /// `SCHED_SPORADIC`, a real-time server with a replenishment policy.
    Sporadic,
}

impl Policy {
    /// The POSIX `sched_*` policy number.
    #[must_use]
    pub const fn as_posix(self) -> i32 {
        match self {
            Self::Other => 0,
            Self::Fifo => 1,
            Self::RoundRobin => 2,
            Self::Sporadic => 3,
        }
    }

    /// Parse a POSIX `sched_*` policy number.
    #[must_use]
    pub const fn from_posix(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Other),
            1 => Some(Self::Fifo),
            2 => Some(Self::RoundRobin),
            3 => Some(Self::Sporadic),
            _ => None,
        }
    }

    /// Whether the policy uses the real-time priority band.
    #[must_use]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Fifo | Self::RoundRobin | Self::Sporadic)
    }

    const fn time_sliced(self) -> bool {
        matches!(self, Self::Other | Self::RoundRobin | Self::Sporadic)
    }
}

/// Map a POSIX real-time priority (`0..=15`) to its internal level (`16..=31`).
#[must_use]
pub const fn to_internal_priority(policy: Policy, priority: i32) -> Option<u8> {
    if !policy.is_realtime() || priority < 0 || priority > 15 {
        return None;
    }
    Some(REALTIME_PRIORITY_MIN + priority as u8)
}

/// Map an internal real-time priority to the POSIX-visible value.
#[must_use]
pub const fn to_posix_priority(policy: Policy, priority: u8) -> Option<i32> {
    if !policy.is_realtime() || priority < REALTIME_PRIORITY_MIN || priority > REALTIME_PRIORITY_MAX
    {
        return None;
    }
    Some((priority - REALTIME_PRIORITY_MIN) as i32)
}

/// The POSIX-visible minimum and maximum priorities for a policy.
#[must_use]
pub const fn priority_range(policy: Policy) -> (i32, i32) {
    if policy.is_realtime() {
        (0, 15)
    } else {
        (0, 0)
    }
}

/// Convert POSIX `nice` in `-20..=19` to the dynamic band (`15..=1`).
#[must_use]
pub const fn nice_to_priority(nice: i8) -> Option<u8> {
    if nice < -20 || nice > 19 {
        return None;
    }
    // There are 39 nice steps but 14 intervals in the dynamic band. Integer rounding is
    // deliberate and monotonic; nice(0) maps to level 8.
    let scaled = (nice as i16 + 20) * 14;
    Some(DYNAMIC_PRIORITY_MAX - ((scaled + 19) / 39) as u8)
}

/// Why a scheduler operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerError {
    /// A zero-processor scheduler or a scheduler with more than 64 processors was requested.
    InvalidProcessorCount,
    /// No table slot is available for another thread.
    ThreadTableFull,
    /// The requested identity is already registered.
    DuplicateThread,
    /// No thread with that identity exists.
    UnknownThread,
    /// The identity names the CPU's idle thread, which cannot be removed or blocked.
    IdleThread,
    /// No table slot is available for another job group.
    JobTableFull,
    /// Job CPU shares must be nonzero.
    InvalidJobShares,
    /// A thread was given a priority outside its policy's band.
    InvalidPriority,
    /// A zero quantum was requested for a time-sliced policy.
    InvalidQuantum,
    /// The affinity mask is empty or names no online processor.
    InvalidAffinity,
    /// The state transition is not valid for the current thread state.
    InvalidState,
    /// The thread is already assigned to a job with an incompatible CPU mask.
    JobAffinityConflict,
}

/// State visible to the scheduler's clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// Eligible to run and linked in one processor's ready queue.
    Ready,
    /// Currently executing on one processor.
    Running,
    /// Waiting on a dispatcher object or an explicit wait condition.
    Waiting,
    /// Explicitly suspended and not eligible to run.
    Suspended,
    /// The thread has exited and can never be dispatched again.
    Terminated,
    /// An idle thread, selected only when its processor has no ready work.
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThreadRecord {
    id: ThreadId,
    policy: Policy,
    base_priority: u8,
    priority: u8,
    quantum: u32,
    remaining: u32,
    affinity: u64,
    job: Option<JobId>,
    state: ThreadState,
    cpu: u8,
    previous: Option<usize>,
    next: Option<usize>,
    wait_age: u32,
    dynamic_boost: u8,
    donated_priority: u8,
}

impl ThreadRecord {
    const EMPTY: Option<Self> = None;

    fn effective_priority(&self) -> u8 {
        self.base_priority
            .saturating_add(self.dynamic_boost)
            .max(self.donated_priority)
            .min(match self.policy {
                Policy::Other => DYNAMIC_PRIORITY_MAX,
                Policy::Fifo | Policy::RoundRobin | Policy::Sporadic => REALTIME_PRIORITY_MAX,
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JobSchedule {
    job: JobId,
    shares: u16,
    virtual_runtime: u64,
}

impl JobSchedule {
    const fn new(job: JobId) -> Self {
        Self {
            job,
            shares: 100,
            virtual_runtime: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadyQueue {
    heads: [Option<usize>; PRIORITY_LEVELS as usize],
    tails: [Option<usize>; PRIORITY_LEVELS as usize],
    bitmap: u32,
    length: usize,
}

impl ReadyQueue {
    const fn empty() -> Self {
        Self {
            heads: [None; PRIORITY_LEVELS as usize],
            tails: [None; PRIORITY_LEVELS as usize],
            bitmap: 0,
            length: 0,
        }
    }
}

/// Scheduling information returned without exposing the intrusive queue links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSnapshot {
    /// Thread identity.
    pub id: ThreadId,
    /// Policy.
    pub policy: Policy,
    /// Base priority before boosts and donation.
    pub base_priority: u8,
    /// Effective priority used by the ready queues.
    pub priority: u8,
    /// Configured quantum in ticks.
    pub quantum: u32,
    /// Remaining quantum in the current dispatch.
    pub remaining: u32,
    /// Allowed processor bitmask.
    pub affinity: u64,
    /// Optional process job membership.
    pub job: Option<JobId>,
    /// Current lifecycle state.
    pub state: ThreadState,
    /// Processor to which this thread is currently assigned.
    pub processor: u8,
    /// Dynamic-priority boost, excluding mutex donation.
    pub dynamic_boost: u8,
    /// Priority donated to this thread by a blocked mutex waiter.
    pub donated_priority: u8,
}

/// A fixed-capacity, per-processor priority scheduler.
///
/// Each priority level is a FIFO linked list represented by links in the thread records; a
/// bitmap makes the highest nonempty ready level a single leading-zero operation. `CPUS` is
/// the number of online processors represented by this instance and must be `1..=64`.
///
/// Job membership narrows a thread's affinity to the job's CPU mask. At equal priority, a
/// virtual-runtime selector shares dispatches between job groups according to configurable
/// CPU shares, rather than letting a job gain CPU simply by creating more threads. Ready work
/// is placed on the least-loaded allowed processor; an idle processor can steal eligible
/// queued work from the busiest same-NUMA-node sibling before looking outside its node.
pub struct Scheduler<const THREADS: usize, const CPUS: usize> {
    threads: [Option<ThreadRecord>; THREADS],
    queues: [ReadyQueue; CPUS],
    current: [Option<usize>; CPUS],
    idle: [Option<usize>; CPUS],
    online_mask: u64,
    cpu_nodes: [u16; CPUS],
    ticks: u64,
    job_masks: [Option<(JobId, u64)>; THREADS],
    job_schedules: [Option<JobSchedule>; THREADS],
    unassigned_virtual_runtime: u64,
}

impl<const THREADS: usize, const CPUS: usize> Scheduler<THREADS, CPUS> {
    /// An empty scheduler. `new` returns an error if `CPUS` is outside `1..=64`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            threads: [ThreadRecord::EMPTY; THREADS],
            queues: [ReadyQueue::empty(); CPUS],
            current: [None; CPUS],
            idle: [None; CPUS],
            online_mask: if CPUS >= 64 {
                u64::MAX
            } else {
                (1u64 << CPUS) - 1
            },
            cpu_nodes: [0; CPUS],
            ticks: 0,
            job_masks: [None; THREADS],
            job_schedules: [None; THREADS],
            unassigned_virtual_runtime: 0,
        }
    }

    /// Validate the compile-time processor and capacity configuration.
    pub fn validate(&self) -> Result<(), SchedulerError> {
        if CPUS == 0 || CPUS > 64 {
            Err(SchedulerError::InvalidProcessorCount)
        } else {
            Ok(())
        }
    }

    /// Number of ticks the scheduler has processed.
    #[must_use]
    pub const fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Set a processor's NUMA node before assigning runnable threads to it.
    pub fn set_processor_node(
        &mut self,
        processor: usize,
        node: u16,
    ) -> Result<(), SchedulerError> {
        if processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        self.cpu_nodes[processor] = node;
        Ok(())
    }

    /// Register a job's allowed-processor mask. Threads in the job inherit its mask.
    pub fn set_job_affinity(&mut self, job: JobId, mask: u64) -> Result<(), SchedulerError> {
        let mask = mask & self.online_mask;
        if mask == 0 {
            return Err(SchedulerError::InvalidAffinity);
        }
        let slot = self
            .job_masks
            .iter()
            .position(|entry| matches!(entry, Some((existing, _)) if *existing == job))
            .or_else(|| self.job_masks.iter().position(Option::is_none))
            .ok_or(SchedulerError::JobTableFull)?;
        self.ensure_job_schedule(job)?;
        for record in self
            .threads
            .iter()
            .flatten()
            .filter(|record| record.job == Some(job))
        {
            if record.affinity & mask == 0 {
                return Err(SchedulerError::JobAffinityConflict);
            }
        }
        self.job_masks[slot] = Some((job, mask));
        let mut indices = [None; THREADS];
        let mut count = 0;
        for (index, record) in self.threads.iter().enumerate() {
            if record.is_some_and(|record| record.job == Some(job)) {
                indices[count] = Some(index);
                count += 1;
            }
        }
        for index in indices.into_iter().flatten() {
            let allowed = self.threads[index].expect("record was found").affinity & mask;
            self.relocate(index, allowed)?;
        }
        Ok(())
    }

    /// Configure a job's relative CPU share when it competes at the same priority.
    ///
    /// Shares default to 100. Larger values accrue virtual runtime more slowly, so a job
    /// receives proportionally more dispatches than jobs with smaller shares.
    pub fn set_job_shares(&mut self, job: JobId, shares: u16) -> Result<(), SchedulerError> {
        if shares == 0 {
            return Err(SchedulerError::InvalidJobShares);
        }
        let slot = self.ensure_job_schedule(job)?;
        self.job_schedules[slot]
            .as_mut()
            .expect("job schedule was allocated")
            .shares = shares;
        Ok(())
    }

    /// Add a non-idle thread and place it in an allowed processor's ready queue.
    pub fn add_thread(
        &mut self,
        id: ThreadId,
        policy: Policy,
        priority: u8,
        quantum: u32,
        affinity: u64,
    ) -> Result<(), SchedulerError> {
        self.validate()?;
        if self.find_index(id).is_some() {
            return Err(SchedulerError::DuplicateThread);
        }
        if !Self::priority_valid(policy, priority) {
            return Err(SchedulerError::InvalidPriority);
        }
        if policy.time_sliced() && quantum == 0 {
            return Err(SchedulerError::InvalidQuantum);
        }
        let affinity = affinity & self.online_mask;
        if affinity == 0 {
            return Err(SchedulerError::InvalidAffinity);
        }
        let index = self
            .threads
            .iter()
            .position(Option::is_none)
            .ok_or(SchedulerError::ThreadTableFull)?;
        let record = ThreadRecord {
            id,
            policy,
            base_priority: priority,
            priority,
            quantum: quantum.max(1),
            remaining: quantum.max(1),
            affinity,
            job: None,
            state: ThreadState::Ready,
            cpu: 0,
            previous: None,
            next: None,
            wait_age: 0,
            dynamic_boost: 0,
            donated_priority: 0,
        };
        self.threads[index] = Some(record);
        if let Err(error) = self.place_in_ready_queue(index, affinity) {
            self.threads[index] = None;
            return Err(error);
        }
        Ok(())
    }

    /// Install the unique idle thread for one processor.
    ///
    /// The idle thread is not linked in a ready queue. It runs only when no higher-priority
    /// thread is eligible on that processor.
    pub fn add_idle_thread(
        &mut self,
        id: ThreadId,
        processor: usize,
    ) -> Result<(), SchedulerError> {
        self.validate()?;
        if processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        if self.find_index(id).is_some() {
            return Err(SchedulerError::DuplicateThread);
        }
        if self.idle[processor].is_some() {
            return Err(SchedulerError::InvalidState);
        }
        let index = self
            .threads
            .iter()
            .position(Option::is_none)
            .ok_or(SchedulerError::ThreadTableFull)?;
        self.threads[index] = Some(ThreadRecord {
            id,
            policy: Policy::Other,
            base_priority: IDLE_PRIORITY,
            priority: IDLE_PRIORITY,
            quantum: 1,
            remaining: 1,
            affinity: 1u64 << processor,
            job: None,
            state: ThreadState::Idle,
            cpu: processor as u8,
            previous: None,
            next: None,
            wait_age: 0,
            dynamic_boost: 0,
            donated_priority: 0,
        });
        self.idle[processor] = Some(index);
        self.current[processor] = Some(index);
        Ok(())
    }

    /// Attach a thread to an existing job and apply the job's affinity mask.
    pub fn join_job(&mut self, id: ThreadId, job: JobId) -> Result<(), SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        self.ensure_job_schedule(job)?;
        let mask = self
            .job_masks
            .iter()
            .find_map(|entry| match entry {
                Some((candidate, mask)) if *candidate == job => Some(*mask),
                _ => None,
            })
            .unwrap_or(self.online_mask);
        let record = self.threads[index].expect("index came from table");
        let allowed = record.affinity & mask;
        if allowed == 0 {
            return Err(SchedulerError::JobAffinityConflict);
        }
        self.threads[index]
            .as_mut()
            .expect("index came from table")
            .job = Some(job);
        if let Err(error) = self.relocate(index, allowed) {
            self.threads[index]
                .as_mut()
                .expect("index came from table")
                .job = None;
            return Err(error);
        }
        Ok(())
    }

    /// Current thread on a processor, including its idle thread if no work is ready.
    #[must_use]
    pub fn current(&self, processor: usize) -> Option<ThreadId> {
        self.current
            .get(processor)
            .and_then(|slot| *slot)
            .and_then(|index| self.threads[index].map(|record| record.id))
    }

    /// Snapshot a thread's scheduling state.
    #[must_use]
    pub fn snapshot(&self, id: ThreadId) -> Option<ThreadSnapshot> {
        let record = self.threads[self.find_index(id)?]?;
        Some(ThreadSnapshot {
            id: record.id,
            policy: record.policy,
            base_priority: record.base_priority,
            priority: record.priority,
            quantum: record.quantum,
            remaining: record.remaining,
            affinity: record.affinity,
            job: record.job,
            state: record.state,
            processor: record.cpu,
            dynamic_boost: record.dynamic_boost,
            donated_priority: record.donated_priority,
        })
    }

    /// Return one thread's effective priority, or `None` if it is unknown.
    #[must_use]
    pub fn effective_priority(&self, id: ThreadId) -> Option<u8> {
        let index = self.find_index(id)?;
        Some(self.threads[index]?.priority)
    }

    /// Iterate over every registered thread identity, including idle and terminated records.
    pub fn thread_ids(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.threads
            .iter()
            .filter_map(|record| record.map(|record| record.id))
    }

    /// Number of threads ready on one processor, excluding its running thread.
    #[must_use]
    pub fn ready_count(&self, processor: usize) -> usize {
        self.queues.get(processor).map_or(0, |queue| queue.length)
    }

    /// The ready thread selected after a clock tick.
    ///
    /// Time-sliced dynamic, round-robin, and sporadic threads rotate to the tail of their
    /// priority queue when their quantum expires. FIFO threads retain the processor until
    /// they block, yield, or a higher-priority thread becomes ready.
    pub fn on_tick(&mut self, processor: usize) -> Result<Option<ThreadId>, SchedulerError> {
        self.validate()?;
        if processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        self.ticks = self.ticks.saturating_add(1);
        self.age_waiting_threads();

        if let Some(index) = self.current[processor] {
            let record = self.threads[index].expect("current thread exists");
            if record.state != ThreadState::Idle {
                self.charge_job_runtime(index);
            }
            if record.state != ThreadState::Idle && record.policy.time_sliced() {
                let remaining = record.remaining.saturating_sub(1);
                self.threads[index]
                    .as_mut()
                    .expect("current thread exists")
                    .remaining = remaining;
                if remaining == 0 {
                    self.rotate_current(processor, index)?;
                }
            }
        }
        self.dispatch(processor)?;
        Ok(self.current(processor))
    }

    /// Select a higher-priority ready thread if one is eligible, otherwise keep the current.
    pub fn dispatch(&mut self, processor: usize) -> Result<Option<ThreadId>, SchedulerError> {
        if processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        let current = self.current[processor];
        let highest_ready = self.highest_ready_priority(processor);
        let should_preempt = match (current, highest_ready) {
            (Some(index), Some(priority)) => {
                let running = self.threads[index].expect("current thread exists");
                running.state == ThreadState::Idle || priority > running.priority
            }
            (Some(_), None) => false,
            (None, _) => true,
        };
        if should_preempt {
            if let Some(index) = current {
                let record = self.threads[index].expect("current thread exists");
                if record.state != ThreadState::Idle {
                    self.threads[index]
                        .as_mut()
                        .expect("current thread exists")
                        .state = ThreadState::Ready;
                    self.enqueue(index, processor)?;
                }
                self.current[processor] = None;
            }
            self.select_next(processor)?;
        } else if current.is_none() {
            self.select_next(processor)?;
        }
        Ok(self.current(processor))
    }

    /// Yield a running thread to the tail of its equal-priority ready queue.
    pub fn yield_thread(&mut self, id: ThreadId) -> Result<Option<ThreadId>, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("index came from table");
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        if record.state != ThreadState::Running {
            return Err(SchedulerError::InvalidState);
        }
        let processor = usize::from(record.cpu);
        self.threads[index]
            .as_mut()
            .expect("record exists")
            .remaining = record.quantum;
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Ready;
        self.current[processor] = None;
        self.enqueue(index, processor)?;
        self.select_next(processor)?;
        Ok(self.current(processor))
    }

    /// Block a running or ready thread while it waits on a dispatcher object.
    pub fn block(&mut self, id: ThreadId) -> Result<Option<ThreadId>, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        match record.state {
            ThreadState::Ready => self.dequeue(index)?,
            ThreadState::Running => self.current[usize::from(record.cpu)] = None,
            ThreadState::Waiting | ThreadState::Suspended | ThreadState::Terminated => {
                return Err(SchedulerError::InvalidState)
            }
            ThreadState::Idle => return Err(SchedulerError::IdleThread),
        }
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Waiting;
        let processor = usize::from(record.cpu);
        self.dispatch(processor)?;
        Ok(self.current(processor))
    }

    /// Wake a waiting thread, granting one temporary dynamic-priority boost.
    pub fn wake(&mut self, id: ThreadId) -> Result<Option<ThreadId>, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        if record.state != ThreadState::Waiting {
            return Err(SchedulerError::InvalidState);
        }
        {
            let record = self.threads[index].as_mut().expect("record exists");
            record.state = ThreadState::Ready;
            record.wait_age = 0;
            if record.policy == Policy::Other {
                record.dynamic_boost = record.dynamic_boost.saturating_add(1);
                record.priority = record.effective_priority();
            }
        }
        self.place_in_ready_queue(index, record.affinity)?;
        let processor = usize::from(self.threads[index].expect("record exists").cpu);
        self.dispatch(processor)?;
        Ok(self.current(processor))
    }

    /// Suspend a waiting or ready thread until [`Scheduler::resume`] is called.
    pub fn suspend(&mut self, id: ThreadId) -> Result<(), SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        match record.state {
            ThreadState::Ready => self.dequeue(index)?,
            ThreadState::Running => {
                let processor = usize::from(record.cpu);
                self.current[processor] = None;
            }
            ThreadState::Waiting => {}
            ThreadState::Suspended | ThreadState::Terminated => {
                return Err(SchedulerError::InvalidState)
            }
            ThreadState::Idle => return Err(SchedulerError::IdleThread),
        }
        let processor = usize::from(record.cpu);
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Suspended;
        self.dispatch(processor).map(|_| ())
    }

    /// Resume a suspended thread and make it ready.
    pub fn resume(&mut self, id: ThreadId) -> Result<Option<ThreadId>, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        if record.state != ThreadState::Suspended {
            return Err(SchedulerError::InvalidState);
        }
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Ready;
        self.place_in_ready_queue(index, record.affinity)?;
        let processor = usize::from(self.threads[index].expect("record exists").cpu);
        self.dispatch(processor)?;
        Ok(self.current(processor))
    }

    /// Terminate a non-idle thread and remove it from all scheduler queues.
    pub fn terminate(&mut self, id: ThreadId) -> Result<Option<ThreadId>, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        let processor = usize::from(record.cpu);
        match record.state {
            ThreadState::Ready => self.dequeue(index)?,
            ThreadState::Running => self.current[processor] = None,
            ThreadState::Waiting | ThreadState::Suspended => {}
            ThreadState::Terminated => return Err(SchedulerError::InvalidState),
            ThreadState::Idle => return Err(SchedulerError::IdleThread),
        }
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Terminated;
        self.threads[index]
            .as_mut()
            .expect("record exists")
            .previous = None;
        self.threads[index].as_mut().expect("record exists").next = None;
        self.dispatch(processor)?;
        Ok(self.current(processor))
    }

    /// Replace a dynamic thread's base priority using POSIX `nice` semantics.
    pub fn set_nice(&mut self, id: ThreadId, nice: i8) -> Result<u8, SchedulerError> {
        let base = nice_to_priority(nice).ok_or(SchedulerError::InvalidPriority)?;
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        if self.threads[index].expect("record exists").policy != Policy::Other {
            return Err(SchedulerError::InvalidPriority);
        }
        self.change_priority(index, base, 0, None)?;
        Ok(self.threads[index].expect("record exists").priority)
    }

    /// Set a thread's real-time priority while preserving the current POSIX policy.
    pub fn set_realtime_priority(
        &mut self,
        id: ThreadId,
        posix_priority: i32,
    ) -> Result<u8, SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let record = self.threads[index].expect("record exists");
        let priority = to_internal_priority(record.policy, posix_priority)
            .ok_or(SchedulerError::InvalidPriority)?;
        self.change_priority(index, priority, 0, None)?;
        Ok(self.threads[index].expect("record exists").priority)
    }

    /// Replace a thread's affinity mask, moving ready work if its current CPU is disallowed.
    pub fn set_affinity(&mut self, id: ThreadId, affinity: u64) -> Result<(), SchedulerError> {
        let index = self.find_index(id).ok_or(SchedulerError::UnknownThread)?;
        let job_mask = self.job_mask_for(self.threads[index].expect("record exists").job);
        let affinity = affinity & self.online_mask & job_mask;
        if affinity == 0 {
            return Err(SchedulerError::InvalidAffinity);
        }
        self.threads[index]
            .as_mut()
            .expect("record exists")
            .affinity = affinity;
        self.relocate(index, affinity)
    }

    /// Donate a waiter's effective priority to the thread that owns a contended mutex.
    ///
    /// Donation is bounded by the waiter's current effective priority and the owner's own
    /// policy band. Call [`Scheduler::clear_donation`] when the mutex is released.
    pub fn donate_priority(
        &mut self,
        owner: ThreadId,
        waiter: ThreadId,
    ) -> Result<u8, SchedulerError> {
        let owner_index = self
            .find_index(owner)
            .ok_or(SchedulerError::UnknownThread)?;
        let waiter_index = self
            .find_index(waiter)
            .ok_or(SchedulerError::UnknownThread)?;
        let waiter_priority = self.threads[waiter_index].expect("record exists").priority;
        let mut owner_record = self.threads[owner_index].expect("record exists");
        let max_priority = if owner_record.policy == Policy::Other {
            DYNAMIC_PRIORITY_MAX
        } else {
            REALTIME_PRIORITY_MAX
        };
        let donation = waiter_priority.min(max_priority);
        owner_record.donated_priority = owner_record.donated_priority.max(donation);
        self.threads[owner_index] = Some(owner_record);
        self.recompute_priority(owner_index)?;
        Ok(self.threads[owner_index].expect("record exists").priority)
    }

    /// Remove a thread's mutex priority donation, preserving its base and dynamic boosts.
    pub fn clear_donation(&mut self, owner: ThreadId) -> Result<u8, SchedulerError> {
        let index = self
            .find_index(owner)
            .ok_or(SchedulerError::UnknownThread)?;
        self.threads[index]
            .as_mut()
            .expect("record exists")
            .donated_priority = 0;
        self.recompute_priority(index)?;
        Ok(self.threads[index].expect("record exists").priority)
    }

    /// Pull one eligible ready thread onto an idle processor.
    ///
    /// The busiest eligible processor on the idle CPU's NUMA node is tried first. If no
    /// same-node queue has a stealable thread, the busiest eligible processor overall wins.
    pub fn balance_idle(
        &mut self,
        idle_processor: usize,
    ) -> Result<Option<ThreadId>, SchedulerError> {
        if idle_processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        if self.queues[idle_processor].length != 0 {
            return Ok(None);
        }
        if self.current[idle_processor].is_some_and(|index| {
            self.threads[index].is_some_and(|record| record.state != ThreadState::Idle)
        }) {
            return Ok(None);
        }
        let local_node = self.cpu_nodes[idle_processor];
        let mut source = self.busiest_eligible(idle_processor, Some(local_node));
        if source.is_none() {
            source = self.busiest_eligible(idle_processor, None);
        }
        let Some(source) = source else {
            return Ok(None);
        };
        let Some(index) = self.stealable_thread(source, idle_processor) else {
            return Ok(None);
        };
        self.dequeue(index)?;
        let record = self.threads[index].as_mut().expect("ready record exists");
        record.cpu = idle_processor as u8;
        record.state = ThreadState::Ready;
        self.enqueue(index, idle_processor)?;
        self.dispatch(idle_processor)
    }

    /// Number of online processors represented by this scheduler.
    #[must_use]
    pub const fn processor_count(&self) -> usize {
        CPUS
    }

    /// A read-only list of the ready thread identities at one priority on one CPU.
    ///
    /// This is intended for diagnostics and deterministic tests, not the schedule path.
    pub fn ready_order(
        &self,
        processor: usize,
        priority: u8,
    ) -> Result<ReadyOrder<'_, THREADS, CPUS>, SchedulerError> {
        if processor >= CPUS || priority >= PRIORITY_LEVELS {
            return Err(SchedulerError::InvalidPriority);
        }
        Ok(ReadyOrder {
            scheduler: self,
            next: self.queues[processor].heads[usize::from(priority)],
        })
    }

    fn priority_valid(policy: Policy, priority: u8) -> bool {
        match policy {
            Policy::Other => (DYNAMIC_PRIORITY_MIN..=DYNAMIC_PRIORITY_MAX).contains(&priority),
            Policy::Fifo | Policy::RoundRobin | Policy::Sporadic => {
                (REALTIME_PRIORITY_MIN..=REALTIME_PRIORITY_MAX).contains(&priority)
            }
        }
    }

    fn find_index(&self, id: ThreadId) -> Option<usize> {
        self.threads
            .iter()
            .position(|record| record.is_some_and(|record| record.id == id))
    }

    fn enqueue(&mut self, index: usize, processor: usize) -> Result<(), SchedulerError> {
        let record = self.threads[index].ok_or(SchedulerError::UnknownThread)?;
        let level = usize::from(record.priority);
        if processor >= CPUS || level >= PRIORITY_LEVELS as usize {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        let tail = self.queues[processor].tails[level];
        {
            let record = self.threads[index].as_mut().expect("record exists");
            record.cpu = processor as u8;
            record.previous = tail;
            record.next = None;
        }
        if let Some(tail_index) = tail {
            self.threads[tail_index].as_mut().expect("tail exists").next = Some(index);
        } else {
            self.queues[processor].heads[level] = Some(index);
        }
        self.queues[processor].tails[level] = Some(index);
        self.queues[processor].bitmap |= 1u32 << level;
        self.queues[processor].length += 1;
        Ok(())
    }

    fn dequeue(&mut self, index: usize) -> Result<(), SchedulerError> {
        let record = self.threads[index].ok_or(SchedulerError::UnknownThread)?;
        if record.state != ThreadState::Ready {
            return Err(SchedulerError::InvalidState);
        }
        let processor = usize::from(record.cpu);
        let level = usize::from(record.priority);
        if processor >= CPUS {
            return Err(SchedulerError::InvalidProcessorCount);
        }
        if let Some(previous) = record.previous {
            self.threads[previous]
                .as_mut()
                .expect("previous exists")
                .next = record.next;
        } else {
            self.queues[processor].heads[level] = record.next;
        }
        if let Some(next) = record.next {
            self.threads[next].as_mut().expect("next exists").previous = record.previous;
        } else {
            self.queues[processor].tails[level] = record.previous;
        }
        if self.queues[processor].heads[level].is_none() {
            self.queues[processor].bitmap &= !(1u32 << level);
        }
        self.queues[processor].length -= 1;
        let record = self.threads[index].as_mut().expect("record exists");
        record.previous = None;
        record.next = None;
        Ok(())
    }

    fn highest_ready_priority(&self, processor: usize) -> Option<u8> {
        let bitmap = self.queues.get(processor)?.bitmap;
        if bitmap == 0 {
            None
        } else {
            Some((u32::BITS - 1 - bitmap.leading_zeros()) as u8)
        }
    }

    fn pop_highest(&mut self, processor: usize) -> Result<Option<usize>, SchedulerError> {
        let Some(priority) = self.highest_ready_priority(processor) else {
            return Ok(None);
        };
        let level = usize::from(priority);
        let mut selected =
            self.queues[processor].heads[level].ok_or(SchedulerError::InvalidState)?;
        let mut selected_runtime =
            self.group_virtual_runtime(self.threads[selected].expect("ready record exists").job);
        let mut cursor = self.threads[selected].expect("ready record exists").next;
        while let Some(index) = cursor {
            let record = self.threads[index].expect("queued record exists");
            let runtime = self.group_virtual_runtime(record.job);
            if runtime < selected_runtime {
                selected = index;
                selected_runtime = runtime;
            }
            cursor = record.next;
        }
        self.dequeue(selected)?;
        Ok(Some(selected))
    }

    fn select_next(&mut self, processor: usize) -> Result<(), SchedulerError> {
        if self.current[processor].is_some() {
            return Ok(());
        }
        let selected = self.pop_highest(processor)?.or(self.idle[processor]);
        if let Some(index) = selected {
            let record = self.threads[index]
                .as_mut()
                .expect("selected record exists");
            if record.state == ThreadState::Idle {
                record.cpu = processor as u8;
            } else {
                record.state = ThreadState::Running;
                record.wait_age = 0;
                if record.remaining == 0 {
                    record.remaining = record.quantum;
                }
            }
            self.current[processor] = Some(index);
        }
        Ok(())
    }

    fn rotate_current(&mut self, processor: usize, index: usize) -> Result<(), SchedulerError> {
        let record = self.threads[index].expect("current record exists");
        if record.state == ThreadState::Idle {
            return Ok(());
        }
        if record.policy == Policy::Other {
            let current = self.threads[index].as_mut().expect("record exists");
            current.dynamic_boost = current.dynamic_boost.saturating_sub(1);
        }
        let next_priority = self.threads[index]
            .expect("record exists")
            .effective_priority();
        let current = self.threads[index].as_mut().expect("record exists");
        current.priority = next_priority;
        current.remaining = record.quantum;
        current.state = ThreadState::Ready;
        self.current[processor] = None;
        self.enqueue(index, processor)
    }

    fn place_in_ready_queue(&mut self, index: usize, affinity: u64) -> Result<(), SchedulerError> {
        let allowed = affinity & self.job_mask_for(self.threads[index].and_then(|r| r.job));
        if allowed == 0 {
            return Err(SchedulerError::InvalidAffinity);
        }
        let mut best: Option<(usize, usize)> = None;
        for processor in 0..CPUS {
            if allowed & (1u64 << processor) == 0 {
                continue;
            }
            let load = self.queues[processor].length
                + usize::from(self.current[processor].is_some_and(|current| {
                    self.threads[current].is_some_and(|record| record.state != ThreadState::Idle)
                }));
            if best.is_none_or(|(_, best_load)| load < best_load) {
                best = Some((processor, load));
            }
        }
        let (processor, _) = best.ok_or(SchedulerError::InvalidAffinity)?;
        self.threads[index].as_mut().expect("record exists").state = ThreadState::Ready;
        self.enqueue(index, processor)
    }

    fn relocate(&mut self, index: usize, allowed: u64) -> Result<(), SchedulerError> {
        let record = self.threads[index].ok_or(SchedulerError::UnknownThread)?;
        match record.state {
            ThreadState::Ready => {
                let old_processor = usize::from(record.cpu);
                let target_is_allowed = allowed & (1u64 << old_processor) != 0;
                if target_is_allowed {
                    return Ok(());
                }
                self.dequeue(index)?;
                self.place_in_ready_queue(index, allowed)
            }
            ThreadState::Running => {
                let old_processor = usize::from(record.cpu);
                if allowed & (1u64 << old_processor) != 0 {
                    return Ok(());
                }
                self.current[old_processor] = None;
                self.threads[index].as_mut().expect("record exists").state = ThreadState::Ready;
                self.place_in_ready_queue(index, allowed)?;
                let new_processor = usize::from(self.threads[index].expect("record exists").cpu);
                self.dispatch(old_processor)?;
                self.dispatch(new_processor).map(|_| ())
            }
            ThreadState::Idle => Err(SchedulerError::IdleThread),
            ThreadState::Waiting | ThreadState::Suspended | ThreadState::Terminated => {
                if allowed & (1u64 << usize::from(record.cpu)) != 0 {
                    Ok(())
                } else {
                    let processor = self.choose_processor(allowed)?;
                    self.threads[index].as_mut().expect("record exists").cpu = processor as u8;
                    Ok(())
                }
            }
        }
    }

    fn choose_processor(&self, allowed: u64) -> Result<usize, SchedulerError> {
        let mut best: Option<(usize, usize)> = None;
        for processor in 0..CPUS {
            if allowed & (1u64 << processor) == 0 {
                continue;
            }
            let load = self.queues[processor].length
                + usize::from(self.current[processor].is_some_and(|index| {
                    self.threads[index].is_some_and(|record| record.state != ThreadState::Idle)
                }));
            if best.is_none_or(|(_, best_load)| load < best_load) {
                best = Some((processor, load));
            }
        }
        best.map(|(processor, _)| processor)
            .ok_or(SchedulerError::InvalidAffinity)
    }

    fn ensure_job_schedule(&mut self, job: JobId) -> Result<usize, SchedulerError> {
        if let Some(slot) = self
            .job_schedules
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.job == job))
        {
            return Ok(slot);
        }
        let slot = self
            .job_schedules
            .iter()
            .position(Option::is_none)
            .ok_or(SchedulerError::JobTableFull)?;
        self.job_schedules[slot] = Some(JobSchedule::new(job));
        Ok(slot)
    }

    fn group_virtual_runtime(&self, job: Option<JobId>) -> u64 {
        match job {
            Some(job) => self
                .job_schedules
                .iter()
                .flatten()
                .find(|schedule| schedule.job == job)
                .map_or(0, |schedule| schedule.virtual_runtime),
            None => self.unassigned_virtual_runtime,
        }
    }

    fn charge_job_runtime(&mut self, index: usize) {
        let record = self.threads[index].expect("running record exists");
        let Some(job) = record.job else {
            self.unassigned_virtual_runtime = self
                .unassigned_virtual_runtime
                .saturating_add(JOB_RUNTIME_SCALE / 100);
            return;
        };
        if let Some(schedule) = self
            .job_schedules
            .iter_mut()
            .flatten()
            .find(|schedule| schedule.job == job)
        {
            let shares = u64::from(schedule.shares);
            let charge = JOB_RUNTIME_SCALE.saturating_add(shares - 1) / shares;
            schedule.virtual_runtime = schedule.virtual_runtime.saturating_add(charge.max(1));
        }
    }

    fn job_mask_for(&self, job: Option<JobId>) -> u64 {
        let Some(job) = job else {
            return self.online_mask;
        };
        self.job_masks
            .iter()
            .find_map(|entry| match entry {
                Some((candidate, mask)) if *candidate == job => Some(*mask),
                _ => None,
            })
            .unwrap_or(self.online_mask)
    }

    fn change_priority(
        &mut self,
        index: usize,
        base_priority: u8,
        dynamic_boost: u8,
        donated_priority: Option<u8>,
    ) -> Result<(), SchedulerError> {
        let record = self.threads[index].ok_or(SchedulerError::UnknownThread)?;
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        let was_ready = record.state == ThreadState::Ready;
        if was_ready {
            self.dequeue(index)?;
        }
        {
            let record = self.threads[index].as_mut().expect("record exists");
            record.base_priority = base_priority;
            record.dynamic_boost = dynamic_boost;
            if let Some(donated) = donated_priority {
                record.donated_priority = donated;
            }
            record.priority = record.effective_priority();
        }
        if was_ready {
            let affinity = self.threads[index].expect("record exists").affinity;
            self.place_in_ready_queue(index, affinity)?;
        }
        let processor = usize::from(self.threads[index].expect("record exists").cpu);
        self.dispatch(processor).map(|_| ())
    }

    fn recompute_priority(&mut self, index: usize) -> Result<(), SchedulerError> {
        let record = self.threads[index].ok_or(SchedulerError::UnknownThread)?;
        if record.state == ThreadState::Idle {
            return Err(SchedulerError::IdleThread);
        }
        let was_ready = record.state == ThreadState::Ready;
        if was_ready {
            self.dequeue(index)?;
        }
        self.threads[index]
            .as_mut()
            .expect("record exists")
            .priority = record.effective_priority();
        if was_ready {
            self.enqueue(index, usize::from(record.cpu))?;
        }
        self.dispatch(usize::from(record.cpu)).map(|_| ())
    }

    fn age_waiting_threads(&mut self) {
        let aging_tick = self.ticks % u64::from(STARVATION_BOOST_INTERVAL) == 0;
        let mut promotions = [None; THREADS];
        let mut promotion_count = 0;
        for (index, slot) in self.threads.iter_mut().enumerate() {
            let Some(record) = slot else { continue };
            if record.state == ThreadState::Ready && record.policy == Policy::Other {
                record.wait_age = record.wait_age.saturating_add(1);
                if aging_tick && record.wait_age >= STARVATION_BOOST_INTERVAL {
                    record.dynamic_boost = record.dynamic_boost.saturating_add(1);
                    promotions[promotion_count] = Some(index);
                    promotion_count += 1;
                    record.wait_age = 0;
                }
            }
        }
        // Keep existing queue order intact: unlink using the old priority, then append each
        // promoted thread to the tail of its new effective-priority queue.
        for index in promotions.into_iter().flatten() {
            let old = self.threads[index].expect("promoted record exists");
            self.dequeue(index)
                .expect("a promoted ready thread is linked in its queue");
            let next_priority = self.threads[index]
                .expect("promoted record exists")
                .effective_priority();
            self.threads[index]
                .as_mut()
                .expect("record exists")
                .priority = next_priority;
            self.enqueue(index, usize::from(old.cpu))
                .expect("an existing processor remains valid");
        }
    }

    fn busiest_eligible(&self, destination: usize, node: Option<u16>) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for source in 0..CPUS {
            if source == destination || self.queues[source].length == 0 {
                continue;
            }
            if node.is_some_and(|node| self.cpu_nodes[source] != node) {
                continue;
            }
            if self.stealable_thread(source, destination).is_none() {
                continue;
            }
            let load = self.queues[source].length
                + usize::from(self.current[source].is_some_and(|index| {
                    self.threads[index].is_some_and(|record| record.state != ThreadState::Idle)
                }));
            if best.is_none_or(|(_, best_load)| load > best_load) {
                best = Some((source, load));
            }
        }
        best.map(|(source, _)| source)
    }

    fn stealable_thread(&self, source: usize, destination: usize) -> Option<usize> {
        for level in (1..PRIORITY_LEVELS).rev() {
            let mut cursor = self.queues[source].tails[level as usize];
            while let Some(index) = cursor {
                let record = self.threads[index]?;
                if record.affinity & (1u64 << destination) != 0 {
                    return Some(index);
                }
                cursor = record.previous;
            }
        }
        None
    }
}

impl<const THREADS: usize, const CPUS: usize> Default for Scheduler<THREADS, CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over one ready queue's FIFO order.
pub struct ReadyOrder<'a, const THREADS: usize, const CPUS: usize> {
    scheduler: &'a Scheduler<THREADS, CPUS>,
    next: Option<usize>,
}

impl<const THREADS: usize, const CPUS: usize> Iterator for ReadyOrder<'_, THREADS, CPUS> {
    type Item = ThreadId;

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.next?;
        let record = self.scheduler.threads[index]?;
        self.next = record.next;
        Some(record.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn id(value: u64) -> ThreadId {
        ThreadId::new(value)
    }

    #[test]
    fn priority_mapping_covers_the_posix_real_time_band_and_nice_range() {
        assert_eq!(to_internal_priority(Policy::Fifo, 0), Some(16));
        assert_eq!(to_internal_priority(Policy::RoundRobin, 15), Some(31));
        assert_eq!(to_internal_priority(Policy::Other, 1), None);
        assert_eq!(to_internal_priority(Policy::Fifo, 16), None);
        assert_eq!(to_posix_priority(Policy::Fifo, 31), Some(15));
        assert_eq!(priority_range(Policy::Fifo), (0, 15));
        assert_eq!(priority_range(Policy::Other), (0, 0));
        assert_eq!(nice_to_priority(-20), Some(15));
        assert_eq!(nice_to_priority(0), Some(8));
        assert_eq!(nice_to_priority(19), Some(1));
        assert_eq!(nice_to_priority(20), None);
    }

    #[test]
    fn fifo_within_priority_and_round_robin_across_quantums() {
        let mut scheduler = Scheduler::<4, 1>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        for thread in 2..=4 {
            scheduler
                .add_thread(id(thread), Policy::Other, 8, 1, 1)
                .unwrap();
        }
        assert_eq!(scheduler.current(0), Some(id(1)));
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(2))));
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(3))));
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(4))));
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(2))));
        assert_eq!(
            scheduler.ready_order(0, 8).unwrap().collect::<Vec<_>>(),
            vec![id(3), id(4)]
        );
    }

    #[test]
    fn fifo_policy_does_not_time_slice_but_higher_priority_preempts() {
        let mut scheduler = Scheduler::<3, 1>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler.add_thread(id(2), Policy::Fifo, 18, 1, 1).unwrap();
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(2))));
        scheduler.add_thread(id(3), Policy::Fifo, 19, 1, 1).unwrap();
        assert_eq!(scheduler.dispatch(0), Ok(Some(id(3))));
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(3))));
        assert_eq!(scheduler.snapshot(id(2)).unwrap().state, ThreadState::Ready);
        scheduler.block(id(3)).unwrap();
        assert_eq!(scheduler.current(0), Some(id(2)));
    }

    #[test]
    fn mutex_priority_donation_bounds_inversion_then_clears() {
        let mut scheduler = Scheduler::<4, 1>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler
            .add_thread(id(2), Policy::Other, 2, 20, 1)
            .unwrap();
        scheduler
            .add_thread(id(3), Policy::Fifo, 30, 10, 1)
            .unwrap();
        scheduler.block(id(3)).unwrap();
        assert_eq!(scheduler.current(0), Some(id(2)));
        assert_eq!(scheduler.donate_priority(id(2), id(3)), Ok(15));
        assert_eq!(scheduler.snapshot(id(2)).unwrap().priority, 15);
        assert_eq!(scheduler.clear_donation(id(2)), Ok(2));
        assert_eq!(scheduler.snapshot(id(2)).unwrap().priority, 2);
    }

    #[test]
    fn starvation_aging_eventually_promotes_a_dynamic_thread_to_the_busy_band() {
        let mut scheduler = Scheduler::<3, 1>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler.add_thread(id(2), Policy::Other, 1, 1, 1).unwrap();
        scheduler
            .add_thread(id(3), Policy::Other, 15, 1, 1)
            .unwrap();
        let mut ran_low = false;
        for _ in 0..500 {
            let current = scheduler.on_tick(0).unwrap().unwrap();
            if current == id(2) {
                ran_low = true;
                break;
            }
        }
        assert!(ran_low, "the low-priority thread starved indefinitely");
    }

    #[test]
    fn affinity_and_idle_load_balance_respect_processor_and_numa_constraints() {
        let mut scheduler = Scheduler::<5, 3>::new();
        scheduler.set_processor_node(0, 0).unwrap();
        scheduler.set_processor_node(1, 0).unwrap();
        scheduler.set_processor_node(2, 1).unwrap();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler.add_idle_thread(id(2), 1).unwrap();
        scheduler.add_idle_thread(id(3), 2).unwrap();
        scheduler
            .add_thread(id(4), Policy::Other, 9, 5, 0b011)
            .unwrap();
        scheduler
            .add_thread(id(5), Policy::Other, 8, 5, 0b001)
            .unwrap();
        assert_eq!(scheduler.snapshot(id(5)).unwrap().processor, 0);
        assert_eq!(scheduler.balance_idle(1).unwrap(), Some(id(4)));
        assert_eq!(scheduler.snapshot(id(4)).unwrap().processor, 1);
        assert_eq!(
            scheduler.set_affinity(id(4), 0b1000),
            Err(SchedulerError::InvalidAffinity)
        );
        scheduler.set_affinity(id(4), 0b010).unwrap();
        assert_eq!(scheduler.snapshot(id(4)).unwrap().processor, 1);
    }

    #[test]
    fn job_affinity_is_intersected_with_thread_affinity() {
        let mut scheduler = Scheduler::<3, 2>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler.add_idle_thread(id(2), 1).unwrap();
        scheduler
            .add_thread(id(3), Policy::Other, 8, 10, 0b11)
            .unwrap();
        let job = JobId::new(42);
        scheduler.set_job_affinity(job, 0b10).unwrap();
        scheduler.join_job(id(3), job).unwrap();
        assert_eq!(scheduler.snapshot(id(3)).unwrap().job, Some(job));
        assert_eq!(scheduler.snapshot(id(3)).unwrap().processor, 1);
        assert_eq!(
            scheduler.set_affinity(id(3), 0b01),
            Err(SchedulerError::InvalidAffinity)
        );
    }

    #[test]
    fn equal_priority_jobs_receive_group_shares_independent_of_thread_count() {
        let mut scheduler = Scheduler::<5, 1>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        let group_a = JobId::new(100);
        let group_b = JobId::new(200);
        scheduler.set_job_shares(group_a, 100).unwrap();
        scheduler.set_job_shares(group_b, 100).unwrap();
        scheduler
            .add_thread(id(2), Policy::RoundRobin, 20, 1, 1)
            .unwrap();
        scheduler
            .add_thread(id(3), Policy::RoundRobin, 20, 1, 1)
            .unwrap();
        scheduler
            .add_thread(id(4), Policy::RoundRobin, 20, 1, 1)
            .unwrap();
        scheduler.join_job(id(2), group_a).unwrap();
        scheduler.join_job(id(3), group_a).unwrap();
        scheduler.join_job(id(4), group_b).unwrap();

        let first_six = (0..6)
            .map(|_| scheduler.on_tick(0).unwrap().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(first_six, vec![id(2), id(4), id(3), id(4), id(2), id(4)]);

        scheduler.set_job_shares(group_a, 200).unwrap();
        assert_eq!(
            scheduler.set_job_shares(group_b, 0),
            Err(SchedulerError::InvalidJobShares)
        );
        let mut group_a_dispatches = 0;
        let mut group_b_dispatches = 0;
        for _ in 0..90 {
            match scheduler.on_tick(0).unwrap().unwrap() {
                thread if thread == id(2) || thread == id(3) => group_a_dispatches += 1,
                thread if thread == id(4) => group_b_dispatches += 1,
                thread => panic!("unexpected ready thread {thread:?}"),
            }
        }
        assert!(group_a_dispatches > group_b_dispatches);
    }

    #[test]
    fn running_thread_cannot_escape_its_affinity_mask() {
        let mut scheduler = Scheduler::<3, 2>::new();
        scheduler.add_idle_thread(id(1), 0).unwrap();
        scheduler.add_idle_thread(id(2), 1).unwrap();
        scheduler
            .add_thread(id(3), Policy::Other, 12, 20, 0b11)
            .unwrap();
        assert_eq!(scheduler.on_tick(0), Ok(Some(id(3))));
        scheduler.set_affinity(id(3), 0b10).unwrap();
        assert_eq!(scheduler.snapshot(id(3)).unwrap().processor, 1);
        assert_eq!(scheduler.current(0), Some(id(1)));
        assert_eq!(scheduler.current(1), Some(id(3)));
    }
}
