// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `csv.rs` (`str_getcsv`, `fgetcsv`, `fputcsv`).
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, regenerate, Case, V};
use php_stdstr::csv::{csv_options, fgetcsv_parse, fputcsv_line, str_getcsv, CsvOptions};

const INPUTS: &[&[u8]] = &[
    b"",
    b"aaa,bbb",
    b"aaa,\"bbb\"",
    b"\"aaa\",\"bbb\"",
    b"\"aaa\",bbb",
    b"\"aaa\",   \"bbb\"",
    b",",
    b"aaa,",
    b",\"aaa\"",
    b"\"\",\"\"",
    b"\"\\\"\",\"aaa\"",
    b"\"\"\"\"\"\",",
    b"\"\"\"\"\",aaa\n",
    b"\"\\\"\"\",aaa\n",
    b"aaa,\"\\\"bbb,ccc\n",
    b"aaa,bbb   ",
    b"aaa,\"bbb   \"",
    b"aaa\"aaa\",\"bbb\"bbb",
    b"aaa\"aaa\"\"\",bbb",
    b"aaa\"\\\"a\",\"bbb\"",
    b"aaa,\"bbb\n",
    b"aaa,\"bbb",
    b"aaa,\"bbb\"",
    b"\"f\", \"o\", \"\"",
    b"foo||bar",
    b"foo|bar",
    b"|foo|-|bar|",
    b"|f.|.|bar|.|-|-.|",
    b".foo..bar.",
    b".foo  . .  bar  .",
    b"\" \"\" \"",
    b"\"cell1\",\"cell2\\\",\"cell3\",\"cell4\"",
    b"\n",
    b"\r\n",
    b"\r",
    b"   ",
    b" \n",
    b"a\r\n",
    b"a\r\r\n",
    b"a\n\n",
    b"\"a\r\nb\"\r\n",
    b"\"a\nb\",c\nd,e\n",
    b"a,b\nc,d\n",
    b"\"a\"x\"b\",c",
    b"\"a\"\"",
    b"\"a\\",
    b"\"a\\\"",
    b"\"\\",
    b"\"",
    b"\"\"",
    b"\"\n",
    b"\"\"\n",
    b"x,\"\n\n\"\n",
    b"  \"a\" ,  b  ,\t\"c\"\t",
    b"\t\"a\"",
    b"a,\x00b,\"c\x00d\"",
    b"caf\xc3\xa9,\"\xc3\xa9\",\xe2\x82\xac",
    b"\xff,\"\xfe\"\xfd,\xc3",
    b"\"\xc3\xa9\\\xc3\xa9\",x",
    b"\"\\\xc3\xa9\"",
    b"a;b;\"c;d\";'e;f'",
    b"'a''b';'c\\'d'",
    b"a\tb\t\"c\td\"",
    b"\xa9a\xa9\"b\xa9\"",
    b"a,b,c\r\n1,2,3\r\n",
];

fn opt_sets() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (",", "\"", "\\"),
        (",", "\"", ""),
        (";", "'", "\\"),
        ("|", "|", ""),
        ("-", "|", ""),
        (".", "|", "-"),
        (".", ".", "."),
        (" ", ".", ""),
        ("\t", "\"", "\\"),
        ("\u{a9}", "\"", ""),
    ]
}

