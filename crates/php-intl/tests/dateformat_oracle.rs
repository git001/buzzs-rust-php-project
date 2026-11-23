// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `IntlDateFormatter` checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{err_name, load, show_s, Checker, V};
use php_intl::calendar::{IntlCalendar, PhpDateTime};
use php_intl::dateformat::{
    self, CalendarArg, DateArg, IntlDateFormatter, ObjectArg, ObjectFormat, Timestamp, TmArray,
    TmElem,
};
use php_intl::timezone::{PhpZone, TimeZone, TimeZoneArg};
use php_intl::Error;

const DEFAULT_TZ: &str = "Europe/Lisbon";

fn new_fmt(
    loc: &str,
    d: i64,
    t: i64,
    tz: &str,
    cal: CalendarArg<'_>,
    pattern: Option<&str>,
) -> IntlDateFormatter {
    IntlDateFormatter::new(
        Some(loc),
        d,
        t,
        Some(TimeZoneArg::Str(tz.as_bytes())),
        cal,
        pattern.map(str::as_bytes),
        DEFAULT_TZ,
    )
    .expect("create")
}

fn date_arg(v: &V) -> DateArg<'_> {
    match v {
        V::Int(i) => DateArg::Int(*i),
        V::Float(f) => DateArg::Float(*f),
        V::Str(s) => DateArg::Str(s.as_bytes()),
        o => panic!("{o:?}"),
    }
}

fn show_ts(r: &php_intl::Result<Timestamp>) -> String {
    match r {
        Ok(Timestamp::Int(i)) => format!("i:{i}"),
        Ok(Timestamp::Float(f)) => format!("d:{}", common::php_float(*f)),
        Err(e) if e.intl_code() == php_intl::UErrorCode::ZERO && e.intl_message().is_none() => {
            "s:!U_ZERO_ERROR".into()
        }
        Err(e) => err_name(e),
    }
}

fn show_off(o: Option<i64>, orig: &V) -> String {
    match o {
        Some(v) => format!("i:{v}"),
        None => orig.show(),
    }
}

fn dt(s: &str) -> PhpDateTime {
    match s {
        "2012-02-29 13:14:15.5 America/Sao_Paulo" => PhpDateTime {
            timestamp: 1_330_532_055,
            microseconds: 500_000,
            zone: Some(PhpZone::Id("America/Sao_Paulo".into())),
        },
        "2000-01-01 +05:30" => PhpDateTime {
            timestamp: 946_665_000,
            microseconds: 0,
            zone: Some(PhpZone::Offset(19800)),
        },
        "@1234567890" => PhpDateTime {
            timestamp: 1_234_567_890,
            microseconds: 0,
            zone: None,
        },
        o => panic!("{o}"),
    }
}

