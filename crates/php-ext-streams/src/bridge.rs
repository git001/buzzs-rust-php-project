// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Glue between the VM and the `php-streams` library.
//!
//! The library reports warnings through a thread-local diagnostics queue and
//! calls back into PHP (user-space wrappers and filters, `php://output`)
//! through traits that have no access to the VM. This module provides:
//!
//! * [`run`]: executes a library call with the VM reachable from those
//!   callbacks ([`with_vm`]), then emits the queued diagnostics as
//!   `func(): message` warnings and re-raises an exception thrown by a
//!   callback;
//! * synchronisation of the INI settings the library consults;
//! * conversion between VM values and the library's [`SValue`].

use std::cell::{Cell, RefCell};

use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_streams::diag;
use php_streams::state;
use php_streams::value::{Key as SKey, Value as SValue};
use php_vm::ext::{Throw, VmResult};
use php_vm::Vm;

thread_local! {
    /// The VM lent to library callbacks for the duration of [`enter`].
    static VM_PTR: Cell<*mut Vm> = const { Cell::new(std::ptr::null_mut()) };
    /// The first exception (or exit) raised inside a callback, re-raised
    /// when control returns to the native function.
    static PENDING: RefCell<Option<Throw>> = const { RefCell::new(None) };
    /// While set, [`flush_diags`] leaves the diagnostics queued (a hook is
    /// running with its stream taken out of the resource).
    static DEFER_FLUSH: Cell<bool> = const { Cell::new(false) };
    /// The contexts of the running (nested) operations.
    static OP_CONTEXTS: RefCell<Vec<Option<php_streams::context::ContextRef>>> =
        const { RefCell::new(Vec::new()) };
    /// The INI generation and the library settings last derived from it
    /// by [`sync_settings`].
    static SYNCED: RefCell<Option<(u64, state::Settings)>> = const { RefCell::new(None) };
}

/// Restores the previous VM pointer when a scope ends (also on unwind).
struct Restore(*mut Vm);

impl Drop for Restore {
    fn drop(&mut self) {
        VM_PTR.with(|p| p.set(self.0));
    }
}

/// Runs `f` with `vm` reachable through [`with_vm`].
///
/// The `&mut Vm` borrow is held for the whole call, so the VM is only ever
/// accessed through the pointer while the caller cannot touch it.
pub fn enter<R>(vm: &mut Vm, f: impl FnOnce() -> R) -> R {
    sync_settings(vm);
    enter_nosync(vm, f)
}

/// [`enter`] without re-reading the INI settings (operations on an open
/// stream do not consult them).
fn enter_nosync<R>(vm: &mut Vm, f: impl FnOnce() -> R) -> R {
    let ptr: *mut Vm = vm;
    let prev = VM_PTR.with(|p| p.replace(ptr));
    let _restore = Restore(prev);
    f()
}

/// Calls `f` with the VM lent by the innermost [`enter`], or returns `None`
/// when no native function is running (e.g. a resource destroyed by the
/// VM's garbage handling).
pub fn with_vm<R>(f: impl FnOnce(&mut Vm) -> R) -> Option<R> {
    let ptr = VM_PTR.with(|p| p.replace(std::ptr::null_mut()));
    if ptr.is_null() {
        return None;
    }
    let _restore = Restore(ptr);
    // SAFETY: `ptr` was derived from the `&mut Vm` passed to `enter`, whose
    // frame is still active (the pointer is reset when it returns). That
    // borrow is not used while `enter`'s closure runs, and the slot is
    // nulled for the duration of `f`, so this is the only live mutable
    // access path to the VM; nested `enter` calls reborrow from it.
    let vm = unsafe { &mut *ptr };
    Some(f(vm))
}

/// Whether a VM is currently reachable.
pub fn has_vm() -> bool {
    VM_PTR.with(|p| !p.get().is_null())
}

/// Records a throw raised inside a callback (the first one wins).
pub fn set_pending(t: Throw) {
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if p.is_none() {
            *p = Some(t);
        }
    });
}

