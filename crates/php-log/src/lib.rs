// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's logging core, in one place for the engine, ext/standard and
//! every SAPI.
//!
//! [`php_log_err_with_severity`] (`main/main.c`) decides where a message
//! goes, from the `error_log` setting:
//!
//! * `syslog` → [`syslog::php_syslog`] (`syslog.ident`, `syslog.facility`,
//!   `syslog.filter`);
//! * a file name → `"[d-M-Y H:i:s e] message\n"` appended to that file,
//!   created with `error_log_mode`;
//!
//! Every line is a [`LogEvent`] rendered through a [`Format`]
//! (`php-observability`, FPM's `access.format` language): the file with
//! [`constants::PHP_ERROR_LOG_FMT`], the SAPI logger with
//! [`constants::PHP_SAPI_LOG_FMT`], which give php-src's lines byte for
//! byte.
//! * unset, or a file that cannot be opened → the SAPI's own logger
//!   (`sapi_module.log_message`): stderr in the CLI, the error log in
//!   Apache, the console of the built-in server, …
//!
//! `error_log` is unset by default, so messages go to the SAPI logger
//! unless the configuration says otherwise. The caller passes `error_log`
//! and supplies the rest through [`LogHost`]: the settings only one
//! destination needs (asked for only then, as php-src reads them), the
//! date in the request's timezone and the SAPI logger.

mod configured;
pub mod syslog;

use std::borrow::Cow;
use std::cell::Cell;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

use php_observability::{constants, Format, Level, LogEvent, LogKind, RequestFields};

pub use configured::ConfiguredLog;
pub use syslog::{SyslogFilter, SyslogSettings};

/// Where `error_log` sends a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination<'a> {
    /// `error_log = syslog`.
    Syslog,
    /// `error_log = <file>`.
    File(&'a [u8]),
    /// `error_log` unset (or empty): the SAPI logger.
    Sapi,
}

impl<'a> Destination<'a> {
    /// The destination an `error_log` value names.
    #[must_use]
    pub fn of(error_log: Option<&'a [u8]>) -> Self {
        match error_log {
            Some(b"syslog") => Destination::Syslog,
            Some(path) if !path.is_empty() => Destination::File(path),
            _ => Destination::Sapi,
        }
    }
}

/// The mode a new log file is created with: `error_log_mode`, or 0644
/// for values outside 1..=0o777.
#[must_use]
pub fn file_mode(error_log_mode: i64) -> u32 {
    if (1..=0o777).contains(&error_log_mode) {
        u32::try_from(error_log_mode).unwrap_or(0o644)
    } else {
        0o644
    }
}

/// What the caller of [`php_log_err_with_severity`] provides.
pub trait LogHost {
    /// `php_format_date("d-M-Y H:i:s e", time(), localtime)`: now as a
    /// log date ([`log_date`]).
    fn log_date(&mut self) -> String {
        log_date(php_support::time::now())
    }

    /// The request being served, for the request fields of log formats
    /// (`None`: no request, the fields print as `-`).
    fn request_fields(&self) -> Option<&dyn RequestFields> {
        None
    }

    /// `error_log_mode` (for a log file).
    fn error_log_mode(&mut self) -> i64 {
        0o644
    }

    /// `syslog.*` (for `error_log = syslog`).
    fn syslog_settings(&mut self) -> SyslogSettings {
        SyslogSettings::default()
    }

    /// `sapi_module.log_message(message, syslog_type_int)`.
    fn sapi_log_message(&mut self, message: &[u8], severity: i32);
}

thread_local! {
    /// `PG(in_error_log)`.
    static IN_ERROR_LOG: Cell<bool> = const { Cell::new(false) };
}

/// A named format, parsed once.
fn named(cell: &'static OnceLock<Format>, src: &'static str) -> &'static Format {
    cell.get_or_init(|| Format::parse(src.as_bytes(), "error_log").expect("built-in format"))
}

/// `ts` as a log date (`d-M-Y H:i:s e`, [`constants::PHP_LOG_DATE_FMT`])
/// in the thread's default timezone (UTC until ext/date sets one).
#[must_use]
pub fn log_date(ts: i64) -> String {
    static FORMAT: OnceLock<Format> = OnceLock::new();
    let mut event = LogEvent::new(LogKind::Error, None);
    event.time = ts;
    let line = named(&FORMAT, constants::PHP_LOG_DATE_FMT)
        .render(&event, None, None, None)
        .line;
    String::from_utf8_lossy(&line).into_owned()
}

