// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mb_parse_str()` (`_php_mb_encoding_handler_ex` in `mb_gpc.c`), the
//! request input translation of `mbstring.encoding_translation`, and the
//! multipart parser's multibyte callbacks (`php_mb_rfc1867_*` in
//! `mbstring.c`).

use php_mbstring::convert::{effective_pair, fast_convert};
use php_mbstring::encoding::{ENCTYPE_WCS2, ENCTYPE_WCS4};
use php_mbstring::{encoding, ConvertBuf, Encoding, EncodingNo, ErrorMode};
use php_runtime::{Value, ZArray, ZStr};
use php_stdmisc::query::{register_variable_ex, url_decode, RegisterOptions};
use php_vm::ext::{Args, ModuleBuilder, RequestInput, Rfc1867Translation, VmResult};
use php_vm::Vm;

use crate::state;

/// `E_WARNING`.
const E_WARNING: i64 = 2;

/// `php_strtok_r` over a separator set: the non-empty tokens.
fn tokens<'a>(s: &'a [u8], seps: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    s.split(move |c| seps.contains(c)).filter(|t| !t.is_empty())
}

fn mb_parse_str(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let input = a.str(0);
    // The C code works on a NUL-terminated copy.
    let res = input
        .as_bytes()
        .split(|&b| b == 0)
        .next()
        .unwrap_or_default()
        .to_vec();
    let seps = vm
        .ini_string("arg_separator.input")
        .unwrap_or_else(|| b"&".to_vec());
    let mut track = ZArray::new();
    let detected = if res.is_empty() {
        None
    } else {
        let h = Handler {
            separators: &seps,
            report_errors: true,
            cookie: false,
            prefix: "mb_parse_str(): ",
        };
        encoding_handler(vm, &h, &res, &mut track)?
    };
    state::with(|s| s.http_input_identify = detected);
    a.set_ref(1, Value::Array(track));
    Ok(Value::Bool(detected.is_some()))
}

/// `php_mb_encoding_handler_info_t`: how one input is split and reported.
struct Handler<'a> {
    separators: &'a [u8],
    /// "Unable to detect encoding" is reported (`mb_parse_str()` only).
    report_errors: bool,
    /// Cookie name mangling of `php_register_variable_ex`.
    cookie: bool,
    /// Warning prefix (`"mb_parse_str(): "`, `"PHP Request Startup: "`).
    prefix: &'a str,
}

/// `_php_mb_encoding_handler_ex()`: splits `res`, url-decodes, detects the
/// source encoding among `http_input_list`, converts to the internal
/// encoding and registers every pair into `track`; returns the encoding
/// used (`None` when nothing was registered).
fn encoding_handler(
    vm: &mut Vm,
    h: &Handler<'_>,
    res: &[u8],
    track: &mut ZArray,
) -> VmResult<Option<&'static Encoding>> {
    let max_vars = if vm.ini_string("max_input_vars").is_some() {
        vm.ini_long("max_input_vars")
    } else {
        1000
    };
    let nesting = if vm.ini_string("max_input_nesting_level").is_some() {
        vm.ini_long("max_input_nesting_level")
    } else {
        64
    };
    // val_list + len_list of the C implementation.
    let seps_count = res.iter().filter(|b| h.separators.contains(b)).count();
    crate::mem::check((seps_count + 1) * 2 * 16)?;
    // Split and url-decode into (name, value) pairs.
    let mut list: Vec<Vec<u8>> = Vec::new();
    for var in tokens(res, h.separators) {
        if let Some(i) = var.iter().position(|&b| b == b'=') {
            list.push(url_decode(&var[..i], false));
            list.push(url_decode(&var[i + 1..], false));
        } else {
            list.push(url_decode(var, false));
            list.push(Vec::new());
        }
    }
    if i64::try_from(list.len()).unwrap_or(i64::MAX) > max_vars.saturating_mul(2) {
        vm.raise(
            E_WARNING,
            &format!(
                "{}Input variables exceeded {max_vars}. To increase the limit change max_input_vars in php.ini.",
                h.prefix
            ),
        )?;
        return Ok(None);
    }
    register_all(vm, h, &list, track, nesting)
}

