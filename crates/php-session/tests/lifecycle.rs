// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Session lifecycle behaviour ported from `ext/session/tests/*.phpt`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines)]

mod common;

use common::{s, show, tempdir, FakeHost};
use php_runtime::{Value, ZArray};
use php_session::api::*;
use php_session::{ini, SessionStatus};

fn files_host(tag: &str, extra: &[(&str, &str)]) -> (FakeHost, std::path::PathBuf) {
    let dir = tempdir(tag);
    let path = dir.to_string_lossy().into_owned();
    let mut ini: Vec<(&str, &str)> = vec![("session.use_strict_mode", "0")];
    ini.extend_from_slice(extra);
    let mut h = FakeHost::with_ini(&ini);
    h.call("ini_set", |h| {
        ini::ini_set(h, "session.save_path", path.as_bytes())
    });
    (h, dir)
}

fn start(h: &mut FakeHost) -> bool {
    h.call("session_start", |h| session_start(h, None)).unwrap()
}

#[test]
fn basic_files_round_trip() {
    let (mut h, dir) = files_host("basic", &[]);
    h.call("session_id", |h| session_id(h, Some(&s("session-basic1"))))
        .unwrap();
    assert!(start(&mut h));
    assert_eq!(h.state.status(), SessionStatus::Active);
    assert_eq!(
        h.headers,
        vec![
            "Set-Cookie: PHPSESSID=session-basic1; path=/; HttpOnly; SameSite=Lax",
            "Expires: Thu, 19 Nov 1981 08:52:00 GMT",
            "Cache-Control: no-store, no-cache, must-revalidate",
            "Pragma: no-cache",
        ]
    );
    // use_only_cookies=1: SID is empty.
    assert_eq!(h.sid_constant.as_deref(), Some(&b""[..]));
    h.set_var("foo", Value::Int(1));
    h.set_var("bar", Value::string("baz"));
    assert!(h
        .call("session_write_close", |h| session_write_close(h))
        .unwrap());
    let data = std::fs::read(dir.join("sess_session-basic1")).unwrap();
    assert_eq!(data, b"foo|i:1;bar|s:3:\"baz\";");

    // Reading back.
    assert!(start(&mut h));
    assert_eq!(h.session_len(), 2);
    let enc = h.call("session_encode", |h| session_encode(h)).unwrap();
    assert_eq!(
        enc.as_str().unwrap().as_bytes(),
        b"foo|i:1;bar|s:3:\"baz\";"
    );
    assert!(h.call("session_destroy", |h| session_destroy(h)).unwrap());
    assert!(!dir.join("sess_session-basic1").exists());
    assert!(h.take_log().is_empty());
}

#[test]
fn start_twice_notices() {
    let (mut h, _d) = files_host("twice", &[]);
    assert!(start(&mut h));
    assert!(start(&mut h));
    assert_eq!(
        h.take_log(),
        vec![
            "Notice: session_start(): Ignoring session_start() because a session is already \
              active (started from /t/test.php on line 3)"
        ]
    );
    assert!(h.call("session_destroy", |h| session_destroy(h)).unwrap());
    assert!(!h.call("session_destroy", |h| session_destroy(h)).unwrap());
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_destroy(): Trying to destroy uninitialized session"]
    );
}

#[test]
fn decode_prefixes() {
    // session_decode_error2.phpt
    let (mut h, _d) = files_host("decode", &[]);
    assert!(start(&mut h));
    let data = b"foo|a:3:{i:0;i:1;i:1;i:2;i:2;i:3;}guff|R:1;blah|R:1;";
    let mut ok_at = Vec::new();
    for i in 0..data.len() {
        if h.state.status() != SessionStatus::Active {
            start(&mut h);
        }
        let ok = h
            .call("session_decode", |h| session_decode(h, &data[..i]))
            .unwrap();
        let log = h.take_log();
        if ok {
            ok_at.push(i);
            assert!(log.is_empty());
            if i == 43 {
                // Both entries share one reference.
                let sess = h.session();
                let a = sess.as_array().unwrap();
                assert!(a.get_str(b"foo").unwrap().is_reference());
                assert!(a.get_str(b"guff").unwrap().is_reference());
            }
        } else {
            assert_eq!(
                log,
                vec![
                    "Warning: session_decode(): Failed to decode session object. Session has \
                      been destroyed"
                ]
            );
            assert_eq!(h.session_len(), 0);
        }
    }
    assert_eq!(ok_at, vec![0, 34, 43]);
}

