// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`VmHost`]: the [`SplHost`] implementation over the VM, plus the
//! conversions between VM throws and runtime errors.

use php_runtime::serialize::SerializedObject;
use php_runtime::{
    ArrayKey, BinaryOp, CastTarget, ErrorLevel, HashTable, Obj, ObjectDump, PropertyPurpose,
    RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_spl::{ClassInfo, HostIterator, NativeOp, ObjectIterator, SplHost, SplObject};
use php_stdarr::ArrayContext;
use php_vm::class::Class;
use php_vm::ext::{NativeIterator, VmResult};
use php_vm::func::{flags, Func};
use php_vm::object::{
    class_of, data, prop_order, set_prop_order, PropKey, PropOrder, PropTableBuilder,
};
use php_vm::vm::{FrameKind, PendingCall};
use php_vm::{Throw, Vm};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// The last property table handed out for a wrapped object, with the
/// iterator positions `ArrayObject`/`ArrayIterator` registered on it.
struct PropCache {
    owner: Weak<dyn php_runtime::PhpObject>,
    table: ZArray,
}

thread_local! {
    /// Persistent property tables of objects wrapped by `ArrayObject` /
    /// `ArrayIterator` (the VM rebuilds `zend_std_get_properties` on each
    /// request, so iterator registrations are kept here), keyed by handle.
    static PROP_CACHE: RefCell<HashMap<u32, PropCache>> = RefCell::new(HashMap::new());
    /// The `RegexIterator` PCRE state (pattern cache, last error). The
    /// `php-ext-pcre` state is private to that crate, so SPL keeps its own.
    static PCRE: RefCell<Option<php_pcre::Pcre>> = const { RefCell::new(None) };
}

/// Converts a VM throw into a runtime error. Bailouts (`exit`, fatal
/// errors) are parked in [`Vm::pending_throw`] and come back out of
/// [`rt_throw`]. So is the unwinder of a destroyed fiber (the hidden
/// graceful-exit object, the only throwable value outside the `Throwable`
/// hierarchy): like `zend_is_unwind_exit()`/`zend_is_graceful_exit()` in
/// `spl_iterators.c` (GH-24264), `CATCH_GET_CHILD` and the other SPL
/// exception sinks must never swallow it.
pub fn to_rt(vm: &mut Vm, t: Throw) -> RuntimeError {
    match t {
        Throw::Exception(o) if is_unwinder(&o) => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(Throw::Exception(o));
            }
            RuntimeError::Fatal(String::new())
        }
        Throw::Exception(o) => RuntimeError::Thrown(Value::Object(o)),
        other => {
            if vm.pending_throw.is_none() {
                vm.pending_throw = Some(other);
            }
            RuntimeError::Fatal(String::new())
        }
    }
}

/// Whether a thrown object is an internal unwinder (the fiber
/// graceful exit) rather than a `Throwable` user code can catch.
fn is_unwinder(o: &Obj) -> bool {
    !class_of(o).instance_of_name(b"throwable")
}

/// Converts a runtime error into a VM throw, creating the exception object
/// for errors raised by `php-spl` (`LogicException`, `TypeError`, ...).
pub fn rt_throw(vm: &mut Vm, e: RuntimeError) -> Throw {
    if let Some(t) = vm.pending_throw.take() {
        return t;
    }
    let class = e.class_name().unwrap_or("Error");
    match e {
        RuntimeError::Thrown(Value::Object(o)) => Throw::Exception(o),
        RuntimeError::Thrown(_) => Throw::Fatal,
        RuntimeError::Fatal(m) => vm.fatal(&m),
        RuntimeError::Custom { class, message } => vm.throw(&class.to_string_lossy(), &message),
        RuntimeError::Error(m)
        | RuntimeError::TypeError(m)
        | RuntimeError::ValueError(m)
        | RuntimeError::ArithmeticError(m)
        | RuntimeError::DivisionByZeroError(m)
        | RuntimeError::ArgumentCountError(m) => vm.throw(class, &m),
    }
}

/// Applies the `pcre.*` INI settings (registered by ext/pcre, when loaded)
/// to SPL's own PCRE state: `RegexIterator` matches honour
/// `pcre.backtrack_limit` like `preg_*` (GH-14290).
fn sync_pcre_ini(vm: &Vm, p: &mut php_pcre::Pcre) {
    if vm.ini_string("pcre.backtrack_limit").is_some() {
        p.set_backtrack_limit(vm.ini_long("pcre.backtrack_limit"));
    }
    if vm.ini_string("pcre.recursion_limit").is_some() {
        p.set_recursion_limit(vm.ini_long("pcre.recursion_limit"));
    }
    if vm.ini_string("pcre.jit").is_some() {
        p.set_jit(vm.ini_bool("pcre.jit"));
    }
}

/// The SPL state attached to an object by the `create` hook.
#[must_use]
pub fn spl_state_of(obj: &Obj) -> Option<Rc<SplObject>> {
    php_vm::object::internal::<Rc<SplObject>, _>(obj, |s| s.clone())
}

