// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Fallible growth of Rust collections against `memory_limit`.
//!
//! [`TryGrow::try_grow`] checks the bytes a growth would allocate with
//! [`try_reserve_n`](crate::try_reserve_n) *before* allocating, then grows
//! with the collection's own `try_reserve`. Over the limit it returns the
//! recorded [`OverLimit`] and leaves the collection unchanged, so a loop
//! that builds a script-sized result stops at the growth that would cross
//! the limit (php-src's `zend_*_falloc` + `RETURN_OOM()`), and the error
//! travels with `?` up to the VM, which reports the fatal.
//!
//! The allocator still counts the real allocation; the check only decides
//! whether it may happen, so nothing is counted twice.

use std::collections::{TryReserveError, VecDeque};
use std::mem::size_of;

use crate::limit::OverLimit;

/// Collections whose growth can be checked against `memory_limit` first.
pub trait TryGrow {
    /// Makes room for at least `additional` more elements, like the
    /// collection's `try_reserve`, unless the allocation that takes would
    /// cross the soft limit (or fails): then the violation is recorded
    /// and returned, and the collection is unchanged.
    ///
    /// # Errors
    ///
    /// The recorded [`OverLimit`].
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit>;
}

impl<T> TryGrow for Vec<T> {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        grow(
            self.len(),
            self.capacity(),
            additional,
            size_of::<T>(),
            |n| self.try_reserve(n),
        )
    }
}

impl TryGrow for String {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        grow(self.len(), self.capacity(), additional, 1, |n| {
            self.try_reserve(n)
        })
    }
}

impl<T> TryGrow for VecDeque<T> {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        grow(
            self.len(),
            self.capacity(),
            additional,
            size_of::<T>(),
            |n| self.try_reserve(n),
        )
    }
}

/// The checked growth of a collection with `len` of `cap` elements of
/// `elem` bytes by `additional`; `reserve` is its `try_reserve`.
fn grow(
    len: usize,
    cap: usize,
    additional: usize,
    elem: usize,
    reserve: impl FnOnce(usize) -> Result<(), TryReserveError>,
) -> Result<(), OverLimit> {
    if cap - len >= additional {
        return Ok(());
    }
    if elem != 0 {
        let Some(required) = len.checked_add(additional) else {
            return Err(crate::refused(usize::MAX));
        };
        let new_cap = amortized_capacity(cap, required, elem);
        crate::try_reserve_n(new_cap - cap, elem, 0)?;
    }
    reserve(additional).map_err(|_| crate::refused(additional.saturating_mul(elem)))
}

/// The capacity std's amortized growth (`RawVec::grow_amortized`) picks
/// for at least `required` elements of `elem` bytes from capacity `cap`:
/// double, at least `required`, at least the minimum non-zero capacity.
fn amortized_capacity(cap: usize, required: usize, elem: usize) -> usize {
    let min_non_zero = if elem == 1 {
        8
    } else if elem <= 1024 {
        4
    } else {
        1
    };
    cap.saturating_mul(2).max(required).max(min_non_zero)
}

#[cfg(test)]
mod tests {
    use super::amortized_capacity;

    #[test]
    fn amortized_capacity_matches_std() {
        let mut v: Vec<u8> = Vec::new();
        v.try_reserve(1).unwrap();
        assert_eq!(v.capacity(), amortized_capacity(0, 1, 1));
        let mut v: Vec<u64> = Vec::with_capacity(5);
        v.extend([1, 2, 3, 4, 5]);
        v.try_reserve(1).unwrap();
        assert_eq!(v.capacity(), amortized_capacity(5, 6, 8));
        let mut v: Vec<[u8; 2048]> = Vec::new();
        v.try_reserve(1).unwrap();
        assert_eq!(v.capacity(), amortized_capacity(0, 1, 2048));
        let mut v: Vec<u32> = Vec::new();
        v.try_reserve(100).unwrap();
        assert_eq!(v.capacity(), amortized_capacity(0, 100, 4));
    }
}