#[test]
fn nul_byte_id_is_rejected() {
    // session_id_nul_byte.phpt
    let (mut h, _d) = files_host("nul", &[("session.use_cookies", "0")]);
    h.call("ini_set", |h| ini::ini_set(h, "session.save_path", b""));
    h.temp = tempdir("nul-tmp");
    h.call("session_id", |h| {
        session_id(h, Some(&s("nulbug\0stillvalid")))
    })
    .unwrap();
    assert!(!start(&mut h));
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_start(): Session ID is too long or contains illegal characters. \
             Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed",
            "Warning: session_start(): Failed to read session data: files (path: )",
        ]
    );
    let id = h.call("session_id", |h| session_id(h, None)).unwrap();
    assert_eq!(id.as_str().unwrap().as_bytes(), b"");
}

#[test]
fn ini_changes_rejected_while_active() {
    let (mut h, _d) = files_host("iniactive", &[]);
    assert!(start(&mut h));
    let r = h.call("ini_set", |h| {
        ini::ini_set(h, "session.serialize_handler", b"php_binary")
    });
    assert_eq!(r, Some(false));
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: ini_set(): Session ini settings cannot be changed when a session is \
              active (started from /t/test.php on line 3)"
        ]
    );
    // Non-session names are not ours.
    assert_eq!(ini::ini_set(&mut h, "precision", b"3"), None);
    h.call("session_write_close", |h| session_write_close(h))
        .unwrap();
    assert_eq!(
        h.call("ini_set", |h| ini::ini_set(
            h,
            "session.serialize_handler",
            b"php_binary"
        )),
        Some(true)
    );
    assert_eq!(
        ini::ini_get(&mut h, "session.serialize_handler").unwrap(),
        b"php_binary"
    );
}

#[test]
fn ini_validation_messages() {
    let mut h = FakeHost::with_ini(&[]);
    let cases: &[(&str, &[u8], &[&str])] = &[
        ("session.name", b"", &["Warning: ini_set(): session.name \"\" must not be numeric, empty, contain null bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\""]),
        ("session.name", b"123", &["Warning: ini_set(): session.name \"123\" must not be numeric, empty, contain null bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\""]),
        ("session.name", b"a.b", &["Warning: ini_set(): session.name \"a.b\" must not be numeric, empty, contain null bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\""]),
        ("session.cookie_lifetime", b"abc", &["Warning: ini_set(): session.cookie_lifetime must be of type int"]),
        ("session.cookie_lifetime", b"-1", &["Warning: ini_set(): session.cookie_lifetime must be between 0 and 9223372034707292159"]),
        ("session.cookie_lifetime", b"99999999999999999999", &["Warning: ini_set(): session.cookie_lifetime must be between 0 and 9223372034707292159"]),
        ("session.cookie_lifetime", b"1.5", &["Warning: ini_set(): session.cookie_lifetime must be of type int"]),
        ("session.cookie_samesite", b"bogus", &["Warning: ini_set(): session.cookie_samesite must be \"Strict\", \"Lax\", \"None\", or \"\""]),
        ("session.cookie_path", b"/a;b", &["Warning: ini_set(): \"session.cookie_path\" must not contain any of the following characters \";\\r\\n\""]),
        ("session.cookie_path", b"/path\0evil", &["Warning: ini_set(): \"session.cookie_path\" must not contain null bytes"]),
        ("session.cookie_domain", b"a b", &["Warning: ini_set(): \"session.cookie_domain\" must not contain any of the following characters \",; \\t\\r\\n\\013\\014\""]),
        ("session.cache_limiter", b"nocache\0evil", &["Warning: ini_set(): \"session.cache_limiter\" must not contain null bytes"]),
        ("session.save_path", b"/x\0y", &["Warning: ini_set(): \"session.save_path\" must not contain null bytes"]),
        ("session.referer_check", b"a\0", &["Warning: ini_set(): \"session.referer_check\" must not contain null bytes"]),
        ("session.serialize_handler", b"wrong_handler", &["Warning: ini_set(): Serialization handler \"wrong_handler\" cannot be found"]),
        ("session.save_handler", b"user", &["Warning: ini_set(): Session save handler \"user\" cannot be set by ini_set()"]),
        ("session.save_handler", b"nope", &["Warning: ini_set(): Session save handler \"nope\" cannot be found"]),
        ("session.sid_length", b"10", &["Deprecated: ini_set(): session.sid_length INI setting is deprecated", "Warning: ini_set(): session.configuration \"session.sid_length\" must be between 22 and 256"]),
        ("session.sid_bits_per_character", b"7", &["Deprecated: ini_set(): session.sid_bits_per_character INI setting is deprecated", "Warning: ini_set(): session.configuration \"session.sid_bits_per_character\" must be between 4 and 6"]),
        ("session.gc_divisor", b"0", &["Warning: ini_set(): session.gc_divisor must be greater than 0"]),
        ("session.gc_probability", b"-1", &["Warning: ini_set(): session.gc_probability must be greater than or equal to 0"]),
        ("session.auto_start", b"1", &[]),
        ("session.upload_progress.freq", b"5", &[]),
    ];
    for (name, value, expected) in cases {
        let r = h.call("ini_set", |h| ini::ini_set(h, name, value));
        assert_eq!(r, Some(false), "{name}={value:?}");
        assert_eq!(h.take_log(), *expected, "{name}");
    }
    let accepted: &[(&str, &[u8], &[&str])] = &[
        ("session.sid_length", b"40", &["Deprecated: ini_set(): session.sid_length INI setting is deprecated"]),
        ("session.sid_bits_per_character", b"5", &["Deprecated: ini_set(): session.sid_bits_per_character INI setting is deprecated"]),
        ("session.use_only_cookies", b"0", &["Deprecated: ini_set(): Disabling session.use_only_cookies INI setting is deprecated"]),
        ("session.use_trans_sid", b"1", &["Deprecated: ini_set(): Enabling session.use_trans_sid INI setting is deprecated"]),
        ("session.referer_check", b"example", &["Deprecated: ini_set(): Usage of session.referer_check INI setting is deprecated"]),
        ("session.cookie_samesite", b"strict", &[]),
        ("session.gc_maxlifetime", b"1k", &[]),
        ("session.name", b"MYSESS", &[]),
    ];
    for (name, value, expected) in accepted {
        let r = h.call("ini_set", |h| ini::ini_set(h, name, value));
        assert_eq!(r, Some(true), "{name}");
        assert_eq!(h.take_log(), *expected, "{name}");
    }
    assert_eq!(h.state.config.sid_length, 40);
    assert_eq!(h.state.config.gc_maxlifetime, 1024);
    assert_eq!(h.state.config.session_name.as_bytes(), b"MYSESS");
    // ini_restore and deactivation.
    ini::ini_restore(&mut h, "session.name");
    assert_eq!(h.state.config.session_name.as_bytes(), b"PHPSESSID");
    ini::deactivate(&mut h);
    assert_eq!(h.state.config.sid_length, 32);
    assert!(h.state.config.use_only_cookies);
    assert!(h.take_log().is_empty());
}

