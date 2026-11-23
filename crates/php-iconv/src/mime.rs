// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `_php_iconv_mime_encode()` / `_php_iconv_mime_decode()` and the
//! base64 / quoted-printable helpers they rely on.

use crate::core::{appendc, appendl, oom_pending, open, SmartStr, ASCII};
use crate::error::{c_prefix, IconvErr};
use crate::ffi::Cd;

/// `ICONV_MIME_DECODE_STRICT`.
pub const MIME_DECODE_STRICT: i64 = 1;
/// `ICONV_MIME_DECODE_CONTINUE_ON_ERROR`.
pub const MIME_DECODE_CONTINUE_ON_ERROR: i64 = 2;

/// Encoding scheme for `iconv_mime_encode()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    /// `B` (base64), the default.
    #[default]
    Base64,
    /// `Q` (quoted-printable).
    QPrint,
}

const QP_TABLE: [u8; 256] = {
    let mut t = [3u8; 256];
    let mut c = 0x21;
    while c < 0x7f {
        t[c] = 1;
        c += 1;
    }
    t[b'=' as usize] = 3;
    t[b'?' as usize] = 3;
    t[b'_' as usize] = 3;
    t
};

const HEX: &[u8; 16] = b"0123456789ABCDEF";

const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `php_base64_encode()`.
pub fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64_CHARS[usize::from(b0 >> 2)]);
        out.push(B64_CHARS[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))]);
        out.push(if chunk.len() > 1 {
            B64_CHARS[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            B64_CHARS[usize::from(b2 & 0x3f)]
        } else {
            b'='
        });
    }
    out
}

/// `php_base64_decode()` (non-strict: unknown characters and `=` are
/// skipped; a trailing partial byte is dropped).
pub fn base64_decode(data: &[u8]) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let mut i = 0usize;
    let mut cur = 0u8;
    for &c in data {
        let Some(ch) = val(c) else { continue };
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
    out
}

fn hexval(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        b'A'..=b'F' => u32::from(c - b'A' + 10),
        b'a'..=b'f' => u32::from(c - b'a' + 10),
        b'\t' | b' ' => 32,
        b'\n' | b'\r' => 16,
        _ => 64,
    }
}

/// `php_quot_print_decode(str, len, replace_us_by_ws)`; `None` on
/// malformed input.
pub fn quot_print_decode(input: &[u8], replace_us_by_ws: bool) -> Option<Vec<u8>> {
    let at = |k: usize| input.get(k).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(input.len());
    let mut left = input.len();
    let mut pos = 0usize;
    while left > 0 && at(pos) != 0 {
        if at(pos) == b'=' {
            left -= 1;
            pos += 1;
            if left == 0 || at(pos) == 0 {
                break;
            }
            let mut hi = hexval(at(pos));
            if hi < 16 {
                left -= 1;
                if left == 0 {
                    return None;
                }
                pos += 1;
                let lo = hexval(at(pos));
                if lo >= 16 {
                    return None;
                }
                #[allow(clippy::cast_possible_truncation)]
                out.push(((hi << 4) | lo) as u8);
                left -= 1;
                pos += 1;
            } else if hi < 64 {
                while hi == 32 {
                    left -= 1;
                    if left == 0 {
                        return None;
                    }
                    pos += 1;
                    hi = hexval(at(pos));
                    if hi == 64 {
                        return None;
                    }
                }
                if at(pos) == b'\r' && left >= 2 && at(pos + 1) == b'\n' {
                    left -= 1;
                    pos += 1;
                }
                left -= 1;
                pos += 1;
            } else {
                return None;
            }
        } else {
            let c = at(pos);
            out.push(if replace_us_by_ws && c == b'_' {
                b' '
            } else {
                c
            });
            left -= 1;
            pos += 1;
        }
    }
    Some(out)
}

/// Converts into `buf[pos..cap]`, growing the physical buffer lazily on
/// `E2BIG` while staying within the logical capacity `cap` (equivalent to
/// one call with a `cap`-sized buffer, without allocating it up front).
fn conv_virtual(
    cd: &mut Cd,
    input: Option<&mut &[u8]>,
    buf: &mut Vec<u8>,
    pos: &mut usize,
    cap: usize,
) -> Result<usize, i32> {
    let mut input = input;
    loop {
        let phys = buf.len().min(cap);
        let r = match input.as_deref_mut() {
            Some(inp) => cd.conv(inp, &mut buf[..phys], pos),
            None => cd.flush(&mut buf[..phys], pos),
        };
        match r {
            Err(libc::E2BIG) if phys < cap => {
                let grow = (buf.len() * 2).clamp(256, cap);
                buf.resize(grow, 0);
            }
            other => return other,
        }
    }
}

