// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! One PHP request: the state shared between the VM, the SAPI functions and
//! the output writer ([`RequestCtx`]), and the request lifecycle
//! ([`run`]) — `php_apache_request_ctor()`, `php_request_startup()`,
//! script execution and `php_request_shutdown()`.
//!
//! A request runs on its own thread (fresh thread-locals for the VM and the
//! extension crates, and a large stack); the context is reachable from
//! native functions through a thread-local ([`with_ctx`]).
//!
//! The parts of `main/SAPI.c` / `main/main.c` that need the VM —
//! `sapi_send_headers()` with its `header_register_callback()` callable
//! ([`php_header`]), `sapi_flush()` ([`sapi_flush`]),
//! `php_handle_aborted_connection()` ([`handle_aborted_connection`]) and
//! `zend_set_timeout()` ([`arm_timer`]) — are installed into the VM as its
//! SAPI hooks by [`crate::functions::Apache2Handler`].

use crate::backend::{Backend, IntField, StrField, Table, TableOp};
use crate::config::ProcessConfig;
use crate::headers::{HeaderFacts, HeaderOp, SapiHeaders};
use php_runtime::{Value, ZStr};
use php_sapi_input::body::{RequestBody, SapiInput};
use php_support::output::{status as output_status, OutputHost};
use php_vm::{Throw, Vm, VmOptions, VmResult};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// `PHP_CONNECTION_ABORTED`.
pub const CONNECTION_ABORTED: i64 = 1;
/// `PHP_CONNECTION_TIMEOUT`.
pub const CONNECTION_TIMEOUT: i64 = 2;

/// What the watchdog found when it checked the [`Timer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// The limit is not reached (or no timer is armed).
    None,
    /// `max_execution_time` elapsed: the executor is to raise the fatal
    /// error.  With `hard_timeout` set the timer now counts that; otherwise
    /// it is disarmed.
    Soft,
    /// `hard_timeout` elapsed after the soft expiry; the timer is disarmed.
    Hard,
}

/// `max_execution_time` bookkeeping shared with the handler thread's
/// watchdog: `zend_set_timeout()` / `zend_timeout_handler()` for a timer
/// the SAPI runs itself.  The request thread's CPU time counts, as
/// php-src's `ITIMER_PROF` timer does on Linux: a script blocked in
/// `sleep()` or on I/O never times out.
///
/// The request thread re-arms it (`set_time_limit()`) while the watchdog
/// checks it, so the state sits behind one lock: a check never sees half
/// of a re-arm.
#[derive(Debug, Default)]
pub struct Timer {
    state: Mutex<TimerState>,
}

/// The [`Timer`]'s state.
#[derive(Debug, Default, Clone, Copy)]
struct TimerState {
    /// Seconds until the armed expiry (0: disarmed).
    limit_secs: i64,
    /// The request thread's CPU time when the timer was (re)armed.
    start_cpu_ns: i64,
    /// `EG(timeout_seconds)`: the configured limit, for messages.
    timeout_secs: i64,
    /// `EG(hard_timeout)` in seconds (0: none).
    hard_secs: i64,
    /// The soft expiry fired; the armed limit is `hard_timeout`.
    hard_armed: bool,
    /// Soft expiries so far: each is `php_on_timeout()`, which ORs
    /// `PHP_CONNECTION_TIMEOUT` into `PG(connection_status)`.
    timeouts: u64,
}

impl Timer {
    fn state(&self) -> MutexGuard<'_, TimerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `zend_set_timeout(secs)`: (re)arms the timer with `secs` seconds
    /// (0 = unlimited) and `hard` as `hard_timeout`.  Call on the request
    /// thread: its CPU clock is the one measured.
    pub fn reset(&self, secs: i64, hard: i64) {
        self.reset_at(thread_cpu_ns(), secs, hard);
    }

    /// [`Timer::reset`] with the request thread's CPU time given.
    pub(crate) fn reset_at(&self, cpu_ns: i64, secs: i64, hard: i64) {
        let mut s = self.state();
        s.start_cpu_ns = cpu_ns;
        s.timeout_secs = secs.max(0);
        s.limit_secs = secs.max(0);
        s.hard_secs = hard.max(0);
        s.hard_armed = false;
    }

    /// The configured limit in seconds (`EG(timeout_seconds)`).
    pub fn limit(&self) -> i64 {
        self.state().timeout_secs
    }

    /// `hard_timeout` in seconds.
    pub fn hard_limit(&self) -> i64 {
        self.state().hard_secs
    }

    /// Whether the limit was exceeded (`PHP_CONNECTION_TIMEOUT`).
    pub fn timed_out(&self) -> bool {
        self.timeouts() > 0
    }

    /// How many times the limit was exceeded in this request.
    pub fn timeouts(&self) -> u64 {
        self.state().timeouts
    }

    /// Disarms the timer (`zend_unset_timeout()`).
    pub fn disarm(&self) {
        let mut s = self.state();
        s.limit_secs = 0;
        s.hard_armed = false;
    }

