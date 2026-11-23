// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/pcntl` for the phprust VM: binds the `php-pcntl` library through
//! the `php_vm::ext` extension API (ref: `ext/pcntl/pcntl.c`).
//!
//! The library owns `PCNTL_G` ([`Pcntl`], generic over the callable type —
//! here a PHP [`Value`]) and every libc call; this crate converts PHP
//! values, maps [`PcntlError`]s to PHP behaviour and runs the PHP signal
//! handlers with `vm.call_value()`.
//!
//! # Signal dispatch
//!
//! Handlers run in `pcntl_signal_dispatch()`, at `declare(ticks=N)` ticks
//! (a VM tick handler) and — with `pcntl_async_signals(true)` — from the
//! VM interrupt handler: every delivery of a signal with a PHP handler
//! raises the VM interrupt (see [`host::chain_interrupt`]), and the
//! interrupt handler dispatches when async signals are on.  Fiber
//! switches are blocked while handlers run, as in PHP.

// Native handlers share one signature (`fn(&mut Vm, &mut Args) ->
// VmResult<Value>`), so infallible ones still return `VmResult`.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps)]

mod host;

use std::cell::RefCell;

use php_pcntl::{
    CpuArg, InfoValue, Pcntl, PcntlError, PcntlResult, ResourceUsage, SigInfo, SignalArg,
    SignalHandler, WaitResult,
};
use php_runtime::numeric::{is_numeric_string_ex, Number};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_vm::ext::{Args, Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

use crate::host::ForkedChild;

thread_local! {
    /// The per-request module globals.
    static STATE: RefCell<Pcntl<Value>> = RefCell::new(Pcntl::new());
}

/// Runs `f` on the module state.
fn with<R>(f: impl FnOnce(&mut Pcntl<Value>) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// The `pcntl` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct PcntlExt;

/// The running function's name (for exception prefixes).
fn fname(a: &Args) -> String {
    a.func
        .as_ref()
        .map_or_else(String::new, |f| f.name.to_string_lossy())
}

/// The exception for a throwing library error.  Argument errors carry the
/// `func(): ` prefix of `zend_argument_*_error()`; other `ValueError`s are
/// plain `zend_value_error()` texts.
fn throw(vm: &mut Vm, a: &Args, e: PcntlError) -> Throw {
    let prefixed = |m: &str| {
        if m.starts_with("Argument #") {
            format!("{}(): {m}", fname(a))
        } else {
            m.to_owned()
        }
    };
    match e {
        PcntlError::ValueError(m) => vm.throw("ValueError", &prefixed(&m)),
        PcntlError::TypeError(m) => vm.throw("TypeError", &prefixed(&m)),
        PcntlError::Error(m) | PcntlError::Warning(m) => vm.throw("Error", &m),
        PcntlError::Fatal(m) => vm.fatal(&m),
        PcntlError::Failed => vm.throw("Error", "pcntl failure"),
    }
}

/// PHP's failure behaviour: `fail` for silent failures and warnings (after
/// emitting it), an exception or fatal error otherwise.
fn failure(vm: &mut Vm, a: &Args, e: PcntlError, fail: Value) -> VmResult<Value> {
    match e {
        PcntlError::Failed => Ok(fail),
        PcntlError::Warning(m) => {
            vm.php_warning(&m)?;
            Ok(fail)
        }
        other => Err(throw(vm, a, other)),
    }
}

/// Maps a result with `ok`, failures to `false`.
fn finish<T>(
    vm: &mut Vm,
    a: &Args,
    r: PcntlResult<T>,
    ok: impl FnOnce(T) -> Value,
) -> VmResult<Value> {
    match r {
        Ok(v) => Ok(ok(v)),
        Err(e) => failure(vm, a, e, Value::Bool(false)),
    }
}

/// An associative array of ints.
fn int_map(entries: &[(&'static str, i64)]) -> Value {
    let mut arr = ZArray::with_capacity(entries.len());
    let t = arr.make_mut();
    for (k, v) in entries {
        t.insert(ArrayKey::Str(ZStr::from(*k)), Value::Int(*v));
    }
    Value::Array(arr)
}

/// A list of ints.
fn int_list(items: impl IntoIterator<Item = i64>) -> Value {
    Value::Array(ZArray::from_values(items.into_iter().map(Value::Int)))
}

/// The `$siginfo` / `$info` array for a signal reported as `signo`.
fn siginfo_value(info: &SigInfo, signo: i64) -> Option<Value> {
    let entries = info.entries(signo)?;
    let mut arr = ZArray::with_capacity(entries.len());
    let t = arr.make_mut();
    for (k, v) in entries {
        let v = match v {
            InfoValue::Int(i) => Value::Int(i),
            InfoValue::Float(f) => Value::Float(f),
        };
        t.insert(ArrayKey::Str(ZStr::from(k)), v);
    }
    Some(Value::Array(arr))
}

/// A `$resource_usage` array (`None` → empty).
fn rusage_value(ru: Option<&ResourceUsage>) -> Value {
    ru.map_or_else(|| Value::Array(ZArray::new()), |r| int_map(&r.entries()))
}

/// The optional int argument `n` (`?int $x = null`).
fn opt_int(a: &Args, n: usize) -> Option<i64> {
    a.opt(n).map(|_| a.int(n))
}

/// `zval_try_get_long()` for an element of a `$signals` array: `None`
/// when the value cannot be used as an integer.
fn try_long(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Null => Some(0),
        Value::Float(f) => float_to_long(*f),
        Value::Str(s) => match is_numeric_string_ex(s.as_bytes(), true).map(|n| n.value) {
            Some(Number::Int(i)) => Some(i),
            Some(Number::Float(f)) => float_to_long(f),
            None => None,
        },
        Value::Reference(r) => try_long(&r.get()),
        _ => None,
    }
}

/// `zend_dval_to_lval_cap`-free conversion used by `zval_try_get_long`:
/// out-of-range and NaN values fail.
fn float_to_long(f: f64) -> Option<i64> {
    #[allow(clippy::cast_precision_loss)]
    let in_range = f.is_finite() && f >= i64::MIN as f64 && f < i64::MAX as f64;
    #[allow(clippy::cast_possible_truncation)]
    in_range.then_some(f as i64)
}

/// Converts a `$signals` array for the library.
fn signal_args(arr: &ZArray) -> (Vec<Option<i64>>, Vec<String>) {
    let mut ints = Vec::with_capacity(arr.len());
    let mut names = Vec::new();
    for (_, v) in arr.iter() {
        let v = v.deref();
        let n = try_long(&v);
        if n.is_none() {
            names.push(v.value_name());
        }
        ints.push(n);
    }
    (ints, names)
}

/// Builds the borrowed [`SignalArg`] view of [`signal_args`].
fn signal_view<'a>(ints: &[Option<i64>], names: &'a [String]) -> Vec<SignalArg<'a>> {
    let mut names = names.iter();
    ints.iter()
        .map(|n| match n {
            Some(i) => SignalArg::Int(*i),
            None => SignalArg::Invalid(names.next().map_or("mixed", String::as_str)),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Signal dispatch
// ---------------------------------------------------------------------------

/// `pcntl_signal_dispatch()`: runs the PHP handlers of the queued signals.
/// A throwing handler stops the batch; the rest stay queued.
fn dispatch(vm: &mut Vm) -> VmResult<()> {
    let Some(mut batch) = with(Pcntl::begin_dispatch) else {
        return Ok(());
    };
    vm.block_fiber_switch(true);
    let mut result = Ok(());
    while let Some(sig) = batch.next() {
        let handler = with(|p| p.handler(sig.signo).clone());
        if let SignalHandler::Callable(cb) = handler {
            let info =
                siginfo_value(&sig.info, sig.signo).unwrap_or_else(|| Value::Array(ZArray::new()));
            if let Err(t) = vm.call_value(&cb, vec![Value::Int(sig.signo), info]) {
                result = Err(t);
                break;
            }
        }
    }
    vm.block_fiber_switch(false);
    with(|p| p.end_dispatch(batch));
    if result.is_err() && php_pcntl::async_interrupt_pending() {
        // Signals a throwing handler left behind are delivered at the next
        // safe point once the exception is handled.
        php_vm::interrupt::request_interrupt();
    }
    result
}

/// The VM interrupt handler (`pcntl_interrupt_function`).
fn on_interrupt(vm: &mut Vm) -> VmResult<()> {
    if php_pcntl::async_interrupt_pending() {
        dispatch(vm)?;
    }
    Ok(())
}

/// The tick handler (`pcntl_signal_dispatch_tick_function`).
fn on_tick(vm: &mut Vm, _ticks: i64) -> VmResult<()> {
    dispatch(vm)
}

fn pcntl_signal_dispatch(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    dispatch(vm)?;
    Ok(Value::Bool(true))
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

fn pcntl_fork(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // The CLI writes unbuffered in PHP: whatever we buffered so far
    // belongs to the parent alone.
    vm.flush_stdout();
    let threaded = host::on_secondary_thread();
    match with(Pcntl::fork) {
        Ok(0) => {
            if threaded {
                ForkedChild::arm();
            }
            // php_child_init(): clear the stat cache.
            let _ = vm.call_value(
                &Value::Str(ZStr::from("clearstatcache")),
                vec![Value::Bool(true)],
            );
            Ok(Value::Int(0))
        }
        Ok(pid) => Ok(Value::Int(pid)),
        Err(e) => failure(vm, a, e, Value::Int(-1)),
    }
}

/// Assigns the outputs of a wait call.
fn wait_outputs(a: &Args, r: &WaitResult, status_arg: usize, rusage_arg: usize) -> Value {
    a.set_ref(status_arg, Value::Int(r.status));
    if a.has(rusage_arg) {
        a.set_ref(rusage_arg, rusage_value(r.resource_usage.as_ref()));
    }
    Value::Int(r.pid)
}

fn pcntl_waitpid(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let status = vm.value_to_int(&a.at(1))?;
    let flags = a.int(2);
    let r = with(|p| p.waitpid(a.int(0), status, flags, a.has(3)));
    Ok(wait_outputs(a, &r, 1, 3))
}

fn pcntl_wait(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let status = vm.value_to_int(&a.at(0))?;
    let flags = a.int(1);
    let r = with(|p| p.wait(status, flags, a.has(2)));
    Ok(wait_outputs(a, &r, 0, 2))
}

fn pcntl_waitid(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let idtype = if a.has(0) { a.int(0) } else { php_pcntl::P_ALL };
    let flags = if a.has(3) {
        a.int(3)
    } else {
        i64::from(libc::WEXITED)
    };
    let want_rusage = a.has(4);
    let r = with(|p| p.waitid(idtype, opt_int(a, 1), flags, want_rusage));
    let Ok((info, ru)) = r else {
        if want_rusage {
            a.set_ref(4, Value::Array(ZArray::new()));
        }
        return Ok(Value::Bool(false));
    };
    if a.has(2) {
        let sigchld = i64::from(libc::SIGCHLD);
        a.set_ref(
            2,
            siginfo_value(&info, sigchld).unwrap_or_else(|| Value::Array(ZArray::new())),
        );
    }
    if want_rusage {
        a.set_ref(4, rusage_value(ru.as_ref()));
    }
    Ok(Value::Bool(true))
}

fn pcntl_exec(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = a.str(0);
    let args = if a.has(1) {
        let arr = a.array(1);
        if !arr.is_list() {
            return Err(throw(vm, a, php_pcntl::exec_args_not_list()));
        }
        let mut out = Vec::with_capacity(arr.len());
        for (_, v) in arr.iter() {
            out.push(vm.value_to_string(&v.deref())?.as_bytes().to_vec());
        }
        Some(out)
    } else {
        None
    };
    let env = if a.has(2) {
        let arr = a.array(2);
        let mut out = Vec::with_capacity(arr.len());
        for (k, v) in arr.iter() {
            let value = vm.value_to_string(&v.deref())?.as_bytes().to_vec();
            let key = match k {
                ArrayKey::Int(i) => i.to_string().into_bytes(),
                ArrayKey::Str(s) => s.as_bytes().to_vec(),
            };
            out.push((key, value));
        }
        Some(out)
    } else {
        None
    };
    vm.flush_stdout();
    let e = with(|p| p.exec(path.as_bytes(), args.as_deref(), env.as_deref()));
    failure(vm, a, e, Value::Bool(false))
}

fn pcntl_alarm(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|p| p.alarm(a.int(0)));
    match r {
        Ok(n) => Ok(Value::Int(n)),
        Err(e) => failure(vm, a, e, Value::Int(0)),
    }
}

fn pcntl_get_last_error(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(with(|p| p.last_error())))
}

fn pcntl_strerror(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(ZStr::from(php_pcntl::strerror(a.int(0)))))
}

// ---------------------------------------------------------------------------
// Status macros
// ---------------------------------------------------------------------------

macro_rules! status_bool {
    ($($name:ident => $f:path),* $(,)?) => {
        $(fn $name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            Ok(Value::Bool($f(a.int(0))))
        })*
    };
}

macro_rules! status_int {
    ($($name:ident => $f:path),* $(,)?) => {
        $(fn $name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            Ok(Value::Int($f(a.int(0))))
        })*
    };
}

