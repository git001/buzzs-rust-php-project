// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmHost`]: the [`ZipHost`] implementation over the VM, and the
//! conversions between VM throws and `php-zip`'s runtime errors.
//!
//! Library calls run inside [`php_ext_streams::bridge::run`], which lends
//! the VM to code that has no direct access to it; the host reaches the VM
//! through [`bridge::with_vm`]. Warnings queued by the library are flushed
//! before any PHP code runs, so their order relative to callback output
//! matches PHP.

use std::cell::RefCell;

use php_ext_streams::bridge;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, RtResult, RuntimeContext, RuntimeError, Value,
};
use php_streams::diag::{self, Diagnostic, Severity, StreamErrorCode};
use php_vm::ext::VmResult;
use php_vm::{Throw, Vm};
use php_zip::{RegexMatcher, ZipHost};

thread_local! {
    /// Library diagnostics held back by [`run_frameless`].
    static HELD: RefCell<Option<Vec<Diagnostic>>> = const { RefCell::new(None) };
    /// The `addPattern()` PCRE state (pattern cache). The `php-ext-pcre`
    /// state is private to that crate, so zip keeps its own.
    static PCRE: RefCell<php_pcre::Pcre> = RefCell::new(php_pcre::Pcre::new());
}

/// Converts a VM throw into a runtime error. Bailouts (`exit`, fatal
/// errors) are parked in [`Vm::pending_throw`] and come back out of
/// [`rt_throw`].
pub(crate) fn to_rt(vm: &mut Vm, t: Throw) -> RuntimeError {
    match t {
        Throw::Exception(o) => RuntimeError::Thrown(Value::Object(o)),
        other => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(other);
            }
            RuntimeError::Fatal(String::new())
        }
    }
}

/// Converts a runtime error into a VM throw, creating the exception object
/// for errors raised by `php-zip`.
pub(crate) fn rt_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        RuntimeError::Thrown(Value::Object(o)) => Throw::Exception(o),
        RuntimeError::Thrown(_) => Throw::Fatal,
        // Recorded and pending: the VM reports it as the error leaves the builtin.
        RuntimeError::MemoryLimit(_) => Throw::Fatal,
        // A bailout whose cause was already reported.
        RuntimeError::Fatal(m) if m.is_empty() => Throw::Fatal,
        RuntimeError::Fatal(m) => vm.fatal(&m),
        RuntimeError::Custom { class, message } => vm.throw(&class.to_string_lossy(), &message),
        RuntimeError::Error(m)
        | RuntimeError::TypeError(m)
        | RuntimeError::ValueError(m)
        | RuntimeError::ArithmeticError(m)
        | RuntimeError::DivisionByZeroError(m)
        | RuntimeError::ArgumentCountError(m) => vm.throw(class, &m),
    }
}

/// The error returned when the VM is not reachable (cannot happen inside
/// [`run`]).
fn no_vm() -> RuntimeError {
    RuntimeError::Fatal(String::new())
}

/// The [`ZipHost`] handed to `php-zip`: every call goes to the VM lent by
/// the enclosing [`run`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VmHost;

/// Converts the throw of PHP code run by the library. Without an
/// executing frame (destructors at request shutdown) PHP cannot propagate
/// an exception: `zend_throw_exception_internal()` reports it as uncaught
/// and bails out, so it becomes a bailout for the library too.
fn callback_error(vm: &mut Vm, t: Throw) -> RuntimeError {
    match t {
        Throw::Exception(o) if !vm.is_executing() => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(Throw::Exception(o));
            }
            RuntimeError::Fatal(String::new())
        }
        t => to_rt(vm, t),
    }
}

impl RuntimeContext for VmHost {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        bridge::with_vm(|vm| {
            bridge::flush_diags(vm);
            let r = vm.error(level.bits(), message);
            r.map_err(|t| callback_error(vm, t))
        })
        .unwrap_or(Ok(()))
    }

    fn precision(&self) -> i64 {
        bridge::with_vm(|vm| vm.precision()).unwrap_or(14)
    }

    fn serialize_precision(&self) -> i64 {
        bridge::with_vm(|vm| vm.serialize_precision()).unwrap_or(-1)
    }

    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        bridge::with_vm(|vm| vm.cast_object(obj, target)).unwrap_or(Ok(None))
    }

    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        bridge::with_vm(|vm| vm.do_operation(op, op1, op2)).unwrap_or(Ok(None))
    }

    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        bridge::with_vm(|vm| vm.compare_objects(op1, op2)).unwrap_or(Ok(1))
    }
}

impl ZipHost for VmHost {
    fn call(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        bridge::with_vm(|vm| {
            // Warnings raised so far come before anything the callback does.
            bridge::flush_diags(vm);
            // With an exception pending, `zend_call_function()` does not
            // call anything.
            if let Some(t) = bridge::take_pending() {
                return Err(to_rt(vm, t));
            }
            let r = vm.call_value(callable, args.to_vec());
            // Destructors of values released by the callback run now, like
            // PHP's refcounting would.
            let r = r.and_then(|v| vm.run_released_destructors().map(|()| v));
            r.map_err(|t| callback_error(vm, t))
        })
        .unwrap_or_else(|| Err(no_vm()))
    }

