// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.

use std::borrow::Cow;
use std::time::Duration;

use super::*;
use crate::constants;
use crate::event::{Level, LogKind};

/// 2011-07-01 15:53:49 UTC, FPM's documentation date.
const AT: i64 = 1_309_535_629;

struct Req;

impl RequestFields for Req {
    fn cpu_percent(&self, part: CpuPart) -> Option<f64> {
        Some(match part {
            CpuPart::User => 1.5,
            CpuPart::System => 0.25,
            CpuPart::Total => 1.75,
        })
    }
    fn duration(&self) -> Option<Duration> {
        Some(Duration::from_micros(1_250_261))
    }
    fn env(&self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
        (name == b"HTTP_HOST").then_some(Cow::Borrowed(b"www.foo.bar"))
    }
    fn script_filename(&self) -> &[u8] {
        b"/srv/test_mem.php"
    }
    fn content_length(&self) -> u64 {
        42
    }
    fn method(&self) -> &[u8] {
        b"GET"
    }
    fn memory_peak(&self) -> u64 {
        3 * 1024 * 1024 + 5
    }
    fn response_header(&self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
        (name == b"Content-Type").then_some(Cow::Borrowed(b"text/html"))
    }
    fn request_header(&self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
        (name == b"User-Agent").then_some(Cow::Borrowed(b"curl/8"))
    }
    fn query_string(&self) -> &[u8] {
        b"N=10000"
    }
    fn request_uri(&self) -> &[u8] {
        b"/test_mem.php"
    }
    fn remote_addr(&self) -> Option<&[u8]> {
        Some(b"192.0.2.7")
    }
    fn status(&self) -> i32 {
        200
    }
    fn started(&self) -> Option<i64> {
        Some(AT)
    }
    fn trace_id(&self) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(b"4bf92f3577b34da6a3ce929d0e0e4736"))
    }
}

