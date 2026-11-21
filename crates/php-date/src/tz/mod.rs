// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The bundled timezone database (port of timelib's `parse_tz.c` plus the
//! `timezonedb.h`, `timezonemap.h` and `fallbackmap.h` tables).
//!
//! The database is PHP's own *slim* timezonedb, embedded byte-for-byte
//! (`tzdb.bin`, regenerated with `tools/gen_tzdb.py`), so that
//! `timezone_version_get()`, `timezone_identifiers_list()`,
//! `DateTimeZone::getTransitions()` and `getLocation()` match PHP exactly.

#[allow(clippy::unreadable_literal)]
mod generated;
pub mod posix;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::time::unixtime2date;
use posix::{parse_posix_str, transitions_for_year, PosixStr};

/// The raw (slim) database blob, exactly as embedded in PHP.
static DATA: &[u8] = include_bytes!("tzdb.bin");

/// One row of the abbreviation tables (`timezone_abbreviations_list()`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbbrEntry {
    /// Lower-case abbreviation, e.g. `"cest"`.
    pub name: &'static str,
    /// Whether this abbreviation denotes daylight saving time.
    pub dst: bool,
    /// UTC offset in seconds, *including* the DST hour.
    pub offset: i64,
    /// Associated timezone identifier, if any.
    pub tz_id: Option<&'static str>,
}

/// The special entry returned for "utc"/"gmt" by [`abbr_search`].
static UTC_ENTRY: AbbrEntry = AbbrEntry {
    name: "utc",
    dst: false,
    offset: 0,
    tz_id: Some("UTC"),
};

/// Errors from loading a zone out of the database (timelib error codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzError {
    /// `TIMELIB_ERROR_CORRUPT_TRANSITIONS_DONT_INCREASE`
    CorruptTransitionsDontIncrease,
    /// `TIMELIB_ERROR_CORRUPT_NO_64BIT_PREAMBLE`
    CorruptNo64BitPreamble,
    /// `TIMELIB_ERROR_UNSUPPORTED_VERSION`
    UnsupportedVersion,
    /// `TIMELIB_ERROR_NO_SUCH_TIMEZONE`
    NoSuchTimezone,
    /// `TIMELIB_ERROR_CORRUPT_POSIX_STRING`
    CorruptPosixString,
}

impl fmt::Display for TzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CorruptTransitionsDontIncrease => {
                "Corrupt tzfile: The transitions in the file don't always increase"
            }
            Self::CorruptNo64BitPreamble => {
                "Corrupt tzfile: The expected 64-bit preamble is missing"
            }
            Self::UnsupportedVersion => {
                "The version used in this timezone identifier is unsupported"
            }
            Self::NoSuchTimezone => "No timezone with this name could be found",
            Self::CorruptPosixString => "The embedded POSIX string is not valid",
        })
    }
}

impl std::error::Error for TzError {}

/// A local time type (`ttinfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TType {
    /// UTC offset in seconds.
    pub offset: i32,
    /// Whether this is daylight saving time.
    pub isdst: bool,
    /// Index into the abbreviation character table.
    pub abbr_idx: usize,
}

/// Geographic information for a zone (`DateTimeZone::getLocation()`).
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    /// ISO 3166 country code, `"??"` when unknown.
    pub country_code: String,
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
    /// Free-form comments.
    pub comments: String,
}

/// The offset information valid at a given instant (`timelib_time_offset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeOffset {
    /// UTC offset in seconds.
    pub offset: i32,
    /// Leap seconds (always 0 for PHP's database).
    pub leap_secs: u32,
    /// Whether DST is in effect.
    pub is_dst: bool,
    /// Abbreviation, e.g. `"CEST"`.
    pub abbr: String,
    /// Start of the period this offset belongs to (`i64::MIN` if unbounded).
    pub transition_time: i64,
}

/// One entry of `DateTimeZone::getTransitions()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// Timestamp of the transition.
    pub ts: i64,
    /// `ts` formatted as `x-m-d\TH:i:sP` in UTC.
    pub time: String,
    /// UTC offset in seconds.
    pub offset: i32,
    /// Whether the new period is DST.
    pub isdst: bool,
    /// Abbreviation of the new period.
    pub abbr: String,
}

