// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Memory, temp and `data:` (RFC 2397) streams (`main/streams/memory.c`).

use crate::context::ContextRef;
use crate::mode::TempMode;
use crate::stat::{mode as smode, StatBuf};
use crate::stream::{
    flags, CastAs, OpFailed, Stream, StreamOps, StreamOption, StreamState, WrapperInfo, OPT_ERR,
    OPT_NOTIMPL, OPT_OK, SEEK_CUR, SEEK_END, SEEK_SET,
};
use crate::value::{Key, Value};
use crate::wrapper::{OpenError, Wrapper};

/// `PHP_STREAM_MAX_MEM`: default `php://temp` memory limit (2 MiB).
pub const MAX_MEM: usize = 2 * 1024 * 1024;

/// `php_stream_memory_ops`.
#[derive(Debug)]
pub struct MemoryOps {
    data: Vec<u8>,
    fpos: usize,
    mode: TempMode,
}

impl MemoryOps {
    /// The buffer (`php_stream_memory_get_buffer`).
    pub fn buffer(&self) -> &[u8] {
        &self.data
    }
}

/// `php_stream_memory_create()`.
pub fn memory_create(mode: TempMode) -> Stream {
    let mut s = Stream::new(
        Box::new(MemoryOps {
            data: Vec::new(),
            fpos: 0,
            mode,
        }),
        mode.as_mode(),
    );
    s.add_flags(flags::NO_BUFFER);
    s
}

/// Where `php_stream_memory_seek()` goes.
pub(crate) enum SeekTarget {
    /// The new position (`size_t` arithmetic, wrapping; reported as
    /// `zend_off_t`).
    At(u64),
    /// Before the start: fails, and the position goes to 0.
    BeforeStart,
    /// An unknown `whence`: fails, the position stays.
    BadWhence,
}

/// The target of `php_stream_memory_seek()` over `len` bytes at `fpos`.
pub(crate) fn seek_target(len: usize, fpos: usize, offset: i64, whence: i32) -> SeekTarget {
    let len = len as u64;
    let fpos = fpos as u64;
    let mag = offset.unsigned_abs();
    let target = match whence {
        SEEK_CUR if offset < 0 => fpos.checked_sub(mag),
        SEEK_CUR => Some(fpos.wrapping_add(mag)),
        SEEK_SET => (offset >= 0).then_some(mag),
        SEEK_END if offset > 0 => Some(len.wrapping_add(mag)),
        SEEK_END => len.checked_sub(mag),
        _ => return SeekTarget::BadWhence,
    };
    target.map_or(SeekTarget::BeforeStart, SeekTarget::At)
}

/// `php_stream_memory_open()`: a memory stream over `data`.
pub fn memory_open(mode: TempMode, data: Vec<u8>) -> Stream {
    let mut s = memory_create(mode);
    if let Some(m) = s.ops_as_mut::<MemoryOps>() {
        m.data = data;
    }
    s
}

