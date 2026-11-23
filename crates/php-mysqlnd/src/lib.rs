// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A pure-Rust `MySQL` / `MariaDB` client with the semantics of PHP's
//! `ext/mysqlnd` — the driver underneath `ext/mysqli`.
//!
//! * [`Connection`] — connect (TCP / Unix socket / TLS, auth plugins
//!   `mysql_native_password`, `caching_sha2_password`, `sha256_password`,
//!   `mysql_clear_password`, auth switch), text queries, multi-results,
//!   `COM_*` commands, charsets and escaping, transactions, statistics.
//! * [`ResultSet`] — buffered / unbuffered result sets, rows decoded into
//!   [`php_runtime::Value`]s exactly like mysqlnd (strings by default, native
//!   ints/floats with `MYSQLI_OPT_INT_AND_FLOAT_NATIVE`).
//! * [`Statement`] — prepared statements over the binary protocol
//!   (parameter encoding, long data, cursors, multiple result sets).
//! * [`charset`], [`stats`], [`ini`], [`consts`] — the mysqlnd charset
//!   table, statistics counters, `mysqlnd.*` INI table and every
//!   `MYSQLI_*` constant.
//!
//! PHP-level warnings raised inside mysqlnd are queued in [`diag`].

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod auth;
pub mod charset;
pub mod codec;
pub mod conn;
pub mod consts;
pub mod diag;
pub mod error;
pub mod infile;
pub mod inflate;
pub mod ini;
pub mod net;
pub mod protocol;
pub mod result;
pub mod stats;
pub mod stmt;
pub mod wire;

pub use conn::{
    poll, pool, CloseType, ConnState, Connection, OptionValue, PollResult, UpsertStatus,
};
pub use error::{Error, ErrorEntry, ErrorInfo, Result};
pub use protocol::Field;
pub use result::{Metadata, ResultSet};
pub use stmt::{ExecuteError, ParamBind, Statement, StmtState};

/// `mysqli_get_client_info()` / `mysqlnd_get_client_info()`.
pub const CLIENT_INFO: &str = "mysqlnd 8.7.0-dev";
/// `mysqli_get_client_version()` (`MYSQLND_VERSION_ID` = `PHP_VERSION_ID`).
pub const CLIENT_VERSION: u64 = 80700;

/// Test helper: renders values compactly (`Value` has no `PartialEq`).
#[cfg(test)]
pub(crate) fn render(v: &php_runtime::Value) -> String {
    use php_runtime::Value;
    match v {
        Value::Null => "N".into(),
        Value::Bool(b) => format!("b:{b}"),
        Value::Int(i) => format!("i:{i}"),
        Value::Float(f) => format!("d:{f:?}"),
        Value::Str(s) => format!("s:{}", s.to_string_lossy()),
        other => format!("{other:?}"),
    }
}

/// Test helper: renders a row.
#[cfg(test)]
pub(crate) fn render_all(v: &[php_runtime::Value]) -> Vec<String> {
    v.iter().map(render).collect()
}
