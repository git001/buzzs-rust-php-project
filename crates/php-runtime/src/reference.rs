// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP references (`zend_reference`): the shared mutable slot behind `&`.
//!
//! php-src's `zend_reference` is 32 bytes: the refcounted header (refcount
//! and type info, 8), the `zval` (16) and `sources` (8), a tagged pointer to
//! the typed properties the reference is bound to. A [`PhpRef`] is one thin
//! pointer to a [`RefBox`] of 32 bytes as well: a `u32` strong count and a
//! `u32` weak-count word (8), then the value in a `RefCell` (24). The
//! `RefCell`'s borrow flag, which makes [`PhpRef::borrow_mut`] and
//! [`PhpRef::try_borrow`] checked and lets them hand out std's `Ref` and
//! `RefMut`, takes the word php-src spends on `sources`.
//!
//! So the VM's metadata (the type sources of typed references,
//! `ZEND_REF_HAS_TYPE_SOURCES`) lives in a thread-local side table keyed by
//! the reference's address, and one bit of the weak-count word says whether
//! the reference has an entry there. An untyped reference, nearly every one,
//! is told apart with one load and never touches the table; only typed
//! references pay a hash lookup where php-src loads `sources`.
//!
//! The counts are kept by hand rather than by `Rc`, which spends two words
//! on them. The weak count exists for the cycle collector's root buffer
//! ([`crate::ref_roots`]), which holds possible roots without keeping them
//! alive, like php-src's root buffer holds them by address.

use crate::value::Value;
use rustc_hash::FxBuildHasher;
use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::rc::Rc;

/// The allocation behind a [`PhpRef`] (`zend_reference`), also known as [`RefInner`].
///
/// `repr(C)` fixes the layout: the two count words (4 + 4), then the value
/// cell (24), 32 bytes in all.
#[repr(C)]
pub struct RefBox {
    /// Number of [`PhpRef`] handles (`GC_REFCOUNT`).
    pub strong: Cell<u32>,
    /// The [`WEAK_COUNT`] bits: the number of [`WeakRef`] handles, plus one
    /// held by the strong handles collectively while any exists (as in
    /// `Rc`). The [`HAS_META`] bit: the reference has an entry in [`META`].
    pub weak: Cell<u32>,
    /// Dropped when the last strong handle goes; the allocation itself stays
    /// until the last weak handle goes.
    pub value: ManuallyDrop<RefCell<Value>>,
}

/// Alias for [`RefBox`] (the reference allocation, matching php-src's 32-byte `zend_reference`).
#[allow(dead_code)]
pub type RefInner = RefBox;

/// Alias for property reference sources attached as metadata.
#[allow(dead_code)]
pub type PropRefSource = Rc<dyn Any>;

#[allow(dead_code)]
impl RefBox {
    /// Borrows the referenced value.
    #[inline]
    #[must_use]
    pub fn val(&self) -> Ref<'_, Value> {
        self.value.borrow()
    }

    /// Borrows the referenced value.
    #[inline]
    #[must_use]
    pub fn borrow(&self) -> Ref<'_, Value> {
        self.value.borrow()
    }

    /// Mutably borrows the referenced value.
    #[inline]
    #[must_use]
    pub fn borrow_mut(&self) -> RefMut<'_, Value> {
        self.value.borrow_mut()
    }
}

/// The bit of [`RefBox::weak`] set while the reference has VM metadata in
/// [`META`] (php-src: `ZEND_REF_HAS_TYPE_SOURCES`, a non-null `sources`).
const HAS_META: u32 = 1 << 31;

/// The weak count's bits of [`RefBox::weak`].
const WEAK_COUNT: u32 = HAS_META - 1;

/// A PHP reference (`zend_reference`): a shared mutable slot holding a value.
///
/// Cloning a `PhpRef` creates another handle to the *same* slot (it is how
/// `$a = &$b` is represented); [`PhpRef::refcount`] reports the number of
/// handles, which `var_dump` uses to decide whether to print `&`.
///
/// The handle is one thin pointer (8 bytes, with a niche for `Option`) to a
/// 32-byte allocation, like the `zend_reference *` in a zval. Like `Rc`, it
/// is neither `Send` nor `Sync`.
pub struct PhpRef {
    ptr: NonNull<RefBox>,
    _owns: PhantomData<RefBox>,
}