    /// The watchdog's check with the request thread's CPU time `cpu_ns`
    /// (`zend_timeout_handler()` on each expiry): a soft expiry re-arms
    /// for `hard_timeout` when that is set and disarms otherwise; a hard
    /// expiry disarms.
    pub fn check(&self, cpu_ns: i64) -> Expiry {
        let mut s = self.state();
        if s.limit_secs <= 0
            || cpu_ns.saturating_sub(s.start_cpu_ns) < s.limit_secs.saturating_mul(1_000_000_000)
        {
            return Expiry::None;
        }
        if s.hard_armed {
            s.limit_secs = 0;
            s.hard_armed = false;
            return Expiry::Hard;
        }
        s.timeouts += 1;
        if s.hard_secs > 0 {
            s.start_cpu_ns = cpu_ns;
            s.limit_secs = s.hard_secs;
            s.hard_armed = true;
        } else {
            s.limit_secs = 0;
        }
        Expiry::Soft
    }
}

/// CPU time of the calling thread in nanoseconds.
pub fn thread_cpu_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we own.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts);
    }
    ts.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec)
}

/// State of one request shared by the VM thread's components.
pub struct RequestCtx {
    /// The web server side.
    pub backend: Box<dyn Backend>,
    /// `SG(sapi_headers)`.
    pub headers: RefCell<SapiHeaders>,
    /// Method, protocol and default content type.
    pub facts: RefCell<HeaderFacts>,
    /// `SG(request_info).headers_only`: a HEAD request (`sapi_activate()`).
    headers_only: bool,
    /// `PHP_OUTPUT_DISABLED` set by [`RequestCtx::write`] itself (see
    /// there).
    output_disabled: Cell<bool>,
    /// Whether implicit flush is enabled (`implicit_flush` or `zlib.output_compression`).
    pub implicit_flush: Cell<bool>,
    /// `PG(connection_status) & PHP_CONNECTION_ABORTED`: the client went
    /// away.
    pub aborted: Cell<bool>,
    /// `php_handle_aborted_connection()` ran for the abort an `ap_rwrite`
    /// noticed (see [`abort_interrupt`]).
    abort_handled: Cell<bool>,
    /// [`Timer::timeouts`] when the abort was last recorded: the abort
    /// assigns `PG(connection_status) = PHP_CONNECTION_ABORTED`, dropping
    /// the `PHP_CONNECTION_TIMEOUT` bit of earlier timeouts.
    timeouts_at_abort: Cell<u64>,
    /// The request body read so far (`SG(request_info).request_body`,
    /// `SG(read_post_bytes)`).
    pub body: RequestBody,
    /// Whether `ap_setup_client_block` ran (and whether a body follows).
    body_ready: Cell<Option<bool>>,
    /// `max_execution_time` timer.
    pub timer: Arc<Timer>,
    /// `header_register_callback()` callable (`SG(send_header_fcc)`).
    pub header_callback: RefCell<Option<Value>>,
}

impl SapiInput for RequestCtx {
    fn body(&self) -> &RequestBody {
        &self.body
    }

    /// `sapi_module.read_post()`: fills `buf` as far as possible; returns
    /// the number of bytes read (0 at the end).
    fn read_post(&self, buf: &mut [u8]) -> usize {
        if self.aborted.get() || self.backend.int_field(IntField::Aborted) != 0 {
            self.note_abort();
            return 0;
        }
        if !self.body_available() {
            return 0;
        }
        let mut total = 0;
        while total < buf.len() {
            if self.backend.int_field(IntField::Aborted) != 0 {
                self.note_abort();
                break;
            }
            let n = self.backend.body_read(&mut buf[total..]);
            let Ok(n) = usize::try_from(n) else {
                self.note_abort();
                break;
            };
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    }
}

impl std::fmt::Debug for RequestCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestCtx")
            .field("headers", &self.headers)
            .field("aborted", &self.aborted)
            .finish_non_exhaustive()
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<RequestCtx>>> = const { RefCell::new(None) };
    static PERSISTENT_VM: RefCell<Option<Vm>> = const { RefCell::new(None) };
    static ACTIVE_VM: Cell<*mut Vm> = const { Cell::new(std::ptr::null_mut()) };
}

pub(crate) fn with_active_vm<R>(f: impl FnOnce(&mut Vm) -> R) -> Option<R> {
    let ptr = ACTIVE_VM.with(Cell::get);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: ACTIVE_VM is set only while the VM reference is uniquely held on the current thread in run().
        Some(f(unsafe { &mut *ptr }))
    }
}

/// Runs `f` with the current request context (`None` outside a request).
pub fn with_ctx<R>(f: impl FnOnce(&RequestCtx) -> R) -> Option<R> {
    let ctx = current()?;
    Some(f(&ctx))
}

/// The current request context.
pub(crate) fn current() -> Option<Rc<RequestCtx>> {
    CURRENT.with(|c| c.borrow().clone())
}

