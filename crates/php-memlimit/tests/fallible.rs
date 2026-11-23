// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The fallible growth API ([`php_memlimit::TryGrow`], [`php_memlimit::poll`],
//! `ZEND_MM_MAX_ALLOCS`) with the accounting allocator installed. One
//! `#[test]`, because the limit is process-wide state.

#![expect(clippy::disallowed_methods, reason = "test code")]

use std::collections::VecDeque;
use std::hint::black_box;

use php_memlimit::{OverLimit, TryGrow};

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;

#[test]
fn fallible_growth_stops_at_the_soft_limit() {
    php_memlimit::request_start(Some(8 * M)).expect("limit");

    // Growth that fits: done, nothing recorded.
    let mut ints: Vec<u64> = Vec::new();
    ints.try_grow(1000).expect("fits");
    assert!(ints.capacity() >= 1000);
    assert_eq!(php_memlimit::poll(), Ok(()));

    // Room already there: no check at all.
    let cap = ints.capacity();
    ints.try_grow(cap).expect("spare capacity");

    // Growth over the limit: refused before allocating, recorded,
    // collection unchanged.
    let before = php_memlimit::usage();
    let err = ints.try_grow(4 * M).expect_err("over the limit");
    assert_eq!(err.limit, 8 * M);
    assert_eq!(err.tried, (4 * M - cap) * 8, "the growth, not the new size");
    assert_eq!(ints.capacity(), cap, "unchanged");
    assert!(php_memlimit::usage() <= before + 4096, "nothing allocated");
    assert_eq!(php_memlimit::poll(), Err(err), "pending for the VM");
    assert_eq!(php_memlimit::check(), Some(err));
    php_memlimit::clear();

    // Strings and deques check the same way.
    let mut text = String::new();
    assert!(text.try_grow(16 * M).is_err());
    assert_eq!(text.capacity(), 0);
    php_memlimit::clear();
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.try_grow(10).expect("small");
    assert!(queue.try_grow(4 * M).is_err());
    php_memlimit::clear();

    // Overflowing sizes are a violation of usize::MAX bytes.
    let mut big: Vec<u16> = Vec::with_capacity(4);
    big.push(1);
    assert_eq!(
        big.try_grow(usize::MAX),
        Err(OverLimit {
            limit: 8 * M,
            tried: usize::MAX
        })
    );
    php_memlimit::clear();

    // The allocator's own record is what `poll` reports.
    let over = black_box(vec![0u8; 9 * M]);
    assert!(php_memlimit::poll().is_err(), "allocation over the limit");
    drop(over);
    php_memlimit::request_end();

    // `ZEND_MM_MAX_ALLOCS`, armed by every request start.
    php_memlimit::set_max_allocs_per_request(2);
    php_memlimit::request_start(Some(64 * M)).expect("limit");
    let first = black_box(Box::new(1u64));
    assert!(!php_memlimit::max_allocs_hit());
    let second = black_box(Box::new(2u64));
    assert!(php_memlimit::max_allocs_hit());
    assert!(php_memlimit::poll().is_err(), "recorded like the limit");
    drop((first, second));
    php_memlimit::request_end();
    php_memlimit::set_max_allocs_per_request(0);
    php_memlimit::request_start(Some(64 * M)).expect("limit");
    let third = black_box(Box::new(3u64));
    assert_eq!(php_memlimit::poll(), Ok(()), "off again");
    drop(third);
    php_memlimit::request_end();
}

/// A builder's buffer never grows past the limit: the refused growth is
/// recorded, later appends are dropped, the error comes back at the end.
/// On its own thread with its own allocator: the limit is this test's.
#[test]
fn checked_buf_stops_growing_at_the_limit() {
    std::thread::spawn(|| {
        php_memlimit::attach_thread();
        php_memlimit::request_start(Some(8 * M)).expect("limit");
        let mut out = php_memlimit::CheckedBuf::new();
        let chunk = [b'x'; 4096];
        let mut appended = 0usize;
        while out.check().is_ok() && appended < 64 * M {
            out.extend_from_slice(&chunk);
            appended += chunk.len();
        }
        let err = out.check().expect_err("stopped at the limit");
        assert_eq!(err.limit, 8 * M);
        assert!(out.len() <= 8 * M, "{}", out.len());
        assert!(php_memlimit::peak() <= 8 * M + 2 * M, "never grew past it");
        assert!(php_memlimit::is_pending(), "recorded for the VM");
        let len = out.len();
        out.push(b'y');
        out.extend([1, 2, 3]);
        assert_eq!(out.len(), len, "nothing appended after the refusal");
        assert!(
            std::fmt::Write::write_str(&mut out, "z").is_err(),
            "fmt::Write fails after the refusal"
        );
        assert_eq!(
            std::io::Write::write(&mut out, b"z").map_err(|e| e.kind()),
            Err(std::io::ErrorKind::OutOfMemory),
            "io::Write too"
        );
        assert_eq!(out.into_vec(), Err(err));
        php_memlimit::clear();
        let mut small = php_memlimit::CheckedBuf::new();
        php_memlimit::ByteSink::extend_from_slice(&mut small, b"ab");
        small.push(b'c');
        write!(small, "i:{};", 42);
        assert!(std::io::Write::write_all(&mut small, b"!").is_ok());
        assert_eq!(small.into_vec().as_deref(), Ok(&b"abci:42;!"[..]));
        php_memlimit::request_end();
    })
    .join()
    .expect("test thread");
}

#[test]
fn env_parsing() {
    assert_eq!(php_memlimit::max_allocs_from_env(None), 0);
    assert_eq!(php_memlimit::max_allocs_from_env(Some(b" 42")), 42);
    assert_eq!(php_memlimit::max_allocs_from_env(Some(b"-3")), 0);
    assert_eq!(php_memlimit::max_allocs_from_env(Some(b"x")), 0);
    assert!(!php_memlimit::trap_from_env(None));
    assert!(!php_memlimit::trap_from_env(Some(b"0")));
    assert!(php_memlimit::trap_from_env(Some(b"1")));
}
