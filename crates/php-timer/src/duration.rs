// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Durations written for people: `30`, `30s`, `500ms`, `1m30s`, `1.5h`,
//! `2d 12h`.
//!
//! A superset of php-fpm's time values (`fpm_conf_set_time`: a number with
//! an optional `s`, `m`, `h` or `d`) and of the units haproxy and Caddy
//! accept (`us`, `ms`, `s`, `m`, `h`, `d`). A number without a unit is in
//! the unit the setting names (seconds for php-fpm's settings,
//! milliseconds for haproxy-style timeouts). Months and years are left
//! out: their length varies, and `M` would read as minutes to some and as
//! months to others.
//!
//! One difference from php-fpm: it reads `10ms` with `atoi` as 10 seconds,
//! this parser as 10 milliseconds.

use std::fmt;
use std::time::Duration;

/// The unit of a duration's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// `us` (also `µs`).
    Micros,
    /// `ms`.
    Millis,
    /// `s`.
    Seconds,
    /// `m`.
    Minutes,
    /// `h`.
    Hours,
    /// `d`.
    Days,
}

impl Unit {
    /// Nanoseconds per unit.
    #[must_use]
    pub const fn nanos(self) -> u128 {
        match self {
            Unit::Micros => 1_000,
            Unit::Millis => 1_000_000,
            Unit::Seconds => 1_000_000_000,
            Unit::Minutes => 60 * 1_000_000_000,
            Unit::Hours => 3_600 * 1_000_000_000,
            Unit::Days => 86_400 * 1_000_000_000,
        }
    }

    /// The unit's suffix.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Unit::Micros => "us",
            Unit::Millis => "ms",
            Unit::Seconds => "s",
            Unit::Minutes => "m",
            Unit::Hours => "h",
            Unit::Days => "d",
        }
    }

    /// The unit a suffix names (ASCII case-insensitive).
    #[must_use]
    pub fn from_suffix(s: &str) -> Option<Unit> {
        Some(match s.to_ascii_lowercase().as_str() {
            "us" | "µs" => Unit::Micros,
            "ms" => Unit::Millis,
            "s" => Unit::Seconds,
            "m" => Unit::Minutes,
            "h" => Unit::Hours,
            "d" => Unit::Days,
            _ => return None,
        })
    }
}

/// What a setting takes: the unit of a number written without one, the
/// finest step it counts in and its largest value (haproxy's
/// `parse_time_err` derives the last two from the setting's storage: an
/// `unsigned int` of milliseconds or seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSpec {
    /// The unit of a number without one.
    pub bare: Unit,
    /// The setting counts in whole multiples of this unit.
    pub resolution: Unit,
    /// The largest value.
    pub max: Duration,
}

impl DurationSpec {
    /// A setting that counts whole `resolution` units in an `int`: at most
    /// 2^31 - 1 of them, seconds when no unit is given. php-fpm's time
    /// settings count seconds this way, haproxy's timeouts milliseconds.
    #[must_use]
    pub const fn counting(resolution: Unit) -> DurationSpec {
        const MAX: u64 = i32::MAX as u64;
        let max = match resolution {
            Unit::Micros => Duration::from_micros(MAX),
            Unit::Millis => Duration::from_millis(MAX),
            Unit::Seconds => Duration::from_secs(MAX),
            Unit::Minutes => Duration::from_secs(MAX * 60),
            Unit::Hours => Duration::from_secs(MAX * 3_600),
            Unit::Days => Duration::from_secs(MAX * 86_400),
        };
        DurationSpec {
            bare: Unit::Seconds,
            resolution,
            max,
        }
    }

    /// Whole seconds, at most 2^31 - 1 (php-fpm's
    /// `request_terminate_timeout`, `pm.process_idle_timeout`, ...).
    pub const SECONDS: DurationSpec = DurationSpec::counting(Unit::Seconds);

    /// Whole milliseconds, at most 2^31 - 1 (24.8 days): connection
    /// timeouts (keep-alive, idle, read, write).
    pub const MILLIS: DurationSpec = DurationSpec::counting(Unit::Millis);

    /// Checks `nanos` against the resolution and the maximum.
    fn check(&self, nanos: u128) -> Result<Duration, DurationError> {
        let step = self.resolution.nanos();
        if nanos > self.max.as_nanos() {
            return Err(DurationError::TooLarge {
                max: self.max,
                resolution: self.resolution,
            });
        }
        if nanos != 0 && nanos < step {
            return Err(DurationError::TooSmall {
                resolution: self.resolution,
            });
        }
        if nanos % step != 0 {
            return Err(DurationError::Inexact {
                resolution: self.resolution,
            });
        }
        // At most `max`, so it fits.
        let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
        let sub = u32::try_from(nanos % 1_000_000_000).unwrap_or(0);
        Ok(Duration::new(secs, sub))
    }
}

