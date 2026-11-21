// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! VM interrupts (`EG(vm_interrupt)`), ticks (`declare(ticks=N)`,
//! `register_tick_function`) and `max_execution_time`.
//!
//! Anything that must run "soon" from outside the executor — a signal
//! handler (pcntl async signals), the execution-time timer, a SAPI noticing
//! a client abort — requests an interrupt; the executor polls for it after
//! every opcode and runs the registered interrupt handlers.
//!
//! # Per thread, like `EG()` under ZTS
//!
//! The flags belong to the thread a VM runs on ([`Signals`]): one thread
//! runs one VM, and a server's worker threads run theirs side by side, so
//! a timeout or an interrupt reaches only the VM it is meant for, and one
//! thread clearing its flags leaves the others' alone. Code on the VM's
//! own thread uses [`request_interrupt`]; another thread (a SAPI's
//! watchdog, the timer thread) holds a [`SignalHandle`] taken on the VM's
//! thread with [`this_thread`]. Signal handlers, which run on whatever
//! thread the kernel picks, reach the process's first VM thread
//! ([`request_interrupt_primary`]): pcntl is only loaded by the CLI SAPIs,
//! which run one VM.
//!
//! # The safe-point flag
//!
//! The executor has several kinds of work that wait for the end of the
//! current instruction: queued destructors, a due garbage collection, a
//! delayed `memory_limit` fatal and the interrupt handlers above. Instead of
//! polling each source after every instruction, whoever raises one of them
//! also sets one combined flag (`raise_safe_point`, php-src's
//! `EG(vm_interrupt)`), and the executor polls only that flag. When it is
//! set, the executor runs its `Vm::safe_point` work, which checks every
//! source in the same order as before, and then `resync_safe_point` clears
//! the flag unless a source is still set. A spuriously set flag costs one
//! pass over the sources and nothing else.
//!
//! The poll reads one process-wide count of the threads whose flag is set
//! ([`THREADS_DUE`]); only when it is not zero does it read the thread's
//! own [`Signals`], through a pointer in a constant-initialised
//! thread-local (no lazy initialisation, no destructor). An extra TLS load
//! on every instruction cost 6.7 % in a tight loop; the count costs what
//! the old process-wide flag did. A thread without a VM points at
//! [`IDLE`], whose flags nobody raises.
//!
//! The delayed `memory_limit` violation is recorded by `php-memlimit`,
//! which raises the flag through its pending hook
//! ([`install_memory_limit_hook`], once per process) on the allocating
//! thread; [`php_memlimit::is_pending`] is only consulted by the safe-point
//! work itself.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use php_runtime::Value;

use crate::vm::{Vm, VmResult};

/// One thread's interrupt state: `EG(vm_interrupt)` (the safe-point flag),
/// the interrupt request and `EG(timed_out)`.
#[derive(Debug, Default)]
pub struct Signals {
    /// The safe-point flag the executor polls (module docs).
    due: AtomicBool,
    /// An interrupt was requested: run the interrupt handlers.
    interrupt: AtomicBool,
    /// `max_execution_time` elapsed and the executor has not raised the
    /// fatal yet.
    timed_out: AtomicBool,
}

impl Signals {
    const fn new() -> Self {
        Self {
            due: AtomicBool::new(false),
            interrupt: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
        }
    }

    /// Sets the flag; the first raise since the last clear counts the
    /// thread in [`THREADS_DUE`].
    #[inline]
    fn raise(&self) {
        if !self.due.swap(true, Ordering::AcqRel) {
            THREADS_DUE.fetch_add(1, Ordering::Release);
        }
    }

    fn interrupt(&self) {
        self.interrupt.store(true, Ordering::Release);
        self.raise();
    }

    /// `zend_timeout_handler`: flags the timeout and interrupts, unless the
    /// previous timeout is still unhandled (then `false`: php-src's
    /// hard-timeout condition).
    fn time_out(&self) -> bool {
        if self.timed_out.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.interrupt();
        true
    }

    /// Clears the flag unless `sources_set()` (evaluated after the clear)
    /// reports work still waiting.
    ///
    /// The clear first, the sources after: a raiser sets its source before
    /// the flag, so either the swap reads (and synchronizes with) that store
    /// and `sources_set` sees the source, or the raiser's store comes later
    /// and the flag ends up set.
    fn resync(&self, sources_set: impl FnOnce() -> bool) {
        if self.due.swap(false, Ordering::AcqRel) {
            THREADS_DUE.fetch_sub(1, Ordering::Release);
        }
        if sources_set() {
            self.raise();
        }
    }
}