/// Whether a callback threw.
pub fn has_pending() -> bool {
    PENDING.with(|p| p.borrow().is_some())
}

/// Whether a callback raised a fatal error (`zend_bailout()`): unlike an
/// exception, a bailout skips the cleanup of the native frames it crosses.
pub fn pending_is_fatal() -> bool {
    PENDING.with(|p| matches!(*p.borrow(), Some(Throw::Fatal)))
}

/// Takes the pending throw.
pub fn take_pending() -> Option<Throw> {
    PENDING.with(|p| p.borrow_mut().take())
}

/// Emits the queued library diagnostics through the VM.
pub fn flush_diags(vm: &mut Vm) {
    if DEFER_FLUSH.with(Cell::get) {
        return;
    }
    let diags = diag::take();
    if !diags.is_empty() {
        crate::errors::report(vm, diags);
    }
}

/// Runs a library call (see [`enter`]), emits its diagnostics and
/// propagates an exception raised by a user callback.
pub fn run<R>(vm: &mut Vm, f: impl FnOnce() -> R) -> VmResult<R> {
    run_ex(vm, true, f)
}

/// [`run`] for operations on open streams (no INI synchronisation).
pub fn run_stream<R>(vm: &mut Vm, f: impl FnOnce() -> R) -> VmResult<R> {
    run_ex(vm, false, f)
}

/// `PHP_STREAM_ERROR_MAX_DEPTH`.
const MAX_OPERATION_DEPTH: usize = 1000;

fn run_ex<R>(vm: &mut Vm, sync: bool, f: impl FnOnce() -> R) -> VmResult<R> {
    let depth = OP_CONTEXTS.with(|s| s.borrow().len());
    if depth >= MAX_OPERATION_DEPTH {
        // php_stream_error_operation_begin() refuses the operation: its
        // errors stay with the enclosing one.
        vm.php_warning(&format!(
            "Stream error operation depth exceeded ({depth}), possible infinite recursion"
        ))?;
        let outer = take_pending();
        let r = if sync {
            enter(vm, f)
        } else {
            enter_nosync(vm, f)
        };
        let inner = take_pending();
        if let Some(t) = outer {
            set_pending(t);
        }
        return match inner {
            Some(t) => Err(t),
            None => Ok(r),
        };
    }
    // Diagnostics queued by library calls made outside a bridge scope
    // belong to their caller, which collects them with `take_diagnostics`.
    // A nested call (from PHP code a callback runs) is its own error
    // operation: the enclosing operation's errors stay queued until it
    // ends (`php_stream_error_operation_begin/end`).
    // (Inside a callback the VM is lent out, so `has_vm()` is false: the
    // operation depth tells nested calls apart from stray library calls.)
    let enclosing = if has_vm() || depth > 0 {
        diag::take()
    } else {
        let _ = diag::take();
        Vec::new()
    };
    let outer = take_pending();
    OP_CONTEXTS.with(|s| s.borrow_mut().push(crate::errors::operation_context()));
    let r = if sync {
        enter(vm, f)
    } else {
        enter_nosync(vm, f)
    };
    flush_diags(vm);
    for d in enclosing.into_iter().rev() {
        diag::push_front(d);
    }
    // The enclosing operation reports with its own context again.
    let enclosing_ctx = OP_CONTEXTS.with(|s| {
        let mut s = s.borrow_mut();
        s.pop();
        s.last().cloned()
    });
    match enclosing_ctx {
        Some(ctx) => crate::errors::set_operation_context(ctx),
        // The outermost operation ended: a user wrapper instance made for
        // a URL operation (unlink, stat, ...) is released now.
        None => crate::userwrap::clear_last_object(),
    }
    let inner = take_pending();
    if let Some(t) = outer {
        set_pending(t);
    }
    match inner {
        Some(t) => Err(t),
        None => Ok(r),
    }
}

