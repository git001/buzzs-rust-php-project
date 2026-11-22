// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmHost`]: the VM seen through the [`PdoHost`] trait of `php-pdo` and
//! the [`php_sqlite::Host`] trait of `php-sqlite`, plus the conversions
//! between VM throws and the error types of those crates.

use php_pdo::{ClassInfo, Pdo, PdoException, PdoHost, PdoRow, PdoStatement, PersistentList};
use php_runtime::serialize::SerializedObject;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray, ZStr,
};
use php_sqlite::{ExceptionClass, PhpError};
use php_vm::class::{cflags, Class};
use php_vm::func::flags;
use php_vm::object::{class_of, data};
use php_vm::vm::FrameKind;
use php_vm::{Throw, Vm, VmResult};
use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    /// `EG(persistent_list)` entries of PDO: lives as long as the process
    /// (persistent connections survive requests).
    static PERSISTENT: RefCell<Option<PersistentList>> = const { RefCell::new(None) };
    /// A warning raised while `PDO` connects (`EH_THROW`), already turned
    /// into its `PDOException`: the previous exception of the next
    /// `PDOException`.
    static EH_THROW_PENDING: RefCell<Option<Obj>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Error conversions
// ---------------------------------------------------------------------------

/// Converts a VM throw into a runtime error. Bailouts (`exit`, fatal
/// errors) are parked in [`Vm::pending_throw`] and come back out of
/// [`rt_throw`].
pub fn to_rt(vm: &mut Vm, t: Throw) -> RuntimeError {
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
/// for errors raised by the library crates.
pub fn rt_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        RuntimeError::Thrown(Value::Object(o)) => Throw::Exception(o),
        RuntimeError::Thrown(_) => Throw::Fatal,
        // Recorded and pending: the VM reports it as the error leaves the builtin.
        RuntimeError::MemoryLimit(_) => Throw::Fatal,
        RuntimeError::Fatal(m) => {
            if m.is_empty() {
                Throw::Fatal
            } else {
                match vm.raise(1, &m) {
                    Err(t) => t,
                    Ok(()) => Throw::Fatal,
                }
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

/// Converts an error of `php-sqlite` into a VM throw (with its previous
/// chain and code).
pub fn sqlite_throw(vm: &mut Vm, e: PhpError) -> Throw {
    match e {
        PhpError::Runtime(r) => rt_throw(vm, r),
        PhpError::Exception(ex) => {
            let t = vm.throw(ex.class.name(), &ex.message);
            if let Throw::Exception(o) = &t {
                if ex.code != 0 && ex.class != ExceptionClass::Error {
                    vm.set_property_raw(o, "code", Value::Int(ex.code));
                }
                if let Some(prev) = ex.previous {
                    if let Throw::Exception(p) = sqlite_throw(vm, *prev) {
                        set_previous(o, &p);
                    }
                }
            }
            t
        }
    }
}

/// Reads a declared or dynamic property without any checks.
pub fn raw_prop(o: &Obj, name: &str) -> Option<Value> {
    let cls = class_of(o).clone();
    let n = ZStr::from(name);
    if let Some(pi) = cls.props.iter().rev().find(|p| p.name == n) {
        return data(o)
            .slots
            .borrow()
            .get(pi.slot as usize)
            .cloned()
            .flatten();
    }
    None
}

/// `zend_exception_set_previous()`: appends `prev` at the end of `exc`'s
/// previous chain (ignoring cycles).
pub fn set_previous(exc: &Obj, prev: &Obj) {
    if Obj::ptr_eq(exc, prev) {
        return;
    }
    let mut cur = exc.clone();
    for _ in 0..10_000 {
        match raw_prop(&cur, "previous") {
            Some(Value::Object(p)) => {
                if Obj::ptr_eq(&p, prev) {
                    return;
                }
                cur = p;
            }
            _ => break,
        }
    }
    let cls = class_of(&cur).clone();
    let n = ZStr::from("previous");
    if let Some(pi) = cls.props.iter().rev().find(|p| p.name == n) {
        if let Some(slot) = data(&cur).slots.borrow_mut().get_mut(pi.slot as usize) {
            *slot = Some(Value::Object(prev.clone()));
        }
    }
}

/// `get_active_function_or_method_name()`: the innermost native frame.
pub fn active_function_name(vm: &Vm) -> String {
    vm.frames
        .iter()
        .rev()
        .find(|f| f.kind == FrameKind::Builtin)
        .and_then(|f| {
            let n = f.builtin_name.clone()?;
            Some(match &f.scope {
                Some(c) if f.this.is_some() || f.called.is_some() => {
                    format!("{}::{}", c.name.to_string_lossy(), n.to_string_lossy())
                }
                _ => n.to_string_lossy(),
            })
        })
        .unwrap_or_default()
}

/// `zend_argument_value_error()` for native methods:
/// `Class::method(): Argument #n ($name) msg` (`n` is 1-based).
pub fn arg_value_error(vm: &mut Vm, a: &php_vm::ext::Args, n: usize, msg: &str) -> Throw {
    let func = active_function_name(vm);
    let name = a
        .func
        .as_ref()
        .and_then(|f| f.params.get(n.saturating_sub(1)))
        .map_or_else(String::new, |p| p.name.to_string_lossy());
    vm.throw(
        "ValueError",
        &format!("{func}(): Argument #{n} (${name}) {msg}"),
    )
}

/// `USED_RET()`: whether the caller uses the return value of the running
/// native function (`false` for a call statement such as `$db->query(...);`).
pub fn used_ret(vm: &Vm) -> bool {
    let Some(b) = vm.frames.iter().rposition(|f| f.kind == FrameKind::Builtin) else {
        return true;
    };
    let Some(caller) = b.checked_sub(1).and_then(|i| vm.frames.get(i)) else {
        return true;
    };
    if caller.kind == FrameKind::Builtin {
        return true;
    }
    match caller
        .ip
        .checked_sub(1)
        .and_then(|i| caller.code.ops.get(i))
    {
        Some(php_vm::opcode::Op::DoCall { dst }) => dst.is_some(),
        _ => true,
    }
}

/// `object_init_ex()`: creates an instance without running its
/// constructor, refusing abstract and other non-instantiable classes.
pub fn instantiate(vm: &mut Vm, cls: &Rc<Class>) -> VmResult<Obj> {
    let kind = if cls.is_interface() {
        Some("interface")
    } else if cls.is_trait() {
        Some("trait")
    } else if cls.is_enum() {
        Some("enum")
    } else if cls.flags & cflags::ABSTRACT != 0 {
        Some("abstract class")
    } else {
        None
    };
    if let Some(k) = kind {
        return Err(vm.throw(
            "Error",
            &format!("Cannot instantiate {k} {}", cls.name.to_string_lossy()),
        ));
    }
    if cls.flags & cflags::NO_NEW != 0 {
        return Err(vm.throw(
            "Error",
            &format!(
                "Instantiation of class {} is not allowed",
                cls.name.to_string_lossy()
            ),
        ));
    }
    vm.new_object(cls)
}

/// Writes a property as code running in `scope` would
/// (`zend_update_property_ex(scope, ...)`).
pub fn write_property_scoped(
    vm: &mut Vm,
    obj: &Obj,
    scope: Option<&Rc<Class>>,
    name: &ZStr,
    value: Value,
) -> VmResult<()> {
    let name = name.to_string_lossy();
    match scope {
        Some(s) => vm.set_property_scoped(obj, &name, value, s),
        None => vm.set_property(obj, &name, value),
    }
}

fn class_info_of(cls: &Rc<Class>) -> ClassInfo {
    let ctor = cls.magic.construct.as_ref();
    ClassInfo {
        name: cls.name.clone(),
        has_constructor: ctor.is_some(),
        constructor_is_public: ctor
            .is_some_and(|m| m.flags & (flags::PRIVATE | flags::PROTECTED) == 0),
        can_unserialize: cls.instance_of_name(b"serializable"),
    }
}

/// `zend_fcc_dup()` of a callable validated in the caller's scope: method
/// callables (`[$this, 'privateMethod']`, `"Class::method"`) are bound to
/// a `Closure` now, so that calling them later (from a database callback,
/// in another scope) still works.
pub fn bind_callable(vm: &mut Vm, callable: Value) -> Value {
    let is_method = match &callable {
        Value::Array(_) => true,
        Value::Str(s) => s.as_bytes().windows(2).any(|w| w == b"::"),
        _ => false,
    };
    if !is_method {
        return callable;
    }
    match vm.call_value(
        &Value::from("Closure::fromCallable"),
        vec![callable.clone()],
    ) {
        Ok(v @ Value::Object(_)) => v,
        _ => callable,
    }
}

/// The handle stored in a `PDO` object.
pub fn pdo_of(o: &Obj) -> Option<Pdo> {
    php_vm::object::internal::<Pdo, _>(o, |p| p.clone())
}

/// The handle stored in a `PDOStatement` object.
pub fn stmt_of(o: &Obj) -> Option<PdoStatement> {
    php_vm::object::internal::<PdoStatement, _>(o, |p| p.clone())
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// The VM seen through [`PdoHost`] and [`php_sqlite::Host`].
pub struct VmHost<'a> {
    /// The VM.
    pub vm: &'a mut Vm,
    persistent: Option<PersistentList>,
}

impl std::fmt::Debug for VmHost<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmHost")
    }
}

impl<'a> VmHost<'a> {
    /// Wraps the VM.
    pub fn new(vm: &'a mut Vm) -> Self {
        VmHost {
            vm,
            persistent: None,
        }
    }

    fn rt<T>(&mut self, r: VmResult<T>) -> RtResult<T> {
        r.map_err(|t| to_rt(self.vm, t))
    }

    fn docref_impl(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        let r = self.vm.php_error_bytes(level.bits(), message.as_bytes());
        self.rt(r)
    }

    fn find(&mut self, class: &ZStr) -> RtResult<Rc<Class>> {
        match self.vm.find_class(class.as_bytes()) {
            Some(c) => Ok(c),
            None => Err(RuntimeError::Error(format!(
                "Class \"{}\" not found",
                class.to_string_lossy()
            ))),
        }
    }

    fn new_instance(&mut self, class: &ZStr) -> RtResult<Obj> {
        let cls = self.find(class)?;
        let r = instantiate(self.vm, &cls);
        self.rt(r)
    }
}

impl Drop for VmHost<'_> {
    fn drop(&mut self) {
        // `try_with`: a host dropped while the thread-locals are being
        // destroyed (process exit) must not panic
        let _ = EH_THROW_PENDING.try_with(|p| p.borrow_mut().take());
        if let Some(p) = self.persistent.take() {
            let _ = PERSISTENT.try_with(|s| {
                let mut s = s.borrow_mut();
                if s.is_none() {
                    *s = Some(p);
                }
            });
        }
    }
}