impl StreamOps for MemoryOps {
    fn label(&self) -> &'static str {
        "MEMORY"
    }

    fn read(&mut self, st: &mut StreamState, buf: &mut [u8]) -> Result<usize, OpFailed> {
        if self.fpos >= self.data.len() {
            st.eof = true;
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - self.fpos);
        buf[..n].copy_from_slice(&self.data[self.fpos..self.fpos + n]);
        self.fpos += n;
        Ok(n)
    }

    fn write(&mut self, _st: &mut StreamState, buf: &[u8]) -> Result<usize, OpFailed> {
        if self.mode == TempMode::ReadOnly {
            return Err(OpFailed);
        }
        if self.mode == TempMode::Append {
            self.fpos = self.data.len();
        }
        let Some(end) = self
            .fpos
            .checked_add(buf.len())
            .filter(|&e| isize::try_from(e).is_ok())
        else {
            return Err(OpFailed);
        };
        if end > self.data.len() {
            let grow = end - self.data.len();
            if !crate::util::reserve_more(&mut self.data, grow) {
                // memory_limit: php://memory cannot grow.
                return Err(OpFailed);
            }
            self.data.resize(end, 0);
        }
        self.data[self.fpos..end].copy_from_slice(buf);
        self.fpos = end;
        Ok(buf.len())
    }

    fn can_seek(&self) -> bool {
        true
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    fn seek(&mut self, st: &mut StreamState, offset: i64, whence: i32, newpos: &mut i64) -> bool {
        let target = match seek_target(self.data.len(), self.fpos, offset, whence) {
            SeekTarget::BadWhence => {
                *newpos = self.fpos as i64;
                return false;
            }
            SeekTarget::BeforeStart => None,
            SeekTarget::At(t) => Some(t),
        };
        if let Some(t) = target {
            self.fpos = usize::try_from(t).unwrap_or(usize::MAX);
            *newpos = t as i64;
            st.eof = false;
            st.fatal_error = false;
            true
        } else {
            self.fpos = 0;
            *newpos = -1;
            false
        }
    }

    fn stat(&mut self, _st: &mut StreamState) -> Option<StatBuf> {
        Some(StatBuf {
            mode: if self.mode == TempMode::ReadOnly {
                0o444
            } else {
                0o666
            } | smode::S_IFREG,
            size: i64::try_from(self.data.len()).unwrap_or(i64::MAX),
            nlink: 1,
            rdev: -1,
            dev: 0xC,
            ino: 0,
            blksize: -1,
            blocks: -1,
            ..StatBuf::default()
        })
    }

    fn set_option(&mut self, _st: &mut StreamState, opt: &mut StreamOption<'_>) -> i64 {
        match opt {
            StreamOption::TruncateSupported => OPT_OK,
            StreamOption::TruncateSetSize(size) => {
                if self.mode == TempMode::ReadOnly {
                    return OPT_ERR;
                }
                let Ok(size) = usize::try_from(*size) else {
                    return OPT_ERR;
                };
                if size <= self.data.len() {
                    self.data.truncate(size);
                    if size < self.fpos {
                        self.fpos = size;
                    }
                } else {
                    // `zend_string_frealloc(..., /* fallible */ true)`: a
                    // size beyond `memory_limit` fails the truncate and
                    // leaves the "Allowed memory size" violation recorded
                    // for the VM, instead of attempting the allocation.
                    let bytes = size.saturating_add(24 + 1).saturating_add(7) & !7;
                    if php_memlimit::try_reserve(bytes).is_err()
                        || self.data.try_reserve_exact(size - self.data.len()).is_err()
                    {
                        return OPT_ERR;
                    }
                    self.data.resize(size, 0);
                }
                OPT_OK
            }
            _ => OPT_NOTIMPL,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// `php_stream_temp_ops` (and `php_stream_rfc2397_ops`).
#[derive(Debug)]
pub struct TempOps {
    inner: Option<Box<Stream>>,
    smax: usize,
    mode: TempMode,
    meta: Option<Vec<(String, Value)>>,
    tmpdir: Option<Vec<u8>>,
    rfc2397: bool,
}

impl TempOps {
    /// Whether the data has been moved to a temporary file.
    pub fn is_file_backed(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|s| s.ops_label() == "STDIO")
    }

    fn spill(&mut self) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        let pos = inner.tell();
        let data = inner
            .ops_as::<MemoryOps>()
            .map(|m| m.buffer().to_vec())
            .unwrap_or_default();
        let Some(mut file) = crate::plain::temporary_stream(self.tmpdir.as_deref(), b"php") else {
            return false;
        };
        let _ = file.write(&data);
        file.seek(pos, SEEK_SET);
        self.inner = Some(Box::new(file));
        true
    }
}

/// `php_stream_temp_create_ex()`.
pub fn temp_create(mode: TempMode, max_memory: usize, tmpdir: Option<&[u8]>) -> Stream {
    let ops = TempOps {
        inner: Some(Box::new(memory_create(mode))),
        smax: max_memory,
        mode,
        meta: None,
        tmpdir: tmpdir.map(<[u8]>::to_vec),
        rfc2397: false,
    };
    let mut s = Stream::new(Box::new(ops), mode.as_mode());
    s.add_flags(flags::NO_BUFFER);
    s
}

/// `php_stream_temp_open()`: a temp stream pre-filled with `data`.
pub fn temp_open(mode: TempMode, max_memory: usize, data: &[u8]) -> Stream {
    let mut s = temp_create(TempMode::Default, max_memory, None);
    if !data.is_empty() {
        if let Some(t) = s.ops_as_mut::<TempOps>() {
            let mut st = dummy_state();
            let _ = t.write(&mut st, data);
            let mut p = 0;
            t.seek(&mut st, 0, SEEK_SET, &mut p);
        }
    }
    if let Some(t) = s.ops_as_mut::<TempOps>() {
        t.mode = mode;
    }
    s
}

fn dummy_state() -> StreamState {
    StreamState {
        id: 0,
        eof: false,
        fatal_error: false,
        flags: 0,
        wrapper_label: "stream",
        has_buffered_data: false,
        last_errno: 0,
    }
}

impl StreamOps for TempOps {
    fn label(&self) -> &'static str {
        if self.rfc2397 {
            "RFC2397"
        } else {
            "TEMP"
        }
    }

    fn read(&mut self, st: &mut StreamState, buf: &mut [u8]) -> Result<usize, OpFailed> {
        let inner = self.inner.as_mut().ok_or(OpFailed)?;
        let got = inner.read(buf);
        st.eof = inner.state.eof;
        got
    }

    fn can_write(&self) -> bool {
        !self.rfc2397
    }

    fn write(&mut self, _st: &mut StreamState, buf: &[u8]) -> Result<usize, OpFailed> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(OpFailed);
        };
        if inner.ops_label() == "MEMORY" {
            let pos = usize::try_from(inner.tell()).unwrap_or(0);
            if pos + buf.len() >= self.smax && !self.spill() {
                crate::wrapper::wrapper_warn(
                    crate::wrapper::DEFAULT_WRAPPER_NAME,
                    crate::wrapper::REPORT_ERRORS,
                    crate::diag::StreamErrorCode::PermissionDenied,
                    "Unable to create temporary file, Check permissions in temporary files directory.",
                );
                return Ok(0);
            }
        }
        self.inner.as_mut().ok_or(OpFailed)?.write(buf)
    }

    fn close(&mut self, _st: &mut StreamState, preserve: bool) -> i32 {
        self.inner.take().map_or(0, |mut s| s.close_ex(preserve))
    }

    fn flush(&mut self, _st: &mut StreamState) -> i32 {
        self.inner.as_mut().map_or(-1, |s| s.flush())
    }

    fn can_seek(&self) -> bool {
        true
    }

    fn seek(&mut self, st: &mut StreamState, offset: i64, whence: i32, newpos: &mut i64) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            *newpos = -1;
            return false;
        };
        let r = inner.seek(offset, whence);
        *newpos = inner.tell();
        st.eof = inner.state.eof;
        r == 0
    }

    fn stat(&mut self, _st: &mut StreamState) -> Option<StatBuf> {
        self.inner.as_mut()?.stat()
    }

    fn set_option(&mut self, _st: &mut StreamState, opt: &mut StreamOption<'_>) -> i64 {
        if let StreamOption::MetaData(out) = opt {
            if let Some(m) = &self.meta {
                out.extend(m.iter().cloned());
            }
            return OPT_OK;
        }
        match self.inner.as_mut() {
            Some(inner) => inner.set_option(opt),
            None => OPT_NOTIMPL,
        }
    }

    fn cast(
        &mut self,
        _st: &mut StreamState,
        kind: CastAs,
        report_errors: bool,
    ) -> Option<std::os::fd::RawFd> {
        let inner = self.inner.as_mut()?;
        if inner.ops_label() == "STDIO" {
            return inner.cast(kind, report_errors);
        }
        if !report_errors {
            return None;
        }
        if !self.spill() {
            crate::wrapper::wrapper_warn(
                crate::wrapper::DEFAULT_WRAPPER_NAME,
                crate::wrapper::REPORT_ERRORS,
                crate::diag::StreamErrorCode::PermissionDenied,
                "Unable to create temporary file.",
            );
            return None;
        }
        self.inner.as_mut()?.cast(kind, report_errors)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Decodes base64 like `php_base64_decode_ex(..., strict)`.
pub fn base64_decode(input: &[u8], strict: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut i = 0usize;
    let mut padding = 0usize;
    let mut cur: u8 = 0;
    for &c in input {
        if c == b'=' {
            padding += 1;
            continue;
        }
        let v: i16 = match c {
            b'A'..=b'Z' => i16::from(c - b'A'),
            b'a'..=b'z' => i16::from(c - b'a') + 26,
            b'0'..=b'9' => i16::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'\t' | b'\n' | b'\r' | b' ' => -1,
            _ => -2,
        };
        if strict {
            if v == -1 {
                continue;
            }
            if v == -2 || padding > 0 {
                return None;
            }
        } else if v < 0 {
            continue;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ch = v as u8;
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
    if strict && i % 4 == 1 {
        return None;
    }
    if strict && padding > 0 && (padding > 2 || (i + padding) % 4 != 0) {
        return None;
    }
    Some(out)
}

/// `php_url_decode()`: `+` to space and `%XX` escapes.
pub fn url_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len()
                && s[i + 1].is_ascii_hexdigit()
                && s[i + 2].is_ascii_hexdigit() =>
            {
                let h = |c: u8| -> u8 {
                    match c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        _ => c - b'A' + 10,
                    }
                };
                out.push(h(s[i + 1]) << 4 | h(s[i + 2]));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// A parsed `data:` URL.
#[derive(Debug, Clone, PartialEq)]
pub struct DataUrl {
    /// The meta data entries (mediatype, parameters, base64) in PHP order.
    pub meta: Vec<(String, Value)>,
    /// The decoded payload.
    pub data: Vec<u8>,
}

/// Parses an RFC 2397 URL (`php_stream_url_wrap_rfc2397`). Errors are the
/// logged messages ("rfc2397: no comma in URL", …).
pub fn parse_data_url(path: &[u8]) -> Result<DataUrl, &'static str> {
    let Some(mut p) = path.strip_prefix(b"data:") else {
        return Err("");
    };
    if p.starts_with(b"//") {
        p = &p[2..];
    }
    let Some(comma) = p.iter().position(|&c| c == b',') else {
        return Err("rfc2397: no comma in URL");
    };
    let mut meta: Vec<(String, Value)> = Vec::new();
    let mut base64 = false;
    if comma != 0 {
        let mut m = &p[..comma];
        let semi = m.iter().position(|&c| c == b';');
        let sep = m.iter().position(|&c| c == b'/');
        if semi.is_none() && sep.is_none() {
            return Err("rfc2397: illegal media type");
        }
        match (semi, sep) {
            (None, _) => {
                meta.push(("mediatype".into(), Value::Str(m.to_vec())));
                m = b"";
            }
            (Some(se), Some(sp)) if sp < se => {
                meta.push(("mediatype".into(), Value::Str(m[..se].to_vec())));
                m = &m[se..];
            }
            (Some(se), _) => {
                if se != 0 || m != b";base64" {
                    return Err("rfc2397: illegal media type");
                }
            }
        }
        while m.first() == Some(&b';') {
            m = &m[1..];
            let eq = m.iter().position(|&c| c == b'=');
            let semi = m.iter().position(|&c| c == b';');
            match eq {
                Some(e) if semi.is_none_or(|s| s > e) => {
                    let vlen = semi.map_or(m.len() - e, |s| s - e) - 1;
                    let name = &m[..e];
                    if name != b"mediatype" {
                        let key = String::from_utf8_lossy(name).into_owned();
                        let val = Value::Str(m[e + 1..e + 1 + vlen].to_vec());
                        if let Some(slot) = meta.iter_mut().find(|(k, _)| *k == key) {
                            slot.1 = val;
                        } else {
                            meta.push((key, val));
                        }
                    }
                    m = &m[e + 1 + vlen..];
                }
                _ => {
                    if m != b"base64" {
                        return Err("rfc2397: illegal parameter");
                    }
                    base64 = true;
                    m = b"";
                    break;
                }
            }
        }
        if !m.is_empty() {
            return Err("rfc2397: illegal URL");
        }
    }
    meta.push(("base64".into(), Value::Bool(base64)));
    let payload = &p[comma + 1..];
    let data = if base64 {
        base64_decode(payload, true).ok_or("rfc2397: unable to decode")?
    } else {
        url_decode(payload)
    };
    Ok(DataUrl { meta, data })
}

/// Builds the stream for a parsed data URL.
pub fn data_url_stream(url: DataUrl, mode: &str) -> Stream {
    let mut s = temp_create(TempMode::Default, usize::MAX, None);
    if let Some(t) = s.ops_as_mut::<TempOps>() {
        let mut st = dummy_state();
        let _ = t.write(&mut st, &url.data);
        let mut p = 0;
        t.seek(&mut st, 0, SEEK_SET, &mut p);
        t.rfc2397 = true;
        let mb = mode.as_bytes();
        t.mode = if mb.first() == Some(&b'r') && mb.get(1) != Some(&b'+') {
            TempMode::ReadOnly
        } else {
            TempMode::Default
        };
        t.meta = Some(url.meta);
    }
    s.mode = mode.chars().take(15).collect();
    s
}

/// The `data:` wrapper (`php_stream_rfc2397_wrapper`).
#[derive(Debug, Default)]
pub struct DataWrapper;

impl Wrapper for DataWrapper {
    fn label(&self) -> &'static str {
        "RFC2397"
    }

    fn is_url(&self) -> bool {
        true
    }

    fn open(
        &self,
        path: &[u8],
        mode: &str,
        _options: u32,
        _opened_path: &mut Option<Vec<u8>>,
        _context: Option<&ContextRef>,
    ) -> Result<Stream, OpenError> {
        match parse_data_url(path) {
            Ok(url) => Ok(data_url_stream(url, mode)),
            Err("") => Err(OpenError::silent()),
            Err(m) => Err(OpenError::msg(m)),
        }
    }
}

/// The [`WrapperInfo`] for `data:` streams.
pub fn data_wrapper_info() -> WrapperInfo {
    WrapperInfo {
        label: "RFC2397",
        is_url: true,
        is_plain_files: false,
    }
}

/// Meta data entries as a PHP array (for tests and the VM).
pub fn meta_value(meta: &[(String, Value)]) -> Value {
    Value::Array(
        meta.iter()
            .map(|(k, v)| (Key::str(k.as_bytes()), v.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
