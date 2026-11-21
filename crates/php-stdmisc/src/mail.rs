// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mail()` (`ext/standard/mail.c`): additional-header building and
//! validation (array form), `To`/`Subject` sanitising, `mail.log`,
//! `mail.add_x_header`, `mail.cr_lf_mode`, and the `sendmail_path`
//! invocation through `/bin/sh -c`; plus `error_log()`
//! (`ext/standard/basic_functions.c`), which logs through
//! [`php_log::php_log_err_with_severity`].

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use php_runtime::{
    ArrayKey, ErrorLevel, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};

use crate::sys;

fn header_check_value(v: &[u8]) -> Option<&'static str> {
    let mut i = 0;
    while i < v.len() {
        match v[i] {
            b'\r' => {
                if v.get(i + 1) != Some(&b'\n') {
                    return Some("contains CR character that is not allowed in the header");
                }
                if v.len() - i >= 3 && matches!(v[i + 2], b' ' | b'\t') {
                    i += 3;
                    continue;
                }
                return Some(
                    "contains CRLF characters that are used as a line separator and are not allowed in the header",
                );
            }
            b'\n' => {
                if v.len() - i >= 2 && matches!(v[i + 1], b' ' | b'\t') {
                    i += 2;
                    continue;
                }
                return Some("contains LF character that is not allowed in the header");
            }
            0 => return Some("contains NULL character that is not allowed in the header"),
            _ => i += 1,
        }
    }
    None
}

fn header_elem(out: &mut Vec<u8>, key: &[u8], val: &Value) -> RtResult<()> {
    let k = String::from_utf8_lossy(key);
    match val {
        Value::Str(s) => {
            if key.iter().any(|&c| !(33..=126).contains(&c) || c == b':') {
                return Err(RuntimeError::ValueError(format!(
                    "Header name \"{k}\" contains invalid characters"
                )));
            }
            if let Some(msg) = header_check_value(s.as_bytes()) {
                return Err(RuntimeError::ValueError(format!("Header \"{k}\" {msg}")));
            }
            out.extend_from_slice(key);
            out.extend_from_slice(b": ");
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\r\n");
            Ok(())
        }
        Value::Array(a) => header_elems(out, key, a),
        other => Err(RuntimeError::TypeError(format!(
            "Header \"{k}\" must be of type array|string, {} given",
            other.value_name()
        ))),
    }
}

fn header_elems(out: &mut Vec<u8>, key: &[u8], arr: &ZArray) -> RtResult<()> {
    let k = String::from_utf8_lossy(key);
    for (ik, v) in arr.iter() {
        if let ArrayKey::Str(s) = ik {
            return Err(RuntimeError::TypeError(format!(
                "Header \"{k}\" must only contain numeric keys, \"{s}\" found"
            )));
        }
        let v = v.deref();
        if !matches!(v, Value::Str(_)) {
            return Err(RuntimeError::TypeError(format!(
                "Header \"{k}\" must only contain values of type string, {} found",
                v.value_name()
            )));
        }
        header_elem(out, key, &v)?;
    }
    Ok(())
}

/// `php_mail_build_headers()`: the `$additional_headers` array form.
pub fn build_headers(headers: &ZArray) -> RtResult<ZStr> {
    const SINGLE: &[&str] = &[
        "orig-date",
        "from",
        "sender",
        "reply-to",
        "cc",
        "bcc",
        "message-id",
        "references",
        "in-reply-to",
    ];
    let mut out = Vec::new();
    for (k, v) in headers.iter() {
        let key = match k {
            ArrayKey::Int(i) => {
                return Err(RuntimeError::TypeError(format!(
                    "Header name cannot be numeric, {i} given"
                )))
            }
            ArrayKey::Str(s) => s.as_bytes(),
        };
        let v = v.deref();
        let lower = key.to_ascii_lowercase();
        if lower == b"to" {
            return Err(RuntimeError::ValueError(
                "The additional headers cannot contain the \"To\" header".into(),
            ));
        }
        if lower == b"subject" {
            return Err(RuntimeError::ValueError(
                "The additional headers cannot contain the \"Subject\" header".into(),
            ));
        }
        if let Some(target) = SINGLE.iter().find(|t| t.as_bytes() == lower.as_slice()) {
            match &v {
                Value::Str(_) => header_elem(&mut out, key, &v)?,
                Value::Array(a) => {
                    if lower == target.as_bytes() {
                        return Err(RuntimeError::TypeError(format!(
                            "Header \"{target}\" must be of type string, array given"
                        )));
                    }
                    header_elems(&mut out, key, a)?;
                }
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "Header \"{}\" must be of type array|string, {} given",
                        String::from_utf8_lossy(key),
                        other.value_name()
                    )))
                }
            }
        } else {
            header_elem(&mut out, key, &v)?;
        }
    }
    if out.len() >= 2 {
        out.truncate(out.len() - 2);
    }
    Ok(ZStr::new(out))
}

