//! Waitable dispatcher objects and the shared `WaitForMultipleObjects`-shaped primitive.
//!
//! Events, recursive owner-tracked mutexes, semaphores, timers, and thread-join objects use
//! one wait protocol. This model is fixed-capacity and host-testable; a kernel wait path pairs
//! its pending `ThreadId` with the scheduler's blocked/runnable transitions.

use crate::ThreadId;

/// Maximum number of objects in one multi-object wait.
pub const MAX_WAIT_OBJECTS: usize = 64;

/// A generation-checked handle to a dispatcher object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DispatcherObject(u32);

impl DispatcherObject {
    /// Reconstruct an opaque id received from a handle table or diagnostic record.
    #[must_use]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    /// The raw value used by a kernel handle table.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    fn encode(slot: usize, generation: u16) -> Self {
        Self((u32::from(generation) << 16) | (slot as u32 + 1))
    }

    fn decode(self) -> Option<(usize, u16)> {
        let slot = (self.0 & 0xFFFF) as usize;
        let generation = (self.0 >> 16) as u16;
        if slot == 0 || generation == 0 {
            None
        } else {
            Some((slot - 1, generation))
        }
    }
}

/// Whether a wait needs one object or every object to become available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitType {
    /// Return as soon as one object is available.
    Any,
    /// Acquire all requested objects atomically before returning.
    All,
}

/// Completion of a wait, also returned later by [`Dispatcher::poll_wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    /// One object satisfied a `WaitType::Any`; `index` is its position in the request.
    Signaled {
        /// The object index in the original wait list.
        index: u8,
    },
    /// Every object satisfied a `WaitType::All` request.
    AllSignaled,
    /// A mutex whose owner exited was acquired; `index` identifies it in the request.
    Abandoned {
        /// The abandoned mutex's index in the original wait list.
        index: u8,
    },
    /// The timeout elapsed without an object becoming available.
    Timeout,
    /// An alert interrupted an alertable wait.
    Alerted,
}

/// Result of beginning a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStart {
    /// The wait could be satisfied without blocking.
    Complete(WaitStatus),
    /// The thread is now represented in the wait table and must be blocked by the scheduler.
    Pending,
}

