//! BSP context-switch proof using the same IRQ frame and priority scheduler as the kernel.
//!
//! M5 is still a one-processor boot image; this module exercises preemptive kernel-thread
//! switching on the BSP. The core run-queue implementation already models multiple processors,
//! affinity, NUMA-aware stealing, and job masks, while AP startup belongs with the HAL SMP work.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use nolx_exec_core::scheduler::{Policy, Scheduler, SchedulerError};
use nolx_exec_core::ThreadId;

use crate::hal::console::Console;
use crate::hal::context::{initialize_kernel_stack, ContextInitError};
use crate::hal::interrupts::InterruptFrame;
use crate::sync::IrqMutex;

const IDLE_THREAD: ThreadId = ThreadId::new(1);
const FIRST_WORKER: u64 = 2;
const WORKER_COUNT: usize = 3;
const WORKER_STEPS: usize = 4;
const THREAD_SLOTS: usize = WORKER_COUNT + 1;
const TRACE_LENGTH: usize = WORKER_COUNT * WORKER_STEPS;
const STACK_SIZE: usize = 16 * 1024;
const EXPECTED_TRACE: &[u8; TRACE_LENGTH] = b"ABCABCABCABC";

#[repr(align(16))]
struct KernelStack([u8; STACK_SIZE]);

impl KernelStack {
    const fn empty() -> Self {
        Self([0; STACK_SIZE])
    }
}

struct SwitchState {
    scheduler: Scheduler<THREAD_SLOTS, 1>,
    stacks: [KernelStack; WORKER_COUNT],
    saved_frames: [usize; THREAD_SLOTS],
    prepared: bool,
    active: bool,
}

impl SwitchState {
    const fn new() -> Self {
        Self {
            scheduler: Scheduler::new(),
            stacks: [const { KernelStack::empty() }; WORKER_COUNT],
            saved_frames: [0; THREAD_SLOTS],
            prepared: false,
            active: false,
        }
    }
}

static SWITCH_STATE: IrqMutex<SwitchState> = IrqMutex::new(SwitchState::new());
static WORKER_DONE: [AtomicBool; WORKER_COUNT] = [const { AtomicBool::new(false) }; WORKER_COUNT];
static TRACE: [AtomicU8; TRACE_LENGTH] = [const { AtomicU8::new(0) }; TRACE_LENGTH];
static TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);
static ROUND_ROBIN_COMPLETE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupError {
    AlreadyPrepared,
    Scheduler(SchedulerError),
    Context(ContextInitError),
}

/// Construct one idle context and three preemptible worker contexts on the BSP.
pub(super) fn prepare() -> Result<(), SetupError> {
    let mut state = SWITCH_STATE.lock();
    if state.prepared {
        return Err(SetupError::AlreadyPrepared);
    }
    state
        .scheduler
        .add_idle_thread(IDLE_THREAD, 0)
        .map_err(SetupError::Scheduler)?;
    for worker in 0..WORKER_COUNT {
        let id = ThreadId::new(FIRST_WORKER + worker as u64);
        state
            .scheduler
            .add_thread(id, Policy::Other, 8, 1, 1)
            .map_err(SetupError::Scheduler)?;
    }

    let entries: [extern "C" fn() -> !; WORKER_COUNT] = [worker_a, worker_b, worker_c];
    for (index, entry) in entries.into_iter().enumerate() {
        let frame = initialize_kernel_stack(&mut state.stacks[index].0, entry, index)
            .map_err(SetupError::Context)?;
        let slot = index + 1;
        state.saved_frames[slot] = frame as usize;
    }
    state.saved_frames[0] = 0;
    state.prepared = true;
    Ok(())
}

/// Announce and enable the on-machine round-robin test after the timer source is live.
pub(super) fn begin(console: &mut Console) -> Result<(), SetupError> {
    let mut state = SWITCH_STATE.lock();
    if !state.prepared || state.active {
        return Err(SetupError::AlreadyPrepared);
    }
    TRACE_COUNT.store(0, Ordering::Relaxed);
    for done in &WORKER_DONE {
        done.store(false, Ordering::Relaxed);
    }
    for trace in &TRACE {
        trace.store(0, Ordering::Relaxed);
    }
    ROUND_ROBIN_COMPLETE.store(false, Ordering::Relaxed);
    let _ = write!(console, "NOLX: round-robin T2: ");
    state.active = true;
    Ok(())
}

