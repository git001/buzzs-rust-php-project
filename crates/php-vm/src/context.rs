// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The VM as `php_runtime::RuntimeContext` (object handlers, diagnostics)
//! and as `php_support::output::OutputHost` (SAPI output).

use crate::class::cflags;
use crate::func::flags;
use crate::object;
use crate::vm::{Throw, Vm, VmResult};
use php_runtime::serialize::{EnumCaseLookup, SerializedObject};
use php_runtime::{
    ArrayKey, BinaryOp, CastTarget, ErrorLevel, HashTable, Obj, ObjectDump, ObjectKind,
    PropertyPurpose, PropertySlot, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_support::output::{OutputHost, OutputStack};
use std::io::Write;

impl Vm {
    /// Converts a VM throw into a runtime error (remembering bailouts).
    pub(crate) fn to_rt(&mut self, t: Throw) -> RuntimeError {
        match t {
            Throw::Exception(o) => RuntimeError::Thrown(Value::Object(o)),
            other => {
                self.pending_throw = Some(other);
                RuntimeError::Fatal(String::new())
            }
        }
    }

    /// Writes output through the output buffering layer.
    pub fn echo(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        php_support::output::write(self, data);
    }

    fn release_output_destructors(&mut self) {
        let pending = self.pending_throw.take();
        let released = self.run_released_destructors();
        self.pending_throw = match (pending, released) {
            (Some(Throw::Exception(old)), Err(Throw::Exception(new))) => {
                crate::exceptions::chain_previous(&new, &old);
                Some(Throw::Exception(new))
            }
            (Some(old), Err(Throw::Exception(_))) => Some(old),
            (_, Err(t)) => Some(t),
            (pending, Ok(())) => self.pending_throw.take().or(pending),
        };
    }

    /// Flushes standard output.
    pub fn flush_stdout(&mut self) {
        let _ = self.stdout.flush();
    }

    /// `__toString` conversion; `None` if the object has no `__toString`.
    pub(crate) fn object_to_string_opt(&mut self, o: &Obj) -> VmResult<Option<ZStr>> {
        let cls = object::class_of(o).clone();
        let Some(m) = cls.magic.to_string.clone() else {
            return Ok(None);
        };
        let r = self.call_method(o, &m, Vec::new())?;
        match r.deref() {
            Value::Str(s) => Ok(Some(s)),
            other => Err(self.make_throw(
                "TypeError",
                &format!(
                    "{}::__toString(): Return value must be of type string, {} returned",
                    object::display_name(&cls.name),
                    crate::typecheck::given_type(&other)
                ),
            )),
        }
    }

    /// The property table for a purpose (honouring `__debugInfo`).
    pub(crate) fn props_for(
        &mut self,
        o: &Obj,
        purpose: PropertyPurpose,
    ) -> VmResult<Option<ZArray>> {
        let cls = object::class_of(o).clone();
        if let Some(f) = cls.hooks.as_ref().and_then(|h| h.properties_for) {
            if let Some(t) = f(self, o, purpose)? {
                return Ok(Some(t));
            }
        }
        self.standard_props_for(o, purpose)
    }

    /// The JSON fallback after a native property handler declined the purpose.
    pub fn standard_json_properties(&mut self, o: &Obj) -> RtResult<Option<ZArray>> {
        self.standard_props_for(o, PropertyPurpose::Json)
            .map_err(|t| self.to_rt(t))
    }

    fn standard_props_for(
        &mut self,
        o: &Obj,
        purpose: PropertyPurpose,
    ) -> VmResult<Option<ZArray>> {
        let cls = object::class_of(o).clone();
        if purpose == PropertyPurpose::Debug {
            if let Some(m) = cls.magic.debug_info.clone() {
                let r = self.call_method(o, &m, Vec::new())?;
                return match r {
                    Value::Array(a) => Ok(Some(a)),
                    Value::Null => {
                        let msg = format!(
                            "Returning null from {}::__debugInfo() is deprecated, return an empty array instead",
                            object::display_name(&cls.name)
                        );
                        self.deprecated(&msg)?;
                        Ok(Some(ZArray::new()))
                    }
                    _ => {
                        let msg = "__debugInfo() must return an array".to_string();
                        Err(self.fatal(&msg))
                    }
                };
            }
        }
        // Serialization, export and JSON initialize lazy objects (and read
        // a proxy's real instance); var_dump and (array) do not.
        let target;
        let o = if matches!(
            purpose,
            PropertyPurpose::Serialize | PropertyPurpose::VarExport | PropertyPurpose::Json
        ) {
            target = self.lazy_object_for(o, None)?;
            &target
        } else {
            o
        };
        if let Some(a) = self.internal_object_array(o)? {
            return Ok(Some(a));
        }
        if matches!(purpose, PropertyPurpose::VarExport | PropertyPurpose::Json) {
            let mut t = object::PropTableBuilder::new(object::data(o));
            let stale = crate::lazy::stale_slots(object::data(o));
            for pi in object::class_of(o).props.clone() {
                if purpose == PropertyPurpose::Json && pi.visibility() != flags::PUBLIC {
                    continue;
                }
                let value =
                    if let Some(getter) = pi.hooks.as_ref().and_then(|hooks| hooks.get.as_ref()) {
                        Some(self.call_hook(o, getter, &pi, Vec::new())?.deref())
                    } else if pi.is_virtual() {
                        None
                    } else if let Some(frozen) = &stale {
                        frozen.get(pi.slot as usize).cloned().flatten()
                    } else {
                        object::data(o)
                            .slots
                            .borrow()
                            .get(pi.slot as usize)
                            .cloned()
                            .flatten()
                    };
                if let Some(value) = value {
                    t.push(Some(pi.slot), crate::props::mangle(&pi), value);
                }
            }
            for (key, value, prop) in self.object_prop_table(o, false) {
                if prop.is_none() {
                    if let Some(value) = value {
                        t.push(None, key, value);
                    }
                }
            }
            return Ok(Some(ZArray::from_table(t.finish())));
        }
        let mut t = HashTable::new();
        for (k, v, _) in self.object_prop_table(o, false) {
            if let Some(v) = v {
                t.insert(k, v);
            }
        }
        Ok(Some(ZArray::from_table(t)))
    }

    /// `zend_std_compare_objects` on two distinct objects.
    fn std_compare(&mut self, lhs: &Obj, rhs: &Obj) -> VmResult<i32> {
        // Objects of different classes are uncomparable, before any lazy
        // one is initialized.
        let cls = object::class_of(lhs);
        if !std::rc::Rc::ptr_eq(cls, object::class_of(rhs)) {
            return Ok(1);
        }
        if cls.is_enum() {
            return Ok(1);
        }
        if let Some(order) = self.internal_compare(lhs, rhs)? {
            return Ok(order);
        }
        let plain = |o: &Obj| {
            let d = object::data(o);
            !crate::lazy::props_built(d) && !crate::lazy::is_lazy(d)
        };
        if plain(lhs) && plain(rhs) {
            return self.compare_slots(lhs, rhs);
        }
        // Either object has a properties table or is lazy:
        // `zend_std_get_properties_ex` initializes a lazy object (an
        // initialized proxy stands for its instance), builds the tables and
        // `zend_compare_symbol_tables` compares them.
        let lhs = self.lazy_object_for(lhs, None)?;
        let rhs = self.lazy_object_for(rhs, None)?;
        if Obj::ptr_eq(&lhs, &rhs) {
            return Ok(0);
        }
        let left = crate::lazy::property_table(&lhs, false);
        let right = crate::lazy::property_table(&rhs, false);
        self.compare_property_tables(&lhs, &left, &right)
    }

    /// Protects `o` against recursive comparison (`Z_PROTECT_RECURSION_P`
    /// / `GC_PROTECT_RECURSION` of its table) for the guard's lifetime.
    fn compare_guard(&mut self, o: &Obj) -> VmResult<php_runtime::ops::RecursionGuard> {
        php_runtime::ops::RecursionGuard::try_enter(php_runtime::ops::GuardKind::Compare, o.addr())
            .ok_or_else(|| {
                self.make_throw("Error", "Nesting level too deep - recursive dependency?")
            })
    }

    /// Two objects without properties tables: the declared slots are
    /// compared in order (`properties_info_table`); a defined slot against
    /// an undefined one makes the objects uncomparable (1).
    fn compare_slots(&mut self, lhs: &Obj, rhs: &Obj) -> VmResult<i32> {
        let _guard = self.compare_guard(lhs)?;
        let left = object::data(lhs).slots.borrow().clone();
        let right = object::data(rhs).slots.borrow().clone();
        for pair in left.iter().zip(right.iter()) {
            match pair {
                (Some(lhs_value), Some(rhs_value)) => {
                    let order = php_runtime::ops::compare(lhs_value, rhs_value, self)
                        .map_err(|err| self.rt_throw(err))?;
                    if order != 0 {
                        return Ok(order);
                    }
                }
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => return Ok(1),
            }
        }
        Ok(0)
    }

    /// `zend_compare_symbol_tables` (`zend_hash_compare`, unordered) on the
    /// properties tables of `lhs` and another object: the element counts
    /// decide first, then every entry of `left` is compared with the entry
    /// of the same key in `right` (a missing key: 1). An undefined declared
    /// slot (`IS_INDIRECT` to `IS_UNDEF`, `None`) orders below a defined
    /// one, unlike in [`Self::compare_slots`].
    fn compare_property_tables(
        &mut self,
        lhs: &Obj,
        left: &[(ArrayKey, Option<Value>)],
        right: &[(ArrayKey, Option<Value>)],
    ) -> VmResult<i32> {
        let _guard = self.compare_guard(lhs)?;
        if left.len() != right.len() {
            return Ok(if left.len() > right.len() { 1 } else { -1 });
        }
        let index: std::collections::HashMap<&ArrayKey, &Option<Value>> =
            right.iter().map(|(key, value)| (key, value)).collect();
        for (key, lhs_value) in left {
            let Some(rhs_value) = index.get(key) else {
                return Ok(1);
            };
            match (lhs_value, rhs_value) {
                (None, None) => {}
                (None, Some(_)) => return Ok(-1),
                (Some(_), None) => return Ok(1),
                (Some(lhs_value), Some(rhs_value)) => {
                    let order = php_runtime::ops::compare(lhs_value, rhs_value, self)
                        .map_err(|err| self.rt_throw(err))?;
                    if order != 0 {
                        return Ok(order);
                    }
                }
            }
        }
        Ok(0)
    }
}

impl RuntimeContext for Vm {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        let r = match self.origin_with_args(message) {
            Some(m) => self.error(level.bits(), &m),
            None => self.error(level.bits(), message),
        };
        r.map_err(|t| self.to_rt(t))
    }

    fn precision(&self) -> i64 {
        self.precision
    }

    fn serialize_precision(&self) -> i64 {
        self.serialize_precision
    }

    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        if let Some(f) = crate::ext::hooks_of(obj).and_then(|h| h.cast) {
            match f(self, obj, target) {
                Ok(Some(v)) => return Ok(Some(v)),
                Ok(None) => {}
                Err(t) => return Err(self.to_rt(t)),
            }
        }
        match target {
            CastTarget::String => match self.object_to_string_opt(obj) {
                Ok(Some(s)) => Ok(Some(Value::Str(s))),
                Ok(None) => Ok(None),
                Err(t) => Err(self.to_rt(t)),
            },
            CastTarget::Bool => Ok(Some(Value::Bool(true))),
            _ => Ok(None),
        }
    }

    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        for v in [op1, op2] {
            if let Value::Object(o) = v {
                if let Some(f) = crate::ext::hooks_of(o).and_then(|h| h.do_operation) {
                    return f(self, op, op1, op2).map_err(|t| self.to_rt(t));
                }
            }
        }
        Ok(None)
    }

    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        if let (Value::Object(a), Value::Object(b)) = (op1, op2) {
            if Obj::ptr_eq(a, b) {
                return Ok(0);
            }
        }
        // Enums use `zend_objects_not_comparable`.
        if [op1, op2]
            .iter()
            .any(|v| matches!(v, Value::Object(o) if object::class_of(o).is_enum()))
        {
            return Ok(1);
        }
        for v in [op1, op2] {
            if let Value::Object(o) = v {
                if let Some(f) = crate::ext::hooks_of(o).and_then(|h| h.compare) {
                    if let Some(r) = f(self, op1, op2).map_err(|t| self.to_rt(t))? {
                        return Ok(r);
                    }
                }
            }
        }
        php_runtime::ops::std_compare_objects(self, op1, op2, |vm, a, b| {
            vm.std_compare(a, b).map_err(|t| vm.to_rt(t))
        })
    }

    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        self.props_for(obj, purpose).map_err(|t| self.to_rt(t))
    }

    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        let cls = object::class_of(obj).clone();
        if cls.is_enum() {
            let case_name = self.enum_case_name(obj).unwrap_or_default();
            let backing_type = cls.backing.map(|b| {
                if b == crate::types::mask::LONG {
                    "int"
                } else {
                    "string"
                }
            });
            let has_debug_info = cls.magic.debug_info.is_some();
            let properties = if purpose == PropertyPurpose::Debug && has_debug_info {
                self.props_for(obj, purpose)
                    .map_err(|t| self.to_rt(t))?
                    .map(|t| {
                        t.iter()
                            .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                            .collect()
                    })
            } else {
                None
            };
            return Ok(ObjectDump {
                kind: ObjectKind::Enum {
                    case_name,
                    backing_type,
                    has_debug_info,
                },
                properties,
            });
        }
        let kind = if cls.lc_name.as_bytes() == b"stdclass" {
            ObjectKind::StdClass
        } else {
            ObjectKind::Regular {
                lazy_prefix: self.lazy_prefix(obj),
            }
        };
        // `zend_lazy_object_debug_info`: an initialized proxy shows its
        // instance.
        if purpose == PropertyPurpose::Debug {
            if let Some(inst) = crate::reflection::lazy::proxy_instance(obj) {
                return Ok(ObjectDump {
                    kind,
                    properties: Some(vec![(
                        ArrayKey::from("instance"),
                        PropertySlot::Value(Value::Object(inst)),
                    )]),
                });
            }
        }
        // Native `get_properties_for` / `get_debug_info` handlers.
        if let Some(f) = cls.hooks.as_ref().and_then(|h| h.properties_for) {
            if let Some(t) = f(self, obj, purpose).map_err(|t| self.to_rt(t))? {
                let properties = t
                    .iter()
                    .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                    .collect();
                return Ok(ObjectDump {
                    kind,
                    properties: Some(properties),
                });
            }
        }
        let custom = purpose == PropertyPurpose::VarExport
            || purpose == PropertyPurpose::Debug && cls.magic.debug_info.is_some()
            || self.has_internal_array(obj);
        if custom {
            let props = self.props_for(obj, purpose).map_err(|t| self.to_rt(t))?;
            return Ok(ObjectDump {
                kind,
                properties: props.map(|t| {
                    t.iter()
                        .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                        .collect()
                }),
            });
        }
        let include_uninit = matches!(purpose, PropertyPurpose::Debug);
        let mut props = Vec::new();
        for (k, v, pi) in self.object_prop_table(obj, include_uninit) {
            match v {
                // The table holds a second handle to a slot's reference: a
                // reference only the property holds is not shown as `&`.
                Some(Value::Reference(r)) if r.refcount() <= 2 => {
                    props.push((k, PropertySlot::Value(r.get())))
                }
                Some(v) => props.push((k, PropertySlot::Value(v))),
                None => {
                    if let Some(pi) = pi {
                        if let Some(ty) = &pi.ty {
                            let decl = self.find_class(pi.class.as_bytes());
                            let s = ty.display(
                                decl.as_ref().map(|c| &c.name),
                                decl.as_ref().and_then(|c| c.parent_name()),
                                None,
                            );
                            props.push((k, PropertySlot::Uninitialized(ZStr::from(s))));
                        }
                    }
                }
            }
        }
        Ok(ObjectDump {
            kind,
            properties: Some(props),
        })
    }

    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        self.serialize_object_vm(obj).map_err(|t| self.to_rt(t))
    }

    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        self.unserialize_create(class).map_err(|t| self.to_rt(t))
    }

    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        self.unserialize_finish(obj, props)
            .map_err(|t| self.to_rt(t))
    }

    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        self.unserialize_enum_vm(class, case)
            .map_err(|t| self.to_rt(t))
    }

    fn unserialize_enum_case(&mut self, class: &ZStr, case: &ZStr) -> RtResult<EnumCaseLookup> {
        self.unserialize_enum_case_vm(class, case)
            .map_err(|t| self.to_rt(t))
    }

    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.unserialize_custom_vm(class, data)
            .map_err(|t| self.to_rt(t))
    }

    fn is_enum(&mut self, obj: &Obj) -> bool {
        object::class_of(obj).is_enum()
    }

    fn is_closure(&mut self, obj: &Obj) -> bool {
        object::class_of(obj).lc_name.as_bytes() == b"closure"
    }

    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        if let Some(f) = crate::ext::hooks_of(obj).and_then(|h| h.count) {
            if let Some(n) = f(self, obj).map_err(|t| self.to_rt(t))? {
                return Ok(Some(n));
            }
        }
        if self.instance_of(obj, b"countable") {
            let r = self
                .call_method_name(obj, "count", Vec::new())
                .map_err(|t| self.to_rt(t))?;
            let n = self.to_long(&r).map_err(|t| self.to_rt(t))?;
            return Ok(Some(n));
        }
        Ok(None)
    }

    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        let (_, unmangled) = php_runtime::dump::unmangle_property_name(name.as_bytes());
        let n = ZStr::from(unmangled);
        let has = self
            .has_property(obj, &n, false)
            .map_err(|t| self.to_rt(t))?;
        if !has {
            return Ok(None);
        }
        self.read_property(obj, &n, crate::opcode::FetchMode::Is)
            .map(Some)
            .map_err(|t| self.to_rt(t))
    }

    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        self.write_mangled_prop(obj, name, value);
        Ok(())
    }

    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        let o = self.create_std_object_vm().map_err(|t| self.to_rt(t))?;
        if let Value::Object(obj) = &o {
            *object::data(obj).dynamic.borrow_mut() = Some(Box::new(props.to_proptable()));
        }
        Ok(o)
    }
}