/// Why a dispatcher operation could not be performed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// Every object slot is in use.
    ObjectTableFull,
    /// The id is null, malformed, stale, or names a deleted object.
    InvalidObject,
    /// The operation was applied to a different dispatcher object type.
    WrongObjectType,
    /// The wait list must contain at least one object.
    EmptyWait,
    /// The wait list exceeds [`MAX_WAIT_OBJECTS`].
    TooManyObjects,
    /// One object appeared twice in the same wait request.
    DuplicateObject,
    /// The thread already has a pending or uncollected wait result.
    AlreadyWaiting,
    /// The waiter table is full.
    WaitTableFull,
    /// A zero-valued semaphore release is invalid.
    ZeroRelease,
    /// Releasing the semaphore would exceed its configured maximum.
    SemaphoreLimit,
    /// The calling thread does not own the mutex.
    NotMutexOwner,
    /// A recursive acquisition count would overflow.
    RecursionOverflow,
    /// The requested timeout or timer deadline would overflow the logical clock.
    ClockOverflow,
    /// The object is named by a currently pending wait and cannot be destroyed yet.
    ObjectBusy,
    /// No slot is available to retain an alert for this thread.
    AlertTableFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectKind {
    Event {
        manual_reset: bool,
        signaled: bool,
    },
    Mutex {
        owner: Option<ThreadId>,
        recursion: u32,
        abandoned: bool,
    },
    Semaphore {
        count: u32,
        maximum: u32,
    },
    Timer {
        manual_reset: bool,
        signaled: bool,
        due: Option<u64>,
        period: u64,
    },
    Thread {
        thread: ThreadId,
        terminated: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObjectSlot {
    generation: u16,
    object: Option<ObjectKind>,
}

impl ObjectSlot {
    const EMPTY: Self = Self {
        generation: 0,
        object: None,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingWait {
    thread: ThreadId,
    objects: [Option<DispatcherObject>; MAX_WAIT_OBJECTS],
    count: u8,
    wait_type: WaitType,
    deadline: Option<u64>,
    alertable: bool,
    sequence: u64,
    completion: Option<WaitStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AlertSlot {
    thread: ThreadId,
}

/// Fixed-capacity dispatcher object table, logical clock, and wait queues.
///
/// A pending wait is completed in FIFO submission order when its resources become available.
/// Auto-reset events, semaphores, and mutexes are consumed by exactly one waiter; manual-reset
/// events and timers remain signaled until explicitly reset or canceled.
pub struct Dispatcher<const OBJECTS: usize, const WAITERS: usize> {
    objects: [ObjectSlot; OBJECTS],
    waiters: [Option<PendingWait>; WAITERS],
    alerts: [Option<AlertSlot>; WAITERS],
    now: u64,
    next_sequence: u64,
}

impl<const OBJECTS: usize, const WAITERS: usize> Dispatcher<OBJECTS, WAITERS> {
    /// An empty dispatcher table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            objects: [ObjectSlot::EMPTY; OBJECTS],
            waiters: [None; WAITERS],
            alerts: [None; WAITERS],
            now: 0,
            next_sequence: 0,
        }
    }

    /// Current logical time in scheduler ticks.
    #[must_use]
    pub const fn now(&self) -> u64 {
        self.now
    }

    /// Create an event object.
    pub fn create_event(
        &mut self,
        manual_reset: bool,
        initially_signaled: bool,
    ) -> Result<DispatcherObject, DispatchError> {
        self.allocate(ObjectKind::Event {
            manual_reset,
            signaled: initially_signaled,
        })
    }

    /// Create a recursive mutex, optionally initially owned by `owner`.
    pub fn create_mutex(
        &mut self,
        owner: Option<ThreadId>,
    ) -> Result<DispatcherObject, DispatchError> {
        self.allocate(ObjectKind::Mutex {
            owner,
            recursion: u32::from(owner.is_some()),
            abandoned: false,
        })
    }

    /// Create a semaphore with `initial` permits and a fixed `maximum`.
    pub fn create_semaphore(
        &mut self,
        initial: u32,
        maximum: u32,
    ) -> Result<DispatcherObject, DispatchError> {
        if maximum == 0 || initial > maximum {
            return Err(DispatchError::SemaphoreLimit);
        }
        self.allocate(ObjectKind::Semaphore {
            count: initial,
            maximum,
        })
    }

    /// Create an unarmed one-shot or periodic timer object.
    pub fn create_timer(&mut self, manual_reset: bool) -> Result<DispatcherObject, DispatchError> {
        self.allocate(ObjectKind::Timer {
            manual_reset,
            signaled: false,
            due: None,
            period: 0,
        })
    }

    /// Create a waitable that becomes signaled when `thread` terminates.
    pub fn create_thread_waitable(
        &mut self,
        thread: ThreadId,
    ) -> Result<DispatcherObject, DispatchError> {
        self.allocate(ObjectKind::Thread {
            thread,
            terminated: false,
        })
    }

    /// Destroy an object that is not referenced by a pending wait.
    pub fn destroy(&mut self, object: DispatcherObject) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        if self.waiters.iter().flatten().any(|waiter| {
            waiter.completion.is_none()
                && waiter.objects[..usize::from(waiter.count)].contains(&Some(object))
        }) {
            return Err(DispatchError::ObjectBusy);
        }
        let slot = &mut self.objects[index];
        slot.object = None;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        Ok(())
    }

    /// Set an event and complete every waiter that is now satisfiable.
    pub fn set_event(&mut self, object: DispatcherObject) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        match self.objects[index].object {
            Some(ObjectKind::Event {
                manual_reset,
                signaled: _,
            }) => {
                self.objects[index].object = Some(ObjectKind::Event {
                    manual_reset,
                    signaled: true,
                });
                self.service_waiters();
                Ok(())
            }
            _ => Err(DispatchError::WrongObjectType),
        }
    }

    /// Reset an event to the nonsignaled state.
    pub fn reset_event(&mut self, object: DispatcherObject) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        match self.objects[index].object {
            Some(ObjectKind::Event { manual_reset, .. }) => {
                self.objects[index].object = Some(ObjectKind::Event {
                    manual_reset,
                    signaled: false,
                });
                Ok(())
            }
            _ => Err(DispatchError::WrongObjectType),
        }
    }

    /// Whether an event or timer currently retains its signaled state.
    pub fn is_signaled(&self, object: DispatcherObject) -> Result<bool, DispatchError> {
        let index = self.object_index(object)?;
        match self.objects[index].object {
            Some(ObjectKind::Event { signaled, .. }) | Some(ObjectKind::Timer { signaled, .. }) => {
                Ok(signaled)
            }
            Some(ObjectKind::Thread { terminated, .. }) => Ok(terminated),
            Some(ObjectKind::Semaphore { count, .. }) => Ok(count != 0),
            Some(ObjectKind::Mutex { owner, .. }) => Ok(owner.is_none()),
            None => Err(DispatchError::InvalidObject),
        }
    }

    /// Release a mutex recursively; ownership is required for every release.
    pub fn release_mutex(
        &mut self,
        object: DispatcherObject,
        owner: ThreadId,
    ) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        let Some(ObjectKind::Mutex {
            owner: current_owner,
            recursion,
            abandoned,
        }) = self.objects[index].object
        else {
            return Err(DispatchError::WrongObjectType);
        };
        if current_owner != Some(owner) {
            return Err(DispatchError::NotMutexOwner);
        }
        let (next_owner, next_recursion) = if recursion > 1 {
            (Some(owner), recursion - 1)
        } else {
            (None, 0)
        };
        self.objects[index].object = Some(ObjectKind::Mutex {
            owner: next_owner,
            recursion: next_recursion,
            abandoned,
        });
        self.service_waiters();
        Ok(())
    }

    /// The current mutex owner and recursion depth, for priority-donation integration.
    pub fn mutex_owner(
        &self,
        object: DispatcherObject,
    ) -> Result<(Option<ThreadId>, u32), DispatchError> {
        let index = self.object_index(object)?;
        match self.objects[index].object {
            Some(ObjectKind::Mutex {
                owner, recursion, ..
            }) => Ok((owner, recursion)),
            _ => Err(DispatchError::WrongObjectType),
        }
    }

    /// Add `release` permits and wake waiters that can now consume them.
    pub fn release_semaphore(
        &mut self,
        object: DispatcherObject,
        release: u32,
    ) -> Result<u32, DispatchError> {
        if release == 0 {
            return Err(DispatchError::ZeroRelease);
        }
        let index = self.object_index(object)?;
        let Some(ObjectKind::Semaphore { count, maximum }) = self.objects[index].object else {
            return Err(DispatchError::WrongObjectType);
        };
        let new_count = count
            .checked_add(release)
            .filter(|count| *count <= maximum)
            .ok_or(DispatchError::SemaphoreLimit)?;
        self.objects[index].object = Some(ObjectKind::Semaphore {
            count: new_count,
            maximum,
        });
        self.service_waiters();
        Ok(new_count)
    }

    /// Arm a timer after `delay` ticks; `period == 0` makes it one-shot.
    pub fn arm_timer(
        &mut self,
        object: DispatcherObject,
        delay: u64,
        period: u64,
    ) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        let Some(ObjectKind::Timer { manual_reset, .. }) = self.objects[index].object else {
            return Err(DispatchError::WrongObjectType);
        };
        let due = self
            .now
            .checked_add(delay)
            .ok_or(DispatchError::ClockOverflow)?;
        self.objects[index].object = Some(ObjectKind::Timer {
            manual_reset,
            signaled: false,
            due: Some(due),
            period,
        });
        if delay == 0 {
            self.expire_timers();
            self.service_waiters();
        }
        Ok(())
    }

    /// Cancel a timer and clear its signaled state.
    pub fn cancel_timer(&mut self, object: DispatcherObject) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        let Some(ObjectKind::Timer { manual_reset, .. }) = self.objects[index].object else {
            return Err(DispatchError::WrongObjectType);
        };
        self.objects[index].object = Some(ObjectKind::Timer {
            manual_reset,
            signaled: false,
            due: None,
            period: 0,
        });
        Ok(())
    }

    /// Advance logical time, fire due timers, service object waits, and expire timeouts.
    pub fn advance_time(&mut self, ticks: u64) -> Result<(), DispatchError> {
        self.now = self
            .now
            .checked_add(ticks)
            .ok_or(DispatchError::ClockOverflow)?;
        self.expire_timers();
        self.service_waiters();
        for waiter in self.waiters.iter_mut().flatten() {
            if waiter.completion.is_none()
                && waiter.deadline.is_some_and(|deadline| deadline <= self.now)
            {
                waiter.completion = Some(WaitStatus::Timeout);
            }
        }
        Ok(())
    }

    /// Begin a `WaitAny` or `WaitAll` wait on one or more objects.
    ///
    /// A pending result must be paired with [`crate::scheduler::Scheduler::block`] by the
    /// executive. Once signaled, [`Dispatcher::poll_wait`] returns the result and the thread
    /// can be made runnable again.
    pub fn wait(
        &mut self,
        thread: ThreadId,
        objects: &[DispatcherObject],
        wait_type: WaitType,
        timeout: Option<u64>,
        alertable: bool,
    ) -> Result<WaitStart, DispatchError> {
        if objects.is_empty() {
            return Err(DispatchError::EmptyWait);
        }
        if objects.len() > MAX_WAIT_OBJECTS {
            return Err(DispatchError::TooManyObjects);
        }
        if self
            .waiters
            .iter()
            .flatten()
            .any(|waiter| waiter.thread == thread)
        {
            return Err(DispatchError::AlreadyWaiting);
        }
        for (index, object) in objects.iter().enumerate() {
            self.object_index(*object)?;
            if objects[..index].contains(object) {
                return Err(DispatchError::DuplicateObject);
            }
        }

        if let Some(status) = self.ready_status(thread, objects, wait_type)? {
            self.consume_wait(thread, objects, wait_type, status)?;
            return Ok(WaitStart::Complete(status));
        }
        if alertable && self.take_alert(thread) {
            return Ok(WaitStart::Complete(WaitStatus::Alerted));
        }
        if timeout == Some(0) {
            return Ok(WaitStart::Complete(WaitStatus::Timeout));
        }
        let deadline = timeout
            .map(|ticks| {
                self.now
                    .checked_add(ticks)
                    .ok_or(DispatchError::ClockOverflow)
            })
            .transpose()?;
        let slot = self
            .waiters
            .iter()
            .position(Option::is_none)
            .ok_or(DispatchError::WaitTableFull)?;
        let mut requested = [None; MAX_WAIT_OBJECTS];
        for (destination, source) in requested.iter_mut().zip(objects.iter()) {
            *destination = Some(*source);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.waiters[slot] = Some(PendingWait {
            thread,
            objects: requested,
            count: objects.len() as u8,
            wait_type,
            deadline,
            alertable,
            sequence,
            completion: None,
        });
        Ok(WaitStart::Pending)
    }

    /// Return and consume a completed wait result, if one exists for `thread`.
    pub fn poll_wait(&mut self, thread: ThreadId) -> Option<WaitStatus> {
        let slot = self
            .waiters
            .iter()
            .position(|entry| entry.is_some_and(|waiter| waiter.thread == thread))?;
        let status = self.waiters[slot]?.completion?;
        self.waiters[slot] = None;
        Some(status)
    }

    /// Cancel a pending wait or discard its completed result.
    pub fn cancel_wait(&mut self, thread: ThreadId) -> bool {
        if let Some(slot) = self
            .waiters
            .iter()
            .position(|entry| entry.is_some_and(|waiter| waiter.thread == thread))
        {
            self.waiters[slot] = None;
            true
        } else {
            false
        }
    }

    /// Alert a thread, completing an alertable wait or retaining an alert for its next wait.
    pub fn alert_thread(&mut self, thread: ThreadId) -> Result<(), DispatchError> {
        if let Some(waiter) = self.waiters.iter_mut().flatten().find(|waiter| {
            waiter.thread == thread && waiter.completion.is_none() && waiter.alertable
        }) {
            waiter.completion = Some(WaitStatus::Alerted);
            return Ok(());
        }
        if self
            .alerts
            .iter()
            .flatten()
            .any(|slot| slot.thread == thread)
        {
            return Ok(());
        }
        let slot = self
            .alerts
            .iter()
            .position(Option::is_none)
            .ok_or(DispatchError::AlertTableFull)?;
        self.alerts[slot] = Some(AlertSlot { thread });
        Ok(())
    }

    /// Return the owner of a mutex this thread is currently blocked on, if any.
    ///
    /// The scheduler uses this to donate the waiter's effective priority and bound priority
    /// inversion. A waiter over several objects reports the first contended mutex in request
    /// order; the scheduler can recompute donations after every ownership transition.
    #[must_use]
    pub fn blocked_mutex_owner(&self, thread: ThreadId) -> Option<ThreadId> {
        let waiter = self
            .waiters
            .iter()
            .flatten()
            .find(|waiter| waiter.thread == thread && waiter.completion.is_none())?;
        waiter.objects[..usize::from(waiter.count)]
            .iter()
            .flatten()
            .find_map(|object| {
                let index = self.object_index(*object).ok()?;
                match self.objects[index].object {
                    Some(ObjectKind::Mutex {
                        owner: Some(owner), ..
                    }) if owner != thread => Some(owner),
                    _ => None,
                }
            })
    }

    /// Mark a thread-join object signaled and abandon its owned mutexes.
    pub fn terminate_thread(&mut self, thread: ThreadId) {
        for slot in &mut self.objects {
            match slot.object {
                Some(ObjectKind::Thread {
                    thread: target,
                    terminated: _,
                }) if target == thread => {
                    slot.object = Some(ObjectKind::Thread {
                        thread: target,
                        terminated: true,
                    });
                }
                Some(ObjectKind::Mutex {
                    owner: Some(owner), ..
                }) if owner == thread => {
                    slot.object = Some(ObjectKind::Mutex {
                        owner: None,
                        recursion: 0,
                        abandoned: true,
                    });
                }
                _ => {}
            }
        }
        self.service_waiters();
    }

    /// Number of pending or completed-but-uncollected waits.
    #[must_use]
    pub fn waiter_count(&self) -> usize {
        self.waiters.iter().flatten().count()
    }

    fn allocate(&mut self, object: ObjectKind) -> Result<DispatcherObject, DispatchError> {
        let index = self
            .objects
            .iter()
            .position(|slot| slot.object.is_none())
            .ok_or(DispatchError::ObjectTableFull)?;
        let generation = self.objects[index].generation.max(1);
        self.objects[index].generation = generation;
        self.objects[index].object = Some(object);
        Ok(DispatcherObject::encode(index, generation))
    }

    fn object_index(&self, object: DispatcherObject) -> Result<usize, DispatchError> {
        let (index, generation) = object.decode().ok_or(DispatchError::InvalidObject)?;
        let slot = self
            .objects
            .get(index)
            .ok_or(DispatchError::InvalidObject)?;
        if slot.generation != generation || slot.object.is_none() {
            return Err(DispatchError::InvalidObject);
        }
        Ok(index)
    }

    fn take_alert(&mut self, thread: ThreadId) -> bool {
        if let Some(index) = self
            .alerts
            .iter()
            .position(|entry| entry.is_some_and(|slot| slot.thread == thread))
        {
            self.alerts[index] = None;
            true
        } else {
            false
        }
    }

    fn ready_status(
        &self,
        thread: ThreadId,
        objects: &[DispatcherObject],
        wait_type: WaitType,
    ) -> Result<Option<WaitStatus>, DispatchError> {
        if wait_type == WaitType::Any {
            for (index, object) in objects.iter().enumerate() {
                let object_index = self.object_index(*object)?;
                if self.is_available(object_index, thread) {
                    if matches!(
                        self.objects[object_index].object,
                        Some(ObjectKind::Mutex {
                            abandoned: true,
                            ..
                        })
                    ) {
                        return Ok(Some(WaitStatus::Abandoned { index: index as u8 }));
                    }
                    return Ok(Some(WaitStatus::Signaled { index: index as u8 }));
                }
            }
            return Ok(None);
        }

        for object in objects {
            let index = self.object_index(*object)?;
            if !self.is_available(index, thread) {
                return Ok(None);
            }
        }
        if let Some((index, _)) = objects.iter().enumerate().find(|(_, object)| {
            let Ok(object_index) = self.object_index(**object) else {
                return false;
            };
            matches!(
                self.objects[object_index].object,
                Some(ObjectKind::Mutex {
                    abandoned: true,
                    ..
                })
            )
        }) {
            Ok(Some(WaitStatus::Abandoned { index: index as u8 }))
        } else {
            Ok(Some(WaitStatus::AllSignaled))
        }
    }

    fn is_available(&self, index: usize, thread: ThreadId) -> bool {
        match self.objects[index].object {
            Some(ObjectKind::Event { signaled, .. }) | Some(ObjectKind::Timer { signaled, .. }) => {
                signaled
            }
            Some(ObjectKind::Mutex {
                owner, recursion, ..
            }) => owner.is_none() || (owner == Some(thread) && recursion < u32::MAX),
            Some(ObjectKind::Semaphore { count, .. }) => count > 0,
            Some(ObjectKind::Thread { terminated, .. }) => terminated,
            None => false,
        }
    }

    fn consume_wait(
        &mut self,
        thread: ThreadId,
        objects: &[DispatcherObject],
        wait_type: WaitType,
        status: WaitStatus,
    ) -> Result<(), DispatchError> {
        if wait_type == WaitType::Any {
            let index = match status {
                WaitStatus::Signaled { index } | WaitStatus::Abandoned { index } => {
                    usize::from(index)
                }
                WaitStatus::AllSignaled | WaitStatus::Timeout | WaitStatus::Alerted => {
                    return Ok(())
                }
            };
            self.consume_one(objects[index], thread)
        } else {
            for object in objects {
                self.consume_one(*object, thread)?;
            }
            Ok(())
        }
    }

    fn consume_one(
        &mut self,
        object: DispatcherObject,
        thread: ThreadId,
    ) -> Result<(), DispatchError> {
        let index = self.object_index(object)?;
        let Some(kind) = self.objects[index].object else {
            return Err(DispatchError::InvalidObject);
        };
        self.objects[index].object = Some(match kind {
            ObjectKind::Event {
                manual_reset: false,
                signaled: true,
            } => ObjectKind::Event {
                manual_reset: false,
                signaled: false,
            },
            ObjectKind::Timer {
                manual_reset: false,
                signaled: true,
                due,
                period,
            } => ObjectKind::Timer {
                manual_reset: false,
                signaled: false,
                due,
                period,
            },
            ObjectKind::Semaphore { count, maximum } if count > 0 => ObjectKind::Semaphore {
                count: count - 1,
                maximum,
            },
            ObjectKind::Mutex {
                owner,
                recursion,
                abandoned,
            } if owner == Some(thread) => ObjectKind::Mutex {
                owner: Some(thread),
                recursion: recursion
                    .checked_add(1)
                    .ok_or(DispatchError::RecursionOverflow)?,
                abandoned,
            },
            ObjectKind::Mutex {
                owner: None,
                abandoned: _,
                ..
            } => ObjectKind::Mutex {
                owner: Some(thread),
                recursion: 1,
                abandoned: false,
            },
            unchanged => unchanged,
        });
        Ok(())
    }

    fn service_waiters(&mut self) {
        loop {
            let mut selected: Option<(usize, u64, WaitStatus)> = None;
            for (index, slot) in self.waiters.iter().enumerate() {
                let Some(waiter) = slot else { continue };
                if waiter.completion.is_some() {
                    continue;
                }
                let count = usize::from(waiter.count);
                let objects = &waiter.objects[..count];
                let mut resolved = [DispatcherObject::from_raw(0); MAX_WAIT_OBJECTS];
                let mut valid = true;
                for (position, object) in objects.iter().enumerate() {
                    if let Some(object) = object {
                        resolved[position] = *object;
                    } else {
                        valid = false;
                        break;
                    }
                }
                if !valid {
                    continue;
                }
                let Ok(Some(status)) =
                    self.ready_status(waiter.thread, &resolved[..count], waiter.wait_type)
                else {
                    continue;
                };
                if selected.is_none_or(|(_, sequence, _)| waiter.sequence < sequence) {
                    selected = Some((index, waiter.sequence, status));
                }
            }
            let Some((index, _, status)) = selected else {
                break;
            };
            let waiter = self.waiters[index].expect("selected waiter exists");
            let count = usize::from(waiter.count);
            let mut resolved = [DispatcherObject::from_raw(0); MAX_WAIT_OBJECTS];
            for (position, object) in waiter.objects[..count].iter().enumerate() {
                resolved[position] = object.expect("validated wait object");
            }
            let result =
                self.consume_wait(waiter.thread, &resolved[..count], waiter.wait_type, status);
            let completion = if result.is_ok() {
                status
            } else {
                // Recursive overflow can only occur for the owning thread. Leave that waiter
                // blocked rather than mutating any of the requested objects partially.
                continue;
            };
            self.waiters[index]
                .as_mut()
                .expect("waiter exists")
                .completion = Some(completion);
        }
    }

    fn expire_timers(&mut self) {
        for slot in &mut self.objects {
            let Some(ObjectKind::Timer {
                manual_reset,
                signaled: _,
                due: Some(due),
                period,
            }) = slot.object
            else {
                continue;
            };
            if due > self.now {
                continue;
            }
            let next_due = if period == 0 {
                None
            } else {
                let elapsed_periods = (self.now.saturating_sub(due) / period).saturating_add(1);
                due.checked_add(period.saturating_mul(elapsed_periods))
                    .filter(|next| *next > self.now)
            };
            slot.object = Some(ObjectKind::Timer {
                manual_reset,
                signaled: true,
                due: next_due,
                period,
            });
        }
    }
}

