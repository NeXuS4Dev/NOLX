//! Local xAPIC/x2APIC access, IPL masking, and calibrated local-timer support.

use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use nolx_hal_core::acpi::{LocalApicNmi, Madt};
use nolx_hal_core::apic::{
    calibrated_frequency_hz, icr_command, periodic_initial_count, priority_class,
    timer_divide_encoding, x2apic_msr, IpiDelivery, IA32_APIC_BASE, IA32_TSC_DEADLINE,
    ICR_DELIVERY_PENDING, LAPIC_TIMER_VECTOR, REG_ICR, REG_ICR_HIGH,
};
use nolx_hal_core::ipl::Ipl;

use super::{hpet, ioapic, rdmsr, wrmsr, CpuFeatures};

const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_ADDRESS_MASK: u64 = 0x0000_000F_FFFF_F000;
const APIC_LVT_MASKED: u32 = 1 << 16;
const APIC_LVT_NMI: u32 = 4 << 8;
const APIC_LVT_ACTIVE_LOW: u32 = 1 << 13;
const APIC_LVT_LEVEL_TRIGGERED: u32 = 1 << 15;
const APIC_SVR_ENABLE: u32 = 1 << 8;
const APIC_LVT_PERIODIC: u32 = 1 << 17;
const APIC_LVT_TSC_DEADLINE: u32 = 2 << 17;
const APIC_PRIORITY_SHIFT: u32 = 4;
const IMCR_SELECT_PORT: u16 = 0x22;
const IMCR_DATA_PORT: u16 = 0x23;
const IMCR_APIC_MODE_REGISTER: u8 = 0x70;
const TIMER_RATE_HZ: u64 = 100;
/// Bounded wait for an interrupt command to retire: a delivery that never completes is a
/// platform fault, and spinning forever inside the startup path would hide it.
const IPI_WAIT: u32 = 1_000_000;
const TIMER_DIVISOR: u16 = 16;
const MODE_DISABLED: u8 = 0;
const MODE_XAPIC: u8 = 1;
const MODE_X2APIC: u8 = 2;
const TIMER_NOT_CALIBRATED: u8 = 0;
const TIMER_LAPIC_PERIODIC: u8 = 1;
const TIMER_TSC_DEADLINE: u8 = 2;
const STANDARD_APIC_MMIO_START: u64 = 0xFEE0_0000;
const STANDARD_APIC_MMIO_END: u64 = 0xFF00_0000;

static MODE: AtomicU8 = AtomicU8::new(MODE_DISABLED);
static LOCAL_APIC_BASE: AtomicU64 = AtomicU64::new(0);
static LAPIC_TICKS_PER_SECOND: AtomicU64 = AtomicU64::new(0);
static TSC_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);
static TIMER_MODE: AtomicU8 = AtomicU8::new(TIMER_NOT_CALIBRATED);
static TSC_DEADLINE_SUPPORTED: AtomicU8 = AtomicU8::new(0);
static NEXT_TSC_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// The two local-interrupt-input values the boot path resolved from the MADT, packed as
/// `LINT0 | LINT1 << 32`, so a wake reset can restore them without re-parsing firmware.
static LINT_VALUES: AtomicU64 = AtomicU64::new(0);

/// Failure while discovering or initializing the local APIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApicError {
    /// CPUID reports no local APIC.
    Unsupported,
    /// Firmware did not describe the current BSP in the MADT.
    ProcessorNotFound,
    /// The local APIC base is not representable or outside the early UC mapping.
    UnmappedLocalApic,
    /// Firmware or the CPU reported an incompatible pre-enabled x2APIC state.
    InvalidMode,
    /// A local-APIC NMI record contains reserved polarity or trigger encodings.
    InvalidNmiConfiguration,
    /// I/O APIC setup failed.
    IoApic(ioapic::IoApicError),
    /// HPET calibration did not produce a usable counter interval.
    CalibrationUnavailable,
}

