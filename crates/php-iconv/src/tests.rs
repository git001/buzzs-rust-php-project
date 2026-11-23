// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Unit tests: settings, diagnostics, output handler, filter, and
//! hand-ported `.phpt` expectations.  The bulk of behavioural coverage is
//! the oracle replay in `tests/oracle.rs`.

use super::*;

fn ic() -> Iconv {
    php_stdmisc::locale::reset_lc_ctype();
    Iconv::new()
}

#[test]
fn constants_and_version() {
    let c = constants();
    assert_eq!(c[0], ("ICONV_IMPL", ConstValue::Str("glibc".into())));
    assert_eq!(c[2], ("ICONV_MIME_DECODE_STRICT", ConstValue::Int(1)));
    assert_eq!(
        c[3],
        ("ICONV_MIME_DECODE_CONTINUE_ON_ERROR", ConstValue::Int(2))
    );
    let v = iconv_version();
    assert!(v.starts_with("2."), "{v}");
}

#[test]
fn encoding_fallbacks() {
    let mut i = ic();
    assert_eq!(i.internal_encoding(), b"UTF-8");
    i.core.default_charset = b"ISO-8859-1".to_vec();
    assert_eq!(i.input_encoding(), b"ISO-8859-1");
    i.core.input_encoding = b"UTF-8".to_vec();
    assert_eq!(i.input_encoding(), b"UTF-8");
    i.core.default_charset.clear();
    assert_eq!(i.output_encoding(), b"UTF-8");
    i.ini.output_encoding = b"SJIS".to_vec();
    assert_eq!(i.output_encoding(), b"SJIS");
}

#[test]
fn set_and_get_encoding() {
    // iconv_get_encoding_basic.phpt / iconv_encoding_basic.phpt
    let mut i = ic();
    let r = i.set_encoding(b"internal_encoding", b"UTF-8");
    assert_eq!(r.value, Some(true));
    assert_eq!(
        r.diagnostic,
        Some(Diagnostic::Deprecated(
            "Use of iconv.internal_encoding is deprecated".into()
        ))
    );
    assert_eq!(
        i.set_encoding(b"OUTPUT_ENCODING", b"ISO-8859-1").value,
        Some(true)
    );
    assert_eq!(
        i.set_encoding(b"foo", b"x"),
        Outcome {
            value: None,
            diagnostic: None
        }
    );
    assert_eq!(
        i.set_encoding(b"input_encoding", &[b'a'; 64]).diagnostic,
        Some(charset_too_long())
    );
    assert_eq!(i.get_encoding(Some(b"foo")).value, None);
    assert_eq!(
        i.get_encoding(None).value,
        Some(EncodingInfo::All {
            input_encoding: b"UTF-8".to_vec(),
            output_encoding: b"ISO-8859-1".to_vec(),
            internal_encoding: b"UTF-8".to_vec(),
        })
    );
    assert_eq!(
        i.get_encoding(Some(b"Output_Encoding")).value,
        Some(EncodingInfo::Single(b"ISO-8859-1".to_vec()))
    );
}

#[test]
fn ini_update_rules() {
    let mut i = ic();
    assert_eq!(
        i.ini_update("iconv.input_encoding", b"UTF-8", false),
        Ok(None)
    );
    assert_eq!(
        i.ini_update("iconv.input_encoding", b"UTF-8", true),
        Ok(Some(Diagnostic::Deprecated(
            "Use of iconv.input_encoding is deprecated".into()
        )))
    );
    // ob_iconv_handler-charset-length-cve-2007-4840.phpt: rejected silently.
    assert_eq!(
        i.ini_update("iconv.output_encoding", &[b'a'; 9000], true),
        Err(())
    );
    assert_eq!(i.ini_update("iconv.bogus", b"x", true), Err(()));
}

