// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Dumps `token_get_all()`-style output for each file given on the command
//! line, one token per line as `NAME<TAB>LINE<TAB>HEX(TEXT)`, preceded by a
//! `== path` header. Used to diff against `/usr/bin/php`.

#![expect(clippy::disallowed_methods, reason = "test code")]

use std::fmt::Write as _;
use std::io::Write as _;

fn main() {
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for path in std::env::args().skip(1) {
        let Ok(src) = std::fs::read(&path) else {
            continue;
        };
        let _ = writeln!(out, "== {path}");
        for t in php_lexer::token_get_all(&src) {
            let name = t.kind.name().map_or_else(
                || String::from_utf8_lossy(t.text).into_owned(),
                str::to_owned,
            );
            let mut hex = String::with_capacity(t.text.len() * 2);
            for b in t.text {
                let _ = write!(hex, "{b:02x}");
            }
            let line = if t.is_char() {
                "-".to_owned()
            } else {
                t.line.to_string()
            };
            let _ = writeln!(out, "{name}\t{line}\t{hex}");
        }
    }
}