/// Measured timer rates after a bounded HPET calibration interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerCalibration {
    /// Local-APIC countdown rate after the selected divide-by-16 prescaler.
    pub lapic_ticks_per_second: u64,
    /// Invariant TSC rate, if available and measured.
    pub tsc_frequency_hz: Option<u64>,
}

/// Initialize the BSP local APIC and mask/program all legacy IRQ routes in the I/O APIC.
///
/// All address and MADT checks happen before enabling the local APIC. IRQ routes remain masked
/// until the timer backend or a device driver explicitly unmasks them.
pub fn init(madt: &Madt, features: CpuFeatures) -> Result<(), ApicError> {
    if !features.has_apic() {
        return Err(ApicError::Unsupported);
    }
    let bsp_apic_id = bsp_apic_id(features);
    if bsp_apic_id > u8::MAX as u32 {
        return Err(ApicError::IoApic(
            ioapic::IoApicError::UnsupportedDestination,
        ));
    }
    let processor = madt
        .local_apics()
        .iter()
        .find(|processor| processor.enabled() && processor.apic_id == bsp_apic_id)
        .ok_or(ApicError::ProcessorNotFound)?;
    let lint_values = lint_nmi_values(madt.local_apic_nmis(), processor.processor_uid)?;

    // SAFETY: CPUID verified local-APIC support, so IA32_APIC_BASE is implemented.
    let previous_base = unsafe { rdmsr(IA32_APIC_BASE) };
    let use_x2apic = features.has_x2apic();
    if previous_base & APIC_BASE_X2APIC != 0 && !use_x2apic {
        return Err(ApicError::InvalidMode);
    }

    let base = if use_x2apic {
        previous_base & APIC_BASE_ADDRESS_MASK
    } else if madt.local_apic_address != 0 {
        madt.local_apic_address
    } else {
        previous_base & APIC_BASE_ADDRESS_MASK
    };
    if base & 0xFFF != 0 || base & !APIC_BASE_ADDRESS_MASK != 0 {
        return Err(ApicError::UnmappedLocalApic);
    }
    if !use_x2apic {
        let mmio_end = base
            .checked_add(0x400)
            .ok_or(ApicError::UnmappedLocalApic)?;
        if base < STANDARD_APIC_MMIO_START || mmio_end > STANDARD_APIC_MMIO_END {
            return Err(ApicError::UnmappedLocalApic);
        }
    }

    // Validate and configure the interrupt-routing topology only after the local controller
    // address and mode are known to be usable. Any failure still leaves the PIC path active.
    ioapic::init(madt, bsp_apic_id).map_err(ApicError::IoApic)?;

    let mut enabled_base = (previous_base & !APIC_BASE_ADDRESS_MASK) | base | APIC_BASE_ENABLE;
    if use_x2apic {
        enabled_base |= APIC_BASE_X2APIC;
    } else {
        enabled_base &= !APIC_BASE_X2APIC;
    }
    // SAFETY: the CPU feature bits and base alignment/range were validated above. The value
    // preserves reserved/BSP bits and only sets the global-enable and selected mode bits.
    unsafe { wrmsr(IA32_APIC_BASE, enabled_base) };

    LOCAL_APIC_BASE.store(base, Ordering::Relaxed);
    MODE.store(
        if use_x2apic { MODE_X2APIC } else { MODE_XAPIC },
        Ordering::Release,
    );
    TSC_DEADLINE_SUPPORTED.store(
        u8::from(features.has_tsc_deadline() && features.has_invariant_tsc()),
        Ordering::Release,
    );

    write_register(nolx_hal_core::apic::reg::TPR, 0);
    write_register(
        nolx_hal_core::apic::reg::LVT_TIMER,
        APIC_LVT_MASKED | u32::from(LAPIC_TIMER_VECTOR),
    );
    write_register(nolx_hal_core::apic::reg::LVT_PERFORMANCE, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_ERROR, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_LINT0, lint_values[0]);
    write_register(nolx_hal_core::apic::reg::LVT_LINT1, lint_values[1]);
    LINT_VALUES.store(
        u64::from(lint_values[0]) | (u64::from(lint_values[1]) << 32),
        Ordering::Release,
    );
    write_register(
        nolx_hal_core::apic::reg::SVR,
        APIC_SVR_ENABLE | u32::from(nolx_hal_core::apic::APIC_SPURIOUS_VECTOR),
    );
    ioapic::unmask_nmi_sources();

    if madt.has_legacy_pic() {
        switch_imcr_to_apic();
    }
    Ok(())
}

