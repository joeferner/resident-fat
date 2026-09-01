//! Dates and times, in the shape FAT stores them.
//!
//! This is a small module carrying a lot of the risk, which is why it is
//! separate and why it is tested on its own. FAT's timestamps are three
//! packed fields with an epoch, a ceiling, and two different resolutions,
//! and every one of those is somewhere an implementation quietly produces a
//! date no tool will accept:
//!
//! * The epoch is **1980**, and the year is seven bits, so the last year is
//!   **2107**. A Unix timestamp from before 1980 has nowhere to go.
//! * Seconds are stored **halved**, so the `time` field can only express
//!   even seconds. The odd second lives in a *separate* field alongside
//!   hundredths, and only the creation timestamp has one.
//! * A date of all zero bits is month 0, day 0 — not the epoch, but an
//!   impossible date, which listing tools render as `1980-00-00`.
//!   [`DateTime::EPOCH`] packs to `0x0021`, and that is what an entry with
//!   no known time should carry. `fsck.vfat` does not check dates at all,
//!   so nothing outside this crate will catch getting it wrong.
//!
//! # Where the time comes from
//!
//! Nothing in this module reads a clock; it converts. What supplies the
//! instant is a [`Clock`], which a consumer hands to
//! [`FileSystem::set_clock`](crate::FileSystem::set_clock) — and which is
//! optional, because a board that does not know the time should still be
//! able to write a file. Without one, everything is stamped
//! [`DateTime::EPOCH`], a date every tool accepts and none mistakes for
//! real information.
//!
//! [`EpochClock`] is that default. [`FnClock`] wraps a plain function, for
//! the common bare-metal shape where the wall clock is a `static` set once
//! the network has answered.

/// Where a filesystem gets the time to stamp on what it writes.
///
/// **Optional, and defaulting to the epoch.** A board with no
/// battery-backed clock believes it is 1970 until something tells it
/// otherwise, and that something is usually the network — so a filesystem
/// that could not write a file until the time was known would make an
/// over-the-air update depend on whether NTP had succeeded. It should not.
/// A file stamped 1980-01-01 is a small loss; a file that cannot be written
/// is a large one.
///
/// So the default is [`EpochClock`], and supplying a real one through
/// [`FileSystem::set_clock`](crate::FileSystem::set_clock) is an
/// improvement a consumer opts into rather than a requirement it has to
/// satisfy.
///
/// # Implementing one
///
/// Return whatever the device believes, in UTC. FAT stores local time with
/// no zone attached, so there is no correct answer available to this crate
/// — a consumer that knows its zone and would rather stamp local time
/// should return local time, and one that does not should return UTC and be
/// consistent about it.
///
/// A clock that has not been set yet should return [`DateTime::EPOCH`]
/// rather than a guess. Out-of-range values are clamped rather than
/// wrapped, so nothing here can produce a date from the wrong century, but
/// an implementation reporting 1970 will have every file stamped 1980 and
/// that is the honest outcome.
pub trait Clock {
    /// The current date and time.
    fn now(&self) -> DateTime;
}

/// A clock that always reports the FAT epoch, and the default.
///
/// Not a placeholder to be replaced before shipping: it is the right answer
/// for a device that genuinely does not know the time, and 1980-01-01 is a
/// date every tool accepts. The alternative that implementations reach for
/// — leaving the fields zero — is month 0 of day 0, which is not a date at
/// all.
#[derive(Debug, Clone, Copy, Default)]
pub struct EpochClock;

impl Clock for EpochClock {
    /// Always [`DateTime::EPOCH`]. A device that does not know the time
    /// says so with a date that exists, rather than guessing.
    fn now(&self) -> DateTime {
        DateTime::EPOCH
    }
}

/// A clock built from a function, for a consumer whose time comes from a
/// global rather than from a value it can hold.
///
/// The common shape on a bare-metal board: the wall clock is an offset in a
/// `static`, set once the network has answered, and reading it needs no
/// state at all.
///
/// ```
/// use resident_fat::time::{DateTime, FnClock};
/// let clock = FnClock::new(|| DateTime::from_unix_seconds(1_767_225_600));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FnClock(fn() -> DateTime);

