// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Unit tests over private `CountingAlloc` instances (never `GLOBAL`, which
//! is not installed in the library's test binary).

use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr;

const M: usize = 1024 * 1024;

/// An allocator that always fails, to exercise the rollback paths.
struct FailingAlloc;

// SAFETY: never hands out memory; `dealloc`/`realloc` are unreachable for
// pointers it did not produce, and returning null is always permitted.
unsafe impl GlobalAlloc for FailingAlloc {
    unsafe fn alloc(&self, _: Layout) -> *mut u8 {
        ptr::null_mut()
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

/// Live allocation made through a test instance; freed on drop.
struct Block<'a> {
    a: &'a CountingAlloc<System>,
    p: *mut u8,
    layout: Layout,
}

impl Drop for Block<'_> {
    fn drop(&mut self) {
        if !self.p.is_null() {
            // SAFETY: `p` came from `a.alloc(layout)`/`a.realloc` with `layout`.
            unsafe { self.a.dealloc(self.p, self.layout) };
        }
    }
}

fn alloc(a: &CountingAlloc<System>, size: usize) -> Block<'_> {
    let layout = Layout::from_size_align(size, 8).expect("layout");
    // SAFETY: `layout` has non-zero size in every caller.
    let p = unsafe { a.alloc(layout) };
    Block { a, p, layout }
}

fn grow(b: &mut Block<'_>, new_size: usize) {
    // SAFETY: `b.p` is live, allocated with `b.layout`; new_size > 0.
    let p = unsafe { b.a.realloc(b.p, b.layout, new_size) };
    if !p.is_null() {
        b.p = p;
        b.layout = Layout::from_size_align(new_size, 8).expect("layout");
    }
}

#[test]
fn accounting_math() {
    let a = CountingAlloc::new(System);
    assert!(!a.is_active());
    assert_eq!(a.usage(), PHP_BASELINE);
    {
        let _b1 = alloc(&a, 1000);
        let _b2 = alloc(&a, 24);
        assert_eq!(a.stats().current, 1024);
        assert_eq!(a.usage(), PHP_BASELINE + 1024);
        assert!(a.is_active());
    }
    let s = a.stats();
    assert_eq!((s.current, s.peak, s.allocs, s.frees), (0, 1024, 2, 2));
    assert_eq!(a.peak(), PHP_BASELINE + 1024);
}

#[test]
fn realloc_accounting() {
    let a = CountingAlloc::new(System);
    let mut b = alloc(&a, 100);
    grow(&mut b, 5000);
    assert_eq!(a.stats().current, 5000);
    grow(&mut b, 10);
    assert_eq!(a.stats().current, 10);
    assert_eq!(a.stats().peak, 5000);
    assert_eq!(a.stats().reallocs, 2);
    drop(b);
    assert_eq!(a.stats().current, 0);
}

#[test]
fn inner_failure_rolls_back() {
    let a = CountingAlloc::new(FailingAlloc);
    let layout = Layout::from_size_align(64, 8).expect("layout");
    // SAFETY: non-zero-size layout.
    assert!(unsafe { a.alloc(layout) }.is_null());
    // SAFETY: as above.
    assert!(unsafe { a.alloc_zeroed(layout) }.is_null());
    let s = a.stats();
    assert_eq!((s.current, s.allocs), (0, 0));
}

#[test]
fn baseline_and_real_usage() {
    let a = CountingAlloc::new(System);
    let _vm = alloc(&a, 5 * M); // "VM structures" before the request
    assert_eq!(a.usage(), PHP_BASELINE + 5 * M);
    a.set_baseline();
    assert_eq!(a.usage(), PHP_BASELINE);
    assert_eq!(a.peak(), PHP_BASELINE);
    assert_eq!(a.real_usage(), CHUNK_SIZE);
    let b = alloc(&a, 2 * M);
    assert_eq!(a.usage(), PHP_BASELINE + 2 * M);
    assert_eq!(a.real_usage(), 2 * CHUNK_SIZE);
    assert_eq!(a.memory_usage(true), 2 * CHUNK_SIZE);
    drop(b);
    assert_eq!(a.real_usage(), CHUNK_SIZE);
    assert_eq!(a.peak_usage(true), 2 * CHUNK_SIZE);
    assert_eq!(a.peak_usage(false), PHP_BASELINE + 2 * M);
}

