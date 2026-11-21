// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The remaining "misc" of PHP's `ext/standard` that is not covered by the
//! string/array/stream crates (php-src master semantics, verified against
//! `/usr/bin/php` with oracle fixtures in `tests/fixtures`).
//!
//! * [`browscap`] — `get_browser()` (browscap.ini matching).
//! * [`crypt`] — `crypt()` with every bundled algorithm (std/ext DES, MD5,
//!   SHA-256/512, bcrypt incl. `$2a$`/`$2x$` bug handling) and the
//!   `password_*` API (bcrypt, pure-Rust Argon2i/Argon2id).
//! * [`exec`] — `escapeshellarg`/`escapeshellcmd`, `exec`/`system`/
//!   `passthru`/`shell_exec`, `proc_nice`.
//! * [`process`] — `proc_open`/`proc_close`/`proc_get_status`/
//!   `proc_terminate` (`posix_spawn`; pipes handed back as descriptors).
//! * [`net`] — `inet_*`, `ip2long`/`long2ip`, host/service/protocol
//!   lookups, `net_get_interfaces`, `openlog`/`syslog`/`closelog`.
//! * [`dns`] — `dns_get_record` (wire-format parser), `dns_get_mx`,
//!   `dns_check_record` over a pluggable [`dns::Resolver`].
//! * [`system`] — `getenv`/`putenv`, `php_uname`, `getrusage`, the sleep
//!   family, `microtime`/`hrtime`, `getmy*`, `ftok`, connection state.
//! * [`info`] — `phpinfo()` rendering primitives and layout.
//! * [`query`] — `http_build_query`, `parse_str` and the SAPI's request
//!   variable registration (`php_register_variable_ex`, `treat_data`).
//! * [`datetime`] — `strptime()`.
//! * [`getopt`] — `getopt()`.
//! * [`highlight`] — `highlight_string`/`highlight_file` HTML and
//!   `php_strip_whitespace`.
//! * [`locale`] — `setlocale`, `localeconv`, `nl_langinfo`.
//! * [`mail`] — `mail()` and `error_log()`.
//! * [`misc`] — `assert_options`, ini-file info, `get_cfg_var`.
//!
//! # Calling convention
//!
//! The VM performs argument parsing and passes Rust values; functions that
//! can emit diagnostics take a [`php_runtime::RuntimeContext`] (warnings
//! are reported with PHP's `func(): ` prefix) and thrown errors come back
//! as `Err(RuntimeError)` with PHP's exact messages. By-reference outputs
//! (`$output`, `$pipes`, `$rest_index`, ...) are returned as values.
//! Request-scoped state lives in small structs the VM keeps per request
//! ([`system::EnvState`], [`locale::LocaleState`], [`net::SyslogState`],
//! [`misc::AssertState`]). The connection state is the VM's
//! (`php_vm::connection`).
//!
//! All `unsafe` code is confined to [`sys`].

pub mod browscap;
pub mod crypt;
pub mod datetime;
pub mod dns;
pub mod exec;
pub mod getopt;
pub mod highlight;
pub mod info;
pub mod locale;
pub mod mail;
pub mod misc;
pub mod net;
pub mod process;
pub mod query;
pub mod reaper;
pub mod sys;
pub mod system;