/// Whether two callable values are the same one (`===`).
fn same_callable(a: &Value, b: &Value) -> bool {
    php_runtime::ops::is_identical(a, b).unwrap_or(false)
}

/// Whether the method `lc` of `cls` is implemented in user code.
fn is_user_method(cls: &Rc<Class>, name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    cls.find_method(lc.as_bytes())
        .is_some_and(|m| matches!(m.func, Func::User(_)))
}

/// `"\0Class\0name"` / `"\0*\0name"` / `"name"`.
fn mangle(flags_: u32, class: &ZStr, name: &ZStr) -> ZStr {
    let mut v = Vec::new();
    if flags_ & flags::PRIVATE != 0 {
        v.push(0);
        v.extend_from_slice(class.as_bytes());
        v.push(0);
    } else if flags_ & flags::PROTECTED != 0 {
        v.extend_from_slice(b"\0*\0");
    } else {
        return name.clone();
    }
    v.extend_from_slice(name.as_bytes());
    ZStr::from(v)
}

/// Splits a mangled property name into (class or `*`, name).
fn demangle(key: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if key.first() == Some(&0) {
        if let Some(end) = key[1..].iter().position(|&b| b == 0) {
            return (Some(&key[1..=end]), &key[end + 2..]);
        }
    }
    (None, key)
}

/// `zend_std_get_properties`: declared (initialized) properties in slot
/// order with mangled names, then dynamic ones. The table registered for
/// the object is reused when its keys did not change, so iterator
/// positions survive.
#[must_use]
pub fn std_properties(obj: &Obj) -> ZArray {
    let fresh = build_properties(obj);
    let cached = PROP_CACHE.with(|c| {
        c.borrow()
            .get(&obj.handle())
            .filter(|e| e.owner.upgrade().is_some_and(|o| Rc::ptr_eq(&o, obj.rc())))
            .map(|e| e.table.clone())
    });
    let Some(mut cached) = cached else {
        remember(obj, &fresh);
        return fresh;
    };
    if cached.len() == fresh.len() && cached.keys().zip(fresh.keys()).all(|(a, b)| a == b) {
        let ht = cached.make_mut();
        for (k, v) in fresh.iter() {
            ht.insert(k.clone(), v.clone());
        }
        remember(obj, &cached);
        return cached;
    }
    let mut fresh = fresh;
    fresh.make_mut().take_iterators_from(cached.make_mut());
    remember(obj, &fresh);
    fresh
}

/// The number of entries [`build_properties`] produces for `obj`, without
/// building the table.
fn std_property_count(obj: &Obj) -> usize {
    let cls = class_of(obj).clone();
    let d = data(obj);
    let declared = {
        let slots = d.slots.borrow();
        cls.props
            .iter()
            .filter(|pi| {
                pi.flags & flags::STATIC == 0
                    && !pi.is_virtual()
                    && matches!(slots.get(pi.slot as usize), Some(Some(_)))
            })
            .count()
    };
    declared + d.dynamic.borrow().as_deref().map_or(0, HashTable::len)
}

/// Records the table handed out for `obj`.
///
/// Only the keys and the iterator positions of the cached table are ever
/// used ([`std_properties`] refreshes the values), so references are not
/// kept: holding them would count as a second owner and make `var_dump()`
/// show the object's property as `&` after `$ao[$k] += $v`.
fn remember(obj: &Obj, table: &ZArray) {
    let mut table = table.clone();
    if table.values().any(|v| matches!(v, Value::Reference(_))) {
        for (_, v) in table.make_mut().iter_mut() {
            if matches!(v, Value::Reference(_)) {
                *v = Value::Null;
            }
        }
    }
    PROP_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 256 {
            c.retain(|_, e| e.owner.strong_count() > 0);
        }
        c.insert(
            obj.handle(),
            PropCache {
                owner: Rc::downgrade(obj.rc()),
                table,
            },
        );
    });
}

/// A property value as `zend_array_dup` copies it: a reference only the
/// object holds (refcount 1, left behind by `$ao[$k]++` or an unset
/// alias) is no reference at all.
fn property_value(v: &Value) -> Value {
    match v {
        Value::Reference(r) if r.refcount() == 1 => r.get(),
        other => other.clone(),
    }
}

fn build_properties(obj: &Obj) -> ZArray {
    let cls = class_of(obj).clone();
    let d = data(obj);
    let mut t = PropTableBuilder::new(d);
    {
        let slots = d.slots.borrow();
        for pi in &cls.props {
            if pi.flags & flags::STATIC != 0 || pi.is_virtual() {
                continue;
            }
            if let Some(Some(v)) = slots.get(pi.slot as usize) {
                t.push(
                    Some(pi.slot),
                    ArrayKey::Str(mangle(pi.flags, &pi.class, &pi.name)),
                    property_value(v),
                );
            }
        }
    }
    if let Some(dynp) = d.dynamic.borrow().as_deref() {
        for (k, v) in dynp {
            t.push(None, k.clone(), property_value(v));
        }
    }
    ZArray::from_table(t.finish())
}