#[test]
fn tracked_mode_reports_exact_real_usage() {
    // token_get_all_TOKEN_PARSE_oom_feedback.phpt: under USE_TRACKED_ALLOC
    // `ini_set('memory_limit', memory_get_usage() + 1024)` succeeds and the
    // next allocations exhaust it.
    let a = CountingAlloc::new(System);
    a.set_baseline();
    assert!(!a.is_tracked());
    a.set_tracked(true);
    assert_eq!(a.real_usage(), PHP_BASELINE);
    assert_eq!(a.memory_usage(true), a.usage());
    assert_eq!(a.set_limit(Some(PHP_BASELINE + 1024)), Ok(()));
    let over = alloc(&a, 4096);
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: PHP_BASELINE + 1024,
            tried: 4096
        })
    );
    assert_eq!(a.real_peak(), PHP_BASELINE + 4096);
    drop(over);
    a.clear();
    a.set_tracked(false);
    assert_eq!(a.real_usage(), CHUNK_SIZE);
}

#[test]
fn tracked_mode_env_selection() {
    let on = |zend: Option<&str>, tracked: Option<&str>| {
        tracked_from_env(zend.map(str::as_bytes), tracked.map(str::as_bytes))
    };
    assert!(on(Some("0"), Some("1")));
    assert!(on(Some(""), Some(" 2x")));
    assert!(on(Some("off"), Some("-1")));
    assert!(!on(Some("1"), Some("1")));
    assert!(!on(Some("0"), Some("0")));
    assert!(!on(Some("0"), None));
    assert!(!on(None, Some("1")));
    assert_eq!(atol(b"  +42abc"), 42);
    assert_eq!(atol(b"-7"), -7);
    assert_eq!(atol(b"x1"), 0);
}

#[test]
fn usage_saturates_below_baseline() {
    let a = CountingAlloc::new(System);
    let b = alloc(&a, M);
    a.set_baseline();
    drop(b);
    assert_eq!(a.usage(), PHP_BASELINE - M.min(PHP_BASELINE));
    assert_eq!(a.real_usage(), CHUNK_SIZE);
}

#[test]
fn peak_reset_semantics() {
    let a = CountingAlloc::new(System);
    a.set_baseline();
    let big = alloc(&a, 3 * M);
    let p1 = a.peak();
    let small = alloc(&a, 1000);
    assert_eq!(a.peak(), p1 + 1000);
    drop(big);
    assert_eq!(a.peak(), p1 + 1000, "peak survives frees");
    a.reset_peak();
    assert_eq!(a.peak(), a.usage());
    assert!(a.peak() < p1);
    drop(small);
    assert!(a.peak() > a.usage());
}

#[test]
fn set_limit_rules() {
    let a = CountingAlloc::new(System);
    a.set_baseline();
    assert_eq!(a.limit(), None);
    assert_eq!(a.hard_limit(), None);
    // Below real usage (one chunk) fails, like `ini_set('memory_limit','1M')`.
    assert_eq!(
        a.set_limit(Some(M)),
        Err(SetLimitError {
            requested: M,
            current: CHUNK_SIZE
        })
    );
    assert_eq!(a.limit(), None);
    assert_eq!(a.set_limit(Some(3 * M)), Ok(()));
    assert_eq!(a.limit(), Some(3 * M));
    assert_eq!(a.hard_limit(), Some(6 * M));
    assert_eq!(a.set_limit(None), Ok(()));
    assert_eq!(a.limit(), None);
    assert_eq!(a.set_limit(Some(LIMIT_MAX)), Ok(()));
    assert_eq!(a.limit(), None, "LIMIT_MAX means unlimited");
    assert_eq!(a.set_limit(Some(0)).map_err(|e| e.requested), Err(0));
    a.force_limit(Some(0));
    assert_eq!(a.limit(), Some(0));
}

#[test]
fn soft_limit_is_flagged_not_failed() {
    let a = CountingAlloc::new(System);
    let _vm = alloc(&a, 3 * M);
    a.request_start(Some(4 * M)).expect("limit");
    // A large block from the request's chunks fits.
    let ok = alloc(&a, M);
    assert!(!ok.p.is_null());
    assert_eq!(a.check(), None);
    // A huge block needs its own pages: 2M of chunks + 3M > 4M.
    let over = alloc(&a, 3 * M);
    assert!(!over.p.is_null(), "soft-limit allocations succeed");
    assert!(a.is_pending());
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 3 * M
        })
    );
    // First violation wins until cleared.
    let _more = alloc(&a, 77);
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 3 * M
        })
    );
    a.clear();
    assert_eq!(a.check(), None);
    // Zend never made the failing allocation: small ones after the fatal
    // (shutdown functions) are served from the existing chunks.
    let _again = alloc(&a, 99);
    assert_eq!(a.check(), None, "the violating block is forgiven");
    // A genuinely new huge block over the limit is flagged again.
    let _big = alloc(&a, 3 * M);
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 3 * M
        })
    );
    a.clear();
    drop(over);
    let _fine = alloc(&a, 10);
    assert_eq!(a.check(), None, "back under the limit");
}

