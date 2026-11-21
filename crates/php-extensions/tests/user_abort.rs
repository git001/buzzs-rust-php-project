// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A client that goes away (`php_handle_aborted_connection`): the SAPI's
//! `ub_write` fails, the request counts as aborted, output stops and,
//! without `ignore_user_abort`, so does the script; its shutdown functions
//! still run. And `connection_status()` after a timeout. Expectations from
//! PHP 8.6 (`php:8.6-fpm`): the CLI on a closed pipe, `fclose(STDOUT)`,
//! and the built-in server with a client that hangs up.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::{Vm, VmOptions};

/// Output to a client that is gone: every write fails.
struct Gone;

impl Write for Gone {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A SAPI whose log the test reads (`error_log()` without `error_log`
/// set goes to the SAPI's log).
#[derive(Default)]
struct Logged(RefCell<Vec<String>>);

impl php_sapi::SapiModule for Logged {
    fn log_message(&self, message: &[u8], _severity: i32) {
        self.0
            .borrow_mut()
            .push(String::from_utf8_lossy(message).into_owned());
    }
}

/// Runs `src` with output to `out`; returns the SAPI log.
fn run(src: &str, ini: &[(&str, &str)], out: Box<dyn Write>) -> Vec<String> {
    let opts = VmOptions {
        ini: ini
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    };
    let mut vm = Vm::with_extensions(&opts, out, php_extensions::ALL);
    let log = Rc::new(Logged::default());
    vm.set_sapi_module(log.clone());
    vm.run_script(src.as_bytes(), "/abort.php");
    drop(vm);
    log.0.take()
}

const SCRIPT: &str = r#"<?php
register_shutdown_function(function () {
    error_log('shutdown status=' . connection_status());
});
echo "first";
error_log('after echo aborted=' . (int) connection_aborted());
"#;

#[test]
fn a_lost_client_stops_the_script() {
    assert_eq!(run(SCRIPT, &[], Box::new(Gone)), ["shutdown status=1"]);
}

#[test]
fn ignore_user_abort_lets_the_script_run_on() {
    assert_eq!(
        run(SCRIPT, &[("ignore_user_abort", "1")], Box::new(Gone)),
        ["after echo aborted=1", "shutdown status=1"]
    );
}

/// `php_on_timeout()`: the timeout bit, with the client still there (a
/// lost client would make the status `ABORTED`, as in php-src).
#[test]
fn the_time_limit_shows_in_connection_status() {
    let src = r"<?php
register_shutdown_function(function () {
    error_log('shutdown status=' . connection_status());
});
set_time_limit(1);
while (true) {}
";
    let log = run(src, &[("log_errors", "0")], Box::new(std::io::sink()));
    assert_eq!(
        log.last().map(String::as_str),
        Some("shutdown status=2"),
        "{log:?}"
    );
}
