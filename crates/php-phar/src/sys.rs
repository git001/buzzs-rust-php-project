// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Operating-system access: files addressed by byte paths, the clock, and
//! the libc `localtime_r()` / `mktime()` pair used for zip DOS timestamps.

use std::cell::Cell;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A byte path as a `Path`.
pub fn path(p: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(p))
}

/// Reads a whole file.
pub fn read_file(p: &[u8]) -> io::Result<Vec<u8>> {
    std::fs::read(path(p))
}

thread_local! {
    static NOW: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Pins `time(NULL)` to a fixed value (for deterministic output), or
/// restores the real clock with `None`.
pub fn set_time_override(t: Option<i64>) {
    NOW.with(|n| n.set(t));
}

/// `time(NULL)`.
pub fn now() -> i64 {
    NOW.with(Cell::get).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// `time(NULL)` truncated to the 32-bit on-disk timestamp.
pub fn now32() -> u32 {
    // Truncation is what the C code does when storing time_t into uint32_t.
    crate::util::lo32(now())
}

thread_local! {
    static ENTRY_NOW: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Pins the clock used for *entry* timestamps (new entries and the phar
/// manifest's per-entry times) separately from [`now`]. Some distribution
/// builds of PHP (e.g. Ubuntu's, for reproducible builds) write `0` there
/// while still stamping magic tar/zip entries with the real time; this
/// lets oracle tests reproduce such archives byte for byte.
pub fn set_entry_time_override(t: Option<i64>) {
    ENTRY_NOW.with(|n| n.set(t));
}

/// `time(NULL)` as used for entry timestamps (see
/// [`set_entry_time_override`]).
pub fn entry_now32() -> u32 {
    match ENTRY_NOW.with(Cell::get) {
        Some(t) => crate::util::lo32(t),
        None => now32(),
    }
}

/// Broken-down local time (the fields phar uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tm {
    /// Years since 1900.
    pub year: i32,
    /// 0-11.
    pub mon: i32,
    /// 1-31.
    pub mday: i32,
    /// 0-23.
    pub hour: i32,
    /// 0-59.
    pub min: i32,
    /// 0-60.
    pub sec: i32,
}

/// `localtime_r()` in the process time zone.
pub fn localtime(t: i64) -> Tm {
    let tt: libc::time_t = t;
    // SAFETY: an all-zero `struct tm` is a valid value (plain integers and a
    // nullable `tm_zone` pointer).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference live locals; localtime_r is the
    // reentrant variant and only writes into `tm`.
    let ok = unsafe { !libc::localtime_r(&raw const tt, &raw mut tm).is_null() };
    if !ok {
        return Tm::default();
    }
    Tm {
        year: tm.tm_year,
        mon: tm.tm_mon,
        mday: tm.tm_mday,
        hour: tm.tm_hour,
        min: tm.tm_min,
        sec: tm.tm_sec,
    }
}

/// `mktime()` of a local broken-down time; DST is taken from `localtime(now)`
/// exactly as `phar_zip_d2u_time()` does (it starts from the current time's
/// `struct tm` and overwrites the date/time fields).
pub fn mktime_from_now(fields: Tm) -> i64 {
    let now: libc::time_t = now();
    // SAFETY: see `localtime`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers to locals.
    unsafe {
        libc::localtime_r(&raw const now, &raw mut tm);
    }
    tm.tm_year = fields.year;
    tm.tm_mon = fields.mon;
    tm.tm_mday = fields.mday;
    tm.tm_hour = fields.hour;
    tm.tm_min = fields.min;
    tm.tm_sec = fields.sec;
    // SAFETY: `tm` is a fully initialised struct tm owned by this frame.
    unsafe { libc::mktime(&raw mut tm) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_roundtrip() {
        let t = 1_234_567_890;
        let tm = localtime(t);
        assert!(tm.year > 100);
        assert_eq!(mktime_from_now(tm), t);
        set_time_override(Some(42));
        assert_eq!(now(), 42);
        set_time_override(None);
        assert!(now() > 42);
    }
}