/// Enable this processor's local APIC in the mode the bootstrap processor selected.
///
/// Every register here is per-processor, so an application processor repeats what [`init`]
/// did for the bootstrap processor *except* the I/O APIC programming and the legacy-PIC
/// switch: those are system-wide and already done. The mode, base and vector assignments are
/// read from the bootstrap processor's discovery instead of being re-derived, because a second
/// processor must not disagree with the first about where its registers are.
///
/// The interrupt inputs and the timer are left masked. An application processor in this kernel
/// takes no device interrupt and keeps no time; it answers inter-processor interrupts and
/// otherwise halts, so anything else arriving on it is a bug that the dispatcher counts rather
/// than a source to arm.
pub fn init_ap() -> Result<(), ApicError> {
    let mode = MODE.load(Ordering::Acquire);
    if mode == MODE_DISABLED {
        return Err(ApicError::Unsupported);
    }
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    // SAFETY: CPUID verified local-APIC support on the bootstrap processor; IA32_APIC_BASE is
    // architectural, and this processor has its own copy of it.
    let previous = unsafe { rdmsr(IA32_APIC_BASE) };
    let mut value = (previous & !APIC_BASE_ADDRESS_MASK) | base | APIC_BASE_ENABLE;
    if mode == MODE_X2APIC {
        value |= APIC_BASE_X2APIC;
    } else {
        value &= !APIC_BASE_X2APIC;
    }
    // SAFETY: as above; the base and mode were validated for the bootstrap processor and are
    // properties of the platform, not of one processor.
    unsafe { wrmsr(IA32_APIC_BASE, value) };

    write_register(nolx_hal_core::apic::reg::TPR, 0);
    write_register(
        nolx_hal_core::apic::reg::LVT_TIMER,
        APIC_LVT_MASKED | u32::from(LAPIC_TIMER_VECTOR),
    );
    write_register(nolx_hal_core::apic::reg::LVT_PERFORMANCE, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_ERROR, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_LINT0, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_LINT1, APIC_LVT_MASKED);
    write_register(
        nolx_hal_core::apic::reg::SVR,
        APIC_SVR_ENABLE | u32::from(nolx_hal_core::apic::APIC_SPURIOUS_VECTOR),
    );
    Ok(())
}

/// Re-enable the bootstrap processor's local APIC after a wake reset.
///
/// A platform wake returns every processor to its power-on state: IA32_APIC_BASE loses the
/// global-enable bit (and any x2APIC selection), the software-enable bit in the spurious-vector
/// register is clear, the task priority, the local-interrupt inputs and the timer are reset.
/// The kernel's own view of the controller survived in RAM, so this restores exactly what the
/// boot path validated -- the base and mode it selected, the LINT values it resolved from the
/// MADT and the calibrated timer -- and programmes nothing new. The I/O APIC is deliberately
/// untouched: its redirection entries belong to the per-device restore that follows, and
/// masking them here would take away routes that restore is about to re-establish.
pub fn resume() -> Result<(), ApicError> {
    if !is_enabled() {
        return Err(ApicError::Unsupported);
    }
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    // SAFETY: CPUID verified local-APIC support before the first `init`; IA32_APIC_BASE is
    // architectural and this processor has its own copy of it.
    let previous = unsafe { rdmsr(IA32_APIC_BASE) };
    let mut value = (previous & !APIC_BASE_ADDRESS_MASK) | base | APIC_BASE_ENABLE;
    if is_x2apic() {
        value |= APIC_BASE_X2APIC;
    } else {
        value &= !APIC_BASE_X2APIC;
    }
    // SAFETY: the base and mode are the ones `init` validated for this platform; a wake does
    // not move the controller.
    unsafe { wrmsr(IA32_APIC_BASE, value) };

    // In x2APIC mode the register writes below are MSR accesses, which only work once the mode
    // bit above is set again -- hence the order.
    write_register(nolx_hal_core::apic::reg::TPR, 0);
    write_register(nolx_hal_core::apic::reg::LVT_PERFORMANCE, APIC_LVT_MASKED);
    write_register(nolx_hal_core::apic::reg::LVT_ERROR, APIC_LVT_MASKED);
    let lint = LINT_VALUES.load(Ordering::Acquire);
    write_register(nolx_hal_core::apic::reg::LVT_LINT0, lint as u32);
    write_register(nolx_hal_core::apic::reg::LVT_LINT1, (lint >> 32) as u32);
    write_register(
        nolx_hal_core::apic::reg::SVR,
        APIC_SVR_ENABLE | u32::from(nolx_hal_core::apic::APIC_SPURIOUS_VECTOR),
    );
    // The rate was calibrated against the HPET at boot and is a property of the platform, not
    // of the sleep, so the timer is re-armed from it rather than calibrated again.
    let _ = start_local_timer();
    Ok(())
}

