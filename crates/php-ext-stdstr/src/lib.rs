// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard` string, HTML, URL, math, formatted-print, pack and scanf
//! functions for the phprust VM.
//!
//! The byte-level algorithms live in `php-stdstr` (and the printf /
//! `number_format` / `round` engines in `php-support`); this crate only
//! translates between PHP values and those libraries: the array forms of
//! `str_replace()`/`substr_replace()`/`strtr()`, by-reference outputs
//! (`&$count`, `&$percent`, `sscanf()` variables), INI lookups
//! (`default_charset`) and the conversion of library errors/notices into
//! thrown exceptions and emitted diagnostics.

// Every handler has the fixed `NativeFn` signature, so infallible ones still
// return `VmResult`, and the short names (`a` for the arguments, `s`, `n`,
// `f`, `l`, `r` …) follow php-src's own variable names.
#![allow(clippy::unnecessary_wraps, clippy::many_single_char_names)]

mod formatted;
mod html;
mod math;
mod misc;
mod strings;
mod url;

use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_stdstr::error::{Level, Notices, StdError, StdResult};
use php_vm::ext::{Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

pub use formatted::format as formatted_print;

/// The string/math/url part of `ext/standard`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdStr;

impl Extension for StdStr {
    fn name(&self) -> &'static str {
        "standard"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        strings::register(m);
        html::register(m);
        url::register(m);
        math::register(m);
        misc::register(m);
        formatted::register(m);
    }
}

/// `E_WARNING`.
pub(crate) const E_WARNING: i64 = 2;
/// `E_NOTICE`.
pub(crate) const E_NOTICE: i64 = 8;
/// `E_DEPRECATED`.
pub(crate) const E_DEPRECATED: i64 = 8192;

/// Emits the diagnostics a `php-stdstr` call collected, in order.
///
/// Diagnostics attributed to a function are emitted like
/// `php_error_docref()` (prefixed with the *running* function's name, so
/// aliases such as `chop` report their own name); the others like
/// `zend_error()`.
pub(crate) fn emit(vm: &mut Vm, notices: Notices) -> VmResult<()> {
    for n in notices {
        match (n.func, n.level) {
            (Some(_), Level::Warning) => vm.php_warning(&n.message)?,
            (Some(_), Level::Notice) => vm.php_notice(&n.message)?,
            (Some(_), Level::Deprecated) => vm.php_deprecated(&n.message)?,
            (None, Level::Warning) => vm.raise(E_WARNING, &n.message)?,
            (None, Level::Notice) => vm.raise(E_NOTICE, &n.message)?,
            (None, Level::Deprecated) => vm.raise(E_DEPRECATED, &n.message)?,
        }
    }
    Ok(())
}

/// Converts a library error into a thrown exception.
pub(crate) fn throw(vm: &mut Vm, e: &StdError) -> Throw {
    vm.throw(e.kind.class_name(), &e.to_string())
}

/// Unwraps a library result, throwing on error.
pub(crate) fn check<T>(vm: &mut Vm, r: StdResult<T>) -> VmResult<T> {
    r.map_err(|e| throw(vm, &e))
}

/// A PHP string value from bytes.
pub(crate) fn bytes(v: impl Into<ZStr>) -> Value {
    Value::Str(v.into())
}

/// `string|false`.
pub(crate) fn opt_bytes(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Bool(false), bytes)
}

/// A packed list of strings.
pub(crate) fn str_list(items: Vec<Vec<u8>>) -> Value {
    Value::Array(ZArray::from_values(items.into_iter().map(bytes)))
}

/// An array from ordered key/value pairs.
pub(crate) fn assoc<K: Into<ArrayKey>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Value {
    let mut t = HashTable::new();
    for (k, v) in pairs {
        t.insert(k, v);
    }
    Value::Array(ZArray::from_table(t))
}

/// The type name PHP uses in "must be of type X, Y given" messages
/// (`zend_zval_value_name`).
pub(crate) fn type_name(v: &Value) -> String {
    v.value_name()
}

/// Converts a value with `zval_get_string()` semantics; plain strings and
/// integers avoid the VM round trip.
pub(crate) fn to_str(vm: &mut Vm, v: &Value) -> VmResult<ZStr> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Reference(r) => to_str(vm, &r.get()),
        other => vm.value_to_string(other),
    }
}

/// `memory_limit` checks for script-sized allocations (PHP's fallible
/// `zend_string_safe_alloc()` + `RETURN_OOM()`); everything else is
/// accounted by `php_memlimit`'s global allocator.
pub(crate) mod mem {
    use php_vm::ext::VmResult;

    /// Allocation size of a `zend_string` of `len` bytes.
    pub(crate) const fn zstr(len: usize) -> usize {
        (24 + len + 1 + 7) & !7
    }

    /// Size of a packed array element (`zval`) holding a string of `len`
    /// bytes.
    pub(crate) const fn elem(len: usize) -> usize {
        16 + zstr(len)
    }

    /// Non-overlapping occurrences of `needle` (non-empty) in `hay`.
    pub(crate) fn occurrences(hay: &[u8], needle: &[u8], ci: bool) -> usize {
        if needle.is_empty() || needle.len() > hay.len() {
            return 0;
        }
        if ci {
            let (h, n) = (hay.to_ascii_lowercase(), needle.to_ascii_lowercase());
            return occurrences(&h, &n, false);
        }
        let mut count = 0;
        let mut p = 0;
        while let Some(r) = php_stdstr::search::memnstr(&hay[p..], needle) {
            count += 1;
            p += r + needle.len();
        }
        count
    }

    /// Reserves `alloc` bytes before building a result of that size; over
    /// the limit, the VM raises `Allowed memory size of N bytes exhausted
    /// (tried to allocate M bytes)` at the caller's line as the error
    /// leaves the builtin.
    pub(crate) fn check(alloc: usize) -> VmResult<()> {
        php_memlimit::try_reserve(alloc)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::mem;

    #[test]
    fn zend_string_sizes() {
        assert_eq!(mem::zstr(0), 32);
        assert_eq!(mem::zstr(7), 32);
        assert_eq!(mem::zstr(8), 40);
        assert_eq!(mem::elem(1), 48);
        assert_eq!(mem::occurrences(b"aXaxa", b"x", false), 1);
        assert_eq!(mem::occurrences(b"aXaxa", b"x", true), 2);
        assert_eq!(mem::occurrences(b"aaaa", b"aa", false), 2);
    }
}
