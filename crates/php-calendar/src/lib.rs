// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Port of PHP's `ext/calendar`.
//!
//! The low-level modules ([`gregor`], [`julian`], [`jewish`], [`french`]) are
//! faithful ports of the `SdnTo*` / `*ToSdn` routines from php-src, including
//! their C `int` overflow behaviour.  The top-level functions mirror the PHP
//! userland functions one-to-one and return plain Rust values; the VM wraps
//! them into zvals.  Errors are reported as [`CalError`], which carries the
//! information needed to build the exact `ValueError` message PHP throws.
//!
//! Functions that depend on the process clock or local time zone
//! (`unixtojd`, `easter_date`, the default year of `easter_days`) take
//! closures so that the caller supplies the time-zone logic.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod easter;
pub mod french;
pub mod gregor;
pub mod jewish;
pub mod julian;

use std::fmt;

pub use easter::{easter_date, easter_days};

/// A calendar date as produced by the `SdnTo*` routines (`0/0/0` = invalid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Ymd {
    /// Year (negative for B.C.; 0 only for invalid dates).
    pub year: i32,
    /// Month, 1-based (0 for invalid dates).
    pub month: i32,
    /// Day of month, 1-based (0 for invalid dates).
    pub day: i32,
}

impl Ymd {
    /// The `0/0/0` value returned for out-of-range serial day numbers.
    pub const ZERO: Self = Self {
        year: 0,
        month: 0,
        day: 0,
    };

    /// Creates a date.
    pub const fn new(year: i32, month: i32, day: i32) -> Self {
        Self { year, month, day }
    }

    /// Builds a date from wide intermediates, truncating like a C `int` store.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn from_wide(year: i64, month: i64, day: i64) -> Self {
        Self::new(year as i32, month as i32, day as i32)
    }

    fn month_index(self) -> usize {
        usize::try_from(self.month).unwrap_or(0)
    }
}

impl fmt::Display for Ymd {
    /// Formats as PHP's `"%i/%i/%i"` (month/day/year).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.month, self.day, self.year)
    }
}

/// `CAL_GREGORIAN`.
pub const CAL_GREGORIAN: i64 = 0;
/// `CAL_JULIAN`.
pub const CAL_JULIAN: i64 = 1;
/// `CAL_JEWISH`.
pub const CAL_JEWISH: i64 = 2;
/// `CAL_FRENCH`.
pub const CAL_FRENCH: i64 = 3;
/// `CAL_NUM_CALS`.
pub const CAL_NUM_CALS: i64 = 4;
/// `CAL_DOW_DAYNO`.
pub const CAL_DOW_DAYNO: i64 = 0;
/// `CAL_DOW_LONG`.
pub const CAL_DOW_LONG: i64 = 1;
/// `CAL_DOW_SHORT`.
pub const CAL_DOW_SHORT: i64 = 2;
/// `CAL_MONTH_GREGORIAN_SHORT`.
pub const CAL_MONTH_GREGORIAN_SHORT: i64 = 0;
/// `CAL_MONTH_GREGORIAN_LONG`.
pub const CAL_MONTH_GREGORIAN_LONG: i64 = 1;
/// `CAL_MONTH_JULIAN_SHORT`.
pub const CAL_MONTH_JULIAN_SHORT: i64 = 2;
/// `CAL_MONTH_JULIAN_LONG`.
pub const CAL_MONTH_JULIAN_LONG: i64 = 3;
/// `CAL_MONTH_JEWISH`.
pub const CAL_MONTH_JEWISH: i64 = 4;
/// `CAL_MONTH_FRENCH`.
pub const CAL_MONTH_FRENCH: i64 = 5;
/// `CAL_EASTER_DEFAULT`.
pub const CAL_EASTER_DEFAULT: i64 = 0;
/// `CAL_EASTER_ROMAN`.
pub const CAL_EASTER_ROMAN: i64 = 1;
/// `CAL_EASTER_ALWAYS_GREGORIAN`.
pub const CAL_EASTER_ALWAYS_GREGORIAN: i64 = 2;
/// `CAL_EASTER_ALWAYS_JULIAN`.
pub const CAL_EASTER_ALWAYS_JULIAN: i64 = 3;
/// `CAL_JEWISH_ADD_ALAFIM_GERESH`.
pub const CAL_JEWISH_ADD_ALAFIM_GERESH: i64 = 0x2;
/// `CAL_JEWISH_ADD_ALAFIM`.
pub const CAL_JEWISH_ADD_ALAFIM: i64 = 0x4;
/// `CAL_JEWISH_ADD_GERESHAYIM`.
pub const CAL_JEWISH_ADD_GERESHAYIM: i64 = 0x8;

