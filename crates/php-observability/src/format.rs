// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`Format`]: FPM's `access.format` language (`fpm_log_write()` in
//! `sapi/fpm/fpm/fpm_log.c`), for every log.
//!
//! The FPM placeholders keep their letters, modifiers, defaults and error
//! messages:
//!
//! | | |
//! |---|---|
//! | `%%` | `%` |
//! | `%C` | CPU in percent of the duration; `%{user}C`, `%{system}C`, `%{total}C` (default) |
//! | `%d` | duration; `%{seconds}d` (default), `%{milli}d`/`%{milliseconds}d`, `%{micro}d`/`%{microseconds}d`; Buzz also `%{minutes}d`, `%{hours}d`, `%{days}d` and the units of `php_timer` (`us`, `ms`, `s`, `m`, `h`, `d`) |
//! | `%{NAME}e` | a request variable as received (`FastCGI` parameters): `%{SERVER_PROTOCOL}e`, headers as `%{HTTP_HOST}e`; else an environment variable of the process (FPM prints `-` there) |
//! | `%f` | script filename |
//! | `%l` | content length |
//! | `%m` | request method |
//! | `%M` | peak memory; `%{bytes}M` (default), `%{kilo}M`, `%{mega}M` |
//! | `%n` | pool name |
//! | `%{Name}o` | response header as sent (name case-sensitive, empty value `-`) |
//! | `%p` / `%P` | PID / parent PID |
//! | `%q` / `%Q` | query string / `?` if there is one |
//! | `%r` | request URI without the query string |
//! | `%R` | address of the connection's peer (behind a web server: the web server) |
//! | `%s` | status |
//! | `%t` / `%T` | request start / log time; `%{strftime}t`, default `%d/%b/%Y:%H:%M:%S %z` |
//! | `%u` | basic-auth user |
//!
//! Buzz adds, under letters FPM does not use, `%{Name}i` (a request
//! header by its own name, as in Apache: `%{User-Agent}i` is
//! `%{HTTP_USER_AGENT}e`) and `%{name}L` for what FPM's access log does not
//! know: `message`, `level`, `type`, `file`, `line`, `kind`, `tz` (the
//! zone's name, `e` of `date()`), `request_id`, `trace_id`, `span_id`.
//! Before parsing, `${NAME}` is replaced by the named format
//! ([`crate::constants`]).
//!
//! Times are rendered in the C locale (stable whatever `setlocale()` a
//! script chose) and in the thread's default timezone
//! ([`php_date::tz::default_zone`]: `date_default_timezone_set()`, else
//! `date.timezone`, else UTC).

use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use php_date::TzInfo;
use php_timer::Unit;

use crate::event::{Attr, LogEvent};
use crate::fields::{CpuPart, NoRequest, RequestFields};

/// FPM's default `%t`/`%T` format.
const DEFAULT_TIME: &[u8] = b"%d/%b/%Y:%H:%M:%S %z";

/// FPM's `format[129]`: longer modifiers are cut to this many bytes.
const MAX_MODIFIER: usize = 128;

/// The `%{name}L` fields.
const EXTENSIONS: &[&str] = &[
    "message",
    "level",
    "type",
    "file",
    "line",
    "kind",
    "tz",
    "request_id",
    "trace_id",
    "span_id",
];

/// Why a format cannot be used; the texts are FPM's (with the setting's
/// name where FPM names `access.format`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// `%x` with an unknown letter.
    InvalidToken {
        /// The setting the format came from.
        setting: String,
        /// The letter.
        letter: u8,
    },
    /// `%{…` without `}`, or `%{…}` at the very end.
    MissingClosingEmbrace {
        /// The setting the format came from.
        setting: String,
    },
    /// `%{…}x` for a letter that takes no modifier.
    EmbraceNotAllowed {
        /// The letter.
        letter: u8,
    },
    /// A modifier the letter does not accept.
    BadModifier {
        /// The letter.
        letter: u8,
        /// The modifier given.
        modifier: Vec<u8>,
        /// FPM's list of the accepted ones.
        allowed: &'static str,
    },
    /// `%e`, `%o` or `%i` without a name.
    MissingName {
        /// The letter.
        letter: u8,
    },
    /// `${NAME}` for a name no format has.
    UnknownName {
        /// The name.
        name: Vec<u8>,
    },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::InvalidToken { setting, letter } => {
                write!(
                    f,
                    "Invalid token in the {setting} (%{})",
                    char::from(*letter)
                )
            }
            FormatError::MissingClosingEmbrace { setting } => {
                write!(f, "missing closing embrace in the {setting}")
            }
            FormatError::EmbraceNotAllowed { letter } => {
                write!(
                    f,
                    "embrace is not allowed for modifier %{}",
                    char::from(*letter)
                )
            }
            FormatError::BadModifier {
                letter,
                modifier,
                allowed,
            } => write!(
                f,
                "only {allowed} are allowed as a modifier for %{} ('{}')",
                char::from(*letter),
                String::from_utf8_lossy(modifier)
            ),
            FormatError::MissingName { letter: b'e' } => f.write_str(
                "the name of the environment variable must be set between embraces for %e",
            ),
            FormatError::MissingName { letter } => write!(
                f,
                "the name of the header must be set between embraces for %{}",
                char::from(*letter)
            ),
            FormatError::UnknownName { name } => write!(
                f,
                "unknown log format ${{{}}}",
                String::from_utf8_lossy(name)
            ),
        }
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryUnit {
    Bytes,
    Kilo,
    Mega,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Cpu(CpuPart),
    Duration(Unit),
    Env(Vec<u8>),
    Script,
    ContentLength,
    Method,
    Memory(MemoryUnit),
    Pool,
    ResponseHeader(Vec<u8>),
    Pid,
    ParentPid,
    Query,
    QueryMark,
    Uri,
    Remote,
    Status,
    Started(Vec<u8>),
    Now(Vec<u8>),
    User,
    RequestHeader(Vec<u8>),
    Extension(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(Vec<u8>),
    Field(Field),
}

/// A parsed log format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    tokens: Vec<Token>,
}

