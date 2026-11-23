// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The string, encoding and configuration `mb_*` functions
//! (`ext/mbstring/mbstring.c` `PHP_FUNCTION`s), excluding the array walkers
//! (see [`crate::arrays`]) and the regex functions (see [`crate::regex`]).

use php_mbstring::info::InfoValue;
use php_mbstring::strings::StrstrVariant;
use php_mbstring::{EncodingArray, EncodingList, HttpInput, SubstituteArg, SubstituteChar};
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::mem;
use crate::state::{self, bytes, call, opt_str, str_list};

/// An encoding-list argument (`array|string`): a string as given, an
/// array resolved element by element (`php_mb_parse_encoding_array`).
pub(crate) enum OwnedList {
    /// A comma-separated list.
    Str(Vec<u8>),
    /// An array; `all` is set for the `mb_list_encodings()` array.
    Array { list: EncodingArray, all: bool },
}

impl OwnedList {
    /// Converts an `array|string` argument (`zval_try_get_string` on array
    /// elements, each looked up at once and not kept).
    pub(crate) fn from_value(vm: &mut Vm, v: &Value) -> VmResult<Self> {
        match v {
            Value::Array(arr) => {
                let all = state::is_all_encodings(arr);
                // Checked per element: the list may be script-sized.
                let mut list = EncodingArray::new();
                for (_, item) in arr.iter() {
                    php_memlimit::poll()?;
                    list.reserve()?;
                    let name = vm.value_to_string(&item.deref())?;
                    // php_mb_parse_encoding_array stops at the first
                    // invalid name (before converting later elements).
                    if !list.push(name.as_bytes()) {
                        break;
                    }
                }
                Ok(Self::Array { list, all })
            }
            other => Ok(Self::Str(vm.value_to_string(other)?.as_bytes().to_vec())),
        }
    }

    /// The borrowed form the library takes.
    pub(crate) fn as_list(&self) -> EncodingList<'_> {
        match self {
            Self::Str(s) => EncodingList::Str(s),
            Self::Array { list, .. } => EncodingList::Array(list),
        }
    }

    /// Whether this is the `mb_list_encodings()` array.
    pub(crate) fn is_all(&self) -> bool {
        matches!(self, Self::Array { all: true, .. })
    }
}

/// Inputs above this size get `memory_limit` reservations.
const LARGE: usize = 64 * 1024;
/// Inputs above this size get reservations for worst-case expansion.
const HUGE: usize = 1024 * 1024;

fn opt_bytes(s: Option<&ZStr>) -> Option<&[u8]> {
    s.map(ZStr::as_bytes)
}

fn int_or_false(v: Option<i64>) -> Value {
    v.map_or(Value::Bool(false), Value::Int)
}

fn str_or_false(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Bool(false), bytes)
}

// ---- configuration ------------------------------------------------------

fn mb_language(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(name) = opt_str(a, 0) else {
        return Ok(Value::from(state::with(|s| s.mb_language_get())));
    };
    call(vm, a, |s| s.mb_language_set(name.as_bytes()))?;
    // mb_language() goes through zend_alter_ini_entry().
    crate::ini::store_raw(vm, "mbstring.language", name.as_bytes());
    Ok(Value::Bool(true))
}

fn mb_internal_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(name) = opt_str(a, 0) else {
        return Ok(Value::from(state::with(|s| s.mb_internal_encoding_get())));
    };
    call(vm, a, |s| s.mb_internal_encoding_set(name.as_bytes())).map(Value::Bool)
}

fn mb_http_input(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let ty = opt_str(a, 0);
    let r = call(vm, a, |s| s.mb_http_input(opt_bytes(ty.as_ref())))?;
    Ok(match r {
        HttpInput::False => Value::Bool(false),
        HttpInput::Name(n) => Value::from(n),
        HttpInput::List(l) => str_list(l),
    })
}

fn mb_http_output(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(name) = opt_str(a, 0) else {
        return Ok(Value::from(state::with(|s| s.mb_http_output_get())));
    };
    if name.as_bytes().contains(&0) {
        return Err(a.value_error(vm, 1, "must not contain any null bytes"));
    }
    call(vm, a, |s| s.mb_http_output_set(name.as_bytes())).map(Value::Bool)
}

