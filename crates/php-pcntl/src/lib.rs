// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/pcntl` (ref: `ext/pcntl/pcntl.c`, `php_signal.c`,
//! `pcntl.stub.php`) for Linux.
//!
//! [`Pcntl`] holds the per-request module globals (`PCNTL_G`): the signal
//! handler table, `last_error` and the `async_signals` flag.  It is generic
//! over the VM's callable type `C`, stored for signals whose handler is a
//! PHP callable.  Stateless helpers (`pcntl_wifexited()` & co.,
//! `pcntl_strerror()`) are free functions.
//!
//! # Signal delivery
//!
//! For a callable handler the process-level handler only records the signal
//! in a lock-free queue ([`signals`]).  The VM runs the PHP callables:
//!
//! * at tick boundaries (`declare(ticks=N)`) and in `pcntl_signal_dispatch()`;
//! * at its next interrupt check when [`async_interrupt_pending`] is true
//!   (i.e. `pcntl_async_signals(true)` and a signal is pending).
//!
//! Dispatch is a two-step protocol so the VM can call into PHP code
//! between the steps:
//!
//! ```ignore
//! if let Some(mut batch) = pcntl.begin_dispatch() {
//!     while let Some(sig) = batch.next() {
//!         if let SignalHandler::Callable(cb) = pcntl.handler(sig.signo) {
//!             let info = sig.info.entries(sig.signo); // the $siginfo array
//!             if vm.call(cb.clone(), sig.signo, info).threw() {
//!                 break; // remaining signals are re-queued
//!             }
//!         }
//!     }
//!     pcntl.end_dispatch(batch);
//! }
//! ```
//!
//! # Errors
//!
//! Failures are a [`PcntlError`].  Messages never include the `"func(): "`
//! prefix; the VM adds it, like `php_error_docref()` does.
//!
//! `pcntl_rfork()`, `pcntl_forkx()`, `pcntl_getqos_class()` and
//! `pcntl_setqos_class()` do not exist on Linux builds of PHP (the VM must
//! not register them: calling them is "Call to undefined function").

mod constants;
pub mod signals;
mod sys;

pub use constants::{constants, P_ALL, P_PGID, P_PID, P_PIDFD, SIG_DFL, SIG_IGN};
pub use signals::{async_interrupt_pending, signals_pending};

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::fmt;

/// Whether `pcntl_rfork()` exists (BSD only).
pub const HAS_RFORK: bool = false;
/// Whether `pcntl_forkx()` exists (Solaris only).
pub const HAS_FORKX: bool = false;
/// Whether the QoS-class functions exist (macOS only).
pub const HAS_QOS_CLASS: bool = false;

/// What a failing pcntl function does in PHP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcntlError {
    /// Throw `ValueError`.
    ValueError(String),
    /// Throw `TypeError`.
    TypeError(String),
    /// Throw a plain `Error`.
    Error(String),
    /// Emit `E_WARNING` and return the function's failure value (`false`,
    /// or `-1` for the fork functions).
    Warning(String),
    /// Raise a fatal `E_ERROR` (`zend_signal`'s "Error installing signal
    /// handler for %d").
    Fatal(String),
    /// Return `false` without a diagnostic.
    Failed,
}

impl fmt::Display for PcntlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueError(m)
            | Self::TypeError(m)
            | Self::Error(m)
            | Self::Warning(m)
            | Self::Fatal(m) => f.write_str(m),
            Self::Failed => f.write_str("failed"),
        }
    }
}

impl std::error::Error for PcntlError {}

/// Result alias for this crate.
pub type PcntlResult<T> = Result<T, PcntlError>;

fn value_error(msg: impl Into<String>) -> PcntlError {
    PcntlError::ValueError(msg.into())
}

fn warning(msg: impl Into<String>) -> PcntlError {
    PcntlError::Warning(msg.into())
}

/// `strerror()` text for `pcntl_strerror()` (the `zend_long` is truncated
/// to `int` as in C).
#[allow(clippy::cast_possible_truncation)]
pub fn strerror(error_code: i64) -> String {
    sys::strerror(error_code as i32)
}

/// `PCNTL_G(num_signals)`: one more than the highest valid signal (65).
pub fn num_signals() -> i64 {
    i64::from(sys::num_signals())
}

// ------------------------------------------------------------ status words

#[allow(clippy::cast_possible_truncation)]
fn int_status(status: i64) -> libc::c_int {
    status as libc::c_int
}

