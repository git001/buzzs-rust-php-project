// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `SplFixedArray` (`spl_fixedarray.c`).

use crate::args::Args;
use crate::exceptions::{error, invalid_argument, out_of_bounds, type_error};
use crate::host::{ObjectIterator, SplHost};
use crate::object::{state_of, SplObject};
use crate::util::{this_obj, DimKind};
use php_runtime::convert::dval_to_lval_safe;
use php_runtime::numeric::handle_numeric_str;
use php_runtime::{
    ArrayKey, ErrorLevel, HashTable, Obj, PropertyPurpose, RtResult, RuntimeError, Value, ZArray,
};
use std::rc::Rc;

/// The internal state of an `SplFixedArray` (`spl_fixedarray`).
#[derive(Debug, Clone, Default)]
pub struct FixedArray {
    /// The elements; the size is `elements.len()`.
    pub elements: Vec<Value>,
    /// `cached_resize`: while a resize runs element destructors, the size
    /// a re-entrant `setSize()` asked for (`Some` = resize in progress).
    pub cached_resize: Option<usize>,
}

/// `sizeof(zval)`, for PHP's allocation-overflow message.
const ZVAL_SIZE: usize = 16;

/// `safe_emalloc(size, sizeof(zval), 0)`: the fatal error PHP raises when
/// the element block size overflows, or when it cannot be allocated.
fn checked_alloc(elements: &mut Vec<Value>, size: usize) -> RtResult<()> {
    if size
        .checked_mul(ZVAL_SIZE)
        .is_none_or(|b| b > isize::MAX.unsigned_abs())
    {
        return Err(RuntimeError::Fatal(format!(
            "Possible integer overflow in memory allocation ({size} * {ZVAL_SIZE} + 0)"
        )));
    }
    let extra = size.saturating_sub(elements.len());
    if elements.try_reserve_exact(extra).is_err() {
        return Err(RuntimeError::Fatal(format!(
            "Out of memory (tried to allocate {} bytes)",
            size * ZVAL_SIZE
        )));
    }
    elements.resize(size, Value::Null);
    Ok(())
}

/// The fallible part of `safe_femalloc(size, sizeof(zval), 0, true)` for
/// growing `current` elements to `size`: `false` when it would cross
/// `memory_limit` (the violation is recorded), and the caller returns
/// `RETURN_OOM()`'s null.
/// Sizes whose byte count overflows are left to [`checked_alloc`]'s fatal.
fn reserve(size: usize, current: usize) -> bool {
    let extra = size.saturating_sub(current);
    if extra == 0 || extra.checked_mul(ZVAL_SIZE).is_none() {
        return true;
    }
    php_memlimit::try_reserve_n(extra, std::mem::size_of::<Value>(), 0).is_ok()
}

impl FixedArray {
    /// Creates an array of `size` nulls.
    #[must_use]
    pub fn with_size(size: usize) -> Self {
        FixedArray {
            elements: vec![Value::Null; size],
            cached_resize: None,
        }
    }

    /// Whether the array may be (re-)initialised by `__construct()`,
    /// `__unserialize()` or `__wakeup()`: it is empty and no resize is
    /// running destructors (`spl_fixedarray_empty` +
    /// `spl_fixedarray_resize_in_progress`).
    #[must_use]
    pub fn can_init(&self) -> bool {
        self.elements.is_empty() && self.cached_resize.is_none()
    }

    /// `toArray()`.
    #[must_use]
    pub fn to_array(&self) -> ZArray {
        ZArray::from_values(self.elements.iter().cloned())
    }
}

/// `spl_offset_convert_to_ulong`: converts an offset to an index. Negative
/// numbers map to huge indexes (always out of range).
pub fn offset_to_index(host: &mut dyn SplHost, offset: &Value) -> RtResult<u64> {
    #[allow(clippy::cast_sign_loss)]
    Ok(match offset {
        Value::Int(n) => *n as u64,
        Value::Str(s) => match handle_numeric_str(s.as_bytes()) {
            Some(n) => n as u64,
            None => return Err(illegal_offset(offset)),
        },
        Value::Float(d) => dval_to_lval_safe(*d, host)? as u64,
        Value::Bool(b) => u64::from(*b),
        Value::Reference(r) => return offset_to_index(host, &r.get()),
        Value::Resource(r) => {
            host.diagnostic(
                ErrorLevel::Warning,
                &format!(
                    "Resource ID#{} used as offset, casting to integer ({})",
                    r.handle(),
                    r.handle()
                ),
            )?;
            r.handle() as u64
        }
        _ => return Err(illegal_offset(offset)),
    })
}

