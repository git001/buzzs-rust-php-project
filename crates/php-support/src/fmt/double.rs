// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Float → string conversions: `zend_dtoa()` digit generation, `zend_gcvt()`
//! (`Zend/zend_strtod.c`), `php_conv_fp()` (`main/snprintf.c`) and
//! `smart_str_append_double()` (`Zend/zend_smart_str.c`).
//!
//! Digit generation relies on Rust's float formatting, which is exact:
//! shortest round-trip output for mode 0 and correctly rounded
//! (round-half-even on the exact binary value) output for fixed precision —
//! the same results `zend_dtoa()` produces in modes 0, 2 and 3.

/// Decimal digits of a float as `zend_dtoa()` returns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtoaDigits {
    /// ASCII digits without trailing zeros.  `"0"` for zero.  May be empty
    /// in fixed mode when the value rounds to zero.
    pub digits: Vec<u8>,
    /// Position of the decimal point relative to the digits: the value is
    /// `0.<digits> × 10^decpt`.
    pub decpt: i32,
    /// Sign bit of the input (set for `-0.0` too).
    pub negative: bool,
}

/// Splits Rust's `{:e}` output (`"d.ddde-5"`) into digits and exponent.
fn split_exp(s: &str) -> (Vec<u8>, i32) {
    let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
    let digits: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    (digits, exp)
}

fn strip_trailing_zeros(digits: &mut Vec<u8>) {
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
}

/// `zend_dtoa(value, 0, …)`: the shortest digit string that round-trips.
/// Must not be called with a non-finite value.
pub fn dtoa_shortest(value: f64) -> DtoaDigits {
    let negative = value.is_sign_negative();
    if value == 0.0 {
        return DtoaDigits {
            digits: b"0".to_vec(),
            decpt: 1,
            negative,
        };
    }
    let (mut digits, exp) = split_exp(&format!("{:e}", value.abs()));
    strip_trailing_zeros(&mut digits);
    DtoaDigits {
        digits,
        decpt: exp + 1,
        negative,
    }
}

/// `zend_dtoa(value, 2, ndigits, …)`: `max(1, ndigits)` correctly rounded
/// significant digits, trailing zeros removed.  Value must be finite.
pub fn dtoa_precision(value: f64, ndigits: i32) -> DtoaDigits {
    let negative = value.is_sign_negative();
    if value == 0.0 {
        return DtoaDigits {
            digits: b"0".to_vec(),
            decpt: 1,
            negative,
        };
    }
    let n = usize::try_from(ndigits.max(1)).unwrap_or(1);
    let (mut digits, exp) = split_exp(&format!("{:.*e}", n - 1, value.abs()));
    strip_trailing_zeros(&mut digits);
    DtoaDigits {
        digits,
        decpt: exp + 1,
        negative,
    }
}

/// `zend_dtoa(value, 3, ndigits, …)`: digits up to `ndigits` places after
/// the decimal point (`ndigits >= 0`), trailing zeros removed.  When the
/// value rounds to zero the digit string is empty and `decpt == -ndigits`.
/// Value must be finite.
pub fn dtoa_fixed(value: f64, ndigits: i32) -> DtoaDigits {
    let negative = value.is_sign_negative();
    if value == 0.0 {
        return DtoaDigits {
            digits: b"0".to_vec(),
            decpt: 1,
            negative,
        };
    }
    if let Some(d) = dtoa_fixed_exact(value.abs(), ndigits) {
        return DtoaDigits { negative, ..d };
    }
    dtoa_fixed_fmt(value, ndigits, negative)
}

/// [`dtoa_fixed`] through Rust's exact (bignum) float formatting.
fn dtoa_fixed_fmt(value: f64, ndigits: i32, negative: bool) -> DtoaDigits {
    let n = usize::try_from(ndigits.max(0)).unwrap_or(0);
    let s = format!("{:.*}", n, value.abs());
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let mut all: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).collect();
    let int_len = i32::try_from(int_part.len()).unwrap_or(i32::MAX);
    let leading = all.iter().take_while(|&&c| c == b'0').count();
    if leading == all.len() {
        return DtoaDigits {
            digits: Vec::new(),
            decpt: -ndigits,
            negative,
        };
    }
    all.drain(..leading);
    strip_trailing_zeros(&mut all);
    DtoaDigits {
        digits: all,
        decpt: int_len - i32::try_from(leading).unwrap_or(0),
        negative,
    }
}