/// The flags of threads without a VM: nothing raises them, so their poll
/// stays false.
static IDLE: Signals = Signals::new();

/// How many threads' safe-point flags are set: the executor's
/// per-instruction poll reads this one process-wide value (a single
/// RIP-relative load, as long as only this crate's non-inlinable code names
/// it) and looks at its own thread's flag only when some thread has work
/// waiting. A thread clears its own flag and count only, so the count is
/// zero again as soon as every thread has run its work.
static THREADS_DUE: AtomicUsize = AtomicUsize::new(0);

/// The first thread that ran a VM, for signal handlers (leaked: it stays
/// valid for the process).
static PRIMARY: AtomicPtr<Signals> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    /// The thread's [`Signals`]: [`IDLE`] or the one [`OWNER`] keeps.
    /// Constant-initialised and without a destructor, so the executor's
    /// poll is a plain TLS load, and it stays readable while other
    /// thread-locals are destroyed.
    static CURRENT: Cell<*const Signals> = const { Cell::new(&raw const IDLE) };
    /// Keeps the thread's [`Signals`] alive; pointing [`CURRENT`] back at
    /// [`IDLE`] when the thread ends.
    static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) };
    /// How deeply this thread nests [`Vm::execute`] (see [`ExecutionGuard`]).
    static EXECUTION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// The owner of a thread's [`Signals`].
struct Owner(Arc<Signals>);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|c| c.set(&raw const IDLE));
    }
}

/// Whether `s` is [`IDLE`]: the thread runs no VM.
#[inline]
fn is_idle(s: &Signals) -> bool {
    std::ptr::eq(s, &raw const IDLE)
}

/// The thread's flags.
#[inline]
fn current() -> &'static Signals {
    // SAFETY: `CURRENT` points at `IDLE` (a static) or at the `Signals`
    // that `OWNER` keeps alive; `Owner::drop` points it back at `IDLE`
    // before releasing them. The reference does not outlive this thread's
    // use of it: callers read a flag and drop it.
    CURRENT.with(|c| unsafe { &*c.get() })
}

/// A handle to one thread's interrupt state, for other threads (a SAPI's
/// watchdog, the timer thread).
#[derive(Debug, Clone)]
pub struct SignalHandle(Arc<Signals>);

impl SignalHandle {
    /// Asks that thread's executor to run its interrupt handlers at its
    /// next safe point.
    pub fn request_interrupt(&self) {
        self.0.interrupt();
    }

    /// A timer the SAPI runs ([`Vm::timer_hook`]) expired: flags the
    /// timeout and requests an interrupt, so the executor raises the
    /// "Maximum execution time" fatal at its next safe point. Returns
    /// `false` when the previous expiry is still unhandled: php-src's
    /// hard-timeout condition, on which it kills the process.
    #[must_use]
    pub fn timer_expired(&self) -> bool {
        self.0.time_out()
    }

    /// Whether a flagged timeout still waits for the executor
    /// (`EG(timed_out)`).
    #[must_use]
    pub fn timeout_pending(&self) -> bool {
        self.0.timed_out.load(Ordering::Acquire)
    }
}

