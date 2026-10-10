//! NOLX interrupt-priority-level values shared by the HAL and executive.
//!
//! Values follow the kernel model: `PASSIVE`, `APC`, `DISPATCH`, device levels 3–26, and
//! `HIGH` at 31. Levels 27–30 are reserved rather than silently acquiring semantics.

/// An IPL value permitted by the NOLX kernel model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Ipl(u8);

impl Ipl {
    /// Thread context; blocking is permitted.
    pub const PASSIVE: Self = Self(0);
    /// Asynchronous procedure-call context.
    pub const APC: Self = Self(1);
    /// Scheduler and deferred-procedure-call context.
    pub const DISPATCH: Self = Self(2);
    /// Lowest device interrupt priority, used as the default for unclassified legacy IRQs.
    pub const DEVICE_MIN: Self = Self(3);
    /// Highest device interrupt priority.
    pub const DEVICE_MAX: Self = Self(26);
    /// NMI/machine-check and other non-maskable high-priority context.
    pub const HIGH: Self = Self(31);

    /// Construct a device IPL in the documented inclusive range `3..=26`.
    #[must_use]
    pub const fn device(priority: u8) -> Option<Self> {
        if priority >= Self::DEVICE_MIN.0 && priority <= Self::DEVICE_MAX.0 {
            Some(Self(priority))
        } else {
            None
        }
    }

    /// Decode a raw per-CPU IPL, rejecting values reserved by the kernel model.
    #[must_use]
    pub const fn from_raw(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::PASSIVE),
            1 => Some(Self::APC),
            2 => Some(Self::DISPATCH),
            3..=26 => Some(Self(value)),
            31 => Some(Self::HIGH),
            _ => None,
        }
    }

    /// Return the numeric IPL value stored in per-CPU data.
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_ipl_round_trips_and_levels_are_ordered() {
        let levels = [
            Ipl::PASSIVE,
            Ipl::APC,
            Ipl::DISPATCH,
            Ipl::DEVICE_MIN,
            Ipl::DEVICE_MAX,
            Ipl::HIGH,
        ];
        for level in levels {
            assert_eq!(Ipl::from_raw(level.value()), Some(level));
        }
        for raw in Ipl::DEVICE_MIN.value()..=Ipl::DEVICE_MAX.value() {
            let level = Ipl::device(raw).unwrap();
            assert_eq!(level.value(), raw);
            assert_eq!(Ipl::from_raw(raw), Some(level));
        }
        for raw in 0..=Ipl::HIGH.value() {
            match Ipl::from_raw(raw) {
                Some(level) => assert_eq!(level.value(), raw),
                None => assert!((27..=30).contains(&raw)),
            }
        }
        assert!(Ipl::PASSIVE < Ipl::APC);
        assert!(Ipl::APC < Ipl::DISPATCH);
        assert!(Ipl::DISPATCH < Ipl::DEVICE_MIN);
        assert!(Ipl::DEVICE_MAX < Ipl::HIGH);
    }

    #[test]
    fn reserved_values_and_out_of_range_device_priorities_are_rejected() {
        for raw in [27, 28, 29, 30, 32, u8::MAX] {
            assert_eq!(Ipl::from_raw(raw), None);
        }
        assert_eq!(Ipl::device(2), None);
        assert_eq!(Ipl::device(27), None);
    }
}
