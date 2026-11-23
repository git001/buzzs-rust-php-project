// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/zlib` for phprust.
//!
//! Bound to the system zlib through `libz-sys` (the library PHP itself
//! links), so every compressed byte matches PHP's output. All `unsafe` code
//! lives in the private `ffi` module.
//!
//! * [`codec`] — `gzcompress()`/`gzuncompress()`, `gzdeflate()`/`gzinflate()`,
//!   `gzencode()`/`gzdecode()`, `zlib_encode()`/`zlib_decode()`.
//! * [`context`] — `deflate_init()`/`deflate_add()`,
//!   `inflate_init()`/`inflate_add()`, `inflate_get_status()`,
//!   `inflate_get_read_len()`.
//! * [`gz`] — [`gz::GzStream`], zlib's `gzFile` semantics over any
//!   `Read + Write + Seek` (the `compress.zlib://` wrapper and
//!   `gzopen()`/`gzread()`/`gzwrite()`/`gzseek()`/…).
//! * [`filter`] — the `zlib.inflate` / `zlib.deflate` stream filters for
//!   `php-streams`.
//! * [`output`] — `ob_gzhandler()`, `zlib.output_compression` and
//!   `zlib_get_coding_type()`.
//! * [`constants`] — every `ZLIB_*` constant.
//!
//! Failures are reported as [`Error`]: warnings carry the message without
//! the `function(): ` prefix (the VM adds it), exceptions carry PHP's full
//! message.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod conv;
mod ffi;

pub mod codec;
pub mod constants;
pub mod context;
pub mod error;
pub mod filter;
pub mod gz;
pub mod output;
pub mod wrapper;

pub use codec::{
    gzcompress, gzdecode, gzdeflate, gzencode, gzinflate, gzuncompress, zlib_decode, zlib_encode,
};
pub use constants::{constants, z_error, zlib_vernum, zlib_version, ConstValue};
pub use context::{DeflateContext, InflateContext};
pub use error::{Error, Result};
