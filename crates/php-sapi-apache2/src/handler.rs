// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The content handler on the httpd side (`php_handler()`): request
//! filtering, the per-process request lock, the request thread and the
//! `max_execution_time` watchdog. The PHP error log reaches httpd's error
//! log through the SAPI logger ([`crate::functions::ApacheLog`]); stderr
//! is httpd's error log itself, as for `mod_php`.
//!
//! # The watchdog
//! php-src arms `ITIMER_PROF` and its `SIGPROF` handler flags the timeout
//! for the executor.  Here the httpd thread that waits for the request
//! thread polls that thread's CPU clock instead ([`watchdog`]); on expiry
//! it flags the timeout through [`php_vm::interrupt::sapi_timer_expired`]
//! and the executor raises the "Maximum execution time" fatal at its next
//! safe point, exactly as the signal handler would.  Only `hard_timeout`
//! (a second expiry finding the first unhandled: a native call that does
//! not return) ends the child process, as `zend_timeout_handler` does.
//!
//! # Threading
//! Each httpd worker thread maintains a persistent VM instance across requests
//! with per-request clean state restoration via `request_reset()`.
//! Requests of one process are serialised by a process-wide mutex: under
//! `mpm_prefork` (one request per process) this costs nothing; under
//! `mpm_worker`/`mpm_event` — only allowed with `PhpRustAllowThreads On` —
//! PHP requests of a child run one at a time.

use crate::backend::{Backend, IntField, LogLevel, StrField};
use crate::request::{Expiry, RunParams, Timer};
use std::cell::Cell;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// httpd's `OK`.
pub const OK: i32 = 0;
/// httpd's `DECLINED`.
pub const DECLINED: i32 = -1;

const PHP_MAGIC_TYPE: &[u8] = b"application/x-httpd-php";
const PHP_SOURCE_MAGIC_TYPE: &[u8] = b"application/x-httpd-php-source";
const PHP_SCRIPT: &[u8] = b"php-script";

/// Stack size of request threads.
#[allow(dead_code)]
const STACK_SIZE: usize = 256 << 20;

static REQUEST_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Set on request threads: a handler invocation there is a
    /// sub-request issued by `virtual()`.
    static IN_REQUEST: Cell<bool> = const { Cell::new(false) };
}

/// `child_init` hook.
pub fn child_init() {
    let _ = php_memlimit::bind_to_current_thread();
    let _ = crate::config::current();
}

/// Executes the registered header callback (`header_register_callback`) before
/// implicit header sends if not already sent.
pub fn execute_header_callback(vm: &mut php_vm::Vm) {
    if let Some(ctx) = crate::request::current() {
        if !ctx.headers_sent() {
            let cb = ctx.header_callback.borrow_mut().take();
            if let Some(cb) = cb {
                if let Err(t) = vm.call_value(&cb, Vec::new()) {
                    if vm.pending_throw.is_none() {
                        vm.pending_throw = Some(t);
                    }
                }
                drop(cb);
                if ctx.headers_sent() {
                    ctx.resend_headers();
                }
            }
        }
    }
}

/// A factory creating the request's backend on the request thread.
pub type BackendFactory = Box<dyn FnOnce() -> Option<Box<dyn Backend>> + Send>;

