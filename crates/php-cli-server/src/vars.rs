// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request variables: `$_SERVER` (`sapi_cli_server_register_variables` +
//! `php_register_server_variables`), `$_GET`, `$_POST`, `$_COOKIE`,
//! `$_FILES`, `$_ENV` and `$_REQUEST` (`php_hash_environment()`,
//! `sapi_read_post_data()` and the rfc1867 handler), and the body parse
//! of `request_parse_body()` ([`parse_body`]), which runs the same POST
//! readers and handlers into fresh arrays.

use crate::parser::Method;
use crate::request::Request;
use crate::sapi::RequestCtx;
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_sapi_input::rfc1867::{self, RegValue, Target, UploadSettings};
use php_stdmisc::query::{self, InputSettings, ParseKind, RegisterOptions};
use php_vm::ext::{ParseBodyOptions, Rfc1867Event};
use php_vm::{Throw, Vm, VmResult};
use std::path::PathBuf;

/// How a `$_SERVER` entry is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// `php_register_known_variable`: stored under the exact name.
    Known,
    /// `php_register_variable_safe`: the name is normalised (`.`/` ` → `_`,
    /// `[` starts an array).
    Filtered,
}

/// What the server knows about itself, for `$_SERVER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFacts<'a> {
    /// `DOCUMENT_ROOT`.
    pub document_root: &'a [u8],
    /// `SERVER_NAME` (the host given to `-S`).
    pub host: &'a [u8],
    /// `SERVER_PORT`.
    pub port: u16,
    /// The router script, if any.
    pub router: Option<&'a [u8]>,
    /// `ip:port` of the client.
    pub client_addr: &'a str,
}

/// The `sapi_cli_server_register_variables()` entries, in order.
pub fn server_entries(req: &Request, facts: &ServerFacts<'_>) -> Vec<(Vec<u8>, Vec<u8>, VarKind)> {
    use VarKind::{Filtered, Known};
    let mut out: Vec<(Vec<u8>, Vec<u8>, VarKind)> = Vec::new();
    let mut known = |k: &str, v: &[u8]| out.push((k.as_bytes().to_vec(), v.to_vec(), Known));
    known("DOCUMENT_ROOT", facts.document_root);
    let addr = facts.client_addr;
    if let Some(colon) = addr.rfind(':') {
        let mut host = &addr[..colon];
        host = host.strip_prefix('[').unwrap_or(host);
        host = host.strip_suffix(']').unwrap_or(host);
        let port: String = addr[colon + 1..].chars().take(7).collect();
        known("REMOTE_ADDR", host.as_bytes());
        known("REMOTE_PORT", port.as_bytes());
    } else {
        known("REMOTE_ADDR", addr.as_bytes());
    }
    known(
        "SERVER_SOFTWARE",
        format!("PHP/{} (Development Server)", php_vm::PHP_VERSION).as_bytes(),
    );
    known(
        "SERVER_PROTOCOL",
        format!(
            "HTTP/{}.{}",
            req.protocol_version / 100,
            req.protocol_version % 100
        )
        .as_bytes(),
    );
    known("SERVER_NAME", facts.host);
    known("SERVER_PORT", facts.port.to_string().as_bytes());
    known("REQUEST_URI", req.uri());
    known("REQUEST_METHOD", req.method.as_str().as_bytes());
    let vpath = req.vpath.clone().unwrap_or_default();
    known("SCRIPT_NAME", &vpath);
    if let Some(pt) = &req.path_translated {
        known("SCRIPT_FILENAME", c_str(pt));
    } else if let Some(r) = facts.router {
        known("SCRIPT_FILENAME", r);
    }
    if let Some(pi) = &req.path_info {
        known("PATH_INFO", pi);
    }
    match &req.path_info {
        Some(pi) if !pi.is_empty() => {
            let mut s = vpath.clone();
            s.extend_from_slice(pi);
            known("PHP_SELF", &s);
        }
        _ => known("PHP_SELF", &vpath),
    }
    if let Some(q) = &req.query_string {
        out.push((b"QUERY_STRING".to_vec(), c_str(q).to_vec(), Filtered));
    }
    for (k, v) in &req.headers {
        let key: Vec<u8> = k
            .iter()
            .map(|&c| {
                if c == b'-' {
                    b'_'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        let mut real = b"HTTP_".to_vec();
        real.extend_from_slice(&key);
        if key == b"CONTENT_TYPE" || key == b"CONTENT_LENGTH" {
            out.push((key, c_str(v).to_vec(), Filtered));
        }
        out.push((real, c_str(v).to_vec(), Filtered));
    }
    out
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
        _ => crate::headers::strtol(&v) != 0,
    }
}

fn input_settings(vm: &Vm) -> InputSettings {
    let sep = ini_bytes(vm, "arg_separator.input");
    InputSettings {
        arg_separator_input: if sep.is_empty() { b"&".to_vec() } else { sep },
        max_input_vars: vm.ini_long("max_input_vars"),
        max_input_nesting_level: vm.ini_long("max_input_nesting_level"),
        display_errors: display_errors(vm),
    }
}

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

pub use php_sapi::{handle_auth_data as auth_data, AuthData};

/// `SG(request_info).content_length`: the length of the received body.
pub(crate) fn content_length(ctx: &RequestCtx) -> i64 {
    i64::try_from(ctx.request.content.as_ref().map_or(0, Vec::len)).unwrap_or(i64::MAX)
}

/// `sapi_read_standard_form_data()`: unless the body is longer than
/// `post_max_size` (a warning; any earlier buffer stays), what the SAPI
/// has not read yet becomes the request body buffer. The buffer stays in
/// the request (no copy): `php://input` and the urlencoded handler read it
/// from there.
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
    ctx.buffer_body();
    let buffered = ctx.request_body().map_or(0, <[u8]>::len);
    if buffered >= SAPI_POST_BLOCK_SIZE && !body_spill_possible(vm, phase)? {
        // php_stream_temp_write() failed: the stream is truncated.
        let len = ctx.request.content.as_ref().map_or(0, Vec::len);
        *ctx.body.borrow_mut() = crate::sapi::Body::Buffered(len);
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

/// `SAPI_POST_BLOCK_SIZE`: the request body buffer
/// (`php_stream_temp_create_ex(TEMP_STREAM_DEFAULT, SAPI_POST_BLOCK_SIZE,
/// PG(upload_tmp_dir))`) moves to a temporary file once it holds this many
/// bytes.
const SAPI_POST_BLOCK_SIZE: usize = 0x4000;

/// Creates and removes a `php*` file in `dir` (`php_do_open_temporary_file()`).
fn temp_file_possible(dir: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir));
    match rfc1867::open_temporary(std::slice::from_ref(&dir)) {
        Some((file, path)) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            true
        }
        None => false,
    }
}

