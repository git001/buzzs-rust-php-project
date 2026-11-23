// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The HTTP transport against canned server responses: request bytes,
//! body framing (Content-Length, chunked, close), status handling, cookies,
//! redirects and digest authentication.

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use common::{with_big_stack, TestHost};
use php_runtime::array::ArrayKey;
use php_runtime::{Value, ZArray};
use php_soap::host::SoapHost;

fn opts(pairs: &[(&str, Value)]) -> ZArray {
    ZArray::from_pairs(
        pairs
            .iter()
            .map(|(k, v)| (ArrayKey::from_bytes(k.as_bytes()), v.clone())),
    )
}

fn client(h: &mut TestHost, extra: &[(&str, Value)]) -> php_runtime::Obj {
    let mut o = vec![
        (
            "location",
            Value::from("http://example.com/soap/server.php"),
        ),
        ("uri", Value::from("urn:test")),
        ("trace", Value::Bool(true)),
    ];
    o.extend(extra.iter().cloned());
    h.new_client("SoapClient", None, &opts(&o)).expect("client")
}

fn do_request(h: &mut TestHost, c: &php_runtime::Obj, location: &str) -> Value {
    php_soap::client::do_request_method(
        h,
        c,
        b"<x/>",
        location.as_bytes(),
        b"urn:test#f",
        1,
        false,
        None,
    )
    .expect("do_request")
}

fn sent(h: &TestHost) -> Vec<String> {
    h.net
        .requests
        .borrow()
        .iter()
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect()
}

fn fault_string(h: &mut TestHost, c: &php_runtime::Obj) -> String {
    match h.read_slot(c, "__soap_fault") {
        Some(Value::Object(f)) => match h.read_slot(&f, "faultstring") {
            Some(Value::Str(s)) => s.to_string_lossy(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

#[test]
fn request_bytes_and_content_length_body() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net.responses.push(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: 5\r\n\r\nhelloEXTRA"
                .to_vec(),
        );
        let c = client(&mut h, &[]);
        let r = do_request(&mut h, &c, "http://example.com:8080/soap/server.php?x=1");
        assert!(matches!(r, Value::Str(ref s) if s.as_bytes() == b"hello"));
        let req = sent(&h);
        assert_eq!(
            req[0],
            format!(
                "POST /soap/server.php?x=1 HTTP/1.1\r\nHost: example.com:8080\r\nConnection: Keep-Alive\r\nUser-Agent: PHP-SOAP/{}\r\nContent-Type: text/xml; charset=utf-8\r\nSOAPAction: \"urn:test#f\"\r\nContent-Length: 4\r\n\r\n<x/>",
                php_soap::PHP_VERSION
            )
        );
        let target = h.net.targets.borrow()[0].clone();
        assert_eq!(
            (target.protocol.as_str(), target.host.as_str(), target.port),
            ("tcp", "example.com", 8080)
        );
        assert!(matches!(
            h.read_slot(&c, "__last_request_headers"),
            Some(Value::Str(_))
        ));
    });
}

#[test]
fn soap12_content_type_basic_auth_and_cookies() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net.responses.push(
            b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=abc; path=/soap; secure\r\nSet-Cookie: lang=en\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        h.net
            .responses
            .push(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec());
        let c = client(
            &mut h,
            &[
                ("login", Value::from("user")),
                ("password", Value::from("pass")),
                ("user_agent", Value::from("agent")),
                ("keep_alive", Value::Bool(false)),
            ],
        );
        let r = php_soap::client::do_request_method(
            &mut h,
            &c,
            b"<x/>",
            b"http://example.com/soap/a",
            b"act",
            2,
            false,
            None,
        )
        .expect("request");
        assert!(matches!(r, Value::Str(ref s) if s.is_empty()));
        let req = sent(&h);
        assert_eq!(
            req[0],
            "POST /soap/a HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\nUser-Agent: agent\r\nContent-Type: application/soap+xml; charset=utf-8; action=\"act\"\r\nContent-Length: 4\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n<x/>"
        );
        // Cookies were stored: secure one is only sent over https.
        let cookies = php_soap::client::get_cookies(&mut h, &c);
        let Value::Array(a) = cookies else {
            panic!("cookies")
        };
        assert_eq!(a.len(), 2);
        do_request(&mut h, &c, "http://example.com/soap/b");
        assert!(
            sent(&h)[1].contains("\r\nCookie: lang=en\r\n"),
            "{}",
            sent(&h)[1]
        );
    });
}

