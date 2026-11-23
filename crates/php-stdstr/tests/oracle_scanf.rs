// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `sscanf()`.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{case, check, lit, regenerate, Case, V};
use php_stdstr::scanf::{sscanf, ScanValue, SscanfResult};

fn sv(v: Option<ScanValue>) -> V {
    match v {
        None => V::Null,
        Some(ScanValue::Int(i)) => V::Int(i),
        Some(ScanValue::Float(f)) => V::Float(f),
        Some(ScanValue::Str(s)) => V::Str(s),
    }
}

/// Array form: `sscanf($input, $format)`.
fn arr(input: &[u8], format: &[u8]) -> Case {
    case(&format!("sscanf({}, {})", lit(input), lit(format)), |_| {
        sscanf(input, format, 0).map(|r| match r {
            SscanfResult::Array(None) => V::Null,
            SscanfResult::Array(Some(v)) => V::from(v.into_iter().map(sv).collect::<Vec<_>>()),
            SscanfResult::Assigned { .. } => V::Str(b"unexpected".to_vec()),
        })
    })
}

/// By-reference form with `n` variables: returns `[count, $v0, ...]`.
fn byref(input: &[u8], format: &[u8], n: usize) -> Case {
    let vars: Vec<String> = (0..n).map(|i| format!("$v{i}")).collect();
    let php = format!(
        "(function () {{ $r = sscanf({}, {}, {}); return [$r, {}]; }})()",
        lit(input),
        lit(format),
        vars.join(", "),
        vars.join(", ")
    );
    case(&php, |_| {
        sscanf(input, format, n).map(|r| match r {
            SscanfResult::Assigned { values, count } => {
                let mut out = vec![V::Int(count)];
                out.extend(values.into_iter().map(sv));
                V::from(out)
            }
            SscanfResult::Array(_) => V::Str(b"unexpected".to_vec()),
        })
    })
}

const INPUTS: &[&[u8]] = &[
    b"",
    b" ",
    b"0",
    b"1",
    b"-1",
    b"+1",
    b"-",
    b"+",
    b"123abc",
    b"  42  ",
    b"-2147483648",
    b"2147483647",
    b"9223372036854775807",
    b"9223372036854775808",
    b"-9223372036854775808",
    b"-9223372036854775809",
    b"18446744073709551615",
    b"18446744073709551616",
    b"99999999999999999999999",
    b"0x1A",
    b"0X1a",
    b"0x",
    b"0xg",
    b"-0x1A",
    b"017",
    b"019",
    b"0129",
    b"08",
    b"1e5",
    b"1.5",
    b"-1.5e-3",
    b".5",
    b"-.5",
    b"1.",
    b".",
    b"1e",
    b"1e+",
    b"1e-x",
    b"1.2.3",
    b"1E3",
    b"1e999",
    b"abc def",
    b"\tabc",
    b"a\0b",
    b"129 12F 123B -123B 01ABC 1G",
    b"0123 -0123 +0123 0129 -0129 +0129",
    b"-11 +11 11 -11.234 +11.234 11.234",
    b"10.12345 10.12345E3 10.12345e3 -10.12345e4",
    b"X = A + B - C",
    b"123456789012345678901234567890123456789012345678901234567890123456789012345",
];