/// This thread's interrupt state, created on first use (every `Vm` takes
/// it when it is created).
#[must_use]
pub fn this_thread() -> SignalHandle {
    let signals = OWNER.with(|o| {
        let mut o = o.borrow_mut();
        let owner = o.get_or_insert_with(|| {
            let signals = Arc::new(Signals::new());
            CURRENT.with(|c| c.set(Arc::as_ptr(&signals)));
            let _ = PRIMARY.compare_exchange(
                std::ptr::null_mut(),
                Arc::into_raw(Arc::clone(&signals)).cast_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            Owner(signals)
        });
        Arc::clone(&owner.0)
    });
    SignalHandle(signals)
}

/// Whether the executor must run its safe-point work after the current
/// instruction (the per-instruction poll).
#[inline]
pub(crate) fn safe_point_due() -> bool {
    THREADS_DUE.load(Ordering::Relaxed) != 0 && current().due.load(Ordering::Relaxed)
}

/// Asks for a safe point after the current instruction: called by whoever
/// queues work for it on this thread (a destructor, a due collection, the
/// allocator's `memory_limit` hook), after setting its own source. No
/// allocation, no lock. A thread without a VM has nothing to run it; its
/// next execution checks the sources ([`ExecutionGuard`]).
#[inline(never)]
pub(crate) fn raise_safe_point() {
    let s = current();
    if !is_idle(s) {
        s.raise();
    }
}

/// `php-memlimit`'s pending hook: a `memory_limit` violation was just
/// recorded (an allocation over the soft limit, or a failed
/// `try_reserve`), possibly inside the allocator, so this only raises the
/// flag: no allocation, no panic. The safe point then `take()`s the
/// violation, and [`resync_safe_point`] keeps the flag up while one is
/// pending.
#[inline(never)]
fn memory_limit_pending() {
    raise_safe_point();
}

/// Installs [`memory_limit_pending`] as `php-memlimit`'s pending hook.
/// Idempotent: the hook is installed once per process, and later calls
/// (every `Vm::new`) find it in place.
pub(crate) fn install_memory_limit_hook() {
    let _ = php_memlimit::set_pending_hook(memory_limit_pending);
}

/// Whether some safe-point work of this thread is waiting.
fn sources_set(s: &Signals) -> bool {
    crate::object::has_pending_destructors()
        || crate::gc::wants_run()
        || s.interrupt.load(Ordering::Acquire)
        || php_memlimit::is_pending()
}

/// Ends a safe point: clears the flag unless one of its sources is still
/// set (work left behind by an error, or raised while the work ran).
#[cold]
#[inline(never)]
pub(crate) fn resync_safe_point() {
    let s = current();
    s.resync(|| sources_set(s));
}

/// Tracks how deeply this thread executes PHP code (created by
/// [`Vm::execute`]). The outermost execution flags work queued while the
/// thread was not executing.
pub(crate) struct ExecutionGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ExecutionGuard {
    /// Enters an execution on this thread.
    #[inline]
    #[must_use]
    pub(crate) fn enter() -> Self {
        let outer = EXECUTION_DEPTH.with(|d| {
            let n = d.get();
            d.set(n.saturating_add(1));
            n == 0
        });
        if outer {
            first_execution_on_thread();
        }
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for ExecutionGuard {
    #[inline]
    fn drop(&mut self) {
        EXECUTION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// This thread starts executing PHP code: work it queued while not
/// executing is flagged.
#[cold]
#[inline(never)]
fn first_execution_on_thread() {
    let s = current();
    if !is_idle(s) && sources_set(s) {
        s.raise();
    }
}

/// Asks this thread's executor to run the interrupt handlers at its next
/// safe point (`zend_atomic_bool_store(&EG(vm_interrupt), true)`).
/// No allocation, no lock: safe in code the allocator runs.
#[inline(never)]
pub fn request_interrupt() {
    let s = current();
    if !is_idle(s) {
        s.interrupt();
    }
}

/// [`request_interrupt`] for the process's first VM thread, from a signal
/// handler (async-signal-safe: two atomic stores).
#[inline(never)]
pub fn request_interrupt_primary() {
    let p = PRIMARY.load(Ordering::Acquire);
    // SAFETY: `PRIMARY` is null or a leaked `Arc<Signals>` (`this_thread`),
    // valid for the rest of the process.
    if let Some(s) = unsafe { p.as_ref() } {
        s.interrupt();
    }
}

/// Whether an interrupt is requested on this thread (seen at the next call
/// boundary).
#[inline(never)]
pub fn interrupt_requested() -> bool {
    current().interrupt.load(Ordering::Acquire)
}

/// An interrupt handler (`zend_interrupt_function`).
pub type InterruptFn = fn(&mut Vm) -> VmResult<()>;

/// A native tick handler (`php_add_tick_function`); receives N of
/// `declare(ticks=N)`.
pub type TickFn = fn(&mut Vm, i64) -> VmResult<()>;

/// A `register_tick_function()` entry.
#[derive(Clone, Debug)]
pub struct UserTick {
    pub(crate) callable: Value,
    pub(crate) args: Vec<Value>,
    pub(crate) calling: bool,
}

/// The execution-time timer of a VM: arms `seconds` (0 = none) on the
/// process's timer thread; when they pass, the VM's thread raises "Maximum
/// execution time of N seconds exceeded" at its next safe point.
///
/// `hard` is `hard_timeout`: like `zend_timeout_handler`, each expiry
/// re-arms the timer for that many seconds, so code that still runs after
/// the timeout fatal (shutdown functions, destructors) times out again.
/// php-src kills the process (exit 124) when that second expiry finds the
/// first one still unhandled; this timer counts wall-clock time rather than
/// `ITIMER_PROF` CPU time (a blocking builtin is not burning CPU), so it
/// stops instead.
fn execution_timer(signals: &SignalHandle, seconds: i64, hard: i64) -> Option<php_timer::Timer> {
    let secs = u64::try_from(seconds).ok().filter(|&s| s > 0)?;
    let hard = u64::try_from(hard).unwrap_or(0);
    let s = signals.clone();
    let timer = php_timer::Timers::global().timer(move |me| {
        if s.timer_expired() && hard > 0 {
            me.arm(Duration::from_secs(hard));
        }
    });
    timer.arm(Duration::from_secs(secs));
    Some(timer)
}

impl Vm {
    /// Runs the interrupt handlers when an interrupt was requested
    /// (`zend_interrupt_helper`). Called at the executor's safe point (once
    /// the combined flag is set) and at call boundaries.
    #[inline]
    pub(crate) fn check_interrupt(&mut self) -> VmResult<()> {
        if current().interrupt.load(Ordering::Relaxed) {
            self.handle_interrupt()?;
        }
        Ok(())
    }

    #[cold]
    fn handle_interrupt(&mut self) -> VmResult<()> {
        let s = current();
        s.interrupt.store(false, Ordering::Release);
        if s.timed_out.swap(false, Ordering::AcqRel) {
            // `zend_timeout`.
            let n = self.timeout_seconds;
            let msg = format!(
                "Maximum execution time of {n} second{} exceeded",
                if n == 1 { "" } else { "s" }
            );
            return self.error(crate::errors::level::ERROR, &msg);
        }
        for h in self.interrupt_handlers.clone() {
            h(self)?;
        }
        Ok(())
    }

    /// `ZEND_TICKS`.
    pub(crate) fn op_ticks(&mut self, n: u32) -> VmResult<()> {
        self.ticks_count += 1;
        if self.ticks_count < n {
            return Ok(());
        }
        self.ticks_count = 0;
        if self.tick_hooks.is_empty() && self.user_ticks.is_empty() {
            return Ok(());
        }
        self.fiber_switch_block += 1;
        let r = self.run_ticks(i64::from(n));
        self.fiber_switch_block -= 1;
        r
    }

    fn run_ticks(&mut self, n: i64) -> VmResult<()> {
        for h in self.tick_hooks.clone() {
            h(self, n)?;
        }
        // `run_user_tick_functions`: no re-entrant call of the same entry.
        let mut i = 0;
        while i < self.user_ticks.len() {
            if self.user_ticks[i].calling {
                i += 1;
                continue;
            }
            self.user_ticks[i].calling = true;
            let (cb, args) = (
                self.user_ticks[i].callable.clone(),
                self.user_ticks[i].args.clone(),
            );
            let r = self.call_value(&cb, args);
            if let Some(t) = self.user_ticks.get_mut(i) {
                t.calling = false;
            }
            r?;
            i += 1;
        }
        Ok(())
    }

    /// `set_time_limit()` / `max_execution_time` (`zend_set_timeout`):
    /// restarts the timer. A SAPI that runs the timer itself
    /// ([`Vm::timer_hook`]) is told the new limit instead; it reports
    /// expiry through a [`SignalHandle`] of the VM's thread.
    pub fn set_time_limit(&mut self, seconds: i64) {
        self.timeout_seconds = seconds;
        // `zend_set_timeout` resets `EG(timed_out)`; the old timer goes.
        self.signals.0.timed_out.store(false, Ordering::Release);
        self.execution_timer = None;
        if let Some(hook) = self.timer_hook {
            hook(self, seconds);
            return;
        }
        let hard = self.ini.get_long("hard_timeout");
        self.execution_timer = execution_timer(&self.signals, seconds, hard);
    }

    /// The interrupt state of this VM's thread, for other threads.
    #[must_use]
    pub fn signals(&self) -> SignalHandle {
        self.signals.clone()
    }

    /// Clears this VM's pending timeout and interrupt and stops its timer,
    /// for a new request.
    pub fn clear_interrupt(&mut self) {
        self.execution_timer = None;
        self.signals.0.timed_out.store(false, Ordering::Release);
        self.signals.0.interrupt.store(false, Ordering::Release);
    }
}

/// `register_tick_function(callable $callback, mixed ...$args): bool`.
pub(crate) fn register_tick_function(vm: &mut Vm, a: &mut crate::func::Args) -> VmResult<Value> {
    let cb = a.at(0);
    let args: Vec<Value> = a.values.iter().skip(1).map(Value::deref).collect();
    vm.user_ticks.push(UserTick {
        callable: cb,
        args,
        calling: false,
    });
    Ok(Value::Bool(true))
}

/// `unregister_tick_function(callable $callback): void`.
pub(crate) fn unregister_tick_function(vm: &mut Vm, a: &mut crate::func::Args) -> VmResult<Value> {
    let cb = a.at(0);
    let target = resolve_fcc(vm, &cb)?;
    let mut i = 0;
    while i < vm.user_ticks.len() {
        let registered = vm.user_ticks[i].callable.clone();
        let equal = match (&target, resolve_fcc(vm, &registered)?) {
            (Some(x), Some(y)) => fcc_equals(x, &y),
            _ => callables_equal(&registered, &cb),
        };
        if equal {
            if vm.user_ticks[i].calling {
                return Err(vm.make_throw(
                    "Error",
                    "Registered tick function cannot be unregistered while it is being executed",
                ));
            }
            vm.user_ticks.remove(i);
            continue;
        }
        i += 1;
    }
    Ok(Value::Null)
}

/// The resolved call of a tick callable (`zend_fcall_info_cache`), or
/// `None` when it is not callable.
fn resolve_fcc(vm: &mut Vm, v: &Value) -> VmResult<Option<crate::vm::PendingCall>> {
    Ok(vm.zend_is_callable(v, true)?.ok())
}

/// `zend_fcc_equals()` (`Zend/zend_API.h`) on resolved calls: the same
/// `$this` and called scope, and the same function: a real closure only
/// equals itself; a first-class-callable (fake) closure equals the
/// function or method it wraps (`zend_fcc_closure_equals_ex`), and
/// trampolines compare by the called method name.
fn fcc_equals(a: &crate::vm::PendingCall, b: &crate::vm::PendingCall) -> bool {
    use php_runtime::Obj;
    use std::rc::Rc;
    // `PendingCall::closure` is only set for real closures
    // (`Vm::closure_call`).
    let real_closure = |c: &crate::vm::PendingCall| c.closure.clone();
    let same_this = match (&a.this, &b.this) {
        (None, None) => true,
        (Some(x), Some(y)) => Obj::ptr_eq(x, y),
        _ => false,
    };
    let same_called = match (&a.called, &b.called) {
        (None, None) => true,
        (Some(x), Some(y)) => Rc::ptr_eq(x, y),
        _ => false,
    };
    if !same_this || !same_called {
        return false;
    }
    match (real_closure(a), real_closure(b)) {
        (Some(x), Some(y)) => Obj::ptr_eq(&x, &y),
        (None, None) => crate::func::Func::ptr_eq(&a.func, &b.func) && a.magic_name == b.magic_name,
        _ => false,
    }
}

/// `zend_fcc_equals` approximated on the callable values: function names
/// case-insensitively, `[object|class, method]` pairs, closures by identity.
fn callables_equal(a: &Value, b: &Value) -> bool {
    fn lc(v: &Value) -> Option<Vec<u8>> {
        v.as_str().map(|s| {
            let b = s.as_bytes();
            b.strip_prefix(b"\\").unwrap_or(b).to_ascii_lowercase()
        })
    }
    match (a.deref(), b.deref()) {
        (Value::Str(_), Value::Str(_)) => lc(a) == lc(b),
        (Value::Object(x), Value::Object(y)) => php_runtime::Obj::ptr_eq(&x, &y),
        (Value::Array(x), Value::Array(y)) => {
            let (Some(x0), Some(x1), Some(y0), Some(y1)) = (
                x.get(&php_runtime::ArrayKey::Int(0)),
                x.get(&php_runtime::ArrayKey::Int(1)),
                y.get(&php_runtime::ArrayKey::Int(0)),
                y.get(&php_runtime::ArrayKey::Int(1)),
            ) else {
                return false;
            };
            let same_target = match (x0.deref(), y0.deref()) {
                (Value::Object(p), Value::Object(q)) => php_runtime::Obj::ptr_eq(&p, &q),
                (Value::Str(_), Value::Str(_)) => lc(x0) == lc(y0),
                _ => false,
            };
            same_target && lc(x1) == lc(y1)
        }
        _ => false,
    }
}

/// `set_time_limit(int $seconds): bool`.
pub(crate) fn set_time_limit(vm: &mut Vm, a: &mut crate::func::Args) -> VmResult<Value> {
    let s = a.int(0);
    let mut sink = Vec::new();
    vm.ini
        .ini_set("max_execution_time", s.to_string().as_bytes(), &mut sink);
    vm.set_time_limit(s);
    Ok(Value::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::{
        current, is_idle, request_interrupt, request_interrupt_primary, this_thread,
        ExecutionGuard, Signals, EXECUTION_DEPTH, IDLE,
    };
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, PoisonError};

    /// Held by the tests that raise flags of other threads: the primary
    /// thread's may be the one another of them checks.
    static FLAGS: Mutex<()> = Mutex::new(());

    #[test]
    fn safe_point_flag_is_cleared_only_without_sources() {
        let sp = Signals::new();
        assert!(!sp.due.load(Ordering::Acquire));
        sp.raise();
        // Work still waiting keeps the flag.
        sp.resync(|| true);
        assert!(sp.due.load(Ordering::Acquire));
        // Nothing left: cleared.
        sp.resync(|| false);
        assert!(!sp.due.load(Ordering::Acquire));
        // A stale resync never sets it without a source.
        sp.resync(|| false);
        assert!(!sp.due.load(Ordering::Acquire));
    }

    #[test]
    fn safe_point_sources_are_read_after_the_clear() {
        let sp = Signals::new();
        sp.raise();
        // A source raised while the check runs (here: inside the closure,
        // i.e. after the swap) leaves the flag set.
        sp.resync(|| {
            assert!(!sp.due.load(Ordering::Acquire));
            sp.raise();
            false
        });
        assert!(sp.due.load(Ordering::Acquire));
    }

    #[test]
    fn each_thread_has_its_own_flags() {
        let _serial = FLAGS.lock().unwrap_or_else(PoisonError::into_inner);
        std::thread::spawn(|| {
            // No VM yet: nothing to interrupt.
            request_interrupt();
            assert!(is_idle(current()));
            assert!(!IDLE.interrupt.load(Ordering::Acquire));
            let mine = this_thread();
            let other = std::thread::spawn(|| {
                let theirs = this_thread();
                request_interrupt();
                assert!(current().interrupt.load(Ordering::Acquire));
                theirs
            })
            .join()
            .expect("other thread");
            assert!(other.0.interrupt.load(Ordering::Acquire));
            assert!(
                !current().interrupt.load(Ordering::Acquire),
                "not this thread's"
            );
            assert!(std::ptr::eq(current(), std::sync::Arc::as_ptr(&mine.0)));
            // A handle from another thread reaches this one.
            let handle = mine.clone();
            std::thread::spawn(move || handle.request_interrupt())
                .join()
                .expect("raiser");
            assert!(current().interrupt.load(Ordering::Acquire));
            assert!(current().due.load(Ordering::Acquire));
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn timeouts_flag_once_until_handled() {
        let s = Signals::new();
        assert!(s.time_out(), "first expiry");
        assert!(s.interrupt.load(Ordering::Acquire));
        assert!(
            !s.time_out(),
            "still unhandled: php-src's hard-timeout case"
        );
        s.timed_out.store(false, Ordering::Release);
        assert!(s.time_out(), "handled, flags again");
    }

    #[test]
    fn signal_handlers_reach_the_first_vm_thread() {
        let _serial = FLAGS.lock().unwrap_or_else(PoisonError::into_inner);
        // Some thread of the test binary registered first; whichever it is,
        // its flags get the request.
        let _ = this_thread();
        request_interrupt_primary();
        let p = super::PRIMARY.load(Ordering::Acquire);
        // SAFETY: set from a leaked `Arc` by `this_thread`.
        let s = unsafe { p.as_ref() }.expect("a primary thread");
        assert!(s.interrupt.load(Ordering::Acquire));
    }

    #[test]
    fn execution_guard_counts_nesting_per_thread() {
        // Run on a fresh thread so no other test's executions interfere.
        std::thread::spawn(|| {
            let depth = || EXECUTION_DEPTH.with(std::cell::Cell::get);
            assert_eq!(depth(), 0);
            let outer = ExecutionGuard::enter();
            assert_eq!(depth(), 1);
            let inner = ExecutionGuard::enter();
            assert_eq!(depth(), 2);
            drop(inner);
            assert_eq!(depth(), 1);
            drop(outer);
            assert_eq!(depth(), 0);
        })
        .join()
        .expect("guard thread");
    }
}
