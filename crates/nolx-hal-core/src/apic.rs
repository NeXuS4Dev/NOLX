//! Portable local-APIC register definitions and timer arithmetic.

use crate::ipl::Ipl;

/// Legacy APIC-base MSR.
pub const IA32_APIC_BASE: u32 = 0x1B;
/// TSC-deadline MSR.
pub const IA32_TSC_DEADLINE: u32 = 0x6E0;
/// APIC timer vector reserved by NOLX.
pub const LAPIC_TIMER_VECTOR: u8 = 48;
/// APIC spurious-vector register value.
pub const APIC_SPURIOUS_VECTOR: u8 = 255;

/// xAPIC register offsets; each register occupies a 16-byte slot.
pub mod reg {
    /// Local APIC ID.
    pub const ID: u16 = 0x020;
    /// Task-priority register.
    pub const TPR: u16 = 0x080;
    /// End-of-interrupt register.
    pub const EOI: u16 = 0x0B0;
    /// Spurious-interrupt vector register.
    pub const SVR: u16 = 0x0F0;
    /// Performance-monitoring LVT entry.
    pub const LVT_PERFORMANCE: u16 = 0x340;
    /// Local interrupt input zero.
    pub const LVT_LINT0: u16 = 0x350;
    /// Local interrupt input one.
    pub const LVT_LINT1: u16 = 0x360;
    /// Error LVT entry.
    pub const LVT_ERROR: u16 = 0x370;
    /// Timer LVT entry.
    pub const LVT_TIMER: u16 = 0x320;
    /// Initial timer count.
    pub const TIMER_INITIAL_COUNT: u16 = 0x380;
    /// Current timer count.
    pub const TIMER_CURRENT_COUNT: u16 = 0x390;
    /// Timer divide configuration.
    pub const TIMER_DIVIDE: u16 = 0x3E0;
}

/// Interrupt-command register (low half in xAPIC mode).
pub const REG_ICR: u16 = 0x300;
/// Interrupt-command register (destination half in xAPIC mode).
pub const REG_ICR_HIGH: u16 = 0x310;

/// Bit 12 of the interrupt-command register: a delivery is still in progress.
pub const ICR_DELIVERY_PENDING: u32 = 1 << 12;
/// Bit 14: assert the level for a level-triggered delivery such as `INIT`.
pub const ICR_LEVEL_ASSERT: u32 = 1 << 14;

/// How an inter-processor interrupt is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiDelivery {
    /// Deliver the vector as an ordinary interrupt.
    Fixed,
    /// Reset the target processor, which then waits for a startup IPI.
    Init,
    /// Start the target processor at `vector << 12`.
    Startup,
}

impl IpiDelivery {
    /// The delivery-mode field an ICR takes.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Fixed => 0,
            Self::Init => 5,
            Self::Startup => 6,
        }
    }
}

/// The interrupt-command value for `vector`, without a destination.
///
/// `INIT` is level-asserted: it is delivered as an edge only after the level is asserted, and
/// the architecture requires the assert bit for the delivery to take effect. A startup IPI
/// carries its target address in the vector field and is never asserted this way.
#[must_use]
pub const fn icr_command(delivery: IpiDelivery, vector: u8) -> u32 {
    let mut value = (delivery.code() << 8) | vector as u32;
    if matches!(delivery, IpiDelivery::Init) {
        value |= ICR_LEVEL_ASSERT;
    }
    value
}

/// Convert an aligned xAPIC register offset to its x2APIC MSR number.
#[must_use]
pub const fn x2apic_msr(register_offset: u16) -> Option<u32> {
    if register_offset > 0x3F0 || register_offset & 0xF != 0 {
        None
    } else {
        Some(0x800 + (register_offset as u32 >> 4))
    }
}

/// Compress a 0–31 NOLX IPL into the APIC's four-bit priority class.
///
/// The APIC provides sixteen preemption classes, so adjacent device IPLs share a hardware
/// class. Software retains the exact IPL; the compressed class is a conservative hardware
/// mask and the NOLX dispatcher remains responsible for exact-level policy.
#[must_use]
pub const fn priority_class(level: Ipl) -> u8 {
    match level.value() {
        0 => 0,
        1 => 1,
        2 => 2,
        3..=26 => 3 + (level.value() - 3) / 2,
        _ => 15,
    }
}

/// Encode a supported APIC timer divisor as the architectural divide-configuration value.
#[must_use]
pub const fn timer_divide_encoding(divisor: u16) -> Option<u8> {
    match divisor {
        1 => Some(0b111),
        2 => Some(0b000),
        4 => Some(0b001),
        8 => Some(0b010),
        16 => Some(0b011),
        32 => Some(0b100),
        64 => Some(0b101),
        128 => Some(0b110),
        _ => None,
    }
}

/// Calculate the APIC timer's initial count for a requested periodic rate.
#[must_use]
pub const fn periodic_initial_count(ticks_per_second: u64, rate_hz: u64) -> Option<u32> {
    if rate_hz == 0 {
        return None;
    }
    let count = ticks_per_second / rate_hz;
    if count == 0 || count > u32::MAX as u64 {
        None
    } else {
        Some(count as u32)
    }
}