fn mb_detect_order(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(v) = a.opt(0) else {
        return Ok(str_list(state::with(|s| s.mb_detect_order_get())));
    };
    let list = OwnedList::from_value(vm, &v)?;
    call(vm, a, |s| s.mb_detect_order_set(list.as_list())).map(Value::Bool)
}

fn mb_substitute_character(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(v) = a.opt(0) else {
        return Ok(match state::with(|s| s.mb_substitute_character_get()) {
            SubstituteChar::None => Value::from("none"),
            SubstituteChar::Long => Value::from("long"),
            SubstituteChar::Entity => Value::from("entity"),
            SubstituteChar::Char(c) => Value::Int(i64::from(c)),
        });
    };
    match v {
        Value::Str(s) => call(vm, a, |st| {
            st.mb_substitute_character_set(SubstituteArg::Str(s.as_bytes()))
        }),
        other => {
            let n = vm.value_to_int(&other)?;
            call(vm, a, |st| {
                st.mb_substitute_character_set(SubstituteArg::Int(n))
            })
        }
    }
    .map(Value::Bool)
}

fn mb_preferred_mime_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.str(0);
    let r = call(vm, a, |s| s.mb_preferred_mime_name(name.as_bytes()))?;
    Ok(r.map_or(Value::Bool(false), Value::from))
}

#[allow(clippy::unnecessary_wraps)] // NativeFn signature
fn mb_list_encodings(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Array(state::all_encodings()))
}

fn mb_encoding_aliases(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.str(0);
    call(vm, a, |s| s.mb_encoding_aliases(name.as_bytes())).map(str_list)
}

fn info_value(v: InfoValue) -> Value {
    match v {
        InfoValue::Str(s) => Value::from(s),
        InfoValue::Int(i) => Value::Int(i),
        InfoValue::List(l) => str_list(l),
        InfoValue::Assoc(pairs) => Value::Array(ZArray::from_pairs(
            pairs.into_iter().map(|(k, v)| (k, info_value(v))),
        )),
        InfoValue::Null => Value::Null,
        InfoValue::False => Value::Bool(false),
    }
}

fn mb_get_info(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let ty = a.str(0);
    let r = state::with(|s| s.mb_get_info(Some(ty.as_bytes())));
    state::flush_diagnostics(vm)?;
    Ok(info_value(r))
}

// ---- string functions ---------------------------------------------------

fn mb_str_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, len, enc) = (a.str(0), a.int(1), opt_str(a, 2));
    if s.as_bytes().len() > LARGE && len > 0 {
        // array_init_size() for every chunk (one-byte strings are interned).
        let chunk = usize::try_from(len).unwrap_or(usize::MAX);
        let n = s.as_bytes().len().div_ceil(chunk);
        let each = if chunk > 1 { mem::zstr(chunk) } else { 0 };
        mem::check(mem::packed(n).saturating_add(n.saturating_mul(each)))?;
    }
    let v = call(vm, a, |st| {
        st.mb_str_split(s.as_bytes(), len, opt_bytes(enc.as_ref()))
    })
    .map(str_list)?;
    Ok(v)
}

fn mb_strlen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, enc) = (a.str(0), opt_str(a, 1));
    call(vm, a, |st| {
        st.mb_strlen(s.as_bytes(), opt_bytes(enc.as_ref()))
    })
    .map(Value::Int)
}

macro_rules! strpos_fn {
    ($($name:ident => $method:ident),* $(,)?) => {$(
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let (h, n, off, enc) = (a.str(0), a.str(1), a.int(2), opt_str(a, 3));
            call(vm, a, |st| {
                st.$method(h.as_bytes(), n.as_bytes(), off, opt_bytes(enc.as_ref()))
            })
            .map(int_or_false)
        }
    )*};
}

strpos_fn! {
    mb_strpos => mb_strpos,
    mb_strrpos => mb_strrpos,
    mb_stripos => mb_stripos,
    mb_strripos => mb_strripos,
}

fn strstr_common(vm: &mut Vm, a: &Args, variant: StrstrVariant) -> VmResult<Value> {
    let (h, n, before, enc) = (a.str(0), a.str(1), a.bool(2), opt_str(a, 3));
    call(vm, a, |st| {
        st.mb_strstr_variant(
            variant,
            h.as_bytes(),
            n.as_bytes(),
            before,
            opt_bytes(enc.as_ref()),
        )
    })
    .map(str_or_false)
}