/// For other extensions calling the library directly (outside [`run`]):
/// the queued diagnostic texts (without the `func(): ` prefix).
pub fn take_diagnostics() -> Vec<String> {
    diag::take()
        .iter()
        .map(php_streams::diag::Diagnostic::text)
        .collect()
}

/// Defers [`flush_diags`] while `f` runs.
pub fn deferring_flush<R>(f: impl FnOnce() -> R) -> R {
    let old = DEFER_FLUSH.with(|d| d.replace(true));
    let r = f();
    DEFER_FLUSH.with(|d| d.set(old));
    r
}

/// Copies the INI settings consulted by the streams layer (`sys_temp_dir`,
/// `upload_tmp_dir`, `open_basedir`, ...) into the library: [`enter`] does
/// it for every native function; code that calls the library outside one
/// (the SAPIs at request startup) calls it first.
pub fn sync_settings(vm: &Vm) {
    // Re-reading 13 directives costs thousands of instructions per stream
    // call; the registry generation tells whether any value can have
    // changed. The cached settings are still written back on every call,
    // because other code (open_basedir checks in session/PDO, SAPIs) may
    // have overwritten the library's copy in between.
    let generation = vm.ini.generation();
    let hit = SYNCED.with(|c| match &*c.borrow() {
        Some((g, snap)) if *g == generation => {
            state::update_settings(|s| apply_settings(s, snap));
            true
        }
        _ => false,
    });
    if hit {
        return;
    }
    let get = |n: &str| vm.ini_string(n).unwrap_or_default();
    let snap = state::Settings {
        include_path: get("include_path"),
        open_basedir: crate::ini::effective_open_basedir(get("open_basedir")),
        sys_temp_dir: get("sys_temp_dir"),
        upload_tmp_dir: get("upload_tmp_dir"),
        allow_url_fopen: vm.ini_bool("allow_url_fopen"),
        allow_url_include: vm.ini_bool("allow_url_include"),
        auto_detect_line_endings: vm.ini_bool("auto_detect_line_endings"),
        default_socket_timeout: vm.ini_long("default_socket_timeout"),
        html_errors: vm.ini_bool("html_errors"),
        sapi_is_cli: true,
        user_agent: vm.ini_string("user_agent").filter(|v| !v.is_empty()),
        from_address: vm.ini_string("from").filter(|v| !v.is_empty()),
        realpath_cache_size_limit: vm.ini_long("realpath_cache_size"),
        realpath_cache_ttl: vm.ini_long("realpath_cache_ttl"),
    };
    state::update_settings(|s| apply_settings(s, &snap));
    SYNCED.with(|c| *c.borrow_mut() = Some((generation, snap)));
}

/// Copies every INI-derived field of `snap` into the library settings
/// (`clone_from` reuses the existing buffers).
fn apply_settings(s: &mut state::Settings, snap: &state::Settings) {
    s.include_path.clone_from(&snap.include_path);
    s.open_basedir.clone_from(&snap.open_basedir);
    s.sys_temp_dir.clone_from(&snap.sys_temp_dir);
    s.upload_tmp_dir.clone_from(&snap.upload_tmp_dir);
    s.allow_url_fopen = snap.allow_url_fopen;
    s.allow_url_include = snap.allow_url_include;
    s.auto_detect_line_endings = snap.auto_detect_line_endings;
    s.default_socket_timeout = snap.default_socket_timeout;
    s.html_errors = snap.html_errors;
    s.sapi_is_cli = snap.sapi_is_cli;
    s.user_agent.clone_from(&snap.user_agent);
    s.from_address.clone_from(&snap.from_address);
    s.realpath_cache_size_limit = snap.realpath_cache_size_limit;
    s.realpath_cache_ttl = snap.realpath_cache_ttl;
}

/// Forces the next [`sync_settings`] to re-read the INI values (state they
/// derive from changed outside the INI registry, e.g. the expanded
/// `open_basedir`).
pub(crate) fn invalidate_settings() {
    SYNCED.with(|c| *c.borrow_mut() = None);
}

