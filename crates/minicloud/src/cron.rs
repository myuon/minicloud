//! When a scheduled job runs.
//!
//! A [`Schedule`] is a cron expression, parsed; [`Schedule::matches`] says
//! whether a [`Minute`] is one it names, and [`Schedule::next_after`] finds the
//! next one. Nothing here starts anything or reads a clock: the caller turns
//! "now" into a [`Minute`] with [`Minute::from_unix`], so what runs when is a
//! function of its arguments and can be tested without waiting.
//!
//! # The expression
//!
//! Five fields, separated by any amount of white space: minute (0-59), hour
//! (0-23), day of the month (1-31), month (1-12) and day of the week (0-6,
//! 0 is Sunday; 7 is accepted as Sunday too, as cron has always done). Each
//! field is a comma-separated list of parts, and a part is one of
//!
//! | part | means |
//! | --- | --- |
//! | `*` | every value the field has |
//! | `*/n` | every `n`th value, from the first |
//! | `a` | that value |
//! | `a-b` | `a` through `b`, both included |
//! | `a-b/n` | every `n`th value from `a` through `b` |
//!
//! A step is 1 or more. `@hourly`, `@daily` (or `@midnight`), `@weekly`,
//! `@monthly` and `@yearly` (or `@annually`) stand for the usual five fields;
//! `@reboot` is refused, because this host has no "at start-up" to hang it on.
//!
//! Month and weekday names (`JAN`, `MON`) are not accepted: every spelling a
//! schedule can be written in is one more way to read it wrongly.
//!
//! # Day of the month and day of the week
//!
//! The two fields are not both required. If neither starts with `*`, a minute
//! matches when *either* does, so `0 0 1 * 0` runs on the 1st and on every
//! Sunday. If one of them starts with `*`, only the other is looked at. This
//! is the rule of Vixie cron, which is the cron most people have used; the
//! alternative, requiring both, would make "the 13th, if a Friday" the one
//! thing that could be written and "the 1st and Sundays" impossible.
//!
//! # UTC, to the minute
//!
//! Every time here is UTC. A schedule that depends on a zone depends on that
//! zone's rules about summer time, and a job that runs twice or not at all one
//! night a year is not one anybody meant to write. The unit is the minute:
//! there are no seconds in an expression, and a [`Minute`] has none.

/// A cron expression, parsed.
///
/// Two schedules are equal when they were written the same way: the original
/// text is kept (for [`Schedule::as_str`]) and is compared too, so `*` and
/// `*/1` are different schedules that happen to match the same minutes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The expression as it was given, trimmed; what stats and logs show.
    text: String,
    /// Bit `n` is set when minute `n` is named.
    minutes: u64,
    /// Bit `n` is set when hour `n` is named.
    hours: u64,
    /// Bit `n` is set when day of the month `n` is named; bit 0 is never set.
    days: u64,
    /// Bit `n` is set when month `n` is named; bit 0 is never set.
    months: u64,
    /// Bit `n` is set when weekday `n` is named; a 7 is folded into bit 0.
    weekdays: u64,
    /// Whether the day field starts with `*`, which decides how it combines
    /// with the weekday ([`Schedule::matches`]).
    day_is_star: bool,
    /// Likewise for the weekday field.
    weekday_is_star: bool,
}

/// A civil date and time in UTC, to the minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Minute {
    pub year: i64,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    /// 0-59.
    pub minute: u32,
    /// 0-6, 0 is Sunday.
    pub weekday: u32,
}

/// How far [`Schedule::next_after`] looks: 366 days of minutes.
const LOOKAHEAD_MINUTES: i64 = 366 * 24 * 60;

impl Minute {
    /// The minute that contains `seconds` since 1970-01-01 00:00:00 UTC.
    ///
    /// Seconds before 1970 are negative and round down, as a clock does:
    /// -1 is 23:59 on 1969-12-31, not 00:00 on 1970-01-01. The date is
    /// Howard Hinnant's `civil_from_days`, which works on the proleptic
    /// Gregorian calendar by counting in 400-year eras so that no leap-year
    /// rule is written out twice.
    pub fn from_unix(seconds: i64) -> Minute {
        let days = seconds.div_euclid(86_400);
        let in_day = seconds.rem_euclid(86_400);

        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        // Months counted from March, so that the leap day is the last day of
        // the year.
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        let year = year_of_era + era * 400 + i64::from(month <= 2);

        Minute {
            year,
            month: month as u32,
            day: day as u32,
            hour: (in_day / 3_600) as u32,
            minute: (in_day % 3_600 / 60) as u32,
            // 1970-01-01 was a Thursday.
            weekday: (days + 4).rem_euclid(7) as u32,
        }
    }

