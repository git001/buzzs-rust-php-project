// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `htmlspecialchars()`, `htmlentities()`, their decoders and
//! `get_html_translation_table()` — a port of `ext/standard/html.c`.
//!
//! # Arguments the VM has to prepare
//!
//! * `flags` is the raw PHP `int` argument (default
//!   [`DEFAULT_FLAGS`] = `ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401`); only
//!   its low bits are meaningful, like the C code's `(int)` cast.
//! * `charset` is the *effective* charset hint: compute it with
//!   [`effective_charset_hint`] from the (nullable) argument and the
//!   `internal_encoding` / `default_charset` ini values. `None` means
//!   UTF-8. Resolution (and the "not supported" warning) happens lazily,
//!   exactly when PHP performs it.
//! * `double_encode` defaults to `true`.

use crate::error::{Notice, Notices};
use crate::html_tables::{
    EntityRow, INV_HTML4, INV_HTML5, TABLE_HTML4, TABLE_HTML5, TO_UNI_CP866, TO_UNI_ISO88591,
    TO_UNI_ISO885915, TO_UNI_ISO88595, TO_UNI_KOI8R, TO_UNI_MACROMAN, TO_UNI_WIN1251,
    TO_UNI_WIN1252, UNIMAP_CP866, UNIMAP_ISO885915, UNIMAP_KOI8R, UNIMAP_MACROMAN, UNIMAP_WIN1251,
    UNIMAP_WIN1252,
};

/// `ENT_HTML_QUOTE_NONE` / `ENT_NOQUOTES`.
pub const ENT_NOQUOTES: i64 = 0;
/// `ENT_HTML_QUOTE_SINGLE`.
pub const ENT_HTML_QUOTE_SINGLE: i64 = 1;
/// `ENT_HTML_QUOTE_DOUBLE` / `ENT_COMPAT`.
pub const ENT_COMPAT: i64 = 2;
/// `ENT_QUOTES`.
pub const ENT_QUOTES: i64 = 3;
/// `ENT_IGNORE`: drop invalid code unit sequences.
pub const ENT_IGNORE: i64 = 4;
/// `ENT_SUBSTITUTE`: replace invalid code unit sequences with U+FFFD.
pub const ENT_SUBSTITUTE: i64 = 8;
/// `ENT_HTML401` document type.
pub const ENT_HTML401: i64 = 0;
/// `ENT_XML1` document type.
pub const ENT_XML1: i64 = 16;
/// `ENT_XHTML` document type.
pub const ENT_XHTML: i64 = 32;
/// `ENT_HTML5` document type.
pub const ENT_HTML5: i64 = 48;
/// `ENT_DISALLOWED`: replace code points invalid for the document type.
pub const ENT_DISALLOWED: i64 = 128;
/// `HTML_SPECIALCHARS` (`get_html_translation_table()` table selector).
pub const HTML_SPECIALCHARS: i64 = 0;
/// `HTML_ENTITIES`.
pub const HTML_ENTITIES: i64 = 1;
/// Default `$flags` of every function in this module.
pub const DEFAULT_FLAGS: i64 = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401;

const QUOTE_SINGLE: u32 = 1;
const QUOTE_DOUBLE: u32 = 2;
const IGNORE_ERRORS: u32 = 4;
const SUBSTITUTE_ERRORS: u32 = 8;
const DOC_TYPE_MASK: u32 = 16 | 32;
const DOC_HTML401: u32 = 0;
const DOC_XML1: u32 = 16;
const DOC_XHTML: u32 = 32;
const DOC_HTML5: u32 = 16 | 32;
const SUBSTITUTE_DISALLOWED: u32 = 128;

/// Longest entity name (without `&` and `;`).
const LONGEST_ENTITY_LENGTH: usize = 31;
/// Longest codeset name in the charset map.
const LONGEST_CODESET_LENGTH: usize = 12;

/// The character sets html.c understands (`enum entity_charset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Charset {
    /// UTF-8.
    Utf8,
    /// ISO-8859-1.
    Iso8859_1,
    /// Windows-1252.
    Cp1252,
    /// ISO-8859-15.
    Iso8859_15,
    /// Windows-1251.
    Cp1251,
    /// ISO-8859-5.
    Iso8859_5,
    /// CP866.
    Cp866,
    /// `MacRoman`.
    MacRoman,
    /// KOI8-R.
    Koi8R,
    /// BIG5.
    Big5,
    /// GB2312 (EUC-CN).
    Gb2312,
    /// BIG5-HKSCS.
    Big5Hkscs,
    /// `Shift_JIS`.
    Sjis,
    /// EUC-JP.
    EucJp,
}

/// `charset_map` from `html_tables.h` (lowercase codesets).
const CHARSET_MAP: &[(&[u8], Charset)] = &[
    (b"iso-8859-1", Charset::Iso8859_1),
    (b"iso8859-1", Charset::Iso8859_1),
    (b"iso-8859-15", Charset::Iso8859_15),
    (b"iso8859-15", Charset::Iso8859_15),
    (b"cp1252", Charset::Cp1252),
    (b"windows-1252", Charset::Cp1252),
    (b"1252", Charset::Cp1252),
    (b"big5", Charset::Big5),
    (b"950", Charset::Big5),
    (b"gb2312", Charset::Gb2312),
    (b"936", Charset::Gb2312),
    (b"big5-hkscs", Charset::Big5Hkscs),
    (b"shift_jis", Charset::Sjis),
    (b"sjis", Charset::Sjis),
    (b"932", Charset::Sjis),
    (b"sjis-win", Charset::Sjis),
    (b"cp932", Charset::Sjis),
    (b"eucjp", Charset::EucJp),
    (b"euc-jp", Charset::EucJp),
    (b"eucjp-win", Charset::EucJp),
    (b"koi8-r", Charset::Koi8R),
    (b"koi8-ru", Charset::Koi8R),
    (b"koi8r", Charset::Koi8R),
    (b"cp1251", Charset::Cp1251),
    (b"windows-1251", Charset::Cp1251),
    (b"win-1251", Charset::Cp1251),
    (b"iso8859-5", Charset::Iso8859_5),
    (b"iso-8859-5", Charset::Iso8859_5),
    (b"cp866", Charset::Cp866),
    (b"866", Charset::Cp866),
    (b"ibm866", Charset::Cp866),
    (b"macroman", Charset::MacRoman),
];

