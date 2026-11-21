// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end checks with the thread dispatcher installed as this test
//! binary's global allocator, on a thread without its own (so `GLOBAL`). Everything runs in one `#[test]` because the limit is
//! process-wide state.

use std::hint::black_box;

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;

#[test]
fn global_allocator_lifecycle() {
    assert!(php_memlimit::GLOBAL.is_active());

    // Request start: usage restarts at the PHP baseline.
    php_memlimit::request_start(Some(10 * M)).expect("10M fits");
    assert_eq!(php_memlimit::limit(), Some(10 * M));
    assert_eq!(php_memlimit::hard_limit(), Some(20 * M));
    let start = php_memlimit::usage();
    assert!(
        start.abs_diff(php_memlimit::PHP_BASELINE) < 64 * 1024,
        "{start}"
    );
    assert_eq!(
        php_memlimit::memory_get_usage(true),
        php_memlimit::CHUNK_SIZE
    );

    // The request thread owns the accounting.
    assert!(php_memlimit::GLOBAL.is_bound_to_current_thread());
    let small: Vec<Box<[u8; 40]>> = (0..1000).map(|_| Box::new([7u8; 40])).collect();
    assert!(small.iter().all(|b| b.iter().all(|&x| x == 7)));
    drop(black_box(small));
    assert_eq!(
        php_memlimit::memory_get_usage(true),
        php_memlimit::CHUNK_SIZE
    );

    // Deltas are visible to memory_get_usage().
    let v = black_box(vec![1u8; M]);
    assert!(php_memlimit::usage() >= start + M);
    drop(v);
    assert!(php_memlimit::memory_get_peak_usage(false) >= start + M);
    php_memlimit::reset_peak();
    assert!(php_memlimit::memory_get_peak_usage(false) < start + M);

    // ini_set('memory_limit', '1M') fails against the real usage.
    let err = php_memlimit::set_limit(Some(M)).expect_err("below usage");
    assert_eq!(
        err.to_string(),
        "Failed to set memory limit to 1048576 bytes (Current memory usage is 2097152 bytes)"
    );

    // Builtins reserve before allocating.
    assert!(php_memlimit::try_reserve(300 * M).is_err());
    let over = php_memlimit::check().expect("reservation recorded");
    assert_eq!(over.limit, 10 * M);
    assert_eq!(over.tried, 300 * M);
    php_memlimit::clear();

    // Crossing the soft limit succeeds but is flagged for the VM.
    let big = black_box(vec![1u8; 12 * M]);
    let over = php_memlimit::check().expect("soft limit crossed");
    assert_eq!(over.limit, 10 * M);
    assert!(over.tried >= 2 * M, "{over:?}");
    assert!(over
        .to_string()
        .starts_with("Allowed memory size of 10485760 bytes exhausted"));
    drop(big);
    php_memlimit::clear();
    assert_eq!(php_memlimit::check(), None);

    // Fallible scope: Rust's try_reserve reports the overrun as an Err.
    let mut buf: Vec<u8> = Vec::new();
    let r = php_memlimit::fallible(|| buf.try_reserve_exact(15 * M));
    assert!(r.is_err());
    assert!(php_memlimit::check().is_some());
    php_memlimit::clear();
    assert!(php_memlimit::fallible(|| buf.try_reserve_exact(M)).is_ok());
    drop(buf);

    // Request end lifts the limit and gives ownership up (this thread was
    // bound by `request_start`).
    php_memlimit::request_end();
    assert_eq!(php_memlimit::limit(), None);
    assert!(!php_memlimit::GLOBAL.is_bound_to_current_thread());
    let big = black_box(vec![1u8; 30 * M]);
    assert_eq!(php_memlimit::check(), None);
    drop(big);

    // php-cli's way: bound explicitly before the request, the thread stays
    // the owner through the teardown that follows `request_end`.
    assert!(php_memlimit::bind_to_current_thread());
    php_memlimit::request_start(Some(10 * M)).expect("10M fits");
    let small: Vec<Box<[u8; 24]>> = (0..100).map(|_| Box::new([1u8; 24])).collect();
    php_memlimit::request_end();
    assert!(php_memlimit::GLOBAL.is_bound_to_current_thread());
    drop(black_box(small));

    // gc_mem_caches() has nothing to collect: mimalloc gives empty pages
    // back itself (php-src without its allocator returns 0 as well).
    let boxes: Vec<Box<[u8; 3072]>> = (0..1024).map(|_| Box::new([2u8; 3072])).collect();
    assert!(boxes.iter().all(|b| b[3071] == 2));
    drop(black_box(boxes));
    assert_eq!(php_memlimit::gc_mem_caches(), 0);
    php_memlimit::unbind_current_thread();
    assert!(!php_memlimit::GLOBAL.is_bound_to_current_thread());
}