/// `php_handler()`.
pub fn handle(r: &dyn Backend, make: BackendFactory) -> i32 {
    let handler = r.str_field(StrField::Handler).unwrap_or_default();
    let is_php =
        handler == PHP_MAGIC_TYPE || handler == PHP_SOURCE_MAGIC_TYPE || handler == PHP_SCRIPT;
    let cfg = crate::config::current();
    let dir = r.dir_config();
    let xbithack = cfg.effective_bool("xbithack", &dir, false)
        && handler == b"text/html"
        && r.int_field(IntField::UserExec) != 0;
    if !(is_php || xbithack) {
        return DECLINED;
    }
    if r.int_field(IntField::PathInfoRejected) != 0 {
        return 404;
    }
    if !cfg.effective_bool("engine", &dir, true) {
        return DECLINED;
    }
    let filename = r.str_field(StrField::Filename).unwrap_or_default();
    match r.int_field(IntField::FileType) {
        0 => {
            log_with_file(r, "script '", &filename, "' not found or unable to stat");
            return 404;
        }
        2 => {
            log_with_file(r, "attempt to invoke directory '", &filename, "' as script");
            return 403;
        }
        _ => {}
    }
    if r.int_field(IntField::IsMain) != 0 || r.int_field(IntField::EnvDiffers) != 0 {
        r.add_cgi_vars();
    }
    let params = RunParams {
        config: cfg,
        timer: Arc::new(Timer::default()),
        highlight: handler.starts_with(PHP_SOURCE_MAGIC_TYPE),
    };
    if IN_REQUEST.with(Cell::get) {
        // virtual(): a nested request on the parent's request thread.
        return run_thread(make, params, None);
    }
    let _guard = REQUEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_thread(make, params, Some(r))
}

fn log_with_file(r: &dyn Backend, pre: &str, file: &[u8], post: &str) {
    let mut msg = pre.as_bytes().to_vec();
    msg.extend_from_slice(file);
    msg.extend_from_slice(post.as_bytes());
    r.log(LogLevel::Err, &msg);
}

/// Runs a request on its own thread with the `max_execution_time`
/// watchdog, as [`handle`] does once its checks passed; `r` receives the
/// log entries.  Public for the SAPI's own tests.
pub fn run_watched(r: &dyn Backend, make: BackendFactory, params: RunParams) -> i32 {
    run_thread(make, params, Some(r))
}

/// Runs the request on the current thread, enforcing `max_execution_time`
/// with a background watchdog thread when `watch` is given.
fn run_thread(make: BackendFactory, params: RunParams, watch: Option<&dyn Backend>) -> i32 {
    let _ = php_memlimit::bind_to_current_thread();
    let prev_in_request = IN_REQUEST.with(|c| c.replace(true));
    struct InRequestGuard(bool);
    impl Drop for InRequestGuard {
        fn drop(&mut self) {
            IN_REQUEST.with(|c| c.set(self.0));
        }
    }
    let _guard = InRequestGuard(prev_in_request);

    let timer = params.timer.clone();
    let (tx, rx) = mpsc::channel::<()>();

    let watchdog_handle = if let Some(r) = watch {
        if let Some(clock) = current_cpu_clock() {
            let timer_clone = timer.clone();
            let raw_r: (usize, usize) = unsafe { std::mem::transmute(r as *const dyn Backend) };
            std::thread::Builder::new()
                .name("php-watchdog".into())
                .spawn(move || {
                    let r: &dyn Backend = unsafe { std::mem::transmute(raw_r) };
                    let mut sink = EngineTimeout { r };
                    watchdog(&timer_clone, &rx, &mut || clock_ns(clock), &mut sink);
                })
                .ok()
        } else {
            None
        }
    } else {
        None
    };

    if let Some(backend) = make() {
        crate::request::run(backend, &params);
    }

    let _ = tx.send(());
    if let Some(h) = watchdog_handle {
        let _ = h.join();
    }
    OK
}

/// Where the watchdog reports: the engine's interrupt flags in production
/// ([`EngineTimeout`]), a recorder in tests.
pub(crate) trait TimeoutSink {
    /// `zend_timeout_handler()`: flags the timeout for the executor.
    fn expired(&mut self);
    /// Whether the flagged timeout is still unhandled (`EG(timed_out)`).
    fn pending(&self) -> bool;
    /// The hard timeout: ends the process (`_exit(124)`).
    fn kill(&mut self, secs: i64, hard: i64);
    /// Whether the client connection has aborted (`r.connection.aborted`).
    fn is_aborted(&self) -> bool {
        false
    }
    /// Requests an engine interrupt to handle the abort gracefully.
    fn abort(&mut self) {}
}