/// Why an inter-processor interrupt could not be issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiError {
    /// No local-APIC interface is active, or the destination is not addressable.
    Unsupported,
    /// The previous delivery did not retire within the bounded wait.
    Busy,
}

/// Send an inter-processor interrupt to one local APIC.
///
/// xAPIC splits the command across two MMIO registers and takes the destination in bits 24-31
/// of the high half; x2APIC carries the same command in one MSR with the destination in the
/// upper 32 bits. Both wait for the delivery-status bit to clear, because a second command
/// written while one is in flight is dropped by the hardware, which would turn a processor
/// that came up slowly into one that never came up at all.
pub fn send_ipi(destination: u32, delivery: IpiDelivery, vector: u8) -> Result<(), IpiError> {
    if !is_enabled() || destination > 0xff {
        return Err(IpiError::Unsupported);
    }
    let command = icr_command(delivery, vector);
    if is_x2apic() {
        let Some(msr) = x2apic_msr(REG_ICR) else {
            return Err(IpiError::Unsupported);
        };
        // SAFETY: x2APIC mode is active, so the interrupt-command MSR is the architectural
        // interface for this processor's ICR.
        unsafe { wrmsr(msr, (u64::from(destination) << 32) | u64::from(command)) };
    } else {
        write_register(REG_ICR_HIGH, destination << 24);
        write_register(REG_ICR, command);
    }
    for _ in 0..IPI_WAIT {
        if read_register(REG_ICR) & ICR_DELIVERY_PENDING == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(IpiError::Busy)
}

/// Whether local APIC programming succeeded and the BSP controller is active.
#[must_use]
pub fn is_enabled() -> bool {
    MODE.load(Ordering::Acquire) != MODE_DISABLED
}

/// Whether the active register interface is x2APIC MSR access rather than xAPIC MMIO.
#[must_use]
pub fn is_x2apic() -> bool {
    MODE.load(Ordering::Acquire) == MODE_X2APIC
}

/// Human-readable name of the selected local-APIC register interface.
#[must_use]
pub fn mode_name() -> &'static str {
    match MODE.load(Ordering::Acquire) {
        MODE_XAPIC => "xAPIC",
        MODE_X2APIC => "x2APIC",
        _ => "disabled",
    }
}

/// Whether a vector belongs to an initialized I/O APIC or local-APIC timer source.
#[must_use]
pub const fn handles_vector(vector: u8) -> bool {
    (vector >= 32 && vector <= 47) || vector == LAPIC_TIMER_VECTOR
}

/// Whether `vector` is the APIC's spurious-vector notification (no EOI is required).
#[must_use]
pub fn is_spurious_vector(vector: u8) -> bool {
    is_enabled() && vector == nolx_hal_core::apic::APIC_SPURIOUS_VECTOR
}

