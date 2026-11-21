// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Source highlighting/stripping, query strings and locale functions.

use crate::ctx::{RtExt, VmQuery};
use php_lexer::WarningKind;
use php_runtime::{ArrayKey, CastTarget, HashTable, RtResult, RuntimeContext, Value, ZArray, ZStr};
use php_stdmisc::highlight::{self, HighlightColors};
use php_stdmisc::locale::{self, LocaleState};
use php_stdmisc::query::{self, InputSettings};
use php_support::output::{self, flags, StartHandler};
use php_vm::ext::{Args, ModuleBuilder, ParseBodyOptions, VmResult};
use php_vm::Vm;
use std::cell::RefCell;

thread_local! {
    static LOCALE: RefCell<LocaleState> = RefCell::new(LocaleState::default());
}

/// `E_COMPILE_WARNING`.
const E_COMPILE_WARNING: i64 = 128;
/// `E_DEPRECATED`.
const E_DEPRECATED: i64 = 8192;

/// `php_get_highlight_struct()`: the `highlight.*` INI colours.
fn colors(vm: &Vm) -> HighlightColors {
    let d = HighlightColors::default();
    let get = |name: &str, def: Vec<u8>| vm.ini_string(name).unwrap_or(def);
    HighlightColors {
        comment: get("highlight.comment", d.comment),
        default: get("highlight.default", d.default),
        html: get("highlight.html", d.html),
        keyword: get("highlight.keyword", d.keyword),
        string: get("highlight.string", d.string),
    }
}

fn short_tags(vm: &Vm) -> bool {
    vm.ini_bool("short_open_tag")
}

/// Prints or returns highlighted HTML (`$return`).
fn emit(vm: &mut Vm, html: &[u8], ret: bool) -> VmResult<Value> {
    vm.output(html);
    let value = if ret {
        let contents = vm.output.contents();
        output::discard(vm);
        contents.map_or(Value::Bool(false), |s| Value::Str(ZStr::from(s)))
    } else {
        Value::Bool(true)
    };
    if let Some(t) = vm.pending_throw.take() {
        return Err(t);
    }
    Ok(value)
}

/// `php_output_start_default()` for the temporary return buffer.
fn start_capture(vm: &mut Vm) -> VmResult<()> {
    let ok = output::start(vm, StartHandler::Default, 0, flags::STDFLAGS);
    if let Some(t) = vm.pending_throw.take() {
        return Err(t);
    }
    if !ok {
        return Err(vm.throw("Error", "Unable to start output handler"));
    }
    Ok(())
}

fn highlight_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let src = a.str(0);
    let ret = a.has(1) && a.bool(1);
    if ret {
        start_capture(vm)?;
    }
    let html = highlight::highlight(src.as_bytes(), &colors(vm), short_tags(vm));
    emit(vm, &html, ret)
}

/// Reads a script for highlighting/stripping like `zend_stream_open()`:
/// uses the include path and enforces wrapper access restrictions.
fn read_script(vm: &mut Vm, path: &[u8]) -> VmResult<Option<Vec<u8>>> {
    use php_streams::wrapper::{REPORT_ERRORS, STREAM_OPEN_FOR_INCLUDE, USE_PATH};

    if path.is_empty() {
        return Err(vm.throw("ValueError", php_streams::open::EMPTY_PATH_ERROR));
    }
    php_ext_streams::bridge::run(vm, || {
        let mut opened = None;
        let stream = php_streams::open::open_wrapper(
            path,
            "rb",
            USE_PATH | REPORT_ERRORS | STREAM_OPEN_FOR_INCLUDE,
            &mut opened,
            None,
        );
        stream.and_then(|mut s| {
            s.add_flags(php_streams::stream::flags::NO_BUFFER);
            let mut source = Vec::new();
            let mut buffer = [0; php_streams::stream::CHUNK_SIZE];
            let result = loop {
                match s.read(&mut buffer) {
                    Ok(0) => break Some(source),
                    Ok(n) => {
                        if !php_streams::util::reserve_more(&mut source, n) {
                            break None;
                        }
                        source.extend_from_slice(&buffer[..n]);
                    }
                    Err(_) => break None,
                }
            };
            s.close();
            result
        })
    })
}

fn highlight_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = a.str(0);
    let ret = a.has(1) && a.bool(1);
    if path.as_bytes().contains(&0) {
        return Err(a.value_error(vm, 1, "must not contain any null bytes"));
    }
    if ret {
        start_capture(vm)?;
    }
    let result = (|| {
        let Some(src) = read_script(vm, path.as_bytes())? else {
            vm.php_warning(&format!(
                "Failed opening '{}' for highlighting",
                String::from_utf8_lossy(path.as_bytes())
            ))?;
            return Ok(None);
        };
        let html = highlight::highlight(&src, &colors(vm), short_tags(vm));
        Ok(Some(html))
    })();
    match result {
        Ok(Some(html)) => emit(vm, &html, ret),
        result => {
            if ret {
                output::end(vm);
            }
            if let Some(t) = vm.pending_throw.take() {
                return Err(t);
            }
            result.map(|_| Value::Bool(false))
        }
    }
}