/// `zend_mm_check_limit` only runs when Zend needs memory from the OS: a
/// small block that fits in an already counted chunk never fails, even
/// close to the limit; one that needs a new chunk past the limit does.
#[test]
fn small_blocks_fail_only_when_a_new_chunk_passes_the_limit() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(4 * M)).expect("limit");
    // Fill up to 64 bytes short of 4M with blocks below the huge size.
    let mut fill = Vec::new();
    while 4 * M - a.usage() > M + 64 {
        fill.push(alloc(&a, M));
    }
    fill.push(alloc(&a, 4 * M - a.usage() - 64));
    assert_eq!(a.check(), None);
    let _tiny = alloc(&a, 64);
    assert_eq!(a.check(), None, "still inside the second chunk");
    let _next = alloc(&a, 1);
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 1
        }),
        "a third chunk would pass 4M"
    );
    drop(fill);
}

#[test]
fn take_suspends_recording_until_clear() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(4 * M)).expect("limit");
    assert_eq!(a.take(), None);
    let x = alloc(&a, 5 * M);
    assert!(a.is_pending());
    let over = a.take().expect("pending");
    assert_eq!(
        over,
        OverLimit {
            limit: 4 * M,
            tried: 5 * M
        }
    );
    assert!(!a.is_pending());
    assert!(a.is_handled());
    assert_eq!(a.check(), None);
    assert_eq!(a.take(), None, "taken once");
    let _y = alloc(&a, 100);
    assert_eq!(a.try_reserve(8 * M).map_err(|e| e.tried), Err(8 * M));
    assert!(!a.is_pending(), "nothing recorded while handled");
    a.clear();
    assert!(!a.is_handled());
    let _z = alloc(&a, 100);
    assert_eq!(a.take(), None, "the taken violation's block is forgiven");
    let _w = alloc(&a, 3 * M);
    assert_eq!(
        a.take(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 3 * M
        }),
        "recording is re-armed after clear"
    );
    drop(x);
}

#[test]
fn hard_ceiling_returns_null_without_handler() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(4 * M)).expect("limit");
    assert_eq!(a.hard_limit(), Some(8 * M));
    let before = a.stats();
    let b = alloc(&a, 9 * M);
    assert!(b.p.is_null());
    assert_eq!(a.stats().current, before.current, "rolled back");
    assert_eq!(
        a.check(),
        None,
        "hard failures do not record a soft violation"
    );
    let mut g = alloc(&a, M);
    grow(&mut g, 9 * M);
    assert_eq!(g.layout.size(), M, "growing realloc past the ceiling fails");
    assert_eq!(a.stats().current, before.current + M);
}

#[test]
fn hard_limit_percent() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(10 * M)).expect("limit");
    assert_eq!(
        a.set_hard_limit_percent(99),
        Err(HardLimitPercentError::Invalid(99))
    );
    assert_eq!(a.set_hard_limit_percent(150), Ok(()));
    assert_eq!(a.hard_limit(), Some(15 * M));
    let b = alloc(&a, 14 * M);
    assert!(!b.p.is_null());
    assert!(a.check().is_some());
    assert_eq!(
        a.set_hard_limit_percent(300),
        Err(HardLimitPercentError::OverSoftLimit(300))
    );
    drop(b);
    assert_eq!(a.set_hard_limit_percent(-1), Ok(()));
    assert_eq!(a.hard_limit(), Some(20 * M));
}

#[test]
fn try_reserve_semantics() {
    let a = CountingAlloc::new(System);
    assert_eq!(a.try_reserve(usize::MAX), Ok(()), "unlimited");
    a.request_start(Some(100 * M)).expect("limit");
    // A huge reservation fits next to the chunks already in use.
    let room = 100 * M - crate::limit::round_to_chunks(a.usage());
    assert!(a.would_fit(room));
    assert_eq!(a.try_reserve(room), Ok(()));
    assert_eq!(a.check(), None);
    let err = a.try_reserve(room + 1).expect_err("over");
    assert_eq!(
        err,
        OverLimit {
            limit: 100 * M,
            tried: room + 1
        }
    );
    assert_eq!(a.check(), Some(err), "reservation failures are recorded");
    a.clear();
    assert_eq!(
        a.try_reserve_n(usize::MAX, 2, 0),
        Err(OverLimit {
            limit: 100 * M,
            tried: usize::MAX
        })
    );
    a.clear();
    assert_eq!(a.try_reserve_n(1024, 1024, 24), Ok(()));
    assert_eq!(a.stats().current, 0, "reservations allocate nothing");
}

