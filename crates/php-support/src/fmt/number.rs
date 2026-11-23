// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `round()` (`_php_math_round()`) and `number_format()`
//! (`_php_math_number_format_ex()` / `_php_math_number_format_long()`) from
//! `ext/standard/math.c`.

// Exact float comparisons are intentional: they mirror the C code.
#![allow(clippy::float_cmp)]

use super::double::php_conv_fp;

/// PHP rounding modes (`PHP_ROUND_*` and the `RoundingMode` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingMode {
    /// `PHP_ROUND_HALF_UP` (away from zero on ties).
    HalfUp,
    /// `PHP_ROUND_HALF_DOWN` (toward zero on ties).
    HalfDown,
    /// `PHP_ROUND_HALF_EVEN`.
    HalfEven,
    /// `PHP_ROUND_HALF_ODD`.
    HalfOdd,
    /// `RoundingMode::AwayFromZero`.
    AwayFromZero,
    /// `RoundingMode::TowardsZero`.
    TowardsZero,
    /// `RoundingMode::PositiveInfinity` (ceiling).
    PositiveInfinity,
    /// `RoundingMode::NegativeInfinity` (floor).
    NegativeInfinity,
}

/// `php_intpow10()`.
fn intpow10(power: i32) -> f64 {
    const POWERS: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    usize::try_from(power)
        .ok()
        .and_then(|p| POWERS.get(p).copied())
        .unwrap_or_else(|| 10f64.powi(power))
}

fn basic_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    let v = integral + 0.5f64.copysign(integral);
    if places > 0 {
        (v / exponent).abs()
    } else {
        (v * exponent).abs()
    }
}

fn zero_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    if places > 0 {
        (integral / exponent).abs()
    } else {
        (integral * exponent).abs()
    }
}

/// `php_round_helper()`.
fn round_helper(integral: f64, value: f64, exponent: f64, places: i32, mode: RoundingMode) -> f64 {
    let value_abs = value.abs();
    let away = integral + 1.0f64.copysign(integral);
    match mode {
        RoundingMode::HalfUp => {
            if value_abs >= basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        RoundingMode::HalfDown => {
            if value_abs > basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        RoundingMode::PositiveInfinity => {
            if value > 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral + 1.0
            } else {
                integral
            }
        }
        RoundingMode::NegativeInfinity => {
            if value < 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral - 1.0
            } else {
                integral
            }
        }
        RoundingMode::TowardsZero => integral,
        RoundingMode::AwayFromZero => {
            if value_abs > zero_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        RoundingMode::HalfEven | RoundingMode::HalfOdd => {
            let edge = basic_edge(integral, exponent, places);
            if value_abs > edge {
                away
            } else if value_abs == edge {
                let even = integral % 2.0 == 0.0;
                if even == (mode == RoundingMode::HalfOdd) {
                    away
                } else {
                    integral
                }
            } else {
                integral
            }
        }
    }
}

/// `_php_math_round(value, places, mode)` — PHP's `round()` for floats.
///
/// For `|places| >= 23` PHP round-trips through its own `snprintf("%15fe%d")`,
/// which writes the current locale's decimal point (see
/// [`super::locale::decimal_point`]), and `zend_strtod()`.
pub fn php_round(value: f64, places: i32, mode: RoundingMode) -> f64 {
    php_round_dp(value, places, mode, super::locale::decimal_point())
}

/// [`php_round`] with an explicit `LCONV_DECIMAL_POINT`.
fn php_round_dp(value: f64, places: i32, mode: RoundingMode, dec_point: u8) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    if places == 0 && value == value.trunc() {
        return value;
    }
    let places = places.max(i32::MIN + 1);
    let exponent = intpow10(places.abs());

    let (mut tmp, tmp2) = if value >= 0.0 {
        let t = if places > 0 {
            value * exponent
        } else {
            value / exponent
        }
        .floor();
        (t, t + 1.0)
    } else {
        let t = if places > 0 {
            value * exponent
        } else {
            value / exponent
        }
        .ceil();
        (t, t - 1.0)
    };
    if (if places > 0 {
        tmp2 / exponent
    } else {
        tmp2 * exponent
    }) == value
    {
        tmp = tmp2;
    }
    if tmp.abs() >= 1e16 {
        return value;
    }
    tmp = round_helper(tmp, value, exponent, places, mode);
    if places.abs() < 23 {
        if places > 0 {
            tmp / exponent
        } else {
            tmp * exponent
        }
    } else {
        // snprintf("%15fe%d") + zend_strtod() round trip. PHP's snprintf
        // writes `%f` with the locale's decimal point, where zend_strtod()
        // stops: with a decimal comma only the integral digits survive
        // (round(1.5e-20, 25) is 150000.0 under de_DE, as in php-src).
        let s = format!("{tmp:.6}e{}", -i64::from(places));
        let s = if dec_point == b'.' {
            s.as_str()
        } else {
            s.split('.').next().unwrap_or_default()
        };
        match s.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            _ => value,
        }
    }
}