status_bool! {
    pcntl_wifexited => php_pcntl::wifexited,
    pcntl_wifstopped => php_pcntl::wifstopped,
    pcntl_wifcontinued => php_pcntl::wifcontinued,
    pcntl_wifsignaled => php_pcntl::wifsignaled,
}

status_int! {
    pcntl_wexitstatus => php_pcntl::wexitstatus,
    pcntl_wtermsig => php_pcntl::wtermsig,
    pcntl_wstopsig => php_pcntl::wstopsig,
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

fn pcntl_signal(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let signo = a.int(0);
    let n = php_pcntl::num_signals();
    if signo < 1 {
        return Err(throw(
            vm,
            a,
            PcntlError::ValueError(
                "Argument #1 ($signal) must be greater than or equal to 1".into(),
            ),
        ));
    }
    if signo >= n {
        return Err(throw(
            vm,
            a,
            PcntlError::ValueError(format!("Argument #1 ($signal) must be less than {n}")),
        ));
    }
    let restart = a.opt(2).map(|_| a.bool(2));
    let handle = a.at(1);
    let handler = if let Value::Int(i) = handle {
        match php_pcntl::int_handler::<Value>(i) {
            Ok(h) => h,
            Err(e) => return Err(throw(vm, a, e)),
        }
    } else if vm.is_callable(&handle) {
        SignalHandler::Callable(handle)
    } else {
        let e = with(|p| p.reject_handler(&handle.value_name()));
        return Err(throw(vm, a, e));
    };
    let callable = matches!(handler, SignalHandler::Callable(_));
    let r = with(|p| p.signal(signo, handler, restart));
    if r.is_ok() && callable {
        host::chain_interrupt(signo);
    }
    finish(vm, a, r, |()| Value::Bool(true))
}

fn pcntl_signal_get_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|p| {
        p.signal_get_handler(a.int(0)).map(|h| match h {
            SignalHandler::Callable(cb) => cb.clone(),
            other => Value::Int(other.as_int().unwrap_or(0)),
        })
    });
    match r {
        Ok(v) => Ok(v),
        Err(e) => failure(vm, a, e, Value::Bool(false)),
    }
}

