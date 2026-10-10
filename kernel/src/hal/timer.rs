//! Periodic clock dispatch with a calibrated LAPIC/TSC-deadline path and PIT fallback.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use nolx_hal_core::time::{pit_ticks_to_ns, PIT_DIVISOR, PIT_TARGET_HZ};

use super::{apic, outb, pic};

const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_MODE_RATE_GENERATOR: u8 = 0x34;
const BACKEND_UNARMED: u8 = 0;
const BACKEND_PIT_PIC: u8 = 1;
const BACKEND_PIT_IOAPIC: u8 = 2;
const BACKEND_LAPIC_PERIODIC: u8 = 3;
const BACKEND_TSC_DEADLINE: u8 = 4;
const NANOS_PER_TICK: u64 = 10_000_000;

static TICKS: AtomicU64 = AtomicU64::new(0);
static BACKEND: AtomicU8 = AtomicU8::new(BACKEND_UNARMED);

/// Initialize the early monotonic counter; keep every periodic source stopped until setup ends.
pub fn init() {
    TICKS.store(0, Ordering::Relaxed);
    BACKEND.store(BACKEND_UNARMED, Ordering::Relaxed);
}

/// Start the best prepared periodic source, with a calibrated hardware path and PIT fallback.
pub fn start() {
    TICKS.store(0, Ordering::Relaxed);
    if apic::is_enabled() && apic::start_local_timer() {
        BACKEND.store(
            if apic::uses_tsc_deadline() {
                BACKEND_TSC_DEADLINE
            } else {
                BACKEND_LAPIC_PERIODIC
            },
            Ordering::Release,
        );
        return;
    }

    // SAFETY: channel-zero command/data ports are the standard 8254-compatible PC PIT. The
    // IDT is live and interrupts remain disabled while the divisor and routing are prepared.
    unsafe {
        outb(PIT_COMMAND, PIT_MODE_RATE_GENERATOR);
        outb(PIT_CHANNEL0, PIT_DIVISOR as u8);
        outb(PIT_CHANNEL0, (PIT_DIVISOR >> 8) as u8);
    }
    if apic::is_enabled() && apic::unmask_legacy_irq(0) {
        BACKEND.store(BACKEND_PIT_IOAPIC, Ordering::Release);
    } else {
        pic::unmask_irq(0);
        BACKEND.store(BACKEND_PIT_PIC, Ordering::Release);
    }
}

/// Advance the counter from a clock-vector handler and rearm one-shot deadline mode.
pub fn on_interrupt(vector: u8) {
    super::hpet::maintain_counter_epoch();
    TICKS.fetch_add(1, Ordering::Relaxed);
    if vector == nolx_hal_core::apic::LAPIC_TIMER_VECTOR && apic::uses_tsc_deadline() {
        apic::rearm_tsc_deadline();
    }
}

/// Elapsed clock interrupts since the selected backend was armed.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Monotonic time in nanoseconds from HPET when available, or from the periodic timer otherwise.
#[must_use]
pub fn monotonic_ns() -> u64 {
    if let Some(hpet_ns) = super::hpet::monotonic_ns() {
        return hpet_ns;
    }
    match BACKEND.load(Ordering::Acquire) {
        BACKEND_LAPIC_PERIODIC | BACKEND_TSC_DEADLINE => ticks().saturating_mul(NANOS_PER_TICK),
        _ => pit_ticks_to_ns(ticks()),
    }
}

/// The requested early periodic rate.
#[must_use]
pub const fn frequency_hz() -> u64 {
    PIT_TARGET_HZ
}

/// Name of the active backend, or `unarmed` before `start`.
#[must_use]
pub fn backend_name() -> &'static str {
    match BACKEND.load(Ordering::Acquire) {
        BACKEND_PIT_PIC => "PIT/PIC",
        BACKEND_PIT_IOAPIC => "PIT/IOAPIC",
        BACKEND_LAPIC_PERIODIC => "calibrated LAPIC",
        BACKEND_TSC_DEADLINE => "TSC deadline",
        _ => "unarmed",
    }
}
