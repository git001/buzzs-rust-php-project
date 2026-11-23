// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! String transformations from `ext/standard/string.c`: case mapping,
//! trimming, padding, wrapping, splitting, translation and replacement,
//! plus the path helpers `basename()`/`dirname()`/`pathinfo()`.
//!
//! All case mapping is ASCII-only (locale-insensitive), as in PHP ≥ 8.2.
//!
//! Unless stated otherwise, every string argument is obtained by the VM with
//! `zval_get_string()` semantics (`Z_PARAM_STR`) and every integer argument
//! with `Z_PARAM_LONG` semantics before calling into this module.

use crate::analysis::charmask;
use crate::error::{Notice, Notices, StdError, StdResult};
use crate::search::memnstr;

/// Converts a length to `i64` (string lengths always fit).
pub(crate) fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Converts a non-negative `i64` to `usize`, saturating.
pub(crate) fn to_usize(n: i64) -> usize {
    usize::try_from(n.max(0)).unwrap_or(usize::MAX)
}

/// `zend_string_tolower()`; backs `strtolower(string $string): string`.
pub fn strtolower(s: &[u8]) -> Vec<u8> {
    s.to_ascii_lowercase()
}

/// `zend_string_toupper()`; backs `strtoupper(string $string): string`.
pub fn strtoupper(s: &[u8]) -> Vec<u8> {
    s.to_ascii_uppercase()
}

/// Backs `ucfirst(string $string): string`.
pub fn ucfirst(s: &[u8]) -> Vec<u8> {
    let mut out = s.to_vec();
    if let Some(c) = out.first_mut() {
        *c = c.to_ascii_uppercase();
    }
    out
}

/// Backs `lcfirst(string $string): string`.
pub fn lcfirst(s: &[u8]) -> Vec<u8> {
    let mut out = s.to_vec();
    if let Some(c) = out.first_mut() {
        *c = c.to_ascii_lowercase();
    }
    out
}

/// Default `$separators` of `ucwords()`.
pub const UCWORDS_DEFAULT_DELIMITERS: &[u8] = b" \t\r\n\x0c\x0b";

/// Backs `ucwords(string $string, string $separators = " \t\r\n\f\v")`.
///
/// `..` ranges in `delimiters` are expanded by [`charmask`] and may push
/// `ucwords(): Invalid '..'-range…` warnings.
pub fn ucwords(s: &[u8], delimiters: &[u8], notices: &mut Notices) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    let mask = charmask(delimiters, "ucwords", notices);
    let mut out = s.to_vec();
    out[0] = out[0].to_ascii_uppercase();
    for i in 1..out.len() {
        if mask[usize::from(out[i - 1])] {
            out[i] = out[i].to_ascii_uppercase();
        }
    }
    out
}

/// Which side(s) `trim()` strips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    /// `ltrim()` (mode 1).
    Left,
    /// `rtrim()` / `chop()` (mode 2).
    Right,
    /// `trim()` (mode 3).
    Both,
}

impl TrimMode {
    /// The PHP function name used to attribute charmask warnings.
    pub const fn func_name(self) -> &'static str {
        match self {
            Self::Left => "ltrim",
            Self::Right => "rtrim",
            Self::Both => "trim",
        }
    }
}

/// Default `$characters` of the trim family: `" \n\r\t\v\0"` plus `\f`
/// (PHP 8.4 added form feed to `php_is_whitespace`).
fn is_trim_whitespace(c: u8) -> bool {
    matches!(c, b' ' | b'\x0c' | b'\n' | b'\r' | b'\t' | b'\x0b' | 0)
}

/// `php_trim_int()`; backs `trim()`, `ltrim()`, `rtrim()` and `chop()`.
///
/// `what` is the `$characters` argument (`None` → PHP's default whitespace
/// set).  A multi-byte `what` goes through [`charmask`], whose warnings are
/// attributed to [`TrimMode::func_name`] (the VM should rewrite the name to
/// `chop` for that alias via [`Notice::func`]).
pub fn trim<'a>(
    s: &'a [u8],
    what: Option<&[u8]>,
    mode: TrimMode,
    notices: &mut Notices,
) -> &'a [u8] {
    // Monomorphised per mask kind: no boxed closure per call.
    match what {
        None => trim_by(s, mode, is_trim_whitespace),
        Some(&[c]) => trim_by(s, mode, |x| x == c),
        Some(w) => {
            let m = charmask(w, mode.func_name(), notices);
            trim_by(s, mode, |x| m[usize::from(x)])
        }
    }
}

/// The trimming loops of [`trim`] for one character mask.
#[inline]
fn trim_by(s: &[u8], mode: TrimMode, mask: impl Fn(u8) -> bool) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    if mode != TrimMode::Right {
        while start < end && mask(s[start]) {
            start += 1;
        }
    }
    if mode != TrimMode::Left {
        while start < end && mask(s[end - 1]) {
            end -= 1;
        }
    }
    &s[start..end]
}

