// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Transliterator` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, load, Checker, V};
use php_intl::transliterator::{self, Transliterator};

fn show(r: php_intl::Result<String>) -> String {
    r.map_or_else(|e| err_name(&e), |s| V::Str(s).show())
}

#[test]
fn transliterator_matches_php() {
    let mut c = Checker::default();
    for row in load("transliterator") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "create" => match Transliterator::create(row[1].s().as_bytes(), row[2].i()) {
                Ok(t) => {
                    c.check(ctx, &format!("s:{}", t.id()), &row[3].show());
                    let inv = Transliterator::create(row[1].s().as_bytes(), row[2].i())
                        .and_then(|t| t.create_inverse())
                        .map(|t| t.id().to_owned());
                    c.check(ctx, &show(inv), &row[4].show());
                }
                Err(e) => c.check(ctx, &err_name(&e), &row[3].show()),
            },
            "trans" => {
                let t = Transliterator::create(row[1].s().as_bytes(), row[2].i()).expect("t");
                c.check(
                    ctx,
                    &show(t.transliterate(row[3].b(), 0, -1, false)),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &show(t.transliterate(row[3].b(), 1, 3, false)),
                    &row[5].show(),
                );
            }
            "rules" => match Transliterator::create_from_rules(row[1].s().as_bytes(), row[2].i()) {
                Ok(t) => {
                    let got = format!(
                        "a:[{},{}]",
                        common::json_str(t.id()),
                        common::json_str(
                            &t.transliterate(b"abc aeiou xyz", 0, -1, false).expect("tr")
                        )
                    );
                    c.check(ctx, &got, &row[3].show());
                }
                Err(e) => {
                    c.check(ctx, &err_name(&e), &row[3].show());
                    let want = row[4]
                        .s()
                        .trim_start_matches("transliterator_create_from_rules: ");
                    let want = want.rsplit_once(": ").map_or(want, |x| x.0);
                    c.check(ctx, e.intl_message().unwrap_or(""), want);
                }
            },
            "ids" => {
                let ids = transliterator::list_ids().expect("ids");
                c.check(ctx, &format!("i:{}", ids.len()), &row[1].show());
                c.check(
                    ctx,
                    &format!("a:{}", common::json_strs(&ids[..50])),
                    &row[2].show(),
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("transliterator");
}
