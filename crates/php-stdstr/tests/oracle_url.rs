// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `url.rs` (urlencode family, `parse_url`).
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{assoc, case, check, lit, pure, regenerate, Case, V};
use php_stdstr::url::{
    parse_url, rawurldecode, rawurlencode, urldecode, urlencode, ParseUrlResult, UrlPart,
};

/// The corpus of `ext/standard/tests/url/urls.inc` plus the bug tests and
/// extra edge cases.
const URLS: &[&str] = &[
    "64.246.30.37",
    "http://64.246.30.37",
    "http://64.246.30.37/",
    "64.246.30.37/",
    "64.246.30.37:80/",
    "php.net",
    "php.net/",
    "http://php.net",
    "http://php.net/",
    "www.php.net",
    "www.php.net/",
    "http://www.php.net",
    "http://www.php.net/",
    "www.php.net:80",
    "http://www.php.net:80",
    "http://www.php.net:80/",
    "http://www.php.net/index.php",
    "www.php.net/?",
    "www.php.net:80/?",
    "http://www.php.net/?",
    "http://www.php.net:80/?",
    "http://www.php.net:80/index.php",
    "http://www.php.net:80/foo/bar/index.php",
    "http://www.php.net:80/this/is/a/very/deep/directory/structure/and/file.php",
    "http://www.php.net:80/this/is/a/very/deep/directory/structure/and/file.php?lots=1&of=2&parameters=3&too=4&here=5",
    "http://www.php.net:80/this/is/a/very/deep/directory/structure/and/",
    "http://www.php.net:80/this/../a/../deep/directory",
    "http://www.php.net:80/this/../a/../deep/directory/",
    "http://www.php.net:80/this/is/a/very/deep/directory/../file.php",
    "http://www.php.net:80/index.php?",
    "http://www.php.net:80/#foo",
    "http://www.php.net:80/?#",
    "http://www.php.net:80/?test=1",
    "http://www.php.net/?test=1&",
    "http://www.php.net:80/?&",
    "http://www.php.net:80/index.php?test=1&",
    "http://www.php.net/index.php?&",
    "http://www.php.net:80/index.php?foo&",
    "http://www.php.net/index.php?&foo",
    "http://www.php.net:80/index.php?test=1&test2=char&test3=mixesCI",
    "www.php.net:80/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://secret@www.php.net:80/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://secret:@www.php.net/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://:hideout@www.php.net:80/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://secret:hideout@www.php.net/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://secret@hideout@www.php.net:80/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "http://secret:hid:out@www.php.net:80/index.php?test=1&test2=char&test3=mixesCI#some_page_ref123",
    "nntp://news.php.net",
    "ftp://ftp.gnu.org/gnu/glic/glibc.tar.gz",
    "zlib:http://foo@bar",
    "zlib:filename.txt",
    "zlib:/path/to/my/file/file.txt",
    "foo://foo@bar",
    "mailto:me@mydomain.com",
    "/foo.php?a=b&c=d",
    "foo.php?a=b&c=d",
    "http://user:passwd@www.example.com:8080?bar=1&boom=0",
    "http://user_me-you:my_pas-word@www.example.com:8080?bar=1&boom=0",
    "file:///path/to/file",
    "file://path/to/file",
    "file:/path/to/file",
    "http://1.2.3.4:/abc.asp?a=1&b=2",
    "http://foo.com#bar",
    "scheme:",
    "foo+bar://baz@bang/bla",
    "gg:9130731",
    "http://user:@pass@host/path?argument?value#etc",
    "http://10.10.10.10/:80",
    "http://x:?",
    "x:blah.com",
    "x:/blah.com",
    "x://::abc/?",
    "http://::?",
    "http://::#",
    "x://::6.5",
    "http://?:/",
    "http://@?:/",
    "file:///:",
    "file:///a:/",
    "file:///ab:/",
    "file:///@:/",
    "file:///:80/",
    "[]",
    "http://[x:80]/",
    "",
    "/",
    "/rest/Users?filter={\"id\":\"123\"}",
    "%:x",
    "https://example.com:0/",
    "http:///blah.com",
    "http://:80",
    "http://user@:80",
    "http://user:pass@:80",
    "http://:",
    "http://@/",
    "http://@:/",
    "http://:/",
    "http://?",
    "http://#",
    "http://?:",
    "http://:?",
    "http://blah.com:123456",
    "http://blah.com:abcdef",
    // bug tests
    "//example.org:81/hi?a=b#c=d",
    "//example.org/hi?a=b#c=d",
    "127.0.0.1:9999?",
    "127.0.0.1:9999#",
    "internal:#feeding",
    "magnet:?xt=urn:sha1:YNCKHTQCWBTRNJIV4WNAE52SJUQCZO5C",
    "http://example.com:80#@google.com/",
    "http://example.com:80?@google.com/",
    "http://user:pass@host",
    "//user:pass@host",
    "//user@host",
    "//php.net/path?query=1:2",
    "//php.net/path.php?query=a:b",
    "//username@php.net/path?query=1:2",
    "//example.org",
    "http://example.com/path/script.html?t=1#fragment?data",
    "http://example.com/path/script.html#fragment?data",
    ":",
    "/busca/?fq=B:20001",
    "/busca/?fq=B:200013",
    "/busca/?fq=home:01234",
    "/busca/?fq=home:012345",
    // extra edge cases
    "http://[::1]:8080/x",
    "http://[::1]/x",
    "http://[::1]",
    "[::1]:80",
    "http://a:65535",
    "http://a:65536",
    "http://a:0",
    "http://a:00080/",
    "http://a:0:5/",
    "//a:0:5/",
    "a:65535",
    "a:65536",
    "a:123456",
    "a:0/x",
    ":80",
    ":80/x",
    "::",
    "a:b@c:d",
    "FILE:///c:/x",
    "file:///c:/x",
    "file:////x",
    "file://",
    "http:",
    "http:/",
    "http://",
    "//",
    "///",
    "?",
    "#",
    "?#",
    "a?b#c?d",
    "http://h/p?#",
    "x y://z",
    "x y:80//z",
    "1http://x",
    "http://u@h@i/",
    "http://u:p:q@h/",
    "http://h:1:2/",
    "http://h:/",
    "h:/",
    "mailto:a@b.c?subject=x",
    "news:comp.lang.php",
    "a.b:80",
    "a.b:800000",
    "a.b:80a",
    "a_b:80?x",
    "a_b:x?y",
    "a_b?:x",
    "//:80",
    "/:80",
    "http://h/\u{1}\u{7f}x\u{1f}?q\u{2}#f\u{3}",
    "http://u\u{5}:p\u{6}@h\u{7}/",
    "s\u{1}:x",
];

