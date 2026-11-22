// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The headroom after the memory fatal ([`php_memlimit::SHUTDOWN_GRACE`]):
//! delivering the violation (`take`) lets shutdown functions allocate up to
//! one php-src chunk past the limit, once per request, without changing the
//! limit or its messages. One `#[test]`, because the limit is process-wide
//! state.

use std::hint::black_box;

use php_memlimit::SHUTDOWN_GRACE;

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;
const LIMIT: usize = 8 * M;

#[test]
fn the_memory_fatal_leaves_room_for_shutdown() {
    // Byte-exact checks (`USE_TRACKED_ALLOC`), so "almost full" is exact;
    // the chunk model is covered end to end by memlimit_p0's
    // `fgets_loop_into_array`.
    php_memlimit::set_tracked(true);
    php_memlimit::request_start(Some(LIMIT)).expect("limit");
    // Fill up to just below the limit, then a growth that does not fit.
    let fill = black_box(vec![1u8; LIMIT - php_memlimit::usage() - M / 2]);
    let over = php_memlimit::try_reserve(M).expect_err("past the limit");
    assert_eq!(over.limit, LIMIT);

    // Before delivery: no room.
    assert!(php_memlimit::try_reserve(M).is_err());
    let taken = php_memlimit::take().expect("pending");
    assert_eq!(taken, over);
    // Request shutdown re-arms recording.
    php_memlimit::clear();

    // After delivery: one chunk of headroom, the limit unchanged.
    assert_eq!(php_memlimit::limit(), Some(LIMIT));
    assert!(php_memlimit::try_reserve(M).is_ok(), "room for shutdown");
    let second = php_memlimit::try_reserve(SHUTDOWN_GRACE + M).expect_err("past the grace");
    assert_eq!(
        second.limit, LIMIT,
        "messages name the limit, not the grace"
    );

    // Only once: a second delivery adds nothing.
    let _ = php_memlimit::take();
    php_memlimit::clear();
    assert!(php_memlimit::try_reserve(SHUTDOWN_GRACE + M).is_err());

    // The next request starts without it.
    drop(fill);
    php_memlimit::request_end();
    php_memlimit::request_start(Some(LIMIT)).expect("limit");
    let fill = black_box(vec![1u8; LIMIT - php_memlimit::usage() - M / 2]);
    assert!(php_memlimit::try_reserve(M).is_err(), "no grace left over");
    drop(fill);
    php_memlimit::request_end();
}
