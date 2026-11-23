// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `strcmp()` & friends, `strnatcmp()`/`strnatcasecmp()`
//! and `substr_compare()`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, pure, regenerate, Case};
use php_stdstr::compare::{strcasecmp, strcmp, strnatcmp, strncasecmp, strncmp, substr_compare};

const STRS: &[&[u8]] = &[
    b"",
    b"a",
    b"A",
    b"b",
    b"Z",
    b"abc",
    b"ABC",
    b"abd",
    b"ab",
    b"abcd",
    b"\0",
    b"a\0b",
    b"a\0c",
    b"\xff",
    b"\xe4",
    b"[",
    b"_",
    b"10",
    b"9",
    b"010",
    b"0010",
    b"1.010",
    b"1.01",
    b"0.1",
    b"0.01",
    b"img12.png",
    b"img10.png",
    b"IMG2.png",
    b"x01",
    b"x1",
    b"x001y",
    b" 0",
    b" 00",
    b"  a",
    b"a  b",
    b"a b",
    b"a\tb ",
    b"0",
    b"00",
    b"000",
    b"0a",
    b"1a2",
    b"1a10",
    b"Hello World",
    b"hello world",
    b"123456789012345678901234567890",
    b"123456789012345678901234567891",
];

const NAT_EXTRA: &[(&[u8], &[u8])] = &[
    (b"rfc1.txt", b"rfc2086.txt"),
    (b"rfc822.txt", b"rfc2086.txt"),
    (b"pic01", b"pic1"),
    (b"pic02000", b"pic2"),
    (b"pic 4 else", b"pic 4 else"),
    (b"pic 5 something", b"pic 5"),
    (b"pic4", b"pic 4 else"),
    (b"1.002.08", b"1.002.10"),
    (b"x2-g8", b"x2-y08"),
    (b"x2-y7", b"x2-y08"),
    (b"x8-y8", b"x2-y08"),
    (b"a 1", b"a  1"),
    (b"a1 ", b"a1"),
    (b"0", b" "),
    (b"00x", b"0x"),
    (b"\t\n1", b"1"),
    (b"1\0", b"1"),
    (b"Abc", b"aBD"),
];

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for a in STRS {
        for b in STRS {
            let (la, lb) = (lit(a), lit(b));
            // memcmp()'s magnitude is implementation-defined (glibc's
            // vectorised variants differ for 2..=7 byte prefixes), so only
            // the sign is compared for memcmp-based results.
            v.push(pure(
                &format!("strcmp({la}, {lb}) <=> 0"),
                strcmp(a, b).signum(),
            ));
            v.push(pure(&format!("strcasecmp({la}, {lb})"), strcasecmp(a, b)));
            v.push(pure(
                &format!("strnatcmp({la}, {lb})"),
                strnatcmp(a, b, false),
            ));
            v.push(pure(
                &format!("strnatcasecmp({la}, {lb})"),
                strnatcmp(a, b, true),
            ));
        }
    }
    for (a, b) in NAT_EXTRA {
        for (x, y) in [(a, b), (b, a)] {
            let (lx, ly) = (lit(x), lit(y));
            v.push(pure(
                &format!("strnatcmp({lx}, {ly})"),
                strnatcmp(x, y, false),
            ));
            v.push(pure(
                &format!("strnatcasecmp({lx}, {ly})"),
                strnatcmp(x, y, true),
            ));
        }
    }
    let ncmp: &[(&[u8], &[u8])] = &[
        (b"abc", b"abd"),
        (b"abc", b"ABD"),
        (b"Hello", b"hello world"),
        (b"", b"a"),
        (b"a\0b", b"a\0c"),
        (b"abcdef", b"abc"),
        (b"\xff", b"\x00"),
    ];
    for (a, b) in ncmp {
        for n in [-1i64, 0, 1, 2, 3, 5, 100, i64::MAX] {
            let (la, lb) = (lit(a), lit(b));
            v.push(case(&format!("strncmp({la}, {lb}, {n}) <=> 0"), |_| {
                strncmp(a, b, n).map(i64::signum)
            }));
            v.push(case(&format!("strncasecmp({la}, {lb}, {n})"), |_| {
                strncasecmp(a, b, n)
            }));
        }
    }
    let hay: &[&[u8]] = &[b"abcde", b"", b"ABCde", b"a\0cde"];
    let needles: &[&[u8]] = &[
        b"", b"bc", b"BC", b"df", b"abc", b"abcdef", b"de", b"e", b"\0c",
    ];
    let offsets = [0i64, 1, 2, 4, 5, 6, -1, -2, -5, -6, -10, i64::MIN];
    let lengths = [None, Some(0), Some(1), Some(2), Some(3), Some(10), Some(-1)];
    for h in hay {
        for n in needles {
            for &o in &offsets {
                for &l in &lengths {
                    for ci in [false, true] {
                        let ls = l.map_or_else(|| "null".to_owned(), |x: i64| x.to_string());
                        let os = if o == i64::MIN {
                            "PHP_INT_MIN".to_owned()
                        } else {
                            o.to_string()
                        };
                        v.push(case(
                            &format!(
                                "substr_compare({}, {}, {os}, {ls}, {ci}) <=> 0",
                                lit(h),
                                lit(n)
                            ),
                            |_| substr_compare(h, n, o, l, ci).map(i64::signum),
                        ));
                    }
                }
            }
        }
    }
    // Exact memcmp magnitudes where glibc returns the byte difference
    // (single bytes and long strings).
    let exact: &[(&[u8], &[u8])] = &[
        (b"a", b"c"),
        (b"c", b"a"),
        (b"\xff", b"\0"),
        (b"\0", b"\xff"),
        (b"Hello World!", b"Hello world!"),
        (b"abcdefghijklmnopqrstuvwxyz", b"abcdefghijklmnopqrstuvwxyA"),
        (b"abcdefghijklmnop", b"abcdefghijklmnopq"),
    ];
    for (a, b) in exact {
        v.push(pure(
            &format!("strcmp({}, {})", lit(a), lit(b)),
            strcmp(a, b),
        ));
        v.push(case(
            &format!("substr_compare({}, {}, 0)", lit(a), lit(b)),
            |_| substr_compare(a, b, 0, None, false),
        ));
        v.push(case(
            &format!("substr_compare({}, {}, 0, null, true)", lit(a), lit(b)),
            |_| substr_compare(a, b, 0, None, true),
        ));
    }
    v
}

#[test]
fn oracle() {
    check("compare", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("compare", &cases());
}
