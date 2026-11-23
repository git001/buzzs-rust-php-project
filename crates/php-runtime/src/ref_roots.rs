// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The possible-root buffer for references (`gc_check_possible_root` for
//! `zend_reference`), consumed by the VM's cycle collector.
//!
//! When a [`PhpRef`] handle is dropped while other handles remain, the
//! reference may now be part of an unreachable cycle (`$a[] = &$a;
//! unset($a);`). Its address is buffered weakly (at most once) so that the
//! collector can start from it. Nothing is recorded until the collector
//! turns recording on.

use crate::reference::{PhpRef, WeakRef};
use crate::Value;
use rustc_hash::FxBuildHasher;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Buffer position by reference address. Only looked up, never iterated,
/// so the hasher changes nothing observable.
type Index = HashMap<usize, usize, FxBuildHasher>;

#[derive(Default)]
struct Buf {
    entries: Vec<WeakRef>,
    index: Index,
    /// Dead entries are dropped when `entries` grows past this (keeps the
    /// buffer bounded while the collector does not run).
    compact_at: usize,
}

impl Buf {
    fn compact(&mut self) {
        self.entries.retain(|w| w.strong_count() > 0);
        self.index.clear();
        for (i, w) in self.entries.iter().enumerate() {
            self.index.insert(w.addr(), i);
        }
        self.compact_at = self.entries.len() * 2;
    }
}

thread_local! {
    static RECORDING: Cell<bool> = const { Cell::new(false) };
    static NOTIFY: Cell<Option<fn()>> = const { Cell::new(None) };
    static BUF: RefCell<Buf> = RefCell::new(Buf::default());
}

/// Called by `PhpRef::drop` when a handle is released and others remain.
/// Costs one thread-local load while recording is off.
pub(crate) fn released(r: &PhpRef) {
    if !RECORDING.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    // Only a reference to an array or object can close a cycle
    // (`Z_COLLECTABLE`).
    if !r
        .try_borrow()
        .is_some_and(|v| matches!(*v, Value::Array(_) | Value::Object(_)))
    {
        return;
    }
    let addr = r.addr();
    let present = BUF
        .try_with(|b| {
            b.try_borrow().map_or(true, |b| {
                b.index.get(&addr).is_some_and(|&i| {
                    b.entries
                        .get(i)
                        .is_some_and(|w| w.strong_count() > 0 && w.addr() == addr)
                })
            })
        })
        .unwrap_or(true);
    if present {
        return;
    }
    // The collector may run (and empty the buffer) before the new root is
    // added, like `gc_possible_root_when_full`.
    if let Some(f) = NOTIFY.try_with(Cell::get).ok().flatten() {
        f();
    }
    let w = r.downgrade();
    let _ = BUF.try_with(|b| {
        if let Ok(mut b) = b.try_borrow_mut() {
            if b.entries.len() >= b.compact_at.max(4096) {
                b.compact();
            }
            let i = b.entries.len();
            b.entries.push(w);
            b.index.insert(addr, i);
        }
    });
}

/// `gc_check_possible_root` for a reference a live temporary or loop
/// variable holds after a collection (`zend_gc_check_root_tmpvars`):
/// buffers it like a release of one of its handles would.
pub fn check_possible_root(r: &PhpRef) {
    released(r);
}

