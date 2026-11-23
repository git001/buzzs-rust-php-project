// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Expectations from php-src master `.phpt` tests for functions/behaviour
//! that PHP 8.3 (the oracle used elsewhere) does not have.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::only_used_in_recursion,
    clippy::format_push_string,
    clippy::default_trait_access,
    clippy::many_single_char_names
)]

use php_mbstring::MbState;

fn st() -> MbState {
    MbState::new()
}

fn trim(s: &str, w: Option<&str>) -> String {
    String::from_utf8(
        st().mb_trim(s.as_bytes(), w.map(str::as_bytes), Some(b"UTF-8"))
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn mb_trim_phpt() {
    let mut m = st();
    let b = |v: &[u8]| v.to_vec();
    assert_eq!(m.mb_trim(b"ABC", None, None).unwrap(), b(b"ABC"));
    assert_eq!(
        m.mb_trim(b" \0\t\nABC \0\t\n", None, None).unwrap(),
        b(b"ABC")
    );
    assert_eq!(
        m.mb_ltrim(b" \0\t\nABC \0\t\n", None, None).unwrap(),
        b(b"ABC \0\t\n")
    );
    assert_eq!(
        m.mb_rtrim(b" \0\t\nABC \0\t\n", None, None).unwrap(),
        b(b" \0\t\nABC")
    );
    assert_eq!(
        m.mb_trim(b" \0\t\nABC \0\t\n", Some(b""), None).unwrap(),
        b(b" \0\t\nABC \0\t\n")
    );
    assert_eq!(m.mb_trim(b"", None, None).unwrap(), b(b""));
    assert_eq!(trim("　あいうえおあお　", Some("　")), "あいうえおあお");
    assert_eq!(trim("foo BAR Spaß", Some("ß")), "foo BAR Spa");
    assert_eq!(trim("foo BAR Spaß", Some("f")), "oo BAR Spaß");
    assert_eq!(trim("foo BAR Spaß", Some("ßf")), "oo BAR Spa");
    assert_eq!(trim("　あいうおえお 　あ", Some("あ　")), "いうおえお ");
    assert_eq!(trim("　あいうおえお 　a", Some("あa")), "　あいうおえお 　");
    assert_eq!(
        m.mb_trim(
            "　あいうおえお 　a".as_bytes(),
            Some(b"\xe3"),
            Some(b"UTF-8")
        )
        .unwrap(),
        "　あいうおえお 　a".as_bytes()
    );
    assert_eq!(trim(&"　".repeat(129), None), "");
    assert_eq!(trim(&("　".repeat(129) + "a"), None), "a");
    let chars: String = (1024u32..2048).filter_map(char::from_u32).collect();
    assert_eq!(trim(&format!("{chars}hello{chars}"), Some(&chars)), "hello");
    let l = m
        .mb_ltrim(
            format!("{chars}hello{chars}").as_bytes(),
            Some(chars.as_bytes()),
            None,
        )
        .unwrap();
    assert_eq!(l.len(), 2053);
    assert_eq!(
        String::from_utf8(
            m.mb_ltrim("ああいああ".as_bytes(), Some("あ".as_bytes()), None)
                .unwrap()
        )
        .unwrap(),
        "いああ"
    );
    assert_eq!(
        trim(" \u{c}\n\r\u{b}\0\u{A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{85}\u{180E}", None),
        ""
    );
    assert_eq!(
        m.mb_ltrim(
            "\u{FFFE}漢字".as_bytes(),
            Some("\u{FFFE}\u{FEFF}".as_bytes()),
            None
        )
        .unwrap(),
        "漢字".as_bytes()
    );
    let le = m
        .mb_convert_encoding("\u{FFFE}漢字".as_bytes(), b"UTF-16LE", None)
        .unwrap()
        .unwrap();
    let w = m
        .mb_convert_encoding("\u{FFFE}\u{FEFF}".as_bytes(), b"UTF-16LE", None)
        .unwrap()
        .unwrap();
    assert_eq!(
        m.mb_ltrim(&le, Some(&w), Some(b"UTF-16LE")).unwrap(),
        b"\x22\x6f\x57\x5b"
    );
    let be = m
        .mb_convert_encoding("\u{FEFF}漢字".as_bytes(), b"UTF-16BE", None)
        .unwrap()
        .unwrap();
    let w = m
        .mb_convert_encoding("\u{FFFE}\u{FEFF}".as_bytes(), b"UTF-16BE", None)
        .unwrap()
        .unwrap();
    assert_eq!(
        m.mb_ltrim(&be, Some(&w), Some(b"UTF-16BE")).unwrap(),
        b"\x6f\x22\x5b\x57"
    );
    assert_eq!(
        m.mb_trim(
            b"\x81\x40\x82\xa0\x81\x40",
            Some(b"\x81\x40"),
            Some(b"SJIS")
        )
        .unwrap(),
        b"\x82\xa0"
    );
    assert_eq!(trim("foo", Some("oo")), "f");
    let e = m
        .mb_trim("\u{180F}".as_bytes(), Some(b""), Some(b"NULL"))
        .unwrap_err();
    assert_eq!(
        e.message_for("mb_trim"),
        "mb_trim(): Argument #3 ($encoding) must be a valid encoding, \"NULL\" given"
    );
}

#[test]
fn mb_ucfirst_lcfirst_phpt() {
    let mut m = st();
    for c in 0u8..128 {
        let up = m.mb_ucfirst(&[c], None).unwrap();
        assert_eq!(up, vec![c.to_ascii_uppercase()]);
        let lo = m.mb_lcfirst(&[c], None).unwrap();
        assert_eq!(lo, vec![c.to_ascii_lowercase()]);
    }
    let uc = |m: &mut MbState, s: &str| {
        String::from_utf8(m.mb_ucfirst(s.as_bytes(), None).unwrap()).unwrap()
    };
    let lc = |m: &mut MbState, s: &str| {
        String::from_utf8(m.mb_lcfirst(s.as_bytes(), None).unwrap()).unwrap()
    };
    assert_eq!(uc(&mut m, ""), "");
    assert_eq!(uc(&mut m, "ａｂ"), "Ａｂ");
    assert_eq!(uc(&mut m, "đắt quá!"), "Đắt quá!");
    assert_eq!(uc(&mut m, "აბგ"), "აბგ");
    assert_eq!(uc(&mut m, "ǉ"), "ǈ");
    assert_eq!(lc(&mut m, "ＡＢＳ"), "ａＢＳ");
    assert_eq!(lc(&mut m, "Đẹp quá!"), "đẹp quá!");
    let sj = |m: &mut MbState, f: bool, h: &[u8]| {
        if f {
            m.mb_ucfirst(h, Some(b"SJIS")).unwrap()
        } else {
            m.mb_lcfirst(h, Some(b"SJIS")).unwrap()
        }
    };
    assert_eq!(
        sj(&mut m, true, b"\x82\x85\x82\x82\x82\x89"),
        b"\x82\x64\x82\x82\x82\x89"
    );
    assert_eq!(
        sj(&mut m, false, b"\x82\x64\x82\x61\x82\x68"),
        b"\x82\x85\x82\x61\x82\x68"
    );
    assert_eq!(sj(&mut m, true, b"\x84\x71"), b"\x84\x41");
    assert_eq!(sj(&mut m, false, b"\x83\x9f"), b"\x83\xbf");
    assert_eq!(sj(&mut m, true, b"\x83\xbf\x84\x71"), b"\x83\x9f\x84\x71");
    assert_eq!(
        m.mb_ucfirst(b"\xa6\xd8", Some(b"EUC-JP")).unwrap(),
        b"\xa6\xb8"
    );
    assert_eq!(
        m.mb_lcfirst(b"\xa6\xb8", Some(b"EUC-JP")).unwrap(),
        b"\xa6\xd8"
    );
    let long = format!("э{}", "A".repeat(65536));
    assert_eq!(uc(&mut m, &long), format!("Э{}", "A".repeat(65536)));
}

#[test]
fn deprecations_for_transfer_encodings() {
    let mut m = st();
    m.mb_strlen(b"abc", Some(b"BASE64")).unwrap();
    m.mb_strlen(b"abc", Some(b"base64")).unwrap();
    let d = m.take_diagnostics();
    assert_eq!(d.len(), 1, "cached encoding name suppresses the repeat");
    assert_eq!(
        d[0].message,
        "Handling Base64 via mbstring is deprecated; use base64_encode/base64_decode instead"
    );
}
