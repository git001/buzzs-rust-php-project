// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The per-request `MBSTRG(...)` state and the glue that turns
//! `php-mbstring` results into VM values, warnings and exceptions.
//!
//! The VM is single-threaded, so the [`MbState`] lives in a thread-local.
//! It is only borrowed for the duration of one library call — never across
//! a call back into the VM — so user callbacks and error handlers may
//! re-enter `mb_*` functions freely.

use std::cell::RefCell;

use php_mbstring::{Level, MbError, MbResult, MbState};
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;

thread_local! {
    static STATE: RefCell<MbState> = RefCell::new(MbState::new());
    /// The array `mb_list_encodings()` hands out (`MBSTRG(all_encodings_list)`),
    /// so that callers passing it back can be recognised.
    static ALL_ENCODINGS: RefCell<Option<ZArray>> = const { RefCell::new(None) };
}

/// Runs `f` with the request's [`MbState`].
pub(crate) fn with<R>(f: impl FnOnce(&mut MbState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Replaces the request state (MINIT/RINIT).
pub(crate) fn replace(st: MbState) {
    STATE.with(|s| *s.borrow_mut() = st);
}

/// The cached `mb_list_encodings()` array.
pub(crate) fn all_encodings() -> ZArray {
    ALL_ENCODINGS.with(|c| {
        c.borrow_mut()
            .get_or_insert_with(|| {
                ZArray::from_values(
                    php_mbstring::state::mb_list_encodings()
                        .into_iter()
                        .map(Value::from),
                )
            })
            .clone()
    })
}

/// Whether `arr` is the very array returned by `mb_list_encodings()`.
pub(crate) fn is_all_encodings(arr: &ZArray) -> bool {
    ALL_ENCODINGS.with(|c| c.borrow().as_ref().is_some_and(|a| ZArray::ptr_eq(a, arr)))
}

/// Raises the diagnostics queued by the library, prefixed with the running
/// function's name.
pub(crate) fn flush_diagnostics(vm: &mut Vm) -> VmResult<()> {
    let diags = with(MbState::take_diagnostics);
    for d in diags {
        match d.level {
            Level::Warning => vm.php_warning(&d.message)?,
            Level::Notice => vm.php_notice(&d.message)?,
            Level::Deprecated => vm.php_deprecated(&d.message)?,
        }
    }
    Ok(())
}

/// The name of the running native function (for exception messages).
pub(crate) fn func_name(a: &Args) -> String {
    a.func
        .as_ref()
        .map_or_else(String::new, |f| f.name.to_string_lossy())
}

/// Converts a library error into a thrown exception.
pub(crate) fn throw(vm: &mut Vm, a: &Args, e: &MbError) -> php_vm::Throw {
    let msg = e.message_for(&func_name(a));
    vm.throw(e.class(), &msg)
}

/// Raises pending diagnostics, then maps `r`'s error to an exception.
pub(crate) fn finish<T>(vm: &mut Vm, a: &Args, r: MbResult<T>) -> VmResult<T> {
    flush_diagnostics(vm)?;
    r.map_err(|e| throw(vm, a, &e))
}

/// Runs a library call with the state, then [`finish`]es it.
pub(crate) fn call<T>(
    vm: &mut Vm,
    a: &Args,
    f: impl FnOnce(&mut MbState) -> MbResult<T>,
) -> VmResult<T> {
    let r = with(f);
    finish(vm, a, r)
}

/// A byte string value.
pub(crate) fn bytes(v: impl Into<Vec<u8>>) -> Value {
    Value::Str(ZStr::new(v.into()))
}

/// `?string` argument `n` (passed and not null).
pub(crate) fn opt_str(a: &Args, n: usize) -> Option<ZStr> {
    match a.opt(n) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// A list of strings as a PHP array.
pub(crate) fn str_list<S: AsRef<[u8]>>(items: impl IntoIterator<Item = S>) -> Value {
    Value::Array(ZArray::from_values(
        items
            .into_iter()
            .map(|s| Value::Str(ZStr::from(s.as_ref()))),
    ))
}

/// Converts a `php-runtime` error (from `php-stdmisc` helpers) into a VM
/// throw; bailouts parked in [`Vm::pending_throw`] come back out.
pub(crate) fn rt_throw(vm: &mut Vm, e: php_runtime::RuntimeError) -> php_vm::Throw {
    use php_runtime::RuntimeError as E;
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        E::Thrown(Value::Object(o)) => php_vm::Throw::Exception(o),
        E::Thrown(_) => php_vm::Throw::Fatal,
        // Recorded and pending: the VM reports it as the error leaves the builtin.
        E::MemoryLimit(_) => php_vm::Throw::Fatal,
        E::Fatal(m) => vm.fatal(&m),
        E::Custom { class, message } => vm.throw(&class.to_string_lossy(), &message),
        E::Error(m)
        | E::TypeError(m)
        | E::ValueError(m)
        | E::ArithmeticError(m)
        | E::DivisionByZeroError(m)
        | E::ArgumentCountError(m) => vm.throw(class, &m),
    }
}
