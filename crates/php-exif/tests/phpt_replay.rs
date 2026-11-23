// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Replays the logic of php-src master `.phpt` tests (copied to
//! `tests/fixtures/phpt`) through the crate API and matches the output
//! against the `--EXPECT--` / `--EXPECTF--` sections, like run-tests.php.
//!
//! Each case reimplements the PHP script with the crate's functions plus
//! `php-runtime`'s `var_dump`/`print_r`; warnings are printed the way the
//! CLI does (`"\nWarning: func(): msg in %s on line %d\n"`).

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::match_same_arms,
    clippy::many_single_char_names
)]

use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use php_exif::diag::Diagnostic;
use php_exif::encoding::BuiltinConverter;
use php_exif::exif::{
    exif_read_data, exif_tagname, exif_thumbnail, ExifConfig, ReadOptions, StreamOrigin,
};
use php_exif::imagesize::{getimagesize, getimagesizefromstring};
use php_exif::imagetype::exif_imagetype;
use php_runtime::dump::{print_r, var_dump};
use php_runtime::{CollectingContext, Value};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// The `oom_*` replays run on the accounting allocator, as the VM does.
php_memlimit::install_global_allocator!();

// ---------------------------------------------------------------------------
// EXPECTF matching (run-tests.php semantics).

#[derive(Debug, Clone)]
enum Tok {
    Lit(Vec<u8>),
    /// (min length, character predicate)
    Class(usize, fn(u8) -> bool),
}

fn any(_: u8) -> bool {
    true
}
fn not_nl(c: u8) -> bool {
    c != b'\n' && c != b'\r'
}
fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}
fn hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}
fn space(c: u8) -> bool {
    c.is_ascii_whitespace()
}
fn int_char(c: u8) -> bool {
    c.is_ascii_digit() || c == b'+' || c == b'-'
}
fn float_char(c: u8) -> bool {
    c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E')
}
fn sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

fn tokenize(pattern: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut lit = Vec::new();
    let mut i = 0;
    while i < pattern.len() {
        if pattern[i] == b'%' && i + 1 < pattern.len() {
            let class: Option<Tok> = match pattern[i + 1] {
                b'e' => Some(Tok::Class(1, sep)),
                b's' => Some(Tok::Class(1, not_nl)),
                b'S' => Some(Tok::Class(0, not_nl)),
                b'a' => Some(Tok::Class(1, any)),
                b'A' => Some(Tok::Class(0, any)),
                b'w' => Some(Tok::Class(0, space)),
                b'i' => Some(Tok::Class(1, int_char)),
                b'd' => Some(Tok::Class(1, digit)),
                b'x' => Some(Tok::Class(1, hex)),
                b'f' => Some(Tok::Class(1, float_char)),
                b'c' => Some(Tok::Class(1, any)),
                b'%' => {
                    lit.push(b'%');
                    None
                }
                b'0' => {
                    lit.push(0);
                    None
                }
                _ => {
                    lit.push(b'%');
                    i += 1;
                    continue;
                }
            };
            if let Some(t) = class {
                if !lit.is_empty() {
                    toks.push(Tok::Lit(std::mem::take(&mut lit)));
                }
                // %c is exactly one character.
                if pattern[i + 1] == b'c' {
                    toks.push(Tok::Class(1, any));
                    toks.push(Tok::Lit(Vec::new()));
                } else {
                    toks.push(t);
                }
            }
            i += 2;
        } else {
            lit.push(pattern[i]);
            i += 1;
        }
    }
    if !lit.is_empty() {
        toks.push(Tok::Lit(lit));
    }
    toks
}

