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

    /// [`try_grow`](Self::try_grow) to exactly `additional` more elements,
    /// like the collection's `try_reserve_exact`.
    ///
    /// # Errors
    ///
    /// The recorded [`OverLimit`].
    fn try_grow_exact(&mut self, additional: usize) -> Result<(), OverLimit>;
}

impl<T> TryGrow for Vec<T> {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        grow(
            self.len(),
            self.capacity(),
            additional,
            size_of::<T>(),
            Growth::Amortized,
            |n| self.try_reserve(n),
        )
    }

    fn try_grow_exact(&mut self, additional: usize) -> Result<(), OverLimit> {
        grow(
            self.len(),
            self.capacity(),
            additional,
            size_of::<T>(),
            Growth::Exact,
            |n| self.try_reserve_exact(n),
        )
    }
}

impl TryGrow for String {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        let (len, cap) = (self.len(), self.capacity());
        grow(len, cap, additional, 1, Growth::Amortized, |n| {
            self.try_reserve(n)
        })
    }

    fn try_grow_exact(&mut self, additional: usize) -> Result<(), OverLimit> {
        let (len, cap) = (self.len(), self.capacity());
        grow(len, cap, additional, 1, Growth::Exact, |n| {
            self.try_reserve_exact(n)
        })
    }
}

impl<T> TryGrow for VecDeque<T> {
    fn try_grow(&mut self, additional: usize) -> Result<(), OverLimit> {
        let (len, cap) = (self.len(), self.capacity());
        grow(
            len,
            cap,
            additional,
            size_of::<T>(),
            Growth::Amortized,
            |n| self.try_reserve(n),
        )
    }

    fn try_grow_exact(&mut self, additional: usize) -> Result<(), OverLimit> {
        let (len, cap) = (self.len(), self.capacity());
        grow(len, cap, additional, size_of::<T>(), Growth::Exact, |n| {
            self.try_reserve_exact(n)
        })
    }
}

/// The byte buffer of a builder that grows one script-sized result
/// (`serialize()`, `var_export()`, an encoder): it never grows past
/// `memory_limit`. Its appends keep `Vec<u8>`'s names and stay infallible
/// at the call site; a growth the limit refuses is recorded (pending for
/// the VM), and from then on the buffer appends nothing more. The builder
/// asks [`CheckedBuf::check`] where it can stop early (per element) and
/// gets the error back from [`CheckedBuf::into_vec`] at the latest, so no
/// append site needs a `?` and none can be missed.
#[derive(Debug, Default)]
pub struct CheckedBuf {
    buf: Vec<u8>,
    over: Option<OverLimit>,
}

impl CheckedBuf {
    /// An empty buffer; nothing is allocated.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            over: None,
        }
    }

    /// Appends `byte`.
    #[inline]
    pub fn push(&mut self, byte: u8) {
        if self.buf.len() < self.buf.capacity() || self.room(1) {
            self.buf.push(byte);
        }
    }

    /// Appends `bytes`.
    #[inline]
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        if self.buf.capacity() - self.buf.len() >= bytes.len() || self.room(bytes.len()) {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Inserts `byte` at `index`, shifting the rest (`Vec::insert`).
    ///
    /// # Panics
    ///
    /// If `index > len`.
    pub fn insert(&mut self, index: usize, byte: u8) {
        if self.buf.len() < self.buf.capacity() || self.room(1) {
            self.buf.insert(index, byte);
        }
    }

    /// Appends formatted text: `write!(buf, "i:{n};")` formats straight
    /// into the buffer, without a `String` in between. Infallible at the
    /// call site like the other appends (a refused growth is in
    /// [`CheckedBuf::check`]).
    pub fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = std::fmt::Write::write_fmt(self, args);
    }

    /// Shortens the buffer to `len` bytes (`Vec::truncate`).
    pub fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// Whether a growth was refused, as the error; the builder stops then.
    ///
    /// # Errors
    ///
    /// The recorded [`OverLimit`].
    #[inline]
    pub fn check(&self) -> Result<(), OverLimit> {
        match self.over {
            Some(over) => Err(over),
            None => Ok(()),
        }
    }

    /// The built bytes.
    ///
    /// # Errors
    ///
    /// The recorded [`OverLimit`] when a growth was refused: the bytes are
    /// incomplete then and are dropped.
    pub fn into_vec(self) -> Result<Vec<u8>, OverLimit> {
        self.check()?;
        Ok(self.buf)
    }

    /// Makes room for `additional` more bytes unless the limit refuses it
    /// (then it is recorded and nothing more is appended).
    #[cold]
    #[inline(never)]
    fn room(&mut self, additional: usize) -> bool {
        if self.over.is_some() {
            return false;
        }
        match self.buf.try_grow(additional) {
            Ok(()) => true,
            Err(over) => {
                self.over = Some(over);
                false
            }
        }
    }
}

/// A byte buffer that helpers append to: a plain `Vec<u8>` or a
/// [`CheckedBuf`], so one encoder serves both.
pub trait ByteSink {
    /// Appends `byte`.
    fn push(&mut self, byte: u8);
    /// Appends `bytes`.
    fn extend_from_slice(&mut self, bytes: &[u8]);
}

impl ByteSink for Vec<u8> {
    #[inline]
    fn push(&mut self, byte: u8) {
        Vec::push(self, byte);
    }

    #[inline]
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        Vec::extend_from_slice(self, bytes);
    }
}

impl ByteSink for CheckedBuf {
    #[inline]
    fn push(&mut self, byte: u8) {
        CheckedBuf::push(self, byte);
    }

    #[inline]
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        CheckedBuf::extend_from_slice(self, bytes);
    }
}

impl Extend<u8> for CheckedBuf {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, bytes: I) {
        for b in bytes {
            self.push(b);
        }
    }
}

/// For formatting code that takes any `fmt::Write`: fails once a growth
/// was refused.
impl std::fmt::Write for CheckedBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.extend_from_slice(s.as_bytes());
        self.over.map_or(Ok(()), |_| Err(std::fmt::Error))
    }
}

/// For encoders that take any `io::Write`: fails with `OutOfMemory` once a
/// growth was refused.
impl std::io::Write for CheckedBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.extend_from_slice(bytes);
        match self.over {
            Some(_) => Err(std::io::ErrorKind::OutOfMemory.into()),
            None => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::ops::Deref for CheckedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

/// How a collection picks its new capacity.
#[derive(Clone, Copy)]
enum Growth {
    /// `try_reserve`: std's amortized doubling.
    Amortized,
    /// `try_reserve_exact`: exactly what is asked for.
    Exact,
}

/// The checked growth of a collection with `len` of `cap` elements of
/// `elem` bytes by `additional`; `reserve` is its `try_reserve` or
/// `try_reserve_exact`, as `growth` says.
fn grow(
    len: usize,
    cap: usize,
    additional: usize,
    elem: usize,
    growth: Growth,
    reserve: impl FnOnce(usize) -> Result<(), TryReserveError>,
) -> Result<(), OverLimit> {
    if cap - len >= additional {
        return Ok(());
    }
    if elem != 0 {
        let Some(required) = len.checked_add(additional) else {
            return Err(crate::refused(usize::MAX));
        };
        let new_cap = match growth {
            Growth::Amortized => amortized_capacity(cap, required, elem),
            Growth::Exact => required,
        };
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