/// `STR_PAD_LEFT`.
pub const STR_PAD_LEFT: i64 = 0;
/// `STR_PAD_RIGHT`.
pub const STR_PAD_RIGHT: i64 = 1;
/// `STR_PAD_BOTH`.
pub const STR_PAD_BOTH: i64 = 2;

fn pad_fill(out: &mut Vec<u8>, n: usize, pad: &[u8]) {
    out.extend(pad.iter().copied().cycle().take(n));
}

/// Backs `str_pad(string $string, int $length, string $pad_string = " ",
/// int $pad_type = STR_PAD_RIGHT)`.
pub fn str_pad(input: &[u8], pad_length: i64, pad: &[u8], pad_type: i64) -> StdResult<Vec<u8>> {
    if pad_length < 0 || to_usize(pad_length) <= input.len() {
        return Ok(input.to_vec());
    }
    if pad.is_empty() {
        return Err(StdError::arg_value(
            "str_pad",
            3,
            "pad_string",
            "must not be empty",
        ));
    }
    let num = to_usize(pad_length) - input.len();
    let (left, right) = match pad_type {
        STR_PAD_RIGHT => (0, num),
        STR_PAD_LEFT => (num, 0),
        STR_PAD_BOTH => (num / 2, num - num / 2),
        _ => {
            return Err(StdError::arg_value(
                "str_pad",
                4,
                "pad_type",
                "must be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH",
            ))
        }
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: the builtin checks the size first with php_memlimit::try_reserve"
    )]
    let mut out = Vec::with_capacity(input.len() + num);
    pad_fill(&mut out, left, pad);
    out.extend_from_slice(input);
    pad_fill(&mut out, right, pad);
    Ok(out)
}

/// Backs `wordwrap(string $string, int $width = 75, string $break = "\n",
/// bool $cut_long_words = false)`.
pub fn wordwrap(text: &[u8], width: i64, brk: &[u8], cut: bool) -> StdResult<Vec<u8>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if brk.is_empty() {
        return Err(StdError::arg_value(
            "wordwrap",
            3,
            "break",
            "must not be empty",
        ));
    }
    if width == 0 && cut {
        return Err(StdError::arg_value(
            "wordwrap",
            4,
            "cut_long_words",
            "cannot be true when argument #2 ($width) is 0",
        ));
    }
    let len = len_i64(text.len());
    let at = |i: i64| text[to_usize(i)];
    if brk.len() == 1 && !cut {
        let b = brk[0];
        let mut out = text.to_vec();
        let (mut laststart, mut lastspace) = (0i64, 0i64);
        for current in 0..len {
            if at(current) == b {
                laststart = current + 1;
                lastspace = current + 1;
            } else if at(current) == b' ' {
                if current - laststart >= width {
                    out[to_usize(current)] = b;
                    laststart = current + 1;
                }
                lastspace = current;
            } else if current - laststart >= width && laststart != lastspace {
                out[to_usize(lastspace)] = b;
                laststart = lastspace + 1;
            }
        }
        return Ok(out);
    }
    let blen = len_i64(brk.len());
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(text.len() + text.len() / 8);
    let copy = |out: &mut Vec<u8>, from: i64, to: i64| {
        out.extend_from_slice(&text[to_usize(from)..to_usize(to)]);
    };
    let (mut laststart, mut lastspace) = (0i64, 0i64);
    let mut current = 0i64;
    while current < len {
        if at(current) == brk[0]
            && current + blen <= len
            && text[to_usize(current)..].starts_with(brk)
        {
            copy(&mut out, laststart, current + blen);
            current += blen - 1;
            laststart = current + 1;
            lastspace = current + 1;
        } else if at(current) == b' ' {
            if current - laststart >= width {
                copy(&mut out, laststart, current);
                out.extend_from_slice(brk);
                laststart = current + 1;
            }
            lastspace = current;
        } else if current - laststart >= width && cut && laststart >= lastspace {
            copy(&mut out, laststart, current);
            out.extend_from_slice(brk);
            laststart = current;
            lastspace = current;
        } else if current - laststart >= width && laststart < lastspace {
            copy(&mut out, laststart, lastspace);
            out.extend_from_slice(brk);
            lastspace += 1;
            laststart = lastspace;
        }
        current += 1;
    }
    if laststart != current {
        copy(&mut out, laststart, current);
    }
    Ok(out)
}