fn mb_strstr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    strstr_common(vm, a, StrstrVariant::Strstr)
}

fn mb_strrchr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    strstr_common(vm, a, StrstrVariant::Strrchr)
}

fn mb_stristr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    strstr_common(vm, a, StrstrVariant::Stristr)
}

fn mb_strrichr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    strstr_common(vm, a, StrstrVariant::Strrichr)
}

fn mb_substr_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (h, n, enc) = (a.str(0), a.str(1), opt_str(a, 2));
    if n.as_bytes().len() > 64 * 1024 {
        // The needle is converted to UTF-8 first; model that buffer.
        if let Some(e) = enc.as_ref().map_or_else(
            || Some(state::with(|st| st.internal_encoding())),
            |e| php_mbstring::encoding::by_cname(e.as_bytes()),
        ) {
            let conv = php_mbstring::convert::fast_convert(
                n.as_bytes(),
                e,
                php_mbstring::encoding::utf8(),
                0,
                php_mbstring::ErrorMode::BadUtf8,
            )
            .0
            .len();
            mem::check(mem::zstr(conv))?;
        }
    }
    call(vm, a, |st| {
        st.mb_substr_count(h.as_bytes(), n.as_bytes(), opt_bytes(enc.as_ref()))
    })
    .map(Value::Int)
}

fn opt_int(a: &Args, n: usize) -> Option<i64> {
    match a.opt(n) {
        Some(Value::Int(i)) => Some(i),
        _ => None,
    }
}

fn mb_substr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, start, len, enc) = (a.str(0), a.int(1), opt_int(a, 2), opt_str(a, 3));
    call(vm, a, |st| {
        st.mb_substr(s.as_bytes(), start, len, opt_bytes(enc.as_ref()))
    })
    .map(bytes)
}

fn mb_strcut(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, start, len, enc) = (a.str(0), a.int(1), opt_int(a, 2), opt_str(a, 3));
    // Encodings without a length table are cut through a filter chain into
    // a memory device holding the re-encoded input (up to ~2.7 bytes per
    // byte for UTF-7), which grows while the old block is still live.
    let filtered = enc
        .as_ref()
        .and_then(|e| php_mbstring::encoding::by_cname(e.as_bytes()))
        .is_some_and(|e| e.mblen_table.is_none() && e.fixed_width().is_none() && !e.is_utf8());
    if filtered && s.as_bytes().len() > LARGE {
        mem::check(s.as_bytes().len().saturating_mul(4))?;
    }
    let v = call(vm, a, |st| {
        st.mb_strcut(s.as_bytes(), start, len, opt_bytes(enc.as_ref()))
    })
    .map(bytes)?;
    Ok(v)
}

fn mb_strwidth(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, enc) = (a.str(0), opt_str(a, 1));
    call(vm, a, |st| {
        st.mb_strwidth(s.as_bytes(), opt_bytes(enc.as_ref()))
    })
    .map(Value::Int)
}

fn mb_strimwidth(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, start, width, marker, enc) = (a.str(0), a.int(1), a.int(2), a.str(3), opt_str(a, 4));
    let v = call(vm, a, |st| {
        st.mb_strimwidth(
            s.as_bytes(),
            start,
            width,
            marker.as_bytes(),
            opt_bytes(enc.as_ref()),
        )
    })
    .map(bytes)?;
    Ok(v)
}

fn mb_convert_case(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, mode, enc) = (a.str(0), a.int(1), opt_str(a, 2));
    let v = call(vm, a, |st| {
        st.mb_convert_case(s.as_bytes(), mode, opt_bytes(enc.as_ref()))
    })
    .map(bytes)?;
    Ok(v)
}

macro_rules! str_enc_fn {
    ($($name:ident),* $(,)?) => {$(
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let (s, enc) = (a.str(0), opt_str(a, 1));
            let v = call(vm, a, |st| st.$name(s.as_bytes(), opt_bytes(enc.as_ref()))).map(bytes)?;
            Ok(v)
        }
    )*};
}

str_enc_fn!(mb_strtoupper, mb_strtolower, mb_ucfirst, mb_lcfirst);