/// Read the BSP's local APIC identifier.
#[must_use]
pub fn current_apic_id() -> u32 {
    let raw = read_register(nolx_hal_core::apic::reg::ID);
    if MODE.load(Ordering::Relaxed) == MODE_X2APIC {
        raw
    } else {
        raw >> 24
    }
}

/// Raise the hardware task-priority register to match a software IPL.
pub fn set_ipl(level: Ipl) {
    let tpr = u32::from(priority_class(level)) << APIC_PRIORITY_SHIFT;
    write_register(nolx_hal_core::apic::reg::TPR, tpr);
}

/// Send the local-APIC EOI for a serviced local or I/O APIC interrupt.
pub fn end_of_interrupt() {
    write_register(nolx_hal_core::apic::reg::EOI, 0);
}

/// Program all legacy IRQ routes through the initialized I/O APIC.
#[must_use]
pub fn unmask_legacy_irq(irq: u8) -> bool {
    is_enabled() && ioapic::unmask_legacy_irq(irq)
}

/// Mask a previously initialized I/O APIC route.
pub fn mask_legacy_irq(irq: u8) {
    if is_enabled() {
        ioapic::mask_legacy_irq(irq);
    }
}

/// Calibrate APIC and (when invariant) TSC rates against the HPET main counter.
pub fn calibrate_timers(
    hpet_frequency: u64,
    features: CpuFeatures,
) -> Result<TimerCalibration, ApicError> {
    if !is_enabled() || hpet_frequency == 0 {
        return Err(ApicError::CalibrationUnavailable);
    }
    let divide = timer_divide_encoding(TIMER_DIVISOR).ok_or(ApicError::CalibrationUnavailable)?;
    if hpet::counter().is_none() {
        return Err(ApicError::CalibrationUnavailable);
    }
    write_register(nolx_hal_core::apic::reg::TIMER_DIVIDE, u32::from(divide));
    write_register(
        nolx_hal_core::apic::reg::LVT_TIMER,
        APIC_LVT_MASKED | u32::from(LAPIC_TIMER_VECTOR),
    );
    write_register(nolx_hal_core::apic::reg::TIMER_INITIAL_COUNT, u32::MAX);

    let Some(hpet_start) = hpet::counter() else {
        write_register(nolx_hal_core::apic::reg::TIMER_INITIAL_COUNT, 0);
        return Err(ApicError::CalibrationUnavailable);
    };
    let tsc_start = if features.has_tsc() {
        Some(read_tsc())
    } else {
        None
    };
    let interval = (hpet_frequency / 50).max(1); // 20 ms for better calibration resolution.
    let Some(hpet_end) = hpet::wait_until_elapsed(hpet_start, interval) else {
        write_register(nolx_hal_core::apic::reg::TIMER_INITIAL_COUNT, 0);
        return Err(ApicError::CalibrationUnavailable);
    };
    let tsc_end = tsc_start.map(|_| read_tsc());
    let remaining = read_register(nolx_hal_core::apic::reg::TIMER_CURRENT_COUNT);
    write_register(nolx_hal_core::apic::reg::TIMER_INITIAL_COUNT, 0);

    let hpet_delta = hpet::elapsed_ticks(hpet_start, hpet_end);
    let lapic_delta = u64::from(u32::MAX.wrapping_sub(remaining));
    let lapic_ticks_per_second = calibrated_frequency_hz(lapic_delta, hpet_delta, hpet_frequency)
        .ok_or(ApicError::CalibrationUnavailable)?;
    if periodic_initial_count(lapic_ticks_per_second, TIMER_RATE_HZ).is_none() {
        return Err(ApicError::CalibrationUnavailable);
    }

    let tsc_frequency_hz = if features.has_tsc() && features.has_invariant_tsc() {
        match (tsc_start, tsc_end) {
            (Some(start), Some(end)) => {
                calibrated_frequency_hz(end.wrapping_sub(start), hpet_delta, hpet_frequency)
            }
            _ => None,
        }
    } else {
        None
    };
    LAPIC_TICKS_PER_SECOND.store(lapic_ticks_per_second, Ordering::Release);
    TSC_FREQUENCY_HZ.store(tsc_frequency_hz.unwrap_or(0), Ordering::Release);
    Ok(TimerCalibration {
        lapic_ticks_per_second,
        tsc_frequency_hz,
    })
}