/// Backs `chunk_split(string $string, int $length = 76, string $separator
/// = "\r\n")`.
pub fn chunk_split(s: &[u8], chunklen: i64, end: &[u8]) -> StdResult<Vec<u8>> {
    if chunklen <= 0 {
        return Err(StdError::arg_value(
            "chunk_split",
            2,
            "length",
            "must be greater than 0",
        ));
    }
    let chunklen = to_usize(chunklen);
    if chunklen > s.len() {
        let mut out = s.to_vec();
        out.extend_from_slice(end);
        return Ok(out);
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: the builtin checks the size first with php_memlimit::try_reserve"
    )]
    let mut out = Vec::with_capacity(s.len() + (s.len() / chunklen + 1) * end.len());
    for chunk in s.chunks(chunklen) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(end);
    }
    Ok(out)
}

/// Backs `nl2br(string $string, bool $use_xhtml = true)`: inserts `<br />`
/// (or `<br>`) before every `\r\n`, `\n\r`, `\n` and `\r`.
pub fn nl2br(s: &[u8], is_xhtml: bool) -> Vec<u8> {
    let tag: &[u8] = if is_xhtml { b"<br />" } else { b"<br>" };
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\r' || c == b'\n' {
            out.extend_from_slice(tag);
            let next = s.get(i + 1).copied();
            if (c == b'\r' && next == Some(b'\n')) || (c == b'\n' && next == Some(b'\r')) {
                out.push(c);
                i += 1;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Backs `strrev(string $string): string`.
pub fn strrev(s: &[u8]) -> Vec<u8> {
    s.iter().rev().copied().collect()
}

/// Backs `str_repeat(string $string, int $times): string`.
#[expect(
    clippy::disallowed_methods,
    reason = "bounded: the builtin checks the size first with php_memlimit::try_reserve"
)]
pub fn str_repeat(s: &[u8], times: i64) -> StdResult<Vec<u8>> {
    if times < 0 {
        return Err(StdError::arg_value(
            "str_repeat",
            2,
            "times",
            "must be greater than or equal to 0",
        ));
    }
    Ok(s.repeat(to_usize(times)))
}

/// Backs `str_split(string $string, int $length = 1): array`.
pub fn str_split(s: &[u8], length: i64) -> StdResult<Vec<Vec<u8>>> {
    if length <= 0 {
        return Err(StdError::arg_value(
            "str_split",
            2,
            "length",
            "must be greater than 0",
        ));
    }
    Ok(s.chunks(to_usize(length)).map(<[u8]>::to_vec).collect())
}

/// `php_strtr_ex()`; backs the three-argument form `strtr($string, $from,
/// $to)`: bytewise translation using the first `min(len(from), len(to))`
/// bytes of both.  Later duplicates in `from` win.
pub fn strtr(s: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let n = from.len().min(to.len());
    if s.is_empty() || n == 0 {
        return s.to_vec();
    }
    let mut xlat: [u8; 256] = std::array::from_fn(|i| u8::try_from(i).unwrap_or(0));
    for (&f, &t) in from[..n].iter().zip(&to[..n]) {
        xlat[usize::from(f)] = t;
    }
    s.iter().map(|&c| xlat[usize::from(c)]).collect()
}

/// Backs the two-argument form `strtr(string $string, array $replace_pairs)`
/// (`php_strtr_array()`): at each position the longest matching key is
/// replaced, and replaced text is never rescanned.
///
/// Integer keys become their decimal string before calling. Pass the pairs
/// in array order. Empty keys push `strtr(): Ignoring replacement of empty string`.
pub fn strtr_pairs<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    s: &[u8],
    pairs: &[(K, V)],
    notices: &mut Notices,
) -> Vec<u8> {
    let result = strtr_pairs_with(s, pairs, notices, |_, v, _, _| {
        Ok::<_, std::convert::Infallible>(v.as_ref())
    });
    match result {
        Ok(out) => out,
        Err(e) => match e {},
    }
}

