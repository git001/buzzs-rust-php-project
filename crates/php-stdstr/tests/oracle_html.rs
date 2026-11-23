// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `php_stdstr::html` against `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use common::{assoc, case, check, lit, regenerate, Case, K, V};
use php_stdstr::html::{
    get_html_translation_table, html_entity_decode, htmlentities, htmlspecialchars,
    htmlspecialchars_decode, ENT_COMPAT, ENT_DISALLOWED, ENT_HTML401, ENT_HTML5, ENT_IGNORE,
    ENT_NOQUOTES, ENT_QUOTES, ENT_SUBSTITUTE, ENT_XHTML, ENT_XML1,
};

const ESCAPE_FLAGS: &[i64] = &[
    ENT_QUOTES,
    ENT_COMPAT,
    ENT_NOQUOTES,
    ENT_QUOTES | ENT_SUBSTITUTE,
    ENT_QUOTES | ENT_IGNORE,
    ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML5,
    ENT_QUOTES | ENT_SUBSTITUTE | ENT_XML1,
    ENT_QUOTES | ENT_SUBSTITUTE | ENT_XHTML,
    ENT_QUOTES | ENT_DISALLOWED | ENT_HTML401,
    ENT_QUOTES | ENT_DISALLOWED | ENT_SUBSTITUTE | ENT_HTML5,
    ENT_QUOTES | ENT_DISALLOWED | ENT_XML1,
    ENT_COMPAT | ENT_DISALLOWED | ENT_XHTML,
];

const DECODE_FLAGS: &[i64] = &[
    ENT_QUOTES,
    ENT_COMPAT,
    ENT_NOQUOTES,
    ENT_QUOTES | ENT_HTML5,
    ENT_QUOTES | ENT_XML1,
    ENT_QUOTES | ENT_XHTML,
    ENT_COMPAT | ENT_HTML5,
    ENT_NOQUOTES | ENT_XHTML,
];

const UTF8_INPUTS: &[&[u8]] = &[
    b"",
    b"plain text",
    b"<a href='x' title=\"y\">&</a>",
    b"&amp; &lt; &foo; &#65; &#x41; &#X41; &#; &#x; &#xZ; &#65 & &; &a",
    b"&apos; &#039; &quot; &#1114111; &#1114112; &#99999999999999999999999;",
    b"&nbsp;&eacute;&NotEqualTilde;&CounterClockwiseContourIntegral;&CounterClockwiseContourIntegralX;",
    "caf\u{e9} \u{fc} \u{20ac} \u{2026} \u{a0} \u{2264}\u{20d2} \u{2202}\u{338} fj f <\u{20d2} =\u{20e5} \u{226b}\u{338}\u{226b}\u{20d2}".as_bytes(),
    "\u{1d538}\u{1d4b5} \u{fb00} \u{205f}\u{200a} \u{205f}x".as_bytes(),
    b"\xC0\x80|\xE0\x80\x80|\xED\xA0\x80|\xF4\x90\x80\x80|\xF0\x9F\x98|a\xC2",
    b"\xC2\x41|\xE2\x82\x41|\xE2\x41\x41|\xF0\x9F\x41\x41|\xF0\x41|\xFF\xFE|\x80",
    b"\x01\x08\x09\x0A\x0B\x0C\x0D\x1F\x7F\xC2\x80\xC2\x9F",
    b"\xEF\xB7\x90|\xEF\xB7\xAF|\xEF\xBF\xBE|\xEF\xBF\xBF|\xF4\x8F\xBF\xBF|\xF0\x9F\xBF\xBE|\xEE\x80\x80",
    b"&#0; &#9; &#11; &#12; &#13; &#x7F; &#x80; &#x9F; &#xD800; &#xFFFE; &#xFDD0; &#x1FFFF;",
    b"&amp &AMP; &Amp; &lt&gt; &&amp;; &#38;amp; &amp;amp;",
    b"abc&",
    b"&ab",
    b"&lt",
    b"&#38",
    b"x&#38;",
];