fn b(s: &str) -> Vec<u8> {
    // Map chars to single bytes (the option strings are Latin-1 bytes).
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

fn record(r: Option<Vec<Vec<u8>>>) -> V {
    match r {
        None => V::from(vec![V::Null]),
        Some(f) => V::from(f),
    }
}

/// `fgets()`-style line splitting (`\n` terminated).
fn lines(s: &[u8]) -> Vec<Vec<u8>> {
    s.split_inclusive(|&c| c == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

fn fgetcsv_all(s: &[u8], opts: CsvOptions) -> V {
    let mut it = lines(s).into_iter();
    let mut out = Vec::new();
    while let Some(first) = it.next() {
        out.push(record(fgetcsv_parse(first, &opts, &mut || it.next())));
    }
    V::from(out)
}

fn php_list(fields: &[&[u8]]) -> String {
    let parts: Vec<String> = fields.iter().map(|f| lit(f)).collect();
    format!("[{}]", parts.join(", "))
}

/// (separator, enclosure, escape, expected master error)
type BadArgs = (&'static [u8], &'static [u8], &'static [u8], &'static str);

const FPUT: &[&[&[u8]]] = &[
    &[b"aaa", b"bbb"],
    &[b"aaa", b"\"bbb\""],
    &[b"", b""],
    &[b"\"\"\"\"\"\"", b""],
    &[b"aaa", b"\"\\\"bbb", b"ccc"],
    &[b"aaa\"\\\"a\"", b"\"bbb\""],
    &[b"\"\\\"\"", b"\"aaa\""],
    &[b"\"\\\"\"\"", b"aaa"],
    &[b"a b", b"c\td", b"e\nf", b"g\rh"],
    &[b"a\\", b"b\\\"", b"\\\"\"", b"x\\y"],
    &[b"a;b", b"c'd", b"e|f", b"g-h", b"i.j"],
    &[],
    &[b"single"],
    &[b"\xc3\xa9\"x", b"\xa9"],
];

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for &input in INPUTS {
        let l = lit(input);
        for (d, e, x) in opt_sets() {
            let (bd, be, bx) = (b(d), b(e), b(x));
            let (ld, le, lx) = (lit(&bd), lit(&be), lit(&bx));
            v.push(case(&format!("str_getcsv({l}, {ld}, {le}, {lx})"), |n| {
                let o = csv_options("str_getcsv", 2, Some(&bd), Some(&be), Some(&bx), n)?;
                Ok(record(str_getcsv(input, &o)))
            }));
            v.push(case(
                &format!(
                    "(function ($s) {{ $f = fopen('php://memory', 'w+'); fwrite($f, $s); rewind($f); $r = []; while (($l = fgetcsv($f, null, {ld}, {le}, {lx})) !== false) $r[] = $l; return $r; }})({l})"
                ),
                |n| {
                    let o = csv_options("fgetcsv", 3, Some(&bd), Some(&be), Some(&bx), n)?;
                    Ok(fgetcsv_all(input, o))
                },
            ));
        }
    }
    for &fields in FPUT {
        let pl = php_list(fields);
        for (d, e, x) in opt_sets() {
            let (bd, be, bx) = (b(d), b(e), b(x));
            let (ld, le, lx) = (lit(&bd), lit(&be), lit(&bx));
            for eol in [None, Some(&b"\r\n"[..]), Some(&b""[..])] {
                let eol_php = eol.map_or("null".to_owned(), lit);
                v.push(case(
                    &format!(
                        "(function ($a) {{ $f = fopen('php://memory', 'w+'); $n = fputcsv($f, $a, {ld}, {le}, {lx}, {eol_php}); rewind($f); return [$n, stream_get_contents($f)]; }})({pl})"
                    ),
                    |n| {
                        let o = csv_options("fputcsv", 3, Some(&bd), Some(&be), Some(&bx), n)?;
                        let line = fputcsv_line(fields, &o, eol);
                        Ok(V::from(vec![V::from(line.len()), V::from(line)]))
                    },
                ));
            }
        }
    }
    // argument errors
    // (separator, enclosure, escape, the error str_getcsv() throws in
    // php-src master — PHP 8.3 did not validate str_getcsv()'s arguments)
    let bad: &[BadArgs] = &[
        (
            b"separator",
            b"\"",
            b"",
            "Argument #2 ($separator) must be a single character",
        ),
        (
            b"",
            b"\"",
            b"",
            "Argument #2 ($separator) must be a single character",
        ),
        (
            b",",
            b"enclosure",
            b"",
            "Argument #3 ($enclosure) must be a single character",
        ),
        (
            b",",
            b"",
            b"",
            "Argument #3 ($enclosure) must be a single character",
        ),
        (
            b",",
            b"\"",
            b"escape",
            "Argument #4 ($escape) must be empty or a single character",
        ),
        (
            b"",
            b"",
            b"ee",
            "Argument #2 ($separator) must be a single character",
        ),
    ];
    for &(d, e, x, msg) in bad {
        let (ld, le, lx) = (lit(d), lit(e), lit(x));
        v.push(
            case(&format!("str_getcsv('a', {ld}, {le}, {lx})"), |n| {
                let o = csv_options("str_getcsv", 2, Some(d), Some(e), Some(x), n)?;
                Ok(record(str_getcsv(b"a", &o)))
            })
            .expect(&format!("!! ValueError: str_getcsv(): {msg}")),
        );
        v.push(case(
            &format!("fputcsv(fopen('php://memory', 'w'), ['a'], {ld}, {le}, {lx})"),
            |n| {
                let o = csv_options("fputcsv", 3, Some(d), Some(e), Some(x), n)?;
                Ok(fputcsv_line(&[b"a"], &o, None).len())
            },
        ));
        v.push(case(
            &format!("fgetcsv(fopen('php://memory', 'r'), null, {ld}, {le}, {lx})"),
            |n| {
                let o = csv_options("fgetcsv", 3, Some(d), Some(e), Some(x), n)?;
                Ok(record(str_getcsv(b"", &o)))
            },
        ));
    }
    // php-src master (8.4+) deprecates omitting $escape; PHP 8.3 does not.
    v.push(
        case("str_getcsv('a,b')", |n| {
            let o = csv_options("str_getcsv", 2, None, None, None, n)?;
            Ok(record(str_getcsv(b"a,b", &o)))
        })
        .expect(
            "Deprecated: str_getcsv(): the $escape parameter must be provided as its default value will change\n=> array (\n  0 => 'a',\n  1 => 'b',\n)",
        ),
    );
    v.push(
        case("str_getcsv('a', ';', \"'\")", |n| {
            let o = csv_options("str_getcsv", 2, Some(b";"), Some(b"'"), None, n)?;
            Ok(record(str_getcsv(b"a", &o)))
        })
        .expect(
            "Deprecated: str_getcsv(): the $escape parameter must be provided as its default value will change\n=> array (\n  0 => 'a',\n)",
        ),
    );
    v
}

#[test]
fn oracle() {
    check("csv", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("csv", &cases());
}