/// Translates pairs with PHP-order replacement conversion: a single pair
/// is converted eagerly, while multiple pairs convert only on each match.
/// Pending notices are passed to the converter so it can emit them before
/// conversion side effects. The final argument is the output buffer's
/// capacity, for allocation checks before appending the replacement.
///
/// # Errors
///
/// Propagates replacement conversion errors.
pub fn strtr_pairs_with<'a, K: AsRef<[u8]>, V, R: AsRef<[u8]>, E>(
    s: &[u8],
    pairs: &'a [(K, V)],
    notices: &mut Notices,
    mut convert: impl FnMut(&[u8], &'a V, &mut Notices, usize) -> Result<R, E>,
) -> Result<Vec<u8>, E> {
    if s.is_empty() || pairs.is_empty() {
        return Ok(s.to_vec());
    }
    if let [(k, v)] = pairs {
        let k = k.as_ref();
        let v = convert(k, v, notices, 0)?;
        if k.is_empty() {
            notices.push(Notice::warning(
                "strtr",
                "Ignoring replacement of empty string",
            ));
            return Ok(s.to_vec());
        }
        return Ok(replace_all(s, k, v.as_ref(), false).0);
    }
    let mut minlen = usize::MAX;
    let mut maxlen = 0;
    let mut map: std::collections::HashMap<&[u8], &V> = std::collections::HashMap::new();
    let mut first = [false; 256];
    for (k, v) in pairs {
        let k = k.as_ref();
        if k.is_empty() {
            notices.push(Notice::warning(
                "strtr",
                "Ignoring replacement of empty string",
            ));
            continue;
        }
        if k.len() > s.len() {
            continue;
        }
        minlen = minlen.min(k.len());
        maxlen = maxlen.max(k.len());
        first[usize::from(k[0])] = true;
        map.insert(k, v);
    }
    if minlen > maxlen {
        return Ok(s.to_vec());
    }
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut pos = 0;
    let mut old_pos = 0;
    while pos + minlen <= s.len() {
        if first[usize::from(s[pos])] {
            let mut len = maxlen.min(s.len() - pos);
            while len >= minlen {
                if let Some(rep) = map.get(&s[pos..pos + len]) {
                    let rep = convert(&s[pos..pos + len], *rep, notices, out.capacity())?;
                    out.extend_from_slice(&s[old_pos..pos]);
                    out.extend_from_slice(rep.as_ref());
                    old_pos = pos + len;
                    pos = old_pos - 1;
                    break;
                }
                len -= 1;
            }
        }
        pos += 1;
    }
    out.extend_from_slice(&s[old_pos..]);
    Ok(out)
}

/// Replaces every non-overlapping occurrence of `needle` (non-empty) in
/// `haystack`, left to right; returns the result and the replacement count.
/// Case-insensitive matching is ASCII-only.
fn replace_all(haystack: &[u8], needle: &[u8], rep: &[u8], ci: bool) -> (Vec<u8>, i64) {
    let lc_h;
    let lc_n;
    let (h, n) = if ci {
        lc_h = haystack.to_ascii_lowercase();
        lc_n = needle.to_ascii_lowercase();
        (&lc_h[..], &lc_n[..])
    } else {
        (haystack, needle)
    };
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(haystack.len());
    let mut count = 0;
    let mut p = 0;
    while let Some(r) = memnstr(&h[p..], n) {
        let r = p + r;
        out.extend_from_slice(&haystack[p..r]);
        out.extend_from_slice(rep);
        count += 1;
        p = r + n.len();
    }
    out.extend_from_slice(&haystack[p..]);
    (out, count)
}