fn event(message: &'static [u8]) -> LogEvent<'static> {
    LogEvent {
        kind: LogKind::Error,
        level: Some(Level::Warning),
        time: AT + 2,
        message: Some(message),
        attrs: &[("file", Attr::Bytes(b"/srv/a.php")), ("line", Attr::Int(7))],
    }
}

fn render(format: &str, request: Option<&dyn RequestFields>) -> String {
    let f = Format::parse(format.as_bytes(), "access.format").expect("valid");
    String::from_utf8(f.render(&event(b"boom"), request, None, None).line).expect("utf-8")
}

fn error(format: &str) -> String {
    Format::parse(format.as_bytes(), "access.format")
        .expect_err("invalid")
        .to_string()
}

#[test]
fn fpm_documentation_format() {
    assert_eq!(
        render(
            r#"%R - %u %t "%m %r%Q%q" %s %f %{milli}d %{kilo}M %C%%"#,
            Some(&Req)
        ),
        r#"192.0.2.7 -  01/Jul/2011:15:53:49 +0000 "GET /test_mem.php?N=10000" 200 /srv/test_mem.php 1250.261 3072 1.75%"#
    );
}

#[test]
fn every_fpm_letter_and_modifier() {
    let r = |f| render(f, Some(&Req));
    assert_eq!(r("%{user}C %{SYSTEM}C %{total}C"), "1.50 0.25 1.75");
    assert_eq!(
        r("%d %{seconds}d %{mili}d %{micro}d %{MicroSeconds}d"),
        "1.250 1.250 1250.261 1250261 1250261"
    );
    assert_eq!(
        r("%{minutes}d %{m}d %{HOURS}d %{d}d"),
        "0.021 0.021 0.000 0.000"
    );
    assert_eq!(r("%{ms}d %{us}d %{s}d"), "1250.261 1250261 1.250");
    assert_eq!(r("%{HTTP_HOST}e %{NOPE}e"), "www.foo.bar -");
    assert_eq!(r("%l %m %n"), "42 GET -");
    assert_eq!(
        r("%M %{bytes}M %{kilobytes}M %{mega}M"),
        "3145733 3145733 3072 3"
    );
    assert_eq!(r("%{Content-Type}o %{X-Nope}o"), "text/html -");
    assert_eq!(r("%{%Y-%m-%dT%H:%M:%S%z}t"), "2011-07-01T15:53:49+0000");
    assert_eq!(r("%T"), "01/Jul/2011:15:53:51 +0000");
    assert_eq!(r("%p"), std::process::id().to_string());
    assert_eq!(r("%P"), std::os::unix::process::parent_id().to_string());
    assert_eq!(r("100%% %"), "100% ");
}

#[test]
fn extensions() {
    let r = |f| render(f, Some(&Req));
    assert_eq!(
        r("%{level}L %{kind}L %{file}L:%{line}L %{type}L %{tz}L %{message}L"),
        "warning error /srv/a.php:7 - UTC boom"
    );
    assert_eq!(r("%{User-Agent}i %{Accept}i"), "curl/8 -");
    assert_eq!(
        r("%{trace_id}L %{span_id}L %{request_id}L"),
        "4bf92f3577b34da6a3ce929d0e0e4736 - -"
    );
}

#[test]
fn without_a_request() {
    assert_eq!(
        render("%R %u|%t %m %f %d %C %q%Q%r %s %l %M", None),
        "- |- - - - -  0 0 0"
    );
}

#[test]
fn named_formats() {
    assert_eq!(
        render("${PHP_ERROR_LOG_FMT}", None),
        "[01-Jul-2011 15:53:51 UTC] boom"
    );
    assert_eq!(render("${PHP_SAPI_LOG_FMT}", None), "boom");
    assert_eq!(
        render("${FPM_ACCESS_LOG_FMT} %{trace_id}L", Some(&Req)),
        r#"192.0.2.7 -  01/Jul/2011:15:53:49 +0000 "GET /test_mem.php" 200 4bf92f3577b34da6a3ce929d0e0e4736"#
    );
    assert_eq!(error("${NO_SUCH_FMT}"), "unknown log format ${NO_SUCH_FMT}");
    assert!(constants::lookup(b"FPM_ACCESS_LOG_FMT").is_some());
    assert_eq!(render("${unterminated", None), "${unterminated");
}

#[test]
fn zone_and_locale() {
    let vienna = php_date::tz::get("Europe/Vienna").expect("zone");
    let f = Format::parse(b"${PHP_ERROR_LOG_FMT} %t", "error_log_format").expect("valid");
    let line = f.render(&event(b"x"), Some(&Req), Some(&vienna), None).line;
    assert_eq!(
        String::from_utf8(line).expect("utf-8"),
        "[01-Jul-2011 17:53:51 Europe/Vienna] x 01/Jul/2011:17:53:49 +0200"
    );
}

#[test]
fn fpm_errors() {
    assert_eq!(error("%x"), "Invalid token in the access.format (%x)");
    assert_eq!(
        error("%{abc"),
        "missing closing embrace in the access.format"
    );
    assert_eq!(
        error("%{abc}"),
        "missing closing embrace in the access.format"
    );
    assert_eq!(error("%{abc}f"), "embrace is not allowed for modifier %f");
    assert_eq!(error("%{abc}%"), "embrace is not allowed for modifier %%");
    assert_eq!(
        error("%{x}C"),
        "only 'total', 'user' or 'system' are allowed as a modifier for %C ('x')"
    );
    assert_eq!(
        error("%{x}d"),
        "only 'seconds', 'milli', 'milliseconds', 'micro' or 'microseconds' are allowed as a modifier for %d ('x')"
    );
    assert_eq!(
        error("%{x}M"),
        "only 'bytes', 'kilo', 'kilobytes', 'mega' or 'megabytes' are allowed as a modifier for %M ('x')"
    );
    assert_eq!(
        error("%e"),
        "the name of the environment variable must be set between embraces for %e"
    );
    assert_eq!(
        error("%o"),
        "the name of the header must be set between embraces for %o"
    );
    assert_eq!(
        error("%{x}L"),
        "only 'message', 'level', 'type', 'file', 'line', 'kind', 'tz', 'request_id', 'trace_id' or 'span_id' are allowed as a modifier for %L ('x')"
    );
    let long = format!("%{{{}}}e", "A".repeat(200));
    let f = Format::parse(long.as_bytes(), "access.format").expect("valid");
    assert_eq!(
        f.tokens,
        vec![Token::Field(Field::Env(vec![b'A'; 128]))],
        "cut to 128 bytes"
    );
}

#[test]
fn the_default_zone_is_the_threads() {
    // ext/date sets it (`date_default_timezone_set()`, `date.timezone`).
    std::thread::spawn(|| {
        php_date::tz::set_default_override(php_date::tz::get("Asia/Tokyo"));
        assert_eq!(
            render("%{tz}L %T", None),
            "Asia/Tokyo 02/Jul/2011:00:53:51 +0900"
        );
        php_date::tz::set_default_override(None);
        assert_eq!(render("%{tz}L", None), "UTC");
    })
    .join()
    .expect("thread");
}

#[test]
fn limit_cuts_the_line() {
    let f = Format::parse(b"%{message}L", "error_log_format").expect("valid");
    let r = f.render(&event(b"0123456789"), None, None, Some(4));
    assert_eq!((r.line.as_slice(), r.truncated), (&b"0123"[..], true));
    let r = f.render(&event(b"0123"), None, None, Some(4));
    assert!(!r.truncated);
}

/// One request of `buzz-check/fpmlog/run.sh` as php-fpm 8.6 saw it.
#[derive(Default)]
struct Fpm86 {
    user: &'static [u8],
    method: &'static [u8],
    uri: &'static [u8],
    query: &'static [u8],
    status: i32,
    script: &'static [u8],
    length: u64,
    host: Option<&'static [u8]>,
    protocol: Option<&'static [u8]>,
}

impl RequestFields for Fpm86 {
    fn env(&self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
        match name {
            b"HTTP_HOST" => self.host.map(Cow::Borrowed),
            b"SERVER_PROTOCOL" => self.protocol.map(Cow::Borrowed),
            _ => None,
        }
    }
    fn script_filename(&self) -> &[u8] {
        self.script
    }
    fn content_length(&self) -> u64 {
        self.length
    }
    fn method(&self) -> &[u8] {
        self.method
    }
    fn pool(&self) -> &[u8] {
        b"www"
    }
    fn response_header(&self, name: &[u8]) -> Option<Cow<'_, [u8]>> {
        match name {
            b"X-Test" if self.status == 201 => Some(Cow::Borrowed(b"yes")),
            b"X-Empty" if self.status == 201 => Some(Cow::Borrowed(b"")),
            b"Content-type" => Some(Cow::Borrowed(b"text/html; charset=UTF-8")),
            _ => None,
        }
    }
    fn query_string(&self) -> &[u8] {
        self.query
    }
    fn request_uri(&self) -> &[u8] {
        self.uri
    }
    fn remote_addr(&self) -> Option<&[u8]> {
        Some(b"127.0.0.1")
    }
    fn status(&self) -> i32 {
        self.status
    }
    fn started(&self) -> Option<i64> {
        Some(1_795_000_000)
    }
    fn auth_user(&self) -> &[u8] {
        self.user
    }
}

