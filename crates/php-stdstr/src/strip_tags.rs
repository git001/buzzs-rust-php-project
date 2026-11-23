// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `strip_tags()` — port of `php_strip_tags_ex()` and `php_tag_find()` from
//! `ext/standard/string.c`.
//!
//! The state machine has five states: 0 = text (output), 1 = inside an HTML
//! tag, 2 = inside a PHP tag (`<?…?>`), 3 = inside a `<!…>` declaration and
//! 4 = inside an HTML comment (`<!--…-->`).  NUL bytes in text are
//! dropped, quotes inside tags hide `>`, and nested `<` inside a tag bump a
//! depth counter so that `<a <b>>` is stripped as a whole.

/// Builds the allowed-tags string for the array form of `strip_tags()`:
/// the VM converts every element with `zval_get_string()` and passes the
/// list here, which yields `"<tag1><tag2>…"`.  An empty list means "no
/// allowed tags" (pass `None` to [`strip_tags`]).
pub fn allowed_tags_from_list<T: AsRef<[u8]>>(tags: &[T]) -> Option<Vec<u8>> {
    if tags.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for t in tags {
        out.push(b'<');
        out.extend_from_slice(t.as_ref());
        out.push(b'>');
    }
    Some(out)
}

/// Backs `strip_tags(string $string, array|string|null $allowed_tags =
/// null): string`.
///
/// `allowed_tags` is the string form (`"<a><b>"`); for the array form use
/// [`allowed_tags_from_list`] first.  Matching is ASCII case-insensitive.
pub fn strip_tags(s: &[u8], allowed_tags: Option<&[u8]>) -> Vec<u8> {
    strip_tags_ex(s, allowed_tags, false)
}

/// `php_tag_find()`: normalises the collected tag (`<a href…>` → `<a>`,
/// `</a>` → `<a>`, `<br/>` → `<br>`) and looks it up in `set` (already
/// lowercased and truncated at its first NUL, like C's `strstr`).
fn tag_find(tag: &[u8], set: &[u8]) -> bool {
    if tag.is_empty() {
        return false;
    }
    let at = |i: usize| tag.get(i).copied().unwrap_or(0).to_ascii_lowercase();
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut norm = Vec::with_capacity(tag.len() + 1);
    let mut state = 0;
    let mut t = 0;
    loop {
        let c = at(t);
        match c {
            b'<' => norm.push(c),
            b'>' => break,
            _ => {
                if is_c_space(c) {
                    if state == 1 {
                        break;
                    }
                } else {
                    if state == 0 {
                        state = 1;
                    }
                    let prev = if t == 0 { 0 } else { tag[t - 1] };
                    if c != b'/' || (prev != b'<' && at(t + 1) != b'>') {
                        norm.push(c);
                    }
                }
            }
        }
        t += 1;
        if t > tag.len() {
            // Defensive: the C loop always finds the closing '>'.
            break;
        }
    }
    norm.push(b'>');
    set.windows(norm.len()).any(|w| w == norm.as_slice())
}