impl Charset {
    /// `CHARSET_UNICODE_COMPAT`: code units are Unicode code points.
    const fn unicode_compat(self) -> bool {
        matches!(self, Self::Utf8 | Self::Iso8859_1)
    }

    /// `CHARSET_SINGLE_BYTE`.
    const fn single_byte(self) -> bool {
        !matches!(self, Self::Utf8) && !self.partial_support()
    }

    /// `CHARSET_PARTIAL_SUPPORT`: multi-byte charsets other than UTF-8.
    const fn partial_support(self) -> bool {
        matches!(
            self,
            Self::Big5 | Self::Gb2312 | Self::Big5Hkscs | Self::Sjis | Self::EucJp
        )
    }

    /// `enc_to_uni_index[charset]`.
    fn to_uni_table(self) -> Option<&'static [u16; 256]> {
        match self {
            Self::Iso8859_1 => Some(&TO_UNI_ISO88591),
            Self::Cp1252 => Some(&TO_UNI_WIN1252),
            Self::Iso8859_15 => Some(&TO_UNI_ISO885915),
            Self::Cp1251 => Some(&TO_UNI_WIN1251),
            Self::Iso8859_5 => Some(&TO_UNI_ISO88595),
            Self::Cp866 => Some(&TO_UNI_CP866),
            Self::MacRoman => Some(&TO_UNI_MACROMAN),
            Self::Koi8R => Some(&TO_UNI_KOI8R),
            _ => None,
        }
    }

    /// `lookup_charset()` without the warning: the charset a (NUL-free,
    /// non-UTF-8) hint names, if supported.
    fn lookup(hint: &[u8]) -> Option<Self> {
        if hint.len() > LONGEST_CODESET_LENGTH {
            return None;
        }
        let lower = hint.to_ascii_lowercase();
        CHARSET_MAP
            .iter()
            .find(|(name, _)| *name == lower.as_slice())
            .map(|&(_, cs)| cs)
    }
}

/// Truncates a C string argument at its first NUL byte.
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |p| &s[..p])
}

/// Computes the effective charset hint the functions of this module take:
/// the explicit argument when it is a non-empty C string, else
/// `internal_encoding`, else `default_charset` (`get_default_charset()`).
/// `None` (all empty) resolves to UTF-8.
pub fn effective_charset_hint<'a>(
    arg: Option<&'a [u8]>,
    internal_encoding: Option<&'a [u8]>,
    default_charset: Option<&'a [u8]>,
) -> Option<&'a [u8]> {
    [arg, internal_encoding, default_charset]
        .into_iter()
        .flatten()
        .map(c_str)
        .find(|s| !s.is_empty())
}

/// `determine_charset()`: resolves an effective hint, warning (unless
/// `quiet`) `Charset "X" is not supported, assuming UTF-8`.
pub fn determine_charset(
    hint: Option<&[u8]>,
    quiet: bool,
    func: &'static str,
    notices: &mut Notices,
) -> Charset {
    let Some(hint) = hint.map(c_str).filter(|h| !h.is_empty()) else {
        return Charset::Utf8;
    };
    if hint.eq_ignore_ascii_case(b"utf-8") {
        return Charset::Utf8;
    }
    if let Some(cs) = Charset::lookup(hint) {
        return cs;
    }
    if !quiet {
        notices.push(Notice::warning(
            func,
            format!(
                "Charset \"{}\" is not supported, assuming UTF-8",
                String::from_utf8_lossy(hint)
            ),
        ));
    }
    Charset::Utf8
}

const fn utf8_lead(c: u8) -> bool {
    c < 0x80 || (c >= 0xC2 && c <= 0xF4)
}

const fn utf8_trail(c: u8) -> bool {
    c >= 0x80 && c <= 0xBF
}

const fn gb2312_lead(c: u8) -> bool {
    c != 0x8E && c != 0x8F && c != 0xA0 && c != 0xFF
}

const fn gb2312_trail(c: u8) -> bool {
    c >= 0xA1 && c <= 0xFE
}

const fn sjis_lead(c: u8) -> bool {
    c != 0x80 && c != 0xA0 && c < 0xFD
}

const fn sjis_trail(c: u8) -> bool {
    c >= 0x40 && c != 0x7F && c < 0xFD
}

/// Result of decoding one character: the code (`Ok`) or failure (`Err`),
/// plus the cursor after it (after a failure, the cursor skips the bytes of
/// the reported illegal sequence).
type NextChar = (Result<u32, ()>, usize);

