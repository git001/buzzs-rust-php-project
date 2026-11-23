// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `version_compare()`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, pure, regenerate, Case};
use php_stdstr::versioning::{version_compare, version_compare_op};

const VERSIONS: &[&[u8]] = &[
    b"",
    b"1",
    b"2",
    b"10",
    b"1.0",
    b"1.0.0",
    b"1.0.1",
    b"1.1",
    b"1..2",
    b"1.0-dev",
    b"1.0a1",
    b"1.0alpha2",
    b"1.0b1",
    b"1.0beta",
    b"1.0RC1",
    b"1.0rc2",
    b"1.0pl1",
    b"1.0p",
    b"1.0#1",
    b"#1",
    b"1.0foo",
    b"-1",
    b"a",
    b"dev",
    b"1.0_dev+x",
    b"5.3.0-dev",
    b"5.3.0",
    b"99999999999999999999.1",
    b"1.0 rc 1",
    b"1.0\x00z",
];

const OPS: &[&[u8]] = &[
    b"<", b"lt", b"<=", b"le", b">", b"gt", b">=", b"ge", b"==", b"=", b"eq", b"!=", b"<>", b"ne",
    b"", b"l", b"??", b"LT",
];

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for a in VERSIONS {
        for b in VERSIONS {
            v.push(pure(
                &format!("version_compare({}, {})", lit(a), lit(b)),
                version_compare(a, b),
            ));
        }
    }
    // php-src master (unlike 8.3) drops a trailing `.` after
    // canonicalisation (version_compare.phpt: "1.2. = 1.2.").
    for (a, b, r) in [
        (&b"1.2."[..], &b"1.2."[..], 0),
        (b"1.2..", b"1.2..", 0),
        (b"1.2.", b"1.2", 0),
        (b"1.0", b"1.0-", 0),
        (b".", b"", 1),
        (b"..", b"1", -1),
    ] {
        v.push(
            pure(
                &format!("version_compare({}, {})", lit(a), lit(b)),
                version_compare(a, b),
            )
            .expect(&format!("=> {r}")),
        );
    }
    for op in OPS {
        for (a, b) in [
            (&b"1.0"[..], &b"1.0.1"[..]),
            (b"1.0", b"1.0"),
            (b"1.0pl1", b"1.0"),
        ] {
            v.push(case(
                &format!("version_compare({}, {}, {})", lit(a), lit(b), lit(op)),
                |_| version_compare_op(a, b, op),
            ));
        }
    }
    v
}

#[test]
fn oracle() {
    check("versioning", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("versioning", &cases());
}