#[test]
fn startup_values_and_fallbacks() {
    let mut h = FakeHost::with_ini(&[
        ("session.name", ""),
        ("session.use_trans_sid", "1"),
        ("session.save_handler", "blah"),
        ("session.upload_progress.freq", "200%"),
    ]);
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: PHP Startup: session.name \"\" must not be numeric, empty, contain null \
             bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\"",
            "Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated",
            "Warning: PHP Startup: session.upload_progress.freq must be less than or equal to 100%",
        ]
    );
    assert_eq!(h.state.config.session_name.as_bytes(), b"PHPSESSID");
    // Unknown save handler at startup: the session is disabled.
    assert_eq!(session_status(&mut h), 0);
    assert!(!start(&mut h));
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_start(): Cannot find session save handler \"blah\" - session \
              startup failed"
        ]
    );
}

#[test]
fn start_options() {
    // session_start_error.phpt / session_start_read_and_close.phpt
    let (mut h, dir) = files_host("options", &[]);
    let mut opts = ZArray::new();
    opts.make_mut().insert(
        "option",
        Value::Object(php_runtime::PlainObject::create("stdClass", ZArray::new())),
    );
    let e = h
        .call("session_start", |h| session_start(h, Some(&opts)))
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "TypeError: session_start(): Option \"option\" must be of type string|int|bool, stdClass given"
    );
    let opts = ZArray::from_values([Value::Int(1)]);
    let e = h
        .call("session_start", |h| session_start(h, Some(&opts)))
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "ValueError: session_start(): Argument #1 ($options) must be of type array with keys as string"
    );
    for (v, msg) in [
        (Value::string("false"), "TypeError: session_start(): Option \"read_and_close\" value must be of type compatible with int, \"false\" given"),
        (Value::Float(1.0), "TypeError: session_start(): Option \"read_and_close\" must be of type string|int|bool, float given"),
    ] {
        let mut opts = ZArray::new();
        opts.make_mut().insert("read_and_close", v);
        let e = h.call("session_start", |h| session_start(h, Some(&opts))).unwrap_err();
        assert_eq!(e.to_string(), msg);
    }
    let mut opts = ZArray::new();
    opts.make_mut().insert("option", Value::Int(1));
    opts.make_mut().insert("gc_divisor", Value::Int(0));
    opts.make_mut().insert("read_and_close", Value::string("1"));
    h.call("session_id", |h| session_id(h, Some(&s("rac"))))
        .unwrap();
    std::fs::write(dir.join("sess_rac"), b"a|i:5;").unwrap();
    assert!(h
        .call("session_start", |h| session_start(h, Some(&opts)))
        .unwrap());
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_start(): Setting option \"option\" failed",
            "Warning: session_start(): session.gc_divisor must be greater than 0",
            "Warning: session_start(): Setting option \"gc_divisor\" failed",
        ]
    );
    // read_and_close: data is loaded, the session is closed again.
    assert_eq!(h.state.status(), SessionStatus::None);
    assert_eq!(h.session_len(), 1);
}

