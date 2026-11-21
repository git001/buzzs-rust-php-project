// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request variables: `$_GET`, `$_POST`, `$_COOKIE`, `$_FILES`,
//! `$_SERVER`, `$_ENV` and `$_REQUEST` (`php_hash_environment()`,
//! `php_apache_sapi_register_variables()`, `sapi_read_post_data()` and the
//! rfc1867 handler), and the body parse of `request_parse_body()`
//! ([`parse_body`]), which runs the same POST readers and handlers into
//! fresh arrays.

use crate::backend::{Backend, IntField, StrField, Table};
use crate::request::{Body, RequestCtx, POST_BLOCK_SIZE};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_sapi_input::rfc1867::{self, RegValue, Target, UploadSettings};
use php_stdmisc::query::{self, InputSettings, ParseKind, RegisterOptions};
use php_vm::ext::{ParseBodyOptions, Rfc1867Event};
use php_vm::{Throw, Vm, VmResult};
use std::path::PathBuf;

/// HTTP authentication data (`SG(request_info).auth_*`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Auth {
    /// `PHP_AUTH_USER`.
    pub user: Option<Vec<u8>>,
    /// `PHP_AUTH_PW`.
    pub password: Option<Vec<u8>>,
    /// `PHP_AUTH_DIGEST`.
    pub digest: Option<Vec<u8>>,
}

const STARTUP: &str = "PHP Request Startup: ";

fn ini_bytes(vm: &Vm, name: &str) -> Vec<u8> {
    vm.ini_string(name).unwrap_or_default()
}

fn input_settings(vm: &Vm) -> InputSettings {
    let sep = ini_bytes(vm, "arg_separator.input");
    InputSettings {
        arg_separator_input: if sep.is_empty() { b"&".to_vec() } else { sep },
        max_input_vars: vm.ini_long("max_input_vars"),
        max_input_nesting_level: vm.ini_long("max_input_nesting_level"),
        display_errors: crate::request::display_errors(vm),
    }
}

/// Emits a startup warning (`php_error_docref` during request startup or
/// `sapi_error`), located at `Unknown:0`.
fn startup_warning(vm: &mut Vm, message: &str, docref: bool) {
    let msg = if docref {
        format!("{STARTUP}{message}")
    } else {
        message.to_string()
    };
    let _ = vm.raise(2, &msg);
}

fn set_global(vm: &mut Vm, name: &str, arr: ZArray) {
    let mut g = vm.globals.borrow_mut();
    let i = g.find_or_add(name.as_bytes()) as usize;
    g.slots[i] = Some(Value::Array(arr));
}

/// `php_register_variable_safe()` into `track`.
fn register(vm: &mut Vm, name: &[u8], value: Value, track: &mut ZArray, cookie: bool) {
    let settings = input_settings(vm);
    let opts = RegisterOptions {
        max_nesting_level: settings.max_input_nesting_level,
        display_errors: settings.display_errors,
        cookie,
        warning_prefix: STARTUP,
    };
    let _ = query::register_variable_ex(name, value, track, &opts, vm);
}

/// The lower-cased content type without parameters, as SAPI.c looks it up.
pub fn content_type_key(ct: &[u8]) -> Vec<u8> {
    let end = ct
        .iter()
        .position(|&c| matches!(c, b';' | b',' | b' '))
        .unwrap_or(ct.len());
    ct[..end].to_ascii_lowercase()
}

/// When the body is parsed, which decides how its problems are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Request startup (`sapi_activate()`, `php_hash_environment()`):
    /// startup warnings.
    Startup,
    /// `request_parse_body()`: warnings of the function, and
    /// `RequestParseBodyException`s for the rfc1867 handler's
    /// `EMIT_WARNING_OR_ERROR()`s (`throw_exceptions`).
    ParseBody,
}

impl Phase {
    /// The function `php_error_docref()` names in warnings (empty during
    /// request startup).
    fn func(self) -> &'static str {
        match self {
            Phase::Startup => "",
            Phase::ParseBody => "request_parse_body",
        }
    }

    /// The prefix of `php_error_docref()` warnings.
    fn docref_prefix(self) -> &'static str {
        match self {
            Phase::Startup => STARTUP,
            Phase::ParseBody => "request_parse_body(): ",
        }
    }
}