/// The production [`TimeoutSink`]: php-vm's interrupt module.
struct EngineTimeout<'a> {
    r: &'a dyn Backend,
}

impl TimeoutSink for EngineTimeout<'_> {
    fn expired(&mut self) {
        php_vm::interrupt::sapi_timer_expired();
    }

    fn pending(&self) -> bool {
        php_vm::interrupt::timeout_pending()
    }

    fn kill(&mut self, secs: i64, hard: i64) {
        hard_timeout(secs, hard);
    }

    fn is_aborted(&self) -> bool {
        self.r.int_field(IntField::Aborted) != 0
    }

    fn abort(&mut self) {
        php_vm::interrupt::request_interrupt();
    }
}

/// The watchdog loop: polls the request thread's CPU clock (`now`) every
/// 50 ms until the request ends (`done` receives, or its sender is gone).
/// A soft expiry is reported to the engine; a hard expiry that finds the
/// soft one still unhandled kills the process (after which the loop ends,
/// for a test sink that returns).
pub(crate) fn watchdog(
    timer: &Timer,
    done: &mpsc::Receiver<()>,
    now: &mut dyn FnMut() -> i64,
    sink: &mut dyn TimeoutSink,
) {
    loop {
        match done.recv_timeout(Duration::from_millis(50)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if sink.is_aborted() {
                    sink.abort();
                }
                match timer.check(now()) {
                    Expiry::None => {}
                    Expiry::Soft => sink.expired(),
                    Expiry::Hard => {
                        if sink.pending() {
                            sink.kill(timer.limit(), timer.hard_limit());
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// The CPU-time clock of the calling thread.
fn current_cpu_clock() -> Option<libc::clockid_t> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: pthread_self is valid for the current thread, &raw mut clock points to our stack variable.
    let rc = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &raw mut clock) };
    (rc == 0).then_some(clock)
}

fn clock_ns(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we own.
    unsafe {
        libc::clock_gettime(clock, &raw mut ts);
    }
    ts.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec)
}

/// `hard_timeout` expired while the executor had not reached a safe point
/// since the `max_execution_time` fatal was flagged (a native call that
/// does not return).  `zend_timeout_handler` writes its message to fd 2 —
/// Apache's error log — and ends the process with `_exit(124)`; httpd
/// starts a replacement child and the client sees the response cut off.
/// The executing file and line belong to the request thread, so they are
/// reported as `Unknown` / 0, as php-src does when nothing executes.
fn hard_timeout(secs: i64, hard: i64) -> ! {
    use std::io::Write;
    let msg = format!(
        "\nFatal error: Maximum execution time of {secs}+{hard} seconds exceeded (terminated) in Unknown on line 0\n"
    );
    let _ = std::io::stderr().write_all(msg.as_bytes());
    // SAFETY: terminating the process without running destructors is the
    // intent: other threads may hold locks and the request_rec.
    unsafe { libc::_exit(124) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;

    #[test]
    fn declines_foreign_handlers() {
        let m = MockBackend::default().with_str(StrField::Handler, b"text/plain");
        assert_eq!(handle(&m, Box::new(|| None)), DECLINED);
    }

    #[test]
    fn missing_scripts_are_404() {
        let m = MockBackend::default()
            .with_str(StrField::Handler, b"application/x-httpd-php")
            .with_str(StrField::Filename, b"/nope.php");
        assert_eq!(handle(&m, Box::new(|| None)), 404);
        assert_eq!(
            m.logs.borrow()[0].1,
            b"script '/nope.php' not found or unable to stat"
        );
        let m = MockBackend::default()
            .with_str(StrField::Handler, b"php-script")
            .with_str(StrField::Filename, b"/dir")
            .with_int(IntField::FileType, 2);
        assert_eq!(handle(&m, Box::new(|| None)), 403);
        let m = MockBackend::default()
            .with_str(StrField::Handler, b"php-script")
            .with_int(IntField::PathInfoRejected, 1);
        assert_eq!(handle(&m, Box::new(|| None)), 404);
    }

    #[test]
    fn engine_off_declines() {
        let m = MockBackend::default()
            .with_str(StrField::Handler, b"php-script")
            .with_int(IntField::FileType, 1);
        m.dir.borrow_mut().push(crate::backend::DirEntry {
            name: "engine".into(),
            value: b"0".to_vec(),
            status: crate::backend::DirStatus::PerDir,
            htaccess: true,
        });
        assert_eq!(handle(&m, Box::new(|| None)), DECLINED);
    }

    /// A [`TimeoutSink`] recording what the watchdog reported.
    #[derive(Default)]
    struct Recorder {
        /// Soft expiries reported.
        expired: usize,
        /// The executor handled the flagged timeout (cleared
        /// `EG(timed_out)`) before the next poll.
        handled: bool,
        /// The hard-timeout kill, with its `(max_execution_time,
        /// hard_timeout)`.
        killed: Option<(i64, i64)>,
        /// Abort signaled.
        aborted: bool,
    }

    impl TimeoutSink for Recorder {
        fn expired(&mut self) {
            self.expired += 1;
        }

        fn pending(&self) -> bool {
            self.expired > 0 && !self.handled
        }

        fn kill(&mut self, secs: i64, hard: i64) {
            self.killed = Some((secs, hard));
        }

        fn is_aborted(&self) -> bool {
            self.aborted
        }

        fn abort(&mut self) {}
    }

    const SEC: i64 = 1_000_000_000;

    /// Runs the watchdog over a scripted CPU clock; the request "ends"
    /// (the done channel's sender is dropped) once the clock runs out.
    fn run_watchdog(timer: &Timer, clock: &[i64], sink: &mut Recorder) {
        let (tx, rx) = mpsc::channel::<()>();
        let mut tx = Some(tx);
        let mut ticks = clock.iter();
        let mut now = || match ticks.next() {
            Some(&t) => t,
            None => {
                drop(tx.take());
                i64::MIN
            }
        };
        watchdog(timer, &rx, &mut now, sink);
    }

    #[test]
    fn watchdog_reports_the_soft_expiry_once() {
        let timer = Timer::default();
        timer.reset_at(0, 2, 0);
        let mut sink = Recorder::default();
        run_watchdog(&timer, &[SEC, 3 * SEC, 4 * SEC, 5 * SEC], &mut sink);
        assert_eq!(sink.expired, 1);
        assert_eq!(sink.killed, None, "no hard_timeout: nothing is killed");
        assert!(timer.timed_out());
    }

    #[test]
    fn watchdog_kills_on_an_unhandled_hard_timeout() {
        let timer = Timer::default();
        timer.reset_at(0, 1, 2);
        let mut sink = Recorder::default();
        // Soft at 1 s, nothing at 2 s, hard at 3 s with the soft one still
        // unhandled: the kill ends the watch before the clock runs out.
        run_watchdog(&timer, &[SEC, 2 * SEC, 3 * SEC, 9 * SEC], &mut sink);
        assert_eq!(sink.expired, 1);
        assert_eq!(sink.killed, Some((1, 2)));
    }

    #[test]
    fn watchdog_spares_a_handled_timeout() {
        let timer = Timer::default();
        timer.reset_at(0, 1, 2);
        let mut sink = Recorder {
            handled: true,
            ..Recorder::default()
        };
        run_watchdog(&timer, &[SEC, 3 * SEC, 4 * SEC], &mut sink);
        assert_eq!(sink.expired, 1);
        assert_eq!(sink.killed, None, "zend_timeout() disarmed the timer");
    }

    #[test]
    fn watchdog_ends_with_the_request() {
        let timer = Timer::default();
        timer.reset_at(0, 0, 0);
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let mut sink = Recorder::default();
        let mut now = || -> i64 { unreachable!("the request already ended") };
        watchdog(&timer, &rx, &mut now, &mut sink);
        assert_eq!(sink.expired, 0);
    }
}
