// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Children of `proc_open()` that nobody waits for.
//!
//! A `process` resource freed without `proc_close()` (`proc_open_rsrc_dtor`)
//! looks at its child once with `WNOHANG` and lets it go: the child keeps
//! running, and when it ends it stays a zombie until PHP exits and init
//! inherits it. That is harmless for one CLI script; a server that runs
//! request after request in one process collects zombies (one pid and one
//! process table slot each) for as long as it lives. So the child is
//! adopted here instead: it is not signalled or waited for (it runs on, as
//! in php-src), and a timer of [`Timers::global`] collects it with
//! `waitpid(pid, WNOHANG)` once it has ended, checking every
//! [`INTERVAL`] while there is any to collect.
//!
//! Only the adopted pids are waited for, never `-1`: the other children
//! (those of a live `proc_open()`, `exec()`, `popen()`) keep their exit
//! status for whoever waits for them. Someone else waiting for `-1` (as
//! `pcntl_wait()` does) may collect an adopted child first; its pid is then
//! free, and [`forget`] drops it when `proc_open()` gets it for a new child,
//! so the new child is not collected in its place.
//!
//! The timer thread does not survive `fork()`; every [`adopt`] collects
//! what has ended too, so the list stays at the children still running.

use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use php_timer::{Timer, Timers};

use crate::sys;

/// How often the ended children are collected while there are any.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// The adopted children still running (as far as the last look knows).
static ORPHANS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

/// The timer that collects them: armed while [`ORPHANS`] is not empty.
static COLLECTOR: LazyLock<Timer> = LazyLock::new(|| {
    Timers::global().timer(|expired| {
        let mut orphans = orphans();
        collect(&mut orphans);
        if !orphans.is_empty() {
            expired.arm(INTERVAL);
        }
    })
});

fn orphans() -> MutexGuard<'static, Vec<libc::pid_t>> {
    // Nothing panics while holding the lock but `Vec::push` running out
    // of memory; the list is valid in any case.
    ORPHANS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits for each child of `orphans` with `WNOHANG` and drops those that
/// ended (or are not this process's children any more).
fn collect(orphans: &mut Vec<libc::pid_t>) {
    orphans.retain(|&pid| loop {
        match sys::waitpid(pid, libc::WNOHANG) {
            Ok((0, _)) => break true,
            Err(libc::EINTR) => {}
            // Collected now, or `ECHILD`: collected by someone else.
            Ok(_) | Err(_) => break false,
        }
    });
}

/// Takes over waiting for the running child `pid`, which nobody else
/// will wait for.
pub fn adopt(pid: libc::pid_t) {
    let mut orphans = orphans();
    collect(&mut orphans);
    let idle = orphans.is_empty();
    orphans.push(pid);
    if idle {
        COLLECTOR.arm(INTERVAL);
    }
}

/// `pid` belongs to a new child: an adopted child with that pid was
/// collected by someone else.
pub fn forget(pid: libc::pid_t) {
    orphans().retain(|&p| p != pid);
}

/// The adopted children not collected yet.
#[must_use]
pub fn pending() -> usize {
    orphans().len()
}
