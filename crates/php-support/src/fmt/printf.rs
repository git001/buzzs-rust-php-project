// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `sprintf()` family engine — a port of `php_formatted_print()` from
//! `ext/standard/formatted_print.c`.
//!
//! Arguments are accessed through the [`FormatArg`] trait so the VM can
//! plug in its own value type; implementations are provided for the common
//! Rust scalar and string types (handy for tests and internal callers).

use std::borrow::Cow;
use std::fmt;

use super::double::{php_conv_fp, zend_gcvt};
use crate::diag::{Diagnostic, DiagnosticSink, ErrorLevel};
use crate::numeric::{scan_numeric, NumericValue};

const FLOAT_PRECISION: i32 = 6;
const MAX_FLOAT_PRECISION: i32 = 53;
const INT_MAX: i64 = i32::MAX as i64;

/// A `sprintf()` argument.  The conversions must behave like
/// `zval_get_long()`, `zval_get_double()` and `zval_get_string()`.
pub trait FormatArg {
    /// `zval_get_long()`.
    fn as_int(&self) -> i64;
    /// `zval_get_double()`.
    fn as_float(&self) -> f64;
    /// `zval_get_string()` (may emit warnings or throw inside the VM; the
    /// formatter carries on like PHP and the caller discards the result if
    /// an exception is pending).
    fn as_str(&self) -> Cow<'_, [u8]>;
    /// `Some(n)` only when the value *is* an integer (`IS_LONG` after
    /// dereferencing), used for `*` width/precision arguments, which are
    /// never coerced.
    fn exact_int(&self) -> Option<i64>;
}

impl FormatArg for i64 {
    fn as_int(&self) -> i64 {
        *self
    }
    #[allow(clippy::cast_precision_loss)]
    fn as_float(&self) -> f64 {
        *self as f64
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.to_string().into_bytes())
    }
    fn exact_int(&self) -> Option<i64> {
        Some(*self)
    }
}

impl FormatArg for i32 {
    fn as_int(&self) -> i64 {
        i64::from(*self)
    }
    fn as_float(&self) -> f64 {
        f64::from(*self)
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.to_string().into_bytes())
    }
    fn exact_int(&self) -> Option<i64> {
        Some(i64::from(*self))
    }
}

/// `zend_dval_to_lval()` without the out-of-range warning: non-finite
/// values become 0, out-of-range values wrap modulo 2^64
/// (`zend_dval_to_lval_slow()`), others truncate toward zero.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(crate) fn dval_to_lval(d: f64) -> i64 {
    const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
    if !d.is_finite() {
        0
    } else if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d) {
        d as i64
    } else {
        let mut m = d % TWO_POW_64;
        if m < 0.0 {
            m += TWO_POW_64;
        }
        i64::from_ne_bytes((m as u64).to_ne_bytes())
    }
}

impl FormatArg for f64 {
    fn as_int(&self) -> i64 {
        dval_to_lval(*self)
    }
    fn as_float(&self) -> f64 {
        *self
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Owned(super::format_double(*self, 14, super::DoubleMode::Plain).into_bytes())
    }
    fn exact_int(&self) -> Option<i64> {
        None
    }
}

impl FormatArg for bool {
    fn as_int(&self) -> i64 {
        i64::from(*self)
    }
    fn as_float(&self) -> f64 {
        f64::from(u8::from(*self))
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(if *self { b"1" } else { b"" })
    }
    fn exact_int(&self) -> Option<i64> {
        None
    }
}

fn bytes_as_int(s: &[u8]) -> i64 {
    match scan_numeric(s, true).map(|r| r.value) {
        Some(NumericValue::Long(l)) => l,
        Some(NumericValue::Double(d)) => dval_to_lval(d),
        None => 0,
    }
}

fn bytes_as_float(s: &[u8]) -> f64 {
    match scan_numeric(s, true).map(|r| r.value) {
        #[allow(clippy::cast_precision_loss)]
        Some(NumericValue::Long(l)) => l as f64,
        Some(NumericValue::Double(d)) => d,
        None => 0.0,
    }
}

impl FormatArg for [u8] {
    fn as_int(&self) -> i64 {
        bytes_as_int(self)
    }
    fn as_float(&self) -> f64 {
        bytes_as_float(self)
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self)
    }
    fn exact_int(&self) -> Option<i64> {
        None
    }
}

impl FormatArg for str {
    fn as_int(&self) -> i64 {
        bytes_as_int(self.as_bytes())
    }
    fn as_float(&self) -> f64 {
        bytes_as_float(self.as_bytes())
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.as_bytes())
    }
    fn exact_int(&self) -> Option<i64> {
        None
    }
}

impl FormatArg for String {
    fn as_int(&self) -> i64 {
        self.as_str().as_int()
    }
    fn as_float(&self) -> f64 {
        self.as_str().as_float()
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.as_bytes())
    }
    fn exact_int(&self) -> Option<i64> {
        None
    }
}

