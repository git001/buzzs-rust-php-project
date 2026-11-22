// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `multipart/form-data` request bodies: a port of `main/rfc1867.c`
//! (`rfc1867_post_handler` and the libapreq-derived multipart buffer).
//!
//! The parser streams the body from a reader closure (the SAPI's
//! `read_post`), writes uploaded files to `upload_tmp_dir`, and produces
//! the ordered list of `$_POST` / `$_FILES` registrations (variable names
//! already normalised as `safe_php_register_variable()` leaves them) plus
//! the warnings PHP would raise.  Registering the variables is left to the
//! caller (it needs the VM for `php_register_variable_ex`).

use php_memlimit::TryGrow;
use php_vm::ext::{Rfc1867Event, Rfc1867Translation};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `FILLUNIT`: the multipart buffer size.
const FILLUNIT: usize = 1024 * 5;

pub use php_sapi::upload_err;

/// INI settings consulted by the upload handler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct UploadSettings {
    /// `max_file_uploads`.
    pub max_file_uploads: i64,
    /// `max_multipart_body_parts` (`-1` = `max_input_vars + max_file_uploads`).
    pub max_multipart_body_parts: i64,
    /// `post_max_size` in bytes (0 = unlimited).
    pub post_max_size: i64,
    /// `max_input_vars`.
    pub max_input_vars: i64,
    /// `upload_max_filesize` in bytes (0 = unlimited).
    pub upload_max_filesize: i64,
    /// `file_uploads`.
    pub file_uploads: bool,
    /// `upload_tmp_dir`: where uploaded files go (empty: the system
    /// temporary directory, which is also the fallback).
    pub upload_tmp_dir: Vec<u8>,
    /// `SG(read_post_bytes)` when the parse starts: what earlier reads of
    /// the body took (a parse at request startup before the one of
    /// `request_parse_body()`, or `php://input`). The parse counts on from
    /// there, against `post_max_size` and in what the observer is told.
    pub read_post_bytes: i64,
}

impl Default for UploadSettings {
    fn default() -> Self {
        UploadSettings {
            max_file_uploads: 20,
            max_multipart_body_parts: -1,
            post_max_size: 8 * 1024 * 1024,
            max_input_vars: 1000,
            upload_max_filesize: 2 * 1024 * 1024,
            file_uploads: true,
            upload_tmp_dir: Vec::new(),
            read_post_bytes: 0,
        }
    }
}

/// The superglobal a registration goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `$_POST`.
    Post,
    /// `$_FILES`.
    Files,
}

/// A registered value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    /// A string.
    Str(Vec<u8>),
    /// An integer (`error`, `size`).
    Long(i64),
}

/// One `php_register_variable_ex()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// Target array.
    pub target: Target,
    /// Variable name (`a[b][c]` syntax).
    pub name: Vec<u8>,
    /// Value.
    pub value: RegValue,
}

/// A warning raised while parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Message.
    pub message: String,
    /// `php_error_docref()` (prefixed with the function, `"PHP Request
    /// Startup: "` during startup) rather than `sapi_error()`.
    pub docref: bool,
    /// An `E_NOTICE` (the temporary file went to the system temporary
    /// directory) rather than an `E_WARNING`; never an exception.
    pub notice: bool,
}

/// The result of [`parse`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Variable registrations, in order.
    pub registrations: Vec<Registration>,
    /// Warnings, in order.
    pub warnings: Vec<Warning>,
    /// Temporary files of successful uploads (`SG(rfc1867_uploaded_files)`).
    pub uploaded: Vec<PathBuf>,
    /// `SG(read_post_bytes)` once the parse is over: the body bytes read
    /// before it ([`UploadSettings::read_post_bytes`]) plus those it read.
    pub read_bytes: i64,
}

/// C `isspace()`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// C string view: bytes before the first NUL.
fn c_str(s: &[u8]) -> &[u8] {
    s.split(|&c| c == 0).next().unwrap_or_default()
}

/// `php_ap_memstr()`: first position of `needle`, allowing a match cut
/// off by the end of the buffer when `partial`. A whole match always comes
/// before a cut-off one, which can only start in the last
/// `needle.len() - 1` bytes, where the rest of the buffer is a prefix of
/// `needle`.
fn memstr(hay: &[u8], needle: &[u8], partial: bool) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let whole = memchr::memmem::find(hay, needle);
    if whole.is_some() || !partial {
        return whole;
    }
    let tail = hay.len().saturating_sub(needle.len() - 1);
    (tail..hay.len()).find(|&i| needle.starts_with(&hay[i..]))
}

/// `ZEND_HAS_OOM()`: a `memory_limit` violation is pending. The parser
/// checks it after every step that can allocate a request-sized amount
/// and then stops (`goto fileupload_done`), so the soft limit is reported
/// once the body is parsed instead of the hard limit being reached.
fn oom() -> bool {
    php_memlimit::is_pending()
}

/// Appends `extra` to `v` with a fallible reallocation (`safe_ferealloc(...,
/// fallible)` / a smart string's growth): growth past `memory_limit` fails,
/// leaving the violation pending, and returns `false`.
fn try_append(v: &mut Vec<u8>, extra: &[u8]) -> bool {
    if v.try_grow(extra.len()).is_err() {
        return false;
    }
    v.extend_from_slice(extra);
    true
}

/// The multipart read buffer (`multipart_buffer`).
struct MultipartBuffer<'a> {
    buffer: Vec<u8>,
    begin: usize,
    bytes_in_buffer: usize,
    bufsize: usize,
    boundary: Vec<u8>,
    boundary_next: Vec<u8>,
    read: &'a mut dyn FnMut(&mut [u8]) -> usize,
    read_bytes: i64,
    post_max_size: i64,
}