/// `mbstr_treat_data()` and `php_mb_post_handler()` with
/// `mbstring.encoding_translation`: the request input converted from the
/// detected HTTP input encoding; `None` leaves it to the default parser.
fn treat_data(
    vm: &mut Vm,
    kind: RequestInput,
    data: &[u8],
    track: &mut ZArray,
) -> Option<VmResult<()>> {
    if !vm.ini_bool("mbstring.encoding_translation") {
        return None;
    }
    let res = data.split(|&b| b == 0).next().unwrap_or_default();
    if res.is_empty() {
        return Some(Ok(()));
    }
    state::with(|s| match kind {
        RequestInput::Get => s.http_input_identify_get = None,
        RequestInput::Post => s.http_input_identify_post = None,
        RequestInput::Cookie => s.http_input_identify_cookie = None,
    });
    let seps = if kind == RequestInput::Cookie {
        b";".to_vec()
    } else {
        vm.ini_string("arg_separator.input")
            .unwrap_or_else(|| b"&".to_vec())
    };
    state::with(|s| s.illegalchars = 0);
    let h = Handler {
        separators: &seps,
        report_errors: false,
        cookie: kind == RequestInput::Cookie,
        prefix: "PHP Request Startup: ",
    };
    let detected = match encoding_handler(vm, &h, res, track) {
        Ok(d) => d,
        Err(t) => return Some(Err(t)),
    };
    state::with(|s| {
        s.http_input_identify = detected;
        if let Some(d) = detected {
            match kind {
                RequestInput::Get => s.http_input_identify_get = Some(d),
                RequestInput::Post => s.http_input_identify_post = Some(d),
                RequestInput::Cookie => s.http_input_identify_cookie = Some(d),
            }
        }
    });
    Some(Ok(()))
}

/// Detects the source encoding, converts and registers every pair;
/// returns the encoding used.
fn register_all(
    vm: &mut Vm,
    h: &Handler<'_>,
    list: &[Vec<u8>],
    track: &mut ZArray,
    nesting: i64,
) -> VmResult<Option<&'static Encoding>> {
    let (from, to, sub, mode, strict) = state::with(|s| {
        (
            s.http_input_list().to_vec(),
            s.internal_encoding(),
            s.illegal_substchar(),
            s.illegal_mode(),
            s.strict_detection,
        )
    });
    let pass = encoding::pass();
    let from_enc = match from.len() {
        0 => pass,
        1 => from[0],
        _ => {
            let strs: Vec<&[u8]> = list.iter().map(Vec::as_slice).collect();
            if let Some(e) =
                php_mbstring::detect::guess_encoding_for_strings(&strs, &from, strict, false)
            {
                e
            } else {
                if h.report_errors {
                    vm.php_warning("Unable to detect encoding")?;
                }
                pass
            }
        }
    };
    let opts = RegisterOptions {
        max_nesting_level: nesting,
        display_errors: vm.ini_bool("display_errors"),
        cookie: h.cookie,
        warning_prefix: h.prefix,
    };
    let convert = from_enc.no != EncodingNo::Pass && to.no != EncodingNo::Pass;
    for pair in list.chunks(2) {
        let (name, val) = if convert {
            let (n, e1) = fast_convert(&pair[0], from_enc, to, sub, mode);
            let (v, e2) = fast_convert(&pair[1], from_enc, to, sub, mode);
            state::with(|s| s.illegalchars += u64::from(e1) + u64::from(e2));
            (n, v)
        } else {
            (pair[0].clone(), pair[1].clone())
        };
        register_variable_ex(&name, Value::Str(ZStr::new(val)), track, &opts, vm)
            .map_err(|e| state::rt_throw(vm, e))?;
        if crate::mem::pending() {
            return Ok(None);
        }
    }
    Ok(Some(from_enc))
}

/// `php_mb_mbchar_bytes()`: the byte length of the character of `enc`
/// starting with `lead` (1 without an encoding or a length table).
fn mbchar_bytes(enc: Option<&'static Encoding>, lead: u8) -> usize {
    let Some(enc) = enc else {
        return 1;
    };
    if let Some(table) = enc.mblen_table {
        // A zero entry would stall the callers' scans.
        usize::from(table[usize::from(lead)]).max(1)
    } else if enc.flag & ENCTYPE_WCS2 != 0 {
        2
    } else if enc.flag & ENCTYPE_WCS4 != 0 {
        4
    } else {
        1
    }
}

/// `mb_fast_convert()` that stops once a `memory_limit` violation is
/// pending: the multipart parser then ends the request with it, so the
/// rest of the output would only be discarded (and could reach the hard
/// limit first).
fn convert_until_oom(
    input: &[u8],
    from: &'static Encoding,
    to: &'static Encoding,
    sub: u32,
    mode: ErrorMode,
) -> Vec<u8> {
    let (from, to) = effective_pair(from, to);
    let mut wchar_buf = [0u32; 128];
    let mut state = 0u32;
    let mut buf = ConvertBuf::new(input.len(), sub, mode);
    let mut rest = input;
    while !rest.is_empty() && !crate::mem::pending() {
        let n = (from.to_wchar)(&mut rest, &mut wchar_buf, &mut state);
        (to.from_wchar)(&wchar_buf[..n], &mut buf, rest.is_empty());
    }
    buf.into_bytes()
}

/// mbstring's multibyte callbacks of the multipart parser for one request
/// (`php_rfc1867_set_multibyte_callbacks()` in `PHP_MINIT(mbstring)`).
struct MultipartTranslation {
    /// `MBSTRG(http_input_list)` (`php_mb_gpc_get_detect_order()`), or the
    /// current detect order when it is empty
    /// (`php_mb_zend_encoding_detector()` with a `NULL` list).
    detect_order: Vec<&'static Encoding>,
    /// `zend_multibyte_get_internal_encoding()`.
    internal: &'static Encoding,
    /// `MBSTRG(current_filter_illegal_substchar)`.
    sub: u32,
    /// `MBSTRG(current_filter_illegal_mode)`.
    mode: ErrorMode,
    /// `mbuff->input_encoding`.
    input: Option<&'static Encoding>,
}

