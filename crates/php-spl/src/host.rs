// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The services SPL needs from the VM: the [`SplHost`] trait and the
//! [`ObjectIterator`] protocol (`zend_object_iterator`).
//!
//! The VM implements [`SplHost`] once. SPL never holds a borrow of an
//! object's state while it calls back into the host, so user code invoked
//! from a callback (an overridden `compare()`, a `CallbackFilterIterator`
//! callback, `__toString`, ...) may freely use the same SPL object again.

use crate::object::SplObject;
use php_runtime::serialize::UnserializeOptions;
use php_runtime::{Obj, RtResult, RuntimeError, Value, ZArray, ZStr};
use php_stdarr::ArrayContext;
use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// Class metadata for `class_implements()`, `class_parents()` and
/// `class_uses()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassInfo {
    /// The class name as declared.
    pub name: ZStr,
    /// Parent classes, nearest first.
    pub parents: Vec<ZStr>,
    /// All implemented interfaces in `ce->interfaces` order.
    pub interfaces: Vec<ZStr>,
    /// Traits used directly by the class (`ce->trait_names`).
    pub traits: Vec<ZStr>,
}

/// What the VM provides to SPL.
///
/// Diagnostics, casts, comparisons and property tables come from
/// [`php_runtime::RuntimeContext`]; `count()` on objects and enum checks
/// from [`ArrayContext`].
pub trait SplHost: ArrayContext {
    /// `php_var_serialize()` of each of `values` with one shared `var_hash`
    /// (`PHP_VAR_SERIALIZE_INIT` / `DESTROY` around the calls, as the legacy
    /// `Serializable::serialize()` methods do), one payload per value.
    /// `None` (the default) makes SPL use php-runtime's serializer.
    fn serialize_values(&mut self, values: &[Value]) -> Option<RtResult<Vec<Vec<u8>>>> {
        let _ = values;
        None
    }

    /// The SPL internal state of an object, created by
    /// [`crate::object::create_state`] when the object was instantiated
    /// (`None` for objects that are not instances of an SPL class).
    fn spl_state(&mut self, obj: &Obj) -> Option<Rc<SplObject>>;

    /// The options of the `var_hash` the legacy `Serializable` formats
    /// unserialize their parts with: the active `unserialize()` call's
    /// (`allowed_classes`, `max_depth` and the depth reached), with
    /// diagnostics naming the active method. The default uses php-runtime's
    /// defaults.
    fn unserialize_options(&mut self) -> UnserializeOptions {
        UnserializeOptions::default()
    }

