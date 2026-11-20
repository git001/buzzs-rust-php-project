// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmHost`]: the [`SessionHost`] implementation over the VM.
//!
//! # Where the session state lives
//!
//! `php-session` reaches its per-request state through
//! [`SessionHost::session`], which hands out `&mut SessionState`. The VM has
//! no per-extension storage, so the state is kept in a thread-local slot
//! ("parked") between calls. Every entry point ([`with_host`]) takes the
//! state out of the slot, works on it, and puts it back.
//!
//! Save handlers, error handlers, `__sleep`/`__wakeup` and friends run user
//! code that may re-enter the module (`parent::open()`, `session_id()`, …).
//! Before any such call the host parks the state again and takes it back
//! afterwards ([`VmHost::parked`]), so the nested entry point sees the same
//! state — exactly the role of PHP's `PS()` globals.

use std::cell::{Cell, RefCell};

use php_runtime::serialize::SerializedObject;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PhpRef, PropertyPurpose, RtResult,
    RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_session::{SessionHost, SessionState};
use php_vm::object::class_of;
use php_vm::vm::FrameKind;
use php_vm::{Throw, Vm};

/// The lifecycle phase, for the `php_error_docref` function prefix used
/// when no PHP code is executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// `MINIT` (`"PHP Startup"`).
    Startup,
    /// `RINIT` (`"PHP Request Startup"`).
    RequestStartup,
    /// The script is running.
    Running,
    /// `RSHUTDOWN` (`"PHP Request Shutdown"`).
    Shutdown,
}

thread_local! {
    /// The parked per-request state.
    static PARKED: RefCell<Option<Box<SessionState>>> = const { RefCell::new(None) };
    /// The current lifecycle phase.
    static PHASE: Cell<Phase> = const { Cell::new(Phase::Startup) };
}

/// Sets the lifecycle phase.
pub(crate) fn set_phase(p: Phase) {
    PHASE.with(|c| c.set(p));
}

fn phase() -> Phase {
    PHASE.with(Cell::get)
}

fn take_parked() -> Box<SessionState> {
    PARKED
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| Box::new(SessionState::new()))
}

fn put_parked(s: Box<SessionState>) {
    PARKED.with(|p| *p.borrow_mut() = Some(s));
}

/// Converts a VM throw into a runtime error; non-exception throws are
/// parked in `vm.pending_throw` (and resumed by [`to_throw`]).
///
/// `exit()` unwinds like an exception in PHP 8 (`unwind_exit`): the session
/// module sees a pending exception and keeps its bookkeeping consistent
/// (`PS(in_save_handler)` is reset). A fatal error is a bailout
/// ([`RuntimeError::Fatal`]): the module behaves as after a `longjmp`.
pub(crate) fn to_rt(vm: &mut Vm, t: Throw) -> RuntimeError {
    match t {
        Throw::Exception(o) => RuntimeError::Thrown(Value::Object(o)),
        Throw::Exit => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(Throw::Exit);
            }
            RuntimeError::Thrown(Value::Null)
        }
        other => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(other);
            }
            RuntimeError::Fatal(String::new())
        }
    }
}

/// Converts a runtime error into a VM throw.
pub(crate) fn to_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        RuntimeError::Thrown(Value::Object(o)) => Throw::Exception(o),
        RuntimeError::Thrown(_) => Throw::Fatal,
        RuntimeError::Fatal(m) => {
            if m.is_empty() {
                Throw::Fatal
            } else {
                vm.fatal(&m)
            }
        }
        RuntimeError::Custom { class, message } => vm.throw(&class.to_string_lossy(), &message),
        RuntimeError::Error(m)
        | RuntimeError::TypeError(m)
        | RuntimeError::ValueError(m)
        | RuntimeError::ArithmeticError(m)
        | RuntimeError::DivisionByZeroError(m)
        | RuntimeError::ArgumentCountError(m) => vm.throw(class, &m),
    }
}

