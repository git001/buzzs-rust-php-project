// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/var.c`, `type.c`: dumping, type inspection and conversion.

use super::dump::{self, Out};
use super::reg;
use super::util::{b, i, s, sv};
use crate::func::Args;
use crate::object;
use crate::vm::{Vm, VmResult};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};

fn var_dump(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _scope = dump::begin_dump();
    for i in 0..a.values.len() {
        let v = a.values[i].deref();
        let mut out = Out::stream();
        let r = dump::var_dump(vm, &mut out, &v, 1, None);
        out.flush(vm);
        if r.is_err() {
            dump::clear_deferred_throw();
        }
        r?;
    }
    raise_after_dump(vm)?;
    Ok(Value::Null)
}

fn debug_zval_dump(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _scope = dump::begin_dump();
    for i in 0..a.values.len() {
        let v = a.values[i].clone();
        let mut out = Out::stream();
        // Our clone and the VM's argument copy are not counted by PHP.
        let r = dump::debug_zval_dump(vm, &mut out, &v, 2);
        out.flush(vm);
        if r.is_err() {
            dump::clear_deferred_throw();
        }
        r?;
    }
    raise_after_dump(vm)?;
    Ok(Value::Null)
}

fn print_r(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _scope = dump::begin_dump();
    let v = a.at(0);
    if b(a, 1) {
        // Output produced by `__debugInfo()` is not captured.
        let mut out = Out::collect();
        let r = dump::print_r(vm, &mut out, &v, 0);
        if r.is_err() {
            dump::clear_deferred_throw();
        }
        r?;
        let s = out.finish(vm)?;
        raise_after_dump(vm)?;
        return Ok(sv(s));
    }
    let mut out = Out::stream();
    let r = dump::print_r(vm, &mut out, &v, 0);
    out.flush(vm);
    if r.is_err() {
        dump::clear_deferred_throw();
    }
    r?;
    raise_after_dump(vm)?;
    Ok(Value::Bool(true))
}

fn var_export(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    if b(a, 1) {
        let mut out = Out::collect();
        dump::var_export(vm, &mut out, &v, 1)?;
        return Ok(sv(out.finish(vm)?));
    }
    // `php_var_export_ex` builds the whole export in a buffer: warnings
    // (circular references) and output of user code run meanwhile come
    // first, and a partial export is still written on an exception.
    let mut out = Out::collect();
    let r = dump::var_export(vm, &mut out, &v, 1);
    let bytes = out.finish(vm)?;
    if !bytes.is_empty() {
        vm.echo(&bytes);
    }
    r?;
    Ok(Value::Null)
}

fn gettype(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(sv(a.at(0).legacy_type_name()))
}

fn get_debug_type(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    Ok(sv(match &v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Resource(r) => {
            if r.is_closed() {
                "resource (closed)".into()
            } else {
                format!(
                    "resource ({})",
                    r.type_name()
                        .map(|t| t.to_string_lossy())
                        .unwrap_or_default()
                )
            }
        }
        Value::Object(o) => {
            let c = object::class_of(o);
            if c.flags & crate::class::cflags::ANON != 0 {
                let parent = c
                    .parent
                    .as_ref()
                    .map(|p| p.name.to_string_lossy())
                    .or_else(|| c.interfaces.first().map(|i| i.name.to_string_lossy()));
                format!("{}@anonymous", parent.unwrap_or_else(|| "class".into()))
            } else {
                c.name.to_string_lossy()
            }
        }
        Value::Reference(_) => String::new(),
    }))
}

fn settype(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(r) = a.reference(0) else {
        return Ok(Value::Bool(false));
    };
    let t = s(a, 1).to_ascii_lowercase();
    let v = r.get();
    let nv = match t.as_bytes() {
        b"int" | b"integer" => Value::Int(vm.to_long(&v)?),
        b"float" | b"double" => Value::Float(vm.to_double(&v)?),
        // `convert_to_string()` leaves "" behind when it throws.
        b"string" => match vm.to_zstr(&v) {
            Ok(s) => Value::Str(s),
            Err(e) => {
                r.set(Value::Str(ZStr::empty()));
                return Err(e);
            }
        },
        b"bool" | b"boolean" => Value::Bool(vm.to_bool(&v)?),
        b"array" => match v {
            Value::Array(_) => v,
            Value::Null => Value::Array(ZArray::new()),
            Value::Object(o) => Value::Array(vm.object_to_array(&o)?),
            Value::Float(f) if f.is_nan() => {
                vm.warning("unexpected NAN value was coerced to array")?;
                Value::Array(ZArray::from_values([r.get()]))
            }
            other => Value::Array(ZArray::from_values([other])),
        },
        b"null" => {
            if matches!(v, Value::Float(f) if f.is_nan()) {
                vm.warning("unexpected NAN value was coerced to null")?;
            }
            Value::Null
        }
        b"object" => match v {
            Value::Object(_) => v,
            Value::Array(arr) => vm.array_to_object(&arr)?,
            Value::Null => vm.create_std_object_vm()?,
            other => {
                let other = if matches!(other, Value::Float(f) if f.is_nan()) {
                    vm.warning("unexpected NAN value was coerced to object")?;
                    r.get()
                } else {
                    other
                };
                let o = vm.create_std_object_vm()?;
                if let Value::Object(obj) = &o {
                    vm.write_dynamic_prop(obj, ZStr::from("scalar"), other);
                }
                o
            }
        },
        b"resource" => {
            return Err(vm.make_throw("ValueError", "Cannot convert to resource type"));
        }
        _ => return Err(super::util::value_error(vm, a, 2, "must be a valid type")),
    };
    vm.assign_ref(&r, nv)?;
    Ok(Value::Bool(true))
}

