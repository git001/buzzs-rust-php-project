// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `pack()` / `unpack()` — port of `ext/standard/pack.c`.
//!
//! The port assumes the reference platform PHP runs on in practice:
//! little-endian, 64-bit `zend_long`, 32-bit `int`, IEEE-754 `float` and
//! `double`.  So `s`/`S`/`l`/`L`/`q`/`Q`/`i`/`I`/`f`/`d` without an
//! endianness modifier are little-endian, and the 64-bit codes
//! (`q Q J P`) are always available.
//!
//! Format grammar (both functions): a code letter, an optional endianness
//! modifier `<` (little) or `>` (big) — only allowed on `s S l L q Q f d` —
//! and an optional repeater (decimal count or `*`).  `unpack()` codes are
//! additionally followed by an element name and separated by `/`.
//!
//! Message texts: the `%c` in PHP's messages is a raw byte; since
//! [`Notice`]/[`StdError`] messages are `String`s, a byte ≥ 0x80 is rendered
//! as the Unicode scalar with the same value (Latin-1 mapping).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::similar_names
)]

use std::collections::HashMap;

use crate::error::{ErrorKind, Notice, Notices, StdError, StdResult};

const INT_MAX: i64 = i32::MAX as i64;

/// The coercions `pack()` applies to its variadic arguments.
///
/// The VM implements this for its own value type so that the conversions
/// (and any diagnostics or exceptions they raise) are exactly PHP's:
///
/// * [`PackValue::to_long`] — `zval_get_long()` (used by `c C s S n v i I
///   l L N V q Q J P`);
/// * [`PackValue::to_double`] — `zval_get_double()` (used by `f g G d e E`);
/// * [`PackValue::to_bytes`] — `zval_get_string()` /
///   `try_convert_to_string()` (used by `a A Z h H`); may fail (e.g. an
///   object without `__toString`).
///
/// For a string code each argument is converted to a string at most once.
pub trait PackValue {
    /// `zval_get_long()`.
    fn to_long(&self) -> i64;
    /// `zval_get_double()`.
    fn to_double(&self) -> f64;
    /// `zval_get_string()`.
    fn to_bytes(&self) -> StdResult<Vec<u8>>;
}

/// A simple scalar argument for [`pack`], with PHP's scalar conversions
/// (using the default `precision=14` for float-to-string).  Arrays and
/// objects need the VM's own [`PackValue`] implementation.
#[derive(Debug, Clone, PartialEq)]
pub enum PackArg {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// integer
    Int(i64),
    /// float
    Float(f64),
    /// string
    Str(Vec<u8>),
}

/// `zend_dval_to_lval_silent()`: modular conversion for out-of-range
/// values, 0 for non-finite ones.
fn dval_to_lval(d: f64) -> i64 {
    if !d.is_finite() {
        return 0;
    }
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d) {
        return d as i64;
    }
    // zend_dval_to_lval_slow(): two's complement modular arithmetic.
    let two_pow_64 = 18_446_744_073_709_551_616.0_f64;
    let mut dmod = d % two_pow_64;
    if dmod < 0.0 {
        // we're going to make this number positive; call ourselves recursively
        // if the result is -zend_long_MIN
        dmod += two_pow_64;
        if dmod >= two_pow_64 {
            return 0;
        }
    }
    if dmod > 9_223_372_036_854_775_807.0 {
        dmod -= two_pow_64;
    }
    dmod as i64
}

/// Leading numeric prefix of a string (`is_numeric_string` with
/// `allow_errors`): returns the value as an int when it is an integer
/// literal that fits, otherwise as a float.
fn numeric_prefix(s: &[u8]) -> Option<Result<i64, f64>> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let start = i;
    if i < s.len() && matches!(s[i], b'+' | b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - digits_start;
    let mut is_float = false;
    if i < s.len() && s[i] == b'.' {
        let j = i + 1;
        let mut k = j;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        if int_digits > 0 || k > j {
            is_float = true;
            i = k;
        }
    }
    if int_digits == 0 && !is_float {
        return None;
    }
    if i < s.len() && matches!(s[i], b'e' | b'E') {
        let mut k = i + 1;
        if k < s.len() && matches!(s[k], b'+' | b'-') {
            k += 1;
        }
        if k < s.len() && s[k].is_ascii_digit() {
            while k < s.len() && s[k].is_ascii_digit() {
                k += 1;
            }
            is_float = true;
            i = k;
        }
    }
    let text = std::str::from_utf8(&s[start..i]).ok()?;
    if !is_float {
        if let Ok(v) = text.parse::<i64>() {
            return Some(Ok(v));
        }
    }
    text.parse::<f64>().ok().map(Err)
}

