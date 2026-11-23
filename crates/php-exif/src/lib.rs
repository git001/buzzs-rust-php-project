// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/exif` plus the image identification shared with
//! `ext/standard/image.c` and `ext/standard/iptc.c`.
//!
//! References: `ext/exif/exif.c`, `ext/standard/image.c`,
//! `ext/standard/php_image.h`, `ext/standard/libavifinfo`,
//! `ext/standard/iptc.c` (php-src master).
//!
//! # Functions
//!
//! | PHP | Rust |
//! |-----|------|
//! | `exif_read_data()` | [`exif::exif_read_data`] |
//! | `exif_thumbnail()` | [`exif::exif_thumbnail`] |
//! | `exif_tagname()` | [`exif::exif_tagname`] |
//! | `exif_imagetype()` | [`imagetype::exif_imagetype`] |
//! | `getimagesize()` / `getimagesizefromstring()` | [`imagesize::getimagesize`] / [`imagesize::getimagesizefromstring`] |
//! | `image_type_to_mime_type()` / `image_type_to_extension()` | [`imagetype::image_type_to_mime_type`] / [`imagetype::image_type_to_extension`] |
//! | `iptcparse()` / `iptcembed()` | [`iptc::iptcparse`] / [`iptc::iptcembed`] |
//! | `IMAGETYPE_*` | [`imagetype::IMAGETYPE_CONSTANTS`] |
//! | `php_image_register_handler()` | [`handlers::register_handler`] |
//! | `EXIF_USE_MBSTRING` | [`EXIF_USE_MBSTRING`] |
//! | `exif.*` INI | [`exif::ExifConfig`], [`exif::INI_ENTRIES`] |
//!
//! # Division of labour with the VM
//!
//! The VM parses arguments (`ValueError`s for empty names / NUL bytes),
//! opens the stream (the stream layer's own "Failed to open stream"
//! warning, then [`exif::UNABLE_TO_OPEN`] for `exif_read_data()` /
//! `exif_thumbnail()`), and passes any `Read + Seek` positioned like the PHP
//! stream together with a [`exif::StreamOrigin`] describing what
//! `php_stream_is(STDIO)` + `stat()` would report.  The functions return the
//! PHP value as a [`php_runtime::Value`] (the exact nested arrays, key order
//! and value types `exif.c` builds) or a typed result, plus the
//! [`diag::Diagnostic`]s to raise *in order*, without the `"func(): "`
//! prefix.
//!
//! Character conversion (UNICODE/JIS `UserComment`, `XP*` tags) goes through
//! [`encoding::EncodingConverter`]; [`encoding::BuiltinConverter`] covers the
//! default settings, the VM may plug in its mbstring.
//!
//! The parsers are written against bounds-checked slices only: every
//! malformed input yields the same warnings as php-src and never panics
//! (see `tests/fuzz.rs`).

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod avif;
mod budget;
pub mod diag;
pub mod encoding;
pub mod exif;
pub mod fmt;
pub mod handlers;
pub mod imagesize;
pub mod imagetype;
mod inflate;
pub mod iptc;
pub mod stream;
pub mod tags;

/// `EXIF_USE_MBSTRING`: 1 when the multibyte conversions are available
/// (always, through [`encoding::EncodingConverter`]).
pub const EXIF_USE_MBSTRING: i64 = 1;

pub use diag::{Diagnostic, Level};
pub use encoding::{BuiltinConverter, EncodingConverter};
pub use exif::{
    exif_read_data, exif_tagname, exif_thumbnail, ExifConfig, ReadOptions, StreamOrigin,
    ThumbnailResult,
};
pub use imagesize::{getimagesize, getimagesizefromstring, GfxInfo, ImageSizeOutcome};
pub use imagetype::{exif_imagetype, image_type_to_extension, image_type_to_mime_type};