/// UTF-8 case of `get_next_char()` (UTR #36 strategy 2).
fn next_char_utf8(s: &[u8], pos: usize) -> NextChar {
    let fail = |adv: usize| (Err(()), pos + adv);
    let len = s.len();
    let c = s[pos];
    let at = |i: usize| u32::from(s[pos + i]);
    if c < 0x80 {
        (Ok(u32::from(c)), pos + 1)
    } else if c < 0xC2 {
        fail(1)
    } else if c < 0xE0 {
        if len - pos < 2 {
            return fail(1);
        }
        if !utf8_trail(s[pos + 1]) {
            return fail(if utf8_lead(s[pos + 1]) { 1 } else { 2 });
        }
        let ch = ((u32::from(c) & 0x1F) << 6) | (at(1) & 0x3F);
        if ch < 0x80 {
            return fail(2);
        }
        (Ok(ch), pos + 2)
    } else if c < 0xF0 {
        let avail = len - pos;
        if avail < 3 || !utf8_trail(s[pos + 1]) || !utf8_trail(s[pos + 2]) {
            return if avail < 2 || utf8_lead(s[pos + 1]) {
                fail(1)
            } else if avail < 3 || utf8_lead(s[pos + 2]) {
                fail(2)
            } else {
                fail(3)
            };
        }
        let ch = ((u32::from(c) & 0x0F) << 12) | ((at(1) & 0x3F) << 6) | (at(2) & 0x3F);
        if ch < 0x800 || (0xD800..=0xDFFF).contains(&ch) {
            return fail(3);
        }
        (Ok(ch), pos + 3)
    } else if c < 0xF5 {
        let avail = len - pos;
        if avail < 4
            || !utf8_trail(s[pos + 1])
            || !utf8_trail(s[pos + 2])
            || !utf8_trail(s[pos + 3])
        {
            return if avail < 2 || utf8_lead(s[pos + 1]) {
                fail(1)
            } else if avail < 3 || utf8_lead(s[pos + 2]) {
                fail(2)
            } else if avail < 4 || utf8_lead(s[pos + 3]) {
                fail(3)
            } else {
                fail(4)
            };
        }
        let ch = ((u32::from(c) & 0x07) << 18)
            | ((at(1) & 0x3F) << 12)
            | ((at(2) & 0x3F) << 6)
            | (at(3) & 0x3F);
        if !(0x10000..=0x0010_FFFF).contains(&ch) {
            return fail(4);
        }
        (Ok(ch), pos + 4)
    } else {
        fail(1)
    }
}

/// `get_next_char()`: decodes the character at `pos` in `charset`.
#[allow(clippy::too_many_lines)]
fn next_char(charset: Charset, s: &[u8], pos: usize) -> NextChar {
    let fail = |adv: usize| (Err(()), pos + adv);
    if pos >= s.len() {
        return fail(1);
    }
    let len = s.len();
    let c = s[pos];
    let pair = |n: u8| (u32::from(c) << 8) | u32::from(n);
    match charset {
        Charset::Utf8 => next_char_utf8(s, pos),
        Charset::Big5 => {
            if (0x81..=0xFE).contains(&c) {
                if len - pos < 2 {
                    return fail(1);
                }
                let n = s[pos + 1];
                if (0x40..=0x7E).contains(&n) || (0xA1..=0xFE).contains(&n) {
                    (Ok(pair(n)), pos + 2)
                } else {
                    fail(1)
                }
            } else {
                (Ok(u32::from(c)), pos + 1)
            }
        }
        Charset::Big5Hkscs => {
            if (0x81..=0xFE).contains(&c) {
                if len - pos < 2 {
                    return fail(1);
                }
                let n = s[pos + 1];
                if (0x40..=0x7E).contains(&n) || (0xA1..=0xFE).contains(&n) {
                    (Ok(pair(n)), pos + 2)
                } else if n != 0x80 && n != 0xFF {
                    fail(1)
                } else {
                    fail(2)
                }
            } else {
                (Ok(u32::from(c)), pos + 1)
            }
        }
        Charset::Gb2312 => {
            if (0xA1..=0xFE).contains(&c) {
                if len - pos < 2 {
                    return fail(1);
                }
                let n = s[pos + 1];
                if gb2312_trail(n) {
                    (Ok(pair(n)), pos + 2)
                } else if gb2312_lead(n) {
                    fail(1)
                } else {
                    fail(2)
                }
            } else if gb2312_lead(c) {
                (Ok(u32::from(c)), pos + 1)
            } else {
                fail(1)
            }
        }
        Charset::Sjis => {
            if (0x81..=0x9F).contains(&c) || (0xE0..=0xFC).contains(&c) {
                if len - pos < 2 {
                    return fail(1);
                }
                let n = s[pos + 1];
                if sjis_trail(n) {
                    (Ok(pair(n)), pos + 2)
                } else if sjis_lead(n) {
                    fail(1)
                } else {
                    fail(2)
                }
            } else if c < 0x80 || (0xA1..=0xDF).contains(&c) {
                (Ok(u32::from(c)), pos + 1)
            } else {
                fail(1)
            }
        }
        Charset::EucJp => {
            if (0xA1..=0xFE).contains(&c) || c == 0x8E {
                if len - pos < 2 {
                    return fail(1);
                }
                let n = s[pos + 1];
                let ok = if c == 0x8E {
                    (0xA1..=0xDF).contains(&n)
                } else {
                    (0xA1..=0xFE).contains(&n)
                };
                if ok {
                    (Ok(pair(n)), pos + 2)
                } else {
                    fail(if n != 0xA0 && n != 0xFF { 1 } else { 2 })
                }
            } else if c == 0x8F {
                let avail = len - pos;
                let hi = |b: u8| (0xA1..=0xFE).contains(&b);
                if avail < 3 || !hi(s[pos + 1]) || !hi(s[pos + 2]) {
                    if avail < 2 || (s[pos + 1] != 0xA0 && s[pos + 1] != 0xFF) {
                        fail(1)
                    } else if avail < 3 || (s[pos + 2] != 0xA0 && s[pos + 2] != 0xFF) {
                        fail(2)
                    } else {
                        fail(3)
                    }
                } else {
                    let ch =
                        (u32::from(c) << 16) | (u32::from(s[pos + 1]) << 8) | u32::from(s[pos + 2]);
                    (Ok(ch), pos + 3)
                }
            } else if c != 0xA0 && c != 0xFF {
                (Ok(u32::from(c)), pos + 1)
            } else {
                fail(1)
            }
        }
        _ => (Ok(u32::from(c)), pos + 1),
    }
}