/// C-locale `isspace()`.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `php_strip_tags_ex()` with the `allow_tag_spaces` flag (`false` for
/// `strip_tags()`: a `<` followed by whitespace is kept as text).
// The `match` arms deliberately mirror the C `switch` cases one to one.
#[allow(clippy::too_many_lines, clippy::collapsible_match)]
pub fn strip_tags_ex(buf: &[u8], allow: Option<&[u8]>, allow_tag_spaces: bool) -> Vec<u8> {
    // `zend_str_tolower_dup_ex` + C-string semantics of `strstr`.
    let allow: Option<Vec<u8>> = allow.map(|a| {
        let a = a.iter().position(|&b| b == 0).map_or(a, |n| &a[..n]);
        a.to_ascii_lowercase()
    });
    let end = buf.len();
    let at = |i: usize| buf.get(i).copied().unwrap_or(0);
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(buf.len());
    let mut tbuf: Vec<u8> = Vec::new();
    let mut lc = 0u8;
    let mut br: i32 = 0;
    let mut depth: i32 = 0;
    let mut in_q = 0u8;
    let mut is_xml = false;
    let mut state = 0u8;
    let mut p = 0usize;

    while p < end {
        let c = buf[p];
        match state {
            0 => match c {
                0 => {}
                b'<' => {
                    if in_q == 0 {
                        if is_c_space(at(p + 1)) && !allow_tag_spaces {
                            out.push(c);
                        } else {
                            lc = b'<';
                            state = 1;
                            if allow.is_some() {
                                tbuf.push(b'<');
                            }
                        }
                    }
                }
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q == 0 {
                        out.push(c);
                    }
                }
                _ => out.push(c),
            },
            1 => {
                let mut reg_char = false;
                match c {
                    0 => {}
                    b'<' => {
                        if in_q == 0 {
                            if is_c_space(at(p + 1)) && !allow_tag_spaces {
                                reg_char = true;
                            } else {
                                depth += 1;
                            }
                        }
                    }
                    b'>' => {
                        if depth != 0 {
                            depth -= 1;
                        } else if in_q == 0 {
                            lc = b'>';
                            if !(is_xml && p >= 1 && buf[p - 1] == b'-') {
                                in_q = 0;
                                state = 0;
                                is_xml = false;
                                if let Some(set) = &allow {
                                    tbuf.push(b'>');
                                    if tag_find(&tbuf, set) {
                                        out.extend_from_slice(&tbuf);
                                    }
                                    tbuf.clear();
                                }
                            }
                        }
                    }
                    b'"' | b'\'' => {
                        if p != 0 && (in_q == 0 || c == in_q) {
                            in_q = if in_q != 0 { 0 } else { c };
                        }
                        reg_char = true;
                    }
                    b'!' => {
                        if p >= 1 && buf[p - 1] == b'<' {
                            state = 3;
                            lc = c;
                        } else {
                            reg_char = true;
                        }
                    }
                    b'?' => {
                        if p >= 1 && buf[p - 1] == b'<' {
                            br = 0;
                            state = 2;
                        } else {
                            reg_char = true;
                        }
                    }
                    _ => reg_char = true,
                }
                if reg_char && allow.is_some() {
                    tbuf.push(c);
                }
            }
            2 => match c {
                b'(' => {
                    if lc != b'"' && lc != b'\'' {
                        lc = b'(';
                        br += 1;
                    }
                }
                b')' => {
                    if lc != b'"' && lc != b'\'' {
                        lc = b')';
                        br -= 1;
                    }
                }
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q == 0 && br == 0 && p >= 1 && lc != b'"' && buf[p - 1] == b'?' {
                        in_q = 0;
                        state = 0;
                        tbuf.clear();
                    }
                }
                b'"' | b'\'' => {
                    if p >= 1 && buf[p - 1] != b'\\' {
                        if lc == c {
                            lc = 0;
                        } else if lc != b'\\' {
                            lc = c;
                        }
                        if p != 0 && (in_q == 0 || c == in_q) {
                            in_q = if in_q != 0 { 0 } else { c };
                        }
                    }
                }
                b'l' | b'L' => {
                    if p > 4
                        && matches!(buf[p - 1], b'm' | b'M')
                        && matches!(buf[p - 2], b'x' | b'X')
                        && buf[p - 3] == b'?'
                        && buf[p - 4] == b'<'
                    {
                        state = 1;
                        is_xml = true;
                    }
                }
                _ => {}
            },
            3 => match c {
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q == 0 {
                        in_q = 0;
                        state = 0;
                        tbuf.clear();
                    }
                }
                b'"' | b'\'' => {
                    if p != 0 && buf[p - 1] != b'\\' && (in_q == 0 || c == in_q) {
                        in_q = if in_q != 0 { 0 } else { c };
                    }
                }
                b'-' => {
                    if p >= 2 && buf[p - 1] == b'-' && buf[p - 2] == b'!' {
                        state = 4;
                    }
                }
                b'E' | b'e' => {
                    if p > 6 && buf[p - 6..p].eq_ignore_ascii_case(b"doctyp") {
                        state = 1;
                    }
                }
                _ => {}
            },
            _ => {
                if c == b'>' && in_q == 0 && p >= 2 && buf[p - 1] == b'-' && buf[p - 2] == b'-' {
                    in_q = 0;
                    state = 0;
                    tbuf.clear();
                }
            }
        }
        p += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        assert_eq!(strip_tags(b"<b>bold</b> text", None), b"bold text");
        assert_eq!(
            strip_tags(b"<p>Hi <a href='x'>l</a><br/></p>", Some(b"<a><br>")),
            b"Hi <a href='x'>l</a><br/>"
        );
        assert_eq!(strip_tags(b"a < b", None), b"a < b");
        assert_eq!(strip_tags(b"x<!-- c -->y", None), b"xy");
        assert_eq!(strip_tags(b"x<?php echo '>'; ?>y", None), b"xy");
        assert_eq!(strip_tags(b"a\0b", None), b"ab");
        assert_eq!(
            allowed_tags_from_list(&[b"a".as_slice(), b"b"]),
            Some(b"<a><b>".to_vec())
        );
    }
}
