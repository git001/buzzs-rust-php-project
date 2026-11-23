// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `search.rs`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check_mapped, lit, pure, regenerate, Case, V};
use php_stdstr::search::{
    explode, str_contains, str_ends_with, str_starts_with, strcspn, stripos, stristr, strpbrk,
    strpos, strrchr, strripos, strrpos, strspn, strstr, substr, substr_count, substr_replace,
    Strtok,
};
use php_stdstr::Notices;
use std::fmt::Write as _;

const HAYS: &[&[u8]] = &[
    b"",
    b"a",
    b"abc",
    b"Hello World, hello world",
    b"aaaa",
    b"abcabcabc",
    b"0123456789a123456789b",
    b"x\0y\0z",
    b"MiXeD\xc3\xa9",
];

const NEEDLES: &[&[u8]] = &[
    b"", b"a", b"A", b"o", b"world", b"WORLD", b"aa", b"abc", b"\0", b"zz", b"9", b"bc",
];

const OFFSETS: &[i64] = &[
    0,
    1,
    2,
    -1,
    -2,
    -3,
    3,
    10,
    -10,
    21,
    -21,
    22,
    -22,
    i64::MAX,
    i64::MIN,
];

fn opt(o: Option<i64>) -> String {
    o.map_or_else(|| "null".to_owned(), |x| x.to_string())
}

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for h in HAYS {
        let lh = lit(h);
        for &o in OFFSETS {
            v.push(pure(
                &format!("substr({lh}, {o})"),
                substr(h, o, None).to_vec(),
            ));
            for &l in &[0i64, 1, 2, -1, -2, 5, -10, 100, i64::MIN, i64::MAX] {
                v.push(pure(
                    &format!("substr({lh}, {o}, {l})"),
                    substr(h, o, Some(l)).to_vec(),
                ));
                v.push(pure(
                    &format!("substr_replace({lh}, 'XY', {o}, {l})"),
                    substr_replace(h, b"XY", o, Some(l)),
                ));
            }
            v.push(pure(
                &format!("substr_replace({lh}, '', {o})"),
                substr_replace(h, b"", o, None),
            ));
        }
        for n in NEEDLES {
            let ln = lit(n);
            v.push(pure(
                &format!("str_contains({lh}, {ln})"),
                str_contains(h, n),
            ));
            v.push(pure(
                &format!("str_starts_with({lh}, {ln})"),
                str_starts_with(h, n),
            ));
            v.push(pure(
                &format!("str_ends_with({lh}, {ln})"),
                str_ends_with(h, n),
            ));
            for before in [false, true] {
                v.push(pure(
                    &format!("strstr({lh}, {ln}, {before})"),
                    strstr(h, n, before),
                ));
                v.push(pure(
                    &format!("stristr({lh}, {ln}, {before})"),
                    stristr(h, n, before),
                ));
                v.push(pure(
                    &format!("strrchr({lh}, {ln}, {before})"),
                    strrchr(h, n, before),
                ));
            }
            v.push(case(&format!("strpbrk({lh}, {ln})"), |_| strpbrk(h, n)));
            for &o in OFFSETS {
                v.push(case(&format!("strpos({lh}, {ln}, {o})"), |_| {
                    strpos(h, n, o)
                }));
                v.push(case(&format!("stripos({lh}, {ln}, {o})"), |_| {
                    stripos(h, n, o)
                }));
                v.push(case(&format!("strrpos({lh}, {ln}, {o})"), |_| {
                    strrpos(h, n, o)
                }));
                v.push(case(&format!("strripos({lh}, {ln}, {o})"), |_| {
                    strripos(h, n, o)
                }));
                for l in [
                    None,
                    Some(0),
                    Some(1),
                    Some(-1),
                    Some(5),
                    Some(-5),
                    Some(30),
                ] {
                    v.push(case(
                        &format!("substr_count({lh}, {ln}, {o}, {})", opt(l)),
                        |_| substr_count(h, n, o, l),
                    ));
                    v.push(pure(
                        &format!("strspn({lh}, {ln}, {o}, {})", opt(l)),
                        strspn(h, n, o, l),
                    ));
                    v.push(pure(
                        &format!("strcspn({lh}, {ln}, {o}, {})", opt(l)),
                        strcspn(h, n, o, l),
                    ));
                }
            }
        }
        for d in [&b""[..], b",", b"a", b"bc", b"abcabcabcabc", b"l", b"\0"] {
            for limit in [i64::MAX, 0, 1, 2, 3, -1, -2, -5, i64::MIN] {
                v.push(case(&format!("explode({}, {lh}, {limit})", lit(d)), |_| {
                    explode(d, h, limit)
                }));
            }
        }
    }
    for (s, toks) in [
        (
            &b"This is\tan example\nstring"[..],
            &[&b" \n\t"[..], b" \n\t", b" ", b"\n", b"\t", b"x"][..],
        ),
        (b"  a  b  ", &[b" ", b" ", b" ", b" "]),
        (b"/something", &[b"/", b"/"]),
        (b"a,,b", &[b",", b"", b","]),
        (b"", &[b"x", b"x"]),
        (b",,,", &[b",", b","]),
        (b"abc", &[b"", b"", b"b"]),
    ] {
        let mut php = format!("[strtok({}, {})", lit(s), lit(toks[0]));
        for t in &toks[1..] {
            let _ = write!(php, ", strtok({})", lit(t));
        }
        php.push(']');
        v.push(case(&php, |n: &mut Notices| {
            let mut st = Strtok::new();
            let mut out = vec![V::from(st.start(s, toks[0], n))];
            for t in &toks[1..] {
                out.push(V::from(st.next(t, n)));
            }
            Ok::<_, php_stdstr::StdError>(V::from(out))
        }));
    }
    for c in &mut v {
        // 8.3's php_strcspn() stopped at a NUL byte when $characters is
        // empty; master's lookup-table version scans the whole window.
        if c.php.starts_with("strcspn(\"x\\x00y\\x00z\", '', ") {
            let rust = c.rust.clone();
            *c = c.clone().expect(&rust);
        }
    }
    v
}

fn master(_php: &str, out: &str) -> String {
    out.replace(
        "explode(): Argument #1 ($separator) cannot be empty",
        "explode(): Argument #1 ($separator) must not be empty, use str_split() to split a string into characters",
    )
    .replace("cannot be empty", "must not be empty")
}

#[test]
fn oracle() {
    check_mapped("search", &cases(), master);
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("search", &cases());
}