#[test]
fn charset_length_limits() {
    let i = ic();
    let long = [b'x'; 64];
    let w = Some(charset_too_long());
    assert_eq!(i.iconv(&long, b"UTF-8", b"a").diagnostic, w);
    assert_eq!(i.strlen(b"a", Some(&long)).diagnostic, w);
    assert_eq!(i.substr(b"a", 0, None, Some(&long)).diagnostic, w);
    assert_eq!(i.strpos(b"a", b"a", 0, Some(&long)).diagnostic, w);
    assert_eq!(i.strrpos(b"a", b"a", Some(&long)).diagnostic, w);
    assert_eq!(i.mime_decode(b"a", 0, Some(&long)).diagnostic, w);
    assert_eq!(i.mime_decode_headers(b"a: b", 0, Some(&long)).diagnostic, w);
    assert_eq!(
        w.as_ref().map(ToString::to_string).as_deref(),
        Some("Encoding parameter exceeds the maximum allowed length of 64 characters")
    );
}

#[test]
fn basic_conversions() {
    let i = ic();
    assert_eq!(
        i.iconv(b"UTF-8", b"ISO-8859-1", "café".as_bytes()).value,
        Some(b"caf\xe9".to_vec())
    );
    assert_eq!(
        i.iconv(b"UTF-8", b"ASCII//TRANSLIT", "aéb€".as_bytes())
            .value,
        Some(b"aebEUR".to_vec())
    );
    let r = i.iconv(b"UTF-8", b"ASCII", b"a\xc3");
    assert_eq!(r.value, None);
    assert_eq!(
        r.diagnostic,
        Some(Diagnostic::Notice(
            "Detected an incomplete multibyte character in input string".into()
        ))
    );
    let r = i.iconv(b"FOO", b"BAR", b"x");
    assert_eq!(
        r.diagnostic,
        Some(Diagnostic::Warning(
            "Wrong encoding, conversion from \"FOO\" to \"BAR\" is not allowed".into()
        ))
    );
    // bug48147.phpt
    assert_eq!(
        i.iconv(b"UTF-8", b"UTF-8//IGNORE", b"aa\xC3\xC3\xC3\xB8aa")
            .value,
        Some(b"aa\xC3\xB8aa".to_vec())
    );
    assert_eq!(i.iconv(b"UTF-8", b"UTF-8//IGNORE", b"\xC3").value, None);
    // translit-failure.phpt is libiconv-only; glibc drops the accent.
    assert_eq!(
        i.iconv(b"ISO-8859-1", b"ASCII//TRANSLIT", b"\xc9crit par %s.")
            .value,
        Some(b"Ecrit par %s.".to_vec())
    );
}

#[test]
fn translit_quotes_phpt() {
    // translit-utf8.phpt expects libiconv output; these are glibc's.
    let i = ic();
    let utf = "\u{201c}Hello\u{201d}\n\u{2018}Hello\u{2019}\n\u{201e}Hello\u{201d}\n\u{201a}Hello\u{2019}\n";
    let mut out = i
        .iconv(b"UTF-8", b"ISO-8859-1//TRANSLIT", utf.as_bytes())
        .value
        .expect("latin1");
    out.extend(
        i.iconv(b"UTF-8", b"ASCII//TRANSLIT", utf.as_bytes())
            .value
            .expect("ascii"),
    );
    assert_eq!(
        out,
        b"\"Hello\"\n'Hello'\n,,Hello\"\n,Hello'\n\"Hello\"\n'Hello'\n,,Hello\"\n,Hello'\n"
            .to_vec()
    );
}