/// Renders `event` with `format` for `host`'s request.
fn render(format: &Format, event: &LogEvent<'_>, host: &mut dyn LogHost) -> Vec<u8> {
    format.render(event, host.request_fields(), None, None).line
}

/// `php_log_err_with_severity(log_message, syslog_type_int)`: logs
/// `message` (cut at a NUL, as the C string is) to `error_log` (`None` =
/// unset) or, failing that, to the SAPI logger. A message logged while
/// another one is being logged on this thread is dropped.
pub fn php_log_err_with_severity(
    message: &[u8],
    severity: i32,
    error_log: Option<&[u8]>,
    host: &mut dyn LogHost,
) {
    if IN_ERROR_LOG.with(|c| c.replace(true)) {
        return;
    }
    let message = message.split(|&c| c == 0).next().unwrap_or_default();
    let mut event = LogEvent::new(LogKind::Error, Some(message));
    event.level = Level::from_syslog(severity);
    match Destination::of(error_log) {
        Destination::Syslog => syslog::php_syslog(severity, message, &host.syslog_settings()),
        Destination::File(path) => {
            let mode = file_mode(host.error_log_mode());
            if !append_to_file(path, mode, &event, host) {
                sapi_log(&event, severity, host);
            }
        }
        Destination::Sapi => sapi_log(&event, severity, host),
    }
    IN_ERROR_LOG.with(|c| c.set(false));
}

/// Hands `event`, rendered with [`constants::PHP_SAPI_LOG_FMT`], to the
/// SAPI logger.
fn sapi_log(event: &LogEvent<'_>, severity: i32, host: &mut dyn LogHost) {
    static FORMAT: OnceLock<Format> = OnceLock::new();
    let line = render(named(&FORMAT, constants::PHP_SAPI_LOG_FMT), event, host);
    host.sapi_log_message(&line, severity);
}

/// Appends `event`, rendered with [`constants::PHP_ERROR_LOG_FMT`]
/// (`"[date] message"`), and a newline to `path`; `false` if it cannot be
/// opened.
fn append_to_file(path: &[u8], mode: u32, event: &LogEvent<'_>, host: &mut dyn LogHost) -> bool {
    static FORMAT: OnceLock<Format> = OnceLock::new();
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(mode)
        .open(std::ffi::OsStr::from_bytes(path))
    else {
        return false;
    };
    let mut line = render(named(&FORMAT, constants::PHP_ERROR_LOG_FMT), event, host);
    line.push(b'\n');
    // One write, so that lines of concurrent writers stay whole (O_APPEND).
    let _ = f.write_all(&line);
    true
}

