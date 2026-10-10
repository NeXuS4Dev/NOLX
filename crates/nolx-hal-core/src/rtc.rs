//! Pure decoding and epoch conversion for the MC146818-compatible real-time clock.
//!
//! The CMOS clock exposes either binary or packed-BCD fields and either 12-hour or
//! 24-hour time. This module validates those encodings and converts the resulting civil
//! time to seconds since 1970-01-01T00:00:00 UTC. A PC-compatible century register is not
//! standardized, so the initial policy maps years 70–99 to 1970–1999 and 00–69 to
//! 2000–2069.

/// CMOS status-register B: the RTC is in binary-data mode rather than packed BCD.
pub const STATUS_B_BINARY: u8 = 1 << 2;
/// CMOS status-register B: the RTC is in 24-hour mode rather than 12-hour mode.
pub const STATUS_B_24_HOUR: u8 = 1 << 1;
/// CMOS status-register B: updates are inhibited while the SET bit is set.
pub const STATUS_B_SET: u8 = 1 << 7;

/// Raw calendar and time fields read from the RTC, plus status register B.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcRegisters {
    /// Seconds register, `0..=59` in binary or packed BCD.
    pub second: u8,
    /// Minutes register, `0..=59` in binary or packed BCD.
    pub minute: u8,
    /// Hours register; bit 7 is the PM flag in 12-hour mode.
    pub hour: u8,
    /// Day-of-month register.
    pub day: u8,
    /// Month register, January is 1.
    pub month: u8,
    /// Two-digit year register.
    pub year: u8,
    /// The RTC's status register B.
    pub status_b: u8,
}

/// A validated Gregorian civil date and time interpreted as UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl RtcDateTime {
    /// The full year in the supported window `1970..=2069`.
    #[must_use]
    pub const fn year(self) -> u16 {
        self.year
    }

    /// The month, `1..=12`.
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of month, starting at 1.
    #[must_use]
    pub const fn day(self) -> u8 {
        self.day
    }

    /// The hour in 24-hour form, `0..=23`.
    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// The minute, `0..=59`.
    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// The second, `0..=59`.
    #[must_use]
    pub const fn second(self) -> u8 {
        self.second
    }

    /// Convert this validated time to seconds since 1970-01-01T00:00:00 UTC.
    #[must_use]
    pub fn unix_seconds(self) -> u64 {
        let mut days = 0_u64;
        let mut year = 1970_u16;
        while year < self.year {
            days += if is_leap_year(year) { 366 } else { 365 };
            year += 1;
        }

        let mut month = 1_u8;
        while month < self.month {
            days += u64::from(days_in_month(self.year, month).expect("validated month"));
            month += 1;
        }
        days += u64::from(self.day - 1);

        days * 86_400
            + u64::from(self.hour) * 3_600
            + u64::from(self.minute) * 60
            + u64::from(self.second)
    }
}

/// Decode raw RTC registers, returning `None` for an invalid or unsupported date/time.
#[must_use]
pub fn decode(registers: RtcRegisters) -> Option<RtcDateTime> {
    if registers.status_b & STATUS_B_SET != 0 {
        return None;
    }

    let binary = registers.status_b & STATUS_B_BINARY != 0;
    let second = decode_field(registers.second, binary)?;
    let minute = decode_field(registers.minute, binary)?;
    let month = decode_field(registers.month, binary)?;
    let day = decode_field(registers.day, binary)?;
    let year_short = decode_field(registers.year, binary)?;
    if second > 59 || minute > 59 || !(1..=12).contains(&month) || year_short > 99 {
        return None;
    }

    let hour = if registers.status_b & STATUS_B_24_HOUR != 0 {
        let hour = decode_field(registers.hour, binary)?;
        if hour > 23 || registers.hour & 0x80 != 0 {
            return None;
        }
        hour
    } else {
        let pm = registers.hour & 0x80 != 0;
        let hour_12 = decode_field(registers.hour & 0x7F, binary)?;
        if !(1..=12).contains(&hour_12) {
            return None;
        }
        (hour_12 % 12) + if pm { 12 } else { 0 }
    };

    let year = if year_short >= 70 {
        1900 + u16::from(year_short)
    } else {
        2000 + u16::from(year_short)
    };
    let max_day = days_in_month(year, month)?;
    if day == 0 || day > max_day {
        return None;
    }

    Some(RtcDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

fn decode_field(value: u8, binary: bool) -> Option<u8> {
    if binary {
        return Some(value);
    }

    let low = value & 0x0F;
    let high = (value >> 4) & 0x0F;
    if low > 9 || high > 9 {
        return None;
    }
    Some(high * 10 + low)
}

#[allow(clippy::manual_is_multiple_of)] // `is_multiple_of` is newer than the workspace MSRV.
fn is_leap_year(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: u16, month: u8) -> Option<u8> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bcd_time(year: u8, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> RtcRegisters {
        RtcRegisters {
            second,
            minute,
            hour,
            day,
            month,
            year,
            status_b: 0,
        }
    }

    #[test]
    fn bcd_12_hour_time_decodes_noon_and_midnight() {
        let noon = decode(bcd_time(0x24, 0x06, 0x15, 0x92, 0x34, 0x56)).unwrap();
        assert_eq!(noon.hour, 12);
        assert_eq!((noon.year, noon.month, noon.day), (2024, 6, 15));

        let midnight = decode(bcd_time(0x24, 0x06, 0x15, 0x12, 0, 0)).unwrap();
        assert_eq!(midnight.hour, 0);
    }

    #[test]
    fn binary_24_hour_epoch_and_year_pivot_are_correct() {
        let epoch = RtcRegisters {
            second: 0,
            minute: 0,
            hour: 0,
            day: 1,
            month: 1,
            year: 70,
            status_b: STATUS_B_BINARY | STATUS_B_24_HOUR,
        };
        let decoded = decode(epoch).unwrap();
        assert_eq!(decoded.year, 1970);
        assert_eq!(decoded.unix_seconds(), 0);

        let y2k = decode(RtcRegisters { year: 0, ..epoch }).unwrap();
        assert_eq!(y2k.year, 2000);
    }

    #[test]
    fn leap_days_and_month_lengths_are_validated() {
        let leap_day = decode(bcd_time(0x24, 0x02, 0x29, 0x12, 0, 0)).unwrap();
        assert_eq!(leap_day.year, 2024);
        assert_eq!(leap_day.hour, 0);

        assert!(decode(bcd_time(0x23, 0x02, 0x29, 0x12, 0, 0)).is_none());
        assert!(decode(bcd_time(0x24, 0x04, 0x31, 0x12, 0, 0)).is_none());
    }

    #[test]
    fn invalid_bcd_fields_and_update_inhibit_are_rejected() {
        assert!(decode(bcd_time(0x2A, 0x01, 0x01, 0x12, 0, 0)).is_none());
        assert!(decode(RtcRegisters {
            status_b: STATUS_B_SET,
            ..bcd_time(0x24, 0x01, 0x01, 0x12, 0, 0)
        })
        .is_none());
    }

    #[test]
    fn epoch_conversion_accounts_for_leap_days_and_time_of_day() {
        let time = decode(bcd_time(0x24, 0x03, 0x01, 0x12, 0, 0)).unwrap();
        assert_eq!(time.unix_seconds(), 1_709_251_200);
    }
}