/// Installs the library hooks (output, executing file name).
pub fn install_hooks() {
    state::set_hooks(state::Hooks {
        output: Some(Box::new(|data: &[u8]| {
            let data = data.to_vec();
            if with_vm(|vm| vm.output(&data)).is_none() {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&data);
            }
        })),
        executed_filename: Some(Box::new(|| {
            with_vm(|vm| vm.current_location().0.as_bytes().to_vec())
        })),
        request_body: None,
    });
}

// ---------------------------------------------------------------------------
// Value conversion
// ---------------------------------------------------------------------------

/// Converts a VM value for the library.
pub fn to_lib(v: &Value) -> SValue {
    match v {
        Value::Null => SValue::Null,
        Value::Bool(b) => SValue::Bool(*b),
        Value::Int(i) => SValue::Int(*i),
        Value::Float(f) => SValue::Float(*f),
        Value::Str(s) => SValue::Str(s.as_bytes().to_vec()),
        Value::Array(a) => SValue::Array(
            a.iter()
                .map(|(k, v)| {
                    let k = match k {
                        ArrayKey::Int(i) => SKey::Int(*i),
                        ArrayKey::Str(s) => SKey::Str(s.as_bytes().to_vec()),
                    };
                    (k, to_lib(v))
                })
                .collect(),
        ),
        Value::Object(o) => SValue::Object {
            class: o.class_name().to_string_lossy(),
            string_value: None,
        },
        Value::Resource(r) => match crate::res::stream_id_of(r) {
            Some(id) => SValue::Stream(id),
            None => SValue::Resource(r.handle()),
        },
        Value::Reference(r) => to_lib(&r.get()),
    }
}

/// Converts a library value for the VM.
pub fn from_lib(v: &SValue) -> Value {
    match v {
        SValue::Null | SValue::Object { .. } | SValue::Resource(_) => Value::Null,
        SValue::Bool(b) => Value::Bool(*b),
        SValue::Int(i) => Value::Int(*i),
        SValue::Float(f) => Value::Float(*f),
        SValue::Str(s) => Value::Str(ZStr::from(s.as_slice())),
        SValue::Array(items) => {
            let mut a = ZArray::new();
            for (k, v) in items {
                match k {
                    SKey::Int(i) => a.make_mut().insert(*i, from_lib(v)),
                    SKey::Str(s) => a
                        .make_mut()
                        .insert(ArrayKey::Str(ZStr::from(s.as_slice())), from_lib(v)),
                };
            }
            Value::Array(a)
        }
        SValue::Stream(id) => crate::res::stream_value_by_id(*id).unwrap_or(Value::Null),
    }
}

/// A PHP string value from bytes.
pub fn bytes(b: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(b.as_ref()))
}

/// The type name PHP prints in "must be of type X, Y given" messages.
pub fn given_type(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Object(o) => o.class_name().to_string_lossy(),
        Value::Resource(_) => "resource".into(),
        Value::Reference(r) => given_type(&r.get()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_roundtrip() {
        let mut a = ZArray::new();
        a.make_mut().insert("k", Value::Int(3));
        a.make_mut().insert(5, bytes("x"));
        let v = Value::Array(a);
        let back = from_lib(&to_lib(&v));
        let arr = back.as_array().expect("array");
        assert!(matches!(arr.get_str(b"k"), Some(Value::Int(3))));
        assert_eq!(
            arr.get_int(5).and_then(Value::as_str).map(ZStr::as_bytes),
            Some(&b"x"[..])
        );
    }

    #[test]
    fn no_vm_outside_enter() {
        assert!(!has_vm());
        assert!(with_vm(|_| ()).is_none());
    }

    #[test]
    fn type_names() {
        assert_eq!(given_type(&Value::Null), "null");
        assert_eq!(given_type(&Value::Bool(false)), "false");
        assert_eq!(given_type(&bytes("a")), "string");
    }
}
