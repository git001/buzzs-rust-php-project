// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Uri\Rfc3986\Uri` and `Uri\Rfc3986\UriBuilder` (ref: `ext/uri/php_uri.c`,
//! `ext/uri/uri_parser_rfc3986.c`).

use php_runtime::{Obj, Value};
use php_uri::api::rfc3986::{self as api, Rfc3986Uri};
use php_uri::UriError;
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, ClassHooks, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::common::{self, bytes, int_or_null, opt_bytes, opt_int, raw_prop, str_or_null, throw};

/// `Uri\Rfc3986\Uri`.
pub(crate) const URI_CLASS: &str = api::CLASS_NAME;
/// `Uri\Rfc3986\UriBuilder`.
const BUILDER_CLASS: &str = "Uri\\Rfc3986\\UriBuilder";
/// `Uri\Rfc3986\UriType`.
const URI_TYPE_ENUM: &str = "Uri\\Rfc3986\\UriType";
/// `Uri\Rfc3986\UriHostType`.
const HOST_TYPE_ENUM: &str = "Uri\\Rfc3986\\UriHostType";

/// The native state of a `Uri\Rfc3986\Uri` (`php_uri_object::uri`); `None`
/// until the constructor or `__unserialize()` ran.
#[derive(Debug, Default)]
struct State(Option<Rfc3986Uri>);

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    php_vm::object::set_internal(o, State::default());
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn clone(_vm: &mut Vm, old: &Obj, new: &Obj) -> VmResult<()> {
    let uri = state_of(old);
    php_vm::object::set_internal(new, State(uri));
    Ok(())
}

fn state_of(o: &Obj) -> Option<Rfc3986Uri> {
    php_vm::object::internal::<State, _>(o, |s| s.0.clone()).flatten()
}

fn set_state(o: &Obj, uri: Rfc3986Uri) {
    php_vm::object::set_internal(o, State(Some(uri)));
}

/// `$this`'s URI (always initialized once a method can be called).
fn this(vm: &mut Vm, a: &Args) -> VmResult<Rfc3986Uri> {
    match a.this.as_ref().and_then(state_of) {
        Some(u) => Ok(u),
        None => Err(vm.throw(
            "Error",
            &format!("{URI_CLASS} object is not correctly initialized"),
        )),
    }
}

/// A new `Uri\Rfc3986\Uri` holding `uri`.
fn new_uri(vm: &mut Vm, uri: Rfc3986Uri) -> VmResult<Value> {
    let cls = common::class(vm, URI_CLASS)?;
    let o = vm.new_object(&cls)?;
    set_state(&o, uri);
    Ok(Value::Object(o))
}

/// The optional `?Uri\Rfc3986\Uri $baseUrl` argument `n`.
fn base_arg(a: &Args, n: usize) -> Option<Rfc3986Uri> {
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

// ------------------------------------------------------------ construction

fn construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let this = a.this_obj();
    if state_of(&this).is_some() {
        return Err(common::throw_reinit(vm, URI_CLASS));
    }
    let base = base_arg(a, 1);
    let uri = Rfc3986Uri::new(a.str(0).as_bytes(), base.as_ref());
    let uri = lib(vm, uri)?;
    set_state(&this, uri);
    Ok(Value::Null)
}

fn parse(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // `object_init_ex()` happens before parsing.
    let cls = common::class(vm, URI_CLASS)?;
    let o = vm.new_object(&cls)?;
    let base = base_arg(a, 1);
    match Rfc3986Uri::parse(a.str(0).as_bytes(), base.as_ref()) {
        Some(uri) => {
            set_state(&o, uri);
            Ok(Value::Object(o))
        }
        None => Ok(Value::Null),
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
    get_scheme => |u: &Rfc3986Uri| opt_bytes(u.scheme());
    get_raw_scheme => |u: &Rfc3986Uri| opt_bytes(u.raw_scheme());
    get_user_info => |u: &Rfc3986Uri| opt_bytes(u.user_info());
    get_raw_user_info => |u: &Rfc3986Uri| opt_bytes(u.raw_user_info());
    get_username => |u: &Rfc3986Uri| opt_bytes(u.username());
    get_raw_username => |u: &Rfc3986Uri| opt_bytes(u.raw_username());
    get_password => |u: &Rfc3986Uri| opt_bytes(u.password());
    get_raw_password => |u: &Rfc3986Uri| opt_bytes(u.raw_password());
    get_host => |u: &Rfc3986Uri| opt_bytes(u.host());
    get_raw_host => |u: &Rfc3986Uri| opt_bytes(u.raw_host());
    get_port => |u: &Rfc3986Uri| opt_int(u.port());
    get_path => |u: &Rfc3986Uri| bytes(u.path());
    get_raw_path => |u: &Rfc3986Uri| bytes(u.raw_path());
    get_query => |u: &Rfc3986Uri| opt_bytes(u.query());
    get_raw_query => |u: &Rfc3986Uri| opt_bytes(u.raw_query());
    get_fragment => |u: &Rfc3986Uri| opt_bytes(u.fragment());
    get_raw_fragment => |u: &Rfc3986Uri| opt_bytes(u.raw_fragment());
    to_string => |u: &Rfc3986Uri| bytes(u.to_string_normalized());
    to_raw_string => |u: &Rfc3986Uri| bytes(u.to_raw_string());
    debug_info => |u: &Rfc3986Uri| common::debug_info(u.debug_info());
    serialize => |u: &Rfc3986Uri| common::serialized(u.serialize());
}

fn get_uri_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    common::enum_case(vm, URI_TYPE_ENUM, u.uri_type().name())
}