#[test]
fn request_lifecycle() {
    let a = CountingAlloc::new(System);
    let _vm = alloc(&a, 3 * M);
    a.request_start(Some(8 * M)).expect("limit");
    let _x = alloc(&a, 9 * M);
    assert!(a.check().is_some());
    a.request_end();
    assert_eq!(a.limit(), None);
    assert_eq!(a.check(), None);
    assert_eq!(a.usage(), PHP_BASELINE + 12 * M, "baseline cleared");
    assert_eq!(a.peak(), a.usage());
    // A failed start leaves the request unlimited.
    let err = a.request_start(Some(M)).expect_err("too small");
    assert_eq!(err.current, CHUNK_SIZE);
    assert_eq!(a.limit(), None);
    assert_eq!(a.usage(), PHP_BASELINE);
}

#[test]
fn memory_limit_ini() {
    let a = CountingAlloc::new(System);
    a.request_start(None).expect("unlimited");
    let none = u64::MAX;
    assert_eq!(
        a.apply_memory_limit_ini(128 << 20, none),
        Ok(MemoryLimitIni::Applied)
    );
    assert_eq!(a.limit(), Some(128 * M));
    assert_eq!(
        a.apply_memory_limit_ini(none, none),
        Ok(MemoryLimitIni::Applied)
    );
    assert_eq!(a.limit(), None);
    // gh17951_runtime_change_3: 1024M over max 512M warns and clamps.
    assert_eq!(
        a.apply_memory_limit_ini(1 << 30, 512 << 20),
        Ok(MemoryLimitIni::ClampedToMax {
            max: 512 << 20,
            warning: Some(clamp_to_max_warning(1 << 30, 512 << 20)),
        })
    );
    assert_eq!(a.limit(), Some(512 * M));
    // gh17951_runtime_change_4: -1 clamps silently.
    assert_eq!(
        a.apply_memory_limit_ini(none, 512 << 20),
        Ok(MemoryLimitIni::ClampedToMax {
            max: 512 << 20,
            warning: None
        })
    );
    assert_eq!(
        a.apply_memory_limit_ini(1 << 20, none),
        Err(SetLimitError {
            requested: M,
            current: CHUNK_SIZE
        })
    );
    assert_eq!(a.limit(), Some(512 * M), "failed set keeps the old limit");
}

#[test]
fn debug_and_gc() {
    let a = CountingAlloc::new(System);
    assert!(format!("{a:?}").starts_with("CountingAlloc"));
    assert_eq!(gc_mem_caches(), 0);
    assert!(!GLOBAL.is_active(), "GLOBAL is not installed in unit tests");
    assert_eq!(usage(), PHP_BASELINE);
}

#[test]
fn cross_thread_accounting_is_exact() {
    let a = CountingAlloc::new(System);
    a.bind_to_current_thread();
    let mine = alloc(&a, 1000);
    let handed_over: Vec<Block<'_>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    for i in 1..2000 {
                        drop(alloc(&a, i));
                    }
                    // Allocated here, freed by the owner thread below.
                    let b = alloc(&a, 500);
                    let b = std::mem::ManuallyDrop::new(b);
                    (b.p.expose_provenance(), b.layout)
                })
            })
            .collect();
        for i in 1..2000 {
            drop(alloc(&a, i));
        }
        workers
            .into_iter()
            .map(|w| {
                let (addr, layout) = w.join().expect("worker");
                let p = ptr::with_exposed_provenance_mut(addr);
                Block { a: &a, p, layout }
            })
            .collect()
    });
    assert_eq!(a.raw_usage(), 1000 + 4 * 500);
    drop(handed_over);
    drop(mine);
    assert_eq!(a.raw_usage(), 0);
}

#[test]
fn uncounted_scope_hides_huge_blocks() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(4 * M)).expect("limit");
    let base = a.usage();
    let small = uncounted(|| alloc(&a, 1000));
    assert_eq!(a.usage(), base + 1000, "small blocks stay counted");
    let mut big = uncounted(|| alloc(&a, 16 * M));
    assert!(
        !big.p.is_null(),
        "neither limit applies to uncounted blocks"
    );
    assert_eq!(a.usage(), base + 1000);
    assert_eq!(a.uncounted_bytes(), 16 * M);
    assert_eq!(a.check(), None, "no soft-limit violation is recorded");
    // Growing it outside the scope keeps it uncounted, past the hard
    // ceiling (2 x 4 MiB) too.
    grow(&mut big, 32 * M);
    assert_eq!(big.layout.size(), 32 * M);
    assert_eq!(a.usage(), base + 1000);
    assert_eq!(a.uncounted_bytes(), 32 * M);
    assert_eq!(a.check(), None);
    assert!(a.peak() < base + M, "uncounted blocks never reach the peak");
    // Shrinking below the huge threshold makes it an ordinary block.
    grow(&mut big, 1024);
    assert_eq!(big.layout.size(), 1024);
    assert_eq!(a.uncounted_bytes(), 0);
    assert_eq!(a.usage(), base + 1000 + 1024);
    drop(big);
    drop(small);
    assert_eq!(a.usage(), base);
    assert_eq!(a.stats().current, 0);
}

