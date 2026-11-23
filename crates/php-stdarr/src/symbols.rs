// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `extract()` and `compact()`, which work on the caller's variables
//! through the [`SymbolTable`] trait.

use crate::util::{arg_value_error, warn, Guard};
use crate::ArrayContext;
use crate::{
    EXTR_IF_EXISTS, EXTR_OVERWRITE, EXTR_PREFIX_ALL, EXTR_PREFIX_IF_EXISTS, EXTR_PREFIX_INVALID,
    EXTR_PREFIX_SAME, EXTR_REFS, EXTR_SKIP,
};
use php_runtime::{ArrayKey, HashTable, PhpRef, RtResult, RuntimeError, Value, ZArray, ZStr};

/// The state of a variable in a symbol table.
#[derive(Debug, Clone)]
pub enum VarSlot {
    /// No such variable.
    Missing,
    /// A compiled variable slot that exists but is unset (`IS_INDIRECT` to
    /// `IS_UNDEF`).
    Undefined,
    /// A defined variable (possibly a `Value::Reference`).
    Defined(Value),
}

/// Access to the active function's variables (`zend_rebuild_symbol_table`).
pub trait SymbolTable {
    /// Looks a variable up.
    fn lookup(&mut self, name: &ZStr) -> VarSlot;

    /// Assigns to an existing (possibly undefined) variable like `$name = $value`:
    /// through its reference when it is one, with typed-reference coercion
    /// (`ZEND_TRY_ASSIGN_COPY_EX`).
    fn assign(&mut self, name: &ZStr, value: Value) -> RtResult<()>;

    /// Stores `value` in the variable's slot, replacing what was there
    /// (breaking a previous reference binding), or creates the variable.
    /// `value` is a `Value::Reference` for `EXTR_REFS` bindings.
    fn set(&mut self, name: &ZStr, value: Value);
}

/// `php_valid_var_name`: `[a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*`.
#[must_use]
pub fn valid_var_name(name: &[u8]) -> bool {
    let first = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    match name.split_first() {
        None => false,
        Some((&c, rest)) => first(c) && rest.iter().all(|&c| first(c) || c.is_ascii_digit()),
    }
}

fn prefixed(prefix: &[u8], name: &[u8]) -> ZStr {
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut v = Vec::with_capacity(prefix.len() + 1 + name.len());
    v.extend_from_slice(prefix);
    v.push(b'_');
    v.extend_from_slice(name);
    ZStr::from(v)
}

fn this_error() -> RuntimeError {
    RuntimeError::Error("Cannot re-assign $this".to_string())
}

/// The source array of `extract()`: elements are made references in place
/// for `EXTR_REFS`.
struct Source {
    holder: PhpRef,
}

impl Source {
    /// The element for binding: a reference (refs mode) or the dereferenced
    /// value.
    fn entry(&self, key: &ArrayKey, refs: bool) -> Value {
        if refs {
            let mut slot = self.holder.borrow_mut();
            if let Value::Array(a) = &mut *slot {
                if let Some(v) = a.make_mut().get_mut(key) {
                    return Value::Reference(v.make_ref());
                }
            }
            return Value::Null;
        }
        let v = match &*self.holder.borrow() {
            Value::Array(a) => a.get(key).cloned().unwrap_or_default(),
            _ => Value::Null,
        };
        crate::util::deref(&v)
    }
}

/// Writes `entry` to `name`: references are bound (`ZVAL_REF`), values
/// assigned to existing variables or added.
fn store(sym: &mut dyn SymbolTable, name: &ZStr, entry: Value, exists: bool) -> RtResult<()> {
    if entry.is_reference() || !exists {
        sym.set(name, entry);
        Ok(())
    } else {
        sym.assign(name, entry)
    }
}