/// `pcntl_wifexited()`.
pub fn wifexited(status: i64) -> bool {
    libc::WIFEXITED(int_status(status))
}
/// `pcntl_wifstopped()`.
pub fn wifstopped(status: i64) -> bool {
    libc::WIFSTOPPED(int_status(status))
}
/// `pcntl_wifsignaled()`.
pub fn wifsignaled(status: i64) -> bool {
    libc::WIFSIGNALED(int_status(status))
}
/// `pcntl_wifcontinued()`.
pub fn wifcontinued(status: i64) -> bool {
    libc::WIFCONTINUED(int_status(status))
}
/// `pcntl_wexitstatus()`.
pub fn wexitstatus(status: i64) -> i64 {
    i64::from(libc::WEXITSTATUS(int_status(status)))
}
/// `pcntl_wtermsig()`.
pub fn wtermsig(status: i64) -> i64 {
    i64::from(libc::WTERMSIG(int_status(status)))
}
/// `pcntl_wstopsig()`.
pub fn wstopsig(status: i64) -> i64 {
    i64::from(libc::WSTOPSIG(int_status(status)))
}

// ---------------------------------------------------------- result shapes

/// A `$resource_usage` array.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceUsage(sys::Rusage);

impl ResourceUsage {
    /// The array keys, in PHP's order.
    pub const KEYS: [&'static str; 17] = [
        "ru_oublock",
        "ru_inblock",
        "ru_msgsnd",
        "ru_msgrcv",
        "ru_maxrss",
        "ru_ixrss",
        "ru_idrss",
        "ru_minflt",
        "ru_majflt",
        "ru_nsignals",
        "ru_nvcsw",
        "ru_nivcsw",
        "ru_nswap",
        "ru_utime.tv_usec",
        "ru_utime.tv_sec",
        "ru_stime.tv_usec",
        "ru_stime.tv_sec",
    ];

    /// `(key, value)` pairs in PHP's order.
    pub fn entries(&self) -> Vec<(&'static str, i64)> {
        Self::KEYS
            .iter()
            .copied()
            .zip(self.0 .0.iter().copied())
            .collect()
    }

    /// One field by key.
    pub fn get(&self, key: &str) -> Option<i64> {
        Self::KEYS
            .iter()
            .position(|k| *k == key)
            .map(|i| self.0 .0[i])
    }
}

/// Outcome of `pcntl_waitpid()`/`pcntl_wait()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitResult {
    /// The return value (child pid, `0` with `WNOHANG`, or `-1`).
    pub pid: i64,
    /// The value to assign to `$status` (unchanged input when the kernel
    /// did not write one).
    pub status: i64,
    /// When `$resource_usage` was passed it becomes an array: these
    /// entries if `pid > 0`, otherwise empty (`None`).
    pub resource_usage: Option<ResourceUsage>,
}

/// A value in a `$siginfo` array.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InfoValue {
    /// `int`.
    Int(i64),
    /// `float` (`utime`/`stime`).
    Float(f64),
}

/// A received `siginfo_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigInfo(sys::RawSigInfo);

impl SigInfo {
    /// `si_signo`.
    pub fn signo(&self) -> i64 {
        self.0.signo().into()
    }
    /// `si_code`.
    pub fn code(&self) -> i64 {
        self.0.code().into()
    }
    /// `si_pid`.
    pub fn pid(&self) -> i64 {
        self.0.pid().into()
    }
    /// `si_uid`.
    pub fn uid(&self) -> i64 {
        self.0.uid().into()
    }
    /// `si_status`.
    pub fn status(&self) -> i64 {
        self.0.status().into()
    }

    /// The `$siginfo` array `pcntl_siginfo_to_zval()` builds for a signal
    /// reported as `signo` (`None` — nothing assigned — when `signo <= 0`).
    #[allow(clippy::cast_precision_loss)]
    pub fn entries(&self, signo: i64) -> Option<Vec<(&'static str, InfoValue)>> {
        use InfoValue::{Float, Int};
        if signo <= 0 {
            return None;
        }
        let r = &self.0;
        let mut v = vec![
            ("signo", Int(r.signo().into())),
            ("errno", Int(r.errno().into())),
            ("code", Int(r.code().into())),
        ];
        let pid_uid = [("pid", Int(r.pid().into())), ("uid", Int(r.uid().into()))];
        match i32::try_from(signo).unwrap_or(0) {
            libc::SIGCHLD => {
                v.push(("status", Int(r.status().into())));
                v.push(("utime", Float(r.utime() as f64)));
                v.push(("stime", Float(r.stime() as f64)));
                v.extend(pid_uid);
            }
            libc::SIGUSR1 | libc::SIGUSR2 => v.extend(pid_uid),
            libc::SIGILL | libc::SIGFPE | libc::SIGSEGV | libc::SIGBUS => {
                v.push(("addr", Int(r.addr())));
            }
            libc::SIGPOLL => {
                v.push(("band", Int(r.band())));
                v.push(("fd", Int(r.fd().into())));
            }
            _ => {}
        }
        if (i64::from(sys::sigrtmin())..=i64::from(sys::sigrtmax())).contains(&signo) {
            v.extend(pid_uid);
        }
        Some(v)
    }
}

