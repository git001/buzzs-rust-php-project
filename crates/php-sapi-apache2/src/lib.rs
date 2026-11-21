// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `apache2handler` SAPI: phprust as an Apache httpd 2.4 module
//! (`mod_php` replacement), built as `libphprust_apache2.so`.
//!
//! * `csrc/mod_phprust.c` — the small C shim declaring `php_module`, the
//!   `php_value`/`php_flag`/`php_admin_value`/`php_admin_flag`/`PHPINIDir`
//!   /`PhpRustAllowThreads` directives and the hooks; it reaches Rust
//!   through [`ffi`].
//! * [`handler`] — `php_handler()`: filtering, the request lock and
//!   thread, the `max_execution_time` watchdog.
//! * [`request`] — one request's lifecycle around the [`php_vm::Vm`].
//! * [`vars`] — `$_GET`/`$_POST`/`$_COOKIE`/`$_FILES`/`$_SERVER`/`$_ENV`/
//!   `$_REQUEST`, with the rfc1867 upload parser of
//!   [`php_sapi_input::rfc1867`].
//! * [`headers`] — `sapi_header_op()` with the Apache header handler;
//!   [`cookie`] — `setcookie()` formatting.
//! * [`functions`] — the PHP functions the SAPI provides (`header()`,
//!   `flush()`, `apache_*()`, ...).
//! * [`config`] — `php.ini` loading and SAPI INI defaults.
//! * [`backend`] — the server abstraction (real: [`ffi::ApacheRequest`],
//!   tests: [`backend::MockBackend`]).
//!
//! See `README.md` for the Apache configuration.

pub mod backend;
pub mod config;
pub mod cookie;
pub mod extensions;
pub mod ffi;
pub mod functions;
pub mod handler;
pub mod headers;
pub mod request;
pub mod vars;