impl PhpRef {
    /// Creates a new reference holding `value`. A reference never directly
    /// contains another reference: nested references are unwrapped.
    #[must_use]
    pub fn new(value: Value) -> Self {
        let value = match value {
            Value::Reference(r) => r.get(),
            v => v,
        };
        let b = Box::new(RefBox {
            strong: Cell::new(1),
            weak: Cell::new(1),
            value: ManuallyDrop::new(RefCell::new(value)),
        });
        PhpRef {
            ptr: NonNull::from(Box::leak(b)),
            _owns: PhantomData,
        }
    }

    #[inline]
    fn inner(&self) -> &RefBox {
        // SAFETY: this handle owns one strong count, so the allocation is
        // alive and its value initialised: the value is dropped only once
        // the strong count is zero, when no handle is left to get here.
        // Every byte of the box is inside a `Cell` or the `RefCell`, so a
        // shared reference to it may coexist with the mutable access those
        // cells hand out (a `RefMut` of the value); nothing else makes a
        // `&mut` into a box before its value is dropped.
        unsafe { self.ptr.as_ref() }
    }

    #[inline]
    fn cell(&self) -> &RefCell<Value> {
        &self.inner().value
    }

    /// Returns a clone of the referenced value.
    #[inline]
    #[must_use]
    pub fn get(&self) -> Value {
        self.cell().borrow().clone()
    }

    /// Returns a clone of the referenced value (alias for [`get`]).
    #[inline]
    #[must_use]
    pub fn val(&self) -> Value {
        self.get()
    }

    /// Replaces the referenced value, returning the old one.
    #[inline]
    pub fn set(&self, value: Value) -> Value {
        let value = match value {
            Value::Reference(r) if !PhpRef::ptr_eq(&r, self) => r.get(),
            Value::Reference(_) => return self.get(),
            v => v,
        };
        std::mem::replace(&mut *self.cell().borrow_mut(), value)
    }

    /// Borrows the referenced value.
    ///
    /// # Panics
    /// Panics if the value is currently mutably borrowed.
    #[inline]
    #[must_use]
    pub fn borrow(&self) -> Ref<'_, Value> {
        self.cell().borrow()
    }

    /// Mutably borrows the referenced value.
    ///
    /// # Panics
    /// Panics if the value is currently borrowed.
    #[inline]
    #[must_use]
    pub fn borrow_mut(&self) -> RefMut<'_, Value> {
        self.cell().borrow_mut()
    }

    /// Borrows the referenced value unless it is currently mutably borrowed.
    #[inline]
    #[must_use]
    pub fn try_borrow(&self) -> Option<Ref<'_, Value>> {
        self.cell().try_borrow().ok()
    }

    /// Mutably borrows the referenced value unless it is currently borrowed.
    #[inline]
    #[must_use]
    pub fn try_borrow_mut(&self) -> Option<RefMut<'_, Value>> {
        self.cell().try_borrow_mut().ok()
    }

    /// Whether two handles denote the same reference.
    #[inline]
    #[must_use]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    /// Number of handles to this reference (`GC_REFCOUNT`).
    #[inline]
    #[must_use]
    pub fn refcount(&self) -> usize {
        self.inner().strong.get() as usize
    }

    /// An address usable as an identity key (for recursion detection and
    /// serialization back-references).
    #[inline]
    #[must_use]
    pub fn addr(&self) -> usize {
        addr_of(self.ptr)
    }

    /// The VM's metadata attached to this reference, if any (one load when
    /// there is none).
    #[inline]
    #[must_use]
    pub fn meta(&self) -> Option<Rc<dyn Any>> {
        if self.inner().weak.get() & HAS_META == 0 {
            return None;
        }
        meta_get(self.addr())
    }

    /// Attaches (or clears) VM metadata. Replacing or clearing drops the
    /// previous metadata; freeing the reference drops it too.
    pub fn set_meta(&self, meta: Option<Rc<dyn Any>>) {
        let word = &self.inner().weak;
        let old = match meta {
            Some(m) => {
                word.set(word.get() | HAS_META);
                meta_insert(self.addr(), m)
            }
            None if word.get() & HAS_META != 0 => {
                word.set(word.get() & !HAS_META);
                meta_remove(self.addr())
            }
            None => None,
        };
        // Only now, with the table released: dropping metadata may run code
        // that uses it.
        drop(old);
    }

    /// Attaches a property source to this reference (for typed references).
    #[inline]
    pub fn add_source(&self, source: Rc<dyn Any>) {
        self.set_meta(Some(source));
    }

    /// A weak handle (for the cycle collector's root buffer).
    pub(crate) fn downgrade(&self) -> WeakRef {
        inc_weak(&self.inner().weak);
        WeakRef { ptr: self.ptr }
    }
}

