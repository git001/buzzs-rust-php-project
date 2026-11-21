// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The SAPI's PHP functions: `ext/standard/head.c` (header, cookies,
//! response code) and the SAPI-dependent parts of `basic_functions.c`
//! (flush, connection status, uploads, time limit) bound to the Apache
//! request, plus `sapi/apache2handler/php_functions.c` (`apache_*`,
//! `getallheaders`, `virtual`).

use crate::backend::{Backend, LookupValue, Table, TableOp};
use crate::cookie::{Cookie, CookieError};
use crate::headers::{HeaderError, HeaderOp};
use crate::multipart::upload_err;
use crate::request::{php_header, sapi_flush, with_ctx, RequestCtx};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_support::ini::{Access, Stage};
use php_vm::ext::{Args, Extension, ModuleBuilder, NativeFn};
use php_vm::{Vm, VmResult};
use std::cell::Cell;
use std::os::unix::ffi::OsStrExt;

/// Internal shutdown function applying the fatal-error 500 status.
pub const FATAL_CHECK: &str = "__apache2handler_fatal_status";

/// `E_WARNING`.
const E_WARNING: i64 = 2;

/// The `UPLOAD_ERR_*` constants of `main/main.stub.php`, in its order:
/// main's own constants, which every SAPI has.
const UPLOAD_ERR_CONSTANTS: &[(&str, i64)] = &[
    ("UPLOAD_ERR_OK", upload_err::OK),
    ("UPLOAD_ERR_INI_SIZE", upload_err::INI_SIZE),
    ("UPLOAD_ERR_FORM_SIZE", upload_err::FORM_SIZE),
    ("UPLOAD_ERR_PARTIAL", upload_err::PARTIAL),
    ("UPLOAD_ERR_NO_FILE", upload_err::NO_FILE),
    ("UPLOAD_ERR_NO_TMP_DIR", upload_err::NO_TMP_DIR),
    ("UPLOAD_ERR_CANT_WRITE", upload_err::CANT_WRITE),
    ("UPLOAD_ERR_EXTENSION", upload_err::EXTENSION),
];

thread_local! {
    static ORIG_INI_SET: Cell<Option<NativeFn>> = const { Cell::new(None) };
    static ORIG_INI_RESTORE: Cell<Option<NativeFn>> = const { Cell::new(None) };
}

/// The `apache2handler` module: SAPI functions and INI entries.
#[derive(Debug, Clone, Copy, Default)]
pub struct Apache2Handler;

fn str_value(s: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(s.as_ref()))
}

/// Runs `f` with the request context; outside a request (should not
/// happen) returns `default`.
fn ctx_or<R>(default: R, f: impl FnOnce(&RequestCtx) -> R) -> R {
    with_ctx(f).unwrap_or(default)
}

/// Where output started (`php_output_get_start_filename/lineno`), as
/// `file:line`, once the output layer recorded it.
fn output_start(vm: &Vm) -> Option<Vec<u8>> {
    let (file, line) = vm.output_started()?;
    let mut at = file.as_bytes().to_vec();
    at.extend_from_slice(format!(":{line}").as_bytes());
    Some(at)
}

/// The `sapi_header_op()` warning for an operation after the headers went
/// out (`sapi_module.sapi_error`, no function prefix).
fn already_sent_warning(vm: &mut Vm) -> VmResult<()> {
    let mut msg = HeaderError::AlreadySent.message().as_bytes().to_vec();
    if let Some(at) = output_start(vm) {
        msg.extend_from_slice(b" by (output started at ");
        msg.extend_from_slice(&at);
        msg.push(b')');
    }
    vm.raise_bytes(E_WARNING, &msg)
}

fn header_result(vm: &mut Vm, r: Result<bool, HeaderError>) -> VmResult<bool> {
    match r {
        Ok(ok) => Ok(ok),
        Err(HeaderError::AlreadySent) => {
            already_sent_warning(vm)?;
            Ok(false)
        }
        Err(e) => {
            vm.raise(E_WARNING, e.message())?;
            Ok(false)
        }
    }
}