impl MultipartBuffer<'_> {
    /// `fill_buffer()`.
    fn fill(&mut self) -> usize {
        if self.bytes_in_buffer > 0 && self.begin != 0 {
            self.buffer
                .copy_within(self.begin..self.begin + self.bytes_in_buffer, 0);
        }
        self.begin = 0;
        if self.post_max_size > 0 && self.read_bytes > self.post_max_size {
            return 0;
        }
        let mut total = 0;
        while self.bytes_in_buffer < self.bufsize {
            let start = self.bytes_in_buffer;
            let n = (self.read)(&mut self.buffer[start..self.bufsize]);
            if n == 0 {
                break;
            }
            self.bytes_in_buffer += n;
            self.read_bytes += i64::try_from(n).unwrap_or(0);
            total += n;
        }
        total
    }

    /// `multipart_buffer_eof()`.
    fn eof(&mut self) -> bool {
        self.bytes_in_buffer == 0 && self.fill() < 1
    }

    /// `next_line()`.
    fn next_line(&mut self) -> Option<Vec<u8>> {
        let data = &self.buffer[self.begin..self.begin + self.bytes_in_buffer];
        if let Some(lf) = data.iter().position(|&c| c == b'\n') {
            let mut line = data[..lf].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.begin += lf + 1;
            self.bytes_in_buffer -= lf + 1;
            Some(line)
        } else if self.bytes_in_buffer < self.bufsize {
            None
        } else {
            let line = data.to_vec();
            self.bytes_in_buffer = 0;
            Some(line)
        }
    }

    /// `get_line()`.
    fn get_line(&mut self) -> Option<Vec<u8>> {
        if let Some(l) = self.next_line() {
            return Some(l);
        }
        self.fill();
        self.next_line()
    }

    /// `find_boundary()`.
    fn find_boundary(&mut self) -> bool {
        while let Some(line) = self.get_line() {
            if c_str(&line) == self.boundary.as_slice() {
                return true;
            }
        }
        false
    }

    /// `multipart_buffer_headers()`: the saved part headers, or `None` when
    /// no further boundary was found. With encoding translation every
    /// header line sets the input encoding (`mbuff->input_encoding`).
    fn headers(
        &mut self,
        mut tr: Option<&mut (dyn Rfc1867Translation + '_)>,
    ) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        if !self.find_boundary() {
            return None;
        }
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut buf_value: Option<Vec<u8>> = None;
        let mut key: Option<Vec<u8>> = None;
        while let Some(raw) = self.get_line() {
            let line = c_str(&raw);
            if line.is_empty() {
                break;
            }
            if let Some(t) = tr.as_deref_mut() {
                t.detect(line);
            }
            let colon = if is_space(line[0]) {
                None
            } else {
                line.iter().position(|&c| c == b':')
            };
            if let Some(colon) = colon {
                if let (Some(_), Some(_)) = (&buf_value, &key) {
                    headers.push((
                        key.take().unwrap_or_default(),
                        buf_value.take().unwrap_or_default(),
                    ));
                }
                let name = &line[..colon];
                if !should_save_header(name) || header_value(&headers, name).is_some() {
                    key = None;
                } else {
                    key = Some(name.to_vec());
                    let value = &line[colon + 1..];
                    let value = &value[value
                        .iter()
                        .position(|&c| !is_space(c))
                        .unwrap_or(value.len())..];
                    if !try_append(buf_value.get_or_insert_with(Vec::new), value) {
                        return None;
                    }
                }
            } else if let Some(b) = buf_value.as_mut() {
                if !try_append(b, line) {
                    return None;
                }
            } else {
                continue;
            }
            // `goto oom`: the parse ends.
            if oom() {
                return None;
            }
        }
        if let (Some(k), Some(v)) = (key, buf_value) {
            headers.push((k, v));
        }
        Some(headers)
    }

    /// `multipart_buffer_read()`: up to `bytes - 1` bytes before the next
    /// boundary; sets `end` when the boundary is in the buffer.
    fn read_part(&mut self, bytes: usize, end: Option<&mut bool>) -> Vec<u8> {
        if bytes > self.bytes_in_buffer {
            self.fill();
        }
        let data = &self.buffer[self.begin..self.begin + self.bytes_in_buffer];
        let bound = memstr(data, &self.boundary_next, true);
        let max = if let Some(b) = bound {
            if let Some(end) = end {
                if memstr(data, &self.boundary_next, false).is_some() {
                    *end = true;
                }
            }
            b
        } else {
            self.bytes_in_buffer
        };
        let mut len = max.min(bytes - 1);
        if len == 0 {
            return Vec::new();
        }
        let mut out = data[..len].to_vec();
        if bound.is_some() && out.last() == Some(&b'\r') {
            out.pop();
            len -= 1;
        }
        self.bytes_in_buffer -= len;
        self.begin += len;
        out
    }

    /// `multipart_buffer_read_body()`.
    fn read_body(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let chunk = self.read_part(FILLUNIT, None);
            if chunk.is_empty() || !try_append(&mut out, &chunk) {
                break;
            }
        }
        out
    }
}

/// `php_mime_should_save_hdr()`.
fn should_save_header(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"Content-Disposition") || name.eq_ignore_ascii_case(b"Content-Type")
}

/// `php_mime_get_hdr_value()`.
fn header_value<'a>(headers: &'a [(Vec<u8>, Vec<u8>)], key: &[u8]) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_slice())
}

/// The byte length of a character by its first byte
/// (`php_mb_mbchar_bytes()`); always 1 without encoding translation, where
/// the `php_ap_*` helpers step one byte at a time.
type CharLen<'a> = &'a dyn Fn(u8) -> usize;

/// [`CharLen`] of the plain (`php_ap_*`) helpers.
#[cfg(test)]
fn single_byte(_: u8) -> usize {
    1
}

/// The input encoding's [`CharLen`] of an optional translation.
fn input_char_len(tr: Option<&dyn Rfc1867Translation>, lead: u8) -> usize {
    tr.map_or(1, |t| t.input_char_len(lead).max(1))
}

/// The internal encoding's [`CharLen`] of an optional translation.
fn internal_char_len(tr: Option<&dyn Rfc1867Translation>, lead: u8) -> usize {
    tr.map_or(1, |t| t.internal_char_len(lead).max(1))
}

