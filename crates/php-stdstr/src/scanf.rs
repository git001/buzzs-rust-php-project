// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `sscanf()` — port of `ext/standard/scanf.c` (`ValidateFormat()` and
//! `php_sscanf_internal()`).
//!
//! Both the input and the format are treated as C strings: scanning stops
//! at the first NUL byte, exactly as in PHP.
//!
//! Supported conversions: `%d %D %i %o %x %X %u %f %e %E %g %s %c %[set]
//! %n %%`, with `*` (assignment suppression), XPG3 positional `%N$`,
//! field widths and the ignored size modifiers `l L h`.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use crate::error::{ErrorKind, StdError, StdResult};

/// Maximum XPG index accepted when no variables are passed
/// (`SCAN_MAX_ARGS`).
const SCAN_MAX_ARGS: i64 = 0xFF;

/// A value produced by a conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanValue {
    /// `%d %i %o %x %u %n` (an unsigned value above `PHP_INT_MAX` from `%u`
    /// is returned as a decimal [`ScanValue::Str`] instead).
    Int(i64),
    /// `%f %e %E %g`.
    Float(f64),
    /// `%s %c %[...]`, and large `%u` values.
    Str(Vec<u8>),
}

/// The outcome of [`sscanf`].
#[derive(Debug, Clone, PartialEq)]
pub enum SscanfResult {
    /// No variables were passed (`sscanf($str, $fmt)`): the returned array
    /// with one entry per conversion (`None` → `null` for conversions that
    /// did not happen), or `None` when PHP returns `null` (the input ended
    /// before the first conversion).
    Array(Option<Vec<Option<ScanValue>>>),
    /// Variables were passed by reference (`sscanf($str, $fmt, $a, $b)`):
    /// `values[i]` is what to assign to the i-th variable (`None` → leave
    /// it untouched), and `count` is the return value — the number of
    /// conversions performed, or `-1` if the input ended before the first
    /// conversion.
    Assigned {
        /// One slot per variable.
        values: Vec<Option<ScanValue>>,
        /// Return value.
        count: i64,
    },
}

/// Truncates at the first NUL byte (C string semantics).
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |n| &s[..n])
}

fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// C `isspace()` in the "C" locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn verr(msg: impl Into<String>) -> StdError {
    StdError::new(ErrorKind::ValueError, msg)
}

/// `strtoul(s + i, &end, 10)` on a digit run: (value, end index).  Saturates
/// to `u64::MAX` on overflow like glibc.
fn strtoul_at(s: &[u8], mut i: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(s[i] - b'0')))
            .unwrap_or(u64::MAX);
        i += 1;
    }
    (v, i)
}