/// A signal taken from the queue by [`Pcntl::begin_dispatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSignal {
    /// The signal number.
    pub signo: i64,
    /// Its `siginfo_t`.
    pub info: SigInfo,
}

/// A batch of signals being dispatched; see [`Pcntl::begin_dispatch`].
#[derive(Debug)]
pub struct Dispatch {
    queue: VecDeque<PendingSignal>,
    mask: sys::SavedMask,
}

impl Dispatch {
    /// The next signal to run a handler for.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<PendingSignal> {
        self.queue.pop_front()
    }
}

/// A registered signal handler (an entry of `php_signal_table`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalHandler<C> {
    /// `SIG_DFL` (also what `pcntl_signal_get_handler()` reports for
    /// signals never registered).
    Default,
    /// `SIG_IGN`.
    Ignore,
    /// A PHP callable.
    Callable(C),
}

impl<C> SignalHandler<C> {
    /// The int `pcntl_signal_get_handler()` returns for the non-callable
    /// variants.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Default => Some(SIG_DFL),
            Self::Ignore => Some(SIG_IGN),
            Self::Callable(_) => None,
        }
    }
}

/// An element of a `$signals` array: an int (after `zval_try_get_long()`)
/// or a value that could not be converted, with its
/// `zend_zval_value_name()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalArg<'a> {
    /// A usable integer.
    Int(i64),
    /// A non-integer value of the named type.
    Invalid(&'a str),
}