fn illegal_offset(offset: &Value) -> php_runtime::RuntimeError {
    type_error(format!(
        "Cannot access offset of type {} on SplFixedArray",
        offset.type_name()
    ))
}

fn index_error() -> php_runtime::RuntimeError {
    out_of_bounds("Index invalid or out of range")
}

fn no_append() -> php_runtime::RuntimeError {
    error("[] operator not supported for SplFixedArray")
}

fn read_helper(st: &SplObject, host: &mut dyn SplHost, offset: Option<&Value>) -> RtResult<Value> {
    let offset = offset.ok_or_else(no_append)?;
    let index = offset_to_index(host, offset)?;
    st.fixed(|f| {
        usize::try_from(index)
            .ok()
            .and_then(|i| f.elements.get(i).cloned())
    })?
    .ok_or_else(index_error)
}

fn write_helper(
    st: &SplObject,
    host: &mut dyn SplHost,
    offset: Option<&Value>,
    value: &Value,
) -> RtResult<()> {
    let offset = offset.ok_or_else(no_append)?;
    let index = offset_to_index(host, offset)?;
    let old = st.fixed(|f| {
        usize::try_from(index)
            .ok()
            .and_then(|i| f.elements.get_mut(i))
            .map(|slot| std::mem::replace(slot, value.deref()))
    })?;
    match old {
        Some(old) => {
            drop(old);
            Ok(())
        }
        None => Err(index_error()),
    }
}

fn unset_helper(st: &SplObject, host: &mut dyn SplHost, offset: &Value) -> RtResult<()> {
    write_helper(st, host, Some(offset), &Value::Null)
}

fn has_helper(
    st: &SplObject,
    host: &mut dyn SplHost,
    offset: &Value,
    check_empty: bool,
) -> RtResult<bool> {
    let index = offset_to_index(host, offset)?;
    let v = st.fixed(|f| {
        usize::try_from(index)
            .ok()
            .and_then(|i| f.elements.get(i).cloned())
    })?;
    match v {
        None => Ok(false),
        Some(v) if check_empty => v.to_bool(host),
        Some(v) => Ok(!v.is_null()),
    }
}

/// `SplFixedArray` methods. `method` is lowercase.
pub(crate) fn call(
    host: &mut dyn SplHost,
    method: &str,
    this: Option<&Obj>,
    args: &[Value],
) -> Option<RtResult<Value>> {
    Some(if method == "fromarray" {
        from_array(host, args)
    } else {
        let this = match this_obj(this) {
            Ok(t) => t,
            Err(e) => return Some(Err(e)),
        };
        match method {
            "__construct" => construct(host, this, args),
            "__wakeup" => wakeup(host, this, args),
            "__serialize" => serialize(host, this, args),
            "__unserialize" => unserialize(host, this, args),
            "count" | "getsize" | "toarray" | "jsonserialize" => simple(host, this, args, method),
            "setsize" => set_size(host, this, args),
            "offsetexists" => {
                let f = "SplFixedArray::offsetExists";
                (|| {
                    let a = Args::new(f, args, &["index"], 1, Some(1))?;
                    let st = state_of(host, this)?;
                    has_helper(&st, host, &a.raw(0), false).map(Value::Bool)
                })()
            }
            "offsetget" => (|| {
                let a = Args::new("SplFixedArray::offsetGet", args, &["index"], 1, Some(1))?;
                let st = state_of(host, this)?;
                read_helper(&st, host, Some(&a.raw(0)))
            })(),
            "offsetset" => (|| {
                let a = Args::new(
                    "SplFixedArray::offsetSet",
                    args,
                    &["index", "value"],
                    2,
                    Some(2),
                )?;
                let st = state_of(host, this)?;
                let off = a.raw(0);
                write_helper(&st, host, Some(&off), &a.raw(1)).map(|()| Value::Null)
            })(),
            "offsetunset" => (|| {
                let a = Args::new("SplFixedArray::offsetUnset", args, &["index"], 1, Some(1))?;
                let st = state_of(host, this)?;
                unset_helper(&st, host, &a.raw(0)).map(|()| Value::Null)
            })(),
            "getiterator" => Args::none("SplFixedArray::getIterator", args)
                .and_then(|()| crate::iter::internal_iterator_object(host, this)),
            _ => return None,
        }
    })
}

