// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `main/php_syslog.c` and the `syslog.*` INI rules of `main/main.c`.
//!
//! [`php_syslog`] opens the log with `syslog.ident` / `syslog.facility`
//! unless the script called [`php_openlog`] (`PG(have_called_openlog)`),
//! then sends the message through the `syslog.filter` rules
//! (`php_syslog_str`). The open state is per thread, as `PG()` is in a
//! ZTS build; `openlog(3)` itself is process-wide in libc.

use std::cell::Cell;
use std::ffi::CString;

/// `syslog.filter` values (`PHP_SYSLOG_FILTER_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyslogFilter {
    /// `all`: control characters pass, newlines split the message.
    All,
    /// `no-ctrl` (default): control characters are escaped as `\xNN`.
    #[default]
    NoCtrl,
    /// `ascii`: control and 8-bit characters are escaped.
    Ascii,
    /// `raw`: the message is sent as is.
    Raw,
}

impl SyslogFilter {
    /// `OnSetLogFilter`: the filter an INI value names (case-sensitive);
    /// `None` for anything else, which the INI handler refuses.
    #[must_use]
    pub fn from_ini(value: &[u8]) -> Option<Self> {
        match value {
            b"all" => Some(Self::All),
            b"no-ctrl" => Some(Self::NoCtrl),
            b"ascii" => Some(Self::Ascii),
            b"raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

/// `OnSetFacility`: the facility an INI value names, as `LOG_*` or by its
/// short name (case-sensitive, like php-src); `None` for anything else.
#[must_use]
pub fn facility_from_ini(value: &[u8]) -> Option<i32> {
    const NAMES: &[(&[u8], &[u8], i32)] = &[
        (b"LOG_AUTH", b"auth", libc::LOG_AUTH),
        (b"LOG_AUTHPRIV", b"authpriv", libc::LOG_AUTHPRIV),
        (b"LOG_CRON", b"cron", libc::LOG_CRON),
        (b"LOG_DAEMON", b"daemon", libc::LOG_DAEMON),
        (b"LOG_FTP", b"ftp", libc::LOG_FTP),
        (b"LOG_KERN", b"kern", libc::LOG_KERN),
        (b"LOG_LPR", b"lpr", libc::LOG_LPR),
        (b"LOG_MAIL", b"mail", libc::LOG_MAIL),
        (b"LOG_NEWS", b"news", libc::LOG_NEWS),
        (b"LOG_SYSLOG", b"syslog", libc::LOG_SYSLOG),
        (b"LOG_USER", b"user", libc::LOG_USER),
        (b"LOG_UUCP", b"uucp", libc::LOG_UUCP),
        (b"LOG_LOCAL0", b"local0", libc::LOG_LOCAL0),
        (b"LOG_LOCAL1", b"local1", libc::LOG_LOCAL1),
        (b"LOG_LOCAL2", b"local2", libc::LOG_LOCAL2),
        (b"LOG_LOCAL3", b"local3", libc::LOG_LOCAL3),
        (b"LOG_LOCAL4", b"local4", libc::LOG_LOCAL4),
        (b"LOG_LOCAL5", b"local5", libc::LOG_LOCAL5),
        (b"LOG_LOCAL6", b"local6", libc::LOG_LOCAL6),
        (b"LOG_LOCAL7", b"local7", libc::LOG_LOCAL7),
    ];
    if value == b"security" {
        return Some(libc::LOG_AUTH);
    }
    NAMES
        .iter()
        .find(|(long, short, _)| value == *long || value == *short)
        .map(|(_, _, f)| *f)
}

/// The `LOG_*` constants ext/standard registers.
pub const SYSLOG_CONSTANTS: &[(&str, i64)] = &[
    ("LOG_EMERG", libc::LOG_EMERG as i64),
    ("LOG_ALERT", libc::LOG_ALERT as i64),
    ("LOG_CRIT", libc::LOG_CRIT as i64),
    ("LOG_ERR", libc::LOG_ERR as i64),
    ("LOG_WARNING", libc::LOG_WARNING as i64),
    ("LOG_NOTICE", libc::LOG_NOTICE as i64),
    ("LOG_INFO", libc::LOG_INFO as i64),
    ("LOG_DEBUG", libc::LOG_DEBUG as i64),
    ("LOG_KERN", libc::LOG_KERN as i64),
    ("LOG_USER", libc::LOG_USER as i64),
    ("LOG_MAIL", libc::LOG_MAIL as i64),
    ("LOG_DAEMON", libc::LOG_DAEMON as i64),
    ("LOG_AUTH", libc::LOG_AUTH as i64),
    ("LOG_SYSLOG", libc::LOG_SYSLOG as i64),
    ("LOG_LPR", libc::LOG_LPR as i64),
    ("LOG_NEWS", libc::LOG_NEWS as i64),
    ("LOG_UUCP", libc::LOG_UUCP as i64),
    ("LOG_CRON", libc::LOG_CRON as i64),
    ("LOG_AUTHPRIV", libc::LOG_AUTHPRIV as i64),
    ("LOG_LOCAL0", libc::LOG_LOCAL0 as i64),
    ("LOG_LOCAL1", libc::LOG_LOCAL1 as i64),
    ("LOG_LOCAL2", libc::LOG_LOCAL2 as i64),
    ("LOG_LOCAL3", libc::LOG_LOCAL3 as i64),
    ("LOG_LOCAL4", libc::LOG_LOCAL4 as i64),
    ("LOG_LOCAL5", libc::LOG_LOCAL5 as i64),
    ("LOG_LOCAL6", libc::LOG_LOCAL6 as i64),
    ("LOG_LOCAL7", libc::LOG_LOCAL7 as i64),
    ("LOG_PID", libc::LOG_PID as i64),
    ("LOG_CONS", libc::LOG_CONS as i64),
    ("LOG_ODELAY", libc::LOG_ODELAY as i64),
    ("LOG_NDELAY", libc::LOG_NDELAY as i64),
    ("LOG_NOWAIT", libc::LOG_NOWAIT as i64),
    ("LOG_PERROR", libc::LOG_PERROR as i64),
];

/// The `syslog.*` INI settings in effect (`PG(syslog_ident)`,
/// `PG(syslog_facility)`, `PG(syslog_filter)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyslogSettings {
    /// `syslog.ident` (default `php`).
    pub ident: Vec<u8>,
    /// `syslog.facility` (default `LOG_USER`).
    pub facility: i32,
    /// `syslog.filter` (default `no-ctrl`).
    pub filter: SyslogFilter,
}

impl Default for SyslogSettings {
    fn default() -> Self {
        SyslogSettings {
            ident: b"php".to_vec(),
            facility: libc::LOG_USER,
            filter: SyslogFilter::NoCtrl,
        }
    }
}

impl SyslogSettings {
    /// The settings from the raw INI values (`None` = unset); values the
    /// INI handlers would refuse leave the default, as at startup.
    #[must_use]
    pub fn from_ini(ident: Option<&[u8]>, facility: Option<&[u8]>, filter: Option<&[u8]>) -> Self {
        let d = Self::default();
        SyslogSettings {
            ident: ident.map_or(d.ident, <[u8]>::to_vec),
            facility: facility.and_then(facility_from_ini).unwrap_or(d.facility),
            filter: filter.and_then(SyslogFilter::from_ini).unwrap_or(d.filter),
        }
    }
}

/// `php_syslog_str()`'s filtering: the lines sent to `syslog(3)`.
#[must_use]
pub fn syslog_lines(message: &[u8], filter: SyslogFilter) -> Vec<Vec<u8>> {
    if filter == SyslogFilter::Raw {
        return vec![message.to_vec()];
    }
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for &c in message {
        if (0x20..=0x7e).contains(&c) || (c >= 0x80 && filter != SyslogFilter::Ascii) {
            buf.push(c);
        } else if c == b'\n' {
            out.push(std::mem::take(&mut buf));
        } else if c < 0x20 && filter == SyslogFilter::All {
            buf.push(c);
        } else {
            const X: &[u8; 16] = b"0123456789abcdef";
            buf.extend_from_slice(&[b'\\', b'x', X[usize::from(c >> 4)], X[usize::from(c & 15)]]);
        }
    }
    out.push(buf);
    out
}

thread_local! {
    /// `PG(have_called_openlog)`.
    static HAVE_CALLED_OPENLOG: Cell<bool> = const { Cell::new(false) };
}

/// Cuts at the first NUL, as a C string argument would.
fn cstring(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// `openlog(3)`; the identity string is kept alive while libc uses it.
fn sys_openlog(ident: &[u8], option: i32, facility: i32) {
    static IDENT: std::sync::Mutex<Option<CString>> = std::sync::Mutex::new(None);
    let c = cstring(ident);
    let mut guard = IDENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: the C string is stored in a static before and while syslog
    // uses it; the previous one is replaced only after openlog switched.
    unsafe { libc::openlog(c.as_ptr(), option, facility) };
    *guard = Some(c);
}

/// `syslog(priority, "%s", message)` (cut at the first NUL).
fn sys_syslog(priority: i32, message: &[u8]) {
    let m = cstring(message);
    // SAFETY: "%s" with a valid C string argument.
    unsafe { libc::syslog(priority, c"%s".as_ptr(), m.as_ptr()) };
}

/// `php_openlog()`: `openlog(3)` and remember that the script did it.
pub fn php_openlog(ident: &[u8], option: i32, facility: i32) {
    sys_openlog(ident, option, facility);
    HAVE_CALLED_OPENLOG.with(|c| c.set(true));
}

/// `php_closelog()`.
pub fn php_closelog() {
    // SAFETY: no preconditions.
    unsafe { libc::closelog() };
    HAVE_CALLED_OPENLOG.with(|c| c.set(false));
}

/// `php_syslog(priority, "%s", message)`: opens the log with the INI
/// identity and facility unless the script opened it, then sends the
/// lines `syslog.filter` makes of `message`.
pub fn php_syslog(priority: i32, message: &[u8], settings: &SyslogSettings) {
    if !HAVE_CALLED_OPENLOG.with(Cell::get) {
        php_openlog(&settings.ident, 0, settings.facility);
    }
    for line in syslog_lines(message, settings.filter) {
        sys_syslog(priority, &line);
    }
}

/// ext/standard's `RSHUTDOWN`: closes a log the request opened.
pub fn request_shutdown() {
    if HAVE_CALLED_OPENLOG.with(Cell::get) {
        php_closelog();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(lines: &[Vec<u8>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect()
    }

    #[test]
    fn filtering() {
        // ext/standard/tests/network/syslog_new_line.phpt, syslog_null_byte.phpt
        let lines = |m: &[u8], f| show(&syslog_lines(m, f));
        assert_eq!(
            lines(b"Hello\nWorld", SyslogFilter::NoCtrl),
            ["Hello", "World"]
        );
        assert_eq!(
            lines(b"a\0b\x01\xc3\xa9", SyslogFilter::NoCtrl),
            ["a\\x00b\\x01\u{e9}"]
        );
        assert_eq!(lines(b"a\x01\xc3", SyslogFilter::Ascii), ["a\\x01\\xc3"]);
        assert_eq!(lines(b"a\x01\n", SyslogFilter::All), ["a\x01", ""]);
        assert_eq!(lines(b"a\nb", SyslogFilter::Raw), ["a\nb"]);
    }

    #[test]
    fn ini_values_are_case_sensitive_and_accept_short_names() {
        assert_eq!(facility_from_ini(b"LOG_LOCAL3"), Some(libc::LOG_LOCAL3));
        assert_eq!(facility_from_ini(b"local3"), Some(libc::LOG_LOCAL3));
        assert_eq!(facility_from_ini(b"user"), Some(libc::LOG_USER));
        assert_eq!(facility_from_ini(b"security"), Some(libc::LOG_AUTH));
        assert_eq!(facility_from_ini(b"log_user"), None);
        assert_eq!(SyslogFilter::from_ini(b"raw"), Some(SyslogFilter::Raw));
        assert_eq!(SyslogFilter::from_ini(b"RAW"), None);
        let s = SyslogSettings::from_ini(None, Some(b"bogus"), Some(b"ascii"));
        assert_eq!(s.facility, libc::LOG_USER);
        assert_eq!(s.filter, SyslogFilter::Ascii);
        assert_eq!(s.ident, b"php");
    }

    #[test]
    fn open_state_follows_openlog_and_request_shutdown() {
        php_syslog(libc::LOG_DEBUG, b"phprust test", &SyslogSettings::default());
        assert!(HAVE_CALLED_OPENLOG.with(Cell::get));
        request_shutdown();
        assert!(!HAVE_CALLED_OPENLOG.with(Cell::get));
    }
}
