// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Small byte-level helpers shared by the formats: little-endian fields,
//! the entry inode, the `phar_path_check()` scanner, `phar_fix_filepath()`,
//! alias validation and case-insensitive searches.

/// Reads a little-endian `u32` at `pos` (callers guarantee bounds).
pub(crate) fn get32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
}

/// Reads a little-endian `u16` at `pos` (callers guarantee bounds).
pub(crate) fn get16(buf: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([buf[pos], buf[pos + 1]])
}

/// Appends a little-endian `u32`.
pub(crate) fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Appends a little-endian `u16`.
pub(crate) fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Saturating `usize` → `u32` conversion for on-disk size fields.
pub(crate) fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// The low 16 bits of a length (C's implicit `uint16_t` truncation).
pub(crate) fn lo16(v: usize) -> u16 {
    u16::try_from(v & 0xFFFF).unwrap_or_default()
}

/// The low 32 bits of a value (C's implicit `uint32_t` truncation).
pub(crate) fn lo32(v: i64) -> u32 {
    u32::try_from(v & 0xFFFF_FFFF).unwrap_or_default()
}

/// `usize` → `i64`, saturating.
pub(crate) fn to_i64(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// `phar_set_inode()`: the fake inode of an entry, derived from the archive
/// path and the entry name (PHP 8.3 semantics; master hashes the
/// `zend_string` header by mistake).
pub fn entry_inode(fname: &[u8], filename: &[u8]) -> u16 {
    const MAXPATHLEN: usize = 4096;
    let mut tmp = Vec::with_capacity(fname.len() + filename.len());
    tmp.extend_from_slice(fname);
    tmp.extend_from_slice(filename);
    tmp.truncate(MAXPATHLEN);
    // `(unsigned short)` keeps the low 16 bits.
    u16::try_from(php_runtime::string::zend_hash_bytes(&tmp) & 0xFFFF).unwrap_or_default()
}

/// `phar_validate_alias()`: aliases may not contain `/ \ : ; \r \n`.
pub fn validate_alias(alias: &[u8]) -> bool {
    !alias
        .iter()
        .any(|c| matches!(c, b'/' | b'\\' | b':' | b';' | b'\n' | b'\r'))
}

/// `phar_path_is_magic_phar_ex()`: `.phar`, `/.phar`, `.phar/...`.
pub fn is_magic_phar(path: &[u8]) -> bool {
    let p = path.strip_prefix(b"/").unwrap_or(path);
    if !p.starts_with(b".phar") {
        return false;
    }
    p.len() == 5 || p[5] == b'/' || p[5] == b'\\'
}

/// The result of `phar_path_check()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCheck {
    /// `pcr_use_query`: the path is valid up to a `?`.
    UseQuery,
    /// `pcr_is_ok`.
    Ok,
    /// An error with its description ("double slash", ...).
    Err(&'static str),
}

impl PathCheck {
    /// Whether the check failed (`> pcr_is_ok`).
    pub fn is_err(self) -> bool {
        matches!(self, PathCheck::Err(_))
    }
}

fn utf8_len(s: &[u8], i: usize) -> Option<usize> {
    let at = |k: usize| s.get(i + k).copied().unwrap_or(0);
    let t = |k: usize| (0x80..=0xBF).contains(&at(k));
    let c = at(0);
    match c {
        0x1A..=0x7F => Some(1),
        0xC2..=0xDF if t(1) => Some(2),
        0xE0 if (0xA0..=0xBF).contains(&at(1)) && t(2) => Some(3),
        0xE1..=0xEC | 0xEE..=0xEF if t(1) && t(2) => Some(3),
        0xED if (0x80..=0x9F).contains(&at(1)) && t(2) => Some(3),
        0xF0 if (0x90..=0xBF).contains(&at(1)) && t(2) && t(3) => Some(4),
        0xF1..=0xF3 if t(1) && t(2) && t(3) => Some(4),
        0xF4 if (0x80..=0x8F).contains(&at(1)) && t(2) && t(3) => Some(4),
        _ => None,
    }
}

/// `phar_path_check()`: validates an entry path. On success returns the
/// (possibly shortened) path: a leading `/` is stripped and a query string
/// (`?...`) cut off.
pub fn path_check(path: &[u8]) -> (PathCheck, &[u8]) {
    if path == b"." {
        return (PathCheck::Err("current directory reference"), path);
    }
    if path == b".." {
        return (PathCheck::Err("upper directory reference"), path);
    }
    // The scanner runs over the NUL-terminated C string.
    let at = |k: usize| path.get(k).copied().unwrap_or(0);
    let eos = |k: usize| at(k) == b'/' || at(k) == 0;
    let mut p = 0usize;
    loop {
        let c = at(p);
        if c == b'/' && at(p + 1) == b'.' && at(p + 2) == b'.' && eos(p + 3) {
            return (PathCheck::Err("upper directory reference"), path);
        }
        if c == b'/' && at(p + 1) == b'/' {
            return (PathCheck::Err("double slash"), path);
        }
        if c == b'/' && at(p + 1) == b'.' && eos(p + 2) {
            return (PathCheck::Err("current directory reference"), path);
        }
        match c {
            b'\\' => return (PathCheck::Err("back-slash"), path),
            b'*' => return (PathCheck::Err("star"), path),
            b'?' => {
                let start = usize::from(path.first() == Some(&b'/'));
                let end = p.max(start);
                return (PathCheck::UseQuery, &path[start..end]);
            }
            0 => {
                let start = usize::from(path.first() == Some(&b'/'));
                // Embedded NUL bytes end the scan early.
                if p != path.len() {
                    return (PathCheck::Err("illegal character"), path);
                }
                return (PathCheck::Ok, &path[start..]);
            }
            _ => {}
        }
        match utf8_len(path, p) {
            Some(n) => p += n,
            None => return (PathCheck::Err("illegal character"), path),
        }
    }
}

fn is_dir_up(e: &[u8]) -> bool {
    e.len() >= 2 && e.iter().all(|&c| c == b'.')
}

/// `phar_fix_filepath()`: removes `.` and `..` segments, producing a path
/// starting with `/` (or relative to `cwd` for `./x` when `cwd` is given).
pub fn fix_filepath(path: &[u8], cwd: Option<&[u8]>) -> Vec<u8> {
    let plen = path.len();
    let mut np: Vec<u8> = match cwd {
        Some(c) if !c.is_empty() && plen > 2 && path[0] == b'.' && path[1] == b'/' => c.to_vec(),
        _ => vec![b'/'],
    };
    // `memchr` loop that skips empty segments: returns (tok, ptr).
    let next = |mut ptr: usize, mut tok: usize| -> (usize, Option<usize>) {
        loop {
            let found = path[ptr..].iter().position(|&c| c == b'/').map(|p| p + ptr);
            match found {
                Some(f) if f == tok => {
                    ptr = f + 1;
                    tok += 1;
                    if ptr > plen {
                        return (tok, None);
                    }
                }
                other => return (tok, other),
            }
        }
    };
    let first = usize::from(path.first() == Some(&b'/'));
    let (mut tok, mut ptr) = next(first, first);
    if ptr.is_none() && plen > tok {
        return match &path[tok..] {
            b"." | b".." => b"/".to_vec(),
            _ => path.to_vec(),
        };
    }
    while let Some(mut end) = ptr {
        loop {
            let seg = &path[tok..end];
            if is_dir_up(seg) {
                while np.len() > 1 && np[np.len() - 1] != b'/' {
                    np.pop();
                }
                if np[0] == b'/' && np.len() > 1 {
                    np.pop();
                }
            } else if seg != b"." {
                if np.len() > 1 {
                    np.push(b'/');
                }
                np.extend_from_slice(seg);
            }
            if end >= plen {
                return np;
            }
            let (t, p) = next(end + 1, end + 1);
            tok = t;
            match p {
                Some(p) => {
                    end = p;
                    break;
                }
                None if plen > tok => {
                    end = plen;
                }
                None => return np,
            }
        }
        ptr = Some(end);
    }
    np
}

/// Case-insensitive search (`php_memnistr` / `php_stristr`); returns the
/// offset of the first match.
pub fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Case-sensitive search (`strstr`/`memmem`).
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// `phar_hex_str()`: upper-case hex.
pub fn hex_upper(data: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(data.len() * 2);
    for &b in data {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0F)]);
    }
    out
}

