// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Capturing what libmagic writes to `stderr`.
//!
//! PHP's bundled libmagic routes `file_magwarn()` (magic-file parse
//! warnings such as ``offset `a' invalid``) through `php_error_docref()`.
//! The system library prints them with `fprintf(stderr, …)` instead, as
//! `"<file>, <line>: Warning: <message>\n"`. To surface them as PHP
//! warnings, the C library's `stderr` stream points at an in-memory
//! stream (`open_memstream`) while the library runs; then what it wrote
//! is parsed.
//!
//! Only C `stdio` writes to `stderr` are captured: file descriptor 2 stays
//! as it is, so Rust's `stderr`, the SAPI logs and other threads writing
//! to it are not affected. A global lock serialises captures.

#![allow(unsafe_code)]

use std::ffi::c_char;
use std::ptr;
use std::sync::Mutex;

use crate::ffi;

static LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` with the C `stderr` stream captured and returns its result
/// together with everything written to it meanwhile. When no memory
/// stream can be opened, `f` still runs and the captured output is empty.
pub(crate) fn with_captured_stderr<T>(f: impl FnOnce() -> T) -> (T, Vec<u8>) {
    let guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut buf: *mut c_char = ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: both out-pointers reference live locals; they are only read
    // after `fclose` below, which finalises them.
    let mem = unsafe { ffi::open_memstream(&raw mut buf, &raw mut len) };
    if mem.is_null() {
        return (f(), Vec::new());
    }
    // SAFETY: `stderr` is the C library's stream pointer; it is replaced
    // only under `LOCK` and restored before the memory stream is closed.
    let saved = unsafe {
        let saved = ffi::stderr;
        ffi::stderr = mem;
        saved
    };
    let result = f();
    // SAFETY: as above; `mem` is open and closed exactly once, which
    // flushes it and sets `buf`/`len`.
    unsafe {
        ffi::stderr = saved;
        ffi::fclose(mem);
    }
    drop(guard);
    let mut out = Vec::new();
    if !buf.is_null() {
        // SAFETY: `open_memstream` made `buf` a malloc'ed buffer of `len`
        // bytes; it is copied, then freed once.
        unsafe {
            out.extend_from_slice(std::slice::from_raw_parts(buf.cast::<u8>(), len));
            ffi::free(buf.cast());
        }
    }
    (result, out)
}

/// Turns captured libmagic diagnostics into PHP warning texts: each line's
/// `"<file>, <line>: "` prefix and `"Warning: "` tag are dropped, as PHP's
/// `file_magwarn()` reports only the message.
pub(crate) fn magwarn_messages(captured: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(captured);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            line.find("Warning: ")
                .map_or(line, |i| &line[i + "Warning: ".len()..])
                .to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_file_and_line_prefix() {
        let msgs = magwarn_messages(
            b"/x/bad.magic, 1: Warning: offset `string' invalid\nWarning: plain\n\nother\n",
        );
        assert_eq!(msgs, ["offset `string' invalid", "plain", "other"]);
    }

    unsafe extern "C" {
        fn fputs(s: *const c_char, stream: *mut ffi::CFile) -> std::ffi::c_int;
    }

    #[test]
    fn captures_c_stderr_but_not_fd2() {
        let ((), out) = with_captured_stderr(|| {
            // SAFETY: a NUL-terminated literal and the (captured) stream.
            unsafe { fputs(c"captured!\n".as_ptr(), ffi::stderr) };
            // Rust writes to fd 2 directly: not captured.
            eprintln!("fd 2 is not captured");
        });
        assert_eq!(out, b"captured!\n");
    }
}
