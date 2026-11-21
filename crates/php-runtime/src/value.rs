// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The PHP value (`zval`) and its refcounted payload types.
//!
//! Copying a [`Value`] (`clone`) is always cheap: strings, arrays, objects,
//! resources and references are reference-counted handles. Arrays are
//! copy-on-write: mutating an array through [`crate::ZArray::make_mut`]
//! separates it from other holders, so `$b = $a; $b[] = 1;` leaves `$a`
//! untouched.
//!
//! PHP references (`&`) are modelled by [`PhpRef`], a shared mutable cell. A
//! variable or array element that is a reference holds `Value::Reference`.

use crate::array::ZArray;
use crate::string::ZStr;
use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefCell, RefMut, UnsafeCell};
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};
use std::rc::{Rc, Weak};

/// A PHP value (`zval`).
#[derive(Clone, Default)]
pub enum Value {
    /// `null`.
    #[default]
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// A double-precision float.
    Float(f64),
    /// A binary string.
    Str(ZStr),
    /// An ordered hash table (copy-on-write).
    Array(ZArray),
    /// An object handle (identity semantics).
    Object(Obj),
    /// A resource handle.
    Resource(Resource),
    /// A PHP reference (`&`): a shared, mutable slot.
    Reference(PhpRef),
}

impl Value {
    /// Creates a string value.
    #[must_use]
    pub fn string(s: impl Into<ZStr>) -> Self {
        Value::Str(s.into())
    }

    /// Creates an empty array value.
    #[must_use]
    pub fn empty_array() -> Self {
        Value::Array(ZArray::new())
    }

    /// The type name used in error messages (`zend_zval_type_name`): `null`,
    /// `bool`, `int`, `float`, `string`, `array`, `resource`, or the class name
    /// for objects. References are looked through.
    #[must_use]
    pub fn type_name(&self) -> String {
        match self {
            Value::Object(o) => o.class_name().to_string_lossy(),
            Value::Reference(r) => r.get().type_name(),
            other => other.simple_type_name().to_string(),
        }
    }

    /// Like [`Value::type_name`] but distinguishes `true`/`false`
    /// (`zend_zval_value_name`).
    #[must_use]
    pub fn value_name(&self) -> String {
        match self {
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            Value::Reference(r) => r.get().value_name(),
            other => other.type_name(),
        }
    }