/// The limits of one body parse (`REQUEST_PARSE_BODY_OPTION_GET()`): the
/// INI settings, each replaced by its `request_parse_body()` option when
/// one was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    max_file_uploads: i64,
    max_input_vars: i64,
    max_multipart_body_parts: i64,
    post_max_size: i64,
    upload_max_filesize: i64,
}

impl Limits {
    fn new(vm: &Vm, options: &ParseBodyOptions) -> Self {
        Limits {
            max_file_uploads: options
                .max_file_uploads
                .unwrap_or_else(|| vm.ini_long("max_file_uploads")),
            max_input_vars: options
                .max_input_vars
                .unwrap_or_else(|| vm.ini_long("max_input_vars")),
            max_multipart_body_parts: options.max_multipart_body_parts.unwrap_or_else(|| {
                vm.ini_string("max_multipart_body_parts")
                    .map_or(-1, |v| crate::headers::strtol(&v))
            }),
            post_max_size: options
                .post_max_size
                .unwrap_or_else(|| vm.ini_long("post_max_size")),
            upload_max_filesize: options
                .upload_max_filesize
                .unwrap_or_else(|| vm.ini_long("upload_max_filesize")),
        }
    }
}

/// Reports a problem of the body parse (`php_error_docref()` when
/// `docref`, else `sapi_error()`): a startup warning during request
/// startup, a warning of `request_parse_body()` afterwards.
fn warn(vm: &mut Vm, phase: Phase, message: &str, docref: bool) -> VmResult<()> {
    match phase {
        Phase::Startup => {
            startup_warning(vm, message, docref);
            Ok(())
        }
        Phase::ParseBody if docref => vm.php_warning(message),
        Phase::ParseBody => vm.raise(2, message),
    }
}

/// Records `t` as `zend_throw_exception_internal()` does: an exception
/// thrown while another is pending gets that one as its `previous`.
/// `exit()` and fatal errors end the parse at once.
fn pend(vm: &mut Vm, pending: &mut Option<Throw>, t: Throw) -> VmResult<()> {
    match (pending.take(), t) {
        (Some(Throw::Exception(old)), Throw::Exception(new)) => {
            vm.chain_exception(&new, &old);
            *pending = Some(Throw::Exception(new));
            Ok(())
        }
        (_, Throw::Exception(new)) => {
            *pending = Some(Throw::Exception(new));
            Ok(())
        }
        (_, t) => Err(t),
    }
}

/// [`pend`] for the error of `r`, if any.
fn pend_err(vm: &mut Vm, pending: &mut Option<Throw>, r: VmResult<()>) -> VmResult<()> {
    match r {
        Ok(()) => Ok(()),
        Err(t) => pend(vm, pending, t),
    }
}

/// `SG(request_info).content_length`: the `Content-Length` header (0
/// without one).
fn content_length(ctx: &RequestCtx) -> i64 {
    ctx.backend
        .table_get(Table::HeadersIn, b"Content-Length")
        .map_or(0, |v| crate::headers::strtol(&v))
}

/// `sapi_read_standard_form_data()`: unless `Content-Length` exceeds
/// `post_max_size` (a warning; any earlier buffer stays), reads what the
/// server still has into a new request body buffer, stopping once more
/// than `post_max_size` bytes of the body were read.
fn read_standard_form_data(
    vm: &mut Vm,
    ctx: &RequestCtx,
    post_max_size: i64,
    phase: Phase,
) -> VmResult<()> {
    let length = content_length(ctx);
    if post_max_size > 0 && length > post_max_size {
        return warn(
            vm,
            phase,
            &format!(
                "POST Content-Length of {length} bytes exceeds the limit of {post_max_size} bytes"
            ),
            true,
        );
    }
    let mut body = Vec::new();
    let mut buf = vec![0u8; POST_BLOCK_SIZE];
    let mut result = Ok(());
    loop {
        let n = ctx.read_post(&mut buf);
        body.extend_from_slice(&buf[..n]);
        if post_max_size > 0 && ctx.read_post_bytes.get() > post_max_size {
            result = warn(
                vm,
                phase,
                &format!(
                    "Actual POST length does not match Content-Length, and exceeds {post_max_size} bytes"
                ),
                true,
            );
            break;
        }
        if n < POST_BLOCK_SIZE {
            break;
        }
    }
    *ctx.body.borrow_mut() = Body::Buffered(body);
    result
}