fn get_host_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    match u.host_type() {
        Some(t) => common::enum_case(vm, HOST_TYPE_ENUM, t.name()),
        None => Ok(Value::Null),
    }
}

// ------------------------------------------------------------ withers

/// `php_uri_property_write_helper`: clones `$this` first, then applies the
/// change to the clone.
fn with(
    vm: &mut Vm,
    a: &Args,
    f: impl FnOnce(&Rfc3986Uri) -> Result<Rfc3986Uri, UriError>,
) -> VmResult<Value> {
    let u = this(vm, a)?;
    let cls = php_vm::object::class_of(&a.this_obj()).clone();
    let o = vm.new_object(&cls)?;
    let new = lib(vm, f(&u))?;
    set_state(&o, new);
    Ok(Value::Object(o))
}

fn with_scheme(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_scheme(v.as_deref()))
}

fn with_user_info(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    with(vm, a, |u| u.with_user_info(v.as_deref()))
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
    let other = base_arg(a, 0);
    let mode = common::comparison_mode(vm, a.opt(1))?;
    Ok(Value::Bool(other.is_some_and(|o| u.equals(&o, mode))))
}

fn resolve(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let u = this(vm, a)?;
    let cls = php_vm::object::class_of(&a.this_obj()).clone();
    let o = vm.new_object(&cls)?;
    let r = lib(vm, u.resolve(a.str(0).as_bytes()))?;
    set_state(&o, r);
    Ok(Value::Object(o))
}

fn unserialize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let this = a.this_obj();
    if state_of(&this).is_some() {
        return Err(common::throw_invalid_serialization(vm, URI_CLASS, true));
    }
    let data = a.array(0);
    let uri = common::unserialize_uri(&data)
        .and_then(|s| Rfc3986Uri::unserialize(&s).ok())
        .filter(|_| common::unserialize_props_ok(&data));
    let Some(uri) = uri else {
        return Err(common::throw_invalid_serialization(vm, URI_CLASS, false));
    };
    set_state(&this, uri);
    Ok(Value::Null)
}

// ------------------------------------------------------------ UriBuilder

/// The builder's private properties, in declaration order.
const BUILDER_PROPS: [&str; 7] = [
    "scheme", "userinfo", "host", "port", "path", "query", "fragment",
];

fn builder_this(a: &Args) -> Value {
    Value::Object(a.this_obj())
}

fn builder_set(
    vm: &mut Vm,
    a: &Args,
    prop: &str,
    validate: impl FnOnce(&mut api::Rfc3986UriBuilder) -> Result<(), UriError>,
) -> VmResult<Value> {
    let mut b = api::Rfc3986UriBuilder::new();
    lib(vm, validate(&mut b))?;
    let o = a.this_obj();
    vm.set_property_raw(&o, prop, a.at(0));
    Ok(builder_this(a))
}

#[allow(clippy::unnecessary_wraps)] // the native hook/function signature
fn b_reset(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    for p in BUILDER_PROPS {
        let v = if p == "path" {
            Value::string("")
        } else {
            Value::Null
        };
        vm.set_property_raw(&o, p, v);
    }
    Ok(builder_this(a))
}

fn b_set_scheme(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    builder_set(vm, a, "scheme", |b| b.set_scheme(v.as_deref()))
}

fn b_set_user_info(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    builder_set(vm, a, "userinfo", |b| b.set_user_info(v.as_deref()))
}

fn b_set_host(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    builder_set(vm, a, "host", |b| b.set_host(v.as_deref()))
}

fn b_set_port(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = int_or_null(a, 0);
    builder_set(vm, a, "port", |b| b.set_port(v))
}

fn b_set_path(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.str(0);
    builder_set(vm, a, "path", |b| b.set_path(v.as_bytes()))
}

fn b_set_query(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    builder_set(vm, a, "query", |b| b.set_query(v.as_deref()))
}

fn b_set_fragment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = str_or_null(a, 0);
    builder_set(vm, a, "fragment", |b| b.set_fragment(v.as_deref()))
}