/// Inserts `thousand_sep` every three digits of `int_digits`.
fn group(int_digits: &[u8], thousand_sep: &[u8], out: &mut Vec<u8>) {
    let n = int_digits.len();
    for (i, &c) in int_digits.iter().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            out.extend_from_slice(thousand_sep);
        }
        out.push(c);
    }
}

/// `_php_math_number_format_ex()` — `number_format()` for a float.
///
/// Note that `number_format()` itself dispatches large integral floats
/// (|d| ≥ 2^52 that fit in an int) to [`number_format_long`];
/// [`number_format`] handles that dispatch.
pub fn number_format_double(d: f64, dec: i32, dec_point: &[u8], thousand_sep: &[u8]) -> Vec<u8> {
    let mut is_negative = d < 0.0;
    let d = php_round(d.abs(), dec, RoundingMode::HalfUp);
    let dec = dec.max(0);
    // strpprintf("%.*F"): NaN/Inf print in lower case without sign.
    let tmp: Vec<u8> = if d.is_nan() {
        b"nan".to_vec()
    } else if d.is_infinite() {
        b"inf".to_vec()
    } else {
        php_conv_fp(b'F', d, false, dec, b'.').0.into_bytes()
    };
    if !tmp.first().is_some_and(u8::is_ascii_digit) {
        return tmp;
    }
    if is_negative && d == 0.0 {
        is_negative = false;
    }
    let (int_part, frac_part) = match tmp.iter().position(|&c| c == b'.') {
        Some(p) if dec > 0 => (&tmp[..p], &tmp[p + 1..]),
        _ => (tmp.as_slice(), &[][..]),
    };
    let want = usize::try_from(dec).unwrap_or(0);
    // `dec` and the separators come from the script: check the result's
    // size first, as php-src allocates it at once (`zend_string_alloc`).
    // Over `memory_limit` the violation is recorded and the VM raises it
    // after the call; the empty result is discarded.
    let size = (tmp.len() / 3)
        .saturating_mul(thousand_sep.len())
        .saturating_add(tmp.len() + 2)
        .saturating_add(dec_point.len())
        .saturating_add(want);
    let mut out = Vec::new();
    if php_memlimit::TryGrow::try_grow_exact(&mut out, size).is_err() {
        return Vec::new();
    }
    if is_negative {
        out.push(b'-');
    }
    group(int_part, thousand_sep, &mut out);
    if dec > 0 {
        out.extend_from_slice(dec_point);
        out.extend_from_slice(frac_part);
        if frac_part.len() < want {
            #[expect(
                clippy::disallowed_methods,
                reason = "checked: try_grow_exact above made room for the decimals"
            )]
            out.resize(out.len() + want - frac_part.len(), b'0');
        }
    }
    out
}

