// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Worker threads with their own allocator (`attach_thread`): usage, peak,
//! `memory_limit` and its violation belong to the thread, like php-src
//! ZTS's heap per thread.

use std::hint::black_box;
use std::sync::{Arc, Barrier};

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;

#[test]
fn each_attached_thread_counts_alone() {
    let ready = Arc::new(Barrier::new(2));
    let spawn = |limit: usize, alloc: usize, ready: Arc<Barrier>| {
        std::thread::spawn(move || {
            php_memlimit::attach_thread();
            assert!(!std::ptr::eq(
                php_memlimit::current(),
                &php_memlimit::GLOBAL
            ));
            php_memlimit::request_start(Some(limit)).expect("limit fits");
            let start = php_memlimit::usage();
            ready.wait();
            let block = black_box(vec![1u8; alloc]);
            let grown = php_memlimit::usage() - start;
            let over = php_memlimit::check();
            // Both threads have allocated when either looks again.
            ready.wait();
            let still = php_memlimit::usage() - start;
            drop(block);
            php_memlimit::clear();
            php_memlimit::request_end();
            (grown, still, over)
        })
    };
    // A stays far below its limit while B crosses its own.
    let a = spawn(64 * M, M, Arc::clone(&ready));
    let b = spawn(8 * M, 12 * M, ready);
    let (a_grown, a_still, a_over) = a.join().expect("thread A");
    let (b_grown, _, b_over) = b.join().expect("thread B");
    assert!((M..M + 64 * 1024).contains(&a_grown), "A {a_grown}");
    assert!((M..M + 64 * 1024).contains(&a_still), "A after B {a_still}");
    assert_eq!(a_over, None, "B's allocation is not A's");
    assert!(b_grown >= 12 * M, "B {b_grown}");
    assert_eq!(b_over.map(|o| o.limit), Some(8 * M));
}

#[test]
fn frees_of_another_threads_blocks_do_not_wrap() {
    // A buffer filled on one thread and freed on another (a server's I/O
    // thread and a worker) must not take the worker's count below zero.
    let buffer = black_box(vec![3u8; 4 * M]);
    std::thread::spawn(move || {
        php_memlimit::attach_thread();
        php_memlimit::request_start(Some(16 * M)).expect("limit fits");
        drop(buffer);
        let small = black_box(vec![0u8; 1024]);
        assert_eq!(php_memlimit::check(), None);
        assert!(php_memlimit::usage() < 16 * M);
        drop(small);
        php_memlimit::request_end();
    })
    .join()
    .expect("worker");
}

#[test]
fn ended_threads_hand_their_allocator_on() {
    let first = std::thread::spawn(|| {
        php_memlimit::attach_thread();
        std::ptr::from_ref(php_memlimit::current()).addr()
    })
    .join()
    .expect("first");
    let second = std::thread::spawn(|| {
        php_memlimit::attach_thread();
        let own = std::ptr::from_ref(php_memlimit::current()).addr();
        php_memlimit::request_start(None).expect("unlimited");
        (own, php_memlimit::usage())
    })
    .join()
    .expect("second");
    // The pool may hand either back, but a reused one starts afresh.
    let (_, usage) = second;
    assert!(usage < php_memlimit::PHP_BASELINE + 64 * 1024, "{usage}");
    assert_ne!(first, 0);
}

thread_local! {
    /// Calls of [`count_hook`] on this thread: the hook is process-wide,
    /// other tests' threads record violations of their own meanwhile.
    static HOOK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count_hook() {
    HOOK_CALLS.with(|c| c.set(c.get() + 1));
}

/// The VM installs the pending hook in `Vm::new`, which a worker thread
/// runs after `attach_thread`: the hook must reach that thread's
/// allocator too, or a recorded violation never interrupts the script.
#[test]
fn a_hook_set_after_attaching_reaches_the_thread() {
    let calls = std::thread::spawn(|| {
        php_memlimit::attach_thread();
        assert!(php_memlimit::set_pending_hook(count_hook));
        php_memlimit::request_start(Some(8 * M)).expect("limit");
        let block = black_box(vec![1u8; 9 * M]);
        let calls = HOOK_CALLS.with(std::cell::Cell::get);
        assert!(php_memlimit::is_pending());
        drop(block);
        php_memlimit::clear();
        php_memlimit::request_end();
        calls
    })
    .join()
    .expect("worker");
    assert_eq!(calls, 1);
}