/// Runs `f` with a [`VmHost`] over `vm`, converting the error.
pub fn with_host<T>(vm: &mut Vm, f: impl FnOnce(&mut VmHost<'_>) -> RtResult<T>) -> VmResult<T> {
    let r = {
        let mut host = VmHost::new(vm);
        f(&mut host)
    };
    r.map_err(|e| rt_throw(vm, e))
}

/// Runs `f` with a [`VmHost`] over `vm`, converting a `php-sqlite` error.
pub fn with_sqlite<T>(
    vm: &mut Vm,
    f: impl FnOnce(&mut VmHost<'_>) -> Result<T, PhpError>,
) -> VmResult<T> {
    let r = {
        let mut host = VmHost::new(vm);
        f(&mut host)
    };
    r.map_err(|e| sqlite_throw(vm, e))
}

impl RuntimeContext for VmHost<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.vm.diagnostic(level, message)
    }
    fn precision(&self) -> i64 {
        self.vm.precision()
    }
    fn serialize_precision(&self) -> i64 {
        self.vm.serialize_precision()
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        self.vm.cast_object(obj, target)
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        self.vm.do_operation(op, op1, op2)
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        self.vm.compare_objects(op1, op2)
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        self.vm.object_properties(obj, purpose)
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        self.vm.object_dump(obj, purpose)
    }
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        self.vm.serialize_object(obj)
    }
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        self.vm.unserialize_object_create(class)
    }
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        self.vm.unserialize_object_finish(obj, props)
    }
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        self.vm.unserialize_enum(class, case)
    }
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.vm.unserialize_custom(class, data)
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        RuntimeContext::count_object(self.vm, obj)
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        self.vm.property_read(obj, name)
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        self.vm.property_write(obj, name, value)
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        self.vm.create_std_object(props)
    }
}

