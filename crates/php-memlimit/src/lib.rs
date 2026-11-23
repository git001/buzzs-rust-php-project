// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP `memory_limit` accounting for phprust.
//!
//! [`CountingAlloc`] wraps a [`GlobalAlloc`](std::alloc::GlobalAlloc) and
//! keeps live/peak byte counters; [`GLOBAL`] is the process-wide instance
//! that `php-cli` installs with [`install_global_allocator!`]. The free
//! functions in this crate operate on [`GLOBAL`].
//!
//! The model mirrors `Zend/zend_alloc.c`'s delayed OOM handling:
//!
//! * an allocation that crosses the soft limit still succeeds but records an
//!   [`OverLimit`]; the VM polls [`is_pending`] at safe points and raises
//!   `Fatal error: Allowed memory size of N bytes exhausted (tried to
//!   allocate M bytes)` with the record obtained from [`take`]; recording
//!   stays suspended until [`clear`] at request shutdown;
//! * builtins about to allocate a known large amount call [`try_reserve`]
//!   first (PHP's fallible allocations + `RETURN_OOM()`); collections
//!   that grow with script-sized input use [`TryGrow`], and loops without
//!   a safe point [`poll`]; the [`OverLimit`] these return travels with `?`
//!   to the VM, which reports the fatal;
//! * for tests, `ZEND_MM_MAX_ALLOCS` fails the n-th allocation of a
//!   request as if over the limit ([`set_max_allocs_per_request`]), and
//!   [`trap_hard_limit`] aborts with a backtrace at the hard ceiling, so a
//!   run with a small `memory_limit` names every growth that is not
//!   checked;
//! * huge allocations made inside [`uncounted`] (an extension's library
//!   allocating through its own `malloc`, like libgmp under `ext/gmp`) are
//!   left out of usage and both limits for their whole lifetime;
//! * an allocation that crosses the hard ceiling (2× the limit by default)
//!   calls the [hard-limit handler](set_hard_limit_handler) or fails, which
//!   aborts the process — the backstop against runaway memory use;
//! * a [heap observer](set_heap_observer) can be told about each
//!   allocation and free of the request thread while
//!   [observing](observe_heap) is on (`ext/zend_test`'s custom ZendMM
//!   handlers); while it is off the fast paths do no extra work.
//!
//! The memory itself comes from [`MiMalloc`] (mimalloc: a heap per
//! thread, built for many small blocks); `zend_mm` is not ported, as in
//! php-src built without the Zend allocator. Each thread of a threaded
//! server counts on its own instance ([`attach_thread`]); see
//! [`CountingAlloc`].
//!
//! See `INTEGRATION.md` in this crate for how the VM and bindings use it.

mod counting;
mod grow;
mod limit;

use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, RefCell};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, PoisonError};

pub use mimalloc::MiMalloc;

pub use counting::{
    uncounted, CountingAlloc, HardLimitHandler, HeapEvent, HeapObserver, PendingHook, Stats,
    SHUTDOWN_GRACE,
};
pub use grow::{ByteSink, CheckedBuf, TryGrow};
pub use limit::{
    clamp_to_max_warning, hard_limit_for, limit_from_ini, round_to_chunks, scale_by_percent,
    HardLimitExceeded, HardLimitPercentError, MemoryLimitIni, OverLimit, SetLimitError, CHUNK_SIZE,
    DEFAULT_LIMIT, LIMIT_MAX, PHP_BASELINE,
};

/// The process's accounting allocator over [`MiMalloc`] (mimalloc), for
/// every thread without one of its own ([`attach_thread`]): the CLI
/// SAPIs' request thread, I/O and timer threads.
///
/// It only sees allocations once installed with
/// [`install_global_allocator!`]; until then usage reads as
/// [`PHP_BASELINE`] and only [`try_reserve`] enforces the limit.
pub static GLOBAL: CountingAlloc<MiMalloc> = CountingAlloc::new(MiMalloc);