impl RequestCtx {
    /// A context for `backend`.
    pub fn new(backend: Box<dyn Backend>, timer: Arc<Timer>) -> Self {
        let status = i32::try_from(backend.int_field(IntField::Status)).unwrap_or(200);
        let facts = HeaderFacts {
            method: backend.str_field(StrField::Method).unwrap_or_default(),
            proto_num: i32::try_from(backend.int_field(IntField::ProtoNum)).unwrap_or(1001),
            ..HeaderFacts::default()
        };
        let headers_only = facts.method == b"HEAD";
        let is_aborted = backend.int_field(IntField::Aborted) != 0;
        RequestCtx {
            backend,
            headers: RefCell::new(SapiHeaders::new(status)),
            facts: RefCell::new(facts),
            headers_only,
            output_disabled: Cell::new(false),
            implicit_flush: Cell::new(false),
            aborted: Cell::new(is_aborted),
            abort_handled: Cell::new(false),
            timeouts_at_abort: Cell::new(0),
            body: RequestBody::new(),
            body_ready: Cell::new(None),
            timer,
            header_callback: RefCell::new(None),
        }
    }

    /// Applies a header operation.
    pub fn header_op(&self, op: HeaderOp<'_>) -> Result<bool, crate::headers::HeaderError> {
        let facts = self.facts.borrow();
        self.headers.borrow_mut().op(op, &facts, &*self.backend)
    }

    /// The default `Content-Type` header, added once before the headers go
    /// out: the first half of `sapi_send_headers()`, which php-src runs
    /// before the `header_register_callback()` callable.
    pub fn add_default_content_type(&self) {
        let facts = self.facts.borrow();
        self.headers
            .borrow_mut()
            .add_default_content_type(&facts, &*self.backend);
    }

    /// `sapi_send_headers()` without the VM (no-op once sent); callers
    /// holding the VM use [`php_header`], which also runs the
    /// `header_register_callback()` callable.
    pub fn send_headers(&self) {
        let facts = self.facts.borrow();
        self.headers.borrow_mut().send(&facts, &*self.backend);
    }

    /// The outer `sapi_send_headers()`'s own `php_apache_sapi_send_headers()`
    /// after a `header_register_callback()` callable whose output already
    /// sent the headers (see [`SapiHeaders::resend`]).
    pub(crate) fn resend_headers(&self) {
        let facts = self.facts.borrow();
        self.headers.borrow_mut().resend(&facts, &*self.backend);
    }

    /// Whether headers were sent (`SG(headers_sent)`).
    pub fn headers_sent(&self) -> bool {
        self.headers.borrow().sent
    }

    /// `SG(request_info).headers_only`: a HEAD request gets no body.
    pub fn headers_only(&self) -> bool {
        self.headers_only
    }

    /// `PG(connection_status)`: `PHP_CONNECTION_ABORTED` once the client
    /// is gone, plus `PHP_CONNECTION_TIMEOUT` for a timeout since the abort
    /// was last recorded (or since the request started).
    pub fn connection_status(&self) -> i64 {
        if !self.aborted.get() && self.backend.int_field(IntField::Aborted) != 0 {
            self.record_abort();
        }
        let mut status = 0;
        if self.aborted.get() {
            status |= CONNECTION_ABORTED;
        }
        if self.timer.timeouts() > self.timeouts_at_abort.get() {
            status |= CONNECTION_TIMEOUT;
        }
        status
    }

    /// `PG(connection_status) = PHP_CONNECTION_ABORTED`.
    fn record_abort(&self) {
        self.aborted.set(true);
        self.timeouts_at_abort.set(self.timer.timeouts());
    }

    /// The client went away (`ap_rwrite` / `ap_rflush` failed).  Records
    /// the abort and asks the executor for a safe point, where
    /// [`abort_interrupt`] runs `php_handle_aborted_connection()`; a caller
    /// holding the VM does that directly.
    fn note_abort(&self) {
        let first = !self.aborted.get();
        self.record_abort();
        if first {
            php_vm::interrupt::request_interrupt();
        }
    }