/// A parsed timezone (`timelib_tzinfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct TzInfo {
    /// The identifier, exactly as it was requested (PHP keeps the caller's case).
    pub name: String,
    /// Backwards-compatible ("BC") flag from the PHP preamble.
    pub bc: bool,
    /// Location data.
    pub location: Location,
    /// Transition times (64-bit section).
    pub trans: Vec<i64>,
    /// Type index for each transition.
    pub trans_idx: Vec<u8>,
    /// Local time types (including those synthesised from the POSIX footer).
    pub types: Vec<TType>,
    /// NUL-separated abbreviation characters.
    pub abbr_chars: Vec<u8>,
    /// The POSIX footer string (may be empty).
    pub posix_string: String,
    /// The parsed POSIX footer.
    pub posix: Option<PosixStr>,
}

struct ByteReader<'a> {
    d: &'a [u8],
    p: usize,
}

impl ByteReader<'_> {
    fn u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        for (i, x) in b.iter_mut().enumerate() {
            *x = self.d.get(self.p + i).copied().unwrap_or(0);
        }
        self.p += 4;
        u32::from_be_bytes(b)
    }

    fn i64(&mut self) -> i64 {
        let mut b = [0u8; 8];
        for (i, x) in b.iter_mut().enumerate() {
            *x = self.d.get(self.p + i).copied().unwrap_or(0);
        }
        self.p += 8;
        i64::from_be_bytes(b)
    }

    fn u8(&mut self) -> u8 {
        let v = self.d.get(self.p).copied().unwrap_or(0);
        self.p += 1;
        v
    }

    fn bytes(&mut self, n: usize) -> &[u8] {
        let start = self.p.min(self.d.len());
        let end = (self.p + n).min(self.d.len());
        self.p += n;
        &self.d[start..end]
    }

    fn usize(&mut self) -> usize {
        usize::try_from(self.u32()).unwrap_or(usize::MAX / 16)
    }
}

/// `timelib_strcasecmp`: ASCII case-insensitive comparison, shorter strings first.
pub(crate) fn strcasecmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}

/// Binary search of the index exactly as `seek_to_tz_position` does it.
fn seek(name: &str) -> Option<usize> {
    let (mut left, mut right) = (
        0_i64,
        i64::try_from(generated::INDEX.len()).unwrap_or(0) - 1,
    );
    while left <= right {
        let mid = (left + right) >> 1;
        let idx = usize::try_from(mid).ok()?;
        match strcasecmp(name, generated::INDEX[idx].0) {
            std::cmp::Ordering::Less => right = mid - 1,
            std::cmp::Ordering::Greater => left = mid + 1,
            std::cmp::Ordering::Equal => return Some(idx),
        }
    }
    None
}

/// Version of the bundled database (`timezone_version_get()`).
pub fn version() -> &'static str {
    generated::VERSION
}

/// Whether `name` is a known identifier (case-insensitive).
pub fn is_valid_id(name: &str) -> bool {
    !name.contains('\0') && seek(name).is_some()
}

/// All identifiers in database order (including backwards-compatible ones).
pub fn all_identifiers() -> impl Iterator<Item = &'static str> {
    generated::INDEX.iter().map(|(n, _)| *n)
}

/// `DateTimeZone::AFRICA` .. `DateTimeZone::PER_COUNTRY` group constants.
pub mod group {
    /// `DateTimeZone::AFRICA`
    pub const AFRICA: i64 = 1;
    /// `DateTimeZone::AMERICA`
    pub const AMERICA: i64 = 2;
    /// `DateTimeZone::ANTARCTICA`
    pub const ANTARCTICA: i64 = 4;
    /// `DateTimeZone::ARCTIC`
    pub const ARCTIC: i64 = 8;
    /// `DateTimeZone::ASIA`
    pub const ASIA: i64 = 16;
    /// `DateTimeZone::ATLANTIC`
    pub const ATLANTIC: i64 = 32;
    /// `DateTimeZone::AUSTRALIA`
    pub const AUSTRALIA: i64 = 64;
    /// `DateTimeZone::EUROPE`
    pub const EUROPE: i64 = 128;
    /// `DateTimeZone::INDIAN`
    pub const INDIAN: i64 = 256;
    /// `DateTimeZone::PACIFIC`
    pub const PACIFIC: i64 = 512;
    /// `DateTimeZone::UTC`
    pub const UTC: i64 = 1024;
    /// `DateTimeZone::ALL`
    pub const ALL: i64 = 2047;
    /// `DateTimeZone::ALL_WITH_BC`
    pub const ALL_WITH_BC: i64 = 4095;
    /// `DateTimeZone::PER_COUNTRY`
    pub const PER_COUNTRY: i64 = 4096;
}