/// Whether the request body buffer can move to a temporary file
/// (`php_stream_fopen_temporary_file(PG(upload_tmp_dir), "php")` in
/// `php_stream_temp_write()`): `upload_tmp_dir`, else (with a notice when
/// it was set) the system's temporary directory (`sys_temp_dir`, `TMPDIR`,
/// `/tmp`). The body itself stays in memory.
fn body_spill_possible(vm: &mut Vm, phase: Phase) -> VmResult<bool> {
    let upload_dir = ini_bytes(vm, "upload_tmp_dir");
    if !upload_dir.is_empty() {
        if temp_file_possible(&upload_dir) {
            return Ok(true);
        }
        // php_open_temporary_fd_ex(): "Use default temporary directory."
        let msg = format!(
            "{}file created in the system's temporary directory",
            phase.docref_prefix()
        );
        vm.raise(8, &msg)?;
    }
    let mut dir = ini_bytes(vm, "sys_temp_dir");
    if dir.is_empty() {
        dir = std::env::var_os("TMPDIR")
            .filter(|t| !t.is_empty())
            .map_or_else(|| b"/tmp".to_vec(), std::ffi::OsString::into_encoded_bytes);
    }
    Ok(temp_file_possible(&dir))
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
    // The request body buffer, read in place: `php_std_post_handler()`
    // streams it through a buffer that only holds the variable being
    // parsed, so the body itself does not count towards memory_limit.
    let Some(data) = ctx.request_body() else {
        return Ok(());
    };
    if let Some(hook) = php_vm::ext::request_input_hook() {
        if let Some(r) = hook(vm, php_vm::ext::RequestInput::Post, data, post) {
            return r;
        }
    }
    let settings = InputSettings {
        max_input_vars: limits.max_input_vars,
        ..input_settings(vm)
    };
    query::treat_data(
        ParseKind::Post,
        data,
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
        // SG(read_post_bytes): what reads before this parse took.
        read_post_bytes: i64::try_from(ctx.post_read_offset.get()).unwrap_or(i64::MAX),
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
    // The parse stopped at a `memory_limit` violation: the request fails
    // with it (`sapi_handle_post()`), registering nothing more.
    if php_memlimit::is_pending() {
        return Ok(());
    }

    let mut pending: Option<Throw> = None;
    for w in &outcome.warnings {
        let reported = if phase == Phase::ParseBody && w.docref {
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

/// `$_GET` (`treat_data(PARSE_GET)`), empty unless `enabled`.
fn parse_get(vm: &mut Vm, req: &Request, settings: &InputSettings, enabled: bool) -> ZArray {
    let mut get = ZArray::new();
    if enabled {
        if let Some(q) = &req.query_string {
            let hooked = php_vm::ext::request_input_hook()
                .and_then(|h| h(vm, php_vm::ext::RequestInput::Get, c_str(q), &mut get));
            if hooked.is_none() {
                let _ =
                    query::treat_data(ParseKind::Get, c_str(q), &mut get, settings, "", None, vm);
            }
        }
    }
    get
}

/// `$_COOKIE` (`treat_data(PARSE_COOKIE)`), empty unless `enabled`.
fn parse_cookie(vm: &mut Vm, req: &Request, settings: &InputSettings, enabled: bool) -> ZArray {
    let mut cookie = ZArray::new();
    if enabled {
        if let Some(c) = req.header(b"cookie") {
            let c = c_str(c).to_vec();
            let hooked = php_vm::ext::request_input_hook()
                .and_then(|h| h(vm, php_vm::ext::RequestInput::Cookie, &c, &mut cookie));
            if hooked.is_none() {
                let _ =
                    query::treat_data(ParseKind::Cookie, &c, &mut cookie, settings, "", None, vm);
            }
        }
    }
    cookie
}

/// Populates every superglobal for the request.
#[allow(clippy::too_many_lines)]
pub fn populate(vm: &mut Vm, ctx: &RequestCtx, facts: &ServerFacts<'_>, auth: &AuthData) {
    let req = &ctx.request;
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

    // $_POST / $_FILES are read at SAPI activation (before $_GET).
    let mut post = ZArray::new();
    let mut files = ZArray::new();
    // `$_GET`/`$_COOKIE` parsed early for the upload observer.
    let mut early: Option<(ZArray, ZArray)> = None;
    let content_type = req.header(b"content-type").map(<[u8]>::to_vec);
    if vm.ini_bool("enable_post_data_reading") && req.method == Method::Post {
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
                            let get = parse_get(vm, req, &settings, has(b'G'));
                            let cookie = parse_cookie(vm, req, &settings, has(b'C'));
                            set_global(vm, "_GET", get.clone());
                            set_global(vm, "_COOKIE", cookie.clone());
                            early = Some((get, cookie));
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
    // `sapi_handle_post()` → `zend_handle_delayed_oom()`: the soft limit
    // was crossed while parsing the body; request startup bails out
    // before any other superglobal is built (the caller reports it).
    if php_memlimit::is_pending() {
        return;
    }

    // $_GET, $_COOKIE
    #[allow(clippy::single_match_else)] // the None arm is a multi-line computation
    let (get, cookie) = match early {
        Some(parsed) => parsed,
        None => {
            let get = parse_get(vm, req, &settings, has(b'G'));
            (get, parse_cookie(vm, req, &settings, has(b'C')))
        }
    };

    // $_SERVER
    let mut server = ZArray::new();
    if let (true, Some(cgi)) = (has(b'S'), &ctx.cgi) {
        // sapi_cgi_register_variables(): the environment, then PHP_SELF.
        use std::os::unix::ffi::OsStrExt;
        for (k, v) in std::env::vars_os() {
            let v = Value::Str(ZStr::from(v.as_bytes()));
            register(vm, k.as_bytes(), v, &mut server, false);
        }
        let self_ = Value::Str(ZStr::from(cgi.php_self.as_slice()));
        register(vm, b"PHP_SELF", self_, &mut server, false);
    }
    if has(b'S') {
        let entries = if ctx.cgi.is_some() {
            Vec::new()
        } else {
            server_entries(req, facts)
        };
        for (k, v, kind) in entries {
            let v = Value::Str(ZStr::from(v));
            match kind {
                VarKind::Known => {
                    server.make_mut().insert(ArrayKey::from_bytes(&k), v);
                }
                VarKind::Filtered => register(vm, &k, v, &mut server, false),
            }
        }
        let t = server.make_mut();
        let s = |v: &[u8]| Value::Str(ZStr::from(v));
        if let Some(u) = &auth.0 {
            t.insert("PHP_AUTH_USER", s(u));
        }
        if let Some(p) = &auth.1 {
            t.insert("PHP_AUTH_PW", s(p));
        }
        if let Some(d) = &auth.2 {
            t.insert("PHP_AUTH_DIGEST", s(d));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        t.insert("REQUEST_TIME_FLOAT", Value::Float(now));
        #[allow(clippy::cast_possible_truncation)]
        t.insert("REQUEST_TIME", Value::Int(now as i64));
        if vm.ini_bool("register_argc_argv") {
            let _ = vm.php_deprecated(
                "Deriving $_SERVER['argv'] from the query string is deprecated. Configure register_argc_argv=0 to turn this message off",
            );
            let q = req.query_string.clone().unwrap_or_default();
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
    let ctx = crate::sapi::current();
    let content_type = ctx
        .as_ref()
        .and_then(|c| c.request.header(b"content-type"))
        .map(|ct| c_str(ct).to_vec());
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
            if ctx.request.method == Method::Post {
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

    fn sample() -> Request {
        Request {
            method: Method::Get,
            protocol_version: 101,
            request_uri: Some(b"/s.php/x?a=1".to_vec()),
            vpath: Some(b"/s.php".to_vec()),
            path_translated: Some(b"/root/s.php".to_vec()),
            path_info: Some(b"/x".to_vec()),
            query_string: Some(b"a=1".to_vec()),
            headers: vec![
                (b"host".to_vec(), b"localhost".to_vec()),
                (b"content-type".to_vec(), b"text/plain".to_vec()),
                (b"x-my-header".to_vec(), b"v".to_vec()),
            ],
            ..Request::default()
        }
    }

    #[test]
    fn server_vars() {
        let facts = ServerFacts {
            document_root: b"/root",
            host: b"localhost",
            port: 8080,
            router: None,
            client_addr: "127.0.0.1:5555",
        };
        let e = server_entries(&sample(), &facts);
        let names: Vec<String> = e
            .iter()
            .map(|(k, _, _)| String::from_utf8_lossy(k).into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "DOCUMENT_ROOT",
                "REMOTE_ADDR",
                "REMOTE_PORT",
                "SERVER_SOFTWARE",
                "SERVER_PROTOCOL",
                "SERVER_NAME",
                "SERVER_PORT",
                "REQUEST_URI",
                "REQUEST_METHOD",
                "SCRIPT_NAME",
                "SCRIPT_FILENAME",
                "PATH_INFO",
                "PHP_SELF",
                "QUERY_STRING",
                "HTTP_HOST",
                "CONTENT_TYPE",
                "HTTP_CONTENT_TYPE",
                "HTTP_X_MY_HEADER"
            ]
        );
        let get = |n: &str| {
            e.iter()
                .find(|(k, _, _)| k == n.as_bytes())
                .map(|(_, v, _)| String::from_utf8_lossy(v).into_owned())
        };
        assert_eq!(get("REMOTE_ADDR").as_deref(), Some("127.0.0.1"));
        assert_eq!(get("REMOTE_PORT").as_deref(), Some("5555"));
        assert_eq!(get("PHP_SELF").as_deref(), Some("/s.php/x"));
        assert_eq!(get("SERVER_PROTOCOL").as_deref(), Some("HTTP/1.1"));
        assert_eq!(get("SERVER_PORT").as_deref(), Some("8080"));
    }

    #[test]
    fn ipv6_client_and_router_filename() {
        let mut r = sample();
        r.path_translated = None;
        r.path_info = None;
        let facts = ServerFacts {
            document_root: b"/root",
            host: b"::1",
            port: 1,
            router: Some(b"/root/router.php"),
            client_addr: "[::1]:4242",
        };
        let e = server_entries(&r, &facts);
        let get = |n: &str| {
            e.iter()
                .find(|(k, _, _)| k == n.as_bytes())
                .map(|(_, v, _)| String::from_utf8_lossy(v).into_owned())
        };
        assert_eq!(get("REMOTE_ADDR").as_deref(), Some("::1"));
        assert_eq!(get("SCRIPT_FILENAME").as_deref(), Some("/root/router.php"));
        assert_eq!(get("PATH_INFO"), None);
        assert_eq!(get("PHP_SELF").as_deref(), Some("/s.php"));
    }

    #[test]
    fn auth() {
        assert_eq!(
            auth_data(Some(b"Basic dXNlcjpwYXNz")),
            (Some(b"user".to_vec()), Some(b"pass".to_vec()), None)
        );
        assert_eq!(
            auth_data(Some(b"Digest x")),
            (None, None, Some(b"x".to_vec()))
        );
        assert_eq!(build_argv(b"a+b"), vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(
            content_type_key(b"Multipart/Form-Data; b=1"),
            b"multipart/form-data"
        );
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

    /// sapi/cgi/tests/bug69487.phpt: the body buffer cannot spill into a
    /// directory that does not exist.
    #[test]
    fn temporary_files_for_the_body_buffer() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir();
        assert!(temp_file_possible(dir.as_os_str().as_bytes()));
        assert!(!temp_file_possible(b"/not-supposed-to-exist"));
    }
}