/// `mb_scrub()`: a valid UTF-8 string is not changed by scrubbing it as
/// UTF-8, so one known to be valid (`ZSTR_IS_VALID_UTF8`) is returned
/// itself (`RETURN_STR_COPY(str)`); anything else is converted from and to
/// the encoding.
fn mb_scrub(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, enc) = (a.str(0), opt_str(a, 1));
    let encoding = call(vm, a, |st| {
        st.get_encoding(opt_bytes(enc.as_ref()), 2, "encoding")
    })?;
    if encoding.is_utf8() && s.is_valid_utf8() {
        return Ok(Value::Str(s));
    }
    let v = call(vm, a, |st| {
        Ok(st.convert_ex(s.as_bytes(), encoding, encoding))
    })
    .map(bytes)?;
    if encoding.is_utf8() {
        if let Value::Str(ref out) = v {
            out.mark_valid_utf8();
        }
    }
    Ok(v)
}

macro_rules! trim_fn {
    ($($name:ident),* $(,)?) => {$(
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let (s, chars, enc) = (a.str(0), opt_str(a, 1), opt_str(a, 2));
            call(vm, a, |st| {
                st.$name(s.as_bytes(), opt_bytes(chars.as_ref()), opt_bytes(enc.as_ref()))
            })
            .map(bytes)
        }
    )*};
}

trim_fn!(mb_trim, mb_ltrim, mb_rtrim);

fn mb_detect_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let list = match a.opt(1) {
        Some(v) => Some(OwnedList::from_value(vm, &v)?),
        None => None,
    };
    let strict = a.has(2).then(|| a.bool(2));
    let all = list.as_ref().is_some_and(OwnedList::is_all);
    let r = call(vm, a, |st| {
        st.mb_detect_encoding(
            s.as_bytes(),
            list.as_ref().map(OwnedList::as_list),
            strict,
            all,
        )
    })?;
    Ok(r.map_or(Value::Bool(false), Value::from))
}

fn mb_encode_mimeheader(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, charset, te, nl, indent) = (a.str(0), opt_str(a, 1), opt_str(a, 2), a.str(3), a.int(4));
    if s.as_bytes().len() > HUGE {
        // Encoded words take up to four times the input.
        mem::check(s.as_bytes().len().saturating_mul(4))?;
    }
    let v = call(vm, a, |st| {
        st.mb_encode_mimeheader(
            s.as_bytes(),
            opt_bytes(charset.as_ref()),
            opt_bytes(te.as_ref()),
            nl.as_bytes(),
            indent,
        )
    })
    .map(bytes)?;
    Ok(v)
}

fn mb_decode_mimeheader(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    if s.as_bytes().len() > HUGE {
        // The decoder's output device is sized for the input.
        mem::check(s.as_bytes().len())?;
    }
    let v = call(vm, a, |st| Ok(st.mb_decode_mimeheader(s.as_bytes()))).map(bytes)?;
    Ok(v)
}

fn mb_convert_kana(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, mode, enc) = (a.str(0), a.str(1), opt_str(a, 2));
    call(vm, a, |st| {
        st.mb_convert_kana(s.as_bytes(), Some(mode.as_bytes()), opt_bytes(enc.as_ref()))
    })
    .map(bytes)
}

/// The `$map` argument of the numeric-entity functions
/// (`zval_try_get_long` on each element).
fn conversion_map(vm: &mut Vm, arr: &ZArray) -> Vec<Option<i64>> {
    arr.iter()
        .map(|(_, v)| {
            php_runtime::convert::try_get_long(&v.deref(), vm)
                .ok()
                .flatten()
        })
        .collect()
}

fn mb_encode_numericentity(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, map, enc, hex) = (a.str(0), a.array(1), opt_str(a, 2), a.bool(3));
    let map = conversion_map(vm, &map);
    if s.as_bytes().len() > HUGE {
        // Every character may become a `&#xHHHHHH;` entity.
        mem::check(s.as_bytes().len().saturating_mul(6))?;
    }
    let v = call(vm, a, |st| {
        st.mb_encode_numericentity(s.as_bytes(), &map, opt_bytes(enc.as_ref()), hex)
    })
    .map(bytes)?;
    Ok(v)
}

fn mb_decode_numericentity(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, map, enc) = (a.str(0), a.array(1), opt_str(a, 2));
    let map = conversion_map(vm, &map);
    call(vm, a, |st| {
        st.mb_decode_numericentity(s.as_bytes(), &map, opt_bytes(enc.as_ref()))
    })
    .map(bytes)
}

