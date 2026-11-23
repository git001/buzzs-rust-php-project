// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `IntlCalendar` checked against fixtures from `/usr/bin/php`.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

mod common;

use common::{err_name, json_strs, load, php_float, Checker, V};
use php_intl::calendar::{self, IntlCalendar, Limit, PhpDateTime};
use php_intl::timezone::{PhpZone, TimeZone};

fn tz(s: &str) -> TimeZone {
    TimeZone::create_time_zone(s.as_bytes()).expect("tz")
}

fn cal(z: &str, l: &str) -> IntlCalendar {
    IntlCalendar::create_instance(&tz(z), Some(l)).expect("cal")
}

fn f(v: f64) -> String {
    format!("d:{}", php_float(v))
}

fn b(v: bool) -> String {
    V::Bool(v).show()
}

fn ints(v: &[i64]) -> String {
    format!(
        "a:[{}]",
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn calendar_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("calendar") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "create" => {
                let k = cal(row[1].s(), row[2].s());
                let class = if k.is_gregorian() {
                    "IntlGregorianCalendar"
                } else {
                    "IntlCalendar"
                };
                let got = [
                    format!("s:{class}"),
                    format!("s:{}", k.cal_type()),
                    format!("s:{}", k.locale(0).expect("loc")),
                    format!("s:{}", k.locale(1).expect("loc")),
                    format!("i:{}", k.first_day_of_week()),
                    format!("i:{}", k.minimal_days_in_first_week()),
                    format!("s:{}", k.time_zone().expect("tz").id().expect("id")),
                ];
                for (i, g) in got.iter().enumerate() {
                    c.check(ctx, g, &row[3 + i].show());
                }
            }
            "dow" => {
                let k = cal(row[1].s(), row[2].s());
                c.check(
                    ctx,
                    &format!("i:{}", k.day_of_week_type(row[3].i()).expect("t")),
                    &row[4].show(),
                );
                let got = k
                    .weekend_transition(row[3].i())
                    .map_or_else(|_| "~F".to_owned(), |v| format!("i:{v}"));
                c.check(ctx, &got, &row[5].show());
            }
            "fields" => {
                let mut k = cal(row[1].s(), row[2].s());
                k.set_time(row[3].f()).expect("set");
                let v: Vec<i64> = (0..24).map(|i| k.get(i).expect("get")).collect();
                c.check(ctx, &ints(&v), &row[4].show());
                c.check(ctx, &b(k.in_daylight_time().expect("dst")), &row[5].show());
                c.check(ctx, &b(k.is_weekend(None).expect("we")), &row[6].show());
                c.check(
                    ctx,
                    &b(k.is_weekend(Some(row[3].f() + 86_400_000.0 * 3.0))
                        .expect("we")),
                    &row[7].show(),
                );
            }
            "limits" => {
                let mut k = cal(row[1].s(), row[2].s());
                k.set_time(row[3].f()).expect("set");
                let mut parts = Vec::new();
                for fl in 0..24 {
                    let l = [
                        Limit::ActualMinimum,
                        Limit::ActualMaximum,
                        Limit::Minimum,
                        Limit::Maximum,
                        Limit::GreatestMinimum,
                        Limit::LeastMaximum,
                    ]
                    .map(|w| k.limit(fl, w).expect("limit"));
                    parts.push(ints(&l)[2..].to_owned());
                }
                c.check(ctx, &format!("a:[{}]", parts.join(",")), &row[4].show());
            }
            "todt" => {
                let mut k = cal(row[1].s(), row[2].s());
                k.set_time(row[3].f()).expect("set");
                if let Ok((ts, spec)) = k.to_date_time() {
                    let zone = match spec {
                        php_intl::timezone::DateTimeZoneSpec::Id(s) => s,
                        php_intl::timezone::DateTimeZoneSpec::Offset(o) => {
                            format!(
                                "{}{:02}:{:02}",
                                if o < 0 { '-' } else { '+' },
                                o.abs() / 3600,
                                (o.abs() / 60) % 60
                            )
                        }
                    };
                    c.check(ctx, &format!("s:{ts} {zone}"), &row[4].show());
                }
            }
            "addroll" => {
                let mut k = cal("UTC", row[1].s());
                k.set_time(1_341_100_800_000.0).expect("set");
                k.add(row[2].i(), row[3].i()).expect("add");
                c.check(ctx, &f(k.get_time().expect("t")), &row[4].show());
                k.set_time(1_341_100_800_000.0).expect("set");
                k.roll(row[2].i(), row[3].i()).expect("roll");
                c.check(ctx, &f(k.get_time().expect("t")), &row[5].show());
            }
            "fdiff" => {
                let mut k = cal("UTC", row[1].s());
                k.set_time(1_341_100_800_000.0).expect("set");
                let got = match k.field_difference(row[2].f(), row[3].i()) {
                    Ok(v) => format!("i:{v}"),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &got, &row[4].show());
                c.check(ctx, &f(k.get_time().expect("t")), &row[5].show());
            }
            "cleared" | "set2" | "setdate" | "setdt" | "setdt5" | "clearfield" => {}
            "wall" => {
                let mut k = cal("America/New_York", "en_US");
                k.set_lenient(row[1] == V::Bool(true));
                k.set_repeated_wall_time_option(row[2].i()).expect("rep");
                k.set_skipped_wall_time_option(row[3].i()).expect("skip");
                c.check(ctx, &b(k.is_lenient()), &row[4].show());
                let mut got = Vec::new();
                for args in [
                    [2013, 2, 10, 2, 30],
                    [2013, 10, 3, 1, 30],
                    [2013, 1, 31, 0, 0],
                ] {
                    k.clear(None).expect("clear");
                    k.set_date_time(args[0], args[1], args[2], args[3], args[4], Some(0))
                        .expect("set");
                    got.push(match k.get_time() {
                        Ok(t) => f(t),
                        Err(e) => err_name(&e),
                    });
                }
                for (i, g) in got.iter().enumerate() {
                    c.check(ctx, g, &row[7 + i].show());
                }
            }
            "greg" => {
                let V::Arr(a) = &row[1] else { panic!() };
                let args: Vec<i64> = a
                    .trim_matches(|ch| ch == '[' || ch == ']')
                    .split(',')
                    .map(|x| x.parse().expect("n"))
                    .collect();
                let g = IntlCalendar::gregorian_from_fields(&args, "Europe/Lisbon").expect("greg");
                c.check(ctx, &f(g.get_time().expect("t")), &row[2].show());
                c.check(
                    ctx,
                    &format!("s:{}", g.time_zone().expect("tz").id().expect("id")),
                    &row[3].show(),
                );
                c.check(ctx, &format!("s:{}", g.cal_type()), &row[5].show());
            }
            "leap" => {
                let g = IntlCalendar::gregorian(&tz("UTC"), Some("en_US")).expect("g");
                c.check(
                    ctx,
                    &b(g.is_leap_year(row[1].i()).expect("leap")),
                    &row[2].show(),
                );
            }
            "change" => {
                let g = IntlCalendar::gregorian(&tz("UTC"), Some("en_US")).expect("g");
                c.check(ctx, &f(g.gregorian_change()), &row[1].show());
            }
            "change2" | "change3" => {
                let mut g = IntlCalendar::gregorian(&tz("UTC"), Some("en_US")).expect("g");
                let (d, y1, y2) = if row[0].s() == "change2" {
                    (-1e20, 1500, 1700)
                } else {
                    (0.0, 1900, 1970)
                };
                g.set_gregorian_change(d).expect("set");
                c.check(ctx, &f(g.gregorian_change()), &row[1].show());
                c.check(ctx, &b(g.is_leap_year(y1).expect("l")), &row[2].show());
                c.check(ctx, &b(g.is_leap_year(y2).expect("l")), &row[3].show());
                if row.len() > 4 {
                    c.check(ctx, &b(g.is_leap_year(1972).expect("l")), &row[4].show());
                }
            }
            "cmp" => {
                let mut a = cal("UTC", "en_US");
                a.set_time(1000.0).expect("t");
                let mut bb = cal("UTC", "en_US");
                bb.set_time(2000.0).expect("t");
                let mut cc = cal("Europe/Lisbon", "en_US");
                cc.set_time(1000.0).expect("t");
                let got = [
                    a.equals(&bb).expect("e"),
                    a.before(&bb).expect("b"),
                    a.after(&bb).expect("a"),
                    a.equals(&cc).expect("e"),
                    a.is_equivalent_to(&bb),
                    a.is_equivalent_to(&cc),
                ];
                for (i, g) in got.iter().enumerate() {
                    c.check(ctx, &b(*g), &row[1 + i].show());
                }
            }
            "kw" => {
                let got = match calendar::keyword_values_for_locale(
                    row[1].s(),
                    row[2].s(),
                    row[3] == V::Bool(true),
                ) {
                    Ok(v) => format!("a:{}", json_strs(&v)),
                    Err(e) => err_name(&e),
                };
                c.check(ctx, &got, &row[4].show());
            }
            "avail" => c.check(
                ctx,
                &format!("a:{}", json_strs(&calendar::available_locales())),
                &row[1].show(),
            ),
            "fromdt" | "fromdt2" | "fromdt3" => {
                let (dt, loc) = match row[0].s() {
                    "fromdt" => (
                        PhpDateTime {
                            timestamp: 1_330_532_055,
                            microseconds: 500_000,
                            zone: Some(PhpZone::Id("America/Sao_Paulo".into())),
                        },
                        None,
                    ),
                    "fromdt2" => (
                        PhpDateTime {
                            timestamp: 1_234_567_890,
                            microseconds: 0,
                            zone: None,
                        },
                        Some("ja_JP@calendar=japanese"),
                    ),
                    _ => (
                        PhpDateTime {
                            timestamp: 946_665_000,
                            microseconds: 0,
                            zone: Some(PhpZone::Offset(19800)),
                        },
                        None,
                    ),
                };
                let k = IntlCalendar::from_date_time(&dt, loc).expect("fromdt");
                c.check(ctx, &f(k.get_time().expect("t")), &row[1].show());
                c.check(
                    ctx,
                    &format!("s:{}", k.time_zone().expect("tz").id().expect("id")),
                    &row[2].show(),
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("calendar");
}

#[test]
fn set_variants() {
    let mut k = cal("UTC", "en_US");
    k.clear(None).unwrap();
    assert_eq!(k.get_time().unwrap(), 0.0);
    assert!(!k.is_set(1).unwrap());
    k.set(&[1, 2010]).unwrap();
    k.set(&[2, 5]).unwrap();
    assert_eq!(k.get_time().unwrap(), 1_275_350_400_000.0);
    k.set_date(2012, 1, 29).unwrap();
    assert_eq!(k.get_time().unwrap(), 1_330_473_600_000.0);
    k.set_date_time(2012, 1, 29, 13, 14, Some(15)).unwrap();
    assert_eq!(k.get_time().unwrap(), 1_330_521_255_000.0);
    k.set_date_time(2012, 1, 29, 13, 14, None).unwrap();
    assert_eq!(k.get_time().unwrap(), 1_330_521_255_000.0);
    k.clear(Some(11)).unwrap();
    assert_eq!(k.get_time().unwrap(), 1_330_474_455_000.0);
    assert!(matches!(
        k.set(&[1, 2, 3, 4]),
        Err(php_intl::Error::ArgumentCount(_))
    ));
    assert_eq!(
        k.set(&[1, 1 << 40]),
        Err(php_intl::Error::arg_value(
            2,
            "must be between -2147483648 and 2147483647"
        ))
    );
}