/// `php_std_post_handler()`: the `application/x-www-form-urlencoded`
/// variables of the request body buffer (none without one), with
/// `max_input_vars` from `limits`.
fn handle_urlencoded(
    vm: &mut Vm,
    ctx: &RequestCtx,
    limits: &Limits,
    phase: Phase,
    post: &mut ZArray,
) -> VmResult<()> {
    let Body::Buffered(data) = ctx.body.borrow().clone() else {
        return Ok(());
    };
    let settings = InputSettings {
        max_input_vars: limits.max_input_vars,
        ..input_settings(vm)
    };
    query::treat_data(
        ParseKind::Post,
        &data,
        post,
        &settings,
        phase.func(),
        None,
        vm,
    )
    .map_err(|e| vm.throw_runtime_error(e))
}

/// `rfc1867_post_handler()`: parses the multipart body from what the server
/// still has into `post` / `files`, reporting the parser's events to the
/// upload observer (`php_rfc1867_callback`), and registers the uploaded
/// files (`SG(rfc1867_uploaded_files)`). For `request_parse_body()` the
/// handler's `EMIT_WARNING_OR_ERROR()`s become chained
/// `RequestParseBodyException`s, returned once the parse is over.
fn handle_multipart(
    vm: &mut Vm,
    ctx: &RequestCtx,
    content_type: &[u8],
    limits: &Limits,
    phase: Phase,
    post: &mut ZArray,
    files: &mut ZArray,
) -> VmResult<()> {
    let upload = UploadSettings {
        max_file_uploads: limits.max_file_uploads,
        max_multipart_body_parts: limits.max_multipart_body_parts,
        post_max_size: limits.post_max_size,
        max_input_vars: limits.max_input_vars,
        upload_max_filesize: limits.upload_max_filesize,
        file_uploads: vm.ini_bool("file_uploads"),
        tmp_dirs: tmp_dirs(vm),
        read_post_bytes: ctx.read_post_bytes.get(),
    };
    let callback = vm.rfc1867_callback;
    // mbstring's multibyte callbacks (`mbstring.encoding_translation`).
    let mut translation = php_vm::ext::rfc1867_translation_hook().and_then(|hook| hook(vm));
    // The parse got past its early checks: MULTIPART_EVENT_START follows
    // the new SG(rfc1867_uploaded_files).
    let mut started = false;
    let mut read = |buf: &mut [u8]| ctx.read_post(buf);
    let outcome = rfc1867::parse_with_events(
        content_type,
        content_length(ctx),
        &upload,
        &mut read,
        &mut |event: Rfc1867Event<'_>, bytes| {
            if matches!(event, Rfc1867Event::Start { .. }) {
                started = true;
            }
            callback.is_none_or(|cb| cb(vm, event, bytes))
        },
        translation.as_deref_mut(),
    );
    if started {
        // The files of an earlier parse are no longer uploaded files.
        let earlier = std::mem::take(&mut vm.rfc1867_uploaded_files);
        ctx.orphaned_uploads.borrow_mut().extend(earlier);
    }
    vm.rfc1867_uploaded_files.extend(outcome.uploaded);

    let mut pending: Option<Throw> = None;
    for w in &outcome.warnings {
        let res = if phase == Phase::ParseBody && w.docref {
            Err(vm.throw("RequestParseBodyException", &w.message))
        } else {
            warn(vm, phase, &w.message, w.docref)
        };
        pend_err(vm, &mut pending, res)?;
    }
    let settings = input_settings(vm);
    let opts = RegisterOptions {
        max_nesting_level: settings.max_input_nesting_level,
        display_errors: settings.display_errors,
        cookie: false,
        warning_prefix: phase.docref_prefix(),
    };
    for reg in outcome.registrations {
        let v = match reg.value {
            RegValue::Str(s) => Value::Str(ZStr::from(s)),
            RegValue::Long(l) => Value::Int(l),
        };
        let track = match reg.target {
            Target::Post => &mut *post,
            Target::Files => &mut *files,
        };
        let res = query::register_variable_ex(&reg.name, v, track, &opts, vm)
            .map_err(|e| vm.throw_runtime_error(e));
        pend_err(vm, &mut pending, res)?;
    }
    pending.map_or(Ok(()), Err)
}