/// `php_next_utf8_char()`: decodes the UTF-8 character at `cursor`,
/// returning the code point (or `None` for an illegal sequence) and the
/// cursor after it.
pub fn next_utf8_char(s: &[u8], cursor: usize) -> (Option<u32>, usize) {
    let (r, next) = next_char(Charset::Utf8, s, cursor);
    (r.ok(), next)
}

/// `php_utf32_utf8()`.
fn push_utf8(out: &mut Vec<u8>, k: u32) {
    // Each pushed value is masked to fit a byte.
    #[allow(clippy::cast_possible_truncation)]
    let b = |v: u32| v as u8;
    if k < 0x80 {
        out.push(b(k));
    } else if k < 0x800 {
        out.extend_from_slice(&[b(0xC0 | (k >> 6)), b(0x80 | (k & 0x3F))]);
    } else if k < 0x10000 {
        out.extend_from_slice(&[
            b(0xE0 | (k >> 12)),
            b(0x80 | ((k >> 6) & 0x3F)),
            b(0x80 | (k & 0x3F)),
        ]);
    } else {
        out.extend_from_slice(&[
            b(0xF0 | (k >> 18)),
            b(0x80 | ((k >> 12) & 0x3F)),
            b(0x80 | ((k >> 6) & 0x3F)),
            b(0x80 | (k & 0x3F)),
        ]);
    }
}

/// `write_octet_sequence()`: UTF-8 encodes for UTF-8, otherwise writes the
/// low byte (the C code stores `code` into an `unsigned char`).
fn write_octets(out: &mut Vec<u8>, charset: Charset, code: u32) {
    if charset == Charset::Utf8 {
        push_utf8(out, code);
    } else {
        out.push(code.to_le_bytes()[0]);
    }
}

/// `unimap_bsearch()`.
fn unimap(table: &[(u16, u8)], code: u32) -> Option<u32> {
    let code = u16::try_from(code).ok()?;
    table
        .binary_search_by_key(&code, |&(u, _)| u)
        .ok()
        .map(|i| u32::from(table[i].1))
        .filter(|&c| c != 0)
}

/// `map_from_unicode()`: maps a code point to the charset's code.
fn map_from_unicode(code: u32, charset: Charset) -> Option<u32> {
    let over_7f = |table: &[(u16, u8)]| {
        if code <= 0x7F {
            Some(code)
        } else {
            unimap(table, code)
        }
    };
    match charset {
        Charset::Iso8859_1 => (code <= 0xFF).then_some(code),
        Charset::Iso8859_5 => {
            if code <= 0xA0 || code == 0xAD {
                Some(code)
            } else if code == 0x2116 {
                Some(0xF0)
            } else if code == 0xA7 {
                Some(0xFD)
            } else if (0x0401..=0x045F).contains(&code) {
                if code == 0x040D || code == 0x0450 || code == 0x045D {
                    None
                } else {
                    Some(code - 0x360)
                }
            } else {
                None
            }
        }
        Charset::Iso8859_15 => {
            if code < 0xA4 || (code > 0xBE && code <= 0xFF) {
                Some(code)
            } else {
                unimap(&UNIMAP_ISO885915, code)
            }
        }
        Charset::Cp1252 => {
            if code <= 0x7F || (0xA0..=0xFF).contains(&code) {
                Some(code)
            } else {
                unimap(&UNIMAP_WIN1252, code)
            }
        }
        Charset::MacRoman => {
            if code == 0x7F {
                None
            } else {
                over_7f(&UNIMAP_MACROMAN)
            }
        }
        Charset::Cp1251 => over_7f(&UNIMAP_WIN1251),
        Charset::Koi8R => over_7f(&UNIMAP_KOI8R),
        Charset::Cp866 => over_7f(&UNIMAP_CP866),
        Charset::Sjis | Charset::EucJp => {
            ((0x20..=0x7D).contains(&code) && code != 0x5C).then_some(code)
        }
        Charset::Big5 | Charset::Big5Hkscs | Charset::Gb2312 => {
            (0x20..=0x7D).contains(&code).then_some(code)
        }
        Charset::Utf8 => None,
    }
}

/// `unicode_cp_is_allowed()`.
fn unicode_cp_is_allowed(cp: u32, doctype: u32) -> bool {
    match doctype {
        DOC_HTML401 => {
            (0x20..=0x7E).contains(&cp)
                || cp == 0x0A
                || cp == 0x09
                || cp == 0x0D
                || (0xA0..=0xD7FF).contains(&cp)
                || (0xE000..=0x0010_FFFF).contains(&cp)
        }
        DOC_HTML5 => {
            (0x20..=0x7E).contains(&cp)
                || ((0x09..=0x0D).contains(&cp) && cp != 0x0B)
                || (0xA0..=0xD7FF).contains(&cp)
                || ((0xE000..=0x0010_FFFF).contains(&cp)
                    && (cp & 0xFFFF) < 0xFFFE
                    && !(0xFDD0..=0xFDEF).contains(&cp))
        }
        DOC_XHTML | DOC_XML1 => {
            (0x20..=0xD7FF).contains(&cp)
                || cp == 0x0A
                || cp == 0x09
                || cp == 0x0D
                || ((0xE000..=0x0010_FFFF).contains(&cp) && cp != 0xFFFE && cp != 0xFFFF)
        }
        _ => true,
    }
}

