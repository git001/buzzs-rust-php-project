// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for the deterministic network functions (`inet_*`,
//! `ip2long`/`long2ip`, protocol/service databases, localhost lookups),
//! plus `openlog`/`syslog`/`closelog` and `net_get_interfaces` shape
//! (the `syslog.filter` rules are tested in php-log).

mod common;

use common::{export, fields, fixture_lines, show, unhex};
use php_runtime::{CollectingContext, Value};
use php_stdmisc::net;

#[test]
fn network_functions_match_oracle() {
    let mut ctx = CollectingContext::new();
    for line in fixture_lines("net.txt") {
        let f = fields(&line);
        let arg = unhex(f[1]);
        let expect = show(&unhex(f[2]));
        let s = String::from_utf8_lossy(&arg).into_owned();
        let split = |s: &str| {
            let (a, b) = s.rsplit_once('/').unwrap();
            (a.to_string(), b.to_string())
        };
        let got: Value = match f[0] {
            "inet_pton" => net::inet_pton(&arg).unwrap(),
            "ip2long" => net::ip2long(&arg).unwrap(),
            "inet_ntop" => net::inet_ntop(&arg),
            "long2ip" => Value::from(net::long2ip(s.parse().unwrap())),
            "getprotobyname" => net::getprotobyname(&arg).unwrap(),
            "getprotobynumber" => net::getprotobynumber(s.parse().unwrap()),
            "getservbyname" => {
                let (a, b) = split(&s);
                net::getservbyname(a.as_bytes(), b.as_bytes()).unwrap()
            }
            "getservbyport" => {
                let (a, b) = split(&s);
                net::getservbyport(a.parse().unwrap(), b.as_bytes()).unwrap()
            }
            "gethostbyname" => Value::Str(net::gethostbyname(&arg, &mut ctx).unwrap()),
            "gethostbynamel" => net::gethostbynamel(&arg, &mut ctx).unwrap(),
            "gethostbyaddr" => net::gethostbyaddr(&arg, &mut ctx).unwrap(),
            other => panic!("unknown {other}"),
        };
        assert_eq!(export(&got), expect, "{}({s:?})", f[0]);
    }
    assert_eq!(
        ctx.messages(),
        ["Warning: gethostbyaddr(): Address is not a valid IPv4 or IPv6 address"]
    );
}

#[test]
fn errors() {
    let mut ctx = CollectingContext::new();
    let e = net::ip2long(b"").unwrap_err();
    assert_eq!(
        e.message(),
        Some("ip2long(): Argument #1 ($ip) must not be empty")
    );
    let e = net::inet_pton(b"1.2\0").unwrap_err();
    assert_eq!(
        e.message(),
        Some("inet_pton(): Argument #1 ($ip) must not contain any null bytes")
    );
    let long = vec![b'a'; 256];
    assert_eq!(net::gethostbyname(&long, &mut ctx).unwrap().len(), 256);
    assert!(matches!(
        net::gethostbynamel(&long, &mut ctx).unwrap(),
        Value::Bool(false)
    ));
    assert_eq!(
        ctx.messages(),
        [
            "Warning: gethostbyname(): Host name cannot be longer than 255 characters",
            "Warning: gethostbynamel(): Host name cannot be longer than 255 characters",
        ]
    );
    assert!(!net::gethostname(&mut ctx).unwrap().is_null());
}

#[test]
fn syslog_functions() {
    let settings = php_log::SyslogSettings::default();
    assert!(net::syslog(7, b"phprust test", &settings));
    assert!(net::openlog(b"phprust", 0, 8).unwrap());
    assert!(net::closelog());
    let e = net::openlog(b"a\0", 0, 8).unwrap_err();
    assert_eq!(
        e.message(),
        Some("openlog(): Argument #1 ($prefix) must not contain any null bytes")
    );
}

#[test]
fn interfaces_shape() {
    let mut ctx = CollectingContext::new();
    let v = net::net_get_interfaces(&mut ctx).unwrap();
    let s = export(&v);
    // ext/standard/tests/network/net_get_interfaces_001.phpt: every
    // interface has "unicast" entries with "flags" and an "up" flag.
    assert!(
        s.contains("'unicast' =>") && s.contains("'flags' =>") && s.contains("'up' =>"),
        "{s}"
    );
    assert!(s.contains("'lo' =>"), "{s}");
}

/// Live comparison with `/usr/bin/php` (machine-dependent, so ignored by
/// default): `cargo test -p php-stdmisc --test net -- --ignored`.
#[test]
#[ignore = "needs /usr/bin/php; compares live machine state"]
fn interfaces_and_hostname_match_live_php() {
    let mut ctx = CollectingContext::new();
    let php = |code: &str| {
        let out = std::process::Command::new("/usr/bin/php")
            .args(["-n", "-d", "memory_limit=1G", "-r", code])
            .output()
            .expect("php");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(
        export(&net::net_get_interfaces(&mut ctx).unwrap()),
        php("var_export(net_get_interfaces());")
    );
    assert_eq!(
        export(&net::gethostname(&mut ctx).unwrap()),
        php("var_export(gethostname());")
    );
}