impl Clone for PhpRef {
    #[inline]
    fn clone(&self) -> Self {
        let strong = &self.inner().strong;
        let n = strong.get();
        // Like `Rc`: a count that wrapped would free the reference while
        // handles to it remain.
        if n == u32::MAX {
            std::process::abort();
        }
        strong.set(n + 1);
        PhpRef {
            ptr: self.ptr,
            _owns: PhantomData,
        }
    }
}

/// Releasing a handle while others remain (`GC_DELREF` to a non-zero
/// count) makes the reference a possible cycle root
/// (`gc_check_possible_root`). Releasing the last one frees the value.
impl Drop for PhpRef {
    #[inline]
    fn drop(&mut self) {
        if self.inner().strong.get() > 1 {
            // May run the collector, which may release other handles of
            // this reference: the count is read again below. This handle
            // still holds its count meanwhile, so the allocation stays.
            crate::ref_roots::released(self);
        }
        let strong = &self.inner().strong;
        let n = strong.get() - 1;
        strong.set(n);
        if n == 0 {
            // SAFETY: this was the last strong handle, and `self` is not
            // used afterwards.
            unsafe { release_last(self.ptr) }
        }
    }
}

/// Like `Value`'s, does not panic while the value is mutably borrowed.
impl fmt::Debug for PhpRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_borrow() {
            Some(v) => write!(f, "PhpRef({:?})", &*v),
            None => f.write_str("PhpRef(<borrowed>)"),
        }
    }
}

/// The address of the allocation at `ptr`: [`PhpRef::addr`], and the key of
/// its [`META`] entry.
#[inline]
fn addr_of(ptr: NonNull<RefBox>) -> usize {
    ptr.as_ptr().cast::<u8>() as usize
}

/// Adds a weak handle to the count in `word`. Like `Rc`, aborts rather than
/// let the count run into [`HAS_META`] (a wrapped count would free the
/// allocation while handles to it remain).
#[inline]
fn inc_weak(word: &Cell<u32>) {
    let n = word.get();
    if n & WEAK_COUNT == WEAK_COUNT {
        std::process::abort();
    }
    word.set(n + 1);
}

/// Frees the value of the allocation at `ptr`, whose strong count has just
/// reached zero, releases the strong handles' collective weak count, then
/// drops its metadata.
///
/// # Safety
/// `ptr` must be a live reference allocation whose strong count is zero and
/// whose value has not been dropped yet (this runs once per allocation).
#[inline(never)]
unsafe fn release_last(ptr: NonNull<RefBox>) {
    /// Finishes the release after the value's drop, also when that drop
    /// panics (as `Rc` still frees its allocation then).
    struct Rest(NonNull<RefBox>);
    impl Drop for Rest {
        fn drop(&mut self) {
            // SAFETY: `release_last`'s contract, now that the value is
            // dropped (or was being dropped by a destructor that panicked:
            // nothing reads it again either way); this runs once.
            unsafe { release_meta_and_weak(self.0) }
        }
    }
    let rest = Rest(ptr);
    // SAFETY: no strong handle is left, so nothing else reaches the value;
    // weak handles only touch the count words (`upgrade` sees the zero
    // strong count). The strong handles' weak count keeps the allocation
    // alive while the value drops, so the PHP destructors that drop may run
    // can clone, upgrade and drop weak handles to it freely. The `&mut` (and
    // the protected one of the drop glue) covers the value field only, never
    // the count words in front of it, as `Rc`'s covers only its value.
    unsafe { ManuallyDrop::drop(&mut (*ptr.as_ptr()).value) };
    drop(rest);
}

