//! Executive process/thread objects, dispatcher policy, and the BSP scheduler.
//!
//! The host-tested [`nolx_exec_core`] crate owns scheduling and wait-transition rules; this
//! module binds them to Object Manager bodies, process address spaces, and x86-64 IRQ contexts.

mod context_switch;
pub mod process;
mod selftest;
pub mod thread;
pub mod user;

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

/// Route a clock tick through the running phase's scheduler and return the frame to
/// restore: the M5 kernel-thread round-robin while it runs, then the ring-3 user threads.
pub(crate) fn on_timer_interrupt(
    frame: *mut crate::hal::interrupts::InterruptFrame,
) -> *mut crate::hal::interrupts::InterruptFrame {
    if user::selftest::running() {
        // SAFETY: the interrupt stub built `frame` on the interrupted thread's kernel stack.
        unsafe { user::on_timer(frame) }
    } else {
        context_switch::on_timer(frame)
    }
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

/// Begin the M6 ring-3 test after the round-robin finishes: spawn `init`, enter ring 3.
pub(crate) fn begin_user_test(console: &mut crate::hal::console::Console) {
    user::selftest::begin(console);
}

/// Whether the ring-3 test has finished and its result is ready.
#[must_use]
pub(crate) fn user_test_complete() -> bool {
    user::selftest::complete()
}

/// Print the M6 ring-3 result marker; return false if the evidence chain was not exact.
pub(crate) fn report_user_test(console: &mut crate::hal::console::Console) -> bool {
    user::selftest::report(console)
}

/// Begin the ring-3 input `read(2)` phase in the dedicated image.
#[cfg(feature = "hal-input-read-test")]
pub(crate) fn begin_input_read_test(console: &mut crate::hal::console::Console) {
    user::selftest::read_test::begin(console);
}

/// Whether the input `read(2)` phase is still running.
#[cfg(feature = "hal-input-read-test")]
#[must_use]
pub(crate) fn input_read_test_running() -> bool {
    user::selftest::read_test::running()
}

/// Whether the input `read(2)` phase had no PS/2 keyboard to read from.
#[cfg(feature = "hal-input-read-test")]
#[must_use]
pub(crate) fn input_read_test_skipped() -> bool {
    user::selftest::read_test::skipped()
}

/// Report the input `read(2)` phase; false when the evidence chain was not exact.
#[cfg(feature = "hal-input-read-test")]
pub(crate) fn report_input_read_test(console: &mut crate::hal::console::Console) -> bool {
    user::selftest::read_test::report(console)
}

/// Begin the ring-3 blocking `poll(2)` phase in the dedicated image.
#[cfg(feature = "hal-input-wait-test")]
pub(crate) fn begin_input_poll_test(console: &mut crate::hal::console::Console) {
    user::selftest::poll_test::begin(console);
}

/// Whether the blocking `poll(2)` phase has resolved (a waiter was spawned, or it was skipped).
#[cfg(feature = "hal-input-wait-test")]
#[must_use]
pub(crate) fn input_poll_test_started() -> bool {
    user::selftest::poll_test::started()
}

/// Whether the blocking `poll(2)` phase is still running.
#[cfg(feature = "hal-input-wait-test")]
#[must_use]
pub(crate) fn input_poll_test_running() -> bool {
    user::selftest::poll_test::running()
}

/// Whether the blocking `poll(2)` phase had no pointer device to wait on.
#[cfg(feature = "hal-input-wait-test")]
#[must_use]
pub(crate) fn input_poll_test_skipped() -> bool {
    user::selftest::poll_test::skipped()
}

/// Report the blocking `poll(2)` phase; false when the evidence chain was not exact.
#[cfg(feature = "hal-input-wait-test")]
pub(crate) fn report_input_poll_test(console: &mut crate::hal::console::Console) -> bool {
    user::selftest::poll_test::report(console)
}
