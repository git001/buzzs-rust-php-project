// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `cli-server` SAPI: running one request on a fresh [`Vm`].
//!
//! Each PHP request runs on its own thread (fresh thread-local state for
//! the VM and the extension crates) while the event loop waits, so the
//! server stays single-threaded like PHP's.  The request context is
//! reachable from the SAPI's native functions through a thread-local
//! ([`with_ctx`]).  Output goes straight to the client socket
//! (`sapi_cli_server_ub_write`), preceded by the response head.

use crate::headers::{HeaderFacts, HeaderOp, SapiHeaders};
use crate::request::Request;
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_sapi_input::body::RequestBody;
use php_vm::ext::{Args, Extension, ModuleBuilder, NativeFn};
use php_vm::{Throw, Vm, VmOptions, VmResult};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

/// `E_WARNING`.
const E_WARNING: i64 = 2;

/// The extensions linked into the binary (stateless, so one set serves
/// every request's VM).
pub type ExtensionFactory = fn() -> &'static [&'static dyn Extension];

/// The server-wide configuration a request needs.
#[derive(Clone)]
pub struct ServerConfig {
    /// Absolute document root.
    pub document_root: Vec<u8>,
    /// Host as given to `-S`.
    pub host: String,
    /// Bound port.
    pub port: u16,
    /// The router script.
    pub router: Option<Vec<u8>>,
    /// INI settings (`-d`), after the SAPI defaults.
    pub ini: Vec<(String, String)>,
    /// The linked extensions.
    pub extensions: ExtensionFactory,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field(
                "document_root",
                &String::from_utf8_lossy(&self.document_root),
            )
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "router",
                &self.router.as_ref().map(|r| String::from_utf8_lossy(r)),
            )
            .finish_non_exhaustive()
    }
}

/// State of one request shared by the VM thread's components.
#[derive(Debug)]
pub struct RequestCtx {
    /// The parsed request.
    pub request: Request,
    /// The client connection (blocking while the script runs); `None`
    /// for a CGI request, which writes to standard output.
    pub stream: Option<TcpStream>,
    /// `ip:port` of the client.
    pub client_addr: String,
    /// `SG(sapi_headers)`.
    pub headers: RefCell<SapiHeaders>,
    /// Method, protocol and default content type.
    pub facts: RefCell<HeaderFacts>,
    /// The connection failed (`PHP_CONNECTION_ABORTED`).
    pub aborted: Cell<bool>,
    /// `client->headers_written`.
    pub headers_written: Cell<bool>,
    /// `sapi_cli_server_discard_headers` is in effect.
    pub discard_headers: Cell<bool>,
    /// The request body read so far (`SG(request_info).request_body`,
    /// `SG(read_post_bytes)`, which is `client->post_read_offset` here).
    pub body: RequestBody,
    /// `header_register_callback()` callable.
    pub header_callback: RefCell<Option<Value>>,
    /// The router returned `false` (set by the return hook).
    pub router_declined: Cell<bool>,
    /// Set when the request runs under the CGI SAPI (`php-cgi`).
    pub cgi: Option<CgiInfo>,
    /// The exit status of the request's `Vm::shutdown`.
    pub exit_status: Cell<i32>,
}

/// What the CGI SAPI (`sapi/cgi/cgi_main.c`, used by `php-cgi`) changes in
/// a request: standard output instead of a socket, `Status:` headers, the
/// environment as `$_SERVER`, and the command-line switches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgiInfo {
    /// `SG(request_info).no_headers` (`-q`, `-f`, `-l`).
    pub no_headers: bool,
    /// `SAPI_OPTION_NO_CHDIR` (`-C`).
    pub no_chdir: bool,
    /// `$_SERVER['PHP_SELF']` (`sapi_cgi_register_variables`).
    pub php_self: Vec<u8>,
    /// Code to run instead of reading `path_translated` (`-i`, `-m`).
    pub source: Option<Vec<u8>>,
    /// `SG(request_info).argv`: the script and its arguments when they
    /// are given on the command line (empty otherwise).
    pub argv: Vec<Vec<u8>>,
    /// `cgi.rfc2616_headers`.
    pub rfc2616_headers: Cell<bool>,
    /// `cgi.nph`.
    pub nph: Cell<bool>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<RequestCtx>>> = const { RefCell::new(None) };
}

/// Runs `f` with the current request context (`None` outside a request).
pub fn with_ctx<R>(f: impl FnOnce(&RequestCtx) -> R) -> Option<R> {
    let ctx = current()?;
    Some(f(&ctx))
}

/// The current request context (`None` outside a request).
pub fn current() -> Option<Rc<RequestCtx>> {
    CURRENT.with(|c| c.borrow().clone())
}

