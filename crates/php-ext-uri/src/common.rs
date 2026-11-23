// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Helpers shared by the RFC 3986 and WHATWG bindings: enum cases, error
//! objects, raw property access and argument conversion.

use std::rc::Rc;

use php_runtime::{ArrayKey, HashTable, Obj, Value, ZArray, ZStr};
use php_uri::{ErrorClass, UriComparisonMode, UriError, UrlValidationError};
use php_vm::class::Class;
use php_vm::ext::{Args, Throw, VmResult};
use php_vm::Vm;

/// `Uri\WhatWg\UrlValidationError`.
pub(crate) const VALIDATION_ERROR_CLASS: &str = "Uri\\WhatWg\\UrlValidationError";
/// `Uri\WhatWg\UrlValidationErrorType`.
pub(crate) const VALIDATION_ERROR_TYPE_ENUM: &str = "Uri\\WhatWg\\UrlValidationErrorType";
/// A registered class by name (`zend_lookup_class` without autoload).
pub(crate) fn class(vm: &mut Vm, name: &str) -> VmResult<Rc<Class>> {
    match vm.find_class(name.as_bytes()) {
        Some(c) => Ok(c),
        None => Err(vm.throw("Error", &format!("Class \"{name}\" not found"))),
    }
}

/// `zend_enum_get_case_cstr(ce, case)`.
pub(crate) fn enum_case(vm: &mut Vm, enum_name: &str, case: &str) -> VmResult<Value> {
    let constant = Value::string("constant");
    vm.call_value(
        &constant,
        vec![Value::string(format!("{enum_name}::{case}"))],
    )
}

/// The case name of an enum instance (`zend_enum_fetch_case_name`).
pub(crate) fn enum_case_name(vm: &mut Vm, v: &Value) -> VmResult<Option<String>> {
    let Value::Object(o) = v else {
        return Ok(None);
    };
    match vm.get_property(o, "name")? {
        Value::Str(s) => Ok(Some(s.to_string_lossy())),
        _ => Ok(None),
    }
}

/// The `$comparisonMode` argument (default `ExcludeFragment`).
pub(crate) fn comparison_mode(vm: &mut Vm, v: Option<Value>) -> VmResult<UriComparisonMode> {
    let Some(v) = v else {
        return Ok(UriComparisonMode::ExcludeFragment);
    };
    Ok(match enum_case_name(vm, &v)?.as_deref() {
        Some("IncludeFragment") => UriComparisonMode::IncludeFragment,
        _ => UriComparisonMode::ExcludeFragment,
    })
}

/// A PHP string value from bytes.
pub(crate) fn bytes(v: Vec<u8>) -> Value {
    Value::Str(ZStr::from(v))
}

/// A `?string` value.
pub(crate) fn opt_bytes(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Null, bytes)
}

/// A `?int` value.
pub(crate) fn opt_int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Int)
}

