// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`JsonHost`]: what the JSON extension needs from the VM.
//!
//! Everything object-related goes through this trait: `JsonSerializable`
//! calls, enum cases, property tables, `stdClass` creation for decoding,
//! recursion guards and the `JsonException` class. It extends
//! [`RuntimeContext`], whose `serialize_precision()` is used for floats and
//! whose `object_properties(obj, PropertyPurpose::Json)` is the default
//! property source.

use php_runtime::{
    Obj, PropertyPurpose, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use std::cell::RefCell;
use std::collections::HashSet;

/// Default nesting limit used by [`JsonHost::json_stack_limit_reached`]: the
/// encoder recurses once per nested array/object, so very deep values are
/// rejected with `JSON_ERROR_DEPTH` (like php-src's `zend.max_allowed_stack_size`
/// check) instead of overflowing the native stack.
pub const DEFAULT_MAX_NESTING: usize = 3000;

/// A recursion-guard slot (`GC_PROTECT_RECURSION` / `ZEND_GUARD_PROTECT_RECURSION`).
///
/// The `usize` is an identity address ([`ZArray::addr`] or [`Obj::addr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardKey {
    /// An array or native property table being encoded (protects its hash table).
    Array(usize),
    /// An object whose properties are being encoded.
    Object(usize),
    /// An object whose `jsonSerialize()` is running (the per-object `JSON`
    /// guard).
    JsonSerialize(usize),
}

impl GuardKey {
    /// The guard for an array value.
    #[must_use]
    pub fn array(a: &ZArray) -> Self {
        GuardKey::Array(a.addr())
    }

    /// The property guard for an object.
    #[must_use]
    pub fn object(o: &Obj) -> Self {
        GuardKey::Object(o.addr())
    }

    /// The `jsonSerialize()` guard for an object.
    #[must_use]
    pub fn json_serialize(o: &Obj) -> Self {
        GuardKey::JsonSerialize(o.addr())
    }
}

thread_local! {
    static GUARDS: RefCell<HashSet<GuardKey>> = RefCell::new(HashSet::new());
}

/// Default [`JsonHost::json_protect`]: a thread-local set, so that nested
/// `json_encode()` calls (from `jsonSerialize()`) see the outer call's
/// guards, like php-src's flags on the objects themselves.
#[must_use]
pub fn default_protect(key: GuardKey) -> bool {
    GUARDS.with(|g| g.borrow_mut().insert(key))
}

/// Default [`JsonHost::json_unprotect`].
pub fn default_unprotect(key: GuardKey) {
    GUARDS.with(|g| {
        g.borrow_mut().remove(&key);
    });
}

/// The services the JSON extension needs from the VM.
///
/// All methods have defaults suitable for a VM without objects; a real VM
/// overrides the object hooks. Method contracts:
///
/// * Encoding an object first asks [`is_json_serializable`](Self::is_json_serializable)
///   (`instanceof JsonSerializable`), then [`is_enum`](Self::is_enum), and
///   otherwise encodes [`json_properties`](Self::json_properties).
/// * Decoding to objects calls [`new_std_class`](Self::new_std_class) when
///   php-src creates the object (at the first member, or at `}` for `{}`),
///   then [`std_class_write`](Self::std_class_write) for each member, so
///   object handle numbers match.
pub trait JsonHost: RuntimeContext {
    /// Reserves space for an output-buffer append. A VM may check its
    /// `memory_limit` before the allocation.
    fn json_reserve(&mut self, buf: &mut Vec<u8>, additional: usize) -> RtResult<()> {
        buf.reserve(additional);
        Ok(())
    }

    /// Whether the object's class implements `JsonSerializable`.
    fn is_json_serializable(&mut self, obj: &Obj) -> bool {
        let _ = obj;
        false
    }

    /// Calls `$obj->jsonSerialize()` and returns its result. Only called when
    /// [`is_json_serializable`](Self::is_json_serializable) returned true; an
    /// `Err` is an exception thrown by the method and aborts the encoding.
    fn json_serialize(&mut self, obj: &Obj) -> RtResult<Value> {
        Err(RuntimeError::Error(format!(
            "Cannot call {}::jsonSerialize() without a VM",
            obj.class_name()
        )))
    }