fn header(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let line = a.str(0);
    let replace = if a.has(1) { a.bool(1) } else { true };
    let code = if a.has(2) { a.int(2) } else { 0 };
    add_response_header(vm, line.as_bytes(), replace, code)?;
    Ok(Value::Null)
}

fn add_response_header(vm: &mut Vm, line: &[u8], replace: bool, code: i64) -> VmResult<bool> {
    let op = if replace {
        HeaderOp::Replace(line, code)
    } else {
        HeaderOp::Add(line, code)
    };
    let r = ctx_or(Ok(false), |c| c.header_op(op));
    let accepted = header_result(vm, r)?;
    if accepted {
        vm.accepted_response_header(line)?;
    }
    Ok(accepted)
}

fn header_remove(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.opt(0).map(|_| a.str(0));
    let r = ctx_or(Ok(false), |c| match &name {
        None => c.header_op(HeaderOp::DeleteAll),
        Some(n) => c.header_op(HeaderOp::Delete(n.as_bytes())),
    });
    header_result(vm, r)?;
    Ok(Value::Null)
}

/// `headers_sent(&$filename, &$line)`: `SG(headers_sent)` plus where
/// output started (empty / 0 when the headers went out without output,
/// e.g. through `flush()`).
fn headers_sent(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let sent = ctx_or(false, RequestCtx::headers_sent);
    with_ctx(|c| {
        let h = c.headers.borrow();
        vm.set_response_mime_state(h.mimetype.clone(), h.send_default_content_type);
    });
    let (file, line) = if sent {
        vm.output_started().unwrap_or_else(|| (ZStr::empty(), 0))
    } else {
        (ZStr::empty(), 0)
    };
    // `ZEND_TRY_ASSIGN_REF_STRING` / `_LONG`: a destructor exception of a
    // replaced value surfaces once both outputs are assigned.
    let mut thrown = Ok(());
    if a.has(0) {
        thrown = vm.try_assign_ref_arg(a, 0, Value::Str(file));
    }
    if a.has(1) {
        let r = vm.try_assign_ref_arg(a, 1, Value::Int(i64::from(line)));
        if thrown.is_ok() {
            thrown = r;
        }
    }
    thrown?;
    Ok(Value::Bool(sent))
}

#[allow(clippy::unnecessary_wraps)]
fn headers_list(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let list = ctx_or(Vec::new(), |c| c.headers.borrow().headers.clone());
    Ok(Value::Array(ZArray::from_values(
        list.into_iter().map(str_value),
    )))
}

fn http_response_code(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let code = if a.has(0) { a.int(0) } else { 0 };
    let Some((sent, current, has_line)) = with_ctx(|c| {
        let h = c.headers.borrow();
        (h.sent, h.response_code, h.status_line.is_some())
    }) else {
        return Ok(Value::Bool(false));
    };
    if code != 0 {
        if sent {
            let mut msg = b"Cannot set response code - headers already sent".to_vec();
            if let Some(at) = output_start(vm) {
                msg.extend_from_slice(b" (output started at ");
                msg.extend_from_slice(&at);
                msg.push(b')');
            }
            vm.php_warning_bytes(&msg)?;
            return Ok(Value::Bool(false));
        }
        if has_line {
            vm.php_warning("Calling http_response_code() after header('HTTP/...') has no effect")?;
        }
        let code = i32::try_from(code).unwrap_or(if code < 0 { i32::MIN } else { i32::MAX });
        with_ctx(|c| c.headers.borrow_mut().response_code = code);
        return Ok(if current != 0 {
            Value::Int(i64::from(current))
        } else {
            Value::Bool(true)
        });
    }
    if current == 0 {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Int(i64::from(current)))
}