    /// `php_apache_sapi_ub_write()`.  Output of the activated output layer
    /// reaches it once `php_header()` allowed it, so the headers are out.
    /// Output written before the layer is activated (request-startup
    /// warnings) finds them unsent: php-src's layer is already active then
    /// and its `php_header()` would refuse a HEAD request's output for the
    /// rest of the request, which this does in its place.
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if !self.headers_sent() {
            let allowed = with_active_vm(|vm| {
                use php_support::output::OutputHost;
                vm.send_headers()
            });
            match allowed {
                Some(false) => {
                    self.output_disabled.set(true);
                }
                None => {
                    self.send_headers();
                    if self.headers_only {
                        self.output_disabled.set(true);
                    }
                }
                _ => {}
            }
        }
        if self.output_disabled.get() || self.aborted.get() {
            return;
        }
        if self.backend.int_field(IntField::Aborted) != 0 || !self.backend.write(data) {
            self.note_abort();
        } else if self.implicit_flush.get() && ACTIVE_VM.with(|p| p.get().is_null()) {
            self.flush();
        }
    }

    /// `php_apache_sapi_flush()` without the VM: sends the headers, then
    /// `ap_rflush`.  `false` when the client is gone (the caller runs
    /// `php_handle_aborted_connection()`).
    pub fn flush(&self) -> bool {
        self.send_headers();
        if self.aborted.get() || self.backend.int_field(IntField::Aborted) != 0 {
            self.note_abort();
            return false;
        }
        if self.backend.flush() && self.backend.int_field(IntField::Aborted) == 0 {
            return true;
        }
        self.note_abort();
        false
    }

    /// Prepares reading the request body; `false` when there is none.
    fn body_available(&self) -> bool {
        if let Some(b) = self.body_ready.get() {
            return b;
        }
        let ok = self.backend.body_setup().unwrap_or(false);
        self.body_ready.set(Some(ok));
        ok
    }

    /// Re-reads the INI values cached by the SAPI (default content type,
    /// timer limit is handled by the callers).
    pub fn refresh_facts(&self, vm: &Vm) {
        let mut f = self.facts.borrow_mut();
        f.default_mimetype = vm.ini_string("default_mimetype").unwrap_or_default();
        f.default_charset = vm.ini_string("default_charset").unwrap_or_default();
        let implicit = vm.ini_bool("implicit_flush") || is_zlib_compression_on(vm);
        self.implicit_flush.set(implicit);
    }
}

/// Whether `zlib.output_compression` is enabled in the VM configuration.
fn is_zlib_compression_on(vm: &Vm) -> bool {
    if let Some(v) = vm.ini_string("zlib.output_compression") {
        if v.eq_ignore_ascii_case(b"on") || v.eq_ignore_ascii_case(b"1") {
            return true;
        }
        if let Ok(n) = std::str::from_utf8(&v).unwrap_or("").trim().parse::<i64>() {
            return n > 0;
        }
    }
    false
}

/// `php_header()` for this SAPI, installed as the VM's `send_headers_hook`
/// (so it runs when output first reaches the SAPI and at request
/// shutdown): `sapi_send_headers()` adds the default `Content-Type`, runs
/// the `header_register_callback()` callable — whose own output sends the
/// headers at once, as in php-src — and sends the headers through
/// [`RequestCtx::send_headers`].  Returns whether output may follow
/// (`SG(request_info).headers_only` denies it to HEAD requests).
pub fn php_header(vm: &mut Vm) -> bool {
    let Some(ctx) = current() else {
        return true;
    };
    if !ctx.headers_sent() {
        ctx.add_default_content_type();
        crate::handler::execute_header_callback(vm);
        ctx.send_headers();
    }
    vm.headers_sent = true;
    !ctx.headers_only()
}

/// `sapi_flush()`: `php_apache_sapi_flush()` — the headers, then
/// `ap_rflush`; a lost client is `php_handle_aborted_connection()`.
pub fn sapi_flush(vm: &mut Vm) -> VmResult<()> {
    php_header(vm);
    if with_ctx(RequestCtx::flush).unwrap_or(true) {
        return Ok(());
    }
    handle_aborted_connection(vm)
}

/// The VM's `sapi_flush_hook`: the output layer's implicit flush
/// (`implicit_flush`, `ob_implicit_flush()`).  A bailout surfaces right
/// after the write that triggered it, as php-src's `longjmp` does.
pub fn implicit_flush(vm: &mut Vm) {
    if let Err(t) = sapi_flush(vm) {
        if vm.pending_throw.is_none() {
            vm.pending_throw = Some(t);
        }
    }
}

/// `php_handle_aborted_connection()`: `PG(connection_status) =
/// PHP_CONNECTION_ABORTED`, output disabled for the rest of the request
/// (`php_output_set_status`), and a bailout — the script stops, shutdown
/// functions and destructors still run — unless `ignore_user_abort` is on.
pub fn handle_aborted_connection(vm: &mut Vm) -> VmResult<()> {
    with_ctx(RequestCtx::record_abort);
    stop_for_aborted_connection(vm)
}

/// The rest of `php_handle_aborted_connection()` once the status is
/// recorded.
fn stop_for_aborted_connection(vm: &mut Vm) -> VmResult<()> {
    with_ctx(|c| c.abort_handled.set(true));
    // `php_output_set_status(PHP_OUTPUT_DISABLED)`.  Spelled out: `Vm`
    // has an inherent `output(&[u8])` method.
    OutputHost::output(vm).set_status(output_status::DISABLED);
    if vm.ini_bool("ignore_user_abort") {
        Ok(())
    } else {
        Err(Throw::Exit)
    }
}

