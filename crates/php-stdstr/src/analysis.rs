// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Character-set masks and string "analysis" functions from
//! `ext/standard/string.c`, `levenshtein.c`, `soundex.c` and `metaphone.c`:
//! `php_charmask`, `similar_text`, `levenshtein`, `soundex`, `metaphone`,
//! `str_word_count` and `count_chars`.
//!
//! All functions operate on bytes and use the C locale's (ASCII-only)
//! character classes, as PHP 8 does.

use crate::error::{Notice, Notices, StdError, StdResult};

/// Port of `php_charmask()`: turns a character list such as `"a..z_"` into
/// a 256-entry membership mask.
///
/// Used by `trim()`/`ltrim()`/`rtrim()`, `ucwords()`, `addcslashes()` and
/// `str_word_count()`.  Malformed `..` ranges push one of PHP's
/// `Invalid '..'-range…` warnings, attributed to `func`, and are otherwise
/// skipped exactly like the C code (the dots of a bad range may still end
/// up in the mask).
pub fn charmask(input: &[u8], func: &'static str, notices: &mut Notices) -> [bool; 256] {
    let mut mask = [false; 256];
    let len = input.len();
    let mut i = 0;
    while i < len {
        let c = input[i];
        if i + 3 < len && input[i + 1] == b'.' && input[i + 2] == b'.' && input[i + 3] >= c {
            for m in &mut mask[usize::from(c)..=usize::from(input[i + 3])] {
                *m = true;
            }
            i += 4;
            continue;
        } else if i + 1 < len && input[i] == b'.' && input[i + 1] == b'.' {
            let msg = if i == 0 {
                "Invalid '..'-range, no character to the left of '..'"
            } else if i + 2 >= len {
                "Invalid '..'-range, no character to the right of '..'"
            } else if input[i - 1] > input[i + 2] {
                "Invalid '..'-range, '..'-range needs to be incrementing"
            } else {
                "Invalid '..'-range"
            };
            notices.push(Notice::warning(func, msg));
        } else {
            mask[usize::from(c)] = true;
        }
        i += 1;
    }
    mask
}

/// `php_similar_str()`: the first longest common substring.
/// Returns `(pos1, pos2, max, count)`.
fn similar_str(a: &[u8], b: &[u8]) -> (usize, usize, usize, usize) {
    let (mut pos1, mut pos2, mut max, mut count) = (0, 0, 0, 0);
    for p in 0..a.len() {
        for q in 0..b.len() {
            let l = a[p..]
                .iter()
                .zip(&b[q..])
                .take_while(|(x, y)| x == y)
                .count();
            if l > max {
                max = l;
                count += 1;
                pos1 = p;
                pos2 = q;
            }
        }
    }
    (pos1, pos2, max, count)
}

/// `php_similar_char()`.
fn similar_char(a: &[u8], b: &[u8]) -> usize {
    let (pos1, pos2, max, count) = similar_str(a, b);
    let mut sum = max;
    if sum != 0 {
        if pos1 != 0 && pos2 != 0 && count > 1 {
            sum += similar_char(&a[..pos1], &b[..pos2]);
        }
        if pos1 + max < a.len() && pos2 + max < b.len() {
            sum += similar_char(&a[pos1 + max..], &b[pos2 + max..]);
        }
    }
    sum
}

/// Backs `similar_text(string $string1, string $string2, &$percent = null): int`.
///
/// Returns the number of matching characters and the similarity percentage
/// (`sim * 200.0 / (len1 + len2)`, or `0.0` for two empty strings).  The VM
/// assigns the percentage to the by-reference third argument only when it
/// was passed.
#[allow(clippy::cast_precision_loss)]
pub fn similar_text(s1: &[u8], s2: &[u8]) -> (i64, f64) {
    if s1.is_empty() && s2.is_empty() {
        return (0, 0.0);
    }
    let sim = similar_char(s1, s2);
    let percent = sim as f64 * 200.0 / (s1.len() + s2.len()) as f64;
    (len_i64(sim), percent)
}

#[allow(clippy::cast_possible_wrap)]
fn len_i64(n: usize) -> i64 {
    n as i64
}

