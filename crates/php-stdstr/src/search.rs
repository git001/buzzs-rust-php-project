// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Searching and slicing from `ext/standard/string.c`: `substr()`,
//! `substr_replace()`, `substr_count()`, the `strpos()` family,
//! `strstr()`/`stristr()`/`strrchr()`, `str_contains()` & co.,
//! `strpbrk()`, `strspn()`/`strcspn()`, `explode()` and `strtok()`.
//!
//! String arguments are obtained by the VM with `Z_PARAM_STR` semantics and
//! integer arguments with `Z_PARAM_LONG` semantics; nullable integer
//! arguments (`?int $length = null`) are passed as `Option<i64>`.

use crate::error::{Notice, Notices, StdError, StdResult};
use crate::transform::{len_i64, to_usize};

/// `zend_memnstr()`: offset of the first occurrence of `needle` in
/// `haystack` (an empty needle matches at 0).
pub fn memnstr(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    match needle.len() {
        0 => Some(0),
        1 => haystack.iter().position(|&c| c == needle[0]),
        n if n > haystack.len() => None,
        n => {
            // `zend_memnstr()`: scan for the first byte, then check the
            // last byte before comparing the whole needle.
            let (first, last) = (needle[0], needle[n - 1]);
            let end = haystack.len() - n;
            let mut i = 0;
            while i <= end {
                i += haystack[i..=end].iter().position(|&c| c == first)?;
                if haystack[i + n - 1] == last && &haystack[i..i + n] == needle {
                    return Some(i);
                }
                i += 1;
            }
            None
        }
    }
}

/// `php_memnistr()`: offset of the first occurrence of `needle` in
/// `haystack` under ASCII case folding, compared in place (what
/// [`memnstr`] over both strings lowercased finds, without the copies).
pub fn memnistr(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    match needle.len() {
        0 => Some(0),
        n if n > haystack.len() => None,
        n => {
            let first = needle[0].to_ascii_lowercase();
            let end = haystack.len() - n;
            (0..=end).find(|&i| {
                haystack[i].to_ascii_lowercase() == first
                    && haystack[i..i + n].eq_ignore_ascii_case(needle)
            })
        }
    }
}

/// `zend_memnrstr()`: offset of the last occurrence of `needle` in
/// `haystack` (an empty needle matches at the end).
pub fn memnrstr(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    match needle.len() {
        0 => Some(haystack.len()),
        1 => haystack.iter().rposition(|&c| c == needle[0]),
        n if n > haystack.len() => None,
        n => haystack.windows(n).rposition(|w| w == needle),
    }
}

fn not_contained(func: &'static str, arg: u32, name: &str) -> StdError {
    StdError::arg_value(
        func,
        arg,
        name,
        "must be contained in argument #1 ($haystack)",
    )
}

/// Backs `substr(string $string, int $offset, ?int $length = null)`.
pub fn substr(s: &[u8], offset: i64, length: Option<i64>) -> &[u8] {
    let len = s.len();
    let f = if offset < 0 {
        // -(size_t)f > len  ⇔  |f| > len
        if offset.unsigned_abs() > len as u64 {
            0
        } else {
            len - to_usize(-offset)
        }
    } else if to_usize(offset) > len {
        return &[];
    } else {
        to_usize(offset)
    };
    let rest = len - f;
    let l = match length {
        None => rest,
        Some(l) if l < 0 => {
            if l.unsigned_abs() > rest as u64 {
                0
            } else {
                rest - to_usize(-l)
            }
        }
        Some(l) => to_usize(l).min(rest),
    };
    &s[f..f + l]
}

/// Backs `substr_replace()` for one string: replaces `length` bytes at
/// `offset` (both PHP-normalised; `None` length means "to the end") with
/// `replacement`.
///
/// For array arguments the VM calls this per element.  When `$offset` /
/// `$length` are arrays, a missing entry means offset `0` / length `None`;
/// when `$replace` is an array a missing entry means `""` (and in
/// single-string mode only its first element is used).  Array offsets or
/// lengths with a string subject are the `TypeError`s
/// [`substr_replace_array_error`] builds.
pub fn substr_replace(s: &[u8], replacement: &[u8], offset: i64, length: Option<i64>) -> Vec<u8> {
    let len = len_i64(s.len());
    let mut f = offset;
    if f < 0 {
        f = (len + f).max(0);
    } else if f > len {
        f = len;
    }
    let mut l = length.unwrap_or(len);
    if l < 0 {
        l = ((len - f) + l).max(0);
    }
    if l > len {
        l = len;
    }
    if f + l > len {
        l = len - f;
    }
    let (f, l) = (to_usize(f), to_usize(l));
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(s.len() - l + replacement.len());
    out.extend_from_slice(&s[..f]);
    out.extend_from_slice(replacement);
    out.extend_from_slice(&s[f + l..]);
    out
}