/// Start the best calibrated local timer; return false when PIT routing is needed.
#[must_use]
pub fn start_local_timer() -> bool {
    if !is_enabled() {
        return false;
    }
    let tsc_hz = TSC_FREQUENCY_HZ.load(Ordering::Acquire);
    if TSC_DEADLINE_SUPPORTED.load(Ordering::Acquire) != 0 && tsc_hz != 0 {
        let interval = (tsc_hz / TIMER_RATE_HZ).max(1);
        let first_deadline = read_tsc().wrapping_add(interval);
        write_register(
            nolx_hal_core::apic::reg::LVT_TIMER,
            u32::from(LAPIC_TIMER_VECTOR) | APIC_LVT_TSC_DEADLINE,
        );
        NEXT_TSC_DEADLINE.store(first_deadline, Ordering::Release);
        // SAFETY: CPUID reports TSC-deadline support and calibration produced a stable TSC rate.
        unsafe { wrmsr(IA32_TSC_DEADLINE, first_deadline) };
        TIMER_MODE.store(TIMER_TSC_DEADLINE, Ordering::Release);
        return true;
    }

    let lapic_rate = LAPIC_TICKS_PER_SECOND.load(Ordering::Acquire);
    let Some(count) = periodic_initial_count(lapic_rate, TIMER_RATE_HZ) else {
        return false;
    };
    let Some(divide) = timer_divide_encoding(TIMER_DIVISOR) else {
        return false;
    };
    write_register(nolx_hal_core::apic::reg::TIMER_DIVIDE, u32::from(divide));
    write_register(
        nolx_hal_core::apic::reg::LVT_TIMER,
        u32::from(LAPIC_TIMER_VECTOR) | APIC_LVT_PERIODIC,
    );
    write_register(nolx_hal_core::apic::reg::TIMER_INITIAL_COUNT, count);
    TIMER_MODE.store(TIMER_LAPIC_PERIODIC, Ordering::Release);
    true
}

/// The calibrated timestamp-counter rate, or `None` before calibration.
#[must_use]
pub fn timestamp_frequency_hz() -> Option<u64> {
    let frequency = TSC_FREQUENCY_HZ.load(Ordering::Acquire);
    (frequency != 0).then_some(frequency)
}

/// Read the timestamp counter, when the boot path calibrated it.
///
/// This is the one clock a wake reset does not stop: the counter is per-processor and keeps
/// running across the sleep, unlike the HPET's, whose enable bit the platform clears. Delays
/// and bounded waits that must survive a resume are measured against it.
#[must_use]
pub fn timestamp() -> Option<u64> {
    timestamp_frequency_hz().map(|_| read_tsc())
}

/// Whether the configured local timer requires a new TSC deadline after each tick.
#[must_use]
pub fn uses_tsc_deadline() -> bool {
    TIMER_MODE.load(Ordering::Acquire) == TIMER_TSC_DEADLINE
}

/// Arm the next periodic TSC deadline after a deadline-mode interrupt.
pub fn rearm_tsc_deadline() {
    if !uses_tsc_deadline() {
        return;
    }
    let frequency = TSC_FREQUENCY_HZ.load(Ordering::Acquire);
    let interval = (frequency / TIMER_RATE_HZ).max(1);
    let now = read_tsc();
    let previous = NEXT_TSC_DEADLINE.load(Ordering::Relaxed);
    let candidate = previous.wrapping_add(interval);
    let next = if candidate.wrapping_sub(now) < (1u64 << 63) {
        candidate
    } else {
        now.wrapping_add(interval)
    };
    NEXT_TSC_DEADLINE.store(next, Ordering::Release);
    // SAFETY: the deadline mode was enabled after CPUID support and invariant-TSC calibration.
    unsafe { wrmsr(IA32_TSC_DEADLINE, next) };
}