/// `header_register_callback()`: kept in `SG(send_header_fcc)` for
/// [`php_header`]; not stored once the headers are out (it would never
/// run).
#[allow(clippy::unnecessary_wraps)]
fn header_register_callback(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    with_ctx(|c| {
        let keep = if c.headers_sent() { None } else { Some(cb) };
        *c.header_callback.borrow_mut() = keep;
    });
    Ok(Value::Bool(true))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn setcookie_common(vm: &mut Vm, a: &mut Args, raw: bool) -> VmResult<Value> {
    let func = a.func_name();
    let mut c = Cookie {
        name: a.str(0).as_bytes().to_vec(),
        value: a.has(1).then(|| a.str(1).as_bytes().to_vec()),
        ..Cookie::default()
    };
    match a.get(2) {
        Some(Value::Array(opts)) => {
            if (3..7).any(|i| a.has(i)) {
                return Err(vm.throw(
                    "ArgumentCountError",
                    &format!(
                        "{func}(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array"
                    ),
                ));
            }
            for (k, v) in opts.iter() {
                let v = v.deref();
                let ArrayKey::Str(k) = k else {
                    return Err(vm.throw(
                        "ValueError",
                        &format!("{func}(): option array cannot have numeric keys"),
                    ));
                };
                let key = k.as_bytes().to_ascii_lowercase();
                match key.as_slice() {
                    b"expires" => c.expires = vm.value_to_int(&v)?,
                    b"path" => c.path = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"domain" => c.domain = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"secure" => c.secure = vm.value_to_bool(&v)?,
                    b"httponly" => c.httponly = vm.value_to_bool(&v)?,
                    b"samesite" => c.samesite = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"partitioned" => c.partitioned = vm.value_to_bool(&v)?,
                    _ => {
                        return Err(vm.throw(
                            "ValueError",
                            &format!("{func}(): option \"{}\" is invalid", k.to_string_lossy()),
                        ))
                    }
                }
            }
        }
        Some(v) => c.expires = vm.value_to_int(&v)?,
        None => {}
    }
    if a.has(3) {
        c.path = Some(a.str(3).as_bytes().to_vec());
    }
    if a.has(4) {
        c.domain = Some(a.str(4).as_bytes().to_vec());
    }
    if a.has(5) {
        c.secure = a.bool(5);
    }
    if a.has(6) {
        c.httponly = a.bool(6);
    }
    let line = match c.header_line(!raw, now()) {
        Ok(l) => l,
        Err(CookieError::ArgumentEmpty(n)) => {
            return Err(a.value_error(vm, n, "must not be empty"))
        }
        Err(CookieError::ArgumentValue(n, msg)) => return Err(a.value_error(vm, n, msg)),
        Err(CookieError::Value(msg)) => {
            return Err(vm.throw("ValueError", &format!("{func}(): {msg}")))
        }
    };
    let r = ctx_or(Ok(false), |ctx| ctx.header_op(HeaderOp::Add(&line, 0)));
    Ok(Value::Bool(header_result(vm, r)?))
}

fn setcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, false)
}

fn setrawcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, true)
}

/// `flush()`: `sapi_flush()` → the headers (with the
/// `header_register_callback()` callable), then `ap_rflush`; a lost client
/// bails out (`php_handle_aborted_connection()`).
fn flush(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    sapi_flush(vm)?;
    Ok(Value::Null)
}

#[allow(clippy::unnecessary_wraps)]
fn php_sapi_name(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(str_value("apache2handler"))
}

/// `connection_aborted()`: `PG(connection_status) & PHP_CONNECTION_ABORTED`
/// — set by a failed write or flush, not by polling the connection.
#[allow(clippy::unnecessary_wraps)]
fn connection_aborted(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(ctx_or(false, |c| c.aborted.get())))
}

/// `connection_status()`: `PG(connection_status)` (`NORMAL`, `ABORTED`,
/// `TIMEOUT` bits).
#[allow(clippy::unnecessary_wraps)]
fn connection_status(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(ctx_or(0, RequestCtx::connection_status)))
}

/// `ignore_user_abort()`: `zend_alter_ini_entry_chars(..., PHP_INI_USER,
/// PHP_INI_STAGE_RUNTIME)`, returning the old setting.
#[allow(clippy::unnecessary_wraps)]
fn ignore_user_abort(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let old = vm.ini_bool("ignore_user_abort");
    if let Some(v) = a.opt(0) {
        let on = matches!(v, Value::Bool(true));
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        let _ = vm.ini.alter(
            "ignore_user_abort",
            if on { b"1" } else { b"0" },
            Access::USER,
            Stage::Runtime,
            false,
            &mut sink,
        );
    }
    Ok(Value::Bool(old))
}

