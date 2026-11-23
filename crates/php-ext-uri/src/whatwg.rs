// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Uri\WhatWg\Url`, `Uri\WhatWg\UrlBuilder`, `Uri\WhatWg\InvalidUrlException`,
//! `Uri\WhatWg\UrlValidationError` and `Uri\WhatWg\url_percent_encode()`
//! (ref: `ext/uri/php_uri.c`, `ext/uri/uri_parser_whatwg.c`).

use php_runtime::{Obj, Value};
use php_uri::api::whatwg::{self as api, WhatWgUrl};
use php_uri::whatwg::UrlPercentEncodingMode;
use php_uri::{UriError, UrlValidationError};
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, ClassHooks, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::common::{self, bytes, int_or_null, opt_bytes, opt_int, raw_prop, str_or_null, throw};

/// `Uri\WhatWg\Url`.
const URL_CLASS: &str = api::CLASS_NAME;
/// `Uri\WhatWg\UrlBuilder`.
const BUILDER_CLASS: &str = "Uri\\WhatWg\\UrlBuilder";
/// `Uri\WhatWg\InvalidUrlException`.
const INVALID_URL_EXCEPTION: &str = "Uri\\WhatWg\\InvalidUrlException";
/// `Uri\WhatWg\UrlHostType`.
const HOST_TYPE_ENUM: &str = "Uri\\WhatWg\\UrlHostType";

/// The native state of a `Uri\WhatWg\Url`; `None` until initialized.
#[derive(Debug, Default)]
struct State(Option<WhatWgUrl>);

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    php_vm::object::set_internal(o, State::default());
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn clone(_vm: &mut Vm, old: &Obj, new: &Obj) -> VmResult<()> {
    let url = state_of(old);
    php_vm::object::set_internal(new, State(url));
    Ok(())
}

fn state_of(o: &Obj) -> Option<WhatWgUrl> {
    php_vm::object::internal::<State, _>(o, |s| s.0.clone()).flatten()
}

fn set_state(o: &Obj, url: WhatWgUrl) {
    php_vm::object::set_internal(o, State(Some(url)));
}

fn this(vm: &mut Vm, a: &Args) -> VmResult<WhatWgUrl> {
    match a.this.as_ref().and_then(state_of) {
        Some(u) => Ok(u),
        None => Err(vm.throw(
            "Error",
            &format!("{URL_CLASS} object is not correctly initialized"),
        )),
    }
}

fn url_arg(a: &Args, n: usize) -> Option<WhatWgUrl> {
    match a.opt(n) {
        Some(Value::Object(o)) => state_of(&o),
        _ => None,
    }
}

/// A library result, after the memory check: a parse cut short at
/// `memory_limit` raises the fatal, not the library's error.
fn lib<T>(vm: &mut Vm, r: Result<T, UriError>) -> VmResult<T> {
    php_memlimit::poll()?;
    r.map_err(|e| throw(vm, &e))
}

/// Assigns `$softErrors`/`$errors` (by-reference argument `n`) when passed.
fn pass_errors(vm: &mut Vm, a: &Args, n: usize, errors: &[UrlValidationError]) -> VmResult<()> {
    if a.has(n) {
        let v = common::validation_errors(vm, errors)?;
        a.set_ref(n, v);
    }
    Ok(())
}

// ------------------------------------------------------------ construction

fn construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let this = a.this_obj();
    if state_of(&this).is_some() {
        return Err(common::throw_reinit(vm, URL_CLASS));
    }
    let base = url_arg(a, 1);
    let r = WhatWgUrl::new(a.str(0).as_bytes(), base.as_ref());
    php_memlimit::poll()?;
    match r {
        Ok((url, soft)) => {
            pass_errors(vm, a, 2, &soft)?;
            set_state(&this, url);
            Ok(Value::Null)
        }
        Err(e) => {
            let t = throw(vm, &e);
            pass_errors(vm, a, 2, &[])?;
            Err(t)
        }
    }
}