/// The `$search`/`$replace` argument of `str_replace()`: a single string or
/// the (already `zval_get_string`-converted) values of an array.
#[derive(Debug)]
pub enum ReplaceArg<'a, T: AsRef<[u8]>> {
    /// A string argument.
    One(&'a [u8]),
    /// An array argument (values in order).
    Many(&'a [T]),
}

impl<T: AsRef<[u8]>> Clone for ReplaceArg<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: AsRef<[u8]>> Copy for ReplaceArg<'_, T> {}

/// `php_str_replace_in_subject()`; backs `str_replace()` (`ci = false`)
/// and `str_ireplace()` (`ci = true`) for one subject string.  Returns the
/// new subject and the number of replacements (for `&$count`).
///
/// With an array `search`, each search string is applied in turn to the
/// result of the previous one, paired with the replacement at the same
/// position (missing replacements are empty); empty search strings are
/// skipped.  A string `search` with an array `replace` is a `TypeError`.
/// The VM applies this to each element of an array `$subject`.
pub fn str_replace<T: AsRef<[u8]>, U: AsRef<[u8]>>(
    subject: &[u8],
    search: ReplaceArg<'_, T>,
    replace: ReplaceArg<'_, U>,
    ci: bool,
) -> StdResult<(Vec<u8>, i64)> {
    let func = if ci { "str_ireplace" } else { "str_replace" };
    match search {
        ReplaceArg::One(needle) => {
            let ReplaceArg::One(rep) = replace else {
                return Err(StdError::arg_type(
                    func,
                    2,
                    "replace",
                    "must be of type string when argument #1 ($search) is a string",
                ));
            };
            if subject.is_empty() || needle.is_empty() {
                return Ok((subject.to_vec(), 0));
            }
            Ok(replace_all(subject, needle, rep, ci))
        }
        ReplaceArg::Many(needles) => {
            let mut cur = subject.to_vec();
            let mut total = 0;
            if cur.is_empty() {
                return Ok((cur, 0));
            }
            for (i, needle) in needles.iter().enumerate() {
                let needle = needle.as_ref();
                let rep: &[u8] = match replace {
                    ReplaceArg::One(r) => r,
                    ReplaceArg::Many(rs) => rs.get(i).map_or(&[][..], AsRef::as_ref),
                };
                if needle.is_empty() {
                    continue;
                }
                let (next, n) = replace_all(&cur, needle, rep, ci);
                total += n;
                cur = next;
                if cur.is_empty() {
                    break;
                }
            }
            Ok((cur, total))
        }
    }
}

/// Backs `str_rot13(string $string): string`.
pub fn str_rot13(s: &[u8]) -> Vec<u8> {
    s.iter()
        .map(|&c| match c {
            b'a'..=b'z' => (c - b'a' + 13) % 26 + b'a',
            b'A'..=b'Z' => (c - b'A' + 13) % 26 + b'A',
            _ => c,
        })
        .collect()
}

/// Backs `chr(int $codepoint): string` (the result is one byte,
/// `codepoint mod 256`).  Since PHP 8.5 an out-of-range value emits an
/// `E_DEPRECATED`.
pub fn chr(c: i64, notices: &mut Notices) -> u8 {
    if !(0..=255).contains(&c) {
        notices.push(Notice::deprecated(
            "chr",
            "Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256",
        ));
    }
    u8::try_from(c & 0xff).unwrap_or(0)
}

/// Backs `ord(string $character): int`: the first byte (0 for `""`).
/// Since PHP 8.5 a string that is not exactly one byte emits an
/// `E_DEPRECATED`.
pub fn ord(s: &[u8], notices: &mut Notices) -> i64 {
    match s.len() {
        1 => {}
        0 => notices.push(Notice::deprecated(
            "ord",
            "Providing an empty string is deprecated",
        )),
        _ => notices.push(Notice::deprecated(
            "ord",
            "Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead",
        )),
    }
    i64::from(s.first().copied().unwrap_or(0))
}

/// Backs `bin2hex(string $string): string` (lower-case hex).
pub fn bin2hex(s: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most twice the input"
    )]
    let mut out = Vec::with_capacity(s.len() * 2);
    for &b in s {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 15)]);
    }
    out
}

/// Backs `hex2bin(string $string): string|false`.  `None` means `false`
/// (after pushing the warning PHP emits).
pub fn hex2bin(s: &[u8], notices: &mut Notices) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        notices.push(Notice::warning(
            "hex2bin",
            "Hexadecimal input string must have an even length",
        ));
        return None;
    }
    let digit = |c: u8| char::from(c).to_digit(16);
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        if let (Some(h), Some(l)) = (digit(pair[0]), digit(pair[1])) {
            out.push(u8::try_from(h * 16 + l).unwrap_or(0));
        } else {
            notices.push(Notice::warning(
                "hex2bin",
                "Input string must be hexadecimal string",
            ));
            return None;
        }
    }
    Some(out)
}

/// `php_utf8_encode()`; backs the (deprecated) `utf8_encode()`: Latin-1 →
/// UTF-8.  The `Function utf8_encode() is deprecated` diagnostic comes from
/// the function's `#[\Deprecated]` attribute and is emitted by the VM.
pub fn utf8_encode(s: &[u8]) -> Vec<u8> {
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: at most twice the input"
    )]
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        if c < 0x80 {
            out.push(c);
        } else {
            out.push(0xc0 | (c >> 6));
            out.push(0x80 | (c & 0x3f));
        }
    }
    out
}

/// `php_utf8_decode()`; backs the (deprecated) `utf8_decode()`: UTF-8 →
/// Latin-1, with `?` for invalid sequences and code points above U+00FF.
pub fn utf8_decode(s: &[u8]) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut pos = 0;
    while pos < s.len() {
        let (c, next) = crate::html::next_utf8_char(s, pos);
        pos = next;
        out.push(c.and_then(|c| u8::try_from(c).ok()).unwrap_or(b'?'));
    }
    out
}

fn check_alnum(func: &'static str, s: &[u8]) -> StdResult<()> {
    if s.is_empty() {
        return Err(StdError::arg_value(func, 1, "string", "must not be empty"));
    }
    if !s.iter().all(u8::is_ascii_alphanumeric) {
        return Err(StdError::arg_value(
            func,
            1,
            "string",
            "must be composed only of alphanumeric ASCII characters",
        ));
    }
    Ok(())
}

/// Backs `str_increment(string $string): string` (PHP ≥ 8.3).
pub fn str_increment(s: &[u8]) -> StdResult<Vec<u8>> {
    check_alnum("str_increment", s)?;
    let mut out = s.to_vec();
    let mut pos = out.len();
    let mut carry = true;
    while carry && pos > 0 {
        pos -= 1;
        let c = out[pos];
        match c {
            b'z' | b'Z' => {
                out[pos] = c - 25;
            }
            b'9' => out[pos] = b'0',
            _ => {
                out[pos] = c + 1;
                carry = false;
            }
        }
    }
    if carry {
        let lead = if out[0] == b'0' { b'1' } else { out[0] };
        out.insert(0, lead);
    }
    Ok(out)
}

