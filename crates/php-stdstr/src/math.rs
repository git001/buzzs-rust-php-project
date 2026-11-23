// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Pure helpers behind `ext/standard/math.c`: base conversion
//! (`base_convert`, `bindec`/`hexdec`/`octdec`, `decbin`/`dechex`/`decoct`),
//! the argument handling of `round()`, and the small arithmetic functions
//! whose error behaviour is worth centralising (`intdiv`, `%`, `fmod`,
//! `fdiv`, `fpow`, `abs`, `floor`/`ceil`, `log`, `hypot`, `deg2rad`,
//! `rad2deg`).
//!
//! The float rounding algorithm itself (`_php_math_round`) lives in
//! `php_support::fmt::number::php_round`; this module only provides the
//! integer short-cut and argument validation of `round()` around it.
//! `number_format()` likewise lives in `php-support`.
//!
//! Trigonometric / exponential wrappers (`sin`, `exp`, `sqrt`, …) are plain
//! libm calls on the `Z_PARAM_DOUBLE`-coerced argument; the VM can call the
//! corresponding `f64` methods directly.

use crate::error::{ErrorKind, Level, Notice, Notices, StdError, StdResult};

/// An `int|float` result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// `IS_LONG`
    Int(i64),
    /// `IS_DOUBLE`
    Float(f64),
}

impl Number {
    /// `zval_get_double()`.
    #[allow(clippy::cast_precision_loss)]
    pub fn to_f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }
}

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// C `isspace()` in the "C" locale.
const fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Value of an alphanumeric digit (`0-9`, `A-Z`, `a-z`), if any.
const fn digit_value(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 10),
        b'a'..=b'z' => Some((c - b'a') as u32 + 10),
        _ => None,
    }
}

/// A diagnostic raised with `zend_error()` (no function-name prefix).
fn zend_error(level: Level, message: &str) -> Notice {
    Notice {
        level,
        func: None,
        message: message.to_owned(),
    }
}

/// `_php_math_basetozval()` — parses a base-`base` (2..=36) number.
///
/// Leading/trailing C whitespace is skipped, a `0x`/`0o`/`0b` prefix is
/// accepted for bases 16/8/2, any other non-digit byte is ignored with the
/// `E_DEPRECATED` "Invalid characters passed for attempted conversion, these
/// have been ignored", and values above `PHP_INT_MAX` continue as a float
/// after the `E_NOTICE` "Input number is larger than `PHP_INT_MAX`, precision
/// has been lost in conversion" (php-src master; PHP 8.3 is silent). Both
/// are raised with `zend_error`, i.e. without a function-name prefix.
///
/// A `base` outside 2..=36 is a caller bug; it is clamped.
pub fn base_to_number(s: &[u8], base: u32, notices: &mut Notices) -> Number {
    let base = base.clamp(2, 36);
    let mut start = 0;
    let mut end = s.len();
    while start < end && is_c_space(s[start]) {
        start += 1;
    }
    while start < end && is_c_space(s[end - 1]) {
        end -= 1;
    }
    let mut s = &s[start..end];
    if s.len() >= 2 && s[0] == b'0' {
        let p = s[1].to_ascii_lowercase();
        if (base == 16 && p == b'x') || (base == 8 && p == b'o') || (base == 2 && p == b'b') {
            s = &s[2..];
        }
    }

    let base_l = i64::from(base);
    let cutoff = i64::MAX / base_l;
    let cutlim = i64::MAX % base_l;
    let mut num: i64 = 0;
    let mut fnum: f64 = 0.0;
    let mut is_float = false;
    let mut invalid = false;

    for &c in s {
        let Some(d) = digit_value(c).filter(|&d| d < base) else {
            invalid = true;
            continue;
        };
        if !is_float {
            let d = i64::from(d);
            if num < cutoff || (num == cutoff && d <= cutlim) {
                num = num * base_l + d;
                continue;
            }
            notices.push(zend_error(
                Level::Notice,
                "Input number is larger than PHP_INT_MAX, precision has been lost in conversion",
            ));
            #[allow(clippy::cast_precision_loss)]
            {
                fnum = num as f64;
            }
            is_float = true;
        }
        fnum = fnum * f64::from(base) + f64::from(d);
    }

    if invalid {
        notices.push(zend_error(
            Level::Deprecated,
            "Invalid characters passed for attempted conversion, these have been ignored",
        ));
    }
    if is_float {
        Number::Float(fnum)
    } else {
        Number::Int(num)
    }
}

