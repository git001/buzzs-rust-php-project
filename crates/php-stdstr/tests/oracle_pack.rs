// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `pack()` / `unpack()`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, regenerate, Case, K, V};
use php_stdstr::pack::{pack, unpack, PackArg, UnpackKey, UnpackValue};
use std::fmt::Write as _;

/// A PHP argument: literal source and our value.
#[derive(Clone)]
struct A(String, PackArg);

fn i(v: i64) -> A {
    A(
        if v == i64::MIN {
            "PHP_INT_MIN".into()
        } else {
            v.to_string()
        },
        PackArg::Int(v),
    )
}
fn f(v: f64) -> A {
    let src = if v.is_nan() {
        "NAN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "INF" } else { "-INF" }.to_owned()
    } else {
        format!("{v:?}")
    };
    A(src, PackArg::Float(v))
}
fn s(v: &[u8]) -> A {
    A(lit(v), PackArg::Str(v.to_vec()))
}
fn null() -> A {
    A("null".into(), PackArg::Null)
}
fn b(v: bool) -> A {
    A(v.to_string(), PackArg::Bool(v))
}

fn hex(v: &[u8]) -> String {
    let mut out = String::new();
    for x in v {
        let _ = write!(out, "{x:02x}");
    }
    out
}

fn pk(fmt: &str, args: &[A]) -> Case {
    let mut php = format!("bin2hex(pack({}", lit(fmt.as_bytes()));
    for a in args {
        php.push_str(", ");
        php.push_str(&a.0);
    }
    php.push_str("))");
    let vals: Vec<PackArg> = args.iter().map(|a| a.1.clone()).collect();
    case(&php, |n| pack(fmt.as_bytes(), &vals, n).map(|r| hex(&r)))
}

fn to_v(r: Option<Vec<(UnpackKey, UnpackValue)>>) -> V {
    match r {
        None => V::Bool(false),
        Some(items) => V::Arr(
            items
                .into_iter()
                .map(|(k, v)| {
                    (
                        match k {
                            UnpackKey::Int(i) => K::Int(i),
                            UnpackKey::Str(s) => K::Str(s),
                        },
                        match v {
                            UnpackValue::Int(i) => V::Int(i),
                            UnpackValue::Float(f) => V::Float(f),
                            UnpackValue::Str(s) => V::Str(s),
                        },
                    )
                })
                .collect(),
        ),
    }
}

fn up_off(fmt: &[u8], data: &[u8], offset: Option<i64>) -> Case {
    let php = match offset {
        Some(o) => format!("unpack({}, {}, {o})", lit(fmt), lit(data)),
        None => format!("unpack({}, {})", lit(fmt), lit(data)),
    };
    case(&php, |n| {
        unpack(fmt, data, offset.unwrap_or(0), n).map(to_v)
    })
}

fn up(fmt: &str, data: &[u8]) -> Case {
    up_off(fmt.as_bytes(), data, None)
}

