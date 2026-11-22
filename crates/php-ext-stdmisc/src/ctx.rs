// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Glue between the VM and the `php-stdmisc` library: error conversion,
//! the [`QueryContext`] implementation and small argument helpers.

use php_runtime::serialize::SerializedObject;
use php_runtime::{
    ArrayKey, BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult,
    RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_stdmisc::query::QueryContext;
use php_vm::class::Class;
use php_vm::ext::acc;
use php_vm::object::{class_of, data};
use php_vm::vm::FrameKind;
use php_vm::{Throw, Vm, VmResult};
use std::rc::Rc;

/// Converts a VM throw into a runtime error; bailouts (`exit`, fatal
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
/// for library-made errors (`ValueError`, ...).
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

/// Extension trait: `RtResult<T>` → `VmResult<T>`.
pub(crate) trait RtExt<T> {
    /// Converts the error with [`rt_throw`].
    fn vm(self, vm: &mut Vm) -> VmResult<T>;
}

impl<T> RtExt<T> for RtResult<T> {
    fn vm(self, vm: &mut Vm) -> VmResult<T> {
        self.map_err(|e| rt_throw(vm, e))
    }
}

/// The class scope of the calling PHP code (the innermost non-builtin
/// frame).
pub(crate) fn calling_scope(vm: &Vm) -> Option<Rc<Class>> {
    vm.frames
        .iter()
        .rev()
        .find(|f| f.kind != FrameKind::Builtin)
        .and_then(|f| f.scope.clone())
}

/// A global variable (`$GLOBALS[name]`), dereferenced.
pub(crate) fn global(vm: &Vm, name: &[u8]) -> Option<Value> {
    let mut g = vm.globals.borrow_mut();
    let i = g.find(name)?;
    g.slots
        .get(i as usize)
        .cloned()
        .flatten()
        .map(|v| v.deref())
}

/// The VM seen as a [`QueryContext`] (for `http_build_query()`).
pub(crate) struct VmQuery<'a> {
    vm: &'a mut Vm,
    scope: Option<Rc<Class>>,
}

impl<'a> VmQuery<'a> {
    pub(crate) fn new(vm: &'a mut Vm) -> Self {
        let scope = calling_scope(vm);
        VmQuery { vm, scope }
    }

    /// `zend_check_property_access` for a declared property of class
    /// `declaring` with visibility `vis`.
    fn accessible(&self, declaring: &ZStr, vis: u32) -> bool {
        if vis == acc::PUBLIC {
            return true;
        }
        let Some(scope) = &self.scope else {
            return false;
        };
        let lc = declaring.as_bytes().to_ascii_lowercase();
        if vis == acc::PRIVATE {
            return scope.lc_name.as_bytes() == lc.as_slice();
        }
        if scope.instance_of_name(&lc) {
            return true;
        }
        self.vm
            .find_class(&lc)
            .is_some_and(|d| d.instance_of_name(scope.lc_name.as_bytes()))
    }
}

impl RuntimeContext for VmQuery<'_> {
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
    fn unserialize_custom(&mut self, class: &ZStr, bytes: &[u8]) -> RtResult<Value> {
        self.vm.unserialize_custom(class, bytes)
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

/// Approximate C stack bytes per `php_url_encode_hash_ex()` recursion.
const URL_ENCODE_FRAME_BYTES: u64 = 128;

impl QueryContext for VmQuery<'_> {
    fn accessible_properties(&mut self, obj: &Obj) -> RtResult<Vec<(ArrayKey, Value)>> {
        let cls = class_of(obj).clone();
        let mut out = Vec::new();
        {
            let slots = data(obj).slots.borrow();
            for pi in &cls.props {
                if pi.is_virtual() || !self.accessible(&pi.class, pi.visibility()) {
                    continue;
                }
                if let Some(v) = slots.get(pi.slot as usize).cloned().flatten() {
                    out.push((ArrayKey::Str(pi.name.clone()), v));
                }
            }
        }
        if let Some(t) = data(obj).dynamic.borrow().as_deref() {
            for (k, v) in t {
                out.push((k.clone(), v.clone()));
            }
        }
        Ok(out)
    }

    /// `zend_call_stack_overflowed()` under `zend.max_allowed_stack_size`:
    /// one `php_url_encode_hash_ex()` level takes about
    /// [`URL_ENCODE_FRAME_BYTES`] of C stack.
    fn max_depth(&self) -> usize {
        let max = self.vm.ini_long("zend.max_allowed_stack_size");
        if max <= 0 {
            return php_stdmisc::query::MAX_DEPTH;
        }
        let reserved = self.vm.ini_long("zend.reserved_stack_size").max(0);
        let budget = u64::try_from(max.saturating_sub(reserved)).unwrap_or(0);
        usize::try_from(budget / URL_ENCODE_FRAME_BYTES)
            .unwrap_or(usize::MAX)
            .min(php_stdmisc::query::MAX_DEPTH)
    }

    fn enum_case_value(&mut self, obj: &Obj) -> RtResult<Option<Option<Value>>> {
        let cls = class_of(obj).clone();
        if !cls.is_enum() {
            return Ok(None);
        }
        if cls.backing.is_none() {
            return Ok(Some(None));
        }
        let v = self
            .vm
            .get_property(obj, "value")
            .map_err(|t| to_rt(self.vm, t))?;
        Ok(Some(Some(v)))
    }
}

impl php_filter::FilterHost for VmQuery<'_> {
    fn is_callable(&mut self, callable: &Value) -> bool {
        self.vm.is_callable(callable)
    }

    fn call_callback(&mut self, callable: &Value, arg: Value) -> RtResult<Value> {
        self.vm
            .call_value(callable, vec![arg])
            .map_err(|t| to_rt(self.vm, t))
    }
}