/// The name `php_error_docref` prefixes: the innermost native function
/// (`Class::method` for methods).
fn active_builtin(vm: &Vm) -> Option<String> {
    let f = vm
        .frames
        .iter()
        .rev()
        .find(|f| f.kind == FrameKind::Builtin)?;
    let n = f.builtin_name.clone()?;
    Some(match &f.scope {
        Some(c) if f.this.is_some() || f.called.is_some() => {
            format!("{}::{}", php_vm::object::display_name(&c.name), n)
        }
        _ => n.to_string_lossy(),
    })
}

/// `php_error_docref(NULL, level, "%s", message)`: while PHP code runs this
/// is the VM's docref; otherwise (request startup/shutdown, shutdown
/// functions) the message is reported "in Unknown on line 0" with the
/// active function or the phase name as prefix.
pub(crate) fn docref(vm: &mut Vm, level: i64, message: &[u8]) -> Result<(), Throw> {
    if vm.is_executing() {
        return vm.php_error_bytes(level, message);
    }
    let prefix = active_builtin(vm).map_or_else(
        || {
            match phase() {
                Phase::Startup => "PHP Startup",
                Phase::RequestStartup => "PHP Request Startup",
                Phase::Shutdown => "PHP Request Shutdown",
                Phase::Running => "Unknown",
            }
            .to_string()
        },
        |f| format!("{f}()"),
    );
    let mut m = format!("{prefix}: ").into_bytes();
    m.extend_from_slice(message);
    if phase() != Phase::Startup {
        return vm.error_at_bytes(level, &m, &ZStr::from("Unknown"), 0);
    }
    // Startup errors are written before the SAPI is activated: they do
    // not count as sent headers (`SG(headers_sent)` is reset by
    // `sapi_activate()`), and an `E_ERROR` does not stop the request.
    let sent = vm.headers_sent;
    let status = vm.exit_status;
    let r = vm.error_at_bytes(level, &m, &ZStr::from("Unknown"), 0);
    vm.headers_sent = sent;
    if r.is_err() && level == 1 {
        vm.exit_status = status;
        php_vm::object::set_destructors_enabled(true);
        return Ok(());
    }
    r
}

/// `zend_error(level, message)` ("in Unknown on line 0" outside PHP code).
fn plain_error(vm: &mut Vm, level: i64, message: &str) -> Result<(), Throw> {
    if vm.is_executing() {
        return vm.raise(level, message);
    }
    if phase() != Phase::Startup {
        return vm.error_at(level, message, &ZStr::from("Unknown"), 0);
    }
    // As in `docref`: a startup error does not count as sent headers, or
    // every later directive would fail `check_state`.
    let sent = vm.headers_sent;
    let r = vm.error_at(level, message, &ZStr::from("Unknown"), 0);
    vm.headers_sent = sent;
    r
}

/// The VM seen as a [`SessionHost`]; owns the session state while an entry
/// point runs.
pub(crate) struct VmHost<'a> {
    pub(crate) vm: &'a mut Vm,
    state: Option<Box<SessionState>>,
}

