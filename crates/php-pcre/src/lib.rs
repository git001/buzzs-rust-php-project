// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/pcre` (`preg_*`) semantics on top of the real PCRE2 engine.
//!
//! This crate reproduces `ext/pcre/php_pcre.c` — pattern delimiter and
//! modifier parsing with PHP's exact warnings, the compiled-pattern cache,
//! `pcre.*` INI limits, the empty-match advancement loop, capture array
//! shapes, replacement string parsing and the `preg_last_error()` codes —
//! while staying independent of the VM's value types:
//!
//! * [`Pcre`] is the per-request state (cache, INI settings, last error).
//!   [`Pcre::compile`] returns a cheap-to-clone [`Regex`] or a
//!   [`CompileError`] whose [`CompileError::message`] is the warning text.
//! * The operations are free functions taking `&Regex` and a copied
//!   [`MatchConfig`], so user callbacks may re-enter the extension:
//!   [`match_one`], [`match_all`], [`replace`], [`replace_callback`],
//!   [`replace_array`], [`split`], [`grep`], [`quote`].
//! * Matching can be split in two phases for `memory_limit` accounting:
//!   [`match_one_spans`] / [`match_all_spans`] record only offsets (see
//!   [`MatchSpans::captured_bytes`] and [`MatchSpans::matches`]), and
//!   [`MatchSpans::into_outcome`] then copies the captures.
//! * [`MatchConfig::with_known_valid_utf8`] lets the caller skip PCRE2's
//!   UTF-8 validation for subjects it knows are valid (PHP's
//!   `IS_STR_VALID_UTF8` flag); [`MatchOutcome::subject_valid_utf8`] reports
//!   when a match established validity.
//! * Arrays come back as [`MatchArray`] (insertion-ordered [`Key`] →
//!   [`Value`]) which maps 1:1 onto PHP arrays.
//!
//! Composition notes for the VM:
//! * `preg_replace()` with a string pattern: compile, then [`replace`] with
//!   [`Replacement::parse`]; with an array pattern use [`replace_array`].
//!   For array subjects repeat per element, summing counts; `preg_filter()`
//!   keeps an element only when its own count is non-zero.
//! * `preg_replace_callback_array()`: for each `pattern => callback` pair in
//!   order, compile and run [`replace_callback`] on the running subject(s);
//!   stop with `null` at the first failure.
//! * `preg_last_error()`: every outcome documents what to store.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod cache;
mod error;
mod exec;
mod ffi;
mod matching;
mod regex;
mod replace;
mod split;
mod value;

pub use cache::{MatchConfig, Pcre, RegexCache, PCRE_CACHE_SIZE};
pub use error::{CompileError, PregError};
pub use matching::{
    match_all, match_all_spans, match_one, match_one_spans, MatchFailure, MatchOutcome, MatchRef,
    MatchSpans,
};
pub use regex::{parse_pattern, ParsedPattern, Regex};
pub use replace::{
    limit_from_php, replace, replace_array, replace_callback, ReplaceOutcome, Replacement,
    ReplacementPiece, Replacements,
};
pub use split::{grep, quote, split};
pub use value::{Key, MatchArray, Value};

/// `PREG_PATTERN_ORDER`
pub const PREG_PATTERN_ORDER: i64 = 1;
/// `PREG_SET_ORDER`
pub const PREG_SET_ORDER: i64 = 2;
/// `PREG_OFFSET_CAPTURE`
pub const PREG_OFFSET_CAPTURE: i64 = 1 << 8;
/// `PREG_UNMATCHED_AS_NULL`
pub const PREG_UNMATCHED_AS_NULL: i64 = 1 << 9;
/// `PREG_SPLIT_NO_EMPTY`
pub const PREG_SPLIT_NO_EMPTY: i64 = 1;
/// `PREG_SPLIT_DELIM_CAPTURE`
pub const PREG_SPLIT_DELIM_CAPTURE: i64 = 1 << 1;
/// `PREG_SPLIT_OFFSET_CAPTURE`
pub const PREG_SPLIT_OFFSET_CAPTURE: i64 = 1 << 2;
/// `PREG_GREP_INVERT`
pub const PREG_GREP_INVERT: i64 = 1;
/// `PREG_NO_ERROR`
pub const PREG_NO_ERROR: i64 = 0;
/// `PREG_INTERNAL_ERROR`
pub const PREG_INTERNAL_ERROR: i64 = 1;
/// `PREG_BACKTRACK_LIMIT_ERROR`
pub const PREG_BACKTRACK_LIMIT_ERROR: i64 = 2;
/// `PREG_RECURSION_LIMIT_ERROR`
pub const PREG_RECURSION_LIMIT_ERROR: i64 = 3;
/// `PREG_BAD_UTF8_ERROR`
pub const PREG_BAD_UTF8_ERROR: i64 = 4;
/// `PREG_BAD_UTF8_OFFSET_ERROR`
pub const PREG_BAD_UTF8_OFFSET_ERROR: i64 = 5;
/// `PREG_JIT_STACKLIMIT_ERROR`
pub const PREG_JIT_STACKLIMIT_ERROR: i64 = 6;

/// `PCRE_VERSION`: the engine's version string, e.g. `"10.42 2022-12-11"`.
pub fn pcre_version() -> String {
    ffi::version()
}

/// `(PCRE_VERSION_MAJOR, PCRE_VERSION_MINOR)` parsed from [`pcre_version`].
pub fn version_numbers() -> (i64, i64) {
    let v = pcre_version();
    let num = v.split_whitespace().next().unwrap_or("");
    let mut it = num.split('.');
    let major = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = it
        .next()
        .map(|s| s.trim_end_matches(|c: char| !c.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (major, minor)
}

/// `PCRE_JIT_SUPPORT`: whether the engine was built with JIT.
pub fn jit_support() -> bool {
    ffi::jit_support()
}

/// "PCRE Unicode Version" from `phpinfo()`.
pub fn unicode_version() -> String {
    ffi::unicode_version()
}

/// "PCRE JIT Target" from `phpinfo()`, if JIT is available.
pub fn jit_target() -> Option<String> {
    ffi::jit_target()
}

#[cfg(test)]
mod tests;