/// Backs `levenshtein(string $string1, string $string2, int
/// $insertion_cost = 1, int $replacement_cost = 1, int $deletion_cost = 1): int`
/// (`reference_levdist`).  There is no length limit since PHP 8.0.
/// Arithmetic wraps like the C `zend_long` code.
pub fn levenshtein(s1: &[u8], s2: &[u8], cost_ins: i64, cost_rep: i64, cost_del: i64) -> i64 {
    if s1.is_empty() {
        return len_i64(s2.len()).wrapping_mul(cost_ins);
    }
    if s2.is_empty() {
        return len_i64(s1.len()).wrapping_mul(cost_del);
    }
    let (s1, s2) = if s1.len() < s2.len() && cost_ins == cost_rep && cost_rep == cost_del {
        (s2, s1)
    } else {
        (s1, s2)
    };
    let mut p1: Vec<i64> = (0..=s2.len())
        .map(|i| len_i64(i).wrapping_mul(cost_ins))
        .collect();
    let mut p2 = vec![0i64; s2.len() + 1];
    for &c1 in s1 {
        p2[0] = p1[0].wrapping_add(cost_del);
        for (i2, &c2) in s2.iter().enumerate() {
            let mut c0 = p1[i2].wrapping_add(if c1 == c2 { 0 } else { cost_rep });
            let d = p1[i2 + 1].wrapping_add(cost_del);
            if d < c0 {
                c0 = d;
            }
            let ins = p2[i2].wrapping_add(cost_ins);
            if ins < c0 {
                c0 = ins;
            }
            p2[i2 + 1] = c0;
        }
        std::mem::swap(&mut p1, &mut p2);
    }
    p1[s2.len()]
}

/// Backs `soundex(string $string): string`.  Always returns four bytes
/// (`"0000"` for input without ASCII letters, including `""`).
pub fn soundex(s: &[u8]) -> Vec<u8> {
    const TABLE: [u8; 26] = [
        0, b'1', b'2', b'3', 0, b'1', b'2', 0, 0, b'2', b'2', b'4', b'5', b'5', 0, b'1', b'2',
        b'6', b'2', b'3', 0, b'1', 0, b'2', 0, b'2',
    ];
    #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
    let mut out = Vec::with_capacity(4);
    let mut last = 0xffu8; // (size_t)-1: never equal to a table code
    for &b in s {
        if out.len() >= 4 {
            break;
        }
        let code = b.to_ascii_uppercase();
        if code.is_ascii_uppercase() {
            let t = TABLE[usize::from(code - b'A')];
            if out.is_empty() {
                out.push(code);
                last = t;
            } else if t != last {
                if t != 0 {
                    out.push(t);
                }
                last = t;
            }
        }
    }
    #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
    out.resize(4, b'0');
    out
}

/// Metaphone letter classes (`_codes`), indexed by uppercase letter.
const META_CODES: [u8; 26] = [
    1, 16, 4, 16, 9, 2, 4, 16, 9, 2, 0, 2, 2, 2, 1, 4, 0, 2, 4, 4, 1, 0, 0, 0, 8, 0,
];

fn meta_encode(c: u8) -> u8 {
    if c.is_ascii_uppercase() {
        META_CODES[usize::from(c - b'A')]
    } else {
        0
    }
}
fn is_vowel(c: u8) -> bool {
    meta_encode(c) & 1 != 0
}
fn affect_h(c: u8) -> bool {
    meta_encode(c) & 4 != 0
}
fn make_soft(c: u8) -> bool {
    meta_encode(c) & 8 != 0
}
fn no_gh_to_f(c: u8) -> bool {
    meta_encode(c) & 16 != 0
}

/// Backs `metaphone(string $string, int $max_phonemes = 0): string`.
///
/// Throws `ValueError` for a negative `max_phonemes`.  The input is treated
/// as a C string: processing stops at the first NUL byte.
///
/// php-src master marks the function `#[\Deprecated(since: '8.6')]`; the
/// VM (not this function) emits `Function metaphone() is deprecated since
/// 8.6, use a userland phonetic matching library instead` on each call.
pub fn metaphone(word: &[u8], max_phonemes: i64) -> StdResult<Vec<u8>> {
    if max_phonemes < 0 {
        return Err(StdError::arg_value(
            "metaphone",
            2,
            "max_phonemes",
            "must be greater than or equal to 0",
        ));
    }
    let max = usize::try_from(max_phonemes).unwrap_or(usize::MAX);
    Ok(metaphone_impl(word, max, true))
}

