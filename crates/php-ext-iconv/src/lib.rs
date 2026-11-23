// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/iconv` for the phprust VM: binds the `php-iconv` library through
//! the `php_vm::ext` extension API (ref: `ext/iconv/iconv.c`).
//!
//! * every `iconv*()` function, with the library's [`Diagnostic`]s raised
//!   as `func(): `-prefixed notices/warnings/deprecations, `ValueError`s or
//!   fatal errors (including the modelled `memory_limit` exhaustion);
//! * the `ICONV_*` constants;
//! * the `iconv.*` INI entries with their on-modify checks and
//!   deprecations ([`ini`]);
//! * `ob_iconv_handler` — exposed as a callable function because the VM
//!   resolves `ob_start()` handlers through the function table (PHP has an
//!   output-handler alias instead);
//! * the `convert.iconv.*` stream filter ([`filter`]).

// Native handlers share one signature (`fn(&mut Vm, &mut Args) ->
// VmResult<Value>`), so infallible ones still return `VmResult`.
#![allow(clippy::unnecessary_wraps)]

mod filter;
mod ini;

use php_iconv::{
    ConstValue, Diagnostic, EncodingInfo, HeaderValue, Iconv, MimeEncodePrefs, ObOp, Outcome,
    SapiMime,
};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_vm::ext::{Args, Extension, ModuleBuilder, VmResult};
use php_vm::Vm;

/// The `iconv` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct IconvExt;

/// `PHP_OUTPUT_HANDLER_START`.
const OUTPUT_START: i64 = 0x01;
/// `PHP_OUTPUT_HANDLER_CLEAN`.
const OUTPUT_CLEAN: i64 = 0x02;
/// `PHP_OUTPUT_HANDLER_FINAL`.
const OUTPUT_FINAL: i64 = 0x08;

/// The running function's name (for `ValueError` prefixes).
fn fname(a: &Args) -> String {
    a.func
        .as_ref()
        .map_or_else(String::new, |f| f.name.to_string_lossy())
}

/// A PHP string from bytes.
fn bytes(b: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(b.as_ref()))
}

/// What C sees of a string: everything before the first NUL.
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |n| &s[..n])
}

/// Raises a library diagnostic (`Ok` for notices/warnings/deprecations,
/// `Err` for thrown or fatal errors).
fn raise(vm: &mut Vm, a: &Args, d: Diagnostic) -> VmResult<()> {
    match d {
        Diagnostic::Warning(m) => vm.php_warning(&m),
        Diagnostic::Notice(m) => vm.php_notice(&m),
        Diagnostic::Deprecated(m) => vm.php_deprecated(&m),
        Diagnostic::ValueError(m) => Err(vm.throw("ValueError", &format!("{}(): {m}", fname(a)))),
        // A fallible allocation recorded the violation: the VM raises the
        // delayed fatal right after this call.
        Diagnostic::OutOfMemory { .. } if php_memlimit::is_pending() => Ok(()),
        Diagnostic::OutOfMemory { requested } => Err(vm.out_of_memory(requested)),
        Diagnostic::Fatal(m) => Err(vm.fatal(&m)),
    }
}

/// Raises the outcome's diagnostic, then maps its value (`None` → `false`).
fn finish<T>(
    vm: &mut Vm,
    a: &Args,
    out: Outcome<T>,
    ok: impl FnOnce(T) -> Value,
) -> VmResult<Value> {
    // `RETURN_OOM()`: an allocation crossed `memory_limit` and the library
    // stopped early (`ZEND_HAS_OOM()`); the VM raises the delayed fatal
    // right after this call, so the partial result is dropped unbuilt.
    if php_memlimit::is_pending() {
        return Ok(Value::Null);
    }
    if let Some(d) = out.diagnostic {
        raise(vm, a, d)?;
    }
    Ok(out.value.map_or(Value::Bool(false), ok))
}

/// A `?string` argument as bytes.
fn opt_str(a: &Args, n: usize) -> Option<ZStr> {
    match a.at(n) {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

fn int_value(n: i64) -> Value {
    Value::Int(n)
}

fn iconv_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (from, to, s) = (a.str(0), a.str(1), a.str(2));
    let st = ini::state(vm);
    let out = st.iconv(from.as_bytes(), to.as_bytes(), s.as_bytes());
    finish(vm, a, out, bytes)
}

fn iconv_strlen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let enc = opt_str(a, 1);
    let st = ini::state(vm);
    let out = st.strlen(s.as_bytes(), enc.as_ref().map(ZStr::as_bytes));
    finish(vm, a, out, int_value)
}

