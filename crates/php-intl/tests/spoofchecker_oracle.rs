// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Spoofchecker` checked against fixtures from `/usr/bin/php` (8.3 has
//! only the ICU checks; skeleton and bidi methods are covered by the phpt
//! ports in `phpt_spoofchecker.rs`).
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines)]

mod common;

use common::{load, Checker, V};
use php_intl::spoofchecker::{checks, level, Report, Spoofchecker};

fn configure(x: &mut Spoofchecker, name: &str) -> Report {
    use checks::{
        ANY_CASE, CHAR_LIMIT, HIDDEN_OVERLAY, INVISIBLE, MIXED_NUMBERS, MIXED_SCRIPT_CONFUSABLE,
        SINGLE_SCRIPT, SINGLE_SCRIPT_CONFUSABLE, WHOLE_SCRIPT_CONFUSABLE,
    };
    let lvl = |x: &mut Spoofchecker, l| {
        x.set_restriction_level(l).expect("level");
        Report::default()
    };
    match name {
        "default" => Report::default(),
        "locales_en" => x.set_allowed_locales(b"en_US"),
        "locales_en_ko" => x.set_allowed_locales(b"en_US, ko_KR"),
        "locales_gr" => x.set_allowed_locales(b"gr_GR"),
        "locales_ru" => x.set_allowed_locales(b"ru"),
        "locales_bad" => x.set_allowed_locales(b"!!"),
        "checks_mixed" => x.set_checks(
            MIXED_SCRIPT_CONFUSABLE | WHOLE_SCRIPT_CONFUSABLE | SINGLE_SCRIPT_CONFUSABLE,
        ),
        "checks_single" => x.set_checks(SINGLE_SCRIPT),
        "checks_invisible" => x.set_checks(INVISIBLE),
        "checks_numbers" => x.set_checks(MIXED_NUMBERS),
        "checks_overlay" => x.set_checks(HIDDEN_OVERLAY | INVISIBLE),
        "checks_anycase" => x.set_checks(ANY_CASE | MIXED_SCRIPT_CONFUSABLE),
        "checks_charlimit" => x.set_checks(CHAR_LIMIT),
        "checks_zero" => x.set_checks(0),
        "checks_bad" => x.set_checks(1 << 20),
        "level_ascii" => lvl(x, level::ASCII),
        "level_single" => lvl(x, level::SINGLE_SCRIPT_RESTRICTIVE),
        "level_moderate" => lvl(x, level::MODERATELY_RESTRICTIVE),
        "level_minimal" => lvl(x, level::MINIMALLY_RESTRICTIVE),
        "level_unrestricted" => lvl(x, level::UNRESTRICTIVE),
        other => panic!("unknown config {other}"),
    }
}

fn warnings(method: &str, r: &Report) -> String {
    r.warnings
        .iter()
        .map(|w| format!("Spoofchecker::{method}(): {w}"))
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn spoofchecker_matches_php() {
    let mut c = Checker::default();
    let mut x = Spoofchecker::new().expect("open");
    for row in load("spoofchecker") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "config" => {
                x = Spoofchecker::new().expect("open");
                let r = configure(&mut x, row[1].s());
                let method = if row[1].s().starts_with("locales") {
                    "setAllowedLocales"
                } else {
                    "setChecks"
                };
                // php 8.3 does not set the global code here (master does).
                c.check(ctx, &warnings(method, &r), row[3].s());
            }
            "susp" => {
                let r = x.is_suspicious(row[2].b());
                c.check(ctx, &V::Bool(r.value).show(), &row[3].show());
                let code = r
                    .error_code
                    .map_or_else(|| "s:unset".to_owned(), |i| format!("i:{i}"));
                c.check(ctx, &code, &row[4].show());
                c.check(ctx, &warnings("isSuspicious", &r.report), row[7].s());
            }
            "conf" => {
                let r = x.are_confusable(row[2].b(), row[3].b());
                c.check(ctx, &V::Bool(r.value).show(), &row[4].show());
                let code = r
                    .error_code
                    .map_or_else(|| "s:unset".to_owned(), |i| format!("i:{i}"));
                c.check(ctx, &code, &row[5].show());
                c.check(ctx, &warnings("areConfusable", &r.report), row[7].s());
            }
            other => panic!("unknown row {other}"),
        }
    }
    c.finish("spoofchecker");
}
