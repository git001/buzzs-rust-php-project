// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `memory_limit` checks of exif.c and image.c (the `ZEND_HAS_OOM()`
//! checks and the fallible allocations) on the process accounting of
//! `php-memlimit`: a block the input file sizes is checked before it is
//! allocated, and once the limit was crossed parsing stops. The violation
//! stays recorded, so the VM raises "Allowed memory size of %d bytes
//! exhausted" when the call returns.

/// Checks an allocation of `n` bytes against `memory_limit`; `false`
/// (and the violation recorded) when it does not fit.
pub(crate) fn reserve(n: u64) -> bool {
    php_memlimit::try_reserve(usize::try_from(n).unwrap_or(usize::MAX)).is_ok()
}

/// `ZEND_HAS_OOM()`: the limit was crossed.
pub(crate) fn exhausted() -> bool {
    php_memlimit::is_pending()
}