fn pcntl_sigprocmask(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let arr = a.array(1);
    let (ints, names) = signal_args(&arr);
    let view = signal_view(&ints, &names);
    let r = with(|p| p.sigprocmask(a.int(0), &view));
    match r {
        Ok(old) => {
            if a.has(2) {
                a.set_ref(2, int_list(old));
            }
            Ok(Value::Bool(true))
        }
        Err(e) => failure(vm, a, e, Value::Bool(false)),
    }
}

/// Assigns `$info` for a waited-for signal and returns its number.
fn waited(a: &Args, info_arg: usize, signo: i64, info: &SigInfo) -> Value {
    if a.has(info_arg) {
        if let Some(v) = siginfo_value(info, signo) {
            a.set_ref(info_arg, v);
        }
    }
    Value::Int(signo)
}

fn pcntl_sigwaitinfo(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let arr = a.array(0);
    let (ints, names) = signal_args(&arr);
    let view = signal_view(&ints, &names);
    let r = with(|p| p.sigwaitinfo(&view));
    finish(vm, a, r, |(signo, info)| waited(a, 1, signo, &info))
}

fn pcntl_sigtimedwait(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let arr = a.array(0);
    let (ints, names) = signal_args(&arr);
    let view = signal_view(&ints, &names);
    let r = with(|p| p.sigtimedwait(&view, a.int(2), a.int(3)));
    finish(vm, a, r, |(signo, info)| waited(a, 1, signo, &info))
}