fn b_build(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let s = |n: &str| match raw_prop(&o, n) {
        Value::Str(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    };
    let port = match raw_prop(&o, "port") {
        Value::Int(n) => Some(n),
        _ => None,
    };
    let base = base_arg(a, 0);
    let uri = Rfc3986Uri::build_from_components(
        base.as_ref(),
        s("scheme").as_deref(),
        s("userinfo").as_deref(),
        s("host").as_deref(),
        port,
        &s("path").unwrap_or_default(),
        s("query").as_deref(),
        s("fragment").as_deref(),
    );
    let uri = lib(vm, uri)?;
    new_uri(vm, uri)
}

// ------------------------------------------------------------ registration

/// Registers `Uri\Rfc3986\UriBuilder` and `Uri\Rfc3986\Uri`.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    let p = acc::PRIVATE;
    ClassBuilder::new(BUILDER_CLASS)
        .flags(class_flags::FINAL | class_flags::NO_DYNAMIC_PROPERTIES)
        .prop("scheme", p, Some("?string"), Some(Value::Null))
        .prop("userinfo", p, Some("?string"), Some(Value::Null))
        .prop("host", p, Some("?string"), Some(Value::Null))
        .prop("port", p, Some("?int"), Some(Value::Null))
        .prop("path", p, Some("string"), Some(Value::string("")))
        .prop("query", p, Some("?string"), Some(Value::Null))
        .prop("fragment", p, Some("?string"), Some(Value::Null))
        .method("reset(): static", acc::PUBLIC, b_reset)
        .method(
            "setScheme(?string $scheme): static",
            acc::PUBLIC,
            b_set_scheme,
        )
        .method(
            "setUserInfo(#[\\SensitiveParameter] ?string $userInfo): static",
            acc::PUBLIC,
            b_set_user_info,
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
            "build(?Uri\\Rfc3986\\Uri $baseUrl = null): Uri\\Rfc3986\\Uri",
            acc::PUBLIC,
            b_build,
        )
        .register(m.vm());

    let pb = acc::PUBLIC;
    ClassBuilder::new(URI_CLASS)
        .flags(class_flags::FINAL | class_flags::NO_DYNAMIC_PROPERTIES | class_flags::READONLY)
        .hooks(ClassHooks {
            create: Some(create),
            clone: Some(clone),
            ..Default::default()
        })
        .method(
            "parse(string $uri, ?Uri\\Rfc3986\\Uri $baseUrl = null): ?static",
            pb | acc::STATIC,
            parse,
        )
        .method(
            "__construct(string $uri, ?Uri\\Rfc3986\\Uri $baseUrl = null)",
            pb,
            construct,
        )
        .method("getUriType(): ?Uri\\Rfc3986\\UriType", pb, get_uri_type)
        .method("getScheme(): ?string", pb, get_scheme)
        .method("getRawScheme(): ?string", pb, get_raw_scheme)
        .method("withScheme(?string $scheme): static", pb, with_scheme)
        .method("getUserInfo(): ?string", pb, get_user_info)
        .method("getRawUserInfo(): ?string", pb, get_raw_user_info)
        .method("withUserInfo(#[\\SensitiveParameter] ?string $userinfo): static", pb, with_user_info)
        .method("getUsername(): ?string", pb, get_username)
        .method("getRawUsername(): ?string", pb, get_raw_username)
        .method("getPassword(): ?string", pb, get_password)
        .method("getRawPassword(): ?string", pb, get_raw_password)
        .method("getHost(): ?string", pb, get_host)
        .method("getRawHost(): ?string", pb, get_raw_host)
        .method("getHostType(): ?Uri\\Rfc3986\\UriHostType", pb, get_host_type)
        .method("withHost(?string $host): static", pb, with_host)
        .method("getPort(): ?int", pb, get_port)
        .method("withPort(?int $port): static", pb, with_port)
        .method("getPath(): string", pb, get_path)
        .method("getRawPath(): string", pb, get_raw_path)
        .method("withPath(string $path): static", pb, with_path)
        .method("getQuery(): ?string", pb, get_query)
        .method("getRawQuery(): ?string", pb, get_raw_query)
        .method("withQuery(?string $query): static", pb, with_query)
        .method("getFragment(): ?string", pb, get_fragment)
        .method("getRawFragment(): ?string", pb, get_raw_fragment)
        .method("withFragment(?string $fragment): static", pb, with_fragment)
        .method(
            "equals(Uri\\Rfc3986\\Uri $uri, Uri\\UriComparisonMode $comparisonMode = Uri\\UriComparisonMode::ExcludeFragment): bool",
            pb,
            equals,
        )
        .method("toString(): string", pb, to_string)
        .method("toRawString(): string", pb, to_raw_string)
        .method("resolve(string $uri): static", pb, resolve)
        .method("__serialize(): array", pb, serialize)
        .method("__unserialize(array $data): void", pb, unserialize)
        .method("__debugInfo(): array", pb, debug_info)
        .register(m.vm());
}
