// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The VM side of the array functions.
//!
//! The functions of `ext/standard/array.c` are implemented by `php-stdarr`
//! and bound by the `php-ext-stdarr` extension crate. This module only
//! provides what that binding needs from VM internals: the
//! [`php_stdarr::ArrayContext`] object hooks, the request RNG accessor
//! shared with `mt_rand()`, the callable diagnostics used for
//! "must be a valid callback, ..." errors, and immediate destructor runs.

use crate::object;
use crate::props::PropLookup;
use crate::vm::Vm;
use php_runtime::{ArrayKey, Obj, PhpRef, RtResult, Value, ZArray, ZStr};
use php_stdarr as sa;

impl sa::ArrayContext for Vm {
    fn is_enum(&self, obj: &Obj) -> bool {
        object::class_of(obj).is_enum()
    }

    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        php_runtime::RuntimeContext::count_object(self, obj)
    }

    fn column_property(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        // `has_property(ZEND_PROPERTY_EXISTS) || has_property(ZEND_PROPERTY_ISSET)`.
        let has = self.column_prop_exists(obj, name)
            || self
                .has_property(obj, name, false)
                .map_err(|t| self.to_rt(t))?;
        if !has {
            return Ok(None);
        }
        self.read_property(obj, name, crate::opcode::FetchMode::Is)
            .map(|v| Some(v.deref()))
            .map_err(|t| self.to_rt(t))
    }

    fn prepare_object_walk(&mut self, obj: &Obj) -> RtResult<()> {
        self.lazy_object_for(obj, None)
            .map(|_| ())
            .map_err(|t| self.to_rt(t))
    }

    fn property_reference(
        &mut self,
        obj: &Obj,
        key: &ArrayKey,
    ) -> RtResult<Option<sa::PropertyRef>> {
        let target = self.lazy_object_for(obj, None).map_err(|t| self.to_rt(t))?;
        let obj = &target;
        let cls = object::class_of(obj).clone();
        if cls
            .hooks
            .as_ref()
            .is_some_and(|h| h.properties_for.is_some())
        {
            // Native property tables: a detached reference, written back.
            let table = php_runtime::RuntimeContext::object_properties(
                self,
                obj,
                php_runtime::PropertyPurpose::ArrayCast,
            )?;
            return Ok(table.and_then(|t| {
                t.get(key).map(|v| match v {
                    Value::Reference(r) => sa::PropertyRef::Bound(r.clone()),
                    other => sa::PropertyRef::Detached(PhpRef::new(other.clone())),
                })
            }));
        }
        let d = object::data(obj);
        // A declared slot (`ZVAL_MAKE_REF` on the property slot);
        // uninitialized typed properties are skipped.
        if let Some(pi) = cls
            .props
            .iter()
            .find(|p| !p.is_virtual() && crate::props::mangle(p) == *key)
        {
            if pi.is_readonly() {
                return Err(self.make_throw(
                    "Error",
                    &format!(
                        "Cannot acquire reference to readonly property {}::${}",
                        pi.class, pi.name
                    ),
                ))
                .map_err(|t| self.to_rt(t));
            }
            let r = {
                let mut slots = d.slots.borrow_mut();
                match slots.get_mut(pi.slot as usize) {
                    Some(Some(v)) => v.make_ref(),
                    _ => return Ok(None),
                }
            };
            self.add_slot_type_source(obj, pi.slot as usize, &r);
            return Ok(Some(sa::PropertyRef::Bound(r)));
        }
        // A dynamic property (numeric names are integer keys in the
        // `(array)` table but string keys in the object).
        let mut dynamic = d.dynamic.borrow_mut();
        let Some(t) = dynamic.as_mut() else {
            return Ok(None);
        };
        let lookup = match key {
            ArrayKey::Int(i) if t.get(key).is_none() => ArrayKey::Str(ZStr::from(i.to_string())),
            k => k.clone(),
        };
        Ok(t.get_mut(&lookup)
            .map(|v| sa::PropertyRef::Bound(v.make_ref())))
    }

    fn this_object(&mut self) -> Option<Value> {
        let n = self.frames.len();
        if n < 2 {
            return None;
        }
        self.frames[n - 2].this.clone().map(Value::Object)
    }
}