#[test]
fn str_functions() {
    let i = ic();
    assert_eq!(i.strlen("äöü".as_bytes(), None).value, Some(3));
    // iconv_substr_out_of_bounds.phpt
    let sub = |s: &str, o: i64, l: Option<i64>| {
        String::from_utf8(i.substr(s.as_bytes(), o, l, None).value.expect("substr")).expect("utf8")
    };
    assert_eq!(sub("foo", 3, None), "");
    assert_eq!(sub("foo", -4, None), "foo");
    assert_eq!(sub("äöü", -4, Some(1)), "ä");
    assert_eq!(sub("äöü", -4, Some(-1)), "äö");
    assert_eq!(sub("äöü", 2, Some(-2)), "");
    // bug79200.phpt
    assert_eq!(i.strlen(b"test test", Some(b"WINDOWS-1258")).value, Some(9));
    assert_eq!(
        i.strpos(b"test test", b"test test", 0, Some(b"WINDOWS-1258"))
            .value,
        Some(0)
    );
    assert_eq!(
        i.substr(b"test test", 0, Some(9), Some(b"WINDOWS-1258"))
            .value,
        Some(b"test test".to_vec())
    );
    assert_eq!(i.strrpos(b"ababab", b"ab", None).value, Some(4));
    assert_eq!(
        i.strpos(b"abc", b"", 0, None),
        Outcome {
            value: None,
            diagnostic: None
        }
    );
    assert_eq!(
        i.strpos(b"abc", b"b", 4, None).diagnostic,
        Some(Diagnostic::ValueError(
            "Argument #3 ($offset) must be contained in argument #1 ($haystack)".into()
        ))
    );
    assert_eq!(
        i.strpos(b"abc", b"b", -4, None).diagnostic,
        Some(Diagnostic::ValueError(
            "Argument #3 ($offset) must be contained in argument #1 ($haystack)".into()
        ))
    );
    // iconv_strpos_error2.phpt
    assert_eq!(
        i.strpos(b"Hello, world", b"world", 2, Some(b"unknown-encoding"))
            .diagnostic,
        Some(Diagnostic::Warning(
            "Wrong encoding, conversion from \"unknown-encoding\" to \"UCS-4LE\" is not allowed"
                .into()
        ))
    );
}

#[test]
fn mime_encode_cases() {
    let mut i = ic();
    // iconv004.phpt
    i.core.internal_encoding = b"ISO-8859-1".to_vec();
    assert_eq!(
        i.mime_encode(b"", b"", &MimeEncodePrefs::default()).value,
        Some(b": =?ISO-8859-1?B??=".to_vec())
    );
    let i = ic();
    let prefs = MimeEncodePrefs {
        scheme: Some(b"Q".to_vec()),
        line_length: Some(30),
        line_break_chars: Some(b"\n".to_vec()),
        ..MimeEncodePrefs::default()
    };
    assert_eq!(
        i.mime_encode(b"Subject", "Prüfung Prüfung Prüfung".as_bytes(), &prefs).value,
        Some(b"Subject: =?UTF-8?Q?Pr=C3=BCf?=\n =?UTF-8?Q?ung=20Pr=C3=BCfun?=\n =?UTF-8?Q?g=20Pr=C3=BCfung?=".to_vec())
    );
    // gh21664.phpt
    let prefs = MimeEncodePrefs {
        input_charset: Some(b"UTF-8".to_vec()),
        output_charset: Some(b"UTF-8".to_vec()),
        ..MimeEncodePrefs::default()
    };
    assert_eq!(
        i.mime_encode(b"Subject", b"\x80", &prefs).diagnostic,
        Some(Diagnostic::Notice(
            "Detected an illegal character in input string".into()
        ))
    );
    // Negative line length: safe_emalloc overflow.
    let prefs = MimeEncodePrefs {
        line_length: Some(-1),
        ..MimeEncodePrefs::default()
    };
    assert_eq!(
        i.mime_encode(b"Subject", b"x", &prefs).diagnostic,
        Some(Diagnostic::Fatal(
            "Possible integer overflow in memory allocation (1 * 18446744073709551615 + 5)".into()
        ))
    );
    // oom_iconv_mime_encode_line_length.phpt
    let prefs = MimeEncodePrefs {
        line_length: Some(i64::MAX),
        ..MimeEncodePrefs::default()
    };
    assert!(matches!(
        i.mime_encode(b"x", b"y", &prefs).diagnostic,
        Some(Diagnostic::OutOfMemory { .. })
    ));
    let prefs = MimeEncodePrefs {
        line_length: Some(5),
        ..MimeEncodePrefs::default()
    };
    assert_eq!(
        i.mime_encode(b"Subject", b"x", &prefs).diagnostic,
        Some(Diagnostic::Warning("Buffer length exceeded".into()))
    );
}