fn bsp_apic_id(features: CpuFeatures) -> u32 {
    if features.has_x2apic() && features.max_leaf >= 0x0B {
        // SAFETY: the CPU reports a standard maximum leaf of at least 0x0B.
        unsafe { super::cpuid(0x0B).3 }
    } else {
        // SAFETY: CPUID leaf 1 was queried during feature detection and provides the BSP APIC ID.
        unsafe { super::cpuid(1).1 >> 24 }
    }
}

fn lint_nmi_values(records: &[LocalApicNmi], processor_uid: u32) -> Result<[u32; 2], ApicError> {
    let mut values = [APIC_LVT_MASKED; 2];
    for record in records
        .iter()
        .filter(|record| record.applies_to(processor_uid))
    {
        if record.flags & !0x000F != 0 {
            return Err(ApicError::InvalidNmiConfiguration);
        }
        let active_low = match record.flags & 0b11 {
            0 | 1 => false,
            3 => true,
            _ => return Err(ApicError::InvalidNmiConfiguration),
        };
        let level_triggered = match (record.flags >> 2) & 0b11 {
            0 | 1 => false,
            3 => true,
            _ => return Err(ApicError::InvalidNmiConfiguration),
        };
        let mut value = APIC_LVT_NMI;
        if active_low {
            value |= APIC_LVT_ACTIVE_LOW;
        }
        if level_triggered {
            value |= APIC_LVT_LEVEL_TRIGGERED;
        }
        values[usize::from(record.lint)] = value;
    }
    Ok(values)
}

fn switch_imcr_to_apic() {
    // SAFETY: ports 0x22/0x23 are the conventional PC IMCR index/data ports; this selects
    // register 0x70 and routes external interrupts through the APIC fabric after IOAPIC setup.
    unsafe {
        super::outb(IMCR_SELECT_PORT, IMCR_APIC_MODE_REGISTER);
        let value = super::inb(IMCR_DATA_PORT);
        super::outb(IMCR_SELECT_PORT, IMCR_APIC_MODE_REGISTER);
        super::outb(IMCR_DATA_PORT, value | 1);
    }
}

fn read_register(offset: u16) -> u32 {
    match MODE.load(Ordering::Acquire) {
        MODE_X2APIC => {
            let Some(msr) = x2apic_msr(offset) else {
                return 0;
            };
            // SAFETY: x2APIC mode was enabled only when CPUID advertised it; `offset` is an
            // architectural register constant mapped to its x2APIC MSR.
            unsafe { rdmsr(msr) as u32 }
        }
        MODE_XAPIC => {
            let address = crate::mm::phys::direct_ptr::<u32>(
                LOCAL_APIC_BASE.load(Ordering::Relaxed) + u64::from(offset),
            );
            // SAFETY: initialization validated this address inside the UC xAPIC MMIO window,
            // which the direct map covers.
            unsafe { ptr::read_volatile(address) }
        }
        _ => 0,
    }
}

fn write_register(offset: u16, value: u32) {
    match MODE.load(Ordering::Acquire) {
        MODE_X2APIC => {
            let Some(msr) = x2apic_msr(offset) else {
                return;
            };
            // SAFETY: x2APIC mode was enabled only when CPUID advertised it; `offset` is an
            // architectural register constant mapped to its x2APIC MSR.
            unsafe { wrmsr(msr, u64::from(value)) };
        }
        MODE_XAPIC => {
            let address = crate::mm::phys::direct_ptr::<u32>(
                LOCAL_APIC_BASE.load(Ordering::Relaxed) + u64::from(offset),
            );
            // SAFETY: initialization validated this address inside the UC xAPIC MMIO window,
            // which the direct map covers.
            unsafe { ptr::write_volatile(address, value) };
        }
        _ => {}
    }
}

fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: CPUID was used to verify the TSC before this function is used. LFENCE serializes
    // prior counter reads; RDTSC returns the value in EDX:EAX and touches no memory.
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}
