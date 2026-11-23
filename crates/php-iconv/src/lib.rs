// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/iconv` (ref: `ext/iconv/iconv.c`, `iconv.stub.php`) on top of
//! the C library's `iconv(3)` (glibc).
//!
//! The PHP-visible functions are methods of [`Iconv`], which holds the
//! `iconv.*` INI values plus the core encoding settings they fall back to
//! (`internal_encoding`, `default_charset`, …).  Every method returns an
//! [`Outcome`]: the value (`None` = PHP `false`) and an optional
//! [`Diagnostic`] (notice / warning / deprecation / `ValueError` / fatal
//! memory error) the VM must raise, with the message text lacking only the
//! `"function(): "` prefix.
//!
//! Lower level building blocks are public too: [`iconv_string`]
//! (`php_iconv_string`), [`mime`] (encode/decode state machines), and
//! [`IconvFilter`] (the `convert.iconv.*` stream filter).

mod core;
mod error;
mod ffi;
mod filter;
pub mod mime;

pub use crate::core::{iconv_string, StringResult, ASCII, UCS4};
pub use crate::error::{charset_too_long, show_error, Diagnostic, IconvErr, CSNMAXLEN};
pub use crate::filter::IconvFilter;
pub use crate::mime::{Scheme, MIME_DECODE_CONTINUE_ON_ERROR, MIME_DECODE_STRICT};

/// `ICONV_IMPL` (`PHP_ICONV_IMPL` for glibc builds).
pub const ICONV_IMPL: &str = "glibc";

/// `ICONV_VERSION`: `gnu_get_libc_version()`.
pub fn iconv_version() -> String {
    ffi::glibc_version()
}

/// The constants registered by the extension, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstValue {
    /// A string constant.
    Str(String),
    /// An int constant.
    Int(i64),
}

/// `register_iconv_symbols()`.
pub fn constants() -> Vec<(&'static str, ConstValue)> {
    vec![
        ("ICONV_IMPL", ConstValue::Str(ICONV_IMPL.to_owned())),
        ("ICONV_VERSION", ConstValue::Str(iconv_version())),
        (
            "ICONV_MIME_DECODE_STRICT",
            ConstValue::Int(MIME_DECODE_STRICT),
        ),
        (
            "ICONV_MIME_DECODE_CONTINUE_ON_ERROR",
            ConstValue::Int(MIME_DECODE_CONTINUE_ON_ERROR),
        ),
    ]
}

/// The value/diagnostic pair every PHP-level operation produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<T> {
    /// The return value; `None` means PHP `false` (or nothing, when the
    /// diagnostic is a thrown error).
    pub value: Option<T>,
    /// A diagnostic to raise before returning.
    pub diagnostic: Option<Diagnostic>,
}

impl<T> Outcome<T> {
    fn ok(value: T) -> Self {
        Self {
            value: Some(value),
            diagnostic: None,
        }
    }

    fn fail(diagnostic: Diagnostic) -> Self {
        Self {
            value: None,
            diagnostic: Some(diagnostic),
        }
    }

    fn falsy() -> Self {
        Self {
            value: None,
            diagnostic: None,
        }
    }
}

/// The value of one header in `iconv_mime_decode_headers()`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    /// A header that occurred once.
    Single(Vec<u8>),
    /// A repeated header: packed list of values.
    Multiple(Vec<Vec<u8>>),
}

/// `iconv_get_encoding()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodingInfo {
    /// `"all"`: `input_encoding`, `output_encoding`, `internal_encoding`.
    All {
        input_encoding: Vec<u8>,
        output_encoding: Vec<u8>,
        internal_encoding: Vec<u8>,
    },
    /// A single encoding.
    Single(Vec<u8>),
}

/// Options of `iconv_mime_encode()` as extracted from the `$options`
/// array by the VM (only string-typed charsets are passed; `line-length`
/// already went through `zval_get_long`, `line-break-chars` through string
/// conversion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MimeEncodePrefs {
    /// `scheme` when it is a string (only its first byte matters).
    pub scheme: Option<Vec<u8>>,
    /// `input-charset` when it is a string.
    pub input_charset: Option<Vec<u8>>,
    /// `output-charset` when it is a string.
    pub output_charset: Option<Vec<u8>>,
    /// `line-length`.
    pub line_length: Option<i64>,
    /// `line-break-chars`.
    pub line_break_chars: Option<Vec<u8>>,
}