impl RequestCtx {
    /// A context for a parsed request on `stream`.
    pub fn new(request: Request, stream: TcpStream, client_addr: String) -> Self {
        let facts = HeaderFacts {
            method: request.method.as_str().as_bytes().to_vec(),
            // `SG(request_info).proto_num` is `major * 100 + minor` here.
            proto_num: request.protocol_version,
            ..HeaderFacts::default()
        };
        RequestCtx {
            request,
            stream: Some(stream),
            client_addr,
            headers: RefCell::new(SapiHeaders::new()),
            facts: RefCell::new(facts),
            aborted: Cell::new(false),
            headers_written: Cell::new(false),
            discard_headers: Cell::new(false),
            body: RequestBody::new(),
            header_callback: RefCell::new(None),
            router_declined: Cell::new(false),
            cgi: None,
            exit_status: Cell::new(0),
        }
    }

    /// A context for a CGI request: output goes to standard output.
    pub fn new_cgi(request: Request, cgi: CgiInfo) -> Self {
        let facts = HeaderFacts {
            method: request.method.as_str().as_bytes().to_vec(),
            // `init_request_info()`: `SG(request_info).proto_num = 1000`.
            proto_num: 1000,
            ..HeaderFacts::default()
        };
        RequestCtx {
            request,
            stream: None,
            client_addr: String::new(),
            headers: RefCell::new(SapiHeaders::new()),
            facts: RefCell::new(facts),
            aborted: Cell::new(false),
            headers_written: Cell::new(false),
            discard_headers: Cell::new(false),
            body: RequestBody::new(),
            header_callback: RefCell::new(None),
            router_declined: Cell::new(false),
            cgi: Some(cgi),
            exit_status: Cell::new(0),
        }
    }

    /// Applies a header operation.
    pub fn header_op(&self, op: HeaderOp<'_>) -> Result<bool, crate::headers::HeaderError> {
        let facts = self.facts.borrow();
        self.headers.borrow_mut().op(op, &facts)
    }

    /// Whether headers were sent.
    pub fn headers_sent(&self) -> bool {
        self.headers.borrow().sent
    }

    /// `php_cli_server_client_send_through()`.
    fn send_through(&self, data: &[u8]) {
        if self.aborted.get() {
            return;
        }
        let ok = match &self.stream {
            Some(stream) => {
                let mut s: &TcpStream = stream;
                s.write_all(data).is_ok()
            }
            // `sapi_cgi_ub_write()`.
            None => std::io::stdout().write_all(data).is_ok(),
        };
        if !ok {
            self.aborted.set(true);
        }
    }

    /// `sapi_send_headers()` (no-op once sent); the header callback is
    /// invoked by callers that have the VM.
    pub fn send_headers(&self) {
        let head = {
            let facts = self.facts.borrow();
            let mut h = self.headers.borrow_mut();
            if !h.finalize(&facts) {
                return;
            }
            if self.discard_headers.get() || self.headers_written.get() {
                return;
            }
            self.headers_written.set(true);
            match &self.cgi {
                // `SG(request_info).no_headers`.
                Some(cgi) if cgi.no_headers => return,
                Some(cgi) => h.render_cgi(cgi.rfc2616_headers.get(), cgi.nph.get()),
                None => h.render(self.request.protocol_version, crate::status::now()),
            }
        };
        self.send_through(&head);
        if self.aborted.get() {
            // SAPI_HEADER_SEND_FAILED resets SG(headers_sent).
            self.headers.borrow_mut().sent = false;
        }
    }

    /// `sapi_cli_server_ub_write()` after the output layer: headers first.
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() || self.aborted.get() {
            return;
        }
        self.send_headers();
        self.send_through(data);
    }

    /// Re-reads the INI values cached by the SAPI.
    pub fn refresh_facts(&self, vm: &Vm) {
        let mut f = self.facts.borrow_mut();
        f.default_mimetype = vm.ini_string("default_mimetype").unwrap_or_default();
        f.default_charset = vm.ini_string("default_charset").unwrap_or_default();
        if let Some(cgi) = &self.cgi {
            cgi.rfc2616_headers.set(vm.ini_bool("cgi.rfc2616_headers"));
            cgi.nph.set(vm.ini_bool("cgi.nph"));
        }
    }
}

/// The VM's standard output: the client socket of the request the VM
/// currently serves (the VM is created before its request is known).
/// `flush()` is a no-op (the VM flushes internally; PHP's `flush()` is a
/// SAPI function).
pub struct ServerOutput(pub Rc<RefCell<Option<Rc<RequestCtx>>>>);

impl std::fmt::Debug for ServerOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServerOutput")
    }
}