fn parse(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cls = common::class(vm, URL_CLASS)?;
    let o = vm.new_object(&cls)?;
    let base = url_arg(a, 1);
    let r = WhatWgUrl::parse(a.str(0).as_bytes(), base.as_ref());
    php_memlimit::poll()?;
    match r {
        Ok((url, soft)) => {
            pass_errors(vm, a, 2, &soft)?;
            set_state(&o, url);
            Ok(Value::Object(o))
        }
        Err(f) => {
            pass_errors(vm, a, 2, &f.errors)?;
            Ok(Value::Null)
        }
    }
}

// ------------------------------------------------------------ getters

macro_rules! getters {
    ($($name:ident => $body:expr;)*) => {
        $(fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let u = this(vm, a)?;
            #[allow(clippy::redundant_closure_call)]
            Ok(($body)(&u))
        })*
    };
}

getters! {
    get_scheme => |u: &WhatWgUrl| bytes(u.scheme());
    is_special_scheme => |u: &WhatWgUrl| Value::Bool(u.is_special_scheme());
    get_username => |u: &WhatWgUrl| opt_bytes(u.username());
    get_password => |u: &WhatWgUrl| opt_bytes(u.password());
    get_ascii_host => |u: &WhatWgUrl| opt_bytes(u.ascii_host());
    get_unicode_host => |u: &WhatWgUrl| opt_bytes(u.unicode_host());
    get_port => |u: &WhatWgUrl| opt_int(u.port());
    get_path => |u: &WhatWgUrl| bytes(u.path());
    get_query => |u: &WhatWgUrl| opt_bytes(u.query());
    get_fragment => |u: &WhatWgUrl| opt_bytes(u.fragment());
    to_ascii_string => |u: &WhatWgUrl| bytes(u.to_ascii_string());
    debug_info => |u: &WhatWgUrl| common::debug_info(u.debug_info());
    serialize => |u: &WhatWgUrl| common::serialized(u.serialize());
}

fn to_unicode_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    Ok(bytes(u.to_unicode_string()))
}

fn get_host_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    match u.host_type() {
        Some(t) => common::enum_case(vm, HOST_TYPE_ENUM, t.name()),
        None => Ok(Value::Null),
    }
}

// ------------------------------------------------------------ withers

#[allow(clippy::many_single_char_names)] // a: args, u: URL, o: object, ...
fn with(
    vm: &mut Vm,
    a: &Args,
    f: impl FnOnce(&WhatWgUrl) -> Result<WhatWgUrl, UriError>,
) -> VmResult<Value> {
    let u = this(vm, a)?;
    let cls = php_vm::object::class_of(&a.this_obj()).clone();
    let o = vm.new_object(&cls)?;
    let new = lib(vm, f(&u))?;
    set_state(&o, new);
    Ok(Value::Object(o))
}

fn with_scheme(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.str(0);
    with(vm, a, |u| u.with_scheme(v.as_bytes()))
}

fn with_username(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_username(v.as_deref()))
}

fn with_password(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_password(v.as_deref()))
}

fn with_host(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_host(v.as_deref()))
}

fn with_port(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = int_or_null(a, 0);
    with(vm, a, |u| u.with_port(v))
}

fn with_path(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.str(0);
    with(vm, a, |u| u.with_path(v.as_bytes()))
}

fn with_query(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_query(v.as_deref()))
}

fn with_fragment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_fragment(v.as_deref()))
}

// ------------------------------------------------------------ the rest

fn equals(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    let other = url_arg(a, 0);
    let mode = common::comparison_mode(vm, a.opt(1))?;
    Ok(Value::Bool(other.is_some_and(|o| u.equals(&o, mode))))
}

#[allow(clippy::many_single_char_names)] // a: args, u: URL, o: object, ...
fn resolve(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    let cls = php_vm::object::class_of(&a.this_obj()).clone();
    let o = vm.new_object(&cls)?;
    let r = u.resolve(a.str(0).as_bytes());
    php_memlimit::poll()?;
    match r {
        Ok((url, soft)) => {
            pass_errors(vm, a, 1, &soft)?;
            set_state(&o, url);
            Ok(Value::Object(o))
        }
        Err(e) => {
            let t = throw(vm, &e);
            pass_errors(vm, a, 1, &[])?;
            Err(t)
        }
    }
}