/// Whether the keys `new` shares with `current` come in another relative
/// order: the table was reordered (sorted), not merely written to.
#[allow(clippy::mutable_key_type)] // ZStr's cached-hash cell never changes Hash/Eq
fn reordered(current: &ZArray, new: &ZArray) -> bool {
    let index: HashMap<&ArrayKey, usize> =
        current.keys().enumerate().map(|(i, k)| (k, i)).collect();
    let mut last = None;
    for k in new.keys() {
        if let Some(&i) = index.get(k) {
            if last.is_some_and(|l| i < l) {
                return true;
            }
            last = Some(i);
        }
    }
    false
}

/// Replaces an object's property table (inverse of [`std_properties`]).
///
/// The table's order becomes the object's property order
/// ([`php_vm::object::PropOrder`]) once it was reordered (an
/// `ArrayObject::asort()` & co. over the object), as Zend's sort of
/// `zobj->properties` shows in every later listing; an object that already
/// has its own order follows later writes (appended keys go last).
pub fn set_std_properties(obj: &Obj, props: &ZArray) {
    let current = build_properties(obj);
    remember(obj, props);
    let cls = class_of(obj).clone();
    let d = data(obj);
    let mut slots = d.slots.borrow_mut();
    let mut seen = vec![false; slots.len()];
    let mut dynamic = HashTable::new();
    let mut order = Vec::with_capacity(props.len());
    for (k, v) in props.iter() {
        let declared = match k {
            ArrayKey::Str(s) => {
                let (scope, name) = demangle(s.as_bytes());
                cls.props.iter().find(|pi| {
                    pi.flags & flags::STATIC == 0
                        && pi.name.as_bytes() == name
                        && match scope {
                            None => pi.flags & (flags::PRIVATE | flags::PROTECTED) == 0,
                            Some(b"*") => pi.flags & flags::PROTECTED != 0,
                            Some(c) => {
                                pi.flags & flags::PRIVATE != 0
                                    && pi.class.as_bytes().eq_ignore_ascii_case(c)
                            }
                        }
                })
            }
            ArrayKey::Int(_) => None,
        };
        match declared {
            Some(pi) => {
                let i = pi.slot as usize;
                if i < slots.len() {
                    slots[i] = Some(v.clone());
                    seen[i] = true;
                }
                order.push(PropKey::Slot(pi.slot));
            }
            None => {
                dynamic.insert(k.clone(), v.clone());
                order.push(PropKey::Dynamic(k.clone()));
            }
        }
    }
    for (i, s) in seen.iter().enumerate() {
        if !s {
            slots[i] = None;
        }
    }
    drop(slots);
    let had_dynamic = d.dynamic.borrow().is_some();
    if had_dynamic || !dynamic.is_empty() {
        *d.dynamic.borrow_mut() = Some(Box::new(dynamic));
    }
    if prop_order(d).is_some() || reordered(&current, props) {
        let order = (!is_default_order(&cls, &order)).then(|| PropOrder::from_keys(order));
        set_prop_order(d, order);
    }
}

/// Whether `order` is the default listing order: declared properties in
/// declaration order, then dynamic ones (the dynamic table keeps their
/// order itself).
fn is_default_order(cls: &Class, order: &[PropKey]) -> bool {
    let position: HashMap<u32, usize> = cls
        .props
        .iter()
        .filter(|pi| pi.flags & flags::STATIC == 0 && !pi.is_virtual())
        .enumerate()
        .map(|(i, pi)| (pi.slot, i))
        .collect();
    let mut last = None;
    let mut dynamic = false;
    for k in order {
        match k {
            PropKey::Dynamic(_) => dynamic = true,
            PropKey::Slot(slot) => {
                let Some(&p) = position.get(slot) else {
                    return false;
                };
                if dynamic || last.is_some_and(|l| p < l) {
                    return false;
                }
                last = Some(p);
            }
        }
    }
    true
}