impl Vm {
    /// `has_property(ZEND_PROPERTY_EXISTS)` without magic: an accessible,
    /// initialized declared property or a dynamic property, even if `null`.
    fn column_prop_exists(&mut self, obj: &Obj, name: &ZStr) -> bool {
        let cls = object::class_of(obj).clone();
        let scope = self.scope();
        match self.lookup_prop(&cls, name, scope.as_ref()) {
            PropLookup::Found(pi) => {
                pi.hooks.is_none()
                    && object::data(obj)
                        .slots
                        .borrow()
                        .get(pi.slot as usize)
                        .is_some_and(Option::is_some)
            }
            PropLookup::Inaccessible(_) => false,
            PropLookup::NotFound => object::data(obj)
                .dynamic
                .borrow()
                .as_ref()
                .is_some_and(|t| t.get(&php_runtime::ArrayKey::Str(name.clone())).is_some()),
        }
    }

    pub(crate) fn random_globals(&mut self) -> &mut php_random::RandomGlobals {
        self.ext
            .random
            .get_or_insert_with(php_random::RandomGlobals::new)
    }

    /// Runs the destructors of objects released so far, now, instead of
    /// after the current opcode (`zval_ptr_dtor` destroying objects inside
    /// a native function, e.g. `array_splice()` dropping removed elements).
    pub fn run_released_destructors(&mut self) -> crate::vm::VmResult<()> {
        if object::has_pending_destructors() {
            self.run_pending_destructors()?;
        }
        Ok(())
    }

    /// Why `v` is not callable, as printed after "must be a valid
    /// callback, " (`zend_is_callable_ex` error text), e.g.
    /// `function "foo" not found or invalid function name`.
    pub fn callable_error_message(&mut self, v: &Value) -> String {
        self.callable_error(v)
    }
}

// ---------------------------------------------------------------------------
// Bare-VM array functions: `min`, `max`, `clamp`, `array_map`, `array_filter`
// ---------------------------------------------------------------------------

/// Lifts a php-stdarr result into the VM (`php-ext-stdarr`'s `rt`).
fn rt<T>(vm: &mut Vm, r: RtResult<T>) -> crate::vm::VmResult<T> {
    r.map_err(|e| vm.throw_runtime_error(e))
}

/// All arguments from position `from` on, dereferenced (the variadic part;
/// the VM already rejects named variadic arguments).
fn rest(a: &crate::func::Args, from: usize) -> Vec<Value> {
    a.values.iter().skip(from).map(Value::deref).collect()
}

/// Integer argument `n`, or `default` when it was not passed.
fn int_or(a: &crate::func::Args, n: usize, default: i64) -> i64 {
    if a.has(n) {
        a.int(n)
    } else {
        default
    }
}

/// A php-stdarr callback calling the PHP callable `cb`, resolving it once at
/// the first call and caching the call (the `zend_fcall_info_cache` that
/// `Z_PARAM_FUNC` fills), as `php-ext-stdarr`'s bridge does. The arguments
/// are moved into the call, so the callee sees PHP's refcounts.
struct VmCallback {
    /// The callable (released once a resolved call holds what it needs).
    cb: Value,
    /// The resolution.
    state: CallbackState,
}

enum CallbackState {
    /// Not called yet.
    Unresolved,
    /// The cached call.
    Prepared(crate::vm::PendingCall),
    /// Not resolvable ahead: each call goes through [`Vm::call_value`].
    Dynamic,
}

fn callback(cb: Value) -> VmCallback {
    VmCallback {
        cb,
        state: CallbackState::Unresolved,
    }
}

impl VmCallback {
    /// Resolves the callable at the first call.
    fn resolve(&mut self, vm: &mut Vm) -> RtResult<()> {
        self.state = CallbackState::Dynamic;
        if needs_resolution(&self.cb) {
            match vm.resolve_callable(&self.cb) {
                Ok(Ok(call)) => self.state = CallbackState::Prepared(call),
                // Not callable that way: the direct path reports it.
                Ok(Err(_)) => {}
                Err(t) => return Err(vm.runtime_error_from(t)),
            }
        } else {
            match vm.prepare_call_value(&self.cb) {
                Ok(Some(call)) => {
                    // The cached call holds the closure or object; the
                    // callable value itself is no longer needed, so the
                    // callee sees the same refcounts as before caching.
                    self.cb = Value::Null;
                    self.state = CallbackState::Prepared(call);
                }
                Ok(None) => {}
                Err(t) => {
                    // Retried (and reported) by `call_value` next time.
                    self.state = CallbackState::Unresolved;
                    return Err(vm.runtime_error_from(t));
                }
            }
        }
        Ok(())
    }
}