/// `extract(array &$array, int $flags = EXTR_OVERWRITE, string $prefix = ""): int`.
///
/// `array` is the argument as passed: a `Value::Reference` when the caller
/// passed a variable (required for `EXTR_REFS` to bind to its elements) or
/// a plain array. `prefix` is `None` when the argument was not passed.
#[allow(clippy::too_many_lines)]
pub fn extract(
    sym: &mut dyn SymbolTable,
    array: &Value,
    flags: i64,
    prefix: Option<&ZStr>,
) -> RtResult<i64> {
    const F: &str = "extract";
    let refs = flags & EXTR_REFS != 0;
    let kind = flags & 0xff;
    if !(EXTR_OVERWRITE..=EXTR_IF_EXISTS).contains(&kind) {
        return Err(arg_value_error(
            F,
            2,
            Some("flags"),
            "must be a valid extract type",
        ));
    }
    if kind > EXTR_SKIP && kind <= EXTR_PREFIX_IF_EXISTS && prefix.is_none() {
        return Err(arg_value_error(
            F,
            3,
            Some("prefix"),
            "is required when using this extract type",
        ));
    }
    if let Some(p) = prefix {
        if !p.is_empty() && !valid_var_name(p.as_bytes()) {
            return Err(arg_value_error(
                F,
                3,
                Some("prefix"),
                "must be a valid identifier",
            ));
        }
    }
    let prefix = prefix.map_or(&[][..], ZStr::as_bytes);
    let holder = match array {
        Value::Reference(r) if refs => r.clone(),
        other => PhpRef::new(crate::util::deref(other)),
    };
    let keys: Vec<ArrayKey> = match &*holder.borrow() {
        Value::Array(a) => a.keys().cloned().collect(),
        other => {
            return Err(crate::util::not_array_error(F, 1, Some("array"), other));
        }
    };
    let src = Source { holder };
    let mut count = 0i64;
    for key in &keys {
        let name = match key {
            ArrayKey::Str(s) => Some(s.clone()),
            ArrayKey::Int(_) => None,
        };
        match kind {
            EXTR_IF_EXISTS => {
                let Some(name) = name else { continue };
                if !matches!(sym.lookup(&name), VarSlot::Defined(_)) {
                    continue;
                }
                if !valid_var_name(name.as_bytes()) || name.as_bytes() == b"GLOBALS" {
                    continue;
                }
                if name.as_bytes() == b"this" {
                    return Err(this_error());
                }
                let e = src.entry(key, refs);
                store(sym, &name, e, true)?;
                count += 1;
            }
            EXTR_OVERWRITE => {
                let Some(name) = name else { continue };
                if !valid_var_name(name.as_bytes()) {
                    continue;
                }
                if name.as_bytes() == b"this" {
                    return Err(this_error());
                }
                let exists = !matches!(sym.lookup(&name), VarSlot::Missing);
                if exists && name.as_bytes() == b"GLOBALS" {
                    continue;
                }
                let e = src.entry(key, refs);
                store(sym, &name, e, exists)?;
                count += 1;
            }
            EXTR_PREFIX_IF_EXISTS | EXTR_PREFIX_SAME => {
                let Some(name) = name else { continue };
                if kind == EXTR_PREFIX_SAME && name.is_empty() {
                    continue;
                }
                let use_prefix = match sym.lookup(&name) {
                    VarSlot::Undefined => {
                        let e = src.entry(key, refs);
                        sym.set(&name, e);
                        count += 1;
                        continue;
                    }
                    VarSlot::Defined(_) => true,
                    VarSlot::Missing => {
                        if kind == EXTR_PREFIX_IF_EXISTS || !valid_var_name(name.as_bytes()) {
                            continue;
                        }
                        name.as_bytes() == b"this"
                    }
                };
                if use_prefix {
                    let fin = prefixed(prefix, name.as_bytes());
                    if valid_var_name(fin.as_bytes()) {
                        let e = src.entry(key, refs);
                        let exists = !matches!(sym.lookup(&fin), VarSlot::Missing);
                        store(sym, &fin, e, exists)?;
                        count += 1;
                    }
                } else {
                    let e = src.entry(key, refs);
                    sym.set(&name, e);
                    count += 1;
                }
            }
            EXTR_PREFIX_ALL | EXTR_PREFIX_INVALID => {
                let fin = match key {
                    ArrayKey::Str(s) => {
                        if kind == EXTR_PREFIX_ALL {
                            if s.is_empty() {
                                continue;
                            }
                            prefixed(prefix, s.as_bytes())
                        } else if !valid_var_name(s.as_bytes()) || s.as_bytes() == b"this" {
                            prefixed(prefix, s.as_bytes())
                        } else {
                            s.clone()
                        }
                    }
                    ArrayKey::Int(i) => prefixed(prefix, i.to_string().as_bytes()),
                };
                if !valid_var_name(fin.as_bytes()) {
                    continue;
                }
                let e = src.entry(key, refs);
                let exists = !matches!(sym.lookup(&fin), VarSlot::Missing);
                store(sym, &fin, e, exists)?;
                count += 1;
            }
            _ => {
                // EXTR_SKIP
                let Some(name) = name else { continue };
                if !valid_var_name(name.as_bytes()) || name.as_bytes() == b"this" {
                    continue;
                }
                match sym.lookup(&name) {
                    VarSlot::Defined(_) => {}
                    VarSlot::Undefined | VarSlot::Missing => {
                        let e = src.entry(key, refs);
                        sym.set(&name, e);
                        count += 1;
                    }
                }
            }
        }
    }
    Ok(count)
}

fn compact_var<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    sym: &mut dyn SymbolTable,
    out: &mut HashTable,
    entry: &Value,
    pos: usize,
    guard: &mut Guard,
) -> RtResult<()> {
    match crate::util::deref(entry) {
        Value::Str(name) => match sym.lookup(&name) {
            VarSlot::Defined(v) => {
                out.insert_str_raw(name, crate::util::deref(&v));
            }
            _ if name.as_bytes() == b"this" => {
                if let Some(this) = ctx.this_object() {
                    out.insert_str_raw(name, this);
                } else {
                    // No `$this`: nothing is added and no warning is emitted.
                }
            }
            _ => {
                warn(
                    ctx,
                    "compact",
                    &format!("Undefined variable ${}", name.to_string_lossy()),
                )?;
            }
        },
        Value::Array(a) => {
            let addr = a.addr();
            if guard.contains(addr) {
                return Err(RuntimeError::Error("Recursion detected".to_string()));
            }
            guard.push(addr);
            let mut res = Ok(());
            for v in a.values() {
                res = compact_var(ctx, sym, out, v, pos, guard);
                if res.is_err() {
                    break;
                }
            }
            guard.pop(addr);
            res?;
        }
        other => {
            warn(
                ctx,
                "compact",
                &format!(
                    "Argument #{pos} must be string or array of strings, {} given",
                    other.value_name()
                ),
            )?;
        }
    }
    Ok(())
}

/// `compact($var_name, ...$var_names): array`.
pub fn compact<C: ArrayContext + ?Sized>(
    ctx: &mut C,
    sym: &mut dyn SymbolTable,
    args: &[Value],
) -> RtResult<ZArray> {
    let mut out = HashTable::new();
    let mut guard = Guard::default();
    for (i, a) in args.iter().enumerate() {
        compact_var(ctx, sym, &mut out, a, i + 1, &mut guard)?;
    }
    Ok(ZArray::from_table(out))
}
