// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Backslash escaping from `ext/standard/string.c`: `addslashes()`,
//! `stripslashes()`, `addcslashes()`, `stripcslashes()` and `quotemeta()`.

use crate::analysis::charmask;
use crate::error::Notices;

/// `php_addslashes()`; backs `addslashes(string $string): string`.
///
/// Prefixes `'`, `"` and `\` with a backslash and turns NUL into `\0`.
pub fn addslashes(s: &[u8]) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(s.len() + s.len() / 8);
    for &c in s {
        match c {
            0 => out.extend_from_slice(b"\\0"),
            b'\'' | b'"' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `php_stripslashes()`; backs `stripslashes(string $string): string`.
///
/// Removes one level of backslashes; `\0` becomes a NUL byte and a trailing
/// lone backslash is dropped.
pub fn stripslashes(s: &[u8]) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.iter();
    while let Some(&c) = it.next() {
        if c == b'\\' {
            if let Some(&n) = it.next() {
                out.push(if n == b'0' { 0 } else { n });
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `php_addcslashes_str()`; backs `addcslashes(string $string, string
/// $characters): string`.
///
/// Every byte listed in `characters` (a [`charmask`] list, so `a..z` ranges
/// work and malformed ranges warn as `addcslashes()`) is escaped: printable
/// bytes get a leading backslash, `\n \t \r \a \v \b \f` use their C
/// escapes and other control / high bytes become three-digit octal.
pub fn addcslashes(s: &[u8], characters: &[u8], notices: &mut Notices) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    if characters.is_empty() {
        return s.to_vec();
    }
    let mask = charmask(characters, "addcslashes", notices);
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most twice the input"
    )]
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        if !mask[usize::from(c)] {
            out.push(c);
            continue;
        }
        out.push(b'\\');
        if (32..=126).contains(&c) {
            out.push(c);
            continue;
        }
        match c {
            b'\n' => out.push(b'n'),
            b'\t' => out.push(b't'),
            b'\r' => out.push(b'r'),
            0x07 => out.push(b'a'),
            0x0b => out.push(b'v'),
            0x08 => out.push(b'b'),
            0x0c => out.push(b'f'),
            _ => {
                // Three octal digits, including leading zeros.
                out.push((c >> 6) + b'0');
                out.push(((c >> 3) & 7) + b'0');
                out.push((c & 7) + b'0');
            }
        }
    }
    out
}

/// `php_stripcslashes()`; backs `stripcslashes(string $string): string`.
///
/// Understands `\n \r \a \t \v \b \f \\`, `\xH`/`\xHH` hex escapes and
/// one-to-three digit octal escapes (truncated to a byte); any other escaped
/// byte is kept without its backslash.
pub fn stripcslashes(s: &[u8]) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let end = s.len();
    let mut i = 0;
    while i < end {
        if s[i] != b'\\' || i + 1 >= end {
            out.push(s[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = s[i];
        let simple = match c {
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b'a' => Some(0x07),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'\\' => Some(b'\\'),
            _ => None,
        };
        if let Some(byte) = simple {
            out.push(byte);
            i += 1;
            continue;
        }
        if c == b'x' && i + 1 < end && s[i + 1].is_ascii_hexdigit() {
            let mut hex = hex_val(s[i + 1]);
            i += 1;
            if i + 1 < end && s[i + 1].is_ascii_hexdigit() {
                hex = hex * 16 + hex_val(s[i + 1]);
                i += 1;
            }
            out.push(hex);
            i += 1;
            continue;
        }
        let mut digits = 0;
        let mut octal: u32 = 0;
        while i < end && (b'0'..=b'7').contains(&s[i]) && digits < 3 {
            octal = octal * 8 + u32::from(s[i] - b'0');
            i += 1;
            digits += 1;
        }
        if digits > 0 {
            // `(char)strtol(...)`: keep the low byte.
            out.push(octal.to_le_bytes()[0]);
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// Backs `quotemeta(string $string): string`: backslash-escapes
/// `. \ + * ? [ ^ ] $ ( )`.
pub fn quotemeta(s: &[u8]) -> Vec<u8> {
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most twice the input"
    )]
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        if matches!(
            c,
            b'.' | b'\\' | b'+' | b'*' | b'?' | b'[' | b'^' | b']' | b'$' | b'(' | b')'
        ) {
            out.push(b'\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slashes() {
        assert_eq!(addslashes(b"O'Re\"il\\ly\0"), b"O\\'Re\\\"il\\\\ly\\0");
        assert_eq!(stripslashes(b"O\\'Re\\\\x\\0y\\"), b"O'Re\\x\0y");
        let mut n = Vec::new();
        assert_eq!(
            addcslashes(b"foo[bar]\n\x01\xff", b"A..Z\0..\x1f\x7f..\xff[]", &mut n),
            b"foo\\[bar\\]\\n\\001\\377"
        );
        assert!(n.is_empty());
        assert_eq!(
            stripcslashes(b"\\x41\\101\\n\\q\\x\\777\\"),
            b"AA\nqx\xff\\"
        );
        assert_eq!(quotemeta(b"1+1=2?"), b"1\\+1=2\\?");
    }
}