fn check_id_allowed(id: &str, what: i64) -> bool {
    const PREFIXES: [(i64, &str); 11] = [
        (group::AFRICA, "Africa/"),
        (group::AMERICA, "America/"),
        (group::ANTARCTICA, "Antarctica/"),
        (group::ARCTIC, "Arctic/"),
        (group::ASIA, "Asia/"),
        (group::ATLANTIC, "Atlantic/"),
        (group::AUSTRALIA, "Australia/"),
        (group::EUROPE, "Europe/"),
        (group::INDIAN, "Indian/"),
        (group::PACIFIC, "Pacific/"),
        (group::UTC, "UTC"),
    ];
    PREFIXES.iter().any(|(bit, prefix)| {
        what & bit != 0
            && id.len() >= prefix.len()
            && id.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    })
}

/// The error message PHP throws when `PER_COUNTRY` is used without a valid code.
pub const PER_COUNTRY_ERROR: &str = "must be a two-letter ISO 3166-1 compatible country code when argument #1 ($timezoneGroup) is DateTimeZone::PER_COUNTRY";

/// `timezone_identifiers_list($group, $country)`.
///
/// Returns `Err(PER_COUNTRY_ERROR)` (a `ValueError` for argument #2) when
/// `what` is `PER_COUNTRY` and `country` is not exactly two bytes long.
pub fn identifiers_list(
    what: i64,
    country: Option<&str>,
) -> Result<Vec<&'static str>, &'static str> {
    let country = country.unwrap_or("").as_bytes();
    if what == group::PER_COUNTRY && country.len() != 2 {
        return Err(PER_COUNTRY_ERROR);
    }
    let mut out = Vec::new();
    for (id, pos) in &generated::INDEX {
        let pos = *pos as usize;
        if what == group::PER_COUNTRY {
            if DATA.get(pos + 5) == Some(&country[0]) && DATA.get(pos + 6) == Some(&country[1]) {
                out.push(*id);
            }
        } else if what == group::ALL_WITH_BC
            || (check_id_allowed(id, what) && DATA.get(pos + 4) == Some(&1))
        {
            out.push(*id);
        }
    }
    Ok(out)
}

/// `timezone_abbreviations_list()` rows, in PHP's order.
pub fn abbreviations_list() -> &'static [AbbrEntry] {
    &generated::ABBREVIATIONS
}

/// timelib's `abbr_search()`: find an abbreviation entry by name, preferring
/// one with `gmtoffset` (pass -1 for "any"), falling back to the offset/DST
/// fallback map.
pub fn abbr_search(word: &str, gmtoffset: i64, isdst: i64) -> Option<&'static AbbrEntry> {
    if word.eq_ignore_ascii_case("utc") || word.eq_ignore_ascii_case("gmt") {
        return Some(&UTC_ENTRY);
    }
    let mut first_found: Option<&'static AbbrEntry> = None;
    for tp in &generated::ABBREVIATIONS {
        if strcasecmp(word, tp.name).is_eq() {
            if first_found.is_none() {
                first_found = Some(tp);
                if gmtoffset == -1 {
                    return Some(tp);
                }
            }
            if tp.offset == gmtoffset {
                return Some(tp);
            }
        }
    }
    if first_found.is_some() {
        return first_found;
    }
    generated::FALLBACK
        .iter()
        .find(|f| f.offset == gmtoffset && i64::from(f.dst) == isdst)
}

/// `timezone_name_from_abbr($abbr, $utcOffset = -1, $isDST = -1)`.
pub fn timezone_id_from_abbr(abbr: &str, gmtoffset: i64, isdst: i64) -> Option<&'static str> {
    abbr_search(abbr, gmtoffset, isdst).and_then(|e| e.tz_id)
}