/// `_php_math_number_format_long()` — `number_format()` for an integer.
pub fn number_format_long(num: i64, dec: i64, dec_point: &[u8], thousand_sep: &[u8]) -> Vec<u8> {
    let mut is_negative = num < 0;
    let mut tmpnum = num.unsigned_abs();
    if dec < 0 {
        if dec < -19 {
            tmpnum = 0;
        } else {
            let power = 10u64.pow(u32::try_from(-dec).unwrap_or(0));
            let rest = tmpnum % power;
            tmpnum /= power;
            tmpnum = if rest >= power / 2 {
                tmpnum.wrapping_mul(power).wrapping_add(power)
            } else {
                tmpnum.wrapping_mul(power)
            };
        }
        if tmpnum == 0 {
            is_negative = false;
        }
    }
    let digits = tmpnum.to_string();
    let decimals = usize::try_from(dec).unwrap_or(0);
    // As in `number_format_double`: `dec` and the separators come from the
    // script, so the result's size is checked first.
    let size = (digits.len() / 3)
        .saturating_mul(thousand_sep.len())
        .saturating_add(digits.len() + 1)
        .saturating_add(dec_point.len())
        .saturating_add(decimals);
    let mut out = Vec::new();
    if php_memlimit::TryGrow::try_grow_exact(&mut out, size).is_err() {
        return Vec::new();
    }
    if is_negative {
        out.push(b'-');
    }
    group(digits.as_bytes(), thousand_sep, &mut out);
    if dec > 0 {
        out.extend_from_slice(dec_point);
        #[expect(
            clippy::disallowed_methods,
            reason = "checked: try_grow_exact above made room for the decimals"
        )]
        out.resize(out.len() + decimals, b'0');
    }
    out
}

/// A `number_format()` argument (`int|float`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
}