/// The module's VM interrupt handler: finishes
/// `php_handle_aborted_connection()` once for an abort that `ap_rwrite`
/// noticed — [`RequestCtx::write`] recorded the status but has no VM;
/// php-src bails out from inside `ub_write`.
pub fn abort_interrupt(vm: &mut Vm) -> VmResult<()> {
    let due = with_ctx(|c| {
        if !c.aborted.get() && c.backend.int_field(IntField::Aborted) != 0 {
            c.record_abort();
        }
        c.aborted.get() && !c.abort_handled.get()
    })
    .unwrap_or(false);
    if due {
        stop_for_aborted_connection(vm)
    } else {
        Ok(())
    }
}

/// The VM's `timer_hook` (`zend_set_timeout()`): (re)arms the request's
/// CPU-time timer with the new `max_execution_time`.
pub fn arm_timer(vm: &mut Vm, secs: i64) {
    let hard = vm.ini_long("hard_timeout");
    with_ctx(|c| c.timer.reset(secs, hard));
}

/// The VM's standard output: `ub_write` to Apache, sending the headers
/// before the first byte.  Writes are dispatched to the active request
/// context through [`with_ctx`].  `flush()` is a no-op: the VM flushes its stdout
/// before log writes and at shutdown, which must neither commit the
/// headers nor force a chunked response (Apache computes Content-Length
/// for responses that end without an intermediate flush).  PHP's
/// `flush()` and the implicit flush go through [`sapi_flush`] instead.
#[derive(Debug, Default, Clone)]
pub struct ApacheOutput;

