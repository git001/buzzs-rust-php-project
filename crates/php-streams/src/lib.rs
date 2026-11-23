// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The PHP streams layer for phprust.
//!
//! A port of `main/streams/` and the file-related parts of `ext/standard`
//! (`file.c`, `filestat.c`, `dir.c`, `link.c`, `filters.c`, `user_filters.c`,
//! `streamsfuncs.c`, `php_fopen_wrapper.c`) plus `main/fopen_wrappers.c`,
//! `main/php_open_temporary_file.c` and `main/php_glob.c`.
//!
//! The crate does not depend on the VM's value model: PHP values crossing the
//! boundary (context options, filter parameters, user wrapper arguments and
//! return values) use the small [`value::Value`] type, user-space wrappers and
//! filters are traits the VM implements, and warnings are reported through
//! [`diag`] (without the `function(): ` prefix, which the VM adds).
//!
//! # Map
//!
//! * [`stream::Stream`] — `php_stream`: read buffer with chunk-size
//!   semantics, `fgets`/`stream_get_line`, write-through writes, seeking
//!   (including emulation), filter chains, meta data, locking, casting.
//!   [`stream::StreamOps`] is the `php_stream_ops` trait.
//! * [`open`] — `php_stream_open_wrapper_ex`, `opendir`, `scandir`, path
//!   operations through wrappers, "Failed to open stream: ..." reporting.
//! * [`wrapper`] — the [`wrapper::Wrapper`] trait, the global/volatile
//!   registries (wrappers and filters) and `php_stream_locate_url_wrapper`.
//! * Wrappers: [`plain`] (files, directories, STDIO ops), [`php_wrapper`]
//!   (`php://`), [`memory`] (memory/temp streams and `data:`),
//!   [`glob_wrapper`] (`glob://`), [`user`] (`stream_wrapper_register`),
//!   [`transports`] (`stream_socket_client` for tcp/udp/unix),
//!   [`http`] (`http://` / `https://`), [`ftp`] (`ftp://` / `ftps://`).
//! * [`filter`] — filter framework, bundled filters and `php_user_filter`.
//! * [`file`](mod@file) / [`filestat`] — the PHP functions' core logic (argument
//!   `ValueError`s are returned as [`file::ArgError`]).
//! * [`paths`], [`pathname`], [`temp`], [`fnmatch`], [`glob`] — path
//!   resolution, `open_basedir`, `basename`/`dirname`/`pathinfo`, temporary
//!   files, pattern matching.
//! * [`state`] — request globals: INI settings, VM hooks (output, request
//!   body, executing file), stat cache.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod builtin;
pub mod context;
pub mod diag;
pub mod file;
pub mod filestat;
pub mod filter;
pub mod fnmatch;
pub mod ftp;
pub mod glob;
pub mod glob_wrapper;
pub mod http;
pub mod memory;
pub mod mode;
pub mod open;
pub mod pathname;
pub mod paths;
pub mod php_wrapper;
pub mod plain;
pub mod poll;
pub mod sockopts;
pub mod stat;
pub mod state;
pub mod stream;
pub mod sys;
pub mod temp;
pub mod transports;
pub mod user;
pub mod util;
pub mod value;
pub mod vcwd;
pub mod wrapper;

#[cfg(test)]
mod test_util;