/// Re-reads the SAPI's cached INI values after a runtime change (the
/// `max_execution_time` timer is restarted by the VM through its timer
/// hook, `OnUpdateTimeout` → `zend_set_timeout`).
fn after_ini_change(vm: &mut Vm) {
    with_ctx(|c| c.refresh_facts(vm));
}

fn ini_set(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(orig) = ORIG_INI_SET.with(Cell::get) else {
        return Ok(Value::Bool(false));
    };
    let r = orig(vm, a)?;
    after_ini_change(vm);
    Ok(r)
}

fn ini_restore(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(orig) = ORIG_INI_RESTORE.with(Cell::get) else {
        return Ok(Value::Null);
    };
    let r = orig(vm, a)?;
    after_ini_change(vm);
    Ok(r)
}

/// `php_apache_sapi_log_message()`: the PHP error log (while `error_log`
/// is unset) to httpd's error log, for the request when one runs
/// (`ap_log_rerror`), else for the server.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApacheLog;

impl php_sapi::SapiModule for ApacheLog {
    fn log_message(&self, message: &[u8], severity: i32) {
        let level = crate::backend::LogLevel::from_syslog(severity);
        if with_ctx(|c| c.backend.log(level, message)).is_none() {
            crate::ffi::log_server(crate::backend::LogLevel::Err, message);
        }
    }
}

/// `set_time_limit()`: `zend_alter_ini_entry_chars_ex("max_execution_time",
/// ..., PHP_INI_USER, PHP_INI_STAGE_RUNTIME)`; `false` when the entry is
/// locked (`php_admin_value`).  On success the entry's `OnUpdateTimeout`
/// restarts the timer: [`Vm::set_time_limit`] through the SAPI's timer
/// hook.
#[allow(clippy::unnecessary_wraps)]
fn set_time_limit(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let secs = a.int(0);
    let mut sink: Vec<php_support::Diagnostic> = Vec::new();
    let ok = vm.ini.alter(
        "max_execution_time",
        secs.to_string().as_bytes(),
        Access::USER,
        Stage::Runtime,
        false,
        &mut sink,
    );
    if ok {
        vm.set_time_limit(secs);
    }
    Ok(Value::Bool(ok))
}

fn table_array(b: &dyn Backend, t: Table) -> Value {
    let mut arr = ZArray::new();
    let h = arr.make_mut();
    for (k, v) in b.table_entries(t) {
        h.insert(ArrayKey::from_bytes(&k), str_value(v));
    }
    Value::Array(arr)
}

#[allow(clippy::unnecessary_wraps)]
fn apache_request_headers(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(ctx_or(Value::Array(ZArray::new()), |c| {
        table_array(&*c.backend, Table::HeadersIn)
    }))
}

#[allow(clippy::unnecessary_wraps)]
fn apache_response_headers(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(ctx_or(Value::Array(ZArray::new()), |c| {
        table_array(&*c.backend, Table::HeadersOut)
    }))
}

#[allow(clippy::unnecessary_wraps)]
fn apache_note(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.str(0);
    let value = a.opt(1).map(|_| a.str(1));
    Ok(ctx_or(Value::Bool(false), |c| {
        let old = c.backend.table_get(Table::Notes, name.as_bytes());
        if let Some(v) = &value {
            c.backend
                .table_op(Table::Notes, TableOp::Set, name.as_bytes(), v.as_bytes());
        }
        old.map_or(Value::Bool(false), str_value)
    }))
}

fn with_target<R>(c: &RequestCtx, walk_to_top: bool, f: impl FnOnce(&dyn Backend) -> R) -> R {
    if walk_to_top {
        if let Some(top) = c.backend.top_request() {
            return f(&*top);
        }
    }
    f(&*c.backend)
}

#[allow(clippy::unnecessary_wraps)]
fn apache_setenv(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let var = a.str(0);
    let val = a.str(1);
    let top = a.has(2) && a.bool(2);
    with_ctx(|c| {
        with_target(c, top, |b| {
            b.table_op(
                Table::SubprocessEnv,
                TableOp::Set,
                var.as_bytes(),
                val.as_bytes(),
            );
        });
    });
    Ok(Value::Bool(true))
}

