// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/gd` for phprust: a binding of the system libgd with PHP's `ext/gd`
//! semantics (ref: ext/gd/gd.c, ext/gd/gd.stub.php). VM-independent;
//! `php-ext-gd` exposes it to PHP.
//!
//! * [`ffi`]: the libgd wrapper ([`Image`], [`Font`], decoders/encoders,
//!   filters, `FreeType` text) and libgd's message queue ([`errors`]);
//! * [`formats`]: `imagecreatefromstring()`'s type detection;
//! * [`font`]: `imageloadfont()`'s file format and `imagecharup()`;
//! * [`filters`]: operations gd.c implements itself;
//! * [`info`]: `gd_info()`, `imagetypes()`, the constants.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "test code"))]

pub mod ffi;
pub mod filters;
pub mod font;
pub mod formats;
pub mod info;

pub use ffi::{affine, errors, Decoder, Encoder, Filter, Font, Image, Point, Rect};

/// The libgd version this crate was built against.
#[must_use]
pub fn gd_version() -> &'static str {
    info::VERSION_STRING
}
