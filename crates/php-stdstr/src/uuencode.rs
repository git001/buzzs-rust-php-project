// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! uuencoding — a port of `ext/standard/uuencode.c`.

use crate::error::{Notice, Notices};

/// `PHP_UU_ENC(c)`.
fn enc(c: u8) -> u8 {
    if c == 0 {
        b'`'
    } else {
        (c & 0o77) + b' '
    }
}

/// `PHP_UU_DEC(c)`.
fn dec(c: u8) -> u8 {
    c.wrapping_sub(b' ') & 0o77
}

/// `php_uuencode()` — backs `convert_uuencode(string $string): string`.
///
/// Lines of up to 45 input bytes, each prefixed by an encoded length
/// character, terminated by a `` ` `` line.  The VM coerces the argument to
/// string.  (An empty string encodes to `` "`\n" `` preceded by a newline
/// rule identical to PHP's.)
pub fn convert_uuencode(src: &[u8]) -> Vec<u8> {
    // Bytes past the end read as the C string's NUL terminator.
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let enc_c2 = |i: usize| enc(((at(i) << 4) & 0o60) | ((at(i + 1) >> 4) & 0o17));
    let enc_c3 = |i: usize| enc(((at(i + 1) << 2) & 0o74) | ((at(i + 2) >> 6) & 0o3));

    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(src.len() / 2 * 3 + 46);
    let e = src.len();
    let mut s = 0usize;
    let mut len = 45usize;
    while s + 3 < e {
        let mut ee = s + len;
        if ee > e {
            ee = e;
            len = ee - s;
            if len % 3 != 0 {
                ee = s + len / 3 * 3;
            }
        }
        // `len` is at most 45 here.
        out.push(enc(u8::try_from(len).unwrap_or(0)));
        while s < ee {
            out.push(enc(at(s) >> 2));
            out.push(enc_c2(s));
            out.push(enc_c3(s));
            out.push(enc(at(s + 2) & 0o77));
            s += 3;
        }
        if len == 45 {
            out.push(b'\n');
        }
    }
    if s < e {
        if len == 45 {
            out.push(enc(u8::try_from(e - s).unwrap_or(0)));
            len = 0;
        }
        out.push(enc(at(s) >> 2));
        out.push(enc_c2(s));
        out.push(if e - s > 1 { enc_c3(s) } else { enc(0) });
        out.push(if e - s > 2 {
            enc(at(s + 2) & 0o77)
        } else {
            enc(0)
        });
    }
    if len < 45 {
        out.push(b'\n');
    }
    out.push(enc(0));
    out.push(b'\n');
    out
}

/// `php_uudecode()`: `None` on malformed input.
pub fn uudecode_raw(src: &[u8]) -> Option<Vec<u8>> {
    if src.is_empty() {
        return None;
    }
    // Reads past the end see the NUL terminator.
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let e = src.len();
    let mut s = 0usize;
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(src.len() * 3 / 4 + 1);
    let mut total_len = 0usize;
    while s < e {
        let len = usize::from(dec(at(s)));
        s += 1;
        if len == 0 {
            break;
        }
        if len > src.len() {
            return None;
        }
        total_len += len;
        // floor(len * 1.33) for len < 45
        let span = if len == 45 { 60 } else { len * 133 / 100 };
        let ee = s + span;
        if ee > e {
            return None;
        }
        while s < ee {
            if s + 4 > e {
                return None;
            }
            out.push((dec(at(s)) << 2) | (dec(at(s + 1)) >> 4));
            out.push((dec(at(s + 1)) << 4) | (dec(at(s + 2)) >> 2));
            out.push((dec(at(s + 2)) << 6) | dec(at(s + 3)));
            s += 4;
        }
        if len < 45 {
            break;
        }
        // skip \n
        s += 1;
    }
    if total_len > out.len() {
        out.push((dec(at(s)) << 2) | (dec(at(s + 1)) >> 4));
        if total_len > 1 {
            out.push((dec(at(s + 1)) << 4) | (dec(at(s + 2)) >> 2));
            if total_len > 2 {
                out.push((dec(at(s + 2)) << 6) | dec(at(s + 3)));
            }
        }
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: total_len never exceeds the decoded input"
    )]
    out.resize(total_len, 0);
    Some(out)
}

/// `convert_uudecode(string $string): string|false`.
///
/// On malformed (or empty) input pushes the warning
/// `convert_uudecode(): Argument #1 ($string) is not a valid uuencoded
/// string` and returns `None` (PHP `false`).  The VM coerces the argument to
/// string.
pub fn convert_uudecode(src: &[u8], notices: &mut Notices) -> Option<Vec<u8>> {
    let r = uudecode_raw(src);
    if r.is_none() {
        notices.push(Notice::warning(
            "convert_uudecode",
            "Argument #1 ($string) is not a valid uuencoded string",
        ));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data: Vec<u8> = (0..200u8).collect();
        for n in [0usize, 1, 2, 3, 4, 44, 45, 46, 90, 91, 200] {
            let enc = convert_uuencode(&data[..n]);
            let mut notices = Vec::new();
            let dec = convert_uudecode(&enc, &mut notices);
            if n == 0 {
                assert!(dec.is_some() || !notices.is_empty());
            } else {
                assert_eq!(dec.as_deref(), Some(&data[..n]), "n={n}");
            }
        }
        assert_eq!(
            convert_uuencode(b"test\ntext text text\r\n"),
            b"5=&5S=`IT97AT('1E>'0@=&5X=`T*\n`\n"
        );
    }

    #[test]
    fn invalid() {
        let mut n = Vec::new();
        assert_eq!(convert_uudecode(b"", &mut n), None);
        assert_eq!(
            n[0].to_string(),
            "convert_uudecode(): Argument #1 ($string) is not a valid uuencoded string"
        );
    }
}