/// `_php_iconv_mime_encode()`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn mime_encode(
    fname: &[u8],
    fval: &[u8],
    max_line_len: usize,
    lfchars: &[u8],
    scheme: Scheme,
    out_charset: &[u8],
    enc: &[u8],
) -> Result<Vec<u8>, IconvErr> {
    let out_charset = c_prefix(out_charset);
    let lfchars = c_prefix(lfchars);
    let out_charset_len = out_charset.len();
    let lfchars_len = lfchars.len();

    if fname.len().wrapping_add(2) >= max_line_len || out_charset_len + 12 >= max_line_len {
        return Err(IconvErr::TooBig);
    }
    let mut cd_pl = open(ASCII, enc)?;
    let mut cd = open(out_charset, enc)?;

    // safe_femalloc(1, max_line_len, 5)
    let Some(buf_size) = max_line_len.checked_add(5) else {
        return Err(IconvErr::Alloc(usize::MAX));
    };
    if buf_size > isize::MAX as usize {
        return Err(IconvErr::Alloc(buf_size));
    }
    php_memlimit::try_reserve(buf_size).map_err(|_| IconvErr::Alloc(buf_size))?;
    let mut buf: Vec<u8> = vec![0; buf_size.min(4096 + fval.len() * 8)];

    let mut ret = SmartStr::default();
    let mut errno = 0;
    let mut char_cnt = max_line_len;

    let _ = appendl(&mut ret, Some(fname), &mut cd_pl, &mut errno);
    char_cnt = char_cnt.wrapping_sub(fname.len());
    ret.append(b": ");
    char_cnt = char_cnt.wrapping_sub(2);

    let mut inp = fval;
    loop {
        let encoded_word_min_len =
            7 + out_charset_len + if scheme == Scheme::Base64 { 4 } else { 3 };
        if char_cnt < encoded_word_min_len + lfchars_len + 1 {
            ret.append(lfchars);
            ret.push(b' ');
            char_cnt = max_line_len - 1;
        }
        ret.append(b"=?");
        char_cnt = char_cnt.wrapping_sub(2);
        ret.append(out_charset);
        char_cnt = char_cnt.wrapping_sub(out_charset_len);
        ret.push(b'?');
        char_cnt = char_cnt.wrapping_sub(1);

        match scheme {
            Scheme::Base64 => {
                let mut out_reserved = 4usize;
                ret.push(b'B');
                ret.push(b'?');
                char_cnt = char_cnt.wrapping_sub(2);
                let prev_in_left = inp.len();
                let ini_in = inp;
                let out_size = char_cnt.wrapping_sub(2) / 4 * 3;
                let mut pos;
                loop {
                    pos = 0;
                    if out_size <= out_reserved {
                        return Err(IconvErr::TooBig);
                    }
                    let out_left = out_size - out_reserved;
                    if let Err(e) =
                        conv_virtual(&mut cd, Some(&mut inp), &mut buf, &mut pos, out_left)
                    {
                        match e {
                            libc::EINVAL => return Err(IconvErr::IllegalChar),
                            libc::EILSEQ => return Err(IconvErr::IllegalSeq),
                            libc::E2BIG => {
                                if prev_in_left == inp.len() {
                                    return Err(IconvErr::TooBig);
                                }
                            }
                            other => return Err(IconvErr::Unknown(other)),
                        }
                    }
                    match conv_virtual(&mut cd, None, &mut buf, &mut pos, out_size) {
                        Ok(_) => break,
                        Err(libc::E2BIG) => {}
                        Err(e) => return Err(IconvErr::Unknown(e)),
                    }
                    if let Err(e) = cd.reset() {
                        return Err(IconvErr::Unknown(e));
                    }
                    out_reserved += 4;
                    inp = ini_in;
                }
                let encoded = base64_encode(&buf[..pos]);
                if char_cnt < encoded.len() {
                    return Err(IconvErr::Unknown(crate::ffi::last_errno()));
                }
                ret.append(&encoded);
                char_cnt -= encoded.len();
                ret.append(b"?=");
                char_cnt = char_cnt.wrapping_sub(2);
            }
            Scheme::QPrint => {
                ret.push(b'Q');
                ret.push(b'?');
                char_cnt = char_cnt.wrapping_sub(2);
                let prev_in_left = inp.len();
                let ini_in = inp;
                let mut out_size = char_cnt.wrapping_sub(2);
                let mut pos = 0;
                while out_size > 0 {
                    pos = 0;
                    if let Err(e) =
                        conv_virtual(&mut cd, Some(&mut inp), &mut buf, &mut pos, out_size)
                    {
                        match e {
                            libc::EINVAL => return Err(IconvErr::IllegalChar),
                            libc::EILSEQ => return Err(IconvErr::IllegalSeq),
                            libc::E2BIG => {
                                if prev_in_left == inp.len() {
                                    return Err(IconvErr::Unknown(e));
                                }
                            }
                            other => return Err(IconvErr::Unknown(other)),
                        }
                    }
                    match conv_virtual(&mut cd, None, &mut buf, &mut pos, out_size) {
                        Ok(_) | Err(libc::E2BIG) => {}
                        Err(e) => return Err(IconvErr::Unknown(e)),
                    }
                    let nbytes_required: usize = buf[..pos]
                        .iter()
                        .map(|&b| usize::from(QP_TABLE[usize::from(b)]))
                        .sum();
                    if nbytes_required <= char_cnt.wrapping_sub(2) {
                        break;
                    }
                    out_size = out_size
                        .saturating_sub((nbytes_required - char_cnt.wrapping_sub(2)).div_ceil(3));
                    inp = ini_in;
                }
                for &b in &buf[..pos] {
                    if QP_TABLE[usize::from(b)] == 1 {
                        ret.push(b);
                        char_cnt = char_cnt.wrapping_sub(1);
                    } else {
                        ret.push(b'=');
                        ret.push(HEX[usize::from(b >> 4)]);
                        ret.push(HEX[usize::from(b & 0x0f)]);
                        char_cnt = char_cnt.wrapping_sub(3);
                    }
                }
                ret.append(b"?=");
                char_cnt = char_cnt.wrapping_sub(2);
                if let Err(e) = cd.reset() {
                    return Err(IconvErr::Unknown(e));
                }
            }
        }
        // `while (in_left > 0 && !ZEND_HAS_OOM())`
        if inp.is_empty() || oom_pending() {
            break;
        }
    }
    Ok(ret.into_vec())
}