impl PdoHost for VmHost<'_> {
    fn call(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        let r = self.vm.call_value(callable, args.to_vec());
        self.rt(r)
    }

    fn check_callable(&mut self, callable: &Value) -> Result<(), String> {
        if self.vm.is_callable(callable) {
            Ok(())
        } else {
            Err(self.vm.callable_error_message(callable))
        }
    }

    fn check_callable_with_deprecations(
        &mut self,
        callable: &Value,
    ) -> RtResult<Result<(), String>> {
        let r = self.vm.resolve_callable(callable);
        Ok(self.rt(r)?.map(|_| ()))
    }

    fn docref(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.docref_impl(level, message)
    }

    fn docref_ex(
        &mut self,
        level: ErrorLevel,
        message: &str,
        exception_pending: bool,
    ) -> RtResult<()> {
        // zend_error_impl(): with an exception pending the user error
        // handler cannot run, and nothing is displayed either.
        if exception_pending
            && self
                .vm
                .error_handlers
                .last()
                .is_some_and(|h| !h.callback.is_null() && h.mask & level.bits() != 0)
        {
            return Ok(());
        }
        self.docref_impl(level, message)
    }

    fn active_function_name(&self) -> String {
        active_function_name(self.vm)
    }

    fn lookup_class(&mut self, name: &ZStr) -> Option<ClassInfo> {
        let cls = match self.vm.lookup_class(name) {
            Ok(c) => c,
            Err(t) => {
                if self.vm.pending_throw.is_none() {
                    self.vm.pending_throw = Some(t);
                }
                None
            }
        }?;
        Some(class_info_of(&cls))
    }

    fn instance_of(&mut self, class: &ZStr, parent: &str) -> bool {
        let Some(cls) = self.vm.find_class(class.as_bytes()) else {
            return class.as_bytes().eq_ignore_ascii_case(parent.as_bytes());
        };
        let lc = parent.trim_start_matches('\\').to_ascii_lowercase();
        cls.instance_of_name(lc.as_bytes())
    }

    fn instantiate(&mut self, class: &ZStr) -> RtResult<Value> {
        self.new_instance(class).map(Value::Object)
    }

    fn call_constructor(&mut self, obj: &Value, args: Option<&ZArray>) -> RtResult<()> {
        let Value::Object(o) = obj else {
            return Ok(());
        };
        if class_of(o).magic.construct.is_none() {
            return Ok(());
        }
        // String keys are named arguments (`zend_call_known_function`
        // with `named_params`).
        let mut positional = Vec::new();
        let mut named = Vec::new();
        for (k, v) in args
            .map(|a| a.iter().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            match k {
                php_runtime::ArrayKey::Str(n) => named.push((n.clone(), v.clone())),
                php_runtime::ArrayKey::Int(_) => {
                    if !named.is_empty() {
                        return Err(RuntimeError::Error(
                            "Cannot use positional argument after named argument".into(),
                        ));
                    }
                    positional.push(v.clone());
                }
            }
        }
        let r = if named.is_empty() {
            self.vm.call_method_by_name(o, "__construct", positional)
        } else {
            self.vm
                .call_method_named(o, "__construct", positional, named)
        };
        self.rt(r).map(|_| ())
    }

    fn update_property(
        &mut self,
        obj: &Value,
        scope: Option<&ZStr>,
        name: &ZStr,
        value: Value,
    ) -> RtResult<()> {
        let Value::Object(o) = obj else {
            return Ok(());
        };
        let scope = scope.and_then(|s| self.vm.find_class(s.as_bytes()));
        let r = write_property_scoped(self.vm, o, scope.as_ref(), name, value);
        self.rt(r)
    }

    fn unserialize_class(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.vm.unserialize_custom(class, data)
    }

    fn instantiate_statement(&mut self, class: &ZStr) -> RtResult<(Value, PdoStatement)> {
        let o = self.new_instance(class)?;
        let st = stmt_of(&o).unwrap_or_else(|| {
            let s = PdoStatement::new();
            s.attach_object(&o);
            php_vm::object::set_internal(&o, s.clone());
            s
        });
        Ok((Value::Object(o), st))
    }

    fn instantiate_pdo(&mut self, class: &ZStr) -> RtResult<(Value, Pdo)> {
        let o = self.new_instance(class)?;
        let p = pdo_of(&o).unwrap_or_else(|| {
            let p = Pdo::new();
            p.attach_object(&o);
            php_vm::object::set_internal(&o, p.clone());
            p
        });
        Ok((Value::Object(o), p))
    }

    fn std_write_property(&mut self, obj: &Value, name: &ZStr, value: Value) -> RtResult<()> {
        if let Value::Object(o) = obj {
            self.vm.set_property_raw(o, &name.to_string_lossy(), value);
        }
        Ok(())
    }

    fn create_row(&mut self, row: PdoRow) -> RtResult<Value> {
        let r = crate::stmt::create_row(self.vm, row);
        self.rt(r).map(Value::Object)
    }

    fn new_pdo_exception(&mut self, exception: PdoException) -> RuntimeError {
        let t = self.vm.throw("PDOException", &exception.message);
        if let Throw::Exception(o) = &t {
            if let Some(prev) = EH_THROW_PENDING.with(|p| p.borrow_mut().take()) {
                set_previous(o, &prev);
            }
            self.vm.set_property_raw(o, "code", exception.code);
            if let Some(info) = exception.error_info {
                self.vm.set_property_raw(o, "errorInfo", Value::Array(info));
            }
        }
        to_rt(self.vm, t)
    }

    fn chain_exception(&mut self, exception: RuntimeError, previous: RuntimeError) -> RuntimeError {
        let te = rt_throw(self.vm, exception);
        let tp = rt_throw(self.vm, previous);
        match (te, tp) {
            (Throw::Exception(e), Throw::Exception(p)) => {
                set_previous(&e, &p);
                RuntimeError::Thrown(Value::Object(e))
            }
            (Throw::Exception(_), other) => to_rt(self.vm, other),
            (te, _) => to_rt(self.vm, te),
        }
    }

    fn read_stream(&mut self, value: &Value) -> Option<Vec<u8>> {
        read_stream(self.vm, value)
    }

    fn memory_stream(&mut self, data: ZStr) -> RtResult<Value> {
        let s = php_streams::memory::memory_open(
            php_streams::mode::TempMode::ReadOnly,
            data.into_bytes(),
        );
        Ok(crate::streams::new_stream(s))
    }

    fn read_uri_line(&mut self, uri: &[u8], max: usize) -> Option<Vec<u8>> {
        let r = crate::streams::run(self.vm, || {
            let mut opened = None;
            php_streams::open::open_wrapper(
                uri,
                "rb",
                php_streams::wrapper::REPORT_ERRORS,
                &mut opened,
                None,
            )
            .and_then(|mut s| s.get_line(Some(max.saturating_sub(1))))
        });
        match r {
            Ok(line) => line,
            Err(t) => {
                if self.vm.pending_throw.is_none() {
                    self.vm.pending_throw = Some(t);
                }
                None
            }
        }
    }

    fn write_output(&mut self, bytes: &[u8]) -> RtResult<()> {
        self.vm.output(bytes);
        Ok(())
    }

    fn cfg_get_string(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let n = String::from_utf8_lossy(name);
        self.vm.ini.config_value(&n).map(<[u8]>::to_vec)
    }

    fn persistent_list(&mut self) -> &mut PersistentList {
        if self.persistent.is_none() {
            let p = PERSISTENT
                .try_with(|s| s.borrow_mut().take())
                .ok()
                .flatten();
            self.persistent = Some(p.unwrap_or_default());
        }
        self.persistent.get_or_insert_with(PersistentList::new)
    }

    fn open_basedir_active(&self) -> bool {
        open_basedir_active(self.vm)
    }

    fn check_open_basedir(&mut self, path: &[u8]) -> bool {
        check_open_basedir(self.vm, path)
    }
}