thread_local! {
    /// The calling thread's own allocator ([`attach_thread`]); `None`:
    /// [`GLOBAL`]. Constant-initialised and without a destructor, so the
    /// allocator can read it at any time, also while the thread's other
    /// thread-locals are destroyed.
    static THREAD: Cell<Option<&'static CountingAlloc<MiMalloc>>> = const { Cell::new(None) };
    /// Hands the thread's own allocator back when the thread ends.
    static DETACH: RefCell<Option<Detach>> = const { RefCell::new(None) };
}

/// Allocators of ended threads, for the next thread to attach.
static POOL: Mutex<Vec<&'static CountingAlloc<MiMalloc>>> = Mutex::new(Vec::new());

/// The allocator that accounts the calling thread's memory: its own
/// ([`attach_thread`]) or [`GLOBAL`]. Every function of this crate without
/// a receiver works on it.
#[inline]
#[must_use]
pub fn current() -> &'static CountingAlloc<MiMalloc> {
    THREAD.with(Cell::get).unwrap_or(&GLOBAL)
}

/// Gives the calling thread its own allocator, so that its usage, peak and
/// `memory_limit` are its own (php-src ZTS: `AG(mm_heap)` per thread):
/// the worker threads of a threaded server call it before their first
/// request. The hard-limit handler and the pending hook apply to every
/// thread whenever they are set ([`set_hard_limit_handler`],
/// [`set_pending_hook`]); the heap observer and tracked mode
/// ([`set_heap_observer`], [`set_tracked`]) are taken from [`GLOBAL`]
/// here, so set those first. The allocator goes back to a pool when the
/// thread ends. Idempotent.
pub fn attach_thread() {
    if THREAD.with(Cell::get).is_some() {
        return;
    }
    let pooled = POOL.lock().unwrap_or_else(PoisonError::into_inner).pop();
    let own: &'static CountingAlloc<MiMalloc> =
        pooled.unwrap_or_else(|| Box::leak(Box::new(CountingAlloc::new(MiMalloc))));
    own.inherit(&GLOBAL);
    let _ = own.bind_to_current_thread();
    THREAD.with(|t| t.set(Some(own)));
    DETACH.with(|d| *d.borrow_mut() = Some(Detach(own)));
}

/// Returns a thread's own allocator to [`POOL`] when the thread ends.
struct Detach(&'static CountingAlloc<MiMalloc>);

impl Drop for Detach {
    fn drop(&mut self) {
        // From here on the thread's frees go to `GLOBAL`.
        let _ = THREAD.try_with(|t| t.set(None));
        self.0.unbind_current_thread();
        self.0.reset_counts();
        POOL.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.0);
    }
}

/// The program's allocator: every call goes to the calling thread's
/// accounting allocator ([`current`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadAlloc;