/// `php_escape_nul_for_log()`: NUL bytes of an error message as `\0`, so
/// that the log line is not cut there.
#[must_use]
pub fn escape_nul_for_log(message: &[u8]) -> Cow<'_, [u8]> {
    if !message.contains(&0) {
        return Cow::Borrowed(message);
    }
    let mut out = Vec::with_capacity(message.len() + 8);
    for &c in message {
        if c == 0 {
            out.extend_from_slice(b"\\0");
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// The syslog severity `php_error_cb` logs an error type with.
#[must_use]
pub fn severity_of_error_type(error_type: i64) -> i32 {
    // E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR | E_USER_ERROR,
    // E_RECOVERABLE_ERROR, E_PARSE
    const ERRORS: i64 = 1 | 16 | 64 | 256 | 4096 | 4;
    // E_WARNING | E_CORE_WARNING | E_COMPILE_WARNING | E_USER_WARNING
    const WARNINGS: i64 = 2 | 32 | 128 | 512;
    // E_DEPRECATED | E_USER_DEPRECATED
    const DEPRECATIONS: i64 = 8192 | 16384;
    if error_type & ERRORS != 0 {
        libc::LOG_ERR
    } else if error_type & WARNINGS != 0 {
        libc::LOG_WARNING
    } else if error_type & DEPRECATIONS != 0 {
        libc::LOG_INFO
    } else {
        libc::LOG_NOTICE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Host {
        sapi: Vec<(Vec<u8>, i32)>,
        mode: Option<i64>,
    }

    impl LogHost for Host {
        fn error_log_mode(&mut self) -> i64 {
            self.mode.unwrap_or(0o644)
        }

        fn sapi_log_message(&mut self, message: &[u8], severity: i32) {
            self.sapi.push((message.to_vec(), severity));
        }
    }

    /// A fresh path in the temporary directory (the file itself removed).
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = php_streams::temp::temp_dir();
        let path = php_streams::temp::tempnam(&dir, name.as_bytes()).expect("tempnam");
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn log_dates() {
        std::thread::spawn(|| {
            assert_eq!(log_date(0), "01-Jan-1970 00:00:00 UTC");
            assert_eq!(log_date(951_782_400), "29-Feb-2000 00:00:00 UTC");
            assert_eq!(log_date(1_793_728_800), "03-Nov-2026 18:00:00 UTC");
            assert_eq!(log_date(-1), "31-Dec-1969 23:59:59 UTC");
            php_date::tz::set_default_configured(php_date::tz::get("Europe/Vienna"));
            assert_eq!(
                log_date(1_793_728_800),
                "03-Nov-2026 19:00:00 Europe/Vienna"
            );
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn destinations() {
        assert_eq!(Destination::of(None), Destination::Sapi);
        assert_eq!(Destination::of(Some(b"")), Destination::Sapi);
        assert_eq!(Destination::of(Some(b"syslog")), Destination::Syslog);
        assert_eq!(
            Destination::of(Some(b"/x.log")),
            Destination::File(b"/x.log")
        );
    }

    #[test]
    fn unset_error_log_goes_to_the_sapi_logger() {
        let mut h = Host::default();
        php_log_err_with_severity(b"PHP Notice:  x\0y", libc::LOG_NOTICE, None, &mut h);
        assert_eq!(h.sapi, [(b"PHP Notice:  x".to_vec(), libc::LOG_NOTICE)]);
    }

    #[test]
    fn files_get_dated_lines_with_the_configured_mode() {
        use std::os::unix::fs::PermissionsExt;
        php_date::tz::set_default_configured(php_date::tz::get("Europe/Vienna"));
        let path = scratch("file.log");
        let error_log = Some(path.as_os_str().as_bytes());
        let mut h = Host {
            mode: Some(0o600),
            ..Host::default()
        };
        php_log_err_with_severity(b"one", libc::LOG_ERR, error_log, &mut h);
        php_log_err_with_severity(b"two", libc::LOG_ERR, error_log, &mut h);
        let text = std::fs::read_to_string(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        for (line, message) in lines.iter().zip(["one", "two"]) {
            // `[d-M-Y H:i:s e] message`
            let (date, rest) = line.split_once("] ").expect("date");
            assert_eq!(rest, message);
            assert_eq!(
                date.len(),
                "[21-Nov-2026 10:00:00 Europe/Vienna".len(),
                "{line}"
            );
            assert!(
                date.starts_with('[') && date.ends_with(" Europe/Vienna"),
                "{line}"
            );
        }
        assert_eq!(mode, 0o600);
        assert_eq!(h.sapi, Vec::<(Vec<u8>, i32)>::new());
    }

    #[test]
    fn unopenable_files_fall_back_to_the_sapi_logger() {
        let mut h = Host::default();
        php_log_err_with_severity(
            b"m",
            libc::LOG_WARNING,
            Some(b"/nonexistent-dir/php.log"),
            &mut h,
        );
        assert_eq!(h.sapi, [(b"m".to_vec(), libc::LOG_WARNING)]);
    }

    #[test]
    fn modes_outside_the_range_mean_0644() {
        assert_eq!(file_mode(0), 0o644);
        assert_eq!(file_mode(0o1777), 0o644);
        assert_eq!(file_mode(0o640), 0o640);
    }

    #[test]
    fn recursive_messages_are_dropped() {
        struct Reentrant(usize);
        impl LogHost for Reentrant {
            fn sapi_log_message(&mut self, _m: &[u8], _s: i32) {
                self.0 += 1;
                php_log_err_with_severity(b"inner", libc::LOG_ERR, None, self);
            }
        }
        let mut h = Reentrant(0);
        php_log_err_with_severity(b"outer", libc::LOG_ERR, None, &mut h);
        assert_eq!(h.0, 1);
    }

    #[test]
    fn nul_escaping_and_severities() {
        assert_eq!(&*escape_nul_for_log(b"a\0b"), b"a\\0b");
        assert!(matches!(escape_nul_for_log(b"ab"), Cow::Borrowed(_)));
        assert_eq!(severity_of_error_type(1), libc::LOG_ERR);
        assert_eq!(severity_of_error_type(4), libc::LOG_ERR);
        assert_eq!(severity_of_error_type(4096), libc::LOG_ERR);
        assert_eq!(severity_of_error_type(2), libc::LOG_WARNING);
        assert_eq!(severity_of_error_type(8), libc::LOG_NOTICE);
        assert_eq!(severity_of_error_type(8192), libc::LOG_INFO);
        assert_eq!(severity_of_error_type(0), libc::LOG_NOTICE);
    }
}