/// Strings in single-byte charsets: every high byte.
fn high_bytes() -> Vec<u8> {
    let mut v: Vec<u8> = b"<&>'\" ".to_vec();
    v.extend(0x80..=0xFF);
    v
}

const SINGLE_BYTE_CHARSETS: &[&str] = &[
    "ISO-8859-1",
    "iso8859-15",
    "cp1252",
    "windows-1251",
    "KOI8-R",
    "koi8-ru",
    "cp866",
    "IBM866",
    "ISO-8859-5",
    "MacRoman",
];

const MULTI_BYTE_CHARSETS: &[&str] = &[
    "BIG5",
    "950",
    "GB2312",
    "936",
    "BIG5-HKSCS",
    "Shift_JIS",
    "sjis-win",
    "932",
    "EUC-JP",
    "eucjp-win",
];

const MB_INPUTS: &[&[u8]] = &[
    b"\xA4\x40<\x81\x40>\xFE\xFE&\x81\x7F\x81\x80\x81\xFF\xA1",
    b"\xB0\xA1\xA1\x41\xA1\xA0\x8E\xA1\x8F\xA1\xA1\x8F\xA1\x41\xA0\xFF\x5C",
    b"\x82\xA0\xA1\xDF\x80\xA0\xFD\x81\x7F\x81\xFD\xE0",
    b"\x8E\xA1\x8E\xE0\x8E\xFF\x8F\xA1\xFF\x8F\xFF\xA1\x8F\xA0\xA0\xFE",
    b"\x01\x0B\x7F\x7E\x5C<'",
];

fn s(v: Vec<u8>) -> V {
    V::Str(v)
}

fn table(t: Vec<(Vec<u8>, Vec<u8>)>) -> V {
    assoc(t.into_iter().map(|(k, v)| (K::Str(k), V::Str(v))))
}

#[allow(clippy::too_many_lines)]
fn escape_cases(out: &mut Vec<Case>) {
    for input in UTF8_INPUTS {
        for &flags in ESCAPE_FLAGS {
            for de in [true, false] {
                let de_s = if de { "true" } else { "false" };
                out.push(case(
                    &format!("htmlspecialchars({}, {flags}, 'UTF-8', {de_s})", lit(input)),
                    |n| {
                        Ok::<_, php_stdstr::StdError>(s(htmlspecialchars(
                            input,
                            flags,
                            Some(b"UTF-8"),
                            de,
                            n,
                        )))
                    },
                ));
                out.push(case(
                    &format!("htmlentities({}, {flags}, null, {de_s})", lit(input)),
                    |n| Ok::<_, php_stdstr::StdError>(s(htmlentities(input, flags, None, de, n))),
                ));
            }
        }
    }
    let hb = high_bytes();
    for cs in SINGLE_BYTE_CHARSETS {
        for flags in [
            ENT_QUOTES,
            ENT_COMPAT | ENT_HTML5,
            ENT_QUOTES | ENT_XML1,
            ENT_QUOTES | ENT_DISALLOWED | ENT_HTML5,
            ENT_QUOTES | ENT_DISALLOWED | ENT_XHTML,
        ] {
            for f in ["htmlentities", "htmlspecialchars"] {
                out.push(case(&format!("{f}({}, {flags}, '{cs}')", lit(&hb)), |n| {
                    let r = if f == "htmlentities" {
                        htmlentities(&hb, flags, Some(cs.as_bytes()), true, n)
                    } else {
                        htmlspecialchars(&hb, flags, Some(cs.as_bytes()), true, n)
                    };
                    Ok::<_, php_stdstr::StdError>(s(r))
                }));
            }
        }
        let ctl: &[u8] = b"\x01\x0B\x0C\x0D\x7F &#1; &#x80; &euro; &bogus;";
        out.push(case(
            &format!(
                "htmlentities({}, {}, '{cs}', false)",
                lit(ctl),
                ENT_QUOTES | ENT_DISALLOWED | ENT_HTML5
            ),
            |n| {
                Ok::<_, php_stdstr::StdError>(s(htmlentities(
                    ctl,
                    ENT_QUOTES | ENT_DISALLOWED | ENT_HTML5,
                    Some(cs.as_bytes()),
                    false,
                    n,
                )))
            },
        ));
    }
    for cs in MULTI_BYTE_CHARSETS {
        for input in MB_INPUTS {
            for flags in [
                ENT_QUOTES,
                ENT_QUOTES | ENT_SUBSTITUTE,
                ENT_QUOTES | ENT_IGNORE,
                ENT_QUOTES | ENT_DISALLOWED | ENT_SUBSTITUTE | ENT_XML1,
            ] {
                out.push(case(
                    &format!("htmlentities({}, {flags}, '{cs}')", lit(input)),
                    |n| {
                        Ok::<_, php_stdstr::StdError>(s(htmlentities(
                            input,
                            flags,
                            Some(cs.as_bytes()),
                            true,
                            n,
                        )))
                    },
                ));
                out.push(case(
                    &format!("htmlspecialchars({}, {flags}, '{cs}')", lit(input)),
                    |n| {
                        Ok::<_, php_stdstr::StdError>(s(htmlspecialchars(
                            input,
                            flags,
                            Some(cs.as_bytes()),
                            true,
                            n,
                        )))
                    },
                ));
            }
        }
    }
    // Charset name resolution (case-insensitive, NUL truncation, length).
    for cs in [
        "utf-8",
        "UtF-8",
        "iso-8859-1",
        "ISO8859-1",
        "WINDOWS-1252",
        "1252",
        "win-1251",
        "866",
        "koi8r",
        "macroman",
        "utf-8\0junk",
    ] {
        let input: &[u8] = "\u{e9}\u{20ac}<".as_bytes();
        out.push(case(
            &format!(
                "htmlentities({}, {ENT_QUOTES}, {})",
                lit(input),
                lit(cs.as_bytes())
            ),
            |n| {
                Ok::<_, php_stdstr::StdError>(s(htmlentities(
                    input,
                    ENT_QUOTES,
                    Some(cs.as_bytes()),
                    true,
                    n,
                )))
            },
        ));
    }
}

