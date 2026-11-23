// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/pdo` core for phprust: the `PDO`, `PDOStatement` and `PDORow`
//! classes as Rust handles, the driver interface, the fetch engine and the
//! error modes, plus the adapters for the `SQLite` driver of `php-sqlite`
//! and the `MySQL` driver over `php-mysqlnd` ([`mysql`]).
//!
//! References: `ext/pdo/pdo.c`, `ext/pdo/pdo_dbh.c`, `ext/pdo/pdo_stmt.c`,
//! `ext/pdo/php_pdo_driver.h`, `ext/pdo/*.stub.php`.
//!
//! # Overview
//!
//! * [`driver`] — [`DbhDriver`] / [`StmtDriver`] (the `pdo_dbh_methods` /
//!   `pdo_stmt_methods` tables) and the driver registry
//!   ([`driver::drivers`], [`driver::register_driver`]); [`sqlite`] adapts
//!   `php_sqlite::pdo` to them.
//! * [`Pdo`] — the state of a `PDO` object: construction (DSN aliases,
//!   `uri:`, persistent connections, `PDO::connect()` driver subclasses),
//!   attributes, transactions, `exec`/`prepare`/`query`/`quote`, errors.
//! * [`PdoStatement`] — the state of a `PDOStatement` object: binding,
//!   `execute`, every fetch mode ([`fetch`]), metadata,
//!   `debugDumpParams()`, iteration ([`StmtIterator`]).
//! * [`PdoRow`] — the lazy `PDO::FETCH_LAZY` row object.
//! * [`PdoHost`] — what the VM provides (object creation, constructor and
//!   callable calls, property writes, `PDOException` objects, output,
//!   `php.ini`, the [`PersistentList`]).
//! * [`classes`] / [`consts`] — the class table and constants for the VM.
//!
//! Every public method takes the host and returns `RtResult`: `Err` is
//! the exception the PHP method throws (warnings go through
//! [`PdoHost::docref`] / [`PdoHost::docref_ex`]).
//!
//! # Wiring into the VM
//!
//! * Register the classes of [`classes::classes`] and `pdo_drivers()`
//!   ([`Pdo::get_available_drivers`]).
//! * When an object of `PDO` (or a subclass) is created, store a
//!   [`Pdo::new`] handle in it and call [`Pdo::attach_object`]; likewise
//!   [`PdoStatement::new`] + [`PdoStatement::attach_object`] for
//!   `PDOStatement` objects. [`PdoHost::instantiate_pdo`] /
//!   [`PdoHost::instantiate_statement`] return those handles.
//! * Methods map one to one (`PDO::query` → [`Pdo::query`], ...). The VM
//!   does the plain ZPP type checks of the stubs; variadic and
//!   mode-dependent arguments are validated here with PHP's messages.
//! * `foreach` / `getIterator()` use [`PdoStatement::iterator`]
//!   ([`StmtIterator`]).
//! * `PDORow` objects ([`PdoHost::create_row`]) route their property and
//!   dimension handlers to [`PdoRow`]; `new PDORow` throws
//!   [`PdoRow::constructor_error`].
//! * `PDOStatement::$queryString` writes go through
//!   [`PdoStatement::check_property_write`].
//! * GC: [`Pdo::gc_values`] / [`PdoStatement::gc_values`].
//! * Driver-specific methods: [`Pdo::sqlite`] gives the
//!   `php_sqlite::pdo::PdoSqliteDbh` behind `Pdo\Sqlite::*()` and the
//!   legacy `sqlite*()` methods ([`DbhDriver::driver_methods`]).

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod classes;
pub mod consts;
pub mod dbh;
pub mod driver;
pub mod dsn;
pub mod error;
pub mod fetch;
pub mod host;
pub mod mysql;
pub mod persistent;
pub mod row;
pub mod sqlite;
pub mod stmt;

pub use dbh::{ConnectArgs, Pdo};
pub use driver::{DbhDriver, StmtDriver};
pub use fetch::StmtIterator;
pub use host::{ClassInfo, Ctx, DriverCx, NullHost, PdoException, PdoHost};
pub use persistent::PersistentList;
pub use row::PdoRow;
pub use stmt::{ParamId, PdoStatement};