/// Binds newly installed references before the raw property-table writeback.
fn set_std_properties_checked(vm: &mut Vm, obj: &Obj, props: &ZArray) -> VmResult<()> {
    let mut target = obj.clone();
    let current = build_properties(obj);
    let mut original = None;
    let mut prepared: Vec<(php_runtime::PhpRef, Value)> = Vec::new();
    for (key, value) in props.iter() {
        let Value::Reference(r) = value else {
            continue;
        };
        if matches!(current.get(key), Some(Value::Reference(old)) if php_runtime::PhpRef::ptr_eq(old, r))
        {
            continue;
        }
        let old = r.get();
        target = match vm.prepare_property_table_reference(&target, key, r) {
            Ok(target) => target,
            Err(t) => {
                if let Some(original) = original {
                    set_std_properties(&target, &original);
                }
                // Restored slots no longer hold the newly added type sources.
                for (r, value) in prepared.into_iter().rev() {
                    r.set(value);
                }
                return Err(t);
            }
        };
        original.get_or_insert_with(|| std_properties(&target));
        // The type source must be live before preparing another reference.
        let cls = class_of(&target);
        let d = data(&target);
        if let Some(pi) = cls.props.iter().find(|pi| {
            !pi.is_virtual()
                && pi.flags & flags::STATIC == 0
                && ArrayKey::Str(mangle(pi.flags, &pi.class, &pi.name)) == *key
        }) {
            d.slots.borrow_mut()[pi.slot as usize] = Some(value.clone());
        } else {
            d.dynamic
                .borrow_mut()
                .get_or_insert_with(Box::default)
                .insert(key.clone(), value.clone());
        }
        prepared.push((r.clone(), old));
    }
    set_std_properties(&target, props);
    Ok(())
}

/// `zend_std_has_property(obj, name, ZEND_PROPERTY_EXISTS)` from `scope`:
/// an initialized declared property visible from the scope, or a dynamic
/// property.
#[must_use]
pub fn has_accessible_property(obj: &Obj, name: &ZStr, scope: Option<&Rc<Class>>) -> bool {
    let cls = class_of(obj).clone();
    let d = data(obj);
    let visible = |pi: &php_vm::class::PropInfo| {
        if pi.flags & flags::PRIVATE != 0 {
            scope.is_some_and(|s| s.name.eq_ignore_ascii_case(pi.class.as_bytes()))
        } else if pi.flags & flags::PROTECTED != 0 {
            scope.is_some_and(|s| {
                s.instance_of_name(pi.class.to_ascii_lowercase().as_bytes())
                    || cls.instance_of_name(s.lc_name.as_bytes())
            })
        } else {
            true
        }
    };
    let slots = d.slots.borrow();
    for pi in cls
        .props
        .iter()
        .filter(|p| p.name == *name && p.flags & flags::STATIC == 0)
    {
        if visible(pi) {
            return matches!(slots.get(pi.slot as usize), Some(Some(_)));
        }
    }
    drop(slots);
    d.dynamic
        .borrow()
        .as_ref()
        .is_some_and(|t| t.get_str(name.as_bytes()).is_some())
}

fn class_info_of(cls: &Rc<Class>) -> ClassInfo {
    let parents = std::iter::successors(cls.parent.clone(), |c| c.parent.clone())
        .map(|c| c.name.clone())
        .collect();
    ClassInfo {
        name: cls.name.clone(),
        parents,
        interfaces: cls.interfaces.iter().map(|i| i.name.clone()).collect(),
        traits: cls.traits.iter().map(|t| t.name.clone()).collect(),
    }
}

/// The VM seen through [`SplHost`].
pub struct VmHost<'a> {
    /// The VM.
    pub vm: &'a mut Vm,
    pcre: Option<php_pcre::Pcre>,
    /// Whether [`SplHost::check_callable`] resolves with the callable
    /// deprecations (`Z_PARAM_FUNC`); off on a host made by
    /// [`VmHost::callable_checked`].
    callable_deprecations: bool,
    /// The callable [`SplHost::check_callable`] resolved last and that
    /// resolution (the `zend_fcall_info_cache` `Z_PARAM_FUNC` fills), which
    /// [`SplHost::call_callable`] calls for the same callable during this
    /// SPL call (`iterator_apply()`, `ArrayObject::uasort()`).
    resolved: Option<(Value, PendingCall)>,
}

impl std::fmt::Debug for VmHost<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmHost")
    }
}

impl<'a> VmHost<'a> {
    /// Wraps the VM.
    pub fn new(vm: &'a mut Vm) -> Self {
        VmHost {
            vm,
            pcre: None,
            callable_deprecations: true,
            resolved: None,
        }
    }

    /// A host for an SPL call whose callable argument the binding has
    /// already resolved in the calling scope, raising its deprecations
    /// (`spl_autoload_register()`, the callback filters' constructors):
    /// [`SplHost::check_callable`] only validates.
    pub fn callable_checked(vm: &'a mut Vm) -> Self {
        let mut host = Self::new(vm);
        host.callable_deprecations = false;
        host
    }

    fn vm_rt<T>(&mut self, r: Result<T, Throw>) -> RtResult<T> {
        r.map_err(|t| to_rt(self.vm, t))
    }
}

impl Drop for VmHost<'_> {
    fn drop(&mut self) {
        if let Some(p) = self.pcre.take() {
            PCRE.with(|s| {
                let mut s = s.borrow_mut();
                if s.is_none() {
                    *s = Some(p);
                }
            });
        }
    }
}

