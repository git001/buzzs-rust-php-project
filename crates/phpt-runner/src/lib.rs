// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A Rust port of php-src's `run-tests.php`: parses `.phpt` files, runs them against a PHP
//! binary and compares the output with run-tests.php's exact rules.
//!
//! The binary (`src/main.rs`) is a thin CLI over this library; the modules are public so the
//! integration tests can exercise the parser and matchers directly.

#![allow(
    clippy::disallowed_methods,
    reason = "test tool, not on the request path"
)]

pub mod baseline;
pub mod bytes;
pub mod cgi;
pub mod diff;
pub mod discover;
pub mod environment;
pub mod expectf;
pub mod ini;
pub mod junit;
pub mod phpt;
pub mod process;
pub mod report;
pub mod result;
pub mod runner;
pub mod schedule;
pub mod scope;
