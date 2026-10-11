//! Integrated host model joining the scheduler and dispatcher wait transitions.
//!
//! The dispatcher determines when a wait is satisfied; this coordinator blocks and wakes the
//! corresponding scheduler thread, retains the completion status, and recomputes mutex priority
//! donation. It is the behavior boundary the kernel Executive uses when its hardware wait path
//! is connected to real thread contexts.

use crate::dispatcher::{
    DispatchError, Dispatcher, DispatcherObject, WaitStart, WaitStatus, WaitType,
};
use crate::scheduler::{Policy, Scheduler, SchedulerError, ThreadSnapshot};
use crate::{JobId, ThreadId};

/// Why an integrated scheduler/dispatcher operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutiveError {
    /// A dispatcher object or wait operation was invalid.
    Dispatcher(DispatchError),
    /// A thread could not make the corresponding scheduler transition.
    Scheduler(SchedulerError),
    /// A thread already has a completed wait result it has not collected.
    UncollectedWaitResult,
    /// The bounded completion table could not retain a woken thread's result.
    CompletionTableFull,
}

/// One bounded model of the Executive scheduler and waitable-object subsystem.
///
/// `THREADS` bounds both scheduler records and retained wait results; `CPUS` is the online CPU
/// count (1..=64); `OBJECTS` and `WAITERS` bound dispatcher object and wait tables.
pub struct ExecutiveCore<
    const THREADS: usize,
    const CPUS: usize,
    const OBJECTS: usize,
    const WAITERS: usize,
> {
    scheduler: Scheduler<THREADS, CPUS>,
    dispatcher: Dispatcher<OBJECTS, WAITERS>,
    completions: [Option<(ThreadId, WaitStatus)>; THREADS],
}