/// Result of [`mime_decode`]: the decoded text and `next_pos` (offset in
/// the input where scanning stopped).
#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: SmartStr,
    pub next_pos: usize,
}

/// `_php_iconv_mime_decode()`.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn mime_decode(s: &[u8], enc: &[u8], mode: i64) -> Result<Decoded, IconvErr> {
    #[allow(clippy::cast_possible_truncation)]
    let mode = mode as i32;
    let strict = mode & 1 != 0;
    let cont = mode & 2 != 0;
    let at = |k: usize| s.get(k).copied().unwrap_or(0);

    let mut cd_pl = open(enc, ASCII)?;
    let mut cd: Option<Cd> = None;
    let mut ret = SmartStr::default();
    let mut errno = 0;
    let mut err: Result<(), IconvErr> = Ok(());

    let mut scan_stat = 0u32;
    let mut csname = 0usize;
    let mut encoded_text = 0usize;
    let mut encoded_text_len = 0usize;
    let mut encoded_word: Option<usize> = None;
    let mut spaces: Option<usize> = None;
    let mut enc_scheme = Scheme::Base64;

    // `p1` is the C pointer as an index; it may step back by one.
    let mut p1 = 0usize;
    let mut str_left = s.len();

    macro_rules! app {
        ($slice:expr) => {
            appendl(&mut ret, Some($slice), &mut cd_pl, &mut errno)
        };
    }
    macro_rules! app_ew {
        ($end:expr) => {
            match encoded_word {
                Some(ew) => appendl(&mut ret, Some(&s[ew..$end]), &mut cd_pl, &mut errno),
                // C passes a NULL source pointer, which flushes the state.
                None => appendl(&mut ret, None, &mut cd_pl, &mut errno),
            }
        };
    }
    macro_rules! appc {
        ($c:expr) => {
            appendc(&mut ret, $c, &mut cd_pl, &mut errno)
        };
    }

    while str_left > 0 {
        let mut eos = false;
        let c = at(p1);
        // `continue_loop` mirrors a C `continue` (skipping nothing else).
        match scan_stat {
            0 => match c {
                b'\r' => scan_stat = 7,
                b'\n' => scan_stat = 8,
                b'=' => {
                    encoded_word = Some(p1);
                    scan_stat = 1;
                }
                b' ' | b'\t' => {
                    spaces = Some(p1);
                    scan_stat = 11;
                }
                _ => {
                    err = appc!(c);
                    if err.is_err() {
                        if cont {
                            err = Ok(());
                        } else {
                            return Err(err.unwrap_err());
                        }
                    }
                    encoded_word = None;
                    if strict {
                        scan_stat = 12;
                    }
                }
            },
            1 => {
                if c == b'?' {
                    csname = p1 + 1;
                    scan_stat = 2;
                } else {
                    if c == b'\r' || c == b'\n' {
                        p1 -= 1;
                    }
                    err = app_ew!(p1 + 1);
                    err?;
                    encoded_word = None;
                    scan_stat = if strict { 12 } else { 0 };
                }
            }
            2 => {
                match c {
                    b'?' => scan_stat = 3,
                    b'*' => scan_stat = 10,
                    b'\r' | b'\n' => {
                        p1 -= 1;
                        let _ = appc!(b'=');
                        let _ = appc!(b'?');
                        err = app!(&s[csname..=p1]);
                        err?;
                        scan_stat = if strict { 12 } else { 0 };
                        str_left -= 1;
                        p1 += 1;
                        continue;
                    }
                    _ => {}
                }
                if scan_stat != 2 {
                    let csname_len = p1 - csname;
                    if csname_len > 79 {
                        if cont {
                            err = app_ew!(p1 + 1);
                            err?;
                            encoded_word = None;
                            scan_stat = if strict { 12 } else { 0 };
                        } else {
                            return Err(IconvErr::Malformed);
                        }
                    } else {
                        cd = None;
                        match Cd::open(enc, &s[csname..p1]) {
                            Ok(new) => cd = Some(new),
                            Err(e) => {
                                if cont {
                                    let mut qmarks = 2;
                                    while qmarks > 0 && str_left > 1 {
                                        p1 += 1;
                                        if at(p1) == b'?' {
                                            qmarks -= 1;
                                        }
                                        str_left -= 1;
                                    }
                                    if at(p1 + 1) == b'=' {
                                        p1 += 1;
                                        if str_left > 1 {
                                            str_left -= 1;
                                        }
                                    }
                                    let end = (p1 + 1).min(s.len());
                                    err = app_ew!(end);
                                    err?;
                                    scan_stat = 12;
                                } else {
                                    return Err(IconvErr::from_open_errno(e));
                                }
                            }
                        }
                    }
                }
            }
            3 => match c {
                b'b' | b'B' => {
                    enc_scheme = Scheme::Base64;
                    scan_stat = 4;
                }
                b'q' | b'Q' => {
                    enc_scheme = Scheme::QPrint;
                    scan_stat = 4;
                }
                _ => {
                    if cont {
                        err = app_ew!(p1 + 1);
                        err?;
                        encoded_word = None;
                        scan_stat = if strict { 12 } else { 0 };
                    } else {
                        return Err(IconvErr::Malformed);
                    }
                }
            },
            4 => {
                if c == b'?' {
                    encoded_text = p1 + 1;
                    scan_stat = 5;
                } else if cont {
                    err = app_ew!(p1 + 1);
                    err?;
                    encoded_word = None;
                    scan_stat = if strict { 12 } else { 0 };
                } else {
                    return Err(IconvErr::Malformed);
                }
            }
            5 if c == b'?' => {
                encoded_text_len = p1 - encoded_text;
                scan_stat = 6;
            }
            7 => {
                if c == b'\n' {
                    scan_stat = 8;
                } else {
                    let _ = appc!(b'\r');
                    let _ = appc!(c);
                    scan_stat = 0;
                }
            }
            8 => {
                if c != b' ' && c != b'\t' {
                    p1 -= 1;
                    str_left = 1;
                } else {
                    if encoded_word.is_none() {
                        let _ = appc!(b' ');
                    }
                    spaces = None;
                    scan_stat = 11;
                }
            }
            6 | 9 => {
                let mut go_choice = scan_stat == 9;
                if scan_stat == 6 {
                    if c == b'=' {
                        scan_stat = 9;
                        if str_left == 1 {
                            eos = true;
                            go_choice = true;
                        }
                    } else if cont {
                        err = app_ew!(p1 + 1);
                        err?;
                        encoded_word = None;
                        scan_stat = if strict { 12 } else { 0 };
                    } else {
                        return Err(IconvErr::Malformed);
                    }
                }
                if go_choice {
                    let is_ws = matches!(c, b'\r' | b'\n' | b' ' | b'\t');
                    if !is_ws && !eos && strict {
                        err = app_ew!(p1 + 1);
                        err?;
                        scan_stat = 12;
                    } else {
                        'choice: {
                            let text = &s[encoded_text..encoded_text + encoded_text_len];
                            let decoded = match enc_scheme {
                                Scheme::Base64 => Some(base64_decode(text)),
                                Scheme::QPrint => quot_print_decode(text, true),
                            };
                            let Some(decoded) = decoded else {
                                if cont {
                                    err = app_ew!(p1 + 1);
                                    err?;
                                    encoded_word = None;
                                    scan_stat = if strict { 12 } else { 0 };
                                    break 'choice;
                                }
                                return Err(IconvErr::Unknown(crate::ffi::last_errno()));
                            };
                            err = match cd.as_mut() {
                                Some(cd) => appendl(&mut ret, Some(&decoded), cd, &mut errno)
                                    .and_then(|()| appendl(&mut ret, None, cd, &mut errno)),
                                None => Err(IconvErr::Unknown(crate::ffi::last_errno())),
                            };
                            if err.is_err() {
                                if cont {
                                    err = app_ew!(p1);
                                    encoded_word = None;
                                    if err.is_err() {
                                        break 'choice;
                                    }
                                } else {
                                    return Err(err.unwrap_err());
                                }
                            }
                            if eos {
                                scan_stat = 0;
                                break 'choice;
                            }
                            match c {
                                b'\r' => scan_stat = 7,
                                b'\n' => scan_stat = 8,
                                b'=' => scan_stat = 1,
                                b' ' | b'\t' => {
                                    spaces = Some(p1);
                                    scan_stat = 11;
                                }
                                _ => {
                                    let _ = appc!(c);
                                    scan_stat = 12;
                                }
                            }
                        }
                    }
                }
            }
            10 if c == b'?' => scan_stat = 3,
            11 => match c {
                b'\r' => scan_stat = 7,
                b'\n' => scan_stat = 8,
                b'=' => {
                    if let (Some(sp), None) = (spaces, encoded_word) {
                        let _ = app!(&s[sp..p1]);
                        spaces = None;
                    }
                    encoded_word = Some(p1);
                    scan_stat = 1;
                }
                b' ' | b'\t' => {}
                _ => {
                    if let Some(sp) = spaces {
                        let _ = app!(&s[sp..p1]);
                        spaces = None;
                    }
                    let _ = appc!(c);
                    encoded_word = None;
                    scan_stat = if strict { 12 } else { 0 };
                }
            },
            12 => match c {
                b'\r' => scan_stat = 7,
                b'\n' => scan_stat = 8,
                b' ' | b'\t' => {
                    spaces = Some(p1);
                    scan_stat = 11;
                }
                b'=' if !strict => {
                    encoded_word = Some(p1);
                    scan_stat = 1;
                }
                _ => {
                    let _ = appc!(c);
                }
            },
            _ => {}
        }
        str_left -= 1;
        p1 += 1;
    }

    match scan_stat {
        0 | 8 | 11 | 12 => {}
        _ => {
            if cont {
                if scan_stat == 1 {
                    let _ = appc!(b'=');
                }
                err = Ok(());
            } else {
                return Err(IconvErr::Malformed);
            }
        }
    }
    err?;
    Ok(Decoded {
        text: ret,
        next_pos: p1,
    })
}