fn unserialize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let this = a.this_obj();
    if state_of(&this).is_some() {
        return Err(common::throw_invalid_serialization(vm, URL_CLASS, true));
    }
    let data = a.array(0);
    let url = common::unserialize_uri(&data)
        .and_then(|s| WhatWgUrl::unserialize(&s).ok())
        .filter(|_| common::unserialize_props_ok(&data));
    let Some(url) = url else {
        return Err(common::throw_invalid_serialization(vm, URL_CLASS, false));
    };
    set_state(&this, url);
    Ok(Value::Null)
}

// ------------------------------------------------------------ UrlBuilder

fn builder_this(a: &Args) -> Value {
    Value::Object(a.this_obj())
}

#[allow(clippy::unnecessary_wraps)] // the native function signature
fn b_reset(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    for p in [
        "scheme", "username", "password", "host", "port", "path", "query", "fragment",
    ] {
        let v = if p == "scheme" || p == "path" {
            Value::string("")
        } else {
            Value::Null
        };
        vm.set_property_raw(&o, p, v);
    }
    Ok(builder_this(a))
}

fn b_set(
    vm: &mut Vm,
    a: &Args,
    prop: &str,
    validate: impl FnOnce() -> Result<(), UriError>,
) -> VmResult<Value> {
    lib(vm, validate())?;
    let o = a.this_obj();
    vm.set_property_raw(&o, prop, a.at(0));
    Ok(builder_this(a))
}