#[test]
fn headers_sent_blocks_start() {
    let (mut h, _d) = files_host("hsent", &[]);
    h.headers_sent = true;
    h.output_start = Some(("/t/out.php".into(), 7));
    assert!(!start(&mut h));
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_start(): Session cannot be started after headers have already \
              been sent (sent from /t/out.php on line 7)"
        ]
    );
    assert_eq!(
        show(
            &h.call("session_name", |h| session_name(h, Some(&s("x"))))
                .unwrap()
        ),
        "bool(false)"
    );
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_name(): Session name cannot be changed after headers have \
              already been sent (sent from /t/out.php on line 7)"
        ]
    );
    assert_eq!(
        h.call("ini_set", |h| ini::ini_set(h, "session.gc_divisor", b"10")),
        Some(false)
    );
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: ini_set(): Session ini settings cannot be changed after headers have \
              already been sent (sent from /t/out.php on line 7)"
        ]
    );
    // session_id() only cares when cookies are used.
    h.call("ini_set", |h| ini::ini_set(h, "session.use_cookies", b"0"));
    h.take_log();
    h.headers_sent = false;
    h.call("ini_set", |h| ini::ini_set(h, "session.use_cookies", b"0"));
    h.headers_sent = true;
    assert!(matches!(
        h.call("session_id", |h| session_id(h, Some(&s("abc"))))
            .unwrap(),
        Value::Str(_)
    ));
    assert!(h.take_log().is_empty());
}

