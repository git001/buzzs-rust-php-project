// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for the `ext/standard/math.c` helpers.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, export_float, lit, pure, regenerate, Case, V};
use php_stdstr::math::{
    abs, base_convert, bindec, ceil, decbin, dechex, decoct, deg2rad, fdiv, floor, fmod, hexdec,
    hypot, int_mod, intdiv, log, octdec, rad2deg, round_long, round_places, Number, RoundingMode,
};
use php_stdstr::{Level, Notices, StdError};

/// php-src master raises this `E_NOTICE`; PHP 8.3 (the oracle) does not.
const OVERFLOW_NOTICE: &str =
    "Input number is larger than PHP_INT_MAX, precision has been lost in conversion";

fn num(n: Number) -> V {
    match n {
        Number::Int(i) => V::Int(i),
        Number::Float(f) => V::Float(f),
    }
}

/// Drops the master-only overflow notice so the result can be compared
/// against PHP 8.3.
fn strip_master_notice(notices: &mut Notices) {
    notices.retain(|n| !(n.level == Level::Notice && n.message == OVERFLOW_NOTICE));
}

/// PHP 8.3 formats float-path `base_convert` results into a 65-byte
/// buffer, keeping only the last 64 digits; master (GH-22395) keeps up to
/// `ZEND_DOUBLE_MAX_LENGTH - 1`.
fn as_php83(r: Result<Vec<u8>, StdError>) -> Result<Vec<u8>, StdError> {
    r.map(|v| v[v.len().saturating_sub(64)..].to_vec())
}

fn flit(f: f64) -> String {
    let s = export_float(f);
    if s.starts_with('-') {
        format!("({s})")
    } else {
        s
    }
}

fn ilit(i: i64) -> String {
    if i == i64::MIN {
        "PHP_INT_MIN".into()
    } else if i < 0 {
        format!("({i})")
    } else {
        i.to_string()
    }
}

const BASE_INPUTS: &[&[u8]] = &[
    b"",
    b"0",
    b"1",
    b"10",
    b"11",
    b"101010",
    b"777",
    b"789",
    b"12345",
    b"0x1A",
    b"0X1a",
    b"0b101",
    b"0B11",
    b"0o17",
    b"0O17",
    b"00x1",
    b"0x",
    b"0b",
    b" 0xFF\t\n",
    b"\r\nFF\t\n",
    b"\x0b\x0c12\x0b",
    b" 1 1 ",
    b"FF",
    b"ff",
    b"fg",
    b"zz",
    b"ZZ",
    b"Zz9",
    b"-1",
    b"+1",
    b"1.5",
    b"1e5",
    b"q12345",
    b"12345+?!",
    b"12345q",
    b"\x00",
    b"1\x002",
    b"\xff\x80",
    b"7fffffffffffffff",
    b"8000000000000000",
    b"ffffffffffffffff",
    b"12345678901234567",
    b"17fffffffffffffff",
    b"fffffffffffffffff",
    b"0111111111111111111111111111111111111111111111111111111111111111",
    b"1111111111111111111111111111111111111111111111111111111111111111",
    b"11111111111111111111111111111111111111111111111111111111111111111",
    b"0000000000000000000000000000000000000000000000000000000000000000000000001",
    b"777777777777777777777",
    b"1777777777777777777777",
    b"7777777777777777777777",
    b"9223372036854775807",
    b"9223372036854775808",
    b"99999999999999999999999",
    b"1y2p0ij32e8e7",
    b"1y2p0ij32e8e8",
    b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
];

const INTS: &[i64] = &[
    0,
    1,
    -1,
    2,
    7,
    8,
    10,
    15,
    16,
    255,
    256,
    -256,
    1023,
    4096,
    12345,
    0x7fff_ffff,
    -0x8000_0000,
    0xffff_ffff,
    0x1_0000_0000,
    1_234_567_890_123,
    i64::MAX,
    i64::MIN,
    i64::MIN + 1,
    -12345,
];