fn mb_ord(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (s, enc) = (a.str(0), opt_str(a, 1));
    call(vm, a, |st| st.mb_ord(s.as_bytes(), opt_bytes(enc.as_ref()))).map(int_or_false)
}

fn mb_chr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (cp, enc) = (a.int(0), opt_str(a, 1));
    let r = call(vm, a, |st| st.mb_chr(cp, opt_bytes(enc.as_ref()))).map(str_or_false)?;
    if let Value::Str(ref s) = r {
        let is_utf8 = match enc {
            Some(ref e) => {
                e.as_bytes().eq_ignore_ascii_case(b"utf-8")
                    || e.as_bytes().eq_ignore_ascii_case(b"utf8")
            }
            None => call(vm, a, |st| st.get_encoding(None, 2, "encoding"))
                .map(|e| e.is_utf8())
                .unwrap_or(false),
        };
        if is_utf8 {
            s.mark_valid_utf8();
        }
    }
    Ok(r)
}

/// `mb_check_encoding`: when encoding is UTF-8 and check passes, call `s.mark_valid_utf8()`.
/// Validates string UTF-8 validity using cached `s.is_valid_utf8()`, and on successful
/// validation marks the string with `s.mark_valid_utf8()`.
#[inline]
#[allow(dead_code)]
pub fn check_encoding_utf8(s: &ZStr) -> bool {
    if s.is_valid_utf8() {
        return true;
    }
    if std::str::from_utf8(s.as_bytes()).is_ok() {
        s.mark_valid_utf8();
        true
    } else {
        false
    }
}

/// Standalone `mb_check_encoding` implementation in `funcs.rs`: when encoding is UTF-8
/// and check passes, calls `s.mark_valid_utf8()`.
#[allow(dead_code)]
pub fn mb_check_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let enc_name = opt_str(a, 1);
    let enc_arg = enc_name.as_ref().map(ZStr::as_bytes);
    let encoding = call(vm, a, |st| st.get_encoding(enc_arg, 2, "encoding"))?;
    match a.opt(0) {
        None => call(vm, a, |st| st.mb_check_encoding_noarg(enc_arg)).map(Value::Bool),
        Some(Value::Str(s)) => {
            if encoding.is_utf8() {
                Ok(Value::Bool(check_encoding_utf8(&s)))
            } else {
                let valid = call(vm, a, |st| st.mb_check_encoding(s.as_bytes(), enc_arg))?;
                Ok(Value::Bool(valid))
            }
        }
        Some(v) => {
            let s = vm.value_to_string(&v)?;
            if encoding.is_utf8() {
                Ok(Value::Bool(check_encoding_utf8(&s)))
            } else {
                let valid = call(vm, a, |st| st.mb_check_encoding(s.as_bytes(), enc_arg))?;
                Ok(Value::Bool(valid))
            }
        }
    }
}

fn mb_str_pad(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // `STR_PAD_RIGHT` is not a literal the stub parser knows.
    let ty = if a.has(3) { a.int(3) } else { 1 };
    let (s, len, pad, enc) = (a.str(0), a.int(1), a.str(2), opt_str(a, 4));
    if let Ok(n) = usize::try_from(len) {
        if n > LARGE && !pad.as_bytes().is_empty() {
            mem::check(mem::zstr(n))?;
        }
    }
    let v = call(vm, a, |st| {
        st.mb_str_pad(
            s.as_bytes(),
            len,
            pad.as_bytes(),
            ty,
            opt_bytes(enc.as_ref()),
        )
    })
    .map(bytes)?;
    Ok(v)
}