/// Parses the zone `name` out of the bundled database
/// (`timelib_parse_tzfile`). Case-insensitive; the returned zone keeps
/// `name` verbatim.
pub fn parse_tzfile(name: &str) -> Result<TzInfo, TzError> {
    if name.contains('\0') {
        return Err(TzError::NoSuchTimezone);
    }
    let idx = seek(name).ok_or(TzError::NoSuchTimezone)?;
    let pos = generated::INDEX[idx].1 as usize;
    parse_blob(name, &DATA[pos..])
}

fn parse_blob(name: &str, data: &[u8]) -> Result<TzInfo, TzError> {
    let mut r = ByteReader { d: data, p: 0 };
    let is_php = data.starts_with(b"PHP");
    let (version, bc, country_code) = if is_php {
        let version = i32::from(data.get(3).copied().unwrap_or(0)) - i32::from(b'0');
        let bc = data.get(4) == Some(&1);
        let cc = String::from_utf8_lossy(data.get(5..7).unwrap_or(b"??"))
            .trim_end_matches('\0')
            .to_string();
        r.p = 20;
        (version, bc, cc)
    } else if data.starts_with(b"TZif") {
        let version = match data.get(4) {
            Some(0) => 0,
            Some(b'2') => 2,
            Some(b'3') => 3,
            Some(b'4') => 4,
            _ => -1,
        };
        r.p = 20;
        (version, false, "??".to_string())
    } else {
        (-1, false, String::new())
    };
    if !(2..=4).contains(&version) {
        return Err(TzError::UnsupportedVersion);
    }

    // 32-bit header and data (skipped).
    let isgmt32 = r.usize();
    let isstd32 = r.usize();
    let leap32 = r.usize();
    let time32 = r.usize();
    let type32 = r.usize();
    let char32 = r.usize();
    r.p += time32 * 5 + type32 * 6 + char32 + leap32 * 8 + isstd32 + isgmt32;

    let pre = r.bytes(5);
    if !(pre == b"TZif2" || pre == b"TZif3" || pre == b"TZif4") {
        return Err(TzError::CorruptNo64BitPreamble);
    }
    r.p += 15;
    let isgmt = r.usize();
    let isstd = r.usize();
    let leapcnt = r.usize();
    let timecnt = r.usize();
    let typecnt = r.usize();
    let charcnt = r.usize();

    let mut trans = Vec::with_capacity(timecnt);
    for i in 0..timecnt {
        let t = r.i64();
        if i > 0 && t <= trans[i - 1] {
            return Err(TzError::CorruptTransitionsDontIncrease);
        }
        trans.push(t);
    }
    let trans_idx = r.bytes(timecnt).to_vec();
    let mut types = Vec::with_capacity(typecnt + 2);
    for _ in 0..typecnt {
        let offset = i32::from_be_bytes(r.u32().to_be_bytes());
        let isdst = r.u8() != 0;
        let abbr_idx = usize::from(r.u8());
        types.push(TType {
            offset,
            isdst,
            abbr_idx,
        });
    }
    let abbr_chars = r.bytes(charcnt).to_vec();
    r.p += leapcnt * 12 + isstd + isgmt;

    // POSIX string delimited by '\n'.
    r.p += 1;
    let begin = r.p.min(data.len());
    while r.p < data.len() && data[r.p] != b'\n' {
        r.p += 1;
    }
    let posix_string = String::from_utf8_lossy(&data[begin..r.p.min(data.len())]).into_owned();
    r.p += 1;

    let mut tz = TzInfo {
        name: name.to_string(),
        bc,
        location: Location {
            country_code,
            latitude: 0.0,
            longitude: 0.0,
            comments: "?".to_string(),
        },
        trans,
        trans_idx,
        types,
        abbr_chars,
        posix_string,
        posix: None,
    };

    if !tz.posix_string.is_empty() && !tz.integrate_posix_string() {
        return Err(TzError::CorruptPosixString);
    }

    if is_php {
        let lat = f64::from(r.u32());
        let lon = f64::from(r.u32());
        let len = r.usize();
        tz.location.latitude = (lat / 100_000.0) - 90.0;
        tz.location.longitude = (lon / 100_000.0) - 180.0;
        tz.location.comments = String::from_utf8_lossy(r.bytes(len)).into_owned();
    }
    Ok(tz)
}