impl RuntimeContext for VmHost<'_> {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.vm.diagnostic(level, message)
    }
    fn precision(&self) -> i64 {
        self.vm.precision()
    }
    fn serialize_precision(&self) -> i64 {
        self.vm.serialize_precision()
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        self.vm.cast_object(obj, target)
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        self.vm.do_operation(op, op1, op2)
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        self.vm.compare_objects(op1, op2)
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        self.vm.object_properties(obj, purpose)
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        self.vm.object_dump(obj, purpose)
    }
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        self.vm.serialize_object(obj)
    }
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        self.vm.unserialize_object_create(class)
    }
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        self.vm.unserialize_object_finish(obj, props)
    }
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        self.vm.unserialize_enum(class, case)
    }
    fn unserialize_enum_case(
        &mut self,
        class: &ZStr,
        case: &ZStr,
    ) -> RtResult<php_runtime::serialize::EnumCaseLookup> {
        self.vm.unserialize_enum_case(class, case)
    }
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        self.vm.unserialize_custom(class, data)
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        RuntimeContext::count_object(self.vm, obj)
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        self.vm.property_read(obj, name)
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        self.vm.property_write(obj, name, value)
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        self.vm.create_std_object(props)
    }
}

impl ArrayContext for VmHost<'_> {
    fn is_enum(&self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        ArrayContext::count_object(self.vm, obj)
    }
    fn column_property(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        self.vm.column_property(obj, name)
    }
    fn this_object(&mut self) -> Option<Value> {
        self.vm.this_object()
    }
}