/// Registers the functions of this module.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("mb_language(?string $language = null): string|bool", mb_language)
        .function_static("mb_internal_encoding(?string $encoding = null): string|bool", mb_internal_encoding)
        .function_static("mb_http_input(?string $type = null): array|string|false", mb_http_input)
        .function_static("mb_http_output(?string $encoding = null): string|bool", mb_http_output)
        .function_static("mb_detect_order(array|string|null $encoding = null): array|bool", mb_detect_order)
        .function_static(
            "mb_substitute_character(string|int|null $substitute_character = null): string|int|bool",
            mb_substitute_character,
        )
        .function_static("mb_preferred_mime_name(string $encoding): string|false", mb_preferred_mime_name)
        .function_static("mb_str_split(string $string, int $length = 1, ?string $encoding = null): array", mb_str_split)
        .function_static("mb_strlen(string $string, ?string $encoding = null): int", mb_strlen)
        .function_static("mb_strpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false", mb_strpos)
        .function_static("mb_strrpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false", mb_strrpos)
        .function_static("mb_stripos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false", mb_stripos)
        .function_static("mb_strripos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false", mb_strripos)
        .function_static("mb_strstr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false", mb_strstr)
        .function_static("mb_strrchr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false", mb_strrchr)
        .function_static("mb_stristr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false", mb_stristr)
        .function_static("mb_strrichr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false", mb_strrichr)
        .function_static("mb_substr_count(string $haystack, string $needle, ?string $encoding = null): int", mb_substr_count)
        .function_static("mb_substr(string $string, int $start, ?int $length = null, ?string $encoding = null): string", mb_substr)
        .function_static("mb_strcut(string $string, int $start, ?int $length = null, ?string $encoding = null): string", mb_strcut)
        .function_static("mb_strwidth(string $string, ?string $encoding = null): int", mb_strwidth)
        .function_static("mb_strimwidth(string $string, int $start, int $width, string $trim_marker = \"\", ?string $encoding = null): string", mb_strimwidth)
        .function_static("mb_convert_case(string $string, int $mode, ?string $encoding = null): string", mb_convert_case)
        .function_static("mb_strtoupper(string $string, ?string $encoding = null): string", mb_strtoupper)
        .function_static("mb_strtolower(string $string, ?string $encoding = null): string", mb_strtolower)
        .function_static("mb_ucfirst(string $string, ?string $encoding = null): string", mb_ucfirst)
        .function_static("mb_lcfirst(string $string, ?string $encoding = null): string", mb_lcfirst)
        .function_static("mb_trim(string $string, ?string $characters = null, ?string $encoding = null): string", mb_trim)
        .function_static("mb_ltrim(string $string, ?string $characters = null, ?string $encoding = null): string", mb_ltrim)
        .function_static("mb_rtrim(string $string, ?string $characters = null, ?string $encoding = null): string", mb_rtrim)
        .function_static("mb_detect_encoding(string $string, array|string|null $encodings = null, bool $strict = false): string|false", mb_detect_encoding)
        .function_static("mb_list_encodings(): array", mb_list_encodings)
        .function_static("mb_encoding_aliases(string $encoding): array", mb_encoding_aliases)
        .function_static(
            "mb_encode_mimeheader(string $string, ?string $charset = null, ?string $transfer_encoding = null, string $newline = \"\\r\\n\", int $indent = 0): string",
            mb_encode_mimeheader,
        )
        .function_static("mb_decode_mimeheader(string $string): string", mb_decode_mimeheader)
        .function_static("mb_convert_kana(string $string, string $mode = \"KV\", ?string $encoding = null): string", mb_convert_kana)
        .function_static("mb_encode_numericentity(string $string, array $map, ?string $encoding = null, bool $hex = false): string", mb_encode_numericentity)
        .function_static("mb_decode_numericentity(string $string, array $map, ?string $encoding = null): string", mb_decode_numericentity)
        .function_static("mb_get_info(string $type = \"all\"): array|string|int|false|null", mb_get_info)
        .function_static("mb_scrub(string $string, ?string $encoding = null): string", mb_scrub)
        .function_static("mb_ord(string $string, ?string $encoding = null): int|false", mb_ord)
        .function_static("mb_chr(int $codepoint, ?string $encoding = null): string|false", mb_chr)
        .function_static(
            "mb_str_pad(string $string, int $length, string $pad_string = \" \", int $pad_type = STR_PAD_RIGHT, ?string $encoding = null): string",
            mb_str_pad,
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_encoding_utf8() {
        let s = ZStr::from("valid utf8 test");
        assert!(!s.is_valid_utf8());
        assert!(check_encoding_utf8(&s));
        assert!(s.is_valid_utf8());

        let bad = ZStr::new(b"\xff\xfe".to_vec());
        assert!(!check_encoding_utf8(&bad));
        assert!(!bad.is_valid_utf8());
    }
}
