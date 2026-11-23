// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `NumberFormatter` behaviour checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, load, show_s, Checker, V};
use php_intl::numfmt::{Number, NumberFormatter, Parsed};
use php_intl::Error;

fn show_num(r: php_intl::Result<Number>) -> String {
    match r {
        Ok(Number::Int(i)) => format!("i:{i}"),
        Ok(Number::Float(f)) => format!("d:{}", common::php_float(f)),
        Err(e) => err_name(&e),
    }
}

fn show_parsed(r: &php_intl::Result<(Parsed, Option<i64>)>) -> String {
    match r {
        Ok((Parsed::Int(i), _)) => format!("i:{i}"),
        Ok((Parsed::Float(f), _)) => format!("d:{}", common::php_float(*f)),
        Err(e) => err_name(e),
    }
}

fn show_ok(r: php_intl::Result<()>) -> String {
    match r {
        Ok(()) => "~T".into(),
        Err(e) => err_name(&e),
    }
}

fn num(v: &V) -> Number {
    match v {
        V::Int(i) => Number::Int(*i),
        V::Float(f) => Number::Float(*f),
        o => panic!("not a number {o:?}"),
    }
}

fn make(loc: &str, st: i64, pat: Option<&str>) -> Option<NumberFormatter> {
    NumberFormatter::new(loc, st, pat.map(str::as_bytes)).ok()
}

#[test]
fn numfmt_matches_php() {
    let mut c = Checker::default();
    for row in load("numfmt") {
        let op = row[0].s();
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match op {
            "format" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                let got = show_s(f.format(num(&row[4]), row[5].i(), false));
                c.check(ctx, &got, &row[6].show());
            }
            "pattern" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                c.check(ctx, &show_s(f.get_pattern()), &row[4].show());
            }
            "locale" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                c.check(ctx, &show_s(f.get_locale(0)), &row[4].show());
                c.check(ctx, &show_s(f.get_locale(1)), &row[5].show());
            }
            "getattr" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                c.check(ctx, &show_num(f.get_attribute(row[4].i())), &row[5].show());
            }
            "gettext" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                c.check(
                    ctx,
                    &show_s(f.get_text_attribute(row[4].i())),
                    &row[5].show(),
                );
            }
            "symbol" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                c.check(ctx, &show_s(f.get_symbol(row[4].i())), &row[5].show());
            }
            "fmtcur" => {
                let f = make(row[1].s(), row[2].i(), row[3].opt_s()).expect("create");
                let got =
                    show_s(f.format_currency(num(&row[4]).to_double(), row[5].s().as_bytes()));
                c.check(ctx, &got, &row[6].show());
            }
            "parse" | "parse_pos1" => {
                // master reports UTF-8 byte offsets, 8.3 UTF-16 ones.
                let ascii = row[3].s().is_ascii();
                if op == "parse_pos1" && !ascii {
                    continue;
                }
                let f = make(row[1].s(), row[2].i(), None).expect("create");
                let (ty, start, res_i, pos_i) = if op == "parse" {
                    (row[4].i(), 0, 5, 6)
                } else {
                    (3, 1, 4, 5)
                };
                let r = f.parse(row[3].s().as_bytes(), ty, Some(start), false);
                c.check(ctx, &show_parsed(&r), &row[res_i].show());
                // php 8.3 writes the error index back on failure; master
                // leaves the offset untouched, so only compare successes.
                if let (Ok((_, p)), true) = (&r, ascii) {
                    c.check(
                        ctx,
                        &format!("i:{}", p.unwrap_or(start)),
                        &row[pos_i].show(),
                    );
                }
            }
            "parse_nopos" => {
                let f = make(row[1].s(), row[2].i(), None).expect("create");
                let r = f.parse(row[3].s().as_bytes(), 3, None, false);
                c.check(ctx, &show_parsed(&r), &row[4].show());
            }
            "parsecur" => {
                let f = make(row[1].s(), 2, None).expect("create");
                let r = f.parse_currency(row[2].s().as_bytes(), Some(0));
                match r {
                    Ok((v, cur, pos)) => {
                        c.check(ctx, &format!("d:{}", common::php_float(v)), &row[3].show());
                        c.check(ctx, &format!("s:{cur}"), &row[4].show());
                        if row[2].s().is_ascii() {
                            c.check(ctx, &format!("i:{}", pos.unwrap_or(0)), &row[5].show());
                        }
                    }
                    Err(e) => c.check(ctx, &err_name(&e), &row[3].show()),
                }
            }
            "setattr" => {
                let mut f = make("en_US", 1, None).expect("create");
                c.check(
                    ctx,
                    &show_ok(f.set_attribute(row[1].i(), num(&row[2]))),
                    &row[3].show(),
                );
                c.check(ctx, &show_num(f.get_attribute(row[1].i())), &row[4].show());
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(12345.678), 0, false)),
                    &row[5].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Int(-1_234_567), 0, false)),
                    &row[6].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(0.000_123), 0, false)),
                    &row[7].show(),
                );
            }
            "settext" => {
                let mut f = make("en_US", row[1].i(), None).expect("create");
                let ok = f.set_text_attribute(row[2].i(), row[3].s().as_bytes());
                c.check(ctx, &show_ok(ok), &row[4].show());
                c.check(
                    ctx,
                    &show_s(f.get_text_attribute(row[2].i())),
                    &row[5].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(-12345.678), 0, false)),
                    &row[6].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Int(42), 0, false)),
                    &row[7].show(),
                );
            }
            "setsym" => {
                let mut f = make("en_US", 1, None).expect("create");
                c.check(
                    ctx,
                    &show_ok(f.set_symbol(row[1].i(), b"_X_")),
                    &row[2].show(),
                );
                c.check(ctx, &show_s(f.get_symbol(row[1].i())), &row[3].show());
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(-12345.678), 0, false)),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(1.5e20), 0, false)),
                    &row[5].show(),
                );
            }
            "setpattern" => {
                let mut f = make("en_US", 1, None).expect("create");
                // php leaves the global code at U_ZERO_ERROR here (object-only code).
                let r = f.set_pattern(row[1].s().as_bytes());
                let got = match &r {
                    Err(Error::Intl(i)) if i.scope == php_intl::Scope::ObjectCode => {
                        "s:!U_ZERO_ERROR".to_owned()
                    }
                    _ => show_ok(r),
                };
                c.check(ctx, &got, &row[2].show());
                c.check(ctx, &show_s(f.get_pattern()), &row[3].show());
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(-12345.678), 0, false)),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(0.5), 0, false)),
                    &row[5].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Int(7), 0, false)),
                    &row[6].show(),
                );
            }
            "locfmt" => {
                // master rejects locales without a known ISO-3 language.
                let Some(f) = make(row[1].s(), row[2].i(), None) else {
                    continue;
                };
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(1234.5), 0, false)),
                    &row[3].show(),
                );
                c.check(ctx, &show_s(f.get_locale(1)), &row[4].show());
            }
            "ruleset" => {
                let mut f = make("en_US", 5, None).expect("create");
                c.check(
                    ctx,
                    &show_ok(f.set_text_attribute(6, row[1].s().as_bytes())),
                    &row[2].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Int(1999), 0, false)),
                    &row[3].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(Number::Float(21.5), 0, false)),
                    &row[4].show(),
                );
            }
            "rbnf" => {}
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("numfmt");
}