/// The `iconv.*` INI entries (empty = unset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IconvIni {
    /// `iconv.input_encoding`.
    pub input_encoding: Vec<u8>,
    /// `iconv.output_encoding`.
    pub output_encoding: Vec<u8>,
    /// `iconv.internal_encoding`.
    pub internal_encoding: Vec<u8>,
}

/// The core INI values used as fallbacks (`php_get_*_encoding()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreEncodings {
    /// `default_charset`.
    pub default_charset: Vec<u8>,
    /// `internal_encoding`.
    pub internal_encoding: Vec<u8>,
    /// `input_encoding`.
    pub input_encoding: Vec<u8>,
    /// `output_encoding`.
    pub output_encoding: Vec<u8>,
}

impl Default for CoreEncodings {
    fn default() -> Self {
        Self {
            default_charset: b"UTF-8".to_vec(),
            internal_encoding: Vec::new(),
            input_encoding: Vec::new(),
            output_encoding: Vec::new(),
        }
    }
}

impl CoreEncodings {
    fn pick<'a>(&'a self, specific: &'a [u8]) -> &'a [u8] {
        if !specific.is_empty() {
            specific
        } else if !self.default_charset.is_empty() {
            &self.default_charset
        } else {
            b"UTF-8"
        }
    }
}

/// Output-handler operation flags relevant to `ob_iconv_handler`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ObOp {
    /// `PHP_OUTPUT_HANDLER_START`.
    pub start: bool,
    /// `PHP_OUTPUT_HANDLER_CLEAN`.
    pub clean: bool,
    /// `PHP_OUTPUT_HANDLER_FINAL`.
    pub final_: bool,
    /// Output was already sent (`php_output_get_status() & PHP_OUTPUT_SENT`).
    pub output_sent: bool,
}

/// SAPI header state consulted by `ob_iconv_handler` on start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SapiMime {
    /// `SG(sapi_headers).mimetype`.
    pub mimetype: Option<Vec<u8>>,
    /// `SG(sapi_headers).send_default_content_type`.
    pub send_default_content_type: bool,
    /// `SG(default_mimetype)` (`None` → `"text/html"`).
    pub default_mimetype: Option<Vec<u8>>,
}

/// Result of one `ob_iconv_handler` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObResult {
    /// `false` → the handler returns `FAILURE`.
    pub success: bool,
    /// A `Content-Type:` header to add (`sapi_add_header(…, 0)`); when it
    /// is added successfully the VM clears `send_default_content_type` and
    /// marks the handler immutable.
    pub header: Option<Vec<u8>>,
    /// The converted output.
    pub output: Vec<u8>,
    /// Conversion diagnostic.
    pub diagnostic: Option<Diagnostic>,
}

/// The iconv module: INI state plus all PHP functions.
#[derive(Debug, Clone, Default)]
pub struct Iconv {
    /// `iconv.*` INI values.
    pub ini: IconvIni,
    /// Core fallbacks.
    pub core: CoreEncodings,
}

fn too_long(cs: &[u8]) -> bool {
    cs.len() >= CSNMAXLEN
}

impl Iconv {
    /// Fresh module state with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// `get_internal_encoding()`.
    pub fn internal_encoding(&self) -> &[u8] {
        if self.ini.internal_encoding.is_empty() {
            self.core.pick(&self.core.internal_encoding)
        } else {
            &self.ini.internal_encoding
        }
    }

    /// `get_input_encoding()`.
    pub fn input_encoding(&self) -> &[u8] {
        if self.ini.input_encoding.is_empty() {
            self.core.pick(&self.core.input_encoding)
        } else {
            &self.ini.input_encoding
        }
    }

    /// `get_output_encoding()`.
    pub fn output_encoding(&self) -> &[u8] {
        if self.ini.output_encoding.is_empty() {
            self.core.pick(&self.core.output_encoding)
        } else {
            &self.ini.output_encoding
        }
    }

    /// INI modification handler for `iconv.input_encoding`,
    /// `iconv.output_encoding` and `iconv.internal_encoding`
    /// (`OnUpdate*Encoding`).  `runtime` is true for the `ACTIVATE` and
    /// `RUNTIME` stages.  `Err(())` = the update is rejected (`FAILURE`);
    /// `Ok(Some(_))` carries the deprecation to raise.
    #[allow(clippy::result_unit_err)]
    pub fn ini_update(
        &mut self,
        name: &str,
        value: &[u8],
        runtime: bool,
    ) -> Result<Option<Diagnostic>, ()> {
        let slot = match name {
            "iconv.input_encoding" => &mut self.ini.input_encoding,
            "iconv.output_encoding" => &mut self.ini.output_encoding,
            "iconv.internal_encoding" => &mut self.ini.internal_encoding,
            _ => return Err(()),
        };
        if value.len() >= CSNMAXLEN {
            return Err(());
        }
        *slot = crate::error::c_prefix(value).to_vec();
        Ok(runtime.then(|| Diagnostic::Deprecated(format!("Use of {name} is deprecated"))))
    }

