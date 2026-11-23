// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Break iterators checked against fixtures from `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

mod common;

use common::{load, Checker, V};
use php_intl::breakiterator::{Class, IntlBreakIterator, Kind, PartsCursor, PartsKey, DONE};

fn make(k: &str, loc: &str) -> IntlBreakIterator {
    let kind = match k {
        "createCharacterInstance" => Kind::Character,
        "createWordInstance" => Kind::Word,
        "createLineInstance" => Kind::Line,
        "createSentenceInstance" => Kind::Sentence,
        "createTitleInstance" => Kind::Title,
        _ => return IntlBreakIterator::create_code_point(),
    };
    IntlBreakIterator::create(kind, Some(loc)).expect("bi").0
}

fn arr(v: &[i64]) -> String {
    format!(
        "a:[{}]",
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn vec_json(v: &[i64]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Mirrors the engine's foreach over an `IntlPartsIterator`.
fn parts(bi: &mut IntlBreakIterator, key: PartsKey) -> String {
    let mut c = PartsCursor::new(key);
    c.rewind(bi);
    let mut index: i64 = -1;
    let mut out = Vec::new();
    loop {
        index += 1;
        if index > 0 {
            if let Some(i) = c.move_forward(bi) {
                index = i;
            }
        }
        if !c.valid() {
            break;
        }
        let v = String::from_utf8_lossy(c.current().expect("cur")).into_owned();
        out.push(format!("[{},{}]", c.key(index), common::json_str(&v)));
    }
    format!("a:[{}]", out.join(","))
}

#[test]
fn breakiterator_matches_php() {
    php_intl::common::set_ini_default_locale(Some("en_US"));
    let mut c = Checker::default();
    for row in load("breakiterator") {
        let ctx = || row.iter().map(V::show).collect::<Vec<_>>().join(" | ");
        match row[0].s() {
            "meta" => {
                let bi = make(row[1].s(), row[2].s());
                let class = match bi.class() {
                    Class::RuleBased => "IntlRuleBasedBreakIterator",
                    Class::CodePoint => "IntlCodePointBreakIterator",
                };
                c.check(ctx, &format!("s:{class}"), &row[3].show());
                if bi.class() == Class::RuleBased {
                    c.check(
                        ctx,
                        &format!("s:{}", bi.locale(0).expect("l")),
                        &row[4].show(),
                    );
                    c.check(
                        ctx,
                        &format!("s:{}", bi.locale(1).expect("l")),
                        &row[5].show(),
                    );
                }
            }
            "walk" => {
                let mut bi = make(row[1].s(), row[2].s());
                let t = row[3].b();
                bi.set_text(t).expect("set");
                let rb = bi.class() == Class::RuleBased;
                let mut fwd = vec![bi.first()];
                let mut st = Vec::new();
                if rb {
                    st.push(vec_json(&bi.rule_status_vec().expect("v")));
                }
                loop {
                    let n = bi.next(None).expect("n");
                    if n == DONE {
                        break;
                    }
                    fwd.push(n);
                    if rb {
                        let s = bi.rule_status();
                        st.push(format!(
                            "[{s},{}]",
                            vec_json(&bi.rule_status_vec().expect("v"))
                        ));
                    }
                }
                c.check(ctx, &arr(&fwd), &row[4].show());
                let mut back = vec![bi.last()];
                loop {
                    let n = bi.previous();
                    if n == DONE {
                        break;
                    }
                    back.push(n);
                }
                c.check(ctx, &arr(&back), &row[5].show());
                let (mut fol, mut pre, mut isb) = (Vec::new(), Vec::new(), Vec::new());
                let len = i64::try_from(t.len()).expect("len");
                for i in -1..=len + 1 {
                    fol.push(bi.following(i).expect("f"));
                    pre.push(bi.preceding(i).expect("p"));
                    isb.push(i64::from(bi.is_boundary(i).expect("b")));
                    isb.push(bi.current());
                }
                c.check(ctx, &arr(&fol), &row[6].show());
                c.check(ctx, &arr(&pre), &row[7].show());
                c.check(ctx, &arr(&isb), &row[8].show());
                let mut nx = Vec::new();
                for n in [0, 1, 2, -1, 5, -3] {
                    bi.first();
                    bi.next(Some(2)).expect("n");
                    nx.push(bi.next(Some(n)).expect("n"));
                    nx.push(bi.current());
                }
                c.check(ctx, &arr(&nx), &row[9].show());
                c.check(ctx, &format!("a:[{}]", st.join(",")), &row[10].show());
                if bi.class() == Class::CodePoint {
                    c.check(ctx, &format!("i:{}", bi.last_code_point()), &row[11].show());
                }
            }
            "parts" => {
                let mut bi = make(row[1].s(), row[2].s());
                bi.set_text(row[3].b()).expect("set");
                let key = IntlBreakIterator::check_parts_key_type(row[4].i()).expect("k");
                c.check(ctx, &parts(&mut bi, key), &row[5].show());
            }
            "cpseq" | "badrules" | "badmsg" => {}
            "rules" => {
                let (mut bi, _) = IntlBreakIterator::create(Kind::Word, Some("en")).expect("bi");
                let r = bi.rules().expect("rules");
                c.check(ctx, &format!("i:{}", r.len()), &row[2].show());
                c.check(
                    ctx,
                    &format!("s:{}", r.chars().take(300).collect::<String>()),
                    &row[3].show(),
                );
            }
            "rbbirules" => {
                let mut bi = IntlBreakIterator::from_rules(
                    b"$LN = [[:letter:] [:number:]];$S = [.;,:];!!forward;$LN+ {1};$S+ {42};!!reverse;$LN+ {1};$S+ {42};!!safe_forward;!!safe_reverse;",
                    false,
                )
                .expect("rbbi");
                c.check(
                    ctx,
                    &format!("s:{}", bi.rules().expect("r")),
                    &row[1].show(),
                );
            }
            other => panic!("unknown op {other}"),
        }
    }
    c.finish("breakiterator");
}

#[test]
fn bad_rules() {
    let e = IntlBreakIterator::from_rules(b"[[", false).unwrap_err();
    assert_eq!(
        e,
        php_intl::Error::IntlException(
            "IntlRuleBasedBreakIterator::__construct(): unable to create RuleBasedBreakIterator from rules (parse error on line 1, offset 1)".into()
        )
    );
}