impl SplHost for VmHost<'_> {
    fn spl_state(&mut self, obj: &Obj) -> Option<Rc<SplObject>> {
        spl_state_of(obj)
    }

    fn serialize_values(&mut self, values: &[Value]) -> Option<RtResult<Vec<Vec<u8>>>> {
        let refs: Vec<&Value> = values.iter().collect();
        let r = self.vm.serialize_values(&refs);
        Some(self.vm_rt(r))
    }

    fn unserialize_options(&mut self) -> php_runtime::serialize::UnserializeOptions {
        self.vm.unserialize_options_in_effect()
    }

    fn unserialize_nested(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> Option<RtResult<Result<(Value, usize), usize>>> {
        let r = self.vm.unserialize_nested(data, offset);
        Some(self.vm_rt(r))
    }

    fn call_method(&mut self, obj: &Obj, name: &str, args: &[Value]) -> RtResult<Value> {
        let r = self.vm.call_method_by_name(obj, name, args.to_vec());
        self.vm_rt(r)
    }

    fn call_method_undef(&mut self, obj: &Obj, name: &str) -> RtResult<Value> {
        let r = self.vm.call_method_with_undef(obj, name, vec![None]);
        self.vm_rt(r)
    }

    fn has_user_method(&mut self, obj: &Obj, name: &str) -> bool {
        is_user_method(class_of(obj), name)
    }

    fn has_method(&mut self, obj: &Obj, name: &str) -> bool {
        let lc = name.to_ascii_lowercase();
        class_of(obj).find_method(lc.as_bytes()).is_some()
    }

    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool {
        let lc = class.trim_start_matches('\\').to_ascii_lowercase();
        class_of(obj).instance_of_name(lc.as_bytes())
    }

    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj> {
        let Some(cls) = self.vm.find_class(class.as_bytes()) else {
            return Err(RuntimeError::Error(format!(
                "Class \"{}\" not found",
                class.to_string_lossy()
            )));
        };
        let r = self.vm.new_object(&cls);
        self.vm_rt(r)
    }

    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj> {
        let r = self.vm.lookup_class(class);
        let Some(cls) = self.vm_rt(r)? else {
            return Err(RuntimeError::Error(format!(
                "Class \"{}\" not found",
                class.to_string_lossy()
            )));
        };
        let r = self.vm.new_object_with_args(&cls, args.to_vec());
        self.vm_rt(r)
    }

    fn class_is_a(&mut self, class: &ZStr, parent: &str) -> Option<bool> {
        let cls = self.vm.find_class(class.as_bytes())?;
        let lc = parent.trim_start_matches('\\').to_ascii_lowercase();
        Some(cls.instance_of_name(lc.as_bytes()))
    }

    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        let cached = self
            .resolved
            .as_ref()
            .filter(|(v, _)| same_callable(v, callable))
            .map(|(_, call)| call.clone());
        let r = match cached {
            // `zend_call_known_fcc()` with the cache `Z_PARAM_FUNC` filled.
            Some(call) => self.vm.call_resolved(call, args.to_vec()),
            None => self.vm.call_value(callable, args.to_vec()),
        };
        self.vm_rt(r)
    }

    fn check_callable(&mut self, callable: &Value) -> Result<(), String> {
        if !self.callable_deprecations {
            return if self.vm.is_callable(callable) {
                Ok(())
            } else {
                Err(self.vm.callable_error_message(callable))
            };
        }
        // `Z_PARAM_FUNC` / `"f"`: `zend_fcall_info_init()` resolves the
        // callable in the calling user frame's scope, raising the callable
        // deprecations once; the calls that follow use the resolution.
        match self.vm.resolve_callable(callable) {
            Ok(Ok(mut call)) => {
                // `zend_call_function()` makes `ZEND_CALL_DYNAMIC` calls.
                call.dynamic = true;
                self.resolved = Some((callable.clone(), call));
                Ok(())
            }
            Ok(Err(reason)) => Err(reason),
            Err(t) => {
                // An autoloader, or a user error handler given a
                // deprecation, threw: `zend_wrong_callback_error()` adds no
                // TypeError to it, and `rt_throw` hands the parked throw on.
                if self.vm.pending_throw.is_none() {
                    self.vm.pending_throw = Some(t);
                }
                Err("cannot use \"self\"/\"parent\"/\"static\" in callables".to_string())
            }
        }
    }

    fn foreign_iterator(&mut self, obj: &Obj) -> Option<RtResult<Box<dyn ObjectIterator>>> {
        let cls = class_of(obj).clone();
        let hook = cls.hooks.as_ref().and_then(|h| h.get_iterator)?;
        // SPL classes (and their subclasses) iterate through php-spl; a
        // user override of the iteration methods iterates through them
        // (`zend_user_it_get_iterator`).
        let is_spl = std::iter::successors(Some(cls.clone()), |c| c.parent.clone())
            .any(|c| php_spl::classes::find_class(&c.name.to_string_lossy()).is_some());
        let overridden = ["rewind", "valid", "current", "key", "next", "getiterator"]
            .iter()
            .any(|m| is_user_method(&cls, m));
        if is_spl || overridden {
            return None;
        }
        let r = hook(self.vm, obj, false)
            .map(|it| Box::new(HostIterator::new(Box::new(it))) as Box<dyn ObjectIterator>);
        Some(self.vm_rt(r))
    }

    fn reserve_iterator_handle(&mut self) -> Option<Box<dyn std::any::Any>> {
        Some(Box::new(php_vm::object::HandleGuard::reserve()))
    }

    fn is_overloaded_object(&mut self, obj: &Obj) -> bool {
        spl_state_of(obj).is_none()
            && class_of(obj)
                .hooks
                .as_ref()
                .is_some_and(|h| h.properties_for.is_some())
    }

    fn std_properties(&mut self, obj: &Obj) -> ZArray {
        std_properties(obj)
    }

    fn std_property_count(&mut self, obj: &Obj) -> usize {
        std_property_count(obj)
    }

    fn std_has_property(&mut self, obj: &Obj, name: &ZStr) -> bool {
        let scope = self
            .vm
            .frames
            .iter()
            .rev()
            .find(|f| f.kind != FrameKind::Builtin)
            .and_then(|f| f.scope.clone());
        has_accessible_property(obj, name, scope.as_ref())
    }

    fn set_std_properties(&mut self, obj: &Obj, props: ZArray) -> RtResult<()> {
        let r = set_std_properties_checked(self.vm, obj, &props);
        self.vm_rt(r)
    }

    fn properties_load(&mut self, obj: &Obj, props: &ZArray) -> RtResult<()> {
        let r = self.vm.object_properties_load(obj, props);
        self.vm_rt(r)
    }

    fn init_lazy_object(&mut self, obj: &Obj) -> RtResult<Obj> {
        let r = self.vm.lazy_object_init(obj);
        self.vm_rt(r)
    }

    fn class_info(&mut self, name: &ZStr, autoload: bool) -> Option<ClassInfo> {
        let cls = if autoload {
            match self.vm.lookup_class(name) {
                Ok(c) => c,
                Err(t) => {
                    if self.vm.pending_throw.is_none() {
                        self.vm.pending_throw = Some(t);
                    }
                    None
                }
            }
        } else {
            self.vm.find_class(name.as_bytes())
        }?;
        Some(class_info_of(&cls))
    }

    fn object_class_info(&mut self, obj: &Obj) -> ClassInfo {
        class_info_of(class_of(obj))
    }

    fn new_internal_iterator(&mut self, it: Box<dyn ObjectIterator>) -> RtResult<Value> {
        let r = crate::iter::new_internal_iterator(self.vm, it);
        self.vm_rt(r)
    }

    fn autoload_include(&mut self, file: &ZStr) -> RtResult<bool> {
        let path = file.to_string_lossy();
        let Some(full) = crate::funcs::resolve_include(self.vm, &path) else {
            return Ok(false);
        };
        let real = std::fs::canonicalize(&full)
            .map_or_else(|_| full.clone(), |p| p.to_string_lossy().into_owned());
        let key = ZStr::from(real.as_str());
        if self.vm.included_set.contains(&key) {
            return Ok(true);
        }
        self.vm.included_set.insert(key.clone());
        self.vm.included.push(key);
        let r = self.vm.include_file(&full);
        self.vm_rt(r)?;
        Ok(true)
    }

    fn pcre(&mut self) -> &mut php_pcre::Pcre {
        if self.pcre.is_none() {
            let mut p = PCRE.with(|s| s.borrow_mut().take()).unwrap_or_default();
            sync_pcre_ini(self.vm, &mut p);
            self.pcre = Some(p);
        }
        self.pcre.get_or_insert_with(php_pcre::Pcre::new)
    }

    fn drive_native(&mut self, it: &mut dyn std::any::Any, op: NativeOp) -> RtResult<Value> {
        let Some(it) = it.downcast_mut::<Box<dyn NativeIterator>>() else {
            return Err(RuntimeError::Error(
                "foreign iterator of unknown type".to_string(),
            ));
        };
        let r = match op {
            NativeOp::Rewind => it.rewind(self.vm).map(|()| Value::Null),
            NativeOp::Valid => it.valid(self.vm).map(Value::Bool),
            NativeOp::Current => it.current(self.vm),
            NativeOp::Key => it.key(self.vm),
            NativeOp::Next => it.next(self.vm).map(|()| Value::Null),
        };
        self.vm_rt(r)
    }

    fn run_pending_destructors(&mut self) -> RtResult<()> {
        // The VM's own queue walk: native `dtor_obj` hooks after any
        // `__destruct`, later exceptions chained to earlier ones.
        let r = self.vm.run_released_destructors();
        self.vm_rt(r)
    }

    fn write_output(&mut self, bytes: &[u8]) -> RtResult<()> {
        self.vm.output(bytes);
        Ok(())
    }
}