impl sa::ArrayCallback<Vm> for VmCallback {
    fn call(&mut self, vm: &mut Vm, mut args: Vec<Value>) -> RtResult<Value> {
        self.call_slice(vm, &mut args)
    }

    fn call_slice(&mut self, vm: &mut Vm, args: &mut [Value]) -> RtResult<Value> {
        if matches!(self.state, CallbackState::Unresolved) {
            self.resolve(vm)?;
        }
        let r = match &self.state {
            CallbackState::Prepared(call) => vm.call_prepared(call, args),
            _ => vm.call_value(&self.cb, args.iter_mut().map(std::mem::take).collect()),
        };
        r.map_err(|t| vm.runtime_error_from(t))
    }
}

/// Whether a callable names a method (`"C::m"`, `[class_or_object, m]`),
/// whose class part depends on the calling scope.
fn needs_resolution(cb: &Value) -> bool {
    match cb {
        Value::Str(s) => s.as_bytes().windows(2).any(|w| w == b"::"),
        Value::Array(_) => true,
        _ => false,
    }
}

/// `HT_MIN_SIZE`.
const HT_MIN_SIZE: u64 = 8;
/// `ZEND_MM_MAX_LARGE_SIZE`: larger blocks are page-aligned huge blocks.
const MAX_LARGE_SIZE: u64 = 2 * 1024 * 1024 - 4096;
/// `sizeof(zend_array)`.
const ARRAY_HEADER: u64 = 56;

/// `zend_hash_check_size`: the table size for `n` elements.
fn table_size(n: u64) -> u64 {
    n.max(HT_MIN_SIZE).next_power_of_two()
}

/// What the allocator really takes for a request of `bytes`.
fn rounded(bytes: u64) -> u64 {
    if bytes > MAX_LARGE_SIZE {
        bytes.div_ceil(4096) * 4096
    } else {
        bytes
    }
}

/// Bytes of the element storage of a packed array of `n` elements
/// (`HT_PACKED_SIZE_EX`).
fn packed_data(n: u64) -> u64 {
    rounded(table_size(n) * 16 + 8)
}

/// Reserves `bytes` against `memory_limit` (`php_memlimit::try_reserve`).
/// On `false` the handler returns without building the array; the VM then
/// raises "Allowed memory size of N bytes exhausted".
fn reserve(bytes: u64) -> bool {
    php_memlimit::try_reserve(usize::try_from(bytes).unwrap_or(usize::MAX)).is_ok()
}

/// [`reserve`] for a list of `n` arrays of `each` packed elements (plus the
/// outer packed list), as built by `array_map(null, ...)`.
fn reserve_nested(n: u64, each: u64) -> bool {
    let inner = ARRAY_HEADER + packed_data(each);
    reserve(packed_data(n).saturating_add(n.saturating_mul(inner)))
}

fn min(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    // Two arguments (`ZEND_FRAMELESS_ICALL_2`'s form) are compared in
    // place, without collecting them into a vector.
    let r = if a.values.len() == 2 {
        sa::basic::min2(vm, &a.at_ref(0), &a.at_ref(1))
    } else {
        sa::basic::min(vm, &rest(a, 0))
    };
    rt(vm, r)
}

fn max(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    // Two arguments (`ZEND_FRAMELESS_ICALL_2`'s form) are compared in
    // place, without collecting them into a vector.
    let r = if a.values.len() == 2 {
        sa::basic::max2(vm, &a.at_ref(0), &a.at_ref(1))
    } else {
        sa::basic::max(vm, &rest(a, 0))
    };
    rt(vm, r)
}

fn clamp(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let r = sa::basic::clamp(vm, &a.at(0), &a.at(1), &a.at(2));
    rt(vm, r)
}