/// Backs `str_decrement(string $string): string` (PHP ≥ 8.3).
pub fn str_decrement(s: &[u8]) -> StdResult<Vec<u8>> {
    check_alnum("str_decrement", s)?;
    let out_of_range = || {
        StdError::arg_value(
            "str_decrement",
            1,
            "string",
            &format!(
                "\"{}\" is out of decrement range",
                String::from_utf8_lossy(s)
            ),
        )
    };
    if s[0] == b'0' {
        return Err(out_of_range());
    }
    let mut out = s.to_vec();
    let mut pos = out.len();
    let mut carry = true;
    while carry && pos > 0 {
        pos -= 1;
        let c = out[pos];
        match c {
            b'a' | b'A' => out[pos] = c + 25,
            b'0' => out[pos] = b'9',
            _ => {
                out[pos] = c - 1;
                carry = false;
            }
        }
    }
    if carry || (out[0] == b'0' && out.len() > 1) {
        if out.len() == 1 {
            return Err(out_of_range());
        }
        out.remove(0);
    }
    Ok(out)
}

fn is_heb(c: u8) -> bool {
    (224..=250).contains(&c)
}
fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}
fn is_newline(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

/// Backs `hebrev(string $string, int $max_chars_per_line = 0)`: converts
/// logical Hebrew text (ISO-8859-8) to visual order.
#[allow(clippy::too_many_lines)]
pub fn hebrev(s: &[u8], max_chars: i64) -> Vec<u8> {
    let n = s.len();
    if n == 0 {
        return Vec::new();
    }
    // The C code may read the terminating NUL one past the end.
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut heb = vec![0u8; n];
    let mut target = n; // index one past the next write position (writes go backwards)
    let (mut block_start, mut block_end) = (0usize, 0usize);
    let mut tmp = 0usize;
    let mut hebrew_block = is_heb(at(0));
    loop {
        if hebrew_block {
            while (is_heb(at(tmp + 1))
                || is_blank(at(tmp + 1))
                || at(tmp + 1).is_ascii_punctuation()
                || at(tmp + 1) == b'\n')
                && block_end < n - 1
            {
                tmp += 1;
                block_end += 1;
            }
            for i in block_start + 1..=block_end + 1 {
                let c = match s[i - 1] {
                    b'(' => b')',
                    b')' => b'(',
                    b'[' => b']',
                    b']' => b'[',
                    b'{' => b'}',
                    b'}' => b'{',
                    b'<' => b'>',
                    b'>' => b'<',
                    b'\\' => b'/',
                    b'/' => b'\\',
                    c => c,
                };
                target -= 1;
                heb[target] = c;
            }
            hebrew_block = false;
        } else {
            while !is_heb(at(tmp + 1)) && at(tmp + 1) != b'\n' && block_end < n - 1 {
                tmp += 1;
                block_end += 1;
            }
            while (is_blank(at(tmp)) || at(tmp).is_ascii_punctuation())
                && at(tmp) != b'/'
                && at(tmp) != b'-'
                && block_end > block_start
            {
                tmp -= 1;
                block_end -= 1;
            }
            for i in (block_start + 1..=block_end + 1).rev() {
                target -= 1;
                heb[target] = s[i - 1];
            }
            hebrew_block = true;
        }
        block_start = block_end + 1;
        if block_end >= n - 1 {
            break;
        }
    }

    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(n);
    let mut begin = n - 1;
    let mut end = n - 1;
    loop {
        let mut char_count: i64 = 0;
        while (max_chars == 0 || (max_chars > 0 && char_count < max_chars)) && begin > 0 {
            char_count += 1;
            begin -= 1;
            if is_newline(heb[begin]) {
                while begin > 0 && is_newline(heb[begin - 1]) {
                    begin -= 1;
                    char_count += 1;
                }
                break;
            }
        }
        if max_chars >= 0 && char_count == max_chars {
            let (mut new_count, mut new_begin) = (char_count, begin);
            while new_count > 0 {
                if is_blank(heb[new_begin]) || is_newline(heb[new_begin]) {
                    break;
                }
                new_begin += 1;
                new_count -= 1;
            }
            if new_count > 0 {
                begin = new_begin;
            }
        }
        let orig_begin = begin;
        if is_blank(heb[begin]) {
            heb[begin] = b'\n';
        }
        while begin <= end && is_newline(heb[begin]) {
            begin += 1;
        }
        if begin <= end {
            out.extend_from_slice(&heb[begin..=end]);
        }
        let mut i = orig_begin;
        while i <= end && is_newline(heb[i]) {
            out.push(heb[i]);
            i += 1;
        }
        begin = orig_begin;
        if begin == 0 {
            break;
        }
        begin -= 1;
        end = begin;
    }
    out
}

/// `php_basename()` (POSIX, ASCII-compatible locale); backs
/// `basename(string $path, string $suffix = "")`.
pub fn basename(path: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return Vec::new();
    }
    let mut start = end - 1;
    while start > 0 && path[start - 1] != b'/' {
        start -= 1;
    }
    if !suffix.is_empty() && suffix.len() < end - start && path[..end].ends_with(suffix) {
        end -= suffix.len();
    }
    path[start..end].to_vec()
}