/// The part of a C string before the first NUL byte.
pub(crate) fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(p) => &b[..p],
        None => b,
    }
}

/// Lossy display of a byte string for messages.
pub(crate) fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The extension of an archive path (`mydata->ext`): from the first `.`
/// after the last `/`. (The C code's "skip a leading dot" check compares
/// against the slash itself and therefore never fires.)
pub fn archive_ext(fname: &[u8]) -> Option<&[u8]> {
    let slash = fname.iter().rposition(|&c| c == b'/')?;
    let tail = &fname[slash..];
    let dot = tail.iter().position(|&c| c == b'.')?;
    Some(&tail[dot..])
}

/// `phar_add_virtual_dirs()`: every proper parent directory of `filename`.
pub fn parent_dirs(filename: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut len = filename.len();
    while let Some(p) = filename[..len].iter().rposition(|&c| c == b'/') {
        if p == 0 {
            break;
        }
        len = p;
        out.push(&filename[..len]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_checks() {
        assert_eq!(path_check(b"a/b.php"), (PathCheck::Ok, &b"a/b.php"[..]));
        assert_eq!(path_check(b"/a/b.php"), (PathCheck::Ok, &b"a/b.php"[..]));
        assert_eq!(path_check(b"a//b").0, PathCheck::Err("double slash"));
        assert_eq!(
            path_check(b"a/../b").0,
            PathCheck::Err("upper directory reference")
        );
        assert_eq!(
            path_check(b"a/./b").0,
            PathCheck::Err("current directory reference")
        );
        assert_eq!(path_check(b"a/.b").0, PathCheck::Ok);
        assert_eq!(path_check(b"a\\b").0, PathCheck::Err("back-slash"));
        assert_eq!(path_check(b"a*b").0, PathCheck::Err("star"));
        assert_eq!(path_check(b"/a?x=1"), (PathCheck::UseQuery, &b"a"[..]));
        assert_eq!(path_check(b"a\x01").0, PathCheck::Err("illegal character"));
        assert_eq!(path_check(b"a\0b").0, PathCheck::Err("illegal character"));
        assert_eq!(path_check("é".as_bytes()).0, PathCheck::Ok);
        assert_eq!(path_check(b"\xff").0, PathCheck::Err("illegal character"));
        assert_eq!(
            path_check(b"..").0,
            PathCheck::Err("upper directory reference")
        );
    }

    #[test]
    fn fix_paths() {
        assert_eq!(fix_filepath(b"/a/b/../c", None), b"/a/c");
        assert_eq!(fix_filepath(b"/a/./b", None), b"/a/b");
        assert_eq!(fix_filepath(b"/..", None), b"/");
        assert_eq!(fix_filepath(b"/a", None), b"/a");
        assert_eq!(fix_filepath(b"/", None), b"/");
        assert_eq!(fix_filepath(b"/a/b/", None), b"/a/b");
        assert_eq!(fix_filepath(b"/../../x", None), b"/x");
        assert_eq!(fix_filepath(b"./x/y", Some(b"dir")), b"dir/x/y");
    }

    #[test]
    fn misc() {
        assert!(validate_alias(b"hi.phar"));
        assert!(!validate_alias(b"a/b"));
        assert!(is_magic_phar(b".phar"));
        assert!(is_magic_phar(b"/.phar/stub.php"));
        assert!(!is_magic_phar(b".phart"));
        assert_eq!(archive_ext(b"/x/y/a.phar.tar"), Some(&b".phar.tar"[..]));
        assert_eq!(archive_ext(b"/x/.a.phar"), Some(&b".a.phar"[..]));
        assert_eq!(archive_ext(b"/x/a"), None);
        assert_eq!(parent_dirs(b"a/b/c"), vec![&b"a/b"[..], &b"a"[..]]);
        assert_eq!(hex_upper(&[0xab, 0x01]), b"AB01");
        assert_eq!(
            find_ci(b"xx__halt_compiler();", b"__HALT_COMPILER();"),
            Some(2)
        );
    }
}
