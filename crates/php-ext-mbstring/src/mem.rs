// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `memory_limit` checks for the `mb_*` functions whose output size the
//! caller controls.
//!
//! Live memory is counted by `php-memlimit`'s allocator; an allocation that
//! crosses the limit is turned into PHP's fatal error by the VM after the
//! call. Where PHP allocates a buffer up front (or a larger one than this
//! implementation needs), [`check`] reserves that size first with
//! [`php_memlimit::try_reserve`], as PHP's fallible allocations do.

/// `sizeof(Bucket)` of a packed array element (a `zval`).
const PACKED_SLOT: usize = 16;

/// Bytes of a `zend_string` of `len` bytes (`_ZSTR_STRUCT_SIZE`, rounded).
pub(crate) fn zstr(len: usize) -> usize {
    (24 + len + 1 + 7) & !7
}

/// Bytes of a packed array of `n` elements.
pub(crate) fn packed(n: usize) -> usize {
    56 + n.max(8).next_power_of_two() * PACKED_SLOT
}

/// Reserves `alloc` bytes PHP allocates up front: over the limit the
/// violation is recorded and `?` leaves the builtin, where the VM raises
/// `Allowed memory size of N bytes exhausted (tried to allocate M bytes)`.
pub(crate) fn check(alloc: usize) -> Result<(), php_memlimit::OverLimit> {
    php_memlimit::try_reserve(alloc)
}

/// Whether an allocation already crossed `memory_limit` (the VM raises
/// the fatal error after the call; loops building results stop early).
pub(crate) fn pending() -> bool {
    php_memlimit::is_pending()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zend_sizes() {
        assert_eq!(zstr(0), 32);
        assert_eq!(zstr(7), 32);
        assert_eq!(zstr(8), 40);
        assert_eq!(packed(3), 56 + 8 * 16);
    }
}