#[test]
fn dateformat_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("dateformat") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "meta" => {
                let f = new_fmt(
                    row[1].s(),
                    row[2].i(),
                    row[3].i(),
                    "America/New_York",
                    CalendarArg::Null,
                    None,
                );
                c.check(ctx, &show_s(f.pattern()), &row[4].show());
                c.check(ctx, &show_s(f.locale(0)), &row[5].show());
                c.check(ctx, &show_s(f.locale(1)), &row[6].show());
                c.check(
                    ctx,
                    &format!("i:{}", f.calendar_type().unwrap_or(-1)),
                    &row[7].show(),
                );
                c.check(ctx, &show_s(f.time_zone_id()), &row[8].show());
                c.check(ctx, &V::Bool(f.is_lenient()).show(), &row[9].show());
            }
            "fmt" => {
                let f = new_fmt(
                    row[1].s(),
                    row[2].i(),
                    row[3].i(),
                    "America/New_York",
                    CalendarArg::Null,
                    None,
                );
                c.check(ctx, &show_s(f.format(date_arg(&row[4]))), &row[5].show());
            }
            "fmtcal" => {
                let f = new_fmt(
                    "en_US",
                    0,
                    0,
                    row[1].s(),
                    CalendarArg::Type(row[2].i()),
                    None,
                );
                c.check(ctx, &show_s(f.format(date_arg(&row[3]))), &row[4].show());
            }
            "pattern" => {
                let f = new_fmt(
                    row[2].s(),
                    0,
                    0,
                    "Europe/Lisbon",
                    CalendarArg::Null,
                    Some(row[1].s()),
                );
                c.check(ctx, &show_s(f.pattern()), &row[3].show());
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Int(1_341_100_800))),
                    &row[4].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Int(-1_000_000_000))),
                    &row[5].show(),
                );
            }
            "parse" | "localtime" => {
                let mut f = new_fmt(
                    "en_US",
                    -2,
                    -2,
                    "UTC",
                    CalendarArg::Null,
                    Some("yyyy-MM-dd HH:mm:ss"),
                );
                let off = match &row[2] {
                    V::Null => None,
                    v => Some(v.i()),
                };
                if row[0].s() == "parse" {
                    let (r, o) = f.parse(row[1].s().as_bytes(), off);
                    c.check(ctx, &show_ts(&r), &row[3].show());
                    c.check(ctx, &show_off(o, &row[2]), &row[4].show());
                } else {
                    let (r, o) = f.localtime(row[1].s().as_bytes(), off);
                    let got = match r {
                        Ok(v) => format!(
                            "a:{{{}}}",
                            v.iter()
                                .map(|(k, v)| format!("\"{k}\":{v}"))
                                .collect::<Vec<_>>()
                                .join(",")
                        ),
                        Err(e) if e.intl_code() == php_intl::UErrorCode::ZERO => {
                            "s:!U_ZERO_ERROR".into()
                        }
                        Err(e) => err_name(&e),
                    };
                    c.check(ctx, &got, &row[3].show());
                    c.check(ctx, &show_off(o, &row[2]), &row[4].show());
                }
            }
            "strict" => {
                let mut f = new_fmt(
                    "en_US",
                    -2,
                    -2,
                    "UTC",
                    CalendarArg::Null,
                    Some("yyyy-MM-dd HH:mm:ss"),
                );
                f.set_lenient(false);
                c.check(
                    ctx,
                    &show_ts(&f.parse(row[1].s().as_bytes(), None).0),
                    &row[2].show(),
                );
            }
            "roundtrip" => {
                let f = new_fmt(
                    row[1].s(),
                    -2,
                    -2,
                    "Europe/Lisbon",
                    CalendarArg::Null,
                    Some(row[2].s()),
                );
                let s = f.format(DateArg::Int(1_341_100_800)).expect("fmt");
                c.check(ctx, &format!("s:{s}"), &row[3].show());
                let (r, o) = f.parse(s.as_bytes(), Some(0));
                c.check(ctx, &show_ts(&r), &row[4].show());
                // master reports UTF-8 offsets, php 8.3 UTF-16 ones.
                if s.is_ascii() {
                    c.check(ctx, &show_off(o, &V::Null), &row[5].show());
                }
            }
            "arr" => {
                let f = new_fmt("en_US", 0, 0, "UTC", CalendarArg::Null, None);
                let V::Arr(json) = &row[1] else { panic!() };
                let mut tm = TmArray::default();
                for part in json.trim_matches(|ch| ch == '{' || ch == '}').split(',') {
                    let (k, v) = part.split_once(':').expect("kv");
                    let v = TmElem::Int(v.parse().expect("int"));
                    match k.trim_matches('"') {
                        "tm_year" => tm.year = v,
                        "tm_mon" => tm.mon = v,
                        "tm_mday" => tm.mday = v,
                        "tm_hour" => tm.hour = v,
                        "tm_min" => tm.min = v,
                        "tm_sec" => tm.sec = v,
                        _ => {}
                    }
                }
                c.check(ctx, &show_s(f.format(DateArg::Array(&tm))), &row[2].show());
            }
            "dtfmt" => {
                let f = new_fmt("en_US", 0, 0, "UTC", CalendarArg::Null, None);
                let d = dt(row[1].s());
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::DateTime(&d))),
                    &row[2].show(),
                );
            }
            "strfmt" => {
                let f = new_fmt("en_US", 0, 0, "UTC", CalendarArg::Null, None);
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Str(row[1].s().as_bytes()))),
                    &row[2].show(),
                );
            }
            "calfmt" => {
                let f = new_fmt("en_US", 0, 0, "UTC", CalendarArg::Null, None);
                let mut k = IntlCalendar::create_instance(
                    &tz("Asia/Tokyo"),
                    Some("ja_JP@calendar=japanese"),
                )
                .expect("c");
                k.set_time(1_341_100_800_000.0).expect("t");
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Calendar(&k))),
                    &row[1].show(),
                );
            }
            "settz" | "setcal" | "setcalobj" | "trad" | "trad2" | "fmtobj" => {}
            other => panic!("unknown op {other}"),
        }
    }
    setters(&mut c);
    format_object(&mut c);
    c.finish("dateformat");
}

fn tz(s: &str) -> TimeZone {
    TimeZone::create_time_zone(s.as_bytes()).expect("tz")
}

