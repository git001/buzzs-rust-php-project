// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The CLI SAPI's own symbols (`php_cli.c`'s `additional_functions`,
//! `php_cli_process_title.c`), the configuration-file functions backed
//! by the SAPI's `php_init_config()` result, and the CLI request
//! variables (`sapi_cli_register_variables`).

use crate::config::Config;
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;
use std::cell::RefCell;
use std::os::unix::ffi::OsStrExt;

/// `sapi_cli_ub_write()`: `write(2)` straight to file descriptor 1.
///
/// `std::io::stdout()` adds a re-entrant lock and a line buffer
/// (a `memrchr` per write) under the VM's own buffer; under the CLI's
/// `implicit_flush=1` every `echo` paid for both layers. Every failed
/// write is an error, a closed descriptor (`fclose(STDOUT)`, `EBADF`)
/// too: `sapi_cli_ub_write` then aborts the connection, and PHP 8.6
/// stops the script with exit status 255.
pub struct RawStdout;

impl std::io::Write for RawStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: `buf` is a live slice of `buf.len()` initialised bytes;
        // write(2) only reads it.
        let n = unsafe { libc::write(libc::STDOUT_FILENO, buf.as_ptr().cast(), buf.len()) };
        usize::try_from(n).map_err(|_| std::io::Error::last_os_error())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `php_ini_opened_path` and `php_ini_scanned_files`.
type IniFiles = (Option<Vec<u8>>, Option<Vec<u8>>);

thread_local! {
    static INI_FILES: RefCell<IniFiles> = const { RefCell::new((None, None)) };
}