/// Runs `f` with the session state un-parked, then synchronises the VM's
/// INI registry with the session directives and parks the state again.
pub(crate) fn with_host<R>(vm: &mut Vm, f: impl FnOnce(&mut VmHost<'_>) -> R) -> R {
    let mut h = VmHost {
        vm,
        state: Some(take_parked()),
    };
    let r = f(&mut h);
    crate::ini::sync(&mut h);
    if let Some(s) = h.state.take() {
        put_parked(s);
    }
    r
}

/// [`with_host`] without the INI synchronisation (for the INI modify
/// hooks: the VM stores the value itself afterwards).
pub(crate) fn with_host_unsynced<R>(vm: &mut Vm, f: impl FnOnce(&mut VmHost<'_>) -> R) -> R {
    let mut h = VmHost {
        vm,
        state: Some(take_parked()),
    };
    let r = f(&mut h);
    if let Some(s) = h.state.take() {
        put_parked(s);
    }
    r
}

impl VmHost<'_> {
    /// The state (always present outside [`VmHost::parked`]).
    pub(crate) fn state(&mut self) -> &mut SessionState {
        self.state
            .get_or_insert_with(|| Box::new(SessionState::new()))
    }

    /// Runs `f` (which may execute PHP code that re-enters the module) with
    /// the state parked.
    pub(crate) fn parked<R>(&mut self, f: impl FnOnce(&mut Vm) -> R) -> R {
        if let Some(s) = self.state.take() {
            put_parked(s);
        }
        let r = f(self.vm);
        self.state = Some(take_parked());
        r
    }

    /// Converts a library result into a VM result.
    pub(crate) fn vm_result<T>(&mut self, r: RtResult<T>) -> Result<T, Throw> {
        r.map_err(|e| to_throw(self.vm, e))
    }
}

impl RuntimeContext for VmHost<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.parked(|vm| plain_error(vm, level.bits(), message).map_err(|t| to_rt(vm, t)))
    }
    fn precision(&self) -> i64 {
        self.vm.precision()
    }
    fn serialize_precision(&self) -> i64 {
        self.vm.serialize_precision()
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        self.parked(|vm| vm.cast_object(obj, target))
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        self.parked(|vm| vm.do_operation(op, op1, op2))
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        self.parked(|vm| vm.compare_objects(op1, op2))
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        self.parked(|vm| vm.object_properties(obj, purpose))
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        self.parked(|vm| vm.object_dump(obj, purpose))
    }
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        self.parked(|vm| vm.serialize_object(obj))
    }
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        self.parked(|vm| vm.unserialize_object_create(class))
    }
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        self.parked(|vm| vm.unserialize_object_finish(obj, props))
    }
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        self.parked(|vm| vm.unserialize_enum(class, case))
    }
    fn unserialize_enum_case(
        &mut self,
        class: &ZStr,
        case: &ZStr,
    ) -> RtResult<php_runtime::serialize::EnumCaseLookup> {
        self.parked(|vm| vm.unserialize_enum_case(class, case))
    }
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.parked(|vm| vm.unserialize_custom(class, data))
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        self.parked(|vm| vm.count_object(obj))
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        self.parked(|vm| vm.property_read(obj, name))
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        self.parked(|vm| vm.property_write(obj, name, value))
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        self.parked(|vm| vm.create_std_object(props))
    }
}

