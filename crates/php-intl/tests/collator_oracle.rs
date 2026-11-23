// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Collator` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::format_collect
)]

mod common;

use common::{err_name, json, load, Checker, J, V};
use php_intl::collator::{Collator, SortValue};

const WORDS: [&str; 39] = [
    "a", "A", "b", "á", "Ä", "ae", "æ", "côte", "coté", "cote", "côté", "z", "ö", "oe", "ß", "ss",
    "ch", "cz", "ll", "lz", "1", "10", "2", "9", " ", "", "-", "ﬁ", "fi", "Ω", "ω", "中", "日本",
    "ア", "あ", "peach", "péché", "pêche", "sin",
];

fn cmp_all(c: &Collator) -> String {
    let mut out = String::new();
    for (i, a) in WORDS.iter().enumerate() {
        for b in &WORDS[i + 1..] {
            let r = c.compare(a.as_bytes(), b.as_bytes()).expect("cmp");
            out.push(char::from(b'0' + u8::try_from(r + 1).expect("digit")));
        }
    }
    format!("s:{out}")
}

fn sort_values(j: &J) -> (Vec<J>, Vec<String>) {
    match j {
        J::Arr(a) => (a.clone(), (0..a.len()).map(|i| i.to_string()).collect()),
        J::Obj(o) => (
            o.iter().map(|(_, v)| v.clone()).collect(),
            o.iter().map(|(k, _)| k.clone()).collect(),
        ),
        _ => panic!(),
    }
}

fn sv(j: &J) -> SortValue<'_> {
    match j {
        J::Null => SortValue::Null,
        J::Bool(b) => SortValue::Bool(*b),
        J::Int(i) => SortValue::Int(*i),
        J::Float(f) => SortValue::Float(*f),
        J::Str(s) => SortValue::Str(s.as_bytes()),
        _ => SortValue::Array,
    }
}

fn render(j: &J) -> String {
    match j {
        J::Null => "null".into(),
        J::Bool(b) => b.to_string(),
        J::Int(i) => i.to_string(),
        J::Float(f) => {
            let s = common::php_float(*f);
            if s.contains(['.', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        J::Str(s) => common::json_str(s),
        _ => "?".into(),
    }
}

fn key_json(k: &str) -> String {
    k.parse::<i64>()
        .map_or_else(|_| common::json_str(k), |n| n.to_string())
}

#[test]
fn collator_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("collator") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "meta" => {
                let (k, _) = Collator::new(row[1].s()).expect("coll");
                c.check(
                    ctx,
                    &format!("s:{}", k.locale(0).expect("l")),
                    &row[2].show(),
                );
                c.check(
                    ctx,
                    &format!("s:{}", k.locale(1).expect("l")),
                    &row[3].show(),
                );
                c.check(ctx, &format!("i:{}", k.strength()), &row[4].show());
            }
            "cmp" => {
                let (k, _) = Collator::new(row[1].s()).expect("coll");
                c.check(ctx, &cmp_all(&k), &row[2].show());
            }
            "cmpstr" => {
                let (mut k, _) = Collator::new(row[1].s()).expect("coll");
                k.set_strength(row[2].i());
                c.check(ctx, &cmp_all(&k), &row[3].show());
            }
            "keys" => {
                let (k, _) = Collator::new(row[1].s()).expect("coll");
                let keys: Vec<String> = WORDS
                    .iter()
                    .map(|w| {
                        k.sort_key(w.as_bytes())
                            .expect("k")
                            .unwrap_or_default()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect()
                    })
                    .collect();
                c.check(
                    ctx,
                    &format!("a:{}", common::json_strs(&keys)),
                    &row[2].show(),
                );
            }
            "attr" => {
                let (k, _) = Collator::new("en_US").expect("coll");
                let got = k
                    .get_attribute(row[1].i())
                    .map_or_else(|e| err_name(&e), |v| format!("i:{v}"));
                c.check(ctx, &got, &row[2].show());
            }
            "setattr" => {
                let (mut k, _) = Collator::new("en_US").expect("coll");
                let ok = k
                    .set_attribute(row[1].i(), row[2].i())
                    .map_or_else(|e| err_name(&e), |()| "~T".into());
                c.check(ctx, &ok, &row[3].show());
                let got = k
                    .get_attribute(row[1].i())
                    .map_or_else(|e| err_name(&e), |v| format!("i:{v}"));
                c.check(ctx, &got, &row[4].show());
                for (i, (a, b)) in [("a", "A"), ("10", "9"), ("a b", "ab"), ("côte", "coté")]
                    .iter()
                    .enumerate()
                {
                    c.check(
                        ctx,
                        &format!("i:{}", k.compare(a.as_bytes(), b.as_bytes()).expect("c")),
                        &row[5 + i].show(),
                    );
                }
            }
            "sort" => {
                let (k, _) = Collator::new(row[1].s()).expect("coll");
                let V::Arr(a) = &row[2] else { panic!() };
                let (vals, keys) = sort_values(&json(a));
                let svs: Vec<SortValue<'_>> = vals.iter().map(sv).collect();
                let order = k.sort_order(&svs, row[3].i()).expect("sort");
                let sorted = format!(
                    "a:[{}]",
                    order
                        .iter()
                        .map(|&i| render(&vals[i]))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                c.check(ctx, &sorted, &row[4].show());
                let ks = format!(
                    "a:[{}]",
                    order
                        .iter()
                        .map(|&i| key_json(&keys[i]))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                c.check(ctx, &ks, &row[5].show());
            }
            "sortkeys" => {
                let (k, _) = Collator::new(row[1].s()).expect("coll");
                let V::Arr(a) = &row[2] else { panic!() };
                let (vals, _) = sort_values(&json(a));
                let svs: Vec<SortValue<'_>> = vals.iter().map(sv).collect();
                let order = k.sort_with_sort_keys_order(&svs).expect("sort");
                let sorted = format!(
                    "a:[{}]",
                    order
                        .iter()
                        .map(|&i| render(&vals[i]))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                c.check(ctx, &sorted, &row[3].show());
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("collator");
}