/// `bindec(string $binary_string): int|float`.
///
/// VM coercion: `Z_PARAM_STR` (the argument is converted to a string).
pub fn bindec(s: &[u8], notices: &mut Notices) -> Number {
    base_to_number(s, 2, notices)
}

/// `hexdec(string $hex_string): int|float`. VM coercion: `Z_PARAM_STR`.
pub fn hexdec(s: &[u8], notices: &mut Notices) -> Number {
    base_to_number(s, 16, notices)
}

/// `octdec(string $octal_string): int|float`. VM coercion: `Z_PARAM_STR`.
pub fn octdec(s: &[u8], notices: &mut Notices) -> Number {
    base_to_number(s, 8, notices)
}

/// `_php_math_basetolong()` — the legacy `PHPAPI` parser (no whitespace or
/// prefix handling; invalid digits skipped silently).  On overflow it warns
/// `func(): Number %s is too big to fit in long` (with the remainder of the
/// input after the offending digit) and returns `PHP_INT_MAX`.
///
/// Not reachable from a PHP function in php-src itself; provided for
/// extensions that call it.  Returns 0 for a base outside 2..=36.
pub fn base_to_long(s: &[u8], base: u32, func: &'static str, notices: &mut Notices) -> i64 {
    if !(2..=36).contains(&base) {
        return 0;
    }
    let base_l = i64::from(base);
    let mut num: i64 = 0;
    for (i, &c) in s.iter().enumerate() {
        let Some(d) = digit_value(c).filter(|&d| d < base) else {
            continue;
        };
        let onum = num;
        num = num.wrapping_mul(base_l).wrapping_add(i64::from(d));
        if num > onum {
            continue;
        }
        // C's "%s" stops at a NUL byte.
        let rest = &s[i + 1..];
        let rest = rest.split(|&b| b == 0).next().unwrap_or_default();
        notices.push(Notice::warning(
            func,
            format!(
                "Number {} is too big to fit in long",
                String::from_utf8_lossy(rest)
            ),
        ));
        return i64::MAX;
    }
    num
}