/// The [`SplHost`] used while the VM is lent to the stream layer
/// (`php_ext_streams::bridge`): every host call borrows the VM from the
/// bridge for its own duration and delegates to a [`VmHost`], so stream
/// operations made by SPL between host calls can reach the VM (user
/// stream wrappers).
pub struct BridgedHost {
    pcre: Option<php_pcre::Pcre>,
}

impl std::fmt::Debug for BridgedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgedHost")
    }
}

impl BridgedHost {
    /// A host for code running inside `bridge::run`.
    #[must_use]
    pub fn new() -> Self {
        BridgedHost { pcre: None }
    }
}

impl Default for BridgedHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BridgedHost {
    fn drop(&mut self) {
        if let Some(p) = self.pcre.take() {
            PCRE.with(|s| {
                let mut s = s.borrow_mut();
                if s.is_none() {
                    *s = Some(p);
                }
            });
        }
    }
}

/// Runs `f` on a [`VmHost`] over the VM lent to the bridge.
fn lent<R>(f: impl FnOnce(&mut VmHost<'_>) -> R) -> Option<R> {
    php_ext_streams::bridge::with_vm(|vm| {
        let mut h = VmHost::new(vm);
        f(&mut h)
    })
}

/// [`lent`] for fallible calls.
fn lent_rt<R>(f: impl FnOnce(&mut VmHost<'_>) -> RtResult<R>) -> RtResult<R> {
    lent(f).unwrap_or_else(|| Err(RuntimeError::Error("the VM is not available".to_string())))
}

impl RuntimeContext for BridgedHost {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        lent_rt(|h| h.diagnostic(level, message))
    }
    fn precision(&self) -> i64 {
        lent(|h| h.precision()).unwrap_or(14)
    }
    fn serialize_precision(&self) -> i64 {
        lent(|h| h.serialize_precision()).unwrap_or(-1)
    }
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        lent_rt(|h| h.cast_object(obj, target))
    }
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        lent_rt(|h| h.do_operation(op, op1, op2))
    }
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        lent_rt(|h| h.compare_objects(op1, op2))
    }
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        lent_rt(|h| h.object_properties(obj, purpose))
    }
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        lent_rt(|h| h.object_dump(obj, purpose))
    }
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
        lent_rt(|h| h.serialize_object(obj))
    }
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        lent_rt(|h| h.unserialize_object_create(class))
    }
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        lent_rt(|h| h.unserialize_object_finish(obj, props))
    }
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        lent_rt(|h| h.unserialize_enum(class, case))
    }
    fn unserialize_enum_case(
        &mut self,
        class: &ZStr,
        case: &ZStr,
    ) -> RtResult<php_runtime::serialize::EnumCaseLookup> {
        lent_rt(|h| h.unserialize_enum_case(class, case))
    }
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        lent_rt(|h| h.unserialize_custom(class, data))
    }
    fn is_enum(&mut self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        lent_rt(|h| RuntimeContext::count_object(h, obj))
    }
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        lent_rt(|h| h.property_read(obj, name))
    }
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        lent_rt(|h| h.property_write(obj, name, value))
    }
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        lent_rt(|h| h.create_std_object(props))
    }
}

impl ArrayContext for BridgedHost {
    fn is_enum(&self, obj: &Obj) -> bool {
        class_of(obj).is_enum()
    }
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        lent_rt(|h| ArrayContext::count_object(h, obj))
    }
    fn column_property(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        lent_rt(|h| h.column_property(obj, name))
    }
    fn this_object(&mut self) -> Option<Value> {
        lent(|h| h.this_object()).flatten()
    }
}