#[test]
fn cookie_params() {
    // session_set_cookie_params_variation7.phpt
    let mut h = FakeHost::with_ini(&[]);
    let call = |h: &mut FakeHost, v: Value| {
        h.call("session_set_cookie_params", |h| {
            session_set_cookie_params(h, &v, None, None, None, None)
        })
    };
    let empty = Value::Array(ZArray::new());
    assert_eq!(
        call(&mut h, empty).unwrap_err().to_string(),
        "ValueError: session_set_cookie_params(): Argument #1 ($lifetime_or_options) must contain at least 1 valid key"
    );
    let mut opts = ZArray::new();
    opts.make_mut().insert("unknown_key", Value::Bool(true));
    opts.make_mut().insert("secure_invalid", Value::Bool(true));
    assert!(call(&mut h, Value::Array(opts)).is_err());
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_set_cookie_params(): Argument #1 ($lifetime_or_options) contains an unrecognized key \"unknown_key\"",
            "Warning: session_set_cookie_params(): Argument #1 ($lifetime_or_options) contains an unrecognized key \"secure_invalid\"",
        ]
    );
    let mut opts = ZArray::new();
    opts.make_mut().insert("lifetime", Value::Int(42));
    let e = h
        .call("session_set_cookie_params", |h| {
            session_set_cookie_params(
                h,
                &Value::Array(opts.clone()),
                Some(&s("/")),
                None,
                None,
                None,
            )
        })
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "ValueError: session_set_cookie_params(): Argument #2 ($path) must be null when argument #1 ($lifetime_or_options) is an array"
    );
    let mut opts = ZArray::new();
    opts.make_mut().insert("lifetime", Value::Int(42));
    opts.make_mut().insert("path", Value::string("/foo"));
    opts.make_mut().insert("domain", Value::string("foo.bar"));
    opts.make_mut().insert("secure", Value::Bool(true));
    opts.make_mut().insert("samesite", Value::string("None"));
    opts.make_mut().insert("partitioned", Value::Int(1));
    assert!(call(&mut h, Value::Array(opts)).unwrap());
    let p = session_get_cookie_params(&mut h);
    let keys: Vec<String> = p
        .keys()
        .map(|k| String::from_utf8_lossy(&k.to_bytes()).into_owned())
        .collect();
    assert_eq!(
        keys,
        [
            "lifetime",
            "path",
            "domain",
            "secure",
            "partitioned",
            "httponly",
            "samesite"
        ]
    );
    assert!(matches!(p.get_str(b"lifetime"), Some(Value::Int(42))));
    assert!(matches!(p.get_str(b"partitioned"), Some(Value::Bool(true))));
    // Positional form.
    assert!(h
        .call("session_set_cookie_params", |h| {
            session_set_cookie_params(
                h,
                &Value::Int(3600),
                Some(&s("/p")),
                None,
                Some(false),
                Some(false),
            )
        })
        .unwrap());
    assert_eq!(h.state.config.cookie_lifetime, 3600);
    assert!(!h.state.config.cookie_httponly);
    // Rejected values stop processing.
    assert!(!h
        .call("session_set_cookie_params", |h| {
            session_set_cookie_params(h, &Value::Int(-1), None, None, None, None)
        })
        .unwrap());
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_set_cookie_params(): session.cookie_lifetime must be between 0 and 9223372034707292159"]
    );
    h.call("ini_set", |h| ini::ini_set(h, "session.use_cookies", b"0"));
    assert!(!call(&mut h, Value::Int(1)).unwrap());
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_set_cookie_params(): Session cookies cannot be used when session.use_cookies is disabled"]
    );
}

#[test]
fn cookie_attributes_and_partitioned() {
    // session_start_partitioned_headers.phpt / bug80774.phpt
    let (mut h, _d) = files_host("partitioned", &[]);
    h.call("session_id", |h| session_id(h, Some(&s("12345"))))
        .unwrap();
    let mut opts = ZArray::new();
    opts.make_mut().insert("partitioned", Value::Bool(true));
    h.call("session_set_cookie_params", |h| {
        session_set_cookie_params(h, &Value::Array(opts.clone()), None, None, None, None)
    })
    .unwrap();
    assert!(!start(&mut h));
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_start(): Partitioned session cookie cannot be used without also configuring it as secure"]
    );
    opts.make_mut().insert("secure", Value::Bool(true));
    h.call("session_set_cookie_params", |h| {
        session_set_cookie_params(h, &Value::Array(opts), None, None, None, None)
    })
    .unwrap();
    h.call("session_id", |h| session_id(h, Some(&s("12345"))))
        .unwrap();
    assert!(start(&mut h));
    assert_eq!(
        h.headers[0],
        "Set-Cookie: PHPSESSID=12345; path=/; secure; Partitioned; HttpOnly; SameSite=Lax"
    );
    h.call("session_abort", |h| session_abort(h)).unwrap();
    h.headers.clear();
    h.call("session_name", |h| session_name(h, Some(&s("foo\\bar"))))
        .unwrap();
    h.call("session_id", |h| session_id(h, Some(&s("12345"))))
        .unwrap();
    let mut opts = ZArray::new();
    opts.make_mut().insert("secure", Value::Bool(false));
    opts.make_mut().insert("partitioned", Value::Bool(false));
    opts.make_mut().insert("lifetime", Value::Int(60));
    h.call("session_set_cookie_params", |h| {
        session_set_cookie_params(h, &Value::Array(opts), None, None, None, None)
    })
    .unwrap();
    assert!(start(&mut h));
    assert_eq!(
        h.headers[0],
        "Set-Cookie: foo\\bar=12345; expires=Tue, 14 Nov 2023 22:14:20 GMT; Max-Age=60; path=/; HttpOnly; SameSite=Lax"
    );
}

