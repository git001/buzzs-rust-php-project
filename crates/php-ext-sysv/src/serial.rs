// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `php_var_serialize()` / `php_var_unserialize()` through the VM.
//!
//! Unlike the PHP functions, the extensions' unserialization is silent:
//! a malformed payload is reported by the caller ("Message corrupted"),
//! and trailing bytes after a complete value are ignored.  [`Quiet`]
//! forwards the runtime context to the VM but drops those two
//! `unserialize()` diagnostics.

use php_runtime::serialize::{self, SerializedObject, UnserializeOptions};
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray, ZStr,
};
use php_vm::ext::{Throw, VmResult};
use php_vm::Vm;

/// Converts a runtime error into a VM throw.
pub(crate) fn to_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
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

/// `php_var_serialize()`.
pub(crate) fn serialize(vm: &mut Vm, v: &Value) -> VmResult<Vec<u8>> {
    serialize::serialize(v, vm).map_err(|e| to_throw(vm, e))
}

/// `php_var_unserialize()`: `Ok(None)` for malformed data.
pub(crate) fn unserialize(vm: &mut Vm, data: &[u8]) -> VmResult<Option<Value>> {
    let opts = UnserializeOptions::default();
    let r = serialize::unserialize(data, &opts, &mut Quiet(vm));
    r.map_err(|e| to_throw(vm, e))
}

/// The VM as a runtime context without `unserialize()`'s own notices.
struct Quiet<'a>(&'a mut Vm);

impl RuntimeContext for Quiet<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        if message.starts_with("unserialize(): Error at offset")
            || message.starts_with("unserialize(): Extra data")
        {
            return Ok(());
        }
        self.0.diagnostic(level, message)
    }
    fn precision(&self) -> i64 {
        self.0.precision()
    }
    fn serialize_precision(&self) -> i64 {
        self.0.serialize_precision()
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        self.0.cast_object(obj, target)
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        self.0.do_operation(op, op1, op2)
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        self.0.compare_objects(op1, op2)
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        self.0.object_properties(obj, purpose)
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        self.0.object_dump(obj, purpose)
    }
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        self.0.serialize_object(obj)
    }
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        self.0.unserialize_object_create(class)
    }
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        self.0.unserialize_object_finish(obj, props)
    }
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        self.0.unserialize_enum(class, case)
    }
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.0.unserialize_custom(class, data)
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        self.0.is_enum(obj)
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        self.0.count_object(obj)
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        self.0.property_read(obj, name)
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        self.0.property_write(obj, name, value)
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        self.0.create_std_object(props)
    }
}
