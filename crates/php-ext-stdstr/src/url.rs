// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/url.c` and `base64.c`.

use php_runtime::Value;
use php_stdstr::base64;
use php_stdstr::url::{self, ParseUrlResult, UrlPart};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::{assoc, bytes, check, opt_bytes};

fn part_value(p: UrlPart) -> Value {
    match p {
        UrlPart::Str(s) => bytes(s),
        UrlPart::Port(p) => Value::Int(i64::from(p)),
    }
}

fn parse_url(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let component = if a.has(1) { a.int(1) } else { -1 };
    let r = url::parse_url(a.str(0).as_bytes(), component);
    Ok(match check(vm, r)? {
        ParseUrlResult::Failed => Value::Bool(false),
        ParseUrlResult::All(parsed) => assoc(
            parsed
                .entries()
                .into_iter()
                .map(|(k, v)| (k, part_value(v))),
        ),
        ParseUrlResult::Component(c) => c.map_or(Value::Null, part_value),
    })
}

fn urlencode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(url::urlencode(a.str(0).as_bytes())))
}

fn urldecode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(url::urldecode(a.str(0).as_bytes())))
}

fn rawurlencode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(url::rawurlencode(a.str(0).as_bytes())))
}

fn rawurldecode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(url::rawurldecode(a.str(0).as_bytes())))
}

fn base64_encode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    // `zend_string_safe_alloc(((len + 2) / 3), 4, 0, 0)`
    crate::mem::check(crate::mem::zstr(s.len().div_ceil(3).saturating_mul(4)))?;
    Ok(bytes(base64::base64_encode(s.as_bytes())))
}

fn base64_decode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(base64::base64_decode(
        a.str(0).as_bytes(),
        a.bool(1),
    )))
}

/// Registers the `url.c` / `base64.c` functions and constants.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "parse_url(string $url, int $component = -1): int|string|array|null|false",
        parse_url,
    )
    .function_static("urlencode(string $string): string", urlencode)
    .function_static("urldecode(string $string): string", urldecode)
    .function_static("rawurlencode(string $string): string", rawurlencode)
    .function_static("rawurldecode(string $string): string", rawurldecode)
    .function_static("base64_encode(string $string): string", base64_encode)
    .function_static(
        "base64_decode(string $string, bool $strict = false): string|false",
        base64_decode,
    );
    for (name, v) in [
        ("PHP_URL_SCHEME", 0),
        ("PHP_URL_HOST", 1),
        ("PHP_URL_PORT", 2),
        ("PHP_URL_USER", 3),
        ("PHP_URL_PASS", 4),
        ("PHP_URL_PATH", 5),
        ("PHP_URL_QUERY", 6),
        ("PHP_URL_FRAGMENT", 7),
        ("PHP_QUERY_RFC1738", 1),
        ("PHP_QUERY_RFC3986", 2),
    ] {
        m.constant(name, Value::Int(v));
    }
}