impl TzInfo {
    /// Abbreviation string stored at `idx` in the character table.
    pub fn abbr_at(&self, idx: usize) -> &str {
        let tail = self.abbr_chars.get(idx..).unwrap_or(&[]);
        let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
        std::str::from_utf8(&tail[..end]).unwrap_or("")
    }

    fn find_ttinfo_index(&self, offset: i64, isdst: bool, abbr: &str) -> Option<usize> {
        self.types.iter().position(|t| {
            i64::from(t.offset) == offset && t.isdst == isdst && self.abbr_at(t.abbr_idx) == abbr
        })
    }

    fn add_new_ttinfo(&mut self, offset: i64, isdst: bool, abbr: &str) -> usize {
        let abbr_idx = self.abbr_chars.len();
        self.abbr_chars.extend_from_slice(abbr.as_bytes());
        self.abbr_chars.push(0);
        self.types.push(TType {
            offset: i32::try_from(offset).unwrap_or(0),
            isdst,
            abbr_idx,
        });
        self.types.len() - 1
    }

    fn integrate_posix_string(&mut self) -> bool {
        let Some(mut ps) = parse_posix_str(&self.posix_string) else {
            return false;
        };
        if let Some(i) = self.find_ttinfo_index(ps.std_offset, false, &ps.std) {
            ps.type_index_std = i;
        } else {
            // timelib returns early here, leaving the DST index at 0.
            ps.type_index_std = self.add_new_ttinfo(ps.std_offset, false, &ps.std.clone());
            self.posix = Some(ps);
            return true;
        }
        if let Some(dst) = ps.dst.clone() {
            ps.type_index_dst = match self.find_ttinfo_index(ps.dst_offset, true, &dst) {
                Some(i) => i,
                None => self.add_new_ttinfo(ps.dst_offset, true, &dst),
            };
        }
        self.posix = Some(ps);
        true
    }

    fn fetch_posix_offset(&self, ts: i64) -> Option<(usize, Option<i64>)> {
        let ps = self.posix.as_ref()?;
        if ps.dst_end.is_none() {
            return Some((ps.type_index_std, self.trans.last().copied()));
        }
        let (year, _, _) = unixtime2date(ts);
        let mut times = Vec::with_capacity(6);
        for y in [year.wrapping_sub(1), year, year.wrapping_add(1)] {
            times.extend_from_slice(&transitions_for_year(ps, y));
        }
        for i in 1..times.len() {
            if ts < times[i].0 {
                return Some((times[i - 1].1, Some(times[i - 1].0)));
            }
        }
        None
    }

    /// `timelib_fetch_timezone_offset`: the type index valid at `ts`, plus
    /// the start of that period.
    pub fn fetch_offset(&self, ts: i64) -> Option<(usize, i64)> {
        if self.trans.is_empty() {
            if self.posix.is_some() {
                return self.fetch_posix_offset(ts).map(|(i, _)| (i, i64::MIN));
            }
            if self.types.len() == 1 {
                return Some((0, i64::MIN));
            }
            return None;
        }
        if ts < self.trans[0] {
            return Some((0, i64::MIN));
        }
        let last = self.trans.len() - 1;
        if ts >= self.trans[last] {
            if self.posix.is_some() {
                return self
                    .fetch_posix_offset(ts)
                    .map(|(i, tt)| (i, tt.unwrap_or(i64::MIN)));
            }
            return Some((usize::from(self.trans_idx[last]), self.trans[last]));
        }
        let (mut left, mut right) = (0usize, last);
        while right - left > 1 {
            let mid = (left + right) >> 1;
            if ts < self.trans[mid] {
                right = mid;
            } else {
                left = mid;
            }
        }
        Some((usize::from(self.trans_idx[left]), self.trans[left]))
    }

    /// `timelib_get_time_zone_info`.
    pub fn time_zone_info(&self, ts: i64) -> TimeOffset {
        match self
            .fetch_offset(ts)
            .and_then(|(i, tt)| self.types.get(i).map(|t| (t, tt)))
        {
            Some((t, tt)) => TimeOffset {
                offset: t.offset,
                leap_secs: 0,
                is_dst: t.isdst,
                abbr: self.abbr_at(t.abbr_idx).to_string(),
                transition_time: tt,
            },
            None => TimeOffset {
                offset: 0,
                leap_secs: 0,
                is_dst: false,
                abbr: self.abbr_at(0).to_string(),
                transition_time: 0,
            },
        }
    }

