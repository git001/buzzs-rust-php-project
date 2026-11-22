// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmHost`]: the [`SoapHost`] implementation over the VM, plus the
//! conversions between VM throws and runtime errors.
//!
//! # The SOAP error handler
//!
//! PHP's ext/soap replaces `zend_error_cb` while a `SoapClient` or
//! `SoapServer` method runs. The VM has no such hook, so the parts that
//! matter are emulated here:
//!
//! * diagnostics raised by `php-soap` itself go through
//!   [`RuntimeContext::diagnostic`], which consults the handler state;
//! * while a `SoapServer` dispatches to user code, `display_errors` is
//!   switched off (what `soap_real_error_handler` does around the original
//!   handler) and a fatal error becomes a fault response whose text is the
//!   error message (`vm.last_error`).

use std::cell::RefCell;
use std::rc::Rc;

use php_runtime::serialize::SerializedObject;
use php_runtime::{
    ArrayKey, BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult,
    RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_soap::globals::{self, ErrorObject};
use php_soap::host::{
    ConnectTarget, EnumKind, HttpConnector, HttpStream, LocalTime, MethodCall, SoapHost,
};
use php_soap::state::SoapObject;
use php_vm::func::flags;
use php_vm::object::{class_of, data};
use php_vm::{Throw, Vm};

/// Class name of the marker error for `exit()` unwinding through the
/// library.
const EXIT_MARK: &[u8] = b"\0soap-exit";
/// Class name of the marker error for `zend_bailout()` after a server
/// fault response was written.
const BAILOUT_MARK: &[u8] = b"\0soap-bailout";

thread_local! {
    /// The message of the last fatal error the VM already reported (so it
    /// is not reported twice when it travels back through the library).
    static REPORTED_FATAL: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Set once a server fault response was written and the request is
    /// bailing out: the VM's `Throw::Fatal` then means "bailout" again when
    /// it unwinds back through the library.
    static BAILING_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// An out-of-memory fatal raised inside the library.
    static OOM: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Records an out-of-memory fatal that must reach the VM unchanged and
/// returns the error the library propagates meanwhile.
pub(crate) fn oom_error(msg: String) -> RuntimeError {
    OOM.with(|o| *o.borrow_mut() = Some(msg));
    RuntimeError::Custom {
        class: ZStr::from(BAILOUT_MARK),
        message: String::new(),
    }
}

/// Takes a pending out-of-memory fatal (see [`oom_error`]); also reports
/// one the allocator recorded while native code ran.
pub(crate) fn take_oom() -> Option<String> {
    OOM.with(|o| o.borrow_mut().take())
        .or_else(|| php_memlimit::take().map(|over| over.to_string()))
}

/// Converts a VM throw into a runtime error.
pub(crate) fn to_rt(vm: &mut Vm, t: Throw) -> RuntimeError {
    match t {
        Throw::Exception(o) => RuntimeError::Thrown(Value::Object(o)),
        Throw::Exit => RuntimeError::Custom {
            class: ZStr::from(EXIT_MARK),
            message: String::new(),
        },
        Throw::Fatal if BAILING_OUT.with(std::cell::Cell::get) => RuntimeError::Custom {
            class: ZStr::from(BAILOUT_MARK),
            message: String::new(),
        },
        Throw::Fatal if !server_context() && last_error_is_oom(vm) => {
            // `soap_real_error_handler` (client): `ZEND_HAS_OOM()` sends the
            // memory-limit fatal to the original handler (the VM already
            // reported it) and the bailout ends the request instead of
            // becoming a SoapFault.
            RuntimeError::Custom {
                class: ZStr::from(BAILOUT_MARK),
                message: String::new(),
            }
        }
        Throw::Fatal => {
            let msg = vm
                .last_error
                .as_ref()
                .map(|e| String::from_utf8_lossy(&e.message).into_owned())
                .unwrap_or_default();
            REPORTED_FATAL.with(|r| *r.borrow_mut() = Some(msg.clone()));
            RuntimeError::Fatal(msg)
        }
        other @ Throw::FiberSuspend => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(other);
            }
            RuntimeError::Fatal(String::new())
        }
    }
}