fn decode_cases(out: &mut Vec<Case>) {
    for input in UTF8_INPUTS {
        for &flags in DECODE_FLAGS {
            out.push(case(
                &format!("htmlspecialchars_decode({}, {flags})", lit(input)),
                |_| Ok::<_, php_stdstr::StdError>(s(htmlspecialchars_decode(input, flags))),
            ));
            out.push(case(
                &format!("html_entity_decode({}, {flags}, 'UTF-8')", lit(input)),
                |n| {
                    Ok::<_, php_stdstr::StdError>(s(html_entity_decode(
                        input,
                        flags,
                        Some(b"UTF-8"),
                        n,
                    )))
                },
            ));
        }
    }
    let ents: &[u8] = b"&euro;&eacute;&copy;&nbsp;&#x401;&#1025;&#x2116;&numero;&Dagger;&#8364;&sect;&#xA4;&#x5C;&#x7E;&#x7F;&#x20;&#x7D;&yen;&Yuml;&OElig;&lt;&nvlt;&#39;&#x22;&apos;";
    for cs in SINGLE_BYTE_CHARSETS.iter().chain(MULTI_BYTE_CHARSETS) {
        for flags in [
            ENT_QUOTES,
            ENT_COMPAT | ENT_HTML5,
            ENT_NOQUOTES | ENT_XHTML,
            ENT_QUOTES | ENT_XML1,
        ] {
            out.push(case(
                &format!("html_entity_decode({}, {flags}, '{cs}')", lit(ents)),
                |n| {
                    Ok::<_, php_stdstr::StdError>(s(html_entity_decode(
                        ents,
                        flags,
                        Some(cs.as_bytes()),
                        n,
                    )))
                },
            ));
        }
    }
    // No '&': charset is never looked at.
    out.push(case("html_entity_decode('abc', 3, 'bogus')", |n| {
        Ok::<_, php_stdstr::StdError>(s(html_entity_decode(b"abc", 3, Some(b"bogus"), n)))
    }));
}