impl Vm {
    /// Writes a property given a (possibly mangled) name, without
    /// visibility checks (unserialize).
    pub(crate) fn write_mangled_prop(&mut self, obj: &Obj, name: &ZStr, value: Value) {
        let (cls_part, unmangled) = php_runtime::dump::unmangle_property_name(name.as_bytes());
        let cls = object::class_of(obj).clone();
        let n = ZStr::from(unmangled);
        let target = match cls_part {
            Some(c) if c != b"*" => cls.props.iter().find(|p| {
                p.name == n && p.class.as_bytes() == c && p.visibility() == flags::PRIVATE
            }),
            _ => cls
                .prop_map
                .get(&n)
                .filter(|p| p.flags & flags::STATIC == 0),
        };
        if let Some(pi) = target {
            object::data(obj).slots.borrow_mut()[pi.slot as usize] = Some(value);
            return;
        }
        self.write_dynamic_prop(obj, n, value);
    }

    /// Serialization decision for an object.
    pub(crate) fn serialize_object_vm(&mut self, obj: &Obj) -> VmResult<SerializedObject> {
        let cls = object::class_of(obj).clone();
        if cls.is_enum() {
            let case = self.enum_case_name(obj).unwrap_or_default();
            return Ok(SerializedObject::Enum {
                class: cls.name.clone(),
                case,
            });
        }
        if cls.flags & cflags::NOT_SERIALIZABLE != 0 || cls.flags & cflags::ANON != 0 {
            let n = if cls.flags & cflags::ANON != 0 {
                "class@anonymous".to_string()
            } else {
                cls.name.to_string_lossy()
            };
            return Err(self.make_throw(
                "Exception",
                &format!("Serialization of '{n}' is not allowed"),
            ));
        }
        if let Some(f) = cls.hooks.as_ref().and_then(|h| h.serialize) {
            if let Some(props) = f(self, obj)? {
                return Ok(SerializedObject::Object {
                    class: cls.name.clone(),
                    props,
                });
            }
        }
        if let Some(m) = cls.magic.serialize.clone() {
            let r =
                php_runtime::serialize::serialize_locked(|| self.call_method(obj, &m, Vec::new()))?;
            return match r {
                Value::Array(a) => Ok(SerializedObject::Object {
                    class: cls.name.clone(),
                    props: a,
                }),
                other => Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "{}::__serialize() must return an array",
                        object::display_name(&cls.name)
                    ),
                ))
                .map(|x: SerializedObject| {
                    drop(other);
                    x
                }),
            };
        }
        if self.instance_of(obj, b"serializable") {
            let r = self.call_method_name(obj, "serialize", Vec::new())?;
            return match r {
                Value::Str(s) => Ok(SerializedObject::Custom {
                    class: cls.name.clone(),
                    data: s.as_bytes().to_vec(),
                }),
                Value::Null => Ok(SerializedObject::Null),
                _ => Err(self.make_throw(
                    "Exception",
                    &format!(
                        "{}::serialize() must return a string or NULL",
                        object::display_name(&cls.name)
                    ),
                )),
            };
        }
        let table = self
            .props_for(obj, PropertyPurpose::Serialize)?
            .unwrap_or_default();
        if let Some(m) = cls.magic.sleep.clone() {
            let r =
                php_runtime::serialize::serialize_locked(|| self.call_method(obj, &m, Vec::new()))?;
            let Value::Array(names) = r else {
                self.warning(&format!(
                    "serialize(): __sleep should return an array only containing the names of instance-variables to serialize"
                ))?;
                return Ok(SerializedObject::Null);
            };
            let mut out = HashTable::new();
            for (_, n) in names.iter() {
                let n = self.to_zstr(&n.deref())?;
                // Find the property by plain or mangled name.
                let mut found = None;
                for (k, v) in table.iter() {
                    if let ArrayKey::Str(ks) = k {
                        let (_, un) = php_runtime::dump::unmangle_property_name(ks.as_bytes());
                        if un == n.as_bytes() {
                            found = Some((k.clone(), v.clone()));
                            break;
                        }
                    }
                }
                match found {
                    Some((k, v)) => {
                        out.insert(k, v);
                    }
                    None => {
                        self.warning(&format!(
                            "serialize(): \"{}\" returned as member variable from __sleep() but does not exist",
                            n.to_string_lossy()
                        ))?;
                    }
                }
            }
            return Ok(SerializedObject::Object {
                class: cls.name.clone(),
                props: ZArray::from_table(out),
            });
        }
        Ok(SerializedObject::Object {
            class: cls.name.clone(),
            props: table,
        })
    }

    fn unserialize_create(&mut self, class: &ZStr) -> VmResult<Value> {
        let cls = match self.lookup_class(class)? {
            Some(c) => c,
            None => {
                let ic = self.find_class(b"__PHP_Incomplete_Class");
                match ic {
                    Some(ic) => {
                        let o = self.instantiate_unchecked(&ic)?;
                        self.write_dynamic_prop(
                            &o,
                            ZStr::from("__PHP_Incomplete_Class_Name"),
                            Value::Str(class.clone()),
                        );
                        return Ok(Value::Object(o));
                    }
                    None => {
                        return Err(self.make_throw(
                            "Error",
                            &format!("Class \"{}\" not found", class.to_string_lossy()),
                        ))
                    }
                }
            }
        };
        if cls.is_enum() {
            return Err(self.make_throw(
                "Exception",
                &format!("Unserialization of '{}' is not allowed", cls.name),
            ));
        }
        if cls.flags & cflags::NOT_SERIALIZABLE != 0 {
            return Err(self.make_throw(
                "Exception",
                &format!("Unserialization of '{}' is not allowed", cls.name),
            ));
        }
        let o = self.instantiate_unchecked(&cls)?;
        Ok(Value::Object(o))
    }

    fn unserialize_finish(&mut self, obj: &Value, props: ZArray) -> VmResult<()> {
        let Value::Object(o) = obj else {
            return Ok(());
        };
        let cls = object::class_of(o).clone();
        if let Some(f) = cls.hooks.as_ref().and_then(|h| h.unserialize) {
            if f(self, o, props.clone())? {
                return Ok(());
            }
        }
        if let Some(m) = cls.magic.unserialize.clone() {
            self.call_method(o, &m, vec![Value::Array(props)])?;
            return Ok(());
        }
        for (k, v) in props.iter() {
            let name = match k {
                ArrayKey::Str(s) => s.clone(),
                ArrayKey::Int(i) => ZStr::from(i.to_string()),
            };
            self.write_mangled_prop(o, &name, v.clone());
        }
        if let Some(m) = cls.magic.wakeup.clone() {
            self.call_method(o, &m, Vec::new())?;
        }
        Ok(())
    }

    fn unserialize_enum_vm(&mut self, class: &ZStr, case: &ZStr) -> VmResult<Value> {
        let Some(cls) = self.lookup_class(class)? else {
            return Err(self.make_throw(
                "Error",
                &format!("Class '{}' not found", class.to_string_lossy()),
            ));
        };
        if !cls.is_enum() {
            return Err(self.make_throw(
                "Error",
                &format!("Class '{}' is not an enum", class.to_string_lossy()),
            ));
        }
        self.enum_case(&cls, case)
    }

    /// `E:` resolution for php-runtime's parser (session, SPL legacy
    /// formats): each failure is reported by the parser with php-src's
    /// warning and offset instead of an exception.
    fn unserialize_enum_case_vm(&mut self, class: &ZStr, case: &ZStr) -> VmResult<EnumCaseLookup> {
        let Some(cls) = self.lookup_class(class)? else {
            return Ok(EnumCaseLookup::ClassNotFound);
        };
        if !cls.is_enum() {
            return Ok(EnumCaseLookup::NotAnEnum);
        }
        let Some(c) = cls.find_const(case.as_bytes()).cloned() else {
            return Ok(EnumCaseLookup::UndefinedCase);
        };
        if !c.is_case {
            return Ok(EnumCaseLookup::NotACase);
        }
        Ok(EnumCaseLookup::Found(self.enum_case(&cls, case)?))
    }

    fn unserialize_custom_vm(&mut self, class: &ZStr, data: &[u8]) -> VmResult<Value> {
        let cls = self.lookup_class_or_error(class)?;
        if cls.flags & cflags::NOT_SERIALIZABLE != 0 {
            return Err(self.make_throw(
                "Exception",
                &format!("Unserialization of '{}' is not allowed", cls.name),
            ));
        }
        let o = self.instantiate_unchecked(&cls)?;
        self.call_method_name(&o, "unserialize", vec![Value::Str(ZStr::from(data))])?;
        Ok(Value::Object(o))
    }
}