/// `ValidateFormat()`: checks the format and returns the number of result
/// slots (`totalSubs`).
#[allow(clippy::too_many_lines)]
fn validate_format(format: &[u8], num_vars: i64) -> StdResult<i64> {
    let mut nassign: Vec<i64> = Vec::new();
    let mut bump = |idx: i64| {
        let idx = usize::try_from(idx).unwrap_or(0);
        if nassign.len() <= idx {
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: SCAN_MAX_ARGS, argument count or format length"
            )]
            nassign.resize(idx + 1, 0);
        }
        nassign[idx] += 1;
    };
    let (mut xpg_size, mut obj_index): (i64, i64) = (0, 0);
    let (mut got_xpg, mut got_sequential) = (false, false);
    let bad_index = |got_xpg: bool| {
        if got_xpg {
            verr("\"%n$\" argument index out of range")
        } else {
            verr("Different numbers of variable names and field specifiers")
        }
    };
    let mixed = || verr("cannot mix \"%\" and \"%n$\" conversion specifiers");

    let mut f = 0usize;
    while at(format, f) != 0 {
        let mut ch = format[f];
        f += 1;
        let mut suppress = false;
        if ch != b'%' {
            continue;
        }
        ch = at(format, f);
        f += 1;
        if ch == b'%' {
            continue;
        }
        if ch == b'*' {
            suppress = true;
            ch = at(format, f);
            f += 1;
        } else if ch.is_ascii_digit() {
            let (v, end) = strtoul_at(format, f - 1);
            if at(format, end) == b'$' {
                // `int value = strtoul(...)`.
                let value = i64::from(v as i32);
                f = end + 1;
                ch = at(format, f);
                f += 1;
                got_xpg = true;
                if got_sequential {
                    return Err(mixed());
                }
                if value < 1 || (num_vars != 0 && value > num_vars) {
                    return Err(bad_index(got_xpg));
                } else if num_vars == 0 {
                    if value > SCAN_MAX_ARGS {
                        return Err(bad_index(got_xpg));
                    }
                    xpg_size = xpg_size.max(value);
                }
                obj_index = value - 1;
            } else {
                got_sequential = true;
                if got_xpg {
                    return Err(mixed());
                }
            }
        } else {
            got_sequential = true;
            if got_xpg {
                return Err(mixed());
            }
        }

        // Width.
        if ch.is_ascii_digit() {
            let (_, end) = strtoul_at(format, f - 1);
            f = end;
            ch = at(format, f);
            f += 1;
        }
        // Size modifier.
        if matches!(ch, b'l' | b'L' | b'h') {
            ch = at(format, f);
            f += 1;
        }
        if !suppress && num_vars != 0 && obj_index >= num_vars {
            return Err(bad_index(got_xpg));
        }
        match ch {
            b'n' | b'd' | b'D' | b'i' | b'o' | b'x' | b'X' | b'u' | b'f' | b'e' | b'E' | b'g'
            | b's' | b'c' => {}
            b'[' => {
                let bad_set = || verr("Unmatched [ in format string");
                if at(format, f) == 0 {
                    return Err(bad_set());
                }
                let mut c = format[f];
                f += 1;
                if c == b'^' {
                    if at(format, f) == 0 {
                        return Err(bad_set());
                    }
                    c = format[f];
                    f += 1;
                }
                if c == b']' {
                    if at(format, f) == 0 {
                        return Err(bad_set());
                    }
                    c = format[f];
                    f += 1;
                }
                while c != b']' {
                    if at(format, f) == 0 {
                        return Err(bad_set());
                    }
                    c = format[f];
                    f += 1;
                }
            }
            0 => {
                // `%c` of NUL ends the C-string message.
                return Err(verr("Bad scan conversion character \""));
            }
            _ => {
                return Err(verr(format!(
                    "Bad scan conversion character \"{}\"",
                    char::from(ch)
                )))
            }
        }
        if !suppress {
            bump(obj_index);
            obj_index += 1;
        }
    }

    let total = if num_vars == 0 {
        if xpg_size != 0 {
            xpg_size
        } else {
            obj_index
        }
    } else {
        num_vars
    };
    for i in 0..usize::try_from(total).unwrap_or(0) {
        let n = nassign.get(i).copied().unwrap_or(0);
        if n > 1 {
            return Err(verr(
                "Variable is assigned by multiple \"%n$\" conversion specifiers",
            ));
        } else if xpg_size == 0 && n == 0 {
            return Err(verr(
                "Variable is not assigned by any conversion specifiers",
            ));
        }
    }
    Ok(total)
}

/// `BuildCharSet()` result: `(exclude, chars, ranges)` with C `char`
/// (signed) semantics for range comparisons.
struct CharSet {
    exclude: bool,
    chars: Vec<u8>,
    ranges: Vec<(i8, i8)>,
}

