// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `version_compare()` — port of `ext/standard/versioning.c`.
//!
//! The C implementation works on NUL-terminated strings, so a version
//! containing a NUL byte is compared only up to that byte; the functions
//! here reproduce that.

use std::cmp::Ordering;

use crate::error::{StdError, StdResult};

/// Truncates at the first NUL byte (C string semantics).
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |n| &s[..n])
}

fn is_dig(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_ndig(c: u8) -> bool {
    !c.is_ascii_digit() && c != b'.'
}

/// `php_canonicalize_version()`: inserts `.` between digit/non-digit
/// transitions, turns `-`, `_`, `+` and other non-alphanumerics into `.`
/// (collapsing runs) and drops one trailing `.`.
pub fn canonicalize_version(version: &[u8]) -> Vec<u8> {
    let version = c_str(version);
    let Some((&first, rest)) = version.split_first() else {
        return Vec::new();
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most twice the input"
    )]
    let mut q = Vec::with_capacity(version.len() * 2);
    q.push(first);
    let mut lp = first;
    for &p in rest {
        let lq = q.last().copied().unwrap_or(0);
        if matches!(p, b'-' | b'_' | b'+') {
            if lq != b'.' {
                q.push(b'.');
            }
        } else if (is_ndig(lp) && is_dig(p)) || (is_dig(lp) && is_ndig(p)) {
            if lq != b'.' {
                q.push(b'.');
            }
            q.push(p);
        } else if !p.is_ascii_alphanumeric() {
            if lq != b'.' {
                q.push(b'.');
            }
        } else {
            q.push(p);
        }
        lp = p;
    }
    if q.last() == Some(&b'.') {
        q.pop();
    }
    q
}

/// `compare_special_version_forms()` order: `dev < alpha = a < beta = b <
/// RC = rc < # < pl = p`, unknown forms sorting before all of them.
fn special_form_order(form: &[u8]) -> i32 {
    const FORMS: [(&[u8], i32); 10] = [
        (b"dev", 0),
        (b"alpha", 1),
        (b"a", 1),
        (b"beta", 2),
        (b"b", 2),
        (b"RC", 3),
        (b"rc", 3),
        (b"#", 4),
        (b"pl", 5),
        (b"p", 5),
    ];
    FORMS
        .iter()
        .find(|(name, _)| form.starts_with(name))
        .map_or(-1, |&(_, order)| order)
}

fn compare_special(f1: &[u8], f2: &[u8]) -> i32 {
    (special_form_order(f1) - special_form_order(f2)).signum()
}

/// `strtol(p, NULL, 10)` on a digit run (saturating like glibc).
fn strtol(s: &[u8]) -> i64 {
    let mut v: i64 = 0;
    for &c in s.iter().take_while(|c| c.is_ascii_digit()) {
        v = v.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    v
}

/// `php_version_compare()`: returns -1, 0 or 1.
///
/// Backs `version_compare($version1, $version2)` (no operator).  The VM
/// passes both arguments as strings (`zpp` string coercion).
pub fn version_compare(v1: &[u8], v2: &[u8]) -> i32 {
    let v1 = c_str(v1);
    let v2 = c_str(v2);
    if v1.is_empty() || v2.is_empty() {
        return match (v1.is_empty(), v2.is_empty()) {
            (true, true) => 0,
            (false, _) => 1,
            _ => -1,
        };
    }
    let ver1 = if v1[0] == b'#' {
        v1.to_vec()
    } else {
        canonicalize_version(v1)
    };
    let ver2 = if v2[0] == b'#' {
        v2.to_vec()
    } else {
        canonicalize_version(v2)
    };
    let at = |v: &[u8], i: usize| v.get(i).copied().unwrap_or(0);
    let (mut p1, mut p2) = (0usize, 0usize);
    // `n1`/`n2` mirror the C pointers: `Some` while a `.` was found
    // (initially non-NULL).
    let (mut n1, mut n2): (Option<usize>, Option<usize>) = (Some(0), Some(0));
    let mut compare = 0;
    while at(&ver1, p1) != 0 && at(&ver2, p2) != 0 && n1.is_some() && n2.is_some() {
        n1 = ver1[p1..].iter().position(|&c| c == b'.').map(|i| p1 + i);
        n2 = ver2[p2..].iter().position(|&c| c == b'.').map(|i| p2 + i);
        let e1 = &ver1[p1..n1.unwrap_or(ver1.len())];
        let e2 = &ver2[p2..n2.unwrap_or(ver2.len())];
        let d1 = is_dig(at(e1, 0));
        let d2 = is_dig(at(e2, 0));
        compare = if d1 && d2 {
            match strtol(e1).cmp(&strtol(e2)) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }
        } else if !d1 && !d2 {
            compare_special(e1, e2)
        } else if d1 {
            compare_special(b"#N#", e2)
        } else {
            compare_special(e1, b"#N#")
        };
        if compare != 0 {
            break;
        }
        if let Some(n) = n1 {
            p1 = n + 1;
        }
        if let Some(n) = n2 {
            p2 = n + 1;
        }
    }
    if compare == 0 {
        if n1.is_some() {
            compare = if is_dig(at(&ver1, p1)) {
                1
            } else {
                version_compare(&ver1[p1.min(ver1.len())..], b"#N#")
            };
        } else if n2.is_some() {
            compare = if is_dig(at(&ver2, p2)) {
                -1
            } else {
                version_compare(b"#N#", &ver2[p2.min(ver2.len())..])
            };
        }
    }
    compare
}

/// `version_compare($version1, $version2, $operator)` with a non-null
/// operator: `<`/`lt`, `<=`/`le`, `>`/`gt`, `>=`/`ge`, `==`/`=`/`eq`,
/// `!=`/`<>`/`ne`.  Anything else throws the `ValueError`.
///
/// The VM passes `$operator` as a string; a `null` operator means the
/// two-argument form ([`version_compare`]).
pub fn version_compare_op(v1: &[u8], v2: &[u8], op: &[u8]) -> StdResult<bool> {
    let c = version_compare(v1, v2);
    Ok(match op {
        b"<" | b"lt" => c == -1,
        b"<=" | b"le" => c != 1,
        b">" | b"gt" => c == 1,
        b">=" | b"ge" => c != -1,
        b"==" | b"=" | b"eq" => c == 0,
        b"!=" | b"<>" | b"ne" => c != 0,
        _ => {
            return Err(StdError::arg_value(
                "version_compare",
                3,
                "operator",
                "must be a valid comparison operator",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical() {
        assert_eq!(canonicalize_version(b"1.0-dev"), b"1.0.dev");
        assert_eq!(canonicalize_version(b"1.0rc1"), b"1.0.rc.1");
        assert_eq!(canonicalize_version(b"1.2."), b"1.2");
        assert_eq!(canonicalize_version(b"1..2"), b"1.2");
        assert_eq!(canonicalize_version(b""), b"");
    }

    #[test]
    fn compare() {
        assert_eq!(version_compare(b"1", b"2"), -1);
        assert_eq!(version_compare(b"10", b"2"), 1);
        assert_eq!(version_compare(b"1.0", b"1.0.0"), -1);
        assert_eq!(version_compare(b"1.0-dev", b"1.0"), -1);
        assert_eq!(version_compare(b"1.0pl1", b"1.0"), 1);
        assert_eq!(version_compare(b"5.2", b"5.2\0x"), 0);
        assert_eq!(version_compare_op(b"1", b"2", b"lt"), Ok(true));
        assert!(version_compare_op(b"1", b"2", b"l").is_err());
    }
}
