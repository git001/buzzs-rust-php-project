// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The VM seen through [`SxeHost`], and conversions between VM throws and
//! [`RuntimeError`]s.
//!
//! * [`VmHost`] borrows the VM directly (object handlers).
//! * [`LentHost`] reaches the VM lent by
//!   [`php_ext_xml::libxml::run`] (methods and functions that parse, load
//!   or save: the VM must stay reachable from libxml2's stream hooks and
//!   the user entity loader while the library runs).

use php_ext_streams::bridge;
use php_runtime::{
    BinaryOp, CastTarget, ErrorLevel, Obj, ObjectDump, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray, ZStr,
};
use php_simplexml::{SxeHost, SxeRef};
use php_vm::ext::{Throw, VmResult};
use php_vm::vm::FrameKind;
use php_vm::Vm;

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
/// for errors raised by `php-simplexml`.
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

/// Attaches `sxe` to `o`.
pub fn set_sxe(o: &Obj, sxe: SxeRef) {
    php_vm::object::set_internal::<SxeRef>(o, sxe);
}

/// The `SimpleXML` state attached to an object.
pub fn sxe_of(o: &Obj) -> Option<SxeRef> {
    php_vm::object::internal::<SxeRef, _>(o, |s| s.clone())
}

/// `php_sxe_object_new` + state: an instance of `class` (no constructor)
/// carrying `sxe`.
pub fn wrap_object(vm: &mut Vm, class: &ZStr, sxe: SxeRef) -> Result<Value, Throw> {
    let cls = vm
        .find_class(class.as_bytes())
        .or_else(|| vm.find_class(b"SimpleXMLElement"));
    let Some(cls) = cls else {
        return Err(vm.throw("Error", "Class \"SimpleXMLElement\" not found"));
    };
    let o = vm.new_object(&cls)?;
    set_sxe(&o, sxe);
    Ok(Value::Object(o))
}

/// `php_error_docref` as PHP formats it: prefixed with the active
/// function — the running native function, else the user function
/// (`Class::method`) or `main` for top-level code. The VM's own docref
/// only knows native functions; object handlers run from user code need
/// the user-function form.
pub fn docref(vm: &mut Vm, level: i64, msg: &[u8]) -> VmResult<()> {
    let mut frames = vm.frames.iter().rev();
    let prefix = match frames.next() {
        Some(f) if f.kind == FrameKind::Builtin => {
            let name = f.builtin_name.clone().unwrap_or_default();
            match &f.scope {
                Some(c) if f.this.is_some() || f.called.is_some() => format!(
                    "{}::{}",
                    php_vm::object::display_name(&c.name).to_string_lossy(),
                    name.to_string_lossy()
                ),
                _ => name.to_string_lossy(),
            }
        }
        Some(f) => match (&f.ufunc, &f.scope) {
            (Some(u), Some(c)) => format!(
                "{}::{}",
                php_vm::object::display_name(&c.name).to_string_lossy(),
                u.name.to_string_lossy()
            ),
            (Some(u), None) => u.name.to_string_lossy(),
            (None, _) => "main".to_owned(),
        },
        None => "main".to_owned(),
    };
    let mut m = format!("{prefix}(): ").into_bytes();
    m.extend_from_slice(msg);
    vm.raise_bytes(level, &m)
}

/// The VM, borrowed.
pub struct VmHost<'a> {
    /// The VM.
    pub vm: &'a mut Vm,
}

impl<'a> VmHost<'a> {
    /// Wraps the VM.
    pub fn new(vm: &'a mut Vm) -> Self {
        VmHost { vm }
    }
}

impl std::fmt::Debug for VmHost<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmHost")
    }
}

impl RuntimeContext for VmHost<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        let r = docref(self.vm, level.bits(), message.as_bytes());
        r.map_err(|t| to_rt(self.vm, t))
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
    fn is_enum(&mut self, obj: &Obj) -> bool {
        self.vm.is_enum(obj)
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

impl SxeHost for VmHost<'_> {
    fn sxe_wrap(&mut self, class: &ZStr, sxe: SxeRef) -> RtResult<Value> {
        wrap_object(self.vm, class, sxe).map_err(|t| to_rt(self.vm, t))
    }

    fn sxe_unwrap(&mut self, obj: &Obj) -> Option<SxeRef> {
        sxe_of(obj)
    }
}

/// The VM lent by [`php_ext_xml::libxml::run`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LentHost;

fn no_vm() -> RuntimeError {
    RuntimeError::Error("SimpleXML: no VM available".into())
}

fn lend<R>(f: impl FnOnce(&mut VmHost<'_>) -> RtResult<R>) -> RtResult<R> {
    bridge::with_vm(|vm| f(&mut VmHost::new(vm))).unwrap_or_else(|| Err(no_vm()))
}

impl RuntimeContext for LentHost {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        lend(|h| h.diagnostic(level, message))
    }
    fn precision(&self) -> i64 {
        bridge::with_vm(|vm| vm.precision()).unwrap_or(14)
    }
    fn serialize_precision(&self) -> i64 {
        bridge::with_vm(|vm| vm.serialize_precision()).unwrap_or(-1)
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        lend(|h| h.cast_object(obj, target))
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        lend(|h| h.do_operation(op, op1, op2))
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        lend(|h| h.compare_objects(op1, op2))
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        lend(|h| h.object_properties(obj, purpose))
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        lend(|h| h.object_dump(obj, purpose))
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        bridge::with_vm(|vm| vm.is_enum(obj)).unwrap_or(false)
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        lend(|h| h.count_object(obj))
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        lend(|h| h.property_read(obj, name))
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        lend(|h| h.property_write(obj, name, value))
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        lend(|h| h.create_std_object(props))
    }
}

impl SxeHost for LentHost {
    fn sxe_wrap(&mut self, class: &ZStr, sxe: SxeRef) -> RtResult<Value> {
        lend(|h| h.sxe_wrap(class, sxe))
    }

    fn sxe_unwrap(&mut self, obj: &Obj) -> Option<SxeRef> {
        sxe_of(obj)
    }
}