fn construct(host: &mut dyn SplHost, this: &Obj, args: &[Value]) -> RtResult<Value> {
    let a = Args::new("SplFixedArray::__construct", args, &["size"], 0, Some(1))?;
    let size = a.opt_long(host, 0, 0)?;
    if size < 0 {
        return Err(a.value_error(0, "must be greater than or equal to 0"));
    }
    let st = state_of(host, this)?;
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    if st.fixed(|f| f.can_init())? && !reserve(size, 0) {
        return Ok(Value::Null);
    }
    st.fixed(|f| {
        if f.can_init() {
            checked_alloc(&mut f.elements, size)
        } else {
            Ok(())
        }
    })??;
    Ok(Value::Null)
}

fn simple(host: &mut dyn SplHost, this: &Obj, args: &[Value], method: &str) -> RtResult<Value> {
    let name = match method {
        "count" => "SplFixedArray::count",
        "getsize" => "SplFixedArray::getSize",
        "toarray" => "SplFixedArray::toArray",
        _ => "SplFixedArray::jsonSerialize",
    };
    Args::none(name, args)?;
    let st = state_of(host, this)?;
    st.fixed(|f| match method {
        "count" | "getsize" => Value::Int(i64::try_from(f.elements.len()).unwrap_or(i64::MAX)),
        _ => Value::Array(f.to_array()),
    })
}

fn set_size(host: &mut dyn SplHost, this: &Obj, args: &[Value]) -> RtResult<Value> {
    let a = Args::new("SplFixedArray::setSize", args, &["size"], 1, Some(1))?;
    let size = a.long(host, 0)?;
    if size < 0 {
        return Err(a.value_error(0, "must be greater than or equal to 0"));
    }
    let st = state_of(host, this)?;
    resize(host, &st, usize::try_from(size).unwrap_or(usize::MAX))?;
    Ok(Value::Bool(true))
}

/// `spl_fixedarray_resize`: destructors of removed elements run without a
/// borrow of the state and may resize again (the last request wins once
/// the current resize finished) but cannot re-initialise the array.
pub fn resize(host: &mut dyn SplHost, st: &Rc<SplObject>, size: usize) -> RtResult<()> {
    enum Step {
        Done,
        Release { removed: Vec<Value>, reverse: bool },
    }
    let step = st.fixed(|f| -> RtResult<Step> {
        if size == f.elements.len() {
            return Ok(Step::Done);
        }
        if f.cached_resize.is_some() {
            f.cached_resize = Some(size);
            return Ok(Step::Done);
        }
        if f.elements.is_empty() {
            checked_alloc(&mut f.elements, size)?;
            return Ok(Step::Done);
        }
        if size > f.elements.len() {
            checked_alloc(&mut f.elements, size)?;
            return Ok(Step::Done);
        }
        f.cached_resize = Some(size);
        let removed = f.elements.split_off(size);
        f.elements.shrink_to_fit();
        // A full clear destroys from the end (`spl_fixedarray_dtor`), a
        // shrink from the cut (`spl_fixedarray_dtor_range`).
        Ok(Step::Release {
            removed,
            reverse: size == 0,
        })
    })??;
    let Step::Release { removed, reverse } = step else {
        return Ok(());
    };
    // Each element is released on its own, its destructor running before
    // the next one is released, with the array already resized.
    let mut failed = None;
    let mut release = |v: Value| {
        drop(v);
        if let Err(e) = host.run_pending_destructors() {
            failed.get_or_insert(e);
        }
    };
    if reverse {
        removed.into_iter().rev().for_each(&mut release);
    } else {
        removed.into_iter().for_each(&mut release);
    }
    let cached = st.fixed(|f| f.cached_resize.take())?;
    if let Some(e) = failed {
        return Err(e);
    }
    match cached {
        Some(c) if c != size => resize(host, st, c),
        _ => Ok(()),
    }
}

