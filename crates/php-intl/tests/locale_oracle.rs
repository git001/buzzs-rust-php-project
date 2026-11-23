// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Locale` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, json, load, Checker, J, V};
use php_intl::locale::{self, LookupEntry, Subtag};

fn opt(r: php_intl::Result<Option<String>>) -> String {
    match r {
        Ok(Some(s)) => format!("s:{s}"),
        Ok(None) => "s:!U_ZERO_ERROR".into(),
        Err(e) if e.intl_code() == php_intl::UErrorCode::ZERO => "s:!U_ZERO_ERROR".into(),
        Err(e) => err_name(&e),
    }
}

fn pairs(v: &[(String, String)]) -> String {
    if v.is_empty() {
        return "a:[]".into();
    }
    format!(
        "a:{{{}}}",
        v.iter()
            .map(|(k, x)| format!("{}:{}", common::json_str(k), common::json_str(x)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[test]
fn locale_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("locale") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "get" => {
                let l = row[1].s();
                c.check(ctx, &opt(locale::get_primary_language(l)), &row[2].show());
                c.check(ctx, &opt(locale::get_script(l)), &row[3].show());
                c.check(ctx, &opt(locale::get_region(l)), &row[4].show());
                c.check(ctx, &opt(locale::canonicalize(l)), &row[5].show());
                let kw = match locale::get_keywords(l) {
                    Ok(Some(v)) => pairs(&v),
                    Ok(None) => "s:!U_ZERO_ERROR".into(),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &kw, &row[6].show());
                let vars = locale::get_all_variants(l).expect("variants");
                c.check(
                    ctx,
                    &format!("a:{}", common::json_strs(&vars)),
                    &row[7].show(),
                );
                c.check(
                    ctx,
                    &pairs(&locale::parse(l).expect("parse")),
                    &row[8].show(),
                );
            }
            "disp" => {
                let (l, d) = (row[1].s(), Some(row[2].s()));
                let f = |r: php_intl::Result<Option<String>>| match r {
                    Ok(Some(s)) => format!("s:{s}"),
                    Ok(None) => "s:!U_ZERO_ERROR".into(),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &f(locale::get_display_name(l, d)), &row[3].show());
                c.check(ctx, &f(locale::get_display_language(l, d)), &row[4].show());
                c.check(ctx, &f(locale::get_display_script(l, d)), &row[5].show());
                c.check(ctx, &f(locale::get_display_region(l, d)), &row[6].show());
                c.check(ctx, &f(locale::get_display_variant(l, d)), &row[7].show());
            }
            "dispkw" | "dispkwv" => {}
            "compose" => {
                let V::Arr(a) = &row[1] else { panic!() };
                let j = json(a);
                let entries: Vec<(String, J)> = match j {
                    J::Obj(o) => o,
                    _ => Vec::new(),
                };
                let get = |k: &str| -> Option<Subtag<'_>> {
                    entries
                        .iter()
                        .find(|(kk, _)| kk == k)
                        .map(|(_, v)| match v {
                            J::Str(s) => Subtag::Str(s.as_str()),
                            J::Arr(items) => Subtag::List(
                                items
                                    .iter()
                                    .map(|i| {
                                        if let J::Str(s) = i {
                                            Some(s.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            ),
                            _ => Subtag::Other,
                        })
                };
                let got = match locale::compose(entries.len(), &get) {
                    Ok(s) => format!("s:{s}"),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &got, &row[2].show());
            }
            "filter" => {
                for (k, canon) in [(3, false), (4, true)] {
                    let got = match locale::filter_matches(row[1].s(), row[2].s(), canon) {
                        Ok(true) => "~T".into(),
                        Ok(false) => "s:!U_ZERO_ERROR".into(),
                        Err(e) => err_name(&e),
                    };
                    c.check(ctx, &got, &row[k].show());
                }
            }
            "lookup" => {
                let V::Arr(a) = &row[1] else { panic!() };
                let J::Arr(items) = json(a) else { panic!() };
                let strs: Vec<String> = items
                    .iter()
                    .map(|i| {
                        if let J::Str(s) = i {
                            s.clone()
                        } else {
                            String::new()
                        }
                    })
                    .collect();
                let entries: Vec<LookupEntry<'_>> = strs
                    .iter()
                    .map(|s| LookupEntry::Str(s.as_bytes()))
                    .collect();
                let canon = row[3] == V::Bool(true);
                c.check(
                    ctx,
                    &opt(locale::lookup(&entries, row[2].s(), canon, Some("xx_XX"))),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &opt(locale::lookup(&entries, row[2].s(), canon, None)),
                    &row[5].show(),
                );
            }
            "accept" => c.check(
                ctx,
                &opt(locale::accept_from_http(row[1].s().as_bytes(), None)),
                &row[2].show(),
            ),
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("locale");
}
