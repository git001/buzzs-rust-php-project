// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Small numeric-string helpers mirroring `Zend/zend_operators.c`
//! (`_is_numeric_string_ex`) and libc `strtol`/`atoi` as PHP uses them.
//!
//! The runtime crate has its own full implementation of PHP's string↔number
//! juggling; this module only contains what the support services need, so
//! that the crate stays independent of the value model.

#![allow(clippy::many_single_char_names, dead_code)]

/// Result of a numeric-string check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumericValue {
    /// An integer that fits in `zend_long`.
    Long(i64),
    /// A float (also used for integers that overflow `zend_long`).
    Double(f64),
}

/// Detailed outcome of scanning a numeric prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NumericScan {
    /// The parsed value.
    pub value: NumericValue,
    /// Integer-looking string that did not fit in a `zend_long`.
    pub overflow: bool,
    /// Non-whitespace data follows the number.
    pub trailing_data: bool,
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Scans `s` the way `_is_numeric_string_ex(…, lval, dval, allow_errors, …)`
/// does when both output pointers are given.  Returns `None` when the string
/// is not numeric at all (or has trailing data and `allow_errors` is false).
pub(crate) fn scan_numeric(s: &[u8], allow_errors: bool) -> Option<NumericScan> {
    if s.is_empty() {
        return None;
    }
    let mut start = 0;
    while start < s.len() && is_ws(s[start]) {
        start += 1;
    }
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut p = start;
    let neg = at(p) == b'-';
    if neg || at(p) == b'+' {
        p += 1;
    }
    let int_start = p;
    let mut is_double = false;
    if at(p).is_ascii_digit() {
        while at(p).is_ascii_digit() {
            p += 1;
        }
        if at(p) == b'.' {
            is_double = true;
            p += 1;
            while at(p).is_ascii_digit() {
                p += 1;
            }
        }
    } else if at(p) == b'.' && at(p + 1).is_ascii_digit() {
        is_double = true;
        p += 1;
        while at(p).is_ascii_digit() {
            p += 1;
        }
    } else {
        return None;
    }
    // Exponent.
    if matches!(at(p), b'e' | b'E') {
        let mut e = p + 1;
        if matches!(at(e), b'-' | b'+') {
            e += 1;
        }
        if at(e).is_ascii_digit() {
            is_double = true;
            p = e;
            while at(p).is_ascii_digit() {
                p += 1;
            }
        }
    }
    let num_end = p;
    let mut q = p;
    while q < s.len() && is_ws(s[q]) {
        q += 1;
    }
    let trailing_data = q != s.len();
    if trailing_data && !allow_errors {
        return None;
    }
    let text = &s[start..num_end];
    if !is_double {
        // Integer: check for overflow.
        let digits = &s[int_start..num_end];
        let magnitude = digits.iter().try_fold(0u128, |acc, &d| {
            let v = acc * 10 + u128::from(d - b'0');
            (v <= u128::from(u64::MAX) + 1).then_some(v)
        });
        let limit = if neg { 1u128 << 63 } else { (1u128 << 63) - 1 };
        match magnitude {
            Some(m) if m <= limit => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let v = if neg {
                    (m as u64).wrapping_neg() as i64
                } else {
                    m as i64
                };
                return Some(NumericScan {
                    value: NumericValue::Long(v),
                    overflow: false,
                    trailing_data,
                });
            }
            _ => {
                let d = parse_f64(text);
                return Some(NumericScan {
                    value: NumericValue::Double(d),
                    overflow: true,
                    trailing_data,
                });
            }
        }
    }
    Some(NumericScan {
        value: NumericValue::Double(parse_f64(text)),
        overflow: false,
        trailing_data,
    })
}