/// `zend_multibyte_encoding_converter()` of a field or file name from the
/// input encoding (if one was detected); the result is used as a C string.
fn translate_name(tr: Option<&mut (dyn Rfc1867Translation + '_)>, name: Vec<u8>) -> Vec<u8> {
    match tr.and_then(|t| t.convert(&name)) {
        Some(converted) => c_str(&converted).to_vec(),
        None => name,
    }
}

/// `php_ap_getword()` / `php_mb_rfc1867_getword()`: the word up to `stop`
/// (respecting quotes, stepping over whole characters) and the rest after
/// any run of `stop` characters.
fn getword<'a>(line: &'a [u8], stop: u8, clen: CharLen<'_>) -> (Vec<u8>, &'a [u8]) {
    let mut pos = 0;
    while pos < line.len() && line[pos] != stop {
        let q = line[pos];
        if q == b'"' || q == b'\'' {
            pos += 1;
            while pos < line.len() && line[pos] != q {
                if line[pos] == b'\\' && line.get(pos + 1) == Some(&q) {
                    pos += 2;
                } else {
                    pos += 1;
                }
            }
            if pos < line.len() {
                pos += 1;
            }
        } else {
            pos = (pos + clen(q)).min(line.len());
        }
    }
    if pos >= line.len() {
        return (line.to_vec(), &line[line.len()..]);
    }
    let word = line[..pos].to_vec();
    while pos < line.len() && line[pos] == stop {
        pos = (pos + clen(stop)).min(line.len());
    }
    (word, &line[pos..])
}

/// `substring_conf()` / `php_mb_rfc1867_substring_conf()`: unescapes `\\`
/// and `\<quote>`, copying other characters whole (a multibyte character
/// whose last byte is a backslash escapes nothing).
fn substring_conf(s: &[u8], quote: u8, clen: CharLen<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() && (quote == 0 || s[i] != quote) {
        if s[i] == b'\\'
            && (s.get(i + 1) == Some(&b'\\') || (quote != 0 && s.get(i + 1) == Some(&quote)))
        {
            i += 1;
            out.push(s[i]);
            i += 1;
        } else {
            let end = (i + clen(s[i])).min(s.len());
            out.extend_from_slice(&s[i..end]);
            i = end;
        }
    }
    out
}

/// `php_ap_getword_conf()` / `php_mb_rfc1867_getword_conf()`.
fn getword_conf(s: &[u8], clen: CharLen<'_>) -> Vec<u8> {
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let s = &s[start..];
    match s.first() {
        None => Vec::new(),
        Some(&q) if q == b'"' || q == b'\'' => substring_conf(&s[1..], q, clen),
        Some(_) => {
            let end = s.iter().position(|&c| is_space(c)).unwrap_or(s.len());
            substring_conf(&s[..end], 0, clen)
        }
    }
}

/// `php_mb_safe_strrchr()`: the last `c` that starts a character; `None`
/// as well when the last character is cut off.
fn safe_strrchr(s: &[u8], c: u8, clen: CharLen<'_>) -> Option<usize> {
    let mut last = None;
    let mut p = 0;
    while p < s.len() {
        if s[p] == c {
            last = Some(p);
        }
        let n = clen(s[p]);
        if s.len() - p < n {
            return None;
        }
        p += n;
    }
    last
}

/// `php_ap_basename()` / `php_mb_rfc1867_basename()`.
fn basename<'a>(path: &'a [u8], clen: CharLen<'_>) -> &'a [u8] {
    let bs = safe_strrchr(path, b'\\', clen);
    let fs = safe_strrchr(path, b'/', clen);
    match (bs, fs) {
        (Some(a), Some(b)) => &path[a.max(b) + 1..],
        (Some(a), None) | (None, Some(a)) => &path[a + 1..],
        (None, None) => path,
    }
}

/// `normalize_protected_variable()`.
pub fn normalize_protected_variable(var: &[u8]) -> Vec<u8> {
    let start = var.iter().position(|&c| c != b' ').unwrap_or(var.len());
    // Work on a NUL-terminated copy to mirror the C pointer arithmetic.
    let mut v: Vec<u8> = c_str(&var[start..]).to_vec();
    for c in &mut v {
        if *c == b'[' {
            break;
        }
        if *c == b' ' || *c == b'.' {
            *c = b'_';
        }
    }
    let Some(open) = v.iter().position(|&c| c == b'[') else {
        return v;
    };
    v.push(0);
    let mut index = open + 1;
    let mut s = index;
    loop {
        while matches!(v[index], b' ' | b'\r' | b'\n' | b'\t') {
            index += 1;
        }
        let rest_len = v[index..].iter().position(|&c| c == 0).unwrap_or(0);
        let indexend = match v[index..index + rest_len].iter().position(|&c| c == b']') {
            Some(p) => index + p + 1,
            None => index + rest_len,
        };
        if s == index {
            s = indexend;
        } else {
            // memmove(s, index, strlen(index) + 1)
            v.copy_within(index..=index + rest_len, s);
            s += indexend - index;
        }
        if v[s] == b'[' {
            s += 1;
            index = s;
        } else {
            break;
        }
    }
    v.truncate(s);
    v
}

/// `php_open_temporary_fd_ex(PG(upload_tmp_dir), "php", ...,
/// PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ON_FALLBACK)`: a new `php` + 13 random
/// characters + 6 file in `upload_tmp_dir`, else in the system temporary
/// directory; `fell_back` when `upload_tmp_dir` was set and failed.
fn open_upload_file(upload_tmp_dir: &[u8]) -> (Option<(std::fs::File, PathBuf)>, bool) {
    let mut fell_back = false;
    let file = php_streams::temp::create_temporary(
        Some(upload_tmp_dir),
        Some(b"php"),
        php_streams::temp::CHECK_ON_FALLBACK,
        &mut || fell_back = true,
    );
    let file = file.map(|(f, path)| (f, PathBuf::from(std::ffi::OsStr::from_bytes(&path))));
    (file, fell_back)
}

struct Parser {
    out: Outcome,
    protected: HashSet<Vec<u8>>,
}

impl Parser {
    fn warn(&mut self, message: String, docref: bool) {
        self.out.warnings.push(Warning {
            message,
            docref,
            notice: false,
        });
    }

    /// `safe_php_register_variable()`: `false` once a `memory_limit`
    /// violation is pending (the parse then ends).
    #[must_use]
    fn register(
        &mut self,
        target: Target,
        name: &[u8],
        value: RegValue,
        override_protection: bool,
    ) -> bool {
        let name = normalize_protected_variable(name);
        if override_protection || !self.protected.contains(&name) {
            self.out.registrations.push(Registration {
                target,
                name,
                value,
            });
        }
        !oom()
    }
}

