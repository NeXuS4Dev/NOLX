//! CMOS real-time clock and boot-time wall-clock anchor.
//!
//! NOLX interprets the PC/AT RTC as UTC. The initial two-digit-year policy and calendar
//! conversion live in `nolx-hal-core`; this module reads a stable CMOS snapshot and advances
//! that second-resolution anchor with the HPET/PIT monotonic clock after boot.

use core::sync::atomic::{AtomicU64, Ordering};

use nolx_hal_core::rtc::{self, RtcDateTime, RtcRegisters};
use nolx_hal_core::time::{realtime_ns, NANOSECONDS_PER_SECOND};

use super::{inb, outb, timer};

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;
const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;
const RTC_UPDATE_IN_PROGRESS: u8 = 1 << 7;
const MAX_UPDATE_POLLS: usize = 10_000;
const MAX_SNAPSHOT_ATTEMPTS: usize = 8;
const NO_WALL_CLOCK: u64 = u64::MAX;

static WALL_CLOCK_BASE_SECONDS: AtomicU64 = AtomicU64::new(NO_WALL_CLOCK);
static WALL_CLOCK_BASE_MONOTONIC_NS: AtomicU64 = AtomicU64::new(0);

/// Read and validate the boot processor's RTC once, before maskable interrupts are enabled.
///
/// The RTC is interpreted as UTC. Returns `None` if the hardware never presents a stable,
/// valid snapshot; a bad clock does not prevent the kernel from booting.
pub(crate) fn init() -> Option<RtcDateTime> {
    let time = read_utc();
    let seconds = time.map_or(NO_WALL_CLOCK, RtcDateTime::unix_seconds);
    if time.is_some() {
        WALL_CLOCK_BASE_MONOTONIC_NS.store(timer::monotonic_ns(), Ordering::Relaxed);
    }
    WALL_CLOCK_BASE_SECONDS.store(seconds, Ordering::Release);
    time
}

/// Read a stable RTC snapshot and decode its binary/BCD and 12/24-hour settings.
#[must_use]
fn read_utc() -> Option<RtcDateTime> {
    for _ in 0..MAX_SNAPSHOT_ATTEMPTS {
        if !wait_until_update_finishes() {
            continue;
        }

        let first = read_fields();
        if update_is_in_progress() {
            continue;
        }
        let second = read_fields();
        if first == second && !update_is_in_progress() {
            return rtc::decode(second);
        }
    }
    None
}

/// Current realtime in nanoseconds since the Unix epoch, advanced from the RTC anchor by
/// the same HPET/PIT-backed monotonic clock used for `CLOCK_MONOTONIC`.
#[must_use]
pub fn unix_time_ns() -> Option<u64> {
    let base_seconds = WALL_CLOCK_BASE_SECONDS.load(Ordering::Acquire);
    if base_seconds == NO_WALL_CLOCK {
        return None;
    }
    let monotonic_base = WALL_CLOCK_BASE_MONOTONIC_NS.load(Ordering::Relaxed);
    let elapsed = timer::monotonic_ns().saturating_sub(monotonic_base);
    Some(realtime_ns(base_seconds, elapsed))
}

/// Current realtime in whole seconds since the Unix epoch.
#[must_use]
pub fn unix_time_seconds() -> Option<u64> {
    unix_time_ns().map(|nanoseconds| nanoseconds / NANOSECONDS_PER_SECOND)
}

fn wait_until_update_finishes() -> bool {
    for _ in 0..MAX_UPDATE_POLLS {
        if !update_is_in_progress() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn update_is_in_progress() -> bool {
    read_cmos_register(RTC_STATUS_A) & RTC_UPDATE_IN_PROGRESS != 0
}

fn read_fields() -> RtcRegisters {
    RtcRegisters {
        second: read_cmos_register(RTC_SECONDS),
        minute: read_cmos_register(RTC_MINUTES),
        hour: read_cmos_register(RTC_HOURS),
        day: read_cmos_register(RTC_DAY),
        month: read_cmos_register(RTC_MONTH),
        year: read_cmos_register(RTC_YEAR),
        status_b: read_cmos_register(RTC_STATUS_B),
    }
}

fn read_cmos_register(register: u8) -> u8 {
    // SAFETY: 0x70/0x71 are the PC/AT CMOS address/data ports. Save/restore the selector,
    // including its NMI-disable bit; calls are serialized during single-CPU boot before
    // maskable interrupts are enabled, and there is no other CMOS client at this milestone.
    unsafe {
        let previous = inb(CMOS_INDEX);
        outb(CMOS_INDEX, (previous & 0x80) | (register & 0x7F));
        let value = inb(CMOS_DATA);
        outb(CMOS_INDEX, previous);
        value
    }
}