impl SplHost for BridgedHost {
    fn spl_state(&mut self, obj: &Obj) -> Option<Rc<SplObject>> {
        spl_state_of(obj)
    }
    fn unserialize_options(&mut self) -> php_runtime::serialize::UnserializeOptions {
        lent(|h| h.unserialize_options()).unwrap_or_default()
    }
    fn unserialize_nested(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> Option<RtResult<Result<(Value, usize), usize>>> {
        lent(|h| h.unserialize_nested(data, offset)).flatten()
    }
    fn call_method(&mut self, obj: &Obj, name: &str, args: &[Value]) -> RtResult<Value> {
        lent_rt(|h| h.call_method(obj, name, args))
    }
    fn call_method_undef(&mut self, obj: &Obj, name: &str) -> RtResult<Value> {
        lent_rt(|h| h.call_method_undef(obj, name))
    }
    fn has_user_method(&mut self, obj: &Obj, name: &str) -> bool {
        is_user_method(class_of(obj), name)
    }
    fn has_method(&mut self, obj: &Obj, name: &str) -> bool {
        let lc = name.to_ascii_lowercase();
        class_of(obj).find_method(lc.as_bytes()).is_some()
    }
    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool {
        let lc = class.trim_start_matches('\\').to_ascii_lowercase();
        class_of(obj).instance_of_name(lc.as_bytes())
    }
    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj> {
        lent_rt(|h| h.create_object(class))
    }
    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj> {
        lent_rt(|h| h.instantiate(class, args))
    }
    fn class_is_a(&mut self, class: &ZStr, parent: &str) -> Option<bool> {
        lent(|h| h.class_is_a(class, parent)).flatten()
    }
    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        lent_rt(|h| h.call_callable(callable, args))
    }
    fn check_callable(&mut self, callable: &Value) -> Result<(), String> {
        lent(|h| h.check_callable(callable)).unwrap_or(Ok(()))
    }
    fn foreign_iterator(&mut self, obj: &Obj) -> Option<RtResult<Box<dyn ObjectIterator>>> {
        lent(|h| h.foreign_iterator(obj)).flatten()
    }
    fn reserve_iterator_handle(&mut self) -> Option<Box<dyn std::any::Any>> {
        Some(Box::new(php_vm::object::HandleGuard::reserve()))
    }
    fn is_overloaded_object(&mut self, obj: &Obj) -> bool {
        lent(|h| h.is_overloaded_object(obj)).unwrap_or(false)
    }
    fn std_properties(&mut self, obj: &Obj) -> ZArray {
        std_properties(obj)
    }

    fn std_property_count(&mut self, obj: &Obj) -> usize {
        std_property_count(obj)
    }
    fn std_has_property(&mut self, obj: &Obj, name: &ZStr) -> bool {
        lent(|h| h.std_has_property(obj, name)).unwrap_or(false)
    }
    fn set_std_properties(&mut self, obj: &Obj, props: ZArray) -> RtResult<()> {
        lent_rt(|h| h.set_std_properties(obj, props))
    }
    fn properties_load(&mut self, obj: &Obj, props: &ZArray) -> RtResult<()> {
        lent_rt(|h| h.properties_load(obj, props))
    }
    fn init_lazy_object(&mut self, obj: &Obj) -> RtResult<Obj> {
        lent_rt(|h| h.init_lazy_object(obj))
    }
    fn class_info(&mut self, name: &ZStr, autoload: bool) -> Option<ClassInfo> {
        lent(|h| h.class_info(name, autoload)).flatten()
    }
    fn object_class_info(&mut self, obj: &Obj) -> ClassInfo {
        class_info_of(class_of(obj))
    }
    fn new_internal_iterator(&mut self, it: Box<dyn ObjectIterator>) -> RtResult<Value> {
        lent_rt(|h| h.new_internal_iterator(it))
    }
    fn autoload_include(&mut self, file: &ZStr) -> RtResult<bool> {
        lent_rt(|h| h.autoload_include(file))
    }
    fn pcre(&mut self) -> &mut php_pcre::Pcre {
        if self.pcre.is_none() {
            let mut p = PCRE.with(|s| s.borrow_mut().take()).unwrap_or_default();
            lent(|h| sync_pcre_ini(h.vm, &mut p));
            self.pcre = Some(p);
        }
        self.pcre.get_or_insert_with(php_pcre::Pcre::new)
    }
    fn drive_native(&mut self, it: &mut dyn std::any::Any, op: NativeOp) -> RtResult<Value> {
        lent_rt(|h| h.drive_native(it, op))
    }
    fn run_pending_destructors(&mut self) -> RtResult<()> {
        lent_rt(|h| h.run_pending_destructors())
    }
    fn write_output(&mut self, bytes: &[u8]) -> RtResult<()> {
        lent_rt(|h| h.write_output(bytes))
    }
}