    /// `iconv(string $from_encoding, string $to_encoding, string $string): string|false`.
    pub fn iconv(&self, from: &[u8], to: &[u8], s: &[u8]) -> Outcome<Vec<u8>> {
        if too_long(from) || too_long(to) {
            return Outcome::fail(charset_too_long());
        }
        let r = iconv_string(s, to, from);
        match (r.err, r.out) {
            (None, Some(out)) => Outcome::ok(out),
            (Some(e), _) => Outcome::fail(show_error(e, to, from)),
            (None, None) => Outcome::falsy(),
        }
    }

    fn charset<'a>(&'a self, enc: Option<&'a [u8]>) -> Result<&'a [u8], Diagnostic> {
        match enc {
            None => Ok(self.internal_encoding()),
            Some(e) if too_long(e) => Err(charset_too_long()),
            Some(e) => Ok(e),
        }
    }

    /// `iconv_strlen(string $string, ?string $encoding = null): int|false`.
    pub fn strlen(&self, s: &[u8], encoding: Option<&[u8]>) -> Outcome<i64> {
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        match crate::core::strlen(s, cs) {
            Ok(n) => Outcome::ok(i64::try_from(n).unwrap_or(i64::MAX)),
            Err(e) => Outcome::fail(show_error(e, UCS4, cs)),
        }
    }

    /// `iconv_substr(string $string, int $offset, ?int $length = null, ?string $encoding = null): string|false`.
    pub fn substr(
        &self,
        s: &[u8],
        offset: i64,
        length: Option<i64>,
        encoding: Option<&[u8]>,
    ) -> Outcome<Vec<u8>> {
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        let length = length.unwrap_or_else(|| i64::try_from(s.len()).unwrap_or(i64::MAX));
        match crate::core::substr(s, offset, length, cs) {
            Ok(r) if r.allocated => Outcome::ok(r.into_vec()),
            Ok(_) => Outcome::falsy(),
            Err(e) => Outcome::fail(show_error(e, UCS4, cs)),
        }
    }