impl<T: FormatArg + ?Sized> FormatArg for &T {
    fn as_int(&self) -> i64 {
        (**self).as_int()
    }
    fn as_float(&self) -> f64 {
        (**self).as_float()
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        (**self).as_str()
    }
    fn exact_int(&self) -> Option<i64> {
        (**self).exact_int()
    }
}

/// An error that aborts formatting (PHP throws and returns nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// A `ValueError` with this message.
    ValueError(String),
    /// An `ArgumentCountError` with this message.
    ArgumentCountError(String),
    /// A fatal `E_ERROR` ("Field width %zd is too long").
    Fatal(String),
    /// The result would pass `memory_limit` (php-src: `zend_string_extend`
    /// in `php_sprintf_appendstring()` fails in `emalloc`).
    MemoryLimit(php_memlimit::OverLimit),
}

impl FormatError {
    /// The exception/error message.
    pub fn message(&self) -> &str {
        match self {
            Self::ValueError(m) | Self::ArgumentCountError(m) | Self::Fatal(m) => m,
            Self::MemoryLimit(_) => "",
        }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::ValueError(_) => "ValueError",
            Self::ArgumentCountError(_) => "ArgumentCountError",
            Self::Fatal(_) => "Fatal error",
            Self::MemoryLimit(over) => return write!(f, "Fatal error: {over}"),
        };
        write!(f, "{kind}: {}", self.message())
    }
}

impl std::error::Error for FormatError {}

/// Where the arguments come from, which decides the error raised when
/// there are too few of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSource {
    /// Variadic arguments following `n` fixed parameters (1 for
    /// `sprintf`/`printf`, 2 for `fprintf`): too few arguments raise
    /// `ArgumentCountError("%d arguments are required, %d given")`.
    Variadic(usize),
    /// An array (`vsprintf`, `vprintf`, `vfprintf`): too few elements raise
    /// `ValueError("The arguments array must contain %d items, %d given")`.
    Array,
}

/// Options for [`php_formatted_print`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions {
    /// Argument origin (controls the missing-argument error).
    pub source: ArgSource,
    /// `localeconv()->decimal_point`, used by `%f`, `%g` and `%G`
    /// (`%F`, `%e`, `%E`, `%h` and `%H` always use `.`). Bindings pass
    /// [`super::locale::decimal_point`]; the default is `.` (C locale).
    pub decimal_point: u8,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            source: ArgSource::Variadic(1),
            decimal_point: b'.',
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

/// Writes the decimal digits of `n` right-aligned into `buf` (at least 20
/// bytes, `u64::MAX` has 20 digits) and returns the index of the first.
fn write_u64_rev(buf: &mut [u8; 21], mut n: u64) -> usize {
    let mut i = buf.len();
    loop {
        i -= 1;
        #[allow(clippy::cast_possible_truncation)]
        let digit = (n % 10) as u8;
        buf[i] = b'0' + digit;
        n /= 10;
        if n == 0 {
            return i;
        }
    }
}

struct Out {
    buf: Vec<u8>,
}