/// All constants registered by the extension, in registration order.
pub const CONSTANTS: [(&str, i64); 21] = [
    ("CAL_GREGORIAN", CAL_GREGORIAN),
    ("CAL_JULIAN", CAL_JULIAN),
    ("CAL_JEWISH", CAL_JEWISH),
    ("CAL_FRENCH", CAL_FRENCH),
    ("CAL_NUM_CALS", CAL_NUM_CALS),
    ("CAL_DOW_DAYNO", CAL_DOW_DAYNO),
    ("CAL_DOW_SHORT", CAL_DOW_SHORT),
    ("CAL_DOW_LONG", CAL_DOW_LONG),
    ("CAL_MONTH_GREGORIAN_SHORT", CAL_MONTH_GREGORIAN_SHORT),
    ("CAL_MONTH_GREGORIAN_LONG", CAL_MONTH_GREGORIAN_LONG),
    ("CAL_MONTH_JULIAN_SHORT", CAL_MONTH_JULIAN_SHORT),
    ("CAL_MONTH_JULIAN_LONG", CAL_MONTH_JULIAN_LONG),
    ("CAL_MONTH_JEWISH", CAL_MONTH_JEWISH),
    ("CAL_MONTH_FRENCH", CAL_MONTH_FRENCH),
    ("CAL_EASTER_DEFAULT", CAL_EASTER_DEFAULT),
    ("CAL_EASTER_ROMAN", CAL_EASTER_ROMAN),
    ("CAL_EASTER_ALWAYS_GREGORIAN", CAL_EASTER_ALWAYS_GREGORIAN),
    ("CAL_EASTER_ALWAYS_JULIAN", CAL_EASTER_ALWAYS_JULIAN),
    ("CAL_JEWISH_ADD_ALAFIM_GERESH", CAL_JEWISH_ADD_ALAFIM_GERESH),
    ("CAL_JEWISH_ADD_ALAFIM", CAL_JEWISH_ADD_ALAFIM),
    ("CAL_JEWISH_ADD_GERESHAYIM", CAL_JEWISH_ADD_GERESHAYIM),
];

/// Abbreviated day names, 0 = Sunday.
pub const DAY_NAME_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
/// Full day names, 0 = Sunday.
pub const DAY_NAME_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// An error thrown as a PHP `ValueError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalError {
    /// `zend_argument_value_error()`: the message is prefixed with
    /// `"<func>(): Argument #<arg> ($<name>) "`.
    Argument {
        /// 1-based argument position.
        arg: u32,
        /// Parameter name without the `$`.
        name: &'static str,
        /// Message tail.
        message: String,
    },
    /// `zend_value_error()`: a plain message.
    Value(String),
}

impl CalError {
    fn arg(arg: u32, name: &'static str, message: impl Into<String>) -> Self {
        Self::Argument {
            arg,
            name,
            message: message.into(),
        }
    }

    /// Full `ValueError` message as PHP prints it when thrown from `func`.
    pub fn message(&self, func: &str) -> String {
        match self {
            Self::Argument { arg, name, message } => {
                format!("{func}(): Argument #{arg} (${name}) {message}")
            }
            Self::Value(m) => m.clone(),
        }
    }
}

/// One of the four supported calendars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Calendar {
    /// `CAL_GREGORIAN`.
    Gregorian,
    /// `CAL_JULIAN`.
    Julian,
    /// `CAL_JEWISH`.
    Jewish,
    /// `CAL_FRENCH`.
    French,
}

impl Calendar {
    /// All calendars in `CAL_*` id order.
    pub const ALL: [Self; 4] = [Self::Gregorian, Self::Julian, Self::Jewish, Self::French];

    /// Looks up a calendar by its `CAL_*` id.
    pub fn from_id(id: i64) -> Option<Self> {
        usize::try_from(id)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }

    /// Converts a date in this calendar to a serial day number (0 = invalid).
    pub fn to_jd(self, year: i64, month: i64, day: i64) -> i64 {
        match self {
            Self::Gregorian => gregor::gregorian_to_sdn(year, month, day),
            Self::Julian => julian::julian_to_sdn(year, month, day),
            Self::Jewish => jewish::jewish_to_sdn(year, month, day),
            Self::French => french::french_to_sdn(year, month, day),
        }
    }

    /// Converts a serial day number to a date in this calendar.
    pub fn from_jd(self, jd: i64) -> Ymd {
        match self {
            Self::Gregorian => gregor::sdn_to_gregorian(jd),
            Self::Julian => julian::sdn_to_julian(jd),
            Self::Jewish => jewish::sdn_to_jewish(jd),
            Self::French => french::sdn_to_french(jd),
        }
    }

    /// Calendar name as reported by `cal_info()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Gregorian => "Gregorian",
            Self::Julian => "Julian",
            Self::Jewish => "Jewish",
            Self::French => "French",
        }
    }

    /// Constant name as reported by `cal_info()`.
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Gregorian => "CAL_GREGORIAN",
            Self::Julian => "CAL_JULIAN",
            Self::Jewish => "CAL_JEWISH",
            Self::French => "CAL_FRENCH",
        }
    }

    /// Number of months per year in the conversion table.
    pub fn num_months(self) -> usize {
        match self {
            Self::Gregorian | Self::Julian => 12,
            Self::Jewish | Self::French => 13,
        }
    }

    /// Maximum days in any month.
    pub fn max_days_in_month(self) -> i64 {
        match self {
            Self::Gregorian | Self::Julian => 31,
            Self::Jewish | Self::French => 30,
        }
    }

    /// Short month names (index 0 is empty).
    pub fn month_names_short(self) -> &'static [&'static str] {
        match self {
            Self::Gregorian | Self::Julian => &gregor::MONTH_NAME_SHORT,
            Self::Jewish => &jewish::JEWISH_MONTH_NAME_LEAP,
            Self::French => &french::FRENCH_MONTH_NAME,
        }
    }

    /// Long month names (index 0 is empty).
    pub fn month_names_long(self) -> &'static [&'static str] {
        match self {
            Self::Gregorian | Self::Julian => &gregor::MONTH_NAME_LONG,
            Self::Jewish => &jewish::JEWISH_MONTH_NAME_LEAP,
            Self::French => &french::FRENCH_MONTH_NAME,
        }
    }
}

fn calendar_arg(cal: i64, arg: u32) -> Result<Calendar, CalError> {
    Calendar::from_id(cal)
        .ok_or_else(|| CalError::arg(arg, "calendar", "must be a valid calendar ID"))
}

/// Day of week for a serial day number, 0 = Sunday (`DayOfWeek()`).
pub fn day_of_week(sdn: i64) -> i64 {
    (sdn % 7 + 8) % 7
}

fn dow_index(sdn: i64) -> usize {
    usize::try_from(day_of_week(sdn)).unwrap_or(0)
}

/// The array returned by `cal_info()` for one calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalInfo {
    /// `"months"`: `(index, name)` pairs, index starting at 1.
    pub months: Vec<(i64, &'static str)>,
    /// `"abbrevmonths"`: `(index, name)` pairs, index starting at 1.
    pub abbrevmonths: Vec<(i64, &'static str)>,
    /// `"maxdaysinmonth"`.
    pub maxdaysinmonth: i64,
    /// `"calname"`.
    pub calname: &'static str,
    /// `"calsymbol"`.
    pub calsymbol: &'static str,
}

impl CalInfo {
    /// Builds the info array for `cal`.
    pub fn for_calendar(cal: Calendar) -> Self {
        let n = cal.num_months();
        let collect = |names: &'static [&'static str]| {
            (1..=n)
                .map(|i| (i64::try_from(i).unwrap_or(0), names[i]))
                .collect()
        };
        Self {
            months: collect(cal.month_names_long()),
            abbrevmonths: collect(cal.month_names_short()),
            maxdaysinmonth: cal.max_days_in_month(),
            calname: cal.name(),
            calsymbol: cal.symbol(),
        }
    }
}