    /// Seconds since 1970-01-01 00:00:00 UTC of the start of this minute: the
    /// inverse of [`Minute::from_unix`] (Hinnant's `days_from_civil`).
    ///
    /// The `weekday` field is not read; it follows from the date, and a
    /// `Minute` built by hand with a wrong one still names the date it says.
    pub fn to_unix(&self) -> i64 {
        let month = i64::from(self.month);
        let year = self.year - i64::from(month <= 2);
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let shifted_month = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        days * 86_400 + i64::from(self.hour) * 3_600 + i64::from(self.minute) * 60
    }
}

impl Schedule {
    /// Parses a cron expression or one of the `@` aliases.
    ///
    /// The error says which part is wrong and what would be right, in the
    /// words of the other refusals in this host.
    pub fn parse(text: &str) -> Result<Schedule, String> {
        let text = text.trim();
        let expanded = if text.starts_with('@') {
            match text {
                "@hourly" => "0 * * * *",
                "@daily" | "@midnight" => "0 0 * * *",
                "@weekly" => "0 0 * * 0",
                "@monthly" => "0 0 1 * *",
                "@yearly" | "@annually" => "0 0 1 1 *",
                "@reboot" => {
                    return Err("`@reboot` is not supported: this host has no start-up \
                         trigger to run it on; use a schedule such as @hourly or `0 * * * *`"
                        .to_string());
                }
                _ => {
                    return Err(format!(
                        "`{text}` is not a known alias; use one of @hourly @daily @midnight \
                         @weekly @monthly @yearly @annually"
                    ));
                }
            }
        } else {
            text
        };

        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "`{text}` has {} fields; a schedule has 5 (minute hour day month weekday), \
                 or one of @hourly @daily @weekly @monthly @yearly",
                fields.len()
            ));
        };

        let mut weekdays = parse_field(weekday, &WEEKDAY)?;
        // 7 is Sunday, spelled the way cron always allowed.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }

        Ok(Schedule {
            text: text.to_string(),
            minutes: parse_field(minute, &MINUTE)?,
            hours: parse_field(hour, &HOUR)?,
            days: parse_field(day, &DAY)?,
            months: parse_field(month, &MONTH)?,
            weekdays,
            day_is_star: day.starts_with('*'),
            weekday_is_star: weekday.starts_with('*'),
        })
    }

    /// The expression as it was written, for stats and logs.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `at` is a minute this schedule runs.
    ///
    /// Minute, hour and month must all match. The day of the month and the day
    /// of the week combine as Vixie cron has them: when neither field starts
    /// with `*`, either one matching is enough; when one does, the other alone
    /// decides (the starred field names every day, so requiring both comes to
    /// the same thing). `at.weekday` is trusted as given; [`Minute::from_unix`]
    /// always makes it right.
    pub fn matches(&self, at: Minute) -> bool {
        let day = self.days & (1 << at.day) != 0;
        let weekday = self.weekdays & (1 << at.weekday) != 0;
        let day_ok = if self.day_is_star || self.weekday_is_star {
            day && weekday
        } else {
            day || weekday
        };
        day_ok
            && self.minutes & (1 << at.minute) != 0
            && self.hours & (1 << at.hour) != 0
            && self.months & (1 << at.month) != 0
    }

    /// The first minute after `after` that this schedule runs, or `None` if
    /// there is none in the next 366 days.
    ///
    /// It steps one minute at a time. That is at most 527 040 checks, each a
    /// handful of integer operations, run once per job per firing; a search
    /// that jumped by field would be faster and would also be the place where
    /// month lengths, leap years and the day-or-weekday rule were all
    /// special-cased a second time. The bound is what lets a schedule that
    /// never comes (`0 0 30 2 *`) end; the price is that one that comes less
    /// than yearly, such as `0 0 29 2 *`, is `None` unless the next 29 February
    /// is within the year.
    pub fn next_after(&self, after: Minute) -> Option<Minute> {
        let start = after.to_unix().div_euclid(60) * 60;
        (1..=LOOKAHEAD_MINUTES)
            .map(|step| Minute::from_unix(start + step * 60))
            .find(|candidate| self.matches(*candidate))
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// What a field is called and the values it takes.
struct Field {
    name: &'static str,
    min: u32,
    /// The largest value that can be written.
    max: u32,
    /// The largest value `*` stands for; differs from `max` only for the
    /// weekday, where 7 may be written but `*` is 0-6.
    star_max: u32,
    /// The range as the error message shows it.
    shown: &'static str,
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    star_max: 59,
    shown: "0-59",
};
const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    star_max: 23,
    shown: "0-23",
};
const DAY: Field = Field {
    name: "day",
    min: 1,
    max: 31,
    star_max: 31,
    shown: "1-31",
};
const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    star_max: 12,
    shown: "1-12",
};
const WEEKDAY: Field = Field {
    name: "weekday",
    min: 0,
    max: 7,
    star_max: 6,
    shown: "0-6, 7 is Sunday too",
};