impl Out {
    /// `php_sprintf_appendstring()`.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn append_string(
        &mut self,
        add: &[u8],
        min_width: usize,
        max_width: usize,
        padding: u8,
        alignment: Align,
        neg: bool,
        expprec: bool,
        always_sign: bool,
    ) -> Result<(), FormatError> {
        let len = add.len();
        let mut copy_len = if expprec { max_width.min(len) } else { len };
        let mut npad = min_width.saturating_sub(copy_len);
        let m_width = min_width.max(copy_len);
        let limit = usize::try_from(INT_MAX).unwrap_or(usize::MAX);
        if m_width > limit.saturating_sub(self.buf.len()).saturating_sub(1) {
            return Err(FormatError::Fatal(format!(
                "Field width {m_width} is too long"
            )));
        }
        php_memlimit::TryGrow::try_grow(&mut self.buf, m_width)
            .map_err(FormatError::MemoryLimit)?;
        let mut add = add;
        if alignment == Align::Right {
            if (neg || always_sign) && padding == b'0' {
                self.buf.push(if neg { b'-' } else { b'+' });
                add = add.get(1..).unwrap_or_default();
                copy_len = copy_len.saturating_sub(1);
            }
            while npad > 0 {
                self.buf.push(padding);
                npad -= 1;
            }
        }
        self.buf.extend_from_slice(&add[..copy_len.min(add.len())]);
        if alignment == Align::Left {
            #[expect(
                clippy::disallowed_methods,
                reason = "checked: try_grow(m_width) above made room for the padding"
            )]
            self.buf.resize(self.buf.len() + npad, padding);
        }
        Ok(())
    }

    /// `php_sprintf_appendint()`.
    fn append_int(
        &mut self,
        number: i64,
        width: usize,
        padding: u8,
        alignment: Align,
        always_sign: bool,
    ) -> Result<(), FormatError> {
        let padding = if alignment == Align::Left && padding == b'0' {
            b' '
        } else {
            padding
        };
        let neg = number < 0;
        let mut buf = [0u8; 21];
        let mut start = write_u64_rev(&mut buf, number.unsigned_abs());
        if neg {
            start -= 1;
            buf[start] = b'-';
        } else if always_sign {
            start -= 1;
            buf[start] = b'+';
        }
        self.append_string(
            &buf[start..],
            width,
            0,
            padding,
            alignment,
            neg,
            false,
            always_sign,
        )
    }

    /// `php_sprintf_appenduint()`.
    fn append_uint(
        &mut self,
        number: u64,
        width: usize,
        padding: u8,
        alignment: Align,
    ) -> Result<(), FormatError> {
        let padding = if alignment == Align::Left && padding == b'0' {
            b' '
        } else {
            padding
        };
        let mut buf = [0u8; 21];
        let start = write_u64_rev(&mut buf, number);
        self.append_string(
            &buf[start..],
            width,
            0,
            padding,
            alignment,
            false,
            false,
            false,
        )
    }

    /// `php_sprintf_append2n()`.
    #[allow(clippy::too_many_arguments)]
    fn append_2n(
        &mut self,
        number: i64,
        width: usize,
        padding: u8,
        alignment: Align,
        bits: u32,
        upper: bool,
        expprec: bool,
    ) -> Result<(), FormatError> {
        let table: &[u8; 16] = if upper {
            b"0123456789ABCDEF"
        } else {
            b"0123456789abcdef"
        };
        let mask = (1u64 << bits) - 1;
        #[allow(clippy::cast_sign_loss)]
        let mut num = number as u64;
        #[expect(clippy::disallowed_methods, reason = "bounded: constant size")]
        let mut s = Vec::with_capacity(64);
        loop {
            #[allow(clippy::cast_possible_truncation)]
            s.push(table[(num & mask) as usize]);
            num >>= bits;
            if num == 0 {
                break;
            }
        }
        s.reverse();
        // NB: the precision is passed as max_width 0 with expprec — a quirk of
        // the C code, which therefore prints nothing for e.g. "%.2x".
        self.append_string(&s, width, 0, padding, alignment, false, expprec, false)
    }

    /// `php_sprintf_appenddouble()`.
    #[allow(clippy::too_many_arguments)]
    fn append_double(
        &mut self,
        number: f64,
        width: usize,
        padding: u8,
        alignment: Align,
        precision: i32,
        fmt: u8,
        always_sign: bool,
        decimal_point: u8,
    ) -> Result<(), FormatError> {
        if number.is_nan() {
            return self.append_string(b"NaN", 3, 0, padding, alignment, false, false, always_sign);
        }
        if number.is_infinite() {
            let neg = number < 0.0;
            let s: &[u8] = if neg { b"-INF" } else { b"INF" };
            return self.append_string(s, s.len(), 0, padding, alignment, neg, false, always_sign);
        }
        let (s, is_negative) = match fmt {
            b'e' | b'E' | b'f' | b'F' => {
                let conv = if fmt == b'f' { b'F' } else { fmt };
                let dp = if fmt == b'f' { decimal_point } else { b'.' };
                let (digits, neg) = php_conv_fp(conv, number, false, precision, dp);
                #[expect(
                    clippy::disallowed_methods,
                    reason = "bounded: the digits of one float (precision capped at MAX_FLOAT_PRECISION)"
                )]
                let mut s = Vec::with_capacity(digits.len() + 1);
                if neg {
                    s.push(b'-');
                } else if always_sign {
                    s.push(b'+');
                }
                s.extend_from_slice(digits.as_bytes());
                (s, neg)
            }
            _ => {
                let precision = if precision == 0 { 1 } else { precision };
                let dp = if matches!(fmt, b'g' | b'G') {
                    decimal_point
                } else {
                    b'.'
                };
                let exp_char = if matches!(fmt, b'G' | b'H') {
                    b'E'
                } else {
                    b'e'
                };
                let digits = zend_gcvt(number, precision, dp, exp_char);
                let neg = digits.starts_with('-');
                #[expect(
                    clippy::disallowed_methods,
                    reason = "bounded: the digits of one float (precision capped at MAX_FLOAT_PRECISION)"
                )]
                let mut s = Vec::with_capacity(digits.len() + 1);
                if !neg && always_sign {
                    s.push(b'+');
                }
                s.extend_from_slice(digits.as_bytes());
                (s, neg)
            }
        };
        self.append_string(
            &s,
            width,
            0,
            padding,
            alignment,
            is_negative,
            false,
            always_sign,
        )
    }
}

/// Result of `php_sprintf_get_argnum()`.
enum ArgNum {
    Next,
    Index(usize),
}

