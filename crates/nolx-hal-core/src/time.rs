//! Timer rate, wrapping-counter, and monotonic-clock conversions.

/// The nominal input frequency of the PC/AT Programmable Interval Timer, in hertz.
pub const PIT_INPUT_HZ: u64 = 1_193_182;
/// NOLX's early boot timer target, in interrupts per second.
pub const PIT_TARGET_HZ: u64 = 100;
/// Channel-zero divisor rounded to the nearest integer for the requested PIT rate.
pub const PIT_DIVISOR: u16 = ((PIT_INPUT_HZ + (PIT_TARGET_HZ / 2)) / PIT_TARGET_HZ) as u16;
/// Nanoseconds in one second.
pub const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

/// Extend a sampled 32-bit counter using its previous 64-bit epoch, saturating at `u64::MAX`.
///
/// Samples are expected at intervals shorter than half the 32-bit counter period. Small
/// backward movements and stale pre-wrap samples are treated as out-of-order reads, so the
/// result never regresses when callers race across CPUs.
#[must_use]
pub fn extend_u32_counter(previous: u64, current: u32) -> u64 {
    let previous_low = previous as u32;
    let high = previous & !u64::from(u32::MAX);
    if current < previous_low {
        if previous_low.wrapping_sub(current) > 0x8000_0000 {
            high.saturating_add(1u64 << 32) | u64::from(current)
        } else {
            previous
        }
    } else if current.wrapping_sub(previous_low) > 0x8000_0000 {
        previous
    } else {
        high | u64::from(current)
    }
}

/// Convert elapsed PIT interrupts to nanoseconds using the programmed divisor.
///
/// The calculation uses `u128` intermediates so a long-running kernel cannot overflow while
/// converting the monotonic tick counter. The result saturates only if nanoseconds exceed
/// the representable `u64` range.
#[must_use]
pub const fn pit_ticks_to_ns(ticks: u64) -> u64 {
    let ns = (ticks as u128 * PIT_DIVISOR as u128 * NANOSECONDS_PER_SECOND as u128)
        / PIT_INPUT_HZ as u128;
    if ns > u64::MAX as u128 {
        u64::MAX
    } else {
        ns as u64
    }
}

/// Combine a whole-second Unix epoch anchor with elapsed monotonic nanoseconds.
///
/// The result saturates at `u64::MAX` rather than wrapping into a date before the epoch.
#[must_use]
pub const fn realtime_ns(epoch_seconds: u64, elapsed_monotonic_ns: u64) -> u64 {
    epoch_seconds
        .saturating_mul(NANOSECONDS_PER_SECOND)
        .saturating_add(elapsed_monotonic_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisor_is_in_the_pit_16_bit_range_and_near_100_hz() {
        let actual_hz = PIT_INPUT_HZ as f64 / f64::from(PIT_DIVISOR);
        assert!((actual_hz - PIT_TARGET_HZ as f64).abs() < 0.01);
    }

    #[test]
    fn monotonic_clock_starts_at_zero_and_tracks_the_programmed_rate() {
        assert_eq!(pit_ticks_to_ns(0), 0);
        let one_second = pit_ticks_to_ns(PIT_TARGET_HZ);
        assert!((999_000_000..=1_001_000_000).contains(&one_second));
    }

    #[test]
    fn conversion_saturates_instead_of_wrapping() {
        assert_eq!(pit_ticks_to_ns(u64::MAX), u64::MAX);
    }

    #[test]
    fn counter_32bit_extension_tracks_wrap_and_ignores_stale_samples() {
        assert_eq!(extend_u32_counter(0xFFFF_FFF0, 5), 0x1_0000_0005);
        assert_eq!(
            extend_u32_counter(0x1_0000_0005, 0xFFFF_FFF0),
            0x1_0000_0005
        );
        assert_eq!(extend_u32_counter(0xFFFF_FFF0, 0xFFFF_FFE0), 0xFFFF_FFF0);
        assert_eq!(extend_u32_counter(0x1_0000_0005, 0x20), 0x1_0000_0020);
    }

    #[test]
    fn counter_32bit_extension_saturates_at_the_u64_limit() {
        assert_eq!(extend_u32_counter(0xFFFF_FFFF_FFFF_FFF0, 5), u64::MAX,);
    }

    #[test]
    fn realtime_clock_advances_from_its_epoch_anchor_and_saturates() {
        assert_eq!(realtime_ns(1_700_000_000, 25), 1_700_000_000_000_000_025);
        assert_eq!(realtime_ns(u64::MAX, 1), u64::MAX);
    }
}