fn table_cases(out: &mut Vec<Case>) {
    let flag_sets = [
        ENT_QUOTES,
        ENT_COMPAT,
        ENT_NOQUOTES,
        ENT_QUOTES | ENT_HTML5,
        ENT_QUOTES | ENT_XML1,
        ENT_QUOTES | ENT_XHTML,
    ];
    for t in [0i64, 1] {
        for flags in flag_sets {
            out.push(case(
                &format!("get_html_translation_table({t}, {flags}, 'UTF-8')"),
                |n| {
                    Ok::<_, php_stdstr::StdError>(table(get_html_translation_table(
                        t,
                        flags,
                        Some(b"UTF-8"),
                        n,
                    )))
                },
            ));
        }
    }
    for cs in SINGLE_BYTE_CHARSETS.iter().chain(MULTI_BYTE_CHARSETS) {
        for flags in [ENT_QUOTES, ENT_COMPAT | ENT_HTML5] {
            out.push(case(
                &format!("get_html_translation_table(1, {flags}, '{cs}')"),
                |n| {
                    Ok::<_, php_stdstr::StdError>(table(get_html_translation_table(
                        1,
                        flags,
                        Some(cs.as_bytes()),
                        n,
                    )))
                },
            ));
        }
    }
    out.push(case("get_html_translation_table(2, 0)", |n| {
        Ok::<_, php_stdstr::StdError>(table(get_html_translation_table(2, 0, None, n)))
    }));
}

/// php-src master changed the unsupported-charset warning text; these are
/// checked against explicit expectations.
fn master_only_cases(out: &mut Vec<Case>) {
    out.push(
        case("htmlentities('<x>', 3, 'bogus')", |n| {
            Ok::<_, php_stdstr::StdError>(s(htmlentities(b"<x>", 3, Some(b"bogus"), true, n)))
        })
        .expect("Warning: htmlentities(): Charset \"bogus\" is not supported, assuming UTF-8\n=> '&lt;x&gt;'"),
    );
    out.push(
        case("htmlspecialchars('', 3, 'bogus')", |n| {
            Ok::<_, php_stdstr::StdError>(s(htmlspecialchars(b"", 3, Some(b"bogus"), true, n)))
        })
        .expect("=> ''"),
    );
    out.push(
        case("html_entity_decode('&amp;', 3, 'utf8')", |n| {
            Ok::<_, php_stdstr::StdError>(s(html_entity_decode(b"&amp;", 3, Some(b"utf8"), n)))
        })
        .expect("Warning: html_entity_decode(): Charset \"utf8\" is not supported, assuming UTF-8\n=> '&'"),
    );
    out.push(
        case("htmlentities('x', 3, 'abcdefghijklm')", |n| {
            Ok::<_, php_stdstr::StdError>(s(htmlentities(b"x", 3, Some(b"abcdefghijklm"), true, n)))
        })
        .expect("Warning: htmlentities(): Charset \"abcdefghijklm\" is not supported, assuming UTF-8\n=> 'x'"),
    );
    out.push(
        case("get_html_translation_table(0, 3, 'x')", |n| {
            Ok::<_, php_stdstr::StdError>(table(get_html_translation_table(0, 3, Some(b"x"), n)))
        })
        .expect(concat!(
            "Warning: get_html_translation_table(): Charset \"x\" is not supported, assuming UTF-8\n",
            "=> array (\n  '\"' => '&quot;',\n  '&' => '&amp;',\n  '\\'' => '&#039;',\n  '<' => '&lt;',\n  '>' => '&gt;',\n)"
        )),
    );
}

