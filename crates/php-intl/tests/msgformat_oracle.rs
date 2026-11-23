// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `MessageFormatter` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, json, load, Checker, J, V};
use php_intl::msgformat::{self, MessageFormatter, MsgKey, MsgParsed, MsgValue};

const TZ: &str = "America/New_York";

fn value(j: &J) -> MsgValue<'_> {
    match j {
        J::Null => MsgValue::Null,
        J::Bool(b) => MsgValue::Bool(*b),
        J::Int(i) => MsgValue::Int(*i),
        J::Float(f) => MsgValue::Float(*f),
        J::Str(s) => MsgValue::Str(s.as_bytes()),
        J::Arr(a) => MsgValue::Array {
            non_empty: !a.is_empty(),
        },
        J::Obj(o) => MsgValue::Array {
            non_empty: !o.is_empty(),
        },
    }
}

fn args(j: &J) -> Vec<(MsgKey<'_>, MsgValue<'_>)> {
    match j {
        J::Arr(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (MsgKey::Index(i64::try_from(i).expect("i")), value(v)))
            .collect(),
        J::Obj(o) => o
            .iter()
            .map(|(k, v)| match k.parse::<i64>() {
                Ok(n) => (MsgKey::Index(n), value(v)),
                Err(_) => (MsgKey::Name(k.as_bytes()), value(v)),
            })
            .collect(),
        _ => panic!("args"),
    }
}

fn show(r: &php_intl::Result<String>) -> String {
    match r {
        Ok(s) => format!("s:{s}"),
        Err(e) => err_name(e),
    }
}

fn show_parsed(r: &php_intl::Result<Vec<MsgParsed>>) -> String {
    match r {
        Ok(v) => format!(
            "a:[{}]",
            v.iter()
                .map(|p| match p {
                    MsgParsed::Int(i) => i.to_string(),
                    MsgParsed::Float(f) => {
                        let s = common::php_float(*f);
                        if s.contains(['.', 'E', 'N', 'I']) {
                            s
                        } else {
                            format!("{s}.0")
                        }
                    }
                    MsgParsed::Str(s) => common::json_str(s),
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        Err(e) => err_name(e),
    }
}

#[test]
fn msgformat_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("msgformat") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "fmt" | "named" => {
                let V::Arr(a) = &row[3] else { panic!("args") };
                let j = json(a);
                let r = msgformat::format_message(row[1].s(), row[2].s().as_bytes(), &args(&j), TZ);
                let want = row[4].show();
                // php 8.3 reports stale error codes for creation failures;
                // only compare success/failure there.
                if want.starts_with("s:!") {
                    c.check(ctx, if r.is_err() { "err" } else { "ok" }, "err");
                } else {
                    c.check(ctx, &show(&r), &want);
                }
            }
            "bad" => {
                let r = msgformat::format_message(
                    "en_US",
                    row[1].s().as_bytes(),
                    &[(MsgKey::Index(0), MsgValue::Int(1))],
                    TZ,
                );
                if row[2].show().starts_with("s:!") {
                    c.check(ctx, if r.is_err() { "err" } else { "ok" }, "err");
                    if row[3].s().starts_with("pattern syntax error") {
                        let msg = r
                            .as_ref()
                            .err()
                            .and_then(|e| e.intl_message())
                            .unwrap_or("")
                            .to_owned();
                        let want = row[3].s().rsplit_once(": ").map_or("", |x| x.0);
                        c.check(ctx, &msg, want);
                    }
                } else {
                    c.check(ctx, &show(&r), &row[2].show());
                }
            }
            "badctor" => {
                let r = MessageFormatter::new("en_US", row[1].s().as_bytes());
                let got = if r.is_ok() {
                    "s:MessageFormatter"
                } else {
                    "s:!IntlException"
                };
                c.check(ctx, got, &row[2].show());
            }
            "parse" => {
                let r =
                    msgformat::parse_message("en_US", row[1].s().as_bytes(), row[2].s().as_bytes());
                c.check(ctx, &show_parsed(&r), &row[3].show());
            }
            "obj" => {
                let mut mf = MessageFormatter::new("de_DE", b"{0,number} {1}").expect("mf");
                c.check(ctx, &format!("s:{}", mf.locale()), &row[1].show());
                let a = [
                    (MsgKey::Index(0), MsgValue::Float(1234.5)),
                    (MsgKey::Index(1), MsgValue::Str(b"x")),
                ];
                c.check(ctx, &show(&mf.format(&a, TZ)), &row[3].show());
                mf.set_pattern(b"{1}: {0,date,yyyy}").expect("set");
                let a = [
                    (MsgKey::Index(0), MsgValue::Int(0)),
                    (MsgKey::Index(1), MsgValue::Str(b"y")),
                ];
                c.check(ctx, &show(&mf.format(&a, TZ)), "s:y: 1970");
                let e = mf.set_pattern(b"{0,choice}").unwrap_err();
                c.check(ctx, &e.intl_code().0.to_string(), "65799");
                c.check(
                    ctx,
                    &String::from_utf8_lossy(mf.pattern()),
                    "{1}: {0,date,yyyy}",
                );
            }
            "setpat" | "setbad" => {}
            "locale" => {
                let mf = MessageFormatter::new(row[1].s(), b"{0}").expect("mf");
                c.check(ctx, &format!("s:{}", mf.locale()), &row[2].show());
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("msgformat");
}