#[test]
fn cache_limiters() {
    let (mut h, dir) = files_host("cache", &[]);
    let script = dir.join("script.php");
    std::fs::write(&script, b"<?php").unwrap();
    let f = std::fs::File::options().write(true).open(&script).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000))
        .unwrap();
    h.script = Some(script);
    for (limiter, expected) in [
        (
            "public",
            vec![
                "Expires: Tue, 14 Nov 2023 22:14:20 GMT",
                "Cache-Control: public, max-age=60",
                "Last-Modified: Sun, 13 Sep 2020 12:26:40 GMT",
            ],
        ),
        (
            "private",
            vec![
                "Expires: Thu, 19 Nov 1981 08:52:00 GMT",
                "Cache-Control: private, max-age=60",
                "Last-Modified: Sun, 13 Sep 2020 12:26:40 GMT",
            ],
        ),
        (
            "PRIVATE_no_expire",
            vec![
                "Cache-Control: private, max-age=60",
                "Last-Modified: Sun, 13 Sep 2020 12:26:40 GMT",
            ],
        ),
        ("", vec![]),
        ("unknown", vec![]),
    ] {
        h.headers.clear();
        let old = h
            .call("session_cache_limiter", |h| {
                session_cache_limiter(h, Some(&s(limiter)))
            })
            .unwrap();
        assert!(matches!(old, Value::Str(_)));
        assert_eq!(
            show(
                &h.call("session_cache_expire", |h| session_cache_expire(h, Some(2)))
                    .unwrap()
            ),
            "int(180)"
        );
        h.call("session_cache_expire", |h| session_cache_expire(h, Some(1)))
            .unwrap();
        assert!(start(&mut h));
        let got: Vec<&str> = h.headers.iter().skip(1).map(String::as_str).collect();
        assert_eq!(got, expected, "{limiter}");
        assert_eq!(
            show(
                &h.call("session_cache_limiter", |h| session_cache_limiter(
                    h,
                    Some(&s("nocache"))
                ))
                .unwrap()
            ),
            "bool(false)"
        );
        assert_eq!(
            show(
                &h.call("session_cache_expire", |h| session_cache_expire(
                    h,
                    Some(10)
                ))
                .unwrap()
            ),
            "int(1)"
        );
        assert_eq!(
            h.take_log(),
            vec![
                "Warning: session_cache_limiter(): Session cache limiter cannot be changed when a session is active (started from /t/test.php on line 3)",
                "Warning: session_cache_expire(): Session cache expiration cannot be changed when a session is active (started from /t/test.php on line 3)",
            ]
        );
        h.call("session_abort", |h| session_abort(h)).unwrap();
        ini::ini_restore(&mut h, "session.cache_expire");
    }
}

#[test]
fn reset_restores_saved_data() {
    // session_reset_basic.phpt
    let (mut h, _d) = files_host("reset", &[]);
    assert!(start(&mut h));
    let id = h.state.id().unwrap().clone();
    h.set_var("foo", Value::Int(123));
    h.call("session_commit", |h| session_commit(h)).unwrap();
    h.call("session_id", |h| session_id(h, Some(&id))).unwrap();
    assert!(start(&mut h));
    h.set_var("bar", Value::Int(456));
    assert_eq!(h.session_len(), 2);
    assert!(h.call("session_reset", |h| session_reset(h)).unwrap());
    assert_eq!(h.session_len(), 1);
    assert!(h.session().as_array().unwrap().get_str(b"foo").is_some());
    // Without an active session these are no-ops.
    h.call("session_abort", |h| session_abort(h)).unwrap();
    assert!(!h.call("session_reset", |h| session_reset(h)).unwrap());
    assert!(!h.call("session_abort", |h| session_abort(h)).unwrap());
    assert!(!h.call("session_unset", |h| session_unset(h)).unwrap());
    assert!(!h
        .call("session_write_close", |h| session_write_close(h))
        .unwrap());
    assert!(h.take_log().is_empty());
}

#[test]
fn encode_failure_releases_lock() {
    // session_write_close_encode_failure.phpt
    let (mut h, dir) = files_host("encfail", &[("session.use_cookies", "0")]);
    assert!(start(&mut h));
    let file = dir.join(format!("sess_{}", h.state.id().unwrap().to_string_lossy()));
    h.set_var("bad|key", Value::string("value"));
    assert!(h
        .call("session_write_close", |h| session_write_close(h))
        .unwrap());
    let f = std::fs::File::open(&file).unwrap();
    assert!(f.try_lock().is_ok());
    drop(f);
    assert!(start(&mut h));
    h.call("session_unset", |h| session_unset(h)).unwrap();
    h.set_var(
        "closure",
        Value::Object(php_runtime::PlainObject::create("Closure", ZArray::new())),
    );
    h.set_var("bad|key", Value::string("value"));
    let e = h
        .call("session_write_close", |h| session_write_close(h))
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Exception: Serialization of 'Closure' is not allowed"
    );
    let f = std::fs::File::open(&file).unwrap();
    assert!(f.try_lock().is_ok());
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_write_close(): Failed to write session data. Data contains invalid key \"bad|key\"",
            "Warning: session_write_close(): Failed to write session data. Data contains invalid key \"bad|key\"",
        ]
    );
    assert_eq!(h.state.status(), SessionStatus::None);
}

