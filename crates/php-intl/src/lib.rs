// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/intl` for phprust, bound to the system ICU through its C API.
//!
//! The crate exposes plain Rust types mirroring the PHP classes
//! (`NumberFormatter`, `IntlDateFormatter`, `Collator`, …) and functions;
//! the VM wraps them into PHP objects.  Errors follow php-src's intl error
//! model, see [`error`].
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![deny(unsafe_code)]

pub mod error;
pub mod ffi;
pub mod ustr;

pub mod breakiterator;
pub mod calendar;
pub mod collator;
pub mod common;
pub mod constants;
pub mod converter;
pub mod dateformat;
pub mod datepatterngenerator;
pub mod grapheme;
pub mod idn;
pub mod intlchar;
pub mod listformatter;
pub mod locale;
pub mod msgformat;
pub mod msgpattern;
pub mod normalizer;
pub mod numberformatter;
pub mod numfmt;
pub mod rangeformatter;
pub mod reldateformatter;
pub mod resourcebundle;
pub mod spoofchecker;
pub mod sys;
pub mod timezone;
pub mod transliterator;

pub use error::{Error, ErrorState, IntlError, IntlSettings, Notice, Result, Scope, UErrorCode};