fn setters(c: &mut Checker) {
    let rows: Vec<_> = load("dateformat")
        .into_iter()
        .filter(|r| r[0].s().starts_with("set") || r[0].s().starts_with("trad"))
        .collect();
    let mut f = new_fmt("en_US", 0, 0, "UTC", CalendarArg::Null, None);
    let mut trad = None;
    for row in &rows {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "settz" => {
                f.set_time_zone(TimeZoneArg::Str(row[1].s().as_bytes()))
                    .expect("settz");
                c.check(ctx, &show_s(f.time_zone_id()), &row[2].show());
                c.check(
                    ctx,
                    &show_s(f.time_zone().expect("tz").id()),
                    &row[3].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Int(1_341_100_800))),
                    &row[4].show(),
                );
            }
            "setcal" => {
                f.set_calendar(CalendarArg::Type(row[1].i()))
                    .expect("setcal");
                c.check(
                    ctx,
                    &format!("i:{}", f.calendar_type().unwrap_or(-1)),
                    &row[2].show(),
                );
                c.check(
                    ctx,
                    &format!("s:{}", f.calendar_object().expect("c").cal_type()),
                    &row[3].show(),
                );
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Int(-12_219_292_800 - 86400 * 20))),
                    &row[4].show(),
                );
            }
            "setcalobj" => {
                let k = IntlCalendar::create_instance(
                    &tz("Asia/Tokyo"),
                    Some("ja_JP@calendar=japanese"),
                )
                .expect("c");
                f.set_calendar(CalendarArg::Object(&k)).expect("set");
                c.check(ctx, "~F", &row[1].show());
                c.check(ctx, &show_s(f.time_zone_id()), &row[2].show());
                c.check(
                    ctx,
                    &show_s(f.format(DateArg::Int(1_341_100_800))),
                    &row[3].show(),
                );
            }
            "trad" => {
                let g = new_fmt("th_TH", 0, 0, "UTC", CalendarArg::Type(0), None);
                c.check(
                    ctx,
                    &format!("s:{}", g.calendar_object().expect("c").cal_type()),
                    &row[1].show(),
                );
                c.check(
                    ctx,
                    &show_s(g.format(DateArg::Int(1_341_100_800))),
                    &row[2].show(),
                );
                trad = Some(g);
            }
            "trad2" => {
                let g = trad.as_mut().expect("trad");
                g.set_calendar(CalendarArg::Type(1)).expect("set");
                c.check(
                    ctx,
                    &format!("s:{}", g.calendar_object().expect("c").cal_type()),
                    &row[1].show(),
                );
                c.check(
                    ctx,
                    &show_s(g.format(DateArg::Int(1_341_100_800))),
                    &row[2].show(),
                );
            }
            _ => {}
        }
    }
}

fn format_object(c: &mut Checker) {
    let d = PhpDateTime {
        timestamp: 1_341_140_400,
        microseconds: 0,
        zone: Some(PhpZone::Id("Europe/Lisbon".into())),
    };
    let cal = IntlCalendar::from_date_time(&d, Some("ja_JP@calendar=japanese")).expect("cal");
    for row in load("dateformat")
        .into_iter()
        .filter(|r| r[0].s() == "fmtobj")
    {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        let pat;
        let fmt = match &row[1] {
            V::Null => ObjectFormat::Default,
            V::Int(i) => ObjectFormat::Style(*i),
            V::Str(s) => {
                pat = s.clone();
                ObjectFormat::Pattern(pat.as_bytes())
            }
            V::Arr(a) => {
                let v: Vec<i64> = a
                    .trim_matches(|ch| ch == '[' || ch == ']')
                    .split(',')
                    .map(|x| x.parse().expect("n"))
                    .collect();
                ObjectFormat::Styles {
                    count: v.len(),
                    date: v.first().copied(),
                    time: v.get(1).copied(),
                }
            }
            o => panic!("{o:?}"),
        };
        let loc = Some(row[2].s());
        c.check(
            ctx,
            &show_s(dateformat::format_object(ObjectArg::DateTime(&d), fmt, loc)),
            &row[3].show(),
        );
        c.check(
            ctx,
            &show_s(dateformat::format_object(
                ObjectArg::Calendar(&cal),
                fmt,
                loc,
            )),
            &row[4].show(),
        );
    }
}

#[test]
fn proleptic_and_errors() {
    let f = new_fmt("en_US", 0, -1, "UTC", CalendarArg::Type(-16), None);
    assert_eq!(
        f.format(DateArg::Int(-12_219_379_200)).unwrap(),
        "Thursday, October 14, 1582"
    );
    assert_eq!(f.calendar_type(), Some(-16));
    let e = IntlDateFormatter::new(Some("en_US"), 0, 0, None, CalendarArg::Type(5), None, "UTC")
        .unwrap_err();
    assert!(e
        .intl_message()
        .unwrap()
        .starts_with("Invalid value for calendar type"));
    assert!(matches!(
        IntlDateFormatter::new(Some("en_US"), -2, 0, None, CalendarArg::Null, None, "UTC"),
        Err(Error::Intl(_))
    ));
}