/// Cases modelled on `html_entity_decode3.phpt` / `htmlentities2x.phpt` and
/// whole-table round trips.
fn phpt_cases(out: &mut Vec<Case>) {
    let numeric: &[u8] = b"&#0;&#1;&#x09;&#x0A;&#x0B;&#x0C;&#x0D;&#x0E;&#x1F;&#x20;&#x27;&#x7F;&#x80;&#x9F;&#xA0;&#xD7FF;&#xD800;&#xDFFF;&#xE000;&#xFFFE;&#xFFFF;&#xFDCF;&#xFDD0;&#xFDEF;&#xFDF0;&#x2FFFE;&#x2FFFF;&#x10FFFF;";
    for doc in [ENT_HTML401, ENT_XML1, ENT_XHTML, ENT_HTML5] {
        for q in [ENT_QUOTES, ENT_NOQUOTES] {
            let flags = doc | q;
            out.push(case(
                &format!("html_entity_decode({}, {flags}, 'UTF-8')", lit(numeric)),
                |n| {
                    Ok::<_, php_stdstr::StdError>(s(html_entity_decode(
                        numeric,
                        flags,
                        Some(b"UTF-8"),
                        n,
                    )))
                },
            ));
            let flags = flags | ENT_DISALLOWED;
            out.push(case(
                &format!(
                    "htmlspecialchars({}, {flags}, 'UTF-8', false)",
                    lit(numeric)
                ),
                |n| {
                    Ok::<_, php_stdstr::StdError>(s(htmlspecialchars(
                        numeric,
                        flags,
                        Some(b"UTF-8"),
                        false,
                        n,
                    )))
                },
            ));
            out.push(case(
                &format!("htmlentities({}, {flags}, 'cp1252', false)", lit(numeric)),
                |n| {
                    Ok::<_, php_stdstr::StdError>(s(htmlentities(
                        numeric,
                        flags,
                        Some(b"cp1252"),
                        false,
                        n,
                    )))
                },
            ));
        }
    }
    for flags in [
        ENT_QUOTES | ENT_HTML401,
        ENT_QUOTES | ENT_HTML5,
        ENT_QUOTES | ENT_XHTML,
        ENT_COMPAT | ENT_HTML5,
    ] {
        out.push(case(
            &format!(
                "html_entity_decode(implode('', get_html_translation_table(1, {flags})), {flags})"
            ),
            |n| {
                let t = get_html_translation_table(1, flags, None, n);
                let joined: Vec<u8> = t.iter().flat_map(|(_, v)| v.clone()).collect();
                Ok::<_, php_stdstr::StdError>(s(html_entity_decode(&joined, flags, None, n)))
            },
        ));
        out.push(case(
            &format!("htmlentities(implode('', array_keys(get_html_translation_table(1, {flags}))), {flags})"),
            |n| {
                let t = get_html_translation_table(1, flags, None, n);
                let joined: Vec<u8> = t.iter().flat_map(|(k, _)| k.clone()).collect();
                Ok::<_, php_stdstr::StdError>(s(htmlentities(&joined, flags, None, true, n)))
            },
        ));
    }
    // XHTML &apos; hack, entity length limit, ampersand runs.
    for input in [
        &b"&apos;&#39;&#039;"[..],
        b"&&&&amp;&&",
        b"&CounterClockwiseContourIntegral; &CounterClockwiseContourIntegralX; &xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx;",
    ] {
        for flags in [ENT_QUOTES | ENT_XHTML, ENT_QUOTES | ENT_XML1, ENT_COMPAT | ENT_XHTML, ENT_QUOTES | ENT_HTML5] {
            out.push(case(
                &format!("htmlspecialchars_decode({}, {flags})", lit(input)),
                |_| Ok::<_, php_stdstr::StdError>(s(htmlspecialchars_decode(input, flags))),
            ));
            out.push(case(
                &format!("htmlentities({}, {flags}, 'UTF-8', false)", lit(input)),
                |n| Ok::<_, php_stdstr::StdError>(s(htmlentities(input, flags, Some(b"UTF-8"), false, n))),
            ));
        }
    }
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    phpt_cases(&mut out);
    escape_cases(&mut out);
    decode_cases(&mut out);
    table_cases(&mut out);
    master_only_cases(&mut out);
    out
}

#[test]
fn oracle() {
    check("html", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("html", &cases());
}