fn matches_at(toks: &[Tok], s: &[u8]) -> bool {
    match toks.first() {
        None => s.is_empty(),
        Some(Tok::Lit(l)) => s.starts_with(l) && matches_at(&toks[1..], &s[l.len()..]),
        Some(Tok::Class(min, pred)) => {
            let mut max = s.iter().take_while(|&&c| pred(c)).count();
            if let Some(Tok::Lit(next)) = toks.get(1) {
                if let Some(&first) = next.first() {
                    // Only try lengths where the next literal can start.
                    loop {
                        if max < *min {
                            return false;
                        }
                        if s.get(max) == Some(&first) && matches_at(&toks[1..], &s[max..]) {
                            return true;
                        }
                        if max == 0 {
                            return false;
                        }
                        max -= 1;
                    }
                }
            }
            (*min..=max).rev().any(|n| matches_at(&toks[1..], &s[n..]))
        }
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &s[start..end.max(start)]
}

fn sections(phpt: &[u8]) -> (bool, Vec<u8>) {
    let text = phpt.split(|&b| b == b'\n').collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut fmt = false;
    let mut on = false;
    for line in text {
        let l = line.strip_suffix(b"\r").unwrap_or(line);
        if l.starts_with(b"--")
            && l.ends_with(b"--")
            && l.len() > 4
            && l[2..l.len() - 2]
                .iter()
                .all(|c| c.is_ascii_uppercase() || *c == b'_')
        {
            on = l == b"--EXPECT--" || l == b"--EXPECTF--";
            fmt = fmt || l == b"--EXPECTF--";
            continue;
        }
        if on {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    (fmt, out)
}

fn check(phpt: &str, output: &[u8]) -> Result<(), String> {
    let data =
        std::fs::read(fixtures().join("phpt").join(phpt)).map_err(|e| format!("{phpt}: {e}"))?;
    let (fmt, expect) = sections(&data);
    // run-tests.php normalises CRLF to LF on both sides.
    let crlf = |s: &[u8]| -> Vec<u8> {
        let mut o = Vec::with_capacity(s.len());
        for (i, &c) in s.iter().enumerate() {
            if !(c == b'\r' && s.get(i + 1) == Some(&b'\n')) {
                o.push(c);
            }
        }
        o
    };
    let expect = crlf(&expect);
    let output = crlf(output);
    let want = trim(&expect);
    let got = trim(&output);
    let ok = if fmt {
        matches_at(&tokenize(want), got)
    } else {
        want == got
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{phpt}\n----- expected\n{}\n----- got\n{}",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(got)
        ))
    }
}

// ---------------------------------------------------------------------------
// A tiny "script" environment.

#[derive(Default)]
struct Out(Vec<u8>, String);

impl Out {
    fn echo(&mut self, s: impl AsRef<[u8]>) {
        self.0.extend_from_slice(s.as_ref());
    }

    fn diags(&mut self, func: &str, diags: &[Diagnostic]) {
        for d in diags {
            self.echo(format!("\n{}: {func}(): ", d.level.label()));
            self.echo(&d.message);
            let file = format!(" in /t/{}.php on line 2\n", self.1);
            self.echo(file);
        }
    }

    fn var_dump(&mut self, v: &Value) {
        var_dump(&mut self.0, v, &mut CollectingContext::new()).expect("var_dump");
    }

    fn print_r(&mut self, v: &Value) {
        let s = print_r(v, &mut CollectingContext::new()).expect("print_r");
        self.echo(s);
    }
}

fn file_origin(path: &Path) -> StreamOrigin {
    let meta = std::fs::metadata(path).expect("metadata");
    StreamOrigin::File {
        path: path.to_string_lossy().as_bytes().to_vec(),
        mtime: 1_700_000_000,
        size: meta.len(),
        regular: meta.is_file(),
    }
}

fn read(
    out: &mut Out,
    rel: &str,
    sections: Option<&str>,
    arrays: bool,
    thumb: bool,
    quiet: bool,
) -> Value {
    let path = fixtures().join("exif").join(rel);
    let opts = ReadOptions {
        required_sections: sections.map(|s| s.as_bytes().to_vec()),
        as_arrays: arrays,
        read_thumbnail: thumb,
    };
    let (v, d) = exif_read_data(
        File::open(&path).expect("open"),
        &file_origin(&path),
        &opts,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    if !quiet {
        out.diags("exif_read_data", &d);
    }
    v
}

fn read_bytes(out: &mut Out, data: &[u8]) -> Value {
    let (v, d) = exif_read_data(
        Cursor::new(data),
        &StreamOrigin::Other,
        &ReadOptions::default(),
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    out.diags("exif_read_data", &d);
    v
}

fn thumb(out: &mut Out, rel: &str, dims: bool) -> Option<php_exif::exif::ThumbnailResult> {
    let path = fixtures().join("exif").join(rel);
    let (r, d) = exif_thumbnail(
        File::open(&path).expect("open"),
        &file_origin(&path),
        dims,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    out.diags("exif_thumbnail", &d);
    r
}

fn thumb_value(r: Option<&php_exif::exif::ThumbnailResult>) -> Value {
    r.map_or(Value::Bool(false), |t| Value::string(t.data.as_slice()))
}

fn get(v: &Value, key: &str) -> Value {
    v.as_array()
        .and_then(|a| a.get_sym(key.as_bytes()))
        .cloned()
        .unwrap_or(Value::Null)
}

fn get_i(v: &Value, i: i64) -> Value {
    v.as_array()
        .and_then(|a| a.get_int(i))
        .cloned()
        .unwrap_or(Value::Null)
}

fn count(v: &Value) -> Value {
    Value::Int(v.as_array().map_or(1, |a| a.len() as i64))
}

fn echo_value(out: &mut Out, v: &Value) {
    match v {
        Value::Str(s) => out.echo(s.as_bytes()),
        Value::Int(i) => out.echo(i.to_string()),
        _ => {}
    }
}

type Script = Box<dyn Fn(&mut Out)>;

fn dump_read(rel: &'static str, tail: &'static str) -> impl Fn(&mut Out) {
    move |o: &mut Out| {
        let v = read(o, rel, None, false, false, false);
        o.var_dump(&v);
        o.echo(tail);
    }
}

fn cases() -> Vec<(&'static str, Script)> {
    let mut c: Vec<(&'static str, Script)> = Vec::new();
    for (phpt, img) in [
        ("bug34704.phpt", "bug34704.jpg"),
        ("bug34704-mb.phpt", "bug34704私はガラスを食べられます.jpg"),
        ("bug50845.phpt", "bug50845.jpg"),
        ("bug60150.phpt", "bug60150.jpg"),
        ("bug68547.phpt", "bug68547.jpg"),
        ("bug74428.phpt", "bug74428/bug74428.jpg"),
        ("bug77564.phpt", "bug77564/bug77564.jpg"),
        ("bug76557.phpt", "bug76557.jpg"),
        ("bug77831.phpt", "bug77831.tiff"),
        ("exif007.phpt", "image007.jpg"),
        ("exif008.phpt", "image008.jpg"),
        ("exif009.phpt", "image009.jpg"),
        ("exif010.phpt", "image010.jpg"),
        ("exif011.phpt", "image011.jpg"),
        ("exif012.phpt", "image012.jpg"),
        ("exif013.phpt", "image013.jpg"),
        ("exif014.phpt", "image014.jpg"),
        ("exif015.phpt", "image015.jpg"),
        ("exif016.phpt", "image016.tiff"),
        ("exif017.phpt", "image017.tiff"),
        ("exif018.phpt", "image018.tiff"),
        ("exif020.phpt", "image020.tiff"),
        ("exif021.phpt", "image021.tiff"),
        ("exif022.phpt", "image022.tiff"),
        ("exif023.phpt", "image023.tiff"),
        ("exif024.phpt", "image024.jpg"),
        ("exif025.phpt", "image025.jpg"),
        ("exif026.phpt", "image026.tiff"),
        ("exif027.phpt", "image027.tiff"),
        ("exif028.phpt", "image028.jpg"),
        ("exif029.phpt", "image029.heic"),
        ("gh19904.phpt", "gh19904.webp"),
        ("exif_read_data_streams.phpt", "image027.tiff"),
    ] {
        let tail = if ["bug77564.phpt", "bug76557.phpt", "bug77831.phpt"].contains(&phpt) {
            "DONE"
        } else {
            ""
        };
        c.push((phpt, Box::new(dump_read(img, tail))));
    }
    // Bare calls: only the warnings are printed.
    for (phpt, imgs, sec, arr, th, tail) in [
        (
            "bug48378.phpt",
            &["bug48378.jpg"][..],
            Some("FILE,COMPUTED,ANY_TAG"),
            false,
            false,
            "",
        ),
        (
            "bug54002.phpt",
            &["bug54002_1.jpg", "bug54002_2.jpg"][..],
            None,
            false,
            false,
            "",
        ),
        (
            "bug76423.phpt",
            &["bug76423.jpg"][..],
            Some("0"),
            true,
            true,
            "",
        ),
        (
            "bug77950.phpt",
            &["bug77950.tiff"][..],
            None,
            false,
            false,
            "DONE",
        ),
        (
            "bug77988.phpt",
            &["bug77988.jpg"][..],
            Some("COMMENT"),
            false,
            true,
            "DONE",
        ),
        (
            "bug78222.phpt",
            &["bug78222.jpg"][..],
            Some("THUMBNAIL"),
            false,
            true,
            "DONE",
        ),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                for i in imgs {
                    read(o, i, sec, arr, th, false);
                }
                o.echo(tail);
            }),
        ));
    }
    c.push((
        "bug76130.phpt",
        Box::new(|o: &mut Out| {
            // error_reporting=E_ALL & ~E_WARNING & ~E_NOTICE
            read(o, "bug76130_1.jpg", None, false, false, true);
            read(o, "bug76130_2.jpg", None, false, false, true);
            o.echo("===DONE===");
        }),
    ));
    for (phpt, img) in [
        ("bug62523_1.phpt", "bug62523_1.jpg"),
        ("bug62523_2.phpt", "bug62523_2.jpg"),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                o.echo("Test\n");
                let v = read(o, img, None, false, false, false);
                o.var_dump(&count(&v));
                o.echo("Done");
            }),
        ));
    }
    c.push((
        "bug62523_3.phpt",
        Box::new(|o: &mut Out| {
            o.echo("Test\n");
            let v = read(o, "bug62523_3.jpg", None, false, false, false);
            o.var_dump(&v);
            o.echo("Done");
        }),
    ));
    for (phpt, img) in [
        ("bug72603.phpt", "bug72603.jpg"),
        ("bug72618.phpt", "bug72618.jpg"),
        ("bug73115.phpt", "bug73115/bug73115.jpg"),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let v = read(o, img, None, false, false, false);
                o.var_dump(&count(&v));
            }),
        ));
    }
    c.push((
        "bug64739.phpt",
        Box::new(|o: &mut Out| {
            o.echo("Test\n");
            let v = read(o, "bug64739.jpg", None, false, false, false);
            for k in ["Title", "Author"] {
                let first = match get(&v, k) {
                    Value::Str(s) => s.as_bytes().first() == Some(&b'?'),
                    _ => false,
                };
                o.var_dump(&Value::Bool(first));
            }
            let mut cfg = ExifConfig::default();
            cfg.set("exif.decode_unicode_motorola", "UCS-2LE", &BuiltinConverter)
                .expect("ini");
            let path = fixtures().join("exif/bug64739.jpg");
            let (v, d) = exif_read_data(
                File::open(&path).expect("open"),
                &file_origin(&path),
                &ReadOptions::default(),
                &cfg,
                &BuiltinConverter,
            );
            o.diags("exif_read_data", &d);
            o.var_dump(&get(&v, "Title"));
            o.var_dump(&get(&v, "Author"));
            o.echo("Done");
        }),
    ));
    for (phpt, img) in [
        ("bug68113.phpt", "bug68113.jpg"),
        ("bug68113-mb.phpt", "bug68113私はガラスを食べられます.jpg"),
        ("bug73737.phpt", "bug73737.tiff"),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let t = thumb(o, img, false);
                o.var_dump(&thumb_value(t.as_ref()));
                if phpt.starts_with("bug68113") {
                    o.echo("Done");
                }
            }),
        ));
    }
    c.push((
        "bug72094.phpt",
        Box::new(|o: &mut Out| {
            for i in 1..=4 {
                let v = read(o, &format!("bug72094_{i}.jpg"), None, false, false, false);
                o.print_r(&v);
            }
            o.echo("DONE");
        }),
    ));
    c.push((
        "bug72627.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "bug72627.tiff", Some("0"), false, true, false);
            o.var_dump(&v);
        }),
    ));
    c.push((
        "bug72682.phpt",
        Box::new(|o: &mut Out| {
            for p in ["nokia.jpg", "samsung.jpg", "panasonic.jpg"] {
                o.echo(format!("{p}: "));
                let v = read(o, &format!("bug72735/{p}"), None, false, false, false);
                o.var_dump(&get(&v, "MakerNote"));
            }
        }),
    ));
    c.push((
        "bug72735.phpt",
        Box::new(|o: &mut Out| {
            for p in ["nokia.jpg", "samsung.jpg", "panasonic.jpg"] {
                o.echo(format!("{p}: "));
                let t = thumb(o, &format!("bug72735/{p}"), false);
                let len = t.map_or(0, |t| t.data.len());
                if len == 0 {
                    o.echo("Error, no length returned\n");
                } else {
                    o.echo(format!("int({len})\n"));
                }
            }
        }),
    ));
    c.push((
        "bug72819.phpt",
        Box::new(|o: &mut Out| {
            let t = thumb(o, "bug72819/bug72819.jpg", false);
            o.var_dump(&Value::Int(t.map_or(0, |t| t.data.len() as i64)));
        }),
    ));
    c.push((
        "bug75785.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "bug75785/P1000506.JPG", Some("EXIF"), true, false, false);
            let gps = get(&v, "GPS");
            echo_value(o, &get_i(&get(&gps, "GPSLatitude"), 0));
            o.echo("\n");
            echo_value(o, &get_i(&get(&gps, "GPSLongitude"), 0));
            o.echo("\n===DONE===");
        }),
    ));
    c.push((
        "bug77184.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "bug77184/DJI_0245_tiny.jpg", None, false, false, false);
            echo_value(o, &get_i(&get(&v, "GPSLatitude"), 2));
            o.echo("\n");
            echo_value(o, &get_i(&get(&v, "GPSLongitude"), 2));
        }),
    ));
    c.push((
        "bug50660.phpt",
        Box::new(|o: &mut Out| {
            for f in ["bug50660/bug50660-1.jpg", "bug50660/bug50660-2.jpg"] {
                let v = read(o, f, None, false, false, false);
                o.var_dump(&Value::Bool(!matches!(v, Value::Bool(false))));
            }
        }),
    ));
    c.push((
        "bug77540.phpt",
        Box::new(|o: &mut Out| {
            let t = thumb(o, "bug77540.jpg", true);
            let (w, h) = t.map_or((42, 42), |t| (t.width, t.height));
            o.echo(format!("Width {w}\nHeight {h}\nDONE"));
        }),
    ));
    c.push((
        "bug77563.phpt",
        Box::new(|o: &mut Out| {
            thumb(o, "bug77563.jpg", false);
            o.echo("DONE");
        }),
    ));
    for (phpt, img, tail) in [
        ("bug77753.phpt", "bug77753.tiff", "DONE"),
        ("float_cast_overflow.phpt", "float_cast_overflow.tiff", ""),
        ("nesting_level_oom.phpt", "nesting_level_oom.tiff", ""),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let v = read(o, img, None, false, false, true);
                o.var_dump(&v);
                o.echo(tail);
            }),
        ));
    }
    c.push((
        "bug71534.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "bug71534.tiff", None, false, false, true);
            o.var_dump(&Value::Bool(matches!(v, Value::Bool(false))));
        }),
    ));
    c.push((
        "exif000.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "test2.jpg", None, false, false, false);
            o.print_r(&v);
        }),
    ));
    c.push((
        "bug68799.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "bug68799.jpg", None, false, false, false);
            o.print_r(&v);
        }),
    ));
    for (phpt, img) in [("exif001.phpt", "test2.jpg"), ("exif003.phpt", "test3.jpg")] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let v = read(o, img, Some(""), true, false, false);
                o.var_dump(&v);
            }),
        ));
    }
    c.push((
        "exif004.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "test4.jpg", Some(""), true, false, false);
            o.var_dump(&get(&v, "WINXP"));
        }),
    ));
    c.push((
        "exif005.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "test5.jpg", Some(""), true, false, false);
            o.var_dump(&get(&v, "IFD0"));
        }),
    ));
    c.push((
        "exif_encoding_crash.phpt",
        Box::new(|o: &mut Out| {
            read(o, "exif_encoding_crash.jpg", None, false, false, false);
            o.echo("*** no core dump ***\n");
        }),
    ));
    c.push((
        "gh11020.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "gh11020.jpg", None, false, false, false);
            o.var_dump(&Value::Bool(v.as_array().is_some()));
            o.var_dump(&get(&v, "Orientation"));
        }),
    ));
    c.push((
        "exif_tagname_basic.phpt",
        Box::new(|o: &mut Out| {
            o.echo("*** Testing exif_tagname() : basic functionality ***\n");
            for t in [0x10E, 0x10F, 0x110] {
                o.var_dump(&exif_tagname(t).map_or(Value::Bool(false), Value::from));
            }
        }),
    ));
    for (phpt, img) in [
        ("exif_imagetype_basic.phpt", "test2.jpg"),
        (
            "exif_imagetype_basic-mb.phpt",
            "test2私はガラスを食べられます.jpg",
        ),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                o.echo("*** Testing exif_imagetype() : basic functionality ***\n");
                let path = fixtures().join("exif").join(img);
                let (t, d) = exif_imagetype(File::open(&path).expect("open"), b"x");
                o.diags("exif_imagetype", &d);
                o.var_dump(&t.map_or(Value::Bool(false), Value::Int));
            }),
        ));
    }
    c.push((
        "exif_thumbnail_streams.phpt",
        Box::new(|o: &mut Out| {
            let t = thumb(o, "sony.jpg", false).expect("thumbnail");
            o.var_dump(&Value::Int(t.data.len() as i64));
            let t = thumb(o, "sony.jpg", true).expect("thumbnail");
            o.var_dump(&Value::Int(t.width));
            o.var_dump(&Value::Int(t.height));
            o.var_dump(&Value::Bool(t.image_type == 2));
        }),
    ));
    c.push((
        "sony.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "sony.jpg", None, false, false, false);
            for t in ["SonyModelID", "Panorama", "AntiBlur"] {
                let found = !get(&v, t).is_null();
                o.echo(format!(
                    "{t} was {}found\n",
                    if found { "" } else { "NOT " }
                ));
            }
        }),
    ));
    c.push((
        "redhat-bug1362571.phpt",
        Box::new(|o: &mut Out| {
            let t = thumb(o, "redhat-bug1362571.jpg", false);
            o.var_dump(&Value::Bool(t.is_some_and(|t| !t.data.is_empty())));
        }),
    ));
    c.push((
        "temporary_buffer_leak.phpt",
        Box::new(|o: &mut Out| {
            let v = read(o, "temporary_buffer_leak.jpg", None, false, false, true);
            o.var_dump(&v);
        }),
    ));
    c.push((
        "tag_with_illegal_zero_components.phpt",
        Box::new(|o: &mut Out| {
            let v = read(
                o,
                "tag_with_illegal_zero_components.jpeg",
                None,
                false,
                false,
                false,
            );
            o.var_dump(&v);
        }),
    ));
    for (phpt, img, sec, th, tail) in [
        (
            "zero_length_makernote_leak.phpt",
            "zero_length_makernote_leak.tiff",
            None,
            false,
            "===DONE===",
        ),
        (
            "duplicate_copyright_tag_leak.phpt",
            "duplicate_copyright_tag_leak.tiff",
            None,
            false,
            "===DONE===",
        ),
        (
            "bug78256.phpt",
            "bug78256.jpg",
            Some("COMMENT"),
            true,
            "DONE",
        ),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                read(o, img, sec, false, th, true);
                o.echo(tail);
            }),
        ));
    }
    c.push((
        "gh16409.phpt",
        Box::new(|o: &mut Out| {
            // A popen() pipe: a plain-file stream whose path cannot be stat()ed.
            let (r, d) = exif_thumbnail(
                Cursor::new(b"1\n".to_vec()),
                &StreamOrigin::UnstatableFile,
                true,
                &ExifConfig::default(),
                &BuiltinConverter,
            );
            o.diags("exif_thumbnail", &d);
            o.var_dump(&thumb_value(r.as_ref()));
        }),
    ));
    for (phpt, img) in [
        ("oss_fuzz_442954659.phpt", "oss_fuzz_442954659/input"),
        ("oss_fuzz_444479893.phpt", "oss_fuzz_444479893/input"),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                read(o, img, None, false, false, false);
            }),
        ));
    }
    c.push((
        "bug78793.phpt",
        Box::new(|o: &mut Out| {
            for _ in 0..10 {
                read(o, "bug77950.tiff", None, false, false, true);
            }
            o.echo("===DONE===");
        }),
    ));
    // data: URLs are non-plain streams (no FileName, FileDateTime 0).
    for (phpt, b64) in [
        (
            "bug78910.phpt",
            "TU0AKgAAAAwgICAgAAIBDwAEAAAAAgAAACKSfCAgAAAAAEZVSklGSUxN",
        ),
        (
            "bug79046.phpt",
            "TU0AKgAAAA7//wAAANUAAQERAAsAAAABAAD4fwAAAA4A",
        ),
        ("bug79282.phpt", "/9jhAAlFeGlmAAAg"),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let v = read_bytes(o, &base64(b64));
                o.var_dump(&v);
            }),
        ));
    }
    c.push((
        "heic_box_overflow.phpt",
        Box::new(|o: &mut Out| {
            let mut d = vec![0, 0, 0, 12];
            d.extend_from_slice(b"ftypheic");
            d.extend_from_slice(&[0, 0, 0, 1]);
            d.extend_from_slice(b"xyzw");
            d.extend_from_slice(&[
                0xff,
                0xff,
                0xff,
                0xff,
                0xff,
                0xff,
                0xff,
                0xff - 11,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]);
            let v = read_bytes(o, &d);
            o.var_dump(&v);
        }),
    ));
    c.push((
        "heic_iloc_underflow.phpt",
        Box::new(|o: &mut Out| {
            let mut d = std::fs::read(fixtures().join("exif/image029.heic")).expect("read");
            d[0x4f8..0x4fc].copy_from_slice(&[0, 0, 0, 1]);
            let v = read_bytes(o, &d);
            o.var_dump(&v);
        }),
    ));
    c.push((
        "heic_meta_box_alloc.phpt",
        Box::new(|o: &mut Out| {
            let mut d = 20u32.to_be_bytes().to_vec();
            d.extend_from_slice(b"ftypheic");
            d.extend_from_slice(&[0; 8]);
            d.extend_from_slice(&0x0800_0000u32.to_be_bytes());
            d.extend_from_slice(b"meta");
            d.extend_from_slice(&[0; 8]);
            d.push(0);
            let v = read_bytes(o, &d);
            o.var_dump(&v);
        }),
    ));
    c
}