/// The `TypeError` for `substr_replace()` with a string subject and an
/// array `$offset` (`arg_num` 3) or `$length` (`arg_num` 4).
pub fn substr_replace_array_error(arg_num: u32) -> StdError {
    let name = if arg_num == 3 { "offset" } else { "length" };
    StdError::arg_type(
        "substr_replace",
        arg_num,
        name,
        "cannot be an array when working on a single string",
    )
}

/// Backs `substr_count(string $haystack, string $needle, int $offset = 0,
/// ?int $length = null)`.
pub fn substr_count(
    haystack: &[u8],
    needle: &[u8],
    offset: i64,
    length: Option<i64>,
) -> StdResult<i64> {
    if needle.is_empty() {
        return Err(StdError::arg_value(
            "substr_count",
            2,
            "needle",
            "must not be empty",
        ));
    }
    let mut h = haystack;
    if offset != 0 {
        let mut o = offset;
        if o < 0 {
            o += len_i64(h.len());
        }
        if o < 0 || to_usize(o) > h.len() {
            return Err(not_contained("substr_count", 3, "offset"));
        }
        h = &h[to_usize(o)..];
    }
    if let Some(mut l) = length {
        if l < 0 {
            l += len_i64(h.len());
        }
        if l < 0 || to_usize(l) > h.len() {
            return Err(not_contained("substr_count", 4, "length"));
        }
        h = &h[..to_usize(l)];
    }
    let mut count = 0;
    let mut p = 0;
    while let Some(r) = memnstr(&h[p..], needle) {
        p += r + needle.len();
        count += 1;
    }
    Ok(count)
}

fn forward_offset(func: &'static str, len: usize, offset: i64) -> StdResult<usize> {
    let o = if offset < 0 {
        offset + len_i64(len)
    } else {
        offset
    };
    if o < 0 || to_usize(o) > len {
        return Err(not_contained(func, 3, "offset"));
    }
    Ok(to_usize(o))
}

/// Backs `strpos(string $haystack, string $needle, int $offset = 0):
/// int|false` (`None` = `false`).
pub fn strpos(haystack: &[u8], needle: &[u8], offset: i64) -> StdResult<Option<i64>> {
    let o = forward_offset("strpos", haystack.len(), offset)?;
    Ok(memnstr(&haystack[o..], needle).map(|p| len_i64(o + p)))
}

/// Backs `stripos()`: like [`strpos`] with ASCII case folding.
pub fn stripos(haystack: &[u8], needle: &[u8], offset: i64) -> StdResult<Option<i64>> {
    let o = forward_offset("stripos", haystack.len(), offset)?;
    Ok(memnistr(&haystack[o..], needle).map(|p| len_i64(o + p)))
}

/// Computes the `[p, e)` window `strrpos()`/`strripos()` search in.
fn reverse_window(
    func: &'static str,
    len: usize,
    needle_len: usize,
    offset: i64,
) -> StdResult<(usize, usize)> {
    if offset >= 0 {
        if to_usize(offset) > len {
            return Err(not_contained(func, 3, "offset"));
        }
        Ok((to_usize(offset), len))
    } else {
        let neg = offset.unsigned_abs();
        if offset < -i64::MAX || neg > len as u64 {
            return Err(not_contained(func, 3, "offset"));
        }
        let neg = usize::try_from(neg).unwrap_or(usize::MAX);
        let e = if neg < needle_len {
            len
        } else {
            len - neg + needle_len
        };
        Ok((0, e))
    }
}

/// Backs `strrpos(string $haystack, string $needle, int $offset = 0)`.
pub fn strrpos(haystack: &[u8], needle: &[u8], offset: i64) -> StdResult<Option<i64>> {
    let (p, e) = reverse_window("strrpos", haystack.len(), needle.len(), offset)?;
    Ok(memnrstr(&haystack[p..e], needle).map(|r| len_i64(p + r)))
}