/// Splits the boundary out of a `multipart/form-data` content type.
fn boundary_of(content_type: &[u8]) -> Result<Vec<u8>, &'static str> {
    let ct = c_str(content_type);
    let pos = memstr(ct, b"boundary", false)
        .or_else(|| memstr(&ct.to_ascii_lowercase(), b"boundary", false))
        .ok_or("Missing boundary in multipart/form-data POST data")?;
    let eq = ct[pos..]
        .iter()
        .position(|&c| c == b'=')
        .ok_or("Missing boundary in multipart/form-data POST data")?;
    let b = &ct[pos + eq + 1..];
    if b.first() == Some(&b'"') {
        let b = &b[1..];
        let end = b
            .iter()
            .position(|&c| c == b'"')
            .ok_or("Invalid boundary in multipart/form-data POST data")?;
        Ok(b[..end].to_vec())
    } else {
        let end = b
            .iter()
            .position(|&c| c == b',' || c == b';')
            .unwrap_or(b.len());
        Ok(b[..end].to_vec())
    }
}

/// `rfc1867_post_handler()`: parses a `multipart/form-data` body.
pub fn parse(
    content_type: &[u8],
    content_length: i64,
    settings: &UploadSettings,
    read: &mut dyn FnMut(&mut [u8]) -> usize,
) -> Outcome {
    parse_with_events(
        content_type,
        content_length,
        settings,
        read,
        &mut |_, _| true,
        None,
    )
}