impl CharSet {
    /// Parses the set starting right after `[`; returns the set and the
    /// format position after the closing `]`.
    fn build(format: &[u8], start: usize) -> (Self, usize) {
        let mut set = Self {
            exclude: false,
            chars: Vec::new(),
            ranges: Vec::new(),
        };
        let mut fmt = start;
        if at(format, fmt) == b'^' {
            set.exclude = true;
            fmt += 1;
        }
        // ch = format++
        let mut ch = fmt;
        fmt += 1;
        let mut range_start = at(format, ch);
        if at(format, ch) == b']' || at(format, ch) == b'-' {
            set.chars.push(at(format, ch));
            ch = fmt;
            fmt += 1;
        }
        while at(format, ch) != b']' {
            if at(format, ch) == 0 {
                // Unreachable after validation; stop defensively.
                return (set, fmt);
            }
            if at(format, fmt) == b'-' {
                range_start = at(format, ch);
            } else if at(format, ch) == b'-' {
                if at(format, fmt) == b']' {
                    set.chars.push(range_start);
                    set.chars.push(b'-');
                } else {
                    ch = fmt;
                    fmt += 1;
                    let s = range_start as i8;
                    let e = at(format, ch) as i8;
                    set.ranges.push(if s < e { (s, e) } else { (e, s) });
                }
            } else {
                set.chars.push(at(format, ch));
            }
            ch = fmt;
            fmt += 1;
        }
        (set, fmt)
    }

    /// `CharInSet()`.
    fn contains(&self, c: u8) -> bool {
        let sc = c as i8;
        let m = self.chars.contains(&c) || self.ranges.iter().any(|&(s, e)| s <= sc && sc <= e);
        m != self.exclude
    }
}

/// glibc `strtol`/`strtoul` on the scanned integer buffer (sign, optional
/// `0x` prefix for base 16, digits).  Returns the value as `u64` bits plus
/// whether it was the signed form.
fn parse_int(buf: &[u8], base: u32, unsigned: bool) -> i64 {
    let mut i = 0;
    let mut neg = false;
    if i < buf.len() && matches!(buf[i], b'+' | b'-') {
        neg = buf[i] == b'-';
        i += 1;
    }
    if base == 16
        && i + 1 < buf.len()
        && buf[i] == b'0'
        && matches!(buf[i + 1], b'x' | b'X')
        && buf.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
    }
    let mut mag: u64 = 0;
    let mut overflow = false;
    while i < buf.len() {
        let Some(d) = char::from(buf[i]).to_digit(base) else {
            break;
        };
        match mag
            .checked_mul(u64::from(base))
            .and_then(|x| x.checked_add(u64::from(d)))
        {
            Some(v) => mag = v,
            None => overflow = true,
        }
        i += 1;
    }
    if unsigned {
        if overflow {
            return -1; // ULONG_MAX
        }
        let v = if neg { mag.wrapping_neg() } else { mag };
        v as i64
    } else if neg {
        if overflow || mag > 1u64 << 63 {
            i64::MIN
        } else {
            (mag as i64).wrapping_neg()
        }
    } else if overflow || mag > i64::MAX as u64 {
        i64::MAX
    } else {
        mag as i64
    }
}

/// `zend_strtod()` on the scanned float buffer (always a valid prefix).
fn parse_float(buf: &[u8]) -> f64 {
    let s = std::str::from_utf8(buf).unwrap_or("0");
    // Rust does not accept a trailing `.` without digits after a sign-only
    // mantissa, but the scanner never produces such buffers; fall back to 0.
    s.parse::<f64>().unwrap_or(0.0)
}

/// Scanning operation selected by the conversion character.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Int,
    Float,
    Str,
    Set,
}

const FLAG_SIGNOK: u32 = 0x10;
const FLAG_NODIGITS: u32 = 0x20;
const FLAG_NOZERO: u32 = 0x40;
const FLAG_XOK: u32 = 0x80;
const FLAG_PTOK: u32 = 0x100;
const FLAG_EXPOK: u32 = 0x200;