/// Backs `strripos()`: like [`strrpos`] with ASCII case folding.
pub fn strripos(haystack: &[u8], needle: &[u8], offset: i64) -> StdResult<Option<i64>> {
    let len = haystack.len();
    if needle.len() == 1 {
        // Single-byte fast path: the window ends at the last byte (or at
        // len + offset for negative offsets) inclusively.
        let (p, e_incl) = if offset >= 0 {
            if to_usize(offset) > len {
                return Err(not_contained("strripos", 3, "offset"));
            }
            (to_usize(offset), len.checked_sub(1))
        } else {
            let neg = offset.unsigned_abs();
            if offset < -i64::MAX || neg > len as u64 {
                return Err(not_contained("strripos", 3, "offset"));
            }
            (0, Some(len - usize::try_from(neg).unwrap_or(len)))
        };
        let lowered = needle[0].to_ascii_lowercase();
        let Some(mut e) = e_incl else { return Ok(None) };
        loop {
            if e < p {
                return Ok(None);
            }
            if haystack.get(e).copied().unwrap_or(0).to_ascii_lowercase() == lowered {
                return Ok(Some(len_i64(e)));
            }
            if e == 0 {
                return Ok(None);
            }
            e -= 1;
        }
    }
    let (p, e) = reverse_window("strripos", len, needle.len(), offset)?;
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    Ok(memnrstr(&h[p..e], &n).map(|r| len_i64(p + r)))
}

fn split_at_found(haystack: &[u8], found: Option<usize>, before: bool) -> Option<Vec<u8>> {
    found.map(|f| {
        if before {
            haystack[..f].to_vec()
        } else {
            haystack[f..].to_vec()
        }
    })
}

/// Backs `strstr(string $haystack, string $needle, bool $before_needle =
/// false): string|false`.
pub fn strstr(haystack: &[u8], needle: &[u8], before_needle: bool) -> Option<Vec<u8>> {
    split_at_found(haystack, memnstr(haystack, needle), before_needle)
}

/// Backs `stristr()`: like [`strstr`] with ASCII case folding.
pub fn stristr(haystack: &[u8], needle: &[u8], before_needle: bool) -> Option<Vec<u8>> {
    let found = memnistr(haystack, needle);
    split_at_found(haystack, found, before_needle)
}

/// Backs `strrchr(string $haystack, string $needle, bool $before_needle =
/// false)`: searches for the last occurrence of the *first byte* of
/// `needle` (a NUL byte when `needle` is empty).
pub fn strrchr(haystack: &[u8], needle: &[u8], before_needle: bool) -> Option<Vec<u8>> {
    let c = needle.first().copied().unwrap_or(0);
    split_at_found(
        haystack,
        haystack.iter().rposition(|&x| x == c),
        before_needle,
    )
}

/// Backs `str_contains()`.
pub fn str_contains(haystack: &[u8], needle: &[u8]) -> bool {
    memnstr(haystack, needle).is_some()
}

/// Backs `str_starts_with()`.
pub fn str_starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.starts_with(needle)
}

/// Backs `str_ends_with()`.
pub fn str_ends_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.ends_with(needle)
}

/// `php_strcspn()`: length of the prefix of `s` made of bytes *not* in
/// `chars`.
fn strcspn_raw(s: &[u8], chars: &[u8]) -> usize {
    s.iter().position(|c| chars.contains(c)).unwrap_or(s.len())
}

/// Backs `strpbrk(string $string, string $characters): string|false`.
pub fn strpbrk(s: &[u8], chars: &[u8]) -> StdResult<Option<Vec<u8>>> {
    if chars.is_empty() {
        return Err(StdError::arg_value(
            "strpbrk",
            2,
            "characters",
            "must be a non-empty string",
        ));
    }
    let shift = strcspn_raw(s, chars);
    Ok((shift < s.len()).then(|| s[shift..].to_vec()))
}

fn spn_window(s: &[u8], offset: i64, length: Option<i64>) -> &[u8] {
    let len = len_i64(s.len());
    let start = if offset < 0 {
        (offset + len).max(0)
    } else {
        offset.min(len)
    };
    let remain = len - start;
    let l = match length {
        None => remain,
        Some(l) if l < 0 => (l + remain).max(0),
        Some(l) => l.min(remain),
    };
    &s[to_usize(start)..to_usize(start + l)]
}

/// Backs `strspn(string $string, string $characters, int $offset = 0,
/// ?int $length = null): int`.
pub fn strspn(s: &[u8], chars: &[u8], offset: i64, length: Option<i64>) -> i64 {
    let w = spn_window(s, offset, length);
    len_i64(w.iter().position(|c| !chars.contains(c)).unwrap_or(w.len()))
}

