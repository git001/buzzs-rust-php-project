// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of `ext/intl/tests/reldateformatter/*.phpt` (php-src master).

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_intl::error::UErrorCode;
use php_intl::numfmt::{style, Number, NumberFormatter};
use php_intl::reldateformatter::{
    Capitalization, IntlRelativeDateTimeFormatter, Style, Unit, CAPITALIZATION_CASES,
};
use php_intl::Error;

const MAX_FRACTION_DIGITS: i64 = 6;
const MIN_FRACTION_DIGITS: i64 = 7;
const ROUNDING_MODE: i64 = 11;
const POSITIVE_PREFIX: i64 = 0;

fn s(r: php_intl::Result<Vec<u8>>) -> String {
    String::from_utf8(r.expect("ok")).expect("utf8")
}

fn plain(locale: &str) -> IntlRelativeDateTimeFormatter {
    IntlRelativeDateTimeFormatter::new(Some(locale), Style::Long, Capitalization::None, None)
        .expect("ctor")
}

#[test]
fn basic() {
    let mut f = plain("en_US");
    let mut fmt = |o: f64, u| s(f.format(o, u, None));
    assert_eq!(fmt(-1.0, Unit::Day), "yesterday");
    assert_eq!(fmt(0.0, Unit::Day), "today");
    assert_eq!(fmt(1.0, Unit::Day), "tomorrow");
    assert_eq!(fmt(-1.0, Unit::Week), "last week");
    assert_eq!(fmt(2.0, Unit::Week), "in 2 weeks");
    assert_eq!(fmt(-1.0, Unit::Sunday), "last Sunday");
    assert_eq!(s(f.format_numeric(-1.0, Unit::Day, None)), "1 day ago");
    assert_eq!(s(f.format_numeric(1.0, Unit::Day, None)), "in 1 day");
    assert_eq!(s(f.format_numeric(1.5, Unit::Hour, None)), "in 1.5 hours");
    let rel = f.format(-1.0, Unit::Day, None).expect("ok");
    let c = s(f.combine_date_and_time(&rel, b"3:45 PM"));
    assert!(c.starts_with("yesterday") && c.ends_with("3:45 PM"), "{c}");
}

#[test]
fn capitalization_locale() {
    let mut nf = NumberFormatter::new("tr", style::DECIMAL, None).expect("nf");
    nf.set_text_attribute(POSITIVE_PREFIX, "istanbul ABC ".as_bytes())
        .expect("attr");
    let mut f = IntlRelativeDateTimeFormatter::new(
        Some("tr"),
        Style::Long,
        Capitalization::BeginningOfSentence,
        Some(&nf),
    )
    .expect("ctor");
    assert_eq!(s(f.format(-1.0, Unit::Day, Some(&nf))), "Dün");
    assert_eq!(
        s(f.format(3.0, Unit::Day, Some(&nf))),
        "İstanbul ABC 3 gün sonra"
    );
    assert_eq!(
        s(f.format_numeric(2.0, Unit::Day, Some(&nf))),
        "İstanbul ABC 2 gün sonra"
    );
    nf.set_text_attribute(POSITIVE_PREFIX, b"izmir XYZ ")
        .expect("attr");
    assert_eq!(
        s(f.format_numeric(2.0, Unit::Day, Some(&nf))),
        "İzmir XYZ 2 gün sonra"
    );
    assert_eq!(
        s(f.combine_date_and_time(b"istanbul", b"12:00")),
        "istanbul 12:00"
    );
    nf.set_text_attribute(POSITIVE_PREFIX, b"").expect("attr");
    assert_eq!(
        s(f.format_numeric(2.0, Unit::Day, Some(&nf))),
        "2 gün sonra"
    );
}

#[test]
fn capitalization() {
    let mut nf = NumberFormatter::new("en_US", style::DECIMAL, None).expect("nf");
    for name in CAPITALIZATION_CASES {
        let ctx = Capitalization::from_case(name).expect("case");
        nf.set_attribute(MIN_FRACTION_DIGITS, Number::Int(0))
            .expect("attr");
        let mut f =
            IntlRelativeDateTimeFormatter::new(Some("en_US"), Style::Narrow, ctx, Some(&nf))
                .expect("ctor");
        let begin = ctx == Capitalization::BeginningOfSentence;
        let (y, i) = if begin {
            ("Yesterday", "In")
        } else {
            ("yesterday", "in")
        };
        assert_eq!(s(f.format(-1.0, Unit::Day, Some(&nf))), y);
        let n = s(f.format_numeric(2.0, Unit::Day, Some(&nf)));
        assert!(
            n == format!("{i} 2d") || n == format!("{i} 2 days"),
            "{name}: {n}"
        );
        nf.set_attribute(MIN_FRACTION_DIGITS, Number::Int(1))
            .expect("attr");
        assert_eq!(s(f.format(-1.0, Unit::Day, Some(&nf))), y);
        let n = s(f.format_numeric(2.0, Unit::Day, Some(&nf)));
        assert!(
            n == format!("{i} 2.0d") || n == format!("{i} 2.0 days"),
            "{name}: {n}"
        );
    }
}

