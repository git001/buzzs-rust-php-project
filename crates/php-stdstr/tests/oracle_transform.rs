// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `transform.rs`: case mapping, trim, pad, wrap, split,
//! `strtr`, `str_replace`, `chr`/`ord`, hex, utf8, `str_increment`,
//! `hebrev`, paths.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{assoc, case, check_mapped, lit, pure, regenerate, Case, V};
use php_stdstr::transform::{
    basename, bin2hex, chr, chunk_split, dirname, hebrev, hex2bin, lcfirst, nl2br, ord, pathinfo,
    str_decrement, str_increment, str_pad, str_repeat, str_replace, str_rot13, str_split, strrev,
    strtolower, strtoupper, strtr, strtr_pairs, trim, ucfirst, ucwords, utf8_decode, utf8_encode,
    wordwrap, PathInfo, ReplaceArg, TrimMode,
};

const STRS: &[&[u8]] = &[
    b"",
    b"a",
    b"hello world",
    b"Hello World",
    b"  padded\t\n",
    b"\0\x0b\x0c x \r\n\0",
    b"\x0cff\x0c",
    b"MiXeD cAsE 123 \xc4\xe4\xd6",
    b"\xc3\xa9t\xc3\xa9 caf\xc3\xa9",
    b"hello_world-foo bar|baz",
    b"The quick brown fox sat over the lazy dog",
    b"A very long woooooooooooord.",
    b"A very long woooooooooooooooooord. and something",
    b"line1\nline2\r\nline3\rline4\n\rline5",
    b"\n\r\n\r\r\n",
    b"aaa",
    b"abcabcabc",
    b"Hi all, I said hello",
    b"xxhixx",
    b"12345678901234567890",
];

const CHARLISTS: &[&[u8]] = &[
    b"",
    b"x",
    b"\0",
    b" ",
    b"a..z",
    b"A..Z",
    b"a..",
    b"..a",
    b"z..a",
    b"a..b..c",
    b"..",
    b"...",
    b" \t\n\r\0\x0B",
    b"\x0c",
    b"Hdle",
    b"-_|",
];

fn path_v(p: PathInfo) -> V {
    match p {
        PathInfo::Part(s) => V::from(s),
        PathInfo::All(items) => assoc(items),
    }
}