/// The metaphone state machine; `max_phonemes == 0` means unlimited.
// The `match` arms deliberately mirror the C `switch` cases one to one.
#[allow(clippy::too_many_lines, clippy::collapsible_match)]
fn metaphone_impl(word: &[u8], max_phonemes: usize, traditional: bool) -> Vec<u8> {
    const SH: u8 = b'X';
    const TH: u8 = b'0';
    // The C code walks a NUL-terminated buffer.
    let at = |i: usize| word.get(i).copied().unwrap_or(0);
    let up = |i: usize| at(i).to_ascii_uppercase();
    let next = |w: usize| up(w + 1);
    let after_next = |w: usize| if at(w + 1) != 0 { up(w + 2) } else { 0 };
    let look_back = |w: usize, n: usize| if w >= n { up(w - n) } else { 0 };
    let look_ahead = |w: usize, n: usize| {
        let mut idx = 0;
        while at(w + idx) != 0 && idx < n {
            idx += 1;
        }
        up(w + idx)
    };
    let mut out = Vec::new();
    let mut w = 0usize;

    // Find the first letter.
    loop {
        let c = at(w);
        if c.is_ascii_alphabetic() {
            break;
        }
        if c == 0 {
            return out;
        }
        w += 1;
    }

    match up(w) {
        b'A' => {
            if next(w) == b'E' {
                out.push(b'E');
                w += 2;
            } else {
                out.push(b'A');
                w += 1;
            }
        }
        b'G' | b'K' | b'P' => {
            if next(w) == b'N' {
                out.push(b'N');
                w += 2;
            }
        }
        b'W' => {
            let n = next(w);
            if n == b'R' {
                out.push(b'R');
                w += 2;
            } else if n == b'H' || is_vowel(n) {
                out.push(b'W');
                w += 2;
            }
        }
        b'X' => {
            out.push(b'S');
            w += 1;
        }
        c @ (b'E' | b'I' | b'O' | b'U') => {
            out.push(c);
            w += 1;
        }
        _ => {}
    }

    while at(w) != 0 && (max_phonemes == 0 || out.len() < max_phonemes) {
        let raw = at(w);
        if !raw.is_ascii_alphabetic() {
            w += 1;
            continue;
        }
        let mut skip = 0;
        let cur = raw.to_ascii_uppercase();
        let prev = look_back(w, 1);
        if cur == prev && cur != b'C' {
            w += 1;
            continue;
        }
        match cur {
            b'B' => {
                if prev != b'M' {
                    out.push(b'B');
                }
            }
            b'C' => {
                let n = next(w);
                if make_soft(n) {
                    if n == b'I' && after_next(w) == b'A' {
                        out.push(SH);
                    } else if prev != b'S' {
                        out.push(b'S');
                    }
                } else if n == b'H' {
                    if !traditional && (prev == b'S' || after_next(w) == b'R') {
                        out.push(b'K');
                    } else {
                        out.push(SH);
                    }
                    skip += 1;
                } else {
                    out.push(b'K');
                }
            }
            b'D' => {
                if next(w) == b'G' && make_soft(after_next(w)) {
                    out.push(b'J');
                    skip += 1;
                } else {
                    out.push(b'T');
                }
            }
            b'G' => {
                let n = next(w);
                if n == b'H' {
                    if !(no_gh_to_f(look_back(w, 3)) || look_back(w, 4) == b'H') {
                        out.push(b'F');
                        skip += 1;
                    }
                } else if n == b'N' {
                    let an = after_next(w);
                    let dropped =
                        !an.is_ascii_alphabetic() || (an == b'E' && look_ahead(w, 3) == b'D');
                    if !dropped {
                        out.push(b'K');
                    }
                } else if make_soft(n) && prev != b'G' {
                    out.push(b'J');
                } else {
                    out.push(b'K');
                }
            }
            b'H' => {
                if is_vowel(next(w)) && !affect_h(prev) {
                    out.push(b'H');
                }
            }
            b'K' => {
                if prev != b'C' {
                    out.push(b'K');
                }
            }
            b'P' => out.push(if next(w) == b'H' { b'F' } else { b'P' }),
            b'Q' => out.push(b'K'),
            b'S' => {
                let n = next(w);
                if n == b'I' && matches!(after_next(w), b'O' | b'A') {
                    out.push(SH);
                } else if n == b'H' {
                    out.push(SH);
                    skip += 1;
                } else if !traditional
                    && n == b'C'
                    && look_ahead(w, 2) == b'H'
                    && look_ahead(w, 3) == b'W'
                {
                    out.push(SH);
                    skip += 2;
                } else {
                    out.push(b'S');
                }
            }
            b'T' => {
                let n = next(w);
                if n == b'I' && matches!(after_next(w), b'O' | b'A') {
                    out.push(SH);
                } else if n == b'H' {
                    out.push(TH);
                    skip += 1;
                } else if !(n == b'C' && after_next(w) == b'H') {
                    out.push(b'T');
                }
            }
            b'V' => out.push(b'F'),
            b'W' => {
                if is_vowel(next(w)) {
                    out.push(b'W');
                }
            }
            b'X' => out.extend_from_slice(b"KS"),
            b'Y' => {
                if is_vowel(next(w)) {
                    out.push(b'Y');
                }
            }
            b'Z' => out.push(b'S'),
            b'F' | b'J' | b'L' | b'M' | b'N' | b'R' => out.push(cur),
            _ => {}
        }
        w += skip + 1;
    }
    out
}