impl Write for ServerOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ctx = self.0.borrow().clone();
        if let Some(ctx) = ctx {
            ctx.write(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `E_FATAL_ERRORS`.
pub fn is_fatal_level(level: i64) -> bool {
    level & (1 | 4 | 16 | 64 | 256 | 4096) != 0
}

/// The 500 status `php_error_cb` sets for a fatal error with
/// `display_errors` off.
fn apply_fatal_status(vm: &Vm, ctx: &RequestCtx) {
    let fatal = vm
        .last_error
        .as_ref()
        .is_some_and(|e| is_fatal_level(e.level));
    if !fatal || php_sapi_input::vars::display_errors(vm) {
        return;
    }
    if !ctx.headers_sent() && ctx.headers.borrow().response_code == 200 {
        let _ = ctx.header_op(HeaderOp::Replace(b"HTTP/1.0 500 Internal Server Error", 0));
    }
}

/// Invokes the `header_register_callback()` callable once, before the
/// headers are sent.
fn call_header_callback(vm: &mut Vm, ctx: &RequestCtx) {
    if ctx.headers_sent() {
        return;
    }
    let cb = ctx.header_callback.borrow_mut().take();
    if let Some(cb) = cb {
        let _ = vm.call_value(&cb, Vec::new());
    }
}

// ---------------------------------------------------------------------
// SAPI functions

fn str_value(s: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(s.as_ref()))
}

fn ctx_or<R>(default: R, f: impl FnOnce(&RequestCtx) -> R) -> R {
    with_ctx(f).unwrap_or(default)
}

fn header_result(vm: &mut Vm, r: Result<bool, crate::headers::HeaderError>) -> VmResult<bool> {
    match r {
        Ok(ok) => Ok(ok),
        Err(e) => {
            // `sapi_header_op()`: the place output started, when known.
            let msg = match (&e, vm.output_started()) {
                (crate::headers::HeaderError::AlreadySent, Some((f, l))) => format!(
                    "{} by (output started at {}:{l})",
                    e.message(),
                    f.to_string_lossy()
                ),
                _ => e.message().to_string(),
            };
            vm.raise(E_WARNING, &msg)?;
            Ok(false)
        }
    }
}

fn header(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let line = a.str(0);
    let replace = if a.has(1) { a.bool(1) } else { true };
    let code = if a.has(2) { a.int(2) } else { 0 };
    add_response_header(vm, line.as_bytes(), replace, code)?;
    Ok(Value::Null)
}

fn add_response_header(vm: &mut Vm, line: &[u8], replace: bool, code: i64) -> VmResult<bool> {
    let op = if replace {
        HeaderOp::Replace(line, code)
    } else {
        HeaderOp::Add(line, code)
    };
    let r = ctx_or(Ok(false), |c| c.header_op(op));
    let accepted = header_result(vm, r)?;
    if accepted {
        vm.accepted_response_header(line)?;
    }
    Ok(accepted)
}

fn header_remove(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a
        .opt(0)
        .filter(|v| !matches!(v, Value::Null))
        .map(|_| a.str(0));
    let r = ctx_or(Ok(false), |c| match &name {
        None => c.header_op(HeaderOp::DeleteAll),
        Some(n) => c.header_op(HeaderOp::Delete(n.as_bytes())),
    });
    header_result(vm, r)?;
    Ok(Value::Null)
}

#[allow(clippy::unnecessary_wraps)]
fn headers_sent(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let sent = ctx_or(false, RequestCtx::headers_sent);
    with_ctx(|c| {
        let h = c.headers.borrow();
        vm.set_response_mime_state(h.mimetype.clone(), h.send_default_content_type);
    });
    if a.has(0) {
        a.set_ref(0, str_value(""));
    }
    if a.has(1) {
        a.set_ref(1, Value::Int(0));
    }
    Ok(Value::Bool(sent))
}

#[allow(clippy::unnecessary_wraps)]
fn headers_list(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let list = ctx_or(Vec::new(), |c| c.headers.borrow().headers.clone());
    Ok(Value::Array(ZArray::from_values(
        list.into_iter().map(str_value),
    )))
}

fn http_response_code(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let code = if a.has(0) { a.int(0) } else { 0 };
    let Some((sent, current, has_line)) = with_ctx(|c| {
        let h = c.headers.borrow();
        (h.sent, h.response_code, h.status_line.is_some())
    }) else {
        return Ok(Value::Bool(false));
    };
    if code != 0 {
        if sent {
            vm.php_warning("Cannot set response code - headers already sent")?;
            return Ok(Value::Bool(false));
        }
        if has_line {
            vm.php_warning("Calling http_response_code() after header('HTTP/...') has no effect")?;
        }
        let code = i32::try_from(code).unwrap_or(if code < 0 { i32::MIN } else { i32::MAX });
        with_ctx(|c| c.headers.borrow_mut().response_code = code);
        return Ok(if current != 0 {
            Value::Int(i64::from(current))
        } else {
            Value::Bool(true)
        });
    }
    if current == 0 {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Int(i64::from(current)))
}

#[allow(clippy::unnecessary_wraps)]
fn header_register_callback(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    with_ctx(|c| *c.header_callback.borrow_mut() = Some(cb));
    Ok(Value::Bool(true))
}

fn setcookie_common(vm: &mut Vm, a: &mut Args, raw: bool) -> VmResult<Value> {
    use crate::cookie::{Cookie, CookieError};
    let func = a.func_name();
    let mut c = Cookie {
        name: a.str(0).as_bytes().to_vec(),
        value: a.has(1).then(|| a.str(1).as_bytes().to_vec()),
        ..Cookie::default()
    };
    match a.get(2) {
        Some(Value::Array(opts)) => {
            if (3..7).any(|i| a.has(i)) {
                return Err(vm.throw(
                    "ArgumentCountError",
                    &format!(
                        "{func}(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array"
                    ),
                ));
            }
            for (k, v) in opts.iter() {
                let v = v.deref();
                let ArrayKey::Str(k) = k else {
                    return Err(vm.throw(
                        "ValueError",
                        &format!("{func}(): option array cannot have numeric keys"),
                    ));
                };
                let key = k.as_bytes().to_ascii_lowercase();
                match key.as_slice() {
                    b"expires" => c.expires = vm.value_to_int(&v)?,
                    b"path" => c.path = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"domain" => c.domain = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"secure" => c.secure = vm.value_to_bool(&v)?,
                    b"httponly" => c.httponly = vm.value_to_bool(&v)?,
                    b"samesite" => c.samesite = Some(vm.value_to_string(&v)?.as_bytes().to_vec()),
                    b"partitioned" => c.partitioned = vm.value_to_bool(&v)?,
                    _ => {
                        return Err(vm.throw(
                            "ValueError",
                            &format!("{func}(): option \"{}\" is invalid", k.to_string_lossy()),
                        ))
                    }
                }
            }
        }
        Some(v) => c.expires = vm.value_to_int(&v)?,
        None => {}
    }
    if a.has(3) {
        c.path = Some(a.str(3).as_bytes().to_vec());
    }
    if a.has(4) {
        c.domain = Some(a.str(4).as_bytes().to_vec());
    }
    if a.has(5) {
        c.secure = a.bool(5);
    }
    if a.has(6) {
        c.httponly = a.bool(6);
    }
    let line = match c.header_line(!raw, crate::status::now()) {
        Ok(l) => l,
        Err(CookieError::ArgumentEmpty(n)) => {
            return Err(a.value_error(vm, n, "must not be empty"))
        }
        Err(CookieError::ArgumentValue(n, msg)) => return Err(a.value_error(vm, n, msg)),
        Err(CookieError::Value(msg)) => {
            return Err(vm.throw("ValueError", &format!("{func}(): {msg}")))
        }
    };
    let r = ctx_or(Ok(false), |ctx| ctx.header_op(HeaderOp::Add(&line, 0)));
    Ok(Value::Bool(header_result(vm, r)?))
}

fn setcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, false)
}

