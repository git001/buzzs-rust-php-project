// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Process-level glue between the pcntl library and the VM.
//!
//! * **Interrupts.** The library's signal handler only queues the signal.
//!   [`chain_interrupt`] wraps it so that every delivery also raises the
//!   VM interrupt flag (`EG(vm_interrupt)`); the extension's interrupt
//!   handler then dispatches when `pcntl_async_signals(true)` is on.
//! * **Exit status of forked children (fallback only).** The CLI normally
//!   runs the VM on the main thread.  When it falls back to a dedicated VM
//!   thread (hard stack limit too low), a forked child consists of that
//!   thread alone and would exit with status 0 when the thread returns;
//!   [`ForkedChild`] then terminates the child with the script's status
//!   from a thread-local destructor, after the VM has been dropped.

use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The library's handler per signal number, as installed before
/// [`chain_interrupt`] replaced it (0 = none).
static LIBRARY_HANDLERS: [AtomicUsize; 128] = [const { AtomicUsize::new(0) }; 128];

/// The `SA_SIGINFO` handler signature.
type SigInfoHandler = extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void);

/// Runs the library's queueing handler, then raises the VM interrupt.
/// Only async-signal-safe work: an atomic load, the chained call (itself
/// async-signal-safe) and an atomic store.
extern "C" fn notify_handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    let slot = usize::try_from(sig)
        .ok()
        .and_then(|i| LIBRARY_HANDLERS.get(i));
    if let Some(addr) = slot.map(|s| s.load(Ordering::Acquire)).filter(|&a| a != 0) {
        // SAFETY: `addr` was read from `sa_sigaction` of an SA_SIGINFO
        // action installed by php-pcntl, i.e. a valid `SigInfoHandler`.
        let f: SigInfoHandler = unsafe { std::mem::transmute::<usize, SigInfoHandler>(addr) };
        f(sig, info, ctx);
    }
    php_vm::interrupt::request_interrupt_primary();
}

/// Wraps the queueing handler php-pcntl just installed for `signo` so
/// deliveries also raise the VM interrupt.  No-op for `SIG_DFL`/`SIG_IGN`.
pub(crate) fn chain_interrupt(signo: i64) {
    let Ok(sig) = libc::c_int::try_from(signo) else {
        return;
    };
    let Some(slot) = usize::try_from(sig)
        .ok()
        .and_then(|i| LIBRARY_HANDLERS.get(i))
    else {
        return;
    };
    let ours = notify_handler as *const () as libc::sighandler_t;
    // SAFETY: plain sigaction(2) calls on zero-initialised structures; the
    // new action is the current one with only the handler replaced.
    unsafe {
        let mut cur: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, std::ptr::null(), &raw mut cur) != 0 {
            return;
        }
        let h = cur.sa_sigaction;
        if h == libc::SIG_DFL
            || h == libc::SIG_IGN
            || h == ours
            || cur.sa_flags & libc::SA_SIGINFO == 0
        {
            return;
        }
        slot.store(h, Ordering::Release);
        cur.sa_sigaction = ours;
        libc::sigaction(sig, &raw const cur, std::ptr::null_mut());
    }
}

/// Whether the VM runs on a thread other than the process's main thread.
pub(crate) fn on_secondary_thread() -> bool {
    // SAFETY: gettid(2)/getpid(2) have no preconditions.
    unsafe { libc::gettid() != libc::getpid() }
}

thread_local! {
    /// Present (with the exit status to use) in a forked child.
    static CHILD: ForkedChild = const { ForkedChild { status: Cell::new(None) } };
}

/// Marker living in the VM thread of a forked child (thread fallback).
pub(crate) struct ForkedChild {
    status: Cell<Option<i32>>,
}

impl ForkedChild {
    /// Called in the child right after `fork()` when the parent's VM ran
    /// on a secondary thread: arms the destructor.
    pub(crate) fn arm() {
        CHILD.with(|c| c.status.set(Some(255)));
    }

    /// Records the script's exit status (at request shutdown).
    pub(crate) fn record(status: i32) {
        CHILD.with(|c| {
            if c.status.get().is_some() {
                c.status.set(Some(status));
            }
        });
    }
}

impl Drop for ForkedChild {
    fn drop(&mut self) {
        if let Some(status) = self.status.get() {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // SAFETY: `_exit` ends the process immediately, as returning
            // from `main` would, minus the parent runtime's atexit work.
            unsafe { libc::_exit(status) };
        }
    }
}

/// Waits (up to 200 ms) until the process has a single thread.
///
/// `unshare(CLONE_NEWUSER)` and `setns()` into a user namespace fail with
/// `EINVAL` in a multi-threaded process.  The parser runs on a short-lived
/// scoped thread whose kernel task may still be tearing down right after
/// the join; PHP itself is single-threaded, so give it time to go away.
pub(crate) fn wait_single_threaded() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        let threads = std::fs::read_dir("/proc/self/task").map_or(1, Iterator::count);
        if threads <= 1 || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}
