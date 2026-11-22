// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `array_merge`, `array_merge_recursive`, `array_replace`,
//! `array_replace_recursive`.

use crate::util::{cannot_add_element, copy_elem, deref, not_array_error, Guard};
use crate::HT_MAX_SIZE;
use php_runtime::{ArrayKey, HashTable, RtResult, RuntimeContext, RuntimeError, Value, ZArray};

/// Checks that every argument is an array; `first_name` is the declared
/// name of the first parameter (`None` when it is variadic).
fn arrays<'a>(
    func: &str,
    args: &'a [Value],
    first_name: Option<&str>,
) -> RtResult<Vec<&'a ZArray>> {
    args.iter()
        .enumerate()
        .map(|(i, a)| match a {
            Value::Array(t) => Ok(t),
            other => Err(not_array_error(
                func,
                i + 1,
                if i == 0 { first_name } else { None },
                other,
            )),
        })
        .collect()
}

fn too_many() -> RuntimeError {
    RuntimeError::Error(format!(
        "The total number of elements must be lower than {HT_MAX_SIZE}"
    ))
}

/// `php_array_merge`: appends `src` to `dest` (string keys overwrite,
/// integer keys are renumbered).
pub fn php_array_merge(dest: &mut HashTable, src: &HashTable) -> RtResult<()> {
    for (k, v) in src {
        match k {
            ArrayKey::Str(s) => {
                dest.try_insert(ArrayKey::Str(s.clone()), copy_elem(v))?;
            }
            ArrayKey::Int(_) => {
                dest.try_push(copy_elem(v))?
                    .map_err(|_| cannot_add_element())?;
            }
        }
    }
    Ok(())
}

/// `php_array_merge_recursive`.
pub fn php_array_merge_recursive<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    dest: &mut HashTable,
    src: &HashTable,
) -> RtResult<()> {
    merge_recursive(ctx, dest, src, &mut Guard::default())
}

fn merge_recursive<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    dest: &mut HashTable,
    src: &HashTable,
    guard: &mut Guard,
) -> RtResult<()> {
    for (k, src_entry) in src {
        let ArrayKey::Str(s) = k else {
            dest.push(copy_elem(src_entry))
                .map_err(|_| cannot_add_element())?;
            continue;
        };
        let Some(dest_entry) = dest.get(k).cloned() else {
            let _ = dest.add(ArrayKey::Str(s.clone()), copy_elem(src_entry));
            continue;
        };
        let src_zval = deref(src_entry);
        let thash = match deref(&dest_entry) {
            Value::Array(a) => Some(a.addr()),
            _ => None,
        };
        if thash.is_some_and(|t| guard.contains(t)) {
            return Err(RuntimeError::Error("Recursion detected".to_string()));
        }
        let mut new_dest = if matches!(dest_entry, Value::Null) {
            let mut t = HashTable::with_capacity(1);
            let _ = t.push(Value::Null);
            ZArray::from_table(t)
        } else {
            dest_entry.to_array(ctx)?
        };
        drop(dest_entry);
        let src_zval = match src_zval {
            Value::Object(_) => Value::Array(src_zval.to_array(ctx)?),
            other => other,
        };
        if let Value::Array(sa) = &src_zval {
            if let Some(t) = thash {
                guard.push(t);
            }
            let r = merge_recursive(ctx, new_dest.make_mut(), sa, guard);
            if let Some(t) = thash {
                guard.pop(t);
            }
            r?;
        } else {
            new_dest
                .make_mut()
                .push(src_zval)
                .map_err(|_| cannot_add_element())?;
        }
        dest.insert(ArrayKey::Str(s.clone()), Value::Array(new_dest));
    }
    Ok(())
}

