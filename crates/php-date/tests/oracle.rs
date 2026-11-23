// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tables: thousands of inputs (mined from php-src's ext/date tests
//! plus deterministic random grammar combinations) with the output PHP
//! produced for them (see `tools/gen_oracle.py` and `tools/probe.php`).

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use std::fs;
use std::path::Path;

fn split_tab(line: &[u8]) -> Vec<Vec<u8>> {
    line.split(|&b| b == b'\t').map(common::unescape).collect()
}

fn check(name: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(format!("{name}.tsv"));
    let data = fs::read(&path).expect("oracle table");
    let mut failures = Vec::new();
    let mut total = 0;
    for line in data.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let sep = line
            .windows(4)
            .position(|w| w == b"\t=>\t")
            .expect("separator");
        let fields = split_tab(&line[..sep]);
        let expected = common::unescape(&line[sep + 4..]);
        let got = common::run(&fields);
        total += 1;
        if got.as_bytes() != expected.as_slice() && got != String::from_utf8_lossy(&expected) {
            failures.push(format!(
                "{}\n  php:  {}\n  rust: {}",
                String::from_utf8_lossy(&line[..sep]),
                String::from_utf8_lossy(&expected),
                got
            ));
        }
    }
    assert!(total > 0);
    assert!(
        failures.is_empty(),
        "{} of {total} {name} cases differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn oracle_date_parse() {
    check("parse");
}

#[test]
fn oracle_strtotime() {
    check("strtotime");
}

#[test]
fn oracle_date_format() {
    check("date");
}

#[test]
fn oracle_date_parse_from_format() {
    check("pff");
}

#[test]
fn oracle_datetime_objects() {
    check("objects");
}

#[test]
fn oracle_intervals_periods_zones() {
    check("misc");
}

#[test]
fn oracle_functions() {
    check("functions");
}