/// `_php_math_longtobase()` — `arg` reinterpreted as unsigned, in base
/// `base` (2..=36; empty string otherwise).
pub fn long_to_base(arg: i64, base: u32) -> Vec<u8> {
    if !(2..=36).contains(&base) {
        return Vec::new();
    }
    let mut value = u64::from_ne_bytes(arg.to_ne_bytes());
    let base = u64::from(base);
    #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
    let mut out = Vec::with_capacity(64);
    loop {
        out.push(DIGITS[usize::try_from(value % base).unwrap_or(0)]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    out
}

/// `decbin(int $num): string` — two's complement for negatives.
/// VM coercion: `Z_PARAM_LONG`.
pub fn decbin(num: i64) -> Vec<u8> {
    long_to_base(num, 2)
}

/// `decoct(int $num): string`. VM coercion: `Z_PARAM_LONG`.
pub fn decoct(num: i64) -> Vec<u8> {
    long_to_base(num, 8)
}

/// `dechex(int $num): string`. VM coercion: `Z_PARAM_LONG`.
pub fn dechex(num: i64) -> Vec<u8> {
    long_to_base(num, 16)
}

/// `ZEND_DOUBLE_MAX_LENGTH` (`3 + DBL_MANT_DIG - DBL_MIN_EXP`): the float
/// path of `_php_math_zvaltobase` emits at most one less digit.
const DOUBLE_MAX_LENGTH: usize = 3 + 53 + 1021;

/// `_php_math_zvaltobase()` — formats an int (as unsigned) or a float
/// (floored, digit by digit with `fmod`) in base `base` (2..=36; empty
/// string otherwise).
///
/// Throws the un-prefixed `ValueError` "An infinite value cannot be
/// converted to base N" for ±INF.
pub fn number_to_base(arg: Number, base: u32) -> StdResult<Vec<u8>> {
    if !(2..=36).contains(&base) {
        return Ok(Vec::new());
    }
    let f = match arg {
        Number::Int(i) => return Ok(long_to_base(i, base)),
        Number::Float(f) => f,
    };
    let mut fvalue = f.floor();
    if fvalue.is_infinite() {
        return Err(StdError::new(
            ErrorKind::ValueError,
            format!("An infinite value cannot be converted to base {base}"),
        ));
    }
    let fbase = f64::from(base);
    let mut out = Vec::new();
    loop {
        // `(int) fmod(fvalue, base)`: always in 0..base for the
        // non-negative finite values base_convert produces.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = ((fvalue % fbase).abs() as usize).min(35);
        out.push(DIGITS[idx]);
        fvalue /= fbase;
        if out.len() >= DOUBLE_MAX_LENGTH - 1 || fvalue.abs() < 1.0 {
            break;
        }
    }
    out.reverse();
    Ok(out)
}

/// `base_convert(string $num, int $from_base, int $to_base): string`.
///
/// VM coercion: `Z_PARAM_STR` for `$num`, `Z_PARAM_LONG` for the bases.
pub fn base_convert(
    num: &[u8],
    from_base: i64,
    to_base: i64,
    notices: &mut Notices,
) -> StdResult<Vec<u8>> {
    const MSG: &str = "must be between 2 and 36 (inclusive)";
    let from = u32::try_from(from_base)
        .ok()
        .filter(|b| (2..=36).contains(b))
        .ok_or_else(|| StdError::arg_value("base_convert", 2, "from_base", MSG))?;
    let to = u32::try_from(to_base)
        .ok()
        .filter(|b| (2..=36).contains(b))
        .ok_or_else(|| StdError::arg_value("base_convert", 3, "to_base", MSG))?;
    let n = base_to_number(num, from, notices);
    number_to_base(n, to)
}

/// PHP's rounding modes (`PHP_ROUND_*` constants and the `RoundingMode`
/// enum cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoundingMode {
    /// `PHP_ROUND_HALF_UP` (1) / `RoundingMode::HalfAwayFromZero`.
    HalfUp,
    /// `PHP_ROUND_HALF_DOWN` (2) / `RoundingMode::HalfTowardsZero`.
    HalfDown,
    /// `PHP_ROUND_HALF_EVEN` (3) / `RoundingMode::HalfEven`.
    HalfEven,
    /// `PHP_ROUND_HALF_ODD` (4) / `RoundingMode::HalfOdd`.
    HalfOdd,
    /// `PHP_ROUND_CEILING` (5) / `RoundingMode::PositiveInfinity`.
    Ceiling,
    /// `PHP_ROUND_FLOOR` (6) / `RoundingMode::NegativeInfinity`.
    Floor,
    /// `PHP_ROUND_TOWARD_ZERO` (7) / `RoundingMode::TowardsZero`.
    TowardZero,
    /// `PHP_ROUND_AWAY_FROM_ZERO` (8) / `RoundingMode::AwayFromZero`.
    AwayFromZero,
}

impl RoundingMode {
    /// The internal `PHP_ROUND_*` integer value (only the `HALF_*` modes
    /// 1–4 are exposed as PHP constants).
    pub const fn as_long(self) -> i64 {
        match self {
            Self::HalfUp => 1,
            Self::HalfDown => 2,
            Self::HalfEven => 3,
            Self::HalfOdd => 4,
            Self::Ceiling => 5,
            Self::Floor => 6,
            Self::TowardZero => 7,
            Self::AwayFromZero => 8,
        }
    }

    /// Validates an integer `$mode` argument of `round()`.
    ///
    /// Throws `round(): Argument #3 ($mode) must be a valid rounding mode
    /// (RoundingMode::*)` for anything but 1..=8.
    pub fn from_long(mode: i64) -> StdResult<Self> {
        Ok(match mode {
            1 => Self::HalfUp,
            2 => Self::HalfDown,
            3 => Self::HalfEven,
            4 => Self::HalfOdd,
            5 => Self::Ceiling,
            6 => Self::Floor,
            7 => Self::TowardZero,
            8 => Self::AwayFromZero,
            _ => {
                return Err(StdError::arg_value(
                    "round",
                    3,
                    "mode",
                    "must be a valid rounding mode (RoundingMode::*)",
                ))
            }
        })
    }

    /// `php_math_round_mode_from_enum()` — maps a `RoundingMode` enum case
    /// name to the mode.
    pub fn from_enum_case(name: &str) -> Option<Self> {
        Some(match name {
            "HalfAwayFromZero" => Self::HalfUp,
            "HalfTowardsZero" => Self::HalfDown,
            "HalfEven" => Self::HalfEven,
            "HalfOdd" => Self::HalfOdd,
            "TowardsZero" => Self::TowardZero,
            "AwayFromZero" => Self::AwayFromZero,
            "NegativeInfinity" => Self::Floor,
            "PositiveInfinity" => Self::Ceiling,
            _ => return None,
        })
    }
}

/// `round()`'s `$precision` → `places` conversion: saturates to the C
/// `int` range.
pub fn round_places(precision: i64) -> i32 {
    i32::try_from(precision).unwrap_or(if precision < 0 { i32::MIN } else { i32::MAX })
}

/// The integer fast path of `round(int $num, int $precision, …)`.
///
/// Returns `Some(float)` when PHP returns `(float) $num` unchanged
/// (`places >= 0`); `None` when the VM must call
/// `php_support::fmt::number::php_round((float) $num, places, mode)`
/// (negative places), exactly like `PHP_FUNCTION(round)`'s fallthrough.
#[allow(clippy::cast_precision_loss)]
pub fn round_long(num: i64, places: i32) -> Option<f64> {
    (places >= 0).then_some(num as f64)
}

/// `abs(int|float $num)`: `abs(PHP_INT_MIN)` is the float `2**63`.
/// VM coercion: `Z_PARAM_NUMBER`.
#[allow(clippy::cast_precision_loss)]
pub fn abs(num: Number) -> Number {
    match num {
        Number::Int(i) => i
            .checked_abs()
            .map_or(Number::Float(-(i as f64)), Number::Int),
        Number::Float(f) => Number::Float(f.abs()),
    }
}

/// `floor(int|float $num): float`. VM coercion: `Z_PARAM_NUMBER`.
pub fn floor(num: Number) -> f64 {
    match num {
        Number::Int(_) => num.to_f64(),
        Number::Float(f) => f.floor(),
    }
}

/// `ceil(int|float $num): float`. VM coercion: `Z_PARAM_NUMBER`.
pub fn ceil(num: Number) -> f64 {
    match num {
        Number::Int(_) => num.to_f64(),
        Number::Float(f) => f.ceil(),
    }
}

/// `fmod(float $num1, float $num2): float` — C `fmod()` (result has the
/// sign of the dividend; NAN for a zero divisor). VM coercion:
/// `Z_PARAM_DOUBLE`.
pub fn fmod(num1: f64, num2: f64) -> f64 {
    num1 % num2
}

/// `fdiv(float $num1, float $num2): float` — IEEE-754 division (±INF/NAN
/// on division by zero). VM coercion: `Z_PARAM_DOUBLE`.
pub fn fdiv(num1: f64, num2: f64) -> f64 {
    num1 / num2
}

/// `fpow(float $num, float $exponent): float` (php-src master) —
/// IEEE-754 `pow()`. VM coercion: `Z_PARAM_DOUBLE`.
pub fn fpow(num: f64, exponent: f64) -> f64 {
    num.powf(exponent)
}

/// `intdiv(int $num1, int $num2): int`.
///
/// Throws `DivisionByZeroError: Division by zero` and
/// `ArithmeticError: Division of PHP_INT_MIN by -1 is not an integer`
/// (both without a function prefix). VM coercion: `Z_PARAM_LONG`.
pub fn intdiv(num1: i64, num2: i64) -> StdResult<i64> {
    if num2 == 0 {
        return Err(StdError::new(
            ErrorKind::DivisionByZeroError,
            "Division by zero",
        ));
    }
    num1.checked_div(num2).ok_or_else(|| {
        StdError::new(
            ErrorKind::ArithmeticError,
            "Division of PHP_INT_MIN by -1 is not an integer",
        )
    })
}

/// The integer core of the `%` operator (`mod_function`), after both
/// operands were converted to int: `DivisionByZeroError: Modulo by zero`
/// for a zero divisor, `0` for a divisor of `-1` (avoiding the
/// `PHP_INT_MIN % -1` trap), C remainder semantics otherwise.
pub fn int_mod(num1: i64, num2: i64) -> StdResult<i64> {
    if num2 == 0 {
        return Err(StdError::new(
            ErrorKind::DivisionByZeroError,
            "Modulo by zero",
        ));
    }
    if num2 == -1 {
        return Ok(0);
    }
    Ok(num1 % num2)
}

/// `log(float $num, float $base = M_E): float`.
///
/// `base == None` means the argument was not passed. Bases 2 and 10 use
/// `log2`/`log10`, base 1 yields NAN, and a base `<= 0` throws
/// `log(): Argument #2 ($base) must be greater than 0`. VM coercion:
/// `Z_PARAM_DOUBLE` for both.
pub fn log(num: f64, base: Option<f64>) -> StdResult<f64> {
    let Some(base) = base else {
        return Ok(num.ln());
    };
    #[allow(clippy::float_cmp)]
    {
        if base == 2.0 {
            return Ok(num.log2());
        }
        if base == 10.0 {
            return Ok(num.log10());
        }
        if base == 1.0 {
            return Ok(f64::NAN);
        }
    }
    if base <= 0.0 {
        return Err(StdError::arg_value(
            "log",
            2,
            "base",
            "must be greater than 0",
        ));
    }
    Ok(num.ln() / base.ln())
}

/// `hypot(float $x, float $y): float` (libm `hypot`). VM coercion:
/// `Z_PARAM_DOUBLE`.
pub fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// `deg2rad(float $num): float` — `(deg / 180.0) * M_PI`.
pub fn deg2rad(num: f64) -> f64 {
    (num / 180.0) * std::f64::consts::PI
}

/// `rad2deg(float $num): float` — `(rad / M_PI) * 180`.
pub fn rad2deg(num: f64) -> f64 {
    (num / std::f64::consts::PI) * 180.0
}

/// The C library's inverse hyperbolic functions, which
/// `PHP_FUNCTION(asinh)`, `acosh` and `atanh` call. Rust's `f64::asinh`,
/// `acosh` and `atanh` are std's own formulas over `ln_1p`/`hypot`, not
/// libm's, and differ from it in the last bit for many arguments
/// (`asinh(1 / 997)`), so PHP's results come from libm here.
mod libm {
    // libm is the C math library every std binary on this target already
    // links (`f64::sin`, `f64::sinh` & co. call into it).
    #[link(name = "m")]
    extern "C" {
        pub fn asinh(x: f64) -> f64;
        pub fn acosh(x: f64) -> f64;
        pub fn atanh(x: f64) -> f64;
    }
}

/// `asinh(float $num): float` (libm `asinh`).
pub fn asinh(num: f64) -> f64 {
    // SAFETY: a pure C math function of a double: no pointers, no global
    // state beyond `errno`, defined for every input (NAN/INF included).
    unsafe { libm::asinh(num) }
}

/// `acosh(float $num): float` (libm `acosh`; NAN below 1).
pub fn acosh(num: f64) -> f64 {
    // SAFETY: as for `asinh`.
    unsafe { libm::acosh(num) }
}

/// `atanh(float $num): float` (libm `atanh`; ±INF at ±1, NAN beyond).
pub fn atanh(num: f64) -> f64 {
    // SAFETY: as for `asinh`.
    unsafe { libm::atanh(num) }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// libm's results, which Rust's `f64::asinh`/`acosh`/`atanh` miss by
    /// one bit for these arguments (`/usr/bin/php -n`).
    #[test]
    fn inverse_hyperbolics_are_libms() {
        assert_eq!(asinh(0.044_318_560_479_840_056), 0.044_304_065_350_020_95);
        assert_eq!(asinh(0.099_966_677_774_075_31), 0.099_800_921_990_461_77);
        assert_eq!(acosh(1.001_003_009_027_081_2), 0.044_784_850_133_250_17);
        assert_eq!(acosh(1.002_006_018_054_162_6), 0.063_330_051_760_081_58);
        assert_eq!(
            atanh(0.002_332_555_814_728_423_7),
            0.002_332_560_045_078_388
        );
        assert_eq!(
            atanh(0.002_665_778_073_975_341_6),
            0.002_665_784_388_673_144_5
        );
        assert_eq!(asinh(1.0), 0.881_373_587_019_543);
        assert_eq!(acosh(2.0), 1.316_957_896_924_816_6);
        assert_eq!(asinh(f64::NEG_INFINITY), f64::NEG_INFINITY);
        assert!(asinh(-0.0).is_sign_negative());
        assert!(acosh(0.0).is_nan() && acosh(f64::NAN).is_nan());
        assert_eq!(acosh(1.0), 0.0);
        assert_eq!(atanh(-1.0), f64::NEG_INFINITY);
        assert!(atanh(2.0).is_nan() && atanh(f64::INFINITY).is_nan());
    }

    fn conv(s: &[u8], base: u32) -> (Number, Vec<Notice>) {
        let mut n = Vec::new();
        let r = base_to_number(s, base, &mut n);
        (r, n)
    }

    #[test]
    fn basetozval() {
        assert_eq!(conv(b"FF", 16).0, Number::Int(255));
        assert_eq!(conv(b" 0xFf\t\n", 16), (Number::Int(255), vec![]));
        assert_eq!(conv(b"0b1010", 2).0, Number::Int(10));
        assert_eq!(conv(b"0o17", 8).0, Number::Int(15));
        let (r, n) = conv(b"fg", 16);
        assert_eq!(r, Number::Int(15));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].level, Level::Deprecated);
        assert_eq!(n[0].func, None);
        let (r, n) = conv(b"17fffffffffffffff", 16);
        assert_eq!(r, Number::Float(2.767_011_611_056_432_7E19));
        assert_eq!(n[0].level, Level::Notice);
        assert_eq!(conv(&[b'1'; 63], 2).0, Number::Int(i64::MAX));
    }

    #[test]
    fn longtobase() {
        assert_eq!(dechex(-1), b"ffffffffffffffff");
        assert_eq!(decbin(0), b"0");
        assert_eq!(decoct(8), b"10");
        assert_eq!(decbin(i64::MIN).len(), 64);
        assert_eq!(long_to_base(35, 36), b"z");
        assert_eq!(long_to_base(35, 37), b"");
    }

    #[test]
    fn zvaltobase() {
        assert_eq!(number_to_base(Number::Float(255.9), 16).unwrap(), b"ff");
        assert_eq!(number_to_base(Number::Float(0.0), 2).unwrap(), b"0");
        let e = number_to_base(Number::Float(f64::INFINITY), 7).unwrap_err();
        assert_eq!(
            e.to_string(),
            "An infinite value cannot be converted to base 7"
        );
        let mut n = Vec::new();
        let r = base_convert(&[b'1'; 61], 36, 16, &mut n).unwrap();
        assert_eq!(r.len(), 78);
        assert!(r.starts_with(b"4b61b5e0639ff"));
        assert_eq!(
            base_convert(b"1", 1, 10, &mut n).unwrap_err().to_string(),
            "base_convert(): Argument #2 ($from_base) must be between 2 and 36 (inclusive)"
        );
        assert_eq!(
            base_convert(b"1", 10, 37, &mut n).unwrap_err().to_string(),
            "base_convert(): Argument #3 ($to_base) must be between 2 and 36 (inclusive)"
        );
    }

    #[test]
    fn basetolong() {
        let mut n = Vec::new();
        assert_eq!(base_to_long(b"ff", 16, "f", &mut n), 255);
        assert_eq!(base_to_long(&[b'f'; 17], 16, "f", &mut n), i64::MAX);
        assert_eq!(n[0].to_string(), "f(): Number f is too big to fit in long");
        assert_eq!(base_to_long(b"1", 1, "f", &mut n), 0);
    }

    #[test]
    fn rounding_args() {
        assert_eq!(RoundingMode::from_long(3).unwrap(), RoundingMode::HalfEven);
        assert_eq!(RoundingMode::from_long(8).unwrap().as_long(), 8);
        assert_eq!(
            RoundingMode::from_long(1234).unwrap_err().to_string(),
            "round(): Argument #3 ($mode) must be a valid rounding mode (RoundingMode::*)"
        );
        assert_eq!(
            RoundingMode::from_enum_case("PositiveInfinity"),
            Some(RoundingMode::Ceiling)
        );
        assert_eq!(RoundingMode::from_enum_case("Nope"), None);
        assert_eq!(round_places(9_876_543_210), i32::MAX);
        assert_eq!(round_places(-9_876_543_210), i32::MIN);
        assert_eq!(round_places(-3), -3);
        assert_eq!(round_long(15, 0), Some(15.0));
        assert_eq!(round_long(15, -1), None);
    }

    #[test]
    fn arithmetic() {
        assert_eq!(intdiv(7, -2).unwrap(), -3);
        assert_eq!(
            intdiv(1, 0).unwrap_err().kind,
            ErrorKind::DivisionByZeroError
        );
        assert_eq!(
            intdiv(i64::MIN, -1).unwrap_err().to_string(),
            "Division of PHP_INT_MIN by -1 is not an integer"
        );
        assert_eq!(int_mod(i64::MIN, -1).unwrap(), 0);
        assert_eq!(int_mod(-7, 3).unwrap(), -1);
        assert_eq!(int_mod(1, 0).unwrap_err().to_string(), "Modulo by zero");
        assert_eq!(
            abs(Number::Int(i64::MIN)),
            Number::Float(9.223_372_036_854_776E18)
        );
        assert_eq!(abs(Number::Int(-3)), Number::Int(3));
        assert!(fmod(1.0, 0.0).is_nan());
        assert_eq!(fmod(-7.5, 2.0), -1.5);
        assert_eq!(fdiv(1.0, -0.0), f64::NEG_INFINITY);
        assert!(log(1.0, Some(1.0)).unwrap().is_nan());
        assert_eq!(
            log(36.0, Some(-4.0)).unwrap_err().to_string(),
            "log(): Argument #2 ($base) must be greater than 0"
        );
        assert_eq!(log(8.0, Some(2.0)).unwrap(), 3.0);
        assert_eq!(floor(Number::Int(5)), 5.0);
        assert_eq!(ceil(Number::Float(-0.5)), -0.0);
    }
}