impl OutputHost for Vm {
    type Callback = Value;

    fn output(&mut self) -> &mut OutputStack<Value> {
        &mut self.output
    }

    fn call_handler(&mut self, callback: &mut Value, buffer: &[u8], mode: i32) -> Option<Vec<u8>> {
        // Kept in a local and stored last: the user exception handler run
        // below may start and flush buffers of its own, re-entering this.
        let mut threw = false;
        // Named native handlers (`php_output_handler_alias`).
        let alias = match callback {
            Value::Str(n) => self.output_handler_aliases.get(n).copied(),
            _ => None,
        };
        // `zend_handle_delayed_oom` leaves an unwind exit pending, and
        // `zend_call_function` runs no user code under a pending exception:
        // a user handler fails while the OOM fatal discards its buffer.
        if self.reporting_oom && alias.is_none() {
            self.output_handler_threw = false;
            return None;
        }
        let r = if let Some(f) = alias {
            (f.handler)(self, buffer, mode)
        } else {
            self.call_value(
                callback,
                vec![Value::Str(ZStr::from(buffer)), Value::Int(i64::from(mode))],
            )
            .and_then(|value| match value {
                Value::Bool(false) => Ok(None),
                Value::Bool(true) | Value::Null => Ok(Some(Vec::new())),
                Value::Str(s) => Ok(Some(s.as_bytes().to_vec())),
                other => self.to_zstr(&other).map(|s| Some(s.as_bytes().to_vec())),
            })
        };
        let output = match r {
            Ok(output) => output,
            // `zend_throw_exception_internal()` with no executing frame
            // (`php_output_end_all()` at request shutdown): the user
            // exception handler gets it; without one the exception is a
            // fatal error, and the bailout discards the buffered output.
            Err(Throw::Exception(e))
                if !self.is_executing() && !crate::fiber::is_graceful_exit(&e) =>
            {
                threw = true;
                if let Err(t) = self.uncaught_exception(e) {
                    self.pending_throw = Some(t);
                }
                None
            }
            Err(t) => {
                threw = true;
                if self.pending_throw.is_none() {
                    self.pending_throw = Some(t);
                }
                None
            }
        };
        // Return values can release captured objects while OG(running) is set.
        self.release_output_destructors();
        // Native callbacks may only record an OOM until they return.
        if let Err(t) = self.raise_pending_oom() {
            self.pending_throw = Some(t);
        }
        self.output_handler_threw = threw;
        output
    }