    /// The type name without class names: objects are `"object"`
    /// (`zend_get_type_by_const`).
    #[must_use]
    pub fn simple_type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Resource(_) => "resource",
            Value::Reference(r) => r.get().simple_type_name(),
        }
    }

    /// The legacy type name returned by `gettype()` (`"integer"`, `"double"`,
    /// `"boolean"`, `"NULL"`, `"resource (closed)"`, ...).
    #[must_use]
    pub fn legacy_type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "double",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Resource(r) => {
                if r.type_name().is_some() {
                    "resource"
                } else {
                    "resource (closed)"
                }
            }
            Value::Reference(r) => r.get().legacy_type_name(),
        }
    }

    /// Whether the value is `null` (references are looked through).
    #[must_use]
    pub fn is_null(&self) -> bool {
        match self {
            Value::Null => true,
            Value::Reference(r) => r.borrow().is_null(),
            _ => false,
        }
    }

    /// Whether this value is a [`Value::Reference`].
    #[must_use]
    pub fn is_reference(&self) -> bool {
        matches!(self, Value::Reference(_))
    }

    /// Returns the dereferenced value (a cheap clone of the referenced value
    /// for references, a clone of `self` otherwise).
    #[inline]
    #[must_use]
    pub fn deref(&self) -> Value {
        match self {
            Value::Reference(r) => r.get(),
            other => other.clone(),
        }
    }

    /// Unwraps a reference in place (`ZVAL_DEREF` + copy): afterwards `self`
    /// is never a `Value::Reference`.
    pub fn unwrap_reference(&mut self) {
        if let Value::Reference(r) = self {
            let inner = r.get();
            *self = inner;
        }
    }

    /// Turns `self` into a reference (`ZVAL_MAKE_REF`) and returns it. If it is
    /// already a reference, returns a handle to the existing one.
    pub fn make_ref(&mut self) -> PhpRef {
        if let Value::Reference(r) = self {
            return r.clone();
        }
        let r = PhpRef::new(std::mem::take(self));
        *self = Value::Reference(r.clone());
        r
    }

    /// Returns the array if this is an array (not looking through references).
    #[must_use]
    pub fn as_array(&self) -> Option<&ZArray> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Returns the string if this is a string (not looking through references).
    #[must_use]
    pub fn as_str(&self) -> Option<&ZStr> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the integer if this is an int.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the float if this is a float.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Returns the object if this is an object.
    #[must_use]
    pub fn as_object(&self) -> Option<&Obj> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Returns a mutable array, separating it (copy-on-write) if shared.
    pub fn as_array_mut(&mut self) -> Option<&mut crate::array::HashTable> {
        match self {
            Value::Array(a) => Some(a.make_mut()),
            _ => None,
        }
    }

    /// Whether the value is a scalar (null excluded): bool, int, float, string.
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
        )
    }

    /// Whether the value is plain (null or scalar): null, bool, int, float, string.
    #[inline(always)]
    #[must_use]
    pub fn is_plain(&self) -> bool {
        matches!(
            self,
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
        )
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("Null"),
            Value::Bool(b) => write!(f, "Bool({b})"),
            Value::Int(i) => write!(f, "Int({i})"),
            Value::Float(x) => write!(f, "Float({x:?})"),
            Value::Str(s) => write!(f, "Str({s:?})"),
            Value::Array(a) => write!(f, "Array({a:?})"),
            Value::Object(o) => write!(f, "{o:?}"),
            Value::Resource(r) => write!(f, "{r:?}"),
            Value::Reference(r) => match r.try_borrow() {
                Some(v) => write!(f, "Reference({:?})", &*v),
                None => f.write_str("Reference(<borrowed>)"),
            },
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i64::from(i))
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<ZStr> for Value {
    fn from(s: ZStr) -> Self {
        Value::Str(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(ZStr::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(ZStr::from(s))
    }
}

impl From<&[u8]> for Value {
    fn from(s: &[u8]) -> Self {
        Value::Str(ZStr::from(s))
    }
}

impl From<ZArray> for Value {
    fn from(a: ZArray) -> Self {
        Value::Array(a)
    }
}

impl From<Obj> for Value {
    fn from(o: Obj) -> Self {
        Value::Object(o)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Self {
        o.map_or(Value::Null, Into::into)
    }
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

pub use crate::reference::PhpRef;

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// The interface the VM implements for its object representation.
///
/// The runtime only needs an object's identity (its handle number, as printed
/// by `var_dump` as `#N`), its class name and a way for the VM to downcast.
/// Everything else (properties, casts, comparison) goes through
/// [`crate::RuntimeContext`].
pub trait PhpObject: Any {
    /// The object handle (`Z_OBJ_HANDLE`), unique among live objects.
    fn handle(&self) -> u32;
    /// The class name (`get_class_name` handler).
    fn class_name(&self) -> ZStr;
    /// Upcast for downcasting to the VM's concrete type.
    fn as_any(&self) -> &dyn Any;
    /// Marks the allocation [`Obj::new`] makes, so that [`Obj::from_rc`]
    /// can recognise its `Rc`. Implementors keep the default.
    #[doc(hidden)]
    fn obj_cell_tag(&self) -> Option<ObjCellTag> {
        None
    }
    /// The last handle of the object, `this`, is being dropped
    /// (`zend_objects_store_del` on `GC_REFCOUNT == 0`): called by
    /// [`Obj::drop`] before the count is decremented. The object may keep
    /// itself alive by cloning `this` (php-src keeps a destructing object
    /// at refcount 1; the VM queues the handle for its `__destruct` call);
    /// otherwise it is freed right after. Implementors without destructors
    /// keep the default.
    #[doc(hidden)]
    fn release_last(&self, this: &Obj) {
        let _ = this;
    }
}

/// The claim of an [`Obj`]'s allocation to be one, returned by
/// [`PhpObject::obj_cell_tag`]: the `TypeId` of its `ObjCell<T>`. Only
/// php-runtime can make one, and [`Obj::from_rc`] checks the claim against
/// the object's real dynamic type (`Any::type_id` through the vtable, which
/// no implementor can fake), so a tag forwarded from another object, or a
/// stale one, is rejected.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct ObjCellTag(TypeId);

/// The header in front of every object payload (`zend_object`'s
/// `handlers` pointer: what a thin handle needs to reach the object's
/// vtable).
#[repr(C)]
struct ObjHeader {
    /// The allocation's own `Rc`, written once by [`Obj::new`] right after
    /// the allocation and never again. It owns no strong count (it is never
    /// dropped: `MaybeUninit` has no drop glue); it lends [`Obj::rc`] a real
    /// `&Rc<dyn PhpObject>` and carries the vtable (drop, size, methods).
    this: UnsafeCell<MaybeUninit<Rc<dyn PhpObject>>>,
    /// `TypeId` of the payload (or of its family, [`Obj::new_in_family`]):
    /// [`Obj::downcast_ref`] to that type is one comparison instead of two
    /// virtual calls.
    type_id: TypeId,
}

/// One object allocation: header, then the payload (`repr(C)`, so the
/// header is at offset 0 and the payload at
/// [`payload_offset`]`::<T>()`).
#[repr(C)]
struct ObjCell<T> {
    header: ObjHeader,
    value: T,
}

/// Offset of the payload in an `ObjCell<T>` (`repr(C)` field placement).
const fn payload_offset<T>() -> usize {
    std::mem::size_of::<ObjHeader>().next_multiple_of(std::mem::align_of::<T>())
}

impl<T: PhpObject> PhpObject for ObjCell<T> {
    #[inline]
    fn handle(&self) -> u32 {
        self.value.handle()
    }
    #[inline]
    fn class_name(&self) -> ZStr {
        self.value.class_name()
    }
    #[inline]
    fn as_any(&self) -> &dyn Any {
        self.value.as_any()
    }
    fn obj_cell_tag(&self) -> Option<ObjCellTag> {
        Some(ObjCellTag(TypeId::of::<Self>()))
    }
    #[inline]
    fn release_last(&self, this: &Obj) {
        self.value.release_last(this);
    }
}

/// An `Rc<dyn PhpObject>` that was not made by [`Obj::new`], adopted by
/// [`Obj::from_rc`] as the payload of a new object handle.
struct ForeignObj(Rc<dyn PhpObject>);

impl PhpObject for ForeignObj {
    fn handle(&self) -> u32 {
        self.0.handle()
    }
    fn class_name(&self) -> ZStr {
        self.0.class_name()
    }
    fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }
    fn release_last(&self, this: &Obj) {
        self.0.release_last(this);
    }
}

/// A handle to a PHP object. Clones share the object; equality is identity.
///
/// The handle is one thin pointer (8 bytes, with a niche for `Option`), like
/// the `zend_object *` in a zval, so that [`Value`] is 16 bytes. It points at
/// an `Rc` allocation whose header holds the allocation's own
/// `Rc<dyn PhpObject>` (the vtable) and the payload's `TypeId`; the payload
/// follows. Each handle owns one strong count of that `Rc`, so
/// [`Obj::refcount`], [`Obj::rc`] and `Weak` handles made from it behave as
/// they did when `Obj` was an `Rc<dyn PhpObject>`. Like `Rc`, it is neither
/// `Send` nor `Sync`.
pub struct Obj {
    ptr: NonNull<ObjHeader>,
    _owns: PhantomData<Rc<dyn PhpObject>>,
}

impl Obj {
    /// Wraps a VM object (one allocation: header + `object`).
    pub fn new<T: PhpObject>(object: T) -> Self {
        Obj::with_type_id(object, TypeId::of::<T>())
    }

    /// Creates a dummy plain object (useful for tests and serialization mock contexts).
    #[must_use]
    pub fn dummy(class: impl Into<ZStr>) -> Self {
        crate::plain::PlainObject::create(class, crate::array::ZArray::new())
    }

    /// Wraps a VM object whose payload `T` starts with an `F`: the payload
    /// is a member of `F`'s family, and [`Obj::downcast_ref`] and
    /// [`Obj::payload_ptr`] for `F` reach that `F` part with the one type
    /// check every other payload type gets. php-src's objects are such a
    /// family: a `zend_object` whose `properties_table` continues past the
    /// struct, sized for the class, in the same allocation.
    ///
    /// # Safety
    /// `T` must hold a valid `F` at offset 0 for as long as the object
    /// lives (a `#[repr(C)]` struct with an `F` first field, say), with
    /// `F`'s alignment (checked at compile time, with the sizes).
    /// [`Obj::downcast_ref`] for `T` itself does not recognise the payload
    /// by its header (it looks for `T` through [`PhpObject::as_any`]).
    #[must_use]
    pub unsafe fn new_in_family<T: PhpObject, F: 'static>(object: T) -> Self {
        const {
            assert!(payload_offset::<T>() == payload_offset::<F>());
            assert!(std::mem::size_of::<T>() >= std::mem::size_of::<F>());
        }
        Obj::with_type_id(object, TypeId::of::<F>())
    }

    /// [`Obj::new_in_family`] with the payload built in place: `init` gets
    /// a pointer to the (uninitialized) payload inside the new allocation
    /// and writes it there, so a large payload (an object with its inline
    /// property slots) is not first assembled on the stack and then copied
    /// into the heap (`zend_objects_new` initializes the `zend_object` it
    /// allocated). The pointer carries the provenance of the whole
    /// allocation, as [`Obj::payload_ptr`]'s does, and may be kept (the
    /// payload's address never changes).
    ///
    /// # Safety
    /// As for [`Obj::new_in_family`]; in addition `init` must leave a valid
    /// `T` at the pointer when it returns. If it panics instead, the
    /// allocation is freed without dropping anything in it.
    #[inline]
    #[must_use]
    pub unsafe fn new_in_family_with<T: PhpObject, F: 'static>(
        init: impl FnOnce(NonNull<T>),
    ) -> Self {
        const {
            assert!(payload_offset::<T>() == payload_offset::<F>());
            assert!(std::mem::size_of::<T>() >= std::mem::size_of::<F>());
        }
        let uninit: Rc<MaybeUninit<ObjCell<T>>> = Rc::new_uninit();
        // `Rc::as_ptr` derives the pointer without creating a reference:
        // it keeps the provenance of the `Rc`'s own pointer (the whole
        // allocation), which `assume_init` and `into_raw` below keep too,
        // so pointers handed to `init` stay valid for the object's life.
        let cell = Rc::as_ptr(&uninit).cast_mut().cast::<ObjCell<T>>();
        // SAFETY: the allocation is fresh and unshared (no reference to it
        // exists); `cell` points at its `ObjCell<T>`, the field
        // projections stay inside it, and `addr_of_mut!` makes no
        // reference. `init` writes the payload (caller contract), so the
        // whole cell is initialized for `assume_init` (`this` is
        // `MaybeUninit` and written below, as in `with_type_id`).
        unsafe {
            ptr::addr_of_mut!((*cell).header).write(ObjHeader {
                this: UnsafeCell::new(MaybeUninit::uninit()),
                type_id: TypeId::of::<F>(),
            });
            init(NonNull::new_unchecked(ptr::addr_of_mut!((*cell).value)));
            let rc: Rc<dyn PhpObject> = uninit.assume_init();
            let raw = Rc::into_raw(rc);
            let header = Obj::header_of(raw);
            (*(*header).this.get())
                .as_mut_ptr()
                .write(Rc::from_raw(raw));
            Obj::from_header(header)
        }
    }

    /// [`Obj::new`] with the type the header records for
    /// [`Obj::downcast_ref`] (`T`'s own, or its family's).
    fn with_type_id<T: PhpObject>(object: T, type_id: TypeId) -> Self {
        let rc: Rc<dyn PhpObject> = Rc::new(ObjCell {
            header: ObjHeader {
                this: UnsafeCell::new(MaybeUninit::uninit()),
                type_id,
            },
            value: object,
        });
        let raw = Rc::into_raw(rc);
        let header = Obj::header_of(raw);
        // SAFETY: `raw` points at the live `ObjCell<T>` whose one strong
        // count the new handle takes over. Nothing has seen `this` yet, and
        // writing through `UnsafeCell::get` is allowed from a shared
        // pointer. The `Rc` written is rebuilt from `into_raw`'s own pointer
        // and is never dropped, so it adds no count.
        unsafe {
            (*(*header).this.get())
                .as_mut_ptr()
                .write(Rc::from_raw(raw));
            Obj::from_header(header)
        }
    }

    /// The header of the `ObjCell` at `cell`, the data pointer of its
    /// `Rc<dyn PhpObject>`.
    #[inline]
    // The pointee is an `ObjCell<T>`: `repr(C)` with the header first, so
    // at least as aligned as the header. Clippy only sees the `dyn`.
    #[allow(clippy::cast_ptr_alignment)]
    fn header_of(cell: *const dyn PhpObject) -> *const ObjHeader {
        cell.cast::<ObjHeader>()
    }

    /// # Safety
    /// `header` must be [`Obj::header_of`] a live `ObjCell` (from
    /// `Rc::into_raw` of its `Rc<dyn PhpObject>`) whose `this` is written,
    /// and the caller must hand one strong count of it to the new handle.
    #[inline]
    unsafe fn from_header(header: *const ObjHeader) -> Self {
        Obj {
            // SAFETY: `Rc::into_raw` never returns null (caller contract).
            ptr: unsafe { NonNull::new_unchecked(header.cast_mut()) },
            _owns: PhantomData,
        }
    }

    #[inline]
    fn header(&self) -> &ObjHeader {
        // SAFETY: this handle owns a strong count, so the allocation and
        // its header are alive; the header is only written by `Obj::new`
        // before the first handle exists.
        unsafe { self.ptr.as_ref() }
    }

    /// Wraps an already reference-counted object.
    ///
    /// An `Rc` obtained from an [`Obj`] (by [`Obj::rc`], or by upgrading a
    /// `Weak` downgraded from it) becomes a handle to that same object (same
    /// identity and refcount). Any other `Rc` is adopted as the payload of a
    /// new object: create VM objects with [`Obj::new`] so that their `Rc`s
    /// keep their identity.
    #[must_use]
    pub fn from_rc(rc: Rc<dyn PhpObject>) -> Self {
        // `Any::type_id` through the vtable (not `rc.type_id()`, which is
        // the `Rc`'s own): the object's dynamic type.
        let dynamic = Any::type_id(&*rc);
        match rc.obj_cell_tag() {
            Some(ObjCellTag(cell)) if cell == dynamic => {
                let raw = Rc::into_raw(rc);
                // SAFETY: the object's dynamic type is an `ObjCell<T>`,
                // which only `Obj::new` makes (writing `this`), so `rc` is
                // that cell's `Rc<dyn PhpObject>`; its strong count moves
                // into the handle.
                unsafe { Obj::from_header(Obj::header_of(raw)) }
            }
            _ => Obj::new(ForeignObj(rc)),
        }
    }

    /// The object's own reference-counted handle (`Rc::strong_count` of it
    /// is [`Obj::refcount`]; `Rc::downgrade` of it gives a weak handle that
    /// [`Obj::from_rc`] turns back into this object).
    #[inline]
    #[must_use]
    pub fn rc(&self) -> &Rc<dyn PhpObject> {
        // SAFETY: `this` was written by `Obj::new` before any handle existed
        // and is never written again; the allocation is alive (see
        // `header`).
        unsafe { (*self.header().this.get()).assume_init_ref() }
    }

    /// A weak handle to the object (see [`ObjWeak`]).
    #[must_use]
    pub fn downgrade(&self) -> ObjWeak {
        ObjWeak(Rc::downgrade(self.rc()))
    }

    /// The object handle number.
    #[inline]
    #[must_use]
    pub fn handle(&self) -> u32 {
        self.rc().handle()
    }

    /// The class name.
    #[must_use]
    pub fn class_name(&self) -> ZStr {
        self.rc().class_name()
    }

    /// Downcasts to the VM's concrete object type (or to the family type of
    /// an object made by [`Obj::new_in_family`]).
    #[inline(always)]
    #[allow(clippy::inline_always)] // one `TypeId` compare on every property access
    #[must_use]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        if self.header().type_id == TypeId::of::<T>() {
            // SAFETY: the payload's type is `T`, or it starts with a `T`
            // and is aligned like one (`Obj::new_in_family`), so a `T` sits
            // at `payload_offset::<T>()` of the allocation (alive while this
            // handle is); only shared references to the payload are ever
            // made.
            Some(unsafe { &(*self.ptr.as_ptr().cast::<ObjCell<T>>()).value })
        } else {
            self.downcast_slow::<T>()
        }
    }

    /// [`Obj::downcast_ref`] of a payload whose header records another
    /// type: through [`PhpObject::as_any`] (two virtual calls). Out of
    /// line, so that the header check inlines into every caller (the VM's
    /// `object::data` on each property access).
    #[cold]
    #[inline(never)]
    fn downcast_slow<T: 'static>(&self) -> Option<&T> {
        self.rc().as_any().downcast_ref::<T>()
    }

    /// The payload, as a raw pointer to its `F` part, if its type or
    /// family ([`Obj::new_in_family`]) is `F`. Unlike the reference
    /// [`Obj::downcast_ref`] returns, which covers the `F` part only, the
    /// pointer may be used to reach the rest of the payload (data a family
    /// member keeps after the `F` part, inside `UnsafeCell`) while the
    /// object is alive, except from inside the payload's own `Drop`: the
    /// drop glue then holds an exclusive reference to the whole cell.
    #[inline]
    #[must_use]
    pub fn payload_ptr<F: 'static>(&self) -> Option<NonNull<F>> {
        if self.header().type_id != TypeId::of::<F>() {
            return None;
        }
        // SAFETY: as in `downcast_ref`, the allocation holds an `F` at
        // `payload_offset::<F>()`, so the field projection stays in
        // bounds; `addr_of!` makes no reference, so the pointer keeps the
        // provenance of the handle's own pointer (the whole allocation).
        let p = unsafe { ptr::addr_of!((*self.ptr.as_ptr().cast::<ObjCell<F>>()).value) };
        NonNull::new(p.cast_mut())
    }

    /// Whether two handles denote the same object (`===`).
    #[inline]
    #[must_use]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    /// Number of handles to the object (`GC_REFCOUNT`).
    #[must_use]
    pub fn refcount(&self) -> usize {
        Rc::strong_count(self.rc())
    }

    /// An address usable as an identity key. It is the data address of
    /// [`Obj::rc`] (and of `Weak`s downgraded from it, `Weak::as_ptr`).
    #[inline]
    #[must_use]
    pub fn addr(&self) -> usize {
        self.ptr.as_ptr().cast::<u8>() as usize
    }

    /// The [`Obj::addr`] of the object whose payload is `payload`, for a
    /// payload that knows only itself (its `Drop`). Meaningful only for a
    /// payload placed by [`Obj::new`].
    #[must_use]
    pub fn addr_of_payload<T: PhpObject>(payload: &T) -> usize {
        (ptr::from_ref(payload).cast::<u8>() as usize).wrapping_sub(payload_offset::<T>())
    }
}

