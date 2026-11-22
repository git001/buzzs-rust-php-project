// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of the conversion primitives of `ext/iconv/iconv.c`:
//! `php_iconv_string`, `_php_iconv_appendl`, `_php_iconv_strlen`,
//! `_php_iconv_substr` and `_php_iconv_strpos`.
//!
//! The algorithms intentionally mirror the C code step by step (buffer
//! sizes, the "sticky" `errno` that is inspected after loops, …) because
//! PHP's observable behaviour depends on these details.

use crate::error::IconvErr;
use crate::ffi::Cd;
use php_memlimit::TryGrow;

/// `GENERIC_SUPERSET_NAME` (`ICONV_UCS4_ENCODING` with glibc).
pub const UCS4: &[u8] = b"UCS-4LE";
/// `ICONV_ASCII_ENCODING`.
pub const ASCII: &[u8] = b"ASCII";
const NB: usize = 4;

/// Optional allocation ceiling standing in for PHP's `memory_limit`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limit(pub Option<usize>);

impl Limit {
    pub(crate) fn check(self, size: usize) -> Result<(), IconvErr> {
        match self.0 {
            Some(max) if size > max => Err(IconvErr::Alloc(size)),
            _ => Ok(()),
        }
    }
}

/// `smart_str_falloc(.., fallible)`: makes room for `extra` more bytes
/// in `buf` under the process-wide `memory_limit` accounting. When that
/// would cross the limit, the violation is recorded (the VM raises the
/// delayed fatal right after the builtin returns, `ZEND_HAS_OOM()`) and
/// [`IconvErr::Alloc`] is returned without allocating.
pub(crate) fn reserve(buf: &mut Vec<u8>, extra: usize) -> Result<(), IconvErr> {
    buf.try_grow(extra).map_err(|_| IconvErr::Alloc(extra))
}

/// [`reserve`] for an exactly sized buffer (`safe_femalloc`/`erealloc`).
pub(crate) fn reserve_exact(buf: &mut Vec<u8>, extra: usize) -> Result<(), IconvErr> {
    buf.try_grow_exact(extra)
        .map_err(|_| IconvErr::Alloc(extra))
}

/// `ZEND_HAS_OOM()`: a `memory_limit` violation is pending; loops over
/// script-sized input stop early (the VM raises the fatal after the call).
pub(crate) fn oom_pending() -> bool {
    php_memlimit::is_pending()
}

/// A `smart_str`: tracks whether a buffer was ever allocated, since PHP
/// distinguishes "nothing appended" (`s == NULL`) from an empty string.
#[derive(Debug, Default, Clone)]
pub struct SmartStr {
    pub buf: Vec<u8>,
    pub allocated: bool,
}

impl SmartStr {
    pub fn append(&mut self, s: &[u8]) {
        self.allocated = true;
        self.buf.extend_from_slice(s);
    }

    pub fn push(&mut self, c: u8) {
        self.allocated = true;
        self.buf.push(c);
    }

    /// `smart_str_extract`.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Opens a converter, mapping failures like PHP.
pub(crate) fn open(to: &[u8], from: &[u8]) -> Result<Cd, IconvErr> {
    Cd::open(to, from).map_err(IconvErr::from_open_errno)
}

/// `_php_check_ignore()` (glibc has `ICONV_BROKEN_IGNORE`).
fn check_ignore(charset: &[u8]) -> bool {
    let cs = crate::error::c_prefix(charset);
    (cs.len() >= 9 && cs.ends_with(b"//IGNORE"))
        || (cs.len() >= 19 && cs.ends_with(b"//IGNORE//TRANSLIT"))
}

/// Result of [`iconv_string`]: the error (if any) and the output buffer
/// PHP would hand back (`*out`), which is partial on conversion errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringResult {
    pub err: Option<IconvErr>,
    pub out: Option<Vec<u8>>,
}