fn php_strip_whitespace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = a.str(0);
    if path.as_bytes().contains(&0) {
        return Err(a.value_error(vm, 1, "must not contain any null bytes"));
    }
    start_capture(vm)?;
    let result = (|| {
        let Some(src) = read_script(vm, path.as_bytes())? else {
            return Ok(None);
        };
        let (out, warnings) = highlight::strip_whitespace(&src, short_tags(vm));
        for w in warnings {
            let level = match w.kind {
                WarningKind::CompileWarning => E_COMPILE_WARNING,
                WarningKind::Deprecated => E_DEPRECATED,
            };
            vm.error_at(level, &w.message, &path, w.line)?;
        }
        Ok(Some(out))
    })();
    match result {
        Ok(Some(out)) => emit(vm, &out, true),
        result => {
            output::end(vm);
            if let Some(t) = vm.pending_throw.take() {
                return Err(t);
            }
            result.map(|_| Value::from(""))
        }
    }
}

fn http_build_query(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.at(0);
    let prefix = if a.has(1) { a.str(1) } else { ZStr::from("") };
    let sep = a.opt(2).map(|_| a.str(2));
    let enc = if a.has(3) {
        a.int(3)
    } else {
        query::PHP_QUERY_RFC1738
    };
    let default_sep = vm.ini_string("arg_separator.output").unwrap_or_default();
    let r = {
        let mut q = VmQuery::new(vm);
        query::http_build_query(
            &mut q,
            &data,
            prefix.as_bytes(),
            sep.as_ref().map(ZStr::as_bytes),
            enc,
            &default_sep,
        )
    };
    r.vm(vm).map(Value::Str)
}

/// The `$options` keys of `request_parse_body()`
/// (`REQUEST_PARSE_BODY_OPTION_*`).
const PARSE_BODY_OPTIONS: [&[u8]; 5] = [
    b"max_file_uploads",
    b"max_input_vars",
    b"max_multipart_body_parts",
    b"post_max_size",
    b"upload_max_filesize",
];

/// `cache_request_parse_body_options()`: validates `$options` and reads
/// each limit (an integer, or a quantity string such as `"8M"`); a later
/// key overrides an earlier one spelled in another case.
fn parse_body_options(vm: &mut Vm, options: &ZArray) -> VmResult<ParseBodyOptions> {
    let mut parsed = ParseBodyOptions::default();
    for (k, v) in options.iter() {
        let key = match k {
            ArrayKey::Int(_) => {
                return Err(vm.throw("ValueError", "Invalid integer key in $options argument"))
            }
            ArrayKey::Str(s) => s.clone(),
        };
        if key.as_bytes().is_empty() {
            return Err(vm.throw(
                "ValueError",
                "Invalid empty string key in $options argument",
            ));
        }
        let Some(index) = PARSE_BODY_OPTIONS
            .iter()
            .position(|n| key.as_bytes().eq_ignore_ascii_case(n))
        else {
            let msg = format!(
                "Invalid key \"{}\" in $options argument",
                key.to_string_lossy()
            );
            return Err(vm.throw("ValueError", &msg));
        };
        // cache_request_parse_body_option().
        let value = match v.deref() {
            Value::Str(s) => {
                let (n, err) = php_support::ini::parse_quantity(s.as_bytes());
                if let Some(err) = err {
                    vm.raise(2, &err)?;
                }
                n
            }
            Value::Int(n) => n,
            other => {
                let msg = format!("Invalid {} value in $options argument", other.value_name());
                return Err(vm.throw("ValueError", &msg));
            }
        };
        let slot = match index {
            0 => &mut parsed.max_file_uploads,
            1 => &mut parsed.max_input_vars,
            2 => &mut parsed.max_multipart_body_parts,
            3 => &mut parsed.post_max_size,
            _ => &mut parsed.upload_max_filesize,
        };
        *slot = Some(value);
    }
    Ok(parsed)
}

