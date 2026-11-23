// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A faithful Rust port of PHP's `ext/date` engine (timelib).
//!
//! This crate knows nothing about PHP values; it exposes plain Rust types
//! that the VM wraps into `DateTime`, `DateTimeImmutable`, `DateTimeZone`,
//! `DateInterval` and `DatePeriod`, plus the procedural `date()`,
//! `strtotime()`, `mktime()`, `date_parse()` … functions.
//!
//! * [`tz`] — the bundled timezone database (identifiers, transitions,
//!   abbreviations, POSIX footer rules).
//! * [`parse`] — `strtotime()` grammar (`parse_date.re`), `createFromFormat`
//!   (`timelib_parse_from_format`) and ISO 8601 intervals/periods.
//! * [`time`] — broken-down time ↔ timestamps, relative time application.
//! * [`interval`] — `diff()`, `add()`, `sub()`.
//! * [`format`](mod@format) — `date()`/`DateTime::format()` and `DateInterval::format()`.
//! * [`astro`] — sunrise/sunset/twilight (`date_sun_info()`).
//! * [`api`] — PHP-function-shaped helpers built on the above.
//!
//! # Example
//!
//! ```
//! use php_date::{api, tz};
//!
//! let ams = tz::get("Europe/Amsterdam").unwrap();
//! // strtotime("next monday 10:00", 1136073600)
//! let ts = api::strtotime(b"next monday 10:00", 1_136_073_600, &ams).unwrap();
//! assert_eq!(api::date(b"D, d M Y H:i:s T", ts, &ams), b"Mon, 02 Jan 2006 10:00:00 CET");
//!
//! // new DateTime('2010-01-31', $ams) ->modify('+1 month')
//! let (t, _errors) = api::date_initialize(b"2010-01-31", None, None, &ams, 0, 0);
//! let mut t = t.unwrap();
//! api::modify(&mut t, b"+1 month").unwrap();
//! assert_eq!(api::format(&t, b"Y-m-d"), b"2010-03-03");
//!
//! // date_parse() errors carry timelib's exact texts and positions.
//! let (_, errors) = php_date::parse::strtotime(b"10:00 11:00");
//! assert_eq!(errors.errors[0].message, "Double time specification");
//! assert_eq!(errors.errors[0].position, 6);
//! ```
//!
//! # Conventions
//!
//! * Strings that PHP treats as binary are `&[u8]` / `Vec<u8>`.
//! * Fields that timelib leaves unset hold [`UNSET`] (`-9999999`).
//! * Messages are returned *without* the `function(): ` / `Class::method(): `
//!   prefix PHP adds when reporting them; the VM adds it.
//! * Arithmetic wraps like the C code does for absurd inputs instead of
//!   panicking.

// The port keeps timelib's field names (y, m, d, h, i, s, ...) and its
// long switch-style functions so it can be checked line-by-line against the
// C sources; these lints would force obscuring that correspondence.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments
)]

pub mod api;
pub mod astro;
pub mod dow;
pub mod format;
pub mod interval;
pub mod parse;
pub mod time;
pub mod tz;

/// timelib's `TIMELIB_UNSET` marker for fields that were not set.
pub const UNSET: i64 = -9_999_999;

pub use time::{RelTime, Time, ZoneType};
pub use tz::TzInfo;