/// `cal_info(int $calendar = -1)`: `-1` yields all four calendars (keys 0..=3),
/// any other valid id yields a single entry.
pub fn cal_info(calendar: i64) -> Result<Vec<CalInfo>, CalError> {
    if calendar == -1 {
        return Ok(Calendar::ALL
            .iter()
            .map(|c| CalInfo::for_calendar(*c))
            .collect());
    }
    Ok(vec![CalInfo::for_calendar(calendar_arg(calendar, 1)?)])
}

const INT32_MAX: i64 = i32::MAX as i64;
const INT32_MIN: i64 = i32::MIN as i64;

/// `cal_days_in_month(int $calendar, int $month, int $year): int`.
pub fn cal_days_in_month(calendar: i64, month: i64, year: i64) -> Result<i64, CalError> {
    let cal = calendar_arg(calendar, 1)?;
    if month <= 0 || month > INT32_MAX - 1 {
        return Err(CalError::arg(
            2,
            "month",
            format!("must be between 1 and {}", INT32_MAX - 1),
        ));
    }
    if year > INT32_MAX - 1 {
        return Err(CalError::arg(
            3,
            "year",
            format!("must be less than {}", INT32_MAX - 1),
        ));
    }
    let sdn_start = cal.to_jd(year, month, 1);
    if sdn_start == 0 {
        return Err(CalError::Value("Invalid date".into()));
    }
    let mut sdn_next = cal.to_jd(year, 1 + month, 1);
    if sdn_next == 0 {
        if year == -1 {
            sdn_next = cal.to_jd(1, 1, 1);
        } else {
            sdn_next = cal.to_jd(year + 1, 1, 1);
            if cal == Calendar::French && sdn_next == 0 {
                // The French calendar ends on 0014-13-05.
                sdn_next = 2_380_953;
            }
        }
    }
    Ok(sdn_next - sdn_start)
}

/// `cal_to_jd(int $calendar, int $month, int $day, int $year): int`.
pub fn cal_to_jd(calendar: i64, month: i64, day: i64, year: i64) -> Result<i64, CalError> {
    let cal = calendar_arg(calendar, 1)?;
    if month <= 0 || month > INT32_MAX - 1 {
        return Err(CalError::arg(
            2,
            "month",
            format!("must be between 1 and {}", INT32_MAX - 1),
        ));
    }
    if !(INT32_MIN..=INT32_MAX).contains(&day) {
        return Err(CalError::arg(
            3,
            "day",
            format!("must be between {INT32_MIN} and {INT32_MAX}"),
        ));
    }
    if year > INT32_MAX - 1 {
        return Err(CalError::arg(
            4,
            "year",
            format!("must be less than {}", INT32_MAX - 1),
        ));
    }
    Ok(cal.to_jd(year, month, day))
}

/// The array returned by `cal_from_jd()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalFromJd {
    /// `"date"`: `"month/day/year"`.
    pub date: String,
    /// `"month"`.
    pub month: i64,
    /// `"day"`.
    pub day: i64,
    /// `"year"`.
    pub year: i64,
    /// `"dow"`: `None` is PHP `null` (Jewish dates before year 1).
    pub dow: Option<i64>,
    /// `"abbrevdayname"`.
    pub abbrevdayname: &'static str,
    /// `"dayname"`.
    pub dayname: &'static str,
    /// `"abbrevmonth"`.
    pub abbrevmonth: &'static str,
    /// `"monthname"`.
    pub monthname: &'static str,
}

/// `cal_from_jd(int $julian_day, int $calendar): array`.
pub fn cal_from_jd(julian_day: i64, calendar: i64) -> Result<CalFromJd, CalError> {
    let cal = Calendar::from_id(calendar)
        .ok_or_else(|| CalError::arg(2, "calendar", "must be a valid calendar ID"))?;
    let ymd = cal.from_jd(julian_day);
    let (dow, abbrevdayname, dayname) = if cal != Calendar::Jewish || ymd.year > 0 {
        let i = dow_index(julian_day);
        (
            Some(day_of_week(julian_day)),
            DAY_NAME_SHORT[i],
            DAY_NAME_LONG[i],
        )
    } else {
        (None, "", "")
    };
    let (abbrevmonth, monthname) = if cal == Calendar::Jewish {
        let name = if ymd.year > 0 {
            jewish::month_names(ymd.year.into())[ymd.month_index()]
        } else {
            ""
        };
        (name, name)
    } else {
        (
            cal.month_names_short()[ymd.month_index()],
            cal.month_names_long()[ymd.month_index()],
        )
    };
    Ok(CalFromJd {
        date: ymd.to_string(),
        month: ymd.month.into(),
        day: ymd.day.into(),
        year: ymd.year.into(),
        dow,
        abbrevdayname,
        dayname,
        abbrevmonth,
        monthname,
    })
}