    /// `iconv_strpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
    pub fn strpos(
        &self,
        haystack: &[u8],
        needle: &[u8],
        offset: i64,
        encoding: Option<&[u8]>,
    ) -> Outcome<i64> {
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        let mut offset = offset;
        if offset < 0 {
            match crate::core::strlen(haystack, cs) {
                Ok(n) => offset = offset.wrapping_add(i64::try_from(n).unwrap_or(i64::MAX)),
                Err(e) => return Outcome::fail(show_error(e, UCS4, cs)),
            }
            if offset < 0 {
                return Outcome::fail(show_error(IconvErr::OutOfBounds, UCS4, cs));
            }
        }
        if needle.is_empty() {
            return Outcome::falsy();
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let off = offset as usize;
        Self::pos_outcome(crate::core::strpos(haystack, needle, off, cs, false), cs)
    }

    /// `iconv_strrpos(string $haystack, string $needle, ?string $encoding = null): int|false`.
    pub fn strrpos(&self, haystack: &[u8], needle: &[u8], encoding: Option<&[u8]>) -> Outcome<i64> {
        if needle.is_empty() {
            return Outcome::falsy();
        }
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        Self::pos_outcome(crate::core::strpos(haystack, needle, 0, cs, true), cs)
    }

    fn pos_outcome(r: Result<Option<usize>, IconvErr>, cs: &[u8]) -> Outcome<i64> {
        match r {
            Ok(Some(p)) => Outcome::ok(i64::try_from(p).unwrap_or(i64::MAX)),
            Ok(None) => Outcome::falsy(),
            Err(e) => Outcome::fail(show_error(e, UCS4, cs)),
        }
    }

    /// `iconv_mime_encode(string $field_name, string $field_value, array $options = []): string|false`.
    pub fn mime_encode(
        &self,
        field_name: &[u8],
        field_value: &[u8],
        prefs: &MimeEncodePrefs,
    ) -> Outcome<Vec<u8>> {
        let mut in_charset = self.internal_encoding();
        let mut out_charset = in_charset;
        let mut scheme = Scheme::Base64;
        if let Some(s) = prefs.scheme.as_deref() {
            match s.first() {
                Some(b'B' | b'b') => scheme = Scheme::Base64,
                Some(b'Q' | b'q') => scheme = Scheme::QPrint,
                _ => {}
            }
        }
        if let Some(cs) = prefs.input_charset.as_deref() {
            if too_long(cs) {
                return Outcome::fail(charset_too_long());
            }
            if !cs.is_empty() {
                in_charset = cs;
            }
        }
        if let Some(cs) = prefs.output_charset.as_deref() {
            if too_long(cs) {
                return Outcome::fail(charset_too_long());
            }
            if !cs.is_empty() {
                out_charset = cs;
            }
        }
        let line_len = prefs.line_length.unwrap_or(76);
        let lfchars = prefs.line_break_chars.as_deref().unwrap_or(b"\r\n");
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let max_line_len = line_len as u64 as usize;
        // safe_femalloc(1, max_line_len, 5) overflow check.
        if max_line_len.checked_add(5).is_none() {
            return Outcome::fail(Diagnostic::Fatal(format!(
                "Possible integer overflow in memory allocation (1 * {max_line_len} + 5)"
            )));
        }
        match mime::mime_encode(
            field_name,
            field_value,
            max_line_len,
            lfchars,
            scheme,
            out_charset,
            in_charset,
        ) {
            Ok(v) => Outcome::ok(v),
            Err(e) => Outcome::fail(show_error(e, out_charset, in_charset)),
        }
    }

    /// `iconv_mime_decode(string $string, int $mode = 0, ?string $encoding = null): string|false`.
    pub fn mime_decode(&self, s: &[u8], mode: i64, encoding: Option<&[u8]>) -> Outcome<Vec<u8>> {
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        match mime::mime_decode(s, cs, mode) {
            Ok(d) => Outcome::ok(d.text.into_vec()),
            Err(e) => Outcome::fail(show_error(e, cs, b"???")),
        }
    }

    /// `iconv_mime_decode_headers(string $headers, int $mode = 0, ?string $encoding = null): array|false`.
    ///
    /// Header names that are canonical decimal integers become integer
    /// keys in PHP; since PHP looks them up with a non-symtable lookup
    /// (which never finds them), repeated numeric headers overwrite instead
    /// of accumulating — this is reproduced here.
    pub fn mime_decode_headers(
        &self,
        s: &[u8],
        mode: i64,
        encoding: Option<&[u8]>,
    ) -> Outcome<Vec<(Vec<u8>, HeaderValue)>> {
        let cs = match self.charset(encoding) {
            Ok(cs) => cs,
            Err(d) => return Outcome::fail(d),
        };
        let mut out: Vec<(Vec<u8>, HeaderValue)> = Vec::new();
        let mut rest = s;
        // `while (enc_str_len_tmp > 0 && !ZEND_HAS_OOM())`
        while !rest.is_empty() && !crate::core::oom_pending() {
            let decoded = match mime::mime_decode(rest, cs, mode) {
                Ok(d) => d,
                Err(e) => return Outcome::fail(show_error(e, cs, b"???")),
            };
            if !decoded.text.allocated {
                break;
            }
            let text = decoded.text.buf;
            if let Some(colon) = text.iter().position(|&b| b == b':') {
                let name = text[..colon].to_vec();
                let mut v = colon + 1;
                while v < text.len() && (text[v] == b' ' || text[v] == b'\t') {
                    v += 1;
                }
                let value = text[v..].to_vec();
                insert_header(&mut out, name, value);
            }
            // PHP always makes progress here (next_pos >= 1); guard anyway.
            let advance = decoded.next_pos.clamp(1, rest.len());
            rest = &rest[advance..];
        }
        Outcome::ok(out)
    }

    /// `iconv_set_encoding(string $type, string $encoding): bool`.
    pub fn set_encoding(&mut self, kind: &[u8], charset: &[u8]) -> Outcome<bool> {
        if too_long(charset) {
            return Outcome::fail(charset_too_long());
        }
        let name = if kind.eq_ignore_ascii_case(b"input_encoding") {
            "iconv.input_encoding"
        } else if kind.eq_ignore_ascii_case(b"output_encoding") {
            "iconv.output_encoding"
        } else if kind.eq_ignore_ascii_case(b"internal_encoding") {
            "iconv.internal_encoding"
        } else {
            return Outcome::falsy();
        };
        match self.ini_update(name, charset, true) {
            Ok(diagnostic) => Outcome {
                value: Some(true),
                diagnostic,
            },
            Err(()) => Outcome::ok(false),
        }
    }

    /// `iconv_get_encoding(string $type = "all"): array|string|false`.
    pub fn get_encoding(&self, kind: Option<&[u8]>) -> Outcome<EncodingInfo> {
        let kind = kind.unwrap_or(b"all");
        if kind.eq_ignore_ascii_case(b"all") {
            Outcome::ok(EncodingInfo::All {
                input_encoding: self.input_encoding().to_vec(),
                output_encoding: self.output_encoding().to_vec(),
                internal_encoding: self.internal_encoding().to_vec(),
            })
        } else if kind.eq_ignore_ascii_case(b"input_encoding") {
            Outcome::ok(EncodingInfo::Single(self.input_encoding().to_vec()))
        } else if kind.eq_ignore_ascii_case(b"output_encoding") {
            Outcome::ok(EncodingInfo::Single(self.output_encoding().to_vec()))
        } else if kind.eq_ignore_ascii_case(b"internal_encoding") {
            Outcome::ok(EncodingInfo::Single(self.internal_encoding().to_vec()))
        } else {
            Outcome::falsy()
        }
    }

    /// `php_iconv_output_conflict()`: `true` when `ob_iconv_handler` may
    /// not be started because `ob_iconv_handler` or `mb_output_handler` is
    /// already active (`level > 0` and one of them is in the stack).
    pub fn ob_conflict(output_level: usize, active_handlers: &[&str]) -> bool {
        output_level > 0
            && active_handlers
                .iter()
                .any(|h| *h == "ob_iconv_handler" || *h == "mb_output_handler")
    }

    /// `php_iconv_output_handler()`.
    pub fn ob_handler(&self, data: &[u8], op: ObOp, sapi: &SapiMime) -> ObResult {
        let mut header = None;
        if op.start {
            if op.output_sent {
                return ObResult {
                    success: false,
                    header: None,
                    output: Vec::new(),
                    diagnostic: None,
                };
            }
            let mut mimetype: Option<&[u8]> = None;
            if let Some(m) = sapi.mimetype.as_deref() {
                if m.len() >= 5 && m[..5].eq_ignore_ascii_case(b"text/") {
                    let end = m.iter().position(|&b| b == b';').unwrap_or(m.len());
                    mimetype = Some(&m[..end]);
                }
            }
            if mimetype.is_none() && sapi.send_default_content_type {
                mimetype = Some(sapi.default_mimetype.as_deref().unwrap_or(b"text/html"));
            }
            if let Some(m) = mimetype {
                if !op.clean || (op.start && !op.final_) {
                    let enc = self.output_encoding();
                    let charset = find(enc, b"//").map_or(enc, |p| &enc[..p]);
                    let mut h = b"Content-Type:".to_vec();
                    h.extend_from_slice(m);
                    h.extend_from_slice(b"; charset=");
                    h.extend_from_slice(charset);
                    header = Some(h);
                }
            }
        }
        let mut output = Vec::new();
        let mut diagnostic = None;
        if !data.is_empty() {
            let out_cs = self.output_encoding();
            let in_cs = self.internal_encoding();
            let r = iconv_string(data, out_cs, in_cs);
            if let Some(e) = r.err {
                diagnostic = Some(show_error(e, out_cs, in_cs));
            }
            output = r.out.unwrap_or_default();
        }
        ObResult {
            success: true,
            header,
            output,
            diagnostic,
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Whether PHP's symtable would turn `key` into an integer key.
fn is_numeric_key(key: &[u8]) -> bool {
    let digits = key.strip_prefix(b"-").unwrap_or(key);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return false;
    }
    if key == b"-0" {
        return false;
    }
    std::str::from_utf8(key)
        .ok()
        .and_then(|k| k.parse::<i64>().ok())
        .is_some()
}

fn insert_header(out: &mut Vec<(Vec<u8>, HeaderValue)>, name: Vec<u8>, value: Vec<u8>) {
    let numeric = is_numeric_key(&name);
    let existing = out.iter_mut().find(|(k, _)| *k == name);
    match existing {
        Some((_, slot)) if numeric => *slot = HeaderValue::Single(value),
        Some((_, slot)) => match slot {
            HeaderValue::Single(prev) => {
                let prev = std::mem::take(prev);
                *slot = HeaderValue::Multiple(vec![prev, value]);
            }
            HeaderValue::Multiple(list) => list.push(value),
        },
        None => out.push((name, HeaderValue::Single(value))),
    }
}

#[cfg(test)]
mod tests;