#[test]
fn uncounted_blocks_freed_outside_the_scope() {
    let a = CountingAlloc::new(System);
    let b = uncounted(|| alloc(&a, 8 * M));
    let layout = Layout::from_size_align(4 * M, 8).expect("layout");
    // SAFETY: non-zero size.
    let z = uncounted(|| unsafe { a.alloc_zeroed(layout) });
    assert!(!z.is_null());
    // SAFETY: `z` points to `4 * M` zeroed bytes.
    let (first, last) = unsafe { (*z, *z.add(4 * M - 1)) };
    assert_eq!((first, last), (0, 0));
    assert_eq!(a.stats().current, 0);
    assert_eq!(a.uncounted_bytes(), 12 * M);
    assert_eq!(a.stats().allocs, 2);
    drop(b);
    // SAFETY: `z` came from `a.alloc_zeroed(layout)`.
    unsafe { a.dealloc(z, layout) };
    assert_eq!(a.stats().current, 0, "frees do not underflow the counters");
    assert_eq!(a.uncounted_bytes(), 0);
    assert_eq!(a.stats().frees, 2);
}

#[test]
fn huge_blocks_outside_the_scope_stay_counted() {
    let a = CountingAlloc::new(System);
    let mut b = alloc(&a, 8 * M);
    assert_eq!(a.stats().current, 8 * M);
    grow(&mut b, 16 * M);
    assert_eq!(a.stats().current, 16 * M);
    assert_eq!(a.uncounted_bytes(), 0);
    drop(b);
    assert_eq!(a.stats().current, 0);
}

#[test]
fn counted_block_grown_huge_in_scope_leaves_the_accounting() {
    let a = CountingAlloc::new(System);
    let mut b = alloc(&a, M);
    assert_eq!(a.stats().current, M);
    uncounted(|| grow(&mut b, 8 * M));
    assert_eq!(b.layout.size(), 8 * M);
    assert_eq!(a.stats().current, 0);
    assert_eq!(a.uncounted_bytes(), 8 * M);
    drop(b);
    assert_eq!(a.stats().current, 0);
    assert_eq!(a.uncounted_bytes(), 0);
}

#[test]
fn full_uncounted_registry_falls_back_to_counting() {
    let a = CountingAlloc::new(System);
    let size = 3 * M;
    let blocks: Vec<Block<'_>> = (0..64).map(|_| uncounted(|| alloc(&a, size))).collect();
    assert!(blocks.iter().all(|b| !b.p.is_null()));
    assert_eq!(a.uncounted_bytes(), 64 * size);
    assert_eq!(a.stats().current, 0);
    let extra = uncounted(|| alloc(&a, size));
    assert_eq!(a.stats().current, size, "the 65th huge block is counted");
    drop(blocks);
    assert_eq!(a.uncounted_bytes(), 0);
    drop(extra);
    assert_eq!(a.stats().current, 0);
}