/// Releases the strong handles' collective weak count of the allocation at
/// `ptr`, whose value is gone, then drops its metadata.
///
/// # Safety
/// As for [`release_last`], once the value is dropped; this runs once per
/// allocation.
unsafe fn release_meta_and_weak(ptr: NonNull<RefBox>) {
    // SAFETY: the collective weak count keeps the allocation alive; a
    // reference to the count word only.
    let word = unsafe { &(*ptr.as_ptr()).weak };
    // The entry goes while the allocation still holds the address, so no
    // later reference can find it.
    let meta = if word.get() & HAS_META != 0 {
        word.set(word.get() & !HAS_META);
        meta_remove(addr_of(ptr))
    } else {
        None
    };
    // SAFETY: the strong handles' collective weak count, released exactly
    // once (a strong count never comes back from zero).
    unsafe { release_weak(ptr) };
    // The value first, then the metadata, as when both were fields of the
    // payload. Nothing in the metadata reaches the allocation without a weak
    // count of its own (no strong handle is left), so it may be freed by
    // now; dropping the metadata last, with the table released, leaks
    // nothing else if that drop panics.
    drop(meta);
}

/// Releases one weak count of the allocation at `ptr`, freeing the
/// allocation when it was the last.
///
/// # Safety
/// `ptr` must be a live reference allocation of which the caller owns one
/// weak count. When the count reaches zero the value must already have been
/// dropped, which holds because the strong handles' collective weak count is
/// released only after it ([`release_meta_and_weak`]).
unsafe fn release_weak(ptr: NonNull<RefBox>) {
    // SAFETY: the allocation is alive (caller contract); a reference to the
    // count word only, so this is fine while the value is being dropped.
    let word = unsafe { &(*ptr.as_ptr()).weak };
    // (`- 1` would borrow from the flag bit, not underflow.)
    debug_assert_ne!(word.get() & WEAK_COUNT, 0, "weak count released twice");
    let n = word.get() - 1;
    word.set(n);
    if n & WEAK_COUNT == 0 {
        debug_assert_eq!(n & HAS_META, 0, "metadata outlived its reference");
        // SAFETY: no handle of any kind is left and the value is dropped;
        // the box was allocated by `Box::new` in `PhpRef::new`, and
        // `ManuallyDrop` keeps `Box`'s drop glue off the value.
        drop(unsafe { Box::from_raw(ptr.as_ptr()) });
    }
}

/// A weak handle to a reference (the cycle collector's root buffer): it
/// does not keep the reference alive, and [`WeakRef::upgrade`] returns a
/// handle to it while it is.
///
/// A weak handle only touches the two count words, never the value, so it
/// may be used (cloned, dropped, upgraded) while the value is being dropped,
/// which PHP destructors running then may do through the root buffer.
pub(crate) struct WeakRef {
    ptr: NonNull<RefBox>,
}

impl WeakRef {
    #[inline]
    fn strong(&self) -> &Cell<u32> {
        // SAFETY: this handle owns one weak count, so the allocation is
        // alive; a reference to the count word only (see the type's doc).
        unsafe { &(*self.ptr.as_ptr()).strong }
    }

    #[inline]
    fn weak(&self) -> &Cell<u32> {
        // SAFETY: as in `strong`.
        unsafe { &(*self.ptr.as_ptr()).weak }
    }

    /// The reference, if it is still alive.
    #[must_use]
    pub(crate) fn upgrade(&self) -> Option<PhpRef> {
        let strong = self.strong();
        let n = strong.get();
        if n == 0 {
            return None;
        }
        if n == u32::MAX {
            std::process::abort();
        }
        strong.set(n + 1);
        Some(PhpRef {
            ptr: self.ptr,
            _owns: PhantomData,
        })
    }

    /// Number of strong handles to the reference (`0` once it is freed).
    #[must_use]
    pub(crate) fn strong_count(&self) -> usize {
        self.strong().get() as usize
    }