#[test]
fn mime_decode_cases() {
    let i = ic();
    // gh7980.phpt
    assert_eq!(
        String::from_utf8(
            i.mime_decode(
                b"=?windows-1258?Q?DSI_Charg=E9_de_Formation_Jean_Dupont?= <jdupont@example.fr>",
                MIME_DECODE_STRICT,
                Some(b"UTF-8")
            )
            .value
            .expect("decoded")
        )
        .expect("utf8"),
        "DSI Chargé de Formation Jean Dupont <jdupont@example.fr>"
    );
    // gh21664.phpt
    assert_eq!(
        i.mime_decode(b"=?utf-8?Q?\xA1?= .", 0, None).diagnostic,
        Some(Diagnostic::Notice(
            "Detected an illegal character in input string".into()
        ))
    );
    assert_eq!(
        i.mime_decode(b"=?UTF-8?X?a?=", 0, None).diagnostic,
        Some(Diagnostic::Warning("Malformed string".into()))
    );
    let h = i
        .mime_decode_headers(
            b"Subject: =?UTF-8?B?SGVsbG8=?=\r\nReceived: a\r\nReceived: b\r\n 1: x\r\n1: y\r\n1: z\r\n",
            0,
            None,
        )
        .value
        .expect("headers");
    assert_eq!(
        h,
        vec![
            (b"Subject".to_vec(), HeaderValue::Single(b"Hello".to_vec())),
            (
                b"Received".to_vec(),
                HeaderValue::Multiple(vec![b"a".to_vec(), b"b 1: x".to_vec()])
            ),
            (b"1".to_vec(), HeaderValue::Single(b"z".to_vec())),
        ]
    );
}

#[test]
fn base64_and_qp_helpers() {
    assert_eq!(mime::base64_encode(b"Hello"), b"SGVsbG8=");
    assert_eq!(mime::base64_encode(b""), b"");
    assert_eq!(mime::base64_decode(b"SGVs bG8=!"), b"Hello");
    assert_eq!(
        mime::quot_print_decode(b"a_b=3Dc", true),
        Some(b"a b=c".to_vec())
    );
    assert_eq!(mime::quot_print_decode(b"a=ZZ", true), None);
    assert_eq!(
        mime::quot_print_decode(b"soft=\r\nbreak", false),
        Some(b"softbreak".to_vec())
    );
}

#[test]
fn ob_handler_phpt() {
    // ob_iconv_handler.phpt: EUC-JP -> Shift_JIS.
    let mut i = ic();
    i.set_encoding(b"internal_encoding", b"EUC-JP");
    i.set_encoding(b"output_encoding", b"Shift_JIS");
    let euc = i
        .iconv(b"UTF-8", b"EUC-JP", "日本語テキスト".as_bytes())
        .value
        .expect("euc");
    let sjis = i
        .iconv(b"UTF-8", b"Shift_JIS", "日本語テキスト".as_bytes())
        .value
        .expect("sjis");
    let op = ObOp {
        start: true,
        final_: true,
        ..ObOp::default()
    };
    let sapi = SapiMime {
        mimetype: None,
        send_default_content_type: true,
        default_mimetype: None,
    };
    let r = i.ob_handler(&euc, op, &sapi);
    assert!(r.success);
    assert_eq!(r.output, sjis);
    assert_eq!(
        r.header.as_deref(),
        Some(&b"Content-Type:text/html; charset=Shift_JIS"[..])
    );
    assert_eq!(r.diagnostic, None);

    // gh7953.phpt: output_encoding with //suffix is cut in the header.
    i.ini.output_encoding = b"ISO-8859-1//TRANSLIT".to_vec();
    let sapi = SapiMime {
        mimetype: Some(b"text/plain; charset=x".to_vec()),
        ..SapiMime::default()
    };
    let r = i.ob_handler(
        b"",
        ObOp {
            start: true,
            clean: true,
            ..ObOp::default()
        },
        &sapi,
    );
    assert_eq!(
        r.header.as_deref(),
        Some(&b"Content-Type:text/plain; charset=ISO-8859-1"[..])
    );
    let r = i.ob_handler(
        b"x",
        ObOp {
            start: true,
            output_sent: true,
            ..ObOp::default()
        },
        &sapi,
    );
    assert!(!r.success);
    assert!(Iconv::ob_conflict(1, &["mb_output_handler"]));
    assert!(!Iconv::ob_conflict(0, &["ob_iconv_handler"]));
}