    /// `php_var_unserialize()` of the one value at `offset` in `data` within
    /// the active `unserialize()` call's shared `var_hash` (so `r:`/`R:`
    /// back-references cross the boundaries of nested `C:` payloads):
    /// `Ok(Ok((value, end)))`, `Ok(Err(failure_offset))`, or `Err` for an
    /// exception. `None` (the default) makes SPL parse the value itself
    /// with a fresh table and [`SplHost::unserialize_options`].
    fn unserialize_nested(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> Option<RtResult<Result<(Value, usize), usize>>> {
        let _ = (data, offset);
        None
    }

    /// `zend_call_method`: calls `$obj->name(...$args)` with normal method
    /// dispatch. When the method resolves to an internal SPL method the VM
    /// routes the call back to [`crate::call_method`].
    fn call_method(&mut self, obj: &Obj, name: &str, args: &[Value]) -> RtResult<Value>;

    /// [`SplHost::call_method`] with one undefined argument (`ZVAL_UNDEF`,
    /// what `spl_array_read_dimension_ex` passes `offsetGet()` for `$o[]`):
    /// a user method reports "Argument #1 (...) not passed".
    fn call_method_undef(&mut self, obj: &Obj, name: &str) -> RtResult<Value> {
        self.call_method(obj, name, &[Value::Null])
    }

    /// Whether the method `name` (any case) of the object's class is
    /// implemented in user code, i.e. a user subclass overrides the SPL
    /// implementation (the `fptr_* != NULL` checks of the C code).
    fn has_user_method(&mut self, obj: &Obj, name: &str) -> bool;

    /// Whether the object's class has a method `name` at all.
    fn has_method(&mut self, obj: &Obj, name: &str) -> bool;

    /// `instanceof`: whether `obj` is an instance of the class or interface
    /// `class` (case-insensitive).
    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool;

    /// `object_init_ex`: creates an instance of `class` without calling its
    /// constructor (SPL state is initialised by
    /// [`crate::object::create_state`]).
    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj>;

    /// `new $class(...$args)`.
    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj>;

    /// Whether `class` exists (looked up without autoloading) and is a
    /// subclass of (or equal to) `parent`; `None` when it does not exist.
    fn class_is_a(&mut self, class: &ZStr, parent: &str) -> Option<bool>;

    /// Calls a PHP callable value.
    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value>;

    /// `zend_is_callable_ex`: `Err(reason)` when not callable, where reason
    /// completes "must be a valid callback, ..." (e.g. `function "x" not
    /// found or invalid function name`).
    fn check_callable(&mut self, callable: &Value) -> Result<(), String>;

    /// Iterators for traversable objects whose iteration the VM implements
    /// itself (generators, internal classes of other extensions). Return
    /// `None` for user classes and SPL classes: [`crate::iter::get_iterator`]
    /// handles `Iterator`, `IteratorAggregate` and SPL objects.
    fn foreign_iterator(&mut self, obj: &Obj) -> Option<RtResult<Box<dyn ObjectIterator>>> {
        let _ = obj;
        None
    }

    /// `zend_iterator_init`: every `zend_object_iterator` is itself an
    /// object and takes a handle from the object store
    /// (`zend_objects_store_put`) when `get_iterator` creates it, so the
    /// objects created while it lives get higher handle numbers. Returns a
    /// token that holds such a handle until it is dropped (the handle then
    /// goes back on the free list, `zend_objects_store_del`). The default
    /// (`None`) reserves nothing.
    fn reserve_iterator_handle(&mut self) -> Option<Box<dyn Any>> {
        None
    }

    /// Whether a non-SPL object has custom `get_properties` /
    /// `get_properties_for` handlers (e.g. `DateTime`, `Closure`), which
    /// makes it unusable as `ArrayObject` storage ("Overloaded object of
    /// type %s is not compatible with %s").
    fn is_overloaded_object(&mut self, obj: &Obj) -> bool {
        let _ = obj;
        false
    }

    /// `zend_std_get_properties`: the object's property table with mangled
    /// keys (declared and dynamic properties).
    fn std_properties(&mut self, obj: &Obj) -> ZArray;

    /// `zend_hash_num_elements(zend_std_get_properties(obj))`: the number
    /// of entries [`std_properties`](Self::std_properties) returns, without
    /// building the table. The default builds it.
    fn std_property_count(&mut self, obj: &Obj) -> usize {
        self.std_properties(obj).len()
    }

    /// `zend_std_has_property(obj, name, ZEND_PROPERTY_EXISTS)`: whether a
    /// property `name` exists and is accessible from the calling scope.
    /// The default checks for a public (unmangled) key.
    fn std_has_property(&mut self, obj: &Obj, name: &ZStr) -> bool {
        self.std_properties(obj).get_str(name.as_bytes()).is_some()
    }

    /// Replaces the object's property table (used when an `ArrayObject`
    /// wraps an object and writes to it). Keys are mangled names.
    fn set_std_properties(&mut self, obj: &Obj, props: ZArray) -> RtResult<()>;

    /// `zend_lazy_object_init` before SPL reads an object's property
    /// table directly (`spl_array_get_hash_table`): initializes a lazy
    /// object and returns the object holding the properties (the real
    /// instance of a proxy). The default returns the object itself.
    fn init_lazy_object(&mut self, obj: &Obj) -> RtResult<Obj> {
        Ok(obj.clone())
    }

    /// `object_properties_load`: loads the extra members of an
    /// `__unserialize()` payload (mangled keys) into the object, with
    /// typed-property checks and the dynamic-property deprecation. The
    /// default writes each member with `property_write`.
    fn properties_load(&mut self, obj: &Obj, props: &ZArray) -> RtResult<()> {
        for (k, v) in props.iter() {
            self.property_write(obj, &k.to_zstr(), v.clone())?;
        }
        Ok(())
    }

    /// Class metadata of a class name (`None` when it does not exist after
    /// optional autoloading).
    fn class_info(&mut self, name: &ZStr, autoload: bool) -> Option<ClassInfo>;

    /// Class metadata of an object's class.
    fn object_class_info(&mut self, obj: &Obj) -> ClassInfo;

    /// Wraps a native iterator in an `InternalIterator` object
    /// (`zend_create_internal_iterator_zval`), as returned by
    /// `SplFixedArray::getIterator()`.
    fn new_internal_iterator(&mut self, it: Box<dyn ObjectIterator>) -> RtResult<Value>;

    /// `spl_autoload()`'s file loading: opens `file` using the include
    /// path and, unless it was already included, compiles and executes it.
    /// Returns whether the file was executed.
    fn autoload_include(&mut self, file: &ZStr) -> RtResult<bool>;

    /// The request's PCRE state (`RegexIterator`): pattern cache, INI
    /// limits and `preg_last_error()`.
    fn pcre(&mut self) -> &mut php_pcre::Pcre;

    /// Runs the destructors of objects released since the last call (the
    /// VM defers `__destruct` to a safe point; SPL code that releases
    /// elements one by one — `SplFixedArray::setSize()` — runs them at the
    /// point PHP would). The default does nothing.
    fn run_pending_destructors(&mut self) -> RtResult<()> {
        Ok(())
    }

    /// Performs `op` on a host-owned native iterator wrapped in a
    /// [`HostIterator`] (as returned by [`SplHost::foreign_iterator`]), so
    /// that it runs as the engine's iterator functions — not as PHP-level
    /// method calls. The default fails: hosts that hand out
    /// `HostIterator`s implement it.
    fn drive_native(&mut self, it: &mut dyn Any, op: NativeOp) -> RtResult<Value> {
        let _ = (it, op);
        Err(RuntimeError::Error(
            "the host does not drive native iterators".to_string(),
        ))
    }

    /// Writes to the output (`fpassthru`).
    fn write_output(&mut self, bytes: &[u8]) -> RtResult<()>;
}

/// The `zend_object_iterator` protocol used by `foreach`, `IteratorIterator`
/// and the `iterator_*()` functions.
pub trait ObjectIterator {
    /// `rewind`.
    fn rewind(&mut self, host: &mut dyn SplHost) -> RtResult<()>;
    /// `valid`.
    fn valid(&mut self, host: &mut dyn SplHost) -> RtResult<bool>;
    /// `get_current_data`; `None` models a `NULL` result, which ends
    /// `foreach` and `iterator_to_array()` silently.
    fn current(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>>;
    /// `get_current_key`; `None` when the iterator has no key function
    /// (`foreach` then uses a running index).
    fn key(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>>;
    /// `move_forward`.
    fn next(&mut self, host: &mut dyn SplHost) -> RtResult<()>;

    /// Whether [`ObjectIterator::fetch`] does a whole `foreach` step in
    /// one pass without running user code between its parts (so a caller
    /// need not check for a pending exception after each part).
    fn supports_fetch(&self) -> bool {
        false
    }

    /// One `foreach` step: advances first when `advance`, then returns
    /// `None` at the end, else the current value (as
    /// [`ObjectIterator::current`]) and, when `want_key`, the key (as
    /// [`ObjectIterator::key`]; `None` also when not wanted).
    fn fetch(
        &mut self,
        host: &mut dyn SplHost,
        advance: bool,
        want_key: bool,
    ) -> RtResult<Option<(Option<Value>, Option<Value>)>> {
        if advance {
            self.next(host)?;
        }
        if !self.valid(host)? {
            return Ok(None);
        }
        let v = self.current(host)?;
        let k = if want_key { self.key(host)? } else { None };
        Ok(Some((v, k)))
    }

    /// For `zend_user_iterator`s: the `Iterator` object whose methods this
    /// iterator calls. Callers may then call the methods directly instead
    /// of borrowing the iterator (so user code can re-enter it).
    fn method_object(&self) -> Option<&Obj> {
        None
    }

    /// For `zend_user_iterator`s: a handle sharing this iterator's state
    /// (the object and the cached current value), used to call the
    /// methods without holding a borrow of the iterator.
    fn method_iter(&self) -> Option<MethodIterator> {
        None
    }
}

/// One step of the `zend_object_iterator` protocol, for
/// [`SplHost::drive_native`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeOp {
    /// `rewind`.
    Rewind,
    /// `valid` (returns a bool).
    Valid,
    /// `get_current_data`.
    Current,
    /// `get_current_key`.
    Key,
    /// `move_forward`.
    Next,
}

/// An iterator owned by the host (e.g. the VM's `get_iterator` result for
/// a generator or a native class), driven through
/// [`SplHost::drive_native`].
pub struct HostIterator {
    inner: Box<dyn Any>,
}

impl fmt::Debug for HostIterator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostIterator")
    }
}

