// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `IntlTimeZone` checked against fixtures from `/usr/bin/php`.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

mod common;

use common::{json_strs, load, show_s, Checker, V};
use php_intl::timezone::{self, EnumerationFilter, PhpZone, TimeZone};

#[test]
fn timezone_matches_php() {
    let mut c = Checker::default();
    for row in load("timezone") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        let tz = |v: &V| TimeZone::create_time_zone(v.s().as_bytes()).expect("tz");
        match row[0].s() {
            "id" => c.check(ctx, &show_s(tz(&row[1]).id()), &row[2].show()),
            "dst" => c.check(
                ctx,
                &format!("i:{}", tz(&row[1]).dst_savings()),
                &row[2].show(),
            ),
            "offset" => {
                let r = tz(&row[1])
                    .offset(row[2].f(), row[3] == V::Bool(true))
                    .expect("offset");
                c.check(ctx, &format!("i:{}", r.0), &row[5].show());
                c.check(ctx, &format!("i:{}", r.1), &row[6].show());
            }
            "display" => {
                let r =
                    tz(&row[1]).display_name(row[4] == V::Bool(true), row[3].i(), Some(row[2].s()));
                c.check(ctx, &show_s(r), &row[5].show());
            }
            "canonical" => {
                let got = match timezone::canonical_id(row[1].s().as_bytes()) {
                    Ok((id, sys)) => format!("a:[{},{sys}]", common::json_str(&id)),
                    Err(e) => common::err_name(&e),
                };
                c.check(ctx, &got, &row[2].show());
            }
            "region" => c.check(
                ctx,
                &show_s(timezone::region(row[1].s().as_bytes())),
                &row[2].show(),
            ),
            "count_equiv" => {
                let n = timezone::count_equivalent_ids(row[1].s().as_bytes()).expect("count");
                c.check(ctx, &format!("i:{n}"), &row[2].show());
            }
            "equiv" => {
                let r = timezone::equivalent_id(row[1].s().as_bytes(), row[2].i());
                c.check(ctx, &show_s(r), &row[3].show());
            }
            "windows" => c.check(
                ctx,
                &show_s(timezone::windows_id(row[1].s().as_bytes())),
                &row[2].show(),
            ),
            // php 8.3 predates IntlTimeZone::getIanaID().
            "iana" if row[2].err() == Some("Error") => {}
            "iana" => c.check(
                ctx,
                &show_s(timezone::iana_id(row[1].s().as_bytes())),
                &row[2].show(),
            ),
            "to_dtz" => {
                let got = match tz(&row[1]).to_date_time_zone() {
                    Ok(timezone::DateTimeZoneSpec::Id(s)) => format!("s:{s}"),
                    Ok(timezone::DateTimeZoneSpec::Offset(o)) => {
                        let sign = if o < 0 { '-' } else { '+' };
                        format!("s:{sign}{:02}:{:02}", o.abs() / 3600, (o.abs() / 60) % 60)
                    }
                    Err(e) => common::err_name(&e),
                };
                // ext/date rejects some ICU IDs; only compare successes.
                if !row[2].s().starts_with('!') {
                    c.check(ctx, &got, &row[2].show());
                }
            }
            "same_rules" => {
                let got = tz(&row[1]).has_same_rules(&tz(&row[2]));
                c.check(ctx, &V::Bool(got).show(), &row[3].show());
            }
            "for_windows" => {
                let r = timezone::id_for_windows_id(row[1].s().as_bytes(), row[2].opt_s());
                c.check(ctx, &show_s(r), &row[3].show());
            }
            // php 8.3 treated numeric strings as raw offsets; master does not.
            "enum" if row[1].opt_s() == Some("001") => {}
            "enum" => {
                let f = row[1]
                    .opt_s()
                    .map_or(EnumerationFilter::All, EnumerationFilter::Country);
                let got = timezone::create_enumeration(f).expect("enum");
                c.check(ctx, &format!("a:{}", json_strs(&got)), &row[2].show());
            }
            "enum_off" => {
                let got = timezone::create_enumeration(EnumerationFilter::RawOffset(row[1].i()))
                    .expect("enum");
                c.check(ctx, &format!("a:{}", json_strs(&got)), &row[2].show());
            }
            "idenum" => {
                let off = match &row[3] {
                    V::Null => None,
                    v => Some(v.i()),
                };
                let got = match timezone::create_time_zone_id_enumeration(
                    row[1].i(),
                    row[2].opt_s(),
                    off,
                ) {
                    Ok(v) => format!("a:{}", json_strs(&v)),
                    Err(e) => common::err_name(&e),
                };
                c.check(ctx, &got, &row[4].show());
            }
            "from_dtz" => {
                let name = row[1].s();
                let z = if name.starts_with('+') || name.starts_with('-') {
                    let sign = if name.starts_with('-') { -1 } else { 1 };
                    let h: i64 = name[1..3].parse().expect("h");
                    let m: i64 = name[4..6].parse().expect("m");
                    PhpZone::Offset(sign * (h * 3600 + m * 60))
                } else if name.contains('/') {
                    PhpZone::Id(name.to_owned())
                } else {
                    PhpZone::Abbr(name.to_owned())
                };
                let got = match timezone::from_date_time_zone(&z) {
                    Ok(t) => show_s(t.id()),
                    Err(_) => "s:!Error".into(),
                };
                c.check(ctx, &got, &row[2].show());
            }
            "tzdata" => c.check(ctx, &show_s(timezone::tz_data_version()), &row[1].show()),
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("timezone");
}