thread_local! {
    /// Events heard by [`record_event`] on this thread.
    static EVENTS: std::cell::RefCell<Vec<HeapEvent>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A heap observer that records what it hears (in the test binary's own
/// allocator, not the instance under test).
fn record_event(e: HeapEvent) {
    EVENTS.with(|v| v.borrow_mut().push(e));
}

fn take_events() -> Vec<HeapEvent> {
    EVENTS.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

#[test]
fn heap_observer_hears_the_observed_thread_only() {
    let a = CountingAlloc::new(System);
    a.bind_to_current_thread();
    a.observe(true);
    assert!(
        !a.is_observing(),
        "nothing to report to without an observer"
    );
    assert!(a.set_observer(record_event));
    assert!(!a.set_observer(record_event), "installed once");
    let quiet = alloc(&a, 64);
    a.observe(true);
    assert!(a.is_observing());
    let mut b = alloc(&a, 100);
    let first = b.p.addr();
    grow(&mut b, 200);
    let grown = b.p.addr();
    assert_eq!(a.gc_mem_caches(), 0);
    drop(b);
    std::thread::scope(|s| {
        s.spawn(|| drop(alloc(&a, 10)));
    });
    a.observe(false);
    assert!(!a.is_observing());
    drop(quiet);
    drop(alloc(&a, 32));
    assert_eq!(
        take_events(),
        vec![
            HeapEvent::Alloc {
                size: 100,
                addr: first
            },
            HeapEvent::Realloc {
                size: 200,
                old: first,
                new: grown
            },
            HeapEvent::Gc { freed: 0 },
            HeapEvent::Free { addr: grown },
        ]
    );
    let s = a.stats();
    assert_eq!((s.current, s.allocs, s.frees, s.reallocs), (0, 4, 4, 1));
}

#[test]
fn observing_keeps_the_limit_semantics() {
    let a = CountingAlloc::new(System);
    a.bind_to_current_thread();
    assert!(a.set_observer(record_event));
    a.request_start(Some(128 * M)).expect("limit");
    a.observe(true);
    let small = alloc(&a, 1000);
    assert!(!small.p.is_null());
    assert!(a.check().is_none(), "an allocation under the limit is fine");
    let over = alloc(&a, 200 * M);
    assert!(!over.p.is_null(), "a soft overrun is still granted");
    assert!(a.check().is_some(), "and recorded");
    a.observe(false);
    drop(over);
    drop(small);
    let heard = take_events();
    assert_eq!(heard.len(), 2, "{heard:?}");
    assert!(matches!(heard[0], HeapEvent::Alloc { size: 1000, .. }));
    assert!(matches!(heard[1], HeapEvent::Alloc { size, .. } if size == 200 * M));
    a.request_end();
    assert_eq!(a.stats().current, 0);
}

// ---- the owner thread -----------------------------------------------------

#[test]
fn ownership_is_exclusive_until_released() {
    let a = CountingAlloc::new(System);
    assert!(a.bind_to_current_thread());
    assert!(a.bind_to_current_thread(), "rebinding the owner is a no-op");
    std::thread::scope(|s| {
        s.spawn(|| {
            assert!(!a.bind_to_current_thread(), "owned by another thread");
            assert!(!a.is_bound_to_current_thread());
            drop(alloc(&a, 24));
        });
    });
    let mine = alloc(&a, 24);
    a.unbind_current_thread();
    assert!(!a.is_bound_to_current_thread());
    // Freed after unbinding: on the shared counter.
    drop(mine);
    std::thread::scope(|s| {
        s.spawn(|| {
            assert!(a.bind_to_current_thread(), "free again");
            drop(alloc(&a, 24));
            a.unbind_current_thread();
        });
    });
    // `request_start` binds and `request_end` unbinds.
    a.request_start(None).expect("unlimited");
    assert!(a.is_bound_to_current_thread());
    a.request_end();
    assert!(!a.is_bound_to_current_thread());
    assert_eq!(a.stats().current, 0);
}

#[test]
fn explicit_binding_survives_request_end() {
    // php-cli binds before the request: the teardown that follows
    // `request_end` (TLS destructors, the Vm's drop) stays on the owner
    // path.
    let a = CountingAlloc::new(System);
    assert!(a.bind_to_current_thread());
    a.request_start(Some(8 * M)).expect("limit");
    assert!(a.is_bound_to_current_thread());
    let b = alloc(&a, 24);
    a.request_end();
    assert!(a.is_bound_to_current_thread(), "bound explicitly: kept");
    let c = alloc(&a, 24);
    drop(b);
    drop(c);
    a.unbind_current_thread();
    assert!(!a.is_bound_to_current_thread());
    // A thread bound by `request_start` itself (a php -S request thread) is
    // unbound by `request_end`, also after a second `request_start` on it.
    a.request_start(None).expect("unlimited");
    assert!(a.is_bound_to_current_thread());
    a.request_start(None).expect("unlimited");
    a.request_end();
    assert!(!a.is_bound_to_current_thread());
    // Binding explicitly during a request pins the ownership too.
    a.request_start(None).expect("unlimited");
    assert!(a.bind_to_current_thread());
    a.request_end();
    assert!(a.is_bound_to_current_thread());
    a.unbind_current_thread();
    assert!(!a.is_bound_to_current_thread());
    // Another thread's `request_end` never touches the owner's binding.
    assert!(a.bind_to_current_thread());
    std::thread::scope(|s| {
        s.spawn(|| {
            assert!(a.request_start(None).is_ok(), "counts on the shared path");
            assert!(!a.is_bound_to_current_thread());
            a.request_end();
        });
    });
    assert!(a.is_bound_to_current_thread());
    a.unbind_current_thread();
    assert_eq!(a.stats().current, 0);
}

#[test]
fn unbinding_while_observed_releases_the_ownership() {
    // Observing parks the owner's ownership (`observe(true)` takes it, so
    // the observed thread's frees are reported on the non-owner path) and
    // gives it back when observing stops. A request-bound thread whose
    // request ends in between must not get it back.
    let a = CountingAlloc::new(System);
    assert!(a.set_observer(record_event));
    a.request_start(None).expect("unlimited");
    assert!(a.is_bound_to_current_thread());
    a.observe(true);
    assert!(a.is_observing());
    assert!(!a.is_bound_to_current_thread());
    a.request_end();
    a.observe(false);
    assert!(
        !a.is_bound_to_current_thread(),
        "the give-back was cancelled: free for the next request"
    );
    std::thread::scope(|s| {
        s.spawn(|| {
            assert!(a.bind_to_current_thread(), "free");
            a.unbind_current_thread();
        });
    });
    // An explicitly bound thread keeps the ownership across an observed
    // request.
    assert!(a.bind_to_current_thread());
    a.request_start(None).expect("unlimited");
    a.observe(true);
    a.request_end();
    a.observe(false);
    assert!(
        a.is_bound_to_current_thread(),
        "bound explicitly: given back"
    );
    a.unbind_current_thread();
    assert!(!a.is_bound_to_current_thread());
    let _ = take_events();
}

#[test]
fn owner_fast_path_accounts_like_the_general_one() {
    // Blocks of the owner up to `MAX_LARGE_SIZE` take the call-free fast
    // path; its figures must be the ones the general path produces.
    let a = CountingAlloc::new(System);
    assert!(a.bind_to_current_thread());
    let x = alloc(&a, 24);
    let y = alloc(&a, 1000);
    let s = a.stats();
    assert_eq!((s.current, s.peak, s.allocs, s.frees), (1024, 1024, 2, 0));
    drop(y);
    let s = a.stats();
    assert_eq!((s.current, s.peak, s.frees), (24, 1024, 1));
    let z = alloc(&a, 500);
    assert_eq!(a.stats().peak, 1024, "below the peak: unchanged");
    let w = alloc(&a, 1000);
    assert_eq!(a.stats().current, 1524);
    assert_eq!(a.stats().peak, 1524, "a new high-water mark");
    // A huge block (through the general path) joins the same counters.
    let huge = crate::limit::MAX_LARGE_SIZE + 1;
    let big = alloc(&a, huge);
    assert_eq!(a.stats().current, 1524 + huge);
    assert_eq!(a.stats().peak, 1524 + huge);
    drop(big);
    drop(w);
    drop(z);
    drop(x);
    let s = a.stats();
    assert_eq!((s.current, s.allocs, s.frees), (0, 5, 5));
    a.unbind_current_thread();
}

#[test]
fn fast_path_defers_to_the_limit_check() {
    // A block that would cross the soft threshold leaves the fast path for
    // the general one, which records the violation and grants the block
    // all the same.
    let a = CountingAlloc::new(System);
    a.request_start(Some(4 * M)).expect("limit");
    assert!(a.is_bound_to_current_thread());
    // Exact bytes (tracked mode), so the threshold is the limit itself.
    a.set_tracked(true);
    let mut fill = Vec::new();
    while a.usage() + 3000 <= 4 * M {
        fill.push(alloc(&a, 3000));
    }
    assert_eq!(a.check(), None);
    let room = 4 * M - a.usage();
    assert!(room < 3000);
    let exact = (room > 0).then(|| alloc(&a, room));
    assert_eq!(a.usage(), 4 * M);
    assert_eq!(a.check(), None, "exactly at the limit is fine");
    let over = alloc(&a, 8);
    assert!(!over.p.is_null(), "granted");
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: 4 * M,
            tried: 8
        })
    );
    assert_eq!(a.peak(), 4 * M + 8);
    a.clear();
    drop(over);
    drop(exact);
    drop(fill);
    assert_eq!(a.stats().current, 0);
    a.request_end();
    assert!(!a.is_bound_to_current_thread());
}