/// `php_sprintf_getnumber()`: parses decimal digits (as `strtol` would),
/// returning -1 when the value is `>= INT_MAX`.
fn get_number(format: &[u8], pos: &mut usize) -> i64 {
    let start = *pos;
    let mut acc: i64 = 0;
    while let Some(&c) = format.get(*pos) {
        if !c.is_ascii_digit() {
            break;
        }
        acc = acc.saturating_mul(10).saturating_add(i64::from(c - b'0'));
        *pos += 1;
    }
    if *pos == start {
        return 0;
    }
    if acc >= INT_MAX {
        -1
    } else {
        acc
    }
}

/// `php_sprintf_get_argnum()`.
fn get_argnum(format: &[u8], pos: &mut usize) -> Result<ArgNum, FormatError> {
    let at = |i: usize| format.get(i).copied().unwrap_or(0);
    let mut temp = *pos;
    while at(temp).is_ascii_digit() {
        temp += 1;
    }
    if at(temp) != b'$' {
        return Ok(ArgNum::Next);
    }
    if temp == *pos {
        return Err(FormatError::ValueError(
            "Argument number specifier must not be empty".into(),
        ));
    }
    let argnum = get_number(format, pos);
    if argnum <= 0 {
        return Err(FormatError::ValueError(format!(
            "Argument number specifier must be greater than zero and less than {INT_MAX}"
        )));
    }
    *pos += 1; // skip '$'
    Ok(ArgNum::Index(
        usize::try_from(argnum - 1).unwrap_or(usize::MAX),
    ))
}