/// Not an asynchronous dispatch point: signals queued while async
/// signals were off do not raise a VM interrupt in PHP either.
fn pcntl_async_signals(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let enable = a.opt(0).map(|_| a.bool(0));
    Ok(Value::Bool(with(|p| p.async_signals(enable))))
}

// ---------------------------------------------------------------------------
// Scheduling, namespaces, affinity
// ---------------------------------------------------------------------------

fn pcntl_getpriority(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = if a.has(1) { a.int(1) } else { 0 };
    let r = with(|p| p.getpriority(opt_int(a, 0), mode));
    finish(vm, a, r, Value::Int)
}

fn pcntl_setpriority(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = if a.has(2) { a.int(2) } else { 0 };
    let r = with(|p| p.setpriority(a.int(0), opt_int(a, 1), mode));
    finish(vm, a, r, |()| Value::Bool(true))
}

fn pcntl_unshare(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if !host::on_secondary_thread() {
        host::wait_single_threaded();
    }
    let r = with(|p| p.unshare(a.int(0)));
    finish(vm, a, r, |()| Value::Bool(true))
}

fn pcntl_setns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let nstype = if a.has(1) {
        a.int(1)
    } else {
        i64::from(libc::CLONE_NEWNET)
    };
    if !host::on_secondary_thread() {
        host::wait_single_threaded();
    }
    let r = with(|p| p.setns(opt_int(a, 0), nstype));
    finish(vm, a, r, |()| Value::Bool(true))
}

