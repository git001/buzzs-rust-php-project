// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's formatted-print engine and float formatting.
//!
//! * [`double`] — `zend_dtoa()` digit generation, `zend_gcvt()`,
//!   `php_conv_fp()` and [`format_double`] (float → string).
//! * [`printf`] — the `sprintf()` family (`ext/standard/formatted_print.c`).
//! * [`number`] — `round()` and `number_format()` (`ext/standard/math.c`).
//! * [`locale`] — the `LC_NUMERIC` decimal point (`LCONV_DECIMAL_POINT`).

pub mod double;
pub mod locale;
pub mod number;
pub mod printf;

pub use double::{format_double, php_conv_fp, zend_gcvt, DoubleMode};
pub use number::{
    number_format, number_format_double, number_format_long, php_round, Number, RoundingMode,
};
pub use printf::{
    php_formatted_print, sprintf, vsprintf, ArgSource, FormatArg, FormatError, FormatOptions,
};

#[cfg(test)]
mod tests_sprintf_cases;

#[cfg(test)]
mod oracle_tests {
    use std::borrow::Cow;

    use super::*;
    use crate::diag::Diagnostic;

    enum Arg {
        I(i64),
        D(f64),
        S(String),
    }

    impl FormatArg for Arg {
        fn as_int(&self) -> i64 {
            match self {
                Self::I(i) => *i,
                Self::D(d) => d.as_int(),
                Self::S(s) => s.as_int(),
            }
        }
        fn as_float(&self) -> f64 {
            match self {
                Self::I(i) => i.as_float(),
                Self::D(d) => *d,
                Self::S(s) => s.as_float(),
            }
        }
        fn as_str(&self) -> Cow<'_, [u8]> {
            match self {
                Self::I(i) => Cow::Owned(i.to_string().into_bytes()),
                Self::D(d) => Cow::Owned(format_double(*d, 14, DoubleMode::Plain).into_bytes()),
                Self::S(s) => Cow::Borrowed(s.as_bytes()),
            }
        }
        fn exact_int(&self) -> Option<i64> {
            match self {
                Self::I(i) => Some(*i),
                _ => None,
            }
        }
    }

    fn hex(b: &[u8]) -> String {
        use std::fmt::Write as _;
        b.iter().fold(String::new(), |mut s, c| {
            let _ = write!(s, "{c:02x}");
            s
        })
    }

    #[test]
    fn sprintf_matches_php() {
        let mut failures = Vec::new();
        for &(format, args, expected) in super::tests_sprintf_cases::CASES {
            let args: Vec<Arg> = args
                .iter()
                .map(|&(t, v)| match t {
                    "integer" => Arg::I(v.parse().unwrap()),
                    "double" => Arg::D(if v == "-0neg" {
                        -0.0
                    } else {
                        v.parse().unwrap()
                    }),
                    _ => Arg::S(v.to_string()),
                })
                .collect();
            let refs: Vec<&dyn FormatArg> = args.iter().map(|a| a as &dyn FormatArg).collect();
            let mut d: Vec<Diagnostic> = Vec::new();
            let got = match sprintf(format.as_bytes(), &refs, &mut d) {
                Ok(v) => format!("OK:{}", hex(&v)),
                Err(e) => {
                    let class = match e {
                        FormatError::ValueError(_) => "ValueError",
                        FormatError::ArgumentCountError(_) => "ArgumentCountError",
                        FormatError::Fatal(_) | FormatError::MemoryLimit(_) => "Fatal",
                    };
                    format!("E:{class}:{}", e.message())
                }
            };
            if got != expected {
                failures.push(format!(
                    "{format} {args:?}: expected {expected} got {got}",
                    args = args.len()
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

#[cfg(test)]
mod tests_num_cases;

#[cfg(test)]
mod number_oracle_tests {
    use super::*;

    /// Cells where php-src master deliberately differs from the 8.3 oracle:
    /// `round()` lost its pre-rounding in 8.4 (RFC "Change the edge case of
    /// `round()`"), and `number_format()` formats integral floats ≥ 2^52 with
    /// the integer code path.
    const MASTER_OVERRIDES: &[(&str, usize, &str)] = &[
        ("1.2345678901234568e+17", 10, "123,456,789,012,345,700"),
        ("2908225142354250.5", 7, "2,908,225,142,354,251"),
        ("2908225142354250.5", 11, "2908225142354251.0"),
        ("-47315072918.923141", 13, "-47315072918.92315"),
        ("3.0037024188803983e+18", 10, "3,003,702,418,880,398,300"),
        ("228266185.02301455", 13, "228266185.02301"),
        ("-40626795282.879272", 13, "-40626795282.87928"),
        ("3.1449890477326602e+18", 10, "3,144,989,047,732,660,200"),
        ("4.110972336079447e+17", 10, "411,097,233,607,944,700"),
    ];

    #[test]
    #[allow(clippy::too_many_lines)]
    fn round_master_spec() {
        // ext/standard/tests/math/round_gh12143_expand_rounding_target.phpt
        use RoundingMode as M;
        let s = |v: f64| format_double(v, -1, DoubleMode::Plain);
        let cases: [(M, [f64; 4], [&str; 6]); 8] = [
            (
                M::HalfUp,
                [0.123_456_789_012_345_65, 12_345_678_901_234_565.0, 0.0, 0.0],
                [
                    "0.1234567890123457",
                    "-0.1234567890123457",
                    "12345678901234570",
                    "-12345678901234570",
                    "4503599627370496",
                    "-4503599627370496",
                ],
            ),
            (
                M::HalfDown,
                [0.123_456_789_012_345_65, 12_345_678_901_234_565.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234560",
                    "-12345678901234560",
                    "4503599627370495",
                    "-4503599627370495",
                ],
            ),
            (
                M::HalfEven,
                [0.123_456_789_012_345_65, 12_345_678_901_234_565.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234560",
                    "-12345678901234560",
                    "4503599627370496",
                    "-4503599627370496",
                ],
            ),
            (
                M::HalfOdd,
                [0.123_456_789_012_345_65, 12_345_678_901_234_565.0, 0.0, 0.0],
                [
                    "0.1234567890123457",
                    "-0.1234567890123457",
                    "12345678901234570",
                    "-12345678901234570",
                    "4503599627370495",
                    "-4503599627370495",
                ],
            ),
            (
                M::AwayFromZero,
                [0.123_456_789_012_345_6, 12_345_678_901_234_567.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234570",
                    "-12345678901234570",
                    "4503599627370496",
                    "-4503599627370496",
                ],
            ),
            (
                M::TowardsZero,
                [0.123_456_789_012_345_66, 12_345_678_901_234_565.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234560",
                    "-12345678901234560",
                    "4503599627370495",
                    "-4503599627370495",
                ],
            ),
            (
                M::PositiveInfinity,
                [0.123_456_789_012_345_6, 12_345_678_901_234_564.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234570",
                    "-12345678901234560",
                    "4503599627370496",
                    "-4503599627370495",
                ],
            ),
            (
                M::NegativeInfinity,
                [0.123_456_789_012_345_6, 12_345_678_901_234_564.0, 0.0, 0.0],
                [
                    "0.1234567890123456",
                    "-0.1234567890123456",
                    "12345678901234560",
                    "-12345678901234570",
                    "4503599627370495",
                    "-4503599627370496",
                ],
            ),
        ];
        for (mode, [frac, big, _, _], expected) in cases {
            let got = [
                s(php_round(frac, 16, mode)),
                s(php_round(-frac, 16, mode)),
                s(php_round(big, -1, mode)),
                s(php_round(-big, -1, mode)),
                s(php_round(4_503_599_627_370_495.5, 0, mode)),
                s(php_round(-4_503_599_627_370_495.5, 0, mode)),
            ];
            assert_eq!(got.map(|g| g), expected.map(String::from), "{mode:?}");
        }
        // round_prerounding.phpt and round_large_exp.phpt
        assert_eq!(s(php_round(0.285, 2, M::HalfUp)), "0.29");
        for (v, p) in [
            (2e-22, 22),
            (1e-22, 22),
            (2e-23, 23),
            (1e-23, 23),
            (2e-24, 24),
            (1e-24, 24),
        ] {
            assert_eq!(php_round(v, p, M::HalfUp).to_bits(), f64::to_bits(v), "{v}");
        }
        for (v, p) in [
            (2e22, -22),
            (1e22, -22),
            (2e23, -23),
            (1e23, -23),
            (2e24, -24),
            (1e24, -24),
        ] {
            assert_eq!(php_round(v, p, M::HalfUp).to_bits(), f64::to_bits(v), "{v}");
        }
    }

    #[test]
    fn floats_match_php() {
        let mut failures = Vec::new();
        for row in super::tests_num_cases::CASES {
            let v: f64 = if row[0] == "-0neg" {
                -0.0
            } else {
                row[0].parse().unwrap()
            };
            let mut got: Vec<String> = vec![row[0].to_string()];
            for p in [-1, 1, 5, 14, 17, 20] {
                got.push(format_double(v, p, DoubleMode::Plain));
            }
            for d in [0, 2, 3, -2] {
                got.push(
                    String::from_utf8(number_format(Number::Float(v), d, b".", b",")).unwrap(),
                );
            }
            for d in [0, 2, 5, -1] {
                got.push(format_double(
                    php_round(v, d, RoundingMode::HalfUp),
                    -1,
                    DoubleMode::ZeroFraction,
                ));
            }
            for (i, (g, e)) in got.iter().zip(row.iter()).enumerate() {
                let e = MASTER_OVERRIDES
                    .iter()
                    .find(|&&(v, c, _)| v == row[0] && c == i)
                    .map_or(*e, |&(_, _, o)| o);
                if g != e {
                    failures.push(format!("{} col {i}: expected {e} got {g}", row[0]));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