/// `php_formatted_print()`: formats `format` with `args`.
///
/// Notices (precision truncation) are sent to `diag` as docref
/// diagnostics.  On error nothing is returned, as PHP throws.
#[allow(clippy::too_many_lines)]
pub fn php_formatted_print(
    format: &[u8],
    args: &[&dyn FormatArg],
    options: FormatOptions,
    diag: &mut dyn DiagnosticSink,
) -> Result<Vec<u8>, FormatError> {
    let at = |i: usize| format.get(i).copied().unwrap_or(0);
    let n_args = args.len();
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: the length of the format string"
    )]
    let mut out = Out {
        buf: Vec::with_capacity(format.len() + 16),
    };
    let mut pos = 0usize;
    let mut currarg = 0usize;
    let mut max_missing: Option<usize> = None;
    let missing = |m: &mut Option<usize>, n: usize| *m = Some(m.map_or(n, |x| x.max(n)));

    while pos < format.len() {
        let Some(off) = format[pos..].iter().position(|&c| c == b'%') else {
            out.buf.extend_from_slice(&format[pos..]);
            break;
        };
        out.buf.extend_from_slice(&format[pos..pos + off]);
        pos += off + 1; // skip '%'

        if at(pos) == b'%' {
            out.buf.push(b'%');
            pos += 1;
            continue;
        }

        let mut alignment = Align::Right;
        let mut adjust_precision = false;
        let mut padding = b' ';
        let mut always_sign = false;
        let mut expprec = false;
        let mut width: i64 = 0;
        let mut precision: i64 = 0;
        let argnum;

        if at(pos).is_ascii_alphabetic() {
            argnum = ArgNum::Next;
        } else {
            argnum = get_argnum(format, &mut pos)?;
            // Modifiers.
            loop {
                match at(pos) {
                    c @ (b' ' | b'0') => padding = c,
                    b'-' => alignment = Align::Left,
                    b'+' => always_sign = true,
                    b'\'' => {
                        if format.len() - pos > 1 {
                            pos += 1;
                            padding = at(pos);
                        } else {
                            return Err(FormatError::ValueError(
                                "Missing padding character".into(),
                            ));
                        }
                    }
                    _ => break,
                }
                pos += 1;
            }
            // Width.
            if at(pos) == b'*' {
                pos += 1;
                let width_argnum = match get_argnum(format, &mut pos)? {
                    ArgNum::Next => {
                        currarg += 1;
                        currarg - 1
                    }
                    ArgNum::Index(i) => i,
                };
                if width_argnum >= n_args {
                    missing(&mut max_missing, width_argnum);
                    continue;
                }
                let Some(w) = args[width_argnum].exact_int() else {
                    return Err(FormatError::ValueError("Width must be an integer".into()));
                };
                if !(0..=INT_MAX).contains(&w) {
                    return Err(FormatError::ValueError(format!(
                        "Width must be between 0 and {INT_MAX}"
                    )));
                }
                width = w;
            } else if at(pos).is_ascii_digit() {
                width = get_number(format, &mut pos);
                if width < 0 {
                    return Err(FormatError::ValueError(format!(
                        "Width must be between 0 and {INT_MAX}"
                    )));
                }
            }
            // Precision.
            if at(pos) == b'.' {
                pos += 1;
                adjust_precision = true;
                if at(pos) == b'*' {
                    pos += 1;
                    let prec_argnum = match get_argnum(format, &mut pos)? {
                        ArgNum::Next => {
                            currarg += 1;
                            currarg - 1
                        }
                        ArgNum::Index(i) => i,
                    };
                    if prec_argnum >= n_args {
                        missing(&mut max_missing, prec_argnum);
                        continue;
                    }
                    let Some(p) = args[prec_argnum].exact_int() else {
                        return Err(FormatError::ValueError(
                            "Precision must be an integer".into(),
                        ));
                    };
                    if !(-1..=INT_MAX).contains(&p) {
                        return Err(FormatError::ValueError(format!(
                            "Precision must be between -1 and {INT_MAX}"
                        )));
                    }
                    precision = p;
                    expprec = true;
                } else if at(pos).is_ascii_digit() {
                    precision = get_number(format, &mut pos);
                    if precision < 0 {
                        return Err(FormatError::ValueError(format!(
                            "Precision must be between 0 and {INT_MAX}"
                        )));
                    }
                    expprec = true;
                }
            }
        }

        if at(pos) == b'l' {
            pos += 1;
        }

        let argnum = match argnum {
            ArgNum::Next => {
                currarg += 1;
                currarg - 1
            }
            ArgNum::Index(i) => i,
        };
        if argnum >= n_args {
            missing(&mut max_missing, argnum);
            continue;
        }

        let spec = at(pos);
        if expprec && precision == -1 && !matches!(spec, b'g' | b'G' | b'h' | b'H') {
            return Err(FormatError::ValueError(
                "Precision -1 is only supported for %g, %G, %h and %H".into(),
            ));
        }

        let arg = args[argnum];
        let width_u = usize::try_from(width).unwrap_or(0);
        let prec_u = usize::try_from(precision).unwrap_or(0);
        match spec {
            b's' => {
                let s = arg.as_str();
                out.append_string(
                    &s, width_u, prec_u, padding, alignment, false, expprec, false,
                )?;
            }
            b'd' => out.append_int(arg.as_int(), width_u, padding, alignment, always_sign)?,
            b'u' => out.append_uint(
                {
                    #[allow(clippy::cast_sign_loss)]
                    let u = arg.as_int() as u64;
                    u
                },
                width_u,
                padding,
                alignment,
            )?,
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'h' | b'H' => {
                #[allow(clippy::cast_possible_truncation)]
                let mut p = precision as i32;
                if !adjust_precision {
                    p = FLOAT_PRECISION;
                } else if p > MAX_FLOAT_PRECISION {
                    diag.report(Diagnostic::docref(
                        ErrorLevel::NOTICE,
                        format!(
                            "Requested precision of {p} digits was truncated to PHP maximum of {MAX_FLOAT_PRECISION} digits"
                        ),
                    ));
                    p = MAX_FLOAT_PRECISION;
                }
                out.append_double(
                    arg.as_float(),
                    width_u,
                    padding,
                    alignment,
                    p,
                    spec,
                    always_sign,
                    options.decimal_point,
                )?;
            }
            b'c' => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                out.buf.push(arg.as_int() as u8);
            }
            b'o' => out.append_2n(arg.as_int(), width_u, padding, alignment, 3, false, expprec)?,
            b'x' => out.append_2n(arg.as_int(), width_u, padding, alignment, 4, false, expprec)?,
            b'X' => out.append_2n(arg.as_int(), width_u, padding, alignment, 4, true, expprec)?,
            b'b' => out.append_2n(arg.as_int(), width_u, padding, alignment, 1, false, expprec)?,
            b'%' => out.buf.push(b'%'),
            0 if pos >= format.len() => {
                return Err(FormatError::ValueError(
                    "Missing format specifier at end of string".into(),
                ));
            }
            c => {
                return Err(FormatError::ValueError(format!(
                    "Unknown format specifier \"{}\"",
                    String::from_utf8_lossy(&[c])
                )));
            }
        }
        pos += 1;
    }

    if let Some(m) = max_missing {
        return Err(match options.source {
            ArgSource::Array => FormatError::ValueError(format!(
                "The arguments array must contain {} items, {n_args} given",
                m + 1
            )),
            ArgSource::Variadic(extra) => FormatError::ArgumentCountError(format!(
                "{} arguments are required, {} given",
                m + extra + 1,
                n_args + extra
            )),
        });
    }
    Ok(out.buf)
}

/// `sprintf($format, ...$args)`.
pub fn sprintf(
    format: &[u8],
    args: &[&dyn FormatArg],
    diag: &mut dyn DiagnosticSink,
) -> Result<Vec<u8>, FormatError> {
    php_formatted_print(format, args, FormatOptions::default(), diag)
}

