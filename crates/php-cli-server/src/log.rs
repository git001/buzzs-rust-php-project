// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The server's console log (`php_cli_server_logf` /
//! `sapi_cli_server_log_write`): `[<asctime>] message` lines on stderr,
//! prefixed with the PID when running with several workers.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

/// `PHP_CLI_SERVER_LOG_PROCESS`: startup banner and fatal server errors.
pub const LOG_PROCESS: i32 = 1;
/// `PHP_CLI_SERVER_LOG_ERROR`: request errors.
pub const LOG_ERROR: i32 = 2;
/// `PHP_CLI_SERVER_LOG_MESSAGE`: access log, connection events, PHP log.
pub const LOG_MESSAGE: i32 = 3;

static LEVEL: AtomicI32 = AtomicI32::new(LOG_MESSAGE);
static WITH_PID: AtomicBool = AtomicBool::new(false);

/// Sets `php_cli_server_log_level` (each `-q` lowers it, minimum 1).
pub fn set_level(level: i32) {
    LEVEL.store(level.max(1), Ordering::Relaxed);
}

/// The current log level.
pub fn level() -> i32 {
    LEVEL.load(Ordering::Relaxed)
}

static PID: AtomicU32 = AtomicU32::new(0);

/// Prefix lines with `[pid]` (`PHP_CLI_SERVER_WORKERS` > 1).
pub fn set_with_pid(on: bool) {
    WITH_PID.store(on, Ordering::Relaxed);
    set_pid(std::process::id());
}

/// The PID shown in log lines (the worker's, also in its request children).
pub fn set_pid(pid: u32) {
    PID.store(pid, Ordering::Relaxed);
}

/// Formats one log line (without writing it).
pub fn format_line(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 40);
    if WITH_PID.load(Ordering::Relaxed) {
        out.extend_from_slice(format!("[{}] ", PID.load(Ordering::Relaxed)).as_bytes());
    }
    out.push(b'[');
    out.extend_from_slice(crate::status::log_time().as_bytes());
    out.extend_from_slice(b"] ");
    out.extend_from_slice(msg);
    out.push(b'\n');
    out
}

/// `sapi_cli_server_log_write()`: writes `msg` when `ty` is enabled.
pub fn write(ty: i32, msg: &[u8]) {
    if level() < ty {
        return;
    }
    raw(&format_line(msg));
}

/// `sapi_cli_server_log_message()`: the PHP error log (while `error_log`
/// is unset) as console lines, `[time] message`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConsoleLog;

impl php_sapi::SapiModule for ConsoleLog {
    fn log_message(&self, message: &[u8], _severity: i32) {
        write(LOG_MESSAGE, message);
    }
}

/// Convenience wrapper for string messages.
pub fn logf(ty: i32, msg: &str) {
    write(ty, msg.as_bytes());
}

/// Writes raw bytes to the real stderr.
pub fn raw(data: &[u8]) {
    let _ = std::io::stderr().write_all(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_shape() {
        let l = format_line(b"hello");
        assert!(l.starts_with(b"["));
        assert!(l.ends_with(b"] hello\n"));
        set_level(0);
        assert_eq!(level(), 1);
        set_level(LOG_MESSAGE);
    }
}