fn iconv_substr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let offset = a.int(1);
    let length = match a.at(2) {
        Value::Null => None,
        _ => Some(a.int(2)),
    };
    let enc = opt_str(a, 3);
    let st = ini::state(vm);
    let out = st.substr(
        s.as_bytes(),
        offset,
        length,
        enc.as_ref().map(ZStr::as_bytes),
    );
    // PHP returns "" (not false) when the substring is empty but valid;
    // the library reports that as an allocated empty result.
    finish(vm, a, out, bytes)
}

fn iconv_strpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (h, n) = (a.str(0), a.str(1));
    let offset = a.int(2);
    let enc = opt_str(a, 3);
    let st = ini::state(vm);
    let out = st.strpos(
        h.as_bytes(),
        n.as_bytes(),
        offset,
        enc.as_ref().map(ZStr::as_bytes),
    );
    finish(vm, a, out, int_value)
}

fn iconv_strrpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (h, n) = (a.str(0), a.str(1));
    let enc = opt_str(a, 2);
    let st = ini::state(vm);
    let out = st.strrpos(h.as_bytes(), n.as_bytes(), enc.as_ref().map(ZStr::as_bytes));
    finish(vm, a, out, int_value)
}

/// Extracts `iconv_mime_encode()`'s `$options` (`zend_hash_find_deref`
/// lookups by exact key).
fn mime_prefs(vm: &mut Vm, opts: &ZArray) -> VmResult<MimeEncodePrefs> {
    let find = |k: &str| opts.get_str(k.as_bytes()).map(Value::deref);
    let string = |v: Option<Value>| match v {
        Some(Value::Str(s)) => Some(s.as_bytes().to_vec()),
        _ => None,
    };
    let mut prefs = MimeEncodePrefs {
        scheme: string(find("scheme")).filter(|s| !s.is_empty()),
        input_charset: string(find("input-charset")),
        output_charset: string(find("output-charset")),
        ..MimeEncodePrefs::default()
    };
    if let Some(v) = find("line-length") {
        prefs.line_length = Some(vm.value_to_int(&v)?);
    }
    if let Some(v) = find("line-break-chars") {
        let s = match v {
            Value::Str(s) => s,
            other => vm.value_to_string(&other)?,
        };
        prefs.line_break_chars = Some(c_str(s.as_bytes()).to_vec());
    }
    Ok(prefs)
}

fn iconv_mime_encode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (name, value) = (a.str(0), a.str(1));
    let prefs = match a.at(2) {
        Value::Array(opts) => mime_prefs(vm, &opts)?,
        _ => MimeEncodePrefs::default(),
    };
    let st = ini::state(vm);
    let out = st.mime_encode(name.as_bytes(), value.as_bytes(), &prefs);
    finish(vm, a, out, bytes)
}

fn iconv_mime_decode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let mode = a.int(1);
    let enc = opt_str(a, 2);
    let st = ini::state(vm);
    let out = st.mime_decode(s.as_bytes(), mode, enc.as_ref().map(ZStr::as_bytes));
    finish(vm, a, out, bytes)
}

fn iconv_mime_decode_headers(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let mode = a.int(1);
    let enc = opt_str(a, 2);
    let st = ini::state(vm);
    let out = st.mime_decode_headers(s.as_bytes(), mode, enc.as_ref().map(ZStr::as_bytes));
    finish(vm, a, out, |headers| {
        let mut arr = ZArray::with_capacity(headers.len());
        let t = arr.make_mut();
        for (name, value) in headers {
            let v = match value {
                HeaderValue::Single(v) => bytes(v),
                HeaderValue::Multiple(list) => {
                    Value::Array(ZArray::from_values(list.into_iter().map(bytes)))
                }
            };
            t.insert(ArrayKey::from_bytes(&name), v);
        }
        Value::Array(arr)
    })
}

fn iconv_set_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (kind, charset) = (a.str(0), a.str(1));
    let mut st = ini::state(vm);
    let out = st.set_encoding(kind.as_bytes(), charset.as_bytes());
    if let Some(d) = out.diagnostic {
        raise(vm, a, d)?;
    }
    let ok = out.value.unwrap_or(false);
    if ok {
        // zend_alter_ini_entry(): the handler accepted the value, store it.
        let name = if kind.as_bytes().eq_ignore_ascii_case(b"input_encoding") {
            "iconv.input_encoding"
        } else if kind.as_bytes().eq_ignore_ascii_case(b"output_encoding") {
            "iconv.output_encoding"
        } else {
            "iconv.internal_encoding"
        };
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        // The registry has no handler for these entries, so storing
        // cannot fail.
        let _ = vm.ini.ini_set(name, charset.as_bytes(), &mut sink);
        ini::mark_modified(name);
    }
    Ok(Value::Bool(ok))
}