impl<const OBJECTS: usize, const WAITERS: usize> Default for Dispatcher<OBJECTS, WAITERS> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(value: u64) -> ThreadId {
        ThreadId::new(value)
    }

    #[test]
    fn manual_and_auto_reset_events_share_the_common_wait_path() {
        let mut dispatcher = Dispatcher::<8, 8>::new();
        let manual = dispatcher.create_event(true, false).unwrap();
        let automatic = dispatcher.create_event(false, false).unwrap();
        assert_eq!(
            dispatcher.wait(thread(1), &[manual], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(
            dispatcher.wait(thread(2), &[manual], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        dispatcher.set_event(manual).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(1)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert_eq!(
            dispatcher.poll_wait(thread(2)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert!(dispatcher.is_signaled(manual).unwrap());
        dispatcher.reset_event(manual).unwrap();
        assert!(!dispatcher.is_signaled(manual).unwrap());

        assert_eq!(
            dispatcher.wait(thread(3), &[automatic], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(
            dispatcher.wait(thread(4), &[automatic], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        dispatcher.set_event(automatic).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(3)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert_eq!(dispatcher.poll_wait(thread(4)), None);
        assert!(!dispatcher.is_signaled(automatic).unwrap());
    }

    #[test]
    fn wait_all_consumes_resources_atomically_and_rejects_duplicate_objects() {
        let mut dispatcher = Dispatcher::<8, 8>::new();
        let event = dispatcher.create_event(true, false).unwrap();
        let semaphore = dispatcher.create_semaphore(0, 2).unwrap();
        assert_eq!(
            dispatcher.wait(thread(1), &[event, semaphore], WaitType::All, None, false),
            Ok(WaitStart::Pending)
        );
        dispatcher.set_event(event).unwrap();
        assert_eq!(dispatcher.poll_wait(thread(1)), None);
        assert_eq!(dispatcher.is_signaled(event), Ok(true));
        dispatcher.release_semaphore(semaphore, 1).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(1)),
            Some(WaitStatus::AllSignaled)
        );
        assert_eq!(dispatcher.is_signaled(semaphore), Ok(false));
        assert_eq!(
            dispatcher.wait(thread(2), &[event, event], WaitType::All, None, false),
            Err(DispatchError::DuplicateObject)
        );
    }

    #[test]
    fn mutexes_track_recursive_ownership_donation_and_abandonment() {
        let mut dispatcher = Dispatcher::<8, 8>::new();
        let mutex = dispatcher.create_mutex(None).unwrap();
        assert_eq!(
            dispatcher.wait(thread(1), &[mutex], WaitType::Any, None, false),
            Ok(WaitStart::Complete(WaitStatus::Signaled { index: 0 }))
        );
        dispatcher
            .wait(thread(1), &[mutex], WaitType::Any, None, false)
            .unwrap();
        assert_eq!(dispatcher.mutex_owner(mutex), Ok((Some(thread(1)), 2)));
        assert_eq!(
            dispatcher.release_mutex(mutex, thread(2)),
            Err(DispatchError::NotMutexOwner)
        );
        assert_eq!(
            dispatcher.wait(thread(2), &[mutex], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        dispatcher.terminate_thread(thread(1));
        assert_eq!(
            dispatcher.poll_wait(thread(2)),
            Some(WaitStatus::Abandoned { index: 0 })
        );
        assert_eq!(dispatcher.mutex_owner(mutex), Ok((Some(thread(2)), 1)));
    }

    #[test]
    fn semaphore_limits_and_fifo_waiter_wakeup_are_enforced() {
        let mut dispatcher = Dispatcher::<4, 4>::new();
        let semaphore = dispatcher.create_semaphore(0, 2).unwrap();
        dispatcher
            .wait(thread(1), &[semaphore], WaitType::Any, None, false)
            .unwrap();
        dispatcher
            .wait(thread(2), &[semaphore], WaitType::Any, None, false)
            .unwrap();
        assert_eq!(
            dispatcher.release_semaphore(semaphore, 3),
            Err(DispatchError::SemaphoreLimit)
        );
        dispatcher.release_semaphore(semaphore, 2).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(1)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert_eq!(
            dispatcher.poll_wait(thread(2)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert_eq!(dispatcher.is_signaled(semaphore), Ok(false));
        assert_eq!(
            dispatcher.release_semaphore(semaphore, 0),
            Err(DispatchError::ZeroRelease)
        );
    }

    #[test]
    fn timeouts_alertable_waits_and_one_shot_periodic_timers_work() {
        let mut dispatcher = Dispatcher::<8, 8>::new();
        let one_shot = dispatcher.create_timer(true).unwrap();
        let periodic = dispatcher.create_timer(false).unwrap();
        dispatcher.arm_timer(one_shot, 3, 0).unwrap();
        dispatcher.arm_timer(periodic, 2, 2).unwrap();
        assert_eq!(
            dispatcher.wait(thread(1), &[one_shot], WaitType::Any, Some(5), false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(
            dispatcher.wait(thread(2), &[periodic], WaitType::Any, Some(10), false),
            Ok(WaitStart::Pending)
        );
        dispatcher.advance_time(2).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(2)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        dispatcher.advance_time(1).unwrap();
        assert_eq!(
            dispatcher.poll_wait(thread(1)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        assert!(dispatcher.is_signaled(one_shot).unwrap());
        assert!(!dispatcher.is_signaled(periodic).unwrap());
        dispatcher.advance_time(2).unwrap();
        assert!(dispatcher.is_signaled(periodic).unwrap());

        assert_eq!(
            dispatcher.wait(thread(3), &[one_shot], WaitType::Any, Some(0), false),
            Ok(WaitStart::Complete(WaitStatus::Signaled { index: 0 }))
        );
        let unsignaled = dispatcher.create_event(false, false).unwrap();
        dispatcher
            .wait(thread(4), &[unsignaled], WaitType::Any, Some(1), false)
            .unwrap();
        dispatcher.advance_time(1).unwrap();
        assert_eq!(dispatcher.poll_wait(thread(4)), Some(WaitStatus::Timeout));
        dispatcher
            .wait(thread(5), &[unsignaled], WaitType::Any, None, true)
            .unwrap();
        dispatcher.alert_thread(thread(5)).unwrap();
        assert_eq!(dispatcher.poll_wait(thread(5)), Some(WaitStatus::Alerted));
    }

    #[test]
    fn thread_join_objects_and_generation_checked_destruction() {
        let mut dispatcher = Dispatcher::<2, 2>::new();
        let join = dispatcher.create_thread_waitable(thread(7)).unwrap();
        let stale = join;
        assert_eq!(
            dispatcher.wait(thread(1), &[join], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        dispatcher.terminate_thread(thread(7));
        assert_eq!(
            dispatcher.poll_wait(thread(1)),
            Some(WaitStatus::Signaled { index: 0 })
        );
        dispatcher.destroy(join).unwrap();
        let replacement = dispatcher.create_event(false, false).unwrap();
        assert_ne!(stale, replacement);
        assert_eq!(
            dispatcher.is_signaled(stale),
            Err(DispatchError::InvalidObject)
        );
    }

    #[test]
    fn wait_limits_and_destroy_busy_checks_are_reported() {
        let mut dispatcher = Dispatcher::<2, 1>::new();
        let event = dispatcher.create_event(false, false).unwrap();
        assert_eq!(
            dispatcher.wait(thread(1), &[], WaitType::Any, None, false),
            Err(DispatchError::EmptyWait)
        );
        assert_eq!(
            dispatcher.wait(thread(1), &[event], WaitType::Any, None, false),
            Ok(WaitStart::Pending)
        );
        assert_eq!(dispatcher.destroy(event), Err(DispatchError::ObjectBusy));
        assert_eq!(
            dispatcher.wait(thread(1), &[event], WaitType::Any, None, false),
            Err(DispatchError::AlreadyWaiting)
        );
        dispatcher.cancel_wait(thread(1));
        dispatcher.destroy(event).unwrap();
    }
}