fn intval(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let base = if a.has(1) { i(a, 1) } else { 10 };
    if base != 10 {
        if let Value::Str(s) = &v {
            let t = s.to_string_lossy();
            let t = t.trim_start();
            let (neg, t) = match t.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, t.strip_prefix('+').unwrap_or(t)),
            };
            let (base, t) = if base == 16 {
                (
                    16,
                    t.strip_prefix("0x")
                        .or_else(|| t.strip_prefix("0X"))
                        .unwrap_or(t),
                )
            } else if base == 8 {
                (
                    8,
                    t.strip_prefix("0o")
                        .or_else(|| t.strip_prefix("0O"))
                        .unwrap_or(t),
                )
            } else if base == 2 {
                (
                    2,
                    t.strip_prefix("0b")
                        .or_else(|| t.strip_prefix("0B"))
                        .unwrap_or(t),
                )
            } else if base == 0 {
                if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                    (16, r)
                } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                    (2, r)
                } else if let Some(r) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                    (8, r)
                } else if t.starts_with('0') && t.len() > 1 {
                    (8, &t[1..])
                } else {
                    (10, t)
                }
            } else {
                (base, t)
            };
            let digits: String = t
                .chars()
                .take_while(|c| c.to_digit(u32::try_from(base).unwrap_or(10)).is_some())
                .collect();
            let n = i128::from_str_radix(&digits, u32::try_from(base).unwrap_or(10)).unwrap_or(0);
            let n = if neg { -n } else { n };
            let n = n.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
            return Ok(Value::Int(i64::try_from(n).unwrap_or(0)));
        }
    }
    Ok(Value::Int(vm.to_long(&v)?))
}

fn floatval(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    Ok(Value::Float(vm.to_double(&v)?))
}

fn strval(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    Ok(Value::Str(vm.to_zstr(&v)?))
}

fn boolval(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    Ok(Value::Bool(vm.to_bool(&v)?))
}

macro_rules! is_fn {
    ($name:ident, $pat:pat) => {
        fn $name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            Ok(Value::Bool(matches!(a.at(0), $pat)))
        }
    };
}

is_fn!(is_int, Value::Int(_));
is_fn!(is_float, Value::Float(_));
is_fn!(is_string, Value::Str(_));
is_fn!(is_bool, Value::Bool(_));
is_fn!(is_array, Value::Array(_));
is_fn!(is_object, Value::Object(_));
is_fn!(is_null, Value::Null);
is_fn!(
    is_scalar,
    Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
);

fn is_resource(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(
        matches!(a.at(0), Value::Resource(r) if !r.is_closed()),
    ))
}

fn is_numeric(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(match a.at(0) {
        Value::Int(_) | Value::Float(_) => true,
        Value::Str(s) => php_runtime::numeric::is_numeric_string_ex(s.as_bytes(), false).is_some(),
        _ => false,
    }))
}

fn is_iterable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(match a.at(0) {
        Value::Array(_) => true,
        Value::Object(o) => vm.instance_of(&o, b"traversable"),
        _ => false,
    }))
}

fn is_countable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(match a.at(0) {
        Value::Array(_) => true,
        Value::Object(o) => vm.instance_of(&o, b"countable"),
        _ => false,
    }))
}