/// Result of [`str_word_count`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordCount {
    /// Format 0: the number of words.
    Count(i64),
    /// Format 1: the words, as a list.
    Words(Vec<Vec<u8>>),
    /// Format 2: `(byte offset, word)` pairs; the offsets are the array keys.
    Positions(Vec<(i64, Vec<u8>)>),
}

/// Backs `str_word_count(string $string, int $format = 0, ?string
/// $characters = null): array|int`.
///
/// Words are runs of ASCII letters, `'`, `-` and the bytes in
/// `characters` (a [`charmask`] list).  A leading `'`/`-` and a trailing
/// `-` are not part of a word unless listed in `characters`.  Throws
/// `ValueError` for a format other than 0, 1 or 2.
pub fn str_word_count(
    s: &[u8],
    format: i64,
    characters: Option<&[u8]>,
    notices: &mut Notices,
) -> StdResult<WordCount> {
    let mut result = match format {
        0 => WordCount::Count(0),
        1 => WordCount::Words(Vec::new()),
        2 => WordCount::Positions(Vec::new()),
        _ => {
            return Err(StdError::arg_value(
                "str_word_count",
                2,
                "format",
                "must be a valid format value",
            ))
        }
    };
    if s.is_empty() {
        return Ok(result);
    }
    let mask = characters.map(|c| charmask(c, "str_word_count", notices));
    let allowed = |b: u8| mask.as_ref().is_some_and(|m| m[usize::from(b)]);
    let mut p = 0;
    let mut e = s.len();
    if (s[0] == b'\'' && !allowed(b'\'')) || (s[0] == b'-' && !allowed(b'-')) {
        p += 1;
    }
    if s[e - 1] == b'-' && !allowed(b'-') {
        e -= 1;
    }
    let in_word = |b: u8| b.is_ascii_alphabetic() || allowed(b) || b == b'\'' || b == b'-';
    while p < e {
        let start = p;
        while p < e && in_word(s[p]) {
            p += 1;
        }
        if p > start {
            let word = &s[start..p];
            match &mut result {
                WordCount::Count(n) => *n += 1,
                WordCount::Words(v) => v.push(word.to_vec()),
                WordCount::Positions(v) => v.push((len_i64(start), word.to_vec())),
            }
        }
        p += 1;
    }
    Ok(result)
}

