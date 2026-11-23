// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of `ext/intl/tests/numberformatter/*.phpt` (php-src master).

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_intl::common::set_ini_default_locale;
use php_intl::error::{Scope, UErrorCode};
use php_intl::numberformatter::{FormatValue, IntlNumberFormatter};
use php_intl::Error;

fn create(sk: &str, loc: &str) -> IntlNumberFormatter {
    IntlNumberFormatter::create_from_skeleton(sk.as_bytes(), loc).expect("create")
}

fn fmt(f: &IntlNumberFormatter, v: FormatValue<'_>) -> String {
    String::from_utf8(f.format(v).expect("format")).expect("utf8")
}

fn global_err(e: &Error) -> (UErrorCode, String) {
    match e {
        Error::Intl(ie) => {
            assert_eq!(ie.scope, Scope::Global);
            (ie.code, ie.message.clone().unwrap_or_default().into_owned())
        }
        other => panic!("expected intl error, got {other:?}"),
    }
}

#[test]
fn basic() {
    use FormatValue::{Float, Int};
    let cases: &[(&str, &str, FormatValue<'_>, &str)] = &[
        (
            "measure-unit/speed-kilometer-per-hour",
            "en_US",
            Int(5),
            "5 km/h",
        ),
        (
            "measure-unit/speed-kilometer-per-hour unit-width-full-name",
            "en_US",
            Int(5),
            "5 kilometers per hour",
        ),
        (
            "measure-unit/speed-kilometer-per-hour unit-width-full-name",
            "en_US",
            Int(1),
            "1 kilometer per hour",
        ),
        (
            "measure-unit/speed-kilometer-per-hour unit-width-full-name",
            "en_US",
            Float(1.5),
            "1.5 kilometers per hour",
        ),
        (
            "measure-unit/speed-kilometer-per-hour unit-width-full-name",
            "de",
            Float(1.5),
            "1,5 Kilometer pro Stunde",
        ),
        (
            "measure-unit/length-meter per-measure-unit/duration-second",
            "en_US",
            Int(5),
            "5 m/s",
        ),
        (
            "measure-unit/digital-gigabyte unit-width-narrow",
            "en_US",
            Float(1.5),
            "1.5GB",
        ),
        (
            "currency/EUR unit-width-full-name",
            "en_US",
            Float(1234.5),
            "1,234.50 euros",
        ),
        ("percent", "en_US", Int(25), "25%"),
        ("compact-short", "en_US", Int(1_234_567), "1.2M"),
        (".00", "en_US", Float(1234.5), "1,234.50"),
        ("group-off", "en_US", Int(1_234_567), "1234567"),
        ("sign-always", "en_US", Int(5), "+5"),
        ("", "en_US", Float(1234.5678), "1,234.5678"),
        ("", "en_US", Float(f64::NAN), "NaN"),
    ];
    for (sk, loc, v, want) in cases {
        assert_eq!(fmt(&create(sk, loc), *v), *want, "{sk} {loc}");
    }
}

#[test]
fn default_locale() {
    let sk = "measure-unit/speed-kilometer-per-hour unit-width-full-name";
    let v = FormatValue::Float(1234.5);
    set_ini_default_locale(Some("de"));
    let de = fmt(&create(sk, ""), v);
    set_ini_default_locale(None);
    assert_eq!(de, "1.234,5 Kilometer pro Stunde");
    assert_eq!(fmt(&create(sk, "en_US"), v), "1,234.5 kilometers per hour");
}

#[test]
fn errors() {
    let e = IntlNumberFormatter::create_from_skeleton(b"invalid skeleton here", "ro").unwrap_err();
    assert_eq!(
        global_err(&e),
        (
            UErrorCode(65811),
            "Number formatter creation failed".to_owned()
        )
    );
    let e = IntlNumberFormatter::create_from_skeleton(b"measure-unit/length-nope", "en_US")
        .unwrap_err();
    assert_eq!(global_err(&e).0, UErrorCode::NUMBER_SKELETON_SYNTAX_ERROR);
    let long = format!("ro_{}", "a".repeat(160));
    let e = IntlNumberFormatter::create_from_skeleton(b"", &long).unwrap_err();
    assert_eq!(
        global_err(&e),
        (
            UErrorCode::ILLEGAL_ARGUMENT_ERROR,
            "Locale string too long, should be no longer than 156 characters".to_owned()
        )
    );
    let e = IntlNumberFormatter::create_from_skeleton(b"percent\xFF", "en_US").unwrap_err();
    assert_eq!(
        global_err(&e),
        (
            UErrorCode::INVALID_CHAR_FOUND,
            "Error converting skeleton to UTF-16".to_owned()
        )
    );
    match IntlNumberFormatter::create_from_skeleton(b"", "invalid-language").unwrap_err() {
        Error::ArgumentValue { arg, message } => {
            assert_eq!(
                (arg, message.as_str()),
                (2, "\"invalid-language\" is invalid")
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn format_failure() {
    let f = create("", "en_US@numbers=foobar");
    let e = f.format(FormatValue::Int(1)).unwrap_err();
    assert_eq!(e.intl_code(), UErrorCode::UNSUPPORTED_ERROR);
    assert_eq!(e.intl_message(), Some("Number formatting failed"));
    assert_eq!(fmt(&create("", "en_US"), FormatValue::Int(1)), "1");
}

#[test]
fn format_int64() {
    let f = create("", "en_US");
    assert_eq!(
        fmt(&f, FormatValue::Int(9_007_199_254_740_993)),
        "9,007,199,254,740,993"
    );
    assert_eq!(
        fmt(&f, FormatValue::Int(i64::MIN)),
        "-9,223,372,036,854,775,808"
    );
}

#[test]
fn format_string() {
    let f = create("precision-unlimited", "en_US");
    let s = |f: &IntlNumberFormatter, v: &str| f.format(FormatValue::Str(v.as_bytes()));
    let ok =
        |f: &IntlNumberFormatter, v: &str| String::from_utf8(s(f, v).expect("ok")).expect("u8");
    assert_eq!(
        ok(&f, "12345678901234567890.123456789"),
        "12,345,678,901,234,567,890.123456789"
    );
    assert_eq!(ok(&f, "9223372036854775808"), "9,223,372,036,854,775,808");
    assert_eq!(ok(&f, "-0.1"), "-0.1");
    assert_eq!(
        ok(&create("", "en_US"), "12345678901234567890.123456789"),
        "12,345,678,901,234,567,890.123457"
    );
    assert_eq!(
        ok(&create("measure-unit/length-meter .00", "de"), "1234.5"),
        "1.234,50 m"
    );
    for bad in ["abc", "", " 12", "1,5", "1e3", "1E-3", "1e999999999"] {
        let e = s(&f, bad).unwrap_err();
        assert_eq!(
            e.intl_code(),
            UErrorCode::DECIMAL_NUMBER_SYNTAX_ERROR,
            "{bad}"
        );
        assert_eq!(e.intl_message(), Some("Number formatting failed"));
    }
    assert_eq!(ok(&f, "12"), "12");
    assert_eq!(ok(&f, &format!("1{}", "0".repeat(999))).len(), 1333);
    match s(&f, "1\x002").unwrap_err() {
        Error::ArgumentValue { arg, message } => {
            assert_eq!(
                (arg, message.as_str()),
                (1, "must not contain any null bytes")
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn locale_posix_modifier() {
    let sk = "measure-unit/length-kilometer unit-width-full-name";
    let v = FormatValue::Float(12345.5);
    assert_eq!(
        fmt(&create(sk, "pt_PT"), v),
        fmt(&create(sk, "pt_PT@euro"), v)
    );
}

#[test]
fn skeleton_scale_exponent() {
    assert_eq!(
        fmt(
            &create("precision-unlimited scale/1000", "en_US"),
            FormatValue::Int(1)
        ),
        "1,000"
    );
    assert_eq!(
        fmt(&create("scale/0.5 percent", "en_US"), FormatValue::Int(1)),
        "0.5%"
    );
    for sk in [
        "precision-unlimited scale/1e3000000",
        "precision-unlimited scale/1E-3000000",
        "scale/1.5e3",
    ] {
        let e = IntlNumberFormatter::create_from_skeleton(sk.as_bytes(), "en_US").unwrap_err();
        assert_eq!(
            global_err(&e),
            (
                UErrorCode::NUMBER_SKELETON_SYNTAX_ERROR,
                "Number formatter creation failed".to_owned()
            )
        );
    }
}