fn pcntl_getcpuaffinity(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|p| p.getcpuaffinity(opt_int(a, 0)));
    finish(vm, a, r, int_list)
}

fn pcntl_setcpuaffinity(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let arr = if a.has(1) { a.array(1) } else { ZArray::new() };
    let values: Vec<Value> = arr.iter().map(|(_, v)| v.deref()).collect();
    let names: Vec<String> = values.iter().map(Value::value_name).collect();
    let cpus: Vec<CpuArg<'_>> = values
        .iter()
        .zip(&names)
        .map(|(v, name)| match v {
            Value::Int(i) => CpuArg::Int(*i),
            Value::Str(s) => CpuArg::Str(s.as_bytes()),
            _ => CpuArg::Invalid(name),
        })
        .collect();
    let r = with(|p| p.setcpuaffinity(opt_int(a, 0), &cpus));
    finish(vm, a, r, |()| Value::Bool(true))
}

fn pcntl_getcpu(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(php_pcntl::getcpu()))
}

// ---------------------------------------------------------------------------
// Request lifecycle
// ---------------------------------------------------------------------------

fn request_startup(_vm: &mut Vm) {
    with(|p| *p = Pcntl::new());
}

fn request_shutdown(vm: &mut Vm) {
    with(Pcntl::request_shutdown);
    ForkedChild::record(vm.exit_status);
}