/// `%.*G` with `precision` significant digits, PHP style (`php_gcvt`).
fn php_gcvt(d: f64, precision: usize) -> String {
    if d.is_nan() {
        return "NAN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let e = format!("{:.*e}", precision.saturating_sub(1), d.abs());
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let decpt = exp + 1;
    let mut s = String::new();
    if d < 0.0 {
        s.push('-');
    }
    if decpt < -3 || decpt > precision as i32 {
        s.push_str(&digits[..1]);
        s.push('.');
        if digits.len() > 1 {
            s.push_str(&digits[1..]);
        } else {
            s.push('0');
        }
        s.push('E');
        s.push(if exp < 0 { '-' } else { '+' });
        s.push_str(&exp.abs().to_string());
    } else if decpt <= 0 {
        s.push_str("0.");
        for _ in 0..-decpt {
            s.push('0');
        }
        s.push_str(digits);
    } else {
        let dp = decpt as usize;
        if digits.len() <= dp {
            s.push_str(digits);
            for _ in digits.len()..dp {
                s.push('0');
            }
        } else {
            s.push_str(&digits[..dp]);
            s.push('.');
            s.push_str(&digits[dp..]);
        }
    }
    s
}

impl PackValue for PackArg {
    fn to_long(&self) -> i64 {
        match self {
            Self::Null => 0,
            Self::Bool(b) => i64::from(*b),
            Self::Int(i) => *i,
            Self::Float(f) => dval_to_lval(*f),
            Self::Str(s) => match numeric_prefix(s) {
                None => 0,
                Some(Ok(i)) => i,
                // zend_dval_to_lval_cap() for numeric strings.
                Some(Err(f)) => {
                    if f.is_nan() {
                        0
                    } else if f >= 9_223_372_036_854_775_808.0 {
                        i64::MAX
                    } else if f < -9_223_372_036_854_775_808.0 {
                        i64::MIN
                    } else {
                        f as i64
                    }
                }
            },
        }
    }

    fn to_double(&self) -> f64 {
        match self {
            Self::Null => 0.0,
            Self::Bool(b) => f64::from(u8::from(*b)),
            Self::Int(i) => *i as f64,
            Self::Float(f) => *f,
            Self::Str(s) => match numeric_prefix(s) {
                None => 0.0,
                Some(Ok(i)) => i as f64,
                Some(Err(f)) => f,
            },
        }
    }

    fn to_bytes(&self) -> StdResult<Vec<u8>> {
        Ok(match self {
            Self::Null | Self::Bool(false) => Vec::new(),
            Self::Bool(true) => b"1".to_vec(),
            Self::Int(i) => i.to_string().into_bytes(),
            Self::Float(f) => php_gcvt(*f, 14).into_bytes(),
            Self::Str(s) => s.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
    None,
}

fn ch(b: u8) -> char {
    char::from(b)
}

/// PHP formats messages into C strings, so a `%c` of NUL ends the text.
fn c_msg(mut msg: String) -> String {
    if let Some(n) = msg.find('\0') {
        msg.truncate(n);
    }
    msg
}

fn value_error(msg: String) -> StdError {
    StdError::new(ErrorKind::ValueError, c_msg(msg))
}

fn not_supported(code: u8) -> StdError {
    value_error(format!(
        "Endianness modifier is not supported for format code '{}'",
        ch(code)
    ))
}

fn inherent(endian: Endian, code: u8) -> StdError {
    value_error(format!(
        "Endianness modifier '{}' cannot be applied to format code '{}' which already has inherent endianness",
        if endian == Endian::Little { '<' } else { '>' },
        ch(code)
    ))
}

/// glibc `strtol(s, NULL, 10)` on the leading digit run, saturating.
fn strtol_digits(s: &[u8]) -> (i64, bool) {
    let mut v: i64 = 0;
    let mut overflow = false;
    for &c in s.iter().take_while(|c| c.is_ascii_digit()) {
        if let Some(n) = v
            .checked_mul(10)
            .and_then(|x| x.checked_add(i64::from(c - b'0')))
        {
            v = n;
        } else {
            overflow = true;
            v = i64::MAX;
        }
    }
    (v, overflow)
}

/// Writes the low `size` bytes of `v` in the given byte order (`php_pack`).
fn put_int(out: &mut [u8], pos: usize, v: i64, size: usize, big: bool) {
    let bytes = if big {
        let b = (v as u64).to_be_bytes();
        b[8 - size..].to_vec()
    } else {
        (v as u64).to_le_bytes()[..size].to_vec()
    };
    out[pos..pos + size].copy_from_slice(&bytes);
}

/// `pack(string $format, mixed ...$values): string`.
///
/// Returns the packed binary string.  Warnings (`'*' ignored`, `N
/// arguments unused`, `outside of string`, `not enough characters in
/// string`, `illegal hex digit`) are pushed to `notices` in PHP's order;
/// format errors are returned as `ValueError`s.
pub fn pack<V: PackValue>(format: &[u8], args: &[V], notices: &mut Notices) -> StdResult<Vec<u8>> {
    let num_args = i64::try_from(args.len()).unwrap_or(i64::MAX);
    let mut strings: Vec<Option<Vec<u8>>> = vec![None; args.len()];
    let mut codes: Vec<(u8, i64, Endian)> = Vec::new();
    let mut currentarg: i64 = 0;
    let formatlen = format.len();
    let mut i = 0;
    let warn = |n: &mut Notices, m: String| n.push(Notice::warning("pack", c_msg(m)));

    while i < formatlen {
        let code = format[i];
        i += 1;
        let mut arg: i64 = 1;
        let mut endian = Endian::None;
        if i < formatlen {
            if format[i] == b'<' {
                endian = Endian::Little;
                i += 1;
            } else if format[i] == b'>' {
                endian = Endian::Big;
                i += 1;
            }
        }
        if i < formatlen {
            let c = format[i];
            if c == b'*' {
                arg = -1;
                i += 1;
            } else if c.is_ascii_digit() {
                // atoi(): (int) strtol(...)
                arg = i64::from(strtol_digits(&format[i..]).0 as i32);
                while i < formatlen && format[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }

        let too_few = || value_error(format!("Type {}: too few arguments", ch(code)));
        let mut consume = |arg: &mut i64| -> StdResult<()> {
            if *arg < 0 {
                *arg = num_args - currentarg;
            }
            if currentarg > INT_MAX - *arg {
                return Err(too_few());
            }
            currentarg += *arg;
            if currentarg > num_args {
                return Err(too_few());
            }
            Ok(())
        };

        match code {
            b'x' | b'X' | b'@' => {
                if endian != Endian::None {
                    return Err(not_supported(code));
                }
                if arg < 0 {
                    warn(notices, format!("Type {}: '*' ignored", ch(code)));
                    arg = 1;
                }
            }
            b'a' | b'A' | b'Z' | b'h' | b'H' => {
                if endian != Endian::None {
                    return Err(not_supported(code));
                }
                if currentarg >= num_args {
                    return Err(value_error(format!(
                        "Type {}: not enough arguments",
                        ch(code)
                    )));
                }
                if arg < 0 {
                    let idx = currentarg as usize;
                    let s = args[idx].to_bytes()?;
                    arg = i64::try_from(s.len()).unwrap_or(i64::MAX);
                    strings[idx] = Some(s);
                    if code == b'Z' {
                        arg += 1;
                    }
                }
                currentarg += 1;
            }
            b'J' | b'P' | b'n' | b'N' | b'v' | b'V' | b'g' | b'G' | b'e' | b'E' => {
                if endian != Endian::None {
                    return Err(inherent(endian, code));
                }
                consume(&mut arg)?;
            }
            b'q' | b'Q' | b's' | b'S' | b'l' | b'L' | b'f' | b'd' => consume(&mut arg)?,
            b'c' | b'C' | b'i' | b'I' => {
                if endian != Endian::None {
                    return Err(not_supported(code));
                }
                consume(&mut arg)?;
            }
            _ => {
                return Err(value_error(format!(
                    "Type {}: unknown format code",
                    ch(code)
                )))
            }
        }
        codes.push((code, arg, endian));
    }

    if currentarg < num_args {
        warn(
            notices,
            format!("{} arguments unused", num_args - currentarg),
        );
    }

    // Calculate output length.
    let mut outputpos: i64 = 0;
    let mut outputsize: i64 = 0;
    for &(code, arg, _) in &codes {
        let inc = |outputpos: &mut i64, a: i64, b: i64| -> StdResult<()> {
            if a < 0 || (INT_MAX - *outputpos) / b < a {
                return Err(value_error(format!(
                    "Type {}: integer overflow in format string",
                    ch(code)
                )));
            }
            *outputpos += a * b;
            Ok(())
        };
        match code {
            b'h' | b'H' => inc(&mut outputpos, arg / 2 + arg % 2, 1)?,
            b'a' | b'A' | b'Z' | b'c' | b'C' | b'x' => inc(&mut outputpos, arg, 1)?,
            b's' | b'S' | b'n' | b'v' => inc(&mut outputpos, arg, 2)?,
            b'i' | b'I' | b'l' | b'L' | b'N' | b'V' | b'f' | b'g' | b'G' => {
                inc(&mut outputpos, arg, 4)?;
            }
            b'q' | b'Q' | b'J' | b'P' | b'd' | b'e' | b'E' => inc(&mut outputpos, arg, 8)?,
            b'X' => {
                outputpos -= arg;
                if outputpos < 0 {
                    warn(notices, format!("Type {}: outside of string", ch(code)));
                    outputpos = 0;
                }
            }
            b'@' => outputpos = arg,
            _ => {}
        }
        outputsize = outputsize.max(outputpos);
    }

    let mut out = vec![0u8; usize::try_from(outputsize).unwrap_or(0)];
    let mut pos: i64 = 0;
    let mut cur = 0usize;
    let mut take_str = |cur: &mut usize| -> StdResult<Vec<u8>> {
        let idx = *cur;
        *cur += 1;
        match strings[idx].take() {
            Some(s) => Ok(s),
            None => args[idx].to_bytes(),
        }
    };

    for &(code, arg, endian) in &codes {
        match code {
            b'a' | b'A' | b'Z' => {
                let s = take_str(&mut cur)?;
                let argu = usize::try_from(arg).unwrap_or(0);
                let mut arg_cp = if code == b'Z' {
                    argu.saturating_sub(1)
                } else {
                    argu
                };
                arg_cp = arg_cp.min(s.len());
                let p = pos as usize;
                let fill = if code == b'A' { b' ' } else { 0 };
                out[p + arg_cp..p + argu].fill(fill);
                out[p..p + arg_cp].copy_from_slice(&s[..arg_cp]);
                pos += arg;
            }
            b'h' | b'H' => {
                let s = take_str(&mut cur)?;
                let mut nibbleshift = if code == b'h' { 0 } else { 4 };
                let mut first = true;
                let mut arg = arg;
                pos -= 1;
                if arg as usize > s.len() {
                    warn(
                        notices,
                        format!("Type {}: not enough characters in string", ch(code)),
                    );
                    arg = s.len() as i64;
                }
                for &n in &s[..usize::try_from(arg).unwrap_or(0)] {
                    let v = match n {
                        b'0'..=b'9' => n - b'0',
                        b'A'..=b'F' => n - b'A' + 10,
                        b'a'..=b'f' => n - b'a' + 10,
                        _ => {
                            warn(
                                notices,
                                format!("Type {}: illegal hex digit {}", ch(code), ch(n)),
                            );
                            0
                        }
                    };
                    if first {
                        pos += 1;
                        out[pos as usize] = 0;
                        first = false;
                    } else {
                        first = true;
                    }
                    out[pos as usize] |= v << nibbleshift;
                    nibbleshift = (nibbleshift + 4) & 7;
                }
                pos += 1;
            }
            b'c' | b'C' | b's' | b'S' | b'n' | b'v' | b'i' | b'I' | b'l' | b'L' | b'N' | b'V'
            | b'q' | b'Q' | b'J' | b'P' => {
                let size = match code {
                    b'c' | b'C' => 1,
                    b's' | b'S' | b'n' | b'v' => 2,
                    b'q' | b'Q' | b'J' | b'P' => 8,
                    _ => 4,
                };
                let big = match code {
                    b'n' | b'N' | b'J' => true,
                    b'v' | b'V' | b'P' => false,
                    _ => endian == Endian::Big,
                };
                for _ in 0..arg {
                    let v = args[cur].to_long();
                    cur += 1;
                    put_int(&mut out, pos as usize, v, size, big);
                    pos += size as i64;
                }
            }
            b'f' | b'g' | b'G' => {
                for _ in 0..arg {
                    let v = args[cur].to_double() as f32;
                    cur += 1;
                    let big = code == b'G' || (code == b'f' && endian == Endian::Big);
                    let b = if big {
                        v.to_be_bytes()
                    } else {
                        v.to_le_bytes()
                    };
                    let p = pos as usize;
                    out[p..p + 4].copy_from_slice(&b);
                    pos += 4;
                }
            }
            b'd' | b'e' | b'E' => {
                for _ in 0..arg {
                    let v = args[cur].to_double();
                    cur += 1;
                    let big = code == b'E' || (code == b'd' && endian == Endian::Big);
                    let b = if big {
                        v.to_be_bytes()
                    } else {
                        v.to_le_bytes()
                    };
                    let p = pos as usize;
                    out[p..p + 8].copy_from_slice(&b);
                    pos += 8;
                }
            }
            b'x' => {
                let p = pos as usize;
                out[p..p + arg as usize].fill(0);
                pos += arg;
            }
            b'X' => {
                pos = (pos - arg).max(0);
            }
            b'@' => {
                if arg > pos {
                    out[pos as usize..arg as usize].fill(0);
                }
                pos = arg;
            }
            _ => {}
        }
    }
    out.truncate(usize::try_from(pos).unwrap_or(0));
    Ok(out)
}

/// A key of the array `unpack()` returns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnpackKey {
    /// Integer key (unnamed elements, or numeric-string names).
    Int(i64),
    /// String key.
    Str(Vec<u8>),
}

/// A value of the array `unpack()` returns.
#[derive(Debug, Clone, PartialEq)]
pub enum UnpackValue {
    /// From integer codes.
    Int(i64),
    /// From `f g G d e E`.
    Float(f64),
    /// From `a A Z h H`.
    Str(Vec<u8>),
}

/// `ZEND_HANDLE_NUMERIC_STR`: canonical decimal integer strings become
/// integer keys.
pub fn symtable_key(s: &[u8]) -> UnpackKey {
    let digits = s.strip_prefix(b"-").unwrap_or(s);
    let canonical = !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && (digits[0] != b'0' || (digits.len() == 1 && s.len() == 1));
    if canonical {
        if let Some(v) = std::str::from_utf8(s)
            .ok()
            .and_then(|t| t.parse::<i64>().ok())
        {
            return UnpackKey::Int(v);
        }
    }
    UnpackKey::Str(s.to_vec())
}

/// Ordered map with PHP's "update keeps position" semantics.
#[derive(Default)]
struct OrderedMap {
    items: Vec<(UnpackKey, UnpackValue)>,
    index: HashMap<UnpackKey, usize>,
}

impl OrderedMap {
    fn update(&mut self, k: UnpackKey, v: UnpackValue) {
        if let Some(&i) = self.index.get(&k) {
            self.items[i].1 = v;
        } else {
            self.index.insert(k.clone(), self.items.len());
            self.items.push((k, v));
        }
    }
}

fn read<const N: usize>(input: &[u8], pos: i64) -> [u8; N] {
    let mut b = [0u8; N];
    let p = usize::try_from(pos).unwrap_or(0);
    b.copy_from_slice(&input[p..p + N]);
    b
}

/// `unpack(string $format, string $string, int $offset = 0): array|false`.
///
/// Returns `Ok(None)` where PHP returns `false` (after pushing the
/// warning), otherwise the ordered key/value pairs of the result array
/// (numeric-string names already turned into integer keys, later
/// duplicates overwriting earlier ones in place).
pub fn unpack(
    format: &[u8],
    input: &[u8],
    offset: i64,
    notices: &mut Notices,
) -> StdResult<Option<Vec<(UnpackKey, UnpackValue)>>> {
    let total = i64::try_from(input.len()).unwrap_or(i64::MAX);
    if offset < 0 || offset > total {
        return Err(StdError::arg_value(
            "unpack",
            3,
            "offset",
            "must be contained in argument #2 ($string)",
        ));
    }
    let input = &input[usize::try_from(offset).unwrap_or(0)..];
    let inputlen = total - offset;
    let mut inputpos: i64 = 0;
    let mut result = OrderedMap::default();
    let warn = |n: &mut Notices, m: String| n.push(Notice::warning("unpack", c_msg(m)));

    let mut f = 0usize;
    let flen = format.len();
    while f < flen {
        let ty = format[f];
        f += 1;
        let mut repetitions: i64 = 1;
        let mut endian = Endian::None;
        if f < flen {
            if format[f] == b'<' {
                endian = Endian::Little;
                f += 1;
            } else if format[f] == b'>' {
                endian = Endian::Big;
                f += 1;
            }
        }
        if f < flen {
            let c = format[f];
            if c.is_ascii_digit() {
                // strtol stops at the terminating NUL of the zend_string.
                let (tmp, overflow) = strtol_digits(&format[f..]);
                if overflow || tmp > INT_MAX {
                    warn(notices, format!("Type {}: integer overflow", ch(ty)));
                    return Ok(None);
                }
                repetitions = tmp;
                while f < flen && format[f].is_ascii_digit() {
                    f += 1;
                }
            } else if c == b'*' {
                repetitions = -1;
                f += 1;
            }
        }
        let name_start = f;
        while f < flen && format[f] != b'/' {
            f += 1;
        }
        let name = &format[name_start..f.min(name_start + 200)];
        let argb = repetitions;

        let size: i64 = match ty {
            b'X' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                if repetitions < 0 {
                    warn(notices, format!("Type {}: '*' ignored", ch(ty)));
                    repetitions = 1;
                }
                -1
            }
            b'@' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                0
            }
            b'a' | b'A' | b'Z' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                let s = repetitions;
                repetitions = 1;
                s
            }
            b'h' | b'H' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                let s = if repetitions > 0 {
                    (repetitions + 1) / 2
                } else {
                    repetitions
                };
                repetitions = 1;
                s
            }
            b'c' | b'C' | b'x' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                1
            }
            b's' | b'S' => 2,
            b'n' | b'v' => {
                if endian != Endian::None {
                    return Err(inherent(endian, ty));
                }
                2
            }
            b'i' | b'I' => {
                if endian != Endian::None {
                    return Err(not_supported(ty));
                }
                4
            }
            b'l' | b'L' | b'f' => 4,
            b'N' | b'V' | b'g' | b'G' => {
                if endian != Endian::None {
                    return Err(inherent(endian, ty));
                }
                4
            }
            b'q' | b'Q' | b'd' => 8,
            b'J' | b'P' | b'e' | b'E' => {
                if endian != Endian::None {
                    return Err(inherent(endian, ty));
                }
                8
            }
            _ => return Err(value_error(format!("Invalid format type {}", ch(ty)))),
        };
        let mut size = size;

        // `for (i = 0; i != repetitions; i++)` with C int semantics.
        let mut i: i64 = 0;
        while i != repetitions {
            if size != 0 && size != -1 && INT_MAX - size + 1 < inputpos {
                warn(notices, format!("Type {}: integer overflow", ch(ty)));
                return Ok(None);
            }
            if inputpos + size <= inputlen {
                let key = if name.is_empty() {
                    UnpackKey::Int(i + 1)
                } else if repetitions == 1 {
                    symtable_key(name)
                } else {
                    let mut n = name.to_vec();
                    n.extend_from_slice((i + 1).to_string().as_bytes());
                    symtable_key(&n)
                };
                let p = usize::try_from(inputpos).unwrap_or(0);
                let remaining = inputlen - inputpos;
                let val: Option<UnpackValue> = match ty {
                    b'a' | b'A' | b'Z' => {
                        let mut len = remaining;
                        if size >= 0 && len > size {
                            len = size;
                        }
                        size = len;
                        let bytes = &input[p..p + usize::try_from(len).unwrap_or(0)];
                        Some(UnpackValue::Str(match ty {
                            b'a' => bytes.to_vec(),
                            b'A' => {
                                let end = bytes
                                    .iter()
                                    .rposition(|b| !matches!(b, 0 | b' ' | b'\t' | b'\r' | b'\n'))
                                    .map_or(0, |e| e + 1);
                                bytes[..end].to_vec()
                            }
                            _ => {
                                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                                bytes[..end].to_vec()
                            }
                        }))
                    }
                    b'h' | b'H' => {
                        if size > INT_MAX / 2 {
                            return Err(StdError::arg_value(
                                "unpack",
                                1,
                                "format",
                                &format!("repeater must be less than or equal to {}", INT_MAX / 2),
                            ));
                        }
                        let mut len = remaining * 2;
                        if size >= 0 && len > size * 2 {
                            len = size * 2;
                        }
                        if len > 0 && argb > 0 {
                            len -= argb % 2;
                        }
                        let mut nibbleshift = if ty == b'h' { 0 } else { 4 };
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "bounded: at most twice the input"
                        )]
                        let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
                        let mut ipos = p;
                        let mut first = true;
                        for _ in 0..len {
                            let cc = (input[ipos] >> nibbleshift) & 0xf;
                            buf.push(if cc < 10 { cc + b'0' } else { cc - 10 + b'a' });
                            nibbleshift = (nibbleshift + 4) & 7;
                            if first {
                                first = false;
                            } else {
                                ipos += 1;
                                first = true;
                            }
                        }
                        Some(UnpackValue::Str(buf))
                    }
                    b'c' => Some(UnpackValue::Int(i64::from(input[p] as i8))),
                    b'C' => Some(UnpackValue::Int(i64::from(input[p]))),
                    b's' | b'S' | b'n' | b'v' => {
                        let b = read::<2>(input, inputpos);
                        let big = ty == b'n' || (ty != b'v' && endian == Endian::Big);
                        let x = if big {
                            u16::from_be_bytes(b)
                        } else {
                            u16::from_le_bytes(b)
                        };
                        Some(UnpackValue::Int(if ty == b's' {
                            i64::from(x as i16)
                        } else {
                            i64::from(x)
                        }))
                    }
                    b'i' => Some(UnpackValue::Int(i64::from(i32::from_le_bytes(read::<4>(
                        input, inputpos,
                    ))))),
                    b'I' => Some(UnpackValue::Int(i64::from(u32::from_le_bytes(read::<4>(
                        input, inputpos,
                    ))))),
                    b'l' | b'L' | b'N' | b'V' => {
                        let b = read::<4>(input, inputpos);
                        let big = ty == b'N' || (ty != b'V' && endian == Endian::Big);
                        let x = if big {
                            u32::from_be_bytes(b)
                        } else {
                            u32::from_le_bytes(b)
                        };
                        Some(UnpackValue::Int(if ty == b'l' {
                            i64::from(x as i32)
                        } else {
                            i64::from(x)
                        }))
                    }
                    b'q' | b'Q' | b'J' | b'P' => {
                        let b = read::<8>(input, inputpos);
                        let big = ty == b'J' || (ty != b'P' && endian == Endian::Big);
                        let x = if big {
                            u64::from_be_bytes(b)
                        } else {
                            u64::from_le_bytes(b)
                        };
                        Some(UnpackValue::Int(x as i64))
                    }
                    b'f' | b'g' | b'G' => {
                        let b = read::<4>(input, inputpos);
                        let big = ty == b'G' || (ty == b'f' && endian == Endian::Big);
                        let v = if big {
                            f32::from_be_bytes(b)
                        } else {
                            f32::from_le_bytes(b)
                        };
                        Some(UnpackValue::Float(f64::from(v)))
                    }
                    b'd' | b'e' | b'E' => {
                        let b = read::<8>(input, inputpos);
                        let big = ty == b'E' || (ty == b'd' && endian == Endian::Big);
                        Some(UnpackValue::Float(if big {
                            f64::from_be_bytes(b)
                        } else {
                            f64::from_le_bytes(b)
                        }))
                    }
                    b'X' => {
                        if inputpos < size {
                            inputpos = -size;
                            i = repetitions - 1;
                            if repetitions >= 0 {
                                warn(notices, format!("Type {}: outside of string", ch(ty)));
                            }
                        }
                        None
                    }
                    b'@' => {
                        if repetitions <= inputlen {
                            inputpos = repetitions;
                        } else {
                            warn(notices, format!("Type {}: outside of string", ch(ty)));
                        }
                        i = repetitions - 1;
                        None
                    }
                    // 'x': skip input.
                    _ => None,
                };
                if let Some(v) = val {
                    result.update(key, v);
                }
                inputpos += size;
                if inputpos < 0 {
                    if size != -1 {
                        warn(notices, format!("Type {}: outside of string", ch(ty)));
                    }
                    inputpos = 0;
                }
            } else if repetitions < 0 {
                break;
            } else {
                let have = inputlen - inputpos;
                warn(
                    notices,
                    format!(
                        "Type {}: not enough input values, need {} values but only {} {} provided",
                        ch(ty),
                        size,
                        have,
                        if have == 1 { "was" } else { "were" }
                    ),
                );
                return Ok(None);
            }
            // C `int` loop counter.
            i = i64::from((i as i32).wrapping_add(1));
        }

        if f < flen {
            f += 1;
        }
    }
    Ok(Some(result.items))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(format: &str, args: &[PackArg]) -> Vec<u8> {
        let mut n = Vec::new();
        pack(format.as_bytes(), args, &mut n).unwrap_or_default()
    }

    #[test]
    fn pack_basic() {
        assert_eq!(p("n", &[PackArg::Int(0x1234)]), b"\x12\x34");
        assert_eq!(p("v", &[PackArg::Int(0x1234)]), b"\x34\x12");
        assert_eq!(p("N", &[PackArg::Int(1)]), b"\0\0\0\x01");
        assert_eq!(p("s>", &[PackArg::Int(1)]), b"\0\x01");
        assert_eq!(p("a3", &[PackArg::Str(b"x".to_vec())]), b"x\0\0");
        assert_eq!(p("A3", &[PackArg::Str(b"x".to_vec())]), b"x  ");
        assert_eq!(p("Z*", &[PackArg::Str(b"ab".to_vec())]), b"ab\0");
        assert_eq!(p("H*", &[PackArg::Str(b"4a5".to_vec())]), b"\x4a\x50");
        assert_eq!(p("h*", &[PackArg::Str(b"4a5".to_vec())]), b"\xa4\x05");
        assert_eq!(p("x2X@4", &[]), b"\0\0\0\0");
    }

    #[test]
    fn pack_errors() {
        let mut n = Vec::new();
        let e = pack::<PackArg>(b"a", &[], &mut n).unwrap_err();
        assert_eq!(e.to_string(), "Type a: not enough arguments");
        let e = pack::<PackArg>(b"N2", &[PackArg::Int(1)], &mut n).unwrap_err();
        assert_eq!(e.to_string(), "Type N: too few arguments");
        let e = pack::<PackArg>(b"B", &[], &mut n).unwrap_err();
        assert_eq!(e.to_string(), "Type B: unknown format code");
        pack(b"N", &[PackArg::Int(1), PackArg::Int(2)], &mut n).unwrap_or_default();
        assert_eq!(n[0].to_string(), "pack(): 1 arguments unused");
    }

    #[test]
    fn unpack_basic() {
        let mut n = Vec::new();
        let r = unpack(b"nlen/a*data", b"\0\x03abc", 0, &mut n)
            .ok()
            .flatten();
        assert_eq!(
            r,
            Some(vec![
                (UnpackKey::Str(b"len".to_vec()), UnpackValue::Int(3)),
                (
                    UnpackKey::Str(b"data".to_vec()),
                    UnpackValue::Str(b"abc".to_vec())
                ),
            ])
        );
        let r = unpack(b"C*", b"\x01\x02", 0, &mut n).ok().flatten();
        assert_eq!(
            r,
            Some(vec![
                (UnpackKey::Int(1), UnpackValue::Int(1)),
                (UnpackKey::Int(2), UnpackValue::Int(2)),
            ])
        );
        assert!(n.is_empty());
    }

    #[test]
    fn keys() {
        assert_eq!(symtable_key(b"12"), UnpackKey::Int(12));
        assert_eq!(symtable_key(b"-3"), UnpackKey::Int(-3));
        assert_eq!(symtable_key(b"012"), UnpackKey::Str(b"012".to_vec()));
        assert_eq!(symtable_key(b"-0"), UnpackKey::Str(b"-0".to_vec()));
        assert_eq!(symtable_key(b"0"), UnpackKey::Int(0));
    }

    #[test]
    fn gcvt() {
        assert_eq!(php_gcvt(0.1 + 0.2, 14), "0.3");
        assert_eq!(php_gcvt(1e25, 14), "1.0E+25");
        assert_eq!(php_gcvt(-1.5, 14), "-1.5");
        assert_eq!(php_gcvt(100.0, 14), "100");
        assert_eq!(php_gcvt(0.00001, 14), "1.0E-5");
    }
}