fn url_value(r: ParseUrlResult) -> V {
    match r {
        ParseUrlResult::Failed => V::Bool(false),
        ParseUrlResult::All(u) => assoc(u.entries().into_iter().map(|(k, v)| (k, part(v)))),
        ParseUrlResult::Component(p) => p.map_or(V::Null, part),
    }
}

fn part(p: UrlPart) -> V {
    match p {
        UrlPart::Str(s) => V::Str(s),
        UrlPart::Port(p) => V::Int(i64::from(p)),
    }
}

/// `bin2hex()` — binary results are compared hex-encoded so that invalid
/// UTF-8 survives the textual fixture byte-exactly.
fn hex(s: &[u8]) -> String {
    s.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

const STRINGS: &[&[u8]] = &[
    b"",
    b"abc",
    b"a b+c~d_e.f-g",
    b"!\"#$%&'()*,/:;<=>?@[\\]^`{|}",
    b"\x00\x01\x7f\x80\xff",
    b"%41%4a%4A%zz%4%%",
    b"%",
    b"%4",
    b"%41",
    b"+%2B+",
    b"%e4%f6%FC",
    b"caf\xc3\xa9 \xe2\x82\xac",
];

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for &u in URLS {
        let l = lit(u.as_bytes());
        v.push(case(&format!("parse_url({l})"), |_| {
            parse_url(u.as_bytes(), -1).map(url_value)
        }));
        for comp in [-2i64, 0, 1, 2, 3, 4, 5, 6, 7, 8, 99] {
            v.push(case(&format!("parse_url({l}, {comp})"), |_| {
                parse_url(u.as_bytes(), comp).map(url_value)
            }));
        }
    }
    for &s in STRINGS {
        let l = lit(s);
        v.push(pure(&format!("urlencode({l})"), urlencode(s)));
        v.push(pure(&format!("rawurlencode({l})"), rawurlencode(s)));
        v.push(pure(
            &format!("bin2hex(urldecode({l}))"),
            hex(&urldecode(s)),
        ));
        v.push(pure(
            &format!("bin2hex(rawurldecode({l}))"),
            hex(&rawurldecode(s)),
        ));
    }
    let all: Vec<u8> = (0..=255u8).collect();
    let l = lit(&all);
    v.push(pure(&format!("urlencode({l})"), urlencode(&all)));
    v.push(pure(&format!("rawurlencode({l})"), rawurlencode(&all)));
    v.push(pure(
        &format!("bin2hex(urldecode(urlencode({l})))"),
        hex(&urldecode(&urlencode(&all))),
    ));
    v.push(pure(
        &format!("bin2hex(rawurldecode(rawurlencode({l})))"),
        hex(&rawurldecode(&rawurlencode(&all))),
    ));
    v
}

#[test]
fn oracle() {
    check("url", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("url", &cases());
}