impl FnClock {
    /// Wraps `now`.
    pub const fn new(now: fn() -> DateTime) -> Self {
        FnClock(now)
    }
}

impl Clock for FnClock {
    /// Whatever the wrapped function returns, called afresh each time —
    /// this holds a function pointer, not a cached instant.
    fn now(&self) -> DateTime {
        (self.0)()
    }
}

/// The earliest year FAT can express. Its own epoch.
pub const FIRST_YEAR: u16 = 1980;

/// The latest year FAT can express: 1980 plus the seven-bit year field.
pub const LAST_YEAR: u16 = 2107;

/// Days from 1970-01-01 to 1980-01-01, the FAT epoch.
const UNIX_TO_FAT_DAYS: i64 = 3652;

/// A date and time, to the millisecond.
///
/// Plain civil fields rather than a count since an epoch, because that is
/// what both ends of this conversion want: the packed form is fields, and a
/// consumer's clock is usually either fields already or a Unix time that
/// [`from_unix_seconds`](Self::from_unix_seconds) converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    /// Full year, such as 2026.
    pub year: u16,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day of the month, starting at 1.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 59.
    pub second: u8,
    /// Millisecond, 0 to 999. Only creation times keep any of this, and
    /// then only to the nearest 10 ms.
    pub millisecond: u16,
}

/// A timestamp packed the way a directory entry stores one.
///
/// Three fields rather than one number, because FAT really does split a
/// timestamp across three: the entry has a date, a time, and — for the
/// creation timestamp alone — a field holding the second the time field
/// could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packed {
    /// Year since 1980 in bits 15..9, month in 8..5, day in 4..0.
    pub date: u16,
    /// Hour in bits 15..11, minute in 10..5, **half** the second in 4..0.
    pub time: u16,
    /// Hundredths of a second, 0 to 199.
    ///
    /// Named `CrtTimeTenth` by the specification, which is a misnomer worth
    /// knowing about: the unit is 10 ms, so the field counts hundredths and
    /// runs to 199 rather than to 9. The extra hundred is the odd second
    /// [`time`](Self::time) had to drop.
    pub hundredths: u8,
}

impl DateTime {
    /// The FAT epoch, 1980-01-01 00:00:00.
    ///
    /// The right value for an entry whose real time is unknown: it is the
    /// earliest date the format can express, and unlike an all-zero date it
    /// is a date that exists.
    pub const EPOCH: DateTime = DateTime {
        year: FIRST_YEAR,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        millisecond: 0,
    };

    /// The last instant FAT can express, 2107-12-31 23:59:59.990.
    ///
    /// The odd second and the hundredths both come from the creation
    /// timestamp's extra field, so an entry without one tops out at
    /// 23:59:58.
    pub const LATEST: DateTime = DateTime {
        year: LAST_YEAR,
        month: 12,
        day: 31,
        hour: 23,
        minute: 59,
        second: 59,
        millisecond: 990,
    };

    /// A date and time, or `None` if it is not one.
    ///
    /// Every field is range-checked against the others, so 31 February and
    /// 29 February 2100 are both refused while 29 February 2000 is not —
    /// the century rule is the part of the leap-year calculation that
    /// implementations skip, and it is the part that goes wrong exactly
    /// once every hundred years.
    ///
    /// The range accepted is wider than FAT can store. Refusing a valid
    /// date because it is out of range would put clamping in the caller's
    /// hands, and the clamp belongs where the format's limits are — see
    /// [`pack`](Self::pack).
    pub fn new(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> Option<Self> {
        if !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
            || millisecond > 999
        {
            return None;
        }
        Some(DateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            millisecond,
        })
    }