fn wakeup(host: &mut dyn SplHost, this: &Obj, args: &[Value]) -> RtResult<Value> {
    Args::none("SplFixedArray::__wakeup", args)?;
    let st = state_of(host, this)?;
    if st.fixed(|f| f.can_init())? {
        // `spl_fixedarray_init(&intern->array, size)` allocates the elements
        // before they are copied out of the property table, so a table too
        // large for `memory_limit` fails there, before any copy is made.
        let size = host.std_property_count(this);
        if !reserve(size, 0) {
            return Ok(Value::Null);
        }
        let props = host.std_properties(this);
        let elements: Vec<Value> = props.values().map(Value::deref).collect();
        st.fixed(|f| f.elements = elements)?;
        host.set_std_properties(this, ZArray::new())?;
    }
    Ok(Value::Null)
}

fn serialize(host: &mut dyn SplHost, this: &Obj, args: &[Value]) -> RtResult<Value> {
    Args::none("SplFixedArray::__serialize", args)?;
    let st = state_of(host, this)?;
    let props = host.std_properties(this);
    let elements = st.fixed(|f| f.elements.clone())?;
    let mut out = HashTable::with_capacity(elements.len() + props.len());
    for v in elements {
        let _ = out.push(v);
    }
    for (k, v) in props.iter() {
        if let ArrayKey::Str(s) = k {
            let _ = out.add(ArrayKey::Str(s.clone()), v.clone());
        }
    }
    Ok(Value::Array(ZArray::from_table(out)))
}

fn unserialize(host: &mut dyn SplHost, this: &Obj, args: &[Value]) -> RtResult<Value> {
    let a = Args::new("SplFixedArray::__unserialize", args, &["data"], 1, Some(1))?;
    let data = a.array(0)?;
    let st = state_of(host, this)?;
    if !st.fixed(|f| f.can_init())? {
        return Ok(Value::Null);
    }
    let mut elements = Vec::new();
    let mut members = HashTable::new();
    for (k, v) in data.iter() {
        match k {
            ArrayKey::Int(_) => elements.push(v.deref()),
            ArrayKey::Str(_) => {
                members.insert(k.clone(), v.clone());
            }
        }
    }
    st.fixed(|f| f.elements = elements)?;
    host.properties_load(this, &ZArray::from_table(members))?;
    Ok(Value::Null)
}