/// Why a duration did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurationError {
    /// Nothing but white space.
    Empty,
    /// A unit that is not one of `us`, `ms`, `s`, `m`, `h`, `d`.
    UnknownUnit(String),
    /// Not a number where one was expected.
    InvalidNumber(String),
    /// A unit not after a number, or a bare number after a part with a
    /// unit (`1m30`).
    MissingUnit,
    /// More than the setting's maximum.
    TooLarge {
        /// The maximum.
        max: Duration,
        /// The setting's resolution, to state the maximum in.
        resolution: Unit,
    },
    /// Not zero, but less than one step of the setting's resolution
    /// (haproxy's `PARSE_TIME_UNDER`: `500ms` for a setting in seconds).
    TooSmall {
        /// The resolution.
        resolution: Unit,
    },
    /// Not a whole number of the setting's resolution (`1.5s` for a
    /// setting in seconds). haproxy rounds up; a value that would mean
    /// something other than it says is refused instead.
    Inexact {
        /// The resolution.
        resolution: Unit,
    },
}

impl fmt::Display for DurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationError::Empty => f.write_str("empty time value"),
            DurationError::UnknownUnit(u) => {
                write!(
                    f,
                    "unknown unit \"{u}\" in time value (use us, ms, s, m, h or d)"
                )
            }
            DurationError::InvalidNumber(n) => write!(f, "invalid number \"{n}\" in time value"),
            DurationError::MissingUnit => f.write_str("time value part without a unit"),
            DurationError::TooLarge { max, resolution } => write!(
                f,
                "time value too large, at most {}{}",
                max.as_nanos() / resolution.nanos(),
                resolution.suffix()
            ),
            DurationError::TooSmall { resolution } => write!(
                f,
                "time value too small, the smallest one other than 0 is 1{}",
                resolution.suffix()
            ),
            DurationError::Inexact { resolution } => write!(
                f,
                "time value is not a whole number of {}",
                resolution.suffix()
            ),
        }
    }
}

impl std::error::Error for DurationError {}

/// Parses `text` as one or more `<number><unit>` parts, white space
/// allowed between them (`1h 30m`, `1h30m`), for a setting as `spec`
/// describes it. A lone number without a unit is in `spec.bare` units.
/// Numbers may have a fraction (`1.5s`), not a sign or an exponent. `0`
/// (in any unit) is accepted: it means "off" for most settings.
///
/// # Errors
///
/// [`DurationError`] for anything else, and for a value outside the
/// setting's range or finer than its resolution.
pub fn parse_duration(text: &str, spec: &DurationSpec) -> Result<Duration, DurationError> {
    let s = text.trim();
    if s.is_empty() {
        return Err(DurationError::Empty);
    }
    let mut total: u128 = 0;
    let mut rest = s;
    let mut parts = 0;
    while !rest.is_empty() {
        let (num, tail) = split_while(rest, is_number);
        if num.is_empty() {
            return Err(DurationError::MissingUnit);
        }
        let (suffix, tail) = split_while(tail, |c| !is_number(c) && !c.is_whitespace());
        let unit = if suffix.is_empty() {
            // Only a value that is one bare number takes the default unit.
            if parts > 0 || !tail.trim_start().is_empty() {
                return Err(DurationError::MissingUnit);
            }
            spec.bare
        } else {
            Unit::from_suffix(suffix)
                .ok_or_else(|| DurationError::UnknownUnit(suffix.to_string()))?
        };
        let too_large = DurationError::TooLarge {
            max: spec.max,
            resolution: spec.resolution,
        };
        total = scaled(num, unit)?
            .and_then(|n| total.checked_add(n))
            .ok_or(too_large)?;
        parts += 1;
        rest = tail.trim_start();
    }
    spec.check(total)
}

