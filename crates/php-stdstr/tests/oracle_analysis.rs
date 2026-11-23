// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `similar_text()`, `levenshtein()`, `soundex()`,
//! `metaphone()`, `str_word_count()`, `count_chars()` and the
//! `php_charmask()` warnings (observed through `str_word_count()`).
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, pure, regenerate, Case, K, V};
use php_stdstr::analysis::{
    count_chars, levenshtein, metaphone, similar_text, soundex, str_word_count, CharCounts,
    WordCount,
};

const WORDS: &[&[u8]] = &[
    b"",
    b"a",
    b"World",
    b"Word",
    b"Hello World",
    b"hello world",
    b"kitten",
    b"sitting",
    b"Saturday",
    b"Sunday",
    b"PHP IS GREAT",
    b"WITH MYSQL",
    b"abcdef",
    b"fedcba",
    b"\0a\0",
    b"\xe4\xf6\xfc",
    b"Thompson",
    b"Robert",
    b"Rupert",
    b"Rubin",
    b"Ashcraft",
    b"Ashcroft",
    b"Tymczak",
    b"Pfister",
    b"Honeyman",
    b"Lloyd",
    b"Euler",
    b"Gauss",
    b"Hilbert",
    b"Knuth",
    b"Lukasiewicz",
    b"Wachs",
    b"_ __ 123",
];

const METAPHONE: &[&[u8]] = &[
    b"",
    b"-",
    b"1234",
    b"Thompson",
    b"Knight",
    b"Gnome",
    b"Pneumonia",
    b"Wright",
    b"White",
    b"Wawa",
    b"Woo",
    b"Xavier",
    b"Aeon",
    b"Eagle",
    b"Ian",
    b"Oscar",
    b"Unicorn",
    b"programming",
    b"Science",
    b"School",
    b"Christ",
    b"Schwarz",
    b"Cia",
    b"Chance",
    b"Accident",
    b"Bacci",
    b"Edge",
    b"Edgar",
    b"Judge",
    b"Laugh",
    b"Night",
    b"Bough",
    b"Hough",
    b"Daughter",
    b"Gnarl",
    b"Signed",
    b"Sign",
    b"Ginger",
    b"Giggle",
    b"Agh",
    b"Ahead",
    b"Thoughts",
    b"Phone",
    b"Queen",
    b"Session",
    b"Asia",
    b"Ship",
    b"Nation",
    b"Thatch",
    b"Watch",
    b"Vivid",
    b"Away",
    b"Box",
    b"Yes",
    b"Rhythm",
    b"Zebra",
    b"Lamb",
    b"Mumble",
    b"Dumb",
    b"Hugh",
    b"Ckafka",
    b"Weight",
    b"Tsch\xc3\xbcss",
    b"ab\0cd",
    b"Hello World, this is a test of metaphone() and more WORDS",
    b"Tic Tac Toe",
    b"aeiou",
    b"GHOST",
    b"GH",
    b"TH",
    b"tH",
    b"Schmidt",
    b"Sciatic",
    b"Dg",
    b"Dge",
    b"Gn",
    b"Gned",
    b"x",
    b"wh",
    b"wr",
    b"w",
    b"cc",
    b"Mcclellan",
];

const WC_STRS: &[&[u8]] = &[
    b"",
    b"Hello World",
    b"Hello fri3nd, you're looking good today!",
    b"'quoted' -dash- trail-",
    b"-",
    b"'",
    b"''",
    b"--",
    b"a-b'c",
    b"Hello\0World",
    b"\xe4bc d\xe9f",
    b"123 abc 456",
    b"  spaced   out  ",
    b"one-two three'four -five six-",
    b"PHP 8.4 _under_scores_",
];

const CHARLISTS: &[Option<&[u8]>] = &[
    None,
    Some(b""),
    Some(b"0..9"),
    Some(b"'"),
    Some(b"-"),
    Some(b"\xe4\xe9"),
    Some(b"_"),
    Some(b"..a"),
    Some(b"a.."),
    Some(b"z..a"),
    Some(b"a..b..c"),
    Some(b"0..9.."),
    Some(b"..."),
    Some(b"...."),
    Some(b"a...b"),
    Some(b".."),
];

fn wc_value(w: WordCount) -> V {
    match w {
        WordCount::Count(n) => V::Int(n),
        WordCount::Words(ws) => V::from(ws),
        WordCount::Positions(ps) => V::Arr(
            ps.into_iter()
                .map(|(k, w)| (K::Int(k), V::Str(w)))
                .collect(),
        ),
    }
}

fn cc_value(c: CharCounts) -> V {
    match c {
        CharCounts::Array(a) => V::Arr(
            a.into_iter()
                .map(|(b, n)| (K::Int(i64::from(b)), V::Int(n)))
                .collect(),
        ),
        CharCounts::Bytes(b) => V::Str(b),
    }
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for a in WORDS {
        v.push(pure(&format!("soundex({})", lit(a)), soundex(a)));
        for b in WORDS {
            let (la, lb) = (lit(a), lit(b));
            let (sim, pct) = similar_text(a, b);
            v.push(pure(
                &format!("(function () {{ $p = null; $r = similar_text({la}, {lb}, $p); return [$r, $p]; }})()"),
                vec![V::Int(sim), V::Float(pct)],
            ));
            v.push(pure(
                &format!("levenshtein({la}, {lb})"),
                levenshtein(a, b, 1, 1, 1),
            ));
        }
    }
    let costs: &[(i64, i64, i64)] = &[
        (2, 1, 1),
        (1, 2, 1),
        (1, 1, 2),
        (0, 0, 0),
        (5, 3, 7),
        (-1, -1, -1),
        (1, 1000, 1),
        (i64::MAX, 1, 1),
    ];
    let pairs: &[(&[u8], &[u8])] = &[
        (b"", b""),
        (b"", b"abc"),
        (b"abc", b""),
        (b"kitten", b"sitting"),
        (b"sitting", b"kitten"),
        (b"1234567890", b"0987654321"),
        (b"abc", b"abc"),
        (b"flaw", b"lawn"),
    ];
    for (a, b) in pairs {
        for (i, r, d) in costs {
            v.push(pure(
                &format!("levenshtein({}, {}, {i}, {r}, {d})", lit(a), lit(b)),
                levenshtein(a, b, *i, *r, *d),
            ));
        }
    }
    for w in METAPHONE {
        for m in [0i64, 1, 2, 3, 5, -1] {
            v.push(case(&format!("metaphone({}, {m})", lit(w)), |_| {
                metaphone(w, m)
            }));
        }
    }
    for s in WC_STRS {
        for fmt in [0i64, 1, 2, 3, -1] {
            for cl in CHARLISTS {
                let cls = cl.map_or_else(|| "null".to_owned(), lit);
                v.push(case(
                    &format!("str_word_count({}, {fmt}, {cls})", lit(s)),
                    |n| str_word_count(s, fmt, *cl, n).map(wc_value),
                ));
            }
        }
    }
    let cc_strs: &[&[u8]] = &[b"", b"Hello World!", b"\0\xff\0abcabc", b"zyxZYX"];
    for s in cc_strs {
        for mode in [0i64, 1, 2, 3, 4, 5, -1] {
            v.push(case(&format!("count_chars({}, {mode})", lit(s)), |_| {
                count_chars(s, mode).map(cc_value)
            }));
        }
    }
    v
}

#[test]
fn oracle() {
    check("analysis", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("analysis", &cases());
}