/// Derive a frequency from the HPET's femtosecond period, rejecting a zero period.
#[must_use]
pub const fn hpet_frequency_hz(period_femtoseconds: u32) -> Option<u64> {
    if period_femtoseconds == 0 {
        None
    } else {
        Some(1_000_000_000_000_000u64 / period_femtoseconds as u64)
    }
}

/// Calibrate a counter against an HPET interval without overflowing intermediate products.
#[must_use]
pub const fn calibrated_frequency_hz(
    counter_delta: u64,
    hpet_delta: u64,
    hpet_frequency_hz: u64,
) -> Option<u64> {
    if counter_delta == 0 || hpet_delta == 0 || hpet_frequency_hz == 0 {
        return None;
    }
    let frequency = (counter_delta as u128 * hpet_frequency_hz as u128) / hpet_delta as u128;
    if frequency == 0 || frequency > u64::MAX as u128 {
        None
    } else {
        Some(frequency as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x2apic_register_mapping_matches_architectural_msr_offsets() {
        assert_eq!(x2apic_msr(reg::ID), Some(0x802));
        assert_eq!(x2apic_msr(reg::TPR), Some(0x808));
        assert_eq!(x2apic_msr(reg::EOI), Some(0x80B));
        assert_eq!(x2apic_msr(reg::SVR), Some(0x80F));
        assert_eq!(x2apic_msr(reg::LVT_TIMER), Some(0x832));
        assert_eq!(x2apic_msr(reg::TIMER_INITIAL_COUNT), Some(0x838));
        assert_eq!(x2apic_msr(reg::TIMER_DIVIDE), Some(0x83E));
        assert_eq!(x2apic_msr(0x081), None);
        assert_eq!(x2apic_msr(0x400), None);
    }

    #[test]
    fn hardware_priority_class_is_monotonic_and_within_tpr_range() {
        assert_eq!(priority_class(Ipl::PASSIVE), 0);
        assert_eq!(priority_class(Ipl::APC), 1);
        assert_eq!(priority_class(Ipl::DISPATCH), 2);
        assert_eq!(priority_class(Ipl::DEVICE_MIN), 3);
        assert_eq!(priority_class(Ipl::DEVICE_MAX), 14);
        assert_eq!(priority_class(Ipl::HIGH), 15);
        let mut previous = 0;
        for raw in Ipl::DEVICE_MIN.value()..=Ipl::DEVICE_MAX.value() {
            let level = Ipl::device(raw).unwrap();
            let class = priority_class(level);
            assert!(class >= previous);
            assert!(class < 16);
            previous = class;
        }
    }

    #[test]
    fn timer_divisors_period_counts_and_calibration_are_checked() {
        assert_eq!(timer_divide_encoding(1), Some(0b111));
        assert_eq!(timer_divide_encoding(16), Some(0b011));
        assert_eq!(timer_divide_encoding(128), Some(0b110));
        assert_eq!(timer_divide_encoding(3), None);
        assert_eq!(periodic_initial_count(1_000_000, 100), Some(10_000));
        assert_eq!(periodic_initial_count(99, 100), None);
        assert_eq!(periodic_initial_count(100, 0), None);
        assert_eq!(periodic_initial_count(u64::MAX, 1), None);
        assert_eq!(hpet_frequency_hz(100_000_000), Some(10_000_000));
        assert_eq!(hpet_frequency_hz(0), None);
        assert_eq!(
            calibrated_frequency_hz(250_000, 10_000, 10_000_000),
            Some(250_000_000)
        );
        assert_eq!(calibrated_frequency_hz(0, 1, 1), None);
        assert_eq!(calibrated_frequency_hz(1, 0, 1), None);
    }

    #[test]
    fn an_init_ipi_is_level_asserted_and_a_startup_ipi_names_its_page() {
        // INIT: delivery mode 5, no vector, assert bit 14 -- the encoding every x86 firmware
        // expects before the startup IPI that follows it.
        assert_eq!(icr_command(IpiDelivery::Init, 0), 0x4500);
        // Startup vector 0x0f starts the target at 0x0f000.
        assert_eq!(icr_command(IpiDelivery::Startup, 0x0f), 0x060f);
        // A fixed IPI is just the vector.
        assert_eq!(icr_command(IpiDelivery::Fixed, 0xf0), 0xf0);
        assert_eq!(IpiDelivery::Init.code(), 5);
        assert_eq!(IpiDelivery::Startup.code(), 6);
        assert_eq!(IpiDelivery::Fixed.code(), 0);
    }

    #[test]
    fn the_icr_is_addressable_in_both_register_models() {
        assert_eq!(x2apic_msr(REG_ICR), Some(0x830));
        // The destination half has no x2APIC counterpart: x2APIC carries the destination in
        // the upper 32 bits of the one register.
        assert_eq!(REG_ICR_HIGH, 0x310);
    }
}