fn array_map(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let cbv = a.at(0);
    let arrays = rest(a, 1);
    if cbv.is_null() && arrays.len() > 1 {
        let longest = arrays
            .iter()
            .map(|v| match v {
                Value::Array(x) => x.len() as u64,
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        if !reserve_nested(longest, arrays.len() as u64) {
            return Ok(Value::Null);
        }
    }
    let r = if cbv.is_null() {
        sa::callback::array_map::<Vm>(vm, None, &arrays)
    } else {
        let mut cb = callback(cbv);
        sa::callback::array_map(vm, Some(&mut cb), &arrays)
    };
    Ok(Value::Array(rt(vm, r)?))
}

fn array_filter(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let h = a.array(0);
    let cbv = a.at(1);
    let mode = int_or(a, 2, sa::ARRAY_FILTER_USE_VALUE);
    let r = if cbv.is_null() {
        sa::callback::array_filter::<Vm>(vm, &h, None, mode)
    } else {
        let mut cb = callback(cbv);
        sa::callback::array_filter(vm, &h, Some(&mut cb), mode)
    };
    Ok(Value::Array(rt(vm, r)?))
}

/// Registers `min()`, `max()`, `clamp()`, `array_map()` and `array_filter()`
/// for the bare [`Vm::new`], attributed to `standard` as `php-ext-stdarr`
/// attributes them. `max()` and `min()` get the frameless fusion
/// (`special::frameless_flags`) from their registration.
pub(crate) fn register_bare_array_fns(vm: &mut Vm) {
    crate::reflection::record_module(vm, "standard", true);
    let mut m = crate::ext::ModuleBuilder::new(vm, "standard");
    m.function_static("min(mixed $value, mixed ...$values): mixed", min);
    m.function_static("max(mixed $value, mixed ...$values): mixed", max);
    m.function_static("clamp(mixed $value, mixed $min, mixed $max): mixed", clamp);
    m.function_static(
        "array_map(?callable $callback, $array, ...$arrays): array",
        array_map,
    );
    m.function_static(
        "array_filter(array $array, ?callable $callback = null, int $mode = 0): array",
        array_filter,
    );
    crate::reflection::record_module(vm, "standard", false);
}

fn count(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let v = a.at(0);
    let mode = a.int(1);
    if mode == sa::COUNT_NORMAL {
        if let Value::Array(arr) = &v {
            if arr.is_packed() {
                return Ok(Value::Int(i64::try_from(arr.len()).unwrap_or(0)));
            }
        }
    }
    let f = a.func_name();
    sa::basic::count(vm, &f, &v, mode)
        .map(Value::Int)
        .map_err(|e| vm.rt_throw(e))
}

fn in_array(vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let n = a.at(0);
    let h = a.array(1);
    let strict = a.bool(2);
    if h.is_packed() {
        if strict {
            match &n {
                Value::Int(target) => {
                    for v in h.values() {
                        if let Value::Int(x) = v.deref() {
                            if x == *target {
                                return Ok(Value::Bool(true));
                            }
                        }
                    }
                    return Ok(Value::Bool(false));
                }
                Value::Str(target) => {
                    for v in h.values() {
                        if let Value::Str(x) = v.deref() {
                            if x.as_bytes() == target.as_bytes() {
                                return Ok(Value::Bool(true));
                            }
                        }
                    }
                    return Ok(Value::Bool(false));
                }
                Value::Bool(target) => {
                    for v in h.values() {
                        if let Value::Bool(x) = v.deref() {
                            if x == *target {
                                return Ok(Value::Bool(true));
                            }
                        }
                    }
                    return Ok(Value::Bool(false));
                }
                _ => {
                    for v in h.values() {
                        if php_runtime::ops::is_identical(&n, &v.deref())
                            .map_err(|e| vm.rt_throw(e))?
                        {
                            return Ok(Value::Bool(true));
                        }
                    }
                    return Ok(Value::Bool(false));
                }
            }
        } else {
            for v in h.values() {
                if php_runtime::ops::is_equal(&n, v, vm).map_err(|e| vm.rt_throw(e))? {
                    return Ok(Value::Bool(true));
                }
            }
            return Ok(Value::Bool(false));
        }
    }
    sa::basic::in_array(vm, &n, &h, strict)
        .map(Value::Bool)
        .map_err(|e| vm.rt_throw(e))
}

fn array_slice(_vm: &mut Vm, a: &mut crate::func::Args) -> crate::vm::VmResult<Value> {
    let arr = a.array(0);
    let offset = a.int(1);
    let length = a.opt(2).map(|_| a.int(2));
    let preserve_keys = a.bool(3);

    if arr.is_packed() {
        let num_in = i64::try_from(arr.len()).unwrap_or(i64::MAX);
        let mut len = length.unwrap_or(num_in);
        let mut off = offset;
        if off > num_in {
            return Ok(Value::Array(ZArray::new()));
        } else if off < 0 {
            off += num_in;
            if off < 0 {
                off = 0;
            }
        }
        if len < 0 {
            len += num_in - off;
        } else if off.unsigned_abs().saturating_add(len.unsigned_abs()) > num_in.unsigned_abs() {
            len = num_in - off;
        }
        if len <= 0 {
            return Ok(Value::Array(ZArray::new()));
        }
        let skip = usize::try_from(off).unwrap_or(0);
        let take = usize::try_from(len).unwrap_or(usize::MAX);

        if !preserve_keys || (off == 0 && arr.is_without_holes()) {
            let vals = arr
                .values()
                .skip(skip)
                .take(take)
                .map(php_stdarr::util::copy_elem);
            return Ok(Value::Array(ZArray::from_values(vals)));
        }
    }

    Ok(Value::Array(sa::basic::array_slice(
        &arr,
        offset,
        length,
        preserve_keys,
    )))
}

/// Registers `count()`, `sizeof()` and the `COUNT_*` constants for the
/// bare [`Vm::new`], attributed to `standard` as `php-ext-stdarr`
/// attributes them.
pub(crate) fn register_bare_count(vm: &mut Vm) {
    crate::reflection::record_module(vm, "standard", true);
    let mut m = crate::ext::ModuleBuilder::new(vm, "standard");
    m.function_static(
        "count(Countable|array $value, int $mode = COUNT_NORMAL): int",
        count,
    );
    m.function_static(
        "sizeof(Countable|array $value, int $mode = COUNT_NORMAL): int",
        count,
    );
    m.function_static(
        "in_array(mixed $needle, array $haystack, bool $strict = false): bool",
        in_array,
    );
    m.function_static(
        "array_slice(array $array, int $offset, ?int $length = null, bool $preserve_keys = false): array",
        array_slice,
    );
    m.constant("COUNT_NORMAL", Value::Int(sa::COUNT_NORMAL));
    m.constant("COUNT_RECURSIVE", Value::Int(sa::COUNT_RECURSIVE));
    crate::reflection::record_module(vm, "standard", false);
}

#[cfg(test)]
mod tests {
    use crate::{Vm, VmOptions};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn bare_vm_has_count_and_with_extensions_starts_without_it() {
        let sink = Sink::default();
        let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        let status = vm.run_script(
            br#"<?php
class C implements Countable { function count(): int { return 7; } }
echo count([1, 2, 3]), " ", sizeof([1, [2, 3]], COUNT_RECURSIVE), " ",
    count(new C), " ", COUNT_NORMAL, " ", COUNT_RECURSIVE, " ",
    (new ReflectionFunction('count'))->getExtensionName(), " ",
    (new ReflectionConstant('COUNT_NORMAL'))->getExtensionName(), "\n";
try { count(42); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
"#,
            "t.php",
        );
        assert_eq!(status, 0);
        let out = String::from_utf8(sink.0.borrow().clone()).expect("utf8");
        assert_eq!(
            out,
            "3 4 7 0 1 standard standard\ncount(): Argument #1 ($value) must be of type Countable|array, int given\n"
        );

        let sink2 = Sink::default();
        let mut ext_vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink2.clone()), &[]);
        let status2 = ext_vm.run_script(
            br"<?php
var_dump(function_exists('count'), function_exists('sizeof'), defined('COUNT_NORMAL'), defined('COUNT_RECURSIVE'));
",
            "t2.php",
        );
        assert_eq!(status2, 0);
        let out2 = String::from_utf8(sink2.0.borrow().clone()).expect("utf8");
        assert_eq!(out2, "bool(false)\nbool(false)\nbool(false)\nbool(false)\n");
    }
}