const FORMATS: &[&[u8]] = &[
    b"%d",
    b"%D",
    b"%i",
    b"%o",
    b"%x",
    b"%X",
    b"%u",
    b"%f",
    b"%e",
    b"%E",
    b"%g",
    b"%s",
    b"%c",
    b"%ld",
    b"%hd",
    b"%Lf",
    b"%3d",
    b"%2s",
    b"%3c",
    b"%2f",
    b"%1x",
    b"%[a-z]",
    b"%[^ ]",
    b"%d%s",
    b"%s %s",
    b"%d %d",
    b"%x %x %x %x %x %x",
    b"%o %o %o %o %o %o",
    b"%u %u %u %u %u %u",
    b"%e %e %e %e",
    b"%c = %c + %c - %c",
    b"%*d%s",
    b"%n",
    b"%d%n",
    b"%s%n%s",
    b"%%%d",
    b"%i%i",
    b"%f%f",
];

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for input in INPUTS {
        for format in FORMATS {
            v.push(arr(input, format));
        }
    }
    // Specific scenarios.
    let specific: &[(&[u8], &[u8])] = &[
        (
            b"Part: Widget Length: 111.53 Width: 22.345 Depth: 12.4",
            b"Part: %s Length: %f Width: %f Depth: %f",
        ),
        (
            b"Part: Widget Serial Number: 1234789 Stock: 25",
            b"Part: %s Serial Number: %d Stock: %d",
        ),
        (
            b"Part: Widget Serial Number: 1234789 Stock: 25",
            b"Part: %s Serial Number: %s Stock: %s",
        ),
        (b"Hello World", b"%s %s"),
        (b"age: 25 name: bob", b"age: %d name: %s"),
        (b"age:25", b"age: %d"),
        (b"age:  25", b"age:%d"),
        (b"abc", b"abd%s"),
        (b"abc", b"abc%s"),
        (b"abc", b"abc"),
        (b"ab", b"abc"),
        (b"abc", b"x%s"),
        (b"12 apples", b"%d %s %s"),
        (b"12", b"%d %s"),
        (b"12 ", b"%d %s"),
        (b"12", b"%d%%"),
        (b"12%", b"%d%%"),
        (b"12%5", b"%d%%%d"),
        (b"hello world", b"%2$s %1$s"),
        (b"hello world", b"%1$s %1$s"),
        (b"hello world", b"%3$s"),
        (b"hello world", b"%255$s"),
        (b"hello world", b"%256$s"),
        (b"hello world", b"%0$s"),
        (b"hello world", b"%4294967297$s"),
        (b"hello world", b"%1$s %s"),
        (b"hello world", b"%s %1$s"),
        (b"hello world", b"%*s %s"),
        (b"hello world", b"%*1$s"),
        (b"hello world", b"%s %*s"),
        (b"hello", b"%"),
        (b"hello", b"%5"),
        (b"hello", b"%l"),
        (b"hello", b"%G"),
        (b"hello", b"%y"),
        (b"hello", b"%["),
        (b"hello", b"%[a"),
        (b"hello", b"%[^"),
        (b"hello", b"%[]"),
        (b"hello", b"%[^]"),
        (b"a]b", b"%[]a]"),
        (b"a]b", b"%[^]]"),
        (b"a-b", b"%[a-]"),
        (b"a-b", b"%[-a]"),
        (b"zyx-", b"%[z-x]"),
        (b"abc-", b"%[a-c-]"),
        (b"abcdef", b"%[a-ce-f]"),
        (b"abcdef", b"%[^d-z]"),
        (b"abcdef", b"%3[a-z]"),
        (b"abcdef", b"%[a-z]%n"),
        (b"\xe9\xe9tx", b"%*[\x80-\xff]%s"),
        (b"ab\xe9", b"%[a-\xff]"),
        (b"\xe9tz", b"%*[^a-z]%s"),
        (b"123", b"%[a-z]"),
        (b"abc 123", b"%[a-z] %d"),
        (b"abc123", b"%[a-z]%[0-9]"),
        (b"   abc", b"%c"),
        (b"   abc", b" %c"),
        (b"abc", b"%c%c%c%c"),
        (b"abc", b"%0c"),
        (b"abc", b"%0s"),
        (b"abc", b"%0d"),
        (b"12345", b"%2d%3d"),
        (b"12345", b"%2d%*1d%d"),
        (b"0x1f", b"%x"),
        (b"0x1f", b"%i"),
        (b"0xx", b"%x"),
        (b"  \t\n 12", b"%d"),
        (b"12 \n 34", b"%d %d"),
        (b"12 \n 34", b"%d%d"),
        (b"1 2 3", b"%d %*d %d"),
        (b"a b", b"%s%n %s%n"),
        (b"", b""),
        (b"abc", b""),
        (b"", b"abc"),
        (b"", b" "),
        (b"", b"%n"),
        (b"", b"%*d"),
        (b"x", b"%*d"),
        (b"1", b"%*d"),
        (b"12abc", b"%d%s\0%d"),
        (b"1 2", b"%d\0%d"),
    ];
    for (input, format) in specific {
        v.push(arr(input, format));
    }
    // By-reference form.
    let refs: &[(&[u8], &[u8], usize)] = &[
        (b"12 apples", b"%d %s", 2),
        (b"12", b"%d %s", 2),
        (b"", b"%d %s", 2),
        (b"x", b"%d %s", 2),
        (b"12", b"%d", 1),
        (b"12", b"%d", 2),
        (b"12", b"%d %d", 1),
        (b"12", b"%d %d %d", 2),
        (b"hello world", b"%2$s %1$s", 2),
        (b"hello world", b"%2$s", 2),
        (b"hello world", b"%3$s", 2),
        (b"hello world", b"%1$s %1$s", 1),
        (b"hello world", b"%s %*s", 1),
        (b"hello world", b"%n", 1),
        (b"-1", b"%u", 1),
        (b"1.5 x", b"%f %c", 2),
        (b"", b"", 1),
        (b"abc", b"%[a-z]%d", 2),
    ];
    for (input, format, n) in refs {
        v.push(byref(input, format, *n));
    }
    v
}

#[test]
fn oracle() {
    check("scanf", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("scanf", &cases());
}