/// `zend_dirname()` (POSIX): one level.
fn dirname_once(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    while end > 0 && path[end - 1] != b'/' {
        end -= 1;
    }
    if end == 0 {
        return b".".to_vec();
    }
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    path[..end].to_vec()
}

/// Backs `dirname(string $path, int $levels = 1)`.
pub fn dirname(path: &[u8], levels: i64) -> StdResult<Vec<u8>> {
    if levels == 1 {
        return Ok(dirname_once(path));
    }
    if levels < 1 {
        return Err(StdError::arg_value(
            "dirname",
            2,
            "levels",
            "must be greater than or equal to 1",
        ));
    }
    let mut cur = path.to_vec();
    let mut levels = levels;
    loop {
        let prev_len = cur.len();
        cur = dirname_once(&cur);
        levels -= 1;
        if !(cur.len() < prev_len && levels > 0) {
            break;
        }
    }
    Ok(cur)
}

/// `PATHINFO_DIRNAME`.
pub const PATHINFO_DIRNAME: i64 = 1;
/// `PATHINFO_BASENAME`.
pub const PATHINFO_BASENAME: i64 = 2;
/// `PATHINFO_EXTENSION`.
pub const PATHINFO_EXTENSION: i64 = 4;
/// `PATHINFO_FILENAME`.
pub const PATHINFO_FILENAME: i64 = 8;
/// `PATHINFO_ALL`.
pub const PATHINFO_ALL: i64 = 15;

