// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Harness self-test.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{check, pure, regenerate, Case};

fn cases() -> Vec<Case> {
    vec![
        pure("1.5e-7", 1.5e-7),
        pure(
            "[1, 'a' => \"x\\0y\"]",
            common::assoc([
                (common::K::Int(0), common::V::Int(1)),
                ("a".into(), common::V::from(&b"x\0y"[..])),
            ]),
        ),
    ]
}

#[test]
fn oracle() {
    check("smoke", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("smoke", &cases());
}