#[test]
fn errors() {
    let e = IntlRelativeDateTimeFormatter::new(Some("f"), Style::Long, Capitalization::None, None)
        .unwrap_err();
    assert!(
        matches!(e, Error::ArgumentValue { arg: 1, ref message } if message == "\"f\" is invalid")
    );
    let long = "a".repeat(157);
    let e =
        IntlRelativeDateTimeFormatter::new(Some(&long), Style::Long, Capitalization::None, None)
            .unwrap_err();
    assert!(matches!(e, Error::ArgumentValue { arg: 1, ref message }
        if message == "must be less than or equal to 156 characters"));
}

#[test]
fn error_state() {
    let mut f = plain("en_US");
    let e = f.combine_date_and_time(b"\x80", b"3:45 PM").unwrap_err();
    assert_eq!(e.intl_code(), UErrorCode::INVALID_CHAR_FOUND);
    assert_eq!(
        e.intl_message(),
        Some("Failed to convert relative date to UTF-16")
    );
    assert_eq!(s(f.format(1.0, Unit::Day, None)), "tomorrow");
    let e = f.combine_date_and_time(b"today", b"\x80").unwrap_err();
    assert_eq!(e.intl_message(), Some("Failed to convert time to UTF-16"));
}

#[test]
fn format_failure() {
    for numeric in [false, true] {
        let mut nf = NumberFormatter::new("en_US", style::DECIMAL, None).expect("nf");
        nf.set_attribute(MAX_FRACTION_DIGITS, Number::Int(0))
            .expect("attr");
        nf.set_attribute(ROUNDING_MODE, Number::Int(7))
            .expect("attr");
        let mut f = IntlRelativeDateTimeFormatter::new(
            Some("en_US"),
            Style::Long,
            Capitalization::None,
            Some(&nf),
        )
        .expect("ctor");
        let r = if numeric {
            f.format_numeric(1.5, Unit::Second, Some(&nf))
        } else {
            f.format(1.5, Unit::Second, Some(&nf))
        };
        assert_eq!(r.unwrap_err().intl_code(), UErrorCode::FORMAT_INEXACT_ERROR);
    }
}

#[test]
fn locales_and_options() {
    let mut fr = plain("fr_FR");
    assert_eq!(s(fr.format_numeric(-1.0, Unit::Day, None)), "il y a 1 jour");
    assert_eq!(
        s(fr.format_numeric(1.5, Unit::Hour, None)),
        "dans 1,5 heure"
    );
    assert_eq!(
        s(plain("zh_CN").format_numeric(3.0, Unit::Month, None)),
        "3个月后"
    );
    let mut b = IntlRelativeDateTimeFormatter::new(
        Some("en_US"),
        Style::Long,
        Capitalization::BeginningOfSentence,
        None,
    )
    .expect("ctor");
    assert_eq!(s(b.format(-1.0, Unit::Day, None)), "Yesterday");
    let mut short =
        IntlRelativeDateTimeFormatter::new(Some("en_US"), Style::Short, Capitalization::None, None)
            .expect("ctor");
    assert_eq!(s(short.format_numeric(3.0, Unit::Day, None)), "in 3 days");
    let mut narrow = IntlRelativeDateTimeFormatter::new(
        Some("en_US"),
        Style::Narrow,
        Capitalization::None,
        None,
    )
    .expect("ctor");
    let n = s(narrow.format_numeric(3.0, Unit::Day, None));
    assert!(n == "in 3d" || n == "in 3 days", "{n}");
    let mut d = IntlRelativeDateTimeFormatter::new(None, Style::Long, Capitalization::None, None)
        .expect("ctor");
    assert_eq!(s(d.format(1.0, Unit::Day, None)), "tomorrow");
}

#[test]
fn numberformatter() {
    let mut nf = NumberFormatter::new("en_US", style::DECIMAL, None).expect("nf");
    nf.set_attribute(MIN_FRACTION_DIGITS, Number::Int(1))
        .expect("attr");
    let mut f = IntlRelativeDateTimeFormatter::new(
        Some("en_US"),
        Style::Long,
        Capitalization::None,
        Some(&nf),
    )
    .expect("ctor");
    assert_eq!(
        s(f.format_numeric(2.0, Unit::Day, Some(&nf))),
        "in 2.0 days"
    );
    assert_eq!(s(f.format(2.0, Unit::Week, Some(&nf))), "in 2.0 weeks");
    nf.set_attribute(MIN_FRACTION_DIGITS, Number::Int(0))
        .expect("attr");
    assert_eq!(s(f.format_numeric(2.0, Unit::Day, Some(&nf))), "in 2 days");
    assert_eq!(s(f.format(2.0, Unit::Week, Some(&nf))), "in 2 weeks");
}
