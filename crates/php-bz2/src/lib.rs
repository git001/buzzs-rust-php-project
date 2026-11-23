// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/bz2` for phprust.
//!
//! Bound to libbz2 through `bzip2-sys`; all `unsafe` code lives in the
//! private `ffi` module.
//!
//! * [`codec`] — `bzcompress()` / `bzdecompress()` (string or `int` error
//!   code results, see [`codec::Output`]).
//! * [`file`] — [`file::BzFile`], libbz2's `BZFILE` (`BZ2_bzread`,
//!   `BZ2_bzwrite`, `BZ2_bzerror`, …) over any `Read`/`Write`.
//! * [`wrapper`] — `bzopen()`, the `compress.bzip2://` wrapper and the
//!   PHP stream ops (`php_stream_bz2io_ops`), `bzerrno()`/`bzerrstr()`/
//!   `bzerror()`.
//! * [`filter`] — the `bzip2.compress` / `bzip2.decompress` stream filters.
//!
//! Failures are reported as [`Error`]: warnings without the `function(): `
//! prefix (the VM adds it), exceptions with PHP's full message.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod conv;
mod ffi;

pub mod codec;
pub mod error;
pub mod file;
pub mod filter;
pub mod wrapper;

pub use codec::{bzcompress, bzdecompress, Output};
pub use error::{Error, Result};

/// `BZ_RUN`.
pub const BZ_RUN: i32 = 0;
/// `BZ_FLUSH`.
pub const BZ_FLUSH: i32 = 1;
/// `BZ_FINISH`.
pub const BZ_FINISH: i32 = 2;
/// `BZ_OK`.
pub const BZ_OK: i32 = 0;
/// `BZ_RUN_OK`.
pub const BZ_RUN_OK: i32 = 1;
/// `BZ_FLUSH_OK`.
pub const BZ_FLUSH_OK: i32 = 2;
/// `BZ_FINISH_OK`.
pub const BZ_FINISH_OK: i32 = 3;
/// `BZ_STREAM_END`.
pub const BZ_STREAM_END: i32 = 4;
/// `BZ_SEQUENCE_ERROR`.
pub const BZ_SEQUENCE_ERROR: i32 = -1;
/// `BZ_PARAM_ERROR`.
pub const BZ_PARAM_ERROR: i32 = -2;
/// `BZ_MEM_ERROR`.
pub const BZ_MEM_ERROR: i32 = -3;
/// `BZ_DATA_ERROR`.
pub const BZ_DATA_ERROR: i32 = -4;
/// `BZ_DATA_ERROR_MAGIC`.
pub const BZ_DATA_ERROR_MAGIC: i32 = -5;
/// `BZ_IO_ERROR`.
pub const BZ_IO_ERROR: i32 = -6;
/// `BZ_UNEXPECTED_EOF`.
pub const BZ_UNEXPECTED_EOF: i32 = -7;
/// `BZ_OUTBUFF_FULL`.
pub const BZ_OUTBUFF_FULL: i32 = -8;
/// `BZ_CONFIG_ERROR`.
pub const BZ_CONFIG_ERROR: i32 = -9;

/// `BZ2_bzlibVersion()` of the bundled/linked libbz2 (shown by `phpinfo()`).
pub const BZLIB_VERSION: &str = "1.0.8, 13-Jul-2019";
