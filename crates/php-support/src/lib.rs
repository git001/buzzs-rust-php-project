// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Runtime support services for phprust that do not depend on the value model.
//!
//! This crate hosts three self-contained subsystems, each a faithful port of
//! the corresponding C code in php-src:
//!
//! * [`ini`] — the INI scanner/parser (`Zend/zend_ini_scanner.l`,
//!   `Zend/zend_ini_parser.y`), `parse_ini_string()` array building, and an
//!   INI directive registry with PHP's core directives and their modify
//!   handlers (`Zend/zend_ini.c`, `main/main.c`, `Zend/zend.c`).
//! * [`output`] — the output buffering stack of `main/output.c`
//!   (`ob_start()` and friends, handler invocation and flush semantics).
//! * [`fmt`] — PHP's float formatting (`zend_gcvt`, `php_conv_fp`), the
//!   `sprintf()` engine of `ext/standard/formatted_print.c`, `round()` and
//!   `number_format()`.
//!
//! Everything is expressed with plain Rust types and small traits so the VM
//! can adapt its own value type (see [`fmt::FormatArg`],
//! [`output::OutputHost`] and [`ini::IniHooks`]).
//!
//! Errors that PHP reports through `zend_error()` / `php_error_docref()` are
//! surfaced as [`Diagnostic`] values; the embedding VM decides how to display
//! them (and adds the `function(): ` prefix for docref-style messages).

pub mod diag;
pub mod fmt;
pub mod ini;
mod numeric;
pub mod output;

pub use diag::{Diagnostic, ErrorLevel};
pub use numeric::{atol, is_numeric_str, NumericValue};