fn from_array(host: &mut dyn SplHost, args: &[Value]) -> RtResult<Value> {
    let a = Args::new(
        "SplFixedArray::fromArray",
        args,
        &["array", "preserveKeys"],
        1,
        Some(2),
    )?;
    let data = a.array(0)?;
    let save_indexes = a.opt_bool(host, 1, true)?;
    let num = data.len();
    let fixed = if num > 0 && save_indexes {
        let size: usize = if data.is_packed() {
            data.used() as usize
        } else {
            let mut max_index: i64 = 0;
            for k in data.keys() {
                match k {
                    ArrayKey::Int(n) if *n >= 0 => max_index = max_index.max(*n),
                    _ => {
                        return Err(invalid_argument(
                            "array must contain only positive integer keys",
                        ))
                    }
                }
            }
            let Some(tmp) = max_index.checked_add(1) else {
                return Err(invalid_argument("integer overflow detected"));
            };
            usize::try_from(tmp).unwrap_or(usize::MAX)
        };
        if !reserve(size, 0) {
            return Ok(Value::Null);
        }
        let mut f = FixedArray::default();
        checked_alloc(&mut f.elements, size)?;
        for (k, v) in data.iter() {
            if let ArrayKey::Int(n) = k {
                if let Some(slot) = usize::try_from(*n).ok().and_then(|i| f.elements.get_mut(i)) {
                    *slot = v.deref();
                }
            }
        }
        f
    } else {
        FixedArray {
            elements: data.values().map(Value::deref).collect(),
            cached_resize: None,
        }
    };
    let obj = host.create_object(&"SplFixedArray".into())?;
    let st = state_of(host, &obj)?;
    st.fixed(|f| *f = fixed)?;
    Ok(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// Object handlers
// ---------------------------------------------------------------------------

/// `read_dimension` handler.
pub fn read_dimension(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
    offset: Option<&Value>,
    kind: DimKind,
) -> RtResult<Value> {
    if kind == DimKind::Is {
        let present = match offset {
            Some(o) => has_dimension(host, obj, st, o, false)?,
            None => false,
        };
        if !present {
            return Ok(Value::Null);
        }
    }
    if host.has_user_method(obj, "offsetGet") {
        let off = offset.cloned().unwrap_or_default();
        return host.call_method(obj, "offsetGet", &[off]);
    }
    read_helper(st, host, offset)
}

/// `write_dimension` handler.
pub fn write_dimension(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
    offset: Option<&Value>,
    value: Value,
) -> RtResult<()> {
    if host.has_user_method(obj, "offsetSet") {
        let off = offset.cloned().unwrap_or_default();
        return host
            .call_method(obj, "offsetSet", &[off, value])
            .map(|_| ());
    }
    write_helper(st, host, offset, &value)
}

/// `unset_dimension` handler.
pub fn unset_dimension(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
    offset: &Value,
) -> RtResult<()> {
    if host.has_user_method(obj, "offsetUnset") {
        return host
            .call_method(obj, "offsetUnset", std::slice::from_ref(offset))
            .map(|_| ());
    }
    unset_helper(st, host, offset)
}

/// `has_dimension` handler.
pub fn has_dimension(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
    offset: &Value,
    check_empty: bool,
) -> RtResult<bool> {
    if host.has_user_method(obj, "offsetExists") {
        let r = host.call_method(obj, "offsetExists", std::slice::from_ref(offset))?;
        return r.to_bool(host);
    }
    has_helper(st, host, offset, check_empty)
}

/// `count_elements` handler.
pub fn count_elements(host: &mut dyn SplHost, obj: &Obj, st: &Rc<SplObject>) -> RtResult<i64> {
    if host.has_user_method(obj, "count") {
        let r = host.call_method(obj, "count", &[])?;
        return r.to_long(host);
    }
    st.fixed(|f| i64::try_from(f.elements.len()).unwrap_or(i64::MAX))
}

/// `get_properties_for` handler: the elements followed by the properties.
pub fn properties_for(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
    purpose: PropertyPurpose,
) -> RtResult<ZArray> {
    let _ = purpose;
    let props = host.std_properties(obj);
    let elements = st.fixed(|f| f.elements.clone())?;
    let mut ht = HashTable::with_capacity(elements.len());
    for v in elements {
        let _ = ht.push(v);
    }
    for (k, v) in props.iter() {
        match k {
            ArrayKey::Str(_) => {
                let _ = ht.add(k.clone(), v.clone());
            }
            ArrayKey::Int(_) => {
                ht.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(ZArray::from_table(ht))
}

/// The `SplFixedArray` iterator (`spl_fixedarray_it`).
#[derive(Debug)]
pub struct FixedArrayIter {
    obj: Obj,
    st: Rc<SplObject>,
    current: i64,
}

impl FixedArrayIter {
    /// Creates the iterator.
    #[must_use]
    pub fn new(obj: Obj, st: Rc<SplObject>) -> Self {
        FixedArrayIter {
            obj,
            st,
            current: 0,
        }
    }
}

impl ObjectIterator for FixedArrayIter {
    fn rewind(&mut self, _host: &mut dyn SplHost) -> RtResult<()> {
        self.current = 0;
        Ok(())
    }
    fn valid(&mut self, _host: &mut dyn SplHost) -> RtResult<bool> {
        let n = self.st.fixed(|f| f.elements.len())?;
        Ok(self.current >= 0 && usize::try_from(self.current).is_ok_and(|c| c < n))
    }
    fn current(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        let _ = &self.obj;
        read_helper(&self.st, host, Some(&Value::Int(self.current))).map(Some)
    }
    fn key(&mut self, _host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        Ok(Some(Value::Int(self.current)))
    }
    fn next(&mut self, _host: &mut dyn SplHost) -> RtResult<()> {
        self.current += 1;
        Ok(())
    }
}