#[test]
fn renders_like_php_fpm_86() {
    // buzz-check/fpmlog/format.txt and render-86.txt (php:8.6-fpm).
    const FORMAT: &str = "%R|%u|%m|%r|%Q|%q|%s|%f|%l|%n|%{HTTP_HOST}e|%{NOPE}e|%{PATH_NOT_SET_HERE}e|%{SERVER_PROTOCOL}e|%{X-Test}o|%{X-None}o|%{X-Empty}o|%{Content-type}o|%{content-type}o|%%|%{%Y}t|%";
    let get = Fpm86 {
        user: b"user",
        method: b"GET",
        uri: b"/t.php",
        query: b"a=1",
        status: 201,
        script: b"/srv/t.php",
        host: Some(b"www.foo.bar"),
        protocol: Some(b"HTTP/1.1"),
        ..Fpm86::default()
    };
    let post = Fpm86 {
        method: b"POST",
        uri: b"/t.php",
        status: 201,
        script: b"/srv/t.php",
        length: 5,
        protocol: Some(b"HTTP/1.1"),
        ..Fpm86::default()
    };
    let missing = Fpm86 {
        method: b"HEAD",
        status: 404,
        ..Fpm86::default()
    };
    assert_eq!(
        render(FORMAT, Some(&get)),
        "127.0.0.1|user|GET|/t.php|?|a=1|201|/srv/t.php|0|www|www.foo.bar|-|-|HTTP/1.1|yes|-|-|text/html; charset=UTF-8|-|%|2026|"
    );
    assert_eq!(
        render(FORMAT, Some(&post)),
        "127.0.0.1||POST|/t.php|||201|/srv/t.php|5|www|-|-|-|HTTP/1.1|yes|-|-|text/html; charset=UTF-8|-|%|2026|"
    );
    assert_eq!(
        render(FORMAT, Some(&missing)),
        "127.0.0.1||HEAD||||404|-|0|www|-|-|-|-|-|-|-|text/html; charset=UTF-8|-|%|2026|"
    );
}

#[test]
fn env_falls_back_to_the_process_environment() {
    // FPM reads only the request's parameters (its `%{PATH}e` is `-`);
    // Buzz also reads the process's environment.
    std::env::set_var("BUZZ_OBSERVABILITY_TEST_ENV", "from-process");
    assert_eq!(
        render("%{BUZZ_OBSERVABILITY_TEST_ENV}e %{HTTP_HOST}e", Some(&Req)),
        "from-process www.foo.bar"
    );
    std::env::remove_var("BUZZ_OBSERVABILITY_TEST_ENV");
}
