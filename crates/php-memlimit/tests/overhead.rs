// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Informational: cost of the accounting on an allocation-heavy loop
//! (pure malloc/free churn, the worst case), comparing a
//! `CountingAlloc<System>` instance against bare `System`.
//! Prints the ratio; asserts only a very loose bound so that a loaded
//! machine or a debug build cannot make it flaky. In release builds the
//! owner-thread overhead measures under 5 % (`cargo test --release
//! -p php-memlimit --test overhead -- --nocapture`).

#![expect(clippy::disallowed_methods, reason = "test code")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::time::{Duration, Instant};

use php_memlimit::CountingAlloc;

const ROUNDS: usize = 200_000;
const LIVE: usize = 64;

fn churn<A: GlobalAlloc>(a: &A) -> Duration {
    let sizes = [16usize, 24, 40, 64, 100, 256, 1024, 4000];
    let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(LIVE);
    let start = Instant::now();
    for i in 0..ROUNDS {
        let layout = Layout::from_size_align(sizes[i % sizes.len()], 8).expect("layout");
        // SAFETY: non-zero-size layout.
        let p = unsafe { a.alloc(layout) };
        assert!(!p.is_null());
        live.push((black_box(p), layout));
        if live.len() == LIVE {
            for (p, l) in live.drain(..) {
                // SAFETY: `p` was allocated by `a` with `l` just above.
                unsafe { a.dealloc(p, l) };
            }
        }
    }
    for (p, l) in live.drain(..) {
        // SAFETY: as above.
        unsafe { a.dealloc(p, l) };
    }
    start.elapsed()
}

#[test]
fn accounting_overhead_is_small() {
    let counting = CountingAlloc::new(System);
    counting.bind_to_current_thread();
    counting.force_limit(Some(1 << 30));
    // Warm up, then take the best of several runs of each.
    churn(&System);
    churn(&counting);
    let best = |f: &dyn Fn() -> Duration| (0..5).map(|_| f()).min().unwrap_or_default();
    let base = best(&|| churn(&System));
    let counted = best(&|| churn(&counting));
    let ratio = counted.as_secs_f64() / base.as_secs_f64().max(1e-9);
    println!(
        "{ROUNDS} alloc/free: System {base:?}, CountingAlloc (owner thread) {counted:?} ({:+.1} %)",
        (ratio - 1.0) * 100.0
    );
    let foreign = CountingAlloc::new(System);
    churn(&foreign);
    let shared = best(&|| churn(&foreign));
    println!(
        "{ROUNDS} alloc/free: CountingAlloc (non-owner thread, locked RMW) {shared:?} ({:+.1} %)",
        (shared.as_secs_f64() / base.as_secs_f64().max(1e-9) - 1.0) * 100.0
    );
    assert_eq!(counting.stats().current, 0);
    assert_eq!(counting.stats().allocs, 6 * ROUNDS);
    assert!(ratio < 10.0, "pathological overhead: {ratio}");
}
