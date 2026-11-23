// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/json` for phprust: `json_encode`, `json_decode`,
//! `json_validate`, `json_last_error(_msg)`, `JsonSerializable` and
//! `JsonException` support.
//!
//! References: `ext/json/json.c`, `json_encoder.c`, `json_scanner.re`,
//! `json_parser.y`, `json.stub.php` (php-src master), `ext/standard/html.c`
//! (`php_next_utf8_char`).
//!
//! # Layers
//!
//! * [`encode()`], [`decode()`], [`validate`]: the engine entry points
//!   (`php_json_encode_zval`, `php_json_decode_ex`, `php_json_validate_ex`),
//!   returning [`JsonError`]s with php-src's exact codes and (for decoding)
//!   error locations.
//! * [`functions`]: the PHP functions themselves on top of a per-request
//!   [`JsonState`] (last error), with the `ValueError`s for invalid
//!   arguments and `JSON_THROW_ON_ERROR` handling.
//! * [`JsonHost`]: everything object-related the VM provides
//!   (`JsonSerializable::jsonSerialize()`, enums, property tables, `stdClass`
//!   creation, recursion guards, `JsonException`). It extends
//!   [`php_runtime::RuntimeContext`]; [`php_runtime::CollectingContext`]
//!   implements it with the defaults (no objects).
//!
//! # Behaviour notes
//!
//! * The decoder is an iterative port of the bison parser that reproduces
//!   when tokens are scanned, so error codes, error locations
//!   (`"Syntax error near location 1:5"`), object creation order and the
//!   `YYMAXDEPTH` stack limit (nesting beyond 4998 arrays is a syntax error)
//!   all match php-src.
//! * The encoder recurses; very deep values are cut off with
//!   `JSON_ERROR_DEPTH` through [`JsonHost::json_stack_limit_reached`]
//!   (php-src uses its native stack limit there).

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "test code"))]

pub mod decode;
pub mod encode;
pub mod error;
pub mod flags;
pub mod functions;
pub mod host;
mod parser;
mod scanner;

pub use decode::{decode, validate};
pub use encode::{encode, encode_string};
pub use error::{DecodeError, EncodeError, ErrorCode, JsonError};
pub use functions::JsonState;
pub use host::{GuardKey, JsonHost};

impl JsonHost for php_runtime::CollectingContext {}

/// The interface the VM must register (`json.stub.php`): `interface
/// JsonSerializable { public function jsonSerialize(): mixed; }`.
pub const JSON_SERIALIZABLE_INTERFACE: &str = "JsonSerializable";
/// The exception class the VM must register: `class JsonException extends
/// Exception {}`, thrown with `JSON_THROW_ON_ERROR` (see
/// [`JsonHost::json_exception`]).
pub const JSON_EXCEPTION_CLASS: &str = "JsonException";

#[cfg(test)]
mod object_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod test_host;
