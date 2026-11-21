// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Environment and process information: `getenv`/`putenv`, `php_uname`,
//! load average, rusage, process ids, script page info, `ftok`, the time
//! and sleep functions, connection state, `getopt` and `get_cfg_var`.

use crate::ctx::{global, RtExt};
use php_runtime::{Value, ZArray, ZStr};
use php_stdmisc::system::{self, EnvState, PageInfo};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;
use std::cell::RefCell;

thread_local! {
    static ENV: RefCell<EnvState> = RefCell::new(EnvState::default());
}

fn getenv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.opt(0).map(|_| a.str(0));
    let local_only = a.has(1) && a.bool(1);
    system::getenv(name.as_ref().map(ZStr::as_bytes), local_only, None).vm(vm)
}

fn putenv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let setting = a.str(0);
    ENV.with(|e| e.borrow_mut().putenv(setting.as_bytes()))
        .vm(vm)
        .map(Value::Bool)
}

fn php_uname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = if a.has(0) { a.str(0) } else { ZStr::from("a") };
    system::php_uname(mode.as_bytes()).vm(vm).map(Value::Str)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn sys_getloadavg(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(system::sys_getloadavg())
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getrusage(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(system::getrusage(if a.has(0) { a.int(0) } else { 0 }))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getmypid(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(system::getmypid()))
}

/// `php_statpage()` for the main script.
fn page_info(vm: &Vm) -> PageInfo {
    let path = vm.script_path.as_bytes();
    let real = !path.is_empty() && path != b"Standard input code" && path != b"Command line code";
    PageInfo::for_script(real.then_some(path))
}

/// The login name of `uid` from the passwd database (`getpwuid()`; files
/// backend).
fn user_name(uid: i64) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _pw = f.next()?;
        (f.next()?.parse::<i64>().ok()? == uid).then(|| name.to_string())
    })
}