/// `numeric_entity_is_allowed()`.
fn numeric_entity_is_allowed(cp: u32, doctype: u32) -> bool {
    match doctype {
        DOC_HTML401 => cp <= 0x0010_FFFF,
        DOC_HTML5 => {
            (0x20..=0x7E).contains(&cp)
                || ((0x09..=0x0C).contains(&cp) && cp != 0x0B)
                || ((0xA0..=0x0010_FFFF).contains(&cp)
                    && (cp & 0xFFFF) < 0xFFFE
                    && !(0xFDD0..=0xFDEF).contains(&cp))
        }
        _ => unicode_cp_is_allowed(cp, doctype),
    }
}

/// Byte at `i`, or the terminating NUL past the end (C strings).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `process_numeric_entity()`: `pos` is just after `#`. Returns the code
/// point (on success) and the position of the last byte examined (the
/// `;` on success).
fn process_numeric_entity(s: &[u8], mut pos: usize) -> (Option<u32>, usize) {
    let hex = matches!(at(s, pos), b'x' | b'X');
    if hex {
        pos += 1;
    }
    let first = at(s, pos);
    if (hex && !first.is_ascii_hexdigit()) || (!hex && !first.is_ascii_digit()) {
        return (None, pos);
    }
    let radix: u64 = if hex { 16 } else { 10 };
    // strtol saturates at ZEND_LONG_MAX; anything above 0x0010_FFFF fails.
    let mut value: u64 = 0;
    while let Some(d) = char::from(at(s, pos)).to_digit(if hex { 16 } else { 10 }) {
        value = value.saturating_mul(radix).saturating_add(u64::from(d));
        pos += 1;
    }
    if at(s, pos) != b';' || value > 0x0010_FFFF {
        return (None, pos);
    }
    (u32::try_from(value).ok(), pos)
}

fn is_entity_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// An inverse (name → code points) map.
#[derive(Clone, Copy)]
enum InvMap {
    Html5,
    Html4,
    BasicNoApos,
    BasicApos,
}

impl InvMap {
    fn resolve(self, name: &[u8]) -> Option<(u32, u32)> {
        let basic = |apos: bool| match name {
            b"gt" => Some((0x3E, 0)),
            b"amp" => Some((0x26, 0)),
            b"lt" => Some((0x3C, 0)),
            b"quot" => Some((0x22, 0)),
            b"apos" if apos => Some((0x27, 0)),
            b"#039" if !apos => Some((0x27, 0)),
            _ => None,
        };
        let table = match self {
            Self::Html5 => INV_HTML5,
            Self::Html4 => INV_HTML4,
            Self::BasicNoApos => return basic(false),
            Self::BasicApos => return basic(true),
        };
        table
            .binary_search_by(|(n, _, _)| n.as_bytes().cmp(name))
            .ok()
            .map(|i| (table[i].1, table[i].2))
    }
}

/// `unescape_inverse_map()`.
fn unescape_inverse_map(all: bool, flags: u32) -> InvMap {
    let doctype = flags & DOC_TYPE_MASK;
    match (all, doctype) {
        (true, DOC_HTML401 | DOC_XHTML) => InvMap::Html4,
        (true, DOC_HTML5) => InvMap::Html5,
        (false, DOC_HTML401) => InvMap::BasicNoApos,
        _ => InvMap::BasicApos,
    }
}

/// Basic entity for `k` (`stage3_table_be_{no,}apos_00000`).
fn basic_entity(k: u32, doctype: u32) -> Option<&'static str> {
    match k {
        0x22 => Some("quot"),
        0x26 => Some("amp"),
        0x27 => Some(if doctype == DOC_HTML401 {
            "#039"
        } else {
            "apos"
        }),
        0x3C => Some("lt"),
        0x3E => Some("gt"),
        _ => None,
    }
}

