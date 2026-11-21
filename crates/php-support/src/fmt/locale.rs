// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What other crates need to know about the calling thread's locale,
//! published by the `setlocale()` binding after every change (and at
//! request shutdown, when PHP restores the "C" locale):
//!
//! * the `LC_NUMERIC` decimal point, as PHP's formatters read it through
//!   `LCONV_DECIMAL_POINT` (`main/snprintf.c`, `main/spprintf.c`,
//!   `ext/standard/formatted_print.c`; `localeconv_decimal_point()` in
//!   `ext/standard/string.c`), so that `%f` needs no `localeconv()` call;
//! * the `LC_CTYPE` locale name a script selected (`BG(ctype_string)`),
//!   which PCRE uses to key its character tables (`ext/pcre/php_pcre.c`).
//!
//! `setlocale()` changes only the calling thread's locale (each request
//! thread of a threaded server has its own, as each FPM process has), so
//! both facts are thread-local. Reading the decimal point is one
//! thread-local load; in the C locale it is `b'.'`.

use std::cell::{Cell, RefCell};

thread_local! {
    static DECIMAL_POINT: Cell<u8> = const { Cell::new(b'.') };
    static CTYPE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// `LCONV_DECIMAL_POINT`: the first byte of the calling thread's
/// `localeconv()->decimal_point` (`b'.'` until [`set_decimal_point`] is
/// called with something else).
#[inline]
#[must_use]
pub fn decimal_point() -> u8 {
    DECIMAL_POINT.with(Cell::get)
}

/// Records the C library's current `*localeconv()->decimal_point` for the
/// calling thread. Called by the `setlocale()` binding after its locale
/// changed.
#[inline]
pub fn set_decimal_point(dp: u8) {
    DECIMAL_POINT.with(|d| d.set(dp));
}

/// `BG(ctype_string)`: the `LC_CTYPE` locale a script of the calling
/// thread selected with `setlocale()`, `None` for "C".
#[must_use]
pub fn ctype_name() -> Option<Vec<u8>> {
    CTYPE.with(|c| c.borrow().clone())
}

/// Records [`ctype_name`] for the calling thread.
pub fn set_ctype_name(name: Option<&[u8]>) {
    CTYPE.with(|c| *c.borrow_mut() = name.map(<[u8]>::to_vec));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_restore() {
        let before = decimal_point();
        assert_eq!(before, b'.');
        set_decimal_point(b',');
        assert_eq!(decimal_point(), b',');
        set_decimal_point(before);
        assert_eq!(decimal_point(), b'.');
    }

    #[test]
    fn each_thread_has_its_own() {
        set_decimal_point(b',');
        set_ctype_name(Some(b"de_DE.UTF-8"));
        let other = std::thread::spawn(|| (decimal_point(), ctype_name()))
            .join()
            .expect("thread");
        assert_eq!(other, (b'.', None));
        assert_eq!(ctype_name().as_deref(), Some(&b"de_DE.UTF-8"[..]));
        set_decimal_point(b'.');
        set_ctype_name(None);
    }
}