/// Clock-interrupt hook: save the interrupted frame, tick the run queues, and return the
/// frame pointer the common assembly epilogue should restore.
pub(super) fn on_timer(frame: *mut InterruptFrame) -> *mut InterruptFrame {
    let mut state = SWITCH_STATE.lock();
    if !state.active {
        return frame;
    }

    let Some(previous) = state.scheduler.current(0) else {
        return frame;
    };
    let previous_slot = thread_slot(previous);
    state.saved_frames[previous_slot] = frame as usize;

    let mut retired_any = false;
    for (worker, done) in WORKER_DONE.iter().enumerate() {
        if !done.load(Ordering::Acquire) {
            continue;
        }
        let id = ThreadId::new(FIRST_WORKER + worker as u64);
        let Some(snapshot) = state.scheduler.snapshot(id) else {
            continue;
        };
        if snapshot.state != nolx_exec_core::scheduler::ThreadState::Terminated
            && state.scheduler.terminate(id).is_ok()
        {
            retired_any = true;
        }
    }
    if !retired_any {
        let _ = state.scheduler.on_tick(0);
    }

    let all_done = WORKER_DONE.iter().all(|done| done.load(Ordering::Acquire));
    if all_done {
        state.active = false;
        ROUND_ROBIN_COMPLETE.store(true, Ordering::Release);
    }

    let next = state.scheduler.current(0).unwrap_or(IDLE_THREAD);
    if next == previous {
        frame
    } else {
        let next_frame = state.saved_frames[thread_slot(next)];
        if next_frame == 0 {
            // A newly selected thread must have a synthetic initial frame. Returning the
            // current frame is safer than loading a null stack if that invariant regresses.
            frame
        } else {
            next_frame as *mut InterruptFrame
        }
    }
}

/// Whether all three kernel threads have finished their timer-driven work.
#[must_use]
pub(super) fn complete() -> bool {
    ROUND_ROBIN_COMPLETE.load(Ordering::Acquire)
}

/// Print the verified trace suffix and return whether the observed thread order was exact.
pub(super) fn report(console: &mut Console) -> bool {
    let count = TRACE_COUNT.load(Ordering::Acquire);
    let exact = count == TRACE_LENGTH
        && TRACE
            .iter()
            .zip(EXPECTED_TRACE.iter())
            .all(|(observed, expected)| observed.load(Ordering::Acquire) == *expected);
    let _ = writeln!(
        console,
        " complete: {WORKER_COUNT} kernel threads, {TRACE_LENGTH} timer quanta, order verified={exact}",
    );
    exact
}

fn thread_slot(thread: ThreadId) -> usize {
    usize::try_from(thread.raw().saturating_sub(IDLE_THREAD.raw()))
        .unwrap_or(0)
        .min(THREAD_SLOTS - 1)
}

fn run_worker(worker: usize, label: u8) -> ! {
    for step in 0..WORKER_STEPS {
        let interrupt_guard = crate::hal::cpu::InterruptGuard::disable();
        let position = TRACE_COUNT.fetch_add(1, Ordering::AcqRel);
        if let Some(trace) = TRACE.get(position) {
            trace.store(label, Ordering::Release);
        }
        let mut console = Console::new();
        console.put_byte(label);
        if step + 1 < WORKER_STEPS {
            // Do not enable IF before HLT: a tick in that gap can switch away,
            // leaving this worker to sleep through its next scheduled quantum.
            interrupt_guard.halt_until_interrupt();
        } else {
            // Publish completion before a final tick can preempt this worker.
            WORKER_DONE[worker].store(true, Ordering::Release);
            drop(interrupt_guard);
        }
    }
    loop {
        crate::hal::context::halt_for_interrupt();
    }
}

extern "C" fn worker_a() -> ! {
    run_worker(0, b'A')
}

extern "C" fn worker_b() -> ! {
    run_worker(1, b'B')
}

extern "C" fn worker_c() -> ! {
    run_worker(2, b'C')
}
