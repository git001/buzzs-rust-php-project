// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `grapheme_*` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, load, Checker, V};
use php_intl::grapheme;

fn bytes(r: php_intl::Result<Vec<u8>>) -> String {
    match r {
        Ok(v) => match String::from_utf8(v) {
            Ok(s) => format!("s:{s}"),
            Err(e) => V::Bytes(e.into_bytes()).show(),
        },
        Err(e) => err_name(&e),
    }
}

fn opt_bytes(r: php_intl::Result<Option<Vec<u8>>>) -> String {
    match r {
        Ok(None) => "s:!U_ZERO_ERROR".into(),
        Ok(Some(v)) => bytes(Ok(v)),
        Err(e) => err_name(&e),
    }
}

fn opt_int(r: php_intl::Result<Option<i64>>) -> String {
    match r {
        Ok(Some(i)) => format!("i:{i}"),
        Ok(None) => "s:!U_ZERO_ERROR".into(),
        Err(e) => err_name(&e),
    }
}

/// Compares ignoring the error code php reported (8.3 vs master differ in
/// which code stays in the global error for `false` returns).
fn norm(s: &str) -> String {
    if s.starts_with("s:!U_") {
        "s:!false".into()
    } else {
        s.to_owned()
    }
}

#[test]
fn grapheme_matches_php() {
    let mut c = Checker::default();
    for row in load("grapheme") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "len" => {
                let got = grapheme::strlen(row[1].b())
                    .map_or_else(|e| err_name(&e), |n| format!("i:{n}"));
                c.check(ctx, &norm(&got), &norm(&row[2].show()));
            }
            // php 8.3 predates master's empty-needle handling.
            "pos" if row[3].b().is_empty() => {}
            "strstr" if row[2].b().is_empty() => {}
            // php 8.3 bugs fixed in master (supplementary prefixes and
            // canonical-equivalent matches; see grapheme_strstr_*.phpt).
            "strstr"
                if String::from_utf8_lossy(row[1].b()).contains('\u{1F468}')
                    || row[4].err().is_some() => {}
            // php 8.3 has no grapheme_str_split().
            "split" if row[3].err() == Some("Error") => {}
            "pos" => {
                let (h, n, off) = (row[2].b(), row[3].b(), row[4].i());
                let r = match row[1].s() {
                    "grapheme_strpos" => grapheme::strpos(h, n, off, ""),
                    "grapheme_stripos" => grapheme::stripos(h, n, off, ""),
                    "grapheme_strrpos" => grapheme::strrpos(h, n, off, ""),
                    _ => grapheme::strripos(h, n, off, ""),
                };
                c.check(ctx, &norm(&opt_int(r)), &norm(&row[5].show()));
            }
            "strstr" => {
                let before = row[3] == V::Bool(true);
                let a = grapheme::strstr(row[1].b(), row[2].b(), before, false, "");
                let b = grapheme::strstr(row[1].b(), row[2].b(), before, true, "");
                c.check(ctx, &norm(&opt_bytes(a)), &norm(&row[4].show()));
                c.check(ctx, &norm(&opt_bytes(b)), &norm(&row[5].show()));
            }
            "substr" => {
                let l = match &row[3] {
                    V::Null => None,
                    v => Some(v.i()),
                };
                let r = grapheme::substr(row[1].b(), row[2].i(), l);
                c.check(ctx, &norm(&bytes(r)), &norm(&row[4].show()));
            }
            "extract" => {
                let (r, next) = grapheme::extract(row[1].b(), row[2].i(), row[3].i(), row[4].i());
                c.check(ctx, &norm(&bytes(r)), &norm(&row[5].show()));
                let n = next.map_or_else(|| "s:unset".to_owned(), |n| format!("i:{n}"));
                // master also advances $next past skipped continuation bytes.
                let st = row[4].i();
                let len = i64::try_from(row[1].b().len()).expect("len");
                let lstart = usize::try_from(if st < 0 { st + len } else { st }).unwrap_or(0);
                let mid = row[1]
                    .b()
                    .get(lstart)
                    .is_some_and(|&b| (0x80..0xc0).contains(&b));
                if !mid {
                    c.check(ctx, &n, &row[6].show());
                }
            }
            "split" => {
                let got = match grapheme::str_split(row[1].b(), row[2].i()) {
                    Ok(parts) => format!(
                        "a:[{}]",
                        parts
                            .iter()
                            .map(|p| common::json_str(&String::from_utf8_lossy(p)))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &norm(&got), &norm(&row[3].show()));
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("grapheme");
}