fn base64(s: &str) -> Vec<u8> {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let Some(v) = table.iter().position(|&t| t == c) else {
            continue;
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[test]
fn exif_phpt_replays() {
    let mut failures = Vec::new();
    let all = cases();
    for (phpt, script) in &all {
        let mut out = Out(Vec::new(), phpt.trim_end_matches(".phpt").to_owned());
        script(&mut out);
        if let Err(e) = check(phpt, &out.0) {
            failures.push(e);
        }
    }
    for f in &failures {
        eprintln!("=========== FAIL {f}\n");
    }
    assert!(
        failures.is_empty(),
        "{} of {} phpt replays failed",
        failures.len(),
        all.len()
    );
}

#[test]
fn expectf_matcher() {
    let t = tokenize(b"a%db%sc");
    assert!(matches_at(&t, b"a12bxyzc"));
    assert!(!matches_at(&t, b"ab c"));
    assert!(matches_at(&tokenize(b"x%Ay"), b"x\n\nzzy"));
    assert!(matches_at(
        &tokenize(b"in %s%ebug.php on"),
        b"in /a/b/bug.php on"
    ));
}

#[test]
fn exif002_thumbnail_is_test1() {
    let mut o = Out::default();
    let t = thumb(&mut o, "test2.jpg", false).expect("thumbnail");
    let t1 = std::fs::read(fixtures().join("exif/test1.jpg")).expect("read");
    assert_eq!(t.data, t1);
}

// ---------------------------------------------------------------------------
// ext/standard/tests/image

fn image(rel: &str) -> PathBuf {
    fixtures().join("image").join(rel)
}

fn gis(o: &mut Out, rel: &str, info: bool) -> (Value, Value) {
    let p = image(rel);
    let r = getimagesize(
        File::open(&p).expect("open"),
        p.to_string_lossy().as_bytes(),
        info,
    );
    o.diags("getimagesize", &r.diagnostics);
    let info = r
        .app_markers
        .clone()
        .map_or(Value::Null, |h| Value::Array(h.into()));
    (r.to_value(), info)
}

#[test]
fn getimagesize_phpt_replays() {
    let mut failures = Vec::new();
    // getimagesize.phpt: every test*pix.* except test13pix.swf, sorted.
    let mut files: Vec<String> = std::fs::read_dir(fixtures().join("image"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("test") && f.contains("pix.") && f != "test13pix.swf")
        .collect();
    files.sort();
    let mut o = Out::default();
    let mut arr = php_runtime::HashTable::new();
    for f in &files {
        let (v, _) = gis(&mut o, f, false);
        arr.insert(php_runtime::ArrayKey::from_bytes(f.as_bytes()), v);
    }
    o.var_dump(&Value::Array(arr.into()));
    if let Err(e) = check("image_getimagesize.phpt", &o.0) {
        failures.push(e);
    }
    for (phpt, script) in image_cases() {
        let mut o = Out(
            Vec::new(),
            phpt.trim_start_matches("image_")
                .trim_end_matches(".phpt")
                .to_owned(),
        );
        script(&mut o);
        if let Err(e) = check(phpt, &o.0) {
            failures.push(e);
        }
    }
    for f in &failures {
        eprintln!("=========== FAIL {f}\n");
    }
    assert!(
        failures.is_empty(),
        "{} getimagesize replays failed",
        failures.len()
    );
}

#[test]
fn getimagesizefromstring_matches_getimagesize() {
    for f in [
        "test1pix.jpg",
        "test4pix.png",
        "200x100.gif",
        "75x50.xbm",
        "75x50.wbmp",
        "test4pix.tiff",
    ] {
        let data = std::fs::read(image(f)).expect("read");
        let a = getimagesizefromstring(&data, false).to_value();
        let mut o = Out::default();
        let (b, _) = gis(&mut o, f, false);
        let mut x = Vec::new();
        let mut y = Vec::new();
        var_dump(&mut x, &a, &mut CollectingContext::new()).expect("dump");
        var_dump(&mut y, &b, &mut CollectingContext::new()).expect("dump");
        assert_eq!(x, y, "{f}");
    }
}

fn dump_gis(o: &mut Out, rel: &str, with_info: bool) {
    let (v, info) = gis(o, rel, with_info);
    o.var_dump(&v);
    if with_info {
        o.var_dump(&info);
    }
}

fn image_cases() -> Vec<(&'static str, Script)> {
    let mut c: Vec<(&'static str, Script)> = Vec::new();
    c.push((
        "image_gh20201.phpt",
        Box::new(|o: &mut Out| dump_gis(o, "gh20201.avif", false)),
    ));
    c.push((
        "image_bug13213.phpt",
        Box::new(|o: &mut Out| dump_gis(o, "bug13213.jpg", false)),
    ));
    c.push((
        "image_bug72278.phpt",
        Box::new(|o: &mut Out| dump_gis(o, "bug72278.jpg", false)),
    ));
    c.push((
        "image_bug70052.phpt",
        Box::new(|o: &mut Out| {
            dump_gis(o, "bug70052_1.wbmp", false);
            dump_gis(o, "bug70052_2.wbmp", false);
        }),
    ));
    c.push((
        "image_bug71848.phpt",
        Box::new(|o: &mut Out| {
            let (v, info) = gis(o, "bug71848.jpg", true);
            o.var_dump(&v);
            let keys: Vec<Value> = info
                .as_array()
                .map(|a| a.keys().map(php_runtime::ArrayKey::to_value).collect())
                .unwrap_or_default();
            o.var_dump(&Value::Array(keys.into_iter().collect()));
        }),
    ));
    c.push((
        "image_getimagesize_256_ico.phpt",
        Box::new(|o: &mut Out| {
            o.echo("*** Testing getimagesize() : 256px ico ***\n");
            dump_gis(o, "32x256.ico", false);
            o.echo("===DONE===");
        }),
    ));
    for (phpt, prefix) in [
        ("image_getimagesize_246x247.phpt", "246x247."),
        ("image_getimagesize_384x385.phpt", "384x385."),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                let mut files: Vec<String> = std::fs::read_dir(fixtures().join("image"))
                    .expect("dir")
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|f| f.starts_with(prefix))
                    .collect();
                files.sort();
                let mut arr = php_runtime::HashTable::new();
                for f in &files {
                    let (v, _) = gis(o, f, false);
                    arr.insert(php_runtime::ArrayKey::from_bytes(f.as_bytes()), v);
                }
                o.var_dump(&Value::Array(arr.into()));
            }),
        ));
    }
    c.push((
        "image_getimagesize_basic.phpt",
        Box::new(|o: &mut Out| {
            o.echo("*** Testing getimagesize() : basic functionality ***\n");
            for (k, f) in [
                ("GIF image file", "200x100.gif"),
                ("JPEG image file", "200x100.jpg"),
                ("PNG image file", "200x100.png"),
                ("SWF image file", "200x100.swf"),
                ("BMP image file", "200x100.bmp"),
                ("TIFF intel byte order image file", "200x100.tiff"),
                ("JPC image file", "test1pix.jpc"),
                ("JP2 image file", "test1pix.jp2"),
                ("IFF image file", "test4pix.iff"),
            ] {
                o.echo(format!("\n-- {k} ({f}) --\n"));
                dump_gis(o, f, true);
            }
        }),
    ));
    c.push((
        "image_getimagesizefromstring1.phpt",
        Box::new(|o: &mut Out| {
            let (a, _) = gis(o, "test.gif", false);
            let data = std::fs::read(image("test.gif")).expect("read");
            let b = getimagesizefromstring(&data, false).to_value();
            o.var_dump(&a);
            o.var_dump(&b);
        }),
    ));
    c.push((
        "image_getimagesizefromstring_iff_overflow.phpt",
        Box::new(|o: &mut Out| {
            let payload = b"FORM\x00\x00\x00\x00ILBMABCD\x7f\xff\xff\xff";
            let r = getimagesizefromstring(payload, false);
            o.diags("getimagesizefromstring", &r.diagnostics);
            o.var_dump(&r.to_value());
            let r = getimagesize(Cursor::new(&payload[..]), b"x", false);
            o.diags("getimagesize", &r.diagnostics);
            o.var_dump(&r.to_value());
        }),
    ));
    c.push((
        "image_getimagesize_swc.phpt",
        Box::new(|o: &mut Out| dump_gis(o, "test13pix.swf", false)),
    ));
    for (phpt, head, f) in [
        (
            "image_getimagesize_tif_mm.phpt",
            "*** Testing getimagesize() : tiff_mm format ***\n",
            "2x2mm.tiff",
        ),
        (
            "image_getimagesize_variation_005.phpt",
            "*** Testing getimagesize() : basic functionality ***\n",
            "test13pix.swf",
        ),
        (
            "image_getimagesize_variation4.phpt",
            "*** Testing getimagesize() : variation ***\n",
            "test13pix.swf",
        ),
        (
            "image_getimagesize_wbmp.phpt",
            "*** Testing getimagesize() : wbmp format ***\n",
            "75x50.wbmp",
        ),
        (
            "image_getimagesize_xbm.phpt",
            "*** Testing getimagesize() : xbm format ***\n",
            "75x50.xbm",
        ),
    ] {
        c.push((
            phpt,
            Box::new(move |o: &mut Out| {
                o.echo(head);
                dump_gis(o, f, true);
            }),
        ));
    }
    c.push((
        "image_getimagesize_variation3.phpt",
        Box::new(|o: &mut Out| {
            o.echo("*** Testing getimagesize() : variation ***\n");
            for (k, f) in [
                ("File with text data", "test.txt"),
                ("File with corrupted BMP data", "200x100_unknown.unknown"),
                ("Non-existent file", "nofile.ext"),
                ("Empty File", "blank_file.bmp"),
            ] {
                o.echo(format!("\n-- {k} ({f}) --\n"));
                for with_info in [false, true] {
                    if image(f).exists() {
                        let (v, _) = gis(o, f, with_info);
                        o.var_dump(&v);
                    } else {
                        // The stream layer's warning (not this crate's job).
                        o.echo("\nWarning: getimagesize(): Failed to open stream: No such file or directory in x on line 2\n");
                        o.var_dump(&Value::Bool(false));
                    }
                }
                o.var_dump(&Value::empty_array());
            }
        }),
    ));
    c.push((
        "image_gh20584.phpt",
        Box::new(|o: &mut Out| {
            let mut payload = vec![b'A'; 8192];
            payload.extend(std::iter::repeat_n(b'B', 8192));
            payload.extend(std::iter::repeat_n(b'Z', 123));
            let app1_len = u16::try_from(2 + payload.len()).expect("len");
            let mut jpeg = b"\xFF\xD8\xFF\xE1".to_vec();
            jpeg.extend_from_slice(&app1_len.to_be_bytes());
            jpeg.extend_from_slice(&payload);
            jpeg.extend_from_slice(b"\xFF\xC0\x00\x0b\x08\x00\x01\x00\x01\x01\x11\x00\xFF\xD9");
            let r = getimagesize(Cursor::new(jpeg), b"x", true);
            let ok = r
                .app_markers
                .as_ref()
                .and_then(|h| h.get_str(b"APP1"))
                .and_then(Value::as_str)
                .is_some_and(|s| s.as_bytes() == payload.as_slice());
            o.var_dump(&Value::Bool(ok));
        }),
    ));
    c.push((
        "image_image_type_to_mime_type_variation3.phpt",
        Box::new(|o: &mut Out| {
            o.echo("*** Testing image_type_to_mime_type() : usage variations ***\n");
            for t in (0..=20).chain([999]) {
                o.echo(format!("\n-- Iteration {t} --\n"));
                o.var_dump(&Value::from(php_exif::imagetype::image_type_to_mime_type(
                    t,
                )));
            }
        }),
    ));
    c.push((
        "image_image_type_to_mime_type_variation4.phpt",
        Box::new(|o: &mut Out| {
            use php_exif::imagetype as it;
            o.echo("*** Testing image_type_to_mime_type() : usage variations ***\n");
            for t in [it::IMAGETYPE_ICO, it::IMAGETYPE_SWC] {
                o.var_dump(&Value::from(it::image_type_to_mime_type(t)));
            }
        }),
    ));
    c
}

