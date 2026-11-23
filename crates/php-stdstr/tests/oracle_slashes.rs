// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `addslashes()`, `stripslashes()`, `addcslashes()`,
//! `stripcslashes()` and `quotemeta()`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, pure, regenerate, Case};
use php_stdstr::slashes::{addcslashes, addslashes, quotemeta, stripcslashes, stripslashes};

const STRS: &[&[u8]] = &[
    b"",
    b"plain",
    b"O'Reilly",
    b"say \"hi\"",
    b"back\\slash",
    b"nul\0byte",
    b"\0",
    b"\\",
    b"\\\\",
    b"\\0",
    b"\\00",
    b"\\'\\\"",
    b"trailing\\",
    b"\\x41\\x4a\\x4G\\x\\xg",
    b"\\101\\60\\7\\777\\400\\8\\9",
    b"\\n\\r\\t\\v\\f\\a\\b\\e\\q\\\\",
    b"\\1234",
    b"\\x123",
    b"\\X41",
    b"a\nb\tc\rd\x07e\x0bf\x08g\x0ch\x01i\x7fj\x80k\xffl",
    b"1+1=2? [a-z]* ^x$ (y) .\\",
    b"zoo['.']",
    b"foo[bar]",
    b"Hello World! \xe4\xf6\xfc",
    b"0123456789abcdefABCDEF~!@#$%^&*()_+`-=",
];

const LISTS: &[&[u8]] = &[
    b"",
    b"A..Z",
    b"a..z",
    b"A..z",
    b"\0..\x1f",
    b"\0..\xff",
    b"\x7f..\xff",
    b"[]'\"\\",
    b"z..a",
    b"..a",
    b"a..",
    b"a..b..c",
    b"..",
    b"...",
    b"\n\t\r",
    b"o",
    b"0..9",
];

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for s in STRS {
        let l = lit(s);
        v.push(pure(&format!("addslashes({l})"), addslashes(s)));
        v.push(pure(&format!("stripslashes({l})"), stripslashes(s)));
        v.push(pure(&format!("stripcslashes({l})"), stripcslashes(s)));
        v.push(pure(&format!("quotemeta({l})"), quotemeta(s)));
        v.push(pure(
            &format!("stripslashes(addslashes({l}))"),
            stripslashes(&addslashes(s)),
        ));
        for list in LISTS {
            v.push(case(&format!("addcslashes({l}, {})", lit(list)), |n| {
                Ok::<_, php_stdstr::StdError>(addcslashes(s, list, n))
            }));
            v.push(case(
                &format!("stripcslashes(addcslashes({l}, {}))", lit(list)),
                |n| Ok::<_, php_stdstr::StdError>(stripcslashes(&addcslashes(s, list, n))),
            ));
        }
    }
    v
}

#[test]
fn oracle() {
    check("slashes", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("slashes", &cases());
}
