// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`RequestFields`]: what a SAPI knows about the request being served.

use std::borrow::Cow;
use std::time::Duration;

/// Which CPU time `%C` reports (`%{user}C`, `%{system}C`, `%{total}C`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuPart {
    /// User time.
    User,
    /// System time.
    System,
    /// User plus system time (the default).
    Total,
}

/// The request a log line belongs to, as the SAPI serving it sees it.
/// Every method has a default meaning "not known", which the format
/// prints as FPM does (`-`, or nothing for `%q`/`%r`/`%u`); a SAPI
/// implements what it has.
pub trait RequestFields {
    /// `%C`: the CPU the request used, in percent of its duration.
    fn cpu_percent(&self, _part: CpuPart) -> Option<f64> {
        None
    }

    /// `%d`: how long the request has run (or ran).
    fn duration(&self) -> Option<Duration> {
        None
    }

    /// `%{NAME}e`: a variable of the request as the SAPI received it (FPM:
    /// the `FastCGI` parameters, what `$_SERVER` starts from): server
    /// specifics such as `REQUEST_METHOD` or `SERVER_PROTOCOL`, and the
    /// request headers as `HTTP_HOST`, `HTTP_USER_AGENT`, ….
    fn env(&self, _name: &[u8]) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// `%f`: the script.
    fn script_filename(&self) -> &[u8] {
        b""
    }

    /// `%l`: the request's content length.
    fn content_length(&self) -> u64 {
        0
    }

    /// `%m`: the request method.
    fn method(&self) -> &[u8] {
        b""
    }

    /// `%M`: the peak of memory PHP allocated for the request.
    fn memory_peak(&self) -> u64 {
        0
    }

    /// `%n`: the pool name.
    fn pool(&self) -> &[u8] {
        b""
    }

    /// `%{Name}o`: a response header as sent; `name` matches the header's
    /// name case-sensitively (FPM compares the stored header line).
    fn response_header(&self, _name: &[u8]) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// `%{Name}i`: a request header.
    fn request_header(&self, _name: &[u8]) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// `%q`: the query string.
    fn query_string(&self) -> &[u8] {
        b""
    }

    /// `%r`: the request URI without the query string.
    fn request_uri(&self) -> &[u8] {
        b""
    }

    /// `%R`: the address of the connection's peer (FPM: the web server
    /// speaking `FastCGI` to it, not `REMOTE_ADDR`).
    fn remote_addr(&self) -> Option<&[u8]> {
        None
    }

    /// `%s`: the response status.
    fn status(&self) -> i32 {
        0
    }

    /// `%t`: when the request was received, in Unix seconds.
    fn started(&self) -> Option<i64> {
        None
    }

    /// `%u`: the basic-auth user.
    fn auth_user(&self) -> &[u8] {
        b""
    }

    /// `%{request_id}L`.
    fn request_id(&self) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// `%{trace_id}L`: the W3C trace id (32 hex digits).
    fn trace_id(&self) -> Option<Cow<'_, [u8]>> {
        None
    }

    /// `%{span_id}L`: the W3C span id (16 hex digits).
    fn span_id(&self) -> Option<Cow<'_, [u8]>> {
        None
    }
}

/// No request (process start, background threads): every request field
/// is unknown.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoRequest;

impl RequestFields for NoRequest {}