const FLOATS: &[f64] = &[
    0.0,
    -0.0,
    0.5,
    -0.5,
    1.0,
    1.5,
    -1.5,
    2.5,
    -2.5,
    3.7,
    -3.7,
    10.0,
    100.0,
    1e15,
    9.223_372_036_854_776e18,
    1e100,
    -1e100,
    1e-10,
    123.456,
    -123.456,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
    std::f64::consts::PI,
    0.1,
    -7.5,
    360.0,
    -45.0,
];

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();

    for s in BASE_INPUTS {
        for (name, f) in [
            ("bindec", bindec as fn(&[u8], &mut Notices) -> Number),
            ("hexdec", hexdec),
            ("octdec", octdec),
        ] {
            v.push(case(&format!("{name}({})", lit(s)), |n| {
                let r = f(s, n);
                strip_master_notice(n);
                Ok::<_, StdError>(num(r))
            }));
        }
        for (from, to) in [
            (2, 10),
            (8, 10),
            (10, 2),
            (10, 16),
            (16, 10),
            (16, 2),
            (16, 15),
            (36, 10),
            (36, 16),
            (10, 36),
            (36, 2),
            (2, 36),
            (9, 10),
            (3, 7),
        ] {
            v.push(case(
                &format!("base_convert({}, {from}, {to})", lit(s)),
                |n| {
                    let r = base_convert(s, from, to, n);
                    strip_master_notice(n);
                    as_php83(r)
                },
            ));
        }
    }
    for (from, to) in [
        (1, 10),
        (0, 10),
        (37, 10),
        (-5, 10),
        (10, 1),
        (10, 37),
        (1, 1),
    ] {
        v.push(case(&format!("base_convert('10', {from}, {to})"), |n| {
            base_convert(b"10", from, to, n)
        }));
    }
    // Infinity, and outputs longer than 64 digits.
    for (s, from, to) in [
        (&b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"[..], 36, 2),
        (b"ffffffffffffffffff", 16, 2),
        (b"ffffffffffffffffffffffffffffffff", 16, 3),
    ] {
        v.push(case(
            &format!("base_convert({}, {from}, {to})", lit(s)),
            |n| {
                let r = base_convert(s, from, to, n);
                strip_master_notice(n);
                as_php83(r)
            },
        ));
    }

    for &i in INTS {
        v.push(pure(&format!("decbin({})", ilit(i)), decbin(i)));
        v.push(pure(&format!("decoct({})", ilit(i)), decoct(i)));
        v.push(pure(&format!("dechex({})", ilit(i)), dechex(i)));
        v.push(pure(&format!("abs({})", ilit(i)), num(abs(Number::Int(i)))));
        v.push(pure(&format!("floor({})", ilit(i)), floor(Number::Int(i))));
        v.push(pure(&format!("ceil({})", ilit(i)), ceil(Number::Int(i))));
        for p in [0, 1, 5, i64::MAX] {
            let r = round_long(i, round_places(p)).expect("non-negative places");
            v.push(pure(&format!("round({}, {})", ilit(i), ilit(p)), r));
        }
        for &j in &[0, 1, -1, 2, -2, 3, 7, -7, 10, i64::MAX, i64::MIN] {
            v.push(case(&format!("intdiv({}, {})", ilit(i), ilit(j)), |_| {
                intdiv(i, j)
            }));
            v.push(case(&format!("{} % {}", ilit(i), ilit(j)), |_| {
                int_mod(i, j)
            }));
        }
    }

    for &f in FLOATS {
        let fl = flit(f);
        v.push(pure(&format!("abs({fl})"), num(abs(Number::Float(f)))));
        v.push(pure(&format!("floor({fl})"), floor(Number::Float(f))));
        v.push(pure(&format!("ceil({fl})"), ceil(Number::Float(f))));
        v.push(pure(&format!("deg2rad({fl})"), deg2rad(f)));
        v.push(pure(&format!("rad2deg({fl})"), rad2deg(f)));
        v.push(case(&format!("log({fl})"), |_| log(f, None)));
        v.push(pure(&format!("is_nan({fl})"), f.is_nan()));
        for &g in &[
            0.0,
            -0.0,
            1.0,
            2.0,
            10.0,
            -4.0,
            3.0,
            0.5,
            f64::INFINITY,
            f64::NAN,
            -7.5,
        ] {
            let gl = flit(g);
            v.push(pure(&format!("fmod({fl}, {gl})"), fmod(f, g)));
            v.push(pure(&format!("fdiv({fl}, {gl})"), fdiv(f, g)));
            v.push(pure(&format!("hypot({fl}, {gl})"), hypot(f, g)));
            v.push(case(&format!("log({fl}, {gl})"), |_| log(f, Some(g))));
        }
    }
    v.push(case("log(36, -4)", |_| log(36.0, Some(-4.0))));
    v.push(case("log(8, 2)", |_| log(8.0, Some(2.0))));
    v.push(case("log(1000, 10)", |_| log(1000.0, Some(10.0))));
    v.push(case("log(27, 3)", |_| log(27.0, Some(3.0))));

    // php-src master only: notices and mode validation.
    v.push(
        case("base_convert('fffffffffffffffff', 16, 15)", |n| {
            base_convert(b"fffffffffffffffff", 16, 15, n)
        })
        .expect(&format!(
            "Notice: {OVERFLOW_NOTICE}\n=> '2ee03c32ad644bd1e1'"
        )),
    );
    v.push(
        case("hexdec('17fffffffffffffff') /* master */", |n| {
            Ok::<_, StdError>(num(hexdec(b"17fffffffffffffff", n)))
        })
        .expect(&format!(
            "Notice: {OVERFLOW_NOTICE}\n=> 2.7670116110564327E+19"
        )),
    );
    v.push(
        case("base_convert('fg', 16, 10) /* master */", |n| {
            base_convert(b"fg", 16, 10, n)
        })
        .expect(
            "Deprecated: Invalid characters passed for attempted conversion, these have been ignored\n=> '15'",
        ),
    );
    // GH-22395: master keeps all 78 digits.
    v.push(
        case(
            "strlen(base_convert(str_repeat('1', 61), 36, 16)) /* master */",
            |n| base_convert(&[b'1'; 61], 36, 16, n).map(|r| r.len()),
        )
        .expect(&format!("Notice: {OVERFLOW_NOTICE}\n=> 78")),
    );
    v.push(
        case(
            "substr(base_convert(str_repeat('1', 61), 36, 16), 0, 13) /* master */",
            |n| base_convert(&[b'1'; 61], 36, 16, n).map(|r| r[..13].to_vec()),
        )
        .expect(&format!("Notice: {OVERFLOW_NOTICE}\n=> '4b61b5e0639ff'")),
    );
    for mode in [0, 9, 1234, -1] {
        v.push(
            case(&format!("round(1.5, 0, {mode}) /* master */"), |_| {
                RoundingMode::from_long(mode).map(RoundingMode::as_long)
            })
            .expect(
                "!! ValueError: round(): Argument #3 ($mode) must be a valid rounding mode (RoundingMode::*)",
            ),
        );
    }
    v
}

#[test]
fn oracle() {
    check("math", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("math", &cases());
}
