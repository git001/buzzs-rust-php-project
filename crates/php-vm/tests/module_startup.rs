// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Extension::module_startup` (the process-wide part of `MINIT`) runs once
//! per process for each extension, and before any VM registers it, however
//! many threads start VMs at the same time; `Extension::register` runs per
//! VM. Extensions sharing a name (the parts of ext/standard) each get
//! their own start.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use php_vm::ext::{Extension, ModuleBuilder};
use php_vm::{Vm, VmOptions};

const NAME: &str = "module_startup_probe";

static STARTS: AtomicUsize = AtomicUsize::new(0);
static REGISTERS: AtomicUsize = AtomicUsize::new(0);
/// Registrations that found the process start not yet done.
static EARLY: AtomicUsize = AtomicUsize::new(0);
static SIBLING_STARTS: AtomicUsize = AtomicUsize::new(0);

/// Shares [`Counted`]'s name and comes first, without a start of its own
/// (as `StdArray` precedes `StdMisc`).
struct Plain;

impl Extension for Plain {
    fn name(&self) -> &'static str {
        NAME
    }

    fn register(&self, _m: &mut ModuleBuilder<'_>) {}
}

struct Counted;

impl Extension for Counted {
    fn name(&self) -> &'static str {
        NAME
    }

    fn register(&self, _m: &mut ModuleBuilder<'_>) {
        if STARTS.load(Ordering::SeqCst) != 1 {
            EARLY.fetch_add(1, Ordering::SeqCst);
        }
        REGISTERS.fetch_add(1, Ordering::SeqCst);
    }

    fn module_startup(&self) {
        // Long enough for the other threads to arrive while it runs.
        std::thread::sleep(std::time::Duration::from_millis(50));
        STARTS.fetch_add(1, Ordering::SeqCst);
    }
}

/// A third extension of the same name with a start of its own.
struct Sibling;

impl Extension for Sibling {
    fn name(&self) -> &'static str {
        NAME
    }

    fn register(&self, _m: &mut ModuleBuilder<'_>) {}

    fn module_startup(&self) {
        SIBLING_STARTS.fetch_add(1, Ordering::SeqCst);
    }
}

fn new_vm() -> Vm {
    Vm::with_extensions(
        &VmOptions::default(),
        Box::new(std::io::sink()),
        &[&Plain, &Counted, &Sibling],
    )
}

#[test]
fn module_startup_runs_once_per_process() {
    const THREADS: usize = 8;
    const VMS_PER_THREAD: usize = 2;
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn(move || {
                    start.wait();
                    for _ in 0..VMS_PER_THREAD {
                        drop(new_vm());
                    }
                })
                .expect("spawn a VM thread")
        })
        .collect();
    for w in workers {
        w.join().expect("VM thread");
    }
    assert_eq!(STARTS.load(Ordering::SeqCst), 1);
    assert_eq!(SIBLING_STARTS.load(Ordering::SeqCst), 1);
    assert_eq!(REGISTERS.load(Ordering::SeqCst), THREADS * VMS_PER_THREAD);
    assert_eq!(EARLY.load(Ordering::SeqCst), 0);
}