fn setrawcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, true)
}

/// `flush()`: `sapi_cli_server_flush` sends the headers.
#[allow(clippy::unnecessary_wraps)]
fn flush(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.flush_stdout();
    let ctx = CURRENT.with(|c| c.borrow().clone());
    if let Some(ctx) = ctx {
        call_header_callback(vm, &ctx);
        ctx.send_headers();
    }
    vm.headers_sent = true;
    Ok(Value::Null)
}

/// PID of the server while a request runs in a forked child of it (0 in
/// the server itself).
static SERVER_PID: AtomicU32 = AtomicU32::new(0);

/// Records, in a request child, the PID of the server process that forked
/// it: PHP runs every request in the server process, so `getmypid()` is
/// the same for all requests a server (or worker) handles.
pub fn set_server_pid(pid: u32) {
    SERVER_PID.store(pid, Ordering::Relaxed);
}

/// The PID `getmypid()` reports.
#[must_use]
pub fn server_pid() -> u32 {
    match SERVER_PID.load(Ordering::Relaxed) {
        0 => std::process::id(),
        pid => pid,
    }
}

#[allow(clippy::unnecessary_wraps)]
fn getmypid(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::from(server_pid())))
}

#[allow(clippy::unnecessary_wraps)]
fn php_sapi_name(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(str_value("cli-server"))
}