/// `vsprintf($format, $array)` (also `vprintf`, `vfprintf`).
pub fn vsprintf(
    format: &[u8],
    args: &[&dyn FormatArg],
    diag: &mut dyn DiagnosticSink,
) -> Result<Vec<u8>, FormatError> {
    php_formatted_print(
        format,
        args,
        FormatOptions {
            source: ArgSource::Array,
            ..FormatOptions::default()
        },
        diag,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(format: &str, args: &[&dyn FormatArg]) -> String {
        let mut d: Vec<Diagnostic> = Vec::new();
        let r = sprintf(format.as_bytes(), args, &mut d).unwrap();
        assert!(d.is_empty(), "unexpected diagnostics {d:?}");
        String::from_utf8_lossy(&r).into_owned()
    }

    fn err(format: &str, args: &[&dyn FormatArg]) -> FormatError {
        let mut d: Vec<Diagnostic> = Vec::new();
        sprintf(format.as_bytes(), args, &mut d).unwrap_err()
    }

    // All expectations below were produced by /usr/bin/php (8.3) unless
    // noted as a master-branch behaviour change.

    #[test]
    fn literal_and_percent() {
        assert_eq!(sp("hello", &[]), "hello");
        assert_eq!(sp("100%%", &[]), "100%");
        assert_eq!(sp("%5%|", &[&1]), "%|");
        assert_eq!(sp("", &[]), "");
    }

    #[test]
    fn integers() {
        assert_eq!(sp("%d", &[&42]), "42");
        assert_eq!(sp("%d", &[&-42]), "-42");
        assert_eq!(sp("%+d|%+d", &[&5, &-5]), "+5|-5");
        assert_eq!(sp("%5d|%-5d|%05d", &[&42, &42, &42]), "   42|42   |00042");
        assert_eq!(sp("%05d|%-05d", &[&-3, &-3]), "-0003|-3   ");
        assert_eq!(sp("%+05d", &[&3]), "+0003");
        assert_eq!(sp("%+05d", &[&0]), "+0000");
        assert_eq!(sp("%'*8d", &[&42]), "******42");
        assert_eq!(sp("%-'x8d", &[&42]), "42xxxxxx");
        assert_eq!(sp("%d", &[&i64::MIN]), "-9223372036854775808");
        assert_eq!(sp("%u", &[&-1]), "18446744073709551615");
        assert_eq!(sp("%5u", &[&3]), "    3");
        assert_eq!(sp("%ld", &[&7]), "7");
        assert_eq!(sp("%d", &[&"12abc"]), "12");
        assert_eq!(sp("%d", &[&"abc"]), "0");
        assert_eq!(sp("%d", &[&3.99]), "3");
        assert_eq!(sp("%d", &[&true]), "1");
        assert_eq!(sp("%d", &[&1e25]), "1590897979265384448");
        assert_eq!(sp("%d", &[&-1e19]), "8446744073709551616");
        assert_eq!(sp("%d", &[&f64::NAN]), "0");
        assert_eq!(sp("%.2d", &[&5]), "5");
    }

    #[test]
    fn bases_and_chars() {
        assert_eq!(sp("%b", &[&5]), "101");
        assert_eq!(sp("%o", &[&8]), "10");
        assert_eq!(sp("%x|%X", &[&255, &255]), "ff|FF");
        assert_eq!(sp("%x", &[&-1]), "ffffffffffffffff");
        assert_eq!(sp("%08b", &[&5]), "00000101");
        assert_eq!(sp("%-8x|", &[&255]), "ff      |");
        assert_eq!(sp("%c%c", &[&65, &322]), "AB");
        assert_eq!(sp("%5c", &[&65]), "A");
        // Quirk: a precision on %x/%o/%b truncates to zero characters.
        assert_eq!(sp("[%.2x]", &[&255]), "[]");
        assert_eq!(sp("[%5.1x]", &[&255]), "[     ]");
    }

    #[test]
    fn strings() {
        assert_eq!(sp("%s", &[&"abc"]), "abc");
        assert_eq!(sp("[%10s]", &[&"abc"]), "[       abc]");
        assert_eq!(sp("[%-10s]", &[&"abc"]), "[abc       ]");
        assert_eq!(sp("[%010s]", &[&"abc"]), "[0000000abc]");
        assert_eq!(sp("[%'#10s]", &[&"abc"]), "[#######abc]");
        assert_eq!(sp("[%.2s]", &[&"abcdef"]), "[ab]");
        assert_eq!(sp("[%5.2s]", &[&"abcdef"]), "[   ab]");
        assert_eq!(sp("[%-5.2s]", &[&"abcdef"]), "[ab   ]");
        assert_eq!(sp("[%.0s]", &[&"abc"]), "[]");
        assert_eq!(sp("[%.s]", &[&"abc"]), "[abc]");
        assert_eq!(sp("[%2s]", &[&"abcdef"]), "[abcdef]");
        assert_eq!(sp("[%s]", &[&1.5]), "[1.5]");
        assert_eq!(sp("[%s]", &[&false]), "[]");
        assert_eq!(sp("[%05s]", &[&"-3"]), "[000-3]");
        assert_eq!(sp("%s", &[&String::from("x")]), "x");
    }

    #[test]
    fn argnum() {
        assert_eq!(sp("%2$s %1$s", &[&"a", &"b"]), "b a");
        assert_eq!(sp("%1$s %1$'x5s", &[&"a"]), "a xxxxa");
        assert_eq!(sp("%1$s %s", &[&"a", &"b"]), "a a");
        assert_eq!(sp("%1$05d", &[&7]), "00007");
    }

    #[test]
    fn floats() {
        assert_eq!(sp("%f", &[&1.5]), "1.500000");
        assert_eq!(sp("%F", &[&1.5]), "1.500000");
        assert_eq!(sp("%.2f", &[&1.005]), "1.00");
        assert_eq!(sp("%.2f", &[&0.125]), "0.12");
        assert_eq!(sp("%.0f", &[&0.5]), "0");
        assert_eq!(sp("%.0f", &[&1.5]), "2");
        assert_eq!(sp("%.0f", &[&2.5]), "2");
        assert_eq!(sp("%.2f", &[&-0.001]), "-0.00");
        assert_eq!(sp("%.2f", &[&-0.0]), "0.00");
        assert_eq!(sp("%10.3f|", &[&3.14158]), "     3.142|");
        assert_eq!(sp("%-10.3f|", &[&3.14158]), "3.142     |");
        assert_eq!(sp("%010.3f", &[&-3.14158]), "-00003.142");
        assert_eq!(sp("%+.1f|%+.1f", &[&2.0, &-2.0]), "+2.0|-2.0");
        assert_eq!(sp("%+010.2f", &[&2.5]), "+000002.50");
        assert_eq!(sp("%.1e", &[&10]), "1.0e+1");
        assert_eq!(sp("%e", &[&10]), "1.000000e+1");
        assert_eq!(sp("%.0e", &[&10]), "1e+1");
        assert_eq!(sp("%E", &[&0.000_123]), "1.230000E-4");
        assert_eq!(sp("%.2e", &[&0]), "0.00e+0");
        assert_eq!(sp("%.3e", &[&-1234.5678]), "-1.235e+3");
        assert_eq!(sp("%g", &[&0.000_012_3]), "1.23e-5");
        assert_eq!(sp("%g", &[&123_456_789.0]), "1.23457e+8");
        assert_eq!(sp("%G", &[&123_456_789.0]), "1.23457E+8");
        assert_eq!(sp("%g", &[&100_000.0]), "100000");
        assert_eq!(sp("%g", &[&1_000_000.0]), "1.0e+6");
        assert_eq!(sp("%.3g", &[&3.14158]), "3.14");
        assert_eq!(sp("%.0g", &[&3.14158]), "3");
        assert_eq!(sp("%g", &[&-0.0]), "-0");
        assert_eq!(sp("%010g", &[&-1.5]), "-0000001.5");
        assert_eq!(sp("%+g", &[&1.5]), "+1.5");
        assert_eq!(sp("%h|%H", &[&1e20, &1e20]), "1.0e+20|1.0E+20");
        assert_eq!(sp("%.*H", &[&-1, &0.1]), "0.1");
        assert_eq!(sp("%.*g", &[&-1, &(0.1 + 0.2)]), "0.30000000000000004");
        assert_eq!(sp("%f", &[&"1.5abc"]), "1.500000");
        assert_eq!(sp("%.15f", &[&0.1]), "0.100000000000000");
        assert_eq!(sp("%.20f", &[&0.1]), "0.10000000000000000555");
        assert_eq!(sp("%f", &[&1e15]), "1000000000000000.000000");
    }

    #[test]
    fn non_finite() {
        assert_eq!(sp("[%10f]", &[&f64::INFINITY]), "[INF]");
        // php-src master prints "-INF" (8.3 printed "INF").
        assert_eq!(sp("[%-10F]", &[&f64::NEG_INFINITY]), "[-INF]");
        assert_eq!(sp("[%010.2e]", &[&f64::NAN]), "[NaN]");
        assert_eq!(sp("[%+f]", &[&f64::INFINITY]), "[INF]");
        assert_eq!(sp("[%5.1g]", &[&f64::NAN]), "[NaN]");
        // The '0' padding sign quirk eats the first character.
        assert_eq!(sp("[%+05f]", &[&f64::INFINITY]), "[+NF]");
        assert_eq!(sp("[%05f]", &[&f64::NEG_INFINITY]), "[-INF]");
    }

    #[test]
    fn star_width_precision() {
        assert_eq!(sp("[%*d]", &[&5, &42]), "[   42]");
        assert_eq!(sp("[%-*d]", &[&5, &42]), "[42   ]");
        assert_eq!(sp("[%.*f]", &[&2, &3.14158]), "[3.14]");
        assert_eq!(sp("[%*.*f]", &[&8, &2, &3.14158]), "[    3.14]");
        assert_eq!(sp("[%3$*1$.*2$f]", &[&8, &2, &3.14158]), "[    3.14]");
        assert_eq!(
            err("%*d", &[&"5", &1]),
            FormatError::ValueError("Width must be an integer".into())
        );
        assert_eq!(
            err("%.*f", &[&1.5, &1]),
            FormatError::ValueError("Precision must be an integer".into())
        );
        assert_eq!(
            err("%*d", &[&-1, &1]),
            FormatError::ValueError("Width must be between 0 and 2147483647".into())
        );
        assert_eq!(
            err("%.*f", &[&-2, &1.0]),
            FormatError::ValueError("Precision must be between -1 and 2147483647".into())
        );
        assert_eq!(
            err("%.*f", &[&-1, &1.0]),
            FormatError::ValueError("Precision -1 is only supported for %g, %G, %h and %H".into())
        );
        assert_eq!(
            err("%*d", &[&5]),
            FormatError::ArgumentCountError("3 arguments are required, 2 given".into())
        );
    }

    #[test]
    fn errors() {
        assert_eq!(
            err("%y", &[&1]),
            FormatError::ValueError("Unknown format specifier \"y\"".into())
        );
        assert_eq!(
            err("%", &[&1]),
            FormatError::ValueError("Missing format specifier at end of string".into())
        );
        assert_eq!(
            err("%5", &[&1]),
            FormatError::ValueError("Missing format specifier at end of string".into())
        );
        assert_eq!(
            err("%", &[]),
            FormatError::ArgumentCountError("2 arguments are required, 1 given".into())
        );
        assert_eq!(
            err("%d", &[]),
            FormatError::ArgumentCountError("2 arguments are required, 1 given".into())
        );
        assert_eq!(
            err("%d %d %d", &[&1]),
            FormatError::ArgumentCountError("4 arguments are required, 2 given".into())
        );
        assert_eq!(
            err("%3$d", &[&1]),
            FormatError::ArgumentCountError("4 arguments are required, 2 given".into())
        );
        assert_eq!(
            err("%0$d", &[&1]),
            FormatError::ValueError(
                "Argument number specifier must be greater than zero and less than 2147483647"
                    .into()
            )
        );
        assert_eq!(
            err("%$d", &[&1]),
            FormatError::ValueError("Argument number specifier must not be empty".into())
        );
        assert_eq!(
            err("%'", &[&1]),
            FormatError::ValueError("Missing padding character".into())
        );
        assert_eq!(
            err("%9999999999d", &[&1]),
            FormatError::ValueError("Width must be between 0 and 2147483647".into())
        );
        assert_eq!(
            err("%.9999999999f", &[&1]),
            FormatError::ValueError("Precision must be between 0 and 2147483647".into())
        );
        assert_eq!(
            err("a\0b%\0", &[&1]),
            FormatError::ValueError("Unknown format specifier \"\0\"".into())
        );
        let mut d: Vec<Diagnostic> = Vec::new();
        assert_eq!(
            vsprintf(b"%d %d", &[&1], &mut d),
            Err(FormatError::ValueError(
                "The arguments array must contain 2 items, 1 given".into()
            ))
        );
        assert_eq!(
            php_formatted_print(
                b"%d",
                &[],
                FormatOptions {
                    source: ArgSource::Variadic(2),
                    decimal_point: b'.'
                },
                &mut d
            ),
            Err(FormatError::ArgumentCountError(
                "3 arguments are required, 2 given".into()
            ))
        );
        assert_eq!(
            err("%y", &[&1]).to_string(),
            "ValueError: Unknown format specifier \"y\""
        );
    }

    #[test]
    fn precision_notice() {
        let mut d: Vec<Diagnostic> = Vec::new();
        let r = sprintf(b"%.60f", &[&1.0], &mut d).unwrap();
        assert_eq!(r.len(), 2 + 53);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].render(Some("sprintf")),
            "sprintf(): Requested precision of 60 digits was truncated to PHP maximum of 53 digits"
        );
        assert_eq!(d[0].level, ErrorLevel::NOTICE);
    }

    #[test]
    fn locale_decimal_point() {
        let mut d: Vec<Diagnostic> = Vec::new();
        let opts = FormatOptions {
            decimal_point: b',',
            ..FormatOptions::default()
        };
        let r = php_formatted_print(
            b"%.2f %.2F %g %e %H",
            &[&1.5, &1.5, &1.5, &1.5, &1.5],
            opts,
            &mut d,
        )
        .unwrap();
        assert_eq!(r, b"1,50 1.50 1,5 1.500000e+0 1.5");
    }
}