fn lookup_row(table: &'static [(u32, EntityRow)], k: u32) -> Option<&'static EntityRow> {
    table
        .binary_search_by_key(&k, |&(cp, _)| cp)
        .ok()
        .map(|i| &table[i].1)
}

/// Masks the PHP `int` flags to the bits html.c looks at.
fn flag_bits(flags: i64) -> u32 {
    u32::try_from(flags & 0xFF).unwrap_or(0)
}

/// `traverse_for_entities()`.
fn traverse_for_entities(
    input: &[u8],
    all: bool,
    flags: u32,
    inv: InvMap,
    charset: Charset,
) -> Vec<u8> {
    let doctype = flags & DOC_TYPE_MASK;
    let end = input.len();
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(end);
    let mut cur = 0;
    while cur < end {
        let Some(off) = input[cur..].iter().position(|&b| b == b'&') else {
            out.extend_from_slice(&input[cur..]);
            break;
        };
        out.extend_from_slice(&input[cur..cur + off]);
        cur += off;
        if end - cur < 4 {
            out.extend_from_slice(&input[cur..]);
            break;
        }

        // Ok((code, code2, entity_end)), Err(None) = invalid incomplete,
        // Err(Some(entity_end)) = invalid complete.
        let parsed: Result<(u32, u32, usize), Option<usize>> = if input[cur + 1] == b'#' {
            match process_numeric_entity(input, cur + 2) {
                (Some(code), semi) => {
                    let basic_ok = code <= 63 && basic_entity(code, DOC_XML1).is_some();
                    if (!all && !basic_ok)
                        || !unicode_cp_is_allowed(code, doctype)
                        || (doctype == DOC_HTML5 && code == 0x0D)
                    {
                        Err(None)
                    } else {
                        Ok((code, 0, semi))
                    }
                }
                (None, _) => Err(None),
            }
        } else {
            let name_start = cur + 1;
            let limit = name_start + LONGEST_ENTITY_LENGTH.min(end - name_start - 1);
            let mut semi = name_start;
            while semi < limit && is_entity_name_byte(input[semi]) {
                semi += 1;
            }
            if input[semi] != b';' || semi == name_start {
                Err(None)
            } else {
                let name = &input[name_start..semi];
                match inv.resolve(name) {
                    Some((c1, c2)) => Ok((c1, c2, semi)),
                    None if doctype == DOC_XHTML && name == b"apos" => Ok((0x27, 0, semi)),
                    None => Err(None),
                }
            }
        };

        let parsed = parsed.and_then(|(code, code2, semi)| {
            if (code == 0x27 && flags & QUOTE_SINGLE == 0)
                || (code == 0x22 && flags & QUOTE_DOUBLE == 0)
            {
                return Err(Some(semi));
            }
            if charset == Charset::Utf8 {
                Ok((code, code2, semi))
            } else {
                match map_from_unicode(code, charset) {
                    Some(mapped) if code2 == 0 => Ok((mapped, 0, semi)),
                    _ => Err(Some(semi)),
                }
            }
        });

        match parsed {
            Ok((code, code2, semi)) => {
                write_octets(&mut out, charset, code);
                if code2 != 0 {
                    write_octets(&mut out, charset, code2);
                }
                cur = semi + 1;
            }
            Err(None) => {
                out.push(input[cur]);
                cur += 1;
            }
            Err(Some(semi)) => {
                out.extend_from_slice(&input[cur..semi]);
                cur = semi;
            }
        }
    }
    out
}

/// `php_unescape_html_entities()`: decodes all entities (`all`) or only the
/// basic ones.
fn unescape_html_entities(
    s: &[u8],
    all: bool,
    flags: i64,
    charset: Option<&[u8]>,
    func: &'static str,
    notices: &mut Notices,
) -> Vec<u8> {
    if !s.contains(&b'&') {
        return s.to_vec();
    }
    let cs = if all {
        determine_charset(charset, false, func, notices)
    } else {
        Charset::Iso8859_1
    };
    let flags = flag_bits(flags);
    traverse_for_entities(s, all, flags, unescape_inverse_map(all, flags), cs)
}

/// Backs `htmlspecialchars_decode(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401)`.
pub fn htmlspecialchars_decode(s: &[u8], flags: i64) -> Vec<u8> {
    let mut sink = Notices::new();
    unescape_html_entities(s, false, flags, None, "htmlspecialchars_decode", &mut sink)
}

/// Backs `html_entity_decode(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null)`; `charset` is
/// the [`effective_charset_hint`].
pub fn html_entity_decode(
    s: &[u8],
    flags: i64,
    charset: Option<&[u8]>,
    notices: &mut Notices,
) -> Vec<u8> {
    unescape_html_entities(s, true, flags, charset, "html_entity_decode", notices)
}

/// `find_entity_for_char()` for the full tables; may advance `cursor` over
/// a second code point that forms a two-code-point entity.
fn find_entity_for_char(
    k: u32,
    charset: Charset,
    table: &'static [(u32, EntityRow)],
    old: &[u8],
    cursor: &mut usize,
) -> Option<&'static str> {
    if k >> 12 > 0x1D {
        return None;
    }
    match lookup_row(table, k)? {
        EntityRow::Single(e) => Some(e),
        EntityRow::Multi { default, seconds } => {
            if *cursor < old.len() {
                if let (Ok(next), after) = next_char(charset, old, *cursor) {
                    if let Some(&(_, e)) = seconds.iter().find(|&&(cp, _)| cp == next) {
                        *cursor = after;
                        return Some(e);
                    }
                }
            }
            *default
        }
    }
}

