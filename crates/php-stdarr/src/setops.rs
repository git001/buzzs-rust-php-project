// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `array_unique` and the `array_intersect*` / `array_diff*` families.
//!
//! The sort-based algorithms (`php_array_intersect`, `php_array_diff`) are
//! ported step by step, including the order in which comparison callbacks
//! are invoked and PHP's quirk of selecting the user callback through a
//! single global slot (`BG(user_compare_fci)`), which is observable in
//! `array_uintersect_uassoc` / `array_udiff_uassoc`.

use crate::compare::{
    compare_data_by, data_compare_string, key_compare_string, SortKind, UserCompare,
};
use crate::util::{copy_elem, not_array_error, ErrSlot};
use crate::{ArrayContext, Callback, HT_MAX_SIZE, SORT_STRING};
use php_runtime::sort::zend_sort;
use php_runtime::{ArrayKey, HashTable, RtResult, RuntimeContext, RuntimeError, Value, ZArray};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// array_unique
// ---------------------------------------------------------------------------

/// `array_unique(array $array, int $flags = SORT_STRING): array`.
pub fn array_unique<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    array: &ZArray,
    flags: i64,
) -> RtResult<ZArray> {
    if array.len() <= 1 {
        return Ok(array.clone());
    }
    if flags == SORT_STRING {
        let mut seen: HashSet<Vec<u8>> = HashSet::with_capacity(array.len());
        let mut out = HashTable::new();
        for (k, v) in array.iter() {
            let s = match v {
                Value::Str(s) => s.clone(),
                other => other.to_zstr(ctx)?,
            };
            if seen.insert(s.as_bytes().to_vec()) {
                let _ = out.add(k.clone(), copy_elem(v));
            }
        }
        return Ok(ZArray::from_table(out));
    }
    let kind = SortKind::from_flags(flags);
    let mut result: HashTable = (**array).clone();
    let mut items: Vec<(ArrayKey, Value, usize)> = array
        .iter()
        .enumerate()
        .map(|(i, (k, v))| (k.clone(), v.clone(), i))
        .collect();
    let mut slot = ErrSlot::default();
    zend_sort(
        &mut items,
        &mut |a: &(ArrayKey, Value, usize), b: &(ArrayKey, Value, usize)| {
            slot.run(|| compare_data_by(ctx, kind, false, &a.1, &b.1))
        },
    );
    if let Some(e) = slot.0 {
        return Err(e);
    }
    let mut lastkept = 0usize;
    for cmpdata in 1..items.len() {
        let c =
            slot.run(|| compare_data_by(ctx, kind, false, &items[lastkept].1, &items[cmpdata].1));
        if c != 0 {
            lastkept = cmpdata;
        } else {
            let p = if items[lastkept].2 > items[cmpdata].2 {
                let p = lastkept;
                lastkept = cmpdata;
                p
            } else {
                cmpdata
            };
            result.remove(&items[p].0);
        }
    }
    slot.into_result()?;
    Ok(ZArray::from_table(result))
}

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

/// Checks that every argument is an array (`zend_argument_type_error`).
fn check_arrays<'a>(func: &str, args: &'a [Value]) -> RtResult<Vec<&'a ZArray>> {
    args.iter()
        .enumerate()
        .map(|(i, a)| match a {
            Value::Array(t) => Ok(t),
            other => Err(not_array_error(
                func,
                i + 1,
                if i == 0 { Some("array") } else { None },
                other,
            )),
        })
        .collect()
}

/// How data (values) are compared.
enum DataCmp<'a, 'b, C: ?Sized> {
    /// Keys only (`*_key` functions).
    None,
    /// `string_compare_function`.
    Internal,
    /// A user callback.
    User(&'a mut Callback<'b, C>),
}

