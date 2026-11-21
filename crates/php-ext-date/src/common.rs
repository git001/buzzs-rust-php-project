// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request state shared by the date functions and classes (`DATEG()`),
//! value conversion helpers and small utilities.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use php_date::api;
use php_date::parse::Errors;
use php_date::tz::{self, TzInfo};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_vm::ext::VmResult;
use php_vm::Vm;

/// `E_WARNING`.
pub(crate) const E_WARNING: i64 = 2;
/// `E_DEPRECATED`.
pub(crate) const E_DEPRECATED: i64 = 8192;

thread_local! {
    /// `DATEG(last_errors)`.
    static LAST_ERRORS: RefCell<Option<Errors>> = const { RefCell::new(None) };
}

/// Clears the per-request state (`RSHUTDOWN`).
pub(crate) fn reset_request_state() {
    tz::set_default_override(None);
    LAST_ERRORS.with(|e| *e.borrow_mut() = None);
}

/// Sets `DATEG(timezone)` (`name` is a valid identifier).
pub(crate) fn set_tz_override(name: &str) {
    tz::set_default_override(tz::get(name));
}

/// Records `name` as `date.timezone` (`DATEG(default_timezone)`) if it is
/// a valid identifier, else none (UTC).
pub(crate) fn set_configured_tz(name: Option<&[u8]>) {
    let zone = name
        .and_then(|n| std::str::from_utf8(n).ok())
        .filter(|n| !n.is_empty() && tz::is_valid_id(n))
        .and_then(tz::get);
    tz::set_default_configured(zone);
}

/// `update_errors_warnings()`: remembers the parse messages for
/// `date_get_last_errors()` (only when there are any).
pub(crate) fn update_errors(e: &Errors) {
    let v = if e.is_empty() { None } else { Some(e.clone()) };
    LAST_ERRORS.with(|l| *l.borrow_mut() = v);
}

/// Forgets the last parse messages.
#[cfg(test)]
pub(crate) fn clear_errors() {
    LAST_ERRORS.with(|l| *l.borrow_mut() = None);
}

/// `DATEG(last_errors)`.
pub(crate) fn last_errors() -> Option<Errors> {
    LAST_ERRORS.with(|l| l.borrow().clone())
}

/// `get_timezone_info()`: the thread's default timezone
/// ([`tz::default_zone`]: the override, else a valid `date.timezone`, else
/// UTC).
#[allow(clippy::unnecessary_wraps)] // the callers' `?`; php-src can throw here
pub(crate) fn default_tz(_vm: &mut Vm) -> VmResult<Arc<TzInfo>> {
    Ok(tz::default_zone())
}

/// The current time as seconds and microseconds (`gettimeofday`).
pub(crate) fn now() -> (i64, i64) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => (
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            i64::from(d.subsec_micros()),
        ),
        Err(e) => {
            // Before 1970: express as a negative timestamp.
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            let us = i64::from(d.subsec_micros());
            if us == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000 - us)
            }
        }
    }
}

/// `php_time()`.
pub(crate) fn php_time() -> i64 {
    now().0
}

/// Converts a `php-date` result tree into a PHP value.
pub(crate) fn to_php(v: &api::Value) -> Value {
    match v {
        api::Value::Null => Value::Null,
        api::Value::Bool(b) => Value::Bool(*b),
        api::Value::Int(i) => Value::Int(*i),
        api::Value::Float(f) => Value::Float(*f),
        api::Value::Str(s) => Value::Str(ZStr::from(s.as_str())),
        api::Value::Array(items) => {
            let mut a = ZArray::new();
            let t = a.make_mut();
            for (k, v) in items {
                let key = match k {
                    api::Key::Int(i) => ArrayKey::Int(*i),
                    api::Key::Str(s) => ArrayKey::Str(ZStr::from(s.as_str())),
                };
                t.insert(key, to_php(v));
            }
            Value::Array(a)
        }
    }
}

/// Lossy UTF-8 view of bytes for messages.
pub(crate) fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A string-keyed lookup in a PHP array.
pub(crate) fn get_str<'a>(a: &'a ZArray, key: &str) -> Option<&'a Value> {
    a.get(&ArrayKey::Str(ZStr::from(key)))
}

/// Inserts `key => v` (string key, `zend_hash_str_update`).
pub(crate) fn set_str(a: &mut ZArray, key: &str, v: Value) {
    a.make_mut().insert(ArrayKey::Str(ZStr::from(key)), v);
}

/// `zend_strtod` on an INI string (leading float, 0 when none).
pub(crate) fn strtod(s: &[u8]) -> f64 {
    let s = std::str::from_utf8(s).unwrap_or("").trim_start();
    let b = s.as_bytes();
    let mut end = 0;
    let mut seen_digit = false;
    if end < b.len() && (b[end] == b'+' || b[end] == b'-') {
        end += 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
        seen_digit = true;
    }
    if end < b.len() && b[end] == b'.' {
        end += 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
            seen_digit = true;
        }
    }
    if seen_digit && end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        let mut e = end + 1;
        if e < b.len() && (b[e] == b'+' || b[e] == b'-') {
            e += 1;
        }
        if e < b.len() && b[e].is_ascii_digit() {
            while e < b.len() && b[e].is_ascii_digit() {
                e += 1;
            }
            end = e;
        }
    }
    if !seen_digit {
        return 0.0;
    }
    s[..end].parse().unwrap_or(0.0)
}

/// `zend_dval_to_lval()`: warns about non-representable floats and wraps
/// like the engine does.
pub(crate) fn dval_to_lval(vm: &mut Vm, d: f64) -> VmResult<i64> {
    use php_runtime::convert::{double_fits_long, dval_to_lval_silent};
    if !d.is_finite() || !double_fits_long(d) {
        vm.raise(
            E_WARNING,
            &format!(
                "The float {} is not representable as an int, cast occurred",
                php_runtime::float::format_double_h(d, -1)
            ),
        )?;
    }
    Ok(dval_to_lval_silent(d))
}

/// An INI value as a float (`zend_ini_double`).
pub(crate) fn ini_double(vm: &Vm, name: &str) -> f64 {
    vm.ini_string(name).map_or(0.0, |v| strtod(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtod_prefixes() {
        assert!((strtod(b"31.7667") - 31.7667).abs() < 1e-12);
        assert!((strtod(b"90.833333") - 90.833_333).abs() < 1e-12);
        assert!((strtod(b"-1.5e2x") + 150.0).abs() < 1e-12);
        assert!(strtod(b"abc").abs() < f64::EPSILON);
    }

    #[test]
    fn errors_state() {
        clear_errors();
        assert!(last_errors().is_none());
        let (_, e) = php_date::parse::strtotime(b"foo");
        update_errors(&e);
        assert!(last_errors().is_some());
        update_errors(&Errors::default());
        assert!(last_errors().is_none());
    }

    #[test]
    fn conversion() {
        let v = api::date_parse(b"2006-12-12");
        let Value::Array(a) = to_php(&v) else {
            panic!("array expected")
        };
        assert!(matches!(get_str(&a, "year"), Some(Value::Int(2006))));
    }
}