#[allow(clippy::unnecessary_wraps)]
fn apache_getenv(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let var = a.str(0);
    let top = a.has(1) && a.bool(1);
    Ok(ctx_or(Value::Bool(false), |c| {
        with_target(c, top, |b| {
            b.table_get(Table::SubprocessEnv, var.as_bytes())
        })
        .map_or(Value::Bool(false), str_value)
    }))
}

#[allow(clippy::unnecessary_wraps)]
fn apache_get_version(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let banner = ctx_or(Vec::new(), |c| c.backend.server_banner());
    Ok(if banner.is_empty() {
        Value::Bool(false)
    } else {
        str_value(banner)
    })
}

#[allow(clippy::unnecessary_wraps)]
fn apache_get_modules(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let mods = ctx_or(Vec::new(), |c| c.backend.loaded_modules());
    Ok(Value::Array(ZArray::from_values(mods.into_iter().map(
        |m| {
            let end = m.iter().position(|&c| c == b'.').unwrap_or(m.len());
            str_value(&m[..end])
        },
    ))))
}

fn lookup_warning(vm: &mut Vm, what: &str, uri: &ZStr) -> VmResult<()> {
    let mut msg = b"Unable to include '".to_vec();
    msg.extend_from_slice(uri.as_bytes());
    msg.extend_from_slice(b"' - ");
    msg.extend_from_slice(what.as_bytes());
    vm.php_warning_bytes(&msg)
}

fn apache_lookup_uri(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let uri = a.str(0);
    let Some(rr) = with_ctx(|c| c.backend.lookup_uri(uri.as_bytes())).flatten() else {
        lookup_warning(vm, "URI lookup failed", &uri)?;
        return Ok(Value::Bool(false));
    };
    if rr.status() != 200 {
        drop(rr);
        lookup_warning(vm, "error finding URI", &uri)?;
        return Ok(Value::Bool(false));
    }
    let info = rr.info();
    drop(rr);
    let Some(cls) = vm.classes.get(b"stdclass".as_slice()).cloned() else {
        return Ok(Value::Bool(false));
    };
    let o = vm.new_object(&cls)?;
    for (name, v) in info {
        let v = match v {
            LookupValue::Int(i) => Value::Int(i),
            LookupValue::Str(s) => str_value(s),
        };
        vm.set_property_raw(&o, name, v);
    }
    Ok(Value::Object(o))
}

fn virtual_(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let uri = a.str(0);
    let Some(rr) = with_ctx(|c| c.backend.lookup_uri(uri.as_bytes())).flatten() else {
        lookup_warning(vm, "URI lookup failed", &uri)?;
        return Ok(Value::Bool(false));
    };
    if rr.status() != 200 {
        drop(rr);
        lookup_warning(vm, "error finding URI", &uri)?;
        return Ok(Value::Bool(false));
    }
    // php_output_end_all(); php_header();
    php_support::output::end_all(vm);
    php_header(vm);
    if rr.run() != 0 {
        drop(rr);
        lookup_warning(vm, "request execution failed", &uri)?;
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(true))
}

