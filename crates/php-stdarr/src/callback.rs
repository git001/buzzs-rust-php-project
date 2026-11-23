// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Callback-driven functions: `array_walk`, `array_walk_recursive`,
//! `array_map`, `array_filter`, `array_reduce`, `array_find`,
//! `array_find_key`, `array_any`, `array_all`.

use crate::util::{arg_value_error, copy_elem, deprecated, deref, not_array_error, Guard};
use crate::{ArrayContext, Callback, PropertyRef};
use crate::{ARRAY_FILTER_USE_BOTH, ARRAY_FILTER_USE_KEY, ARRAY_FILTER_USE_VALUE};
use php_runtime::{
    ops, ArrayKey, HashTable, IterId, Obj, PhpRef, PropertyPurpose, RtResult, RuntimeContext,
    RuntimeError, Value, ZArray,
};

// ---------------------------------------------------------------------------
// array_walk
// ---------------------------------------------------------------------------

/// One step of the walk: the key and the (now referenced) element at the
/// iterator's position, advancing the iterator. `Ok(None)` at the end.
fn walk_fetch(target: &PhpRef, iter: IterId) -> RtResult<Option<(ArrayKey, PhpRef)>> {
    let mut slot = target.borrow_mut();
    let Value::Array(a) = &mut *slot else {
        return Err(RuntimeError::TypeError(
            "Iterated value is no longer an array or object".to_string(),
        ));
    };
    let ht = a.make_mut();
    let pos = ht.iterator_pos(iter);
    let p = ht.valid_pos(pos);
    let Some((key, _)) = ht.at(p) else {
        return Ok(None);
    };
    let key = key.clone();
    let Some(elem) = ht.at_mut(p) else {
        return Ok(None);
    };
    let r = elem.make_ref();
    let next = ht.pos_forward(p);
    ht.iterator_set_pos(iter, next);
    Ok(Some((key, r)))
}

/// Processes one walked element: recurses into arrays (`array_walk_recursive`)
/// or calls the callback with `(&$value, $key[, $arg])`.
fn walk_element<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    key: &ArrayKey,
    elem: PhpRef,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
    recursive: bool,
    guard: &mut Guard,
) -> RtResult<()> {
    let is_array = matches!(&*elem.borrow(), Value::Array(_));
    if recursive && is_array {
        let addr = {
            let mut b = elem.borrow_mut();
            match &mut *b {
                Value::Array(a) => {
                    a.make_mut();
                    a.addr()
                }
                _ => 0,
            }
        };
        if guard.contains(addr) {
            return Err(RuntimeError::Error("Recursion detected".to_string()));
        }
        guard.push(addr);
        let r = walk_inner(ctx, &elem, callback, userdata, recursive, guard);
        guard.pop(addr);
        return r;
    }
    // The reference is moved into the arguments: like PHP, the element slot
    // and the callee's parameter are its only holders.
    let elem = Value::Reference(elem);
    match userdata {
        Some(u) => callback.call_slice(ctx, &mut [elem, key.to_value(), u.clone()]),
        None => callback.call_slice(ctx, &mut [elem, key.to_value()]),
    }
    .map(drop)
}

