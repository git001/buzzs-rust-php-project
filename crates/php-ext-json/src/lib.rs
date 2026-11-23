// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/json` for the phprust VM: binds the `php-json` library through the
//! `php_vm::ext` extension API.
//!
//! * `json_encode`, `json_decode`, `json_validate`, `json_last_error`,
//!   `json_last_error_msg` with php-src's exact `ValueError`s and
//!   `JSON_THROW_ON_ERROR` handling (all in [`php_json::functions`]).
//! * The `JSON_*` constants.
//! * `JsonSerializable` / `JsonException` (registered here only when the VM
//!   does not already provide them).
//!
//! The per-request last-error state (`JSON_G(error_details)`) lives in a
//! thread-local [`JsonState`], reset at request shutdown.

mod host;

use host::VmJsonHost;
use php_json::{functions, JsonState};
use php_runtime::{ArrayKey, Value, ZStr};
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, Extension, ModuleBuilder, VmResult};
use php_vm::Vm;
use std::cell::RefCell;

thread_local! {
    /// `JSON_G(error_details)` of the running request.
    static STATE: RefCell<JsonState> = RefCell::new(JsonState::new());
}

/// Runs `f` with the request's [`JsonState`] taken out of the thread-local
/// slot, so that nested `json_*` calls made from `jsonSerialize()` see (and
/// update) a consistent state.
fn with_state<R>(f: impl FnOnce(&mut JsonState) -> R) -> R {
    let mut st = STATE.with(|s| *s.borrow());
    let r = f(&mut st);
    STATE.with(|s| *s.borrow_mut() = st);
    r
}

/// The `json` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json;

/// Validates string UTF-8 using `s.is_valid_utf8()` to fast-path string validation
/// without re-checking bytes with `std::str::from_utf8`. If bytes are valid UTF-8,
/// marks with `s.mark_valid_utf8()`.
fn check_and_mark_utf8(s: &ZStr) -> bool {
    if s.is_valid_utf8() {
        return true;
    }
    if std::str::from_utf8(s.as_bytes()).is_ok() {
        s.mark_valid_utf8();
        true
    } else {
        false
    }
}

/// How deep [`check_value_utf8`] looks. The pass only pre-marks strings as
/// valid UTF-8 for the encoder; anything deeper is checked by the encoder
/// itself, so stopping early changes nothing but speed.
const UTF8_PREPASS_MAX_DEPTH: usize = 64;

/// Recursively fast-paths and marks valid UTF-8 on string values and keys before
/// encoding. `open` holds the arrays being walked: an array reached again
/// through a reference (`$a[] = &$a`) is a cycle the encoder reports as
/// "Recursion detected", so it is not walked twice.
fn check_value_utf8(v: &Value, open: &mut Vec<usize>) {
    match v {
        Value::Str(s) => {
            check_and_mark_utf8(s);
        }
        Value::Array(arr) => {
            let addr = arr.addr();
            if open.len() >= UTF8_PREPASS_MAX_DEPTH || open.contains(&addr) {
                return;
            }
            open.push(addr);
            for (key, entry) in arr.iter() {
                if let ArrayKey::Str(k) = key {
                    check_and_mark_utf8(k);
                }
                check_value_utf8(entry, open);
            }
            open.pop();
        }
        Value::Reference(r) => {
            check_value_utf8(&r.get(), open);
        }
        _ => {}
    }
}

/// Marks decoded string values and keys as valid UTF-8 since valid JSON is
/// guaranteed to be UTF-8.
fn mark_decoded_utf8(vm: &mut Vm, v: &Value) {
    match v {
        Value::Str(s) => {
            s.mark_valid_utf8();
        }
        Value::Array(arr) => {
            for (key, entry) in arr.iter() {
                if let ArrayKey::Str(k) = key {
                    k.mark_valid_utf8();
                }
                mark_decoded_utf8(vm, entry);
            }
        }
        Value::Object(o) => {
            if let Ok(Some(props)) = vm.standard_json_properties(o) {
                for (key, entry) in props.iter() {
                    if let ArrayKey::Str(k) = key {
                        k.mark_valid_utf8();
                    }
                    mark_decoded_utf8(vm, entry);
                }
            }
        }
        Value::Reference(r) => {
            mark_decoded_utf8(vm, &r.get());
        }
        _ => {}
    }
}

fn json_encode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let value = a.at(0);
    check_value_utf8(&value, &mut Vec::new());
    let flags = if a.has(1) { a.int(1) } else { 0 };
    let depth = if a.has(2) {
        a.int(2)
    } else {
        php_json::flags::DEFAULT_DEPTH
    };
    let mut st = STATE.with(|s| *s.borrow());
    let r = {
        let mut h = VmJsonHost::new(vm);
        functions::json_encode(&mut st, &mut h, &value, flags, depth)
    };
    STATE.with(|s| *s.borrow_mut() = st);
    let r = r.map_err(|e| host::rt_throw(vm, e))?;
    if let Value::Str(out) = &r {
        out.mark_valid_utf8();
    }
    Ok(r)
}