fn str_value(s: &[u8]) -> Value {
    Value::Str(ZStr::from(s))
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_loaded_file(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(INI_FILES.with(|f| {
        f.borrow()
            .0
            .as_deref()
            .map_or(Value::Bool(false), str_value)
    }))
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_scanned_files(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(INI_FILES.with(|f| {
        f.borrow()
            .1
            .as_deref()
            .map_or(Value::Bool(false), str_value)
    }))
}

fn cli_set_process_title(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let title = a.str(0);
    if crate::proctitle::set(title.as_bytes()) {
        return Ok(Value::Bool(true));
    }
    vm.php_warning("cli_set_process_title had an error: Not initialized correctly")?;
    Ok(Value::Bool(false))
}

fn cli_get_process_title(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    if let Some(t) = crate::proctitle::get() {
        return Ok(str_value(&t));
    }
    vm.php_warning("cli_get_process_title had an error: Not initialized correctly")?;
    Ok(Value::Null)
}

/// The CLI's `sapi_module`: its log on stderr (`sapi_cli_log_message`),
/// its output on standard output, and exit status 255 when that fails
/// (`sapi_cli_ub_write`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CliSapi;

impl php_sapi::SapiModule for CliSapi {
    fn log_message(&self, message: &[u8], severity: i32) {
        php_sapi::StderrLog.log_message(message, severity);
    }

    /// `sapi_cli_ub_write()` writes to file descriptor 1 at once, so a
    /// lost reader (`EPIPE`) or a closed descriptor (`EBADF`) shows in
    /// this write: the buffer in front of [`RawStdout`] is emptied here.
    fn ub_write(&self, out: &mut dyn std::io::Write, data: &[u8]) -> Result<(), php_sapi::Aborted> {
        out.write_all(data)
            .and_then(|()| out.flush())
            .map_err(|_| php_sapi::Aborted)
    }

    fn exit_status_on_abort(&self) -> Option<i32> {
        Some(255)
    }
}

/// Registers the CLI functions and constants on a freshly created VM.
pub fn register(vm: &mut Vm, cfg: &Config) {
    vm.set_sapi_module(std::rc::Rc::new(CliSapi));
    INI_FILES.with(|f| *f.borrow_mut() = (cfg.opened_path.clone(), cfg.scanned_files.clone()));
    vm.register_function("php_ini_loaded_file(): string|false", php_ini_loaded_file);
    vm.register_function(
        "php_ini_scanned_files(): string|false",
        php_ini_scanned_files,
    );
    vm.register_function(
        "cli_set_process_title(string $title): bool",
        cli_set_process_title,
    );
    vm.register_function("cli_get_process_title(): ?string", cli_get_process_title);
    vm.constants.insert(
        ZStr::from("PHP_CLI_PROCESS_TITLE"),
        Value::Bool(crate::proctitle::available()),
    );
    vm.constants
        .entry(ZStr::from("PHP_BUILD_DATE"))
        .or_insert_with(|| Value::Str(ZStr::from(crate::BUILD_DATE)));
}

/// What `sapi_cli_register_variables` needs to know about the request.
pub struct Request<'a> {
    /// `php_self` (the script as given, or `Standard input code`).
    pub php_self: &'a [u8],
    /// `script_filename` (empty for stdin and `-r`).
    pub script_filename: &'a [u8],
    /// `SG(request_info).argv`.
    pub argv: &'a [Vec<u8>],
}

fn env_table() -> HashTable {
    let mut t = HashTable::new();
    for (k, v) in std::env::vars_os() {
        t.insert(ArrayKey::from_bytes(k.as_bytes()), str_value(v.as_bytes()));
    }
    t
}

/// Whether `variables_order` names the track-vars array `c` (`strchr`
/// for either case).
fn variables_order_has(vm: &Vm, c: u8) -> bool {
    vm.ini_string("variables_order")
        .unwrap_or_default()
        .iter()
        .any(|x| x.eq_ignore_ascii_case(&c))
}

/// Sets the global `name` to `v` (`zend_hash_update(&EG(symbol_table))`).
fn set_global_bytes(vm: &Vm, name: &[u8], v: Value) {
    let mut g = vm.globals.borrow_mut();
    let i = g.find_or_add(name) as usize;
    g.slots[i] = Some(v);
}

/// `php_auto_globals_create_env`: `$_ENV` holds the environment when
/// `variables_order` contains `E`.
fn create_env(vm: &mut Vm, name: &[u8]) {
    let env = if variables_order_has(vm, b'E') {
        env_table()
    } else {
        HashTable::new()
    };
    set_global_bytes(vm, name, Value::Array(ZArray::from_table(env)));
}

/// `php_auto_globals_create_request`: the merge of the GET/POST/COOKIE
/// track vars, which the CLI never fills.
fn create_request(vm: &mut Vm, name: &[u8]) {
    set_global_bytes(vm, name, Value::Array(ZArray::new()));
}

/// `php_auto_globals_create_server` with `sapi_cli_register_variables`:
/// the environment, the script names, the request time and `argv`/`argc`
/// (an empty array unless `variables_order` contains `S`).
fn server_array(vm: &Vm, req: &Request<'_>, argv: &Value, argc: &Value) -> Value {
    let mut server = HashTable::new();
    if variables_order_has(vm, b'S') {
        server = env_table();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        for (k, v) in [
            ("PHP_SELF", req.php_self),
            ("SCRIPT_NAME", req.php_self),
            ("SCRIPT_FILENAME", req.script_filename),
            ("PATH_TRANSLATED", req.script_filename),
            ("DOCUMENT_ROOT", b"".as_slice()),
        ] {
            server.insert(ArrayKey::from(k), str_value(v));
        }
        server.insert(
            ArrayKey::from("REQUEST_TIME_FLOAT"),
            Value::Float(now.as_secs_f64()),
        );
        server.insert(
            ArrayKey::from("REQUEST_TIME"),
            Value::Int(i64::try_from(now.as_secs()).unwrap_or(0)),
        );
        server.insert(ArrayKey::from("argv"), argv.clone());
        server.insert(ArrayKey::from("argc"), argc.clone());
    }
    Value::Array(ZArray::from_table(server))
}

/// `php_hash_environment()` for the CLI, in php-src's symbol-table order:
/// `$argv`/`$argc` (`php_build_argv`), then the auto-globals in
/// registration order (`zend_activate_auto_globals`), where the JIT ones
/// (`auto_globals_jit`) are armed instead of created, and finally
/// `$_SERVER`, which `do_cli` always requests (`zend_is_auto_global`).
/// A JIT `$_ENV`/`$_REQUEST` appears only once a compiled script names it.
pub fn register_variables(vm: &mut Vm, req: &Request<'_>) {
    let values: Vec<Value> = req.argv.iter().map(|a| str_value(a)).collect();
    let argc = Value::Int(i64::try_from(values.len()).unwrap_or(0));
    let argv = Value::Array(ZArray::from_values(values));
    set_global_bytes(vm, b"argv", argv.clone());
    set_global_bytes(vm, b"argc", argc.clone());

    let jit = vm.ini_bool("auto_globals_jit");
    // `php_startup_auto_globals` runs before `SG(request_info).argc` is
    // set, so `$_SERVER` is JIT only without `register_argc_argv`.
    let server_jit = jit && !vm.ini_bool("register_argc_argv");
    for name in [b"_GET".as_slice(), b"_POST", b"_COOKIE"] {
        set_global_bytes(vm, name, Value::Array(ZArray::new()));
    }
    if !server_jit {
        set_global_bytes(vm, b"_SERVER", server_array(vm, req, &argv, &argc));
    }
    if jit {
        vm.arm_auto_global(b"_ENV", create_env);
        vm.arm_auto_global(b"_REQUEST", create_request);
    } else {
        create_env(vm, b"_ENV");
        create_request(vm, b"_REQUEST");
    }
    set_global_bytes(vm, b"_FILES", Value::Array(ZArray::new()));
    if server_jit {
        set_global_bytes(vm, b"_SERVER", server_array(vm, req, &argv, &argc));
    }
}

/// Sets a global variable (`$argn`, `$argi`).
pub fn set_global(vm: &mut Vm, name: &str, v: Value) {
    let mut g = vm.globals.borrow_mut();
    let i = g.find_or_add(name.as_bytes()) as usize;
    g.slots[i] = Some(v);
}
