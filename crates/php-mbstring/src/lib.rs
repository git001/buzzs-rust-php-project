// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/mbstring` for phprust: libmbfl's encodings and the `mb_*`
//! string functions, as plain Rust APIs over byte strings.
//!
//! The VM wraps these: encodings are passed by name, results are byte
//! strings / integers, PHP exceptions are reported as [`MbError`] values
//! carrying PHP's exact messages, and warnings/deprecations are pushed to
//! [`MbState::diagnostics`].
//!
//! # Usage from the VM
//!
//! One [`MbState`] lives per request (`MbState::with_ini` at startup,
//! [`MbState::request_shutdown`] at the end). Every `mb_*` function is a
//! method on it taking the PHP arguments after zpp coercion (`Option` for
//! nullable/omitted ones):
//!
//! * returns `Ok(value)`; `Option` results map `None` to PHP `false`;
//! * `Err(MbError)` is a thrown exception — [`MbError::class`] and
//!   [`MbError::message_for`]`("mb_strlen")` give PHP's class and message;
//! * after each call, drain [`MbState::take_diagnostics`] and raise them as
//!   `E_WARNING`/`E_DEPRECATED` prefixed with `"func(): "`.
//!
//! Array-shaped arguments are traversed by the VM: `mb_convert_encoding`
//! on arrays ([`MbState::convert_encoding_args`] +
//! [`MbState::convert_encoding_value`] per key/string),
//! `mb_check_encoding` on arrays ([`strings::check_str_encoding`]),
//! `mb_convert_variables` ([`MbState::convert_variables_encodings`] +
//! [`MbState::convert_ex`]), and encoding-list arguments are passed as
//! [`EncodingList`]. INI changes go through [`MbState::ini_set`].
//!
//! Lower layers are usable directly: [`encoding`] (registry and lookup),
//! [`convert`] (decode/encode with substitution), [`unicode`] (properties,
//! case mapping, display width), [`detect`] (encoding detection), [`onig`]
//! (the system Oniguruma behind `mb_ereg*`).
//!
//! Ref: `ext/mbstring/` (`mbstring.c`, `php_unicode.c`, `libmbfl/`).

pub mod codecs;
pub mod convert;
pub mod detect;
pub mod encoding;
pub mod error;
pub mod info;
pub mod legacy;
pub mod mbregex;
pub mod mime;
pub mod numericentity;
pub mod onig;
pub mod state;
pub mod strcut;
pub mod strings;
pub mod tables;
pub mod unicode;

pub use convert::{ConvertBuf, ErrorMode, BAD_INPUT};
pub use encoding::{Encoding, EncodingNo};
pub use error::{Diagnostic, Level, MbError, MbResult};
pub use state::{
    ArrayEntry, EncodingArray, EncodingList, HttpInput, Language, MbState, SubstituteArg,
    SubstituteChar,
};