/// `php_mail_detect_multiple_crlf()`.
#[must_use]
pub fn detect_multiple_crlf(hdr: &[u8]) -> bool {
    let hdr = hdr.split(|&c| c == 0).next().unwrap_or_default();
    if hdr.is_empty() {
        return false;
    }
    if !(33..=126).contains(&hdr[0]) || hdr[0] == b':' {
        return true;
    }
    let at = |i: usize| hdr.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < hdr.len() {
        match hdr[i] {
            b'\r' => {
                if at(i + 1) == 0
                    || at(i + 1) == b'\r'
                    || (at(i + 1) == b'\n' && matches!(at(i + 2), 0 | b'\n' | b'\r'))
                {
                    return true;
                }
                i += 2;
            }
            b'\n' => {
                if matches!(at(i + 1), 0 | b'\r' | b'\n') {
                    return true;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    false
}

/// Sanitises `$to`/`$subject`: trailing whitespace removed, control
/// characters replaced by spaces except CRLF + WSP folding.
#[must_use]
pub fn sanitize_header_line(s: &[u8]) -> Vec<u8> {
    let mut v = s.to_vec();
    while v
        .last()
        .is_some_and(|&c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        v.pop();
    }
    // The C loop runs over a C string.
    if let Some(n) = v.iter().position(|&c| c == 0) {
        v.truncate(n);
    }
    let mut i = 0;
    while i < v.len() {
        if v[i] < 0x20 || v[i] == 0x7f {
            if v[i] == b'\r'
                && v.get(i + 1) == Some(&b'\n')
                && matches!(v.get(i + 2), Some(b' ' | b'\t'))
            {
                i += 2;
                while matches!(v.get(i + 1), Some(b' ' | b'\t')) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            v[i] = b' ';
        }
        i += 1;
    }
    v
}

/// The `mail.*` / `sendmail_path` INI settings.
#[derive(Debug, Clone, Default)]
pub struct MailSettings {
    /// `sendmail_path` (empty = mail unavailable).
    pub sendmail_path: Vec<u8>,
    /// `mail.force_extra_parameters`.
    pub force_extra_parameters: Vec<u8>,
    /// `mail.add_x_header`.
    pub add_x_header: bool,
    /// `mail.log` (a file name or `syslog`; empty = off).
    pub log: Vec<u8>,
    /// `mail.cr_lf_mode` (`crlf`, `lf`, `mixed`, `os`).
    pub cr_lf_mode: Vec<u8>,
    /// `mail.mixed_lf_and_crlf`.
    pub mixed_lf_and_crlf: bool,
    /// `syslog.*`, for `mail.log = syslog`.
    pub syslog: php_log::SyslogSettings,
}

/// What `mail()` needs to know about the running script.
#[derive(Debug, Clone, Default)]
pub struct ScriptInfo {
    /// `zend_get_executed_filename()`.
    pub filename: Vec<u8>,
    /// `zend_get_executed_lineno()`.
    pub lineno: u32,
    /// `php_getuid()` (script owner).
    pub uid: i64,
}

/// The `$additional_headers` argument.
#[derive(Debug, Clone, Copy)]
pub enum HeadersArg<'a> {
    /// Not passed / `null`.
    None,
    /// A string.
    Str(&'a [u8]),
    /// An array.
    Array(&'a ZArray),
}

/// `mail($to, $subject, $message, $additional_headers, $additional_params)`.
/// `log_date` formats the current time for `mail.log` files, which are
/// appended to with [`append_log_file`].
#[allow(clippy::too_many_arguments)]
pub fn mail<C: RuntimeContext + ?Sized>(
    to: &[u8],
    subject: &[u8],
    message: &[u8],
    headers: HeadersArg<'_>,
    extra_params: Option<&[u8]>,
    settings: &MailSettings,
    script: &ScriptInfo,
    log_date: &dyn Fn(i64) -> String,
    ctx: &mut C,
) -> RtResult<bool> {
    mail_ex(
        to,
        subject,
        message,
        headers,
        extra_params,
        settings,
        script,
        log_date,
        ctx,
        &mut append_log_file::<C>,
    )
}

/// [`mail`] with the `mail.log` file writer supplied by the caller:
/// `log_file(ctx, filename, line)` is `php_mail_log_to_file()`. An error
/// it returns (an exception thrown by an error handler for the open
/// warning) aborts the mail before the delivery program runs (GH-7875).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn mail_ex<C: RuntimeContext + ?Sized>(
    to: &[u8],
    subject: &[u8],
    message: &[u8],
    headers: HeadersArg<'_>,
    extra_params: Option<&[u8]>,
    settings: &MailSettings,
    script: &ScriptInfo,
    log_date: &dyn Fn(i64) -> String,
    ctx: &mut C,
    log_file: &mut dyn FnMut(&mut C, &[u8], &[u8]) -> RtResult<()>,
) -> RtResult<bool> {
    for (n, name, v) in [
        (1, "to", to),
        (2, "subject", subject),
        (3, "message", message),
    ] {
        if v.contains(&0) {
            return Err(RuntimeError::ValueError(format!(
                "mail(): Argument #{n} (${name}) must not contain any null bytes"
            )));
        }
    }
    if extra_params.is_some_and(|p| p.contains(&0)) {
        return Err(RuntimeError::ValueError(
            "mail(): Argument #5 ($additional_params) must not contain any null bytes".into(),
        ));
    }
    let headers_str: Option<Vec<u8>> = match headers {
        HeadersArg::None => None,
        HeadersArg::Str(h) => {
            if h.contains(&0) {
                return Err(RuntimeError::ValueError(
                    "mail(): Argument #4 ($additional_headers) must not contain any null bytes"
                        .into(),
                ));
            }
            let end = h
                .iter()
                .rposition(|&c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b))
                .map_or(0, |i| i + 1);
            Some(h[..end].to_vec())
        }
        HeadersArg::Array(a) => Some(build_headers(a)?.as_bytes().to_vec()),
    };
    let to_r = sanitize_header_line(to);
    let subject_r = sanitize_header_line(subject);
    let extra_cmd = if settings.force_extra_parameters.is_empty() {
        match extra_params {
            Some(p) => Some(crate::exec::escapeshellcmd(p)?),
            None => None,
        }
    } else {
        Some(crate::exec::escapeshellcmd(
            &settings.force_extra_parameters,
        )?)
    };

    let headers_opt = headers_str.filter(|h| !h.is_empty());
    php_mail_ex(
        &to_r,
        &subject_r,
        message,
        headers_opt.as_deref(),
        extra_cmd.as_deref(),
        settings,
        script,
        log_date,
        ctx,
        log_file,
    )
}

/// The default `mail.log` writer: appends `line` to the file `filename`
/// (created if missing); a file that cannot be opened is skipped silently.
///
/// # Errors
///
/// Never fails; the `RtResult` matches the `log_file` callback of
/// [`mail_ex`] and [`php_mail_ex`].
pub fn append_log_file<C: ?Sized>(_ctx: &mut C, filename: &[u8], line: &[u8]) -> RtResult<()> {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(std::ffi::OsStr::from_bytes(filename))
    {
        let _ = f.write_all(line);
    }
    Ok(())
}

/// `php_mail()`: logging, header checks and the sendmail pipe. `mail.log`
/// files are appended to with [`append_log_file`].
#[allow(clippy::too_many_arguments)]
pub fn php_mail<C: RuntimeContext + ?Sized>(
    to: &[u8],
    subject: &[u8],
    message: &[u8],
    headers: Option<&[u8]>,
    extra_cmd: Option<&[u8]>,
    settings: &MailSettings,
    script: &ScriptInfo,
    log_date: &dyn Fn(i64) -> String,
    ctx: &mut C,
) -> RtResult<bool> {
    php_mail_ex(
        to,
        subject,
        message,
        headers,
        extra_cmd,
        settings,
        script,
        log_date,
        ctx,
        &mut append_log_file::<C>,
    )
}

/// [`php_mail`] with the `mail.log` file writer supplied by the caller
/// (see [`mail_ex`]). ref: ext/standard/mail.c `php_mail()`: when writing
/// the log leaves an exception (`EG(exception)`), no mail is sent.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn php_mail_ex<C: RuntimeContext + ?Sized>(
    to: &[u8],
    subject: &[u8],
    message: &[u8],
    headers: Option<&[u8]>,
    extra_cmd: Option<&[u8]>,
    settings: &MailSettings,
    script: &ScriptInfo,
    log_date: &dyn Fn(i64) -> String,
    ctx: &mut C,
    log_file: &mut dyn FnMut(&mut C, &[u8], &[u8]) -> RtResult<()>,
) -> RtResult<bool> {
    let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if !settings.log.is_empty() {
        let mut logline = format!(
            "mail() on [{}:{}]: To: {} -- Headers: {} -- Subject: {}",
            lossy(&script.filename),
            script.lineno,
            lossy(to),
            lossy(headers.unwrap_or_default()),
            lossy(subject)
        );
        if headers.is_some() {
            logline = logline.replace(['\r', '\n'], " ");
        }
        if settings.log == b"syslog" {
            php_log::syslog::php_syslog(libc::LOG_NOTICE, logline.as_bytes(), &settings.syslog);
        } else {
            let line = format!("[{}] {logline}\n", log_date(sys::now()));
            log_file(ctx, &settings.log, line.as_bytes())?;
        }
    }
    let line_sep: &[u8] = match settings.cr_lf_mode.as_slice() {
        b"lf" | b"mixed" | b"os" => b"\n",
        _ => {
            if settings.mixed_lf_and_crlf {
                b"\n"
            } else {
                b"\r\n"
            }
        }
    };
    let mut hdr: Option<Vec<u8>> = headers.map(<[u8]>::to_vec);
    if settings.add_x_header {
        let base = script
            .filename
            .rsplit(|&c| c == b'/')
            .next()
            .unwrap_or_default()
            .to_vec();
        let mut x = format!("X-PHP-Originating-Script: {}:", script.uid).into_bytes();
        x.extend_from_slice(&base);
        if let Some(h) = headers.filter(|h| !h.is_empty()) {
            x.extend_from_slice(line_sep);
            x.extend_from_slice(h);
        }
        hdr = Some(x);
    }
    if hdr.as_deref().is_some_and(detect_multiple_crlf) {
        ctx.diagnostic(
            ErrorLevel::Warning,
            "mail(): Multiple or malformed newlines found in additional_header",
        )?;
        return Ok(false);
    }
    if settings.sendmail_path.is_empty() {
        return Ok(false);
    }
    let mut cmd = settings.sendmail_path.clone();
    if let Some(e) = extra_cmd {
        cmd.push(b' ');
        cmd.extend_from_slice(e);
    }
    let child = {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(std::ffi::OsStr::from_bytes(&cmd))
            .stdin(Stdio::piped())
            .spawn()
    };
    let Ok(mut child) = child else {
        ctx.diagnostic(
            ErrorLevel::Warning,
            &format!(
                "mail(): Could not execute mail delivery program '{}'",
                lossy(&settings.sendmail_path)
            ),
        )?;
        return Ok(false);
    };
    let mut body = Vec::new();
    body.extend_from_slice(b"To: ");
    body.extend_from_slice(to);
    body.extend_from_slice(line_sep);
    body.extend_from_slice(b"Subject: ");
    body.extend_from_slice(subject);
    body.extend_from_slice(line_sep);
    if let Some(h) = &hdr {
        body.extend_from_slice(h);
        body.extend_from_slice(line_sep);
    }
    body.extend_from_slice(line_sep);
    let msg = message.split(|&c| c == 0).next().unwrap_or_default();
    if settings.cr_lf_mode == b"lf" {
        let mut i = 0;
        while i < msg.len() {
            if msg[i] == b'\r' && msg.get(i + 1) == Some(&b'\n') {
                body.push(b'\n');
                i += 2;
            } else {
                body.push(msg[i]);
                i += 1;
            }
        }
    } else {
        body.extend_from_slice(msg);
    }
    body.extend_from_slice(line_sep);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&body);
    }
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            ctx.diagnostic(
                ErrorLevel::Warning,
                &format!(
                    "mail(): Sendmail pclose failed {errno} ({})",
                    sys::strerror(errno)
                ),
            )?;
            return Ok(false);
        }
    };
    if let Some(sig) = status.signal() {
        ctx.diagnostic(
            ErrorLevel::Warning,
            &format!(
                "mail(): Sendmail killed by signal {sig} ({})",
                sys::strsignal(sig)
            ),
        )?;
        return Ok(false);
    }
    let Some(ret) = status.code() else {
        ctx.diagnostic(ErrorLevel::Warning, "mail(): Sendmail did not exit")?;
        return Ok(false);
    };
    // EX_OK and EX_TEMPFAIL are success.
    if ret != 0 && ret != 75 {
        ctx.diagnostic(
            ErrorLevel::Warning,
            &format!("mail(): Sendmail exited with non-zero exit code {ret}"),
        )?;
        return Ok(false);
    }
    Ok(true)
}