impl Clone for Obj {
    #[inline]
    fn clone(&self) -> Self {
        std::mem::forget(Rc::clone(self.rc()));
        Obj {
            ptr: self.ptr,
            _owns: PhantomData,
        }
    }
}

impl Drop for Obj {
    #[inline]
    fn drop(&mut self) {
        let rc = self.rc();
        if Rc::strong_count(rc) == 1 {
            // The object's last handle: `zend_objects_store_del`. The object
            // may clone `this` (a pending destructor keeps it alive at
            // count 1, as php-src's `GC_SET_REFCOUNT(obj, 1)` does), in
            // which case the decrement below leaves it alive.
            rc.release_last(self);
        }
        // SAFETY: this handle owns one strong count; the bitwise copy of the
        // allocation's own `Rc` is an `Rc` owning exactly that count, and
        // dropping it releases it (and frees the object if it was the last).
        // `self` is not used afterwards.
        unsafe { drop(ptr::read(self.rc())) };
    }
}

impl PartialEq for Obj {
    fn eq(&self, other: &Self) -> bool {
        Obj::ptr_eq(self, other)
    }
}

impl fmt::Debug for Obj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Object({}#{})", self.class_name(), self.handle())
    }
}

/// A weak handle to an [`Obj`]: it does not keep the object alive, and
/// [`ObjWeak::upgrade`] returns the same object (identity and refcount)
/// while it is.
///
/// It is the allocation's own `Weak<dyn PhpObject>` (two words), not a thin
/// pointer that would re-read the header: weak handles are used while their
/// object's payload is being dropped (the VM's object store and destructor
/// revival replace, drop and upgrade the weak handle of the dying object
/// from inside `Drop`), and the drop glue holds an exclusive reference to
/// the whole cell, header included, for as long as it runs. `Weak` only
/// touches the counts in front of the cell, which is allowed then (Miri,
/// `payload_drop_may_create_objects_and_drop_weaks`).
#[derive(Clone)]
pub struct ObjWeak(Weak<dyn PhpObject>);