/// `php_escape_html_entities_ex()`: the engine behind
/// [`htmlspecialchars`] (`all == false`) and [`htmlentities`]
/// (`all == true`). `func` names the PHP function for diagnostics; `quiet`
/// suppresses the charset warning and the partial-support notice.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn escape_html_entities(
    old: &[u8],
    all: bool,
    flags: i64,
    charset: Option<&[u8]>,
    double_encode: bool,
    quiet: bool,
    func: &'static str,
    notices: &mut Notices,
) -> Vec<u8> {
    let flags = flag_bits(flags);
    let charset = determine_charset(charset, quiet, func, notices);
    let doctype = flags & DOC_TYPE_MASK;
    let mut all = all;
    if all {
        if !quiet && charset.partial_support() {
            notices.push(Notice::e_notice(
                func,
                "Only basic entities substitution is supported for multi-byte encodings other \
                 than UTF-8; functionality is equivalent to htmlspecialchars",
            ));
        }
        // LIMIT_ALL
        all = !charset.partial_support() && doctype != DOC_XML1;
    }

    let table: &'static [(u32, EntityRow)] = if doctype == DOC_HTML5 {
        TABLE_HTML5
    } else {
        TABLE_HTML4
    };
    let to_uni = if all && !charset.unicode_compat() {
        charset.to_uni_table()
    } else {
        None
    };
    let inv = unescape_inverse_map(true, flags);
    let replacement: &[u8] = if charset == Charset::Utf8 {
        "\u{FFFD}".as_bytes()
    } else {
        b"&#xFFFD;"
    };

    let oldlen = old.len();
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(oldlen + oldlen / 4);
    let mut cursor = 0;
    while cursor < oldlen {
        let before = cursor;
        let byte = old[cursor];
        let mut this_char = u32::from(byte);
        if byte >= 0x80 {
            let (r, next) = next_char(charset, old, cursor);
            cursor = next;
            if let Ok(c) = r {
                this_char = c;
            } else if flags & IGNORE_ERRORS != 0 {
                continue;
            } else if flags & SUBSTITUTE_ERRORS != 0 {
                out.extend_from_slice(replacement);
                continue;
            } else {
                return Vec::new();
            }
        } else {
            cursor += 1;
        }
        let mut mbsequence: &[u8] = &old[before..cursor];

        if this_char == u32::from(b'&') {
            if double_encode {
                out.extend_from_slice(b"&amp;");
                continue;
            }
            let ent_len = if at(old, cursor) == b'#' {
                match process_numeric_entity(old, cursor + 1) {
                    (Some(cp), semi)
                        if flags & SUBSTITUTE_DISALLOWED == 0
                            || numeric_entity_is_allowed(cp, doctype) =>
                    {
                        Some(semi - cursor)
                    }
                    _ => None,
                }
            } else {
                let mut next = cursor;
                while next < oldlen && is_entity_name_byte(old[next]) {
                    next += 1;
                }
                let name = &old[cursor..next];
                if at(old, next) != b';' || name.is_empty() {
                    None
                } else if inv.resolve(name).is_some() || (doctype == DOC_XHTML && name == b"apos") {
                    Some(name.len())
                } else {
                    None
                }
            };
            match ent_len {
                Some(n) => {
                    out.push(b'&');
                    out.extend_from_slice(&old[cursor..cursor + n]);
                    out.push(b';');
                    cursor += n + 1;
                }
                None => out.extend_from_slice(b"&amp;"),
            }
            continue;
        }

        let is_unquoted = (this_char == 0x27 && flags & QUOTE_SINGLE == 0)
            || (this_char == 0x22 && flags & QUOTE_DOUBLE == 0);
        if !is_unquoted {
            let mut rep = None;
            let mut pass_through = false;
            if all {
                if let Some(t) = to_uni {
                    // single-byte charset: this_char < 0x100
                    this_char = u32::from(t[usize::from(byte)]);
                    if this_char == 0xFFFF {
                        pass_through = true;
                    }
                }
                if !pass_through {
                    rep = find_entity_for_char(this_char, charset, table, old, &mut cursor);
                }
            } else if this_char < 64 {
                rep = basic_entity(this_char, doctype);
            }
            if let Some(e) = rep {
                out.push(b'&');
                out.extend_from_slice(e.as_bytes());
                out.push(b';');
                continue;
            }
            if !pass_through && flags & SUBSTITUTE_DISALLOWED != 0 {
                let disallowed = if charset.unicode_compat() || to_uni.is_some() {
                    !unicode_cp_is_allowed(this_char, doctype)
                } else {
                    this_char <= 0x7D && !unicode_cp_is_allowed(this_char, doctype)
                };
                if disallowed {
                    mbsequence = replacement;
                }
            }
        }
        out.extend_from_slice(mbsequence);
    }
    out
}

/// Backs `htmlspecialchars(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null, bool
/// $double_encode = true)`; `charset` is the [`effective_charset_hint`].
pub fn htmlspecialchars(
    s: &[u8],
    flags: i64,
    charset: Option<&[u8]>,
    double_encode: bool,
    notices: &mut Notices,
) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    escape_html_entities(
        s,
        false,
        flags,
        charset,
        double_encode,
        false,
        "htmlspecialchars",
        notices,
    )
}

/// Backs `htmlentities(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null, bool
/// $double_encode = true)`; `charset` is the [`effective_charset_hint`].
pub fn htmlentities(
    s: &[u8],
    flags: i64,
    charset: Option<&[u8]>,
    double_encode: bool,
    notices: &mut Notices,
) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    escape_html_entities(
        s,
        true,
        flags,
        charset,
        double_encode,
        false,
        "htmlentities",
        notices,
    )
}

/// Inserts `key => value` like `add_assoc_stringl_ex()` (an existing key
/// keeps its position and gets the new value).
fn add_assoc(arr: &mut Vec<(Vec<u8>, Vec<u8>)>, key: Vec<u8>, entity: &str) {
    #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
    let mut value = Vec::with_capacity(entity.len() + 2);
    value.push(b'&');
    value.extend_from_slice(entity.as_bytes());
    value.push(b';');
    if let Some(slot) = arr.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        arr.push((key, value));
    }
}

/// `write_s3row_data()`.
fn write_row(row: &EntityRow, orig_cp: u32, charset: Charset, arr: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    let mut key = Vec::new();
    write_octets(&mut key, charset, orig_cp);
    match row {
        EntityRow::Single(e) => add_assoc(arr, key, e),
        EntityRow::Multi { default, seconds } => {
            if let Some(e) = default {
                add_assoc(arr, key.clone(), e);
            }
            for &(uni_cp, e) in *seconds {
                let spe = if charset.unicode_compat() {
                    uni_cp
                } else {
                    match map_from_unicode(uni_cp, charset) {
                        Some(c) => c,
                        None => continue,
                    }
                };
                let mut k2 = key.clone();
                write_octets(&mut k2, charset, spe);
                add_assoc(arr, k2, e);
            }
        }
    }
}