fn is_callable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let syntax_only = b(a, 1);
    let r = if syntax_only {
        match &v {
            Value::Str(_) => true,
            Value::Object(_) => vm.is_callable_value(&v),
            Value::Array(x) => {
                x.len() == 2
                    && matches!(
                        x.get(&ArrayKey::Int(0)).map(Value::deref),
                        Some(Value::Object(_) | Value::Str(_))
                    )
                    && matches!(
                        x.get(&ArrayKey::Int(1)).map(Value::deref),
                        Some(Value::Str(_))
                    )
            }
            _ => false,
        }
    } else {
        let is_relative_array = match &v {
            Value::Array(x) if x.len() == 2 => {
                let target = x.get(&ArrayKey::Int(0)).map(Value::deref);
                let method = x.get(&ArrayKey::Int(1)).map(Value::deref);
                match (target, method) {
                    (Some(Value::Object(o)), Some(Value::Str(m))) => {
                        let cname = object::class_of(&o).name.clone();
                        Some((cname, m.clone()))
                    }
                    (Some(Value::Str(cname)), Some(Value::Str(m))) => {
                        Some((cname.clone(), m.clone()))
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        if let Some((target_name, method_name)) = is_relative_array {
            let m_bytes = method_name.as_bytes();
            let is_relative = if let Some(p) = m_bytes.windows(2).position(|w| w == b"::") {
                let scope = &m_bytes[..p];
                scope.eq_ignore_ascii_case(b"parent")
                    || scope.eq_ignore_ascii_case(b"self")
                    || scope.eq_ignore_ascii_case(b"static")
            } else {
                false
            };

            if is_relative {
                vm.deprecated(&format!(
                    "Callables of the form [\"{}\", \"{}\"] are deprecated",
                    target_name.to_string_lossy(),
                    method_name.to_string_lossy()
                ))?;
                vm.zend_is_callable(&v, true)?.is_ok()
            } else {
                vm.zend_is_callable(&v, false)?.is_ok()
            }
        } else {
            vm.zend_is_callable(&v, false)?.is_ok()
        }
    };
    if a.has(2) {
        let name = vm.callable_name(&v);
        a.set_ref(2, sv(name));
    }
    Ok(Value::Bool(r))
}

/// Marks a registered builtin `#[\Deprecated(since: ..., message: ...)]`.
fn deprecate(vm: &mut Vm, name: &str, since: &str, message: &str) {
    let lc = ZStr::from(name);
    if let Some(crate::func::Func::Builtin(b)) = vm.functions.get(&lc).cloned() {
        let mut nb = (*b).clone();
        nb.flags |= crate::func::flags::DEPRECATED;
        vm.functions
            .insert(lc.clone(), crate::func::Func::Builtin(std::rc::Rc::new(nb)));
        let suffix =
            crate::attrs::format_suffix(Some(&ZStr::from(since)), Some(&ZStr::from(message)));
        vm.builtin_deprecations.insert(lc, suffix);
    }
}

/// Registers the var functions.
pub(crate) fn register(vm: &mut Vm) {
    reg(
        vm,
        "var_dump(mixed $value, mixed ...$values): void",
        var_dump,
    );
    reg(
        vm,
        "debug_zval_dump(mixed $value, mixed ...$values): void",
        debug_zval_dump,
    );
    reg(
        vm,
        "print_r(mixed $value, bool $return = false): string|true",
        print_r,
    );
    reg(
        vm,
        "var_export(mixed $value, bool $return = false): ?string",
        var_export,
    );
    super::serial::register(vm);
    reg(vm, "gettype(mixed $value): string", gettype);
    reg(vm, "get_debug_type(mixed $value): string", get_debug_type);
    reg(vm, "settype(mixed &$var, string $type): bool", settype);
    reg(vm, "intval(mixed $value, int $base = 10): int", intval);
    reg(vm, "floatval(mixed $value): float", floatval);
    reg(vm, "doubleval(mixed $value): float", floatval);
    reg(vm, "strval(mixed $value): string", strval);
    reg(vm, "boolval(mixed $value): bool", boolval);
    reg(vm, "is_int(mixed $value): bool", is_int);
    reg(vm, "is_integer(mixed $value): bool", is_int);
    reg(vm, "is_long(mixed $value): bool", is_int);
    reg(vm, "is_float(mixed $value): bool", is_float);
    reg(vm, "is_double(mixed $value): bool", is_float);
    reg(vm, "is_string(mixed $value): bool", is_string);
    reg(vm, "is_bool(mixed $value): bool", is_bool);
    reg(vm, "is_array(mixed $value): bool", is_array);
    reg(vm, "is_object(mixed $value): bool", is_object);
    reg(vm, "is_null(mixed $value): bool", is_null);
    reg(vm, "is_scalar(mixed $value): bool", is_scalar);
    reg(vm, "is_resource(mixed $value): bool", is_resource);
    reg(vm, "is_numeric(mixed $value): bool", is_numeric);
    reg(vm, "is_iterable(mixed $value): bool", is_iterable);
    reg(vm, "is_countable(mixed $value): bool", is_countable);
    reg(
        vm,
        "is_callable(mixed $value, bool $syntax_only = false, &$callable_name = null): bool",
        is_callable,
    );
    deprecate(vm, "doubleval", "8.6", "use floatval() instead");
    deprecate(vm, "is_integer", "8.6", "use is_int() instead");
    deprecate(vm, "is_long", "8.6", "use is_int() instead");
    deprecate(vm, "is_double", "8.6", "use is_float() instead");
}

/// Raises an exception a native debug-info handler left pending, once the
/// dump is written: one parked with `php_vm::ext::defer_handler_throw`
/// (dom) or left in the VM's pending throw (`xmlreader_get_debug_info`).
/// Both are taken, so neither leaks into a later dump.
fn raise_after_dump(vm: &mut Vm) -> VmResult<()> {
    let deferred = dump::take_deferred_throw();
    vm.take_pending_throw()?;
    deferred
}