#[allow(clippy::unnecessary_wraps)]
fn php_sapi_name_cgi(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(str_value(CGI_SAPI_NAME))
}

#[allow(clippy::unnecessary_wraps)]
fn connection_aborted(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(ctx_or(false, |c| c.aborted.get())))
}

#[allow(clippy::unnecessary_wraps)]
fn connection_status(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::from(ctx_or(false, |c| c.aborted.get()))))
}

/// `apache_request_headers()` / `getallheaders()`.
#[allow(clippy::unnecessary_wraps)]
fn apache_request_headers(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let mut arr = ZArray::new();
    with_ctx(|c| {
        let t = arr.make_mut();
        for (k, v) in &c.request.headers_original_case {
            t.insert(ArrayKey::from_bytes(k), str_value(v));
        }
    });
    Ok(Value::Array(arr))
}

/// `apache_response_headers()` (`add_response_header`).
#[allow(clippy::unnecessary_wraps)]
fn apache_response_headers(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let list = ctx_or(Vec::new(), |c| c.headers.borrow().headers.clone());
    let mut arr = ZArray::new();
    let t = arr.make_mut();
    for h in list {
        let Some(colon) = h.iter().position(|&c| c == b':') else {
            continue;
        };
        let mut len = colon;
        while len > 0 && (h[len - 1] == b' ' || h[len - 1] == b'\t') {
            len -= 1;
        }
        if len == 0 {
            continue;
        }
        let mut p = colon + 1;
        while p < h.len() && (h[p] == b' ' || h[p] == b'\t') {
            p += 1;
        }
        t.insert(ArrayKey::from_bytes(&h[..len]), str_value(&h[p..]));
    }
    Ok(Value::Array(arr))
}

/// Records the router's top-level return value (see [`crate::router`]).
#[allow(clippy::unnecessary_wraps)]
fn router_return(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    with_ctx(|c| c.router_declined.set(matches!(v, Value::Bool(false))));
    Ok(v)
}

/// `cgi_sapi_module.name`.
pub const CGI_SAPI_NAME: &str = "cgi-fcgi";

/// The `cgi` module's INI entries (`PHP_INI_BEGIN` of `cgi_main.c`):
/// name, default, whether `PHP_INI_ALL` (else `PHP_INI_SYSTEM`), boolean.
const CGI_INI: &[(&str, Option<&str>, bool, bool)] = &[
    ("cgi.rfc2616_headers", Some("0"), true, true),
    ("cgi.nph", Some("0"), true, true),
    ("cgi.check_shebang_line", Some("1"), false, true),
    ("cgi.force_redirect", Some("1"), false, true),
    ("cgi.redirect_status_env", None, false, false),
    ("cgi.fix_pathinfo", Some("1"), false, true),
    ("cgi.discard_path", Some("0"), false, true),
    ("fastcgi.logging", Some("1"), false, true),
];

/// The `cli_server` module plus the SAPI-provided functions; with `cgi`,
/// the `cgi-fcgi` module of `php-cgi` instead, which provides the same
/// functions over standard output (`sapi/cgi/cgi_main.c`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CliServerModule {
    /// Register as the CGI SAPI.
    pub cgi: bool,
}

