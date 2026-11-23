// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Expectations hand-ported from ext/intl/tests/grapheme*.phpt.

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_intl::grapheme as g;

fn s(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap()
}

#[test]
fn grapheme_empty() {
    assert_eq!(g::strpos(b"abc", b"", 0, "").unwrap(), Some(0));
    assert_eq!(g::strpos(b"abc", b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::stripos(b"abc", b"", 0, "").unwrap(), Some(0));
    assert_eq!(g::stripos(b"abc", b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::strrpos(b"abc", b"", 0, "").unwrap(), Some(3));
    assert_eq!(g::strrpos(b"abc", b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::strripos(b"abc", b"", 0, "").unwrap(), Some(3));
    assert_eq!(g::strripos(b"abc", b"", 1, "").unwrap(), Some(3));
    assert_eq!(
        s(g::strstr(b"abc", b"", false, false, "").unwrap().unwrap()),
        "abc"
    );
    assert_eq!(
        s(g::strstr(b"abc", b"", true, false, "").unwrap().unwrap()),
        ""
    );
    assert_eq!(
        s(g::strstr(b"abc", b"", false, true, "").unwrap().unwrap()),
        "abc"
    );
    let a = "äbc".as_bytes();
    assert_eq!(g::strpos(a, b"", 0, "").unwrap(), Some(0));
    assert_eq!(g::strpos(a, b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::stripos(a, b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::strrpos(a, b"", 0, "").unwrap(), Some(3));
    assert_eq!(g::strrpos(a, b"", -1, "").unwrap(), Some(2));
    assert_eq!(g::strripos(a, b"", 1, "").unwrap(), Some(3));
    assert_eq!(
        s(g::strstr(a, b"", false, false, "").unwrap().unwrap()),
        "äbc"
    );
    assert_eq!(s(g::strstr(a, b"", true, true, "").unwrap().unwrap()), "");
}

#[test]
fn grapheme_empty_offset_multibyte() {
    let h = "😀x".as_bytes();
    assert_eq!(g::strpos(h, b"", 0, "").unwrap(), Some(0));
    assert_eq!(g::strpos(h, b"", 1, "").unwrap(), Some(1));
    assert_eq!(g::stripos(h, b"", 1, "").unwrap(), Some(1));
    assert_eq!(g::strpos(h, b"", -1, "").unwrap(), Some(1));
    assert_eq!(g::strrpos(h, b"", 0, "").unwrap(), Some(2));
    assert_eq!(g::strrpos(h, b"", 1, "").unwrap(), Some(2));
    assert_eq!(g::strripos(h, b"", 1, "").unwrap(), Some(2));
    assert_eq!(g::strrpos(h, b"", -1, "").unwrap(), Some(1));
    assert_eq!(
        g::strpos(h, b"", 5, "").unwrap_err(),
        php_intl::Error::arg_value(3, "must be contained in argument #1 ($haystack)")
    );
}

#[test]
fn grapheme_strrpos_negative_offset_overlap() {
    for gr in ["a", "\u{00E9}", "e\u{0301}", "\u{1F600}"] {
        let h = gr.repeat(3);
        let n = gr.repeat(2);
        for f in [g::strrpos, g::strripos] {
            let v: Vec<i64> = [0, 1, -1, -2, -3]
                .iter()
                .map(|&o| f(h.as_bytes(), n.as_bytes(), o, "").unwrap().unwrap())
                .collect();
            assert_eq!(v, vec![1, 1, 1, 1, 0], "{gr}");
        }
    }
    assert_eq!(
        g::strripos("ÉéÉ".as_bytes(), "éé".as_bytes(), -1, "").unwrap(),
        Some(1)
    );
    assert_eq!(
        g::strrpos("ééxéé".as_bytes(), "éé".as_bytes(), -1, "").unwrap(),
        Some(3)
    );
    assert_eq!(
        g::strrpos("ééxéé".as_bytes(), "éé".as_bytes(), -3, "").unwrap(),
        Some(0)
    );
}

#[test]
fn grapheme_search_empty_haystack() {
    for n in ["a", "\u{00E9}", "e\u{0301}", "\r\n"] {
        assert_eq!(g::strpos(b"", n.as_bytes(), 0, "").unwrap(), None);
        assert_eq!(g::stripos(b"", n.as_bytes(), 0, "").unwrap(), None);
        assert_eq!(g::strrpos(b"", n.as_bytes(), 0, "").unwrap(), None);
        assert_eq!(g::strripos(b"", n.as_bytes(), 0, "").unwrap(), None);
        assert_eq!(
            g::strstr(b"", n.as_bytes(), false, false, "").unwrap(),
            None
        );
        assert_eq!(g::strstr(b"", n.as_bytes(), true, true, "").unwrap(), None);
    }
    assert_eq!(g::strpos(b"", b"", 0, "").unwrap(), Some(0));
    assert_eq!(
        s(g::strstr(b"", b"", true, false, "").unwrap().unwrap()),
        ""
    );
    for o in [-1, 1] {
        assert!(matches!(
            g::strpos(b"", "é".as_bytes(), o, ""),
            Err(php_intl::Error::ArgumentValue { .. })
        ));
        assert!(matches!(
            g::strripos(b"", "é".as_bytes(), o, ""),
            Err(php_intl::Error::ArgumentValue { .. })
        ));
    }
}

#[test]
fn grapheme_strstr_supplementary_prefix_and_canonical() {
    for (p, before) in [
        ("\u{1F600}", "😀"),
        ("\u{1F600}\u{1F600}", "😀😀"),
        ("\u{1F600}e\u{0301}", "😀e\u{301}"),
    ] {
        let h = format!("{p}abc");
        assert_eq!(
            s(g::strstr(h.as_bytes(), b"a", false, false, "")
                .unwrap()
                .unwrap()),
            "abc"
        );
        assert_eq!(
            s(g::strstr(h.as_bytes(), b"a", true, false, "")
                .unwrap()
                .unwrap()),
            before
        );
        assert_eq!(
            s(g::strstr(h.as_bytes(), b"A", false, true, "")
                .unwrap()
                .unwrap()),
            "abc"
        );
        assert_eq!(
            s(g::strstr(h.as_bytes(), b"A", true, true, "")
                .unwrap()
                .unwrap()),
            before
        );
    }
    for (h, n, a, b) in [
        ("e\u{0301}x", "\u{00E9}", "e\u{0301}x", ""),
        ("\u{00E9}x", "e\u{0301}", "\u{00E9}x", ""),
        ("pe\u{0301}x", "\u{00E9}", "e\u{0301}x", "p"),
        ("pe\u{0301}x\u{00E9}", "\u{00E9}", "e\u{0301}x\u{00E9}", "p"),
        ("abc", "b", "bc", "a"),
    ] {
        assert_eq!(
            s(g::strstr(h.as_bytes(), n.as_bytes(), false, false, "")
                .unwrap()
                .unwrap()),
            a
        );
        assert_eq!(
            s(g::strstr(h.as_bytes(), n.as_bytes(), true, false, "")
                .unwrap()
                .unwrap()),
            b
        );
    }
    assert_eq!(g::strstr(b"abc", b"z", false, false, "").unwrap(), None);
}

#[test]
fn grapheme_out_of_bounds() {
    for f in [g::strpos, g::stripos, g::strrpos, g::strripos] {
        assert_eq!(f(b"foo", b"bar", 3, "").unwrap(), None);
        assert_eq!(f("äöü".as_bytes(), b"bar", 3, "").unwrap(), None);
        assert_eq!(f(b"foo", b"bar", -3, "").unwrap(), None);
        assert_eq!(f("äöü".as_bytes(), b"bar", -3, "").unwrap(), None);
        assert_eq!(f(b"", b"bar", 0, "").unwrap(), None);
        assert!(f(b"foo", b"bar", 4, "").is_err());
        assert!(f(b"foo", b"bar", -4, "").is_err());
    }
}
