// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Expectations hand-ported from php-src `.phpt` files that the oracle
//! fixtures do not already cover.

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_runtime::{CollectingContext, Value, ZArray};
use php_stdmisc::crypt::{crypt, password};

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `ext/standard/tests/crypt/bcrypt_salt_dollar.phpt`
#[test]
fn bcrypt_salt_dollar() {
    for i in 0..23 {
        let salt = format!("$2y$04${}$", "0".repeat(i));
        let r = crypt(b"foo", salt.as_bytes());
        if i < 22 {
            assert_eq!(s(&r), "*0", "{salt}");
        } else {
            assert_eq!(
                s(&r),
                "$2y$04$000000000000000000000u2a2UpVexIt9k3FMJeAVr3c04F5tcI8K"
            );
        }
    }
}

/// `crypt/des_fallback_invalid_salt.phpt`, `crypt/bcrypt_invalid_algorithm.phpt`,
/// `strings/crypt_chars.phpt`
#[test]
fn crypt_misc_phpts() {
    assert_eq!(s(&crypt(b"test", b"$:#")), "*0");
    assert_eq!(s(&crypt(b"test", b"$:5zd$01\n")), "*0");
    for salt in [
        &b"$23$04$1234567890123456789012345"[..],
        b"$20$04$1234567890123456789012345",
        b"$2g$04$1234567890123456789012345",
    ] {
        assert_eq!(s(&crypt(b"test", salt)), "*0");
    }
    assert_eq!(s(&crypt("À1234abcd".as_bytes(), b"99")), "99PxawtsTfX56");
    assert_eq!(s(&crypt("À9234abcd".as_bytes(), b"99")), "99jcVcGxUZOWk");
    assert_eq!(
        s(&crypt("À1234abcd".as_bytes(), b"_01234567")),
        "_01234567IBjxKliXXRQ"
    );
    assert_eq!(
        s(&crypt("À9234abcd".as_bytes(), b"_01234567")),
        "_012345678OSGpGQRVHA"
    );
}

/// `password/password_bcrypt_errors.phpt`, `password_hash_error_argon2.phpt`,
/// `password_removed_salt_option.phpt`
#[test]
fn password_errors() {
    let mut ctx = CollectingContext::new();
    let opts = |pairs: &[(&str, Value)]| ZArray::from_pairs(pairs.iter().cloned());
    let e = password::password_hash(
        b"foo",
        password::AlgoArg::Str(b"2y"),
        Some(&opts(&[("cost", Value::Int(3))])),
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        Some("Invalid bcrypt cost parameter specified: 3")
    );
    let e = password::password_hash(
        b"foo",
        password::AlgoArg::Str(b"2y"),
        Some(&opts(&[("cost", Value::Int(32))])),
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        Some("Invalid bcrypt cost parameter specified: 32")
    );
    for (o, msg) in [
        (
            ("memory_cost", 0),
            "Memory cost is outside of allowed memory range",
        ),
        (
            ("time_cost", 0),
            "Time cost is outside of allowed time range",
        ),
        (("threads", 0), "Invalid number of threads"),
    ] {
        let e = password::password_hash(
            b"test",
            password::AlgoArg::Str(b"argon2id"),
            Some(&opts(&[(o.0, Value::Int(o.1))])),
            &mut ctx,
        )
        .unwrap_err();
        assert_eq!(e.message(), Some(msg));
    }
    let h = password::password_hash(
        b"rasmuslerdorf",
        password::AlgoArg::Int(1),
        Some(&opts(&[
            ("cost", Value::Int(4)),
            ("salt", Value::from("123456789012345678901")),
        ])),
        &mut ctx,
    )
    .unwrap();
    assert!(password::password_verify(b"rasmuslerdorf", h.as_bytes()));
    assert_eq!(
        ctx.messages(),
        ["Warning: password_hash(): The \"salt\" option has been ignored, since providing a custom salt is no longer supported"]
    );
}

/// `password/password_bcrypt_short.phpt`, `password_algos()`.
#[test]
fn password_short_and_big() {
    // A truncated bcrypt hash never verifies.
    assert!(!password::password_verify(b"", b"$2y$04$"));
    assert!(!password::password_verify(
        b"",
        b"$2y$04$000000000000000000000"
    ));
    let algos: Vec<String> = password::password_algos()
        .values()
        .map(|v| s(v.as_str().unwrap().as_bytes()))
        .collect();
    assert_eq!(algos, ["2y", "argon2i", "argon2id"]);
}