/// Parses an ASCII float literal that is already known to be well formed.
pub(crate) fn parse_f64(text: &[u8]) -> f64 {
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// `is_numeric_string(str, len, &lval, &dval, false)`: returns the numeric
/// value of a fully numeric string (leading and trailing whitespace allowed).
/// Integers that overflow `zend_long` are returned as [`NumericValue::Double`].
pub fn is_numeric_str(s: &[u8]) -> Option<NumericValue> {
    scan_numeric(s, false).map(|r| r.value)
}

/// `is_numeric_string(str, len, NULL, NULL, false) == IS_LONG`.
///
/// Without output pointers PHP keeps counting digits past the `zend_long`
/// range and only compares 19-digit strings against `LONG_MIN`, so very long
/// digit strings are (surprisingly) classified as integers.
pub(crate) fn is_numeric_long_type_only(s: &[u8]) -> bool {
    let Some(scan) = scan_numeric(s, false) else {
        return false;
    };
    match scan.value {
        NumericValue::Long(_) => true,
        NumericValue::Double(_) if scan.overflow => {
            // Count significant digits (after sign and leading zeros).
            let mut start = 0;
            while start < s.len() && is_ws(s[start]) {
                start += 1;
            }
            let neg = s.get(start) == Some(&b'-');
            if matches!(s.get(start), Some(b'-' | b'+')) {
                start += 1;
            }
            while s.get(start) == Some(&b'0') {
                start += 1;
            }
            let digits = s[start..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 19 {
                let cmp = s[start..start + 19].cmp(b"9223372036854775808".as_slice());
                cmp == std::cmp::Ordering::Less || (cmp == std::cmp::Ordering::Equal && neg)
            } else {
                digits > 19
            }
        }
        NumericValue::Double(_) => false,
    }
}

/// C `strtol(s, &end, 0)` with 64-bit `long`: skips leading `isspace()`
/// characters, accepts a sign, hex/oct/bin prefixes (`0x`, `0o`, `0b`), and saturates on overflow.
/// Returns the value and the number of bytes consumed (0 when no digits were found).
pub(crate) fn strtol(s: &[u8]) -> (i64, usize) {
    let mut p = 0;
    while p < s.len() && is_ws(s[p]) {
        p += 1;
    }
    let neg = s.get(p) == Some(&b'-');
    if matches!(s.get(p), Some(b'-' | b'+')) {
        p += 1;
    }
    let (base, prefix_len): (i128, usize) =
        if p + 2 <= s.len() && s[p] == b'0' && matches!(s[p + 1], b'x' | b'X') {
            (16, 2)
        } else if p + 2 <= s.len() && s[p] == b'0' && matches!(s[p + 1], b'o' | b'O') {
            (8, 2)
        } else if p + 2 <= s.len() && s[p] == b'0' && matches!(s[p + 1], b'b' | b'B') {
            (2, 2)
        } else {
            (10, 0)
        };
    p += prefix_len;
    let digits_start = p;
    let mut acc: i128 = 0;
    while let Some(&c) = s.get(p) {
        let d = match c {
            b'0'..=b'9' if (c - b'0') < base as u8 => i128::from(c - b'0'),
            b'a'..=b'f' if base == 16 => i128::from(c - b'a' + 10),
            b'A'..=b'F' if base == 16 => i128::from(c - b'A' + 10),
            _ => break,
        };
        acc = (acc * base + d).min(i128::from(u64::MAX) + 1);
        p += 1;
    }
    if p == digits_start {
        return (0, 0);
    }
    let v = if neg { -acc } else { acc };
    #[allow(clippy::cast_possible_truncation)]
    let clamped = v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    (clamped, p)
}

/// `ZEND_ATOL()` — `strtol(s, NULL, 10)`.
#[must_use]
pub fn atol(s: &[u8]) -> i64 {
    strtol(s).0
}

/// C `atoi()` as glibc implements it: `(int) strtol(s, NULL, 10)`.
pub(crate) fn atoi(s: &[u8]) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let v = atol(s) as i32;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_strings() {
        assert_eq!(is_numeric_str(b"1"), Some(NumericValue::Long(1)));
        assert_eq!(is_numeric_str(b" 12 "), Some(NumericValue::Long(12)));
        assert_eq!(is_numeric_str(b"-5"), Some(NumericValue::Long(-5)));
        assert_eq!(is_numeric_str(b"+5"), Some(NumericValue::Long(5)));
        assert_eq!(is_numeric_str(b"1.5"), Some(NumericValue::Double(1.5)));
        assert_eq!(is_numeric_str(b".5"), Some(NumericValue::Double(0.5)));
        assert_eq!(is_numeric_str(b"1."), Some(NumericValue::Double(1.0)));
        assert_eq!(is_numeric_str(b"1e3"), Some(NumericValue::Double(1000.0)));
        assert_eq!(is_numeric_str(b"1e"), None);
        assert_eq!(is_numeric_str(b"."), None);
        assert_eq!(is_numeric_str(b""), None);
        assert_eq!(is_numeric_str(b"abc"), None);
        assert_eq!(is_numeric_str(b"0x1A"), None);
        assert_eq!(is_numeric_str(b"12abc"), None);
        assert_eq!(
            is_numeric_str(b"9223372036854775807"),
            Some(NumericValue::Long(i64::MAX))
        );
        assert_eq!(
            is_numeric_str(b"-9223372036854775808"),
            Some(NumericValue::Long(i64::MIN))
        );
        assert_eq!(
            is_numeric_str(b"9223372036854775808"),
            Some(NumericValue::Double(9_223_372_036_854_775_808.0))
        );
        let scan = scan_numeric(b"12abc", true).unwrap();
        assert_eq!(scan.value, NumericValue::Long(12));
        assert!(scan.trailing_data);
    }

    #[test]
    fn long_type_only() {
        assert!(is_numeric_long_type_only(b"10"));
        assert!(!is_numeric_long_type_only(b"1.0"));
        assert!(!is_numeric_long_type_only(b"9223372036854775808"));
        assert!(is_numeric_long_type_only(b"-9223372036854775808"));
        assert!(is_numeric_long_type_only(b"123456789012345678901234"));
    }

    #[test]
    fn strtol_atoi() {
        assert_eq!(strtol(b"  -12x"), (-12, 5));
        assert_eq!(strtol(b"x"), (0, 0));
        assert_eq!(atol(b"99999999999999999999"), i64::MAX);
        assert_eq!(atoi(b"99999999999999999999"), -1);
        assert_eq!(atoi(b"42"), 42);
        assert_eq!(atoi(b"+7 "), 7);
    }
}