impl ObjWeak {
    /// The object, if it is still alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<Obj> {
        let rc = self.0.upgrade()?;
        let raw = Rc::into_raw(rc);
        // SAFETY: an `ObjWeak` is only made by `Obj::downgrade`, so `rc` is
        // the `Rc<dyn PhpObject>` of an `ObjCell` made by `Obj::new` (its
        // `this` is written); the strong count `upgrade` took moves into
        // the handle.
        Some(unsafe { Obj::from_header(Obj::header_of(raw)) })
    }

    /// Number of strong handles to the object (`0` once it is freed).
    #[must_use]
    pub fn strong_count(&self) -> usize {
        self.0.strong_count()
    }

    /// The [`Obj::addr`] of the object (valid as a key even after the
    /// object is freed, while this handle exists).
    #[must_use]
    pub fn addr(&self) -> usize {
        self.0.as_ptr().cast::<u8>() as usize
    }
}

impl fmt::Debug for ObjWeak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjWeak({:#x})", self.addr())
    }
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

struct ResourceInner {
    handle: i64,
    type_name: RefCell<Option<ZStr>>,
    data: RefCell<Option<Box<dyn Any>>>,
}

/// A PHP resource (`zend_resource`): a numbered handle with a type name and
/// an opaque payload owned by the extension that created it.
#[derive(Clone)]
pub struct Resource(Rc<ResourceInner>);

thread_local! {
    static NEXT_RESOURCE_ID: Cell<i64> = const { Cell::new(1) };
}

impl Resource {
    /// Creates a resource with an explicit handle number.
    #[must_use]
    pub fn with_handle(
        handle: i64,
        type_name: impl Into<ZStr>,
        data: Option<Box<dyn Any>>,
    ) -> Self {
        Resource(Rc::new(ResourceInner {
            handle,
            type_name: RefCell::new(Some(type_name.into())),
            data: RefCell::new(data),
        }))
    }

    /// Creates a resource with the next free handle number of this thread.
    #[must_use]
    pub fn new(type_name: impl Into<ZStr>, data: Option<Box<dyn Any>>) -> Self {
        let handle = NEXT_RESOURCE_ID.with(|c| {
            let h = c.get();
            c.set(h + 1);
            h
        });
        Self::with_handle(handle, type_name, data)
    }

    /// Sets the next handle number allocated by [`Resource::new`].
    pub fn set_next_handle(handle: i64) {
        NEXT_RESOURCE_ID.with(|c| c.set(handle));
    }

    /// The next handle number [`Resource::new`] allocates.
    #[must_use]
    pub fn next_handle() -> i64 {
        NEXT_RESOURCE_ID.with(Cell::get)
    }

    /// The resource handle number (`Resource id #N`).
    #[must_use]
    pub fn handle(&self) -> i64 {
        self.0.handle
    }

    /// The type name, or `None` once closed (printed as `Unknown`).
    #[must_use]
    pub fn type_name(&self) -> Option<ZStr> {
        self.0.type_name.borrow().clone()
    }

    /// Closes the resource: drops the payload and forgets the type.
    pub fn close(&self) -> Option<Box<dyn Any>> {
        *self.0.type_name.borrow_mut() = None;
        self.0.data.borrow_mut().take()
    }

    /// Whether the resource has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0.type_name.borrow().is_none()
    }

    /// Borrows the payload.
    #[must_use]
    pub fn data(&self) -> Ref<'_, Option<Box<dyn Any>>> {
        self.0.data.borrow()
    }

    /// Mutably borrows the payload.
    #[must_use]
    pub fn data_mut(&self) -> RefMut<'_, Option<Box<dyn Any>>> {
        self.0.data.borrow_mut()
    }

    /// Whether two handles denote the same resource.
    #[must_use]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
}