/// An element of `pcntl_setcpuaffinity()`'s `$cpu_ids`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuArg<'a> {
    /// An int.
    Int(i64),
    /// A string (must be a canonical integer string).
    Str(&'a [u8]),
    /// Anything else, with its `zend_zval_value_name()`.
    Invalid(&'a str),
}

/// `ZEND_HANDLE_NUMERIC`: a canonical decimal integer string fitting a
/// `zend_long`.
fn handle_numeric(s: &[u8]) -> Option<i64> {
    let digits = s.strip_prefix(b"-").unwrap_or(s);
    if digits.is_empty()
        || digits.len() > 20
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits[0] == b'0' && (digits.len() > 1 || s.len() != digits.len()))
    {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// The per-request pcntl state (`PCNTL_G`).
#[derive(Debug)]
pub struct Pcntl<C> {
    handlers: BTreeMap<i64, SignalHandler<C>>,
    last_error: i32,
    async_signals: bool,
    processing: bool,
    deferred: VecDeque<PendingSignal>,
}

impl<C> Default for Pcntl<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Pcntl<C> {
    /// Fresh request state (`PHP_RINIT_FUNCTION(pcntl)`).
    pub fn new() -> Self {
        signals::set_async(false);
        Self {
            handlers: BTreeMap::new(),
            last_error: 0,
            async_signals: false,
            processing: false,
            deferred: VecDeque::new(),
        }
    }

    /// `pcntl_get_last_error()` / `pcntl_errno()`.
    pub fn last_error(&self) -> i64 {
        self.last_error.into()
    }

    fn fail(&mut self, errno: i32) {
        self.last_error = errno;
    }

    // ------------------------------------------------------------- fork

    /// `pcntl_fork()`.  `Ok(0)` in the child (the VM then runs
    /// `php_child_init()`), the child's pid in the parent.  On failure the
    /// `Warning` is emitted and PHP returns `-1`.
    ///
    /// In a multi-threaded host only async-signal-safe work is allowed in
    /// the child until it execs or exits.
    pub fn fork(&mut self) -> PcntlResult<i64> {
        // The timer thread (max_execution_time) does not survive the fork:
        // the child starts with no pending deadline and a new thread on
        // the next arm (`php_child_init()` → `zend_max_execution_timer_init()`).
        let timers = php_timer::Timers::global().prepare_fork();
        let forked = sys::fork();
        match forked {
            Ok(0) => timers.child(),
            _ => drop(timers),
        }
        match forked {
            Ok(pid) => Ok(pid.into()),
            Err(e) => {
                self.fail(e);
                Err(warning(match e {
                    libc::EAGAIN => {
                        format!("Error {e}: Reached the maximum limit of number of processes")
                    }
                    libc::ENOMEM => format!("Error {e}: Insufficient memory"),
                    libc::ENOSYS => format!("Error {e}: Unimplemented"),
                    libc::EBADF => format!("Error {e}: File descriptor concurrency issue"),
                    _ => format!("Error {e}"),
                }))
            }
        }
    }

    /// `pcntl_alarm()`.
    pub fn alarm(&mut self, seconds: i64) -> PcntlResult<i64> {
        let secs = u32::try_from(seconds).map_err(|_| {
            value_error(format!(
                "Argument #1 ($seconds) must be between 0 and {}",
                u32::MAX
            ))
        })?;
        Ok(sys::alarm(secs).into())
    }

    // ------------------------------------------------------------- wait

    fn wait_result(
        &mut self,
        r: (i32, Option<sys::Rusage>, i32),
        status: libc::c_int,
        want: bool,
    ) -> WaitResult {
        let (pid, ru, errno) = r;
        if pid < 0 {
            self.fail(errno);
        }
        WaitResult {
            pid: pid.into(),
            status: status.into(),
            resource_usage: if want && pid > 0 {
                ru.map(ResourceUsage)
            } else {
                None
            },
        }
    }

    /// `pcntl_waitpid(int $process_id, &$status, int $flags = 0,
    /// &$resource_usage = [])`; `status` is the current value of `$status`
    /// (after `zval_get_long()`), `want_rusage` whether `$resource_usage`
    /// was passed.
    #[allow(clippy::cast_possible_truncation)]
    pub fn waitpid(
        &mut self,
        process_id: i64,
        status: i64,
        flags: i64,
        want_rusage: bool,
    ) -> WaitResult {
        let mut st = int_status(status);
        let r = sys::waitpid(
            process_id as libc::pid_t,
            &mut st,
            flags as libc::c_int,
            want_rusage,
        );
        self.wait_result(r, st, want_rusage)
    }

    /// `pcntl_wait(&$status, int $flags = 0, &$resource_usage = [])`.
    #[allow(clippy::cast_possible_truncation)]
    pub fn wait(&mut self, status: i64, flags: i64, want_rusage: bool) -> WaitResult {
        let mut st = int_status(status);
        let r = sys::wait(&mut st, flags as libc::c_int, want_rusage);
        self.wait_result(r, st, want_rusage)
    }

    /// `pcntl_waitid(int $idtype = P_ALL, ?int $id = null, &$info = [],
    /// int $flags = WEXITED, &$resource_usage = [])`.
    ///
    /// On success returns the `$info` source (build it with
    /// `info.entries(SIGCHLD)`) and, when requested, the resource usage.
    /// `Err(Failed)` (`last_error` set) maps to `false`; `$resource_usage`,
    /// when passed, is then an empty array.
    #[allow(clippy::cast_possible_truncation)]
    pub fn waitid(
        &mut self,
        idtype: i64,
        id: Option<i64>,
        flags: i64,
        want_rusage: bool,
    ) -> PcntlResult<(SigInfo, Option<ResourceUsage>)> {
        match sys::waitid(idtype, id.unwrap_or(0), flags as libc::c_int, want_rusage) {
            Ok((info, ru)) => Ok((SigInfo(info), ru.map(ResourceUsage))),
            Err(e) => {
                self.fail(e);
                Err(PcntlError::Failed)
            }
        }
    }

    // ------------------------------------------------------------- exec

    /// `pcntl_exec(string $path, array $args = [], array $env_vars = [])`.
    ///
    /// The VM converts `$args` elements to strings (after checking the
    /// array is a list — see [`exec_args_not_list`]) and `$env_vars` to
    /// `(name, value)` pairs (integer keys as decimal strings), propagating
    /// conversion exceptions in element order.  Only returns on failure:
    /// a `ValueError` for NUL bytes, or the `Warning` "Error has occurred:
    /// (errno %d) %s" (PHP returns `false`).
    pub fn exec(
        &mut self,
        path: &[u8],
        args: Option<&[Vec<u8>]>,
        env_vars: Option<&[(Vec<u8>, Vec<u8>)]>,
    ) -> PcntlError {
        let nul = |m: &str| value_error(m.to_owned());
        let Ok(cpath) = CString::new(path) else {
            return nul("Argument #1 ($path) must not contain any null bytes");
        };
        let mut arg_list = vec![cpath.clone()];
        for a in args.unwrap_or_default() {
            match CString::new(a.as_slice()) {
                Ok(c) => arg_list.push(c),
                Err(_) => {
                    return nul(
                        "Argument #2 ($args) individual argument must not contain null bytes",
                    )
                }
            }
        }
        let envp = match env_vars {
            None => None,
            Some(vars) => {
                let mut out = Vec::with_capacity(vars.len());
                for (k, v) in vars {
                    if v.contains(&0) {
                        return nul("Argument #3 ($env_vars) value for environment variable must not contain null bytes");
                    }
                    if k.contains(&0) {
                        return nul("Argument #3 ($env_vars) name for environment variable must not contain null bytes");
                    }
                    let mut pair = k.clone();
                    pair.push(b'=');
                    pair.extend_from_slice(v);
                    out.push(CString::new(pair).unwrap_or_default());
                }
                Some(out)
            }
        };
        let e = sys::exec(&cpath, &arg_list, envp.as_deref());
        self.fail(e);
        warning(format!(
            "Error has occurred: (errno {e}) {}",
            sys::strerror(e)
        ))
    }

    // ---------------------------------------------------------- signals

    fn check_signo_range(signo: i64) -> PcntlResult<()> {
        let n = num_signals();
        if signo < 1 {
            return Err(value_error(
                "Argument #1 ($signal) must be greater than or equal to 1",
            ));
        }
        if signo >= n {
            return Err(value_error(format!(
                "Argument #1 ($signal) must be less than {n}"
            )));
        }
        Ok(())
    }

    /// `pcntl_signal(int $signal, callable|int $handler, ?bool
    /// $restart_syscalls = null)`.
    ///
    /// An int handler other than `SIG_DFL`/`SIG_IGN` must be rejected by
    /// the VM with [`invalid_int_handler`]; a non-callable non-int with
    /// [`Pcntl::reject_handler`].  Without an explicit `$restart_syscalls`
    /// it defaults to true, except for `SIGALRM`.
    pub fn signal(
        &mut self,
        signo: i64,
        handler: SignalHandler<C>,
        restart_syscalls: Option<bool>,
    ) -> PcntlResult<()> {
        Self::check_signo_range(signo)?;
        let restart = restart_syscalls.unwrap_or(signo != i64::from(libc::SIGALRM));
        let disposition = match handler {
            SignalHandler::Default => sys::Disposition::Default,
            SignalHandler::Ignore => sys::Disposition::Ignore,
            SignalHandler::Callable(_) => sys::Disposition::Queue { restart },
        };
        #[allow(clippy::cast_possible_truncation)]
        if let Err(e) = sys::install(signo as libc::c_int, disposition) {
            self.fail(e);
            return Err(PcntlError::Fatal(format!(
                "Error installing signal handler for {signo}"
            )));
        }
        self.handlers.insert(signo, handler);
        Ok(())
    }

    /// The `TypeError` for a handler that is neither an int nor callable;
    /// also sets `last_error = EINVAL` as PHP does.
    pub fn reject_handler(&mut self, type_name: &str) -> PcntlError {
        self.fail(libc::EINVAL);
        PcntlError::TypeError(format!(
            "Argument #2 ($handler) must be of type callable|int, {type_name} given"
        ))
    }

    /// The handler registered for `signo` ([`SignalHandler::Default`] if
    /// none), as the dispatcher sees it.
    pub fn handler(&self, signo: i64) -> &SignalHandler<C> {
        self.handlers.get(&signo).unwrap_or(&SignalHandler::Default)
    }

    /// `pcntl_signal_get_handler(int $signal)`.
    pub fn signal_get_handler(&self, signo: i64) -> PcntlResult<&SignalHandler<C>> {
        let n = num_signals();
        if signo < 1 || signo >= n {
            return Err(value_error(format!(
                "Argument #1 ($signal) must be between 1 and {}",
                n - 1
            )));
        }
        Ok(self.handler(signo))
    }

    /// Whether `begin_dispatch()` would have work (`pending_signals`).
    pub fn has_pending(&self) -> bool {
        !self.deferred.is_empty() || signals::signals_pending()
    }

    /// Starts `pcntl_signal_dispatch()`: blocks all signals, takes the
    /// queued signals and marks the queue as being processed.  `None` when
    /// nothing is pending or a dispatch is already running (re-entrancy).
    /// Must be paired with [`Pcntl::end_dispatch`].
    pub fn begin_dispatch(&mut self) -> Option<Dispatch> {
        if !self.has_pending() || self.processing {
            return None;
        }
        let mask = sys::block_all();
        let mut queue = std::mem::take(&mut self.deferred);
        queue.extend(signals::drain().into_iter().map(|(s, i)| PendingSignal {
            signo: s.into(),
            info: SigInfo(i),
        }));
        if queue.is_empty() {
            Self::settle_pending();
            sys::restore_mask(&mask);
            return None;
        }
        self.processing = true;
        Some(Dispatch { queue, mask })
    }

    /// Clears the pending flag unless a signal arrived meanwhile.
    fn settle_pending() {
        signals::clear_pending();
        if signals::any_ready() {
            signals::set_pending();
        }
    }

    /// Finishes a dispatch: signals the VM did not get to (a handler threw)
    /// are put back at the head of the queue and stay pending; the signal
    /// mask is restored.
    pub fn end_dispatch(&mut self, dispatch: Dispatch) {
        let Dispatch { queue, mask } = dispatch;
        if queue.is_empty() {
            Self::settle_pending();
        } else {
            self.deferred = queue;
            signals::set_pending();
        }
        self.processing = false;
        sys::restore_mask(&mask);
    }

    /// `pcntl_async_signals(?bool $enable = null)`: returns the previous
    /// setting and, when `enable` is given, changes it.
    pub fn async_signals(&mut self, enable: Option<bool>) -> bool {
        let old = self.async_signals;
        if let Some(on) = enable {
            self.async_signals = on;
            signals::set_async(on);
        }
        old
    }

    /// `PHP_RSHUTDOWN_FUNCTION(pcntl)`: resets every signal with a
    /// registered non-default handler to `SIG_DFL` and forgets the table
    /// and the queue.
    pub fn request_shutdown(&mut self) {
        for (&signo, h) in &self.handlers {
            if !matches!(h, SignalHandler::Default) {
                #[allow(clippy::cast_possible_truncation)]
                let _ = sys::install(signo as libc::c_int, sys::Disposition::Default);
            }
        }
        self.handlers.clear();
        self.deferred.clear();
        let _ = signals::drain();
        signals::clear_pending();
        self.async_signals = false;
        signals::set_async(false);
    }

    /// `php_pcntl_set_user_signal_infos()`.
    fn signal_set(
        arg_num: u32,
        signals: &[SignalArg<'_>],
        allow_empty: bool,
    ) -> PcntlResult<Vec<i32>> {
        let name = "signals";
        if !allow_empty && signals.is_empty() {
            return Err(value_error(format!(
                "Argument #{arg_num} (${name}) must not be empty"
            )));
        }
        let n = num_signals();
        let mut out = Vec::with_capacity(signals.len());
        for s in signals {
            match *s {
                SignalArg::Invalid(t) => {
                    return Err(PcntlError::TypeError(format!(
                        "Argument #{arg_num} (${name}) signals must be of type int, {t} given"
                    )))
                }
                SignalArg::Int(v) if v < 1 || v >= n => {
                    return Err(value_error(format!(
                        "Argument #{arg_num} (${name}) signals must be between 1 and {}",
                        n - 1
                    )))
                }
                #[allow(clippy::cast_possible_truncation)]
                SignalArg::Int(v) => out.push(v as i32),
            }
        }
        Ok(out)
    }

    /// `pcntl_sigprocmask(int $mode, array $signals, &$old_signals = null)`:
    /// returns the previously blocked signals (for `$old_signals`).
    pub fn sigprocmask(&mut self, mode: i64, signals: &[SignalArg<'_>]) -> PcntlResult<Vec<i64>> {
        let how = [libc::SIG_BLOCK, libc::SIG_UNBLOCK, libc::SIG_SETMASK]
            .into_iter()
            .find(|&h| i64::from(h) == mode)
            .ok_or_else(|| {
                value_error(
                    "Argument #1 ($mode) must be one of SIG_BLOCK, SIG_UNBLOCK, or SIG_SETMASK",
                )
            })?;
        let set = Self::signal_set(2, signals, how == libc::SIG_SETMASK)?;
        match sys::sigprocmask(how, &set) {
            Ok(old) => Ok(old.into_iter().map(i64::from).collect()),
            Err(e) => {
                self.fail(e);
                Err(warning(sys::strerror(e)))
            }
        }
    }

    fn waited(
        &mut self,
        r: Result<(i32, sys::RawSigInfo), i32>,
        silent_eagain: bool,
    ) -> PcntlResult<(i64, SigInfo)> {
        match r {
            Ok((mut signo, info)) => {
                if signo == 0 && info.signo() != 0 {
                    signo = info.signo();
                }
                Ok((signo.into(), SigInfo(info)))
            }
            Err(e) if silent_eagain && e == libc::EAGAIN => Err(PcntlError::Failed),
            Err(e) => {
                self.fail(e);
                Err(warning(sys::strerror(e)))
            }
        }
    }

    /// `pcntl_sigwaitinfo(array $signals, &$info = [])`: the signal number
    /// and the `$info` source (`info.entries(signo)`).
    pub fn sigwaitinfo(&mut self, signals: &[SignalArg<'_>]) -> PcntlResult<(i64, SigInfo)> {
        let set = Self::signal_set(1, signals, false)?;
        let r = sys::sigwaitinfo(&set);
        self.waited(r, false)
    }

    /// `pcntl_sigtimedwait(array $signals, &$info = [], int $seconds = 0,
    /// int $nanoseconds = 0)`; a timeout is `Err(Failed)`.
    pub fn sigtimedwait(
        &mut self,
        signals: &[SignalArg<'_>],
        seconds: i64,
        nanoseconds: i64,
    ) -> PcntlResult<(i64, SigInfo)> {
        let set = Self::signal_set(1, signals, false)?;
        if seconds < 0 {
            return Err(value_error(
                "Argument #3 ($seconds) must be greater than or equal to 0",
            ));
        }
        if !(0..1_000_000_000).contains(&nanoseconds) {
            return Err(value_error(
                "Argument #4 ($nanoseconds) must be between 0 and 1e9",
            ));
        }
        if seconds == 0 && nanoseconds == 0 {
            // zend_value_error() without argument context: the VM must not
            // prefix this message.
            return Err(value_error(
                "pcntl_sigtimedwait(): At least one of argument #3 ($seconds) or argument #4 ($nanoseconds) must be greater than 0",
            ));
        }
        let r = sys::sigtimedwait(&set, seconds, nanoseconds);
        self.waited(r, true)
    }

    // --------------------------------------------------------- priority

    /// `pcntl_getpriority(?int $process_id = null, int $mode = PRIO_PROCESS)`.
    pub fn getpriority(&mut self, process_id: Option<i64>, mode: i64) -> PcntlResult<i64> {
        match sys::getpriority(mode, process_id.unwrap_or(0)) {
            Ok(p) => Ok(p.into()),
            Err(e) => {
                self.fail(e);
                Err(match e {
                    libc::ESRCH => warning(format!(
                        "Error {e}: No process was located using the given parameters"
                    )),
                    libc::EINVAL => value_error(
                        "Argument #2 ($mode) must be one of PRIO_PGRP, PRIO_USER, or PRIO_PROCESS",
                    ),
                    _ => warning(format!("Unknown error {e} has occurred")),
                })
            }
        }
    }

    /// `pcntl_setpriority(int $priority, ?int $process_id = null, int $mode = PRIO_PROCESS)`.
    pub fn setpriority(
        &mut self,
        priority: i64,
        process_id: Option<i64>,
        mode: i64,
    ) -> PcntlResult<()> {
        sys::setpriority(mode, process_id.unwrap_or(0), priority).map_err(|e| {
            self.fail(e);
            match e {
                libc::ESRCH => warning(format!(
                    "Error {e}: No process was located using the given parameters"
                )),
                libc::EINVAL => value_error("Argument #3 ($mode) must be one of PRIO_PGRP, PRIO_USER, or PRIO_PROCESS"),
                libc::EPERM => warning(format!(
                    "Error {e}: A process was located, but neither its effective nor real user ID matched the effective user ID of the caller"
                )),
                libc::EACCES => warning(format!(
                    "Error {e}: Only a super user may attempt to increase the process priority"
                )),
                _ => warning(format!("Unknown error {e} has occurred")),
            }
        })
    }

    // ------------------------------------------------------- namespaces

    /// `pcntl_unshare(int $flags)`.
    pub fn unshare(&mut self, flags: i64) -> PcntlResult<()> {
        sys::unshare(flags).map_err(|e| {
            self.fail(e);
            match e {
                libc::EINVAL => value_error(
                    "Argument #1 ($flags) must be a combination of CLONE_* flags, or at least one flag is unsupported by the kernel",
                ),
                libc::ENOMEM => warning(format!("Error {e}: Insufficient memory for unshare")),
                libc::EPERM => warning(format!("Error {e}: No privilege to use these flags")),
                libc::ENOSPC => warning(format!(
                    "Error {e}: Reached the maximum nesting limit for one of the specified namespaces"
                )),
                libc::EUSERS => warning(format!(
                    "Error {e}: Reached the maximum nesting limit for the user namespace"
                )),
                _ => warning(format!("Unknown error {e} has occurred")),
            }
        })
    }

    /// `pcntl_setns(?int $process_id = null, int $nstype = CLONE_NEWNET)`.
    pub fn setns(&mut self, process_id: Option<i64>, nstype: i64) -> PcntlResult<()> {
        let pid = process_id.unwrap_or_else(|| sys::getpid().into());
        let fd = sys::pidfd_open(pid).map_err(|e| {
            self.fail(e);
            match e {
                libc::EINVAL | libc::ESRCH => value_error(format!(
                    "Argument #1 ($process_id) is not a valid process ({pid})"
                )),
                libc::ENFILE => warning(format!(
                    "Error {e}: File descriptors per-process limit reached"
                )),
                libc::ENODEV => warning(format!("Error {e}: Anonymous inode fs unsupported")),
                libc::ENOMEM => warning(format!("Error {e}: Insufficient memory for pidfd_open")),
                _ => warning(format!("Error {e}")),
            }
        })?;
        sys::setns_and_close(fd, nstype).map_err(|e| {
            self.fail(e);
            #[allow(clippy::cast_possible_truncation)]
            let as_int = nstype as i32;
            match e {
                libc::ESRCH => value_error(format!(
                    "Argument #1 ($process_id) process no longer available ({pid})"
                )),
                libc::EINVAL => value_error(format!(
                    "Argument #2 ($nstype) is an invalid nstype ({as_int})"
                )),
                libc::EPERM => warning(format!(
                    "Error {e}: No required capability for this process"
                )),
                _ => warning(format!("Error {e}")),
            }
        })
    }

    // --------------------------------------------------------- affinity

    fn affinity_error(&mut self, e: i32, pid: i64, setting: bool) -> PcntlError {
        self.fail(e);
        match e {
            libc::ESRCH => {
                value_error(format!("Argument #1 ($process_id) invalid process ({pid})"))
            }
            libc::EPERM => warning("Calling process not having the proper privileges"),
            libc::EINVAL if setting => value_error(
                "Argument #2 ($cpu_ids) invalid cpu affinity mask size or unmapped cpu id(s)",
            ),
            // zend_value_error() without argument context: not prefixed.
            libc::EINVAL => value_error("invalid cpu affinity mask size"),
            _ => warning(format!("Error {e}")),
        }
    }

    /// `pcntl_getcpuaffinity(?int $process_id = null)`.
    pub fn getcpuaffinity(&mut self, process_id: Option<i64>) -> PcntlResult<Vec<i64>> {
        let pid = process_id.unwrap_or(0);
        let maxcpus = usize::try_from(sys::nprocessors_conf()).unwrap_or(0);
        match sys::getaffinity(pid, maxcpus) {
            Ok(cpus) => Ok(cpus
                .into_iter()
                .map(|c| i64::try_from(c).unwrap_or(i64::MAX))
                .collect()),
            Err(e) => Err(self.affinity_error(e, pid, false)),
        }
    }

    /// `pcntl_setcpuaffinity(?int $process_id = null, array $cpu_ids = [])`.
    pub fn setcpuaffinity(
        &mut self,
        process_id: Option<i64>,
        cpu_ids: &[CpuArg<'_>],
    ) -> PcntlResult<()> {
        if cpu_ids.is_empty() {
            return Err(value_error("Argument #2 ($cpu_ids) must not be empty"));
        }
        let pid = process_id.unwrap_or(0);
        let maxcpus = sys::nprocessors_conf();
        let mut set = Vec::with_capacity(cpu_ids.len());
        for c in cpu_ids {
            let cpu = match *c {
                CpuArg::Int(i) => i,
                CpuArg::Str(s) => handle_numeric(s).ok_or_else(|| {
                    value_error(format!(
                        "Argument #2 ($cpu_ids) cpu id invalid value ({})",
                        String::from_utf8_lossy(s)
                    ))
                })?,
                CpuArg::Invalid(t) => {
                    return Err(PcntlError::TypeError(format!(
                        "Argument #2 ($cpu_ids) value must be of type int|string, {t} given"
                    )))
                }
            };
            if cpu < 0 || cpu >= maxcpus {
                return Err(value_error(format!(
                    "Argument #2 ($cpu_ids) cpu id must be between 0 and {} ({cpu})",
                    maxcpus - 1
                )));
            }
            set.push(usize::try_from(cpu).unwrap_or(usize::MAX));
        }
        sys::setaffinity(pid, &set).map_err(|e| self.affinity_error(e, pid, true))
    }
}

/// `pcntl_getcpu()`.
pub fn getcpu() -> i64 {
    sys::getcpu().into()
}

/// The `ValueError` for `pcntl_exec()` with a non-list `$args`.
pub fn exec_args_not_list() -> PcntlError {
    value_error("Argument #2 ($args) must be a list array")
}

/// The `ValueError` for an int `$handler` other than `SIG_DFL`/`SIG_IGN`.
pub fn invalid_int_handler() -> PcntlError {
    value_error(
        "Argument #2 ($handler) must be either SIG_DFL or SIG_IGN when an integer value is given",
    )
}

/// Maps an int `$handler` to a [`SignalHandler`] (`Err` →
/// [`invalid_int_handler`]).
pub fn int_handler<C>(value: i64) -> PcntlResult<SignalHandler<C>> {
    match value {
        SIG_DFL => Ok(SignalHandler::Default),
        SIG_IGN => Ok(SignalHandler::Ignore),
        _ => Err(invalid_int_handler()),
    }
}

#[cfg(test)]
mod tests;