/// [`parse`] reporting its progress to `events` (`php_rfc1867_callback`),
/// which also receives `SG(read_post_bytes)`; an event returning `false`
/// skips the field or cancels the upload, as `FAILURE` does in PHP.
/// `translation` is mbstring's multibyte callbacks
/// (`mbstring.encoding_translation`): names, file names and values are
/// converted from the input encoding detected on the part headers.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn parse_with_events(
    content_type: &[u8],
    content_length: i64,
    settings: &UploadSettings,
    read: &mut dyn FnMut(&mut [u8]) -> usize,
    events: &mut dyn FnMut(Rfc1867Event<'_>, i64) -> bool,
    translation: Option<&mut (dyn Rfc1867Translation + '_)>,
) -> Outcome {
    let mut tr = translation;
    let mut p = Parser {
        out: Outcome::default(),
        protected: HashSet::new(),
    };
    let post_max_size = settings.post_max_size;
    if post_max_size > 0 && content_length > post_max_size {
        p.warn(
            format!(
                "POST Content-Length of {content_length} bytes exceeds the limit of {post_max_size} bytes"
            ),
            true,
        );
        return p.out;
    }
    let mut upload_cnt = settings.max_file_uploads;
    let mut body_parts_cnt = if settings.max_multipart_body_parts < 0 {
        settings.max_input_vars + upload_cnt
    } else {
        settings.max_multipart_body_parts
    };
    let body_parts_limit = body_parts_cnt;
    let boundary = match boundary_of(content_type) {
        Ok(b) => b,
        Err(msg) => {
            p.warn(msg.to_string(), true);
            return p.out;
        }
    };
    if boundary.len() > FILLUNIT - 4 {
        p.warn(
            "Boundary too large in multipart/form-data POST data".to_string(),
            false,
        );
        return p.out;
    }
    let bufsize = (boundary.len() + 6).max(FILLUNIT);
    let mut full = b"--".to_vec();
    full.extend_from_slice(&boundary);
    let mut next = b"\n--".to_vec();
    next.extend_from_slice(&boundary);
    let mut mb = MultipartBuffer {
        buffer: vec![0; bufsize + 1],
        begin: 0,
        bytes_in_buffer: 0,
        bufsize,
        boundary: full,
        boundary_next: next,
        read,
        // `fill_buffer()` and `fileupload_done` test SG(read_post_bytes),
        // which counts the whole request.
        read_bytes: settings.read_post_bytes,
        post_max_size,
    };
    let mut anonymous_index = 0u32;
    let mut count = 0i64;
    let mut max_file_size: i64 = 0;

    // A failed start event goes straight to `fileupload_done`.
    let started = events(Rfc1867Event::Start { content_length }, mb.read_bytes) && !oom();

    'parts: while started && !mb.eof() {
        if oom() {
            break;
        }
        let Some(headers) = mb.headers(tr.as_deref_mut()) else {
            break;
        };
        let Some(cd) = header_value(&headers, b"Content-Disposition") else {
            continue;
        };
        body_parts_cnt -= 1;
        if body_parts_cnt < 0 {
            p.warn(
                format!(
                    "Multipart body parts limit exceeded {body_parts_limit}. To increase the limit change max_multipart_body_parts in php.ini."
                ),
                true,
            );
            break;
        }
        let mut cd: &[u8] = &cd[cd.iter().position(|&c| !is_space(c)).unwrap_or(cd.len())..];
        let mut param: Option<Vec<u8>> = None;
        let mut filename: Option<Vec<u8>> = None;
        while !cd.is_empty() {
            let (pair, rest) = getword(cd, b';', &|c| input_char_len(tr.as_deref(), c));
            if oom() {
                break 'parts;
            }
            cd = &rest[rest
                .iter()
                .position(|&c| !is_space(c))
                .unwrap_or(rest.len())..];
            if pair.contains(&b'=') {
                let (key, value) = getword(&pair, b'=', &|c| input_char_len(tr.as_deref(), c));
                if oom() {
                    break 'parts;
                }
                if key.eq_ignore_ascii_case(b"name") {
                    let word = getword_conf(value, &|c| input_char_len(tr.as_deref(), c));
                    param = Some(translate_name(tr.as_deref_mut(), word));
                } else if key.eq_ignore_ascii_case(b"filename") {
                    let word = getword_conf(value, &|c| input_char_len(tr.as_deref(), c));
                    filename = Some(translate_name(tr.as_deref_mut(), word));
                }
                if oom() {
                    break 'parts;
                }
            }
        }
        if oom() {
            break;
        }

        // A normal form variable.
        if filename.is_none() {
            if let Some(param) = param {
                let mut value = mb.read_body();
                if oom() {
                    break;
                }
                if let Some(converted) = tr.as_deref_mut().and_then(|t| t.convert(&value)) {
                    value = converted;
                }
                count += 1;
                if count <= settings.max_input_vars {
                    if oom() {
                        break;
                    }
                    let event = Rfc1867Event::FormData {
                        name: &param,
                        value: &value,
                    };
                    if !events(event, mb.read_bytes) {
                        continue;
                    }
                    if oom() {
                        break;
                    }
                    if !p.register(Target::Post, &param, RegValue::Str(value.clone()), false) {
                        break;
                    }
                } else {
                    if count == settings.max_input_vars + 1 {
                        p.warn(
                            format!(
                                "Input variables exceeded {}. To increase the limit change max_input_vars in php.ini.",
                                settings.max_input_vars
                            ),
                            true,
                        );
                    }
                    let event = Rfc1867Event::FormData {
                        name: &param,
                        value: &value,
                    };
                    events(event, mb.read_bytes);
                    if oom() {
                        break;
                    }
                }
                if normalize_protected_variable(&param).eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                    max_file_size = php_support::atol(c_str(&value));
                }
                continue;
            }
        }

        let mut skip_upload = false;
        if !settings.file_uploads {
            skip_upload = true;
        } else if upload_cnt <= 0 {
            skip_upload = true;
            if upload_cnt == 0 {
                upload_cnt -= 1;
                p.warn(
                    "Maximum number of allowable file uploads has been exceeded".to_string(),
                    true,
                );
            }
        }
        if param.is_none() && filename.is_none() {
            p.warn("File Upload Mime headers garbled".to_string(), true);
            break;
        }
        let param = param.unwrap_or_else(|| {
            let n = anonymous_index.to_string().into_bytes();
            anonymous_index += 1;
            n
        });
        if !skip_upload {
            let mut c: i64 = 0;
            for (i, &ch) in param.iter().enumerate() {
                if ch == b'[' {
                    c += 1;
                } else if ch == b']' {
                    c -= 1;
                    if let Some(&n) = param.get(i + 1) {
                        if n != b'[' {
                            skip_upload = true;
                            break;
                        }
                    }
                }
                if c < 0 {
                    skip_upload = true;
                    break;
                }
            }
            if c != 0 {
                skip_upload = true;
            }
        }
        if skip_upload {
            continue;
        }
        let filename = filename.unwrap_or_default();
        let event = Rfc1867Event::FileStart {
            name: &param,
            filename: &filename,
        };
        if !events(event, mb.read_bytes) {
            continue;
        }
        if oom() {
            break;
        }

        let mut cancel: i64 = 0;
        let mut total: i64 = 0;
        let mut temp: Option<PathBuf> = None;
        let mut end = false;
        if filename.is_empty() {
            cancel = upload_err::NO_FILE;
        }
        if cancel == 0 {
            let mut chunk = mb.read_part(FILLUNIT, Some(&mut end));
            let mut file = None;
            let (opened, fell_back) = open_upload_file(&settings.upload_tmp_dir);
            if fell_back {
                p.out.warnings.push(Warning {
                    message: php_streams::temp::FALLBACK_NOTICE.to_string(),
                    docref: true,
                    notice: true,
                });
            }
            if let Some((f, path)) = opened {
                file = Some(f);
                temp = Some(path);
            } else {
                p.warn(
                    "File upload error - unable to create a temporary file".to_string(),
                    true,
                );
                cancel = upload_err::NO_TMP_DIR;
            }
            upload_cnt -= 1;
            while cancel == 0 && !chunk.is_empty() {
                let blen = i64::try_from(chunk.len()).unwrap_or(i64::MAX);
                let event = Rfc1867Event::FileData {
                    offset: total,
                    length: blen,
                };
                if !events(event, mb.read_bytes) {
                    cancel = upload_err::EXTENSION;
                } else if settings.upload_max_filesize > 0
                    && total + blen > settings.upload_max_filesize
                {
                    cancel = upload_err::INI_SIZE;
                } else if max_file_size != 0 && total + blen > max_file_size {
                    cancel = upload_err::FORM_SIZE;
                } else if let Some(f) = file.as_mut() {
                    if f.write_all(&chunk).is_err() {
                        cancel = upload_err::CANT_WRITE;
                    } else {
                        total += blen;
                    }
                }
                if oom() {
                    break;
                }
                if cancel == 0 {
                    chunk = mb.read_part(FILLUNIT, Some(&mut end));
                }
            }
            drop(file);
        }
        if cancel == 0 && !end {
            cancel = upload_err::PARTIAL;
        }
        let temp_bytes = temp.as_ref().map(|t| t.as_os_str().as_bytes().to_vec());
        let event = Rfc1867Event::FileEnd {
            temp_filename: temp_bytes.as_deref(),
            cancel_upload: cancel,
        };
        if !events(event, mb.read_bytes) {
            cancel = upload_err::EXTENSION;
        }
        if cancel == 0 && !oom() {
            if let Some(t) = &temp {
                p.out.uploaded.push(t.clone());
            }
        } else if let Some(t) = temp.take() {
            if cancel != upload_err::NO_TMP_DIR {
                let _ = std::fs::remove_file(t);
            }
        }
        if oom() {
            break;
        }

        let arr = param
            .iter()
            .position(|&c| c == b'[')
            .filter(|_| param.last() == Some(&b']'));
        let name_of = |base: &[u8], field: &str| -> Vec<u8> {
            let mut n = Vec::new();
            if let Some(start) = arr {
                n.extend_from_slice(&base[..start.min(base.len())]);
                n.push(b'[');
                n.extend_from_slice(field.as_bytes());
                n.extend_from_slice(b"][");
                n.extend_from_slice(&param[start + 1..param.len() - 1]);
                n.push(b']');
            } else {
                n.extend_from_slice(base);
                n.push(b'[');
                n.extend_from_slice(field.as_bytes());
                n.push(b']');
            }
            n
        };
        if !p.register(
            Target::Files,
            &name_of(&param, "name"),
            RegValue::Str(basename(&filename, &|c| internal_char_len(tr.as_deref(), c)).to_vec()),
            false,
        ) {
            break;
        }
        if !p.register(
            Target::Files,
            &name_of(&param, "full_path"),
            RegValue::Str(filename.clone()),
            false,
        ) {
            break;
        }
        let ctype = if cancel != 0 {
            Vec::new()
        } else {
            header_value(&headers, b"Content-Type")
                .map(|v| v.split(|&c| c == b';').next().unwrap_or_default().to_vec())
                .unwrap_or_default()
        };
        if !p.register(
            Target::Files,
            &name_of(&param, "type"),
            RegValue::Str(ctype),
            false,
        ) {
            break;
        }
        // add_protected_variable() normalises `param` in place.
        let nparam = normalize_protected_variable(&param);
        p.protected.insert(nparam.clone());
        if oom() {
            break;
        }
        let tmp_base = if arr.is_some() { param.clone() } else { nparam };
        let tmp_name = normalize_protected_variable(&name_of(&tmp_base, "tmp_name"));
        p.protected.insert(tmp_name.clone());
        let tmp_value = temp
            .as_ref()
            .map(|t| t.as_os_str().as_bytes().to_vec())
            .unwrap_or_default();
        if !p.register(Target::Files, &tmp_name, RegValue::Str(tmp_value), true) {
            break;
        }
        if !p.register(
            Target::Files,
            &name_of(&param, "error"),
            RegValue::Long(cancel),
            false,
        ) {
            break;
        }
        if !p.register(
            Target::Files,
            &name_of(&param, "size"),
            RegValue::Long(if cancel == 0 { total } else { 0 }),
            false,
        ) {
            break;
        }
    }
    p.out.read_bytes = mb.read_bytes;
    if post_max_size > 0 && mb.read_bytes > post_max_size {
        p.warn(
            format!(
                "Actual POST length does not match Content-Length, and exceeds {post_max_size} bytes"
            ),
            false,
        );
    }
    events(Rfc1867Event::End, mb.read_bytes);
    p.out
}