    /// `timelib_get_time_zone_offset_info`: (offset, transition time, `is_dst`).
    pub fn offset_info(&self, ts: i64) -> Option<(i32, i64, bool)> {
        let (i, tt) = self.fetch_offset(ts)?;
        let t = self.types.get(i)?;
        Some((t.offset, tt, t.isdst))
    }

    /// `timelib_timestamp_is_in_dst`: 1, 0, or -1 if unknown.
    pub fn is_dst(&self, ts: i64) -> i32 {
        match self.offset_info(ts) {
            Some((_, _, dst)) => i32::from(dst),
            None => -1,
        }
    }

    fn transition_for_type(&self, type_index: usize, ts: i64) -> Transition {
        let t = self.types.get(type_index).copied().unwrap_or(TType {
            offset: 0,
            isdst: false,
            abbr_idx: 0,
        });
        Transition {
            ts,
            time: crate::format::date_gmt("x-m-d\\TH:i:sP", ts),
            offset: t.offset,
            isdst: t.isdst,
            abbr: self.abbr_at(t.abbr_idx).to_string(),
        }
    }

    /// `DateTimeZone::getTransitions($begin, $end)`. Use `i64::MIN` and
    /// `i64::from(i32::MAX)` for PHP's defaults.
    pub fn transitions(&self, timestamp_begin: i64, timestamp_end: i64) -> Vec<Transition> {
        let mut out = Vec::new();
        let timecnt = self.trans.len();
        let mut begin = 0usize;
        let found;
        if timestamp_begin == i64::MIN {
            out.push(self.transition_for_type(0, timestamp_begin));
            found = true;
        } else {
            let mut f = false;
            while begin < timecnt {
                if self.trans[begin] > timestamp_begin {
                    if begin > 0 {
                        out.push(self.transition_for_type(
                            usize::from(self.trans_idx[begin - 1]),
                            timestamp_begin,
                        ));
                    } else {
                        out.push(self.transition_for_type(0, timestamp_begin));
                    }
                    f = true;
                    break;
                }
                begin += 1;
            }
            found = f;
        }

        let has_dst_rule = self.posix.as_ref().is_some_and(|p| p.dst_end.is_some());
        if found {
            for i in begin..timecnt {
                if self.trans[i] < timestamp_end {
                    out.push(
                        self.transition_for_type(usize::from(self.trans_idx[i]), self.trans[i]),
                    );
                } else {
                    return out;
                }
            }
        } else if timecnt > 0 {
            if has_dst_rule {
                let to = self.time_zone_info(timestamp_begin);
                out.push(Transition {
                    ts: timestamp_begin,
                    time: crate::format::date_gmt("x-m-d\\TH:i:sP", timestamp_begin),
                    offset: to.offset,
                    isdst: to.is_dst,
                    abbr: to.abbr,
                });
            } else {
                out.push(self.transition_for_type(
                    usize::from(self.trans_idx[timecnt - 1]),
                    timestamp_begin,
                ));
            }
        } else {
            out.push(self.transition_for_type(0, timestamp_begin));
        }

        if let (true, Some(ps)) = (has_dst_rule, self.posix.as_ref()) {
            let last_transition_ts = self.trans.last().copied().unwrap_or(0);
            let (start_y, _, _) = unixtime2date(last_transition_ts);
            let (end_y, _, _) = unixtime2date(timestamp_end);
            let mut y = start_y;
            while y <= end_y {
                for (t, ty) in transitions_for_year(ps, y) {
                    if t <= last_transition_ts || t < timestamp_begin {
                        continue;
                    }
                    if t > timestamp_end {
                        return out;
                    }
                    out.push(self.transition_for_type(ty, t));
                }
                y += 1;
            }
        }
        out
    }
}

fn cache() -> &'static Mutex<HashMap<String, Arc<TzInfo>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<TzInfo>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Looks up (and caches) a zone by identifier, case-insensitively, like
/// PHP's `php_date_parse_tzfile`. The zone keeps the caller's spelling.
pub fn get(name: &str) -> Option<Arc<TzInfo>> {
    if let Ok(c) = cache().lock() {
        if let Some(tz) = c.get(name) {
            return Some(Arc::clone(tz));
        }
    }
    let tz = Arc::new(parse_tzfile(name).ok()?);
    if let Ok(mut c) = cache().lock() {
        c.insert(name.to_string(), Arc::clone(&tz));
    }
    Some(tz)
}

