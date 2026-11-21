// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`LogEvent`]: one thing to log.

use std::borrow::Cow;

/// Which log an event is for; each configured log output takes the kinds
/// it is set up for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogKind {
    /// PHP's error log (`error_log`, the SAPI logger, syslog).
    Error,
    /// One line per finished request (FPM `access.log`).
    Access,
    /// A request that ran too long (FPM `slowlog`).
    Slow,
    /// A log of one's own, by name.
    Custom(Cow<'static, str>),
}

impl LogKind {
    /// The name `%{kind}L` prints.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            LogKind::Error => "error",
            LogKind::Access => "access",
            LogKind::Slow => "slow",
            LogKind::Custom(name) => name,
        }
    }
}

/// Severity, as syslog priorities (`php_log_err_with_severity()` passes
/// them): emergency 0 … debug 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// `LOG_EMERG`.
    Emergency,
    /// `LOG_ALERT`.
    Alert,
    /// `LOG_CRIT`.
    Critical,
    /// `LOG_ERR`.
    Error,
    /// `LOG_WARNING`.
    Warning,
    /// `LOG_NOTICE`.
    Notice,
    /// `LOG_INFO`.
    Info,
    /// `LOG_DEBUG`.
    Debug,
}

impl Level {
    /// The level of a syslog priority (`None` outside 0..=7, e.g. the -1
    /// that `error_log($message, 4)` passes).
    #[must_use]
    pub fn from_syslog(priority: i32) -> Option<Self> {
        Some(match priority {
            0 => Level::Emergency,
            1 => Level::Alert,
            2 => Level::Critical,
            3 => Level::Error,
            4 => Level::Warning,
            5 => Level::Notice,
            6 => Level::Info,
            7 => Level::Debug,
            _ => return None,
        })
    }

    /// The name `%{level}L` prints.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Level::Emergency => "emergency",
            Level::Alert => "alert",
            Level::Critical => "critical",
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Notice => "notice",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

/// A value attached to an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr<'a> {
    /// Bytes (PHP strings need not be UTF-8).
    Bytes(&'a [u8]),
    /// An integer.
    Int(i64),
}

/// One thing to log. It borrows what it carries, so building one costs
/// nothing; a [`Format`](crate::Format) renders it at once.
#[derive(Debug, Clone)]
pub struct LogEvent<'a> {
    /// The log it is for.
    pub kind: LogKind,
    /// Its severity, if it has one (access lines do not).
    pub level: Option<Level>,
    /// When it happened (`%T`), in Unix seconds (`time(NULL)`).
    pub time: i64,
    /// Its text (`%{message}L`).
    pub message: Option<&'a [u8]>,
    /// Further values by name (`%{type}L`, `%{file}L`, `%{line}L`).
    pub attrs: &'a [(&'static str, Attr<'a>)],
}

impl<'a> LogEvent<'a> {
    /// An event of `kind` with `message`, happening now.
    #[must_use]
    pub fn new(kind: LogKind, message: Option<&'a [u8]>) -> Self {
        Self {
            kind,
            level: None,
            time: php_support::time::now(),
            message,
            attrs: &[],
        }
    }

    /// The attribute `name`.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&Attr<'a>> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
}
