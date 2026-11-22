// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmFilterHost`]: the [`FilterHost`] implementation over the VM, plus
//! the conversions between VM throws and runtime errors.

use std::cell::RefCell;

use php_filter::{FilterHost, FILTER_FAILED_EXCEPTION_CLASS};
use php_pcre::{MatchConfig, Pcre};
use php_runtime::serialize::SerializedObject;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray, ZStr,
};
use php_vm::object::class_of;
use php_vm::{Throw, Vm};

thread_local! {
    /// The compiled-pattern cache of `FILTER_VALIDATE_REGEXP`.
    static PCRE: RefCell<Pcre> = RefCell::new(Pcre::new());
}

/// Converts a VM throw into a runtime error; bailouts (`exit`, fatal
/// errors) are parked in `vm.pending_throw`.
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

/// Converts a runtime error into a VM throw.
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

/// The VM seen through [`RuntimeContext`] + [`FilterHost`].
pub(crate) struct VmFilterHost<'a> {
    pub(crate) vm: &'a mut Vm,
}

impl RuntimeContext for VmFilterHost<'_> {
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

/// The `pcre.*` limits currently configured.
fn match_config(vm: &Vm) -> MatchConfig {
    MatchConfig::from_ini(
        vm.ini_long("pcre.backtrack_limit"),
        vm.ini_long("pcre.recursion_limit"),
        vm.ini_bool("pcre.jit"),
    )
}

impl FilterHost for VmFilterHost<'_> {
    fn regex_match(&mut self, pattern: &ZStr, subject: &[u8]) -> RtResult<Option<bool>> {
        let (compiled, warnings) = PCRE.with(|p| {
            let mut p = p.borrow_mut();
            let r = p.compile(pattern.as_bytes());
            (r, p.take_warnings())
        });
        for w in warnings {
            self.vm
                .php_warning(&String::from_utf8_lossy(&w))
                .map_err(|t| to_rt(self.vm, t))?;
        }
        let re = match compiled {
            Ok(re) => re,
            Err(e) => {
                self.vm
                    .php_warning(&String::from_utf8_lossy(&e.message()))
                    .map_err(|t| to_rt(self.vm, t))?;
                return Ok(None);
            }
        };
        let cfg = match_config(self.vm);
        let out = php_pcre::match_one(&re, &cfg, subject, false, 0, 0);
        Ok(Some(matches!(out.result, Ok(n) if n > 0)))
    }

    fn is_callable(&mut self, callable: &Value) -> bool {
        self.vm.is_callable(callable)
    }

    fn call_callback(&mut self, callable: &Value, arg: Value) -> RtResult<Value> {
        self.vm
            .call_value(callable, vec![arg])
            .map_err(|t| to_rt(self.vm, t))
    }

    fn filter_failed_exception(&mut self, message: String) -> RuntimeError {
        match self
            .vm
            .create_exception(FILTER_FAILED_EXCEPTION_CLASS, &message, 0)
        {
            Ok(o) => RuntimeError::Thrown(Value::Object(o)),
            Err(t) => to_rt(self.vm, t),
        }
    }
}