/// Backs `strcspn(string $string, string $characters, int $offset = 0,
/// ?int $length = null): int`.
pub fn strcspn(s: &[u8], chars: &[u8], offset: i64, length: Option<i64>) -> i64 {
    len_i64(strcspn_raw(spn_window(s, offset, length), chars))
}

/// Backs `explode(string $separator, string $string, int $limit =
/// PHP_INT_MAX): array`.
pub fn explode(delim: &[u8], s: &[u8], limit: i64) -> StdResult<Vec<Vec<u8>>> {
    Ok(explode_slices(delim, s, limit)?
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect())
}

/// [`explode`] as borrowed pieces of `s` (`php_explode()` /
/// `php_explode_negative_limit()`), so the caller can build each element
/// without an intermediate copy: a piece that is all of `s` is `s` itself.
pub fn explode_slices<'a>(delim: &[u8], s: &'a [u8], limit: i64) -> StdResult<Vec<&'a [u8]>> {
    if delim.is_empty() {
        return Err(StdError::arg_value(
            "explode",
            1,
            "separator",
            "must not be empty, use str_split() to split a string into characters",
        ));
    }
    if s.is_empty() {
        return Ok(if limit >= 0 { vec![s] } else { Vec::new() });
    }
    if limit == 0 || limit == 1 {
        return Ok(vec![s]);
    }
    let mut pieces: Vec<&[u8]> = Vec::new();
    let mut p = 0;
    while let Some(r) = memnstr(&s[p..], delim) {
        // With a positive limit the last element holds the unsplit
        // remainder.
        if limit > 1 && len_i64(pieces.len()) == limit - 1 {
            break;
        }
        pieces.push(&s[p..p + r]);
        p += r + delim.len();
    }
    pieces.push(&s[p..]);
    if limit < 0 {
        let n = pieces.len();
        if n == 1 {
            return Ok(Vec::new());
        }
        let keep = len_i64(n) + limit;
        pieces.truncate(if keep > 0 { to_usize(keep) } else { 0 });
    }
    Ok(pieces)
}

/// The per-request state behind `strtok()` (`BG(strtok_string)` /
/// `BG(strtok_last)`).
#[derive(Debug, Clone, Default)]
pub struct Strtok {
    state: Option<(Vec<u8>, usize)>,
}

impl Strtok {
    /// A tokenizer with no string set.
    pub fn new() -> Self {
        Self::default()
    }

    /// `strtok(string $string, string $token)`: starts tokenizing `s` and
    /// returns the first token (`None` = `false`).
    pub fn start(&mut self, s: &[u8], token: &[u8], notices: &mut Notices) -> Option<Vec<u8>> {
        self.state = Some((s.to_vec(), 0));
        self.next(token, notices)
    }

