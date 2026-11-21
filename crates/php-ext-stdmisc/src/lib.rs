// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The "misc" of `ext/standard` for the phprust VM: binds the
//! `php-stdmisc` library through the `php_vm::ext` extension API.
//!
//! * [`browscap`] — `get_browser()`.
//! * [`crypt`] — `crypt()` and `password_*`.
//! * [`exec`] — `escapeshell*`, `exec`/`system`/`passthru`/`shell_exec`,
//!   `proc_nice`.
//! * [`proc`] — `proc_open`/`proc_close`/`proc_get_status`/`proc_terminate`
//!   (pipes as `php-ext-streams` stream resources).
//! * [`head`] — `setcookie`/`setrawcookie` (validation, then the VM's `header()`).
//! * [`net`] — address conversion, host/service/protocol lookups, DNS,
//!   `net_get_interfaces`, syslog.
//! * [`env`] — environment/process information, time and sleep functions,
//!   connection state, `getopt`, `get_cfg_var`.
//! * [`text`] — highlighting, `php_strip_whitespace`, `http_build_query`,
//!   `parse_str`, locale functions.
//! * [`ini`] — `ini_get_all`, `parse_ini_string`/`parse_ini_file`,
//!   `get_included_files`.
//! * [`misc`] — `mail`, `error_log`, `assert_options`, `phpinfo`,
//!   `phpcredits`.
//!
//! Per-request state (locale, `putenv` changes, syslog) lives in
//! thread-locals and is reset by request-shutdown hooks; the assert
//! callback (`ASSERTG(callback)`) lives in the VM's `ext` state.

mod browscap;
mod credits;
mod crypt;
mod ctx;
mod env;
mod exec;
mod head;
mod ini;
mod misc;
mod net;
mod proc;
mod text;

pub use misc::php_mail;
use php_vm::ext::{Extension, ModuleBuilder};

/// The `ext/standard` misc functions (registered under the `standard`
/// extension name).
#[derive(Debug, Clone, Copy, Default)]
pub struct StdMisc;

impl Extension for StdMisc {
    fn name(&self) -> &'static str {
        "standard"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        browscap::register(m);
        crypt::register(m);
        exec::register(m);
        proc::register(m);
        head::register(m);
        net::register(m);
        env::register(m);
        text::register(m);
        misc::register(m);
        ini::register(m);
    }
}