/// A rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The line, without a line end.
    pub line: Vec<u8>,
    /// Whether it was cut at the limit (FPM then notes "the log buffer is
    /// over the configured limit").
    pub truncated: bool,
}

/// Replaces each `${NAME}` by the named format.
fn expand_names(src: &[u8]) -> Result<Vec<u8>, FormatError> {
    let mut out = Vec::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.windows(2).position(|w| w == b"${") {
        out.extend_from_slice(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.iter().position(|&b| b == b'}') else {
            out.extend_from_slice(&rest[at..]);
            return Ok(out);
        };
        let name = &after[..end];
        let format = crate::constants::lookup(name).ok_or_else(|| FormatError::UnknownName {
            name: name.to_vec(),
        })?;
        out.extend_from_slice(format.as_bytes());
        rest = &after[end + 1..];
    }
    out.extend_from_slice(rest);
    Ok(out)
}

/// `strcasecmp(modifier, any of names) == 0`.
fn is_one_of(modifier: &[u8], names: &[&str]) -> bool {
    names
        .iter()
        .any(|n| modifier.eq_ignore_ascii_case(n.as_bytes()))
}

/// The field of `letter` with `modifier` (empty = none); `Ok(None)` for
/// `%%`.
fn field(letter: u8, modifier: &[u8]) -> Result<Option<Field>, FormatError> {
    let bad = |allowed| FormatError::BadModifier {
        letter,
        modifier: modifier.to_vec(),
        allowed,
    };
    let named = |make: fn(Vec<u8>) -> Field| {
        if modifier.is_empty() {
            Err(FormatError::MissingName { letter })
        } else {
            Ok(make(modifier.to_vec()))
        }
    };
    let takes_modifier = matches!(
        letter,
        b'C' | b'd' | b'e' | b'M' | b'o' | b't' | b'T' | b'i' | b'L'
    );
    if !takes_modifier && !modifier.is_empty() {
        return Err(FormatError::EmbraceNotAllowed { letter });
    }
    Ok(Some(match letter {
        b'%' => return Ok(None),
        b'C' => Field::Cpu(if modifier.is_empty() || is_one_of(modifier, &["total"]) {
            CpuPart::Total
        } else if is_one_of(modifier, &["user"]) {
            CpuPart::User
        } else if is_one_of(modifier, &["system"]) {
            CpuPart::System
        } else {
            return Err(bad("'total', 'user' or 'system'"));
        }),
        b'd' => Field::Duration(if modifier.is_empty() || is_one_of(modifier, &["seconds"]) {
            Unit::Seconds
        } else if is_one_of(modifier, &["milliseconds", "milli", "miliseconds", "mili"]) {
            Unit::Millis
        } else if is_one_of(modifier, &["microseconds", "micro"]) {
            Unit::Micros
        } else if is_one_of(modifier, &["minutes"]) {
            Unit::Minutes
        } else if is_one_of(modifier, &["hours"]) {
            Unit::Hours
        } else if is_one_of(modifier, &["days"]) {
            Unit::Days
        } else if let Some(unit) = std::str::from_utf8(modifier).ok().and_then(Unit::from_suffix) {
            unit
        } else {
            return Err(bad(
                "'seconds', 'milli', 'milliseconds', 'micro' or 'microseconds'",
            ));
        }),
        b'e' => named(Field::Env)?,
        b'f' => Field::Script,
        b'l' => Field::ContentLength,
        b'm' => Field::Method,
        b'M' => Field::Memory(if modifier.is_empty() || is_one_of(modifier, &["bytes"]) {
            MemoryUnit::Bytes
        } else if is_one_of(modifier, &["kilobytes", "kilo"]) {
            MemoryUnit::Kilo
        } else if is_one_of(modifier, &["megabytes", "mega"]) {
            MemoryUnit::Mega
        } else {
            return Err(bad("'bytes', 'kilo', 'kilobytes', 'mega' or 'megabytes'"));
        }),
        b'n' => Field::Pool,
        b'o' => named(Field::ResponseHeader)?,
        b'i' => named(Field::RequestHeader)?,
        b'p' => Field::Pid,
        b'P' => Field::ParentPid,
        b'q' => Field::Query,
        b'Q' => Field::QueryMark,
        b'r' => Field::Uri,
        b'R' => Field::Remote,
        b's' => Field::Status,
        b't' => Field::Started(modifier.to_vec()),
        b'T' => Field::Now(modifier.to_vec()),
        b'u' => Field::User,
        b'L' => Field::Extension(
            EXTENSIONS
                .iter()
                .find(|n| n.as_bytes() == modifier)
                .copied()
                .ok_or_else(|| {
                    bad("'message', 'level', 'type', 'file', 'line', 'kind', 'tz', 'request_id', 'trace_id' or 'span_id'")
                })?,
        ),
        _ => unreachable!("checked by the caller"),
    }))
}

impl Format {
    /// Parses `src`, the value of `setting` (named in error messages as
    /// FPM names `access.format`). `${NAME}` is replaced first.
    ///
    /// # Errors
    ///
    /// What FPM refuses (its message as [`FormatError`]'s `Display`), and
    /// unknown `${NAME}`s.
    pub fn parse(src: &[u8], setting: &str) -> Result<Format, FormatError> {
        let src = expand_names(src)?;
        let mut tokens = Vec::new();
        let mut literal = Vec::new();
        let mut i = 0;
        while i < src.len() {
            let c = src[i];
            if c != b'%' {
                literal.push(c);
                i += 1;
                continue;
            }
            // `%`, then an optional `{modifier}`, then the letter.
            i += 1;
            let mut modifier: &[u8] = b"";
            if src.get(i) == Some(&b'{') {
                let start = i + 1;
                let Some(len) = src[start..].iter().position(|&b| b == b'}') else {
                    return Err(FormatError::MissingClosingEmbrace {
                        setting: setting.to_string(),
                    });
                };
                let end = start + len;
                if end + 1 >= src.len() {
                    return Err(FormatError::MissingClosingEmbrace {
                        setting: setting.to_string(),
                    });
                }
                modifier = &src[start..end.min(start + MAX_MODIFIER)];
                i = end + 1;
            }
            // A lone `%` at the end prints nothing, as in FPM.
            let Some(&letter) = src.get(i) else { break };
            i += 1;
            if !b"%CdefilmMnoPpqQrRstTuL".contains(&letter) {
                return Err(FormatError::InvalidToken {
                    setting: setting.to_string(),
                    letter,
                });
            }
            match field(letter, modifier)? {
                None => literal.push(b'%'),
                Some(f) => {
                    if !literal.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut literal)));
                    }
                    tokens.push(Token::Field(f));
                }
            }
        }
        if !literal.is_empty() {
            tokens.push(Token::Literal(literal));
        }
        Ok(Format { tokens })
    }

    /// Renders `event`, belonging to `request` (`None`: no request), with
    /// times in `zone` (`None`: the thread's default timezone), cut at
    /// `limit` bytes if given.
    #[must_use]
    pub fn render(
        &self,
        event: &LogEvent<'_>,
        request: Option<&dyn RequestFields>,
        zone: Option<&Arc<TzInfo>>,
        limit: Option<usize>,
    ) -> Rendered {
        let request = request.unwrap_or(&NoRequest);
        let default;
        let zone = match zone {
            Some(z) => z,
            None => {
                default = php_date::tz::default_zone();
                &default
            }
        };
        let mut line = Vec::new();
        for token in &self.tokens {
            match token {
                Token::Literal(text) => line.extend_from_slice(text),
                Token::Field(f) => render_field(f, event, request, zone, &mut line),
            }
            if let Some(limit) = limit {
                if line.len() > limit {
                    line.truncate(limit);
                    return Rendered {
                        line,
                        truncated: true,
                    };
                }
            }
        }
        Rendered {
            line,
            truncated: false,
        }
    }
}