/// The directories tried for uploaded files.
fn tmp_dirs(vm: &Vm) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut dirs = Vec::new();
    for name in ["upload_tmp_dir", "sys_temp_dir"] {
        let v = ini_bytes(vm, name);
        if !v.is_empty() {
            dirs.push(PathBuf::from(std::ffi::OsStr::from_bytes(&v)));
        }
    }
    if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        dirs.push(PathBuf::from(t));
    }
    dirs.push(PathBuf::from("/tmp"));
    dirs
}

/// `php_build_argv()` for a web request: the query string split on `+`.
pub fn build_argv(query: &[u8]) -> Vec<Vec<u8>> {
    if query.is_empty() {
        return Vec::new();
    }
    query.split(|&c| c == b'+').map(<[u8]>::to_vec).collect()
}

/// `$_COOKIE` (`treat_data(PARSE_COOKIE)`), empty unless `enabled`.
fn parse_cookie(vm: &mut Vm, b: &dyn Backend, settings: &InputSettings, enabled: bool) -> ZArray {
    let mut cookie = ZArray::new();
    if enabled {
        if let Some(c) = b.table_get(Table::HeadersIn, b"Cookie") {
            let _ = query::treat_data(ParseKind::Cookie, &c, &mut cookie, settings, "", None, vm);
        }
    }
    cookie
}

/// `php_autoglobal_merge()`.
fn merge(dest: &mut HashTable, src: &ZArray) {
    for (k, v) in src.iter() {
        let v = v.deref();
        if let (Value::Array(sa), Some(Value::Array(da))) = (&v, dest.get_mut(k).map(|d| &mut *d)) {
            let mut da2 = da.clone();
            merge(da2.make_mut(), sa);
            *da = da2;
            continue;
        }
        dest.insert(k.clone(), v);
    }
}

