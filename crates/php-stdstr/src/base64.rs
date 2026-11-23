// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! MIME base64 — a port of `ext/standard/base64.c`.

const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PAD: u8 = b'=';

/// `base64_reverse_table`: `-1` for skippable whitespace (`\t`, `\n`,
/// `\r`, space), `-2` for any other non-alphabet byte.
fn reverse(c: u8) -> i8 {
    match c {
        b'A'..=b'Z' => i8::try_from(c - b'A').unwrap_or(-2),
        b'a'..=b'z' => i8::try_from(c - b'a' + 26).unwrap_or(-2),
        b'0'..=b'9' => i8::try_from(c - b'0' + 52).unwrap_or(-2),
        b'+' => 62,
        b'/' => 63,
        b'\t' | b'\n' | b'\r' | b' ' => -1,
        _ => -2,
    }
}

/// `php_base64_encode_ex(str, len, flags)`; `padding = false` is
/// `PHP_BASE64_NO_PADDING`.
pub fn base64_encode_ex(input: &[u8], padding: bool) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        out.push(TABLE[usize::from(c[0] >> 2)]);
        out.push(TABLE[usize::from(((c[0] & 0x03) << 4) | (c[1] >> 4))]);
        out.push(TABLE[usize::from(((c[1] & 0x0f) << 2) | (c[2] >> 6))]);
        out.push(TABLE[usize::from(c[2] & 0x3f)]);
    }
    match *chunks.remainder() {
        [a] => {
            out.push(TABLE[usize::from(a >> 2)]);
            out.push(TABLE[usize::from((a & 0x03) << 4)]);
            if padding {
                out.extend_from_slice(b"==");
            }
        }
        [a, b] => {
            out.push(TABLE[usize::from(a >> 2)]);
            out.push(TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))]);
            out.push(TABLE[usize::from((b & 0x0f) << 2)]);
            if padding {
                out.push(PAD);
            }
        }
        _ => {}
    }
    out
}

/// `base64_encode(string $string): string`.  The VM coerces the argument to
/// string.
pub fn base64_encode(input: &[u8]) -> Vec<u8> {
    base64_encode_ex(input, true)
}

/// `base64_decode(string $string, bool $strict = false): string|false`
/// (`php_base64_decode_ex`).
///
/// Non-strict mode skips every byte outside the alphabet and ignores `=`
/// wherever it appears.  Strict mode only skips `\t`, `\n`, `\r` and space,
/// and fails (`None` → PHP `false`) on any other foreign byte, on data
/// after padding, on a truncated final group (a single character), or on
/// wrong padding length.  The VM coerces `$string` to string and `$strict`
/// to bool.
pub fn base64_decode(input: &[u8], strict: bool) -> Option<Vec<u8>> {
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut i: usize = 0;
    let mut padding: usize = 0;
    // The partially assembled byte (`out[j]` in C).
    let mut cur: u8 = 0;
    for &c in input {
        if c == PAD {
            padding += 1;
            continue;
        }
        let ch = reverse(c);
        if strict {
            if ch == -1 {
                continue;
            }
            if ch == -2 || padding > 0 {
                return None;
            }
        } else if ch < 0 {
            continue;
        }
        let ch = ch.unsigned_abs();
        match i % 4 {
            0 => cur = ch << 2,
            1 => {
                out.push(cur | (ch >> 4));
                cur = (ch & 0x0f) << 4;
            }
            2 => {
                out.push(cur | (ch >> 2));
                cur = (ch & 0x03) << 6;
            }
            _ => out.push(cur | ch),
        }
        i += 1;
    }
    if strict && i % 4 == 1 {
        return None;
    }
    if strict && padding > 0 && (padding > 2 || (i + padding) % 4 != 0) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let e = base64_encode(s);
            assert_eq!(base64_decode(&e, true).as_deref(), Some(s));
        }
        assert_eq!(base64_encode(b"foob"), b"Zm9vYg==");
        assert_eq!(base64_encode_ex(b"foob", false), b"Zm9vYg");
    }

    #[test]
    fn strict_rules() {
        assert_eq!(
            base64_decode(b"Zm9vYg", true).as_deref(),
            Some(&b"foob"[..])
        );
        assert_eq!(base64_decode(b"Zm9vY", true), None);
        assert_eq!(base64_decode(b"Zm9vYg=", true), None);
        assert_eq!(
            base64_decode(b"Zm9v\nYg==", true).as_deref(),
            Some(&b"foob"[..])
        );
        assert_eq!(base64_decode(b"Zm9vYg==a", true), None);
        assert_eq!(base64_decode(b"Zm9v!Yg==", true), None);
        assert_eq!(
            base64_decode(b"Zm9v!Yg==a", false).as_deref(),
            Some(&b"foob\x06"[..])
        );
        assert_eq!(base64_decode(b"Zm9vYg===", true), None);
    }
}