    /// The instant `seconds` after 1970-01-01 00:00:00 UTC, clamped into
    /// what FAT can hold.
    ///
    /// Clamped rather than refused, because the usual caller is a clock and
    /// the usual out-of-range value is a device that has not learnt the
    /// time yet. A file stamped at the epoch is better than a file that
    /// cannot be written, and a great deal better than one stamped with the
    /// low bits of whatever the clock said.
    pub fn from_unix_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);

        // Below the epoch there is no year to represent, so the fields have
        // to come from the epoch rather than from the calculation.
        if year < i32::from(FIRST_YEAR) {
            return DateTime::EPOCH;
        }
        if year > i32::from(LAST_YEAR) {
            return DateTime::LATEST;
        }

        DateTime {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: (rest / 3600) as u8,
            minute: (rest / 60 % 60) as u8,
            second: (rest % 60) as u8,
            millisecond: 0,
        }
    }

    /// Seconds since 1970-01-01 00:00:00 UTC, dropping the milliseconds.
    pub fn to_unix_seconds(&self) -> i64 {
        days_from_civil(
            i32::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        ) * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// Days since 1980-01-01, which is what a FAT date field counts in.
    ///
    /// Only meaningful for a date the format can hold; a date outside the
    /// range gives a number outside it too.
    pub fn days_since_fat_epoch(&self) -> i64 {
        days_from_civil(
            i32::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        ) - UNIX_TO_FAT_DAYS
    }

    /// Packs into the three fields a directory entry stores, clamping to
    /// the range FAT can express.
    ///
    /// Clamping is at both ends and is silent, for the reason given on
    /// [`from_unix_seconds`](Self::from_unix_seconds): the alternative is a
    /// write that fails because a clock is wrong, or a date field holding
    /// the low bits of a year it could not fit.
    pub fn pack(&self) -> Packed {
        let clamped = self.clamp_to_fat();
        Packed {
            date: ((clamped.year - FIRST_YEAR) << 9)
                | (u16::from(clamped.month) << 5)
                | u16::from(clamped.day),
            // Halved, losing the odd second, which `hundredths` carries.
            time: (u16::from(clamped.hour) << 11)
                | (u16::from(clamped.minute) << 5)
                | u16::from(clamped.second / 2),
            hundredths: (clamped.second % 2) * 100 + (clamped.millisecond / 10) as u8,
        }
    }

    /// Unpacks a directory entry's fields.
    ///
    /// `hundredths` is the creation timestamp's extra field; pass zero for
    /// the modification and access timestamps, which have none. A date of
    /// zero — the all-zero entry an implementation with no clock writes —
    /// comes back as [`EPOCH`](Self::EPOCH) rather than as month 0 day 0.
    ///
    /// Otherwise the fields are reported as they were stored, without
    /// being checked: these bytes came off a volume this crate did not
    /// write, and reporting 31 February faithfully is more use than
    /// substituting a plausible date for a corrupt one.
    pub fn unpack(packed: Packed) -> Self {
        if packed.date == 0 {
            return DateTime::EPOCH;
        }
        let month = ((packed.date >> 5) & 0x0F) as u8;
        let day = (packed.date & 0x1F) as u8;
        let year = FIRST_YEAR + (packed.date >> 9);
        let hundredths = packed.hundredths.min(199);

        DateTime {
            year,
            month,
            day,
            hour: ((packed.time >> 11) & 0x1F) as u8,
            minute: ((packed.time >> 5) & 0x3F) as u8,
            second: ((packed.time & 0x1F) * 2) as u8 + hundredths / 100,
            millisecond: u16::from(hundredths % 100) * 10,
        }
    }

    /// This instant, moved to the nearest end of the representable range if
    /// it is outside it.
    fn clamp_to_fat(&self) -> Self {
        if *self < DateTime::EPOCH {
            DateTime::EPOCH
        } else if *self > DateTime::LATEST {
            DateTime::LATEST
        } else {
            *self
        }
    }
}

impl Default for DateTime {
    /// [`DateTime::EPOCH`], not an all-zero date. Zero is month 0 of day 0,
    /// which is not a date at all; see this module's header.
    fn default() -> Self {
        DateTime::EPOCH
    }
}