impl Write for ApacheOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        with_ctx(|c| c.write(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `E_ERROR | E_PARSE | E_COMPILE_ERROR | E_USER_ERROR | E_RECOVERABLE_ERROR`:
/// the levels after which `php_error_cb` turns a 200 into a 500 when
/// errors are not displayed.
pub fn is_fatal_level(level: i64) -> bool {
    matches!(level, 1 | 4 | 64 | 256 | 4096)
}

/// The 500 status of `php_error_cb` for a fatal error with
/// `display_errors` off, applied when the request's last error is fatal.
pub fn apply_fatal_status(vm: &Vm) {
    let fatal = vm
        .last_error
        .as_ref()
        .is_some_and(|e| is_fatal_level(e.level));
    if !fatal || php_sapi_input::vars::display_errors(vm) {
        return;
    }
    with_ctx(|ctx| {
        if !ctx.headers_sent() && ctx.headers.borrow().response_code == 200 {
            let _ = ctx.header_op(HeaderOp::Replace(b"HTTP/1.0 500 Internal Server Error", 0));
        }
    });
}

/// Applies the merged `php_value`-family directives (`apply_config()`):
/// `php_admin_*` with `PHP_INI_SYSTEM` access (which also locks the entry
/// against `ini_set`), the others with `PHP_INI_PERDIR`, `.htaccess` ones
/// at the HTACCESS stage.
pub fn apply_dir_config(vm: &mut Vm, entries: &[crate::backend::DirEntry]) {
    use php_support::ini::{Access, Stage};
    let mut sink: Vec<php_support::Diagnostic> = Vec::new();
    for e in entries {
        let access = match e.status {
            crate::backend::DirStatus::PerDir => Access::PERDIR,
            crate::backend::DirStatus::System => Access::SYSTEM,
        };
        let stage = if e.htaccess {
            Stage::Htaccess
        } else {
            Stage::Activate
        };
        vm.ini
            .alter(&e.name, &e.value, access, stage, false, &mut sink);
    }
    sync_cached_ini(vm);
}

/// Mirrors the VM's cached copies of `error_reporting`, `precision` and
/// `serialize_precision` after INI changes made outside `ini_set()`.
pub fn sync_cached_ini(vm: &mut Vm) {
    vm.error_reporting = match vm.ini.get("error_reporting") {
        Some(v) if !v.is_empty() => std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0),
        _ => 30719,
    };
    vm.precision = vm.ini.get_long("precision");
    vm.serialize_precision = vm.ini.get_long("serialize_precision");
}

/// Parameters of [`run`].
#[derive(Debug, Clone)]
pub struct RunParams {
    /// The process configuration.
    pub config: Arc<ProcessConfig>,
    /// The timer watched by the handler thread.
    pub timer: Arc<Timer>,
    /// `application/x-httpd-php-source`: highlight instead of executing.
    pub highlight: bool,
}

pub use php_sapi::{handle_auth_data as auth_data, AuthData};

/// `SG(request_info)` as `php_apache_request_ctor()` and `sapi_activate()`
/// fill it from the `request_rec`, with the authentication already worked
/// out (`php_handle_auth_data()`, else `r->user`).
fn request_info(
    r: &dyn Backend,
    user: Option<&[u8]>,
    password: Option<&[u8]>,
    digest: Option<&[u8]>,
) -> php_sapi::RequestInfo {
    let header = |name: &[u8]| r.table_get(Table::HeadersIn, name);
    let method = r.str_field(StrField::Method);
    php_sapi::RequestInfo {
        headers_only: php_sapi::RequestInfo::is_head(method.as_deref()),
        request_method: method,
        query_string: r.str_field(StrField::Args),
        cookie_data: header(b"Cookie"),
        content_length: header(b"Content-Length").map_or(0, |v| php_support::atol(&v)),
        path_translated: r.str_field(StrField::Filename),
        request_uri: r.str_field(StrField::Uri),
        content_type: header(b"Content-Type"),
        auth_user: user.map(<[u8]>::to_vec),
        auth_password: password.map(<[u8]>::to_vec),
        auth_digest: digest.map(<[u8]>::to_vec),
        proto_num: i32::try_from(r.int_field(IntField::ProtoNum)).unwrap_or(0),
        ..php_sapi::RequestInfo::default()
    }
}

/// Escapes `s` for a single-quoted PHP string literal.
fn php_single_quoted(s: &[u8]) -> Vec<u8> {
    let mut out = vec![b'\''];
    for &c in s {
        if c == b'\'' || c == b'\\' {
            out.push(b'\\');
        }
        out.push(c);
    }
    out.push(b'\'');
    out
}

/// Runs one request on the current thread (`php_handler()` from
/// `php_apache_request_ctor()` to `php_apache_request_dtor()`), maintaining
/// a persistent VM per worker thread and resetting state between requests.
pub fn run(backend: Box<dyn Backend>, params: &RunParams) {
    let _ = php_memlimit::bind_to_current_thread();
    let ctx = Rc::new(RequestCtx::new(backend, params.timer.clone()));
    CURRENT.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    let _cleanup = RequestScopeGuard;

    PERSISTENT_VM.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if borrow.is_none() {
            let opts = VmOptions {
                ini: params.config.ini.clone(),
            };
            let exts = crate::extensions::all();
            let refs: Vec<&dyn php_vm::ext::Extension> = exts.iter().map(AsRef::as_ref).collect();
            let vm = Vm::with_extensions(&opts, Box::new(ApacheOutput), &refs);
            *borrow = Some(vm);
        }
        let vm = borrow.as_mut().unwrap();

        apply_dir_config(vm, &ctx.backend.dir_config());
        ctx.refresh_facts(vm);
        if ctx.implicit_flush.get() {
            let mut sink = Vec::new();
            vm.ini.alter(
                "implicit_flush",
                b"1",
                php_support::ini::Access::SYSTEM,
                php_support::ini::Stage::Runtime,
                false,
                &mut sink,
            );
            php_support::output::ob_implicit_flush(vm, true);
        }
        // `zend_set_timeout(EG(timeout_seconds))` at request startup, through
        // the VM's timer hook ([`arm_timer`] → `params.timer`): it also clears
        // an engine timeout flag a previous request left unhandled.  With
        // `max_input_time` at -1 the limit covers reading the input too;
        // `Vm::run_script` re-arms it when the script starts.
        let limit = vm.ini_long("max_execution_time");
        vm.set_time_limit(limit);

        // php_apache_request_ctor()
        let headers_in_auth = ctx.backend.table_get(Table::HeadersIn, b"Authorization");
        let (mut user, password, digest) = auth_data(headers_in_auth.as_deref());
        if user.is_none() {
            user = ctx.backend.str_field(StrField::User);
        }
        ctx.backend.set_user(user.as_deref());
        vm.request_info = request_info(
            &*ctx.backend,
            user.as_deref(),
            password.as_deref(),
            digest.as_deref(),
        );
        for h in ["Content-Length", "Last-Modified", "Expires", "ETag"] {
            ctx.backend
                .table_op(Table::HeadersOut, TableOp::Unset, h.as_bytes(), b"");
        }
        if vm.ini_bool("last_modified") {
            ctx.backend.set_last_modified();
        }

        // php_request_startup()
        vm.script_path = ZStr::from("Unknown");
        if vm.ini_bool("expose_php") {
            let line = format!("X-Powered-By: PHP/{}", php_vm::PHP_VERSION);
            let _ = ctx.header_op(HeaderOp::Replace(line.as_bytes(), 0));
        }
        php_sapi_input::vars::populate(vm, &*ctx);
        install_input_hook();
        vm.shutdown_functions
            .insert(0, (Value::from(crate::functions::FATAL_CHECK), Vec::new()));

        if ctx.backend.int_field(IntField::Aborted) != 0 {
            ctx.record_abort();
            if !vm.ini_bool("ignore_user_abort") {
                finish(vm, &ctx);
                vm.request_reset();
                return;
            }
        }

        struct VmGuard;
        impl Drop for VmGuard {
            fn drop(&mut self) {
                ACTIVE_VM.with(|c| c.set(std::ptr::null_mut()));
            }
        }
        ACTIVE_VM.with(|c| c.set(vm as *mut Vm));
        let _vm_guard = VmGuard;

        let filename = ctx
            .backend
            .str_field(StrField::Filename)
            .unwrap_or_default();
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(&filename));
        let saved_cwd = std::env::current_dir().ok();
        if let Some(dir) = path.parent() {
            let _ = std::env::set_current_dir(dir);
        }
        let script = String::from_utf8_lossy(&filename).into_owned();
        if params.highlight {
            let mut src = b"<?php highlight_file(".to_vec();
            src.extend_from_slice(&php_single_quoted(&filename));
            src.extend_from_slice(b");");
            vm.run_script(&src, &script);
        } else {
            let src = std::fs::read(&path).unwrap_or_default();
            vm.run_script(&src, &script);
        }
        finish(vm, &ctx);
        if let Some(cwd) = saved_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
        vm.request_reset();
    });
}