// SAFETY: every method forwards its arguments unchanged to one
// `CountingAlloc`, whose `GlobalAlloc` impl upholds the contract. A block
// may be freed or resized through another thread's allocator than the one
// that served it: every block is a block of the one `MiMalloc` they all
// wrap, which frees it whoever asks.
unsafe impl GlobalAlloc for ThreadAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { current().alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { current().alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { current().dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged (see the impl).
        unsafe { current().realloc(ptr, layout, new_size) }
    }
}

/// Declares [`ThreadAlloc`] as the program's `#[global_allocator]`. Invoke
/// once, at item level, in the binary crate (`php-cli`'s `main.rs`).
#[macro_export]
macro_rules! install_global_allocator {
    () => {
        #[global_allocator]
        static PHP_MEMLIMIT_GLOBAL_ALLOCATOR: $crate::ThreadAlloc = $crate::ThreadAlloc;
    };
}

/// `memory_get_usage(false)` on [`current`].
pub fn usage() -> usize {
    current().usage()
}

/// `memory_get_usage(true)` on [`current`].
pub fn real_usage() -> usize {
    current().real_usage()
}

/// [`CountingAlloc::uncounted_bytes`] on [`current`].
pub fn uncounted_bytes() -> usize {
    current().uncounted_bytes()
}

/// `memory_get_usage($real_usage)` on [`current`].
pub fn memory_get_usage(real_usage: bool) -> usize {
    current().memory_usage(real_usage)
}

/// [`CountingAlloc::set_tracked`] on [`GLOBAL`] (process-wide; [`attach_thread`] passes it on).
pub fn set_tracked(tracked: bool) {
    GLOBAL.set_tracked(tracked);
}

/// Zend's allocator selection from the environment (`zend_mm_init` /
/// `alloc_globals_ctor`): tracked mode when `USE_ZEND_ALLOC` parses (like
/// `ZEND_ATOL`) to 0 and `USE_TRACKED_ALLOC` to non-zero.
pub fn tracked_from_env(use_zend_alloc: Option<&[u8]>, use_tracked_alloc: Option<&[u8]>) -> bool {
    use_zend_alloc.is_some_and(|v| atol(v) == 0) && use_tracked_alloc.is_some_and(|v| atol(v) != 0)
}

/// `ZEND_ATOL`/`strtol(s, NULL, 10)`: leading whitespace, optional sign,
/// decimal digits; anything unparsable is 0.
fn atol(s: &[u8]) -> i64 {
    let s = s.trim_ascii_start();
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for &b in digits.iter().take_while(|b| b.is_ascii_digit()) {
        n = n.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    if neg {
        -n
    } else {
        n
    }
}

/// `memory_get_peak_usage(false)` on [`current`].
pub fn peak() -> usize {
    current().peak()
}

/// `memory_get_peak_usage($real_usage)` on [`current`].
pub fn memory_get_peak_usage(real_usage: bool) -> usize {
    current().peak_usage(real_usage)
}

/// `memory_reset_peak_usage()` on [`current`].
pub fn reset_peak() {
    current().reset_peak();
}

/// [`CountingAlloc::gc_mem_caches`] on [`current`] (`gc_mem_caches()`).
pub fn gc_mem_caches() -> usize {
    current().gc_mem_caches()
}

/// [`CountingAlloc::set_observer`] on [`GLOBAL`] (process-wide; [`attach_thread`] passes it on).
pub fn set_heap_observer(observer: HeapObserver) -> bool {
    GLOBAL.set_observer(observer)
}

/// [`CountingAlloc::observe`] on [`current`].
pub fn observe_heap(on: bool) {
    current().observe(on);
}

/// [`CountingAlloc::is_observing`] on [`current`].
pub fn is_observing_heap() -> bool {
    current().is_observing()
}

/// [`CountingAlloc::bind_to_current_thread`] on [`current`]:
/// [`request_start`] binds the request thread for the request; a SAPI
/// that runs its requests on one thread (php-cli) calls this earlier, on
/// that thread (e.g. before module startup), so that startup allocations
/// count on the owner path too and the binding outlives [`request_end`]
/// (the teardown stays on the owner path). `false` while another thread
/// owns it.
pub fn bind_to_current_thread() -> bool {
    current().bind_to_current_thread()
}

/// [`CountingAlloc::unbind_current_thread`] on [`current`] ([`request_end`]
/// does it for a thread that [`request_start`] bound; an explicitly bound
/// thread calls it when it is done).
pub fn unbind_current_thread() {
    current().unbind_current_thread();
}

/// [`CountingAlloc::set_baseline`] on [`current`].
pub fn set_baseline() {
    current().set_baseline();
}

/// [`CountingAlloc::persistent`] on [`current`]: runs `f` with the bytes it
/// allocates or frees (net) left out of `memory_get_usage()`, like
/// php-src's persistent (`pemalloc`) tables. Wrap only the buffer
/// operation (a `Vec`'s growth or release) in it.
pub fn persistent<R>(f: impl FnOnce() -> R) -> R {
    current().persistent(f)
}

/// The current soft limit of [`GLOBAL`] (`None` = unlimited).
pub fn limit() -> Option<usize> {
    current().limit()
}

/// [`CountingAlloc::set_limit`] on [`current`].
pub fn set_limit(limit: Option<usize>) -> Result<(), SetLimitError> {
    current().set_limit(limit)
}

/// [`CountingAlloc::hard_limit`] on [`current`].
pub fn hard_limit() -> Option<usize> {
    current().hard_limit()
}

/// [`CountingAlloc::set_hard_limit_percent`] on [`current`].
pub fn set_hard_limit_percent(percent: i64) -> Result<(), HardLimitPercentError> {
    current().set_hard_limit_percent(percent)
}

/// [`CountingAlloc::apply_memory_limit_ini`] on [`current`].
pub fn apply_memory_limit_ini(
    value: u64,
    max_memory_limit: u64,
) -> Result<MemoryLimitIni, SetLimitError> {
    current().apply_memory_limit_ini(value, max_memory_limit)
}

/// [`CountingAlloc::check`] on [`current`].
#[inline]
pub fn check() -> Option<OverLimit> {
    current().check()
}

/// [`CountingAlloc::is_pending`] on [`current`].
#[inline]
pub fn is_pending() -> bool {
    current().is_pending()
}

/// [`CountingAlloc::take`] on [`current`].
pub fn take() -> Option<OverLimit> {
    current().take()
}

/// [`CountingAlloc::clear`] on [`current`].
pub fn clear() {
    current().clear();
}

/// [`CountingAlloc::try_reserve`] on [`current`].
pub fn try_reserve(bytes: usize) -> Result<(), OverLimit> {
    current().try_reserve(bytes)
}

/// [`CountingAlloc::try_reserve_n`] on [`current`].
pub fn try_reserve_n(count: usize, elem_size: usize, extra: usize) -> Result<(), OverLimit> {
    current().try_reserve_n(count, elem_size, extra)
}

/// The pending violation as an error, for loops that grow without a safe
/// point (`if (ZEND_HAS_OOM()) RETURN_OOM();`): `poll()?` once per
/// iteration bounds the overrun to one iteration.
///
/// # Errors
///
/// The pending [`OverLimit`].
#[inline]
pub fn poll() -> Result<(), OverLimit> {
    match check() {
        Some(over) => Err(over),
        None => Ok(()),
    }
}

/// [`CountingAlloc::refused`] on [`current`].
pub fn refused(tried: usize) -> OverLimit {
    current().refused(tried)
}

/// `ZEND_MM_MAX_ALLOCS` for every request: [`request_start`] arms
/// [`CountingAlloc::set_max_allocs`] with it (0 = off). Set once by the
/// SAPI at startup.
static MAX_ALLOCS_PER_REQUEST: AtomicU64 = AtomicU64::new(0);

/// Sets the allocation of every request that fails as if over the limit
/// (`ZEND_MM_MAX_ALLOCS`, read by `php_request_startup`; see
/// [`max_allocs_from_env`]). 0 turns it off.
pub fn set_max_allocs_per_request(n: u64) {
    MAX_ALLOCS_PER_REQUEST.store(n, Relaxed);
}

/// The `ZEND_MM_MAX_ALLOCS` value (`ZEND_ATOL`); 0 when unset, unparsable
/// or not positive.
pub fn max_allocs_from_env(value: Option<&[u8]>) -> u64 {
    value.map_or(0, |v| u64::try_from(atol(v)).unwrap_or(0))
}

/// [`CountingAlloc::max_allocs_hit`] on [`current`].
pub fn max_allocs_hit() -> bool {
    current().max_allocs_hit()
}

/// Whether the hard-ceiling test mode is on: `PHPRUST_MEMORY_TRAP` parses
/// (like `ZEND_ATOL`) to non-zero.
pub fn trap_from_env(value: Option<&[u8]>) -> bool {
    value.is_some_and(|v| atol(v) != 0)
}

/// Exit status of the [`trap_hard_limit`] test mode (128 + `SIGABRT`, but
/// a plain exit: test runs leave no core dumps behind).
pub const TRAP_EXIT_STATUS: i32 = 134;

/// The hard-ceiling handler of the test mode ([`trap_from_env`]): the
/// message and a backtrace of the allocation on stderr, then exit with
/// [`TRAP_EXIT_STATUS`]. A growth that reaches the hard ceiling was not
/// checked against the soft limit first; the backtrace names it. The
/// handler runs with the ceiling disabled, so capturing the backtrace may
/// allocate.
pub fn trap_hard_limit(e: HardLimitExceeded) -> ! {
    let trace = std::backtrace::Backtrace::force_capture();
    let _ = writeln!(
        std::io::stderr(),
        "{e}: growth not checked against memory_limit\n{trace}"
    );
    std::process::exit(TRAP_EXIT_STATUS)
}

/// The SAPI's allocator settings from the environment, once at startup
/// (before the first request): tracked mode ([`tracked_from_env`] of
/// `USE_ZEND_ALLOC`, `USE_TRACKED_ALLOC`), `ZEND_MM_MAX_ALLOCS`
/// ([`set_max_allocs_per_request`]) and the hard-ceiling handler: `normal`,
/// or [`trap_hard_limit`] under `PHPRUST_MEMORY_TRAP`.
pub fn configure_from_env(normal: HardLimitHandler) {
    let var = |name: &str| std::env::var_os(name).map(std::ffi::OsString::into_encoded_bytes);
    let trap = trap_from_env(var("PHPRUST_MEMORY_TRAP").as_deref());
    set_hard_limit_handler(if trap { trap_hard_limit } else { normal });
    set_tracked(tracked_from_env(
        var("USE_ZEND_ALLOC").as_deref(),
        var("USE_TRACKED_ALLOC").as_deref(),
    ));
    set_max_allocs_per_request(max_allocs_from_env(var("ZEND_MM_MAX_ALLOCS").as_deref()));
}

/// [`CountingAlloc::would_fit`] on [`current`].
pub fn would_fit(bytes: usize) -> bool {
    current().would_fit(bytes)
}

/// An `emalloc(bytes)` that a library would make unconditionally, checked
/// before the library does the work (bcmath's `bc_num`, hash's `pbkdf2`
/// output): a size past the hard ceiling is really allocated, so the hard
/// ceiling's handler reacts as `zend_mm_hard_limit_error` does (GH-17398);
/// below it the size is checked against `memory_limit` like any growth.
/// php-src grants such a block and raises the fatal at its next check,
/// after the doomed work; this raises it before.
///
/// # Errors
///
/// The recorded [`OverLimit`], also when the system refuses the block.
pub fn emalloc_check(bytes: usize) -> Result<(), OverLimit> {
    let beyond_hard = hard_limit().is_some_and(|hard| bytes > hard.saturating_sub(usage()));
    if beyond_hard {
        // Rust refuses capacities above `isize::MAX` without asking the
        // allocator, but any such size is far beyond every hard ceiling.
        let mut probe: Vec<u8> = Vec::new();
        if probe
            .try_reserve_exact(bytes.min(isize::MAX.unsigned_abs()))
            .is_err()
        {
            return Err(refused(bytes));
        }
        drop(std::hint::black_box(probe));
    }
    try_reserve(bytes)
}

/// [`CountingAlloc::set_hard_limit_handler`] for the whole process: on
/// [`GLOBAL`] and on every thread's own allocator, also one attached before.
pub fn set_hard_limit_handler(handler: HardLimitHandler) -> bool {
    let _ = counting::PROCESS_HARD_HANDLER.set(handler);
    GLOBAL.set_hard_limit_handler(handler)
}

/// [`CountingAlloc::set_pending_hook`] for the whole process: on [`GLOBAL`]
/// and on every thread's own allocator, also one attached before.
pub fn set_pending_hook(hook: PendingHook) -> bool {
    let _ = counting::PROCESS_PENDING_HOOK.set(hook);
    GLOBAL.set_pending_hook(hook)
}

/// [`CountingAlloc::request_start`] on [`current`], arming the
/// [`set_max_allocs_per_request`] test mode.
pub fn request_start(limit: Option<usize>) -> Result<(), SetLimitError> {
    let alloc = current();
    let r = alloc.request_start(limit);
    let max_allocs = MAX_ALLOCS_PER_REQUEST.load(Relaxed);
    if max_allocs != 0 {
        alloc.set_max_allocs(max_allocs);
    }
    r
}

/// [`CountingAlloc::request_end`] on [`current`].
pub fn request_end() {
    current().request_end();
}

#[cfg(test)]
mod tests;