/// `unpack(fmt, pack(fmt, arg))` round trip.
fn rt(fmt: &str, arg: &A) -> Case {
    let php = format!("unpack({0}, pack({0}, {1}))", lit(fmt.as_bytes()), arg.0);
    case(&php, |n| {
        let packed = pack(fmt.as_bytes(), std::slice::from_ref(&arg.1), n)?;
        unpack(fmt.as_bytes(), &packed, 0, n).map(to_v)
    })
}

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    // Integer codes over a range of values.
    let ints = [
        0,
        1,
        -1,
        127,
        128,
        -127,
        -128,
        -129,
        255,
        256,
        32767,
        32768,
        65534,
        65535,
        65537,
        -1000,
        -64434,
        -65535,
        2_147_483_647,
        2_147_483_648,
        2_147_483_650,
        -2_147_483_648,
        4_294_967_295,
        4_294_967_296,
        -4_294_967_296,
        0xffff_ffff_fffe,
        i64::MAX,
        i64::MIN,
    ];
    for code in [
        "c", "C", "s", "S", "n", "v", "i", "I", "l", "L", "N", "V", "q", "Q", "J", "P",
    ] {
        for &x in &ints {
            v.push(pk(code, &[i(x)]));
            v.push(rt(code, &i(x)));
        }
    }
    // Floats.
    let floats = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        1.5,
        0.1,
        0.591_234_709_823_149,
        -0.591_234_709_823_149,
        1e16,
        -1e16,
        1.234_567_890_123_456_8e19,
        3.4e38,
        3.5e38,
        1e-40,
        1e300,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ];
    for code in ["f", "g", "G", "d", "e", "E"] {
        for &x in &floats {
            v.push(pk(code, &[f(x)]));
            if !x.is_nan() {
                v.push(rt(code, &f(x)));
            }
        }
        for a in [
            s(b""),
            s(b"a"),
            s(b" "),
            s(b"1.5abc"),
            s(b" 2e3"),
            null(),
            i(1),
            i(-1),
            b(true),
        ] {
            v.push(pk(code, &[a]));
        }
    }
    // Conversions of ints from other types.
    for a in [
        s(b"12abc"),
        s(b"  42"),
        s(b"1e3"),
        s(b"0x1A"),
        s(b"abc"),
        s(b"-7.9"),
        f(3.99),
        f(-3.99),
        f(1e19),
        f(-1e19),
        f(3e19),
        f(f64::NAN),
        f(f64::INFINITY),
        null(),
        b(true),
        b(false),
    ] {
        v.push(pk("N", &[a]));
    }
    // Strings.
    for (fmt, a) in [
        ("a", s(b"hello world")),
        ("a*", s(b"hello world")),
        ("a0", s(b"hello")),
        ("a9", s(b"hello")),
        ("A", s(b"hello world")),
        ("A*", s(b"hello world")),
        ("A9", s(b"hello")),
        ("A5", s(b"foo ")),
        ("A4", s(b"fooo")),
        ("Z0", s(b"f")),
        ("Z5", s(b"foo\0")),
        ("Z4", s(b"fooo")),
        ("Z4", s(b"foo")),
        ("Z*", s(b"foo")),
        ("Z", s(b"foo")),
        ("Z1", s(b"foo")),
        ("a*", i(123)),
        ("a*", f(1.5)),
        ("a*", f(0.1)),
        ("a*", f(1e25)),
        ("a*", f(-1e-7)),
        ("a*", f(100.0)),
        ("a*", b(true)),
        ("a*", b(false)),
        ("a*", null()),
        ("A*", s(b"")),
        ("Z*", s(b"")),
        ("H", i(4)),
        ("H*", s(b"48656c6c6f")),
        ("H*", s(b"4865x6c6c6f")),
        ("H*", s(b"abc")),
        ("H3", s(b"abcd")),
        ("H5", s(b"abc")),
        ("H*", s(b"ABCDEF")),
        ("h*", s(b"48656c6c6f")),
        ("h3", s(b"123")),
        ("h", i(3_000_000)),
        ("H0", s(b"ab")),
        ("H*", s(b"")),
        ("H2", s(b"g!z")),
        ("a2147483648", s(b"x")),
    ] {
        v.push(pk(fmt, &[a]));
    }
    // Multiple args, x/X/@.
    v.push(pk("nvc*", &[i(0x1234), i(0x5678), i(65), i(66)]));
    v.push(pk("C*", &[]));
    v.push(pk("C*", &[i(1), i(2), i(3)]));
    v.push(pk("C0", &[]));
    v.push(pk("NnC", &[i(1), i(2), i(3)]));
    v.push(pk("x", &[]));
    v.push(pk("x5", &[]));
    v.push(pk("x*", &[]));
    v.push(pk("X", &[]));
    v.push(pk("X*", &[]));
    v.push(pk("a4X2", &[s(b"abcd")]));
    v.push(pk("a4X5a1", &[s(b"abcd"), s(b"z")]));
    v.push(pk("a4X2a1", &[s(b"abcd"), s(b"z")]));
    v.push(pk("@4", &[]));
    v.push(pk("@*", &[]));
    v.push(pk("a6@2", &[s(b"abcdef")]));
    v.push(pk("a2@5a1", &[s(b"ab"), s(b"c")]));
    v.push(pk("@0", &[]));
    v.push(pk("x4294967295", &[]));
    v.push(pk("a4294967297", &[s(b"q")]));
    v.push(pk("x2147483647x", &[]));
    v.push(pk("N4294967296", &[i(1)]));
    // Errors and warnings.
    v.push(pk("a", &[]));
    v.push(pk("H", &[]));
    v.push(pk("N", &[]));
    v.push(pk("N2", &[i(1)]));
    v.push(pk("N", &[i(1), i(2), i(3)]));
    v.push(pk("B", &[]));
    v.push(pk("nB", &[i(1)]));
    v.push(pk("", &[]));
    v.push(pk("\0", &[]));
    v.push(pk("H", &[s(b"\0")]));
    v.push(up("\0", b"a"));
    v.push(pk("", &[i(1)]));
    v.push(pk("C2147483647", &[i(1)]));
    v.push(pk("CC2147483647", &[i(1), i(1)]));
    v.push(pk("d*", &[f(1.0), f(2.0)]));
    v.push(pk("aaaa", &[s(b"1"), s(b"2"), s(b"3")]));

    // Endianness modifiers (php-src master only; 8.3 rejects them).
    let master: &[(&str, &[A], &str)] = &[
        ("s<", &[i(0x1234)], "=> '3412'"),
        ("s>", &[i(0x1234)], "=> '1234'"),
        ("S>2", &[i(1), i(2)], "=> '00010002'"),
        ("l>", &[i(0x0102_0304)], "=> '01020304'"),
        ("L<", &[i(0x0102_0304)], "=> '04030201'"),
        ("q>", &[i(1)], "=> '0000000000000001'"),
        ("Q<*", &[i(1), i(2)], "=> '01000000000000000200000000000000'"),
        ("f>", &[f(1.0)], "=> '3f800000'"),
        ("d>", &[f(1.0)], "=> '3ff0000000000000'"),
        ("d<", &[f(1.0)], "=> '000000000000f03f'"),
        ("n<", &[i(1)], "!! ValueError: Endianness modifier '<' cannot be applied to format code 'n' which already has inherent endianness"),
        ("v>", &[i(1)], "!! ValueError: Endianness modifier '>' cannot be applied to format code 'v' which already has inherent endianness"),
        ("J<", &[i(1)], "!! ValueError: Endianness modifier '<' cannot be applied to format code 'J' which already has inherent endianness"),
        ("E>", &[f(1.0)], "!! ValueError: Endianness modifier '>' cannot be applied to format code 'E' which already has inherent endianness"),
        ("c<", &[i(1)], "!! ValueError: Endianness modifier is not supported for format code 'c'"),
        ("a>", &[i(1)], "!! ValueError: Endianness modifier is not supported for format code 'a'"),
        ("x<", &[i(1)], "!! ValueError: Endianness modifier is not supported for format code 'x'"),
        ("@<", &[i(1)], "!! ValueError: Endianness modifier is not supported for format code '@'"),
    ];
    for (fmt, args, exp) in master {
        v.push(pk(fmt, args).expect(exp));
    }

    // unpack().
    v.push(up("nlen/a*data", b"\0\x05hello"));
    v.push(up("c2chars/nint", b"\x04\x00\xa0\x00"));
    v.push(up("C*", b"\x01\x02\x03"));
    v.push(up("C*x/C", b"\x01\x02\x03"));
    v.push(up("C*", b""));
    v.push(up("Cfoo*", b"\x01\x02\x03"));
    v.push(up("C2", b"\x01"));
    v.push(up("C2", b""));
    v.push(up("N", b"\x01\x02"));
    v.push(up("N", b"\x01"));
    v.push(up("N2", b"\x00\x00\x00\x01\x00\x00\x00\x02"));
    v.push(up("N2x/C", b"\x00\x00\x00\x01\x00\x00\x00\x02\x09"));
    v.push(up("C0", b"\x01"));
    v.push(up("a", b"hello"));
    v.push(up("a*", b"hello"));
    v.push(up("a0", b"hello"));
    v.push(up("a10", b"hello"));
    v.push(up("A*", b"foo\0\rbar\0 \t\r\n"));
    v.push(up("A4", b"foo\0\rbar\0 \t\r\n"));
    v.push(up("A*", b"  \0"));
    v.push(up("Z*", b"foo\0\rbar\0 \t\r\n"));
    v.push(up("Z9", b"foo\0\rbar\0 \t\r\n"));
    v.push(up("Z2", b"\0"));
    v.push(up("Z2", b"\0\0"));
    v.push(up("Z2", b"A\0"));
    v.push(up("Z2", b"AB\0"));
    v.push(up("Z2", b"ABC"));
    v.push(up("H*", b"\x12\x34\xab"));
    v.push(up("h*", b"\x12\x34\xab"));
    v.push(up("H3", b"\x12\x34\xab"));
    v.push(up("H4", b"\x12\x34\xab"));
    v.push(up("H5", b"\x12\x34\xab"));
    v.push(up("H7", b"\x12\x34\xab"));
    v.push(up("H8", b"\x12\x34\xab"));
    v.push(up("H0", b"\x12\x34\xab"));
    v.push(up("H", b"\x12\x34\xab"));
    v.push(up("H*a/Cb", b"\x12\x34"));
    v.push(up("H2a/Cb", b"\x12\x34"));
    v.push(up("H*", b""));
    v.push(up("H2147483647", b"\x12"));
    v.push(up("h1073741824", b"\x12"));
    v.push(up("H1073741823", b"\x12"));
    v.push(up("x", b""));
    v.push(up("x*", b"abc"));
    v.push(up("x2/a", b"abc"));
    v.push(up("x4/a", b"abc"));
    v.push(up("X", b"abc"));
    v.push(up("X*", b"abc"));
    v.push(up("a2/X3/a", b"abcdef"));
    v.push(up("a2/X/a", b"abcdef"));
    v.push(up("a2/X2/a", b"abcdef"));
    v.push(up("Va/X", b"\x01\0\0\0\x02\0\0\0"));
    v.push(up("Va/X4", b"\x01\0\0\0\x02\0\0\0"));
    v.push(up("V1a/X4/V1b/V1c/X4/V1d", b"\x01\0\0\0\x02\0\0\0"));
    v.push(up("@", b"abc"));
    v.push(up("@0/a", b"abc"));
    v.push(up("@2/a", b"abc"));
    v.push(up("@3/a", b"abc"));
    v.push(up("@4/a", b"abc"));
    v.push(up("@*", b"abc"));
    v.push(up("a/@*/a", b"abc"));
    v.push(up("c", b"\xff"));
    v.push(up("C", b"\xff"));
    v.push(up("s", b"\xff\xff"));
    v.push(up("S", b"\xff\xff"));
    v.push(up("n", b"\xff\xfe"));
    v.push(up("v", b"\xff\xfe"));
    v.push(up("i", b"\xff\xff\xff\xff"));
    v.push(up("I", b"\xff\xff\xff\xff"));
    v.push(up("l", b"\xff\xff\xff\xff"));
    v.push(up("L", b"\xff\xff\xff\xff"));
    v.push(up("N", b"\xff\xff\xff\xfe"));
    v.push(up("V", b"\xff\xff\xff\xfe"));
    v.push(up("q", b"\xff\xff\xff\xff\xff\xff\xff\xff"));
    v.push(up("Q", b"\xfe\xff\xff\xff\xff\xff\xff\xff"));
    v.push(up("J", b"\x80\0\0\0\0\0\0\x01"));
    v.push(up("P", b"\x01\0\0\0\0\0\0\x80"));
    v.push(up("f", b"\0\0\x80\x3f"));
    v.push(up("g", b"\0\0\x80\x3f"));
    v.push(up("G", b"\x3f\x80\0\0"));
    v.push(up("d", b"\0\0\0\0\0\0\xf0\x3f"));
    v.push(up("e", b"\0\0\0\0\0\0\xf0\x3f"));
    v.push(up("E", b"\x3f\xf0\0\0\0\0\0\0"));
    v.push(up("f", b"\0\0\xc0\x7f"));
    v.push(up("B", b"abc"));
    v.push(up("CB", b"abc"));
    v.push(up("", b"abc"));
    v.push(up("C1/C1", b"ab"));
    v.push(up("Ca/Ca", b"ab"));
    v.push(up("C2a/Ca1", b"abc"));
    v.push(up("C5/C1", b"abcdefg"));
    v.push(up("C0x/C", b"ab"));
    v.push(up("C-1/C1", b"abc"));
    v.push(up("Cfoo/Cbar", b"ab"));
    v.push(up("Cfoo/Cbar/", b"ab"));
    v.push(up("C/", b"ab"));
    v.push(up("C//C", b"ab"));
    v.push(up("C*-1/C", b"ab"));
    v.push(up("C2147483648", b"ab"));
    v.push(up("C99999999999999999999", b"ab"));
    v.push(up("C2147483647", b"ab"));
    v.push(up("a2147483647", b"ab"));
    v.push(up(
        "Cxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        b"a",
    ));
    v.push(up_off(b"C", b"abc", Some(1)));
    v.push(up_off(b"a*", b"abc", Some(3)));
    v.push(up_off(b"C", b"abc", Some(3)));
    // php-src master renamed the parameter `$data` to `$string`.
    let off_err =
        "!! ValueError: unpack(): Argument #3 ($offset) must be contained in argument #2 ($string)";
    v.push(up_off(b"C", b"abc", Some(4)).expect(off_err));
    v.push(up_off(b"C", b"abc", Some(-1)).expect(off_err));
    v.push(up_off(
        b"l2",
        b"pad\x04\x03\x02\x01\x08\x07\x06\x05",
        Some(3),
    ));
    v.push(up_off(
        b"@4/l",
        b"pad\x04\x03\x02\x01\x08\x07\x06\x05",
        Some(3),
    ));

    let master_up: &[(&str, &[u8], &str)] = &[
        ("s<", b"\x34\x12", "=> array (\n  1 => 4660,\n)"),
        ("s>", b"\x12\x34", "=> array (\n  1 => 4660,\n)"),
        ("s>", b"\xff\xfe", "=> array (\n  1 => -2,\n)"),
        ("l>", b"\x01\x02\x03\x04", "=> array (\n  1 => 16909060,\n)"),
        ("L>a/x/Cb", b"\xff\xff\xff\xff\0\x05", "=> array (\n  'a' => 4294967295,\n  'b' => 5,\n)"),
        ("q>", b"\0\0\0\0\0\0\0\x02", "=> array (\n  1 => 2,\n)"),
        ("f>", b"\x3f\x80\0\0", "=> array (\n  1 => 1.0,\n)"),
        ("d>val", b"\x3f\xf0\0\0\0\0\0\0", "=> array (\n  'val' => 1.0,\n)"),
        ("n<", b"\0\0\0\0\0\0\0\0", "!! ValueError: Endianness modifier '<' cannot be applied to format code 'n' which already has inherent endianness"),
        ("g<", b"\0\0\0\0\0\0\0\0", "!! ValueError: Endianness modifier '<' cannot be applied to format code 'g' which already has inherent endianness"),
        ("X>", b"\0\0\0\0\0\0\0\0", "!! ValueError: Endianness modifier is not supported for format code 'X'"),
        ("H>", b"\0\0\0\0\0\0\0\0", "!! ValueError: Endianness modifier is not supported for format code 'H'"),
        ("i<", b"\0\0\0\0\0\0\0\0", "!! ValueError: Endianness modifier is not supported for format code 'i'"),
    ];
    for (fmt, data, exp) in master_up {
        v.push(up(fmt, data).expect(exp));
    }
    v
}

#[test]
fn oracle() {
    check("pack", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("pack", &cases());
}