fn open_basedir_active(vm: &Vm) -> bool {
    vm.ini_string("open_basedir").is_some_and(|v| !v.is_empty())
}

fn check_open_basedir(vm: &mut Vm, path: &[u8]) -> bool {
    if !open_basedir_active(vm) {
        return false;
    }
    let basedir = vm.ini_string("open_basedir").unwrap_or_default();
    php_streams::state::update_settings(|s| s.open_basedir = basedir);
    let allowed = php_streams::paths::check_open_basedir(path);
    // While PDO connects, warnings become exceptions (EH_THROW in the PDO
    // core, which this path bypasses): the warning turns into the
    // `PDOException` that becomes the previous exception of the next one
    // the connection throws.
    let func = active_function_name(vm);
    if func == "PDO::__construct" || func == "PDO::connect" {
        for d in php_streams::diag::take() {
            let msg = format!("{func}(): {}", d.text());
            let t = vm.throw("PDOException", &msg);
            if let Throw::Exception(o) = t {
                EH_THROW_PENDING.with(|p| *p.borrow_mut() = Some(o));
            }
        }
    }
    let _ = crate::streams::flush_diagnostics(vm);
    !allowed
}

use crate::streams::read_stream;

impl php_sqlite::Host for VmHost<'_> {
    fn call(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        let r = self.vm.call_value(callable, args.to_vec());
        self.rt(r)
    }
    fn docref(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.docref_impl(level, message)
    }
    fn active_function_name(&self) -> String {
        active_function_name(self.vm)
    }
    fn read_stream(&mut self, value: &Value) -> Option<Vec<u8>> {
        read_stream(self.vm, value)
    }
    fn open_basedir_active(&self) -> bool {
        open_basedir_active(self.vm)
    }
    fn check_open_basedir(&mut self, path: &[u8]) -> bool {
        check_open_basedir(self.vm, path)
    }
    fn sqlite3_extension_dir(&self) -> Option<Vec<u8>> {
        self.vm
            .ini_string("sqlite3.extension_dir")
            .filter(|v| !v.is_empty())
    }
    fn sqlite3_defensive(&self) -> bool {
        self.vm.ini_bool("sqlite3.defensive")
    }
}