#[test]
fn chunked_and_close_delimited_bodies() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net
            .responses
            .push(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec());
        h.net
            .responses
            .push(b"HTTP/1.0 200 OK\r\n\r\nuntil close".to_vec());
        let c = client(&mut h, &[]);
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Str(ref s) if s.as_bytes() == b"hello world"));
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Str(ref s) if s.as_bytes() == b"until close"));
    });
}

#[test]
fn status_line_without_code_and_errors() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net
            .responses
            .push(b"HTTP/1.1\r\nContent-Type: text/xml\r\nContent-Length: 0\r\n\r\n".to_vec());
        h.net.responses.push(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/html\r\nContent-Length: 4\r\n\r\noops".to_vec());
        let c = client(&mut h, &[]);
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Str(ref s) if s.is_empty()));
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Null));
        assert_eq!(fault_string(&mut h, &c), "Internal Server Error");
        let r = do_request(&mut h, &c, "ftp://example.com/");
        assert!(matches!(r, Value::Null));
        assert_eq!(
            fault_string(&mut h, &c),
            "Unknown protocol. Only http and https are allowed."
        );
        let r = do_request(&mut h, &c, "test://");
        assert!(matches!(r, Value::Null));
        assert_eq!(fault_string(&mut h, &c), "Unable to parse URL");
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Null));
        assert_eq!(fault_string(&mut h, &c), "Could not connect to host");
    });
}

#[test]
fn redirects_follow_same_origin_only() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net.responses.push(
            b"HTTP/1.1 302 Found\r\nLocation: next.php\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        h.net
            .responses
            .push(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec());
        h.net.responses.push(
            b"HTTP/1.1 301 Moved\r\nLocation: http://evil.example/\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        );
        let c = client(&mut h, &[]);
        let r = do_request(&mut h, &c, "http://example.com/dir/first.php");
        assert!(matches!(r, Value::Str(ref s) if s.as_bytes() == b"ok"));
        assert!(sent(&h)[1].starts_with("POST /dir/next.php HTTP/1.1\r\n"));
        let r = do_request(&mut h, &c, "http://example.com/");
        assert!(matches!(r, Value::Null));
        assert_eq!(
            fault_string(&mut h, &c),
            "Redirection to a different origin is not allowed"
        );
    });
}

#[test]
fn digest_challenge_is_answered() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net.responses.push(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"r\", nonce=\"n\", qop=\"auth\", opaque=\"o\"\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        h.net
            .responses
            .push(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec());
        let c = client(
            &mut h,
            &[
                ("login", Value::from("u")),
                ("password", Value::from("p")),
                (
                    "authentication",
                    Value::Int(php_soap::consts::SOAP_AUTHENTICATION_DIGEST),
                ),
            ],
        );
        let r = do_request(&mut h, &c, "http://example.com/x");
        assert!(matches!(r, Value::Str(ref s) if s.as_bytes() == b"ok"));
        let second = &sent(&h)[1];
        assert!(second.contains("Authorization: Digest username=\"u\", realm=\"r\", nonce=\"n\", uri=\"/x\", qop=auth, nc=00000001, cnonce=\""), "{second}");
        assert!(second.contains("\", opaque=\"o\"\r\n"), "{second}");
    });
}

#[test]
fn header_injection_is_truncated_with_warning() {
    with_big_stack(|| {
        let mut h = TestHost::new();
        h.net
            .responses
            .push(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec());
        let c = client(
            &mut h,
            &[("user_agent", Value::from("ua\r\nX-Injected: 1"))],
        );
        do_request(&mut h, &c, "http://example.com/");
        assert!(sent(&h)[0].contains("User-Agent: ua\r\nContent-Type"));
        let out = String::from_utf8_lossy(&h.finish()).into_owned();
        assert!(out.contains("Warning: SoapClient::__doRequest(): Header User-Agent value contains newline characters and has been truncated"));
    });
}