// ---------------------------------------------------------------------------
// memory_limit (oom_*.phpt), on the accounting allocator.

const MIB: usize = 1024 * 1024;

/// Request startup with `memory_limit` = `mb` MiB: the input built before
/// is not counted, as the file on disk in the PHP test.
fn limit(mb: usize) {
    php_memlimit::request_start(Some(mb * MIB)).expect("limit");
}

/// What the VM does as the call returns: the pending violation becomes the
/// fatal error, and the request ends.
fn deliver(o: &mut Out) {
    if let Some(over) = php_memlimit::take() {
        let file = format!(" in /t/{}.php on line 2\n", o.1);
        o.echo(format!("\nFatal error: {over}{file}"));
    }
    php_memlimit::request_end();
}

fn read_limited(o: &mut Out, data: &[u8], arrays: bool, mb: usize) -> Value {
    let opts = ReadOptions {
        as_arrays: arrays,
        ..ReadOptions::default()
    };
    limit(mb);
    let (v, d) = exif_read_data(
        Cursor::new(data),
        &StreamOrigin::Other,
        &opts,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    o.diags("exif_read_data", &d);
    deliver(o);
    v
}

fn big_thumbnail_tiff(size: u32) -> Vec<u8> {
    let ifd1_off: u32 = 8 + 2 + 2 * 12 + 4;
    let payload_off = ifd1_off + 2 + 2 * 12 + 4;
    let mut t = b"II*\x00".to_vec();
    t.extend_from_slice(&8u32.to_le_bytes());
    let entry = |t: &mut Vec<u8>, tag: u16, fmt: u16, n: u32, v: u32| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&fmt.to_le_bytes());
        t.extend_from_slice(&n.to_le_bytes());
        t.extend_from_slice(&v.to_le_bytes());
    };
    t.extend_from_slice(&2u16.to_le_bytes());
    entry(&mut t, 0x0100, 4, 1, 1);
    entry(&mut t, 0x0101, 4, 1, 1);
    t.extend_from_slice(&ifd1_off.to_le_bytes());
    t.extend_from_slice(&2u16.to_le_bytes());
    entry(&mut t, 0x0111, 4, 1, payload_off);
    entry(&mut t, 0x0117, 4, 1, size);
    t.extend_from_slice(&0u32.to_le_bytes());
    t.resize(t.len() + size as usize, 0xAB);
    t
}