#[test]
fn stream_filter_basics() {
    php_stdmisc::locale::reset_lc_ctype();
    // iconv_stream_filter.phpt data: ISO-2022-JP -> EUC-JP.
    let mut f = IconvFilter::from_filter_name(b"convert.iconv.ISO-2022-JP/EUC-JP").expect("filter");
    let out = f
        .filter_buckets(&[b"\x1b$B$3$s$K$A$O\x1b(Biconv\n"], true)
        .expect("ok");
    assert_eq!(out, b"\xa4\xb3\xa4\xf3\xa4\xcb\xa4\xc1\xa4\xcficonv\n");
    assert!(IconvFilter::from_filter_name(b"convert.iconv.ISO-2022-JP.EUC-JP").is_some());
    // iconv_stream_filter_delimiter.phpt
    assert!(IconvFilter::from_filter_name(b"convert.iconv.ISO-2022-JP\0EUC-JP").is_none());
    assert!(IconvFilter::from_filter_name(b"convert.iconv.UTF-8").is_none());
    let long = [b"convert.iconv.".as_slice(), &[b'x'; 64], b"/UTF-8"].concat();
    assert!(IconvFilter::from_filter_name(&long).is_none());

    // Split multibyte sequence across buckets, then seek resets.
    let mut f = IconvFilter::new(b"UTF-8", b"UTF-16BE").expect("filter");
    assert_eq!(f.filter(b"a\xc3", false).expect("ok"), b"\0a");
    assert_eq!(f.filter(b"\xa9", true).expect("ok"), b"\0\xe9");
    assert_eq!(f.filter(b"\xc3", false).expect("ok"), b"");
    f.seek().expect("seek");
    assert_eq!(f.filter(b"b", true).expect("ok"), b"\0b");
    // gh17047.phpt
    let mut f = IconvFilter::new(b"UTF-16BE", b"UTF-16BE").expect("filter");
    assert_eq!(f.filter(b"te", false), Ok(b"te".to_vec()));
    let mut f = IconvFilter::new(b"UTF-8", b"ISO-8859-1").expect("filter");
    assert_eq!(
        f.filter("€".as_bytes(), true),
        Err("iconv stream filter (\"UTF-8\"=>\"ISO-8859-1\"): invalid multibyte sequence".into())
    );
}

#[test]
fn fallible_reserve_makes_room() {
    // Without the accounting allocator installed (test binaries), no
    // `memory_limit` is enforced: the reservation always succeeds.
    let mut v: Vec<u8> = Vec::new();
    assert!(crate::core::reserve(&mut v, 300).is_ok());
    assert!(v.capacity() >= 300);
    let mut w = vec![1u8; 10];
    assert!(crate::core::reserve_exact(&mut w, 5).is_ok());
    assert!(w.capacity() >= 15);
    assert!(!crate::core::oom_pending());
    // A large substring still converts in full (gh17399_substr's input).
    let s = vec![b'a'; 64 * 1024];
    let out = ic().substr(&s, 0, None, Some(b"UTF-8"));
    assert_eq!(out.value.as_ref().map(Vec::len), Some(s.len()));
}
