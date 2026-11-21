// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request variables: `$_GET`, `$_POST`, `$_COOKIE`, `$_FILES`,
//! `$_SERVER`, `$_ENV` and `$_REQUEST` (`php_hash_environment()`,
//! `sapi_read_post_data()` and the rfc1867 handler), and the body parse
//! of `request_parse_body()` ([`parse_body`]), which runs the same POST
//! readers and handlers into fresh arrays.
//!
//! The request is `vm.request_info` (`SG(request_info)`), which the SAPI
//! fills before [`populate`]; the body comes through [`SapiInput`], the
//! SAPI's own `$_SERVER` entries through [`SapiVariables`].

use crate::body::{self, SapiInput, SAPI_POST_BLOCK_SIZE};
use crate::rfc1867::{self, RegValue, Target, UploadSettings};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_stdmisc::query::{self, InputSettings, ParseKind, RegisterOptions};
use php_vm::ext::{ParseBodyOptions, Rfc1867Event};
use php_vm::{Throw, Vm, VmResult};

/// What a SAPI adds to the request variables (`sapi_module_struct`).
pub trait SapiVariables: SapiInput {
    /// `sapi_module.register_server_variables`: the SAPI's `$_SERVER`
    /// entries (through [`register_variable_safe`], or under their exact
    /// name for `php_register_known_variable()`).
    fn register_server_variables(&self, vm: &mut Vm, server: &mut ZArray);

    /// `sapi_get_request_time()`: the request's start in seconds.
    fn request_time(&self) -> f64 {
        php_support::time::now_float()
    }
}

/// The part of `s` before the first NUL (values are C strings).
fn c_str(s: &[u8]) -> &[u8] {
    s.split(|&c| c == 0).next().unwrap_or_default()
}

const STARTUP: &str = "PHP Request Startup: ";

fn ini_bytes(vm: &Vm, name: &str) -> Vec<u8> {
    vm.ini_string(name).unwrap_or_default()
}

/// `PG(display_errors)` as a boolean.
pub fn display_errors(vm: &Vm) -> bool {
    let v = ini_bytes(vm, "display_errors").to_ascii_lowercase();
    match v.as_slice() {
        b"on" | b"yes" | b"true" | b"stdout" | b"stderr" => true,
        _ => php_support::atol(&v) != 0,
    }
}