#[test]
fn encode_without_session() {
    let mut h = FakeHost::with_ini(&[]);
    assert_eq!(
        show(&h.call("session_encode", |h| session_encode(h)).unwrap()),
        "bool(false)"
    );
    assert!(!h
        .call("session_decode", |h| session_decode(h, b"a|i:1;"))
        .unwrap());
    assert_eq!(
        h.take_log(),
        vec![
            "Warning: session_encode(): Cannot encode non-existent session",
            "Warning: session_decode(): Session data cannot be decoded when there is no active session",
        ]
    );
}

#[test]
fn ids_from_get_post_and_referer() {
    let (mut h, _d) = files_host("get", &[("session.use_only_cookies", "0")]);
    h.take_log();
    h.set_global("_GET", &[("PHPSESSID", Value::string("fromget"))]);
    h.set_global("_POST", &[("PHPSESSID", Value::string("frompost"))]);
    assert!(start(&mut h));
    assert_eq!(h.state.id().unwrap().as_bytes(), b"fromget");
    // A non-cookie ID: SID is defined and no cookie is re-sent? (send_cookie stays false)
    assert_eq!(h.sid_constant.as_deref(), Some(&b"PHPSESSID=fromget"[..]));
    assert!(!h.headers.iter().any(|l| l.starts_with("Set-Cookie")));
    h.call("session_abort", |h| session_abort(h)).unwrap();

    // Referer check rejects foreign referers.
    let (mut h, _d) = files_host(
        "referer",
        &[
            ("session.use_only_cookies", "0"),
            ("session.referer_check", "example.com"),
        ],
    );
    h.take_log();
    h.set_global("_GET", &[("PHPSESSID", Value::string("fromget"))]);
    h.server
        .insert("HTTP_REFERER".into(), Value::string("http://evil.test/"));
    assert!(start(&mut h));
    assert_ne!(h.state.id().unwrap().as_bytes(), b"fromget");
    // Dangerous characters drop the ID.
    let (mut h, _d) = files_host("danger", &[]);
    h.set_global("_COOKIE", &[("PHPSESSID", Value::string("a<b"))]);
    assert!(start(&mut h));
    assert_eq!(h.state.id().unwrap().len(), 32);
    // A non-string cookie forces a new ID and cookie.
    let (mut h, _d) = files_host("arrcookie", &[]);
    h.set_global(
        "_COOKIE",
        &[(
            "PHPSESSID",
            Value::Array(ZArray::from_values([Value::Int(1)])),
        )],
    );
    assert!(start(&mut h));
    assert!(h.headers[0].starts_with("Set-Cookie: PHPSESSID="));
}

#[test]
fn module_name_and_save_path() {
    let mut h = FakeHost::with_ini(&[]);
    assert_eq!(
        show(
            &h.call("session_module_name", |h| session_module_name(h, None))
                .unwrap()
        ),
        "string(5) \"files\""
    );
    assert_eq!(
        show(
            &h.call("session_module_name", |h| session_module_name(
                h,
                Some(&s("blah"))
            ))
            .unwrap()
        ),
        "bool(false)"
    );
    assert_eq!(
        h.call("session_module_name", |h| session_module_name(
            h,
            Some(&s("user"))
        ))
        .unwrap_err()
        .to_string(),
        "ValueError: session_module_name(): Argument #1 ($module) must not be \"user\""
    );
    assert_eq!(
        h.call("session_module_name", |h| session_module_name(
            h,
            Some(&s("a\0b"))
        ))
        .unwrap_err()
        .to_string(),
        "ValueError: session_module_name(): Argument #1 ($module) must not contain any null bytes"
    );
    assert_eq!(
        show(
            &h.call("session_module_name", |h| session_module_name(
                h,
                Some(&s("FILES"))
            ))
            .unwrap()
        ),
        "string(5) \"files\""
    );
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_module_name(): Session handler module \"blah\" cannot be found"]
    );
    assert_eq!(
        show(
            &h.call("session_save_path", |h| session_save_path(
                h,
                Some(&s("/x"))
            ))
            .unwrap()
        ),
        "string(0) \"\""
    );
    assert_eq!(
        show(
            &h.call("session_save_path", |h| session_save_path(h, None))
                .unwrap()
        ),
        "string(2) \"/x\""
    );
    assert_eq!(
        h.call("session_name", |h| session_name(h, Some(&s("AB\0CD"))))
            .unwrap_err()
            .to_string(),
        "ValueError: session_name(): Argument #1 ($name) must not contain any null bytes"
    );
    assert_eq!(
        show(
            &h.call("session_name", |h| session_name(h, Some(&s("\t"))))
                .unwrap()
        ),
        "string(9) \"PHPSESSID\""
    );
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_name(): session.name \"\t\" must not be numeric, empty, contain null bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\""]
    );
}