impl Field {
    /// A number written in this field, in range.
    fn value(&self, text: &str) -> Result<u32, String> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "`{text}` is not a number in the {} field; write a number, a range like 1-5, \
                 or `*`",
                self.name
            ));
        }
        // Too many digits to be a u32 is out of range all the same.
        let value = text.parse::<u32>().unwrap_or(u32::MAX);
        if value < self.min || value > self.max {
            return Err(format!(
                "`{text}` is out of range for the {} field ({})",
                self.name, self.shown
            ));
        }
        Ok(value)
    }
}

/// The values a field names, as bits.
fn parse_field(text: &str, field: &Field) -> Result<u64, String> {
    let mut bits = 0u64;
    for part in text.split(',') {
        bits |= parse_part(part, text, field)?;
    }
    Ok(bits)
}

/// One comma-separated part of a field, as bits. `whole` is the field it is
/// part of, for the message.
fn parse_part(part: &str, whole: &str, field: &Field) -> Result<u64, String> {
    if part.is_empty() {
        return Err(format!(
            "`{whole}` has an empty part in the {} field; write a number, a range like 1-5, \
             or `*`, with no comma left over",
            field.name
        ));
    }

    let (range, step) = match part.split_once('/') {
        Some((range, step)) => (range, Some(step)),
        None => (part, None),
    };
    let stride = match step {
        None => 1,
        Some(step) => match step.parse::<u32>() {
            Ok(n) if n >= 1 && step.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => return Err(format!("`{part}` is not a step: a step is 1 or more")),
        },
    };

    let (first, last) = if range == "*" {
        (field.min, field.star_max)
    } else if let Some((from, to)) = range.split_once('-') {
        let (from, to) = (field.value(from)?, field.value(to)?);
        if from > to {
            return Err(format!(
                "`{part}` runs backwards in the {} field; write the smaller number first",
                field.name
            ));
        }
        (from, to)
    } else if step.is_some() {
        return Err(format!(
            "`{part}` has a step but no range to step through; write `*/{}` or a range like \
             {range}-{}/{}",
            stride, field.star_max, stride
        ));
    } else {
        let value = field.value(range)?;
        (value, value)
    };

    Ok((first..=last)
        .step_by(stride as usize)
        .fold(0u64, |bits, value| bits | 1 << value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minute at this civil time, with its weekday worked out.
    fn at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Minute {
        let civil = Minute {
            year,
            month,
            day,
            hour,
            minute,
            weekday: 0,
        };
        Minute::from_unix(civil.to_unix())
    }

    fn schedule(text: &str) -> Schedule {
        Schedule::parse(text).unwrap()
    }

    fn refusal(text: &str) -> String {
        Schedule::parse(text).unwrap_err()
    }

    #[test]
    fn the_epoch_is_a_thursday_at_midnight_in_1970() {
        let m = Minute::from_unix(0);
        assert_eq!(
            m,
            Minute {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                weekday: 4,
            }
        );
    }

    #[test]
    fn a_leap_day_is_a_day() {
        let m = Minute::from_unix(1_709_164_800 + 13 * 3_600 + 5 * 60 + 59);
        assert_eq!(
            m,
            Minute {
                year: 2024,
                month: 2,
                day: 29,
                hour: 13,
                minute: 5,
                weekday: 4,
            }
        );
    }

    #[test]
    fn the_day_after_a_century_leap_day_is_the_first_of_march() {
        let m = Minute::from_unix(951_868_800);
        assert_eq!((m.year, m.month, m.day, m.weekday), (2000, 3, 1, 3));
    }

    #[test]
    fn a_negative_number_of_seconds_rounds_down_to_the_minute_before_1970() {
        let m = Minute::from_unix(-1);
        assert_eq!(
            m,
            Minute {
                year: 1969,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                weekday: 3,
            }
        );
        assert_eq!(Minute::from_unix(-60), m);
        assert_eq!(Minute::from_unix(-61).minute, 58);
    }

    #[test]
    fn today_is_a_monday() {
        let m = Minute::from_unix(1_791_763_200);
        assert_eq!((m.year, m.month, m.day, m.weekday), (2026, 10, 12, 1));
    }

    #[test]
    fn to_unix_is_the_inverse_of_from_unix_over_several_centuries() {
        for seconds in (-6_000_000_000i64..6_000_000_000).step_by(86_400 * 7 + 3_660) {
            let minute = Minute::from_unix(seconds);
            assert_eq!(minute.to_unix(), seconds.div_euclid(60) * 60, "{minute:?}");
        }
    }

    #[test]
    fn a_star_matches_every_minute() {
        let s = schedule("* * * * *");
        for seconds in (0..40 * 86_400).step_by(60 * 7 + 1) {
            assert!(s.matches(Minute::from_unix(seconds)));
        }
    }

    #[test]
    fn a_step_of_fifteen_matches_the_quarter_hours_only() {
        let s = schedule("*/15 * * * *");
        let matching: Vec<u32> = (0..60)
            .filter(|minute| s.matches(at(2026, 10, 12, 10, *minute)))
            .collect();
        assert_eq!(matching, vec![0, 15, 30, 45]);
    }

    #[test]
    fn nine_on_weekdays_matches_nine_on_weekdays_only() {
        let s = schedule("0 9 * * 1-5");
        assert!(s.matches(at(2026, 10, 12, 9, 0)), "Monday");
        assert!(s.matches(at(2026, 10, 16, 9, 0)), "Friday");
        assert!(!s.matches(at(2026, 10, 17, 9, 0)), "Saturday");
        assert!(!s.matches(at(2026, 10, 18, 9, 0)), "Sunday");
        assert!(!s.matches(at(2026, 10, 12, 9, 1)));
        assert!(!s.matches(at(2026, 10, 12, 8, 0)));
    }

    #[test]
    fn a_day_and_a_weekday_together_run_on_either() {
        let s = schedule("0 0 1 * 0");
        assert!(s.matches(at(2026, 10, 1, 0, 0)), "the 1st, a Thursday");
        assert!(s.matches(at(2026, 10, 4, 0, 0)), "a Sunday, the 4th");
        assert!(!s.matches(at(2026, 10, 5, 0, 0)), "a Monday, the 5th");

        let friday_13th = schedule("0 0 13 * 5");
        assert!(friday_13th.matches(at(2026, 10, 13, 0, 0)), "a Tuesday");
        assert!(friday_13th.matches(at(2026, 10, 16, 0, 0)), "a Friday");
    }

    #[test]
    fn a_starred_day_or_weekday_leaves_the_other_alone_to_decide() {
        let sundays = schedule("0 0 * * 0");
        assert!(sundays.matches(at(2026, 10, 4, 0, 0)));
        assert!(!sundays.matches(at(2026, 10, 1, 0, 0)));

        let firsts = schedule("0 0 1 * *");
        assert!(firsts.matches(at(2026, 10, 1, 0, 0)));
        assert!(!firsts.matches(at(2026, 10, 4, 0, 0)));
    }

    #[test]
    fn a_month_narrows_a_day_and_weekday_that_would_run_either() {
        let s = schedule("0 0 1 11 0");
        assert!(!s.matches(at(2026, 10, 1, 0, 0)));
        assert!(s.matches(at(2026, 11, 1, 0, 0)));
        assert!(s.matches(at(2026, 11, 8, 0, 0)));
    }

    #[test]
    fn the_aliases_stand_for_their_five_fields() {
        let pairs = [
            ("@hourly", "0 * * * *"),
            ("@daily", "0 0 * * *"),
            ("@midnight", "0 0 * * *"),
            ("@weekly", "0 0 * * 0"),
            ("@monthly", "0 0 1 * *"),
            ("@yearly", "0 0 1 1 *"),
            ("@annually", "0 0 1 1 *"),
        ];
        for (alias, fields) in pairs {
            let a = schedule(alias);
            let b = schedule(fields);
            assert_eq!(
                (a.minutes, a.hours, a.days, a.months, a.weekdays),
                (b.minutes, b.hours, b.days, b.months, b.weekdays),
                "{alias}"
            );
            assert_eq!(a.as_str(), alias);
        }
    }

    #[test]
    fn at_reboot_is_refused_and_the_message_says_why() {
        let message = refusal("@reboot");
        assert!(message.contains("`@reboot`"), "{message}");
        assert!(message.contains("no start-up trigger"), "{message}");
    }

    #[test]
    fn an_unknown_alias_is_refused_with_the_known_ones() {
        let message = refusal("@sometimes");
        assert!(
            message.starts_with("`@sometimes` is not a known alias"),
            "{message}"
        );
        assert!(message.contains("@hourly"), "{message}");
    }

    #[test]
    fn the_wrong_number_of_fields_is_refused_with_the_count() {
        assert_eq!(
            refusal("0 * * *"),
            "`0 * * *` has 4 fields; a schedule has 5 (minute hour day month weekday), or one \
             of @hourly @daily @weekly @monthly @yearly"
        );
        assert!(refusal("0 * * * * *").contains("has 6 fields"));
        assert!(refusal("").contains("has 0 fields"));
    }

    #[test]
    fn a_value_out_of_range_is_refused_naming_the_field_and_its_range() {
        assert_eq!(
            refusal("62 * * * *"),
            "`62` is out of range for the minute field (0-59)"
        );
        assert!(refusal("* 24 * * *").contains("for the hour field (0-23)"));
        assert!(refusal("* * 0 * *").contains("for the day field (1-31)"));
        assert!(refusal("* * * 13 *").contains("for the month field (1-12)"));
        assert!(refusal("* * * * 8").contains("for the weekday field (0-6, 7 is Sunday too)"));
        assert!(refusal("0-99999999999 * * * *").contains("is out of range"));
    }

    #[test]
    fn a_step_of_zero_is_refused() {
        assert_eq!(
            refusal("*/0 * * * *"),
            "`*/0` is not a step: a step is 1 or more"
        );
        assert!(refusal("0-30/0 * * * *").contains("is not a step"));
        assert!(refusal("*/x * * * *").contains("is not a step"));
    }

    #[test]
    fn a_step_without_a_range_is_refused_with_what_to_write() {
        let message = refusal("5/15 * * * *");
        assert!(message.contains("has a step but no range"), "{message}");
        assert!(message.contains("`*/15`"), "{message}");
    }

    #[test]
    fn an_empty_part_is_refused() {
        assert!(refusal("1, * * * *").contains("has an empty part in the minute field"));
        assert!(refusal(",1 * * * *").contains("empty part"));
        assert!(refusal("1,,2 * * * *").contains("empty part"));
        assert!(refusal("1- * * * *").contains("is not a number"));
    }

    #[test]
    fn a_range_that_runs_backwards_is_refused() {
        assert_eq!(
            refusal("* 17-9 * * *"),
            "`17-9` runs backwards in the hour field; write the smaller number first"
        );
    }

    #[test]
    fn a_name_instead_of_a_number_is_refused() {
        assert!(refusal("0 0 * JAN *").contains("`JAN` is not a number in the month field"));
        assert!(refusal("0 0 * * MON").contains("is not a number"));
    }

    #[test]
    fn lists_ranges_and_stepped_ranges_combine() {
        let s = schedule("1,3,5-7,20-30/5 * * * *");
        let matching: Vec<u32> = (0..60)
            .filter(|minute| s.matches(at(2026, 10, 12, 10, *minute)))
            .collect();
        assert_eq!(matching, vec![1, 3, 5, 6, 7, 20, 25, 30]);
    }

    #[test]
    fn any_amount_of_white_space_separates_the_fields() {
        let s = schedule("  0\t9   * *  1-5 \n");
        assert_eq!(s.as_str(), "0\t9   * *  1-5");
        assert!(s.matches(at(2026, 10, 12, 9, 0)));
    }

    #[test]
    fn a_weekday_of_seven_is_a_sunday() {
        let s = schedule("0 0 * * 7");
        assert!(s.matches(at(2026, 10, 4, 0, 0)));
        assert!(!s.matches(at(2026, 10, 5, 0, 0)));
        assert_eq!(s.weekdays, 1);

        let weekend = schedule("0 0 * * 5-7");
        assert!(weekend.matches(at(2026, 10, 16, 0, 0)), "Friday");
        assert!(weekend.matches(at(2026, 10, 17, 0, 0)), "Saturday");
        assert!(weekend.matches(at(2026, 10, 18, 0, 0)), "Sunday");
        assert!(!weekend.matches(at(2026, 10, 19, 0, 0)), "Monday");
    }

    #[test]
    fn a_star_in_the_weekday_field_is_zero_to_six() {
        assert_eq!(schedule("* * * * *").weekdays, 0b111_1111);
        assert_eq!(schedule("* * * * */2").weekdays, 0b101_0101);
    }

    #[test]
    fn next_after_is_the_first_matching_minute_strictly_later() {
        let s = schedule("*/15 * * * *");
        assert_eq!(
            s.next_after(at(2026, 10, 12, 10, 7)),
            Some(at(2026, 10, 12, 10, 15))
        );
        assert_eq!(
            s.next_after(at(2026, 10, 12, 10, 15)),
            Some(at(2026, 10, 12, 10, 30))
        );
        assert_eq!(
            s.next_after(at(2026, 10, 12, 10, 45)),
            Some(at(2026, 10, 12, 11, 0))
        );
    }

    #[test]
    fn next_after_crosses_midnight_month_and_year() {
        assert_eq!(
            schedule("@daily").next_after(at(2026, 10, 12, 23, 59)),
            Some(at(2026, 10, 13, 0, 0))
        );
        assert_eq!(
            schedule("@monthly").next_after(at(2026, 12, 31, 12, 0)),
            Some(at(2027, 1, 1, 0, 0))
        );
        assert_eq!(
            schedule("@yearly").next_after(at(2026, 10, 12, 0, 0)),
            Some(at(2027, 1, 1, 0, 0))
        );
    }

    #[test]
    fn next_after_ignores_the_weekday_a_hand_built_minute_claims() {
        let mut after = at(2026, 10, 12, 10, 0);
        after.weekday = 6;
        assert_eq!(
            schedule("30 10 * * *").next_after(after),
            Some(at(2026, 10, 12, 10, 30))
        );
    }

    #[test]
    fn next_after_finds_a_leap_day_when_it_is_within_the_year() {
        assert_eq!(
            schedule("0 0 29 2 *").next_after(at(2027, 10, 12, 0, 0)),
            Some(at(2028, 2, 29, 0, 0))
        );
    }

    #[test]
    fn a_schedule_that_never_comes_has_no_next_minute() {
        assert_eq!(
            schedule("0 0 30 2 *").next_after(at(2026, 10, 12, 0, 0)),
            None
        );
        assert_eq!(
            schedule("2 3 30 2 *").next_after(at(2026, 10, 12, 0, 0)),
            None
        );
    }

    #[test]
    fn the_original_text_is_what_is_shown() {
        let s = schedule("*/5  * * * *");
        assert_eq!(s.as_str(), "*/5  * * * *");
        assert_eq!(s.to_string(), "*/5  * * * *");
    }
}