/// `jdtogregorian(int $julian_day): string`.
pub fn jdtogregorian(julian_day: i64) -> String {
    gregor::sdn_to_gregorian(julian_day).to_string()
}

/// `gregoriantojd(int $month, int $day, int $year): int`.
pub fn gregoriantojd(month: i64, day: i64, year: i64) -> i64 {
    gregor::gregorian_to_sdn(year, month, day)
}

/// `jdtojulian(int $julian_day): string`.
pub fn jdtojulian(julian_day: i64) -> String {
    julian::sdn_to_julian(julian_day).to_string()
}

/// `juliantojd(int $month, int $day, int $year): int`.
pub fn juliantojd(month: i64, day: i64, year: i64) -> i64 {
    julian::julian_to_sdn(year, month, day)
}

/// `jdtofrench(int $julian_day): string`.
pub fn jdtofrench(julian_day: i64) -> String {
    french::sdn_to_french(julian_day).to_string()
}

/// `frenchtojd(int $month, int $day, int $year): int`.
pub fn frenchtojd(month: i64, day: i64, year: i64) -> i64 {
    french::french_to_sdn(year, month, day)
}

/// `jewishtojd(int $month, int $day, int $year): int`.
pub fn jewishtojd(month: i64, day: i64, year: i64) -> Result<i64, CalError> {
    if !(INT32_MIN..=INT32_MAX).contains(&year) {
        return Err(CalError::arg(
            3,
            "year",
            format!("must be between {INT32_MIN} and {INT32_MAX}"),
        ));
    }
    Ok(jewish::jewish_to_sdn(year, month, day))
}

/// ISO-8859-8 Hebrew alphabet used by `heb_number_to_chars()`.
const ALEF_BET: &[u8; 25] =
    b"0\xE0\xE1\xE2\xE3\xE4\xE5\xE6\xE7\xE8\xE9\xEB\xEC\xEE\xF0\xF1\xF2\xF4\xF6\xF7\xF8\xF9\xFA\0\0";

/// Renders `n` (1..=9999) in Hebrew numerals; `None` outside that range
/// (PHP then prints `(null)`).
pub fn heb_number_to_chars(n: i64, flags: i64) -> Option<Vec<u8>> {
    if !(1..=9999).contains(&n) {
        return None;
    }
    let letter = |i: i64| ALEF_BET[usize::try_from(i).unwrap_or(0)];
    let mut out = Vec::with_capacity(18);
    let mut n = n;
    if n / 1000 != 0 {
        out.push(letter(n / 1000));
        if CAL_JEWISH_ADD_ALAFIM_GERESH & flags != 0 {
            out.push(b'\'');
        }
        if CAL_JEWISH_ADD_ALAFIM & flags != 0 {
            out.extend_from_slice(b" \xE0\xEC\xF4\xE9\xED ");
        }
        n %= 1000;
    }
    let end_of_alafim = out.len();
    while n >= 400 {
        out.push(letter(22));
        n -= 400;
    }
    if n >= 100 {
        out.push(letter(18 + n / 100));
        n %= 100;
    }
    if n == 15 || n == 16 {
        out.push(letter(9));
        out.push(letter(n - 9));
    } else {
        if n >= 10 {
            out.push(letter(9 + n / 10));
            n %= 10;
        }
        if n > 0 {
            out.push(letter(n));
        }
    }
    if CAL_JEWISH_ADD_GERESHAYIM & flags != 0 {
        match out.len() - end_of_alafim {
            0 => {}
            1 => out.push(b'\''),
            _ => {
                let last = out.len() - 1;
                let c = out[last];
                out[last] = b'"';
                out.push(c);
            }
        }
    }
    Some(out)
}

