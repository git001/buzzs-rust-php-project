// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/phar` for phprust.
//!
//! A port of `ext/phar` (`phar.c`, `tar.c`, `zip.c`, `util.c`, `stream.c`,
//! `dirstream.c`, `phar_object.c`, `func_interceptors.c`) that does not
//! depend on the VM:
//!
//! * [`archive`] — the in-memory model ([`archive::Archive`],
//!   [`archive::Entry`]) and entry content access.
//! * [`phar_format`], [`tar_format`], [`zip_format`] — readers and writers
//!   for the three container formats, byte-compatible with PHP's output.
//! * [`registry`] — the per-request archive registry (`PHAR_G`): file name
//!   and alias maps, `phar.readonly` / `phar.require_hash` /
//!   `phar.cache_list`, opening, creating and flushing archives.
//! * [`signature`] — MD5/SHA signatures (via `php-hash`) and OpenSSL
//!   signatures through an [`signature::OpenSslHook`].
//! * [`stub`] — the default stub (byte-exact) and stub normalisation.
//! * [`object`] — the `Phar`, `PharData` and `PharFileInfo` methods with
//!   their exact exceptions.
//! * [`wrapper`] — the `phar://` stream and directory wrapper for
//!   `php-streams`.
//! * [`web`] — `Phar::webPhar()` routing, the mime table and
//!   `Phar::mungServer()`.
//! * [`intercept`] — what `Phar::interceptFileFuncs()` replaces.
//! * [`include`] — the compile-file / resolve-path hooks for including
//!   phars directly.
//! * [`info`] — `phpinfo()` rows and INI entries.
//!
//! # Embedding
//!
//! The VM is expected to:
//!
//! * register [`wrapper::PharWrapper`] as the `phar` stream wrapper
//!   (`php_streams::wrapper::register_global`);
//! * forward `phar.readonly` / `phar.require_hash` to
//!   [`registry::set_ini`] (startup and `ini_set()`), `phar.cache_list` to
//!   [`registry::load_cache_list`], and call [`registry::reset_request`] at
//!   request shutdown;
//! * install an [`signature::OpenSslHook`] when ext/openssl is available
//!   and call [`registry::set_compression_support`] to mirror ext/zlib and
//!   ext/bz2 availability;
//! * back `Phar`/`PharData` objects with [`object::PharObject`] and
//!   `PharFileInfo` with [`object::PharFileInfo`] (calling `release()` on
//!   destruction), throw the returned [`error::PharError`]s, construct the
//!   objects described by [`object::Converted`] and the info-class paths
//!   returned by `offset_get()`, and drive `buildFromIterator()` through
//!   [`object::BuildState`];
//! * route `Phar::webPhar()` through [`web::web_phar`] with a
//!   [`web::WebHost`], and consult [`intercept`] from the intercepted file
//!   functions once `Phar::interceptFileFuncs()` was called.
//!
//! Default stub text and on-disk bytes follow php-src master;
//! [`stub::set_flavor`] and [`sys::set_entry_time_override`] exist so the
//! tests can compare byte for byte against a PHP 8.3 build.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod archive;
mod bz2;
pub mod compress;
pub mod consts;
pub mod error;
pub mod include;
pub mod info;
pub mod intercept;
pub mod object;
pub mod omap;
pub mod phar_format;
pub mod registry;
pub mod signature;
pub mod stub;
pub mod sys;
pub mod tar_format;
pub mod util;
pub mod web;
pub mod wrapper;
pub mod zip_format;
