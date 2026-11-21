// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The formats Buzz writes by default, by name. A format embeds one as
//! `${NAME}` (haproxy offers its `HAPROXY_HTTP_LOG_FMT` & co. the same
//! way): `"${FPM_ACCESS_LOG_FMT} %{trace_id}L"` is FPM's access line plus
//! the trace id.

/// The text of [`PHP_LOG_DATE_FMT`], for `concat!`.
macro_rules! log_date_fmt {
    () => {
        "%{%d-%b-%Y %H:%M:%S}T %{tz}L"
    };
}

/// PHP's log date, `php_format_date("d-M-Y H:i:s e")`: in the request's
/// timezone (`date.timezone`), UTC without one.
pub const PHP_LOG_DATE_FMT: &str = log_date_fmt!();

/// PHP's error log file (`php_log_err_with_severity()`):
/// `[d-M-Y H:i:s e] message`.
pub const PHP_ERROR_LOG_FMT: &str = concat!("[", log_date_fmt!(), "] %{message}L");

/// The SAPI logger (`sapi_module.log_message`) and syslog: the message
/// alone; the SAPI or syslog adds what it adds.
pub const PHP_SAPI_LOG_FMT: &str = "%{message}L";

/// FPM's default `access.format`.
pub const FPM_ACCESS_LOG_FMT: &str = "%R - %u %t \"%m %r\" %s";

/// The named formats, for `${NAME}`.
pub const ALL: &[(&str, &str)] = &[
    ("PHP_LOG_DATE_FMT", PHP_LOG_DATE_FMT),
    ("PHP_ERROR_LOG_FMT", PHP_ERROR_LOG_FMT),
    ("PHP_SAPI_LOG_FMT", PHP_SAPI_LOG_FMT),
    ("FPM_ACCESS_LOG_FMT", FPM_ACCESS_LOG_FMT),
];

/// The format named `name`.
#[must_use]
pub fn lookup(name: &[u8]) -> Option<&'static str> {
    ALL.iter()
        .find(|(n, _)| n.as_bytes() == name)
        .map(|(_, f)| *f)
}