/// Exact integer fast path of [`dtoa_fixed`] for a finite, positive
/// `value` and `ndigits <= 22`: with `value = m × 2^e`, `value × 10^n` is
/// `m × 10^n` shifted by `e`, which fits in a `u128` (`m < 2^53`,
/// `10^22 < 2^74`), and is rounded half-to-even on the exact remainder —
/// the same result as the bignum path. `None` when the product overflows.
fn dtoa_fixed_exact(value: f64, ndigits: i32) -> Option<DtoaDigits> {
    let n = u32::try_from(ndigits).ok().filter(|&n| n <= 22)?;
    let bits = value.to_bits();
    let biased = i32::try_from((bits >> 52) & 0x7ff).ok()?;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if biased == 0 {
        (frac, -1074)
    } else {
        (frac | (1u64 << 52), biased - 1075)
    };
    let pow10 = 10u128.pow(n);
    let q = if e >= 0 {
        if e > 74 {
            return None;
        }
        (u128::from(m) << e).checked_mul(pow10)?
    } else {
        let shift = e.unsigned_abs();
        let num = u128::from(m) * pow10;
        if shift >= 128 {
            // num < 2^127 <= half of 2^shift: rounds to zero.
            0
        } else {
            let q = num >> shift;
            let r = num & ((1u128 << shift) - 1);
            let half = 1u128 << (shift - 1);
            if r > half || (r == half && q & 1 == 1) {
                q + 1
            } else {
                q
            }
        }
    };
    if q == 0 {
        return Some(DtoaDigits {
            digits: Vec::new(),
            decpt: -ndigits,
            negative: false,
        });
    }
    let mut digits = q.to_string().into_bytes();
    let len = i32::try_from(digits.len()).ok()?;
    strip_trailing_zeros(&mut digits);
    Some(DtoaDigits {
        digits,
        decpt: len - ndigits,
        negative: false,
    })
}

/// `zend_gcvt(value, ndigit, dec_point, exponent, buf)`.
///
/// Formats with `ndigit` significant digits (mode 2), or with the shortest
/// round-trip representation when `ndigit < 0` (mode 0, using 17 as the
/// exponential-notation threshold).  Uses exponential notation when the
/// decimal exponent is below -4 or not less than the precision; the
/// exponent has no leading zeros and always carries a sign (`1.0E+25`).
///
/// Infinity and NaN become `INF`, `-INF` and `NAN`, truncated to `ndigit`
/// characters as the C implementation does with its `snprintf()`.
pub fn zend_gcvt(value: f64, ndigit: i32, dec_point: u8, exponent: u8) -> String {
    let (d, ndigit) = if ndigit >= 0 {
        if value.is_finite() {
            (dtoa_precision(value, ndigit), ndigit)
        } else {
            return non_finite(value, ndigit);
        }
    } else if value.is_finite() {
        (dtoa_shortest(value), 17)
    } else {
        return non_finite(value, 17);
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most the ~770 significant digits of a double"
    )]
    let mut out: Vec<u8> = Vec::with_capacity(d.digits.len() + 8);
    if d.negative {
        out.push(b'-');
    }
    let mut decpt = d.decpt;
    if (decpt >= 0 && decpt > ndigit) || decpt < -3 {
        // Exponential format, e.g. 1.2345e+13.
        decpt -= 1;
        let exp_negative = decpt < 0;
        let decpt = decpt.unsigned_abs();
        out.push(d.digits[0]);
        out.push(dec_point);
        if d.digits.len() == 1 {
            out.push(b'0');
        } else {
            out.extend_from_slice(&d.digits[1..]);
        }
        out.push(exponent);
        out.push(if exp_negative { b'-' } else { b'+' });
        out.extend_from_slice(decpt.to_string().as_bytes());
    } else if decpt < 0 {
        // Standard format 0.000ddd.
        out.push(b'0');
        out.push(dec_point);
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: decpt is -3..=-1 here (below -3 takes the exponential branch)"
        )]
        out.resize(out.len() + decpt.unsigned_abs() as usize, b'0');
        out.extend_from_slice(&d.digits);
    } else {
        // Standard format.
        let decpt = usize::try_from(decpt).unwrap_or(0);
        for i in 0..decpt {
            out.push(d.digits.get(i).copied().unwrap_or(b'0'));
        }
        if d.digits.len() > decpt {
            if decpt == 0 {
                out.push(b'0');
            }
            out.push(dec_point);
            out.extend_from_slice(&d.digits[decpt..]);
        }
    }
    bytes_to_string(out)
}