impl Extension for PcntlExt {
    fn name(&self) -> &'static str {
        "pcntl"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        m.info(module_info);
        for (name, value) in php_pcntl::constants() {
            m.constant(name, Value::Int(value));
        }
        m.on_request_startup(request_startup)
            .on_request_shutdown(request_shutdown);
        m.vm().register_interrupt_handler(on_interrupt);
        m.vm().register_tick_handler(on_tick);
        m.function_static("pcntl_fork(): int", pcntl_fork)
            .function_static(
                "pcntl_waitpid(int $process_id, &$status, int $flags = 0, &$resource_usage = []): int",
                pcntl_waitpid,
            )
            .function_static(
                "pcntl_waitid(int $idtype = P_ALL, ?int $id = null, &$info = [], int $flags = WEXITED, &$resource_usage = []): bool",
                pcntl_waitid,
            )
            .function_static(
                "pcntl_wait(&$status, int $flags = 0, &$resource_usage = []): int",
                pcntl_wait,
            )
            .function_static(
                "pcntl_signal(int $signal, $handler, ?bool $restart_syscalls = null): bool",
                pcntl_signal,
            )
            .function_static("pcntl_signal_get_handler(int $signal)", pcntl_signal_get_handler)
            .function_static("pcntl_signal_dispatch(): bool", pcntl_signal_dispatch)
            .function_static(
                "pcntl_sigprocmask(int $mode, array $signals, &$old_signals = null): bool",
                pcntl_sigprocmask,
            )
            .function_static(
                "pcntl_sigwaitinfo(array $signals, &$info = []): int|false",
                pcntl_sigwaitinfo,
            )
            .function_static(
                "pcntl_sigtimedwait(array $signals, &$info = [], int $seconds = 0, int $nanoseconds = 0): int|false",
                pcntl_sigtimedwait,
            )
            .function_static("pcntl_wifexited(int $status): bool", pcntl_wifexited)
            .function_static("pcntl_wifstopped(int $status): bool", pcntl_wifstopped)
            .function_static("pcntl_wifcontinued(int $status): bool", pcntl_wifcontinued)
            .function_static("pcntl_wifsignaled(int $status): bool", pcntl_wifsignaled)
            .function_static("pcntl_wexitstatus(int $status): int|false", pcntl_wexitstatus)
            .function_static("pcntl_wtermsig(int $status): int|false", pcntl_wtermsig)
            .function_static("pcntl_wstopsig(int $status): int|false", pcntl_wstopsig)
            .function_static(
                "pcntl_exec(string $path, array $args = [], array $env_vars = []): false",
                pcntl_exec,
            )
            .function_static("pcntl_alarm(int $seconds): int", pcntl_alarm)
            .function_static("pcntl_get_last_error(): int", pcntl_get_last_error)
            .function_static("pcntl_errno(): int", pcntl_get_last_error)
            .function_static(
                "pcntl_getpriority(?int $process_id = null, int $mode = PRIO_PROCESS): int|false",
                pcntl_getpriority,
            )
            .function_static(
                "pcntl_setpriority(int $priority, ?int $process_id = null, int $mode = PRIO_PROCESS): bool",
                pcntl_setpriority,
            )
            .function_static("pcntl_strerror(int $error_code): string", pcntl_strerror)
            .function_static("pcntl_async_signals(?bool $enable = null): bool", pcntl_async_signals)
            .function_static("pcntl_unshare(int $flags): bool", pcntl_unshare)
            .function_static(
                "pcntl_setns(?int $process_id = null, int $nstype = CLONE_NEWNET): bool",
                pcntl_setns,
            )
            .function_static(
                "pcntl_getcpuaffinity(?int $process_id = null): array|false",
                pcntl_getcpuaffinity,
            )
            .function_static(
                "pcntl_setcpuaffinity(?int $process_id = null, array $cpu_ids = []): bool",
                pcntl_setcpuaffinity,
            )
            .function_static("pcntl_getcpu(): int", pcntl_getcpu);
    }
}

/// `PHP_MINFO_FUNCTION`.
fn module_info(_vm: &mut Vm) -> Vec<php_vm::ext::InfoItem> {
    vec![
        php_vm::ext::InfoItem::TableStart,
        php_vm::ext::InfoItem::Row(vec!["pcntl support".into(), "enabled".into()]),
        php_vm::ext::InfoItem::TableEnd,
    ]
}