impl SessionHost for VmHost<'_> {
    fn session(&mut self) -> &mut SessionState {
        self.state()
    }

    fn unserialize_options(&mut self) -> php_runtime::serialize::UnserializeOptions {
        self.vm.unserialize_options_in_effect()
    }

    fn begin_unserialize(&mut self) -> bool {
        self.vm.begin_unserialize_context();
        true
    }

    fn end_unserialize(&mut self, run: bool, bailout: bool) -> RtResult<()> {
        self.parked(|vm| {
            vm.end_unserialize_context(run, bailout)
                .map_err(|t| to_rt(vm, t))
        })
    }

    fn unserialize_shared(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> Option<RtResult<Result<(Value, usize), usize>>> {
        if !self.vm.unserialize_context_active() {
            return None;
        }
        Some(self.parked(|vm| {
            vm.unserialize_nested(data, offset)
                .map_err(|t| to_rt(vm, t))
        }))
    }

    fn unserialize_shared_cell(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> Option<RtResult<Result<(Value, usize), usize>>> {
        if !self.vm.unserialize_context_active() {
            return None;
        }
        Some(self.parked(|vm| {
            vm.unserialize_nested_cell(data, offset)
                .map_err(|t| to_rt(vm, t))
        }))
    }

    fn unwrap_unserialized_cells(&mut self, values: &mut [Value]) {
        self.vm.unwrap_unserialized_cells(values);
    }

    fn serialize_shared(
        &mut self,
        vars: &ZArray,
        keys: Option<&[ZStr]>,
    ) -> Option<RtResult<Vec<Vec<u8>>>> {
        let whole;
        let values: Vec<&Value> = if let Some(keys) = keys {
            keys.iter()
                .filter_map(|k| vars.get_str(k.as_bytes()))
                .collect()
        } else {
            whole = Value::Array(vars.clone());
            vec![&whole]
        };
        Some(self.parked(|vm| vm.serialize_values(&values).map_err(|t| to_rt(vm, t))))
    }

    fn emit_diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.parked(|vm| docref(vm, level.bits(), message.as_bytes()).map_err(|t| to_rt(vm, t)))
    }

    fn emit_diagnostic_after_bailout(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.parked(|vm| {
            let mut m = b"Unknown: ".to_vec();
            m.extend_from_slice(message.as_bytes());
            vm.error_at_bytes(level.bits(), &m, &ZStr::from("Unknown"), 0)
                .map_err(|t| to_rt(vm, t))
        })
    }

    fn call_handler(&mut self, callable: &Value, args: &[Value]) -> RtResult<Option<Value>> {
        self.parked(|vm| {
            // With no frame at all (request shutdown) an exception cannot
            // propagate: `zend_call_function()` reports it as uncaught
            // right away and bails out.
            let toplevel = vm.frames.is_empty();
            match vm.call_value(callable, args.to_vec()) {
                Ok(v) => Ok(Some(v)),
                Err(Throw::Exception(o)) if toplevel => {
                    crate::shutdown::report_uncaught(vm, &o);
                    Err(to_rt(vm, Throw::Fatal))
                }
                Err(t) => Err(to_rt(vm, t)),
            }
        })
    }

    fn callable_name(&mut self, callable: &Value) -> String {
        crate::callable_name(self.vm, callable)
    }

    fn check_callable(&mut self, callable: &Value) -> Result<(), String> {
        let ok = self.parked(|vm| vm.is_callable(callable));
        if ok {
            Ok(())
        } else {
            Err(self.parked(|vm| vm.callable_error_message(callable)))
        }
    }

    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool {
        class_of(obj).instance_of_name(class.to_ascii_lowercase().as_bytes())
    }

    fn has_method(&mut self, obj: &Obj, method_lc: &str) -> bool {
        class_of(obj).find_method(method_lc.as_bytes()).is_some()
    }

    fn bind_session_global(&mut self, slot: Option<PhpRef>) {
        let old = {
            let mut g = self.vm.globals.borrow_mut();
            let i = g.find_or_add(b"_SESSION") as usize;
            std::mem::replace(&mut g.slots[i], slot.map(Value::Reference))
        };
        // Release the previous value outside the borrow (destructors).
        drop(old);
    }

    fn headers_sent(&self) -> bool {
        self.vm.headers_sent
    }

    fn output_start(&self) -> Option<(String, u32)> {
        self.vm
            .output_started()
            .map(|(file, line)| (file.to_string_lossy(), line))
    }

    fn add_header(&mut self, line: &[u8], replace: bool) {
        crate::headers::add(line, replace);
        // `sapi_add_header_ex()` through the active SAPI: `php -S` and
        // apache2handler send it, the CLI drops it.
        let r = self.parked(|vm| {
            vm.add_response_header(line, replace)
                .map(drop)
                .map_err(|t| to_rt(vm, t))
        });
        if let Err(e) = r {
            self.state().set_exception(e);
        }
    }

    fn delete_header_prefix(&mut self, prefix: &[u8]) {
        crate::headers::delete_prefix(prefix);
        let r = self
            .parked(|vm| crate::headers::sapi_delete_prefix(vm, prefix).map_err(|t| to_rt(vm, t)));
        if let Err(e) = r {
            self.state().set_exception(e);
        }
    }

    fn request_global(&mut self, name: &str) -> Option<ZArray> {
        let mut g = self.vm.globals.borrow_mut();
        let i = g.find(name.as_bytes())? as usize;
        match g.slots.get(i).and_then(Option::as_ref).map(Value::deref) {
            Some(Value::Array(a)) => Some(a),
            _ => None,
        }
    }

    fn server_var(&mut self, name: &str) -> Option<Value> {
        let server = self.request_global("_SERVER")?;
        server
            .get(&php_runtime::ArrayKey::from(name))
            .map(Value::deref)
    }

    fn script_path(&self) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let p = self.vm.script_path.as_bytes();
        (!p.is_empty()).then(|| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(p)))
    }

    fn executing_location(&self) -> Option<(String, u32)> {
        if !self.vm.is_executing() {
            return None;
        }
        let (file, line) = self.vm.current_location();
        Some((file.to_string_lossy(), line))
    }

    fn temp_dir(&self) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        match self.vm.ini_string("sys_temp_dir") {
            Some(mut d) if !d.is_empty() => {
                while d.len() > 1 && d.last() == Some(&b'/') {
                    d.pop();
                }
                std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&d))
            }
            _ => std::env::temp_dir(),
        }
    }

    fn open_basedir_denied(&mut self, path: &[u8]) -> bool {
        let basedir = self.vm.ini_string("open_basedir").unwrap_or_default();
        if basedir.is_empty() {
            return false;
        }
        // `PG(open_basedir)` holds the directories a runtime `ini_set()`
        // expanded (`OnUpdateBaseDir`), not the INI string.
        let basedir = php_ext_streams::ini::effective_open_basedir(basedir);
        php_streams::state::update_settings(|s| s.open_basedir.clone_from(&basedir));
        if php_streams::paths::check_open_basedir_ex(path, false) {
            return false;
        }
        let mut msg = b"open_basedir restriction in effect. File(".to_vec();
        msg.extend_from_slice(path);
        msg.extend_from_slice(b") is not within the allowed path(s): (");
        msg.extend_from_slice(&basedir);
        msg.push(b')');
        let r = self.parked(|vm| docref(vm, 2, &msg).map_err(|t| to_rt(vm, t)));
        if let Err(e) = r {
            self.state().set_exception(e);
        }
        true
    }

    fn url_rewrite_session_var(&mut self, name: &[u8], id: &[u8]) {
        crate::rewrite::session_var(self.vm, name, id);
    }

    fn define_sid(&mut self, value: &[u8]) {
        // An existing `SID` (ours or a user constant) only gets its value
        // replaced; a new one is `#[\Deprecated]`.
        let name = ZStr::from("SID");
        let existed = self
            .vm
            .constants
            .insert(name.clone(), Value::Str(ZStr::from(value)))
            .is_some();
        if !existed {
            self.vm.deprecated_constants.insert(
                name,
                b" since 8.4, as GET/POST sessions were deprecated".to_vec(),
            );
        }
    }

    fn register_session_shutdown(&mut self) -> bool {
        crate::shutdown::register(self.vm);
        true
    }

    fn remove_session_shutdown(&mut self) {
        crate::shutdown::remove(self.vm);
    }

    fn append_write_close_shutdown(&mut self) -> bool {
        crate::shutdown::append_write_close(self.vm);
        true
    }

    fn alter_foreign_ini(&mut self, name: &str, value: &[u8]) -> bool {
        if !self.vm.ini.contains(name) {
            return false;
        }
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        let ok = self.vm.ini.alter(
            name,
            value,
            php_support::ini::Access::USER,
            php_support::ini::Stage::Runtime,
            false,
            &mut sink,
        );
        for d in sink {
            let r = self.parked(|vm| docref(vm, i64::from(d.level.0), d.message.as_bytes()));
            if let Err(t) = r {
                let e = to_rt(self.vm, t);
                self.state().set_exception(e);
            }
        }
        ok
    }
}
