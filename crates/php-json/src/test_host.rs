// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A small object model implementing [`JsonHost`] for the unit tests.

use crate::host::{GuardKey, JsonHost};
use php_runtime::{
    ArrayKey, Diagnostic, ErrorLevel, HashTable, Obj, ObjectDump, ObjectKind, PhpObject,
    PropertyPurpose, PropertySlot, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

/// `jsonSerialize()` implementation of a test class.
pub(crate) type SerializeFn = Rc<dyn Fn(&mut TestHost, &Obj) -> RtResult<Value>>;

/// The kind of a test object.
#[derive(Clone)]
pub(crate) enum Kind {
    Plain,
    RebuiltProperties,
    Serializable(SerializeFn),
    UnitEnum(ZStr),
    BackedEnum(ZStr, Value),
}

/// A test object: a class name, a kind and a property table.
pub(crate) struct TestObject {
    handle: u32,
    class: ZStr,
    pub(crate) kind: Kind,
    pub(crate) props: RefCell<HashTable>,
}

impl PhpObject for TestObject {
    fn handle(&self) -> u32 {
        self.handle
    }
    fn class_name(&self) -> ZStr {
        self.class.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The test host.
#[derive(Default)]
pub(crate) struct TestHost {
    next_handle: u32,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) serialize_precision: Option<i64>,
    /// Log of `jsonSerialize()` calls etc.
    pub(crate) log: Vec<String>,
}

impl std::fmt::Debug for TestHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestHost").finish_non_exhaustive()
    }
}

fn obj_ref(o: &Obj) -> &TestObject {
    o.downcast_ref::<TestObject>().expect("test object")
}

impl TestHost {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Creates an object with the given class, kind and properties (keys
    /// used verbatim, possibly mangled).
    pub(crate) fn object(&mut self, class: &str, kind: Kind, props: &[(&[u8], Value)]) -> Value {
        let mut table = HashTable::new();
        for (k, v) in props {
            table.insert_str_raw(ZStr::from(*k), v.clone());
        }
        self.object_with_table(class, kind, table)
    }

    pub(crate) fn object_with_table(&mut self, class: &str, kind: Kind, table: HashTable) -> Value {
        self.next_handle += 1;
        Value::Object(Obj::new(TestObject {
            handle: self.next_handle,
            class: ZStr::from(class),
            kind,
            props: RefCell::new(table),
        }))
    }

    /// A `JsonSerializable` object whose `jsonSerialize()` runs `f`.
    pub(crate) fn serializable(
        &mut self,
        class: &str,
        props: &[(&[u8], Value)],
        f: impl Fn(&mut TestHost, &Obj) -> RtResult<Value> + 'static,
    ) -> Value {
        self.object(class, Kind::Serializable(Rc::new(f)), props)
    }

    /// Sets a property of a test object.
    pub(crate) fn set_prop(obj: &Value, name: &[u8], value: Value) {
        if let Value::Object(o) = obj {
            obj_ref(o)
                .props
                .borrow_mut()
                .insert_str_raw(ZStr::from(name), value);
        }
    }
}

impl RuntimeContext for TestHost {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.diagnostics.push(Diagnostic {
            level,
            message: message.to_string(),
        });
        Ok(())
    }

    fn serialize_precision(&self) -> i64 {
        self.serialize_precision.unwrap_or(-1)
    }

    fn object_properties(&mut self, obj: &Obj, _: PropertyPurpose) -> RtResult<Option<ZArray>> {
        Ok(Some(ZArray::from_table(
            obj_ref(obj).props.borrow().clone(),
        )))
    }

    fn object_dump(&mut self, obj: &Obj, _: PropertyPurpose) -> RtResult<ObjectDump> {
        let t = obj_ref(obj);
        let kind = match &t.kind {
            Kind::UnitEnum(case) => ObjectKind::Enum {
                case_name: case.clone(),
                backing_type: None,
                has_debug_info: false,
            },
            Kind::BackedEnum(case, v) => ObjectKind::Enum {
                case_name: case.clone(),
                backing_type: Some(if matches!(v, Value::Int(_)) {
                    "int"
                } else {
                    "string"
                }),
                has_debug_info: false,
            },
            _ if t.class == "stdClass" => ObjectKind::StdClass,
            _ => ObjectKind::Regular { lazy_prefix: "" },
        };
        let props = t
            .props
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
            .collect();
        Ok(ObjectDump {
            kind,
            properties: Some(props),
        })
    }

    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        Ok(self.object(&class.to_string_lossy(), Kind::Plain, &[]))
    }

    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        if let Value::Object(o) = obj {
            let mut t = obj_ref(o).props.borrow_mut();
            for (k, v) in props.iter() {
                let key = match k {
                    ArrayKey::Int(i) => ZStr::from(i.to_string()),
                    ArrayKey::Str(s) => s.clone(),
                };
                t.insert_str_raw(key, v.clone());
            }
        }
        Ok(())
    }

    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        Ok(self.object_with_table("stdClass", Kind::Plain, props.to_proptable()))
    }
}

impl JsonHost for TestHost {
    fn json_native_properties(&mut self, obj: &Obj) -> RtResult<Option<(ZArray, GuardKey)>> {
        if !matches!(obj_ref(obj).kind, Kind::RebuiltProperties) {
            return Ok(None);
        }
        let table = ZArray::from_table(obj_ref(obj).props.borrow().clone());
        let guard = GuardKey::array(&table);
        Ok(Some((table, guard)))
    }

    fn is_json_serializable(&mut self, obj: &Obj) -> bool {
        matches!(obj_ref(obj).kind, Kind::Serializable(_))
    }

    fn json_serialize(&mut self, obj: &Obj) -> RtResult<Value> {
        let Kind::Serializable(f) = obj_ref(obj).kind.clone() else {
            return Err(RuntimeError::Error("not serializable".into()));
        };
        self.log
            .push(format!("{}::jsonSerialize()", obj.class_name()));
        f(self, obj)
    }

    fn is_enum(&mut self, obj: &Obj) -> bool {
        matches!(obj_ref(obj).kind, Kind::UnitEnum(_) | Kind::BackedEnum(..))
    }

    fn enum_backing_value(&mut self, obj: &Obj) -> Option<Value> {
        match &obj_ref(obj).kind {
            Kind::BackedEnum(_, v) => Some(v.clone()),
            _ => None,
        }
    }

    fn std_class_write(&mut self, obj: &Value, name: ZStr, value: Value) -> RtResult<()> {
        Self::set_prop(obj, name.as_bytes(), value);
        Ok(())
    }

    fn json_exception(&mut self, message: &str, code: i64) -> RuntimeError {
        RuntimeError::Error(format!("JsonException({code}): {message}"))
    }
}