/// `php_array_intersect_key` / `php_array_diff_key`: hash lookups of the
/// keys of the first array in the others, optionally comparing data.
fn key_based<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Value],
    mut data: DataCmp<'_, '_, C>,
    intersect: bool,
) -> RtResult<ZArray> {
    let tables = check_arrays(func, args)?;
    let mut out = HashTable::new();
    for (k, val) in tables[0].iter() {
        let val = copy_elem(val);
        let mut ok = true;
        for other in &tables[1..] {
            let matched = match other.get(k) {
                None => false,
                Some(d) => match &mut data {
                    DataCmp::None => true,
                    DataCmp::Internal => data_compare_string(ctx, &val, d)? == 0,
                    DataCmp::User(cb) => {
                        let mut uc = UserCompare::new(func, &mut **cb);
                        uc.compare_plain(ctx, &val, d)? == 0
                    }
                },
            };
            if intersect && !matched {
                ok = false;
                break;
            }
            if !intersect && matched {
                ok = false;
                break;
            }
        }
        if ok {
            let _ = out.add(k.clone(), val);
        }
    }
    Ok(ZArray::from_table(out))
}

/// `INTERSECT_NORMAL` / `DIFF_NORMAL` (values), `*_KEY` (keys only) or
/// `*_ASSOC` (keys, then values).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Normal,
    Key,
    Assoc,
}

/// The comparison state of `php_array_intersect` / `php_array_diff`.
struct Cmps<'a, 'b, C: ?Sized> {
    func: &'a str,
    data_cb: Option<&'a mut Callback<'b, C>>,
    key_cb: Option<&'a mut Callback<'b, C>>,
    /// Which callback `BG(user_compare_fci)` currently holds.
    current_is_key: bool,
    warned: bool,
    slot: ErrSlot,
}

impl<C: ArrayContext + ?Sized> Cmps<'_, '_, C> {
    fn user(&mut self, ctx: &mut C, a: &Value, b: &Value) -> i32 {
        let (func, warned) = (self.func, &mut self.warned);
        let cb = if self.current_is_key {
            self.key_cb.as_deref_mut().or(self.data_cb.as_deref_mut())
        } else {
            self.data_cb.as_deref_mut().or(self.key_cb.as_deref_mut())
        };
        let Some(cb) = cb else { return 0 };
        self.slot.run(|| {
            let mut uc = UserCompare::new(func, cb);
            uc.warned = *warned;
            let r = uc.compare(ctx, a, b);
            *warned = uc.warned;
            r
        })
    }

    fn data(&mut self, ctx: &mut C, a: &(ArrayKey, Value), b: &(ArrayKey, Value)) -> i32 {
        if self.data_cb.is_some() {
            self.user(ctx, &a.1, &b.1)
        } else {
            self.slot.run(|| data_compare_string(ctx, &a.1, &b.1))
        }
    }

    fn key(&mut self, ctx: &mut C, a: &(ArrayKey, Value), b: &(ArrayKey, Value)) -> i32 {
        if self.key_cb.is_some() {
            self.user(ctx, &a.0.to_value(), &b.0.to_value())
        } else {
            key_compare_string(&a.0, &b.0)
        }
    }
}

type List = Vec<(ArrayKey, Value)>;

/// Builds and sorts the per-argument bucket buckets.
fn sorted_lists<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    tables: &[&ZArray],
    behavior: Behavior,
    cmps: &mut Cmps<'_, '_, C>,
) -> Vec<List> {
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: one entry per array argument"
    )]
    let mut buckets = Vec::with_capacity(tables.len());
    for a in tables {
        let mut list: List = a.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if list.len() > 1 {
            if behavior == Behavior::Normal {
                zend_sort(
                    &mut list,
                    &mut |x: &(ArrayKey, Value), y: &(ArrayKey, Value)| cmps.data(ctx, x, y),
                );
            } else {
                zend_sort(
                    &mut list,
                    &mut |x: &(ArrayKey, Value), y: &(ArrayKey, Value)| cmps.key(ctx, x, y),
                );
            }
        }
        buckets.push(list);
    }
    buckets
}