#[test]
fn create_id_prefixes() {
    // session_create_id_invalid_prefix.phpt / session_create_id_basic.phpt
    let mut h = FakeHost::with_ini(&[]);
    let id = h
        .call("session_create_id", |h| session_create_id(h, None))
        .unwrap();
    assert_eq!(id.as_str().unwrap().len(), 32);
    let id = h
        .call("session_create_id", |h| {
            session_create_id(h, Some(&s("abc-")))
        })
        .unwrap();
    assert!(id.as_str().unwrap().as_bytes().starts_with(b"abc-"));
    assert_eq!(
        show(
            &h.call("session_create_id", |h| session_create_id(h, Some(&s("_"))))
                .unwrap()
        ),
        "bool(false)"
    );
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_create_id(): Prefix cannot contain special characters. Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed"]
    );
    let long = "a".repeat(257);
    assert_eq!(
        h.call("session_create_id", |h| session_create_id(h, Some(&s(&long)))).unwrap_err().to_string(),
        "ValueError: session_create_id(): Argument #1 ($prefix) cannot be longer than 256 characters"
    );
    assert_eq!(
        h.call("session_create_id", |h| session_create_id(
            h,
            Some(&s("\0"))
        ))
        .unwrap_err()
        .to_string(),
        "ValueError: session_create_id(): Argument #1 ($prefix) must not contain any null bytes"
    );
    // sid_bits_per_character / sid_length shape the ID.
    h.call("ini_set", |h| {
        ini::ini_set(h, "session.sid_bits_per_character", b"6")
    });
    h.call("ini_set", |h| ini::ini_set(h, "session.sid_length", b"40"));
    let id = h
        .call("session_create_id", |h| session_create_id(h, None))
        .unwrap();
    let id = id.as_str().unwrap().clone();
    assert_eq!(id.len(), 40);
    assert!(id
        .as_bytes()
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b',' || *c == b'-'));
}

#[test]
fn status_and_disabled_serializer() {
    // session_status_disabled.phpt-like: a bogus serializer at startup.
    let mut h = FakeHost::with_ini(&[("session.serialize_handler", "blah")]);
    assert_eq!(session_status(&mut h), 0);
    assert!(!start(&mut h));
    assert_eq!(
        h.take_log(),
        vec!["Warning: session_start(): Cannot find session serialization handler \"blah\" - session startup failed"]
    );
    let (mut h, _d) = files_host("status", &[]);
    assert_eq!(session_status(&mut h), 1);
    start(&mut h);
    assert_eq!(session_status(&mut h), 2);
}

#[test]
fn auto_start_messages() {
    let dir = tempdir("auto");
    let path = dir.to_string_lossy().into_owned();
    let mut h = FakeHost::with_ini(&[("session.auto_start", "1"), ("session.save_path", &path)]);
    assert_eq!(h.state.status(), SessionStatus::Active);
    h.state.config.auto_start = true;
    // Started during request startup: no file/line is known.
    let mut fresh = FakeHost::new();
    fresh.location = None;
    let map: std::collections::HashMap<String, Vec<u8>> = [
        ("session.auto_start", "1"),
        ("session.save_path", path.as_str()),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
    .collect();
    php_session::module_startup(&mut fresh, &|n| map.get(n).cloned());
    php_session::request_startup(&mut fresh).unwrap();
    fresh.location = Some(("/t/test.php".into(), 3));
    assert!(fresh
        .call("session_start", |h| session_start(h, None))
        .unwrap());
    let args = [Value::Null];
    let _ = args;
    assert_eq!(
        fresh.take_log(),
        vec!["Notice: session_start(): Ignoring session_start() because a session is already active (session started automatically)"]
    );
    php_session::request_shutdown(&mut fresh).unwrap();
}