impl<const THREADS: usize, const CPUS: usize, const OBJECTS: usize, const WAITERS: usize>
    ExecutiveCore<THREADS, CPUS, OBJECTS, WAITERS>
{
    /// An empty integrated Executive model.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scheduler: Scheduler::new(),
            dispatcher: Dispatcher::new(),
            completions: [None; THREADS],
        }
    }

    /// Borrow the scheduler for policy configuration and diagnostics.
    #[must_use]
    pub const fn scheduler(&self) -> &Scheduler<THREADS, CPUS> {
        &self.scheduler
    }

    /// Mutably borrow the scheduler for the kernel's architecture switch bridge.
    #[must_use]
    pub fn scheduler_mut(&mut self) -> &mut Scheduler<THREADS, CPUS> {
        &mut self.scheduler
    }

    /// Borrow the dispatcher for object inspection.
    #[must_use]
    pub const fn dispatcher(&self) -> &Dispatcher<OBJECTS, WAITERS> {
        &self.dispatcher
    }

    /// Mutably borrow the dispatcher to create or destroy waitable objects.
    #[must_use]
    pub fn dispatcher_mut(&mut self) -> &mut Dispatcher<OBJECTS, WAITERS> {
        &mut self.dispatcher
    }

    /// Add a non-idle thread to the scheduler.
    pub fn add_thread(
        &mut self,
        id: ThreadId,
        policy: Policy,
        priority: u8,
        quantum: u32,
        affinity: u64,
    ) -> Result<(), ExecutiveError> {
        self.scheduler
            .add_thread(id, policy, priority, quantum, affinity)
            .map_err(ExecutiveError::Scheduler)
    }

    /// Install one idle thread for a processor.
    pub fn add_idle_thread(
        &mut self,
        id: ThreadId,
        processor: usize,
    ) -> Result<(), ExecutiveError> {
        self.scheduler
            .add_idle_thread(id, processor)
            .map_err(ExecutiveError::Scheduler)
    }

    /// Add the thread to a job and constrain it by that job's processor mask.
    pub fn join_job(&mut self, thread: ThreadId, job: JobId) -> Result<(), ExecutiveError> {
        self.scheduler
            .join_job(thread, job)
            .map_err(ExecutiveError::Scheduler)
    }

    /// Start a common `WaitAny`/`WaitAll` wait and block the scheduler thread when pending.
    pub fn wait(
        &mut self,
        thread: ThreadId,
        objects: &[DispatcherObject],
        wait_type: WaitType,
        timeout: Option<u64>,
        alertable: bool,
    ) -> Result<WaitStart, ExecutiveError> {
        if self
            .completions
            .iter()
            .any(|entry| entry.is_some_and(|(completed, _)| completed == thread))
        {
            return Err(ExecutiveError::UncollectedWaitResult);
        }
        let result = self
            .dispatcher
            .wait(thread, objects, wait_type, timeout, alertable)
            .map_err(ExecutiveError::Dispatcher)?;
        if result == WaitStart::Pending {
            if let Err(error) = self.scheduler.block(thread) {
                self.dispatcher.cancel_wait(thread);
                return Err(ExecutiveError::Scheduler(error));
            }
            self.recompute_donations()?;
        }
        Ok(result)
    }

    /// Set an event, then wake and schedule every thread whose common wait became satisfied.
    pub fn set_event(&mut self, object: DispatcherObject) -> Result<usize, ExecutiveError> {
        self.dispatcher
            .set_event(object)
            .map_err(ExecutiveError::Dispatcher)?;
        self.wake_completed()
    }

    /// Release semaphore permits, then wake eligible waiters.
    pub fn release_semaphore(
        &mut self,
        object: DispatcherObject,
        permits: u32,
    ) -> Result<usize, ExecutiveError> {
        self.dispatcher
            .release_semaphore(object, permits)
            .map_err(ExecutiveError::Dispatcher)?;
        self.wake_completed()
    }

    /// Release an owner-tracked mutex, then wake its next waiter and recalculate donation.
    pub fn release_mutex(
        &mut self,
        object: DispatcherObject,
        owner: ThreadId,
    ) -> Result<usize, ExecutiveError> {
        self.dispatcher
            .release_mutex(object, owner)
            .map_err(ExecutiveError::Dispatcher)?;
        let awakened = self.wake_completed()?;
        self.recompute_donations()?;
        Ok(awakened)
    }

    /// Advance dispatcher time, fire timers, expire timeouts, and wake completed waiters.
    pub fn advance_time(&mut self, ticks: u64) -> Result<usize, ExecutiveError> {
        self.dispatcher
            .advance_time(ticks)
            .map_err(ExecutiveError::Dispatcher)?;
        self.wake_completed()
    }

    /// Deliver an alert to a thread and wake it if an alertable wait was interrupted.
    pub fn alert_thread(&mut self, thread: ThreadId) -> Result<usize, ExecutiveError> {
        self.dispatcher
            .alert_thread(thread)
            .map_err(ExecutiveError::Dispatcher)?;
        self.wake_completed()
    }

    /// Terminate a thread, signal its join objects, abandon its mutexes, and wake waiters.
    pub fn terminate_thread(&mut self, thread: ThreadId) -> Result<usize, ExecutiveError> {
        self.scheduler
            .terminate(thread)
            .map_err(ExecutiveError::Scheduler)?;
        self.dispatcher.terminate_thread(thread);
        let awakened = self.wake_completed()?;
        self.recompute_donations()?;
        Ok(awakened)
    }

    /// Collect a signaled, timed-out, or alerted wait result for a thread.
    pub fn take_wait_result(&mut self, thread: ThreadId) -> Option<WaitStatus> {
        let slot = self
            .completions
            .iter()
            .position(|entry| entry.is_some_and(|(completed, _)| completed == thread))?;
        self.completions[slot].take().map(|(_, status)| status)
    }

    /// Process a clock tick and return the next ready identity for the architecture bridge.
    pub fn on_tick(&mut self, processor: usize) -> Result<Option<ThreadId>, ExecutiveError> {
        self.scheduler
            .on_tick(processor)
            .map_err(ExecutiveError::Scheduler)
    }

    /// Snapshot one thread's state through the integrated scheduler.
    #[must_use]
    pub fn snapshot(&self, thread: ThreadId) -> Option<ThreadSnapshot> {
        self.scheduler.snapshot(thread)
    }

    fn wake_completed(&mut self) -> Result<usize, ExecutiveError> {
        let mut ids = [None; THREADS];
        for (count, id) in self.scheduler.thread_ids().enumerate() {
            ids[count] = Some(id);
        }
        let mut awakened = 0;
        for thread in ids.into_iter().flatten() {
            if self
                .scheduler
                .snapshot(thread)
                .is_none_or(|snapshot| snapshot.state != crate::scheduler::ThreadState::Waiting)
            {
                continue;
            }
            let Some(status) = self.dispatcher.poll_wait(thread) else {
                continue;
            };
            let result_slot = self
                .completions
                .iter()
                .position(Option::is_none)
                .ok_or(ExecutiveError::CompletionTableFull)?;
            self.scheduler
                .wake(thread)
                .map_err(ExecutiveError::Scheduler)?;
            self.completions[result_slot] = Some((thread, status));
            awakened += 1;
        }
        if awakened != 0 {
            self.recompute_donations()?;
        }
        Ok(awakened)
    }

    fn recompute_donations(&mut self) -> Result<(), ExecutiveError> {
        let mut ids = [None; THREADS];
        for (count, id) in self.scheduler.thread_ids().enumerate() {
            ids[count] = Some(id);
        }
        for thread in ids.into_iter().flatten() {
            if self
                .scheduler
                .snapshot(thread)
                .is_some_and(|snapshot| snapshot.state != crate::scheduler::ThreadState::Idle)
            {
                self.scheduler
                    .clear_donation(thread)
                    .map_err(ExecutiveError::Scheduler)?;
            }
        }
        // A bounded number of passes propagates donation along a mutex-owner chain and stops
        // even if a malformed ownership cycle exists.
        for _ in 0..THREADS {
            let mut changed = false;
            for waiter in ids.into_iter().flatten() {
                if self
                    .scheduler
                    .snapshot(waiter)
                    .is_none_or(|snapshot| snapshot.state != crate::scheduler::ThreadState::Waiting)
                {
                    continue;
                }
                let Some(owner) = self.dispatcher.blocked_mutex_owner(waiter) else {
                    continue;
                };
                let before = self.scheduler.effective_priority(owner);
                let after = self
                    .scheduler
                    .donate_priority(owner, waiter)
                    .map_err(ExecutiveError::Scheduler)?;
                changed |= before != Some(after);
            }
            if !changed {
                break;
            }
        }
        Ok(())
    }
}