fn walk_inner<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    target: &PhpRef,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
    recursive: bool,
    guard: &mut Guard,
) -> RtResult<()> {
    let iter = {
        let mut slot = target.borrow_mut();
        let Value::Array(a) = &mut *slot else {
            return Ok(());
        };
        if a.is_empty() {
            return Ok(());
        }
        let ht = a.make_mut();
        let pos = ht.valid_pos(0);
        ht.iterator_add(pos)
    };
    let mut result = Ok(());
    loop {
        let (key, elem) = match walk_fetch(target, iter) {
            Ok(Some(step)) => step,
            Ok(None) => break,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        if let Err(e) = walk_element(ctx, &key, elem, callback, userdata, recursive, guard) {
            result = Err(e);
            break;
        }
    }
    if let Some(mut slot) = target.try_borrow_mut() {
        if let Value::Array(a) = &mut *slot {
            a.make_mut().iterator_del(iter);
        }
    }
    result
}

/// Walks the live property table of an object (`Z_OBJPROP`): each step
/// re-reads the table (the callback may add or remove properties) and
/// continues after the previously visited key; every property slot is
/// bound to a reference through [`ArrayContext::property_reference`].
///
/// [`ArrayContext::prepare_object_walk`] prepares the object once before
/// the walk reads its live `ArrayCast` table.
fn walk_object<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    obj: &Obj,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
    recursive: bool,
    guard: &mut Guard,
) -> RtResult<()> {
    ctx.prepare_object_walk(obj)?;
    let mut last: Option<(ArrayKey, u32)> = None;
    loop {
        let table = ctx
            .object_properties(obj, PropertyPurpose::ArrayCast)?
            .unwrap_or_default();
        let pos = match &last {
            None => table.valid_pos(0),
            Some((k, p)) => match table.position_of(k) {
                Some(q) => table.valid_pos(q + 1),
                None => table.valid_pos(p + 1),
            },
        };
        let Some((key, _)) = table.at(pos) else {
            return Ok(());
        };
        let key = key.clone();
        drop(table);
        last = Some((key.clone(), pos));
        match ctx.property_reference(obj, &key)? {
            None => {}
            Some(PropertyRef::Bound(r)) => {
                walk_element(ctx, &key, r, callback, userdata, recursive, guard)?;
            }
            Some(PropertyRef::Detached(r)) => {
                let before = r.get();
                let result =
                    walk_element(ctx, &key, r.clone(), callback, userdata, recursive, guard);
                let after = r.get();
                let write = match ops::is_identical(&before, &after) {
                    Ok(false) => ctx.property_write(obj, &key.to_zstr(), after),
                    Ok(true) => Ok(()),
                    Err(e) => Err(e),
                };
                result?;
                write?;
            }
        }
    }
}

fn php_walk<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    func: &str,
    array: &PhpRef,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
    recursive: bool,
) -> RtResult<()> {
    let mut guard = Guard::default();
    match array.get() {
        Value::Array(_) => walk_inner(ctx, array, callback, userdata, recursive, &mut guard),
        Value::Object(o) => {
            deprecated(
                ctx,
                func,
                &format!(
                    "Passing an object for argument #1 $array to {func}() is deprecated, call get_object_vars() first instead"
                ),
            )?;
            walk_object(ctx, &o, callback, userdata, recursive, &mut guard)
        }
        other => Err(not_array_error(func, 1, Some("array"), &other)),
    }
}

/// `array_walk(array|object &$array, callable $callback, mixed $arg = UNKNOWN): true`.
///
/// The callback receives `(&$value, $key[, $arg])`; `$value` is passed as a
/// [`Value::Reference`] to the element, so a by-reference parameter
/// modifies the array.
pub fn array_walk<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    array: &PhpRef,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
) -> RtResult<()> {
    php_walk(ctx, "array_walk", array, callback, userdata, false)
}

/// `array_walk_recursive(array|object &$array, callable $callback, mixed $arg = UNKNOWN): true`.
pub fn array_walk_recursive<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    array: &PhpRef,
    callback: &mut Callback<'_, C>,
    userdata: Option<&Value>,
) -> RtResult<()> {
    php_walk(ctx, "array_walk_recursive", array, callback, userdata, true)
}

// ---------------------------------------------------------------------------
// array_map
// ---------------------------------------------------------------------------

/// `array_map(?callable $callback, array $array, array ...$arrays): array`.
///
/// `arrays` are the array arguments (argument #2 onwards).
pub fn array_map<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    callback: Option<&mut Callback<'_, C>>,
    arrays: &[Value],
) -> RtResult<ZArray> {
    const F: &str = "array_map";
    if let [single] = arrays {
        let Value::Array(input) = single else {
            return Err(not_array_error(F, 2, Some("array"), single));
        };
        let Some(cb) = callback else {
            return Ok(input.clone());
        };
        if input.is_empty() {
            return Ok(input.clone());
        }
        let mut out = HashTable::with_capacity(input.len());
        if input.is_packed() {
            for pos in 0..input.used() {
                if let Some((_, v)) = input.at(pos) {
                    let r = cb.call_slice(ctx, &mut [v.clone()])?;
                    out.insert(i64::from(pos), r);
                }
            }
        } else {
            for (k, v) in input.iter() {
                let r = cb.call_slice(ctx, &mut [v.clone()])?;
                let _ = out.add(k.clone(), r);
            }
        }
        return Ok(ZArray::from_table(out));
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: one entry per array argument"
    )]
    let mut tables = Vec::with_capacity(arrays.len());
    let mut maxlen = 0usize;
    for (i, a) in arrays.iter().enumerate() {
        let Value::Array(t) = a else {
            return Err(not_array_error(
                F,
                i + 2,
                if i == 0 { Some("array") } else { None },
                a,
            ));
        };
        maxlen = maxlen.max(t.len());
        tables.push(t);
    }
    if maxlen == 0 {
        return Ok(ZArray::new());
    }
    let mut iters: Vec<_> = tables.iter().map(|t| t.values()).collect();
    let mut out = HashTable::with_capacity(maxlen);
    let mut cb = callback;
    // One argument buffer for all the calls; the callback moves the
    // arguments out of it.
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: one entry per array argument"
    )]
    let mut params: Vec<Value> = Vec::with_capacity(iters.len());
    for _ in 0..maxlen {
        params.clear();
        params.extend(
            iters
                .iter_mut()
                .map(|it| it.next().cloned().unwrap_or(Value::Null)),
        );
        let v = match cb.as_deref_mut() {
            None => Value::Array(ZArray::from_values(params.drain(..))),
            Some(cb) => cb.call_slice(ctx, &mut params)?,
        };
        let _ = out.push(v);
    }
    Ok(ZArray::from_table(out))
}

