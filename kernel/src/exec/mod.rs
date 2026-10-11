//! Executive process/thread objects, dispatcher policy, and the BSP scheduler.
//!
//! The host-tested [`nolx_exec_core`] crate owns scheduling and wait-transition rules; this
//! module binds them to Object Manager bodies, process address spaces, and x86-64 IRQ contexts.

mod context_switch;
pub mod process;
mod selftest;
pub mod thread;

pub use nolx_exec_core::scheduler::{
    nice_to_priority, priority_range, to_internal_priority, to_posix_priority, Policy,
    SchedulerError, ThreadSnapshot, ThreadState, DEFAULT_QUANTUM, DYNAMIC_PRIORITY_MAX,
    DYNAMIC_PRIORITY_MIN, IDLE_PRIORITY, PRIORITY_LEVELS, REALTIME_PRIORITY_MAX,
    REALTIME_PRIORITY_MIN,
};
pub use nolx_exec_core::{JobId, ProcessId, ThreadId};
pub use process::{Process, ProcessCreateError, ProcessExitError, PROCESS_HANDLE_TABLE_ENTRIES};
pub use thread::{Thread, ThreadCreateError, THREAD_APC_CAPACITY};

use nolx_hal_core::console::HalConsole;

/// First priority in the real-time band.
pub const REALTIME_BASE: u8 = REALTIME_PRIORITY_MIN;
/// Highest priority usable from user mode.
pub const MAX_USER_PRIORITY: u8 = REALTIME_PRIORITY_MAX;

/// Scheduling parameters stored in the kernel Thread object body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedState {
    /// Scheduling policy.
    pub policy: Policy,
    /// Base priority before dynamic boosts and priority donation.
    pub base_priority: u8,
    /// Current effective priority used by dispatcher queues.
    pub current_priority: u8,
    /// Configured time quantum in timer ticks.
    pub quantum: u32,
    /// CPUs this thread may run on, as a mask over the first 64 processors.
    pub affinity: u64,
}

impl Default for SchedState {
    fn default() -> Self {
        Self {
            policy: Policy::Other,
            base_priority: 8,
            current_priority: 8,
            quantum: DEFAULT_QUANTUM,
            affinity: u64::MAX,
        }
    }
}

/// Register process/thread state, exercise process-owned resources, and prepare the BSP test.
pub fn init(console: &mut dyn HalConsole) {
    let (minimum, maximum) = priority_range(Policy::Fifo);
    let _ = writeln!(
        console,
        "exec: {PRIORITY_LEVELS} priority levels, dynamic 1..15, real-time {REALTIME_BASE}..{MAX_USER_PRIORITY}, \
         SCHED_FIFO priority range {minimum}..{maximum}, queued per-processor run queues"
    );
    if let Err(error) = selftest::run(console) {
        let _ = writeln!(console, "NOLX: process selftest: FAILED: {error}");
        panic!("process selftest: {error}");
    }
    if let Err(error) = context_switch::prepare() {
        let _ = writeln!(console, "NOLX: scheduler setup: FAILED: {error:?}");
        panic!("scheduler setup: {error:?}");
    }
    let _ = writeln!(
        console,
        "NOLX: executive scheduler prepared: BSP idle context, three kernel threads, 16 KiB stacks"
    );
}

/// Route a clock tick through the executive scheduler and return the context to restore.
pub(crate) fn on_timer_interrupt(
    frame: *mut crate::hal::interrupts::InterruptFrame,
) -> *mut crate::hal::interrupts::InterruptFrame {
    context_switch::on_timer(frame)
}

/// Begin the QEMU T2 kernel-thread round-robin test after the periodic clock is armed.
pub(crate) fn begin_round_robin(console: &mut crate::hal::console::Console) {
    if let Err(error) = context_switch::begin(console) {
        panic!("round-robin test setup: {error:?}");
    }
}

/// Whether all kernel workers in the M5 on-machine test have exited.
#[must_use]
pub(crate) fn round_robin_complete() -> bool {
    context_switch::complete()
}

/// Print the QEMU T2 round-robin result; return false if its trace was not exact.
pub(crate) fn report_round_robin(console: &mut crate::hal::console::Console) -> bool {
    context_switch::report(console)
}