    /// `strtok(string $token)` (the one-argument form, and also the
    /// two-argument form with `$token === null`): returns the next token
    /// (`None` = `false`).  Warns when no string has been set.
    pub fn next(&mut self, token: &[u8], notices: &mut Notices) -> Option<Vec<u8>> {
        let Some((s, last)) = self.state.as_mut() else {
            notices.push(Notice::warning(
                "strtok",
                "Both arguments must be provided when starting tokenization",
            ));
            return None;
        };
        let pe = s.len();
        let mut p = *last;
        if p >= pe {
            return None;
        }
        let mut table = [false; 256];
        for &c in token {
            table[usize::from(c)] = true;
        }
        let mut skipped = 0;
        while table[usize::from(s[p])] {
            p += 1;
            if p >= pe {
                self.state = None;
                return None;
            }
            skipped += 1;
        }
        p += 1;
        while p < pe && !table[usize::from(s[p])] {
            p += 1;
        }
        let tok = s[*last + skipped..p].to_vec();
        *last = p + 1;
        Some(tok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memnistr_matches_lowercased_memnstr() {
        let hays: [&[u8]; 5] = [b"", b"A", b"Hello, World", b"aAaAb", b"x\xC4\xE4Ab"];
        let needles: [&[u8]; 7] = [b"", b"a", b"WORLD", b"o, w", b"AAB", b"\xe4", b"zz"];
        for h in hays {
            for n in needles {
                assert_eq!(
                    memnistr(h, n),
                    memnstr(&h.to_ascii_lowercase(), &n.to_ascii_lowercase()),
                    "{h:?} {n:?}"
                );
            }
        }
        // Oracle: stripos("Hello, World", "WORLD") is 7, stristr("aAaAb",
        // "AAB") is "aAb", stripos("x\xC4", "\xE4") is false (ASCII only).
        assert_eq!(stripos(b"Hello, World", b"WORLD", 0).ok(), Some(Some(7)));
        assert_eq!(stristr(b"aAaAb", b"AAB", false), Some(b"aAb".to_vec()));
        assert_eq!(stripos(b"x\xC4", b"\xE4", 0).ok(), Some(None));
    }

    #[test]
    fn memnstr_matches_naive_search() {
        let hays: [&[u8]; 6] = [
            b"",
            b"a",
            b"abcabcab",
            b"aaaaab",
            b"xyzxyzaxyzb",
            b"ab,cd,,ef,",
        ];
        let needles: [&[u8]; 8] = [
            b"a",
            b"ab",
            b"abc",
            b"cab",
            b"aab",
            b",,",
            b"zb",
            b"abcabcabc",
        ];
        for h in hays {
            for n in needles {
                let naive = if n.len() > h.len() {
                    None
                } else {
                    h.windows(n.len()).position(|w| w == n)
                };
                assert_eq!(memnstr(h, n), naive, "{h:?} {n:?}");
            }
        }
        assert_eq!(memnstr(b"abc", b""), Some(0));
    }

    #[test]
    fn substr_rules() {
        assert_eq!(substr(b"abcdef", -1, None), b"f");
        assert_eq!(substr(b"abcdef", -10, Some(2)), b"ab");
        assert_eq!(substr(b"abcdef", 10, None), b"");
        assert_eq!(substr(b"abcdef", 1, Some(-10)), b"");
        assert_eq!(substr(b"abc", i64::MIN, Some(i64::MIN)), b"");
        assert_eq!(substr_replace(b"Hello", b"J", 0, Some(1)), b"Jello");
        assert_eq!(substr_replace(b"Hello", b"!!", -1, Some(0)), b"Hell!!o");
    }

    #[test]
    fn explode_limits() {
        let e = |l| explode(b",", b"a,b,c", l).unwrap_or_default();
        assert_eq!(e(2), vec![b"a".to_vec(), b"b,c".to_vec()]);
        assert_eq!(e(-1), vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(e(0), vec![b"a,b,c".to_vec()]);
        assert_eq!(e(-5), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn explode_slices_match_php() {
        // Oracle: PHP 8.3 json_encode(explode(...)).
        let e = |d: &[u8], s: &'static [u8], l| explode_slices(d, s, l).unwrap_or_default();
        let none: Vec<&[u8]> = Vec::new();
        assert_eq!(e(b",", b"a,b,c", 3), vec![&b"a"[..], b"b", b"c"]);
        assert_eq!(e(b",", b"a,b,c", 4), vec![&b"a"[..], b"b", b"c"]);
        assert_eq!(e(b",", b",a,", -1), vec![&b""[..], b"a"]);
        assert_eq!(e(b",", b"", -1), none);
        assert_eq!(e(b",", b"", 0), vec![&b""[..]]);
        assert_eq!(e(b",", b"abc", -1), none);
        assert_eq!(e(b",,", b"a,,,b,,", 2), vec![&b"a"[..], b",b,,"]);
        assert_eq!(e(b",", b"a,b,c", -2), vec![&b"a"[..]]);
        assert!(explode_slices(b"", b"a", 1).is_err());
        // A piece that is the whole input is the input itself.
        let s: &[u8] = b"abc";
        assert!(std::ptr::eq(e(b",", s, i64::MAX)[0], s));
    }

    #[test]
    fn strtok_state() {
        let mut n = Notices::new();
        let mut t = Strtok::new();
        assert_eq!(t.start(b"  a b  ", b" ", &mut n), Some(b"a".to_vec()));
        assert_eq!(t.next(b" ", &mut n), Some(b"b".to_vec()));
        assert_eq!(t.next(b" ", &mut n), None);
        assert!(n.is_empty());
        assert_eq!(t.next(b" ", &mut n), None);
        assert_eq!(n.len(), 1);
    }

    #[test]
    fn positions() {
        assert_eq!(
            strrpos(b"0123456789a123456789b", b"7", -5).ok(),
            Some(Some(7))
        );
        assert_eq!(strripos(b"aXa", b"x", -2).ok(), Some(Some(1)));
        assert!(strpos(b"abc", b"a", 4).is_err());
        assert_eq!(strspn(b"42 is the answer", b"1234567890", 0, None), 2);
        assert_eq!(strcspn(b"abcd", b"cd", -3, Some(-1)), 1);
    }
}