/// `php_iconv_string()`.
pub fn iconv_string(
    input: &[u8],
    out_charset: &[u8],
    in_charset: &[u8],
    limit: Limit,
) -> StringResult {
    let ignore_ilseq = check_ignore(out_charset);
    let mut cd = match open(out_charset, in_charset) {
        Ok(cd) => cd,
        Err(e) => {
            return StringResult {
                err: Some(e),
                out: None,
            }
        }
    };
    let in_len = input.len();
    let mut inp = input;
    let mut bsz = in_len + 32;
    if let Err(e) = limit.check(bsz) {
        return StringResult {
            err: Some(e),
            out: None,
        };
    }
    let mut buf: Vec<u8> = Vec::new();
    if let Err(e) = reserve_exact(&mut buf, bsz) {
        return StringResult {
            err: Some(e),
            out: None,
        };
    }
    buf.resize(bsz, 0);
    let mut pos = 0usize;
    let mut result: Result<usize, i32> = Ok(0);
    let mut errno = 0;

    while !inp.is_empty() {
        result = cd.conv(&mut inp, &mut buf[..bsz], &mut pos);
        if let Err(e) = result {
            errno = e;
            if ignore_ilseq && errno == libc::EILSEQ {
                if inp.len() <= 1 {
                    result = Ok(0);
                } else {
                    errno = 0;
                    inp = &inp[1..];
                    continue;
                }
            }
            if errno == libc::E2BIG && !inp.is_empty() {
                bsz += in_len;
                let extra = bsz - buf.len();
                if let Err(e) = limit
                    .check(bsz)
                    .and_then(|()| reserve_exact(&mut buf, extra))
                {
                    return StringResult {
                        err: Some(e),
                        out: None,
                    };
                }
                buf.resize(bsz, 0);
                continue;
            }
        }
        break;
    }

    if result.is_ok() {
        loop {
            result = cd.flush(&mut buf[..bsz], &mut pos);
            match result {
                Ok(_) => break,
                Err(e) => {
                    errno = e;
                    if e == libc::E2BIG {
                        bsz += 16;
                        buf.resize(bsz, 0);
                    } else {
                        break;
                    }
                }
            }
        }
    }

    let err = if result.is_err() {
        match errno {
            libc::EINVAL => Some(IconvErr::IllegalChar),
            libc::EILSEQ => Some(IconvErr::IllegalSeq),
            libc::E2BIG => Some(IconvErr::TooBig),
            e => {
                return StringResult {
                    err: Some(IconvErr::Unknown(e)),
                    out: None,
                }
            }
        }
    } else {
        None
    };
    buf.truncate(pos);
    StringResult {
        err,
        out: Some(buf),
    }
}

/// `_php_iconv_appendl()`; `None` input flushes the shift state.
/// `errno` receives the errno of every failing `iconv()` call.
pub(crate) fn appendl(
    d: &mut SmartStr,
    s: Option<&[u8]>,
    cd: &mut Cd,
    errno: &mut i32,
    limit: Limit,
) -> Result<(), IconvErr> {
    let mut growth = 128usize;
    if let Some(s) = s {
        let mut inp = s;
        while !inp.is_empty() {
            let start = d.buf.len();
            limit.check(start + growth)?;
            reserve(&mut d.buf, growth)?;
            d.allocated = true;
            d.buf.resize(start + growth, 0);
            let mut pos = start;
            if let Err(e) = cd.conv(&mut inp, &mut d.buf, &mut pos) {
                *errno = e;
                match e {
                    libc::EINVAL => {
                        d.buf.truncate(start);
                        return Err(IconvErr::IllegalChar);
                    }
                    libc::EILSEQ => {
                        d.buf.truncate(start);
                        return Err(IconvErr::IllegalSeq);
                    }
                    libc::E2BIG => {}
                    other => {
                        d.buf.truncate(start);
                        return Err(IconvErr::Unknown(other));
                    }
                }
            }
            d.buf.truncate(pos);
            growth <<= 1;
        }
    } else {
        loop {
            let start = d.buf.len();
            limit.check(start + growth)?;
            reserve(&mut d.buf, growth)?;
            d.allocated = true;
            d.buf.resize(start + growth, 0);
            let mut pos = start;
            match cd.flush(&mut d.buf, &mut pos) {
                Ok(_) => {
                    d.buf.truncate(pos);
                    break;
                }
                Err(e) => {
                    *errno = e;
                    if e != libc::E2BIG {
                        d.buf.truncate(start);
                        return Err(IconvErr::Unknown(e));
                    }
                }
            }
            d.buf.truncate(pos);
            growth <<= 1;
        }
    }
    Ok(())
}

/// `_php_iconv_appendc()`.
pub(crate) fn appendc(
    d: &mut SmartStr,
    c: u8,
    cd: &mut Cd,
    errno: &mut i32,
    limit: Limit,
) -> Result<(), IconvErr> {
    appendl(d, Some(&[c]), cd, errno, limit)
}

/// `_php_iconv_strlen()`.
pub fn strlen(s: &[u8], enc: &[u8]) -> Result<usize, IconvErr> {
    let mut cd = open(UCS4, enc)?;
    let mut buf = [0u8; NB * 2];
    let mut errno = 0;
    let mut more = !s.is_empty();
    let mut inp = s;
    let mut cnt = 0usize;
    while more {
        let mut pos = 0;
        more = !inp.is_empty();
        let r = if more {
            cd.conv(&mut inp, &mut buf, &mut pos)
        } else {
            cd.flush(&mut buf, &mut pos)
        };
        if let Err(e) = r {
            errno = e;
        }
        if pos == 0 {
            break;
        }
        cnt += pos / NB;
    }
    match errno {
        libc::EINVAL => Err(IconvErr::IllegalChar),
        libc::EILSEQ => Err(IconvErr::IllegalSeq),
        0 | libc::E2BIG => Ok(cnt),
        e => Err(IconvErr::Unknown(e)),
    }
}

