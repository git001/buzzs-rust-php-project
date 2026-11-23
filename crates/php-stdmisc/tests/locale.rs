// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Locale-dependent oracle tests. The locale is process-global, so
//! everything runs sequentially inside one test.

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use common::{export, fields, fixture_lines, show, unhex};
use php_runtime::{CollectingContext, Value, ZArray};
use php_stdmisc::{exec, locale};

fn parse_php_value(src: &str) -> Value {
    // Only the shapes produced by the generator: NULL, ints, 'strings'.
    match src {
        "NULL" => Value::Null,
        s if s.starts_with('\'') => Value::from(&s[1..s.len() - 1]),
        s => Value::Int(s.parse().unwrap()),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn locale_dependent_functions() {
    let mut ctx = CollectingContext::new();

    // escapeshellarg / escapeshellcmd in C.UTF-8 and C.
    for line in fixture_lines("escapeshell.txt") {
        let f = fields(&line);
        assert!(php_stdmisc::sys::setlocale(libc::LC_CTYPE, Some(f[0].as_bytes())).is_some());
        let input = unhex(f[1]);
        assert_eq!(
            show(&exec::escapeshellarg(&input).unwrap()),
            show(&unhex(f[2])),
            "escapeshellarg {} {:?}",
            f[0],
            show(&input)
        );
        assert_eq!(
            show(&exec::escapeshellcmd(&input).unwrap()),
            show(&unhex(f[3])),
            "escapeshellcmd {} {:?}",
            f[0],
            show(&input)
        );
    }

    let mut state = locale::LocaleState::default();
    for line in fixture_lines("locale.txt") {
        let f = fields(&line);
        let expect = show(&unhex(f[f.len() - 1]));
        match f[0] {
            "set" => {
                let cat: i64 = f[1].parse().unwrap();
                let arg = parse_php_value(&show(&unhex(f[2])));
                state
                    .setlocale(libc::LC_ALL.into(), &[Value::from("C")], &mut ctx)
                    .unwrap();
                let got = state.setlocale(cat, &[arg], &mut ctx).unwrap();
                assert_eq!(export(&got), expect, "{line}");
            }
            "multi" => {
                state
                    .setlocale(libc::LC_ALL.into(), &[Value::from("C")], &mut ctx)
                    .unwrap();
                let args: Vec<Value> = ["zz", "yy", "C.UTF-8", "C"]
                    .iter()
                    .map(|s| Value::from(*s))
                    .collect();
                assert_eq!(
                    export(
                        &state
                            .setlocale(libc::LC_ALL.into(), &args, &mut ctx)
                            .unwrap()
                    ),
                    expect
                );
            }
            "arr" | "arrnone" | "emptyarr" => {
                let items: &[&str] = match f[0] {
                    "arr" => &["zz", "POSIX", "C"],
                    "arrnone" => &["zz", "yy"],
                    _ => &[],
                };
                let a = Value::Array(ZArray::from_values(items.iter().map(|s| Value::from(*s))));
                assert_eq!(
                    export(
                        &state
                            .setlocale(libc::LC_ALL.into(), &[a], &mut ctx)
                            .unwrap()
                    ),
                    expect
                );
            }
            "bad" => {
                assert_eq!(
                    export(&state.setlocale(99, &[Value::from("C")], &mut ctx).unwrap()),
                    expect
                );
            }
            "conv" => {
                state
                    .setlocale(libc::LC_ALL.into(), &[Value::from(f[1])], &mut ctx)
                    .unwrap();
                assert_eq!(export(&Value::Array(locale::localeconv())), expect);
            }
            "langinfo" => {
                state
                    .setlocale(libc::LC_ALL.into(), &[Value::from(f[1])], &mut ctx)
                    .unwrap();
                let name = f[2];
                let v: i64 = f[3].parse().unwrap();
                let c = locale::CONSTANTS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .expect(name);
                assert_eq!(c.1, v, "constant {name}");
                assert_eq!(
                    export(&locale::nl_langinfo(v, &mut ctx).unwrap()),
                    expect,
                    "{name}"
                );
            }
            t => panic!("unknown tag {t}"),
        }
    }
    assert!(ctx.messages().is_empty(), "{:?}", ctx.messages());

    // Errors (php-src master semantics).
    let e = state
        .setlocale(6, &[Value::from("a\0b")], &mut ctx)
        .unwrap_err();
    assert_eq!(
        e.message(),
        Some("setlocale(): Argument #2 ($locales) must not contain any null bytes")
    );
    let e = state
        .setlocale(6, &[Value::from("x"), Value::from("a\0b")], &mut ctx)
        .unwrap_err();
    assert_eq!(
        e.message(),
        Some("setlocale(): Argument #3 ($rest) must not contain any null bytes")
    );
    let e = state
        .setlocale(
            6,
            &[Value::from("x"), Value::Array(ZArray::new())],
            &mut ctx,
        )
        .unwrap_err();
    assert_eq!(
        e.message(),
        Some("setlocale(): Argument #3 ($rest) must be of type ?string, array given")
    );
    let e = state
        .setlocale(
            6,
            &[Value::Array(ZArray::new()), Value::from("x")],
            &mut ctx,
        )
        .unwrap_err();
    assert_eq!(
        e.message(),
        Some("setlocale() expects exactly 2 arguments when argument #2 ($locales) is an array, 3 given")
    );
    let long = "a".repeat(255);
    assert_eq!(
        export(
            &state
                .setlocale(6, &[Value::from(long.as_str())], &mut ctx)
                .unwrap()
        ),
        "false"
    );
    assert_eq!(
        ctx.messages(),
        ["Warning: setlocale(): Specified locale name is too long"]
    );
    assert_eq!(
        export(&locale::nl_langinfo(99999, &mut ctx).unwrap()),
        "false"
    );
    assert_eq!(
        ctx.messages()[1],
        "Warning: nl_langinfo(): Item '99999' is not valid"
    );

    // localeconv_decimal_point(): `.` in C, `,` in a decimal-comma locale
    // (en_DK.utf8 when installed).
    state
        .setlocale(libc::LC_ALL.into(), &[Value::from("C")], &mut ctx)
        .unwrap();
    assert_eq!(locale::decimal_point(), b'.');
    let dk = state
        .setlocale(
            libc::LC_NUMERIC.into(),
            &[Value::from("en_DK.utf8")],
            &mut ctx,
        )
        .unwrap();
    if !matches!(dk, Value::Bool(false)) {
        assert_eq!(locale::decimal_point(), b',');
    }

    state.request_shutdown();
    assert!(!state.locale_changed);
    assert_eq!(locale::decimal_point(), b'.');
}