impl fmt::Debug for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Resource(#{} {})",
            self.handle(),
            self.type_name()
                .map_or_else(|| "Unknown".to_string(), |t| t.to_string_lossy())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestObj(u32);
    impl PhpObject for TestObj {
        fn handle(&self) -> u32 {
            self.0
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Foo")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn type_names() {
        assert_eq!(Value::Null.type_name(), "null");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Bool(true).value_name(), "true");
        assert_eq!(Value::Int(1).type_name(), "int");
        assert_eq!(Value::Float(1.0).type_name(), "float");
        assert_eq!(Value::from("x").type_name(), "string");
        assert_eq!(Value::empty_array().type_name(), "array");
        let o = Obj::new(TestObj(3));
        assert_eq!(Value::Object(o.clone()).type_name(), "Foo");
        assert_eq!(Value::Object(o).simple_type_name(), "object");
        assert_eq!(Value::Int(1).legacy_type_name(), "integer");
    }

    #[test]
    fn references_share() {
        let mut a = Value::Int(1);
        let r = a.make_ref();
        assert!(a.is_reference());
        r.set(Value::Int(5));
        assert_eq!(a.deref().as_int(), Some(5));
        let b = a.clone();
        assert_eq!(r.refcount(), 3);
        if let Value::Reference(rb) = &b {
            assert!(PhpRef::ptr_eq(rb, &r));
        }
        a.unwrap_reference();
        assert!(!a.is_reference());
        let guard = r.borrow();
        assert!(r.try_borrow_mut().is_none());
        drop(guard);
        assert!(r.try_borrow_mut().is_some());
        // nested references are flattened
        let nested = PhpRef::new(Value::Reference(PhpRef::new(Value::Int(2))));
        assert_eq!(nested.get().as_int(), Some(2));
    }

    #[test]
    fn objects_identity() {
        let o = Obj::new(TestObj(1));
        let p = o.clone();
        let q = Obj::new(TestObj(1));
        assert!(Obj::ptr_eq(&o, &p));
        assert!(!Obj::ptr_eq(&o, &q));
        assert!(o.downcast_ref::<TestObj>().is_some());
        assert_eq!(o.handle(), 1);
    }

    /// A zval is 16 bytes (`zend_types.h`); every handle is one pointer.
    #[test]
    fn value_is_16_bytes() {
        use std::mem::size_of;
        assert_eq!(size_of::<Value>(), 16);
        assert_eq!(size_of::<Option<Value>>(), 16);
        assert_eq!(size_of::<Obj>(), 8);
        assert_eq!(size_of::<Option<Obj>>(), 8);
        // A weak handle is the allocation's fat `Weak` (see `ObjWeak`).
        assert_eq!(size_of::<ObjWeak>(), 16);
        assert_eq!(size_of::<Option<ObjWeak>>(), 16);
        assert_eq!(size_of::<PhpRef>(), 8);
        assert_eq!(size_of::<Resource>(), 8);
    }

    /// Counts payload drops.
    struct Counted(u32, Rc<Cell<u32>>);
    impl PhpObject for Counted {
        fn handle(&self) -> u32 {
            self.0
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
            self.1.set(self.1.get() + 1);
        }
    }

    #[test]
    fn object_refcount_and_single_drop() {
        let drops = Rc::new(Cell::new(0));
        let o = Obj::new(Counted(4, drops.clone()));
        assert_eq!(o.refcount(), 1);
        let p = o.clone();
        let q = p.clone();
        assert_eq!(o.refcount(), 3);
        assert_eq!(Rc::strong_count(o.rc()), 3);
        drop(p);
        assert_eq!(q.refcount(), 2);
        drop(o);
        assert_eq!(drops.get(), 0);
        assert_eq!(q.handle(), 4);
        assert_eq!(q.class_name(), "Counted");
        assert_eq!(format!("{q:?}"), "Object(Counted#4)");
        drop(q);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn object_rc_round_trip_keeps_identity() {
        let o = Obj::new(TestObj(2));
        let rc = o.rc().clone();
        // The dynamic type `from_rc` checks the tag against is the cell's.
        assert_eq!(Any::type_id(&*rc), TypeId::of::<ObjCell<TestObj>>());
        assert_eq!(o.refcount(), 2);
        assert_eq!(Rc::as_ptr(&rc).cast::<u8>() as usize, o.addr());
        let back = Obj::from_rc(rc);
        assert!(Obj::ptr_eq(&o, &back));
        assert_eq!(back, o);
        assert_eq!(o.refcount(), 2);
        drop(back);
        assert_eq!(o.refcount(), 1);
        // `Rc::as_any` reaches the payload, not the header.
        assert!(o.rc().as_any().downcast_ref::<TestObj>().is_some());
        assert_eq!(o.rc().handle(), 2);
    }

    #[test]
    fn object_std_weak_round_trip() {
        let drops = Rc::new(Cell::new(0));
        let o = Obj::new(Counted(1, drops.clone()));
        let w: Weak<dyn PhpObject> = Rc::downgrade(o.rc());
        assert_eq!(w.as_ptr().cast::<u8>() as usize, o.addr());
        let up = Obj::from_rc(w.upgrade().expect("alive"));
        assert!(Obj::ptr_eq(&o, &up));
        assert_eq!(o.refcount(), 2);
        drop((o, up));
        assert_eq!(drops.get(), 1);
        assert!(w.upgrade().is_none());
        assert_eq!(w.strong_count(), 0);
    }

    #[test]
    fn object_thin_weak() {
        let drops = Rc::new(Cell::new(0));
        let o = Obj::new(Counted(9, drops.clone()));
        let w = o.downgrade();
        let w2 = w.clone();
        assert_eq!(w.addr(), o.addr());
        assert_eq!(w.strong_count(), 1);
        assert_eq!(Rc::weak_count(o.rc()), 2);
        let up = w.upgrade().expect("alive");
        assert!(Obj::ptr_eq(&o, &up));
        assert_eq!(o.refcount(), 2);
        drop(up);
        drop(w2);
        assert_eq!(Rc::weak_count(o.rc()), 1);
        drop(o);
        assert_eq!(drops.get(), 1);
        assert!(w.upgrade().is_none());
        assert_eq!(w.strong_count(), 0);
        assert!(format!("{w:?}").starts_with("ObjWeak(0x"));
        let w3 = w.clone();
        drop(w);
        assert!(w3.upgrade().is_none());
    }

    #[test]
    fn foreign_rc_is_adopted() {
        let rc: Rc<dyn PhpObject> = Rc::new(TestObj(7));
        // Not an `Rc`'s type id, and not a cell's.
        assert_eq!(Any::type_id(&*rc), TypeId::of::<TestObj>());
        let o = Obj::from_rc(rc.clone());
        assert_eq!(o.handle(), 7);
        assert_eq!(o.class_name(), "Foo");
        // The payload is reached through the adopted `Rc` (slow path).
        assert!(o.downcast_ref::<TestObj>().is_some());
        assert!(o.downcast_ref::<Counted>().is_none());
        assert_eq!(Rc::strong_count(&rc), 2);
        // Each adoption is a new object.
        let p = Obj::from_rc(rc.clone());
        assert!(!Obj::ptr_eq(&o, &p));
        drop((o, p));
        assert_eq!(Rc::strong_count(&rc), 1);
    }

    /// An implementor forwarding the tag of another object must not be
    /// taken for that object.
    struct Forwarder(Obj);
    impl PhpObject for Forwarder {
        fn handle(&self) -> u32 {
            self.0.handle()
        }
        fn class_name(&self) -> ZStr {
            self.0.class_name()
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn obj_cell_tag(&self) -> Option<ObjCellTag> {
            self.0.rc().obj_cell_tag()
        }
    }

    #[test]
    fn forwarded_tag_is_rejected() {
        let inner = Obj::new(TestObj(3));
        let o = Obj::from_rc(Rc::new(Forwarder(inner.clone())));
        assert!(!Obj::ptr_eq(&o, &inner));
        assert!(o.downcast_ref::<Forwarder>().is_some());
        assert!(o.downcast_ref::<TestObj>().is_none());
        assert_eq!(inner.refcount(), 2);
        drop(o);
        assert_eq!(inner.refcount(), 1);
    }

    #[repr(align(32))]
    struct Aligned(u8);
    impl PhpObject for Aligned {
        fn handle(&self) -> u32 {
            u32::from(self.0)
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Aligned")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn payload_address_maps_to_object_address() {
        let a = Obj::new(TestObj(1));
        let pa = a.downcast_ref::<TestObj>().expect("payload");
        assert_eq!(Obj::addr_of_payload(pa), a.addr());
        let b = Obj::new(Aligned(5));
        let pb = b.downcast_ref::<Aligned>().expect("payload");
        assert_eq!(std::ptr::from_ref(pb).cast::<u8>() as usize % 32, 0);
        assert_eq!(Obj::addr_of_payload(pb), b.addr());
        assert_eq!(b.handle(), 5);
        let c = Obj::new(Counted(2, Rc::new(Cell::new(0))));
        let pc = c.downcast_ref::<Counted>().expect("payload");
        assert_eq!(Obj::addr_of_payload(pc), c.addr());
    }

    thread_local! {
        static SLOT: RefCell<Option<ObjWeak>> = const { RefCell::new(None) };
        static REVIVED: RefCell<Vec<Obj>> = const { RefCell::new(Vec::new()) };
    }

    /// A payload whose drop replaces the weak slot that points at the
    /// object being freed and creates a new object (the VM's object store
    /// and destructor queue are touched from inside `Drop` like this).
    struct Reviver(u32);
    impl PhpObject for Reviver {
        fn handle(&self) -> u32 {
            self.0
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Reviver")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    impl Drop for Reviver {
        fn drop(&mut self) {
            if self.0 == 0 {
                return;
            }
            let addr = Obj::addr_of_payload(self);
            SLOT.with(|s| {
                let old = s.borrow().as_ref().map(ObjWeak::addr);
                assert_eq!(old, Some(addr));
                assert!(s.borrow().as_ref().and_then(ObjWeak::upgrade).is_none());
            });
            let revived = Obj::new(Reviver(0));
            SLOT.with(|s| *s.borrow_mut() = Some(revived.downgrade()));
            REVIVED.with(|r| r.borrow_mut().push(revived));
        }
    }

    #[test]
    fn payload_drop_may_create_objects_and_drop_weaks() {
        let o = Obj::new(Reviver(1));
        SLOT.with(|s| *s.borrow_mut() = Some(o.downgrade()));
        drop(o);
        let revived = REVIVED.with(|r| r.borrow_mut().pop()).expect("revived");
        let slot = SLOT.with(|s| s.borrow_mut().take()).expect("slot");
        assert!(Obj::ptr_eq(&slot.upgrade().expect("alive"), &revived));
        drop(revived);
        assert!(slot.upgrade().is_none());
    }

    thread_local! {
        static QUEUE: RefCell<Vec<Obj>> = const { RefCell::new(Vec::new()) };
    }

    /// Like the VM's object data: the first time its last handle goes, it
    /// queues the handle for a destructor call (`release_last` clones
    /// `this`), so the same cell stays alive; the second time it is freed.
    struct Pending {
        destructed: Cell<bool>,
        hooks: Rc<Cell<u32>>,
        drops: Rc<Cell<u32>>,
    }
    impl PhpObject for Pending {
        fn handle(&self) -> u32 {
            7
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Pending")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn release_last(&self, this: &Obj) {
            self.hooks.set(self.hooks.get() + 1);
            // php-src: `GC_REFCOUNT(obj) == 0` on entry, 1 during `dtor_obj`.
            assert_eq!(this.refcount(), 1);
            if !self.destructed.replace(true) {
                QUEUE.with(|q| q.borrow_mut().push(this.clone()));
            }
        }
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn release_last_keeps_the_same_cell_alive_for_a_pending_destructor() {
        let hooks = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let o = Obj::new(Pending {
            destructed: Cell::new(false),
            hooks: hooks.clone(),
            drops: drops.clone(),
        });
        let addr = o.addr();
        let w = o.downgrade();
        let std_weak: Weak<dyn PhpObject> = Rc::downgrade(o.rc());
        // Not the last handle: no hook.
        drop(o.clone());
        assert_eq!(hooks.get(), 0);
        // The last handle: queued instead of freed, identity kept.
        drop(o);
        assert_eq!((hooks.get(), drops.get()), (1, 0));
        let queued = QUEUE.with(|q| q.borrow_mut().pop()).expect("queued");
        assert_eq!(queued.addr(), addr);
        assert_eq!(queued.refcount(), 1);
        assert!(Obj::ptr_eq(&w.upgrade().expect("alive"), &queued));
        assert!(Obj::ptr_eq(
            &Obj::from_rc(std_weak.upgrade().expect("alive")),
            &queued
        ));
        assert!(queued
            .downcast_ref::<Pending>()
            .is_some_and(|p| p.destructed.get()));
        // Upgrades made and dropped meanwhile were never the last handle.
        assert_eq!(hooks.get(), 1);
        // The queued handle goes: the hook declines to revive, the payload
        // is dropped exactly once and the cell is gone.
        drop(queued);
        assert_eq!((hooks.get(), drops.get()), (2, 1));
        assert!(w.upgrade().is_none());
        assert!(std_weak.upgrade().is_none());
        assert!(QUEUE.with(|q| q.borrow().is_empty()));
    }

    /// The default hook does nothing: objects without one are freed on
    /// their last handle, also when adopted through `from_rc`.
    #[test]
    fn default_release_last_frees_on_the_last_handle() {
        let drops = Rc::new(Cell::new(0));
        let o = Obj::new(Counted(1, drops.clone()));
        let back = Obj::from_rc(o.rc().clone());
        drop(o);
        assert_eq!(drops.get(), 0);
        drop(back);
        assert_eq!(drops.get(), 1);
        let foreign: Rc<dyn PhpObject> = Rc::new(Counted(2, drops.clone()));
        drop(Obj::from_rc(foreign));
        assert_eq!(drops.get(), 2);
    }

    /// The common part of a payload family (like the VM's object data,
    /// whose property slots follow it inline).
    struct Head {
        id: u32,
        /// Sum of the tail seen by the payload's `Drop`.
        seen: Rc<Cell<u64>>,
    }

    /// A family member with room for `N` values after the head.
    #[repr(C)]
    struct Tailed<const N: usize> {
        head: Head,
        tail: UnsafeCell<[u64; N]>,
    }
    impl<const N: usize> PhpObject for Tailed<N> {
        fn handle(&self) -> u32 {
            self.head.id
        }
        fn class_name(&self) -> ZStr {
            ZStr::from("Tailed")
        }
        fn as_any(&self) -> &dyn Any {
            &self.head
        }
    }
    impl<const N: usize> Drop for Tailed<N> {
        fn drop(&mut self) {
            // Through the drop's own exclusive reference, never through a
            // pointer made from a handle.
            let sum = self.tail.get_mut().iter().sum();
            self.head.seen.set(sum);
        }
    }

    #[test]
    fn family_members_are_recognised_by_their_common_part() {
        let seen = Rc::new(Cell::new(0));
        // SAFETY: `Tailed` is `repr(C)` with its `Head` first.
        let o = unsafe {
            Obj::new_in_family::<Tailed<3>, Head>(Tailed {
                head: Head {
                    id: 5,
                    seen: seen.clone(),
                },
                tail: UnsafeCell::new([1, 2, 3]),
            })
        };
        let p = Obj::new(Counted(6, Rc::new(Cell::new(0))));
        assert_eq!(o.downcast_ref::<Head>().map(|h| h.id), Some(5));
        assert_eq!(o.handle(), 5);
        assert!(p.downcast_ref::<Head>().is_none());
        // The member type is not what the header records.
        assert!(o.downcast_ref::<Tailed<3>>().is_none());
        assert!(o.payload_ptr::<Tailed<3>>().is_none());
        assert!(p.payload_ptr::<Head>().is_none());
        // The payload pointer reaches the tail, also while shared
        // references to the object are around.
        let base = o.payload_ptr::<Head>().expect("family member");
        let head: &Head = o.downcast_ref::<Head>().expect("head");
        // SAFETY: the payload is a `Tailed<3>`; its tail (inside
        // `UnsafeCell`, so shared references to the payload allow the
        // writes) is in bounds of the allocation `base` may reach.
        let tail = unsafe {
            base.cast::<u8>()
                .add(std::mem::offset_of!(Tailed<3>, tail))
                .cast::<u64>()
        };
        unsafe { tail.add(1).write(20) };
        assert_eq!(unsafe { tail.add(1).read() }, 20);
        assert_eq!(head.id, 5);
        assert_eq!(o.addr() + payload_offset::<Head>(), base.as_ptr() as usize);
        assert_eq!(
            // SAFETY: the payload is a live `Tailed<3>`.
            Obj::addr_of_payload::<Tailed<3>>(unsafe { &*base.as_ptr().cast() }),
            o.addr()
        );
        // Weak handles and `from_rc` keep the family header.
        let w = o.downgrade();
        let back = Obj::from_rc(o.rc().clone());
        assert!(back.downcast_ref::<Head>().is_some());
        drop(back);
        drop(p);
        drop(o);
        assert!(w.upgrade().is_none());
        assert_eq!(seen.get(), 1 + 20 + 3);
    }

    #[test]
    fn family_members_can_be_built_in_place() {
        let seen = Rc::new(Cell::new(0));
        let mut kept: Option<NonNull<Tailed<2>>> = None;
        // SAFETY: `Tailed` is `repr(C)` with its `Head` first; the closure
        // writes both fields.
        let o = unsafe {
            Obj::new_in_family_with::<Tailed<2>, Head>(|p| {
                kept = Some(p);
                let t = p.as_ptr();
                ptr::addr_of_mut!((*t).head).write(Head {
                    id: 9,
                    seen: seen.clone(),
                });
                ptr::addr_of_mut!((*t).tail).write(UnsafeCell::new([4, 5]));
            })
        };
        assert_eq!(o.downcast_ref::<Head>().map(|h| h.id), Some(9));
        assert_eq!(o.handle(), 9);
        assert_eq!(o.refcount(), 1);
        // The pointer handed to `init` is the payload's for good.
        let p = kept.expect("init ran");
        assert_eq!(
            o.payload_ptr::<Head>().map(|h| h.as_ptr() as usize),
            Some(p.as_ptr() as usize)
        );
        // SAFETY: the payload is a live `Tailed<2>`; its tail is in an
        // `UnsafeCell`.
        unsafe {
            ptr::addr_of_mut!((*p.as_ptr()).tail)
                .cast::<u64>()
                .write(40);
        }
        let back = Obj::from_rc(o.rc().clone());
        assert!(Obj::ptr_eq(&o, &back));
        assert_eq!(o.refcount(), 2);
        drop(back);
        let w = o.downgrade();
        drop(o);
        assert!(w.upgrade().is_none());
        assert_eq!(seen.get(), 40 + 5);
    }

    #[test]
    fn values_of_objects_clone_and_drop() {
        let drops = Rc::new(Cell::new(0));
        let v = Value::Object(Obj::new(Counted(1, drops.clone())));
        let copies: Vec<Value> = (0..10).map(|_| v.clone()).collect();
        if let Value::Object(o) = &v {
            assert_eq!(o.refcount(), 11);
        }
        drop(copies);
        assert_eq!(drops.get(), 0);
        let mut r = v;
        let rf = r.make_ref();
        assert_eq!(rf.get().type_name(), "Counted");
        drop((r, rf));
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn resources() {
        let r = Resource::with_handle(5, "stream", None);
        assert_eq!(r.handle(), 5);
        assert_eq!(r.type_name().unwrap(), "stream");
        r.close();
        assert!(r.is_closed());
        assert_eq!(Value::Resource(r).legacy_type_name(), "resource (closed)");
    }
}