fn non_finite(value: f64, ndigit: i32) -> String {
    let full = if value.is_nan() {
        "NAN"
    } else if value.is_sign_negative() {
        "-INF"
    } else {
        "INF"
    };
    let max = usize::try_from(ndigit).unwrap_or(0);
    full[..full.len().min(max)].to_string()
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    // Only ASCII digits, signs and the caller-supplied separator bytes.
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// How [`format_double`] decorates its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoubleMode {
    /// Plain `zend_gcvt()` output with `'.'` and `'E'`, as used for string
    /// conversion (`echo 1.5`, `(string) 1e25` → `"1.0E+25"`), `print_r()`
    /// and `var_dump()` (`float(0.1)`).
    #[default]
    Plain,
    /// Like [`DoubleMode::Plain`] but appends `".0"` when a finite result
    /// has no decimal point, as `var_export()` and `json_encode()` do
    /// (`smart_str_append_double(…, zero_fraction = true)`).
    ZeroFraction,
}

/// Formats a float the way PHP turns it into a string for a given
/// `precision` (the `precision` / `serialize_precision` INI value).
///
/// * `precision == -1` — shortest round-trip representation (`0.1`,
///   `1.0E+25`, `1.0E-7`).
/// * `precision == 0` — treated as 1, like `smart_str_append_double()`.
/// * otherwise — that many significant digits (`precision=14` gives
///   `0.30000000000000004` → `"0.3"`).
///
/// Non-finite values print as `INF`, `-INF` and `NAN`.  This is
/// `zend_double_to_str()` / `smart_str_append_double()`.
pub fn format_double(value: f64, precision: i32, mode: DoubleMode) -> String {
    let precision = if precision == 0 { 1 } else { precision };
    let mut s = zend_gcvt(value, precision, b'.', b'E');
    if mode == DoubleMode::ZeroFraction && value.is_finite() && !s.contains('.') {
        s.push_str(".0");
    }
    s
}

/// `ecvt`/`fcvt`-style digit buffer produced by the static `__cvt()` helper
/// in `main/snprintf.c` (with padding enabled).
struct Cvt {
    digits: Vec<u8>,
    decpt: i32,
    negative: bool,
}

/// `php_ecvt(value, ndigit)`: exactly `ndigit` significant digits.
#[expect(
    clippy::disallowed_methods,
    reason = "bounded: only php_conv_fp calls it, with ndigit clamped to NDIG - 1"
)]
fn php_ecvt(value: f64, ndigit: i32) -> Cvt {
    let width = usize::try_from(ndigit).unwrap_or(0);
    if value == 0.0 {
        let mut digits = vec![b'0'];
        digits.resize(width.max(1), b'0');
        return Cvt {
            digits,
            decpt: 1,
            negative: false,
        };
    }
    let d = dtoa_precision(value, ndigit);
    let mut digits = d.digits;
    if digits.len() < width {
        digits.resize(width, b'0');
    }
    Cvt {
        digits,
        decpt: d.decpt,
        negative: d.negative,
    }
}

/// `php_fcvt(value, ndigit)`: digits up to `ndigit` places after the point.
#[expect(
    clippy::disallowed_methods,
    reason = "bounded: only php_conv_fp calls it, with ndigit clamped to NDIG - 1"
)]
fn php_fcvt(value: f64, ndigit: i32) -> Cvt {
    let width = usize::try_from(ndigit).unwrap_or(0);
    if value == 0.0 {
        let mut digits = vec![b'0'];
        digits.resize(width.max(1), b'0');
        return Cvt {
            digits,
            decpt: 0,
            negative: false,
        };
    }
    let d = dtoa_fixed(value, ndigit);
    let total = usize::try_from(ndigit + d.decpt).unwrap_or(0);
    let mut digits = d.digits;
    if digits.len() < total {
        digits.resize(total, b'0');
    }
    Cvt {
        digits,
        decpt: d.decpt,
        negative: d.negative,
    }
}

