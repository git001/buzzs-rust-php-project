// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What the engine and the SAPIs observe, and how it is written down.
//!
//! Every log line — PHP's error log, the access log, the slow log, a log
//! of one's own — is a [`LogEvent`] rendered through a [`Format`]: FPM's
//! `access.format` language (`sapi/fpm/fpm/fpm_log.c`, documented in
//! `sapi/fpm/www.conf.in`) for all of them, so that an error log can carry
//! the request's method, URI or duration as an access log can. php-src
//! formats only its access log this way.
//!
//! A format is parsed once ([`Format::parse`]); rendering it reads the
//! event, the request it belongs to ([`RequestFields`], supplied by the
//! SAPI) and the process. As in FPM, the line is rendered at once, with
//! the values of that moment, on the thread that logs.
//!
//! The formats Buzz writes by default are named constants
//! ([`constants`]) that a format can embed as `${NAME}`, as haproxy's
//! `HAPROXY_HTTP_LOG_FMT` can be embedded in its `log-format`.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod constants;
mod event;
mod fields;
mod format;

pub use event::{Attr, Level, LogEvent, LogKind};
pub use fields::{CpuPart, RequestFields};
pub use format::{Format, FormatError, Rendered};
pub use php_date::TzInfo;