    fn compile_regex(&mut self, regex: &[u8]) -> Option<RegexMatcher> {
        bridge::with_vm(|vm| compile_regex(vm, regex)).flatten()
    }
}

/// `pcre_get_compiled_regex()`: compiles through the PCRE cache, emitting
/// PCRE's warnings right away (the library's own are immediate too).
fn compile_regex(vm: &mut Vm, regex: &[u8]) -> Option<RegexMatcher> {
    let (result, warnings, cfg) = PCRE.with(|p| {
        let mut p = p.borrow_mut();
        p.set_backtrack_limit(vm.ini_long("pcre.backtrack_limit"));
        p.set_recursion_limit(vm.ini_long("pcre.recursion_limit"));
        p.set_jit(vm.ini_bool("pcre.jit"));
        let r = p.compile(regex);
        (r, p.take_warnings(), p.config())
    });
    let warn = |vm: &mut Vm, w: &[u8]| {
        if let Err(t) = vm.php_warning_bytes(w) {
            bridge::set_pending(t);
        }
    };
    for w in warnings {
        warn(vm, &w);
    }
    match result {
        Ok(re) => Some(Box::new(
            move |s: &[u8]| matches!(php_pcre::match_one(&re, &cfg, s, false, 0, 0).result, Ok(n) if n > 0),
        )),
        Err(e) => {
            warn(vm, &e.message());
            None
        }
    }
}

/// The diagnostics sink installed by [`run`]: emits through the VM at once.
fn emit_now(d: Diagnostic) {
    let pass = HELD.with(|h| match h.borrow_mut().as_mut() {
        Some(held) => {
            held.push(d);
            None
        }
        None => Some(d),
    });
    let Some(d) = pass else {
        return;
    };
    let done = bridge::with_vm(|vm| {
        // php_stream_display_wrapper_errors() stays silent when an
        // exception has already been thrown.
        if d.code == StreamErrorCode::OpenFailed && bridge::has_pending() {
            return;
        }
        let text = d.text();
        let r = match d.severity {
            Severity::Warning => vm.php_warning(&text),
            Severity::Notice => vm.php_notice(&text),
            Severity::Deprecated => vm.php_deprecated(&text),
            Severity::Error => vm.raise(1, &text),
        };
        if let Err(t) = r {
            bridge::set_pending(t);
        }
    });
    if done.is_none() {
        // No VM lent (cannot happen inside `run`): queue it (the handler is
        // detached while it runs, so this does not recurse).
        diag::emit(d);
    }
}

/// Runs a `php-zip` call with a [`VmHost`]: diagnostics are emitted with
/// the running function's prefix afterwards, and errors (exceptions from
/// callbacks, bailouts, the library's own exceptions) become throws.
pub(crate) fn run<R>(vm: &mut Vm, f: impl FnOnce(&mut dyn ZipHost) -> RtResult<R>) -> VmResult<R> {
    // Warnings are raised while the library runs (`php_error_docref()`
    // inside the C code), so a user error handler sees the archive in the
    // state it has at that point (e.g. "being closed").
    let prev = diag::set_handler(Some(Box::new(emit_now)));
    let r = bridge::run(vm, || f(&mut VmHost));
    diag::set_handler(prev);
    let r = match r {
        Ok(r) => r,
        Err(t) => {
            // A warning handler threw; a bailout parked meanwhile wins.
            return Err(vm.pending_throw.take().unwrap_or(t));
        }
    };
    match r {
        Ok(v) => match vm.pending_throw.take() {
            Some(t) => Err(t),
            None => Ok(v),
        },
        Err(e) => Err(rt_throw(vm, e)),
    }
}

/// [`run`] for code running without an executing PHP frame (the
/// destructor at request shutdown).
///
/// There, PHP turns any exception into an "Uncaught" fatal error and bails
/// out of libzip at once, so the library's later warnings (e.g. "Cannot
/// destroy the zip context" after a cancel callback returned garbage) are
/// never shown. The library's diagnostics are therefore held back and
/// emitted only when the call completes without an exception.
pub(crate) fn run_frameless<R>(
    vm: &mut Vm,
    f: impl FnOnce(&mut dyn ZipHost) -> RtResult<R>,
) -> VmResult<R> {
    let outer = HELD.with(|h| h.borrow_mut().replace(Vec::new()));
    let r = run(vm, f);
    let held = HELD
        .with(|h| std::mem::replace(&mut *h.borrow_mut(), outer))
        .unwrap_or_default();
    if let Err(Throw::Exception(_)) = &r {
        // The exception bails out (`zend_exception_error()` +
        // `zend_bailout()`); after a bailout during the shutdown
        // destructors PHP marks every object destructed, so values the
        // callbacks released meanwhile never run their destructors.
        drop(php_vm::object::take_pending_destructors());
        php_vm::object::set_destructors_enabled(false);
    }
    if r.is_ok() {
        let prev = diag::set_handler(Some(Box::new(emit_now)));
        bridge::enter(vm, || held.into_iter().for_each(emit_now));
        diag::set_handler(prev);
        if let Some(t) = bridge::take_pending() {
            return Err(t);
        }
    }
    r
}