/// Populates every superglobal for the request.
#[allow(clippy::too_many_lines)]
pub fn populate(vm: &mut Vm, ctx: &RequestCtx, auth: &Auth) {
    let b = &*ctx.backend;
    let order = {
        let o = ini_bytes(vm, "variables_order");
        if o.is_empty() {
            b"EGPCS".to_vec()
        } else {
            o
        }
    };
    let has = |c: u8| order.iter().any(|x| x.eq_ignore_ascii_case(&c));
    let settings = input_settings(vm);

    // $_GET
    let mut get = ZArray::new();
    if has(b'G') {
        let q = b.str_field(StrField::Args).unwrap_or_default();
        let _ = query::treat_data(ParseKind::Get, &q, &mut get, &settings, "", None, vm);
    }

    // $_POST / $_FILES (sapi_activate + php_auto_globals_create_post)
    let mut post = ZArray::new();
    let mut files = ZArray::new();
    // `$_COOKIE` parsed early for the upload observer.
    let mut early_cookie: Option<ZArray> = None;
    let method = b.str_field(StrField::Method).unwrap_or_default();
    let content_type = b.table_get(Table::HeadersIn, b"Content-Type");
    if vm.ini_bool("enable_post_data_reading") && method == b"POST" {
        if let Some(ct) = &content_type {
            let limits = Limits::new(vm, &ParseBodyOptions::default());
            // Startup problems are warnings only: the results are dropped.
            match content_type_key(ct).as_slice() {
                b"application/x-www-form-urlencoded" => {
                    let _ = read_standard_form_data(vm, ctx, limits.post_max_size, Phase::Startup);
                    if has(b'P') {
                        let _ = handle_urlencoded(vm, ctx, &limits, Phase::Startup, &mut post);
                    }
                }
                b"multipart/form-data" => {
                    if has(b'P') {
                        if vm.rfc1867_callback.is_some() {
                            // php_session_rfc1867_early_find_sid(): the
                            // observer looks the session ID up in
                            // $_COOKIE / $_GET while the body is parsed.
                            let cookie = parse_cookie(vm, b, &settings, has(b'C'));
                            set_global(vm, "_GET", get.clone());
                            set_global(vm, "_COOKIE", cookie.clone());
                            early_cookie = Some(cookie);
                        }
                        let _ = handle_multipart(
                            vm,
                            ctx,
                            ct,
                            &limits,
                            Phase::Startup,
                            &mut post,
                            &mut files,
                        );
                    }
                }
                // php_default_post_reader(): a body no handler is
                // registered for is still read.
                _ => {
                    let _ = read_standard_form_data(vm, ctx, limits.post_max_size, Phase::Startup);
                }
            }
        }
    }

    // $_COOKIE
    let cookie = match early_cookie {
        Some(parsed) => parsed,
        None => parse_cookie(vm, b, &settings, has(b'C')),
    };

    // $_SERVER
    let mut server = ZArray::new();
    if has(b'S') {
        for (k, v) in b.table_entries(Table::SubprocessEnv) {
            register(vm, &k, Value::Str(ZStr::from(v)), &mut server, false);
        }
        let uri = b.str_field(StrField::Uri).unwrap_or_default();
        register(
            vm,
            b"PHP_SELF",
            Value::Str(ZStr::from(uri)),
            &mut server,
            false,
        );
        let t = server.make_mut();
        let s = |v: &[u8]| Value::Str(ZStr::from(v));
        if let Some(u) = &auth.user {
            t.insert("PHP_AUTH_USER", s(u));
        }
        if let Some(p) = &auth.password {
            t.insert("PHP_AUTH_PW", s(p));
        }
        if let Some(d) = &auth.digest {
            t.insert("PHP_AUTH_DIGEST", s(d));
        }
        #[allow(clippy::cast_precision_loss)]
        let now = b.int_field(IntField::RequestTime) as f64 / 1_000_000.0;
        t.insert("REQUEST_TIME_FLOAT", Value::Float(now));
        #[allow(clippy::cast_possible_truncation)]
        t.insert("REQUEST_TIME", Value::Int(now as i64));
        if vm.ini_bool("register_argc_argv") {
            let q = b.str_field(StrField::Args).unwrap_or_default();
            let args = build_argv(&q);
            let count = i64::try_from(args.len()).unwrap_or(0);
            let t = server.make_mut();
            t.insert(
                "argv",
                Value::Array(ZArray::from_values(
                    args.into_iter().map(|a| Value::Str(ZStr::from(a))),
                )),
            );
            t.insert("argc", Value::Int(count));
        }
    }

    // $_ENV
    let mut env = ZArray::new();
    if has(b'E') {
        use std::os::unix::ffi::OsStrExt;
        let t = env.make_mut();
        for (k, v) in std::env::vars_os() {
            t.insert(
                ArrayKey::from_bytes(k.as_bytes()),
                Value::Str(ZStr::from(v.as_bytes())),
            );
        }
    }

    // $_REQUEST
    let mut request = ZArray::new();
    {
        let ro = ini_bytes(vm, "request_order");
        let ro = if ro.is_empty() { order.clone() } else { ro };
        let t = request.make_mut();
        for c in ro {
            match c.to_ascii_lowercase() {
                b'g' => merge(t, &get),
                b'p' => merge(t, &post),
                b'c' => merge(t, &cookie),
                _ => {}
            }
        }
    }

    set_global(vm, "_GET", get);
    set_global(vm, "_POST", post);
    set_global(vm, "_COOKIE", cookie);
    set_global(vm, "_FILES", files);
    set_global(vm, "_SERVER", server);
    set_global(vm, "_ENV", env);
    set_global(vm, "_REQUEST", request);
}

