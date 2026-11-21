// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Replays `tests/data/oracle.tsv`, generated from the reference PHP
//! (`tools/gen_oracle.php`), against this crate.

use php_iconv::{Diagnostic, HeaderValue, Iconv, IconvFilter, MimeEncodePrefs, Outcome};
use std::fmt::Write as _;

#[derive(Debug, Clone)]
enum Val {
    Str(Vec<u8>),
    Int(i64),
    Null,
    Prefs(MimeEncodePrefs),
    Chunks(Vec<Vec<u8>>),
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn parse(v: &str) -> Val {
    if v == "n" {
        return Val::Null;
    }
    let (tag, rest) = v.split_once(':').expect("tag");
    match tag {
        "s" => Val::Str(unhex(rest)),
        "i" => Val::Int(rest.parse().expect("int")),
        "c" => Val::Chunks(rest.split('/').map(unhex).collect()),
        "p" => {
            let f: Vec<&str> = rest.split(',').collect();
            let opt = |x: &str| (x != "-").then(|| unhex(x));
            Val::Prefs(MimeEncodePrefs {
                scheme: opt(f[0]),
                input_charset: opt(f[1]),
                output_charset: opt(f[2]),
                line_length: (f[3] != "-").then(|| f[3].parse().expect("ll")),
                line_break_chars: opt(f[4]),
            })
        }
        _ => panic!("bad value {v}"),
    }
}

fn s(v: &Val) -> &[u8] {
    match v {
        Val::Str(b) => b,
        other => panic!("expected string, got {other:?}"),
    }
}

fn i(v: &Val) -> i64 {
    match v {
        Val::Int(n) => *n,
        other => panic!("expected int, got {other:?}"),
    }
}

fn opt_s(v: Option<&Val>) -> Option<&[u8]> {
    match v {
        None | Some(Val::Null) => None,
        Some(x) => Some(s(x)),
    }
}

fn opt_i(v: Option<&Val>) -> Option<i64> {
    match v {
        None | Some(Val::Null) => None,
        Some(x) => Some(i(x)),
    }
}

fn diag(d: Option<&Diagnostic>) -> String {
    match d {
        None => "-".into(),
        Some(Diagnostic::Notice(m)) => format!("N:{m}"),
        Some(Diagnostic::Warning(m)) => format!("W:{m}"),
        Some(Diagnostic::Deprecated(m)) => format!("D:{m}"),
        Some(Diagnostic::ValueError(m)) => format!("V:{m}"),
        Some(other) => format!("?:{other}"),
    }
}

fn enc_str(o: &Outcome<Vec<u8>>) -> String {
    o.value
        .as_ref()
        .map_or("F".into(), |v| format!("s:{}", hex(v)))
}

fn enc_int(o: &Outcome<i64>) -> String {
    o.value.map_or("F".into(), |v| format!("i:{v}"))
}

#[allow(clippy::too_many_lines)]
fn run_line(ic: &Iconv, line: &str) -> Option<String> {
    let cols: Vec<&str> = line.split('\t').collect();
    let arrow = cols.iter().position(|c| *c == "=>").expect("=>");
    let func = cols[0];
    let args: Vec<Val> = cols[1..arrow].iter().map(|c| parse(c)).collect();
    let expected = format!("{}\t{}", cols[arrow + 1], cols[arrow + 2]);
    let got = match func {
        "iconv" => {
            let o = ic.iconv(s(&args[0]), s(&args[1]), s(&args[2]));
            format!("{}\t{}", enc_str(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_strlen" => {
            let o = ic.strlen(s(&args[0]), opt_s(args.get(1)));
            format!("{}\t{}", enc_int(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_substr" => {
            let o = ic.substr(
                s(&args[0]),
                i(&args[1]),
                opt_i(args.get(2)),
                opt_s(args.get(3)),
            );
            format!("{}\t{}", enc_str(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_strpos" => {
            let o = ic.strpos(
                s(&args[0]),
                s(&args[1]),
                opt_i(args.get(2)).unwrap_or(0),
                opt_s(args.get(3)),
            );
            let v = if matches!(o.diagnostic, Some(Diagnostic::ValueError(_))) {
                "n".into()
            } else {
                enc_int(&o)
            };
            format!("{v}\t{}", diag(o.diagnostic.as_ref()))
        }
        "iconv_strrpos" => {
            let o = ic.strrpos(s(&args[0]), s(&args[1]), opt_s(args.get(2)));
            format!("{}\t{}", enc_int(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_mime_encode" => {
            let Val::Prefs(p) = &args[2] else {
                panic!("prefs")
            };
            let o = ic.mime_encode(s(&args[0]), s(&args[1]), p);
            format!("{}\t{}", enc_str(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_mime_decode" => {
            let o = ic.mime_decode(
                s(&args[0]),
                opt_i(args.get(1)).unwrap_or(0),
                opt_s(args.get(2)),
            );
            format!("{}\t{}", enc_str(&o), diag(o.diagnostic.as_ref()))
        }
        "iconv_mime_decode_headers" => {
            let o = ic.mime_decode_headers(
                s(&args[0]),
                opt_i(args.get(1)).unwrap_or(0),
                opt_s(args.get(2)),
            );
            let v = o.value.as_ref().map_or("F".into(), |hs| {
                let parts: Vec<String> = hs
                    .iter()
                    .map(|(k, v)| match v {
                        HeaderValue::Single(x) => format!("{}={}", hex(k), hex(x)),
                        HeaderValue::Multiple(xs) => format!(
                            "{}=[{}]",
                            hex(k),
                            xs.iter().map(|x| hex(x)).collect::<Vec<_>>().join(",")
                        ),
                    })
                    .collect();
                format!("h:{}", parts.join(";"))
            });
            format!("{v}\t{}", diag(o.diagnostic.as_ref()))
        }
        "filter" => {
            let Val::Chunks(chunks) = &args[1] else {
                panic!("chunks")
            };
            match IconvFilter::from_filter_name(s(&args[0])) {
                // The streams layer reports the factory failure.
                None => format!(
                    "F\tW:Unable to create or locate filter \"{}\"",
                    String::from_utf8_lossy(s(&args[0]))
                ),
                Some(mut f) => {
                    let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
                    // Each fwrite() is one bucket; a failing bucket is dropped
                    // but the stream keeps going.  stream_filter_remove()
                    // flushes with the closing flag; on failure the streams
                    // layer warns and fclose() flushes once more.
                    let mut out = Vec::new();
                    let mut warns: Vec<String> = Vec::new();
                    for b in &refs {
                        match f.append_bucket(Some(b)) {
                            Ok(o) => out.extend(o),
                            Err(w) => warns.push(format!("W:{w}")),
                        }
                    }
                    match f.append_bucket(None) {
                        Ok(o) => out.extend(o),
                        Err(w) => {
                            warns.push(format!("W:{w}"));
                            warns.push("W:Unable to flush filter, not removing".into());
                            if let Err(w) = f.append_bucket(None) {
                                warns.push(format!("W:{w}"));
                            }
                        }
                    }
                    let d = if warns.is_empty() {
                        "-".to_string()
                    } else {
                        warns.join("|")
                    };
                    format!("s:{}\t{d}", hex(&out))
                }
            }
        }
        other => panic!("unknown func {other}"),
    };
    (got != expected).then(|| format!("{line}\n   got: {got}"))
}

#[test]
fn oracle() {
    php_stdmisc::locale::reset_lc_ctype();
    let data = include_str!("data/oracle.tsv");
    let ic = Iconv::new();
    let mut failures = Vec::new();
    let mut total = 0;
    for line in data.lines() {
        total += 1;
        if let Some(f) = run_line(&ic, line) {
            failures.push(f);
        }
    }
    let show: Vec<&String> = failures.iter().take(40).collect();
    assert!(
        failures.is_empty(),
        "{} of {total} oracle cases failed:\n{}",
        failures.len(),
        show.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