#[allow(clippy::unnecessary_wraps)]
fn fatal_status(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    crate::request::apply_fatal_status(vm);
    Ok(Value::Null)
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_loaded_file(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(crate::config::current()
        .loaded_file
        .as_ref()
        .map_or(Value::Bool(false), |p| str_value(p.as_os_str().as_bytes())))
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_scanned_files(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let cfg = crate::config::current();
    if cfg.scanned_files.is_empty() {
        return Ok(Value::Bool(false));
    }
    let mut s = Vec::new();
    for f in &cfg.scanned_files {
        s.extend_from_slice(f.as_os_str().as_bytes());
        s.extend_from_slice(b",\n");
    }
    Ok(str_value(s))
}

fn shutdown_hook(vm: &mut Vm) {
    crate::request::apply_fatal_status(vm);
}

impl Extension for Apache2Handler {
    fn name(&self) -> &'static str {
        "apache2handler"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        use php_support::ini::{IniDirective, ModifyHandler};
        {
            // The SAPI's `sapi_module` entries the VM calls back into:
            // header_handler, send_headers (php_header), flush, the
            // execution timer, the aborted-connection bailout the output
            // writer cannot raise itself, and the body parse of
            // request_parse_body().
            let vm = m.vm();
            vm.response_header_hook = Some(add_response_header);
            vm.send_headers_hook = Some(php_header);
            vm.sapi_flush_hook = Some(crate::request::implicit_flush);
            vm.timer_hook = Some(crate::request::arm_timer);
            vm.interrupt_handlers.push(crate::request::abort_interrupt);
            vm.parse_body_hook = Some(crate::vars::parse_body);
        }
        // PHP_INI_BEGIN() of php_functions.c.
        for (name, default) in [("xbithack", "0"), ("engine", "1"), ("last_modified", "0")] {
            m.ini(IniDirective::new(
                name,
                Some(default),
                Access::ALL,
                ModifyHandler::Bool,
                "apache2handler",
            ));
        }
        {
            let vm = m.vm();
            for c in ["STDIN", "STDOUT", "STDERR"] {
                vm.constants.remove(c.as_bytes());
            }
            // Defined unattributed (not through `m.constant`), so that
            // `get_defined_constants(true)` files them under `Core`, like
            // the other engine constants.
            for (name, value) in UPLOAD_ERR_CONSTANTS {
                vm.constants.insert(ZStr::from(*name), Value::Int(*value));
            }
            if let Some(b) = vm.native_function("ini_set") {
                ORIG_INI_SET.with(|c| c.set(Some(b.handler)));
            }
            if let Some(b) = vm.native_function("ini_restore") {
                ORIG_INI_RESTORE.with(|c| c.set(Some(b.handler)));
            }
            vm.set_sapi_module(std::rc::Rc::new(ApacheLog));
        }
        m.constant("PHP_SAPI", str_value("apache2handler"))
            .function_static("header(string $header, bool $replace = true, int $response_code = 0): void", header)
            .function_static("header_remove(?string $name = null): void", header_remove)
            .function_static("headers_sent(&$filename = null, &$line = null): bool", headers_sent)
            .function_static("headers_list(): array", headers_list)
            .function_static("http_response_code(int $response_code = 0): int|bool", http_response_code)
            .function_static("header_register_callback(callable $callback): true", header_register_callback)
            .function_static(
                "setcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
                setcookie,
            )
            .function_static(
                "setrawcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
                setrawcookie,
            )
            .function_static("flush(): void", flush)
            .function_static("php_sapi_name(): string|false", php_sapi_name)
            .function_static("connection_aborted(): bool", connection_aborted)
            .function_static("connection_status(): int", connection_status)
            .function_static("ignore_user_abort(?bool $enable = null): bool", ignore_user_abort)
            .function_static("set_time_limit(int $seconds): bool", set_time_limit)
            .function_static(
                "ini_set(string $option, string|int|float|bool|null $value): string|false",
                ini_set,
            )
            .function_static(
                "ini_alter(string $option, string|int|float|bool|null $value): string|false",
                ini_set,
            )
            .function_static("ini_restore(string $option): void", ini_restore)
            .function_static("php_ini_loaded_file(): string|false", php_ini_loaded_file)
            .function_static("php_ini_scanned_files(): string|false", php_ini_scanned_files)
            .function_static("apache_lookup_uri(string $filename): object|false", apache_lookup_uri)
            .function_static("virtual(string $uri): bool", virtual_)
            .function_static("apache_request_headers(): array", apache_request_headers)
            .function_static("getallheaders(): array", apache_request_headers)
            .function_static("apache_response_headers(): array", apache_response_headers)
            .function_static(
                "apache_note(string $note_name, ?string $note_value = null): string|false",
                apache_note,
            )
            .function_static(
                "apache_setenv(string $variable, string $value, bool $walk_to_top = false): bool",
                apache_setenv,
            )
            .function_static(
                "apache_getenv(string $variable, bool $walk_to_top = false): string|false",
                apache_getenv,
            )
            .function_static("apache_get_version(): string|false", apache_get_version)
            .function_static("apache_get_modules(): array", apache_get_modules)
            .function(&format!("{FATAL_CHECK}(): void"), fatal_status)
            .on_request_shutdown(shutdown_hook);
    }
}