/// `NDIG` from `main/snprintf.c`.
const NDIG: i32 = 320;

/// `php_conv_fp()`: formats `num` in `'F'` (fixed), `'e'` or `'E'`
/// (exponential) notation with `precision` digits after the point.
///
/// Returns the unsigned digits and whether the number is negative (the
/// caller adds the sign).  Zero is never negative here, even `-0.0`.
/// `add_dp` forces a decimal point (the `#` flag).  The exponent has no
/// padding: `1.0e+1`, `1.5e-7`.
pub fn php_conv_fp(
    format: u8,
    num: f64,
    add_dp: bool,
    precision: i32,
    dec_point: u8,
) -> (String, bool) {
    let precision = precision.clamp(0, NDIG - 2);
    if !num.is_finite() {
        let s = if num.is_nan() { "NAN" } else { "INF" };
        return (s.to_string(), false);
    }
    let cvt = if format == b'F' {
        php_fcvt(num, precision)
    } else {
        php_ecvt(num, precision + 1)
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: precision is clamped to NDIG - 2 above"
    )]
    let mut out: Vec<u8> = Vec::with_capacity(cvt.digits.len() + 8);
    let mut p = cvt.digits.as_slice();
    let mut decimal_point = cvt.decpt;
    if format == b'F' {
        if decimal_point <= 0 {
            if num != 0.0 || precision > 0 {
                out.push(b'0');
                if precision > 0 {
                    out.push(dec_point);
                    while decimal_point < 0 {
                        out.push(b'0');
                        decimal_point += 1;
                    }
                } else if add_dp {
                    out.push(dec_point);
                }
            }
        } else {
            let take = usize::try_from(decimal_point).unwrap_or(0).min(p.len());
            out.extend_from_slice(&p[..take]);
            p = &p[take..];
            if precision > 0 || add_dp {
                out.push(dec_point);
            }
        }
    } else {
        out.push(p[0]);
        p = &p[1..];
        if precision > 0 || add_dp {
            out.push(b'.');
        }
    }
    out.extend_from_slice(p);
    if format != b'F' {
        out.push(format);
        let exp = decimal_point - 1;
        if exp == 0 {
            out.extend_from_slice(b"+0");
        } else {
            out.push(if exp < 0 { b'-' } else { b'+' });
            out.extend_from_slice(exp.unsigned_abs().to_string().as_bytes());
        }
    }
    (bytes_to_string(out), cvt.negative)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(v: f64, p: i32) -> String {
        format_double(v, p, DoubleMode::Plain)
    }

    #[test]
    fn fixed_exact_matches_bignum_path() {
        let mut vals = vec![
            0.125,
            0.375,
            2.5,
            3.5,
            0.5,
            1.5,
            1e15 + 0.5,
            0.045,
            1.005,
            1.0,
            9.995,
            123_456_789.125,
            0.1,
            0.2,
            0.3,
            1e-7,
            5e-324,
            1e22,
            1e23,
            9.007_199_254_740_993e15,
            1.8e19,
            3.4e38,
            0.000_049_999,
            99.999_999_999,
            1e-22,
            2.5e-5,
            4.35,
            0.615,
        ];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = f64::from_bits(x >> 1);
            if v.is_finite() && v != 0.0 {
                vals.push(v);
            }
            #[allow(clippy::cast_precision_loss)]
            vals.push((x % 1_000_000) as f64 / 1000.0);
        }
        for v in vals {
            for nd in 0..=22 {
                let fast = dtoa_fixed(v, nd);
                let slow = dtoa_fixed_fmt(v, nd, false);
                assert_eq!(fast, slow, "{v:e} with {nd} digits");
            }
        }
    }

    #[test]
    fn shortest_mode() {
        // Values checked against `php -d precision=-1 -r 'echo $v;'`.
        assert_eq!(g(0.1, -1), "0.1");
        assert_eq!(g(0.1 + 0.2, -1), "0.30000000000000004");
        assert_eq!(g(1e25, -1), "1.0E+25");
        assert_eq!(g(1e15, -1), "1000000000000000");
        assert_eq!(g(1e16, -1), "10000000000000000");
        assert_eq!(g(1e17, -1), "1.0E+17");
        assert_eq!(g(123_456_789_012_345_680.0, -1), "1.2345678901234568E+17");
        assert_eq!(g(1e-7, -1), "1.0E-7");
        assert_eq!(g(0.0001, -1), "0.0001");
        assert_eq!(g(0.00001, -1), "1.0E-5");
        assert_eq!(g(-0.0, -1), "-0");
        assert_eq!(g(0.0, -1), "0");
        assert_eq!(g(1.0, -1), "1");
        assert_eq!(g(-1.5, -1), "-1.5");
        assert_eq!(g(100.0, -1), "100");
        assert_eq!(g(5e-324, -1), "5.0E-324");
        assert_eq!(g(f64::MAX, -1), "1.7976931348623157E+308");
        assert_eq!(g(f64::INFINITY, -1), "INF");
        assert_eq!(g(f64::NEG_INFINITY, -1), "-INF");
        assert_eq!(g(f64::NAN, -1), "NAN");
        assert_eq!(g(1e23, -1), "1.0E+23");
        assert_eq!(g(9_007_199_254_740_993.0, -1), "9007199254740992");
    }

    #[test]
    fn precision_mode() {
        // `echo` with the default precision=14.
        assert_eq!(g(0.1 + 0.2, 14), "0.3");
        assert_eq!(g(1.0 / 3.0, 14), "0.33333333333333");
        assert_eq!(g(1e15, 14), "1.0E+15");
        assert_eq!(g(1e14, 14), "1.0E+14");
        assert_eq!(g(123_456_789_012_345.0, 14), "1.2345678901234E+14");
        assert_eq!(g(12_345_678_901_234.0, 14), "12345678901234");
        assert_eq!(g(-0.0, 14), "-0");
        assert_eq!(g(2.5, 1), "2");
        assert_eq!(g(3.5, 1), "4");
        assert_eq!(g(1.5, 0), "2");
        assert_eq!(g(0.000_123_4, 14), "0.0001234");
        assert_eq!(g(0.000_012_34, 14), "1.234E-5");
        assert_eq!(g(1e100, 14), "1.0E+100");
        assert_eq!(g(f64::INFINITY, 14), "INF");
        assert_eq!(g(f64::INFINITY, 1), "I");
        assert_eq!(g(f64::NEG_INFINITY, 1), "-");
        assert_eq!(g(f64::NEG_INFINITY, 2), "-I");
        assert_eq!(g(f64::NAN, 1), "N");
        assert_eq!(g(f64::NAN, 2), "NA");
        assert_eq!(g(f64::INFINITY, 0), "I");
        assert_eq!(g(1.0, 17), "1");
        assert_eq!(g(0.1, 17), "0.10000000000000001");
        assert_eq!(g(0.1, 20), "0.10000000000000000555");
    }

    #[test]
    fn zero_fraction() {
        assert_eq!(format_double(1.0, -1, DoubleMode::ZeroFraction), "1.0");
        assert_eq!(format_double(-0.0, -1, DoubleMode::ZeroFraction), "-0.0");
        assert_eq!(format_double(1.5, -1, DoubleMode::ZeroFraction), "1.5");
        assert_eq!(format_double(1e25, -1, DoubleMode::ZeroFraction), "1.0E+25");
        assert_eq!(
            format_double(f64::INFINITY, -1, DoubleMode::ZeroFraction),
            "INF"
        );
        assert_eq!(format_double(f64::NAN, -1, DoubleMode::ZeroFraction), "NAN");
    }

    #[test]
    fn gcvt_separators() {
        assert_eq!(zend_gcvt(1e25, 6, b',', b'e'), "1,0e+25");
        assert_eq!(zend_gcvt(0.5, 6, b',', b'e'), "0,5");
        assert_eq!(zend_gcvt(5.0, 0, b'.', b'e'), "5.0e+0");
        assert_eq!(zend_gcvt(1234.5, 2, b'.', b'e'), "1.2e+3");
    }

    #[test]
    fn dtoa_modes() {
        assert_eq!(
            dtoa_fixed(0.001, 2),
            DtoaDigits {
                digits: vec![],
                decpt: -2,
                negative: false
            }
        );
        assert_eq!(
            dtoa_fixed(0.009, 2),
            DtoaDigits {
                digits: b"1".to_vec(),
                decpt: -1,
                negative: false
            }
        );
        assert_eq!(
            dtoa_fixed(-123.456, 2),
            DtoaDigits {
                digits: b"12346".to_vec(),
                decpt: 3,
                negative: true
            }
        );
        assert_eq!(
            dtoa_fixed(0.5, 0),
            DtoaDigits {
                digits: vec![],
                decpt: 0,
                negative: false
            }
        );
        assert_eq!(
            dtoa_fixed(1.5, 0),
            DtoaDigits {
                digits: b"2".to_vec(),
                decpt: 1,
                negative: false
            }
        );
        assert_eq!(
            dtoa_fixed(100.0, 2),
            DtoaDigits {
                digits: b"1".to_vec(),
                decpt: 3,
                negative: false
            }
        );
        assert_eq!(
            dtoa_precision(0.125, 2),
            DtoaDigits {
                digits: b"12".to_vec(),
                decpt: 0,
                negative: false
            }
        );
        assert_eq!(
            dtoa_shortest(120.0),
            DtoaDigits {
                digits: b"12".to_vec(),
                decpt: 3,
                negative: false
            }
        );
    }

    #[test]
    fn conv_fp_fixed() {
        let f = |v: f64, p: i32| php_conv_fp(b'F', v, false, p, b'.');
        assert_eq!(f(123.456, 2), ("123.46".to_string(), false));
        assert_eq!(f(-123.456, 2), ("123.46".to_string(), true));
        assert_eq!(f(0.0, 2), ("0.00".to_string(), false));
        assert_eq!(f(-0.0, 2), ("0.00".to_string(), false));
        assert_eq!(f(0.0, 0), ("0".to_string(), false));
        assert_eq!(f(0.3, 0), ("0".to_string(), false));
        assert_eq!(f(0.5, 0), ("0".to_string(), false));
        assert_eq!(f(1.5, 0), ("2".to_string(), false));
        assert_eq!(f(2.5, 0), ("2".to_string(), false));
        assert_eq!(f(0.125, 2), ("0.12".to_string(), false));
        assert_eq!(f(0.001, 2), ("0.00".to_string(), false));
        assert_eq!(f(-0.001, 2), ("0.00".to_string(), true));
        assert_eq!(f(0.009, 2), ("0.01".to_string(), false));
        assert_eq!(f(0.05, 1), ("0.1".to_string(), false));
        assert_eq!(
            f(1e23, 2),
            ("99999999999999991611392.00".to_string(), false)
        );
        assert_eq!(f(1.0, 6), ("1.000000".to_string(), false));
        assert_eq!(
            php_conv_fp(b'F', 0.3, true, 0, b'.'),
            ("0.".to_string(), false)
        );
        assert_eq!(
            php_conv_fp(b'F', 3.0, true, 0, b'.'),
            ("3.".to_string(), false)
        );
        assert_eq!(
            php_conv_fp(b'F', 0.0, true, 0, b'.'),
            ("0".to_string(), false)
        );
        assert_eq!(
            php_conv_fp(b'F', 1.5, false, 2, b','),
            ("1,50".to_string(), false)
        );
    }

    #[test]
    fn conv_fp_exp() {
        let e = |v: f64, p: i32| php_conv_fp(b'e', v, false, p, b'.').0;
        assert_eq!(e(10.0, 1), "1.0e+1");
        assert_eq!(e(10.0, 0), "1e+1");
        assert_eq!(e(10.0, 6), "1.000000e+1");
        assert_eq!(e(0.0, 2), "0.00e+0");
        assert_eq!(e(1.0, 2), "1.00e+0");
        assert_eq!(e(1.5e-7, 3), "1.500e-7");
        assert_eq!(e(1.0e100, 2), "1.00e+100");
        assert_eq!(e(9.99, 1), "1.0e+1");
        assert_eq!(php_conv_fp(b'E', 12345.678, false, 2, b'.').0, "1.23E+4");
        assert_eq!(
            php_conv_fp(b'e', -2.5, false, 1, b'.'),
            ("2.5e+0".to_string(), true)
        );
        assert_eq!(
            php_conv_fp(b'e', f64::INFINITY, false, 1, b'.'),
            ("INF".to_string(), false)
        );
        assert_eq!(php_conv_fp(b'e', 3.0, true, 0, b'.').0, "3.e+0");
    }
}
