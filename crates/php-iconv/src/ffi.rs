// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Safe wrapper around the C library's `iconv_open`/`iconv`/`iconv_close`.
//!
//! This is the only module of the crate containing `unsafe` code.  The
//! wrapper mirrors the C calling convention closely (input slice that is
//! advanced, output buffer + write position, `(size_t)-1` + `errno` on
//! failure) because PHP's algorithms depend on the exact partial-progress
//! semantics of `iconv(3)`.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString};

/// A conversion descriptor (`iconv_t`).
#[derive(Debug)]
pub struct Cd {
    raw: libc::iconv_t,
}

/// Converts a charset name to a C string the way C sees it (truncated at
/// the first NUL byte).
fn charset_cstr(name: &[u8]) -> CString {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    // Cannot fail: no interior NUL remains.
    CString::new(&name[..end]).unwrap_or_default()
}

thread_local! {
    /// The `errno` left behind by the most recent failing iconv call on
    /// this thread.  PHP occasionally reports a stale global `errno`
    /// ("Unknown error (%d)"); this mirrors it deterministically.
    static LAST_ERRNO: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Reads the calling thread's `errno` and records it as the last error.
fn errno() -> i32 {
    let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    LAST_ERRNO.with(|c| c.set(e));
    e
}

/// The errno of the most recent failing iconv call on this thread (what
/// PHP's global `errno` would most likely hold).
pub fn last_errno() -> i32 {
    LAST_ERRNO.with(std::cell::Cell::get)
}

impl Cd {
    /// `iconv_open(to, from)`; `Err(errno)` on failure.
    pub fn open(to: &[u8], from: &[u8]) -> Result<Self, i32> {
        let to = charset_cstr(to);
        let from = charset_cstr(from);
        // SAFETY: both arguments are valid NUL-terminated strings.
        let raw = unsafe { libc::iconv_open(to.as_ptr(), from.as_ptr()) };
        if raw as isize == -1 {
            Err(errno())
        } else {
            Ok(Self { raw })
        }
    }

    /// `iconv(cd, &in, &in_left, &out, &out_left)`.
    ///
    /// `input` is advanced past the consumed bytes; converted bytes are
    /// written to `out[*pos..]` and `*pos` is advanced.  Returns the number
    /// of irreversible conversions, or `Err(errno)`.
    pub fn conv(
        &mut self,
        input: &mut &[u8],
        out: &mut [u8],
        pos: &mut usize,
    ) -> Result<usize, i32> {
        let mut in_ptr = input.as_ptr().cast::<libc::c_char>().cast_mut();
        let mut in_left = input.len();
        let tail = &mut out[*pos..];
        let mut out_ptr = tail.as_mut_ptr().cast::<libc::c_char>();
        let mut out_left = tail.len();
        // SAFETY: the pointers/lengths describe live, correctly sized
        // buffers; iconv never writes through the input pointer.
        let r = unsafe {
            libc::iconv(
                self.raw,
                &raw mut in_ptr,
                &raw mut in_left,
                &raw mut out_ptr,
                &raw mut out_left,
            )
        };
        let err = if r == usize::MAX { Some(errno()) } else { None };
        let consumed = input.len() - in_left;
        *input = &input[consumed..];
        *pos += tail.len() - out_left;
        err.map_or(Ok(r), Err)
    }

    /// `iconv(cd, NULL, NULL, &out, &out_left)`: emit the shift sequence
    /// returning to the initial state.
    pub fn flush(&mut self, out: &mut [u8], pos: &mut usize) -> Result<usize, i32> {
        let tail = &mut out[*pos..];
        let mut out_ptr = tail.as_mut_ptr().cast::<libc::c_char>();
        let mut out_left = tail.len();
        // SAFETY: as in `conv`; null input requests a state flush.
        let r = unsafe {
            libc::iconv(
                self.raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut out_ptr,
                &raw mut out_left,
            )
        };
        let err = if r == usize::MAX { Some(errno()) } else { None };
        *pos += tail.len() - out_left;
        err.map_or(Ok(r), Err)
    }

    /// `iconv(cd, NULL, NULL, NULL, NULL)`: reset the conversion state.
    pub fn reset(&mut self) -> Result<usize, i32> {
        // SAFETY: all-null arguments are the documented reset request.
        let r = unsafe {
            libc::iconv(
                self.raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if r == usize::MAX {
            Err(errno())
        } else {
            Ok(r)
        }
    }
}

impl Drop for Cd {
    fn drop(&mut self) {
        // SAFETY: `raw` came from a successful iconv_open and is closed
        // exactly once.
        unsafe {
            libc::iconv_close(self.raw);
        }
    }
}

// SAFETY: an iconv_t is an opaque heap object with no thread affinity; it
// is only ever used through `&mut self`, so moving it between threads is
// sound.
unsafe impl Send for Cd {}

/// `gnu_get_libc_version()` — the value of `ICONV_VERSION` with glibc.
pub fn glibc_version() -> String {
    extern "C" {
        fn gnu_get_libc_version() -> *const libc::c_char;
    }
    // SAFETY: gnu_get_libc_version returns a pointer to a static,
    // NUL-terminated string.
    unsafe { CStr::from_ptr(gnu_get_libc_version()) }
        .to_string_lossy()
        .into_owned()
}