impl HostIterator {
    /// Wraps a host iterator.
    #[must_use]
    pub fn new(inner: Box<dyn Any>) -> Self {
        HostIterator { inner }
    }
}

impl ObjectIterator for HostIterator {
    fn rewind(&mut self, host: &mut dyn SplHost) -> RtResult<()> {
        host.drive_native(self.inner.as_mut(), NativeOp::Rewind)
            .map(|_| ())
    }
    fn valid(&mut self, host: &mut dyn SplHost) -> RtResult<bool> {
        let v = host.drive_native(self.inner.as_mut(), NativeOp::Valid)?;
        Ok(matches!(v, Value::Bool(true)))
    }
    fn current(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        host.drive_native(self.inner.as_mut(), NativeOp::Current)
            .map(Some)
    }
    fn key(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        host.drive_native(self.inner.as_mut(), NativeOp::Key)
            .map(Some)
    }
    fn next(&mut self, host: &mut dyn SplHost) -> RtResult<()> {
        host.drive_native(self.inner.as_mut(), NativeOp::Next)
            .map(|_| ())
    }
}

/// `zend_user_iterator`: iterates an `Iterator` object by calling its
/// `rewind`/`valid`/`current`/`key`/`next` methods. Like
/// `zend_user_it_get_current_data`, the value returned by `current()` is
/// cached until the next `next()`/`rewind()`; clones share the cache.
#[derive(Debug, Clone)]
pub struct MethodIterator {
    /// The iterator object.
    pub obj: Obj,
    value: Rc<RefCell<Option<Value>>>,
}