/// `get_current_user()`: the owner of the main script (`""` when there is
/// none, e.g. `php -r`, it cannot be stat'ed, or the uid has no passwd
/// entry).
#[allow(clippy::unnecessary_wraps)] // native handler signature
fn get_current_user(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let path = vm.script_path.as_bytes();
    let real = !path.is_empty() && path != b"Standard input code" && path != b"Command line code";
    if !real {
        return Ok(Value::from(""));
    }
    // `sapi_get_stat()` failed (e.g. the script was deleted): no user.
    let page = page_info(vm);
    if page.inode < 0 {
        return Ok(Value::from(""));
    }
    let name = user_name(page.uid).unwrap_or_default();
    Ok(Value::from(name))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getmyuid(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(page_info(vm).getmyuid())
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getmygid(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(page_info(vm).getmygid())
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getmyinode(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(page_info(vm).getmyinode())
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getlastmod(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(page_info(vm).getlastmod())
}

fn ftok(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (file, project) = (a.str(0), a.str(1));
    system::ftok(file.as_bytes(), project.as_bytes(), vm)
        .vm(vm)
        .map(Value::Int)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn microtime(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(system::microtime(a.has(0) && a.bool(0)))
}

/// The local UTC offset (seconds) and DST flag at `ts`, from the date
/// extension's `date('Z')`/`date('I')`.
fn tz_offset(vm: &mut Vm, ts: i64) -> VmResult<(i64, bool)> {
    let r = vm.call_value(
        &Value::from("date"),
        vec![Value::from("Z I"), Value::Int(ts)],
    )?;
    let s = vm.value_to_string(&r)?;
    let s = String::from_utf8_lossy(s.as_bytes()).into_owned();
    let mut it = s.split(' ');
    let off = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let dst = it.next() == Some("1");
    Ok((off, dst))
}

fn gettimeofday(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let as_float = a.has(0) && a.bool(0);
    if as_float {
        return Ok(system::gettimeofday(true, |_| (0, false)));
    }
    let now = system::microtime(true);
    #[allow(clippy::cast_possible_truncation)]
    let ts = match now {
        Value::Float(f) => f as i64,
        _ => 0,
    };
    let (off, dst) = tz_offset(vm, ts)?;
    Ok(system::gettimeofday(false, |_| (off, dst)))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn hrtime(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(system::hrtime(a.has(0) && a.bool(0)))
}

fn sleep(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.flush_stdout();
    system::sleep(a.int(0)).vm(vm).map(Value::Int)
}

fn usleep(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.flush_stdout();
    system::usleep(a.int(0)).vm(vm).map(|()| Value::Null)
}

fn time_nanosleep(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.flush_stdout();
    system::time_nanosleep(a.int(0), a.int(1)).vm(vm)
}

fn time_sleep_until(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.flush_stdout();
    system::time_sleep_until(a.float(0), vm)
        .vm(vm)
        .map(Value::Bool)
}

/// `connection_aborted()`: `PG(connection_status) & PHP_CONNECTION_ABORTED`.
#[allow(clippy::unnecessary_wraps)] // native handler signature
fn connection_aborted(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(
        vm.connection_status() & php_vm::connection::ABORTED != 0,
    ))
}

/// `connection_status()`: `PG(connection_status)`.
#[allow(clippy::unnecessary_wraps)] // native handler signature
fn connection_status(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(vm.connection_status()))
}

/// `ignore_user_abort($enable)`: returns the previous setting (`bool` in
/// php-src master: `RETURN_BOOL(old_setting)`), updating the
/// `ignore_user_abort` INI entry, which is `PG(ignore_user_abort)`.
#[allow(clippy::unnecessary_wraps)] // native handler signature
fn ignore_user_abort(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let old = vm.ini_bool("ignore_user_abort");
    if let Some(v) = a.opt(0) {
        let enable = matches!(v, Value::Bool(true));
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        let _ = vm.ini.ini_set(
            "ignore_user_abort",
            if enable { b"1" } else { b"0" },
            &mut sink,
        );
    }
    Ok(Value::Bool(old))
}

fn getopt(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let short = a.str(0);
    let long: Option<ZArray> = a.has(1).then(|| a.array(1));
    let server_argv = match global(vm, b"_SERVER") {
        Some(Value::Array(s)) => s.get_str(b"argv").map(Value::deref),
        _ => None,
    };
    let argv = server_argv.or_else(|| global(vm, b"argv"));
    let (result, rest_index) =
        php_stdmisc::getopt::getopt(short.as_bytes(), long.as_ref(), argv.as_ref(), vm).vm(vm)?;
    if a.has(2) {
        a.set_ref(2, Value::Int(rest_index));
    }
    Ok(result)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn get_cfg_var(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.str(0);
    Ok(vm
        .configuration_value(name.as_bytes())
        .unwrap_or(Value::Bool(false)))
}

/// Registers the functions and constants.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, v) in [
        ("CONNECTION_ABORTED", php_vm::connection::ABORTED),
        ("CONNECTION_NORMAL", php_vm::connection::NORMAL),
        ("CONNECTION_TIMEOUT", php_vm::connection::TIMEOUT),
    ] {
        m.constant(name, Value::Int(v));
    }
    m.function_static(
        "getenv(?string $name = null, bool $local_only = false): string|array|false",
        getenv,
    )
    .function_static("putenv(string $assignment): bool", putenv)
    .function_static("php_uname(string $mode = \"a\"): string", php_uname)
    .function_static("sys_getloadavg(): array|false", sys_getloadavg)
    .function_static("getrusage(int $mode = 0): array|false", getrusage)
    .function_static("getmypid(): int|false", getmypid)
    .function_static("getmyuid(): int|false", getmyuid)
    .function_static("get_current_user(): string", get_current_user)
    .function_static("getmygid(): int|false", getmygid)
    .function_static("getmyinode(): int|false", getmyinode)
    .function_static("getlastmod(): int|false", getlastmod)
    .function_static("ftok(string $filename, string $project_id): int", ftok)
    .function_static("microtime(bool $as_float = false): string|float", microtime)
    .function_static(
        "gettimeofday(bool $as_float = false): array|float",
        gettimeofday,
    )
    .function_static(
        "hrtime(bool $as_number = false): array|int|float|false",
        hrtime,
    )
    .function_static("sleep(int $seconds): int", sleep)
    .function_static("usleep(int $microseconds): void", usleep)
    .function_static(
        "time_nanosleep(int $seconds, int $nanoseconds): array|bool",
        time_nanosleep,
    )
    .function_static("time_sleep_until(float $timestamp): bool", time_sleep_until)
    .function_static("connection_aborted(): bool", connection_aborted)
    .function_static("connection_status(): int", connection_status)
    .function_static(
        "ignore_user_abort(?bool $enable = null): bool",
        ignore_user_abort,
    )
    .function_static(
        "getopt(string $short_options, array $long_options = [], &$rest_index = null): array|false",
        getopt,
    )
    .function_static(
        "get_cfg_var(string $option): string|array|false",
        get_cfg_var,
    )
    .on_request_shutdown(|_| ENV.with(|e| e.borrow_mut().request_shutdown()));
}