/// The SAPI's part of `request_parse_body()` ([`php_vm::ext::ParseBodyFn`],
/// `ext/standard/http.c`): `sapi_read_post_data()` and `sapi_handle_post()`
/// again, for any request method, into fresh arrays. The body is read
/// from where earlier reads left it: once request startup, `php://input`
/// or an earlier call consumed it, there is nothing left to parse.
pub fn parse_body(vm: &mut Vm, options: &ParseBodyOptions) -> VmResult<(ZArray, ZArray)> {
    let ctx = crate::request::current();
    let content_type = ctx
        .as_ref()
        .and_then(|c| c.backend.table_get(Table::HeadersIn, b"Content-Type"));
    let (Some(ctx), Some(ct)) = (ctx, content_type) else {
        return Err(vm.throw(
            "RequestParseBodyException",
            "Request does not provide a content type",
        ));
    };
    let limits = Limits::new(vm, options);
    let phase = Phase::ParseBody;
    let mut post = ZArray::new();
    let mut files = ZArray::new();
    let mut pending: Option<Throw> = None;
    match content_type_key(&ct).as_slice() {
        b"application/x-www-form-urlencoded" => {
            let r = read_standard_form_data(vm, &ctx, limits.post_max_size, phase);
            pend_err(vm, &mut pending, r)?;
            // Once a warning handler threw, the handler's warnings would
            // not reach it, and its result is dropped.
            if pending.is_none() {
                let r = handle_urlencoded(vm, &ctx, &limits, phase, &mut post);
                pend_err(vm, &mut pending, r)?;
            }
        }
        b"multipart/form-data" => {
            let r = handle_multipart(vm, &ctx, &ct, &limits, phase, &mut post, &mut files);
            pend_err(vm, &mut pending, r)?;
        }
        _ => {
            // php_default_post_reader(): a POST body no handler is
            // registered for is read all the same (replacing the buffer).
            let method = ctx.backend.str_field(StrField::Method).unwrap_or_default();
            if method == b"POST" {
                let r = read_standard_form_data(vm, &ctx, limits.post_max_size, phase);
                pend_err(vm, &mut pending, r)?;
            }
            let msg = format!(
                "Content-Type \"{}\" is not supported",
                String::from_utf8_lossy(&ct)
            );
            let t = vm.throw("RequestParseBodyException", &msg);
            pend(vm, &mut pending, t)?;
        }
    }
    match pending {
        Some(t) => Err(t),
        None => Ok((post, files)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_keys() {
        assert_eq!(
            content_type_key(b"Multipart/Form-Data; boundary=x"),
            b"multipart/form-data"
        );
        assert_eq!(
            content_type_key(b"application/x-www-form-urlencoded"),
            b"application/x-www-form-urlencoded"
        );
    }

    #[test]
    fn argv_from_query() {
        assert_eq!(
            build_argv(b"a+b+c"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert!(build_argv(b"").is_empty());
    }

    #[test]
    fn merge_is_recursive() {
        let mut a = ZArray::new();
        {
            let t = a.make_mut();
            let mut inner = ZArray::new();
            inner.make_mut().insert("x", Value::Int(1));
            t.insert("k", Value::Array(inner));
            t.insert("s", Value::Int(1));
        }
        let mut b = ZArray::new();
        {
            let t = b.make_mut();
            let mut inner = ZArray::new();
            inner.make_mut().insert("y", Value::Int(2));
            t.insert("k", Value::Array(inner));
            t.insert("s", Value::Int(2));
        }
        let mut dest = HashTable::new();
        merge(&mut dest, &a);
        merge(&mut dest, &b);
        let Some(Value::Array(k)) = dest.get_str(b"k") else {
            panic!("k missing")
        };
        assert_eq!(k.len(), 2);
        assert!(matches!(dest.get_str(b"s"), Some(Value::Int(2))));
    }
}