/// Whether `c` belongs to a number (digits and the fraction's `.`).
fn is_number(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

/// `s` split after its longest prefix of characters for which `keep`
/// holds.
fn split_while(s: &str, keep: impl Fn(char) -> bool) -> (&str, &str) {
    s.split_at(s.find(|c| !keep(c)).unwrap_or(s.len()))
}

/// `num` (digits with at most one `.`) times `unit`, in nanoseconds,
/// truncated below a nanosecond; `None` when that overflows.
fn scaled(num: &str, unit: Unit) -> Result<Option<u128>, DurationError> {
    let invalid = || DurationError::InvalidNumber(num.to_string());
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if (int.is_empty() && frac.is_empty()) || frac.contains('.') {
        return Err(invalid());
    }
    let per = unit.nanos();
    // `int` is digits only, so a failed parse is an overflow.
    let whole = if int.is_empty() {
        Some(0)
    } else {
        int.parse::<u128>().ok().and_then(|n| n.checked_mul(per))
    };
    let Some(mut nanos) = whole else {
        return Ok(None);
    };
    // The fraction digit by digit: per / 10, per / 100, ... (exact for the
    // units above down to their nanosecond).
    let mut place = per;
    for d in frac.bytes() {
        place /= 10;
        if place == 0 {
            break;
        }
        nanos += u128::from(d - b'0') * place;
    }
    Ok(Some(nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole seconds, no maximum to speak of.
    const SECS: DurationSpec = DurationSpec {
        bare: Unit::Seconds,
        resolution: Unit::Micros,
        max: Duration::MAX,
    };

    fn secs(s: &str) -> Result<Duration, DurationError> {
        parse_duration(s, &SECS)
    }

    #[test]
    fn whole_seconds_as_php_fpm_writes_them() {
        let whole_secs = |s| parse_duration(s, &DurationSpec::SECONDS);
        assert_eq!(whole_secs("30"), Ok(Duration::from_secs(30)));
        assert_eq!(whole_secs("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(whole_secs("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(whole_secs("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(whole_secs("1d"), Ok(Duration::from_secs(86_400)));
        assert_eq!(whole_secs("0"), Ok(Duration::ZERO));
        assert_eq!(
            whole_secs("2147483647"),
            Ok(Duration::from_secs(2_147_483_647))
        );
    }

    #[test]
    fn small_units_combinations_and_fractions() {
        assert_eq!(secs("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(secs("250us"), Ok(Duration::from_micros(250)));
        assert_eq!(secs("250µs"), Ok(Duration::from_micros(250)));
        assert_eq!(secs("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(secs(" 1h 30m "), Ok(Duration::from_secs(5400)));
        assert_eq!(secs("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(secs(".25h"), Ok(Duration::from_secs(900)));
        assert_eq!(secs("10S"), Ok(Duration::from_secs(10)));
        assert_eq!(secs("5M"), Ok(Duration::from_secs(300)));
        assert_eq!(secs("2D"), Ok(Duration::from_secs(172_800)));
    }

    #[test]
    fn bare_numbers_take_the_settings_unit() {
        let ms = DurationSpec {
            bare: Unit::Millis,
            ..DurationSpec::MILLIS
        };
        assert_eq!(parse_duration("1500", &ms), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_duration("2s", &ms), Ok(Duration::from_secs(2)));
        assert_eq!(
            parse_duration("30", &DurationSpec::MILLIS),
            Ok(Duration::from_secs(30))
        );
    }

    #[test]
    fn range_and_resolution_of_the_setting() {
        let whole_secs = |s| parse_duration(s, &DurationSpec::SECONDS);
        let in_secs = Unit::Seconds;
        // haproxy's PARSE_TIME_UNDER, refused instead of rounded up.
        assert_eq!(
            whole_secs("500ms"),
            Err(DurationError::TooSmall {
                resolution: in_secs
            })
        );
        assert_eq!(
            whole_secs("1.5s"),
            Err(DurationError::Inexact {
                resolution: in_secs
            })
        );
        assert_eq!(whole_secs("0ms"), Ok(Duration::ZERO));
        assert_eq!(
            whole_secs("2147483648"),
            Err(DurationError::TooLarge {
                max: DurationSpec::SECONDS.max,
                resolution: in_secs
            })
        );
        let whole_ms = |s| parse_duration(s, &DurationSpec::MILLIS);
        assert_eq!(
            whole_ms("1500us"),
            Err(DurationError::Inexact {
                resolution: Unit::Millis
            })
        );
        assert_eq!(
            whole_ms("2147483647ms"),
            Ok(Duration::from_millis(2_147_483_647))
        );
        assert!(matches!(
            whole_ms("25d"),
            Err(DurationError::TooLarge { .. })
        ));
        assert_eq!(
            whole_ms("25d").map_err(|e| e.to_string()),
            Err("time value too large, at most 2147483647ms".to_string())
        );
        assert_eq!(
            whole_secs("500ms").map_err(|e| e.to_string()),
            Err("time value too small, the smallest one other than 0 is 1s".to_string())
        );
    }

    #[test]
    fn rejects() {
        assert_eq!(secs(""), Err(DurationError::Empty));
        assert_eq!(secs("  "), Err(DurationError::Empty));
        assert_eq!(secs("5x"), Err(DurationError::UnknownUnit("x".into())));
        assert_eq!(secs("1y"), Err(DurationError::UnknownUnit("y".into())));
        assert_eq!(secs("1m30"), Err(DurationError::MissingUnit));
        assert_eq!(secs("30 60"), Err(DurationError::MissingUnit));
        assert_eq!(secs("s"), Err(DurationError::MissingUnit));
        assert_eq!(secs("-5s"), Err(DurationError::MissingUnit));
        assert_eq!(
            secs("1.2.3s"),
            Err(DurationError::InvalidNumber("1.2.3".into()))
        );
        assert_eq!(secs("."), Err(DurationError::InvalidNumber(".".into())));
        assert!(matches!(
            secs("99999999999999999999999999999999999999999d"),
            Err(DurationError::TooLarge { .. })
        ));
    }
}