// ---------------------------------------------------------------- error_log

/// `error_log($message, $message_type, $destination, $additional_headers)`.
/// Type 1 mails through [`php_mail`]; type 3 appends to `destination`
/// (plain files; the VM handles stream wrappers itself if needed).
#[allow(clippy::too_many_arguments)]
pub fn error_log<C: RuntimeContext + php_log::LogHost>(
    message: &[u8],
    message_type: i64,
    destination: Option<&[u8]>,
    headers: Option<&[u8]>,
    error_log_setting: Option<&[u8]>,
    mail_settings: &MailSettings,
    script: &ScriptInfo,
    ctx: &mut C,
) -> RtResult<bool> {
    if destination.is_some_and(|d| d.contains(&0)) {
        return Err(RuntimeError::ValueError(
            "error_log(): Argument #3 ($destination) must not contain any null bytes".into(),
        ));
    }
    match message_type {
        1 => {
            let Some(to) = destination else {
                return Ok(false);
            };
            // `mail.log` dates the line in the default timezone.
            let now = ctx.log_date();
            let date = move |_: i64| now.clone();
            php_mail(
                to,
                b"PHP error_log message",
                message,
                headers,
                None,
                mail_settings,
                script,
                &date,
                ctx,
            )
        }
        2 => Err(RuntimeError::ValueError(
            "TCP/IP option is not available for error logging".into(),
        )),
        3 => {
            let path = destination.unwrap_or_default();
            match std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(std::ffi::OsStr::from_bytes(path))
            {
                Ok(mut f) => Ok(f.write_all(message).is_ok()),
                Err(e) => {
                    ctx.diagnostic(
                        ErrorLevel::Warning,
                        &format!(
                            "error_log({}): Failed to open stream: {}",
                            String::from_utf8_lossy(path),
                            sys::strerror(e.raw_os_error().unwrap_or(0))
                        ),
                    )?;
                    Ok(false)
                }
            }
        }
        4 => {
            ctx.sapi_log_message(message.split(|&c| c == 0).next().unwrap_or_default(), -1);
            Ok(true)
        }
        _ => {
            php_log::php_log_err_with_severity(message, libc::LOG_NOTICE, error_log_setting, ctx);
            Ok(true)
        }
    }
}
