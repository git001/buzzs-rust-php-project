// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Quoted-printable (RFC 2045) — a port of `ext/standard/quot_print.c`.

const HEX: &[u8; 16] = b"0123456789ABCDEF";
/// `PHP_QPRINT_MAXL`
const MAXL: usize = 75;

/// `php_quot_print_encode()` — backs
/// `quoted_printable_encode(string $string): string`.
///
/// Control characters, DEL, bytes ≥ 0x80, `=` and a space before CR are
/// encoded as `=XX`; lines are soft-broken with `=\r\n` so that no line
/// exceeds 76 characters, never splitting a UTF-8 lead byte from the room
/// its sequence needs.  `\r\n` pairs pass through and reset the column.
/// The VM coerces the argument to string.
pub fn quoted_printable_encode(s: &[u8]) -> Vec<u8> {
    // Reads past the end see the NUL terminator.
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most three times the input"
    )]
    let mut out = Vec::with_capacity(s.len() * 3);
    let mut lp: usize = 0;
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        i += 1;
        let remaining = s.len() - i;
        if c == b'\r' && at(i) == b'\n' && remaining > 0 {
            out.extend_from_slice(b"\r\n");
            i += 1;
            lp = 0;
        } else if c.is_ascii_control()
            || c & 0x80 != 0
            || c == b'='
            || (c == b' ' && at(i) == b'\r')
        {
            lp += 3;
            if (lp > MAXL && c <= 0x7f)
                || (c > 0x7f && c <= 0xdf && lp + 3 > MAXL)
                || (c > 0xdf && c <= 0xef && lp + 6 > MAXL)
                || (c > 0xef && c <= 0xf4 && lp + 9 > MAXL)
            {
                out.extend_from_slice(b"=\r\n");
                lp = 3;
            }
            out.push(b'=');
            out.push(HEX[usize::from(c >> 4)]);
            out.push(HEX[usize::from(c & 0xf)]);
        } else {
            lp += 1;
            if lp > MAXL {
                out.extend_from_slice(b"=\r\n");
                lp = 1;
            }
            out.push(c);
        }
    }
    out
}

fn hex2int(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'A'..=b'F' => c - b'A' + 10,
        _ => c.wrapping_sub(b'a').wrapping_add(10),
    }
}

/// `quoted_printable_decode(string $string): string`.
///
/// Works on the string up to its first NUL byte (like the C code).  `=XX`
/// (either case) decodes; `=` followed by optional spaces/tabs and a line
/// break (CRLF, CR or LF) or the end of input is a soft line break; any
/// other `=` is copied literally.  The VM coerces the argument to string.
pub fn quoted_printable_decode(s: &[u8]) -> Vec<u8> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0usize;
    while at(i) != 0 {
        if at(i) == b'=' {
            if at(i + 1).is_ascii_hexdigit() && at(i + 2).is_ascii_hexdigit() {
                out.push((hex2int(at(i + 1)) << 4).wrapping_add(hex2int(at(i + 2))));
                i += 3;
            } else {
                let mut k = 1;
                while at(i + k) == b' ' || at(i + k) == b'\t' {
                    k += 1;
                }
                match at(i + k) {
                    0 => i += k,
                    b'\r' if at(i + k + 1) == b'\n' => i += k + 2,
                    b'\r' | b'\n' => i += k + 1,
                    _ => {
                        out.push(at(i));
                        i += 1;
                    }
                }
            }
        } else {
            out.push(at(i));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode() {
        assert_eq!(
            quoted_printable_encode(b"a=b \r\nc\xc3\xa9"),
            b"a=3Db=20\r\nc=C3=A9"
        );
        let long = vec![b'x'; 80];
        let e = quoted_printable_encode(&long);
        assert_eq!(&e[75..78], b"=\r\n");
    }

    #[test]
    fn decode() {
        assert_eq!(
            quoted_printable_decode(b"a=3db=\r\nc=  \nd=\re=zz="),
            b"a=bcde=zz"
        );
        assert_eq!(quoted_printable_decode(b"a\0=41"), b"a");
    }
}
