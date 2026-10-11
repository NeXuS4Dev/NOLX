//! Kernel Thread object body: architecture context, stable stacks, TEB-equivalent, APC queues,
//! and scheduling parameters.

use core::mem::size_of;

use nolx_exec_core::thread::{StackDescriptor, ThreadContext};
use nolx_exec_core::{ProcessId, ThreadId};

use crate::obj::{type_by_name, ObjectHeader};

use super::{Policy, SchedState};

/// Fixed number of kernel and user APCs held by one thread object.
pub const THREAD_APC_CAPACITY: usize = 16;

/// A Thread object registered with the Object Manager.
pub struct Thread {
    /// Object Manager header.
    pub header: ObjectHeader,
    /// Stable thread identity.
    pub id: ThreadId,
    /// Process that owns this thread.
    pub process: ProcessId,
    /// Priority, scheduling policy, quantum, and affinity.
    pub sched: SchedState,
    /// CPU context, kernel/user stack metadata, TEB-equivalent, and APC queues.
    pub context: ThreadContext<THREAD_APC_CAPACITY>,
    /// Exit status once the thread has completed.
    pub exit_status: Option<u64>,
}

impl Thread {
    /// Create a kernel thread body with a validated, stable kernel stack.
    ///
    /// `entry_point` is the kernel address at which a future dispatcher starts the thread;
    /// the function is not entered by this constructor.
    pub fn new(
        id: ThreadId,
        process: ProcessId,
        entry_point: u64,
        argument: usize,
        kernel_stack: StackDescriptor,
        sched: SchedState,
    ) -> Result<Self, ThreadCreateError> {
        if entry_point == 0 {
            return Err(ThreadCreateError::InvalidEntryPoint);
        }
        let (type_index, _) = type_by_name("Thread").ok_or(ThreadCreateError::TypeNotRegistered)?;
        let body_size =
            u32::try_from(size_of::<Self>()).map_err(|_| ThreadCreateError::BodyTooLarge)?;
        let mut context = ThreadContext::kernel(id, process, kernel_stack);
        context.cpu.instruction_pointer = entry_point;
        context.cpu.stack_pointer = kernel_stack.top - 8;
        context.cpu.general[0] = argument as u64;
        Ok(Self {
            header: ObjectHeader::new(type_index, body_size),
            id,
            process,
            sched,
            context,
            exit_status: None,
        })
    }

    /// Whether the scheduler should publish the thread as a joinable, signaled object.
    #[must_use]
    pub const fn terminated(&self) -> bool {
        self.exit_status.is_some()
    }

    /// Publish a final status and make the thread object waitable.
    ///
    /// A second termination is refused so the thread-join object changes state exactly once.
    pub fn terminate(&mut self, exit_status: u64) -> Result<(), ThreadCreateError> {
        if self.exit_status.is_some() {
            return Err(ThreadCreateError::AlreadyTerminated);
        }
        self.exit_status = Some(exit_status);
        Ok(())
    }

    /// Change scheduling policy and effective priority metadata after validation.
    pub fn set_policy(
        &mut self,
        policy: Policy,
        priority: u8,
        quantum: u32,
    ) -> Result<(), ThreadCreateError> {
        let valid = match policy {
            Policy::Other => (1..=15).contains(&priority),
            Policy::Fifo | Policy::RoundRobin | Policy::Sporadic => (16..=31).contains(&priority),
        };
        if !valid || (policy != Policy::Fifo && quantum == 0) {
            return Err(ThreadCreateError::InvalidSchedulingState);
        }
        self.sched.policy = policy;
        self.sched.base_priority = priority;
        self.sched.current_priority = priority;
        self.sched.quantum = quantum.max(1);
        Ok(())
    }
}

/// Why a kernel Thread object could not be initialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadCreateError {
    /// The Object Manager has not registered its built-in Thread type.
    TypeNotRegistered,
    /// The Thread body size does not fit in the Object Manager header's size field.
    BodyTooLarge,
    /// A thread needs an entry point before it can be scheduled.
    InvalidEntryPoint,
    /// Policy and priority bands or quantum did not agree.
    InvalidSchedulingState,
    /// The thread was already terminated.
    AlreadyTerminated,
}
