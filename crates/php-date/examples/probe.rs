// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle probe: reads tab-separated commands on stdin and prints results
//! in the same canonical format as `tools/probe.php`, so the two can be
//! diffed against `/usr/bin/php`.
//!
//! Commands (fields backslash-escaped: `\t`, `\n`, `\\`):
//! * `parse <str>` — `date_parse()`
//! * `pff <format> <str>` — `date_parse_from_format()`
//! * `strtotime <tz> <now> <str>`
//! * `date <tz> <ts> <format>`
//! * `create <tz> <str> <format>` — `new DateTime($str)` then `format()`
//! * `modify <tz> <str> <modifier> <format>`
//! * `diff <tz> <a> <b> <format>` — `$a->diff($b)->format($format)`
//! * `cff <tz> <cff-format> <str> <format>` — `createFromFormat` then `format()`

#![expect(clippy::disallowed_methods, reason = "test code")]

use std::io::{self, BufRead, Write};

#[path = "../tests/common/mod.rs"]
mod common;

use common::{run, unescape};

fn main() {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let fields: Vec<Vec<u8>> = line.split('\t').map(|f| unescape(f.as_bytes())).collect();
        let r = run(&fields);
        let _ = writeln!(out, "{r}");
    }
}