fn merge_wrapper<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Value],
    recursive: bool,
) -> RtResult<ZArray> {
    if args.is_empty() {
        return Ok(ZArray::new());
    }
    let tables = arrays(func, args, None)?;
    let count: u64 = tables.iter().map(|a| a.len() as u64).sum();
    if count >= u64::from(HT_MAX_SIZE) {
        return Err(too_many());
    }
    if let [a, b] = tables[..] {
        let ret = if a.is_empty() {
            Some(b)
        } else if b.is_empty() {
            Some(a)
        } else {
            None
        };
        if let Some(ret) = ret {
            if ret.is_packed() {
                if ret.is_without_holes() {
                    return Ok(ret.clone());
                }
            } else if ret.keys().all(|k| matches!(k, ArrayKey::Str(_))) {
                return Ok(ret.clone());
            }
        }
    }
    let src = tables[0];
    let mut dest = HashTable::with_capacity(usize::try_from(count).unwrap_or(0));
    for (k, v) in src.iter() {
        match k {
            ArrayKey::Str(s) => {
                let _ = dest.try_add(ArrayKey::Str(s.clone()), copy_elem(v))?;
            }
            ArrayKey::Int(_) => {
                let _ = dest.try_push(copy_elem(v))?;
            }
        }
    }
    for a in &tables[1..] {
        if recursive {
            php_array_merge_recursive(ctx, &mut dest, a)?;
        } else {
            php_array_merge(&mut dest, a)?;
        }
    }
    Ok(ZArray::from_table(dest))
}

/// `array_merge(array ...$arrays): array`.
pub fn array_merge<C: RuntimeContext + ?Sized>(ctx: &mut C, args: &[Value]) -> RtResult<ZArray> {
    merge_wrapper(ctx, "array_merge", args, false)
}

/// `array_merge_recursive(array ...$arrays): array`.
pub fn array_merge_recursive<C: RuntimeContext + ?Sized>(
    ctx: &mut C,
    args: &[Value],
) -> RtResult<ZArray> {
    merge_wrapper(ctx, "array_merge_recursive", args, true)
}

/// `php_array_replace_recursive`.
pub fn php_array_replace_recursive(dest: &mut HashTable, src: &HashTable) -> RtResult<()> {
    replace_recursive(dest, src, &mut Guard::default())
}

fn replace_recursive(dest: &mut HashTable, src: &HashTable, guard: &mut Guard) -> RtResult<()> {
    for (k, src_entry) in src {
        let src_zval = deref(src_entry);
        let dest_entry = dest.get(k).cloned();
        let (Value::Array(sa), Some(dest_entry)) = (&src_zval, dest_entry) else {
            dest.insert(k.clone(), copy_elem(src_entry));
            continue;
        };
        let Value::Array(da) = deref(&dest_entry) else {
            dest.insert(k.clone(), copy_elem(src_entry));
            continue;
        };
        let same_ref = match (src_entry, &dest_entry) {
            (Value::Reference(a), Value::Reference(b)) => {
                php_runtime::PhpRef::ptr_eq(a, b) && a.refcount() % 2 == 1
            }
            _ => false,
        };
        if guard.contains(da.addr()) || guard.contains(sa.addr()) || same_ref {
            return Err(RuntimeError::Error("Recursion detected".to_string()));
        }
        drop(dest_entry);
        // SEPARATE_ZVAL: the destination slot gets its own copy (a reference
        // in the slot is broken).
        let mut new_dest = da;
        new_dest.make_mut();
        let (dk, sk) = (new_dest.addr(), sa.addr());
        guard.push(dk);
        guard.push(sk);
        let r = replace_recursive(new_dest.make_mut(), sa, guard);
        guard.pop(sk);
        guard.pop(dk);
        dest.insert(k.clone(), Value::Array(new_dest));
        r?;
    }
    Ok(())
}

fn replace_wrapper(func: &str, args: &[Value], recursive: bool) -> RtResult<ZArray> {
    let tables = arrays(func, args, Some("array"))?;
    let Some(first) = tables.first() else {
        return Ok(ZArray::new());
    };
    let mut dest: HashTable = (***first).clone();
    for a in &tables[1..] {
        if recursive {
            php_array_replace_recursive(&mut dest, a)?;
        } else {
            dest.merge(a, true);
        }
    }
    Ok(ZArray::from_table(dest))
}

/// `array_replace(array $array, array ...$replacements): array`.
pub fn array_replace(args: &[Value]) -> RtResult<ZArray> {
    replace_wrapper("array_replace", args, false)
}

/// `array_replace_recursive(array $array, array ...$replacements): array`.
pub fn array_replace_recursive(args: &[Value]) -> RtResult<ZArray> {
    replace_wrapper("array_replace_recursive", args, true)
}
