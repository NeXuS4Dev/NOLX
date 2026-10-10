//! Process and thread objects, and the dispatcher.
//!
//! M3 skeleton: the object layouts and the priority model, which the POSIX subsystem needs
//! in order to describe a process at all. The scheduler itself is M5; the model below is
//! what it will schedule.

use core::fmt::Write;

use crate::hal::console::Console;
use crate::obj::ObjectHeader;

/// Number of priority levels. Matches `docs/architecture/06-scheduler.md`.
pub const PRIORITY_LEVELS: u8 = 32;
/// First level of the real-time band.
pub const REALTIME_BASE: u8 = 16;
/// Highest level usable from user mode.
pub const MAX_USER_PRIORITY: u8 = 31;
/// The idle thread's level.
pub const IDLE_PRIORITY: u8 = 0;

/// A scheduling policy, as POSIX names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// `SCHED_OTHER`: the default, priority-boosted, time-sliced policy.
    Other,
    /// `SCHED_FIFO`: runs until it blocks, yields, or is preempted by a higher priority.
    Fifo,
    /// `SCHED_RR`: like `Fifo` but round-robins among equals on a quantum.
    RoundRobin,
    /// `SCHED_SPORADIC`: a server with a replenishment period.
    Sporadic,
}

impl Policy {
    /// The POSIX constant for `sched_getscheduler`.
    #[must_use]
    pub const fn as_posix(self) -> i32 {
        match self {
            Self::Other => 0,
            Self::Fifo => 1,
            Self::RoundRobin => 2,
            Self::Sporadic => 3,
        }
    }

    /// Parse a POSIX policy constant.
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

    /// `true` if this policy belongs to the real-time band.
    #[must_use]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Fifo | Self::RoundRobin | Self::Sporadic)
    }
}

/// Map a POSIX priority onto an internal level.
///
/// POSIX leaves the range to the implementation and requires only that
/// `sched_get_priority_min` and `sched_get_priority_max` report it. NOLX uses the whole
/// real-time band, so the mapping is the identity shifted into it.
#[must_use]
pub const fn to_internal_priority(policy: Policy, posix_priority: i32) -> Option<u8> {
    if posix_priority < 0 {
        return None;
    }
    let level = if policy.is_realtime() {
        REALTIME_BASE.saturating_add(posix_priority as u8)
    } else {
        // SCHED_OTHER's only knob is `nice`, which the POSIX subsystem translates into the
        // dynamic band rather than coming through here.
        return None;
    };
    if level > MAX_USER_PRIORITY {
        None
    } else {
        Some(level)
    }
}

/// Map an internal level back to a POSIX priority.
#[must_use]
pub const fn to_posix_priority(policy: Policy, level: u8) -> Option<i32> {
    if !policy.is_realtime() || level < REALTIME_BASE {
        return None;
    }
    Some(i32::from(level - REALTIME_BASE))
}

/// The POSIX-visible priority range for a policy.
#[must_use]
pub const fn priority_range(policy: Policy) -> (i32, i32) {
    if policy.is_realtime() {
        (0, i32::from(MAX_USER_PRIORITY - REALTIME_BASE))
    } else {
        (0, 0)
    }
}

/// A thread's scheduling state.
#[derive(Clone, Copy, Debug)]
pub struct SchedState {
    /// Current policy.
    pub policy: Policy,
    /// Base priority before boosts.
    pub base_priority: u8,
    /// Current effective priority, which includes decay and priority donation.
    pub current_priority: u8,
    /// Remaining quantum, in clock ticks.
    pub quantum: u32,
    /// CPUs this thread may run on, as a bitmask over the first 64 processors.
    pub affinity: u64,
}

impl Default for SchedState {
    fn default() -> Self {
        Self {
            policy: Policy::Other,
            base_priority: 8,
            current_priority: 8,
            quantum: 20,
            affinity: u64::MAX,
        }
    }
}

/// A thread object.
pub struct Thread {
    /// The object header. The body follows it in memory.
    pub header: ObjectHeader,
    /// Scheduling state.
    pub sched: SchedState,
    /// The process this thread belongs to.
    pub process: u64,
}

/// A process object.
pub struct Process {
    /// The object header.
    pub header: ObjectHeader,
    /// Parent process id.
    pub parent: u64,
    /// POSIX process group id, set by `setpgid`.
    pub pgid: i32,
    /// POSIX session id, set by `setsid`.
    pub sid: i32,
    /// Exit status once terminated, as `waitid` reports it.
    pub exit_status: i32,
    /// `true` once the process has exited but has not been reaped. This is the zombie state
    /// POSIX requires, and it exists because a parent must be able to learn the status
    /// after the fact.
    pub zombie: bool,
}

/// Register the process and thread types and report the priority model.
pub fn init(console: &mut Console) {
    let (min, max) = priority_range(Policy::Fifo);
    let _ = writeln!(
        console,
        "exec: {PRIORITY_LEVELS} priority levels, real-time band from {REALTIME_BASE}, \
         SCHED_FIFO priority range {min}..{max}"
    );
}