impl Extension for CliServerModule {
    fn name(&self) -> &'static str {
        if self.cgi {
            CGI_SAPI_NAME
        } else {
            "cli_server"
        }
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        use php_support::ini::{Access, IniDirective, ModifyHandler};
        m.vm().response_header_hook = Some(add_response_header);
        m.vm().parse_body_hook = Some(crate::vars::parse_body);
        if self.cgi {
            for &(name, default, all, boolean) in CGI_INI {
                m.ini(IniDirective::new(
                    name,
                    default,
                    if all { Access::ALL } else { Access::SYSTEM },
                    if boolean {
                        ModifyHandler::Bool
                    } else {
                        ModifyHandler::String
                    },
                    CGI_SAPI_NAME,
                ));
            }
        } else {
            m.ini(IniDirective::new(
                "cli_server.color",
                Some("0"),
                Access::ALL,
                ModifyHandler::Bool,
                "cli_server",
            ));
        }
        {
            let vm = m.vm();
            // cli-server has no STDIN/STDOUT/STDERR constants.  The stream
            // resources own descriptors 0-2, so dropping them would close
            // the server's console: keep them alive, unreachable.
            for c in ["STDIN", "STDOUT", "STDERR"] {
                if let Some(v) = vm.constants.remove(c.as_bytes()) {
                    std::mem::forget(v);
                }
            }
            // `sapi_module.log_message`: the CGI SAPI's stderr, the built-in
            // server's console.
            let log: Rc<dyn php_sapi::SapiModule> = if self.cgi {
                Rc::new(php_sapi::StderrLog)
            } else {
                Rc::new(crate::log::ConsoleLog)
            };
            vm.set_sapi_module(log);
        }
        let (sapi, sapi_name): (&str, NativeFn) = if self.cgi {
            (CGI_SAPI_NAME, php_sapi_name_cgi)
        } else {
            ("cli-server", php_sapi_name)
        };
        m.constant("PHP_SAPI", str_value(sapi))
            .function_static("header(string $header, bool $replace = true, int $response_code = 0): void", header)
            .function_static("header_remove(?string $name = null): void", header_remove)
            .function_static("headers_sent(&$filename = null, &$line = null): bool", headers_sent)
            .function_static("headers_list(): array", headers_list)
            .function_static("http_response_code(int $response_code = 0): int|bool", http_response_code)
            .function_static("header_register_callback(callable $callback): true", header_register_callback)
            .function_static(
                "setcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
                setcookie,
            )
            .function_static(
                "setrawcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
                setrawcookie,
            )
            .function_static("flush(): void", flush)
            .function_static("php_sapi_name(): string|false", sapi_name)
            .function_static("getmypid(): int|false", getmypid)
            .function_static("connection_aborted(): bool", connection_aborted)
            .function_static("connection_status(): int", connection_status)
            .function_static("apache_request_headers(): array", apache_request_headers)
            .function_static("getallheaders(): array", apache_request_headers)
            .function_static("apache_response_headers(): array", apache_response_headers);
        if !self.cgi {
            m.function(
                &format!("{}(mixed $value): mixed", crate::router::RETURN_HOOK),
                router_return,
            );
        }
    }
}

// ---------------------------------------------------------------------
// Request lifecycle

/// What the event loop does after the PHP part of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The response is complete; close the connection.
    Done,
    /// The router declined a static file: serve it.
    ServeStatic,
}

/// Makes `php://input` read the request body.
fn install_input_hook(ctx: &Rc<RequestCtx>) {
    use php_ext_streams::bridge::with_vm;
    let body_ctx = ctx.clone();
    php_streams::state::set_hooks(php_streams::state::Hooks {
        output: Some(Box::new(|data: &[u8]| {
            let data = data.to_vec();
            let _ = with_vm(|vm| vm.output(&data));
        })),
        executed_filename: Some(Box::new(|| {
            with_vm(|vm| vm.current_location().0.as_bytes().to_vec())
        })),
        request_body: Some(Box::new(move || php_sapi_input::body::input(&*body_ctx))),
    });
    php_streams::state::update_settings(|s| s.sapi_is_cli = false);
}

/// `php_execute_script()`: prepend file, the script, append file.
fn execute_script(vm: &mut Vm, src: &[u8], path: &[u8]) -> Result<(), Throw> {
    let ini_path = |vm: &Vm, n: &str| vm.ini_string(n).filter(|v| !v.is_empty());
    if let Some(p) = ini_path(vm, "auto_prepend_file") {
        vm.include_file(&String::from_utf8_lossy(&p))?;
    }
    let p = ZStr::from(path);
    vm.run_main(src, &p)?;
    if let Some(p) = ini_path(vm, "auto_append_file") {
        vm.include_file(&String::from_utf8_lossy(&p))?;
    }
    Ok(())
}

/// `php_cli_server_log_response()`.
pub fn log_response(
    client: &str,
    req: &Request,
    status: i32,
    message: Option<&str>,
    vm: Option<&Vm>,
) {
    let mut line = format!("{client} [{status}]: {} ", req.method.as_str()).into_bytes();
    line.extend_from_slice(req.uri());
    if let Some(m) = message {
        line.extend_from_slice(b" - ");
        line.extend_from_slice(m.as_bytes());
    }
    if let Some(e) = vm.and_then(|vm| vm.last_error.as_ref()) {
        if is_fatal_level(e.level) {
            line.extend_from_slice(b" - ");
            line.extend_from_slice(&e.message);
            line.extend_from_slice(b" in ");
            line.extend_from_slice(e.file.as_bytes());
            line.extend_from_slice(format!(" on line {}", e.line).as_bytes());
        }
    }
    crate::log::write(crate::log::LOG_MESSAGE, &line);
}

/// Request shutdown: shutdown functions and destructors, output flush,
/// the headers (if no output was produced) and upload cleanup.
fn finish(vm: &mut Vm, ctx: &RequestCtx, r: Result<(), Throw>) {
    apply_fatal_status(vm, ctx);
    ctx.exit_status.set(vm.shutdown(r));
    call_header_callback(vm, ctx);
    ctx.send_headers();
    cleanup_uploads(vm, ctx);
}