/// Backs `get_html_translation_table(int $table = HTML_SPECIALCHARS, int
/// $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401, string $encoding =
/// "UTF-8")`: the ordered `character => entity` pairs. `charset` is the
/// [`effective_charset_hint`] of the `$encoding` argument (an omitted
/// argument behaves like an empty one).
pub fn get_html_translation_table(
    table: i64,
    flags: i64,
    charset: Option<&[u8]>,
    notices: &mut Notices,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let charset = determine_charset(charset, false, "get_html_translation_table", notices);
    let flags = flag_bits(flags);
    let doctype = flags & DOC_TYPE_MASK;
    let all = table != 0 && !charset.partial_support() && doctype != DOC_XML1;
    let skip = |code: u32| {
        (code == 0x27 && flags & QUOTE_SINGLE == 0) || (code == 0x22 && flags & QUOTE_DOUBLE == 0)
    };
    let mut arr = Vec::new();
    if all {
        let ms: &'static [(u32, EntityRow)] = if doctype == DOC_HTML5 {
            TABLE_HTML5
        } else {
            TABLE_HTML4
        };
        if charset.unicode_compat() {
            let max = if charset.single_byte() {
                0x100
            } else {
                0x1E000
            };
            for (cp, row) in ms.iter().filter(|(cp, _)| *cp < max) {
                if !skip(*cp) {
                    write_row(row, *cp, charset, &mut arr);
                }
            }
        } else if let Some(to_uni) = charset.to_uni_table() {
            for (i, &uni) in (0u32..).zip(to_uni.iter()) {
                if skip(i) {
                    continue;
                }
                if let Some(row) = lookup_row(ms, u32::from(uni)) {
                    write_row(row, i, charset, &mut arr);
                }
            }
        }
    } else {
        for j in 0..64 {
            if skip(j) {
                continue;
            }
            if let Some(e) = basic_entity(j, doctype) {
                write_row(&EntityRow::Single(e), j, Charset::Iso8859_1, &mut arr);
            }
        }
    }
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsc(s: &[u8], flags: i64) -> Vec<u8> {
        htmlspecialchars(s, flags, None, true, &mut Notices::new())
    }

    #[test]
    fn basic_specialchars() {
        assert_eq!(
            hsc(b"<a href='x'>&\"</a>", DEFAULT_FLAGS),
            b"&lt;a href=&#039;x&#039;&gt;&amp;&quot;&lt;/a&gt;"
        );
        assert_eq!(hsc(b"'\"", ENT_NOQUOTES), b"'\"");
        assert_eq!(hsc(b"'\"", ENT_COMPAT), b"'&quot;");
        assert_eq!(hsc(b"'", ENT_QUOTES | ENT_HTML5), b"&apos;");
        assert_eq!(hsc(b"a\xffb", ENT_QUOTES), b"");
        assert_eq!(hsc(b"a\xffb", ENT_QUOTES | ENT_IGNORE), b"ab");
        assert_eq!(hsc(b"a\xffb", DEFAULT_FLAGS), "a\u{FFFD}b".as_bytes());
    }

    #[test]
    fn no_double_encode() {
        let mut n = Notices::new();
        assert_eq!(
            htmlspecialchars(
                b"&amp; &foo; &#x41; &#1114112; &",
                DEFAULT_FLAGS,
                None,
                false,
                &mut n
            ),
            b"&amp; &amp;foo; &#x41; &amp;#1114112; &amp;"
        );
    }

    #[test]
    fn entities_and_decode_roundtrip() {
        let mut n = Notices::new();
        let s = "café ≤ ∂̸ <>".as_bytes();
        let enc = htmlentities(s, ENT_QUOTES | ENT_HTML5, None, true, &mut n);
        assert_eq!(enc, b"caf&eacute; &leq; &npart; &lt;&gt;");
        assert_eq!(
            html_entity_decode(&enc, ENT_QUOTES | ENT_HTML5, None, &mut n),
            s
        );
        assert!(n.is_empty());
    }

    #[test]
    fn charset_warning() {
        let mut n = Notices::new();
        let _ = htmlentities(b"x", DEFAULT_FLAGS, Some(b"bogus"), true, &mut n);
        assert_eq!(
            n[0].to_string(),
            "htmlentities(): Charset \"bogus\" is not supported, assuming UTF-8"
        );
        assert_eq!(
            determine_charset(Some(b"WINDOWS-1251"), false, "f", &mut n),
            Charset::Cp1251
        );
        assert_eq!(
            effective_charset_hint(Some(b"\0x"), None, Some(b"koi8-r")),
            Some(&b"koi8-r"[..])
        );
    }

    #[test]
    fn translation_table_basic() {
        let t =
            get_html_translation_table(HTML_SPECIALCHARS, DEFAULT_FLAGS, None, &mut Notices::new());
        let keys: Vec<&[u8]> = t.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, [&b"\""[..], b"&", b"'", b"<", b">"]);
    }

    #[test]
    fn utf8_decoder() {
        assert_eq!(next_utf8_char(b"\xe2\x82\xac", 0), (Some(0x20AC), 3));
        assert_eq!(next_utf8_char(b"\xe2\x82", 0), (None, 2));
        assert_eq!(next_utf8_char(b"\xed\xa0\x80", 0), (None, 3));
        assert_eq!(next_utf8_char(b"\xc0\x80", 0), (None, 1));
    }
}