/// Result of [`count_chars`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharCounts {
    /// Modes 0–2: `byte value => count` pairs in ascending byte order.
    Array(Vec<(u8, i64)>),
    /// Modes 3–4: the (unique / unused) bytes in ascending order.
    Bytes(Vec<u8>),
}

/// Backs `count_chars(string $string, int $mode = 0): array|string`.
///
/// * 0 — every byte value with its count;
/// * 1 — only byte values that occur;
/// * 2 — only byte values that do not occur (count 0);
/// * 3 — string of the distinct bytes used;
/// * 4 — string of the bytes not used.
///
/// Throws `ValueError` for any other mode.
pub fn count_chars(s: &[u8], mode: i64) -> StdResult<CharCounts> {
    if !(0..=4).contains(&mode) {
        return Err(StdError::arg_value(
            "count_chars",
            2,
            "mode",
            "must be between 0 and 4 (inclusive)",
        ));
    }
    let mut counts = [0i64; 256];
    for &b in s {
        counts[usize::from(b)] += 1;
    }
    let iter = (0..=255u8).map(|b| (b, counts[usize::from(b)]));
    Ok(match mode {
        0 => CharCounts::Array(iter.collect()),
        1 => CharCounts::Array(iter.filter(|&(_, n)| n != 0).collect()),
        2 => CharCounts::Array(iter.filter(|&(_, n)| n == 0).collect()),
        3 => CharCounts::Bytes(iter.filter(|&(_, n)| n != 0).map(|(b, _)| b).collect()),
        _ => CharCounts::Bytes(iter.filter(|&(_, n)| n == 0).map(|(b, _)| b).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_of(s: &[u8]) -> (Vec<u8>, Vec<String>) {
        let mut n = Vec::new();
        let m = charmask(s, "trim", &mut n);
        let set = (0..=255u8).filter(|&b| m[usize::from(b)]).collect();
        (set, n.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn charmask_ranges() {
        assert_eq!(mask_of(b"a..e").0, b"abcde");
        assert_eq!(mask_of(b"x").0, b"x");
        let (set, w) = mask_of(b"..a");
        assert_eq!(set, b".a");
        assert_eq!(
            w,
            ["trim(): Invalid '..'-range, no character to the left of '..'"]
        );
        let (_, w) = mask_of(b"a..");
        assert_eq!(
            w,
            ["trim(): Invalid '..'-range, no character to the right of '..'"]
        );
        let (_, w) = mask_of(b"z..a");
        assert_eq!(
            w,
            ["trim(): Invalid '..'-range, '..'-range needs to be incrementing"]
        );
        let (set, w) = mask_of(b"a..b..c");
        assert_eq!(set, b".abc");
        assert_eq!(w, ["trim(): Invalid '..'-range"]);
    }

    #[test]
    fn similar_and_lev() {
        let (n, p) = similar_text(b"World", b"Word");
        assert_eq!(n, 4);
        assert!((p - 88.888_888_888_888_89).abs() < 1e-9);
        assert_eq!(similar_text(b"", b""), (0, 0.0));
        assert_eq!(levenshtein(b"kitten", b"sitting", 1, 1, 1), 3);
        assert_eq!(levenshtein(b"", b"abc", 2, 1, 1), 6);
        assert_eq!(levenshtein(b"abc", b"", 2, 1, 5), 15);
    }

    #[test]
    fn soundex_metaphone() {
        assert_eq!(soundex(b"Robert"), b"R163");
        assert_eq!(soundex(b""), b"0000");
        assert_eq!(metaphone(b"Thompson", 0).unwrap_or_default(), b"0MPSN");
        assert_eq!(metaphone(b"Knight", 0).unwrap_or_default(), b"NFT");
        assert!(metaphone(b"x", -1).is_err());
    }

    #[test]
    fn words_and_counts() {
        let mut n = Vec::new();
        assert_eq!(
            str_word_count(b"Hello fri3nd, you're looking good today!", 0, None, &mut n),
            Ok(WordCount::Count(7))
        );
        assert!(str_word_count(b"x", 3, None, &mut n).is_err());
        assert_eq!(
            count_chars(b"abca", 3),
            Ok(CharCounts::Bytes(b"abc".to_vec()))
        );
        assert!(count_chars(b"", 5).is_err());
    }
}