fn b_set_scheme(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.str(0);
    b_set(vm, a, "scheme", || api::validate_scheme(v.as_bytes()))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_set_username(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    b_set(vm, a, "username", || Ok(()))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_set_password(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    b_set(vm, a, "password", || Ok(()))
}

fn b_set_host(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    b_set(vm, a, "host", || {
        v.as_deref().map_or(Ok(()), api::validate_host)
    })
}

fn b_set_port(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = int_or_null(a, 0);
    b_set(vm, a, "port", || v.map_or(Ok(()), api::validate_port))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_set_path(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    b_set(vm, a, "path", || Ok(()))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_set_query(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    b_set(vm, a, "query", || Ok(()))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_set_fragment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    b_set(vm, a, "fragment", || Ok(()))
}

#[allow(clippy::many_single_char_names)] // a: args, u: URL, o: object, ...
fn b_build(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let s = |n: &str| match raw_prop(&o, n) {
        Value::Str(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    };
    let (scheme, username, password, host, path, query, fragment) = (
        s("scheme").unwrap_or_default(),
        s("username"),
        s("password"),
        s("host"),
        s("path").unwrap_or_default(),
        s("query"),
        s("fragment"),
    );
    let port = match raw_prop(&o, "port") {
        Value::Int(n) => Some(n),
        _ => None,
    };
    let base = url_arg(a, 0);
    let c = api::Components {
        scheme: &scheme,
        username: username.as_deref(),
        password: password.as_deref(),
        host: host.as_deref(),
        port,
        path: &path,
        query: query.as_deref(),
        fragment: fragment.as_deref(),
    };
    match api::build_from_components(base.as_ref().map(WhatWgUrl::url), &c) {
        Ok((url, soft)) => {
            pass_errors(vm, a, 1, &soft)?;
            let cls = common::class(vm, URL_CLASS)?;
            let o = vm.new_object(&cls)?;
            set_state(&o, WhatWgUrl::from_url(url));
            Ok(Value::Object(o))
        }
        Err(e) => {
            let t = throw(vm, &e);
            pass_errors(vm, a, 1, &[])?;
            Err(t)
        }
    }
}

// ------------------------------------------------------------ exceptions

/// `Uri\WhatWg\InvalidUrlException::__construct(string $message = "", array
/// $errors = [], int $code = 0, ?Throwable $previous = null)`.
fn exception_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    // `zend_update_exception_properties()`: only the passed arguments.
    if a.has(0) {
        vm.set_property_raw(&o, "message", Value::Str(a.str(0)));
    }
    if a.has(2) && a.int(2) != 0 {
        vm.set_property_raw(&o, "code", Value::Int(a.int(2)));
    }
    if let Some(previous) = a.opt(3) {
        vm.set_property_raw(&o, "previous", previous);
    }
    let errors = if a.has(1) {
        let errors = a.array(1);
        let class = common::class(vm, common::VALIDATION_ERROR_CLASS)?;
        let ok = errors.is_list()
            && errors.iter().all(|(_, v)| match v {
                Value::Object(e) => php_vm::object::class_of(e).is_subclass_of(&class),
                _ => false,
            });
        if !ok {
            let e = php_uri::api::invalid_url_exception_errors_value_error();
            return Err(throw(vm, &e));
        }
        Value::Array(errors)
    } else {
        Value::Array(php_runtime::ZArray::new())
    };
    // `zend_update_property_ex()` writes with the class as (fake) scope:
    // the implicit protected(set) of the readonly property allows it once.
    let scope = common::class(vm, INVALID_URL_EXCEPTION)?;
    vm.set_property_scoped(&o, "errors", errors, &scope)?;
    Ok(Value::Null)
}

/// `Uri\WhatWg\UrlValidationError::__construct(string $context,
/// UrlValidationErrorType $type, bool $failure)`.
fn validation_error_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    // Written with the class as scope, like `zend_update_property_ex()`.
    let scope = common::class(vm, common::VALIDATION_ERROR_CLASS)?;
    vm.set_property_scoped(&o, "context", Value::Str(a.str(0)), &scope)?;
    vm.set_property_scoped(&o, "type", a.at(1), &scope)?;
    vm.set_property_scoped(&o, "failure", Value::Bool(a.bool(2)), &scope)?;
    Ok(Value::Null)
}

/// `Uri\WhatWg\url_percent_encode(string $input, UrlPercentEncodingMode
/// $mode): string`.
#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn url_percent_encode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = common::enum_case_name(vm, &a.at(1))?;
    let mode = name
        .as_deref()
        .and_then(UrlPercentEncodingMode::from_name)
        .unwrap_or(UrlPercentEncodingMode::Fragment);
    Ok(bytes(api::url_percent_encode(a.str(0).as_bytes(), mode)))
}

// ------------------------------------------------------------ registration

/// Registers the `Uri\WhatWg` builder and URL classes.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    let p = acc::PRIVATE;
    ClassBuilder::new(BUILDER_CLASS)
        .flags(class_flags::FINAL)
        .prop("scheme", p, Some("string"), Some(Value::string("")))
        .prop("username", p, Some("?string"), Some(Value::Null))
        .prop("password", p, Some("?string"), Some(Value::Null))
        .prop("host", p, Some("?string"), Some(Value::Null))
        .prop("port", p, Some("?int"), Some(Value::Null))
        .prop("path", p, Some("string"), Some(Value::string("")))
        .prop("query", p, Some("?string"), Some(Value::Null))
        .prop("fragment", p, Some("?string"), Some(Value::Null))
        .method("reset(): static", acc::PUBLIC, b_reset)
        .method(
            "setScheme(string $scheme): static",
            acc::PUBLIC,
            b_set_scheme,
        )
        .method(
            "setUsername(?string $username): static",
            acc::PUBLIC,
            b_set_username,
        )
        .method(
            "setPassword(#[\\SensitiveParameter] ?string $password): static",
            acc::PUBLIC,
            b_set_password,
        )
        .method("setHost(?string $host): static", acc::PUBLIC, b_set_host)
        .method("setPort(?int $port): static", acc::PUBLIC, b_set_port)
        .method("setPath(string $path): static", acc::PUBLIC, b_set_path)
        .method("setQuery(?string $query): static", acc::PUBLIC, b_set_query)
        .method(
            "setFragment(?string $fragment): static",
            acc::PUBLIC,
            b_set_fragment,
        )
        .method(
            "build(?Uri\\WhatWg\\Url $baseUrl = null, &$softErrors = null): Uri\\WhatWg\\Url",
            acc::PUBLIC,
            b_build,
        )
        .register(m.vm());

    let pb = acc::PUBLIC;
    ClassBuilder::new(URL_CLASS)
        .flags(class_flags::FINAL | class_flags::NO_DYNAMIC_PROPERTIES | class_flags::READONLY)
        .hooks(ClassHooks {
            create: Some(create),
            clone: Some(clone),
            ..Default::default()
        })
        .method(
            "parse(string $uri, ?Uri\\WhatWg\\Url $baseUrl = null, &$errors = null): ?static",
            pb | acc::STATIC,
            parse,
        )
        .method(
            "__construct(string $uri, ?Uri\\WhatWg\\Url $baseUrl = null, &$softErrors = null)",
            pb,
            construct,
        )
        .method("getScheme(): string", pb, get_scheme)
        .method("withScheme(string $scheme): static", pb, with_scheme)
        .method("isSpecialScheme(): bool", pb, is_special_scheme)
        .method("getUsername(): ?string", pb, get_username)
        .method("withUsername(?string $username): static", pb, with_username)
        .method("getPassword(): ?string", pb, get_password)
        .method("withPassword(#[\\SensitiveParameter] ?string $password): static", pb, with_password)
        .method("getAsciiHost(): ?string", pb, get_ascii_host)
        .method("getUnicodeHost(): ?string", pb, get_unicode_host)
        .method("getHostType(): ?Uri\\WhatWg\\UrlHostType", pb, get_host_type)
        .method("withHost(?string $host): static", pb, with_host)
        .method("getPort(): ?int", pb, get_port)
        .method("withPort(?int $port): static", pb, with_port)
        .method("getPath(): string", pb, get_path)
        .method("withPath(string $path): static", pb, with_path)
        .method("getQuery(): ?string", pb, get_query)
        .method("withQuery(?string $query): static", pb, with_query)
        .method("getFragment(): ?string", pb, get_fragment)
        .method("withFragment(?string $fragment): static", pb, with_fragment)
        .method(
            "equals(Uri\\WhatWg\\Url $url, Uri\\UriComparisonMode $comparisonMode = Uri\\UriComparisonMode::ExcludeFragment): bool",
            pb,
            equals,
        )
        .method("toAsciiString(): string", pb, to_ascii_string)
        .method("toUnicodeString(): string", pb, to_unicode_string)
        .method("resolve(string $uri, &$softErrors = null): static", pb, resolve)
        .method("__serialize(): array", pb, serialize)
        .method("__unserialize(array $data): void", pb, unserialize)
        .method("__debugInfo(): array", pb, debug_info)
        .register(m.vm());

    m.function_static(
        "Uri\\WhatWg\\url_percent_encode(string $input, Uri\\WhatWg\\UrlPercentEncodingMode $mode): string",
        url_percent_encode,
    );
}

/// Registers `Uri\WhatWg\InvalidUrlException` (after
/// `Uri\InvalidUriException`) and `Uri\WhatWg\UrlValidationError`.
pub(crate) fn register_exception(m: &mut ModuleBuilder<'_>) {
    ClassBuilder::new(INVALID_URL_EXCEPTION)
        .extends(m.vm(), "Uri\\InvalidUriException")
        .flags(class_flags::NO_DYNAMIC_PROPERTIES)
        .prop("errors", acc::PUBLIC | acc::READONLY, Some("array"), None)
        .method(
            "__construct(string $message = \"\", array $errors = [], int $code = 0, ?Throwable $previous = null)",
            acc::PUBLIC,
            exception_construct,
        )
        .register(m.vm());
}

/// Registers `Uri\WhatWg\UrlValidationError`.
pub(crate) fn register_validation_error(m: &mut ModuleBuilder<'_>) {
    let ro = acc::PUBLIC | acc::READONLY;
    ClassBuilder::new(common::VALIDATION_ERROR_CLASS)
        .flags(class_flags::FINAL | class_flags::NO_DYNAMIC_PROPERTIES | class_flags::READONLY)
        .prop("context", ro, Some("string"), None)
        .prop("type", ro, Some("Uri\\WhatWg\\UrlValidationErrorType"), None)
        .prop("failure", ro, Some("bool"), None)
        .method(
            "__construct(string $context, Uri\\WhatWg\\UrlValidationErrorType $type, bool $failure)",
            acc::PUBLIC,
            validation_error_construct,
        )
        .register(m.vm());
}
