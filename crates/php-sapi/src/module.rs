// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `sapi_module_struct` (`main/SAPI.h`): what the running SAPI does for
//! the engine. Each SAPI implements [`SapiModule`] once and hands it to
//! the VM; the engine calls it instead of asking which SAPI runs.
//!
//! It holds the logger and the output (`ub_write`, `flush`); further
//! callbacks move here as the SAPIs need them (`FastCGI`, FPM).

use std::io::Write;

/// The client is gone: [`SapiModule::ub_write`] or [`SapiModule::flush`]
/// could not deliver output. The engine then runs
/// `php_handle_aborted_connection()`: the connection counts as aborted,
/// output is switched off and, without `ignore_user_abort`, the script
/// stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aborted;

/// The callbacks of `sapi_module_struct` the engine uses.
pub trait SapiModule {
    /// `log_message(message, syslog_type_int)`: the SAPI's own error log,
    /// where PHP's error log goes while `error_log` is unset (stderr in
    /// the CLI, httpd's error log, the built-in server's console, …).
    /// `severity` is a syslog priority (`LOG_ERR`, …), or -1 for
    /// `error_log($message, 4)`.
    fn log_message(&self, message: &[u8], severity: i32);

    /// `ub_write(str, len)`: delivers script output to the client, here
    /// through `out`, the output stream the VM was made with. `Err` when
    /// the client is gone (`sapi_cli_ub_write` on a failed `write()`,
    /// the built-in server on a failed `send()`, FPM on a failed
    /// `fcgi_write()`).
    ///
    /// # Errors
    /// [`Aborted`] when the output could not be delivered.
    fn ub_write(&self, out: &mut dyn Write, data: &[u8]) -> Result<(), Aborted> {
        out.write_all(data).map_err(|_| Aborted)
    }

    /// `flush()`: pushes delivered output on to the client
    /// (`sapi_cli_flush`: `fflush(stdout)`, where `EBADF`, a closed
    /// standard output, is no abort).
    ///
    /// # Errors
    /// [`Aborted`] when the client is gone.
    fn flush(&self, out: &mut dyn Write) -> Result<(), Aborted> {
        match out.flush() {
            Err(e) if e.raw_os_error() != Some(libc::EBADF) => Err(Aborted),
            _ => Ok(()),
        }
    }

    /// The exit status a failed [`SapiModule::ub_write`] gives the
    /// process, if any: the CLI's 255 (`sapi_cli_ub_write`:
    /// `EG(exit_status) = 255`); the other SAPIs leave it.
    fn exit_status_on_abort(&self) -> Option<i32> {
        None
    }
}

/// `sapi_cli_log_message()` / `sapi_cgi_log_message()`: the message and a
/// newline on stderr.
#[derive(Debug, Clone, Copy, Default)]
pub struct StderrLog;

impl SapiModule for StderrLog {
    fn log_message(&self, message: &[u8], _severity: i32) {
        let mut line = Vec::with_capacity(message.len() + 1);
        line.extend_from_slice(message);
        line.push(b'\n');
        // One write, so that the line stays whole between processes.
        let _ = std::io::stderr().lock().write_all(&line);
    }
}

/// The request body as the SAPI delivers it (`sapi_module.read_post`),
/// which every server provides in its own way: CGI from standard input,
/// the built-in server from the body it received, `FastCGI` from `STDIN`
/// records, HTTP from its spool or stream. Implemented by the SAPI's
/// request object rather than by [`SapiModule`]: requests run on threads
/// with their own state, where php-src reads `SG(request_info)`. The read
/// loop, `SG(read_post_bytes)`, the request body buffer and `php://input`
/// are `php-sapi-input`'s (`main/SAPI.c`).
pub trait SapiPost {
    /// The whole request content when the SAPI already holds it in memory
    /// (the built-in server): it is then read from `SG(read_post_bytes)` on
    /// and the request body buffer is a view into it.
    fn content(&self) -> Option<&[u8]> {
        None
    }

    /// `sapi_module.read_post` of a SAPI that streams the body: fills
    /// `buf` as far as it can and returns the bytes read, 0 at the end.
    /// `offset` is `SG(read_post_bytes)`, the body bytes read so far. Not
    /// called when [`SapiPost::content`] is `Some`.
    fn read_post(&self, _offset: u64, _buf: &mut [u8]) -> usize {
        0
    }
}