/// `value`, or `-` when it is empty (FPM's `*x ? x : "-"`).
fn or_dash(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(if value.is_empty() { b"-" } else { value });
}

fn strftime(out: &mut Vec<u8>, format: &[u8], t: i64, zone: &Arc<TzInfo>) {
    let format = if format.is_empty() {
        DEFAULT_TIME
    } else {
        format
    };
    if let Some(s) = php_date::format::strftime(format, t, Some(zone)) {
        out.extend_from_slice(&s);
    }
}

#[allow(clippy::cast_precision_loss)] // durations and CPU shares far below 2^52
fn render_field(
    f: &Field,
    event: &LogEvent<'_>,
    request: &dyn RequestFields,
    zone: &Arc<TzInfo>,
    out: &mut Vec<u8>,
) {
    use std::io::Write;
    match f {
        Field::Cpu(part) => match request.cpu_percent(*part) {
            Some(p) => {
                let _ = write!(out, "{p:.2}");
            }
            None => out.push(b'-'),
        },
        Field::Duration(unit) => match request.duration() {
            // Microseconds whole (FPM's `%lu`), every other unit with three
            // decimals (FPM's `%.3f`).
            Some(d) if *unit == Unit::Micros => {
                let _ = write!(out, "{}", d.as_micros());
            }
            Some(d) => {
                let _ = write!(out, "{:.3}", d.as_nanos() as f64 / unit.nanos() as f64);
            }
            None => out.push(b'-'),
        },
        Field::Env(name) => {
            if let Some(v) = request.env(name) {
                out.extend_from_slice(&v);
            } else if let Some(v) = std::env::var_os(std::ffi::OsStr::from_bytes(name)) {
                out.extend_from_slice(v.as_bytes());
            } else {
                out.push(b'-');
            }
        }
        Field::Script => or_dash(out, request.script_filename()),
        Field::ContentLength => {
            let _ = write!(out, "{}", request.content_length());
        }
        Field::Method => or_dash(out, request.method()),
        Field::Memory(unit) => {
            let m = request.memory_peak();
            let _ = write!(
                out,
                "{}",
                match unit {
                    MemoryUnit::Bytes => m,
                    MemoryUnit::Kilo => m / 1024,
                    MemoryUnit::Mega => m / 1024 / 1024,
                }
            );
        }
        Field::Pool => or_dash(out, request.pool()),
        Field::ResponseHeader(name) => {
            or_dash(out, request.response_header(name).as_deref().unwrap_or(b""));
        }
        Field::RequestHeader(name) => {
            or_dash(out, request.request_header(name).as_deref().unwrap_or(b""));
        }
        Field::Pid => {
            let _ = write!(out, "{}", std::process::id());
        }
        Field::ParentPid => {
            let _ = write!(out, "{}", std::os::unix::process::parent_id());
        }
        Field::Query => out.extend_from_slice(request.query_string()),
        Field::QueryMark => {
            if !request.query_string().is_empty() {
                out.push(b'?');
            }
        }
        Field::Uri => out.extend_from_slice(request.request_uri()),
        Field::Remote => or_dash(out, request.remote_addr().unwrap_or(b"")),
        Field::Status => {
            let _ = write!(out, "{}", request.status());
        }
        Field::Started(format) => match request.started() {
            Some(t) => strftime(out, format, t, zone),
            None => out.push(b'-'),
        },
        Field::Now(format) => strftime(out, format, event.time, zone),
        Field::User => out.extend_from_slice(request.auth_user()),
        Field::Extension(name) => render_extension(name, event, request, zone, out),
    }
}

fn render_extension(
    name: &str,
    event: &LogEvent<'_>,
    request: &dyn RequestFields,
    zone: &Arc<TzInfo>,
    out: &mut Vec<u8>,
) {
    use std::io::Write;
    let bytes = |out: &mut Vec<u8>, v: Option<&[u8]>| match v {
        Some(v) => out.extend_from_slice(v),
        None => out.push(b'-'),
    };
    match name {
        "message" => bytes(out, event.message),
        "level" => bytes(out, event.level.map(|l| l.name().as_bytes())),
        "kind" => out.extend_from_slice(event.kind.name().as_bytes()),
        "tz" => out.extend_from_slice(zone.name.as_bytes()),
        "request_id" => bytes(out, request.request_id().as_deref()),
        "trace_id" => bytes(out, request.trace_id().as_deref()),
        "span_id" => bytes(out, request.span_id().as_deref()),
        attr => match event.attr(attr) {
            Some(Attr::Bytes(v)) => out.extend_from_slice(v),
            Some(Attr::Int(n)) => {
                let _ = write!(out, "{n}");
            }
            None => out.push(b'-'),
        },
    }
}

#[cfg(test)]
mod tests;
