// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `IntlChar` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{load, php_float, Checker, V};
use php_intl::intlchar::{self, CharOrInt, CodePoint};

const BOOLS: [&str; 28] = [
    "isUAlphabetic",
    "isULowercase",
    "isUUppercase",
    "isUWhiteSpace",
    "islower",
    "isupper",
    "istitle",
    "isdigit",
    "isalpha",
    "isalnum",
    "isxdigit",
    "ispunct",
    "isgraph",
    "isblank",
    "isdefined",
    "isspace",
    "isJavaSpaceChar",
    "isWhitespace",
    "iscntrl",
    "isISOControl",
    "isprint",
    "isbase",
    "isMirrored",
    "isIDStart",
    "isIDPart",
    "isIDIgnorable",
    "isJavaIDStart",
    "isJavaIDPart",
];
const INTS: [&str; 5] = [
    "charDirection",
    "charType",
    "getCombiningClass",
    "charDigitValue",
    "getBlockCode",
];
const CHARS: [&str; 5] = [
    "charMirror",
    "tolower",
    "toupper",
    "totitle",
    "getBidiPairedBracket",
];

/// `json_encode` with `JSON_INVALID_UTF8_SUBSTITUTE` (one U+FFFD per
/// invalid sequence, as PHP's decoder consumes it).
fn js(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => common::json_str(s),
        Err(_) => common::json_str("\u{fffd}"),
    }
}

fn coi(r: php_intl::Result<CharOrInt>) -> String {
    match r {
        Ok(CharOrInt::Str(s)) => js(&s),
        Ok(CharOrInt::Int(i)) => i.to_string(),
        Err(_) => "null".into(),
    }
}

fn int(r: php_intl::Result<i64>) -> String {
    r.map_or_else(|_| "null".to_owned(), |i| i.to_string())
}

fn fl(f: f64) -> String {
    let s = php_float(f);
    if s.contains(['.', 'E', 'N', 'I']) {
        s
    } else {
        format!("{s}.0")
    }
}

#[test]
fn intlchar_matches_php() {
    let mut c = Checker::default();
    for row in load("intlchar") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "cp" => {
                let a = match &row[1] {
                    V::Int(i) => CodePoint::Int(*i),
                    V::Str(s) => CodePoint::Str(s.as_bytes()),
                    other => panic!("{other:?}"),
                };
                let mut r: Vec<String> = Vec::new();
                for m in BOOLS {
                    r.push(
                        intlchar::bool_method(m, a)
                            .map_or_else(|_| "null".into(), |b| b.to_string()),
                    );
                }
                for m in INTS {
                    r.push(int(intlchar::int_method(m, a)));
                }
                for m in CHARS {
                    r.push(coi(intlchar::char_method(m, a)));
                }
                r.push(coi(intlchar::fold_case(a, 0)));
                r.push(coi(intlchar::fold_case(a, 1)));
                r.push(intlchar::digit(a, 10).map_or_else(
                    |e| {
                        if e.intl_message() == Some("Invalid digit") {
                            "false".into()
                        } else {
                            "null".into()
                        }
                    },
                    |i| i.to_string(),
                ));
                r.push(intlchar::digit(a, 16).map_or_else(
                    |e| {
                        if e.intl_message() == Some("Invalid digit") {
                            "false".into()
                        } else {
                            "null".into()
                        }
                    },
                    |i| i.to_string(),
                ));
                r.push(intlchar::get_numeric_value(a).map_or_else(|_| "null".into(), fl));
                r.push(intlchar::char_age(a).map_or_else(
                    |_| "null".into(),
                    |v| format!("[{},{},{},{}]", v[0], v[1], v[2], v[3]),
                ));
                for nc in 0..5 {
                    r.push(intlchar::char_name(a, nc).map_or_else(|_| "null".into(), |s| js(&s)));
                }
                r.push(
                    intlchar::get_fc_nfkc_closure(a)
                        .map_or_else(|_| "null".into(), |s| common::json_str(&s)),
                );
                r.push(intlchar::chr(a).map_or_else(|_| "null".into(), |s| js(&s)));
                r.push(int(intlchar::ord(a)));
                for p in [0, 1, 6, 0x1000, 0x1002, 0x1005, 0x100A, 0x4000] {
                    r.push(
                        intlchar::has_binary_property(a, p)
                            .map_or_else(|_| "null".into(), |b| b.to_string()),
                    );
                    r.push(int(intlchar::get_int_property_value(a, p)));
                }
                c.check(ctx, &format!("a:[{}]", r.join(",")), &row[2].show());
            }
            "prop" => {
                let p = row[1].i();
                let name = |r: php_intl::Result<String>| {
                    r.map_or_else(|_| "~F".to_owned(), |s| V::Str(s).show())
                };
                c.check(
                    ctx,
                    &format!("i:{}", intlchar::get_int_property_min_value(p)),
                    &row[2].show(),
                );
                c.check(
                    ctx,
                    &format!("i:{}", intlchar::get_int_property_max_value(p)),
                    &row[3].show(),
                );
                c.check(
                    ctx,
                    &name(intlchar::get_property_name(p, 1)),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &name(intlchar::get_property_name(p, 0)),
                    &row[5].show(),
                );
                c.check(
                    ctx,
                    &name(intlchar::get_property_value_name(p, 1, 1)),
                    &row[6].show(),
                );
                c.check(
                    ctx,
                    &name(intlchar::get_property_value_name(p, 2, 0)),
                    &row[7].show(),
                );
            }
            "penum" => {
                let n = row[1].s().as_bytes();
                c.check(
                    ctx,
                    &format!("i:{}", intlchar::get_property_enum(n)),
                    &row[2].show(),
                );
                c.check(
                    ctx,
                    &format!("i:{}", intlchar::get_property_value_enum(0x1005, b"Lu")),
                    &row[3].show(),
                );
                c.check(
                    ctx,
                    &format!("i:{}", intlchar::get_property_value_enum(0x1001, n)),
                    &row[4].show(),
                );
            }
            "fromname" => {
                let r = intlchar::char_from_name(row[1].s().as_bytes(), row[2].i());
                c.check(
                    ctx,
                    &r.map_or_else(|_| "~N".to_owned(), |i| format!("i:{i}")),
                    &row[3].show(),
                );
            }
            "fordigit" => c.check(
                ctx,
                &format!("i:{}", intlchar::for_digit(row[1].i(), row[2].i())),
                &row[3].show(),
            ),
            "types" => {
                let mut v = Vec::new();
                intlchar::enum_char_types(&mut |s, l, t| {
                    if v.len() < 40 {
                        v.push(format!("[{s},{l},{t}]"));
                    }
                    true
                });
                c.check(ctx, &format!("a:[{}]", v.join(",")), &row[1].show());
            }
            "names" => {
                let mut v = Vec::new();
                intlchar::enum_char_names(
                    CodePoint::Int(0x1F600),
                    CodePoint::Int(0x1F610),
                    0,
                    &mut |cp, n, name| {
                        v.push(format!("[{cp},{n},{}]", js(name)));
                        true
                    },
                )
                .expect("names");
                c.check(ctx, &format!("a:[{}]", v.join(",")), &row[1].show());
            }
            "version" => {
                let v = intlchar::get_unicode_version();
                c.check(
                    ctx,
                    &format!("a:[{},{},{},{}]", v[0], v[1], v[2], v[3]),
                    &row[1].show(),
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("intlchar");
}