/// `php_sapi_filter()` for `PARSE_POST` over a parsed body: every
/// non-empty string becomes its `filter.default` result, whatever its
/// type; an empty value stays `""`.
fn filter_post_values(
    q: &mut VmQuery<'_>,
    arr: &ZArray,
    filter: i64,
    flags: i64,
) -> RtResult<ZArray> {
    let mut out = HashTable::new();
    for (k, v) in arr.iter() {
        let nv = match v.deref() {
            Value::Array(inner) => Value::Array(filter_post_values(q, &inner, filter, flags)?),
            Value::Str(s) if !s.as_bytes().is_empty() => {
                php_filter::apply_default_filter(q, s.as_bytes(), filter, flags)?
            }
            other => other,
        };
        out.insert(k.clone(), nv);
    }
    Ok(ZArray::from_table(out))
}

/// `PHP_FUNCTION(request_parse_body)` (`ext/standard/http.c`): validates
/// `$options` like `cache_request_parse_body_options()`, then has the web
/// SAPI read and parse the request body ([`Vm::parse_body_hook`]: its
/// content type's POST handler, with `RequestParseBodyException`s for
/// what are warnings at request startup) and returns `[$post, $files]`.
/// The CLI request has no content type (`SG(request_info).content_type`).
fn request_parse_body(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let options = match a.opt(0) {
        Some(Value::Array(options)) => parse_body_options(vm, &options)?,
        _ => ParseBodyOptions::default(),
    };
    let Some(parse) = vm.parse_body_hook else {
        return Err(vm.throw(
            "RequestParseBodyException",
            "Request does not provide a content type",
        ));
    };
    let (mut post, files) = parse(vm, &options)?;
    // sapi_module.input_filter: ext/filter filters the form fields.
    if let Some((filter, flags)) = default_filter(vm) {
        let r = filter_post_values(&mut VmQuery::new(vm), &post, filter, flags);
        post = r.vm(vm)?;
    }
    Ok(Value::Array(ZArray::from_values([
        Value::Array(post),
        Value::Array(files),
    ])))
}

/// The request-variable INI settings used by `parse_str()`.
fn input_settings(vm: &Vm) -> InputSettings {
    let d = InputSettings::default();
    InputSettings {
        arg_separator_input: vm
            .ini_string("arg_separator.input")
            .unwrap_or(d.arg_separator_input),
        max_input_vars: vm
            .ini_string("max_input_vars")
            .map_or(d.max_input_vars, |_| vm.ini_long("max_input_vars")),
        max_input_nesting_level: vm
            .ini_string("max_input_nesting_level")
            .map_or(d.max_input_nesting_level, |_| {
                vm.ini_long("max_input_nesting_level")
            }),
        display_errors: vm.ini_bool("display_errors"),
    }
}

/// `filter.default` / `filter.default_flags` when the filter extension is
/// loaded and selects a filter other than `FILTER_UNSAFE_RAW`.
fn default_filter(vm: &Vm) -> Option<(i64, i64)> {
    let raw = vm.ini_string("filter.default")?;
    let (id, _) = php_filter::parse_default_filter_ini(&raw);
    if id == php_filter::constants::FILTER_UNSAFE_RAW {
        return None;
    }
    let flags = vm
        .ini_string("filter.default_flags")
        .filter(|v| !v.is_empty());
    Some((id, php_filter::parse_default_flags_ini(flags.as_deref())))
}

/// `sapi_module.input_filter` for `PARSE_STRING` (`php_sapi_filter`):
/// applies the default filter to every value of the parsed array.
fn filter_values(q: &mut VmQuery<'_>, arr: &ZArray, filter: i64, flags: i64) -> RtResult<ZArray> {
    let mut out = HashTable::new();
    for (k, v) in arr.iter() {
        let nv = match v.deref() {
            Value::Array(inner) => Value::Array(filter_values(q, &inner, filter, flags)?),
            Value::Str(s) => {
                match php_filter::apply_default_filter(q, s.as_bytes(), filter, flags)? {
                    v @ Value::Str(_) => v,
                    Value::Bool(false) | Value::Null => Value::from(""),
                    other => Value::Str(other.to_zstr(q)?),
                }
            }
            other => other,
        };
        out.insert(k.clone(), nv);
    }
    Ok(ZArray::from_table(out))
}

fn parse_str(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let settings = input_settings(vm);
    let mut arr = query::parse_str(s.as_bytes(), &settings, vm).vm(vm)?;
    if let Some((filter, flags)) = default_filter(vm) {
        let r = filter_values(&mut VmQuery::new(vm), &arr, filter, flags);
        arr = r.vm(vm)?;
    }
    a.set_ref(1, Value::Array(arr));
    Ok(Value::Null)
}