/// Takes the buffered references that are still alive and empties the
/// buffer.
#[must_use]
pub fn take() -> Vec<Value> {
    let entries = BUF
        .try_with(|b| {
            b.try_borrow_mut()
                .map(|mut b| {
                    b.index.clear();
                    std::mem::take(&mut b.entries)
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|w| w.upgrade())
        .map(Value::Reference)
        .collect()
}

/// Number of buffered references still alive.
#[must_use]
pub fn len() -> usize {
    BUF.try_with(|b| {
        b.try_borrow().map_or(0, |b| {
            b.entries.iter().filter(|w| w.strong_count() > 0).count()
        })
    })
    .unwrap_or(0)
}

/// Buffered references set aside by [`stash`], held weakly: the roots a
/// due collection will look at, kept apart from roots buffered after it
/// became due (`gc_possible_root_when_full` collects before buffering).
#[derive(Default)]
pub struct Stash(Vec<WeakRef>);

impl Stash {
    /// Number of set-aside references still alive.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.iter().filter(|w| w.strong_count() > 0).count()
    }

    /// Whether no set-aside reference is alive.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for Stash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stash({})", self.0.len())
    }
}

/// Moves every buffered reference out of the buffer, weakly.
#[must_use]
pub fn stash() -> Stash {
    let entries = BUF
        .try_with(|b| {
            b.try_borrow_mut()
                .map(|mut b| {
                    b.index.clear();
                    std::mem::take(&mut b.entries)
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Stash(
        entries
            .into_iter()
            .filter(|w| w.strong_count() > 0)
            .collect(),
    )
}

/// Puts set-aside references back, ahead of those buffered since (they
/// were buffered first). References freed in the meantime are dropped,
/// and one buffered again is kept once.
pub fn unstash(s: Stash) {
    if s.0.is_empty() {
        return;
    }
    let _ = BUF.try_with(|b| {
        if let Ok(mut b) = b.try_borrow_mut() {
            let newer = std::mem::take(&mut b.entries);
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: the entries of two existing buffers"
            )]
            let mut entries = Vec::with_capacity(s.0.len() + newer.len());
            let mut index = Index::with_capacity_and_hasher(s.0.len() + newer.len(), FxBuildHasher);
            for w in s.0.into_iter().chain(newer) {
                if w.strong_count() == 0 {
                    continue;
                }
                let addr = w.addr();
                if index.contains_key(&addr) {
                    continue;
                }
                index.insert(addr, entries.len());
                entries.push(w);
            }
            b.entries = entries;
            b.index = index;
            b.compact_at = b.compact_at.max(b.entries.len() * 2);
        }
    });
}

/// Turns recording on or off; returns the previous setting.
pub fn set_recording(on: bool) -> bool {
    RECORDING.try_with(|r| r.replace(on)).unwrap_or(false)
}

/// Sets the function called before a new root is buffered.
pub fn set_notify(f: Option<fn()>) {
    let _ = NOTIFY.try_with(|n| n.set(f));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_shared_releases_once() {
        set_recording(true);
        let r = PhpRef::new(Value::empty_array());
        let r2 = r.clone();
        let r3 = r.clone();
        drop(r2);
        drop(r3);
        assert_eq!(len(), 1);
        let roots = take();
        assert_eq!(roots.len(), 1);
        assert_eq!(len(), 0);
        drop(roots);
        drop(r);
        set_recording(false);
    }

    #[test]
    fn stash_sets_roots_aside_and_unstash_puts_them_first() {
        set_recording(true);
        drop(take());
        let a = PhpRef::new(Value::empty_array());
        let b = PhpRef::new(Value::empty_array());
        drop(a.clone());
        let s = stash();
        assert_eq!((s.len(), len()), (1, 0));
        drop(b.clone());
        drop(a.clone());
        assert_eq!(len(), 2);
        unstash(s);
        assert_eq!(len(), 2);
        let roots = take();
        assert!(matches!(&roots[0], Value::Reference(r) if PhpRef::ptr_eq(r, &a)));
        assert!(matches!(&roots[1], Value::Reference(r) if PhpRef::ptr_eq(r, &b)));
        drop(roots);
        set_recording(false);
    }

    /// Many references, each released several times, are buffered once
    /// each and in release order (the address index finds every one).
    #[test]
    fn many_references_are_buffered_once_in_order() {
        set_recording(true);
        drop(take());
        let refs: Vec<PhpRef> = (0..300)
            .map(|_| PhpRef::new(Value::empty_array()))
            .collect();
        for _ in 0..3 {
            for r in &refs {
                drop(r.clone());
            }
        }
        assert_eq!(len(), refs.len());
        let roots = take();
        assert_eq!(roots.len(), refs.len());
        assert!(roots
            .iter()
            .zip(&refs)
            .all(|(v, r)| matches!(v, Value::Reference(x) if PhpRef::ptr_eq(x, r))));
        drop(roots);
        set_recording(false);
    }

    #[test]
    fn scalars_are_not_collectable() {
        set_recording(true);
        let r = PhpRef::new(Value::Int(1));
        drop(r.clone());
        assert_eq!(len(), 0);
        set_recording(false);
    }

    #[test]
    fn nothing_recorded_when_off() {
        set_recording(false);
        let r = PhpRef::new(Value::Int(1));
        drop(r.clone());
        assert_eq!(len(), 0);
    }
}