thread_local! {
    /// `DATEG(timezone)`: the zone `date_default_timezone_set()` chose for
    /// the request.
    static OVERRIDE: std::cell::RefCell<Option<Arc<TzInfo>>> = const { std::cell::RefCell::new(None) };
    /// `DATEG(default_timezone)`: the valid `date.timezone` of the request.
    static CONFIGURED: std::cell::RefCell<Option<Arc<TzInfo>>> = const { std::cell::RefCell::new(None) };
}

/// The default timezone of the calling thread (`guess_timezone()`): the
/// one `date_default_timezone_set()` chose, else `date.timezone`, else
/// UTC. ext/date keeps both up to date ([`set_default_override`],
/// [`set_default_configured`]); everything else only reads it here, as
/// it reads the clock through `php_support::time::now()`.
#[must_use]
pub fn default_zone() -> Arc<TzInfo> {
    OVERRIDE
        .with(|o| o.borrow().clone())
        .or_else(|| CONFIGURED.with(|c| c.borrow().clone()))
        .or_else(|| get("UTC"))
        .expect("the timezone database has UTC")
}

/// Sets (`Some`) or clears (`None`, request end) the zone chosen with
/// `date_default_timezone_set()`.
pub fn set_default_override(zone: Option<Arc<TzInfo>>) {
    OVERRIDE.with(|o| *o.borrow_mut() = zone);
}

/// Sets the zone `date.timezone` names (`None`: unset or invalid).
pub fn set_default_configured(zone: Option<Arc<TzInfo>>) {
    CONFIGURED.with(|c| *c.borrow_mut() = zone);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_count() {
        assert_eq!(version(), "2026.6");
        assert_eq!(all_identifiers().count(), 598);
    }

    #[test]
    fn lookup_is_case_insensitive_and_keeps_name() {
        let tz = get("europe/amsterdam").unwrap();
        assert_eq!(tz.name, "europe/amsterdam");
        assert_eq!(tz.location.country_code, "NL");
        assert!(get("Mars/Olympus_Mons").is_none());
        assert!(is_valid_id("UTC"));
        assert!(is_valid_id("utc"));
        assert!(!is_valid_id("Foo"));
    }

    #[test]
    fn offsets() {
        let tz = get("Europe/Amsterdam").unwrap();
        let o = tz.time_zone_info(1_119_000_000); // June 2005
        assert_eq!((o.offset, o.is_dst, o.abbr.as_str()), (7200, true, "CEST"));
        let o = tz.time_zone_info(1_104_537_600); // Jan 2005
        assert_eq!((o.offset, o.is_dst, o.abbr.as_str()), (3600, false, "CET"));
        // Far future: computed from the POSIX footer.
        let o = tz.time_zone_info(4_102_444_800 + 180 * 86400); // mid 2100
        assert_eq!((o.offset, o.abbr.as_str()), (7200, "CEST"));
        let utc = get("UTC").unwrap();
        assert_eq!(utc.time_zone_info(0).abbr, "UTC");
    }

    #[test]
    fn identifiers_list_groups() {
        let all = identifiers_list(group::ALL, None).unwrap();
        assert_eq!(all.last(), Some(&"UTC"));
        assert!(all.contains(&"Europe/London"));
        assert!(!all.contains(&"US/Eastern"));
        let bc = identifiers_list(group::ALL_WITH_BC, None).unwrap();
        assert_eq!(bc.len(), 598);
        let nl = identifiers_list(group::PER_COUNTRY, Some("NL")).unwrap();
        assert_eq!(nl, vec!["Europe/Amsterdam"]);
        assert!(identifiers_list(group::PER_COUNTRY, Some("N")).is_err());
    }

    #[test]
    fn abbreviation_search() {
        assert_eq!(timezone_id_from_abbr("CET", -1, -1), Some("Europe/Berlin"));
        assert_eq!(timezone_id_from_abbr("gmt", -1, -1), Some("UTC"));
        assert_eq!(timezone_id_from_abbr("", 3600, 0), Some("Europe/Paris"));
        assert_eq!(timezone_id_from_abbr("foo", 12345, 0), None);
    }
}