/// `jdtojewish(int $julian_day, bool $hebrew = false, int $flags = 0): string`.
///
/// Returns raw bytes: the Hebrew form is ISO-8859-8 encoded.
pub fn jdtojewish(julian_day: i64, hebrew: bool, flags: i64) -> Result<Vec<u8>, CalError> {
    let ymd = jewish::sdn_to_jewish(julian_day);
    if !hebrew {
        return Ok(ymd.to_string().into_bytes());
    }
    if ymd.year <= 0 || ymd.year > 9999 {
        return Err(CalError::Value("Year out of range (0-9999)".into()));
    }
    let null = || b"(null)".to_vec();
    let mut out = heb_number_to_chars(ymd.day.into(), flags).unwrap_or_else(null);
    out.push(b' ');
    out.extend_from_slice(jewish::heb_month_names(ymd.year.into())[ymd.month_index()]);
    out.push(b' ');
    out.extend(heb_number_to_chars(ymd.year.into(), flags).unwrap_or_else(null));
    Ok(out)
}

/// Result of `jddayofweek()`: an int for mode 0 (and unknown modes), a string otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayOfWeek {
    /// Day number, 0 = Sunday.
    Number(i64),
    /// Day name.
    Name(&'static str),
}

/// `jddayofweek(int $julian_day, int $mode = CAL_DOW_DAYNO): int|string`.
pub fn jddayofweek(julian_day: i64, mode: i64) -> DayOfWeek {
    let i = dow_index(julian_day);
    match mode {
        CAL_DOW_LONG => DayOfWeek::Name(DAY_NAME_LONG[i]),
        CAL_DOW_SHORT => DayOfWeek::Name(DAY_NAME_SHORT[i]),
        _ => DayOfWeek::Number(day_of_week(julian_day)),
    }
}

/// `jdmonthname(int $julian_day, int $mode): string`.
pub fn jdmonthname(julian_day: i64, mode: i64) -> &'static str {
    match mode {
        CAL_MONTH_GREGORIAN_LONG => {
            gregor::MONTH_NAME_LONG[gregor::sdn_to_gregorian(julian_day).month_index()]
        }
        CAL_MONTH_JULIAN_SHORT => {
            gregor::MONTH_NAME_SHORT[julian::sdn_to_julian(julian_day).month_index()]
        }
        CAL_MONTH_JULIAN_LONG => {
            gregor::MONTH_NAME_LONG[julian::sdn_to_julian(julian_day).month_index()]
        }
        CAL_MONTH_JEWISH => {
            let ymd = jewish::sdn_to_jewish(julian_day);
            if ymd.year > 0 {
                jewish::month_names(ymd.year.into())[ymd.month_index()]
            } else {
                ""
            }
        }
        CAL_MONTH_FRENCH => {
            french::FRENCH_MONTH_NAME[french::sdn_to_french(julian_day).month_index()]
        }
        _ => gregor::MONTH_NAME_SHORT[gregor::sdn_to_gregorian(julian_day).month_index()],
    }
}

const SECS_PER_DAY: i64 = 24 * 3600;
const UNIX_EPOCH_JD: i64 = 2_440_588;

/// `jdtounix(int $julian_day): int`.
pub fn jdtounix(julian_day: i64) -> Result<i64, CalError> {
    if julian_day < UNIX_EPOCH_JD || (julian_day - UNIX_EPOCH_JD) > i64::MAX / SECS_PER_DAY {
        return Err(CalError::arg(
            1,
            "julian_day",
            format!(
                "jday must be between {UNIX_EPOCH_JD} and {}",
                i64::MAX / SECS_PER_DAY + UNIX_EPOCH_JD
            ),
        ));
    }
    Ok((julian_day - UNIX_EPOCH_JD) * SECS_PER_DAY)
}

/// `unixtojd(?int $timestamp = null): int|false`.
///
/// The caller resolves a `null` timestamp to the current time.  `localtime`
/// converts a timestamp to a local `(year, month 1..=12, day)` triple (PHP uses
/// `php_localtime_r`); `None` from it yields `Ok(None)`, i.e. PHP `false`.
pub fn unixtojd(
    timestamp: i64,
    localtime: impl FnOnce(i64) -> Option<(i64, i64, i64)>,
) -> Result<Option<i64>, CalError> {
    if timestamp < 0 {
        return Err(CalError::arg(
            1,
            "timestamp",
            "must be greater than or equal to 0",
        ));
    }
    Ok(localtime(timestamp).map(|(y, m, d)| gregor::gregorian_to_sdn(y, m, d)))
}

#[cfg(test)]
mod tests;