/// `destroy_uploaded_files_hash()`: deletes the uploads that were not
/// moved, and those an earlier hash held.
fn cleanup_uploads(vm: &mut Vm, ctx: &RequestCtx) {
    php_sapi_input::rfc1867::cleanup(&std::mem::take(&mut vm.rfc1867_uploaded_files));
    php_sapi_input::rfc1867::cleanup(&ctx.body.take_orphaned_uploads());
}

/// A VM created ahead of its request (module startup is the expensive
/// part of a request, so the server prepares the next VM while idle).
pub struct PreparedVm {
    /// The VM.
    pub vm: Vm,
    /// The request its [`ServerOutput`] writes to.
    pub slot: Rc<RefCell<Option<Rc<RequestCtx>>>>,
}

impl std::fmt::Debug for PreparedVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedVm")
    }
}

/// Creates the VM for the next request on the current thread (module
/// startup).
pub fn prepare_vm(cfg: &ServerConfig) -> PreparedVm {
    let opts = VmOptions {
        ini: cfg.ini.clone(),
    };
    let module = CliServerModule::default();
    let mut refs: Vec<&dyn Extension> = (cfg.extensions)().to_vec();
    refs.push(&module);
    let slot = Rc::new(RefCell::new(None));
    let vm = Vm::with_extensions(&opts, Box::new(ServerOutput(slot.clone())), &refs);
    PreparedVm { vm, slot }
}

/// Runs the PHP part of a request (`php_cli_server_dispatch` from request
/// startup to `php_request_shutdown`) on the thread that prepared `vm`.
/// The VM is handed back so that the caller can report the outcome before
/// paying for its destruction.
pub fn run_request(
    vm: PreparedVm,
    ctx: &Rc<RequestCtx>,
    cfg: &ServerConfig,
    is_static: bool,
) -> (Outcome, Vm) {
    CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    *vm.slot.borrow_mut() = Some(ctx.clone());
    let mut machine = vm.vm;
    let outcome = run_request_inner(&mut machine, ctx, cfg, is_static);
    *vm.slot.borrow_mut() = None;
    CURRENT.with(|c| *c.borrow_mut() = None);
    (outcome, machine)
}