    fn sapi_write(&mut self, data: &[u8]) {
        self.headers_sent = true;
        if self.sapi_module.ub_write(&mut *self.stdout, data).is_err() {
            if let Some(status) = self.sapi_module.exit_status_on_abort() {
                self.exit_status = status;
            }
            self.handle_aborted_connection();
        }
    }

    #[inline]
    fn send_headers(&mut self) -> bool {
        // Every write after the first: `SG(headers_sent)` is already set.
        if self.headers_sent {
            return true;
        }
        crate::builtins::headers::send_headers(self)
    }

    fn check_handler_conflicts(&mut self, name: &[u8]) -> bool {
        self.output_handler_conflicts
            .get(&ZStr::from(name))
            .copied()
            .is_none_or(|check| check(self, name))
    }

    fn before_internal_handler(&mut self, name: &[u8]) {
        if let Some(hook) = self.output_handler_hooks.get(&ZStr::from(name)).copied() {
            hook(self);
        }
    }

    fn has_pending_exception(&self) -> bool {
        self.pending_throw.is_some()
    }

    fn handler_threw(&self) -> bool {
        self.output_handler_threw || self.pending_throw.is_some()
    }

    fn has_pending_fatal(&self) -> bool {
        // `exit` while user code runs is an unwind exception: the handler
        // fails and the stack unwinds normally. Only a frameless exit (at
        // shutdown) bails out like a fatal.
        match self.pending_throw {
            Some(Throw::Fatal) => true,
            Some(Throw::Exit) => self.frames.is_empty(),
            _ => false,
        }
    }

    fn release_handler_callback(&mut self, callback: Value) {
        drop(callback);
        self.release_output_destructors();
    }

    fn sapi_flush(&mut self) {
        if self.sapi_module.flush(&mut *self.stdout).is_err() {
            self.handle_aborted_connection();
        }
    }

    fn report(&mut self, diagnostic: php_support::Diagnostic) {
        let msg = diagnostic.message.clone();
        let level = i64::from(diagnostic.level.0);
        let r = if diagnostic.docref {
            self.docref(level, &msg)
        } else {
            self.error(level, &msg)
        };
        if let Err(t) = r {
            if matches!(t, Throw::Fatal) || self.pending_throw.is_none() {
                self.pending_throw = Some(t);
            }
        }
    }
}
