// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Small helpers shared by the modules.

use php_runtime::{Value, ZStr};
use php_vm::class::{AttributeInfo, ConstExpr};
use php_vm::vm::FrameKind;
use php_vm::Vm;

/// `E_ERROR`.
pub(crate) const E_ERROR: i64 = 1;
/// `E_WARNING`.
pub(crate) const E_WARNING: i64 = 2;
/// `E_NOTICE`.
pub(crate) const E_NOTICE: i64 = 8;

/// Whether the running native function was called from code compiled
/// with `declare(strict_types=1)` (`ZEND_ARG_USES_STRICT_TYPES()`): calls
/// from internal code are never strict.
pub(crate) fn caller_strict(vm: &Vm) -> bool {
    let n = vm.frames.len();
    if n < 2 {
        return false;
    }
    let caller = &vm.frames[n - 2];
    caller.kind != FrameKind::Builtin && caller.code.strict_types
}

/// `zend_zval_type_name`.
pub(crate) fn type_name(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Object(o) => o.class_name().to_string_lossy(),
        Value::Resource(_) => "resource".into(),
        Value::Reference(r) => type_name(&r.get()),
    }
}

/// `zend_zval_value_name`: like [`type_name`] but `true`/`false` and the
/// class name for objects.
pub(crate) fn value_name(v: &Value) -> String {
    match v {
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        Value::Reference(r) => value_name(&r.get()),
        other => type_name(other),
    }
}

/// An attribute with positional (`None`) and named arguments.
pub(crate) fn attribute(name: &str, args: Vec<(Option<&str>, Value)>) -> AttributeInfo {
    AttributeInfo {
        name: ZStr::from(name),
        args: args
            .into_iter()
            .map(|(n, v)| (n.map(ZStr::from), ConstExpr::Value(v)))
            .collect(),
        line: 0,
        strict: false,
    }
}

/// A string value.
pub(crate) fn s(text: &str) -> Value {
    Value::Str(ZStr::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(value_name(&Value::Bool(true)), "true");
        assert_eq!(type_name(&Value::Bool(true)), "bool");
        assert_eq!(value_name(&Value::Float(1.0)), "float");
        assert_eq!(value_name(&Value::Null), "null");
    }

    #[test]
    fn attribute_args() {
        let a = attribute("Foo", vec![(Some("arg"), s("x")), (None, Value::Int(1))]);
        assert_eq!(a.name.as_bytes(), b"Foo");
        assert_eq!(a.args.len(), 2);
        assert!(a.args[0].0.is_some());
    }
}