/// `number_format(num, decimals, decimal_separator, thousands_separator)`
/// after argument parsing (`decimals` must already be checked to fit in an
/// `int`; PHP throws "must be between" otherwise).
///
/// A result past `memory_limit` is not built: the violation is recorded
/// (`php_memlimit::try_reserve`) and an empty string returned.
#[allow(clippy::cast_possible_truncation)]
pub fn number_format(num: Number, dec: i32, dec_point: &[u8], thousand_sep: &[u8]) -> Vec<u8> {
    match num {
        Number::Int(n) => number_format_long(n, i64::from(dec), dec_point, thousand_sep),
        Number::Float(d) => {
            let fits_long =
                (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d);
            if (d >= 4_503_599_627_370_496.0 || d <= -4_503_599_627_370_496.0) && fits_long {
                number_format_long(d as i64, i64::from(dec), dec_point, thousand_sep)
            } else {
                number_format_double(d, dec, dec_point, thousand_sep)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf(d: f64, dec: i32) -> String {
        String::from_utf8(number_format(Number::Float(d), dec, b".", b",")).unwrap()
    }

    #[test]
    fn round_basic() {
        let r = |v, p| php_round(v, p, RoundingMode::HalfUp);
        assert_eq!(r(1.005, 2), 1.01);
        assert_eq!(r(0.285, 2), 0.29);
        assert_eq!(r(2.5, 0), 3.0);
        assert_eq!(r(-2.5, 0), -3.0);
        assert_eq!(r(1.955, 2), 1.96);
        assert_eq!(r(5.045, 2), 5.05);
        assert_eq!(r(5.055, 2), 5.06);
        assert_eq!(r(1234.5678, -2), 1200.0);
        assert_eq!(r(1250.0, -2), 1300.0);
        assert_eq!(r(3.0, 0), 3.0);
        assert!(r(f64::NAN, 2).is_nan());
        assert_eq!(r(1e20, 2), 1e20);
        assert_eq!(php_round(2.5, 0, RoundingMode::HalfEven), 2.0);
        assert_eq!(php_round(3.5, 0, RoundingMode::HalfEven), 4.0);
        assert_eq!(php_round(2.5, 0, RoundingMode::HalfOdd), 3.0);
        assert_eq!(php_round(-2.5, 0, RoundingMode::HalfDown), -2.0);
        assert_eq!(php_round(1.21, 1, RoundingMode::PositiveInfinity), 1.3);
        assert_eq!(php_round(-1.21, 1, RoundingMode::NegativeInfinity), -1.3);
        assert_eq!(php_round(1.29, 1, RoundingMode::TowardsZero), 1.2);
        assert_eq!(php_round(1.21, 1, RoundingMode::AwayFromZero), 1.3);
        assert_eq!(php_round(1.0, 30, RoundingMode::HalfUp), 1.0);
        assert_eq!(php_round(0.123_456, 25, RoundingMode::HalfUp), 0.123_456);
    }

    #[test]
    fn round_large_places_locale_decimal_point() {
        // /usr/bin/php under setlocale(LC_ALL, "en_DK.utf8") (decimal
        // comma) vs "C".
        let m = RoundingMode::HalfUp;
        assert_eq!(php_round_dp(1.5e-20, 25, m, b'.'), 1.5e-20);
        assert_eq!(php_round_dp(1.5e-20, 25, m, b','), 150_000.0);
        assert_eq!(php_round_dp(-1.5e-20, 25, m, b','), -150_000.0);
        assert_eq!(php_round_dp(1.234_567_89, 23, m, b','), 1.234_567_89);
        assert_eq!(php_round_dp(0.1, 30, m, b','), 0.1);
        // |places| < 23 never formats.
        assert_eq!(php_round_dp(1.005, 2, m, b','), 1.01);
    }

    #[test]
    fn number_format_floats() {
        // Expectations from /usr/bin/php.
        assert_eq!(nf(f64::INFINITY, 2), "inf");
        assert_eq!(nf(f64::NEG_INFINITY, 0), "inf");
        assert_eq!(nf(f64::NAN, 3), "nan");
        assert_eq!(nf(-0.4, 0), "0");
        assert_eq!(nf(-0.5, 0), "-1");
        assert_eq!(nf(1234.5, 0), "1,235");
        assert_eq!(nf(1.005, 2), "1.01");
        assert_eq!(nf(0.5, 0), "1");
        assert_eq!(nf(1234.567, 2), "1,234.57");
        assert_eq!(nf(123.0, 0), "123");
        assert_eq!(nf(123_456.0, 0), "123,456");
        assert_eq!(nf(1_234_567.891, 2), "1,234,567.89");
        assert_eq!(nf(0.0, 2), "0.00");
        assert_eq!(nf(-0.0, 2), "0.00");
        assert_eq!(nf(1234.5678, -2), "1,200");
        assert_eq!(
            nf(1e80, 2),
            "100,000,000,000,000,000,026,609,864,708,367,276,537,402,401,181,200,809,098,131,977,453,489,758,916,313,088.00"
        );
        assert_eq!(nf(1e15, 2), "1,000,000,000,000,000.00");
        assert_eq!(nf(-1e16, 1), "-10,000,000,000,000,000.0");
        assert_eq!(
            String::from_utf8(number_format(Number::Float(-1234.567), 2, b",", b".")).unwrap(),
            "-1.234,57"
        );
        assert_eq!(
            String::from_utf8(number_format(Number::Float(1234.567), 2, b"", b"")).unwrap(),
            "123457"
        );
        assert_eq!(
            String::from_utf8(number_format(Number::Float(1234.5), 1, b" dec ", b" sep ")).unwrap(),
            "1 sep 234 dec 5"
        );
    }

    #[test]
    fn number_format_ints() {
        let nl = |n: i64, d: i64| String::from_utf8(number_format_long(n, d, b".", b",")).unwrap();
        assert_eq!(nl(1_234_567, 0), "1,234,567");
        assert_eq!(nl(-1_234_567, 2), "-1,234,567.00");
        assert_eq!(nl(1_234_567, -3), "1,235,000");
        assert_eq!(nl(1_234_499, -3), "1,234,000");
        assert_eq!(nl(-499, -3), "0");
        assert_eq!(nl(-500, -3), "-1,000");
        assert_eq!(nl(i64::MIN, 0), "-9,223,372,036,854,775,808");
        assert_eq!(nl(i64::MAX, -19), "10,000,000,000,000,000,000");
        assert_eq!(nl(5, -20), "0");
        assert_eq!(nl(0, 0), "0");
        assert_eq!(nl(999, 0), "999");
        assert_eq!(nl(1000, 0), "1,000");
    }
}
