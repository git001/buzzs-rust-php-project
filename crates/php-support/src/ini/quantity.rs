// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `zend_ini_parse_quantity()`, `zend_ini_parse_uquantity()` and
//! `zend_ini_parse_bool()` from `Zend/zend_ini.c`.

use std::fmt::Write as _;

use crate::numeric::atoi;

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `zend_ini_parse_bool()`: `true`/`yes`/`on` (case-insensitive) or a
/// non-zero `atoi()`.
pub fn parse_bool(s: &[u8]) -> bool {
    s.eq_ignore_ascii_case(b"true")
        || s.eq_ignore_ascii_case(b"yes")
        || s.eq_ignore_ascii_case(b"on")
        || atoi(s) != 0
}

/// `smart_str_append_escaped()`.
pub(crate) fn escape(s: &[u8]) -> String {
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: the length of an existing string"
    )]
    let mut out = String::with_capacity(s.len());
    for &c in s {
        if c < 32 || c == b'\\' || c > 126 {
            out.push('\\');
            match c {
                b'\n' => out.push('n'),
                b'\r' => out.push('r'),
                b'\t' => out.push('t'),
                0x0c => out.push('f'),
                0x0b => out.push('v'),
                b'\\' => out.push('\\'),
                0x1b => out.push('e'),
                _ => {
                    let _ = write!(out, "x{c:02X}");
                }
            }
        } else {
            out.push(char::from(c));
        }
    }
    out
}

/// glibc `strtoull()` on a string that starts with a digit.  Returns the
/// value, the number of bytes consumed and whether it overflowed.
fn strtoull(s: &[u8], base: u32) -> (u64, usize, bool) {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut p = 0;
    let mut base = base;
    if (base == 0 || base == 16)
        && at(0) == b'0'
        && matches!(at(1), b'x' | b'X')
        && at(2).is_ascii_hexdigit()
    {
        p = 2;
        base = 16;
    } else if base == 0 {
        base = if at(0) == b'0' { 8 } else { 10 };
    }
    let start = p;
    let mut acc: u64 = 0;
    let mut overflow = false;
    while let Some(d) = char::from(at(p)).to_digit(base) {
        if p >= s.len() {
            break;
        }
        match acc
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => acc = v,
            None => overflow = true,
        }
        p += 1;
    }
    if p == start {
        return (0, 0, false);
    }
    (if overflow { u64::MAX } else { acc }, p, overflow)
}