    /// The [`PhpRef::addr`] of the reference (valid as a key even after the
    /// reference is freed, while this handle exists).
    #[must_use]
    pub(crate) fn addr(&self) -> usize {
        addr_of(self.ptr)
    }
}

impl Clone for WeakRef {
    fn clone(&self) -> Self {
        inc_weak(self.weak());
        WeakRef { ptr: self.ptr }
    }
}

impl Drop for WeakRef {
    fn drop(&mut self) {
        // SAFETY: this handle owns one weak count.
        unsafe { release_weak(self.ptr) }
    }
}

impl fmt::Debug for WeakRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WeakRef({:#x})", self.addr())
    }
}

// ---------------------------------------------------------------------------
// The metadata side table
// ---------------------------------------------------------------------------

/// VM metadata by reference address.
type MetaMap = HashMap<usize, Rc<dyn Any>, FxBuildHasher>;

thread_local! {
    /// The metadata of the live references whose [`HAS_META`] bit is set,
    /// by address (a reference never leaves its thread). An entry goes with
    /// `set_meta(None)` or right after its reference's value, while the
    /// allocation still holds the address; the last one to go frees the
    /// table's buffer.
    ///
    /// `ManuallyDrop`: no thread-exit destructor. One would drop the
    /// metadata of references still alive at exit before the thread-locals
    /// holding those references, and leave the table unusable for their
    /// drops.
    static META: RefCell<ManuallyDrop<MetaMap>> = const {
        RefCell::new(ManuallyDrop::new(HashMap::with_hasher(FxBuildHasher)))
    };
}

/// The metadata of the reference at `addr`, whose [`HAS_META`] bit is set.
#[inline(never)]
fn meta_get(addr: usize) -> Option<Rc<dyn Any>> {
    META.with(|t| t.borrow().get(&addr).cloned())
}

/// Stores the metadata of the reference at `addr`. Returns the entry it
/// replaces, for the caller to drop once the table is released (dropping
/// metadata may run code that uses the table).
#[inline(never)]
fn meta_insert(addr: usize, meta: Rc<dyn Any>) -> Option<Rc<dyn Any>> {
    META.with(|t| t.borrow_mut().insert(addr, meta))
}