#[test]
fn observing_takes_the_ownership_and_gives_it_back() {
    let a = CountingAlloc::new(System);
    assert!(a.bind_to_current_thread());
    assert!(a.set_observer(record_event));
    let before = alloc(&a, 24);
    a.observe(true);
    assert!(
        !a.is_bound_to_current_thread(),
        "the observed thread gives the ownership up"
    );
    let during = alloc(&a, 24);
    let during_addr = during.p.addr();
    drop(during);
    let before_addr = before.p.addr();
    // A block of the owner freed while observed: reported.
    drop(before);
    a.observe(false);
    assert!(a.is_bound_to_current_thread(), "ownership comes back");
    let after = alloc(&a, 24);
    assert_eq!(
        take_events(),
        vec![
            HeapEvent::Alloc {
                size: 24,
                addr: during_addr
            },
            HeapEvent::Free { addr: during_addr },
            HeapEvent::Free { addr: before_addr },
        ]
    );
    drop(after);
    assert_eq!(a.stats().current, 0);
}

// ---- pending hook ---------------------------------------------------------

/// Calls of [`count_pending`] (only `pending_hook_runs_once_per_violation`
/// installs it).
static PENDING_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn count_pending() {
    PENDING_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn pending_hook_runs_once_per_violation() {
    let calls = || PENDING_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    let a = CountingAlloc::new(System);
    assert!(a.set_pending_hook(count_pending));
    assert!(!a.set_pending_hook(count_pending), "installed once");
    a.request_start(Some(4 * M)).expect("limit");
    let _small = alloc(&a, 100);
    assert_eq!(calls(), 0, "no violation, no call");
    let over = alloc(&a, 5 * M);
    assert!(a.is_pending());
    assert_eq!(calls(), 1, "an allocation over the soft limit");
    let _more = alloc(&a, 3 * M);
    assert_eq!(calls(), 1, "the first violation wins: nothing new recorded");
    assert!(a.take().is_some());
    assert!(a.try_reserve(16 * M).is_err());
    assert_eq!(calls(), 1, "nothing is recorded while handled");
    a.clear();
    assert!(a.try_reserve(16 * M).is_err());
    assert_eq!(calls(), 2, "try_reserve records, and calls, too");
    a.clear();
    drop(over);
    a.request_end();
}

#[test]
fn persistent_growth_and_release_leave_usage_and_peak_alone() {
    let a = CountingAlloc::new(System);
    a.set_baseline();
    let _warm = alloc(&a, 64);
    let start = a.usage();
    let peak = a.peak();
    // Growth inside the scope does not show.
    let mut b = a.persistent(|| alloc(&a, 4096));
    assert_eq!(a.usage(), start);
    assert_eq!(a.peak(), peak);
    // Neither does a reallocation...
    a.persistent(|| grow(&mut b, 8192));
    assert_eq!(a.usage(), start);
    // ...nor the release.
    a.persistent(|| drop(b));
    assert_eq!(a.usage(), start);
    assert_eq!(a.peak(), peak);
    // Ordinary blocks still count.
    let c = alloc(&a, 100);
    assert_eq!(a.usage(), start + 100);
    drop(c);
    assert_eq!(a.usage(), start);
}

#[test]
fn persistent_block_freed_outside_the_scope_shows_as_a_release() {
    let a = CountingAlloc::new(System);
    a.set_baseline();
    let start = a.usage();
    let b = a.persistent(|| alloc(&a, 1000));
    assert_eq!(a.usage(), start);
    drop(b);
    assert_eq!(a.usage(), start - 1000);
}

#[test]
fn persistent_scope_moves_the_limit_threshold_with_the_baseline() {
    let a = CountingAlloc::new(System);
    a.set_baseline();
    a.set_tracked(true);
    a.set_limit(Some(PHP_BASELINE + 10_000)).expect("limit");
    let _p = a.persistent(|| alloc(&a, 8_000));
    // 8,000 persistent bytes do not count: 9,000 more still fit.
    let _b = alloc(&a, 9_000);
    assert!(a.check().is_none());
}

#[test]
fn max_allocs_fails_the_nth_allocation_like_the_limit() {
    let a = CountingAlloc::new(System);
    a.request_start(None).expect("unlimited");
    a.set_max_allocs(3);
    let _b1 = alloc(&a, 100);
    let _b2 = alloc(&a, 100);
    assert!(!a.is_pending() && !a.max_allocs_hit());
    let b3 = alloc(&a, 200);
    assert!(
        !b3.p.is_null(),
        "granted, like an infallible Zend allocation"
    );
    assert!(a.max_allocs_hit());
    assert_eq!(
        a.check(),
        Some(OverLimit {
            limit: LIMIT_MAX,
            tried: 200
        }),
        "recorded like a soft-limit violation"
    );
    a.clear();
    let _b4 = alloc(&a, 100);
    assert!(!a.is_pending(), "only the n-th allocation fails");

    a.set_max_allocs(1);
    assert!(!a.max_allocs_hit(), "re-arming forgets the hit");
    let f = alloc(&a, 64);
    assert!(!f.p.is_null() && a.is_pending() && a.max_allocs_hit());
    a.clear();

    a.set_max_allocs(2);
    a.request_end();
    let _c1 = alloc(&a, 64);
    let _c2 = alloc(&a, 64);
    assert!(!a.is_pending(), "request end turns it off");
}

#[test]
fn refused_records_like_try_reserve() {
    let a = CountingAlloc::new(System);
    a.request_start(Some(8 * M)).expect("limit");
    let over = a.refused(123);
    assert_eq!(
        over,
        OverLimit {
            limit: 8 * M,
            tried: 123
        }
    );
    assert_eq!(a.check(), Some(over));
    a.request_end();
    assert_eq!(a.refused(5).limit, LIMIT_MAX, "unlimited");
}