/// `zend_parse_arg_path_str()` for a scalar locale argument.
fn locale_arg(vm: &mut Vm, n: usize, v: Value, strict: bool) -> VmResult<Value> {
    let label = if n == 2 {
        "#2 ($locales)".to_string()
    } else {
        format!("#{n}")
    };
    let expected = if n == 2 {
        "array|string|null"
    } else {
        "?string"
    };
    let parsed = match &v {
        Value::Null | Value::Str(_) => Some(v.clone()),
        Value::Int(_) | Value::Float(_) | Value::Bool(_) if !strict => {
            Some(Value::Str(vm.value_to_string(&v)?))
        }
        Value::Object(o) if !strict => vm.cast_object(o, CastTarget::String).vm(vm)?,
        _ => None,
    };
    let Some(v) = parsed else {
        return Err(vm.throw(
            "TypeError",
            &format!(
                "setlocale(): Argument {label} must be of type {expected}, {} given",
                v.value_name()
            ),
        ));
    };
    if let Value::Str(s) = &v {
        if s.as_bytes().contains(&0) {
            return Err(vm.throw(
                "ValueError",
                &format!("setlocale(): Argument {label} must not contain any null bytes"),
            ));
        }
    }
    Ok(v)
}

/// `setlocale($category, $locales, ...$rest)`: the arguments are tried in
/// order after all scalar arguments have been validated.
fn setlocale(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cat = a.int(0);
    let mut callers = vm.frames.iter().rev().skip(1);
    let mut caller = callers.next();
    if caller.is_some_and(|f| {
        f.kind == php_vm::vm::FrameKind::Builtin
            && f.builtin_name.is_none()
            && f.func.as_ref().is_some_and(|func| {
                matches!(
                    func.name().as_bytes(),
                    b"call_user_func" | b"call_user_func_array"
                )
            })
    }) {
        caller = callers.next();
    }
    let strict =
        caller.is_some_and(|f| f.kind != php_vm::vm::FrameKind::Builtin && f.code.strict_types);
    let mut args = Vec::with_capacity(a.values.len().saturating_sub(1));
    for (i, v) in a.values.iter().skip(1).map(Value::deref).enumerate() {
        if i == 0 && matches!(v, Value::Array(_)) {
            if a.values.len() > 2 {
                return Err(vm.throw(
                    "ArgumentCountError",
                    &format!(
                        "setlocale() expects exactly 2 arguments when argument #2 ($locales) is an array, {} given",
                        a.values.len()
                    ),
                ));
            }
            args.push(v);
        } else {
            args.push(locale_arg(vm, i + 2, v, strict)?);
        }
    }
    let r = LOCALE.with(|l| {
        let mut st = l.borrow().clone();
        let r = st.setlocale(cat, &args, vm);
        *l.borrow_mut() = st;
        r
    });
    publish_locale();
    r.vm(vm)
}

/// Publishes the thread's locale facts after it may have changed: the
/// `LC_NUMERIC` decimal point for the printf engine (`LCONV_DECIMAL_POINT`
/// in `ext/standard/formatted_print.c` and `main/snprintf.c`) and
/// `BG(ctype_string)` for PCRE's character tables.
fn publish_locale() {
    php_support::fmt::locale::set_decimal_point(locale::decimal_point());
    LOCALE.with(|l| {
        php_support::fmt::locale::set_ctype_name(
            l.borrow().ctype_string.as_ref().map(ZStr::as_bytes),
        );
    });
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn localeconv(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Array(locale::localeconv()))
}

fn nl_langinfo(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    locale::nl_langinfo(a.int(0), vm).vm(vm)
}

/// Registers the functions and constants.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, v) in locale::CONSTANTS {
        m.constant(name, Value::Int(*v));
    }
    m.constant("PHP_QUERY_RFC1738", Value::Int(query::PHP_QUERY_RFC1738))
        .constant("PHP_QUERY_RFC3986", Value::Int(query::PHP_QUERY_RFC3986));
    m.function_static(
        "highlight_string(string $string, bool $return = false): string|true",
        highlight_string,
    )
    .function_static(
        "highlight_file(string $filename, bool $return = false): string|bool",
        highlight_file,
    )
    .function_static(
        "show_source(string $filename, bool $return = false): string|bool",
        highlight_file,
    )
    .function_static("php_strip_whitespace(string $filename): string", php_strip_whitespace)
    .function_static(
        "http_build_query(array|object $data, string $numeric_prefix = \"\", ?string $arg_separator = null, int $encoding_type = PHP_QUERY_RFC1738): string",
        http_build_query,
    )
    .function_static("parse_str(string $string, &$result): void", parse_str)
    .function_static("request_parse_body(?array $options = null): array", request_parse_body)
    .function_static("setlocale(int $category, $locales, ...$rest): string|false", setlocale)
    .function_static("localeconv(): array", localeconv)
    .function_static("nl_langinfo(int $item): string|false", nl_langinfo)
    .on_request_shutdown(|_| {
        LOCALE.with(|l| l.borrow_mut().request_shutdown());
        publish_locale();
    });
    locale::reset_lc_ctype();
    publish_locale();
}