/// Removes the metadata of the reference at `addr`, returned as by
/// [`meta_insert`]. The last entry's removal frees the table's buffer.
#[inline(never)]
fn meta_remove(addr: usize) -> Option<Rc<dyn Any>> {
    META.with(|t| {
        let mut t = t.borrow_mut();
        let old = t.remove(&addr);
        if t.is_empty() {
            t.shrink_to_fit();
        }
        old
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Obj, PhpObject};
    use crate::ZStr;
    use std::collections::HashSet;
    use std::hash::BuildHasher;
    use std::mem::{align_of, offset_of, size_of};

    /// Entries and capacity of this thread's metadata table.
    fn meta_table() -> (usize, usize) {
        META.with(|t| {
            let t = t.borrow();
            (t.len(), t.capacity())
        })
    }

    /// php-src's `zend_reference` is 32 bytes (refcounted header 8, zval 16,
    /// sources 8), and so is ours: the counts (8) and the value cell (24),
    /// whose borrow flag takes the word of `sources` (the metadata is in the
    /// side table).
    #[test]
    fn reference_allocation_is_32_bytes() {
        assert_eq!(size_of::<PhpRef>(), 8);
        assert_eq!(size_of::<Option<PhpRef>>(), 8);
        assert_eq!(size_of::<WeakRef>(), 8);
        assert_eq!(size_of::<Option<WeakRef>>(), 8);
        assert_eq!(size_of::<RefCell<Value>>(), 24);
        assert_eq!(size_of::<RefBox>(), 32);
        assert_eq!(size_of::<RefInner>(), 32);
        assert_eq!(align_of::<RefBox>(), 8);
        assert_eq!(align_of::<RefInner>(), 8);
        assert_eq!(offset_of!(RefBox, weak), 4);
        assert_eq!(offset_of!(RefBox, value), 8);
    }

    #[test]
    fn handles_share_one_slot_and_count() {
        let r = PhpRef::new(Value::Int(1));
        assert_eq!(r.refcount(), 1);
        let r2 = r.clone();
        assert_eq!(r.refcount(), 2);
        assert!(PhpRef::ptr_eq(&r, &r2));
        assert_eq!(r.addr(), r2.addr());
        r2.set(Value::Int(5));
        assert_eq!(r.get().as_int(), Some(5));
        assert_eq!(r.val().as_int(), Some(5));
        drop(r2);
        assert_eq!(r.refcount(), 1);
        let other = PhpRef::new(Value::Int(0));
        assert!(!PhpRef::ptr_eq(&r, &other));
        assert_ne!(r.addr(), other.addr());
        // Storing a handle to itself keeps the value; another reference is
        // looked through, and a nested reference is flattened on creation.
        assert_eq!(r.set(Value::Reference(r.clone())).as_int(), Some(5));
        assert_eq!(r.refcount(), 1);
        assert_eq!(
            r.set(Value::Reference(PhpRef::new(Value::Int(9)))).as_int(),
            Some(5)
        );
        assert_eq!(r.get().as_int(), Some(9));
        let nested = PhpRef::new(Value::Reference(r.clone()));
        assert!(!PhpRef::ptr_eq(&nested, &r));
        assert_eq!(nested.get().as_int(), Some(9));
        assert_eq!(r.refcount(), 1);
    }

    #[test]
    fn borrows_are_checked() {
        let r = PhpRef::new(Value::Int(1));
        assert_eq!(format!("{r:?}"), "PhpRef(Int(1))");
        let v = Value::Reference(r.clone());
        assert_eq!(format!("{v:?}"), "Reference(Int(1))");
        {
            let guard = r.borrow();
            assert!(r.try_borrow().is_some());
            assert!(r.try_borrow_mut().is_none());
            assert_eq!(guard.as_int(), Some(1));
        }
        {
            let mut guard = r.borrow_mut();
            *guard = Value::Int(2);
            assert!(r.try_borrow().is_none());
            assert!(r.try_borrow_mut().is_none());
            assert_eq!(format!("{v:?}"), "Reference(<borrowed>)");
            assert_eq!(format!("{r:?}"), "PhpRef(<borrowed>)");
        }
        assert_eq!(r.get().as_int(), Some(2));
        assert!(r.try_borrow_mut().is_some());
    }

    #[test]
    fn meta_is_shared_by_handles_and_freed_with_the_reference() {
        let r = PhpRef::new(Value::Int(1));
        assert!(r.meta().is_none());
        // Clearing what is not there is a no-op.
        r.set_meta(None);
        assert_eq!(meta_table(), (0, 0));
        let m: Rc<dyn Any> = Rc::new(7u8);
        r.set_meta(Some(m.clone()));
        assert_eq!(Rc::strong_count(&m), 2);
        assert_eq!(meta_table().0, 1);
        let got = r.meta().expect("meta");
        assert!(Rc::ptr_eq(&got, &m));
        assert_eq!(got.downcast_ref::<u8>(), Some(&7));
        assert_eq!(Rc::strong_count(&m), 3);
        drop(got);
        // Reading leaves the entry in place.
        assert!(r.meta().is_some());
        assert_eq!(Rc::strong_count(&m), 2);
        // A clone of the handle sees the same entry; replacing drops the old.
        let r2 = r.clone();
        let m2: Rc<dyn Any> = Rc::new(8u8);
        r2.set_meta(Some(m2.clone()));
        assert_eq!(Rc::strong_count(&m), 1);
        assert_eq!(meta_table().0, 1);
        assert_eq!(
            r.meta().and_then(|x| x.downcast_ref::<u8>().copied()),
            Some(8)
        );
        r2.set_meta(None);
        assert!(r.meta().is_none());
        assert_eq!(Rc::strong_count(&m2), 1);
        // The last entry's removal gives the table's buffer back.
        assert_eq!(meta_table(), (0, 0));
        // Freeing the reference frees its metadata.
        r.set_meta(Some(m2.clone()));
        assert_eq!(Rc::strong_count(&m2), 2);
        drop((r, r2));
        assert_eq!(Rc::strong_count(&m2), 1);
        assert_eq!(meta_table(), (0, 0));
    }

    #[test]
    fn meta_entries_are_per_reference() {
        let byte = |r: &PhpRef| r.meta().and_then(|m| m.downcast_ref::<u8>().copied());
        let a = PhpRef::new(Value::Null);
        let b = PhpRef::new(Value::Null);
        let c = PhpRef::new(Value::Null);
        a.set_meta(Some(Rc::new(1u8)));
        b.set_meta(Some(Rc::new(2u8)));
        assert_eq!((byte(&a), byte(&b), byte(&c)), (Some(1), Some(2), None));
        assert_eq!(meta_table().0, 2);
        drop(a);
        assert_eq!((byte(&b), byte(&c)), (Some(2), None));
        assert_eq!(meta_table().0, 1);
        b.set_meta(None);
        assert_eq!(meta_table(), (0, 0));
        drop((b, c));
    }

    #[test]
    fn weak_count_and_meta_bit_share_a_word() {
        let word = Cell::new(HAS_META | 5);
        inc_weak(&word);
        assert_eq!(word.get(), HAS_META | 6);
        let word = Cell::new(WEAK_COUNT - 1);
        inc_weak(&word);
        assert_eq!(word.get(), WEAK_COUNT);
    }

    /// Counts drops of a value held by a reference.
    struct Counted(Rc<Cell<u32>>);
    impl PhpObject for Counted {
        fn handle(&self) -> u32 {
            1
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Counted")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn last_handle_frees_the_value() {
        let drops = Rc::new(Cell::new(0));
        let r = PhpRef::new(Value::Object(Obj::new(Counted(drops.clone()))));
        let copies: Vec<PhpRef> = (0..5).map(|_| r.clone()).collect();
        assert_eq!(r.refcount(), 6);
        drop(copies);
        assert_eq!(r.refcount(), 1);
        assert_eq!(drops.get(), 0);
        drop(r);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn weak_handles_do_not_keep_the_value_alive() {
        let drops = Rc::new(Cell::new(0));
        let r = PhpRef::new(Value::Object(Obj::new(Counted(drops.clone()))));
        let w = r.downgrade();
        assert_eq!(w.strong_count(), 1);
        assert_eq!(w.addr(), r.addr());
        assert_eq!(format!("{w:?}"), format!("WeakRef({:#x})", r.addr()));
        let r2 = w.upgrade().expect("alive");
        assert!(PhpRef::ptr_eq(&r, &r2));
        assert_eq!(r.refcount(), 2);
        drop(r2);
        assert_eq!(w.strong_count(), 1);
        drop(r);
        // The value went with the last strong handle; the allocation stays
        // for the weak handles, which now see a dead reference.
        assert_eq!(drops.get(), 1);
        assert_eq!(w.strong_count(), 0);
        assert!(w.upgrade().is_none());
        let w2 = w.clone();
        drop(w);
        assert_eq!(w2.strong_count(), 0);
        assert!(w2.upgrade().is_none());
        drop(w2);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn weak_handles_outlived_by_the_reference() {
        let drops = Rc::new(Cell::new(0));
        let r = PhpRef::new(Value::Object(Obj::new(Counted(drops.clone()))));
        let w = r.downgrade();
        let w2 = w.clone();
        drop(w);
        drop(w2);
        assert_eq!(drops.get(), 0);
        assert_eq!(r.get().type_name(), "Counted");
        drop(r);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn weak_handles_leave_the_meta_bit_alone() {
        let drops = Rc::new(Cell::new(0));
        let r = PhpRef::new(Value::Object(Obj::new(Counted(drops.clone()))));
        let m: Rc<dyn Any> = Rc::new(3u8);
        r.set_meta(Some(m.clone()));
        let w = r.downgrade();
        let w2 = w.clone();
        drop(w);
        let w3 = r.downgrade();
        assert!(r.meta().is_some_and(|x| Rc::ptr_eq(&x, &m)));
        // Freeing the value takes the metadata with it, though weak handles
        // keep the allocation.
        drop(r);
        assert_eq!(drops.get(), 1);
        assert_eq!(Rc::strong_count(&m), 1);
        assert_eq!(meta_table(), (0, 0));
        assert_eq!((w2.strong_count(), w3.strong_count()), (0, 0));
        assert!(w3.upgrade().is_none());
        drop((w2, w3));
    }

    /// A value whose drop upgrades, clones and drops weak handles to the
    /// reference being freed (as PHP destructors may through the root
    /// buffer).
    struct TouchesWeak(Cell<Option<WeakRef>>, Rc<Cell<bool>>);
    impl PhpObject for TouchesWeak {
        fn handle(&self) -> u32 {
            2
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("TouchesWeak")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    impl Drop for TouchesWeak {
        fn drop(&mut self) {
            if let Some(w) = self.0.take() {
                assert_eq!(w.strong_count(), 0);
                assert!(w.upgrade().is_none());
                let w2 = w.clone();
                drop(w);
                assert!(w2.upgrade().is_none());
                drop(w2);
                self.1.set(true);
            }
        }
    }

    #[test]
    fn weak_handles_usable_while_the_value_drops() {
        let ran = Rc::new(Cell::new(false));
        let r = PhpRef::new(Value::Null);
        let w = r.downgrade();
        r.set(Value::Object(Obj::new(TouchesWeak(
            Cell::new(Some(w)),
            ran.clone(),
        ))));
        // With metadata, too: it is dropped after the value.
        r.set_meta(Some(Rc::new(0u8)));
        drop(r);
        assert!(ran.get());
        assert_eq!(meta_table(), (0, 0));
    }

    /// Logs its drop (as a value held by a reference, or as metadata).
    struct Logs(&'static str, Rc<RefCell<Vec<&'static str>>>);
    impl PhpObject for Logs {
        fn handle(&self) -> u32 {
            3
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Logs")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    impl Drop for Logs {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    #[test]
    fn value_is_freed_before_its_meta() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let r = PhpRef::new(Value::Object(Obj::new(Logs("value", log.clone()))));
        r.set_meta(Some(Rc::new(Logs("meta", log.clone()))));
        drop(r);
        assert_eq!(*log.borrow(), ["value", "meta"]);
    }

    /// Metadata whose drop sets and reads another reference's metadata.
    struct SetsOther(PhpRef, Rc<Cell<u32>>);
    impl Drop for SetsOther {
        fn drop(&mut self) {
            self.0.set_meta(Some(Rc::new(1u8)));
            assert!(self.0.meta().is_some());
            self.0.set_meta(None);
            self.1.set(self.1.get() + 1);
        }
    }

    #[test]
    fn metadata_drop_may_use_the_table() {
        let drops = Rc::new(Cell::new(0));
        let other = PhpRef::new(Value::Null);
        let r = PhpRef::new(Value::Null);
        r.set_meta(Some(Rc::new(SetsOther(other.clone(), drops.clone()))));
        // Replaced,
        r.set_meta(Some(Rc::new(SetsOther(other.clone(), drops.clone()))));
        assert_eq!(drops.get(), 1);
        // cleared,
        r.set_meta(None);
        assert_eq!(drops.get(), 2);
        // and freed with the reference.
        r.set_meta(Some(Rc::new(SetsOther(other.clone(), drops.clone()))));
        drop(r);
        assert_eq!(drops.get(), 3);
        assert!(other.meta().is_none());
        assert_eq!(other.refcount(), 1);
        assert_eq!(meta_table(), (0, 0));
    }

    #[test]
    fn addresses_spread_over_buckets() {
        // `rustc-hash`'s FxHasher multiplies, then rotates the product's
        // best-mixed high bits down to the low ones that pick the bucket.
        let s = FxBuildHasher;
        // 64 allocations 32 bytes apart (one allocator bin), as indices into
        // a 64-bucket table: 57 distinct (a bare product gives 2, its low
        // five bits being zero).
        let buckets: HashSet<u64> = (0..64usize)
            .map(|i| s.hash_one(0x7f12_3456_7000 + i * 32) & 63)
            .collect();
        assert!(buckets.len() >= 48, "{}", buckets.len());
    }
}
