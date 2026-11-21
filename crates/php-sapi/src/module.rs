// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `sapi_module_struct` (`main/SAPI.h`): what the running SAPI does for
//! the engine. Each SAPI implements [`SapiModule`] once and hands it to
//! the VM; the engine calls it instead of asking which SAPI runs.
//!
//! It starts with the logger; further callbacks move here as the SAPIs
//! need them (`FastCGI`, FPM).

use std::io::Write;

/// The callbacks of `sapi_module_struct` the engine uses.
pub trait SapiModule {
    /// `log_message(message, syslog_type_int)`: the SAPI's own error log,
    /// where PHP's error log goes while `error_log` is unset (stderr in
    /// the CLI, httpd's error log, the built-in server's console, …).
    /// `severity` is a syslog priority (`LOG_ERR`, …), or -1 for
    /// `error_log($message, 4)`.
    fn log_message(&self, message: &[u8], severity: i32);
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