fn fatal_text(o: &Out) -> bool {
    o.0.windows(14).any(|w| w == b"Fatal error: A")
}

#[test]
fn oom_phpt_replays() {
    // A thread of its own, with its own counters and limit (the other
    // tests of this binary run unlimited).
    std::thread::spawn(|| {
        php_memlimit::attach_thread();
        oom_cases();
    })
    .join()
    .expect("oom cases");
}

fn oom_cases() {
    let mut failures = Vec::new();
    let mut run = |phpt: &'static str, f: &dyn Fn(&mut Out)| {
        let mut o = Out(Vec::new(), phpt.trim_end_matches(".phpt").to_owned());
        f(&mut o);
        if let Err(e) = check(phpt, &o.0) {
            failures.push(e);
        }
        assert!(fatal_text(&o) || phpt.contains("tiff"), "{phpt}");
    };
    run("oom_exif_read_data_tags.phpt", &|o| {
        let n: u32 = 1000;
        let payload_len: u32 = 256 * 1024;
        let payload_off: u32 = 8 + 2 + n * 12 + 4;
        let mut t = b"II*\x00".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&u16::try_from(n).expect("n").to_le_bytes());
        for i in 0..n {
            t.extend_from_slice(&(0x9000 + u16::try_from(i & 0xFF).expect("tag")).to_le_bytes());
            t.extend_from_slice(&1u16.to_le_bytes());
            t.extend_from_slice(&payload_len.to_le_bytes());
            t.extend_from_slice(&payload_off.to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes());
        t.resize(t.len() + payload_len as usize, b'A');
        let v = read_limited(o, &t, true, 8);
        if !fatal_text(o) {
            o.var_dump(&v);
        }
    });
    // Buzz keeps no JPEG sections (DIFFERENCES.md): the file is read
    // where php-src raises the memory fatal.
    {
        let n = (2 * 1024 * 1024 - 4) / 4;
        let mut j = b"\xFF\xD8".to_vec();
        for _ in 0..n {
            j.extend_from_slice(b"\xFF\xE9\x00\x02");
        }
        j.extend_from_slice(b"\xFF\xD9");
        let mut o = Out(Vec::new(), "oom_exif_scan_jpeg_header".to_owned());
        let v = read_limited(&mut o, &j, false, 8);
        assert!(!fatal_text(&o), "{}", String::from_utf8_lossy(&o.0));
        assert!(matches!(v, Value::Array(_) | Value::Bool(_)), "{v:?}");
    }
    for (phpt, size) in [
        ("oom_exif_thumbnail_large_file_001.phpt", 12 * 1024 * 1024),
        ("oom_exif_thumbnail_large_file_002.phpt", 20 * 1024 * 1024),
    ] {
        run(phpt, &move |o| {
            let t = big_thumbnail_tiff(size);
            limit(8);
            let (r, d) = exif_thumbnail(
                Cursor::new(t),
                &StreamOrigin::Other,
                false,
                &ExifConfig::default(),
                &BuiltinConverter,
            );
            o.diags("exif_thumbnail", &d);
            deliver(o);
            if !fatal_text(o) {
                o.var_dump(&thumb_value(r.as_ref()));
            }
        });
    }
    run("oom_exif_thumbnail_tiff.phpt", &|o| {
        let mut t = big_thumbnail_tiff(0);
        // StripOffsets = 1, StripByteCounts = 0xFFFFFFFF
        let off = 8 + 2 + 2 * 12 + 4 + 2;
        t[off + 8..off + 12].copy_from_slice(&1u32.to_le_bytes());
        t[off + 12 + 8..off + 12 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        limit(128);
        let (r, d) = exif_thumbnail(
            Cursor::new(t),
            &StreamOrigin::Other,
            false,
            &ExifConfig::default(),
            &BuiltinConverter,
        );
        o.diags("exif_thumbnail", &d);
        deliver(o);
        o.var_dump(&thumb_value(r.as_ref()));
    });
    run("oom_exif_numeric_metadata.phpt", &|o| {
        let base = std::fs::read(fixtures().join("exif/image011.jpg")).expect("read");
        let entry_count: u32 = 900;
        let component_count: u32 = 12000;
        let data_offset = 8 + 2 + entry_count * 12 + 4;
        let mut tiff = b"II*\x00".to_vec();
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&u16::try_from(entry_count).expect("n").to_le_bytes());
        for i in 0..entry_count {
            tiff.extend_from_slice(&(0xF000 + u16::try_from(i).expect("i")).to_le_bytes());
            tiff.extend_from_slice(&3u16.to_le_bytes());
            tiff.extend_from_slice(&component_count.to_le_bytes());
            tiff.extend_from_slice(&data_offset.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.resize(tiff.len() + component_count as usize * 2, 0);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut jpeg = b"\xff\xd8\xff\xe1".to_vec();
        jpeg.extend_from_slice(&u16::try_from(app1.len() + 2).expect("len").to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&base[2..]);
        read_limited(o, &jpeg, false, 128);
    });
    run("image_oom_getimagesize_xbm.phpt", &|o| {
        let data = vec![b'x'; 12 * 1024 * 1024];
        limit(4);
        let r = getimagesize(Cursor::new(data), b"x", false);
        o.diags("getimagesize", &r.diagnostics);
        deliver(o);
        if !fatal_text(o) {
            o.var_dump(&r.to_value());
        }
    });
    for f in &failures {
        eprintln!("=========== FAIL {f}\n");
    }
    assert!(failures.is_empty(), "{} oom replays failed", failures.len());
}