/// `_php_iconv_substr()`.  On success returns the `smart_str` (whose
/// `allocated` flag decides between a string and `false`).
pub fn substr(
    s: &[u8],
    offset: i64,
    len: i64,
    enc: &[u8],
    limit: Limit,
) -> Result<SmartStr, IconvErr> {
    let total_len = strlen(s, enc)?;
    #[allow(clippy::cast_possible_wrap)]
    let total = total_len as i64;
    let mut offset = offset;
    let mut len = len;
    if offset < 0 {
        offset = offset.wrapping_add(total);
        if offset < 0 {
            offset = 0;
        }
    } else if offset > total {
        offset = total;
    }
    if len < 0 {
        len = len.wrapping_add(total - offset);
        if len < 0 {
            len = 0;
        }
    } else if len > total {
        len = total;
    }
    if offset + len > total {
        len = total - offset;
    }

    let mut ret = SmartStr::default();
    if len == 0 {
        ret.allocated = true;
        return Ok(ret);
    }

    let mut cd1 = open(UCS4, enc)?;
    let mut cd2: Option<Cd> = None;
    let mut err: Option<IconvErr> = None;
    let mut errno = 0;
    let mut more = !s.is_empty() && len > 0;
    let mut inp = s;
    let mut cnt: i64 = 0;
    let mut buf = [0u8; NB];
    while more {
        let mut pos = 0;
        more = !inp.is_empty() && len > 0;
        let r = if more {
            cd1.conv(&mut inp, &mut buf, &mut pos)
        } else {
            cd1.flush(&mut buf, &mut pos)
        };
        if let Err(e) = r {
            errno = e;
        }
        if pos == 0 {
            break;
        }
        if cnt >= offset {
            if cd2.is_none() {
                match Cd::open(enc, UCS4) {
                    Ok(cd) => cd2 = Some(cd),
                    Err(e) => {
                        errno = e;
                        err = Some(IconvErr::from_open_errno(e));
                        break;
                    }
                }
            }
            if let Some(cd) = cd2.as_mut() {
                if appendl(&mut ret, Some(&buf), cd, &mut errno, limit).is_err() {
                    break;
                }
            }
            len -= 1;
        }
        cnt += 1;
    }

    match errno {
        libc::EINVAL => err = Some(IconvErr::IllegalChar),
        libc::EILSEQ => err = Some(IconvErr::IllegalSeq),
        _ => {}
    }
    if let Some(e) = err {
        return Err(e);
    }
    if let Some(cd) = cd2.as_mut() {
        let _ = appendl(&mut ret, None, cd, &mut errno, limit);
    }
    Ok(ret)
}

/// `_php_iconv_strpos()`.  `Ok(None)` means "not found".
pub fn strpos(
    haystack: &[u8],
    needle: &[u8],
    offset: usize,
    enc: &[u8],
    reverse: bool,
    limit: Limit,
) -> Result<Option<usize>, IconvErr> {
    let conv = iconv_string(needle, UCS4, enc, limit);
    if let Some(e) = conv.err {
        return Err(e);
    }
    let ndl = conv.out.unwrap_or_default();
    let mut cd = open(UCS4, enc)?;

    let mut ndl_pos = 0usize;
    let mut ndl_left = ndl.len();
    let mut match_ofs: Option<usize> = None;
    let mut retval: Option<usize> = None;
    let mut err: Option<IconvErr> = None;
    let mut more = !haystack.is_empty();
    let mut inp = haystack;
    let mut cnt = 0usize;
    let mut buf = [0u8; NB];
    let ndl_at = |i: usize| ndl.get(i..i + NB);

    while more {
        let mut pos = 0;
        more = !inp.is_empty();
        let r = if more {
            cd.conv(&mut inp, &mut buf, &mut pos)
        } else {
            cd.flush(&mut buf, &mut pos)
        };
        if pos == 0 {
            break;
        }
        if let Err(e) = r {
            match e {
                libc::EINVAL => err = Some(IconvErr::IllegalChar),
                libc::EILSEQ => err = Some(IconvErr::IllegalSeq),
                libc::E2BIG => {}
                other => err = Some(IconvErr::Unknown(other)),
            }
        }
        if cnt >= offset {
            if ndl_at(ndl_pos) == Some(&buf[..]) {
                if match_ofs.is_none() {
                    match_ofs = Some(cnt);
                }
                ndl_pos += NB;
                ndl_left -= NB;
                if ndl_left == 0 {
                    retval = match_ofs;
                    if reverse {
                        ndl_pos = 0;
                        ndl_left = ndl.len();
                        match_ofs = None;
                    } else {
                        break;
                    }
                }
            } else {
                let mut i = 0usize;
                let mut j = NB;
                let lim = ndl_pos;
                while j < lim {
                    if ndl_at(j) == ndl_at(i) {
                        i += NB;
                    } else {
                        j -= i;
                        i = 0;
                    }
                    j += NB;
                }
                if ndl_at(i) == Some(&buf[..]) {
                    match_ofs = match_ofs.map(|m| m + (lim - i) / NB);
                    i += NB;
                    ndl_pos = i;
                    ndl_left = ndl.len() - i;
                } else {
                    match_ofs = None;
                    ndl_pos = 0;
                    ndl_left = ndl.len();
                }
            }
        }
        cnt += 1;
    }

    if let Some(e) = err {
        return Err(e);
    }
    if offset > cnt {
        return Err(IconvErr::OutOfBounds);
    }
    Ok(retval)
}
