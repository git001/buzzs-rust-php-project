// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! IDN and `Normalizer` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, load, Checker, V};
use php_intl::{idn, normalizer};

#[test]
fn idn_normalizer_matches_php() {
    let mut c = Checker::default();
    for row in load("idn_normalizer") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "idn" => {
                let f = if row[1].s() == "idn_to_ascii" {
                    idn::to_ascii
                } else {
                    idn::to_utf8
                };
                match f(row[2].b(), row[3].i(), idn::VARIANT_UTS46) {
                    Ok(o) => {
                        let v = o.value.map_or_else(
                            || "s:!U_ZERO_ERROR".to_owned(),
                            |v| V::Str(String::from_utf8_lossy(&v).into_owned()).show(),
                        );
                        c.check(ctx, &v, &row[4].show());
                        let info = o.info.map_or_else(
                            || "~N".to_owned(),
                            |i| {
                                format!(
                                    "a:{{\"result\":{},\"isTransitionalDifferent\":{},\"errors\":{}}}",
                                    common::json_str(&String::from_utf8_lossy(&i.result)),
                                    i.is_transitional_different,
                                    i.errors
                                )
                            },
                        );
                        c.check(ctx, &info, &row[5].show());
                    }
                    Err(_) => c.check(
                        ctx,
                        "false",
                        if row[4].err().is_some() {
                            "false"
                        } else {
                            "ok"
                        },
                    ),
                }
            }
            "norm" => {
                let n = normalizer::normalize(row[1].b(), row[2].i())
                    .map_or_else(|e| err_name(&e), |s| V::Str(s).show());
                c.check(ctx, &n, &row[3].show());
                let i = normalizer::is_normalized(row[1].b(), row[2].i()).map_or_else(
                    |_| "s:no".to_owned(),
                    |b| format!("s:{}", if b { "yes" } else { "no" }),
                );
                c.check(ctx, &i, &row[4].show());
            }
            "raw" => {
                let r = normalizer::raw_decomposition(row[1].b(), row[2].i()).expect("raw");
                c.check(
                    ctx,
                    &r.map_or_else(|| "~N".to_owned(), |s| V::Str(s).show()),
                    &row[3].show(),
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("idn_normalizer");
}