/// Result of `pathinfo()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathInfo {
    /// A single component (string result).
    Part(Vec<u8>),
    /// `PATHINFO_ALL`: the array entries in PHP's order (`dirname` only
    /// when non-empty, `extension` only when the basename contains a dot).
    All(Vec<(&'static str, Vec<u8>)>),
}

/// Backs `pathinfo(string $path, int $flags = PATHINFO_ALL)`.
pub fn pathinfo(path: &[u8], opt: i64) -> StdResult<PathInfo> {
    if !(PATHINFO_DIRNAME..=PATHINFO_ALL).contains(&opt) {
        return Err(StdError::arg_value(
            "pathinfo",
            2,
            "flags",
            "must be one of the PATHINFO_* constants",
        ));
    }
    if opt < PATHINFO_ALL && (opt & (opt - 1)) != 0 {
        return Err(StdError::arg_value(
            "pathinfo",
            2,
            "flags",
            "must be only one of the PATHINFO_* constants",
        ));
    }
    // php_dirname() works on a C string copy: the result stops at a NUL.
    let dir = || {
        let d = dirname_once(path);
        let cut = d.iter().position(|&c| c == 0).unwrap_or(d.len());
        d[..cut].to_vec()
    };
    if opt == PATHINFO_DIRNAME {
        return Ok(PathInfo::Part(dir()));
    }
    let base = basename(path, b"");
    if opt == PATHINFO_BASENAME {
        return Ok(PathInfo::Part(base));
    }
    let dot = base.iter().rposition(|&c| c == b'.');
    if opt == PATHINFO_EXTENSION {
        return Ok(PathInfo::Part(
            dot.map_or_else(Vec::new, |p| base[p + 1..].to_vec()),
        ));
    }
    let filename = dot.map_or_else(|| base.clone(), |p| base[..p].to_vec());
    if opt == PATHINFO_FILENAME {
        return Ok(PathInfo::Part(filename));
    }
    let mut all = Vec::new();
    let d = dir();
    if !d.is_empty() {
        all.push(("dirname", d));
    }
    if let Some(p) = dot {
        all.push(("basename", base.clone()));
        all.push(("extension", base[p + 1..].to_vec()));
    } else {
        all.push(("basename", base));
    }
    all.push(("filename", filename));
    Ok(PathInfo::All(all))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn basics() {
        let mut n = Notices::new();
        assert_eq!(
            ucwords(b"hello world-foo", b" -", &mut n),
            b"Hello World-Foo"
        );
        assert_eq!(trim(b"  x \0", None, TrimMode::Both, &mut n), b"x");
        assert_eq!(trim(b"xxaxx", Some(b"x"), TrimMode::Left, &mut n), b"axx");
        assert_eq!(
            str_pad(b"5", 3, b"0", STR_PAD_LEFT).ok(),
            Some(b"005".to_vec())
        );
        assert_eq!(
            str_pad(b"ab", 7, b"xy", STR_PAD_BOTH).ok(),
            Some(b"xyabxyx".to_vec())
        );
        assert_eq!(
            wordwrap(b"The quick brown fox", 10, b"<br>", false).ok(),
            Some(b"The quick<br>brown fox".to_vec())
        );
        assert_eq!(
            wordwrap(b"A very long woooooooooooord.", 8, b"\n", true).ok(),
            Some(b"A very\nlong\nwooooooo\nooooord.".to_vec())
        );
        assert_eq!(
            nl2br(b"a\r\nb\n\rc\nd", false),
            b"a<br>\r\nb<br>\n\rc<br>\nd"
        );
        assert_eq!(strtr(b"Hi all", b"ai", b"eo"), b"Ho ell");
        let pairs = [(&b"Hi"[..], &b"Hello"[..]), (b"Hello", b"Hi"), (b"H", b"X")];
        assert_eq!(
            strtr_pairs(b"Hi all, I said Hello", &pairs, &mut n),
            b"Hello all, I said Hi"
        );
        assert_eq!(str_increment(b"Az").ok(), Some(b"Ba".to_vec()));
        assert_eq!(str_increment(b"zz").ok(), Some(b"aaa".to_vec()));
        assert_eq!(str_increment(b"99").ok(), Some(b"100".to_vec()));
        assert_eq!(str_decrement(b"Ba").ok(), Some(b"Az".to_vec()));
        assert_eq!(str_decrement(b"100").ok(), Some(b"99".to_vec()));
        assert_eq!(str_decrement(b"aaa").ok(), Some(b"zz".to_vec()));
        assert!(str_decrement(b"a").is_err());
        assert_eq!(basename(b"/etc/sudoers.d/", b".d"), b"sudoers");
        assert_eq!(dirname(b"/usr/local/lib", 2).ok(), Some(b"/usr".to_vec()));
        assert_eq!(dirname_once(b"a"), b".");
        assert_eq!(dirname_once(b"//a"), b"/");
        assert!(n.is_empty());
        assert_eq!(
            str_repeat(b"x", -1).map_err(|e| e.kind),
            Err(ErrorKind::ValueError)
        );
    }

    #[test]
    fn strtr_pair_conversion_is_lazy() {
        let pairs = [(&b""[..], 0), (b"a", 1), (b"aa", 2), (b"unused", 3)];
        let mut notices = Notices::new();
        let mut converted = Vec::new();
        let out = strtr_pairs_with(b"aaa", &pairs, &mut notices, |_, v, n, _| {
            if converted.is_empty() {
                assert_eq!(n.len(), 1);
                n.clear();
            }
            converted.push(*v);
            Ok::<_, ()>(v.to_string().into_bytes())
        });
        assert_eq!(out, Ok(b"21".to_vec()));
        assert_eq!(converted, [2, 1]);
        assert!(notices.is_empty());

        let out = strtr_pairs_with(b"", &pairs, &mut notices, |_, _, _, _| {
            Err::<Vec<u8>, _>("unexpected conversion")
        });
        assert_eq!(out, Ok(Vec::new()));
        assert!(notices.is_empty());
    }

    #[test]
    fn strtr_pair_conversion_errors_and_single_pair_order() {
        let mut notices = Notices::new();
        let pairs = [(&b""[..], 0)];
        let out = strtr_pairs_with(b"x", &pairs, &mut notices, |_, _, n, capacity| {
            assert!(n.is_empty());
            assert_eq!(capacity, 0);
            Ok::<_, ()>(b"ignored".to_vec())
        });
        assert_eq!(out, Ok(b"x".to_vec()));
        assert_eq!(notices.len(), 1);
        notices.clear();

        let out = strtr_pairs_with(b"x", &pairs, &mut notices, |_, _, _, _| {
            Err::<Vec<u8>, _>("conversion failed")
        });
        assert_eq!(out, Err("conversion failed"));
        assert!(notices.is_empty());

        let pairs = [(&b"a"[..], 0), (b"b", 1)];
        let mut converted = Vec::new();
        let out = strtr_pairs_with(b"ab", &pairs, &mut notices, |_, v, _, _| {
            converted.push(*v);
            Err::<Vec<u8>, _>("conversion failed")
        });
        assert_eq!(out, Err("conversion failed"));
        assert_eq!(converted, [0]);
    }
}
