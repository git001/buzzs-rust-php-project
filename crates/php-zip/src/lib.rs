// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/zip` for phprust, bound to the system libzip.
//!
//! * [`ZipArchive`] — the object state and every method of the class
//!   (`open`/`openString`, `add*`, `set*`/`get*`, `stat*`, `extractTo`,
//!   `getFrom*`, `getStream*`, callbacks, the virtual properties);
//! * [`stream`] — entry streams ([`stream::ZipStreamOps`]) and the `zip://`
//!   wrapper ([`ZipWrapper`]) for `php-streams`;
//! * [`procedural`] — the deprecated `zip_open()`/`zip_read()`/
//!   `zip_entry_*()` functions;
//! * [`consts`] — the `ZipArchive::*` constants and `phpinfo()` rows;
//! * [`class`] — method/parameter tables from `php_zip.stub.php`.
//!
//! Exceptions are returned as [`php_runtime::RuntimeError`] with PHP's
//! exact messages; warnings and deprecations go through
//! [`php_streams::diag`] (without the `function(): ` prefix). PHP code
//! (progress/cancel callbacks, `addPattern()` regexes, value conversions)
//! runs through the [`ZipHost`] the VM passes to the methods that need it.
//!
//! All `unsafe` code wraps libzip calls; the raw declarations are in the
//! private `ffi` module.
//!
//! # VM integration
//!
//! * Object handlers: create with [`ZipArchive::new`]; `read_property` →
//!   [`ZipArchive::read_property`] (falls back to the standard handler for
//!   other names); `write_property` / `get_property_ptr_ptr` on one of
//!   [`PROPERTY_NAMES`] → [`error::readonly_property`]; `has_property` →
//!   [`ZipArchive::has_property`]; `get_properties` → [`ZipArchive::properties`] merged in front
//!   of the dynamic properties; `clone_obj` is NULL (uncloneable);
//!   `dtor_obj` → [`ZipArchive::destroy`]; `free_obj` → [`ZipArchive::free`]
//!   (or simply dropping the value).
//! * Methods: [`class::METHODS`] lists names, parameters and (tentative)
//!   return types; `__serialize`/`__unserialize` throw
//!   [`ZipArchive::serialize`] / [`ZipArchive::unserialize`].
//!   `getExternalAttributes*()` return the values to assign to the two
//!   by-reference parameters.
//! * Streams: `getStream*()` return a ready [`php_streams::stream::Stream`]
//!   (ops label `"zip"`, `orig_path` = entry name, no wrapper); register
//!   [`ZipWrapper`] as `"zip"`.
//! * `RuntimeError::Fatal` returned from a method means a callback bailed
//!   out (`exit()`); the VM should continue unwinding.
//!
//! # Known gaps
//!
//! * Archives closed when the last *stream* lets go of them (after the
//!   object was destroyed) run without the VM, so progress/cancel callbacks
//!   are not invoked then (PHP would call them).
//! * `expand_filepath()` comes from `php-streams` and is purely lexical,
//!   while PHP resolves symlinks of existing path components
//!   (`CWD_FILEPATH`); the `filename` property can differ for paths through
//!   symlinks.
//! * Features of libzip newer than the system 1.7.3 (seekable entry
//!   streams, zstd, torrentzip, `LENGTH_UNCHECKED`, …) are compiled in only
//!   when building against such a libzip; that code path is untested here.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod extract;
mod ffi;
mod paths;
mod pattern;
mod source;

pub mod archive;
pub mod class;
pub mod consts;
pub mod error;
pub mod host;
pub mod procedural;
pub mod stream;

pub use archive::{EntryStat, ExtractFiles, OpenResult, ZipArchive, PROPERTY_NAMES};
pub use host::{RegexMatcher, ZipHost};
pub use stream::{ZipStreamOps, ZipWrapper};

/// The libzip version this crate was built against (`LIBZIP_VERSION`).
pub const LIBZIP_HEADERS_VERSION: &str = env!("PHP_ZIP_LIBZIP_VERSION");