/// The `?string` argument `n` as bytes.
pub(crate) fn str_or_null(a: &Args, n: usize) -> Option<Vec<u8>> {
    match a.opt(n) {
        Some(Value::Str(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// The `?int` argument `n`.
pub(crate) fn int_or_null(a: &Args, n: usize) -> Option<i64> {
    match a.opt(n) {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    }
}

/// Reads a declared property slot directly (no visibility checks), like
/// the `Z_*_PROP_*_DEREF_P` accessors of `php_uri.c`.
pub(crate) fn raw_prop(o: &Obj, name: &str) -> Value {
    let data = php_vm::object::data(o);
    let Some(pi) = data
        .class
        .props
        .iter()
        .rev()
        .find(|p| p.name.as_bytes() == name.as_bytes())
    else {
        return Value::Null;
    };
    let slots = data.slots.borrow();
    match slots.get(pi.slot as usize) {
        Some(Some(Value::Reference(r))) => r.get(),
        Some(Some(v)) => v.clone(),
        _ => Value::Null,
    }
}

/// Initializes a declared property and marks a readonly one as
/// initialized (`zend_update_property` from inside the class scope).
pub(crate) fn init_prop(vm: &mut Vm, o: &Obj, name: &str, v: Value) {
    vm.set_property_raw(o, name, v);
    let data = php_vm::object::data(o);
    if let Some(pi) = data
        .class
        .props
        .iter()
        .rev()
        .find(|p| p.name.as_bytes() == name.as_bytes())
    {
        data.set_slot_unset(pi.slot as usize, true);
    }
}

/// Creates one `Uri\WhatWg\UrlValidationError` (`append_validation_error`).
pub(crate) fn validation_error(vm: &mut Vm, e: &UrlValidationError) -> VmResult<Value> {
    let cls = class(vm, VALIDATION_ERROR_CLASS)?;
    let o = vm.new_object(&cls)?;
    init_prop(vm, &o, "context", bytes(e.context.to_vec()));
    let ty = enum_case(vm, VALIDATION_ERROR_TYPE_ENUM, e.error_type.name())?;
    init_prop(vm, &o, "type", ty);
    init_prop(vm, &o, "failure", Value::Bool(e.failure));
    Ok(Value::Object(o))
}

/// The list of `UrlValidationError` objects for `$errors`/`$softErrors` and
/// `InvalidUrlException::$errors`.
pub(crate) fn validation_errors(vm: &mut Vm, errors: &[UrlValidationError]) -> VmResult<Value> {
    let mut ht = HashTable::with_capacity(errors.len());
    for e in errors {
        // Each error copies the rest of the input as its context
        // (quadratic in the input, as in php-src): stop at the limit.
        php_memlimit::poll()?;
        let v = validation_error(vm, e)?;
        let _ = ht.push(v);
    }
    Ok(Value::Array(ZArray::from_table(ht)))
}

/// Throws a library error as the PHP exception it describes.
pub(crate) fn throw(vm: &mut Vm, e: &UriError) -> Throw {
    if e.class != ErrorClass::InvalidUrlException {
        return vm.throw(e.class.class_name(), &e.message);
    }
    match invalid_url_exception(vm, e) {
        Ok(o) => Throw::Exception(o),
        Err(t) => t,
    }
}

/// `zend_throw_exception(InvalidUrlException, msg)` followed by the update
/// of its `errors` property.
fn invalid_url_exception(vm: &mut Vm, e: &UriError) -> VmResult<Obj> {
    let errors = validation_errors(vm, &e.errors)?;
    let cls = class(vm, ErrorClass::InvalidUrlException.class_name())?;
    let o = vm.new_object(&cls)?;
    vm.set_property_raw(&o, "message", Value::string(e.message.as_str()));
    init_prop(vm, &o, "errors", errors);
    Ok(o)
}

/// Throws `Exception: Invalid serialization data for <class> object`,
/// optionally chained to `Error: Cannot modify readonly object of class
/// <class>` (`uri_unserialize()` on an initialized object).
pub(crate) fn throw_invalid_serialization(vm: &mut Vm, class_name: &str, reinit: bool) -> Throw {
    let msg = php_uri::api::invalid_serialization_data(class_name).message;
    if !reinit {
        return vm.throw("Exception", &msg);
    }
    let previous = php_uri::api::cannot_modify_readonly_object(class_name).message;
    let build = |vm: &mut Vm| -> VmResult<Obj> {
        let err_cls = class(vm, "Error")?;
        let err = vm.new_object(&err_cls)?;
        vm.set_property_raw(&err, "message", Value::string(previous.as_str()));
        let exc_cls = class(vm, "Exception")?;
        let exc = vm.new_object(&exc_cls)?;
        vm.set_property_raw(&exc, "message", Value::string(msg.as_str()));
        vm.set_property_raw(&exc, "previous", Value::Object(err));
        Ok(exc)
    };
    match build(vm) {
        Ok(o) => Throw::Exception(o),
        Err(t) => t,
    }
}

/// `Error: Cannot modify readonly object of class <class>`.
pub(crate) fn throw_reinit(vm: &mut Vm, class_name: &str) -> Throw {
    let e = php_uri::api::cannot_modify_readonly_object(class_name);
    throw(vm, &e)
}

/// The `__serialize()` array `[["uri" => $uri], []]`.
pub(crate) fn serialized(uri: Vec<u8>) -> Value {
    let mut state = HashTable::new();
    state.insert(
        ArrayKey::from_bytes(php_uri::api::SERIALIZE_URI_FIELD.as_bytes()),
        bytes(uri),
    );
    let mut out = HashTable::new();
    let _ = out.push(Value::Array(ZArray::from_table(state)));
    let _ = out.push(Value::Array(ZArray::new()));
    Value::Array(ZArray::from_table(out))
}

/// Validates the `__unserialize()` array and returns the `uri` string;
/// `None` means "Invalid serialization data". The second array must be
/// empty, which is checked by [`unserialize_props_ok`] after parsing (PHP's
/// order of checks).
pub(crate) fn unserialize_uri(data: &ZArray) -> Option<Vec<u8>> {
    if data.len() != 2 {
        return None;
    }
    let Some(Value::Array(state)) = index(data, 0) else {
        return None;
    };
    if state.len() != 1 {
        return None;
    }
    match state.get_str(php_uri::api::SERIALIZE_URI_FIELD.as_bytes()) {
        Some(Value::Str(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// The second `__unserialize()` array must be an empty array.
pub(crate) fn unserialize_props_ok(data: &ZArray) -> bool {
    matches!(index(data, 1), Some(Value::Array(a)) if a.is_empty())
}

/// `zend_hash_index_find(data, i)`.
fn index(data: &ZArray, i: i64) -> Option<&Value> {
    data.get(&ArrayKey::Int(i))
}

/// `__debugInfo()` from the library's ordered pairs.
pub(crate) fn debug_info(pairs: Vec<(&'static str, php_uri::api::DebugValue)>) -> Value {
    use php_uri::api::DebugValue;
    let mut ht = HashTable::new();
    for (k, v) in pairs {
        let v = match v {
            DebugValue::Null => Value::Null,
            DebugValue::Str(s) => bytes(s),
            DebugValue::Int(n) => Value::Int(n),
        };
        ht.insert(ArrayKey::from_bytes(k.as_bytes()), v);
    }
    Value::Array(ZArray::from_table(ht))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(v: &Value) -> ZArray {
        match v {
            Value::Array(a) => a.clone(),
            _ => panic!("not an array"),
        }
    }

    #[test]
    fn serialize_round_trip() {
        let data = arr(&serialized(b"https://example.com".to_vec()));
        assert_eq!(
            unserialize_uri(&data).as_deref(),
            Some(&b"https://example.com"[..])
        );
        assert!(unserialize_props_ok(&data));
    }

    #[test]
    fn unserialize_rejects_bad_shapes() {
        assert_eq!(unserialize_uri(&ZArray::new()), None);
        let mut t = HashTable::new();
        let _ = t.push(Value::Null);
        let _ = t.push(Value::Array(ZArray::new()));
        assert_eq!(unserialize_uri(&ZArray::from_table(t)), None);
        // Numeric-string keys are not integer keys.
        let mut state = HashTable::new();
        state.insert(ArrayKey::from_bytes(b"uri"), Value::string("/x"));
        let mut t = HashTable::new();
        t.insert_str_raw(ZStr::from("0"), Value::Array(ZArray::from_table(state)));
        t.insert_str_raw(ZStr::from("1"), Value::Array(ZArray::new()));
        assert_eq!(unserialize_uri(&ZArray::from_table(t)), None);
    }

    #[test]
    fn value_helpers() {
        assert!(matches!(opt_int(None), Value::Null));
        assert!(matches!(opt_int(Some(3)), Value::Int(3)));
        assert!(matches!(opt_bytes(Some(b"a".to_vec())), Value::Str(s) if s.as_bytes() == b"a"));
    }
}