impl<const THREADS: usize, const CPUS: usize, const OBJECTS: usize, const WAITERS: usize> Default
    for ExecutiveCore<THREADS, CPUS, OBJECTS, WAITERS>
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::WaitStart;
    use crate::scheduler::ThreadState;

    fn thread(value: u64) -> ThreadId {
        ThreadId::new(value)
    }

    #[test]
    fn signaling_an_event_unblocks_the_scheduler_and_retains_the_wait_result() {
        let mut executive = ExecutiveCore::<3, 1, 4, 4>::new();
        executive.add_idle_thread(thread(1), 0).unwrap();
        executive
            .add_thread(thread(2), Policy::Other, 8, 2, 1)
            .unwrap();
        executive.on_tick(0).unwrap();
        let event = executive
            .dispatcher_mut()
            .create_event(false, false)
            .unwrap();
        assert_eq!(
            executive.wait(thread(2), &[event], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(
            executive.snapshot(thread(2)).unwrap().state,
            ThreadState::Waiting
        );
        assert_eq!(executive.set_event(event), Ok(1));
        assert_eq!(
            executive.snapshot(thread(2)).unwrap().state,
            ThreadState::Running
        );
        assert_eq!(
            executive.take_wait_result(thread(2)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert_eq!(executive.take_wait_result(thread(2)), None);
    }

    #[test]
    fn blocked_high_priority_waiter_donates_through_mutex_and_timeout_wakes_it() {
        let mut executive = ExecutiveCore::<4, 1, 4, 4>::new();
        executive.add_idle_thread(thread(1), 0).unwrap();
        executive
            .add_thread(thread(2), Policy::Other, 2, 20, 1)
            .unwrap();
        executive
            .add_thread(thread(3), Policy::Fifo, 30, 4, 1)
            .unwrap();
        executive.on_tick(0).unwrap();
        let mutex = executive
            .dispatcher_mut()
            .create_mutex(Some(thread(2)))
            .unwrap();
        assert_eq!(
            executive.wait(thread(3), &[mutex], WaitType::Any, Some(3), false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(
            executive.scheduler().effective_priority(thread(2)),
            Some(15)
        );
        assert_eq!(executive.advance_time(3), Ok(1));
        assert_eq!(executive.scheduler().effective_priority(thread(2)), Some(2));
        assert_eq!(
            executive.snapshot(thread(3)).unwrap().state,
            ThreadState::Running
        );
        assert_eq!(
            executive.wait(thread(3), &[mutex], WaitType::Any, None, false),
            Err(ExecutiveError::UncollectedWaitResult)
        );
        assert_eq!(
            executive.take_wait_result(thread(3)),
            Some(WaitStatus::Timeout)
        );
    }

    #[test]
    fn alertable_waits_and_thread_join_signal_the_same_scheduler_transition() {
        let mut executive = ExecutiveCore::<4, 1, 4, 4>::new();
        executive.add_idle_thread(thread(1), 0).unwrap();
        executive
            .add_thread(thread(2), Policy::Other, 8, 1, 1)
            .unwrap();
        executive
            .add_thread(thread(3), Policy::Other, 7, 1, 1)
            .unwrap();
        executive.on_tick(0).unwrap();
        let event = executive
            .dispatcher_mut()
            .create_event(false, false)
            .unwrap();
        let join = executive
            .dispatcher_mut()
            .create_thread_waitable(thread(3))
            .unwrap();
        executive
            .wait(thread(2), &[event], WaitType::Any, None, true)
            .unwrap();
        assert_eq!(executive.alert_thread(thread(2)), Ok(1));
        assert_eq!(
            executive.take_wait_result(thread(2)),
            Some(WaitStatus::Alerted)
        );
        executive
            .wait(thread(2), &[join], WaitType::Any, None, false)
            .unwrap();
        assert_eq!(executive.terminate_thread(thread(3)), Ok(1));
        assert_eq!(
            executive.take_wait_result(thread(2)),
            Some(WaitStatus::Signaled { index: 0 })
        );
    }
}