// ---------------------------------------------------------------------------
// array_filter / array_reduce
// ---------------------------------------------------------------------------

/// `array_filter(array $array, ?callable $callback = null, int $mode = 0): array`.
pub fn array_filter<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: Option<&mut Callback<'_, C>>,
    mode: i64,
) -> RtResult<ZArray> {
    if ![
        ARRAY_FILTER_USE_VALUE,
        ARRAY_FILTER_USE_BOTH,
        ARRAY_FILTER_USE_KEY,
    ]
    .contains(&mode)
    {
        return Err(arg_value_error(
            "array_filter",
            3,
            Some("mode"),
            "must be one of ARRAY_FILTER_USE_VALUE, ARRAY_FILTER_USE_KEY, or ARRAY_FILTER_USE_BOTH",
        ));
    }
    if array.is_empty() {
        return Ok(ZArray::new());
    }
    let mut out = HashTable::new();
    let mut cb = callback;
    for (k, v) in array {
        let keep = match cb.as_deref_mut() {
            Some(cb) => {
                let r = match mode {
                    ARRAY_FILTER_USE_KEY => cb.call_slice(ctx, &mut [k.to_value()])?,
                    ARRAY_FILTER_USE_BOTH => cb.call_slice(ctx, &mut [v.clone(), k.to_value()])?,
                    _ => cb.call_slice(ctx, &mut [v.clone()])?,
                };
                r.to_bool(ctx)?
            }
            None => v.to_bool(ctx)?,
        };
        if keep {
            let _ = out.add(k.clone(), copy_elem(v));
        }
    }
    Ok(ZArray::from_table(out))
}

/// `array_reduce(array $array, callable $callback, mixed $initial = null): mixed`.
pub fn array_reduce<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
    initial: Option<&Value>,
) -> RtResult<Value> {
    let mut acc = initial.cloned().unwrap_or(Value::Null);
    for v in array.values() {
        let r = callback.call_slice(ctx, &mut [acc, v.clone()])?;
        acc = deref(&r);
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------
// array_find family
// ---------------------------------------------------------------------------

/// `php_array_find`: the first `(key, value)` for which the callback's
/// result (converted to bool, xor `negate`) is true.
pub fn php_array_find<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
    negate: bool,
) -> RtResult<Option<(ArrayKey, Value)>> {
    for (k, v) in array {
        let r = callback.call_slice(ctx, &mut [v.clone(), k.to_value()])?;
        if r.to_bool(ctx)? ^ negate {
            return Ok(Some((k.clone(), deref(v))));
        }
    }
    Ok(None)
}

/// `array_find(array $array, callable $callback): mixed`.
pub fn array_find<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
) -> RtResult<Value> {
    Ok(php_array_find(ctx, array, callback, false)?.map_or(Value::Null, |(_, v)| v))
}

/// `array_find_key(array $array, callable $callback): mixed`.
pub fn array_find_key<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
) -> RtResult<Value> {
    Ok(php_array_find(ctx, array, callback, false)?.map_or(Value::Null, |(k, _)| k.to_value()))
}

/// `array_any(array $array, callable $callback): bool`.
pub fn array_any<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
) -> RtResult<bool> {
    Ok(php_array_find(ctx, array, callback, false)?.is_some())
}

/// `array_all(array $array, callable $callback): bool`.
pub fn array_all<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    array: &HashTable,
    callback: &mut Callback<'_, C>,
) -> RtResult<bool> {
    Ok(php_array_find(ctx, array, callback, true)?.is_none())
}