impl MethodIterator {
    /// Wraps an `Iterator` object.
    #[must_use]
    pub fn new(obj: Obj) -> Self {
        MethodIterator {
            obj,
            value: Rc::new(RefCell::new(None)),
        }
    }

    /// `zend_user_it_invalidate_current`: drops the cached value.
    pub fn invalidate_current(&self) {
        let old = self.value.borrow_mut().take();
        drop(old);
    }
}

impl ObjectIterator for MethodIterator {
    fn method_object(&self) -> Option<&Obj> {
        Some(&self.obj)
    }
    fn method_iter(&self) -> Option<MethodIterator> {
        Some(self.clone())
    }
    fn rewind(&mut self, host: &mut dyn SplHost) -> RtResult<()> {
        self.invalidate_current();
        host.call_method(&self.obj, "rewind", &[]).map(|_| ())
    }
    fn valid(&mut self, host: &mut dyn SplHost) -> RtResult<bool> {
        let v = host.call_method(&self.obj, "valid", &[])?;
        v.to_bool(host)
    }
    fn current(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        let cached = self.value.borrow().clone();
        if let Some(v) = cached {
            return Ok(Some(v));
        }
        let v = host.call_method(&self.obj, "current", &[])?;
        *self.value.borrow_mut() = Some(v.clone());
        Ok(Some(v))
    }
    fn key(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        host.call_method(&self.obj, "key", &[]).map(Some)
    }
    fn next(&mut self, host: &mut dyn SplHost) -> RtResult<()> {
        self.invalidate_current();
        host.call_method(&self.obj, "next", &[]).map(|_| ())
    }
}

/// Error used when an object unexpectedly lacks SPL state (cannot happen
/// for objects created through [`crate::object::create_state`]).
#[must_use]
pub fn missing_state(obj: &Obj) -> RuntimeError {
    RuntimeError::Error(format!(
        "Object of class {} is not properly initialized",
        obj.class_name()
    ))
}
