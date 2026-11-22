// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmJsonHost`]: the [`JsonHost`] implementation over the VM, plus the
//! conversions between VM throws and runtime errors.

use php_json::{GuardKey, JsonHost};
use php_runtime::serialize::SerializedObject;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray, ZStr,
};
use php_vm::object::class_of;
use php_vm::{Throw, Vm};

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
/// for runtime-made errors (`ValueError`, `JsonException`, ...).
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

/// The VM seen through [`RuntimeContext`] + [`JsonHost`].
pub(crate) struct VmJsonHost<'a> {
    vm: &'a mut Vm,
    input_len: usize,
}

impl<'a> VmJsonHost<'a> {
    pub(crate) fn new(vm: &'a mut Vm, input_len: usize) -> Self {
        VmJsonHost { vm, input_len }
    }
}

impl RuntimeContext for VmJsonHost<'_> {
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

impl JsonHost for VmJsonHost<'_> {
    fn json_reserve(&mut self, buf: &mut Vec<u8>, additional: usize) -> RtResult<()> {
        if additional > buf.capacity() - buf.len() {
            let output = buf.len().saturating_add(additional);
            if let Err(t) = super::check_memory(self.vm, self.input_len, output) {
                return Err(to_rt(self.vm, t));
            }
            php_memlimit::TryGrow::try_grow(buf, additional)?;
        }
        Ok(())
    }

    fn is_json_serializable(&mut self, obj: &Obj) -> bool {
        class_of(obj).instance_of_name(b"jsonserializable")
    }

    fn json_serialize(&mut self, obj: &Obj) -> RtResult<Value> {
        self.vm
            .call_method_by_name(obj, "jsonSerialize", Vec::new())
            .map_err(|t| to_rt(self.vm, t))
    }

    fn is_enum(&mut self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }

    fn enum_backing_value(&mut self, obj: &Obj) -> Option<Value> {
        class_of(obj).backing?;
        self.vm.get_property(obj, "value").ok()
    }

    /// `zend_get_properties_for(obj, ZEND_PROP_PURPOSE_JSON)`: the VM's JSON
    /// property table (lazy objects initialized, `get` hooks run, virtual
    /// properties without `get` absent, non-public names mangled so the
    /// encoder skips them, dynamic properties appended).
    fn json_properties(&mut self, obj: &Obj) -> RtResult<Option<ZArray>> {
        self.vm.standard_json_properties(obj)
    }

    fn json_native_properties(&mut self, obj: &Obj) -> RtResult<Option<(ZArray, GuardKey)>> {
        let cls = class_of(obj);
        let Some(handler) = cls.hooks.as_ref().and_then(|h| h.properties_for) else {
            return Ok(None);
        };
        let table = handler(self.vm, obj, PropertyPurpose::Json).map_err(|t| to_rt(self.vm, t))?;
        Ok(table.map(|table| {
            let guard = if cls.props.iter().any(|p| p.hooks.is_some()) {
                GuardKey::object(obj)
            } else {
                GuardKey::array(&table)
            };
            (table, guard)
        }))
    }

    fn std_class_write(&mut self, obj: &Value, name: ZStr, value: Value) -> RtResult<()> {
        if let Value::Object(o) = obj {
            return self.vm.property_write(o, &name, value);
        }
        Ok(())
    }

    fn json_stack_limit_reached(&mut self, _nesting: usize) -> bool {
        self.vm.native_stack_exhausted()
    }

    fn json_exception(&mut self, message: &str, code: i64) -> RuntimeError {
        match self
            .vm
            .create_exception(php_json::JSON_EXCEPTION_CLASS, message, code)
        {
            Ok(o) => RuntimeError::Thrown(Value::Object(o)),
            Err(t) => to_rt(self.vm, t),
        }
    }
}