impl Rfc1867Translation for MultipartTranslation {
    fn detect(&mut self, line: &[u8]) {
        // php_mb_zend_encoding_detector(): a lone "pass" is never returned.
        self.input = match self.detect_order.as_slice() {
            [only] if only.no == EncodingNo::Pass => None,
            order => php_mbstring::detect::guess_encoding_for_strings(&[line], order, false, false),
        };
    }

    fn input_char_len(&self, lead: u8) -> usize {
        mbchar_bytes(self.input, lead)
    }

    fn internal_char_len(&self, lead: u8) -> usize {
        mbchar_bytes(Some(self.internal), lead)
    }

    fn convert(&mut self, s: &[u8]) -> Option<Vec<u8>> {
        let from = self.input?;
        let converted = convert_until_oom(s, from, self.internal, self.sub, self.mode);
        if crate::mem::pending() {
            return Some(converted);
        }
        // php_mb_zend_encoding_converter() hands back a copy of
        // mb_fast_convert()'s result (emalloc + memcpy): both are live at
        // once, which is what pushes a part past memory_limit. Once a
        // violation is pending the request fails anyway, and the copy
        // could only reach the hard limit.
        let copy = converted.as_slice().to_vec();
        drop(converted);
        Some(copy)
    }
}

/// `php_rfc1867_encoding_translation()`: mbstring's multibyte callbacks
/// for this request's multipart body, with `mbstring.encoding_translation`.
fn multipart_translation(vm: &mut Vm) -> Option<Box<dyn Rfc1867Translation>> {
    if !vm.ini_bool("mbstring.encoding_translation") {
        return None;
    }
    let t = state::with(|s| {
        let detect_order = if s.http_input_list().is_empty() {
            s.detect_order().to_vec()
        } else {
            s.http_input_list().to_vec()
        };
        MultipartTranslation {
            detect_order,
            internal: s.internal_encoding(),
            sub: s.illegal_substchar(),
            mode: s.illegal_mode(),
            input: None,
        }
    });
    Some(Box::new(t))
}

/// Registers `mb_parse_str`.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("mb_parse_str(string $string, &$result): bool", mb_parse_str);
    php_vm::ext::set_request_input_hook(treat_data);
    php_vm::ext::set_rfc1867_translation_hook(multipart_translation);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(name: &str) -> &'static Encoding {
        encoding::by_name(name.as_bytes()).unwrap()
    }

    fn translation(order: &[&str], internal: &str) -> MultipartTranslation {
        MultipartTranslation {
            detect_order: order.iter().map(|n| enc(n)).collect(),
            internal: enc(internal),
            sub: 0x3f,
            mode: ErrorMode::Char,
            input: None,
        }
    }

    /// tests/basic/029.phpt: a Shift_JIS field name whose characters all
    /// end in 0x5C converts to UTF-8.
    #[test]
    fn shift_jis_input_converts_to_the_internal_encoding() {
        let mut t = translation(&["SJIS"], "UTF-8");
        assert_eq!(t.convert(b"x"), None);
        t.detect(b"Content-Disposition: form-data; name=\"\x97\x5c\x8e\x5c\x94\x5c\"");
        assert_eq!(t.input_char_len(0x97), 2);
        assert_eq!(t.input_char_len(b'"'), 1);
        assert_eq!(t.internal_char_len(0xe4), 3);
        assert_eq!(
            t.convert(b"\x97\x5c\x8e\x5c\x94\x5c").unwrap(),
            b"\xe4\xba\x88\xe8\x9a\x95\xe8\x83\xbd"
        );
    }

    #[test]
    fn a_lone_pass_detects_nothing() {
        let mut t = translation(&[], "UTF-8");
        t.detect_order = vec![encoding::pass()];
        t.detect(b"Content-Type: text/plain");
        assert!(t.input.is_none());
        assert_eq!(t.input_char_len(0x97), 1);
        assert_eq!(t.convert(b"\x80"), None);
    }

    #[test]
    fn mbchar_bytes_by_encoding_type() {
        assert_eq!(mbchar_bytes(None, 0xff), 1);
        assert_eq!(mbchar_bytes(Some(enc("UTF-32")), b'a'), 4);
        assert_eq!(mbchar_bytes(Some(enc("ISO-8859-1")), 0xe0), 1);
    }

    #[test]
    fn conversion_matches_fast_convert() {
        let input = b"\x80abc\xe0";
        let (want, _) = fast_convert(input, enc("CP1252"), enc("UTF-8"), 0x3f, ErrorMode::Char);
        assert_eq!(
            convert_until_oom(input, enc("CP1252"), enc("UTF-8"), 0x3f, ErrorMode::Char),
            want
        );
    }
}