/// `zend_ini_consume_quantity_prefix()`.
fn consume_prefix(s: &[u8], base: u32) -> usize {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut p = 0;
    while p < s.len() && is_ws(s[p]) {
        p += 1;
    }
    if matches!(at(p), b'+' | b'-') {
        p += 1;
    }
    if at(p) == b'0' && !at(p + 1).is_ascii_digit() {
        if p + 1 == s.len() {
            return p;
        }
        match at(p + 1) {
            b'x' | b'X' | b'o' | b'O' => p += 2,
            b'b' | b'B' if base != 16 => p += 2,
            _ => {}
        }
    }
    p
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signed {
    Signed,
    Unsigned,
}

/// `zend_ini_parse_quantity_internal()`: returns the (wrapping) value and
/// the error string PHP would build.
#[allow(clippy::too_many_lines)]
fn parse_internal(value: &[u8], signed: Signed) -> (u64, Option<String>) {
    let invalid = || escape(value);
    let mut start = 0;
    let mut end = value.len();
    while start < end && is_ws(value[start]) {
        start += 1;
    }
    while start < end && is_ws(value[end - 1]) {
        end -= 1;
    }
    if start == end {
        return (0, None);
    }
    let s = &value[..end];
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut digits = start;
    let mut is_negative = false;
    if at(digits) == b'+' {
        digits += 1;
    } else if at(digits) == b'-' {
        is_negative = true;
        digits += 1;
    }
    if !at(digits).is_ascii_digit() {
        return (
            0,
            Some(format!(
                "Invalid quantity \"{}\": no valid leading digits, interpreting as \"0\" for backwards compatibility",
                invalid()
            )),
        );
    }
    let mut base = 0;
    let mut skip_prefix_eval = false;
    if at(digits) == b'0' && !at(digits + 1).is_ascii_digit() {
        if digits + 1 == end {
            return (0, None);
        }
        match at(digits + 1) {
            b'g' | b'G' | b'm' | b'M' | b'k' | b'K' => skip_prefix_eval = true,
            b'x' | b'X' => base = 16,
            b'o' | b'O' => base = 8,
            b'b' | b'B' => base = 2,
            c => {
                return (
                    0,
                    Some(format!(
                        "Invalid prefix \"0{}\", interpreting as \"0\" for backwards compatibility",
                        char::from(c)
                    )),
                );
            }
        }
        if !skip_prefix_eval {
            digits += 2;
            if digits == end || consume_prefix(&s[digits..], base) != 0 {
                return (
                    0,
                    Some(format!(
                        "Invalid quantity \"{}\": no digits after base prefix, interpreting as \"0\" for backwards compatibility",
                        invalid()
                    )),
                );
            }
        }
    }
    let (mut retval, consumed, range_err) = strtoull(&s[digits..], base);
    let digits_end_raw = digits + consumed;
    let mut overflow = false;
    if range_err {
        overflow = true;
    } else if signed == Signed::Unsigned {
        if is_negative {
            if retval == 1 && digits_end_raw == end {
                retval = u64::MAX;
            } else {
                overflow = true;
            }
        }
    } else if is_negative && retval == 1u64 << 63 {
        retval = retval.wrapping_neg();
    } else if retval > i64::MAX as u64 {
        overflow = true;
    } else if is_negative {
        retval = retval.wrapping_neg();
    }
    if consumed == 0 {
        return (
            0,
            Some(format!(
                "Invalid quantity \"{}\": no valid leading digits, interpreting as \"0\" for backwards compatibility",
                invalid()
            )),
        );
    }
    let mut digits_end = digits_end_raw;
    while digits_end < end && is_ws(s[digits_end]) {
        digits_end += 1;
    }
    if digits_end != end {
        let last = s[end - 1];
        let factor: u64 = match last {
            b'g' | b'G' => 1 << 30,
            b'm' | b'M' => 1 << 20,
            b'k' | b'K' => 1 << 10,
            _ => {
                return (
                    retval,
                    Some(format!(
                        "Invalid quantity \"{}\": unknown multiplier \"{}\", interpreting as \"{}\" for backwards compatibility",
                        invalid(),
                        escape(&[last]),
                        escape(&s[..digits_end])
                    )),
                );
            }
        };
        if !overflow {
            overflow = if signed == Signed::Signed {
                let sv = i64::from_ne_bytes(retval.to_ne_bytes());
                let f = i64::from_ne_bytes(factor.to_ne_bytes());
                if sv > 0 {
                    sv > i64::MAX / f
                } else {
                    sv < i64::MIN / f
                }
            } else {
                retval > u64::MAX / factor
            };
        }
        retval = retval.wrapping_mul(factor);
        if digits_end != end - 1 {
            return (
                retval,
                Some(format!(
                    "Invalid quantity \"{}\", interpreting as \"{}{}\" for backwards compatibility",
                    invalid(),
                    escape(&s[..digits_end]),
                    escape(&[last])
                )),
            );
        }
    }
    if overflow {
        return (
            retval,
            Some(format!(
                "Invalid quantity \"{}\": value is out of range, using overflow result for backwards compatibility",
                invalid()
            )),
        );
    }
    (retval, None)
}

/// `zend_ini_parse_quantity()`: parses `"128M"`, `"0x10K"`, `"-1"`, …
/// Returns the value and, when the input is not well formed, the message
/// PHP warns with (the value is still what PHP uses).
pub fn parse_quantity(value: &[u8]) -> (i64, Option<String>) {
    let (v, e) = parse_internal(value, Signed::Signed);
    (i64::from_ne_bytes(v.to_ne_bytes()), e)
}

/// `zend_ini_parse_uquantity()` (`"-1"` means `u64::MAX`).
pub fn parse_uquantity(value: &[u8]) -> (u64, Option<String>) {
    parse_internal(value, Signed::Unsigned)
}

/// The warning `zend_ini_parse_quantity_warn()` raises for `setting`.
pub fn quantity_warning(setting: &str, errstr: &str) -> String {
    format!("Invalid \"{setting}\" setting. {errstr}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> (i64, Option<String>) {
        parse_quantity(s.as_bytes())
    }

    #[test]
    fn valid_quantities() {
        assert_eq!(q("128M"), (128 << 20, None));
        assert_eq!(q("1G"), (1 << 30, None));
        assert_eq!(q("2k"), (2048, None));
        assert_eq!(q(" 42 "), (42, None));
        assert_eq!(q("-1"), (-1, None));
        assert_eq!(q("0"), (0, None));
        assert_eq!(q(""), (0, None));
        assert_eq!(q("0x10"), (16, None));
        assert_eq!(q("0o17"), (15, None));
        assert_eq!(q("0b101"), (5, None));
        assert_eq!(q("0755"), (493, None));
        assert_eq!(q("0K"), (0, None));
        assert_eq!(q("1 M"), (1 << 20, None));
        assert_eq!(q("+5"), (5, None));
        assert_eq!(q("-9223372036854775808"), (i64::MIN, None));
    }

    #[test]
    fn invalid_quantities() {
        // Messages verified with `php -r 'ini_parse_quantity(...)'`.
        assert_eq!(
            q("abc"),
            (0, Some("Invalid quantity \"abc\": no valid leading digits, interpreting as \"0\" for backwards compatibility".into()))
        );
        assert_eq!(
            q("12X"),
            (12, Some("Invalid quantity \"12X\": unknown multiplier \"X\", interpreting as \"12\" for backwards compatibility".into()))
        );
        assert_eq!(
            q("1MB"),
            (1, Some("Invalid quantity \"1MB\": unknown multiplier \"B\", interpreting as \"1\" for backwards compatibility".into()))
        );
        assert_eq!(
            q("1KM"),
            (
                1 << 20,
                Some(
                    "Invalid quantity \"1KM\", interpreting as \"1M\" for backwards compatibility"
                        .into()
                )
            )
        );
        assert_eq!(
            q("0z"),
            (
                0,
                Some(
                    "Invalid prefix \"0z\", interpreting as \"0\" for backwards compatibility"
                        .into()
                )
            )
        );
        assert_eq!(
            q("0x"),
            (0, Some("Invalid quantity \"0x\": no digits after base prefix, interpreting as \"0\" for backwards compatibility".into()))
        );
        assert_eq!(
            q("0x-1"),
            (0, Some("Invalid quantity \"0x-1\": no digits after base prefix, interpreting as \"0\" for backwards compatibility".into()))
        );
        assert_eq!(
            q("99999999999999999999"),
            (-1, Some("Invalid quantity \"99999999999999999999\": value is out of range, using overflow result for backwards compatibility".into()))
        );
        assert_eq!(
            q("9223372036854775807K"),
            (-1024, Some("Invalid quantity \"9223372036854775807K\": value is out of range, using overflow result for backwards compatibility".into()))
        );
        assert_eq!(
            q("1\n2"),
            (1, Some("Invalid quantity \"1\\n2\": unknown multiplier \"2\", interpreting as \"1\\n\" for backwards compatibility".into()))
        );
    }

    #[test]
    fn unsigned() {
        assert_eq!(parse_uquantity(b"-1"), (u64::MAX, None));
        assert_eq!(parse_uquantity(b"128M"), (128 << 20, None));
        assert!(parse_uquantity(b"-2").1.unwrap().contains("out of range"));
    }

    #[test]
    fn bools() {
        assert!(parse_bool(b"On"));
        assert!(parse_bool(b"YES"));
        assert!(parse_bool(b"true"));
        assert!(parse_bool(b"1"));
        assert!(parse_bool(b"2abc"));
        assert!(!parse_bool(b"off"));
        assert!(!parse_bool(b""));
        assert!(!parse_bool(b"0"));
        assert!(!parse_bool(b"tru"));
    }

    #[test]
    fn escaping() {
        assert_eq!(escape(b"a\0b\\\x1b\xff"), "a\\x00b\\\\\\e\\xFF");
    }
}
