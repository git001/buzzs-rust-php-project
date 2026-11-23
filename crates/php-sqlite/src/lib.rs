// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/sqlite3` and `ext/pdo_sqlite` for phprust, bound to the
//! system `libsqlite3`.
//!
//! References: `ext/sqlite3/sqlite3.c`, `ext/pdo_sqlite/sqlite_driver.c`,
//! `ext/pdo_sqlite/sqlite_statement.c`, `ext/pdo_sqlite/pdo_sqlite.c`,
//! `ext/pdo/pdo_sql_parser.re`, `ext/pdo_sqlite/sqlite_sql_parser.re`,
//! `ext/pdo/pdo_sqlstate.c`.
//!
//! # Overview
//!
//! * [`sqlite3`] — the `SQLite3`, `SQLite3Stmt` and `SQLite3Result` classes
//!   as plain Rust handles ([`Sqlite3`], [`Sqlite3Stmt`],
//!   [`Sqlite3Result`]) with every warning and exception text of
//!   `sqlite3.c`.
//! * [`pdo`] — the driver side of `PDO` for `SQLite` (the `pdo_dbh_methods` /
//!   `pdo_stmt_methods` tables plus the `Pdo\Sqlite` methods), the PDO SQL
//!   placeholder parser ([`pdo::parser`]) and the SQLSTATE table
//!   ([`pdo::sqlstate`]) the PDO core in the VM reuses.
//! * [`blob`] — `openBlob()` streams as a `php-streams` [`StreamOps`]
//!   implementation.
//! * [`host`] — the [`Host`] trait the VM implements (calling user
//!   callables, diagnostics, `open_basedir`, INI settings, stream reads)
//!   and the per-call context [`Cx`] that tracks the pending exception the
//!   way `EG(exception)` does.
//!
//! All `unsafe` code lives in the private `ffi` module.
//!
//! [`StreamOps`]: php_streams::stream::StreamOps

// FFI code naturally has `argc`/`argv`, `src`/`dst` pairs.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::similar_names)]
// "SQLite" is a proper noun, not an identifier.
#![allow(clippy::doc_markdown)]

pub mod blob;
pub mod constants;
mod conv;
pub mod error;
mod ffi;
pub mod host;
pub mod pdo;
pub mod sqlite3;

pub use blob::BlobStream;
pub use error::{Exception, ExceptionClass, PhpError};
pub use host::{Cx, Host};
pub use sqlite3::{ParamKey, Sqlite3, Sqlite3Result, Sqlite3Stmt};

/// The `SQLite` library version string (`sqlite3_libversion()`).
pub fn sqlite_version() -> String {
    ffi::libversion()
}

/// The `SQLite` library version number (`sqlite3_libversion_number()`).
pub fn sqlite_version_number() -> i64 {
    i64::from(ffi::libversion_number())
}
