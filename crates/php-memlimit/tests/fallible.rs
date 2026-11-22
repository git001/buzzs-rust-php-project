// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The fallible growth API ([`php_memlimit::TryGrow`], [`php_memlimit::poll`],
//! `ZEND_MM_MAX_ALLOCS`) with the accounting allocator installed. One
//! `#[test]`, because the limit is process-wide state.

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