fn json_decode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let json = a.str(0);
    let assoc = a.opt(1).map(|v| matches!(v, Value::Bool(true)));
    let depth = if a.has(2) {
        a.int(2)
    } else {
        php_json::flags::DEFAULT_DEPTH
    };
    let flags = if a.has(3) { a.int(3) } else { 0 };
    let mut st = STATE.with(|s| *s.borrow());
    let r = {
        let mut h = VmJsonHost::new(vm);
        functions::json_decode(&mut st, &mut h, json.as_bytes(), assoc, depth, flags)
    };
    STATE.with(|s| *s.borrow_mut() = st);
    if php_memlimit::is_pending() {
        // `RETURN_OOM()`: the VM raises the memory-limit fatal after the
        // call, even with `JSON_THROW_ON_ERROR`.
        return Ok(Value::Null);
    }
    let r = r.map_err(|e| host::rt_throw(vm, e))?;
    mark_decoded_utf8(vm, &r);
    Ok(r)
}

fn json_validate(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let json = a.str(0);
    let depth = if a.has(1) {
        a.int(1)
    } else {
        php_json::flags::DEFAULT_DEPTH
    };
    let flags = if a.has(2) { a.int(2) } else { 0 };
    with_state(|st| functions::json_validate(st, json.as_bytes(), depth, flags))
        .map(Value::Bool)
        .map_err(|e| host::rt_throw(vm, e))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn json_last_error(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(with_state(|st| functions::json_last_error(st))))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn json_last_error_msg(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::from(with_state(|st| {
        functions::json_last_error_msg(st)
    })))
}

impl Extension for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for (name, value) in php_json::flags::CONSTANTS {
            m.constant(name, Value::Int(*value));
        }
        m.function_static(
            "json_encode(mixed $value, int $flags = 0, int $depth = 512): string|false",
            json_encode,
        )
        .function_static(
            "json_decode(string $json, ?bool $associative = null, int $depth = 512, int $flags = 0): mixed",
            json_decode,
        )
        .function_static(
            "json_validate(string $json, int $depth = 512, int $flags = 0): bool",
            json_validate,
        )
        .function_static("json_last_error(): int", json_last_error)
        .function_static("json_last_error_msg(): string", json_last_error_msg);

        let vm = m.vm();
        if vm.find_class(b"jsonserializable").is_none() {
            ClassBuilder::new(php_json::JSON_SERIALIZABLE_INTERFACE)
                .flags(class_flags::INTERFACE)
                .abstract_method("jsonSerialize(): mixed", acc::PUBLIC)
                .register(vm);
        }
        if vm.find_class(b"jsonexception").is_none() {
            ClassBuilder::new(php_json::JSON_EXCEPTION_CLASS)
                .extends(vm, "Exception")
                .register(vm);
        }
        m.on_request_shutdown(|_| STATE.with(|s| s.borrow_mut().clear()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `$a = []; $a[] = &$a;`: the UTF-8 pre-pass stops at the cycle
    /// instead of recursing until the stack overflows (json/tests/003).
    #[test]
    fn utf8_prepass_stops_at_reference_cycles() {
        let r = php_runtime::PhpRef::new(Value::empty_array());
        let mut arr = php_runtime::ZArray::new();
        assert!(arr.make_mut().push(Value::Reference(r.clone())).is_ok());
        r.set(Value::Array(arr.clone()));
        let mut open = Vec::new();
        check_value_utf8(&Value::Array(arr), &mut open);
        assert!(open.is_empty());
        // Break the cycle so the test does not leak it.
        r.set(Value::Null);
    }

    #[test]
    fn state_round_trip() {
        with_state(|st| st.last_error = php_json::JsonError::new(php_json::ErrorCode::Syntax));
        assert_eq!(with_state(|st| functions::json_last_error(st)), 4);
        with_state(JsonState::clear);
        assert_eq!(with_state(|st| functions::json_last_error(st)), 0);
    }

    #[test]
    fn constants_complete() {
        let names: Vec<&str> = php_json::flags::CONSTANTS.iter().map(|c| c.0).collect();
        assert!(names.contains(&"JSON_THROW_ON_ERROR"));
        assert!(names.contains(&"JSON_ERROR_NON_BACKED_ENUM"));
    }

    #[test]
    fn utf8_caching_encode_and_decode() {
        let s = ZStr::from("hello world");
        assert!(!s.is_valid_utf8());
        assert!(check_and_mark_utf8(&s));
        assert!(s.is_valid_utf8());

        let invalid = ZStr::new(b"\xff\xfe".to_vec());
        assert!(!check_and_mark_utf8(&invalid));
        assert!(!invalid.is_valid_utf8());

        let out = ZStr::from(r#"{"key":"val"}"#);
        out.mark_valid_utf8();
        assert!(out.is_valid_utf8());
    }
}