fn run_request_inner(
    vm: &mut Vm,
    ctx: &Rc<RequestCtx>,
    cfg: &ServerConfig,
    is_static: bool,
) -> Outcome {
    ctx.refresh_facts(vm);

    // php_cli_server_request_startup() + php_request_startup()
    let auth = crate::vars::auth_data(ctx.request.header(b"authorization"));
    // php_cli_server_client_populate_request_info(), php_handle_auth_data()
    // and sapi_activate() (cookie_data, headers_only).
    let req = &ctx.request;
    let method = req.method.as_str().as_bytes().to_vec();
    vm.request_info = php_sapi::RequestInfo {
        headers_only: php_sapi::RequestInfo::is_head(Some(&method)),
        request_method: Some(method),
        query_string: req.query_string.clone(),
        cookie_data: req.header(b"cookie").map(<[u8]>::to_vec),
        content_length: i64::try_from(req.content.as_ref().map_or(0, Vec::len)).unwrap_or(i64::MAX),
        path_translated: req.path_translated.clone(),
        request_uri: req.request_uri.clone(),
        content_type: req.header(b"content-type").map(<[u8]>::to_vec),
        auth_user: auth.0.clone(),
        auth_password: auth.1.clone(),
        auth_digest: auth.2.clone(),
        proto_num: req.protocol_version,
        argv: ctx.cgi.as_ref().map(|c| c.argv.clone()).unwrap_or_default(),
        ..php_sapi::RequestInfo::default()
    };
    let ob = vm.ini.get_long("output_buffering");
    let implicit_flush = vm.ini.get_bool("implicit_flush");
    php_support::output::request_startup(vm, None, ob, implicit_flush);
    if vm.ini_bool("expose_php") {
        let line = format!("X-Powered-By: PHP/{}", php_vm::PHP_VERSION);
        let _ = ctx.header_op(HeaderOp::Replace(line.as_bytes(), 0));
    }
    let limit = u64::try_from(vm.ini.get_long("memory_limit")).unwrap_or(u64::MAX);
    if php_memlimit::request_start(php_memlimit::limit_from_ini(limit)).is_err() {
        php_memlimit::current().force_limit(Some(php_memlimit::DEFAULT_LIMIT));
    }
    let facts = crate::vars::ServerFacts {
        document_root: &cfg.document_root,
        host: cfg.host.as_bytes(),
        port: cfg.port,
        router: cfg.router.as_deref(),
        client_addr: &ctx.client_addr,
    };
    // php_hash_environment() (which parses the body, reporting uploads to
    // the rfc1867 observer) precedes zend_activate_modules() (RINIT).
    let vars = crate::vars::ServerVars { ctx, facts: &facts };
    php_sapi_input::vars::populate(vm, &vars);
    for hook in vm.startup_hooks.clone() {
        hook(vm);
    }
    install_input_hook(ctx);
    vm.script_path = ZStr::from("Unknown");
    // `sapi_handle_post()`: a `memory_limit` violation recorded while the
    // body was parsed is reported right away ("in Unknown on line 0"),
    // and the request startup fails: no script runs.
    if php_memlimit::is_pending() {
        finish(vm, ctx, Ok(()));
        return Outcome::Done;
    }

    let mut r = Ok(());
    if let Some(router) = &cfg.router {
        let src = std::fs::read(std::ffi::OsStr::from_bytes(router)).unwrap_or_default();
        let src = crate::router::rewrite(&src).unwrap_or(src);
        r = execute_script(vm, &src, router);
        let declined = r.is_ok() && ctx.router_declined.get();
        if !declined {
            finish(vm, ctx, r);
            return Outcome::Done;
        }
    }

    if let Some(cgi) = &ctx.cgi {
        run_cgi_script(vm, ctx, cgi);
        return Outcome::Done;
    }

    if !is_static {
        let path = ctx.request.path_translated.clone().unwrap_or_default();
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        if let Some(dir) = p.parent() {
            let _ = std::env::set_current_dir(dir);
        }
        r = if let Ok(src) = std::fs::read(p) {
            execute_script(vm, &src, &path)
        } else {
            let msg = format!(
                "Failed opening required '{}' (include_path='{}')",
                String::from_utf8_lossy(&path),
                String::from_utf8_lossy(&vm.ini_string("include_path").unwrap_or_default())
            );
            let _ = vm.raise(64, &msg);
            Err(Throw::Fatal)
        };
        apply_fatal_status(vm, ctx);
        let status = ctx.headers.borrow().response_code;
        log_response(&ctx.client_addr, &ctx.request, status, None, Some(vm));
        if status == 304 {
            ctx.headers.borrow_mut().send_default_content_type = false;
        }
        finish(vm, ctx, r);
        return Outcome::Done;
    }

    // Router declined a static file: shut the request down without
    // sending headers, then serve the file.
    ctx.headers.borrow_mut().send_default_content_type = false;
    ctx.discard_headers.set(true);
    vm.shutdown(r);
    ctx.send_headers();
    cleanup_uploads(vm, ctx);
    Outcome::ServeStatic
}

/// The script part of `main()` in `cgi_main.c`: `php_fopen_primary_script()`
/// (a 404 "No input file specified." or 403 "Access denied." page when it
/// fails), the change to the script's directory (`php_execute_script`,
/// unless `-C`), then the script and request shutdown.
fn run_cgi_script(vm: &mut Vm, ctx: &RequestCtx, cgi: &CgiInfo) {
    let path = ctx.request.path_translated.clone().unwrap_or_default();
    let (src, shown) = if let Some(src) = &cgi.source {
        (src.clone(), b"Standard input code".to_vec())
    } else {
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        match std::fs::read(p)
            .ok()
            .filter(|_| !path.is_empty() && p.is_file())
        {
            Some(src) => {
                if !cgi.no_chdir {
                    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                        let _ = std::env::set_current_dir(dir);
                    }
                }
                (src, path.clone())
            }
            None => {
                let denied = std::fs::File::open(p)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                    && !path.is_empty();
                let (code, msg): (i32, &[u8]) = if denied {
                    (403, &b"Access denied.\n"[..])
                } else {
                    (404, &b"No input file specified.\n"[..])
                };
                let _ = ctx.header_op(HeaderOp::SetStatus(code));
                vm.output(msg);
                finish(vm, ctx, Ok(()));
                ctx.exit_status.set(255);
                return;
            }
        }
    };
    let r = execute_script(vm, &src, &shown);
    finish(vm, ctx, r);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_levels() {
        assert!(is_fatal_level(1));
        assert!(is_fatal_level(64));
        assert!(is_fatal_level(4096));
        assert!(!is_fatal_level(2));
        assert!(!is_fatal_level(8192));
    }

    #[test]
    fn request_children_report_the_server_pid() {
        assert_eq!(server_pid(), std::process::id());
        set_server_pid(4242);
        assert_eq!(server_pid(), 4242);
        set_server_pid(0);
        assert_eq!(server_pid(), std::process::id());
    }
}