/// Deletes the temporary files of uploads that were not moved
/// (`destroy_uploaded_files_hash()`).
pub fn cleanup(files: &[PathBuf]) {
    for f in files {
        let _ = std::fs::remove_file(Path::new(f));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ct: &str, body: &[u8], settings: &UploadSettings) -> Outcome {
        let mut pos = 0;
        let mut read = |buf: &mut [u8]| {
            let n = buf.len().min(body.len() - pos);
            buf[..n].copy_from_slice(&body[pos..pos + n]);
            pos += n;
            n
        };
        parse(
            ct.as_bytes(),
            i64::try_from(body.len()).unwrap(),
            settings,
            &mut read,
        )
    }

    /// Uploads into PHP's temporary directory: each file gets its own
    /// random name (`php_open_temporary_fd()`), so the tests can share it.
    fn tmp_settings() -> UploadSettings {
        UploadSettings {
            upload_tmp_dir: php_streams::temp::temp_dir(),
            ..UploadSettings::default()
        }
    }

    fn names(o: &Outcome) -> Vec<(Target, String)> {
        o.registrations
            .iter()
            .map(|r| (r.target, String::from_utf8_lossy(&r.name).into_owned()))
            .collect()
    }

    const CT: &str = "multipart/form-data; boundary=XyZ";

    #[test]
    fn fields_and_file() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nhello\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"C:\\\\dir\\\\x.txt\"\r\nContent-Type: text/plain; charset=x\r\n\r\nfile\r\ncontent\r\n\
--XyZ--\r\n";
        let o = run(CT, body, &tmp_settings());
        assert!(o.warnings.is_empty(), "{:?}", o.warnings);
        assert_eq!(
            names(&o),
            vec![
                (Target::Post, "a".into()),
                (Target::Files, "f[name]".into()),
                (Target::Files, "f[full_path]".into()),
                (Target::Files, "f[type]".into()),
                (Target::Files, "f[tmp_name]".into()),
                (Target::Files, "f[error]".into()),
                (Target::Files, "f[size]".into()),
            ]
        );
        assert_eq!(o.registrations[0].value, RegValue::Str(b"hello".to_vec()));
        assert_eq!(o.registrations[1].value, RegValue::Str(b"x.txt".to_vec()));
        assert_eq!(
            o.registrations[2].value,
            RegValue::Str(b"C:\\dir\\x.txt".to_vec())
        );
        assert_eq!(
            o.registrations[3].value,
            RegValue::Str(b"text/plain".to_vec())
        );
        assert_eq!(o.registrations[5].value, RegValue::Long(0));
        assert_eq!(o.registrations[6].value, RegValue::Long(13));
        assert_eq!(o.uploaded.len(), 1);
        assert_eq!(std::fs::read(&o.uploaded[0]).unwrap(), b"file\r\ncontent");
        cleanup(&o.uploaded);
    }

    #[test]
    fn array_uploads_and_empty_file() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"u[]\"; filename=\"\"\r\n\r\n\r\n--XyZ--\r\n";
        let o = run(CT, body, &tmp_settings());
        assert_eq!(
            names(&o),
            vec![
                (Target::Files, "u[name][]".into()),
                (Target::Files, "u[full_path][]".into()),
                (Target::Files, "u[type][]".into()),
                (Target::Files, "u[tmp_name][]".into()),
                (Target::Files, "u[error][]".into()),
                (Target::Files, "u[size][]".into()),
            ]
        );
        assert_eq!(
            o.registrations[4].value,
            RegValue::Long(upload_err::NO_FILE)
        );
        assert!(o.uploaded.is_empty());
    }

    #[test]
    fn size_limits() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"MAX_FILE_SIZE\"\r\n\r\n3\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n12345\r\n--XyZ--\r\n";
        let o = run(CT, body, &tmp_settings());
        let err = o
            .registrations
            .iter()
            .find(|r| r.name == b"f[error]")
            .unwrap();
        assert_eq!(err.value, RegValue::Long(upload_err::FORM_SIZE));
        let mut s = tmp_settings();
        s.upload_max_filesize = 2;
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n12345\r\n--XyZ--\r\n";
        let o = run(CT, body, &s);
        let err = o
            .registrations
            .iter()
            .find(|r| r.name == b"f[error]")
            .unwrap();
        assert_eq!(err.value, RegValue::Long(upload_err::INI_SIZE));
        assert!(o.uploaded.is_empty());
    }

    #[test]
    fn post_max_size_and_boundary_errors() {
        let mut s = tmp_settings();
        s.post_max_size = 4;
        let o = run(CT, b"0123456789", &s);
        assert_eq!(
            o.warnings[0].message,
            "POST Content-Length of 10 bytes exceeds the limit of 4 bytes"
        );
        let o = run("multipart/form-data", b"x", &tmp_settings());
        assert_eq!(
            o.warnings[0].message,
            "Missing boundary in multipart/form-data POST data"
        );
        let o = run("multipart/form-data; boundary=\"abc", b"x", &tmp_settings());
        assert_eq!(
            o.warnings[0].message,
            "Invalid boundary in multipart/form-data POST data"
        );
        assert_eq!(
            boundary_of(b"multipart/form-data; BOUNDARY=\"q q\"").unwrap(),
            b"q q"
        );
    }

    #[test]
    fn max_file_uploads() {
        let mut s = tmp_settings();
        s.max_file_uploads = 1;
        let body =
            b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"; filename=\"1\"\r\n\r\n1\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"b\"; filename=\"2\"\r\n\r\n2\r\n--XyZ--\r\n";
        let o = run(CT, body, &s);
        assert_eq!(
            o.warnings[0].message,
            "Maximum number of allowable file uploads has been exceeded"
        );
        assert!(o.registrations.iter().all(|r| !r.name.starts_with(b"b")));
        cleanup(&o.uploaded);
    }

    #[test]
    fn protected_names_are_not_overwritten() {
        let body =
            b"--XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\n1\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nx\r\n--XyZ--\r\n";
        let o = run(CT, body, &tmp_settings());
        assert!(o.registrations.iter().all(|r| r.target == Target::Files));
        cleanup(&o.uploaded);
    }

    #[test]
    fn helpers() {
        assert_eq!(normalize_protected_variable(b" a.b[ x ][y"), b"a_b[x ][y");
        assert_eq!(normalize_protected_variable(b"a[ b][ c]"), b"a[b][c]");
        assert_eq!(normalize_protected_variable(b"plain"), b"plain");
        assert_eq!(basename(b"a/b\\c", &single_byte), b"c");
        assert_eq!(basename(b"a\\b/c", &single_byte), b"c");
        assert_eq!(getword_conf(b" \"a\\\"b\" ", &single_byte), b"a\"b");
        assert_eq!(getword_conf(b"plain rest", &single_byte), b"plain");
        let (w, rest) = getword(b"name=\"a;b\"; filename=x", b';', &single_byte);
        assert_eq!(w, b"name=\"a;b\"");
        assert_eq!(rest, b" filename=x");
        assert_eq!(memstr(b"abc\n--X", b"\n--XY", true), Some(3));
        assert_eq!(memstr(b"abc\n--X", b"\n--XY", false), None);
        // A whole match wins over a cut-off one further on; the first byte
        // of the needle repeating does not hide a match.
        assert_eq!(memstr(b"\n--XY \n--X", b"\n--XY", true), Some(0));
        assert_eq!(memstr(b"\n\n\n--XY", b"\n--XY", false), Some(2));
        assert_eq!(memstr(b"ab\n", b"\n--XY", true), Some(2));
        assert_eq!(memstr(b"\n-", b"\n--XY", false), None);
        assert_eq!(memstr(b"", b"\n--XY", true), None);
        assert_eq!(memstr(b"abc", b"", true), None);
    }

    /// A Shift_JIS-like translation: 0x81..=0x9F lead two-byte characters
    /// once a header line was seen; conversion brackets the bytes.
    struct FakeSjis {
        detected: bool,
        converted: usize,
    }

    impl Rfc1867Translation for FakeSjis {
        fn detect(&mut self, _line: &[u8]) {
            self.detected = true;
        }
        fn input_char_len(&self, lead: u8) -> usize {
            if self.detected && (0x81..=0x9f).contains(&lead) {
                2
            } else {
                1
            }
        }
        fn internal_char_len(&self, _lead: u8) -> usize {
            1
        }
        fn convert(&mut self, s: &[u8]) -> Option<Vec<u8>> {
            self.converted += 1;
            self.detected.then(|| [&b"<"[..], s, b">"].concat())
        }
    }

    /// tests/basic/029.phpt: with translation, a two-byte character whose
    /// trail byte is a backslash does not escape the closing quote, and the
    /// name, file name and value are converted.
    #[test]
    fn translated_names_and_values() {
        let body =
            b"--XyZ\r\nContent-Disposition: form-data; name=\"\x97\\\x8e\\\x94\\\"\r\n\r\nv\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"d/x.txt\"\r\n\r\n1\r\n--XyZ--\r\n";
        let mut pos = 0;
        let mut read = |buf: &mut [u8]| {
            let n = buf.len().min(body.len() - pos);
            buf[..n].copy_from_slice(&body[pos..pos + n]);
            pos += n;
            n
        };
        let mut tr = FakeSjis {
            detected: false,
            converted: 0,
        };
        let o = parse_with_events(
            CT.as_bytes(),
            i64::try_from(body.len()).unwrap(),
            &tmp_settings(),
            &mut read,
            &mut |_, _| true,
            Some(&mut tr),
        );
        assert_eq!(o.registrations[0].name, b"<\x97\\\x8e\\\x94\\>");
        assert_eq!(o.registrations[0].value, RegValue::Str(b"<v>".to_vec()));
        assert_eq!(o.registrations[1].name, b"<f>[name]");
        assert_eq!(o.registrations[1].value, RegValue::Str(b"x.txt>".to_vec()));
        assert_eq!(
            o.registrations[2].value,
            RegValue::Str(b"<d/x.txt>".to_vec())
        );
        // name, value, then the file part's name and file name.
        assert_eq!(tr.converted, 4);
        cleanup(&o.uploaded);

        // Without translation the trail byte escapes the quote.
        let o = run(CT, body, &tmp_settings());
        assert_eq!(o.registrations[0].name, b"\x97\\\x8e\\\x94\"");
        cleanup(&o.uploaded);
    }

    #[test]
    fn multibyte_basename() {
        let two = |c: u8| -> usize {
            if c == 0x81 {
                2
            } else {
                1
            }
        };
        // The backslash is the trail byte of a two-byte character.
        assert_eq!(basename(b"x\x81\\y", &two), b"x\x81\\y");
        assert_eq!(basename(b"x\x81\\y", &single_byte), b"y");
        // A cut-off last character hides every separator.
        assert_eq!(basename(b"a/\x81", &two), b"a/\x81");
        assert_eq!(basename(b"a/b\x81\x81", &two), b"b\x81\x81");
        let (w, rest) = getword(b"\x81;x;;y", b';', &two);
        assert_eq!(w, b"\x81;x");
        assert_eq!(rest, b"y");
        assert_eq!(substring_conf(b"\x81\\\"z", b'"', &two), b"\x81\\");
    }

    /// Runs [`parse_with_events`], logging the events; `verdict` decides
    /// each event's result.
    fn run_events(
        body: &[u8],
        verdict: &dyn Fn(&Rfc1867Event<'_>) -> bool,
    ) -> (Outcome, Vec<String>) {
        let mut pos = 0;
        let mut read = |buf: &mut [u8]| {
            let n = buf.len().min(body.len() - pos);
            buf[..n].copy_from_slice(&body[pos..pos + n]);
            pos += n;
            n
        };
        let mut log = Vec::new();
        let o = parse_with_events(
            CT.as_bytes(),
            i64::try_from(body.len()).unwrap(),
            &tmp_settings(),
            &mut read,
            &mut |e, _| {
                let entry = match e {
                    Rfc1867Event::Start { content_length } => format!("start {content_length}"),
                    Rfc1867Event::FormData { name, value } => format!(
                        "formdata {}={}",
                        String::from_utf8_lossy(name),
                        String::from_utf8_lossy(value)
                    ),
                    Rfc1867Event::FileStart { name, filename } => format!(
                        "file_start {} {}",
                        String::from_utf8_lossy(name),
                        String::from_utf8_lossy(filename)
                    ),
                    Rfc1867Event::FileData { offset, length } => {
                        format!("file_data {offset} {length}")
                    }
                    Rfc1867Event::FileEnd {
                        temp_filename,
                        cancel_upload,
                    } => format!("file_end {} {cancel_upload}", temp_filename.is_some()),
                    Rfc1867Event::End => "end".to_string(),
                };
                log.push(entry);
                verdict(&e)
            },
            None,
        );
        (o, log)
    }

    const PROGRESS_BODY: &[u8] = b"--XyZ\r\nContent-Disposition: form-data; name=\"PHP_SESSION_UPLOAD_PROGRESS\"\r\n\r\ntest\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"t.txt\"\r\nContent-Type: text/plain\r\n\r\ncontents\r\n\
--XyZ--\r\n";

    #[test]
    fn upload_events_in_order() {
        let (o, log) = run_events(PROGRESS_BODY, &|_| true);
        assert_eq!(
            log,
            [
                format!("start {}", PROGRESS_BODY.len()),
                "formdata PHP_SESSION_UPLOAD_PROGRESS=test".to_string(),
                "file_start file t.txt".to_string(),
                "file_data 0 8".to_string(),
                "file_end true 0".to_string(),
                "end".to_string(),
            ]
        );
        assert_eq!(o.registrations[0].target, Target::Post);
        assert_eq!(
            o.registrations.last().map(|r| &r.value),
            Some(&RegValue::Long(8))
        );
        assert_eq!(o.uploaded.len(), 1);
        cleanup(&o.uploaded);
    }

    #[test]
    fn failed_events_skip_fields_and_cancel_uploads() {
        let (o, _) = run_events(PROGRESS_BODY, &|e| {
            !matches!(
                e,
                Rfc1867Event::FormData { .. } | Rfc1867Event::FileData { .. }
            )
        });
        // The field is not registered; the file reports UPLOAD_ERR_EXTENSION.
        assert!(o.registrations.iter().all(|r| r.target == Target::Files));
        let error = o
            .registrations
            .iter()
            .find(|r| r.name == b"file[error]")
            .map(|r| r.value.clone());
        assert_eq!(error, Some(RegValue::Long(upload_err::EXTENSION)));
        assert!(o.uploaded.is_empty());

        let (o, log) = run_events(PROGRESS_BODY, &|e| !matches!(e, Rfc1867Event::Start { .. }));
        assert!(o.registrations.is_empty());
        assert_eq!(log.len(), 2);
        assert_eq!(log[1], "end");
    }

    #[test]
    fn parse_without_an_observer_is_unchanged() {
        let (with_events, _) = run_events(PROGRESS_BODY, &|_| true);
        let plain = run(CT, PROGRESS_BODY, &tmp_settings());
        assert_eq!(names(&plain), names(&with_events));
        cleanup(&with_events.uploaded);
        cleanup(&plain.uploaded);
    }

    #[test]
    fn a_later_parse_counts_on_from_earlier_reads() {
        // request_parse_body() after a startup parse: SG(read_post_bytes)
        // already exceeds post_max_size, so fill_buffer() reads nothing and
        // fileupload_done warns (sapi_error(), no docref).
        let mut s = tmp_settings();
        s.post_max_size = 100;
        s.read_post_bytes = 150;
        let mut reads = 0;
        let mut read = |_: &mut [u8]| {
            reads += 1;
            0_usize
        };
        let mut seen = Vec::new();
        let o = parse_with_events(
            CT.as_bytes(),
            0,
            &s,
            &mut read,
            &mut |_, bytes| {
                seen.push(bytes);
                true
            },
            None,
        );
        assert_eq!(reads, 0);
        assert_eq!(o.read_bytes, 150);
        assert_eq!(seen, [150, 150], "start and end");
        assert_eq!(
            o.warnings,
            [Warning {
                message: "Actual POST length does not match Content-Length, and exceeds 100 bytes"
                    .to_string(),
                docref: false,
                notice: false,
            }]
        );

        // Below the limit the body is parsed, and the observer sees the
        // bytes of the whole request.
        let mut s = tmp_settings();
        s.read_post_bytes = 40;
        let mut pos = 0;
        let mut read = |buf: &mut [u8]| {
            let n = buf.len().min(PROGRESS_BODY.len() - pos);
            buf[..n].copy_from_slice(&PROGRESS_BODY[pos..pos + n]);
            pos += n;
            n
        };
        let mut last = 0;
        let o = parse_with_events(
            CT.as_bytes(),
            i64::try_from(PROGRESS_BODY.len()).unwrap(),
            &s,
            &mut read,
            &mut |_, bytes| {
                last = bytes;
                true
            },
            None,
        );
        assert!(o.warnings.is_empty(), "{:?}", o.warnings);
        assert_eq!(
            o.read_bytes,
            40 + i64::try_from(PROGRESS_BODY.len()).unwrap()
        );
        assert_eq!(last, o.read_bytes);
        assert_eq!(o.uploaded.len(), 1);
        cleanup(&o.uploaded);
    }
}