    /// Whether the object is an enum case.
    fn is_enum(&mut self, obj: &Obj) -> bool {
        let _ = obj;
        false
    }

    /// The backing value of a backed enum case (`None` for pure enums).
    fn enum_backing_value(&mut self, obj: &Obj) -> Option<Value> {
        let _ = obj;
        None
    }

    /// The property table to encode (`zend_get_properties_for(obj,
    /// ZEND_PROP_PURPOSE_JSON)`), in property-table order with mangled keys
    /// (`"\0Class\0name"` for private, `"\0*\0name"` for protected): the
    /// encoder skips mangled keys, so only public properties (declared, in
    /// declaration order, then dynamic) are output. Uninitialized typed
    /// properties must be absent; hooked properties must hold the value of
    /// their `get` hook (virtual properties without `get` are absent).
    /// `Ok(None)` encodes as `{}`.
    fn json_properties(&mut self, obj: &Obj) -> RtResult<Option<ZArray>> {
        self.object_properties(obj, PropertyPurpose::Json)
    }

    /// A native property handler's table and recursion identity. Unhooked
    /// handlers protect the table, which may be rebuilt on each visit;
    /// hooked properties protect the object instead. `None` falls back to
    /// ordinary property snapshots from [`json_properties`](Self::json_properties),
    /// protected by the object identity.
    fn json_native_properties(&mut self, obj: &Obj) -> RtResult<Option<(ZArray, GuardKey)>> {
        let _ = obj;
        Ok(None)
    }

    /// Creates an empty `stdClass` object (`object_init`) for `json_decode`.
    fn new_std_class(&mut self) -> RtResult<Value> {
        self.create_std_object(ZArray::new())
    }

    /// Writes a property of a `stdClass` created by
    /// [`new_std_class`](Self::new_std_class) (`zend_std_write_property`).
    /// `name` is used verbatim (never numeric-normalized); it never starts
    /// with a NUL byte.
    fn std_class_write(&mut self, obj: &Value, name: ZStr, value: Value) -> RtResult<()> {
        let _ = (obj, name, value);
        Err(RuntimeError::Error(
            "Cannot write object properties without a VM".to_string(),
        ))
    }

    /// Marks `key` as being encoded. Returns `false` if it already was
    /// (recursion). The default uses a thread-local set; a VM may map these
    /// onto the same flags `var_dump()` and friends use.
    fn json_protect(&mut self, key: GuardKey) -> bool {
        default_protect(key)
    }

    /// Clears a mark set by [`json_protect`](Self::json_protect).
    fn json_unprotect(&mut self, key: GuardKey) {
        default_unprotect(key);
    }

    /// The stack-limit check of `php_json_encode_array`
    /// (`zend_call_stack_overflowed`): `nesting` is the number of arrays and
    /// objects currently open in this encoder. Returning `true` makes the
    /// encoder fail with `JSON_ERROR_DEPTH`.
    fn json_stack_limit_reached(&mut self, nesting: usize) -> bool {
        nesting >= DEFAULT_MAX_NESTING
    }

    /// Creates the `JsonException` thrown with `JSON_THROW_ON_ERROR`
    /// (`zend_throw_exception(php_json_exception_ce, message, code)`).
    fn json_exception(&mut self, message: &str, code: i64) -> RuntimeError {
        let _ = code;
        RuntimeError::Error(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_vm_host_keeps_default_stack_limit() {
        let mut host = php_runtime::CollectingContext::new();
        assert!(!host.json_stack_limit_reached(DEFAULT_MAX_NESTING - 1));
        assert!(host.json_stack_limit_reached(DEFAULT_MAX_NESTING));
    }

    #[test]
    fn default_guards() {
        let k = GuardKey::Array(12345);
        assert!(default_protect(k));
        assert!(!default_protect(k));
        default_unprotect(k);
        assert!(default_protect(k));
        default_unprotect(k);
        assert_ne!(GuardKey::Object(1), GuardKey::JsonSerialize(1));
    }
}