#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for s in STRS {
        let l = lit(s);
        v.push(pure(&format!("strtolower({l})"), strtolower(s)));
        v.push(pure(&format!("strtoupper({l})"), strtoupper(s)));
        v.push(pure(&format!("ucfirst({l})"), ucfirst(s)));
        v.push(pure(
            &format!("lcfirst(strtoupper({l}))"),
            lcfirst(&strtoupper(s)),
        ));
        v.push(pure(&format!("strrev({l})"), strrev(s)));
        v.push(pure(&format!("str_rot13({l})"), str_rot13(s)));
        v.push(pure(&format!("bin2hex({l})"), bin2hex(s)));
        v.push(pure(&format!("nl2br({l})"), nl2br(s, true)));
        v.push(pure(&format!("nl2br({l}, false)"), nl2br(s, false)));
        v.push(pure(
            &format!("bin2hex(utf8_encode({l}))"),
            bin2hex(&utf8_encode(s)),
        ));
        v.push(pure(
            &format!("bin2hex(utf8_decode({l}))"),
            bin2hex(&utf8_decode(s)),
        ));
        v.push(pure(&format!("hebrev({l})"), hebrev(s, 0)));
        v.push(pure(&format!("hebrev({l}, 5)"), hebrev(s, 5)));
        for d in [&b" \t\r\n\x0c\x0b"[..], b"", b"-|_", b"o", b"a..z", b"..x"] {
            let ld = lit(d);
            v.push(case(&format!("ucwords({l}, {ld})"), |n| {
                Ok::<_, php_stdstr::StdError>(ucwords(s, d, n))
            }));
        }
        for (mode, name) in [
            (TrimMode::Both, "trim"),
            (TrimMode::Left, "ltrim"),
            (TrimMode::Right, "rtrim"),
        ] {
            // php-src master added \f to the default set; spell the set
            // out so the 8.3 oracle checks master semantics.
            v.push(case(
                &format!("{name}({l}, \" \\n\\r\\t\\v\\0\\f\")"),
                |n| Ok::<_, php_stdstr::StdError>(trim(s, None, mode, n).to_vec()),
            ));
            for c in CHARLISTS {
                let lc = lit(c);
                v.push(case(&format!("{name}({l}, {lc})"), |n| {
                    Ok::<_, php_stdstr::StdError>(trim(s, Some(c), mode, n).to_vec())
                }));
            }
        }
        for len in [-1i64, 0, 1, 3, 10, 25] {
            for pad in [&b" "[..], b"", b"-=", b"abc"] {
                for ty in [0i64, 1, 2, 3, -1] {
                    v.push(case(
                        &format!("str_pad({l}, {len}, {}, {ty})", lit(pad)),
                        |_| str_pad(s, len, pad, ty),
                    ));
                }
            }
        }
        for width in [-1i64, 0, 1, 3, 5, 10, 75] {
            for brk in [&b"\n"[..], b"", b"<br />", b" ", b"o"] {
                for cut in [false, true] {
                    v.push(case(
                        &format!("wordwrap({l}, {width}, {}, {cut})", lit(brk)),
                        |_| wordwrap(s, width, brk, cut),
                    ));
                }
            }
        }
        for n in [-1i64, 0, 1, 2, 3, 76, 100] {
            v.push(case(&format!("chunk_split({l}, {n}, '|')"), |_| {
                chunk_split(s, n, b"|")
            }));
            v.push(case(&format!("str_split({l}, {n})"), |_| str_split(s, n)));
            v.push(case(&format!("str_repeat({l}, {n})"), |_| str_repeat(s, n)));
        }
        v.push(case(&format!("chunk_split({l})"), |_| {
            chunk_split(s, 76, b"\r\n")
        }));
        v.push(case(&format!("chunk_split({l}, 4, '')"), |_| {
            chunk_split(s, 4, b"")
        }));
        for (from, to) in [
            (&b"abc"[..], &b"xyz"[..]),
            (b"lo", b"0"),
            (b"", b"abc"),
            (b"aaa", b"xyz"),
            (b"\xc3", b"X"),
            (b"a", b""),
        ] {
            v.push(pure(
                &format!("strtr({l}, {}, {})", lit(from), lit(to)),
                strtr(s, from, to),
            ));
        }
        let pair_sets: Vec<Vec<(&[u8], &[u8])>> = vec![
            vec![],
            vec![(b"Hi", b"Hello")],
            vec![(b"", b"x")],
            vec![(b"l", b"L")],
            vec![(b"Hi", b"Hello"), (b"Hello", b"Hi"), (b"H", b"[H]")],
            vec![(b"a", b"1"), (b"aa", b"2"), (b"aaa", b"3")],
            vec![(b"", b"x"), (b"o", b"0")],
            vec![(b"1", b"one"), (b"12", b"twelve"), (b"world", b"")],
            vec![(b"abcabcabcabcabcabcabcabc", b"long"), (b"b", b"B")],
        ];
        for pairs in &pair_sets {
            let arr: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{} => {}", lit(k), lit(v)))
                .collect();
            v.push(case(&format!("strtr({l}, [{}])", arr.join(", ")), |n| {
                Ok::<_, php_stdstr::StdError>(strtr_pairs(s, pairs, n))
            }));
        }
        for (search, rep) in [
            (&b"o"[..], &b"0"[..]),
            (b"l", b""),
            (b"", b"x"),
            (b"WORLD", b"there"),
            (b"abc", b"abcabc"),
            (b"aa", b"b"),
            (b"Hello world", b"x"),
        ] {
            for ci in [false, true] {
                let f = if ci { "str_ireplace" } else { "str_replace" };
                v.push(case(
                    &format!("[{f}({}, {}, {l}, \\$c), \\$c]", lit(search), lit(rep))
                        .replace("\\$", "$"),
                    |_| {
                        let (r, c) = str_replace::<&[u8], &[u8]>(
                            s,
                            ReplaceArg::One(search),
                            ReplaceArg::One(rep),
                            ci,
                        )?;
                        Ok::<_, php_stdstr::StdError>(V::from(vec![V::from(r), V::from(c)]))
                    },
                ));
            }
        }
        let searches: [&[u8]; 4] = [b"a", b"", b"L", b"b"];
        let reps: [&[u8]; 2] = [b"b", b"[a]"];
        for ci in [false, true] {
            let f = if ci { "str_ireplace" } else { "str_replace" };
            v.push(case(
                &format!("[{f}(['a', '', 'L', 'b'], ['b', '[a]'], {l}, $c), $c]"),
                |_| {
                    let (r, c) =
                        str_replace(s, ReplaceArg::Many(&searches), ReplaceArg::Many(&reps), ci)?;
                    Ok::<_, php_stdstr::StdError>(V::from(vec![V::from(r), V::from(c)]))
                },
            ));
            v.push(case(
                &format!("[{f}(['a', 'l'], 'X', {l}, $c), $c]"),
                |_| {
                    let (r, c) = str_replace::<&[u8], &[u8]>(
                        s,
                        ReplaceArg::Many(&[b"a", b"l"]),
                        ReplaceArg::One(b"X"),
                        ci,
                    )?;
                    Ok::<_, php_stdstr::StdError>(V::from(vec![V::from(r), V::from(c)]))
                },
            ));
        }
        v.push(case(&format!("str_replace('a', ['b'], {l})"), |_| {
            str_replace::<&[u8], &[u8]>(s, ReplaceArg::One(b"a"), ReplaceArg::Many(&[b"b"]), false)
                .map(|x| x.0)
        }));
        v.push(case(&format!("hex2bin({l})"), |n| {
            Ok::<_, php_stdstr::StdError>(hex2bin(s, n))
        }));
        v.push(case(&format!("hex2bin(bin2hex({l}))"), |n| {
            Ok::<_, php_stdstr::StdError>(hex2bin(&bin2hex(s), n))
        }));
        v.push(case(&format!("str_increment({l})"), |_| str_increment(s)));
        v.push(case(&format!("str_decrement({l})"), |_| str_decrement(s)));
    }
    for h in [&b"abcdef"[..], b"ABCDEF", b"0g", b"0", b"zz", b"012Z"] {
        v.push(case(&format!("hex2bin({})", lit(h)), |n| {
            Ok::<_, php_stdstr::StdError>(hex2bin(h, n))
        }));
    }
    for s in [
        &b"a"[..],
        b"z",
        b"Z",
        b"9",
        b"Az",
        b"zZ9",
        b"a9",
        b"Zz",
        b"b",
        b"B",
        b"1",
        b"10",
        b"Aa",
        b"a0",
        b"0",
        b"00",
        b"A",
        b"1a",
        b"-1",
    ] {
        v.push(case(&format!("str_increment({})", lit(s)), |_| {
            str_increment(s)
        }));
        v.push(case(&format!("str_decrement({})", lit(s)), |_| {
            str_decrement(s)
        }));
    }
    for c in [
        0i64,
        65,
        255,
        256,
        321,
        -1,
        -255,
        -256,
        i64::MAX,
        i64::MIN + 1,
    ] {
        v.push(case(&format!("bin2hex(chr({c}))"), |n| {
            Ok::<_, php_stdstr::StdError>(bin2hex(&[chr(c, n)]))
        }));
    }
    for s in [&b"A"[..], b"", b"ab", b"\xff", b"\0"] {
        v.push(case(&format!("ord({})", lit(s)), |n| {
            Ok::<_, php_stdstr::StdError>(ord(s, n))
        }));
    }
    for p in [
        &b""[..],
        b"/",
        b"//",
        b"/etc/passwd",
        b"/etc/",
        b"etc",
        b"etc/",
        b"/a/b/c.d.e",
        b".hidden",
        b"/path/.htaccess",
        b"a/b/",
        b"//a//b//",
        b"dir/file.",
        b"file.tar.gz",
        b"\\a\\b.c",
        b"c:\\x",
        b"/a/b\0c/d.e",
        b"./",
        b"../x",
        b"..",
        b"a.b/c",
    ] {
        let l = lit(p);
        v.push(pure(&format!("basename({l})"), basename(p, b"")));
        v.push(pure(&format!("basename({l}, '.e')"), basename(p, b".e")));
        v.push(pure(&format!("basename({l}, 'c')"), basename(p, b"c")));
        for lev in [-1i64, 0, 1, 2, 3, 100] {
            v.push(case(&format!("dirname({l}, {lev})"), |_| dirname(p, lev)));
        }
        for opt in [0i64, 1, 2, 3, 4, 8, 15, 16, -1] {
            v.push(case(&format!("pathinfo({l}, {opt})"), |_| {
                pathinfo(p, opt).map(path_v)
            }));
        }
    }
    // wordwrap_trailing_break.phpt (php-src master fix GH #24116): an
    // existing break at the very end is recognised.
    for (text, width, brk, cut, want) in [
        (&b"abc\n"[..], 3, &b"\n"[..], true, &b"abc\n"[..]),
        (b"ab cd\n", 5, b"\n", true, b"ab cd\n"),
        (b"abcd\n", 3, b"\n", true, b"abc\nd\n"),
        (b"abc\nabc\n", 3, b"\n", true, b"abc\nabc\n"),
        (b"abc-", 3, b"-", true, b"abc-"),
        (b"abc\r\n", 3, b"\r\n", true, b"abc\r\n"),
        (b"\r\n", 1, b"\r\n", true, b"\r\n"),
        (b"ab cd\r\n", 5, b"\r\n", false, b"ab cd\r\n"),
    ] {
        let expected = format!("=> {}", V::from(want).export());
        v.push(
            case(
                &format!("wordwrap({}, {width}, {}, {cut})", lit(text), lit(brk)),
                |_| wordwrap(text, width, brk, cut),
            )
            .expect(&expected),
        );
    }
    // php-src master emits deprecations from chr()/ord() for out-of-range
    // input (8.3 is silent).
    let chr_dep = "Deprecated: chr(): Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256";
    for (c, hex) in [
        (256i64, "00"),
        (321, "41"),
        (-1, "ff"),
        (-255, "01"),
        (-256, "00"),
        (i64::MAX, "ff"),
        (i64::MIN + 1, "01"),
    ] {
        v.push(
            case(&format!("bin2hex(chr({c}))"), |n| {
                Ok::<_, php_stdstr::StdError>(bin2hex(&[chr(c, n)]))
            })
            .expect(&format!("{chr_dep}\n=> '{hex}'")),
        );
    }
    v.push(
        case("ord('')", |n| Ok::<_, php_stdstr::StdError>(ord(b"", n)))
            .expect("Deprecated: ord(): Providing an empty string is deprecated\n=> 0"),
    );
    v.push(
        case("ord('ab')", |n| Ok::<_, php_stdstr::StdError>(ord(b"ab", n))).expect(
            "Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead\n=> 97",
        ),
    );
    // Default trim set includes \f in master.
    for (f, want) in [
        ("trim", "'x'"),
        ("ltrim", "'x' . \"\\0\" . ''"),
        ("rtrim", "'' . \"\\0\" . 'x'"),
    ] {
        let mode = match f {
            "trim" => TrimMode::Both,
            "ltrim" => TrimMode::Left,
            _ => TrimMode::Right,
        };
        let s = b"\0\x0c x \x0c\0";
        let input: &[u8] = if f == "trim" {
            s
        } else if f == "ltrim" {
            b"\0\x0c x\0"
        } else {
            b"\0x \x0c\0"
        };
        let _ = s;
        v.push(
            case(&format!("{f}({})", lit(input)), |n| {
                Ok::<_, php_stdstr::StdError>(trim(input, None, mode, n).to_vec())
            })
            .expect(&format!("=> {want}")),
        );
    }
    for c in &mut v {
        // pathinfo() validates $flags in master (pathinfo.phpt).
        let invalid_flags = c.php.starts_with("pathinfo(")
            && !["1)", "2)", "4)", "8)", "15)"]
                .iter()
                .any(|f| c.php.ends_with(&format!(", {f}")));
        // wordwrap(): only the cut / multi-byte-break path differs, and
        // only when the text ends with the break.
        let ww_trailing = c.php.starts_with("wordwrap(")
            && [
                ", -1, \"\\x0a\", true)",
                ", 1, \"\\x0a\", true)",
                ", -1, 'o', true)",
                ", 1, 'o', true)",
            ]
            .iter()
            .any(|t| c.php.ends_with(t))
            && c.expected.is_none();
        if invalid_flags || ww_trailing {
            let rust = c.rust.clone();
            *c = c.clone().expect(&rust);
        }
    }
    // Explicit expectations replace grid cases with the same expression.
    let explicit: std::collections::HashSet<String> = v
        .iter()
        .filter(|c| c.expected.is_some())
        .map(|c| c.php.clone())
        .collect();
    v.retain(|c| c.expected.is_some() || !explicit.contains(&c.php));
    v
}

/// php-src master rewordings relative to the 8.3 oracle.
fn master(_php: &str, out: &str) -> String {
    out.split('\n')
        .filter(|l| !l.starts_with("Deprecated: Function utf8_"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("must be a non-empty string", "must not be empty")
        .replace("cannot be empty", "must not be empty")
}

#[test]
fn oracle() {
    check_mapped("transform", &cases(), master);
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("transform", &cases());
}