fn iconv_get_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let kind = a.str(0);
    let st = ini::state(vm);
    let out = st.get_encoding(Some(kind.as_bytes()));
    finish(vm, a, out, |info| match info {
        EncodingInfo::All {
            input_encoding,
            output_encoding,
            internal_encoding,
        } => {
            let mut arr = ZArray::with_capacity(3);
            let t = arr.make_mut();
            t.insert(ArrayKey::from("input_encoding"), bytes(input_encoding));
            t.insert(ArrayKey::from("output_encoding"), bytes(output_encoding));
            t.insert(
                ArrayKey::from("internal_encoding"),
                bytes(internal_encoding),
            );
            Value::Array(arr)
        }
        EncodingInfo::Single(e) => bytes(e),
    })
}

/// `ob_iconv_handler` (`php_iconv_output_handler`), called by the output
/// layer as `handler(string $contents, int $status)`.
fn ob_iconv_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (data, status) = (a.str(0), a.int(1));
    let st: Iconv = ini::state(vm);
    let op = ObOp {
        start: status & OUTPUT_START != 0,
        clean: status & OUTPUT_CLEAN != 0,
        final_: status & OUTPUT_FINAL != 0,
        // `php_output_get_status() & PHP_OUTPUT_SENT`.
        output_sent: vm.output.status() & php_support::output::status::SENT != 0,
    };
    // `SG(sapi_headers).mimetype` / `send_default_content_type` /
    // `SG(default_mimetype)`.
    let sapi = SapiMime {
        mimetype: vm.response_mimetype(),
        send_default_content_type: vm.sends_default_content_type(),
        default_mimetype: vm.ini_string("default_mimetype").filter(|m| !m.is_empty()),
    };
    let r = st.ob_handler(data.as_bytes(), op, &sapi);
    // `sapi_add_header(content_type, len, 0)`: on success the default
    // Content-Type is dropped and the handler becomes immutable.
    if let Some(h) = &r.header {
        if vm.add_response_header(h, true)? {
            vm.clear_default_content_type();
            vm.output.set_running_immutable();
        }
    }
    if let Some(d) = r.diagnostic {
        raise(vm, a, d)?;
    }
    if !r.success {
        return Ok(Value::Bool(false));
    }
    Ok(bytes(r.output))
}

/// `RINIT`.
fn request_startup(_vm: &mut Vm) {
    ini::reset();
}

impl Extension for IconvExt {
    fn name(&self) -> &'static str {
        "iconv"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for (name, value) in php_iconv::constants() {
            let v = match value {
                ConstValue::Str(s) => Value::string(s),
                ConstValue::Int(n) => Value::Int(n),
            };
            m.constant(name, v);
        }
        ini::register(m);
        filter::register();
        m.on_request_startup(request_startup);
        m.function_static(
            "iconv_strlen(string $string, ?string $encoding = null): int|false",
            iconv_strlen,
        )
        .function_static(
            "iconv_substr(string $string, int $offset, ?int $length = null, ?string $encoding = null): string|false",
            iconv_substr,
        )
        .function_static(
            "iconv_strpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false",
            iconv_strpos,
        )
        .function_static(
            "iconv_strrpos(string $haystack, string $needle, ?string $encoding = null): int|false",
            iconv_strrpos,
        )
        .function_static(
            "iconv_mime_encode(string $field_name, string $field_value, array $options = []): string|false",
            iconv_mime_encode,
        )
        .function_static(
            "iconv_mime_decode(string $string, int $mode = 0, ?string $encoding = null): string|false",
            iconv_mime_decode,
        )
        .function_static(
            "iconv_mime_decode_headers(string $headers, int $mode = 0, ?string $encoding = null): array|false",
            iconv_mime_decode_headers,
        )
        .function_static(
            "iconv(string $from_encoding, string $to_encoding, string $string): string|false",
            iconv_fn,
        )
        .function_static(
            "iconv_set_encoding(string $type, string $encoding): bool",
            iconv_set_encoding,
        )
        .function_static(
            "iconv_get_encoding(string $type = \"all\"): array|string|false",
            iconv_get_encoding,
        )
        .function_static(
            "ob_iconv_handler(string $contents, int $status): string|false",
            ob_iconv_handler,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_string_prefix() {
        assert_eq!(c_str(b"ab\0cd"), b"ab");
        assert_eq!(c_str(b"abc"), b"abc");
    }

    #[test]
    fn output_flags() {
        assert_eq!(OUTPUT_START | OUTPUT_CLEAN | OUTPUT_FINAL, 11);
    }
}