/// Whether the fatal error the VM just raised is the `memory_limit` one
/// (`zend_handle_delayed_oom`, i.e. `ZEND_HAS_OOM()` is set).
fn last_error_is_oom(vm: &Vm) -> bool {
    vm.last_error.as_ref().is_some_and(|e| {
        // E_ERROR
        e.level == 1 && e.message.starts_with(b"Allowed memory size of ")
    })
}

/// Converts a runtime error into a VM throw, creating exception objects
/// for errors raised by `php-soap` and reporting its fatal errors.
pub(crate) fn to_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    if let Some(msg) = OOM.with(|o| o.borrow_mut().take()) {
        return vm.fatal(&msg);
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        RuntimeError::Thrown(Value::Object(o)) => Throw::Exception(o),
        RuntimeError::Thrown(_) => Throw::Fatal,
        // Recorded and pending: the VM reports it as the error leaves the builtin.
        RuntimeError::MemoryLimit(_) => Throw::Fatal,
        RuntimeError::Fatal(m) => {
            let reported = REPORTED_FATAL.with(|r| r.borrow_mut().take());
            if m.is_empty() || reported.as_deref() == Some(m.as_str()) {
                Throw::Fatal
            } else {
                vm.fatal(&m)
            }
        }
        RuntimeError::Custom { class, .. } if class.as_bytes() == EXIT_MARK => Throw::Exit,
        RuntimeError::Custom { class, .. } if class.as_bytes() == BAILOUT_MARK => {
            BAILING_OUT.with(|b| b.set(true));
            vm.exit_status = 255;
            php_vm::object::set_destructors_enabled(false);
            Throw::Fatal
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

/// Whether a server error context is active: errors are not displayed
/// (`PG(display_errors) = 0` around the original handler).
fn server_context() -> bool {
    globals::with(|g| {
        g.use_soap_error_handler && !matches!(g.error_object, Some(ErrorObject::Client(..)))
    })
}

/// Runs `f` with `display_errors` off when a SOAP server context is active.
pub(crate) fn quiet_if_server<R>(vm: &mut Vm, f: impl FnOnce(&mut Vm) -> R) -> R {
    if server_context() {
        with_display_off(vm, f)
    } else {
        f(vm)
    }
}

/// Runs `f` with `display_errors=0`.
pub(crate) fn with_display_off<R>(vm: &mut Vm, f: impl FnOnce(&mut Vm) -> R) -> R {
    let old = vm.ini.get("display_errors").map(<[u8]>::to_vec);
    set_ini(vm, "display_errors", b"0");
    let r = f(vm);
    set_ini(vm, "display_errors", old.as_deref().unwrap_or(b"1"));
    r
}

/// Runs `f` with `error_reporting = 0`, leaving `error_get_last()`
/// untouched (internal stream operations whose failures PHP reports
/// differently).
pub(crate) fn silenced<R>(vm: &mut Vm, f: impl FnOnce(&mut Vm) -> R) -> R {
    let old = vm.error_reporting;
    let last = vm.last_error.take();
    vm.error_reporting = 0;
    let r = f(vm);
    vm.error_reporting = old;
    vm.last_error = last;
    r
}

fn set_ini(vm: &mut Vm, name: &str, value: &[u8]) {
    let _ = vm.call_value(
        &Value::from("ini_set"),
        vec![Value::from(name), Value::from(value)],
    );
}

/// `"\0Class\0name"` / `"\0*\0name"` / `"name"`.
fn mangle(flags_: u32, class: &ZStr, name: &ZStr) -> ZStr {
    let mut v = Vec::new();
    if flags_ & flags::PRIVATE != 0 {
        v.push(0);
        v.extend_from_slice(class.as_bytes());
        v.push(0);
    } else if flags_ & flags::PROTECTED != 0 {
        v.extend_from_slice(b"\0*\0");
    } else {
        return name.clone();
    }
    v.extend_from_slice(name.as_bytes());
    ZStr::from(v)
}

/// The VM seen as a [`SoapHost`].
pub(crate) struct VmHost<'a> {
    /// The VM.
    pub(crate) vm: &'a mut Vm,
}

impl<'a> VmHost<'a> {
    /// Wraps the VM.
    pub(crate) fn new(vm: &'a mut Vm) -> Self {
        VmHost { vm }
    }

    /// Converts a library result into a VM result.
    pub(crate) fn vm_result<T>(&mut self, r: RtResult<T>) -> Result<T, Throw> {
        r.map_err(|e| to_throw(self.vm, e))
    }

    /// Calls a PHP function by name.
    /// Calls an internal function the way php-src's C code does (not seen
    /// by observers).
    fn call_named(&mut self, name: &str, args: Vec<Value>) -> RtResult<Value> {
        self.vm
            .call_function(name, args)
            .map_err(|t| to_rt(self.vm, t))
    }

    /// The declared property `name` of `obj` as seen from the object's own
    /// class (private properties of parents are skipped).
    fn declared_prop(obj: &Obj, name: &[u8]) -> Option<Rc<php_vm::class::PropInfo>> {
        let cls = class_of(obj);
        cls.props
            .iter()
            .rev()
            .find(|p| {
                p.name.as_bytes() == name
                    && p.flags & flags::STATIC == 0
                    && p.hooks.is_none()
                    && (p.flags & flags::PRIVATE == 0 || p.class.eq_ignore_ascii_case(&cls.name))
            })
            .cloned()
    }

    /// The slot of a property declared by an internal SOAP class (the
    /// `OBJ_PROP_NUM` accessors of the C code).
    fn internal_slot(obj: &Obj, name: &str) -> Option<usize> {
        let cls = class_of(obj);
        cls.props
            .iter()
            .find(|p| {
                p.name.as_bytes() == name.as_bytes()
                    && p.flags & flags::STATIC == 0
                    && is_internal_class(&p.class)
            })
            .map(|p| p.slot as usize)
    }

    /// Iterates an `Iterator`.
    fn iterate_iterator(&mut self, it: &Obj) -> RtResult<Vec<(Value, Value)>> {
        let mut out = Vec::new();
        let vm = &mut *self.vm;
        let call = |vm: &mut Vm, m: &str| {
            vm.call_method_by_name(it, m, Vec::new())
                .map_err(|t| to_rt(vm, t))
        };
        call(vm, "rewind")?;
        loop {
            let valid = call(vm, "valid")?;
            if !valid.to_bool(&mut *vm)? {
                break;
            }
            let v = call(vm, "current")?;
            let k = call(vm, "key")?;
            out.push((k, v));
            call(vm, "next")?;
        }
        Ok(out)
    }
}

/// Whether `name` is one of the classes this extension declares (or the
/// `Exception` base of `SoapFault`).
fn is_internal_class(name: &ZStr) -> bool {
    php_soap::classes::find_class(&name.to_string_lossy()).is_some()
        || name.eq_ignore_ascii_case(b"Exception")
        || name.eq_ignore_ascii_case(b"Error")
}

impl RuntimeContext for VmHost<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        let client_wsdl = globals::with(|g| {
            g.use_soap_error_handler
                && matches!(g.error_object, Some(ErrorObject::Client(_, true)))
                && g.error_code == Some("WSDL")
        });
        if client_wsdl {
            // "Ignore libxml warnings during WSDL parsing".
            return Ok(());
        }
        let vm = &mut *self.vm;
        quiet_if_server(vm, |vm| vm.error(level.bits(), message)).map_err(|t| to_rt(self.vm, t))
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
        self.vm.count_object(obj)
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

impl SoapHost for VmHost<'_> {
    fn soap_state(&mut self, obj: &Obj) -> Option<Rc<SoapObject>> {
        php_vm::object::internal::<Rc<SoapObject>, _>(obj, |s| Rc::clone(s))
    }

    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool {
        class_of(obj).instance_of_name(class.to_ascii_lowercase().as_bytes())
    }

    fn fetch_class(&mut self, name: &[u8]) -> RtResult<Option<ZStr>> {
        match self.vm.lookup_class(&ZStr::from(name)) {
            Ok(Some(c)) => Ok(Some(c.name.clone())),
            Ok(None) => Err(RuntimeError::Error(format!(
                "Class \"{}\" not found",
                String::from_utf8_lossy(name)
            ))),
            Err(t) => Err(to_rt(self.vm, t)),
        }
    }

    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj> {
        let cls = match self.vm.lookup_class(class) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Err(RuntimeError::Error(format!(
                    "Class \"{}\" not found",
                    class.to_string_lossy()
                )))
            }
            Err(t) => return Err(to_rt(self.vm, t)),
        };
        // `object_init_ex` refuses what `new` refuses.
        let kind = if cls.flags & php_vm::ext::class_flags::INTERFACE != 0 {
            Some("interface")
        } else if cls.is_enum() {
            Some("enum")
        } else if cls.flags & php_vm::ext::class_flags::ABSTRACT != 0 {
            Some("abstract class")
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(RuntimeError::Error(format!(
                "Cannot instantiate {kind} {}",
                php_vm::object::display_name(&cls.name).to_string_lossy()
            )));
        }
        self.vm.new_object(&cls).map_err(|t| to_rt(self.vm, t))
    }

    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj> {
        let cls = match self.vm.lookup_class(class) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Err(RuntimeError::Error(format!(
                    "Class \"{}\" not found",
                    class.to_string_lossy()
                )))
            }
            Err(t) => return Err(to_rt(self.vm, t)),
        };
        let args = args.to_vec();
        let vm = &mut *self.vm;
        quiet_if_server(vm, |vm| vm.new_object_with_args(&cls, args)).map_err(|t| to_rt(self.vm, t))
    }

    fn set_object_class(&mut self, _obj: &Obj, _class: &ZStr) {
        // The VM cannot change the class of a live object.
    }

    fn read_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<Option<Value>> {
        if let Some(pi) = Self::declared_prop(obj, name) {
            return Ok(data(obj).slots.borrow()[pi.slot as usize]
                .as_ref()
                .map(Value::deref));
        }
        if let Some(t) = data(obj).dynamic.borrow().as_ref() {
            if let Some(v) = t.get(&ArrayKey::from_bytes(name)) {
                return Ok(Some(v.deref()));
            }
        }
        if class_of(obj).magic.get.is_none() {
            return Ok(None);
        }
        let n = String::from_utf8_lossy(name).into_owned();
        self.vm
            .get_property(obj, &n)
            .map(Some)
            .map_err(|t| to_rt(self.vm, t))
    }

    fn write_property(&mut self, obj: &Obj, name: &[u8], value: Value) -> RtResult<()> {
        if let Some(pi) = Self::declared_prop(obj, name) {
            data(obj).slots.borrow_mut()[pi.slot as usize] = Some(value);
            return Ok(());
        }
        let n = String::from_utf8_lossy(name).into_owned();
        self.vm
            .set_property(obj, &n, value)
            .map_err(|t| to_rt(self.vm, t))
    }

    fn unset_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<()> {
        if let Some(pi) = Self::declared_prop(obj, name) {
            data(obj).slots.borrow_mut()[pi.slot as usize] = None;
            return Ok(());
        }
        if let Some(t) = data(obj).dynamic.borrow_mut().as_mut() {
            t.remove(&ArrayKey::from_bytes(name));
        }
        Ok(())
    }

    fn property_table(&mut self, obj: &Obj) -> ZArray {
        let cls = class_of(obj).clone();
        let mut out = ZArray::new();
        {
            let slots = data(obj).slots.borrow();
            let t = out.make_mut();
            for p in &cls.props {
                if p.flags & flags::STATIC != 0 || p.is_virtual() {
                    continue;
                }
                if let Some(Some(v)) = slots.get(p.slot as usize) {
                    t.insert(ArrayKey::Str(mangle(p.flags, &p.class, &p.name)), v.clone());
                }
            }
        }
        if let Some(d) = data(obj).dynamic.borrow().as_deref() {
            let t = out.make_mut();
            for (k, v) in d {
                t.insert(k.clone(), v.clone());
            }
        }
        out
    }

    fn read_slot(&mut self, obj: &Obj, name: &str) -> Option<Value> {
        let slot = Self::internal_slot(obj, name)?;
        data(obj).slots.borrow().get(slot).cloned().flatten()
    }

    fn write_slot(&mut self, obj: &Obj, name: &str, value: Value) {
        if let Some(slot) = Self::internal_slot(obj, name) {
            if let Some(s) = data(obj).slots.borrow_mut().get_mut(slot) {
                *s = Some(value);
            }
        } else {
            self.vm.set_property_raw(obj, name, value);
        }
    }

    fn enum_kind(&mut self, obj: &Obj) -> EnumKind {
        let cls = class_of(obj).clone();
        if !cls.is_enum() {
            return EnumKind::NotEnum;
        }
        if cls.backing.is_none() {
            return EnumKind::Pure;
        }
        match self.read_property(obj, b"value") {
            Ok(Some(v)) => EnumKind::Backed(v),
            _ => EnumKind::Pure,
        }
    }

    fn iterate(&mut self, obj: &Obj) -> RtResult<Vec<(Value, Value)>> {
        let cls = class_of(obj).clone();
        if cls.lc_name.as_bytes() == b"generator" {
            // `get_iterator` checks for a finished generator first.
            let Value::Array(a) = self.call_named(
                "iterator_to_array",
                vec![Value::Object(obj.clone()), Value::Bool(true)],
            )?
            else {
                return Ok(Vec::new());
            };
            return Ok(a.iter().map(|(k, v)| (k.to_value(), v.clone())).collect());
        }
        if cls.instance_of_name(b"iterator") {
            return self.iterate_iterator(obj);
        }
        if cls.instance_of_name(b"iteratoraggregate") {
            let mut cur = obj.clone();
            for _ in 0..32 {
                let it = self
                    .vm
                    .call_method_by_name(&cur, "getIterator", Vec::new())
                    .map_err(|t| to_rt(self.vm, t))?;
                let Value::Object(it) = it else {
                    return Ok(Vec::new());
                };
                if class_of(&it).instance_of_name(b"iterator") {
                    return self.iterate_iterator(&it);
                }
                cur = it;
            }
        }
        Ok(Vec::new())
    }

    fn format_date(&mut self, obj: &Obj, format: &str) -> Option<Vec<u8>> {
        match self
            .vm
            .call_method_by_name(obj, "format", vec![Value::from(format)])
        {
            Ok(Value::Str(s)) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    fn localtime(&mut self, ts: i64) -> Option<LocalTime> {
        let lt = self
            .call_named("localtime", vec![Value::Int(ts), Value::Bool(true)])
            .ok()?;
        let off = self
            .call_named("date", vec![Value::from("Z"), Value::Int(ts)])
            .ok()?;
        let Value::Array(a) = lt else {
            return None;
        };
        let field = |k: &str| match a.get_str(k.as_bytes()).map(Value::deref) {
            Some(Value::Int(i)) => i,
            _ => 0,
        };
        let n = |v: i64| u32::try_from(v).unwrap_or(0);
        let gmtoff = match off {
            Value::Str(s) => s.to_string_lossy().trim().parse().unwrap_or(0),
            Value::Int(i) => i,
            _ => 0,
        };
        Some(LocalTime {
            year: field("tm_year") + 1900,
            month: n(field("tm_mon") + 1),
            day: n(field("tm_mday")),
            hour: n(field("tm_hour")),
            minute: n(field("tm_min")),
            second: n(field("tm_sec")),
            gmtoff,
        })
    }

    fn exception_object(&mut self, err: &RuntimeError) -> Option<Obj> {
        match err {
            RuntimeError::Thrown(Value::Object(o)) => Some(o.clone()),
            RuntimeError::Thrown(_) | RuntimeError::Fatal(_) => None,
            RuntimeError::Custom { class, .. }
                if class.as_bytes() == EXIT_MARK || class.as_bytes() == BAILOUT_MARK =>
            {
                None
            }
            other => match to_throw(self.vm, other.clone()) {
                Throw::Exception(o) => Some(o),
                _ => None,
            },
        }
    }

    fn is_unwind_exit(&mut self, err: &RuntimeError) -> bool {
        matches!(err, RuntimeError::Custom { class, .. }
            if class.as_bytes() == EXIT_MARK || class.as_bytes() == BAILOUT_MARK)
    }

    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        let args = args.to_vec();
        let vm = &mut *self.vm;
        quiet_if_server(vm, |vm| vm.call_value(callable, args)).map_err(|t| to_rt(self.vm, t))
    }

    fn call_function(&mut self, name: &[u8], args: &[Value]) -> RtResult<Option<Value>> {
        if self.function_exists(name).is_none() {
            return Ok(None);
        }
        let callable = Value::from(name);
        self.call_callable(&callable, args).map(Some)
    }

    fn function_exists(&mut self, name: &[u8]) -> Option<ZStr> {
        let lc = ZStr::from(name.to_ascii_lowercase());
        self.vm.functions.get(&lc).map(|f| f.name().clone())
    }

    fn defined_functions(&mut self) -> Vec<ZStr> {
        let Ok(Value::Array(a)) = self.call_named("get_defined_functions", Vec::new()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for key in [&b"internal"[..], b"user"] {
            if let Some(Value::Array(list)) = a.get_str(key).map(Value::deref) {
                for v in list.values() {
                    if let Value::Str(s) = v.deref() {
                        let canon = self.function_exists(s.as_bytes()).unwrap_or(s);
                        out.push(canon);
                    }
                }
            }
        }
        out
    }

    fn class_methods(&mut self, class: &ZStr) -> Vec<(ZStr, bool)> {
        let Some(cls) = self.vm.find_class(class.as_bytes()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen: Vec<ZStr> = Vec::new();
        let mut cur = Some(cls);
        while let Some(c) = cur {
            for lc in &c.method_order {
                if let Some(m) = c.methods.get(lc) {
                    if !seen.contains(lc) {
                        seen.push(lc.clone());
                        let public = m.flags & (flags::PRIVATE | flags::PROTECTED) == 0;
                        out.push((m.name.clone(), public));
                    }
                }
            }
            cur = c.parent.as_ref().map(Rc::clone);
        }
        out
    }

    fn call_method(&mut self, obj: &Obj, name: &[u8], args: &[Value]) -> RtResult<MethodCall> {
        let cls = class_of(obj).clone();
        let n = String::from_utf8_lossy(name).into_owned();
        let lc = n.to_ascii_lowercase();
        if cls.find_method(lc.as_bytes()).is_none() && cls.magic.call.is_none() {
            return Ok(MethodCall::NotCallable(format!(
                "class {} does not have a method \"{n}\"",
                php_vm::object::display_name(&cls.name).to_string_lossy()
            )));
        }
        let args = args.to_vec();
        let vm = &mut *self.vm;
        let r = quiet_if_server(vm, |vm| vm.call_method_by_name(obj, &n, args));
        r.map(MethodCall::Returned).map_err(|t| to_rt(self.vm, t))
    }

    fn memory_check(&mut self, bytes: usize) -> RtResult<()> {
        // The accounting allocator records crossing `memory_limit`; the VM
        // only notices at its next safe point, so long native loops poll
        // here (PHP raises the fatal inside the allocator).
        let mut msg = php_memlimit::take().map(|over| over.to_string());
        if msg.is_none() {
            if let Some(limit) = php_memlimit::limit() {
                if limit > 0 && bytes > limit {
                    msg = Some(format!(
                        "Allowed memory size of {limit} bytes exhausted (tried to allocate 65536 bytes)"
                    ));
                }
            }
        }
        let Some(msg) = msg else {
            return Ok(());
        };
        if server_context() {
            // `soap_real_error_handler` turns it into a fault response.
            return Err(RuntimeError::Fatal(msg));
        }
        // Client: `ZEND_HAS_OOM()` bypasses the SoapFault conversion.
        Err(oom_error(msg))
    }

    fn memory_reserve(&mut self, bytes: usize) -> RtResult<()> {
        // A block that would cross `memory_limit` is not allocated: the
        // violation is recorded (as `zend_mm_alloc_huge` raises it before
        // allocating) and reported like any other one.
        if bytes > 0 {
            let _ = php_memlimit::try_reserve(bytes);
        }
        self.memory_check(0)
    }

    fn load_resource(&mut self, url: &str, context: Option<&Value>) -> Result<Vec<u8>, String> {
        let lower = url.to_ascii_lowercase();
        for scheme in ["http", "https"] {
            if lower.starts_with(&format!("{scheme}://"))
                && !crate::net::has_wrapper(self.vm, scheme)
            {
                return crate::net::http_get(self.vm, url, context)
                    .ok_or_else(|| format!("failed to load external entity \"{url}\"\n"));
            }
        }
        let mut args = vec![Value::from(url), Value::Bool(false)];
        if let Some(c) = context {
            args.push(c.clone());
        }
        let vm = &mut *self.vm;
        let r = silenced(vm, |vm| vm.call_function("file_get_contents", args));
        match r {
            Ok(Value::Str(s)) => Ok(s.as_bytes().to_vec()),
            Ok(_) => Err(format!("failed to load external entity \"{url}\"\n")),
            Err(t) => {
                // A user stream wrapper threw: keep the exception pending.
                if let Throw::Exception(_) = t {
                    self.vm.pending_throw = Some(t);
                }
                Err(format!("failed to load external entity \"{url}\"\n"))
            }
        }
    }

    fn realpath(&mut self, path: &str) -> Option<String> {
        match self.call_named("realpath", vec![Value::from(path)]) {
            Ok(Value::Str(s)) => Some(s.to_string_lossy()),
            _ => None,
        }
    }

    fn now(&mut self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
    }

    fn write_output(&mut self, data: &[u8]) {
        self.vm.output(data);
    }

    fn add_header(&mut self, line: &str) {
        let _ = silenced(self.vm, |vm| {
            vm.call_function("header", vec![Value::from(line)])
        });
    }

    fn ob_start(&mut self) -> bool {
        php_support::output::start(
            &mut *self.vm,
            php_support::output::StartHandler::Default,
            0,
            php_support::output::flags::STDFLAGS,
        )
    }

    fn ob_contents(&mut self) -> Option<Vec<u8>> {
        self.vm.output.contents().filter(|c| !c.is_empty())
    }

    fn ob_discard(&mut self) {
        // With no active buffer PHP's notice goes to the SOAP error handler
        // (not displayed in a server context).
        if self.vm.output.level() > 0 {
            php_support::output::discard(&mut *self.vm);
        }
    }

    fn server_var(&mut self, name: &str) -> Option<Vec<u8>> {
        let server = global_array(self.vm, b"_SERVER")?;
        match server.get_str(name.as_bytes()).map(Value::deref) {
            Some(Value::Str(s)) => Some(s.as_bytes().to_vec()),
            Some(v @ (Value::Int(_) | Value::Float(_) | Value::Bool(_))) => {
                v.to_zstr(&mut *self.vm).ok().map(|s| s.as_bytes().to_vec())
            }
            _ => None,
        }
    }

    fn request_line(&mut self) -> (Option<String>, Option<String>) {
        let s = |h: &mut Self, n: &str| {
            h.server_var(n)
                .map(|v| String::from_utf8_lossy(&v).into_owned())
        };
        (s(self, "REQUEST_METHOD"), s(self, "QUERY_STRING"))
    }

    /// `php_stream_copy_to_mem()` of `php://input`: one allocation of the
    /// body's size, checked against `memory_limit` (past it there is no
    /// body, and the VM raises the fatal as the call returns).
    fn request_body(&mut self) -> Option<Vec<u8>> {
        let src = php_streams::state::request_body()?;
        if src.is_empty() {
            return None;
        }
        let mut body = Vec::new();
        php_memlimit::TryGrow::try_grow_exact(&mut body, src.len()).ok()?;
        body.resize(src.len(), 0);
        let n = src.read_at(0, &mut body);
        body.truncate(n);
        Some(body)
    }

    fn ini_get(&mut self, name: &str) -> Option<Vec<u8>> {
        self.vm.ini_string(name)
    }

    fn context_create(&mut self) -> Value {
        self.call_named("stream_context_create", Vec::new())
            .unwrap_or(Value::Null)
    }

    fn context_get_option(
        &mut self,
        context: &Value,
        wrapper: &str,
        option: &str,
    ) -> Option<Value> {
        let Ok(Value::Array(opts)) =
            self.call_named("stream_context_get_options", vec![context.clone()])
        else {
            return None;
        };
        match opts.get_str(wrapper.as_bytes()).map(Value::deref) {
            Some(Value::Array(w)) => w.get_str(option.as_bytes()).map(Value::deref),
            _ => None,
        }
    }

    fn context_set_option(&mut self, context: &Value, wrapper: &str, option: &str, value: Value) {
        let _ = self.call_named(
            "stream_context_set_option",
            vec![
                context.clone(),
                Value::from(wrapper),
                Value::from(option),
                value,
            ],
        );
    }

    fn connector(&mut self) -> Option<&mut dyn HttpConnector> {
        Some(self)
    }

    fn random_bytes(&mut self, n: usize) -> RtResult<Vec<u8>> {
        let len = i64::try_from(n).unwrap_or(0);
        match self.call_named("random_bytes", vec![Value::Int(len)])? {
            Value::Str(s) => Ok(s.as_bytes().to_vec()),
            _ => Ok(vec![0; n]),
        }
    }

    fn bailout(&mut self) -> RuntimeError {
        RuntimeError::Custom {
            class: ZStr::from(BAILOUT_MARK),
            message: String::new(),
        }
    }

    fn session_available(&mut self) -> bool {
        self.function_exists(b"session_start").is_some()
    }

    fn session_get_object(&mut self) -> RtResult<Option<Value>> {
        // PHP_SESSION_DISABLED = 0, PHP_SESSION_ACTIVE = 2.
        let status = self.call_named("session_status", Vec::new())?;
        if !matches!(status, Value::Int(0 | 2)) {
            self.call_named("session_start", Vec::new())?;
        }
        let Some(sess) = global_array(self.vm, b"_SESSION") else {
            return Ok(None);
        };
        Ok(sess.get_str(b"_bogus_session_name").map(Value::deref))
    }

    fn session_set_object(&mut self, value: Value) {
        let mut g = self.vm.globals.borrow_mut();
        let i = g.find_or_add(b"_SESSION") as usize;
        match &mut g.slots[i] {
            Some(Value::Reference(r)) => {
                let mut cur = r.get();
                if let Value::Array(a) = &mut cur {
                    a.make_mut()
                        .insert(ArrayKey::from_bytes(b"_bogus_session_name"), value);
                    r.set(cur);
                }
            }
            Some(Value::Array(a)) => {
                a.make_mut()
                    .insert(ArrayKey::from_bytes(b"_bogus_session_name"), value);
            }
            _ => {}
        }
    }
}

/// A superglobal array (`$_SERVER`, `$_SESSION`).
fn global_array(vm: &Vm, name: &[u8]) -> Option<ZArray> {
    let mut g = vm.globals.borrow_mut();
    let i = g.find(name)? as usize;
    match g.slots.get(i).and_then(Option::as_ref).map(Value::deref) {
        Some(Value::Array(a)) => Some(a),
        _ => None,
    }
}

impl HttpConnector for VmHost<'_> {
    fn connect(&mut self, target: &ConnectTarget) -> Option<Box<dyn HttpStream>> {
        crate::net::connect(self.vm, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_names_are_mangled_like_zend() {
        let c = ZStr::from("SoapClient");
        let n = ZStr::from("uri");
        assert_eq!(
            mangle(flags::PRIVATE, &c, &n).as_bytes(),
            b"\0SoapClient\0uri"
        );
        assert_eq!(mangle(flags::PROTECTED, &c, &n).as_bytes(), b"\0*\0uri");
        assert_eq!(mangle(flags::PUBLIC, &c, &n).as_bytes(), b"uri");
    }

    #[test]
    fn internal_classes() {
        assert!(is_internal_class(&ZStr::from("SoapFault")));
        assert!(is_internal_class(&ZStr::from("exception")));
        assert!(!is_internal_class(&ZStr::from("MyClient")));
    }

    #[test]
    fn throws_round_trip_through_runtime_errors() {
        let exit = RuntimeError::Custom {
            class: ZStr::from(EXIT_MARK),
            message: String::new(),
        };
        assert!(
            matches!(&exit, RuntimeError::Custom { class, .. } if class.as_bytes() == EXIT_MARK)
        );
        let oom = oom_error("Allowed memory size of 1 bytes exhausted".into());
        assert!(
            matches!(oom, RuntimeError::Custom { class, .. } if class.as_bytes() == BAILOUT_MARK)
        );
        assert_eq!(
            OOM.with(|o| o.borrow_mut().take()).as_deref(),
            Some("Allowed memory size of 1 bytes exhausted")
        );
    }
}