/// Port of `php_array_intersect`.
#[allow(clippy::too_many_lines)]
fn intersect_generic<'b, C: ArrayContext + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Value],
    behavior: Behavior,
    data_cb: Option<&mut Callback<'b, C>>,
    key_cb: Option<&mut Callback<'b, C>>,
) -> RtResult<ZArray> {
    let key_user = key_cb.is_some();
    let data_user = data_cb.is_some();
    let mut cmps = Cmps {
        func,
        data_cb,
        key_cb,
        current_is_key: behavior != Behavior::Normal && key_user,
        warned: false,
        slot: ErrSlot::default(),
    };
    let tables = check_arrays(func, args)?;
    let buckets = sorted_lists(ctx, &tables, behavior, &mut cmps);
    let mut result: HashTable = (**tables[0]).clone();
    let n_args = buckets.len();
    let sizes: Vec<usize> = buckets.iter().map(Vec::len).collect();
    let mut ptrs = vec![0usize; n_args];
    let mut c: i32 = 0;
    'outer: while ptrs[0] < sizes[0] {
        if behavior != Behavior::Normal && key_user {
            cmps.current_is_key = true;
        }
        let mut i = 1;
        while i < n_args {
            if behavior == Behavior::Normal {
                while ptrs[i] < sizes[i] {
                    c = cmps.data(ctx, &buckets[0][ptrs[0]], &buckets[i][ptrs[i]]);
                    if c <= 0 {
                        break;
                    }
                    ptrs[i] += 1;
                }
            } else {
                while ptrs[i] < sizes[i] {
                    c = cmps.key(ctx, &buckets[0][ptrs[0]], &buckets[i][ptrs[i]]);
                    if c <= 0 {
                        break;
                    }
                    ptrs[i] += 1;
                }
                if c == 0 && ptrs[i] < sizes[i] && behavior == Behavior::Assoc {
                    if data_user {
                        cmps.current_is_key = false;
                    }
                    if cmps.data(ctx, &buckets[0][ptrs[0]], &buckets[i][ptrs[i]]) != 0 {
                        c = 1;
                        if key_user {
                            cmps.current_is_key = true;
                        }
                    }
                }
            }
            if ptrs[i] == sizes[i] {
                while ptrs[0] < sizes[0] {
                    result.remove(&buckets[0][ptrs[0]].0);
                    ptrs[0] += 1;
                }
                break 'outer;
            }
            if c != 0 {
                break;
            }
            ptrs[i] += 1;
            i += 1;
        }
        if c != 0 {
            loop {
                result.remove(&buckets[0][ptrs[0]].0);
                ptrs[0] += 1;
                if ptrs[0] == sizes[0] {
                    break 'outer;
                }
                if behavior == Behavior::Normal {
                    if cmps.data(ctx, &buckets[0][ptrs[0]], &buckets[i][ptrs[i]]) >= 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        } else {
            loop {
                ptrs[0] += 1;
                if ptrs[0] == sizes[0] {
                    break 'outer;
                }
                if behavior == Behavior::Normal {
                    if cmps.data(ctx, &buckets[0][ptrs[0] - 1], &buckets[0][ptrs[0]]) != 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }
    cmps.slot.into_result()?;
    Ok(ZArray::from_table(result))
}

/// Port of `php_array_diff`.
#[allow(clippy::too_many_lines)]
fn diff_generic<'b, C: ArrayContext + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Value],
    behavior: Behavior,
    data_cb: Option<&mut Callback<'b, C>>,
    key_cb: Option<&mut Callback<'b, C>>,
) -> RtResult<ZArray> {
    let key_user = key_cb.is_some();
    let data_user = data_cb.is_some();
    let mut cmps = Cmps {
        func,
        data_cb,
        key_cb,
        current_is_key: behavior != Behavior::Normal && key_user,
        warned: false,
        slot: ErrSlot::default(),
    };
    let tables = check_arrays(func, args)?;
    let buckets = sorted_lists(ctx, &tables, behavior, &mut cmps);
    let mut result: HashTable = (**tables[0]).clone();
    let n_args = buckets.len();
    let sizes: Vec<usize> = buckets.iter().map(Vec::len).collect();
    let mut ptrs = vec![0usize; n_args];
    'outer: while ptrs[0] < sizes[0] {
        if behavior != Behavior::Normal && key_user {
            cmps.current_is_key = true;
        }
        let mut c: i32 = 1;
        for i in 1..n_args {
            let mut ptr = ptrs[i];
            if behavior == Behavior::Normal {
                while ptrs[i] < sizes[i] {
                    c = cmps.data(ctx, &buckets[0][ptrs[0]], &buckets[i][ptrs[i]]);
                    if c <= 0 {
                        break;
                    }
                    ptrs[i] += 1;
                }
            } else {
                while ptr < sizes[i] {
                    c = cmps.key(ctx, &buckets[0][ptrs[0]], &buckets[i][ptr]);
                    if c == 0 {
                        break;
                    }
                    ptr += 1;
                }
            }
            if c == 0 {
                match behavior {
                    Behavior::Normal => {
                        if ptrs[i] < sizes[i] {
                            ptrs[i] += 1;
                        }
                        break;
                    }
                    Behavior::Assoc => {
                        if ptr < sizes[i] {
                            if data_user {
                                cmps.current_is_key = false;
                            }
                            if cmps.data(ctx, &buckets[0][ptrs[0]], &buckets[i][ptr]) != 0 {
                                c = -1;
                                if key_user {
                                    cmps.current_is_key = true;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    Behavior::Key => break,
                }
            }
        }
        if c == 0 {
            loop {
                result.remove(&buckets[0][ptrs[0]].0);
                ptrs[0] += 1;
                if ptrs[0] == sizes[0] {
                    break 'outer;
                }
                if behavior == Behavior::Normal {
                    if cmps.data(ctx, &buckets[0][ptrs[0] - 1], &buckets[0][ptrs[0]]) != 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        } else {
            loop {
                ptrs[0] += 1;
                if ptrs[0] == sizes[0] {
                    break 'outer;
                }
                if behavior == Behavior::Normal {
                    if cmps.data(ctx, &buckets[0][ptrs[0] - 1], &buckets[0][ptrs[0]]) != 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }
    cmps.slot.into_result()?;
    Ok(ZArray::from_table(result))
}

// ---------------------------------------------------------------------------
// array_intersect family
// ---------------------------------------------------------------------------

/// `php_array_intersect_get_key`: the set key of a value (its string
/// conversion under symbol-table normalization).
fn value_set_key<C: RuntimeContext + ?Sized>(ctx: &mut C, v: &Value) -> RtResult<ArrayKey> {
    Ok(match crate::util::deref(v) {
        Value::Int(i) => ArrayKey::Int(i),
        Value::Str(s) => ArrayKey::from_zstr(&s),
        other => ArrayKey::from_zstr(&other.to_zstr(ctx)?),
    })
}

/// `php_array_intersect_hash` (master's hash-based `array_intersect()`).
fn intersect_hash<C: RuntimeContext + ?Sized>(ctx: &mut C, tables: &[&ZArray]) -> RtResult<ZArray> {
    let mut result: HashTable = (**tables[0]).clone();
    if tables.iter().any(|a| a.is_empty()) {
        let keys: Vec<ArrayKey> = result.keys().cloned().collect();
        for k in &keys {
            result.remove(k);
        }
        return Ok(ZArray::from_table(result));
    }
    let mut set: HashTable = HashTable::with_capacity(tables[1].len());
    for v in tables[1].values() {
        let k = value_set_key(ctx, v)?;
        set.insert(k, Value::Int(1));
    }
    for (i, a) in tables.iter().enumerate().skip(2) {
        let i = i64::try_from(i).unwrap_or(i64::MAX);
        for v in a.values() {
            let k = value_set_key(ctx, v)?;
            if let Some(Value::Int(n)) = set.get_mut(&k) {
                if *n == i - 1 {
                    *n = i;
                }
            }
        }
    }
    let want = i64::try_from(tables.len() - 1).unwrap_or(i64::MAX);
    let mut delete = Vec::new();
    for (k, v) in &result {
        let sk = value_set_key(ctx, v)?;
        if !matches!(set.get(&sk), Some(Value::Int(n)) if *n == want) {
            delete.push(k.clone());
        }
    }
    for k in &delete {
        result.remove(k);
    }
    Ok(ZArray::from_table(result))
}

/// `array_intersect(array $array, array ...$arrays): array`.
pub fn array_intersect<C: ArrayContext + ?Sized>(ctx: &mut C, args: &[Value]) -> RtResult<ZArray> {
    if args.len() >= 2 {
        let tables = check_arrays("array_intersect", args)?;
        return intersect_hash(ctx, &tables);
    }
    intersect_generic(ctx, "array_intersect", args, Behavior::Normal, None, None)
}

/// `array_uintersect(array $array, ...$rest): array` (`args` are the
/// arrays, `data_compare` the trailing callback).
pub fn array_uintersect<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    intersect_generic(
        ctx,
        "array_uintersect",
        args,
        Behavior::Normal,
        Some(data_compare),
        None,
    )
}

/// `array_intersect_key(array $array, array ...$arrays): array`.
pub fn array_intersect_key<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
) -> RtResult<ZArray> {
    key_based(ctx, "array_intersect_key", args, DataCmp::None, true)
}

/// `array_intersect_assoc(array $array, array ...$arrays): array`.
pub fn array_intersect_assoc<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
) -> RtResult<ZArray> {
    key_based(ctx, "array_intersect_assoc", args, DataCmp::Internal, true)
}

/// `array_uintersect_assoc(array $array, ...$rest): array`.
pub fn array_uintersect_assoc<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    key_based(
        ctx,
        "array_uintersect_assoc",
        args,
        DataCmp::User(data_compare),
        true,
    )
}

/// `array_intersect_ukey(array $array, ...$rest): array`.
pub fn array_intersect_ukey<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    key_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    intersect_generic(
        ctx,
        "array_intersect_ukey",
        args,
        Behavior::Key,
        None,
        Some(key_compare),
    )
}

/// `array_intersect_uassoc(array $array, ...$rest): array`.
pub fn array_intersect_uassoc<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    key_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    intersect_generic(
        ctx,
        "array_intersect_uassoc",
        args,
        Behavior::Assoc,
        None,
        Some(key_compare),
    )
}

/// `array_uintersect_uassoc(array $array, ...$rest): array`.
pub fn array_uintersect_uassoc<'b, C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'b, C>,
    key_compare: &mut Callback<'b, C>,
) -> RtResult<ZArray> {
    intersect_generic(
        ctx,
        "array_uintersect_uassoc",
        args,
        Behavior::Assoc,
        Some(data_compare),
        Some(key_compare),
    )
}

// ---------------------------------------------------------------------------
// array_diff family
// ---------------------------------------------------------------------------

/// `array_diff(array $array, array ...$arrays): array`.
pub fn array_diff<C: RuntimeContext + ?Sized>(ctx: &mut C, args: &[Value]) -> RtResult<ZArray> {
    const F: &str = "array_diff";
    let Some(Value::Array(first)) = args.first() else {
        return Err(not_array_error(
            F,
            1,
            Some("array"),
            args.first().unwrap_or(&Value::Null),
        ));
    };
    let check = |i: usize, v: &Value| -> RtResult<()> {
        if let Value::Array(_) = v {
            Ok(())
        } else {
            Err(not_array_error(F, i + 1, None, v))
        }
    };
    if first.is_empty() {
        for (i, a) in args.iter().enumerate().skip(1) {
            check(i, a)?;
        }
        return Ok(ZArray::new());
    }
    if first.len() == 1 {
        let Some(value) = first.values().next() else {
            return Ok(ZArray::new());
        };
        let search = value.to_zstr(ctx)?;
        let mut found = false;
        for (i, a) in args.iter().enumerate().skip(1) {
            check(i, a)?;
            if !found {
                if let Value::Array(t) = a {
                    for v in t.values() {
                        if v.to_zstr(ctx)? == search {
                            found = true;
                            break;
                        }
                    }
                }
            }
        }
        return Ok(if found { ZArray::new() } else { first.clone() });
    }
    let mut num: u64 = 0;
    for (i, a) in args.iter().enumerate().skip(1) {
        check(i, a)?;
        if let Value::Array(t) = a {
            num += t.len() as u64;
        }
    }
    if num == 0 {
        return Ok(first.clone());
    }
    if num >= u64::from(HT_MAX_SIZE) {
        return Err(RuntimeError::Error(format!(
            "The total number of elements must be lower than {HT_MAX_SIZE}"
        )));
    }
    let mut exclude: HashSet<Vec<u8>> = HashSet::new();
    for a in &args[1..] {
        if let Value::Array(t) = a {
            for v in t.values() {
                exclude.insert(v.to_zstr(ctx)?.as_bytes().to_vec());
            }
        }
    }
    let mut out = HashTable::with_capacity(first.len());
    for (k, v) in first.iter() {
        let s = v.to_zstr(ctx)?;
        if !exclude.contains(s.as_bytes()) {
            let _ = out.add(k.clone(), copy_elem(v));
        }
    }
    Ok(ZArray::from_table(out))
}

/// `array_udiff(array $array, ...$rest): array`.
pub fn array_udiff<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    diff_generic(
        ctx,
        "array_udiff",
        args,
        Behavior::Normal,
        Some(data_compare),
        None,
    )
}

/// `array_diff_key(array $array, array ...$arrays): array`.
pub fn array_diff_key<C: RuntimeContext + ?Sized>(ctx: &mut C, args: &[Value]) -> RtResult<ZArray> {
    key_based(ctx, "array_diff_key", args, DataCmp::None, false)
}

/// `array_diff_assoc(array $array, array ...$arrays): array`.
pub fn array_diff_assoc<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
) -> RtResult<ZArray> {
    key_based(ctx, "array_diff_assoc", args, DataCmp::Internal, false)
}

/// `array_udiff_assoc(array $array, ...$rest): array`.
pub fn array_udiff_assoc<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    key_based(
        ctx,
        "array_udiff_assoc",
        args,
        DataCmp::User(data_compare),
        false,
    )
}

/// `array_diff_ukey(array $array, ...$rest): array`.
pub fn array_diff_ukey<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    key_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    diff_generic(
        ctx,
        "array_diff_ukey",
        args,
        Behavior::Key,
        None,
        Some(key_compare),
    )
}

/// `array_diff_uassoc(array $array, ...$rest): array`.
pub fn array_diff_uassoc<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    key_compare: &mut Callback<'_, C>,
) -> RtResult<ZArray> {
    diff_generic(
        ctx,
        "array_diff_uassoc",
        args,
        Behavior::Assoc,
        None,
        Some(key_compare),
    )
}

/// `array_udiff_uassoc(array $array, ...$rest): array`.
pub fn array_udiff_uassoc<'b, C: ArrayContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
    data_compare: &mut Callback<'b, C>,
    key_compare: &mut Callback<'b, C>,
) -> RtResult<ZArray> {
    diff_generic(
        ctx,
        "array_udiff_uassoc",
        args,
        Behavior::Assoc,
        Some(data_compare),
        Some(key_compare),
    )
}