/// `sscanf(string $string, string $format, mixed &...$vars)`.
///
/// `num_vars` is the number of by-reference variables passed (0 for the
/// array-returning form).  Format errors are thrown as `ValueError`s.
///
/// The VM must coerce `$string` and `$format` to strings (zpp) and, in the
/// by-reference form, assign each `Some` slot of
/// [`SscanfResult::Assigned::values`] to the corresponding reference.
#[allow(clippy::too_many_lines)]
pub fn sscanf(input: &[u8], format: &[u8], num_vars: usize) -> StdResult<SscanfResult> {
    let input = c_str(input);
    let format = c_str(format);
    let num_vars_i = i64::try_from(num_vars).unwrap_or(i64::MAX);
    let total_vars = validate_format(format, num_vars_i)?;
    let mut slots: Vec<Option<ScanValue>> = vec![None; usize::try_from(total_vars).unwrap_or(0)];
    // C `int objIndex`.
    let mut obj_index: i32 = 0;
    let mut assign = |obj_index: &mut i32, v: ScanValue| {
        if let Some(slot) = usize::try_from(*obj_index)
            .ok()
            .and_then(|i| slots.get_mut(i))
        {
            *slot = Some(v);
        }
        *obj_index = obj_index.wrapping_add(1);
    };

    let mut s = 0usize; // position in input
    let mut f = 0usize; // position in format
    let mut nconversions: i64 = 0;
    let mut underflow = false;

    'scan: while at(format, f) != 0 {
        let mut ch = format[f];
        f += 1;
        let mut suppress = false;

        if is_space(ch) {
            while is_space(at(input, s)) {
                s += 1;
            }
            continue;
        }
        if ch != b'%' || at(format, f) == b'%' {
            if ch == b'%' {
                f += 1;
            }
            // literal:
            if at(input, s) == 0 {
                underflow = true;
                break 'scan;
            }
            let sch = input[s];
            s += 1;
            if ch != sch {
                break 'scan;
            }
            continue;
        }
        ch = at(format, f);
        f += 1;

        if ch == b'*' {
            suppress = true;
            ch = at(format, f);
            f += 1;
        } else if ch.is_ascii_digit() {
            let (v, end) = strtoul_at(format, f - 1);
            if at(format, end) == b'$' {
                f = end + 1;
                ch = at(format, f);
                f += 1;
                // `objIndex = varStart + value - 1` truncated to `int`.
                obj_index = v.wrapping_sub(1) as i32;
            }
        }

        let mut width: u64 = 0;
        if ch.is_ascii_digit() {
            let (v, end) = strtoul_at(format, f - 1);
            width = v;
            f = end;
            ch = at(format, f);
            f += 1;
        }
        if matches!(ch, b'l' | b'L' | b'h') {
            ch = at(format, f);
            f += 1;
        }

        let mut noskip = false;
        let mut unsigned = false;
        let mut base: u32 = 10;
        let op = match ch {
            b'n' => {
                if !suppress {
                    assign(&mut obj_index, ScanValue::Int(s as i64));
                }
                nconversions += 1;
                continue;
            }
            b'd' | b'D' => Op::Int,
            b'i' => {
                base = 0;
                Op::Int
            }
            b'o' => {
                base = 8;
                Op::Int
            }
            b'x' | b'X' => {
                base = 16;
                Op::Int
            }
            b'u' => {
                unsigned = true;
                Op::Int
            }
            b'f' | b'e' | b'E' | b'g' => Op::Float,
            b'c' => {
                noskip = true;
                if width == 0 {
                    width = 1;
                }
                Op::Str
            }
            b'[' => {
                noskip = true;
                Op::Set
            }
            _ => Op::Str, // 's' (others rejected by validation)
        };

        if at(input, s) == 0 {
            underflow = true;
            break 'scan;
        }
        if !noskip {
            while at(input, s) != 0 && is_space(input[s]) {
                s += 1;
            }
            if at(input, s) == 0 {
                underflow = true;
                break 'scan;
            }
        }

        match op {
            Op::Str => {
                if width == 0 {
                    width = u64::MAX;
                }
                let mut end = s;
                while at(input, end) != 0 {
                    if is_space(input[end]) {
                        break;
                    }
                    end += 1;
                    width -= 1;
                    if width == 0 {
                        break;
                    }
                }
                if !suppress {
                    assign(&mut obj_index, ScanValue::Str(input[s..end].to_vec()));
                }
                s = end;
            }
            Op::Set => {
                if width == 0 {
                    width = u64::MAX;
                }
                let (set, nf) = CharSet::build(format, f);
                f = nf;
                let mut end = s;
                while at(input, end) != 0 {
                    if !set.contains(input[end]) {
                        break;
                    }
                    end += 1;
                    width -= 1;
                    if width == 0 {
                        break;
                    }
                }
                if s == end {
                    break 'scan;
                }
                if !suppress {
                    assign(&mut obj_index, ScanValue::Str(input[s..end].to_vec()));
                }
                s = end;
            }
            Op::Int => {
                if width == 0 || width > 63 {
                    width = 63;
                }
                let mut flags = FLAG_SIGNOK | FLAG_NODIGITS | FLAG_NOZERO;
                let mut buf: Vec<u8> = Vec::new();
                while width > 0 {
                    let c = at(input, s);
                    let add = match c {
                        b'0' => {
                            if base == 16 {
                                flags |= FLAG_XOK;
                            }
                            if base == 0 {
                                base = 8;
                                flags |= FLAG_XOK;
                            }
                            if flags & FLAG_NOZERO != 0 {
                                flags &= !(FLAG_SIGNOK | FLAG_NODIGITS | FLAG_NOZERO);
                            } else {
                                flags &= !(FLAG_SIGNOK | FLAG_XOK | FLAG_NODIGITS);
                            }
                            true
                        }
                        b'1'..=b'7' => {
                            if base == 0 {
                                base = 10;
                            }
                            flags &= !(FLAG_SIGNOK | FLAG_XOK | FLAG_NODIGITS);
                            true
                        }
                        b'8' | b'9' => {
                            if base == 0 {
                                base = 10;
                            }
                            if base <= 8 {
                                false
                            } else {
                                flags &= !(FLAG_SIGNOK | FLAG_XOK | FLAG_NODIGITS);
                                true
                            }
                        }
                        b'A'..=b'F' | b'a'..=b'f' => {
                            if base <= 10 {
                                false
                            } else {
                                flags &= !(FLAG_SIGNOK | FLAG_XOK | FLAG_NODIGITS);
                                true
                            }
                        }
                        b'+' | b'-' if flags & FLAG_SIGNOK != 0 => {
                            flags &= !FLAG_SIGNOK;
                            true
                        }
                        b'x' | b'X' if flags & FLAG_XOK != 0 && buf.len() == 1 => {
                            base = 16;
                            flags &= !FLAG_XOK;
                            true
                        }
                        _ => false,
                    };
                    if !add {
                        break;
                    }
                    buf.push(c);
                    s += 1;
                    if at(input, s) == 0 {
                        break;
                    }
                    width -= 1;
                }
                if flags & FLAG_NODIGITS != 0 {
                    if at(input, s) == 0 {
                        underflow = true;
                    }
                    break 'scan;
                } else if matches!(buf.last(), Some(b'x' | b'X')) {
                    buf.pop();
                    s -= 1;
                }
                if !suppress {
                    let value = parse_int(&buf, base, unsigned);
                    if unsigned && value < 0 {
                        assign(
                            &mut obj_index,
                            ScanValue::Str((value as u64).to_string().into_bytes()),
                        );
                    } else {
                        assign(&mut obj_index, ScanValue::Int(value));
                    }
                }
            }
            Op::Float => {
                if width == 0 || width > 63 {
                    width = 63;
                }
                let mut flags = FLAG_SIGNOK | FLAG_NODIGITS | FLAG_PTOK | FLAG_EXPOK;
                let mut buf: Vec<u8> = Vec::new();
                while width > 0 {
                    let c = at(input, s);
                    let add = match c {
                        b'0'..=b'9' => {
                            flags &= !(FLAG_SIGNOK | FLAG_NODIGITS);
                            true
                        }
                        b'+' | b'-' if flags & FLAG_SIGNOK != 0 => {
                            flags &= !FLAG_SIGNOK;
                            true
                        }
                        b'.' if flags & FLAG_PTOK != 0 => {
                            flags &= !(FLAG_SIGNOK | FLAG_PTOK);
                            true
                        }
                        b'e' | b'E' if flags & (FLAG_NODIGITS | FLAG_EXPOK) == FLAG_EXPOK => {
                            flags =
                                (flags & !(FLAG_EXPOK | FLAG_PTOK)) | FLAG_SIGNOK | FLAG_NODIGITS;
                            true
                        }
                        _ => false,
                    };
                    if !add {
                        break;
                    }
                    buf.push(c);
                    s += 1;
                    if at(input, s) == 0 {
                        break;
                    }
                    width -= 1;
                }
                if flags & FLAG_NODIGITS != 0 {
                    if flags & FLAG_EXPOK != 0 {
                        if at(input, s) == 0 {
                            underflow = true;
                        }
                        break 'scan;
                    }
                    // Bad exponent: back up over 'e' and maybe a sign.
                    let last = buf.pop();
                    s -= 1;
                    if !matches!(last, Some(b'e' | b'E')) {
                        buf.pop();
                        s -= 1;
                    }
                }
                if !suppress {
                    assign(&mut obj_index, ScanValue::Float(parse_float(&buf)));
                }
            }
        }
        nconversions += 1;
    }

    let eof = underflow && nconversions == 0;
    Ok(if num_vars == 0 {
        SscanfResult::Array(if eof { None } else { Some(slots) })
    } else {
        SscanfResult::Assigned {
            values: slots,
            count: if eof { -1 } else { nconversions },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(input: &str, format: &str) -> Option<Vec<Option<ScanValue>>> {
        match sscanf(input.as_bytes(), format.as_bytes(), 0) {
            Ok(SscanfResult::Array(a)) => a,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn basic() {
        assert_eq!(
            arr("age: 25 name: bob", "age: %d name: %s"),
            Some(vec![
                Some(ScanValue::Int(25)),
                Some(ScanValue::Str(b"bob".to_vec()))
            ])
        );
        assert_eq!(arr("", "%d"), None);
        assert_eq!(arr("x", "%d"), Some(vec![None]));
        assert_eq!(arr("0x1A", "%i"), Some(vec![Some(ScanValue::Int(26))]));
        assert_eq!(
            arr("-1", "%u"),
            Some(vec![Some(ScanValue::Str(b"18446744073709551615".to_vec()))])
        );
        assert_eq!(
            arr("1.5e3x", "%f"),
            Some(vec![Some(ScanValue::Float(1500.0))])
        );
        assert_eq!(
            arr("abc123", "%[a-z]"),
            Some(vec![Some(ScanValue::Str(b"abc".to_vec()))])
        );
    }

    #[test]
    fn by_ref() {
        assert_eq!(
            sscanf(b"12 apples", b"%d %s", 2),
            Ok(SscanfResult::Assigned {
                values: vec![
                    Some(ScanValue::Int(12)),
                    Some(ScanValue::Str(b"apples".to_vec()))
                ],
                count: 2
            })
        );
        assert_eq!(
            sscanf(b"", b"%d", 1),
            Ok(SscanfResult::Assigned {
                values: vec![None],
                count: -1
            })
        );
    }

    #[test]
    fn errors() {
        let e = |f: &str, n| {
            sscanf(b"x", f.as_bytes(), n)
                .map(|_| ())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            e("%d", 2),
            "Variable is not assigned by any conversion specifiers"
        );
        assert_eq!(
            e("%d%d", 1),
            "Different numbers of variable names and field specifiers"
        );
        assert_eq!(e("%G", 0), "Bad scan conversion character \"G\"");
        assert_eq!(e("%[a", 0), "Unmatched [ in format string");
        assert_eq!(
            e("%1$s%s", 0),
            "cannot mix \"%\" and \"%n$\" conversion specifiers"
        );
        assert_eq!(e("%2$s", 1), "\"%n$\" argument index out of range");
    }
}