/// Cleanup guard executed at the end of each request on the worker thread:
/// forgets the request's context.
struct RequestScopeGuard;

impl Drop for RequestScopeGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
    }
}

/// After `php_request_shutdown()` (which `Vm::run_script` performed: the
/// shutdown functions applied the fatal status, the output layer sent the
/// headers through [`php_header`]): the timer is disarmed
/// (`zend_unset_timeout`), the headers go out if nothing did yet, and
/// uploaded files that were not moved are deleted.
fn finish(vm: &mut Vm, ctx: &RequestCtx) {
    ctx.timer.disarm();
    apply_fatal_status(vm);
    php_header(vm);
    php_sapi_input::rfc1867::cleanup(&std::mem::take(&mut vm.rfc1867_uploaded_files));
    php_sapi_input::rfc1867::cleanup(&ctx.body.take_orphaned_uploads());
}

/// Makes `php://input` read the request body through the SAPI (the
/// streams layer's output/filename hooks are re-installed as
/// `php-ext-streams` sets them up).
fn install_input_hook() {
    use php_ext_streams::bridge::with_vm;
    php_streams::state::set_hooks(php_streams::state::Hooks {
        output: Some(Box::new(|data: &[u8]| {
            let data = data.to_vec();
            let _ = with_vm(|vm| vm.output(&data));
        })),
        executed_filename: Some(Box::new(|| {
            with_vm(|vm| vm.current_location().0.as_bytes().to_vec())
        })),
        request_body: Some(Box::new(|| {
            with_ctx(php_sapi_input::body::input).unwrap_or_default()
        })),
    });
    php_streams::state::update_settings(|s| s.sapi_is_cli = false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;

    const SEC: i64 = 1_000_000_000;

    #[test]
    fn timer_unlimited_never_expires() {
        let t = Timer::default();
        t.reset_at(0, 0, 0);
        assert_eq!(t.check(i64::MAX), Expiry::None);
        assert!(!t.timed_out());
        assert_eq!(t.limit(), 0);
    }

    #[test]
    fn timer_soft_expiry_without_hard_timeout_disarms() {
        let t = Timer::default();
        t.reset_at(10 * SEC, 2, 0);
        assert_eq!(t.check(10 * SEC + 2 * SEC - 1), Expiry::None);
        assert!(!t.timed_out());
        assert_eq!(t.check(12 * SEC), Expiry::Soft);
        assert!(t.timed_out(), "PHP_CONNECTION_TIMEOUT");
        assert_eq!(t.limit(), 2);
        // Disarmed: php-src's zend_timeout() unsets the timer.
        assert_eq!(t.check(i64::MAX), Expiry::None);
    }

    #[test]
    fn timer_hard_timeout_follows_the_soft_expiry() {
        let t = Timer::default();
        t.reset_at(0, 3, 5);
        assert_eq!(t.check(3 * SEC), Expiry::Soft);
        assert_eq!(t.hard_limit(), 5);
        // Counting hard_timeout from the soft expiry.
        assert_eq!(t.check(3 * SEC + 5 * SEC - 1), Expiry::None);
        assert_eq!(t.check(8 * SEC), Expiry::Hard);
        assert_eq!(
            t.check(i64::MAX),
            Expiry::None,
            "disarmed after the hard expiry"
        );
    }

    #[test]
    fn timer_reset_cancels_the_hard_phase() {
        let t = Timer::default();
        t.reset_at(0, 1, 5);
        assert_eq!(t.check(SEC), Expiry::Soft);
        // set_time_limit() in a shutdown function re-arms a soft timer.
        t.reset_at(SEC, 2, 5);
        assert_eq!(t.check(2 * SEC), Expiry::None);
        assert_eq!(t.check(3 * SEC), Expiry::Soft);
        t.disarm();
        assert_eq!(t.check(i64::MAX), Expiry::None);
    }

    #[test]
    fn timer_reset_uses_the_thread_clock() {
        let t = Timer::default();
        t.reset(1, 0);
        assert!(t.state().start_cpu_ns <= thread_cpu_ns());
        assert_eq!(t.check(thread_cpu_ns()), Expiry::None);
        t.reset(-5, -1);
        assert_eq!(t.limit(), 0, "negative limits are unlimited");
        assert_eq!(t.hard_limit(), 0);
    }

    #[test]
    fn ctx_output_sends_headers_once() {
        let ctx = RequestCtx::new(Box::new(MockBackend::default()), Arc::new(Timer::default()));
        assert!(!ctx.headers_sent());
        ctx.write(b"hi");
        assert!(ctx.headers_sent());
        ctx.write(b" there");
        assert!(ctx.flush());
        assert!(!ctx.aborted.get());
        assert_eq!(ctx.connection_status(), 0);
    }

    #[test]
    fn flush_sends_headers_like_php_apache_sapi_flush() {
        let ctx = RequestCtx::new(Box::new(MockBackend::default()), Arc::new(Timer::default()));
        assert!(ctx.flush());
        assert!(
            ctx.headers_sent(),
            "sapi_send_headers() precedes ap_rflush()"
        );
    }

    #[test]
    fn lost_client_marks_the_connection_aborted() {
        let m = MockBackend::default();
        *m.disconnected.borrow_mut() = true;
        let ctx = RequestCtx::new(Box::new(m), Arc::new(Timer::default()));
        ctx.write(b"hi");
        assert!(ctx.aborted.get());
        assert_eq!(ctx.connection_status(), CONNECTION_ABORTED);
        assert!(!ctx.flush());
        // Later writes are dropped without touching the server again.
        ctx.write(b"more");
        assert!(ctx.aborted.get());
    }

    #[test]
    fn connection_status_reports_timeouts() {
        let timer = Arc::new(Timer::default());
        let ctx = RequestCtx::new(Box::new(MockBackend::default()), timer.clone());
        timer.reset_at(0, 1, 0);
        assert_eq!(timer.check(SEC), Expiry::Soft);
        assert_eq!(timer.timeouts(), 1);
        assert_eq!(ctx.connection_status(), CONNECTION_TIMEOUT);
    }

    #[test]
    fn an_abort_replaces_the_status_and_a_timeout_adds_to_it() {
        // `PG(connection_status) = PHP_CONNECTION_ABORTED` drops an earlier
        // timeout; `php_on_timeout()` ORs a later one in.
        let timer = Arc::new(Timer::default());
        let ctx = RequestCtx::new(Box::new(MockBackend::default()), timer.clone());
        timer.reset_at(0, 1, 0);
        assert_eq!(timer.check(SEC), Expiry::Soft);
        ctx.record_abort();
        assert_eq!(ctx.connection_status(), CONNECTION_ABORTED);
        // set_time_limit() in a shutdown function, then a second timeout.
        timer.reset_at(SEC, 1, 0);
        assert_eq!(timer.check(2 * SEC), Expiry::Soft);
        assert_eq!(
            ctx.connection_status(),
            CONNECTION_ABORTED | CONNECTION_TIMEOUT
        );
        // Another failed flush records the abort again.
        ctx.record_abort();
        assert_eq!(ctx.connection_status(), CONNECTION_ABORTED);
    }

    #[test]
    fn head_requests_get_no_body() {
        let m = MockBackend::default().with_str(StrField::Method, b"HEAD");
        let ctx = RequestCtx::new(Box::new(m), Arc::new(Timer::default()));
        assert!(ctx.headers_only());
        let m = MockBackend::default().with_str(StrField::Method, b"GET");
        let ctx = RequestCtx::new(Box::new(m), Arc::new(Timer::default()));
        assert!(!ctx.headers_only());
    }

    #[test]
    fn ctx_reads_body() {
        let m = MockBackend::default();
        m.body_in.borrow_mut().extend_from_slice(b"abc");
        let ctx = RequestCtx::new(Box::new(m), Arc::new(Timer::default()));
        assert_eq!(php_sapi_input::body::input(&ctx), b"abc");
        assert_eq!(php_sapi_input::body::input(&ctx), b"abc");
        assert_eq!(ctx.body.read_post_bytes(), 3);
    }

    #[test]
    fn quoting() {
        assert_eq!(php_single_quoted(b"a'b\\c"), b"'a\\'b\\\\c'");
    }

    #[test]
    fn client_aborted_field_detected() {
        let m = MockBackend::default().with_int(IntField::Aborted, 1);
        let ctx = RequestCtx::new(Box::new(m), Arc::new(Timer::default()));
        assert!(ctx.aborted.get());
        assert_eq!(ctx.connection_status(), CONNECTION_ABORTED);
        assert!(!ctx.flush());
    }

    #[test]
    fn implicit_flush_flag_detected() {
        let opts = VmOptions::default();
        let mut vm = Vm::new(&opts, Box::new(ApacheOutput));
        let ctx = RequestCtx::new(Box::new(MockBackend::default()), Arc::new(Timer::default()));
        assert!(!ctx.implicit_flush.get());
        let mut sink = Vec::new();
        vm.ini.alter(
            "implicit_flush",
            b"1",
            php_support::ini::Access::SYSTEM,
            php_support::ini::Stage::Runtime,
            false,
            &mut sink,
        );
        ctx.refresh_facts(&vm);
        assert!(ctx.implicit_flush.get());
    }
}