/// The INI settings of the variable parsers.
pub fn input_settings(vm: &Vm) -> InputSettings {
    let sep = ini_bytes(vm, "arg_separator.input");
    InputSettings {
        arg_separator_input: if sep.is_empty() { b"&".to_vec() } else { sep },
        max_input_vars: vm.ini_long("max_input_vars"),
        max_input_nesting_level: vm.ini_long("max_input_nesting_level"),
        display_errors: display_errors(vm),
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

/// `php_register_variable_safe()` into `track` during request startup.
pub fn register_variable_safe(vm: &mut Vm, name: &[u8], value: Value, track: &mut ZArray) {
    let settings = input_settings(vm);
    let opts = RegisterOptions {
        max_nesting_level: settings.max_input_nesting_level,
        display_errors: settings.display_errors,
        cookie: false,
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
                    .map_or(-1, |v| php_support::atol(&v))
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

/// A `php_error_docref()` notice of the body parse.
fn notice(vm: &mut Vm, phase: Phase, message: &str) -> VmResult<()> {
    vm.raise(8, &format!("{}{message}", phase.docref_prefix()))
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

/// `PG(upload_tmp_dir)`, with the streams layer's copy of the INI
/// settings brought up to date (`sys_temp_dir` and `open_basedir` decide
/// where a temporary file may go when it is not usable).
pub fn upload_tmp_dir(vm: &Vm) -> Vec<u8> {
    php_ext_streams::bridge::sync_settings(vm);
    ini_bytes(vm, "upload_tmp_dir")
}

/// `php_build_argv()` for a web request: the query string split on `+`.
pub fn build_argv(query: &[u8]) -> Vec<Vec<u8>> {
    if query.is_empty() {
        return Vec::new();
    }
    query.split(|&c| c == b'+').map(<[u8]>::to_vec).collect()
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

/// `check_http_proxy()` (bug #72573, "httpoxy", CVE-2016-5385): a request
/// header `Proxy` must not pose as the `HTTP_PROXY` environment variable
/// that HTTP clients take their proxy from. An `HTTP_PROXY` entry becomes
/// the process environment's value, or goes when there is none.
fn check_http_proxy(track: &mut ZArray) {
    use std::os::unix::ffi::OsStrExt;
    if track.get_str(b"HTTP_PROXY").is_none() {
        return;
    }
    let key = ArrayKey::from_bytes(b"HTTP_PROXY");
    let t = track.make_mut();
    match std::env::var_os("HTTP_PROXY") {
        Some(local) => {
            t.insert(key, Value::Str(ZStr::from(local.as_bytes())));
        }
        None => {
            t.remove(&key);
        }
    }
}

/// `SG(request_info).content_type`, as a C string.
fn content_type(vm: &Vm) -> Option<Vec<u8>> {
    vm.request_info
        .content_type
        .as_deref()
        .map(|ct| c_str(ct).to_vec())
}

/// Whether `SG(request_info).request_method` is `POST`.
fn is_post(vm: &Vm) -> bool {
    vm.request_info.request_method.as_deref() == Some(b"POST")
}

/// `sapi_read_standard_form_data()`: unless the body is longer than
/// `post_max_size` (a warning; any earlier buffer stays), what the SAPI
/// has not read yet becomes the request body buffer, read until more than
/// `post_max_size` bytes came. A buffer of `SAPI_POST_BLOCK_SIZE` bytes
/// or more must be able to move to a temporary file, else it is dropped.
fn read_standard_form_data<S: SapiInput + ?Sized>(
    vm: &mut Vm,
    s: &S,
    post_max_size: i64,
    phase: Phase,
) -> VmResult<()> {
    let length = vm.request_info.content_length;
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
    if body::buffer_rest(s, post_max_size) {
        warn(
            vm,
            phase,
            &format!(
                "Actual POST length does not match Content-Length, and exceeds {post_max_size} bytes"
            ),
            true,
        )?;
    }
    let buffered = body::request_body(s).map_or(0, |b| b.len());
    if buffered >= SAPI_POST_BLOCK_SIZE && !body_spill_possible(vm, phase)? {
        // php_stream_temp_write() failed: the stream is truncated.
        body::discard_request_body(s);
        warn(
            vm,
            phase,
            "Unable to create temporary file, Check permissions in temporary files directory.",
            true,
        )?;
        warn(
            vm,
            phase,
            "POST data can't be buffered; all data discarded",
            true,
        )?;
    }
    Ok(())
}

/// Whether the request body buffer can move to a temporary file
/// (`php_stream_fopen_temporary_file(PG(upload_tmp_dir), "php")` in
/// `php_stream_temp_write()`): `upload_tmp_dir`, else (with a notice when
/// it was set) the system's temporary directory. The test file is removed
/// again; the body itself stays in memory.
fn body_spill_possible(vm: &mut Vm, phase: Phase) -> VmResult<bool> {
    use std::os::unix::ffi::OsStrExt;
    let dir = upload_tmp_dir(vm);
    let mut fell_back = false;
    let file =
        php_streams::temp::create_temporary(Some(&dir), Some(b"php"), 0, &mut || fell_back = true);
    if fell_back {
        notice(vm, phase, php_streams::temp::FALLBACK_NOTICE)?;
    }
    let Some((file, path)) = file else {
        return Ok(false);
    };
    drop(file);
    let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&path));
    Ok(true)
}

/// `php_std_post_handler()`: the `application/x-www-form-urlencoded`
/// variables of the request body buffer (none without one), with
/// `max_input_vars` from `limits`.
fn handle_urlencoded<S: SapiInput + ?Sized>(
    vm: &mut Vm,
    s: &S,
    limits: &Limits,
    phase: Phase,
    post: &mut ZArray,
) -> VmResult<()> {
    // The request body buffer, read in place: `php_std_post_handler()`
    // streams it through a buffer that only holds the variable being
    // parsed, so the body itself does not count towards memory_limit.
    let Some(data) = body::request_body(s) else {
        return Ok(());
    };
    if let Some(hook) = php_vm::ext::request_input_hook() {
        if let Some(r) = hook(vm, php_vm::ext::RequestInput::Post, &data, post) {
            return r;
        }
    }
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

/// `rfc1867_post_handler()`: parses the multipart body from what the SAPI
/// has not read yet into `post` / `files`, reporting the parser's events
/// to the upload observer (`php_rfc1867_callback`), and registers the
/// uploaded files (`SG(rfc1867_uploaded_files)`). For `request_parse_body()`
/// the handler's `EMIT_WARNING_OR_ERROR()`s become chained
/// `RequestParseBodyException`s, returned once the parse is over.
fn handle_multipart<S: SapiInput + ?Sized>(
    vm: &mut Vm,
    s: &S,
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
        upload_tmp_dir: upload_tmp_dir(vm),
        // SG(read_post_bytes): what reads before this parse took.
        read_post_bytes: s.body().read_post_bytes(),
    };
    let callback = vm.rfc1867_callback;
    // mbstring's multibyte callbacks (`mbstring.encoding_translation`).
    let mut translation = php_vm::ext::rfc1867_translation_hook().and_then(|hook| hook(vm));
    // The parse got past its early checks: MULTIPART_EVENT_START follows
    // the new SG(rfc1867_uploaded_files).
    let mut started = false;
    let mut read = |buf: &mut [u8]| body::read_post_block(s, buf);
    let outcome = rfc1867::parse_with_events(
        content_type,
        vm.request_info.content_length,
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
        s.body()
            .orphan_uploads(std::mem::take(&mut vm.rfc1867_uploaded_files));
    }
    vm.rfc1867_uploaded_files.extend(outcome.uploaded);
    // The parse stopped at a `memory_limit` violation: the request fails
    // with it (`sapi_handle_post()`), registering nothing more.
    if php_memlimit::is_pending() {
        return Ok(());
    }

    let mut pending: Option<Throw> = None;
    for w in &outcome.warnings {
        let reported = if w.notice {
            notice(vm, phase, &w.message)
        } else if phase == Phase::ParseBody && w.docref {
            Err(vm.throw("RequestParseBodyException", &w.message))
        } else {
            warn(vm, phase, &w.message, w.docref)
        };
        pend_err(vm, &mut pending, reported)?;
    }
    let settings = input_settings(vm);
    let opts = RegisterOptions {
        max_nesting_level: settings.max_input_nesting_level,
        display_errors: settings.display_errors,
        cookie: false,
        warning_prefix: phase.docref_prefix(),
    };
    for registration in outcome.registrations {
        let v = match registration.value {
            RegValue::Str(s) => Value::Str(ZStr::from(s)),
            RegValue::Long(l) => Value::Int(l),
        };
        let track = match registration.target {
            Target::Post => &mut *post,
            Target::Files => &mut *files,
        };
        let registered = query::register_variable_ex(&registration.name, v, track, &opts, vm)
            .map_err(|e| vm.throw_runtime_error(e));
        pend_err(vm, &mut pending, registered)?;
        if php_memlimit::is_pending() {
            break;
        }
    }
    pending.map_or(Ok(()), Err)
}

/// `$_GET` (`treat_data(PARSE_GET)` of `SG(request_info).query_string`),
/// empty unless `enabled`.
fn parse_get(vm: &mut Vm, settings: &InputSettings, enabled: bool) -> ZArray {
    let mut get = ZArray::new();
    let query = vm.request_info.query_string.clone();
    if let (true, Some(q)) = (enabled, query) {
        let q = c_str(&q);
        let hooked = php_vm::ext::request_input_hook()
            .and_then(|h| h(vm, php_vm::ext::RequestInput::Get, q, &mut get));
        if hooked.is_none() {
            let _ = query::treat_data(ParseKind::Get, q, &mut get, settings, "", None, vm);
        }
    }
    get
}

/// `$_COOKIE` (`treat_data(PARSE_COOKIE)` of
/// `SG(request_info).cookie_data`), empty unless `enabled`.
fn parse_cookie(vm: &mut Vm, settings: &InputSettings, enabled: bool) -> ZArray {
    let mut cookie = ZArray::new();
    let data = vm.request_info.cookie_data.clone();
    if let (true, Some(c)) = (enabled, data) {
        let c = c_str(&c);
        let hooked = php_vm::ext::request_input_hook()
            .and_then(|h| h(vm, php_vm::ext::RequestInput::Cookie, c, &mut cookie));
        if hooked.is_none() {
            let _ = query::treat_data(ParseKind::Cookie, c, &mut cookie, settings, "", None, vm);
        }
    }
    cookie
}

/// Populates every superglobal for the request, in php-src's order:
/// `sapi_activate()` reads a POST body (`sapi_read_post_data()`), then
/// `php_hash_environment()` builds `$_GET`, runs the POST handler for
/// `$_POST`/`$_FILES` (`sapi_handle_post()`), then `$_COOKIE`, `$_SERVER`,
/// `$_ENV` and `$_REQUEST`. A `memory_limit` violation while the body is
/// parsed stops it before `$_COOKIE`; the caller then fails the request.
#[allow(clippy::too_many_lines)]
pub fn populate<S: SapiVariables + ?Sized>(vm: &mut Vm, s: &S) {
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
    // `ZEND_HAS_OOM()` for the registration loops of `main/php_variables.c`.
    query::set_oom_pending_hook(php_memlimit::is_pending);

    // sapi_activate() → sapi_read_post_data(): the POST reader. Startup
    // problems are warnings only: the results are dropped.
    let content_type = content_type(vm);
    let post_body = match &content_type {
        Some(ct) if vm.ini_bool("enable_post_data_reading") && is_post(vm) => {
            Some((ct.clone(), content_type_key(ct)))
        }
        _ => None,
    };
    let limits = Limits::new(vm, &ParseBodyOptions::default());
    if let Some((_, key)) = &post_body {
        // multipart/form-data has no reader: its handler reads the SAPI.
        // php_default_post_reader(): a body no handler is registered for
        // is still read.
        if key.as_slice() != b"multipart/form-data" {
            let _ = read_standard_form_data(vm, s, limits.post_max_size, Phase::Startup);
        }
    }

    // $_GET
    let get = parse_get(vm, &settings, has(b'G'));

    // $_POST / $_FILES: the POST handler (sapi_handle_post()).
    let mut post = ZArray::new();
    let mut files = ZArray::new();
    // `$_COOKIE` parsed early for the upload observer.
    let mut early_cookie: Option<ZArray> = None;
    // php_auto_globals_create_post(): not once the headers were sent, e.g.
    // by a startup warning displayed without output buffering ($_POST and
    // $_FILES stay empty; the body read above is still php://input).
    let handle_post = has(b'P') && !vm.headers_sent;
    if let (Some((ct, key)), true) = (&post_body, handle_post) {
        match key.as_slice() {
            b"application/x-www-form-urlencoded" => {
                let _ = handle_urlencoded(vm, s, &limits, Phase::Startup, &mut post);
            }
            b"multipart/form-data" => {
                if vm.rfc1867_callback.is_some() {
                    // php_session_rfc1867_early_find_sid(): the observer
                    // looks the session ID up in $_COOKIE / $_GET while the
                    // body is parsed.
                    let cookie = parse_cookie(vm, &settings, has(b'C'));
                    set_global(vm, "_GET", get.clone());
                    set_global(vm, "_COOKIE", cookie.clone());
                    early_cookie = Some(cookie);
                }
                let _ = handle_multipart(vm, s, ct, &limits, Phase::Startup, &mut post, &mut files);
            }
            _ => {}
        }
    }
    // `sapi_handle_post()` → `zend_handle_delayed_oom()`: the soft limit
    // was crossed while parsing the body; request startup bails out
    // before any other superglobal is built (the caller reports it).
    if php_memlimit::is_pending() {
        return;
    }

    // $_COOKIE
    let cookie = match early_cookie {
        Some(parsed) => parsed,
        None => parse_cookie(vm, &settings, has(b'C')),
    };

    // $_SERVER
    let mut server = ZArray::new();
    if has(b'S') {
        s.register_server_variables(vm, &mut server);
        let info = &vm.request_info;
        let auth = [
            ("PHP_AUTH_USER", info.auth_user.clone()),
            ("PHP_AUTH_PW", info.auth_password.clone()),
            ("PHP_AUTH_DIGEST", info.auth_digest.clone()),
        ];
        let t = server.make_mut();
        for (name, value) in auth {
            if let Some(v) = value {
                t.insert(name, Value::Str(ZStr::from(v)));
            }
        }
        let now = s.request_time();
        t.insert("REQUEST_TIME_FLOAT", Value::Float(now));
        #[allow(clippy::cast_possible_truncation)]
        t.insert("REQUEST_TIME", Value::Int(now as i64));
        if vm.ini_bool("register_argc_argv") {
            let _ = vm.php_deprecated(
                "Deriving $_SERVER['argv'] from the query string is deprecated. Configure register_argc_argv=0 to turn this message off",
            );
            let q = vm.request_info.query_string.clone().unwrap_or_default();
            let args = build_argv(c_str(&q));
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

    check_http_proxy(&mut server);

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

    check_http_proxy(&mut env);

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
/// or an earlier call consumed it, there is nothing left to parse. `s` is
/// the current request, `None` outside one.
pub fn parse_body<S: SapiInput + ?Sized>(
    vm: &mut Vm,
    options: &ParseBodyOptions,
    s: Option<&S>,
) -> VmResult<(ZArray, ZArray)> {
    let (Some(s), Some(ct)) = (s, content_type(vm)) else {
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
            let r = read_standard_form_data(vm, s, limits.post_max_size, phase);
            pend_err(vm, &mut pending, r)?;
            // Once a warning handler threw, the handler's warnings would
            // not reach it, and its result is dropped.
            if pending.is_none() {
                let r = handle_urlencoded(vm, s, &limits, phase, &mut post);
                pend_err(vm, &mut pending, r)?;
            }
        }
        b"multipart/form-data" => {
            let r = handle_multipart(vm, s, &ct, &limits, phase, &mut post, &mut files);
            pend_err(vm, &mut pending, r)?;
        }
        _ => {
            // php_default_post_reader(): a POST body no handler is
            // registered for is read all the same (replacing the buffer).
            if is_post(vm) {
                let r = read_standard_form_data(vm, s, limits.post_max_size, phase);
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

    fn vm_with(ini: &[(&str, &str)]) -> Vm {
        let opts = php_vm::VmOptions {
            ini: ini
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        };
        Vm::with_extensions(&opts, Box::new(std::io::sink()), &[])
    }

    /// `REQUEST_PARSE_BODY_OPTION_GET()`: an option replaces its INI value.
    #[test]
    fn limits_prefer_the_options() {
        let vm = vm_with(&[("max_input_vars", "7"), ("post_max_size", "1K")]);
        let ini = Limits::new(&vm, &ParseBodyOptions::default());
        assert_eq!(ini.max_input_vars, 7);
        assert_eq!(ini.post_max_size, 1024);
        assert_eq!(ini.max_multipart_body_parts, -1);
        let options = ParseBodyOptions {
            max_input_vars: Some(1),
            max_multipart_body_parts: Some(3),
            upload_max_filesize: Some(0),
            ..ParseBodyOptions::default()
        };
        let set = Limits::new(&vm, &options);
        assert_eq!(set.max_input_vars, 1);
        assert_eq!(set.max_multipart_body_parts, 3);
        assert_eq!(set.upload_max_filesize, 0);
        assert_eq!(set.post_max_size, 1024);
        assert_eq!(set.max_file_uploads, ini.max_file_uploads);
    }

    /// Exceptions raised during one parse chain like
    /// `zend_throw_exception_internal()`; a fatal error ends the parse.
    #[test]
    fn pending_exceptions_chain() {
        let mut vm = vm_with(&[]);
        let first = vm.throw("RequestParseBodyException", "first");
        let Throw::Exception(first_obj) = first.clone() else {
            panic!("not an exception");
        };
        let second = vm.throw("RequestParseBodyException", "second");
        let mut pending = None;
        pend(&mut vm, &mut pending, first).unwrap();
        pend_err(&mut vm, &mut pending, Ok(())).unwrap();
        pend_err(&mut vm, &mut pending, Err(second)).unwrap();
        let Some(Throw::Exception(last)) = pending.clone() else {
            panic!("nothing pending");
        };
        let previous = vm
            .call_method_by_name(&last, "getPrevious", Vec::new())
            .unwrap();
        assert!(matches!(previous, Value::Object(o) if php_runtime::Obj::ptr_eq(&o, &first_obj)));
        assert!(matches!(
            pend(&mut vm, &mut pending, Throw::Fatal),
            Err(Throw::Fatal)
        ));
    }

    /// Startup problems are startup warnings; `request_parse_body()` ones
    /// name the function (docref) or not (`sapi_error()`).
    #[test]
    fn phase_prefixes() {
        assert_eq!(Phase::Startup.func(), "");
        assert_eq!(Phase::Startup.docref_prefix(), STARTUP);
        assert_eq!(Phase::ParseBody.func(), "request_parse_body");
        assert_eq!(Phase::ParseBody.docref_prefix(), "request_parse_body(): ");
    }

    /// The body buffer can spill into `upload_tmp_dir`, else into the
    /// system temporary directory; bug69487.phpt (sapi/cgi) covers the case
    /// where neither works.
    #[test]
    fn temporary_files_for_the_body_buffer() {
        let tmp = std::env::temp_dir();
        let mut vm = vm_with(&[("upload_tmp_dir", &tmp.to_string_lossy())]);
        assert!(body_spill_possible(&mut vm, Phase::Startup).unwrap());
        let mut vm = vm_with(&[("upload_tmp_dir", "/not-supposed-to-exist")]);
        assert!(body_spill_possible(&mut vm, Phase::Startup).unwrap());
    }
}