/// Whether `year` has a 29 February.
///
/// Divisible by four, except centuries, except every fourth century. 2000
/// is a leap year and 2100 is not, and both fall inside what FAT can store.
pub fn is_leap_year(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// How many days `month` has in `year`.
fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

// The civil-date conversions below are Howard Hinnant's `days_from_civil`
// and `civil_from_days`, which shift the year to start in March so that the
// leap day lands at the end of it. That removes the special case entirely:
// the century rule falls out of the 400-year cycle being exactly 146097
// days, rather than being a branch someone has to remember to write.

/// Days from 1970-01-01 to `year-month-day`.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(if month <= 2 { year - 1 } else { year });
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * i64::from(if month > 2 { month - 3 } else { month + 9 }) + 2) / 5
        + i64::from(day)
        - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (
        (if month <= 2 { year + 1 } else { year }) as i32,
        month,
        day,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> DateTime {
        DateTime::new(year, month, day, hour, minute, second, millisecond)
            .expect("the test named a date that does not exist")
    }

    /// The epoch packs to a date that exists, which an all-zero field does
    /// not: zero is month 0 of day 0.
    #[test]
    fn the_epoch_is_not_all_zeroes() {
        let packed = DateTime::EPOCH.pack();
        assert_eq!(packed.date, 0x0021, "1980-01-01 is year 0, month 1, day 1");
        assert_eq!(packed.time, 0);
        assert_eq!(packed.hundredths, 0);
    }

    /// An all-zero date reads back as the epoch rather than as month 0.
    #[test]
    fn an_all_zero_date_unpacks_to_the_epoch() {
        let zero = Packed {
            date: 0,
            time: 0,
            hundredths: 0,
        };
        assert_eq!(DateTime::unpack(zero), DateTime::EPOCH);
    }

    /// The last instant the format can express uses every bit of the year
    /// field.
    #[test]
    fn the_ceiling_fills_the_year_field() {
        let packed = DateTime::LATEST.pack();
        assert_eq!(packed.date >> 9, 127, "2107 is 1980 plus a full seven bits");
        assert_eq!(packed.date, (127 << 9) | (12 << 5) | 31);
        // 23:59:59.99 -- the time field holds 58 seconds, the extra field
        // the odd second and the hundredths.
        assert_eq!(packed.time, (23 << 11) | (59 << 5) | 29);
        assert_eq!(packed.hundredths, 199);
        assert_eq!(DateTime::unpack(packed), DateTime::LATEST);
    }

    /// Dates below the epoch and above the ceiling are clamped, not wrapped.
    ///
    /// Wrapping is the failure that matters: 1979 would become 2107 rather
    /// than 1980, so a device with an unset clock would stamp its files a
    /// century in the future instead of at the epoch.
    #[test]
    fn out_of_range_dates_clamp_to_the_ends() {
        let before = at(1979, 12, 31, 23, 59, 59, 0);
        assert_eq!(DateTime::unpack(before.pack()), DateTime::EPOCH);

        let after = at(2108, 1, 1, 0, 0, 0, 0);
        assert_eq!(DateTime::unpack(after.pack()), DateTime::LATEST);

        // And from the other direction: a Unix time from 1970, which is
        // what a clock that has never been set reports.
        assert_eq!(DateTime::from_unix_seconds(0), DateTime::EPOCH);
        assert_eq!(DateTime::from_unix_seconds(-1), DateTime::EPOCH);
        assert_eq!(DateTime::from_unix_seconds(i64::MAX / 2), DateTime::LATEST);
        assert_eq!(DateTime::from_unix_seconds(i64::MIN / 2), DateTime::EPOCH);
    }

    /// Seconds are stored halved, so an odd second survives only in the
    /// creation timestamp's extra field.
    #[test]
    fn seconds_are_stored_two_at_a_time() {
        let odd = at(2026, 6, 15, 10, 30, 45, 0);
        let packed = odd.pack();
        assert_eq!(packed.time & 0x1F, 22, "45 seconds is stored as 22");
        assert_eq!(packed.hundredths, 100, "the odd second moves here");
        assert_eq!(DateTime::unpack(packed), odd, "the extra field restores it");

        // Without the extra field -- a modification time -- the odd second
        // is gone, and the result is the even second below.
        let without = DateTime::unpack(Packed {
            hundredths: 0,
            ..packed
        });
        assert_eq!(without, at(2026, 6, 15, 10, 30, 44, 0));
    }

    /// The extra field holds hundredths, not tenths, whatever it is called.
    #[test]
    fn the_creation_field_counts_hundredths() {
        let stamp = at(2026, 6, 15, 10, 30, 44, 990);
        assert_eq!(stamp.pack().hundredths, 99);
        assert_eq!(DateTime::unpack(stamp.pack()), stamp);

        // Sub-10 ms precision is not stored, and rounds down rather than up
        // -- a file must not claim to have been written in the future.
        let fine = at(2026, 6, 15, 10, 30, 44, 999);
        assert_eq!(fine.pack().hundredths, 99);
        assert_eq!(DateTime::unpack(fine.pack()).millisecond, 990);
    }

    /// The century rule, which is the half of the leap-year calculation
    /// that gets skipped. Both cases fall inside FAT's range.
    #[test]
    fn leap_years_include_2000_and_exclude_2100() {
        assert!(is_leap_year(2000), "divisible by 400");
        assert!(!is_leap_year(2100), "a century that is not");
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));

        assert!(DateTime::new(2000, 2, 29, 0, 0, 0, 0).is_some());
        assert!(DateTime::new(2100, 2, 29, 0, 0, 0, 0).is_none());
        assert!(DateTime::new(2023, 2, 29, 0, 0, 0, 0).is_none());
        assert!(DateTime::new(2024, 2, 29, 0, 0, 0, 0).is_some());
    }

    /// Unix conversion agrees with the calendar across the century
    /// boundary, which is where a naive "every fourth year" is a day out
    /// from 2100-03-01 onwards.
    #[test]
    fn unix_conversion_survives_2100() {
        // 2100-02-28 is the last day of that February.
        let last = at(2100, 2, 28, 12, 0, 0, 0);
        let next = at(2100, 3, 1, 12, 0, 0, 0);
        assert_eq!(
            next.to_unix_seconds() - last.to_unix_seconds(),
            86_400,
            "2100 has no 29 February, so these are consecutive days"
        );
        assert_eq!(DateTime::from_unix_seconds(next.to_unix_seconds()), next);

        // 2000 does have one.
        let leap = at(2000, 2, 28, 12, 0, 0, 0);
        let after = at(2000, 3, 1, 12, 0, 0, 0);
        assert_eq!(after.to_unix_seconds() - leap.to_unix_seconds(), 2 * 86_400);
        assert_eq!(
            DateTime::from_unix_seconds(leap.to_unix_seconds() + 86_400),
            at(2000, 2, 29, 12, 0, 0, 0)
        );
    }

    /// The two epochs are a known number of days apart, and the FAT epoch
    /// is day zero of its own count.
    #[test]
    fn the_epochs_line_up() {
        assert_eq!(DateTime::EPOCH.to_unix_seconds(), 315_532_800);
        assert_eq!(DateTime::EPOCH.days_since_fat_epoch(), 0);
        assert_eq!(
            at(1980, 1, 2, 0, 0, 0, 0).days_since_fat_epoch(),
            1,
            "the day after the epoch"
        );
    }

    /// Every representable second round-trips, sampled across the whole
    /// range rather than at a few hand-picked dates.
    #[test]
    fn packing_round_trips_across_the_range() {
        let first = DateTime::EPOCH.to_unix_seconds();
        let last = DateTime::LATEST.to_unix_seconds();
        // A stride that is coprime with 60, 3600 and 86400, so the samples
        // do not all land on the same second, minute or hour of the day.
        let stride = 999_983;

        let mut at_second = first;
        while at_second <= last {
            let original = DateTime::from_unix_seconds(at_second);
            let back = DateTime::unpack(original.pack());
            assert_eq!(back, original, "{at_second} did not survive packing");
            assert_eq!(back.to_unix_seconds(), at_second);
            at_second += stride;
        }
    }
}
