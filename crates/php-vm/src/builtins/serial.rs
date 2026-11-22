// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `serialize()` / `unserialize()` (`ext/standard/var.c`,
//! `ext/standard/var_unserializer.re`).
//!
//! Like php-src, nested calls share one serialization context: a
//! `serialize()` made from `Serializable::serialize()` continues the outer
//! numbering (back-references work across the boundary), and an
//! `unserialize()` made from `Serializable::unserialize()` sees the outer
//! `var_hash`; calls made from `__sleep()`, `__serialize()`, `__wakeup()`,
//! `__unserialize()` and autoloaders get a fresh one (`serialize_lock`).
//!
//! Unserialization stores every value in a cell (a [`PhpRef`]) that its
//! container holds as `Value::Reference`, so `R:` can turn any earlier slot
//! into a reference; once the payload is complete, cells nobody else shares
//! are unwrapped back into plain values.

use super::util::{arr, s, sv};
use crate::class::{cflags, Class, PropInfo};
use crate::func::{flags, Args};
use crate::object;
use crate::typecheck::{given_type, TypeCtx};
use crate::vm::{Throw, Vm, VmResult};
use php_memlimit::CheckedBuf;
use php_runtime::dump::unmangle_property_name;
use php_runtime::float::zend_gcvt;
use php_runtime::numeric::{handle_numeric_str, strtod};
use php_runtime::serialize::{AllowedClasses, UnserializeOptions};
use php_runtime::{ArrayKey, HashTable, Obj, PhpRef, PropertyPurpose, Value, ZArray, ZStr};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const E_WARNING: i64 = 2;
const E_DEPRECATED: i64 = 8192;

const INCOMPLETE_CLASS: &str = "__PHP_Incomplete_Class";
const MAGIC_MEMBER: &str = "__PHP_Incomplete_Class_Name";
/// `HT_MAX_SIZE` (64-bit).
const HT_MAX_SIZE: i64 = 0x4000_0000;

// ---------------------------------------------------------------------------
// Shared state (`BG(serialize)`, `BG(unserialize)`, `BG(serialize_lock)`)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SerData {
    n: i64,
    /// Identity → `n` of the first occurrence (-1: failed `Serializable`).
    seen: HashMap<usize, i64>,
    /// Keeps registered values alive so their addresses stay unique.
    keep: Vec<Value>,
}

enum Dtor {
    /// Kept alive until the end (overwritten values).
    Keep(#[allow(dead_code)] Value),
    /// Delayed `__wakeup()`.
    Wakeup(Obj),
    /// Delayed `__unserialize($data)`.
    Unserialize(Obj, Value),
}

struct UnserData {
    /// `var_hash` entries (`None`: invalidated after a failure).
    slots: Vec<Option<PhpRef>>,
    dtors: Vec<Dtor>,
    /// Lowercase allowed class names (`None`: all classes).
    allowed: Option<HashSet<Vec<u8>>>,
    cur_depth: i64,
    max_depth: i64,
    /// Objects created, whose properties need their cells unwrapped.
    objects: Vec<Obj>,
    /// Cells targeted by `R:` (true references): an overwritten location
    /// holding one is detached instead of reused.
    shared: HashSet<usize>,
    /// Every cell created by this context.
    cells: HashSet<usize>,
}

type SerRc = Rc<RefCell<SerData>>;
type UnserRc = Rc<RefCell<UnserData>>;

thread_local! {
    static LOCK: Cell<u32> = const { Cell::new(0) };
    static SER: RefCell<(u32, Option<SerRc>)> = const { RefCell::new((0, None)) };
    static UNSER: RefCell<(u32, Option<UnserRc>)> = const { RefCell::new((0, None)) };
    static ABANDONED_SER: RefCell<Vec<SerRc>> = const { RefCell::new(Vec::new()) };
    static ABANDONED_UNSER: RefCell<Vec<UnserRc>> = const { RefCell::new(Vec::new()) };
    /// The contexts opened by [`Vm::begin_unserialize_context`] (and
    /// whether each is the outermost), innermost last.
    static OPENED_UNSER: RefCell<Vec<(UnserRc, bool)>> = const { RefCell::new(Vec::new()) };
}

/// `php_basic_early_rinit()` / `PHP_RINIT(basic)`: discard contexts left
/// behind by a bailout before any request hooks can deserialize sessions.
fn request_startup(_vm: &mut Vm) {
    LOCK.with(|l| l.set(0));
    let ser = SER.with(|s| std::mem::take(&mut *s.borrow_mut()));
    let unser = UNSER.with(|s| std::mem::take(&mut *s.borrow_mut()));
    let abandoned_ser = ABANDONED_SER.with(|s| std::mem::take(&mut *s.borrow_mut()));
    let abandoned_unser = ABANDONED_UNSER.with(|s| std::mem::take(&mut *s.borrow_mut()));
    let opened_unser = OPENED_UNSER.with(|s| std::mem::take(&mut *s.borrow_mut()));
    drop((ser, unser, abandoned_ser, abandoned_unser, opened_unser));
}

impl Vm {
    /// Opens a real serialization context, then abandons it as
    /// `zend_test_serialization_bailout()` does before `zend_bailout()`.
    pub fn serialization_bailout(&mut self, unserialize: bool, locked: bool) -> Throw {
        if unserialize {
            let (d, _) = unser_init(self);
            ABANDONED_UNSER.with(|u| u.borrow_mut().push(d));
        } else {
            let d = ser_init();
            ABANDONED_SER.with(|s| s.borrow_mut().push(d));
        }
        if locked {
            LOCK.with(|l| l.set(l.get() + 1));
        }
        Throw::Fatal
    }

    /// The actual `BG(serialize_lock)`, `BG(serialize)` and
    /// `BG(unserialize)` state exposed by `zend_test`.
    pub fn serialization_state(&self) -> ZArray {
        let (ser_level, ser_data) = SER.with(|s| {
            let s = s.borrow();
            (s.0, s.1.is_some())
        });
        let (unser_level, unser_data) = UNSER.with(|u| {
            let u = u.borrow();
            (u.0, u.1.is_some())
        });
        ZArray::from_pairs([
            ("lock", Value::Int(i64::from(LOCK.with(Cell::get)))),
            ("serialize_level", Value::Int(i64::from(ser_level))),
            ("serialize_data", Value::Bool(ser_data)),
            ("unserialize_level", Value::Int(i64::from(unser_level))),
            ("unserialize_data", Value::Bool(unser_data)),
        ])
    }
}

/// Runs `f` with `BG(serialize_lock)` raised (user callbacks get a fresh
/// context).
fn locked<R>(f: impl FnOnce() -> VmResult<R>) -> VmResult<R> {
    php_runtime::serialize::serialize_locked(|| {
        LOCK.with(|l| l.set(l.get() + 1));
        let r = f();
        if !matches!(r, Err(Throw::Fatal)) {
            LOCK.with(|l| l.set(l.get().saturating_sub(1)));
        }
        r
    })
}

fn is_locked() -> bool {
    LOCK.with(Cell::get) > 0
}

/// `php_var_serialize_init()`.
fn ser_init() -> SerRc {
    if is_locked() {
        return Rc::default();
    }
    SER.with(|s| {
        let mut s = s.borrow_mut();
        if s.0 == 0 || s.1.is_none() {
            let d: SerRc = Rc::default();
            *s = (1, Some(d.clone()));
            d
        } else {
            s.0 += 1;
            s.1.clone().unwrap_or_default()
        }
    })
}

/// `php_var_serialize_destroy()`.
fn ser_destroy() {
    if is_locked() {
        return;
    }
    SER.with(|s| {
        let mut s = s.borrow_mut();
        s.0 = s.0.saturating_sub(1);
        if s.0 == 0 {
            s.1 = None;
        }
    });
}

// ---------------------------------------------------------------------------
// serialize
// ---------------------------------------------------------------------------

/// `php_add_var_hash()`: 0 for a first occurrence, else the earlier number.
fn add_var_hash(d: &SerRc, var: &Value) -> i64 {
    let mut d = d.borrow_mut();
    d.n += 1;
    let key = match var {
        Value::Reference(r) => match &*r.borrow() {
            Value::Object(o) => o.addr(),
            _ => r.addr(),
        },
        Value::Object(o) => o.addr(),
        _ => return 0,
    };
    if let Some(&id) = d.seen.get(&key) {
        if matches!(var, Value::Reference(_)) && id != -1 {
            d.n -= 1;
        }
        return id;
    }
    let n = d.n;
    d.seen.insert(key, n);
    d.keep.push(var.clone());
    0
}

fn put_long(buf: &mut CheckedBuf, n: i64) {
    buf.extend_from_slice(format!("i:{n};").as_bytes());
}

fn put_string(buf: &mut CheckedBuf, s: &[u8]) {
    buf.extend_from_slice(format!("s:{}:\"", s.len()).as_bytes());
    buf.extend_from_slice(s);
    buf.extend_from_slice(b"\";");
}

fn put_key(buf: &mut CheckedBuf, k: &ArrayKey) {
    match k {
        ArrayKey::Int(i) => put_long(buf, *i),
        ArrayKey::Str(s) => put_string(buf, s.as_bytes()),
    }
}

fn put_class_header(buf: &mut CheckedBuf, name: &[u8]) {
    buf.extend_from_slice(format!("O:{}:\"", name.len()).as_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(b"\":");
}

/// The class name an incomplete object stands for (`php_lookup_class_name`).
fn incomplete_name(o: &Obj) -> Option<ZStr> {
    let d = object::data(o);
    let dynp = d.dynamic.borrow();
    match dynp.as_ref()?.get(&ArrayKey::Str(ZStr::from(MAGIC_MEMBER))) {
        Some(Value::Str(s)) => Some(s.clone()),
        Some(Value::Reference(r)) => match r.get() {
            Value::Str(s) => Some(s),
            _ => None,
        },
        _ => None,
    }
}

fn is_incomplete(cls: &Class) -> bool {
    cls.lc_name.as_bytes() == b"__php_incomplete_class"
}

/// The value written for an element: references nobody else holds are
/// serialized as plain values.
fn element(v: &Value) -> Value {
    match v {
        Value::Reference(r) if r.refcount() == 1 => r.get(),
        other => other.clone(),
    }
}

struct Serializer<'a> {
    /// The output: never grows past `memory_limit` (checked per element).
    buf: CheckedBuf,
    d: &'a SerRc,
}

impl Serializer<'_> {
    /// `php_var_serialize_nested_data()`.
    fn nested(
        &mut self,
        vm: &mut Vm,
        parent: Option<&ZArray>,
        t: &ZArray,
        count: usize,
        mut skip_magic: bool,
    ) -> VmResult<()> {
        self.buf.extend_from_slice(format!("{count}:{{").as_bytes());
        if count > 0 {
            for (k, v) in t.iter() {
                // The output stops growing at `memory_limit`; so does the walk.
                self.buf.check()?;
                if skip_magic
                    && matches!(k, ArrayKey::Str(s) if s.as_bytes() == MAGIC_MEMBER.as_bytes())
                {
                    skip_magic = false;
                    continue;
                }
                put_key(&mut self.buf, k);
                let data = element(v);
                if let (Some(p), Value::Array(a)) = (parent, &data) {
                    if ZArray::ptr_eq(p, a) {
                        add_var_hash(self.d, &Value::Array(p.clone()));
                        self.buf.extend_from_slice(b"N;");
                        continue;
                    }
                }
                self.value(vm, &data)?;
            }
        }
        self.buf.push(b'}');
        Ok(())
    }

    /// `php_var_serialize_intern()`.
    fn value(&mut self, vm: &mut Vm, v: &Value) -> VmResult<()> {
        let Some(_depth) = super::dump::Depth::enter(vm) else {
            return Err(super::dump::stack_size_error(vm));
        };
        let already = add_var_hash(self.d, v);
        if already != 0 {
            if already == -1 {
                self.buf.extend_from_slice(b"N;");
                return Ok(());
            }
            if matches!(v, Value::Reference(_)) {
                self.buf
                    .extend_from_slice(format!("R:{already};").as_bytes());
                return Ok(());
            }
            if matches!(v, Value::Object(_)) {
                self.buf
                    .extend_from_slice(format!("r:{already};").as_bytes());
                return Ok(());
            }
        }
        match v.deref() {
            Value::Bool(false) => self.buf.extend_from_slice(b"b:0;"),
            Value::Bool(true) => self.buf.extend_from_slice(b"b:1;"),
            Value::Null => self.buf.extend_from_slice(b"N;"),
            Value::Int(i) => put_long(&mut self.buf, i),
            Value::Float(f) => {
                let p =
                    i32::try_from(vm.ini_long("serialize_precision").clamp(-1, 1000)).unwrap_or(17);
                self.buf.extend_from_slice(b"d:");
                self.buf
                    .extend_from_slice(zend_gcvt(f, p, b'.', b'E').as_bytes());
                self.buf.push(b';');
            }
            Value::Str(s) => put_string(&mut self.buf, s.as_bytes()),
            Value::Array(a) => {
                self.buf.extend_from_slice(b"a:");
                let a2 = a.clone();
                self.nested(vm, Some(&a2), &a, a.len(), false)?;
            }
            Value::Object(o) => self.object(vm, &o)?,
            Value::Resource(_) | Value::Reference(_) => self.buf.extend_from_slice(b"i:0;"),
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn object(&mut self, vm: &mut Vm, o: &Obj) -> VmResult<()> {
        let cls = object::class_of(o).clone();
        if not_serializable(&cls) {
            return Err(vm.make_throw(
                "Exception",
                &format!(
                    "Serialization of '{}' is not allowed",
                    cls.name.to_string_lossy()
                ),
            ));
        }
        if cls.is_enum() {
            let case = vm.enum_case_name(o).unwrap_or_default();
            self.buf
                .extend_from_slice(format!("E:{}:\"", cls.name.len() + 1 + case.len()).as_bytes());
            self.buf.extend_from_slice(cls.name.as_bytes());
            self.buf.push(b':');
            self.buf.extend_from_slice(case.as_bytes());
            self.buf.extend_from_slice(b"\";");
            return Ok(());
        }
        // `__serialize()` (or a native equivalent).
        let native = cls.hooks.as_ref().and_then(|h| h.serialize);
        if native.is_some() || cls.magic.serialize.is_some() {
            let data = if let Some(m) = cls.magic.serialize.clone() {
                let r = locked(|| vm.call_method(o, &m, Vec::new()))?;
                match r.deref() {
                    Value::Array(a) => Some(a),
                    _ => {
                        return Err(vm.make_throw(
                            "TypeError",
                            &format!(
                                "{}::__serialize() must return an array",
                                object::display_name(&cls.name)
                            ),
                        ))
                    }
                }
            } else if let Some(f) = native {
                locked(|| f(vm, o))?
            } else {
                None
            };
            if let Some(data) = data {
                put_class_header(&mut self.buf, cls.name.as_bytes());
                self.buf
                    .extend_from_slice(format!("{}:{{", data.len()).as_bytes());
                for (k, v) in data.iter() {
                    put_key(&mut self.buf, k);
                    self.value(vm, &element(v))?;
                }
                self.buf.push(b'}');
                return Ok(());
            }
        }
        // `Serializable` (`zend_user_serialize`): no lock, nested
        // `serialize()` calls share this context.
        if cls.instance_of_name(b"serializable") && !is_incomplete(&cls) {
            let r = vm.call_method_name(o, "serialize", Vec::new())?;
            match r {
                Value::Str(data) => {
                    self.buf
                        .extend_from_slice(format!("C:{}:\"", cls.name.len()).as_bytes());
                    self.buf.extend_from_slice(cls.name.as_bytes());
                    self.buf
                        .extend_from_slice(format!("\":{}:{{", data.len()).as_bytes());
                    self.buf.extend_from_slice(data.as_bytes());
                    self.buf.push(b'}');
                }
                Value::Null => {
                    // Later references to this object become `N;`.
                    if let Some(id) = self.d.borrow_mut().seen.get_mut(&o.addr()) {
                        *id = -1;
                    }
                    self.buf.extend_from_slice(b"N;");
                }
                _ => {
                    return Err(vm.make_throw(
                        "Exception",
                        &format!("{}::serialize() must return a string or NULL", cls.name),
                    ))
                }
            }
            return Ok(());
        }
        if !is_incomplete(&cls) {
            if let Some(m) = cls.magic.sleep.clone() {
                let r = locked(|| vm.call_method(o, &m, Vec::new()))?;
                let Value::Array(names) = r.deref() else {
                    vm.docref(
                        E_WARNING,
                        &format!(
                            "{}::__sleep() should return an array only containing the names of instance-variables to serialize",
                            cls.name
                        ),
                    )?;
                    self.buf.extend_from_slice(b"N;");
                    return Ok(());
                };
                let props = Self::sleep_props(vm, o, &cls, &names)?;
                put_class_header(&mut self.buf, cls.name.as_bytes());
                let t = ZArray::from_table(props);
                self.nested(vm, None, &t, t.len(), false)?;
                return Ok(());
            }
        }
        let (name, incomplete) = if is_incomplete(&cls) {
            (
                incomplete_name(o).unwrap_or_else(|| ZStr::from(INCOMPLETE_CLASS)),
                true,
            )
        } else {
            (cls.name.clone(), false)
        };
        put_class_header(&mut self.buf, name.as_bytes());
        let t = serialize_table(vm, o)?;
        let mut count = t.len();
        if count > 0 && incomplete {
            count -= 1;
        }
        self.nested(vm, None, &t, count, incomplete)
    }

    /// `php_var_serialize_get_sleep_props()`.
    fn sleep_props(vm: &mut Vm, o: &Obj, cls: &Rc<Class>, names: &ZArray) -> VmResult<HashTable> {
        let props = serialize_table(vm, o)?;
        let mut out = HashTable::new();
        for (_, nv) in names.iter() {
            let nv = nv.deref();
            if !matches!(nv, Value::Str(_)) {
                vm.docref(
                    E_WARNING,
                    &format!(
                        "{}::__sleep() should return an array only containing the names of instance-variables to serialize",
                        cls.name
                    ),
                )?;
            }
            let name = vm.to_zstr(&nv)?;
            let mut priv_name = vec![0];
            priv_name.extend_from_slice(cls.name.as_bytes());
            priv_name.push(0);
            priv_name.extend_from_slice(name.as_bytes());
            let mut prot_name = b"\0*\0".to_vec();
            prot_name.extend_from_slice(name.as_bytes());
            let mut done = false;
            for cand in [name.as_bytes().to_vec(), priv_name, prot_name] {
                let key = ArrayKey::Str(ZStr::from(cand.as_slice()));
                if let Some(v) = props.get(&key) {
                    if out.get(&key).is_some() {
                        vm.docref(
                            E_WARNING,
                            &format!(
                                "\"{}\" is returned from __sleep() multiple times",
                                name.to_string_lossy()
                            ),
                        )?;
                    } else {
                        out.insert(key, v.clone());
                    }
                    done = true;
                    break;
                }
                if uninit_typed(o, cls, &cand) {
                    done = true;
                    break;
                }
            }
            if !done {
                vm.docref(
                    E_WARNING,
                    &format!(
                        "\"{}\" returned as member variable from __sleep() but does not exist",
                        name.to_string_lossy()
                    ),
                )?;
            }
        }
        Ok(out)
    }
}

/// `zend_get_properties_for(ZEND_PROP_PURPOSE_SERIALIZE)`: an uninitialized
/// lazy object created with `SKIP_INITIALIZATION_ON_SERIALIZE` serializes
/// its raw properties instead of being initialized.
fn serialize_table(vm: &mut Vm, o: &Obj) -> VmResult<ZArray> {
    const SKIP_INITIALIZATION_ON_SERIALIZE: i64 = 8;
    let skip = object::data(o)
        .lazy
        .borrow()
        .as_ref()
        .is_some_and(|i| !i.initialized && i.options & SKIP_INITIALIZATION_ON_SERIALIZE != 0);
    if skip {
        let mut t = HashTable::new();
        for (k, v, _) in vm.object_prop_table(o, false) {
            if let Some(v) = v {
                t.insert(k, v);
            }
        }
        return Ok(ZArray::from_table(t));
    }
    Ok(vm
        .props_for(o, PropertyPurpose::Serialize)?
        .unwrap_or_default())
}

/// Whether the (mangled) name is a declared typed property that is
/// uninitialized (silently skipped by `__sleep()`).
fn uninit_typed(o: &Obj, cls: &Class, mangled: &[u8]) -> bool {
    let Some(pi) = declared_exact(cls, mangled) else {
        return false;
    };
    pi.ty.is_some()
        && object::data(o)
            .slots
            .borrow()
            .get(pi.slot as usize)
            .is_none_or(Option::is_none)
}

/// The declared property whose mangled name is exactly `key`.
fn declared_exact<'c>(cls: &'c Class, key: &[u8]) -> Option<&'c PropInfo> {
    let (c, name) = unmangle_property_name(key);
    cls.props.iter().map(|p| &**p).find(|p| {
        if p.flags & flags::STATIC != 0 || p.name.as_bytes() != name {
            return false;
        }
        let vis = p.visibility();
        match c {
            None => vis == flags::PUBLIC,
            Some(b"*") => vis == flags::PROTECTED,
            Some(cn) => vis == flags::PRIVATE && p.class.as_bytes() == cn,
        }
    })
}

impl Vm {
    /// `PHP_VAR_SERIALIZE_INIT`, one `php_var_serialize()` per value, then
    /// `PHP_VAR_SERIALIZE_DESTROY` (the session serializers'
    /// `PS_ENCODE_LOOP`): the values share one `var_hash`, so later ones
    /// back-reference earlier ones, and a `serialize()` made from a
    /// `Serializable::serialize()` meanwhile joins it (bug #79031). The
    /// values are taken as stored in an array (a reference nobody else
    /// holds is a plain value). Returns one payload per value.
    pub fn serialize_values(&mut self, values: &[&Value]) -> VmResult<Vec<Vec<u8>>> {
        php_runtime::serialize::serialize_isolated(|| {
            let saved_ser = SER.with(|s| std::mem::replace(&mut *s.borrow_mut(), (0, None)));
            let saved_lock = LOCK.with(|l| l.replace(0));
            struct VmSerGuard {
                ser: (u32, Option<SerRc>),
                lock: u32,
            }
            impl Drop for VmSerGuard {
                fn drop(&mut self) {
                    SER.with(|s| *s.borrow_mut() = self.ser.clone());
                    LOCK.with(|l| l.set(self.lock));
                }
            }
            let _guard = VmSerGuard {
                ser: saved_ser,
                lock: saved_lock,
            };
            let d = ser_init();
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                let mut s = Serializer {
                    buf: CheckedBuf::new(),
                    d: &d,
                };
                let r = s.value(self, &element(v)).and_then(|()| Ok(s.buf.check()?));
                match r {
                    Ok(()) => out.push(s.buf.into_vec()?),
                    Err(Throw::Fatal) => {
                        ABANDONED_SER.with(|s| s.borrow_mut().push(d.clone()));
                        return Err(Throw::Fatal);
                    }
                    Err(t) => {
                        ser_destroy();
                        return Err(t);
                    }
                }
            }
            ser_destroy();
            Ok(out)
        })
    }
}

#[allow(clippy::many_single_char_names)]
fn serialize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.values.first().cloned().unwrap_or_default();
    let d = ser_init();
    let mut s = Serializer {
        buf: CheckedBuf::new(),
        d: &d,
    };
    // The root: a by-value argument is never a reference.
    let r = s.value(vm, &v.deref()).and_then(|()| Ok(s.buf.check()?));
    if matches!(r, Err(Throw::Fatal)) {
        ABANDONED_SER.with(|s| s.borrow_mut().push(d.clone()));
        return Err(Throw::Fatal);
    }
    ser_destroy();
    r?;
    Ok(sv(s.buf.into_vec()?))
}

// ---------------------------------------------------------------------------
// unserialize
// ---------------------------------------------------------------------------

/// `zend_is_valid_class_name()`.
fn valid_class_name(name: &[u8]) -> bool {
    name.iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'\\' || c >= 0x80)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn at(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0)
    }

    fn max(&self) -> usize {
        self.data.len()
    }

    /// `uiv = [0-9]+` at `i`; (value, end).
    fn uiv(&self, i: usize) -> Option<(u64, usize)> {
        let mut j = i;
        let mut v: u64 = 0;
        while j < self.data.len() && self.data[j].is_ascii_digit() {
            v = v
                .wrapping_mul(10)
                .wrapping_add(u64::from(self.data[j] - b'0'));
            j += 1;
        }
        (j > i).then_some((v, j))
    }

    /// `parse_iv2()`: optional sign, digits; warns on overflow.
    fn parse_iv2(&self, vm: &mut Vm, i: usize) -> VmResult<(i64, usize)> {
        let mut j = i;
        let neg = self.at(j) == b'-';
        if matches!(self.at(j), b'+' | b'-') {
            j += 1;
        }
        while self.at(j) == b'0' && j < self.data.len() {
            j += 1;
        }
        let start = j;
        let mut v: u128 = 0;
        while j < self.data.len() && self.data[j].is_ascii_digit() {
            v = (v * 10 + u128::from(self.data[j] - b'0')).min(u128::from(u64::MAX) + 1);
            j += 1;
        }
        let limit: u128 = if neg { 1 << 63 } else { (1 << 63) - 1 };
        // MAX_LENGTH_OF_LONG - 1 = 19 digits at most.
        if j - start > 19 || v > limit {
            vm.docref(E_WARNING, "Numerical result out of range")?;
            return Ok((if neg { i64::MIN } else { i64::MAX }, j));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let r = if neg {
            (v as u64).wrapping_neg() as i64
        } else {
            v as i64
        };
        Ok((r, j))
    }

    /// End of a `d:` payload matching `iv | nv | nvexp`.
    fn float_end(&self, i: usize) -> Option<usize> {
        let mut j = i;
        if matches!(self.at(j), b'+' | b'-') {
            j += 1;
        }
        let int_start = j;
        while j < self.data.len() && self.data[j].is_ascii_digit() {
            j += 1;
        }
        let int_digits = j - int_start;
        let mut is_nv = false;
        if self.at(j) == b'.' {
            let f = j + 1;
            let mut k = f;
            while k < self.data.len() && self.data[k].is_ascii_digit() {
                k += 1;
            }
            if int_digits == 0 && k == f {
                return None;
            }
            j = k;
            is_nv = true;
        } else if int_digits == 0 {
            return None;
        }
        let _ = is_nv;
        if matches!(self.at(j), b'e' | b'E') {
            let mut k = j + 1;
            if matches!(self.at(k), b'+' | b'-') {
                k += 1;
            }
            let (_, e) = self.uiv(k)?;
            j = e;
        }
        Some(j)
    }
}

/// What a parse step produced (`return 0` is `None`).
type Step = VmResult<Option<Value>>;

struct Unserializer<'a> {
    cur: Cursor<'a>,
    d: UnserRc,
}

fn warn_depth(vm: &mut Vm, max: i64) -> VmResult<()> {
    vm.docref(
        E_WARNING,
        &format!(
            "Maximum depth of {max} exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting"
        ),
    )
}

impl Unserializer<'_> {
    fn push_dtor(&self, d: Dtor) {
        self.d.borrow_mut().dtors.push(d);
    }

    /// Whether `c` is a cell of this unserialization.
    fn own(&self, c: &PhpRef) -> bool {
        self.d.borrow().cells.contains(&c.addr())
    }

    fn enter_depth(&self, vm: &mut Vm) -> VmResult<bool> {
        let (cur, max) = {
            let d = self.d.borrow();
            (d.cur_depth, d.max_depth)
        };
        if max > 0 && cur >= max {
            warn_depth(vm, max)?;
            return Ok(false);
        }
        self.d.borrow_mut().cur_depth += 1;
        Ok(true)
    }

    fn leave_depth(&self) {
        self.d.borrow_mut().cur_depth -= 1;
    }

    /// An array/object key (`var_hash == NULL`): `i:`, `s:` or `S:` only.
    fn key(&mut self, vm: &mut Vm) -> VmResult<Option<ArrayKey>> {
        let start = self.cur.pos;
        match self.cur.at(start) {
            b'a' if self.cur.at(start + 1) == b':' => {
                // `"a:" uiv ":" "{"` sets the cursor before `!var_hash` fails.
                if let Some((_, end)) = self.cur.uiv(start + 2) {
                    if self.cur.at(end) == b':' && self.cur.at(end + 1) == b'{' {
                        self.cur.pos = end + 2;
                    }
                }
                return Ok(None);
            }
            b'R' | b'r' if self.cur.at(start + 1) == b':' => {
                if let Some((_, end)) = self.cur.uiv(start + 2) {
                    if self.cur.at(end) == b';' {
                        self.cur.pos = end + 1;
                    }
                }
                return Ok(None);
            }
            b'}' => {
                vm.docref(E_WARNING, "Unexpected end of serialized data")?;
                return Ok(None);
            }
            _ => {}
        }
        Ok(match self.scalar(vm, true)? {
            Some(Value::Int(i)) => Some(ArrayKey::Int(i)),
            Some(Value::Str(s)) => Some(ArrayKey::Str(s)),
            _ => None,
        })
    }

    /// `N;`, `b:`, `i:`, `d:`, `s:`, `S:` (shared by keys and values).
    #[allow(clippy::too_many_lines)]
    fn scalar(&mut self, vm: &mut Vm, is_key: bool) -> Step {
        let start = self.cur.pos;
        let tag = self.cur.at(start);
        let colon = self.cur.at(start + 1) == b':';
        match tag {
            b'N' if self.cur.at(start + 1) == b';' => {
                self.cur.pos = start + 2;
                Ok(Some(Value::Null))
            }
            b'b' if colon
                && matches!(self.cur.at(start + 2), b'0' | b'1')
                && self.cur.at(start + 3) == b';' =>
            {
                self.cur.pos = start + 4;
                Ok(Some(Value::Bool(self.cur.at(start + 2) == b'1')))
            }
            b'i' if colon => {
                let mut j = start + 2;
                if matches!(self.cur.at(j), b'+' | b'-') {
                    j += 1;
                }
                let Some((_, end)) = self.cur.uiv(j) else {
                    return Ok(None);
                };
                if self.cur.at(end) != b';' {
                    return Ok(None);
                }
                self.cur.pos = end + 1;
                let (v, _) = self.cur.parse_iv2(vm, start + 2)?;
                Ok(Some(Value::Int(v)))
            }
            b'd' if colon => {
                let rest = &self.cur.data[(start + 2).min(self.cur.max())..];
                for (lit, v) in [
                    (&b"NAN;"[..], f64::NAN),
                    (&b"INF;"[..], f64::INFINITY),
                    (&b"-INF;"[..], f64::NEG_INFINITY),
                ] {
                    if rest.starts_with(lit) {
                        self.cur.pos = start + 2 + lit.len();
                        return Ok(Some(Value::Float(v)));
                    }
                }
                let Some(end) = self.cur.float_end(start + 2) else {
                    return Ok(None);
                };
                if self.cur.at(end) != b';' {
                    return Ok(None);
                }
                self.cur.pos = end + 1;
                let (v, _) = strtod(&self.cur.data[start + 2..end]);
                Ok(Some(Value::Float(v)))
            }
            b's' | b'S' if colon => {
                let Some((len, after)) = self.cur.uiv(start + 2) else {
                    return Ok(None);
                };
                if self.cur.at(after) != b':' || self.cur.at(after + 1) != b'"' {
                    return Ok(None);
                }
                let body = after + 2;
                let maxlen = self.cur.max() - body.min(self.cur.max());
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                if maxlen < len {
                    self.cur.pos = start + 2;
                    return Ok(None);
                }
                let (bytes, end) = if tag == b'S' {
                    match unescape(self.cur.data, body, len) {
                        Some(x) => x,
                        None => return Ok(None),
                    }
                } else {
                    (self.cur.data[body..body + len].to_vec(), body + len)
                };
                if self.cur.at(end) != b'"' {
                    self.cur.pos = end;
                    return Ok(None);
                }
                if self.cur.at(end + 1) != b';' {
                    self.cur.pos = end + 1;
                    return Ok(None);
                }
                self.cur.pos = end + 2;
                if tag == b'S' {
                    vm.docref(E_DEPRECATED, "Unserializing the 'S' format is deprecated")?;
                }
                // `zend_string_finit_fast`: empty and one-byte `s:` values
                // are the interned `ZSTR_EMPTY_ALLOC()` / `ZSTR_CHAR(c)`
                // (keys go through `zend_string_finit_existing_interned`).
                let s = if tag == b's' && !is_key && bytes.len() <= 1 {
                    bytes
                        .first()
                        .map_or_else(ZStr::empty, |&c| ZStr::from_byte(c))
                } else {
                    ZStr::from(bytes)
                };
                Ok(Some(Value::Str(s)))
            }
            _ => Ok(None),
        }
    }

    /// `php_var_unserialize_internal()` with a `var_hash`: returns the
    /// container value (`Value::Reference(cell)`). `loc` is the cell of the
    /// location being overwritten (a duplicate key), which PHP reuses.
    fn value(&mut self, vm: &mut Vm, loc: Option<PhpRef>) -> Step {
        let start = self.cur.pos;
        if start >= self.cur.max() {
            return Ok(None);
        }
        let tag = self.cur.data[start];
        let loc = loc.filter(|c| !self.d.borrow().shared.contains(&c.addr()));
        if let Some(c) = &loc {
            let old = c.set(Value::Null);
            self.push_dtor(Dtor::Keep(old));
        }
        if tag == b'R' {
            return Ok(self.back_ref(start, loc.as_ref()));
        }
        let cell = loc.unwrap_or_else(|| PhpRef::new(Value::Null));
        {
            let mut d = self.d.borrow_mut();
            d.cells.insert(cell.addr());
            d.slots.push(Some(cell.clone()));
        }
        let ok = match tag {
            b'r' => self.obj_ref(start, &cell),
            b'a' => self.array(vm, start, &cell)?,
            b'O' | b'C' => self.object(vm, start, tag == b'C', &cell)?,
            b'E' => self.enum_case(vm, start)?,
            b'}' => {
                vm.docref(E_WARNING, "Unexpected end of serialized data")?;
                None
            }
            _ => self.scalar(vm, false)?,
        };
        Ok(ok.map(|v| {
            if !matches!(tag, b'a' | b'O' | b'C') {
                cell.set(v);
            }
            Value::Reference(cell)
        }))
    }

    /// `"R:" uiv ";"`.
    fn back_ref(&mut self, start: usize, loc: Option<&PhpRef>) -> Option<Value> {
        if self.cur.at(start + 1) != b':' {
            return None;
        }
        let (id, end) = self.cur.uiv(start + 2)?;
        if self.cur.at(end) != b';' {
            return None;
        }
        self.cur.pos = end + 1;
        let target = {
            let d = self.d.borrow();
            usize::try_from(id)
                .ok()
                .filter(|&i| i >= 1)
                .and_then(|i| d.slots.get(i - 1).cloned().flatten())
        };
        let target = target?;
        // A reference to the location being written.
        if loc.is_some_and(|l| PhpRef::ptr_eq(l, &target)) {
            return None;
        }
        self.d.borrow_mut().shared.insert(target.addr());
        Some(Value::Reference(target))
    }

    /// `"r:" uiv ";"`: the same object as an earlier slot.
    fn obj_ref(&mut self, start: usize, cell: &PhpRef) -> Option<Value> {
        if self.cur.at(start + 1) != b':' {
            return None;
        }
        let (id, end) = self.cur.uiv(start + 2)?;
        if self.cur.at(end) != b';' {
            return None;
        }
        self.cur.pos = end + 1;
        let target = {
            let d = self.d.borrow();
            usize::try_from(id)
                .ok()
                .filter(|&i| i >= 1)
                .and_then(|i| d.slots.get(i - 1).cloned().flatten())
        };
        match target {
            Some(t) if PhpRef::ptr_eq(&t, cell) => None,
            Some(t) => match t.get() {
                Value::Object(o) => Some(Value::Object(o)),
                _ => None,
            },
            None => None,
        }
    }

    /// `process_nested_array_data()` into an ordered key list.
    fn nested_array(&mut self, vm: &mut Vm, elements: i64) -> VmResult<Option<HashTable>> {
        if !self.enter_depth(vm)? {
            return Ok(None);
        }
        let r = self.nested_array_inner(vm, elements);
        self.leave_depth();
        r
    }

    fn nested_array_inner(&mut self, vm: &mut Vm, elements: i64) -> VmResult<Option<HashTable>> {
        // `array_finit_size_mixed`: never packed, since `var_hash` may keep
        // pointers to the elements.
        let cap = usize::try_from(elements)
            .unwrap_or(0)
            .min(self.cur.max().saturating_sub(self.cur.pos));
        // The element count is the input's: checked against
        // `memory_limit` before the table for all of them is allocated. Over
        // the limit, the violation is pending and the call fails like the
        // `ZEND_HAS_OOM()` stop below (the fatal follows the call), so the
        // unserializer cleans up as on any failure.
        let Ok(mut t) = HashTable::try_with_capacity_mixed(cap) else {
            return Ok(None);
        };
        let mut remaining = elements;
        while remaining > 0 {
            remaining -= 1;
            let Some(key) = self.key(vm)? else {
                return Ok(None);
            };
            let key = match key {
                ArrayKey::Str(s) => match handle_numeric_str(s.as_bytes()) {
                    Some(i) => ArrayKey::Int(i),
                    None => ArrayKey::Str(s),
                },
                k @ ArrayKey::Int(_) => k,
            };
            let loc = match t.get(&key) {
                Some(Value::Reference(c)) => Some(c.clone()),
                _ => None,
            };
            let Some(v) = self.value(vm, loc)? else {
                return Ok(None);
            };
            t.insert(key, v);
            if remaining > 0 {
                let prev = self.cur.at(self.cur.pos.wrapping_sub(1));
                if prev != b';' && prev != b'}' {
                    self.cur.pos -= 1;
                    return Ok(None);
                }
            }
            // `ZEND_HAS_OOM()`: stop at the element that crossed
            // `memory_limit` (the fatal follows the call).
            if php_memlimit::is_pending() {
                return Ok(None);
            }
        }
        Ok(Some(t))
    }

    fn finish_nested(&mut self) -> bool {
        if self.cur.pos >= self.cur.max() || self.cur.data[self.cur.pos] != b'}' {
            return false;
        }
        self.cur.pos += 1;
        true
    }

    /// `"a:" uiv ":" "{"`.
    fn array(&mut self, vm: &mut Vm, start: usize, cell: &PhpRef) -> Step {
        if self.cur.at(start + 1) != b':' {
            return Ok(None);
        }
        let Some((_, end)) = self.cur.uiv(start + 2) else {
            return Ok(None);
        };
        if self.cur.at(end) != b':' || self.cur.at(end + 1) != b'{' {
            return Ok(None);
        }
        let (elements, _) = self.cur.parse_iv2(vm, start + 2)?;
        self.cur.pos = end + 2;
        let remaining = i64::try_from(self.cur.max() - self.cur.pos).unwrap_or(i64::MAX);
        if !(0..HT_MAX_SIZE).contains(&elements) || elements > remaining / 2 {
            return Ok(None);
        }
        if elements == 0 {
            cell.set(Value::Array(ZArray::new()));
            return Ok(self.finish_nested().then_some(Value::Null));
        }
        let Some(t) = self.nested_array(vm, elements)? else {
            return Ok(None);
        };
        cell.set(Value::Array(ZArray::from_table(t)));
        Ok(self.finish_nested().then_some(Value::Null))
    }

    /// Resolves the class of an `O:`/`C:` entry; `(class, incomplete)`.
    fn resolve_class(&mut self, vm: &mut Vm, name: &ZStr) -> VmResult<Option<(Rc<Class>, bool)>> {
        let ic = || -> Option<Rc<Class>> { None };
        let _ = ic;
        let lc = name.as_bytes().to_ascii_lowercase();
        // A huge class name: the lowercase copy may already have crossed
        // `memory_limit`; php-src's lookup fails the same way
        // (`oom_unserialize_007.phpt`).
        if php_memlimit::is_pending() {
            return Ok(None);
        }
        let allowed = {
            let d = self.d.borrow();
            d.allowed.as_ref().map(|a| a.contains(&lc))
        };
        let incomplete = |vm: &mut Vm| {
            vm.find_class(INCOMPLETE_CLASS.as_bytes())
                .map(|c| (c, true))
        };
        if allowed == Some(false) {
            if !valid_class_name(name.as_bytes()) {
                return Ok(None);
            }
            return Ok(incomplete(vm));
        }
        if let Some(c) = vm.find_class(&lc) {
            if c.flags & cflags::ANON == 0 {
                return Ok(Some((c, false)));
            }
        }
        if !valid_class_name(name.as_bytes()) {
            return Ok(None);
        }
        if let Some(c) = locked(|| vm.lookup_class(name))? {
            return Ok(Some((c, false)));
        }
        let func = vm
            .ini_string("unserialize_callback_func")
            .unwrap_or_default();
        if func.is_empty() {
            return Ok(incomplete(vm));
        }
        let f = Value::Str(ZStr::from(func.as_slice()));
        // `call_user_function()`: `zend_is_callable_ex()` in the calling
        // user frame's scope, deprecations ("Use of \"parent\" in
        // callables …") included.
        let mut call = match vm.zend_is_callable(&f, false)? {
            Ok(call) => call,
            Err(err) => {
                return Err(vm.make_throw(
                    "Error",
                    &format!("Invalid callback {}, {err}", String::from_utf8_lossy(&func)),
                ));
            }
        };
        call.args = vec![Some(Value::Str(name.clone()))];
        locked(|| vm.call_pending(call))?;
        if let Some(c) = locked(|| vm.lookup_class(name))? {
            Ok(Some((c, false)))
        } else {
            vm.docref(
                E_WARNING,
                &format!(
                    "Function {}() hasn't defined the class it was called for",
                    String::from_utf8_lossy(&func)
                ),
            )?;
            Ok(incomplete(vm))
        }
    }

    /// `object ":" uiv ":" ["]`.
    #[allow(clippy::too_many_lines)]
    fn object(&mut self, vm: &mut Vm, start: usize, custom: bool, cell: &PhpRef) -> Step {
        if self.cur.at(start + 1) != b':' {
            return Ok(None);
        }
        let Some((len, after)) = self.cur.uiv(start + 2) else {
            return Ok(None);
        };
        if self.cur.at(after) != b':' || self.cur.at(after + 1) != b'"' {
            return Ok(None);
        }
        let body = after + 2;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let maxlen = self.cur.max().saturating_sub(body);
        if maxlen < len || len == 0 {
            self.cur.pos = start + 2;
            return Ok(None);
        }
        let end = body + len;
        if self.cur.at(end) != b'"' {
            self.cur.pos = end;
            return Ok(None);
        }
        if self.cur.at(end + 1) != b':' {
            self.cur.pos = end + 1;
            return Ok(None);
        }
        let name_bytes = &self.cur.data[body..end];
        if name_bytes[0] == 0 || name_bytes[0] == b'\\' {
            return Ok(None);
        }
        let name = ZStr::from(name_bytes);
        let Some((cls, incomplete)) = self.resolve_class(vm, &name)? else {
            return Ok(None);
        };
        self.cur.pos = end;
        if not_serializable(&cls) {
            return Err(vm.make_throw(
                "Exception",
                &format!("Unserialization of '{}' is not allowed", cls.name),
            ));
        }
        if custom {
            return self.object_custom(vm, &cls, incomplete, &name, cell);
        }
        if self.cur.pos >= self.cur.max().saturating_sub(2) {
            vm.warning("Bad unserialize data")?;
            return Ok(None);
        }
        let (elements, n_end) = self.cur.parse_iv2(vm, self.cur.pos + 2)?;
        self.cur.pos = n_end;
        let remaining = i64::try_from(self.cur.max().saturating_sub(end + 2)).unwrap_or(i64::MAX);
        if elements < 0 || elements > remaining / 2 {
            return Ok(None);
        }
        if self.cur.at(n_end) != b':' {
            return Ok(None);
        }
        if self.cur.at(n_end + 1) != b'{' {
            self.cur.pos = n_end + 1;
            return Ok(None);
        }
        self.cur.pos = n_end + 2;
        // `ce->__unserialize`: a native `unserialize` hook only stands in
        // for the method (it declines without one), so SPL classes such as
        // `RegexIterator` restore plain properties (gc_043).
        let has_unserialize = !incomplete && cls.magic.unserialize.is_some();
        if !incomplete && cls.instance_of_name(b"serializable") && !has_unserialize {
            vm.warning(&format!(
                "Erroneous data format for unserializing '{}'",
                cls.name
            ))?;
            return Ok(None);
        }
        let o = vm.instantiate(&cls)?;
        cell.set(Value::Object(o.clone()));
        self.d.borrow_mut().objects.push(o.clone());
        if incomplete {
            vm.write_dynamic_prop(&o, ZStr::from(MAGIC_MEMBER), Value::Str(name));
        }
        self.object_common(vm, &o, elements, has_unserialize)
    }

    /// `object_common()`.
    fn object_common(
        &mut self,
        vm: &mut Vm,
        o: &Obj,
        elements: i64,
        has_unserialize: bool,
    ) -> Step {
        let cls = object::class_of(o).clone();
        if has_unserialize {
            if elements >= HT_MAX_SIZE {
                return Ok(None);
            }
            let t = self.nested_array(vm, elements)?;
            let ok = t.is_some() && self.finish_nested();
            let ary = Value::Array(ZArray::from_table(t.unwrap_or_default()));
            if !ok {
                mark_destructed(o);
                self.push_dtor(Dtor::Keep(ary));
                return Ok(None);
            }
            self.push_dtor(Dtor::Unserialize(o.clone(), ary));
            return Ok(Some(Value::Null));
        }
        let has_wakeup = !is_incomplete(&cls) && cls.magic.wakeup.is_some();
        let ok = self.nested_object(vm, o, elements)? && self.finish_nested();
        if !ok {
            if has_wakeup {
                mark_destructed(o);
            }
            return Ok(None);
        }
        if has_wakeup {
            self.push_dtor(Dtor::Wakeup(o.clone()));
        }
        Ok(Some(Value::Null))
    }

    /// `process_nested_object_data()`.
    fn nested_object(&mut self, vm: &mut Vm, o: &Obj, elements: i64) -> VmResult<bool> {
        if !self.enter_depth(vm)? {
            return Ok(false);
        }
        let r = self.nested_object_inner(vm, o, elements);
        self.leave_depth();
        r
    }

    #[allow(clippy::too_many_lines, clippy::many_single_char_names)]
    fn nested_object_inner(&mut self, vm: &mut Vm, o: &Obj, elements: i64) -> VmResult<bool> {
        let cls = object::class_of(o).clone();
        let mut remaining = elements;
        while remaining > 0 {
            remaining -= 1;
            let Some(key) = self.key(vm)? else {
                return Ok(false);
            };
            let key = match key {
                ArrayKey::Str(s) => s,
                ArrayKey::Int(i) => ZStr::from(i.to_string()),
            };
            let Some(target) = Self::prop_target(vm, o, &cls, &key)? else {
                return Ok(false);
            };
            let loc = match &target {
                Target::Declared(pi) => {
                    match object::data(o).slots.borrow().get(pi.slot as usize) {
                        Some(Some(Value::Reference(c))) if self.own(c) => Some(c.clone()),
                        _ => None,
                    }
                }
                Target::Dynamic(k) => match object::data(o)
                    .dynamic
                    .borrow()
                    .as_ref()
                    .and_then(|t| t.get(&ArrayKey::Str(k.clone())).cloned())
                {
                    Some(Value::Reference(c)) if self.own(&c) => Some(c),
                    _ => None,
                },
            };
            // A reused location (duplicate key) drops its old value, and
            // with it this slot's type source.
            let reused = loc.is_some();
            let Some(v) = self.value(vm, loc)? else {
                return Ok(false);
            };
            match &target {
                Target::Declared(pi) => {
                    let cell = match &v {
                        Value::Reference(c) => Some(c.clone()),
                        _ => None,
                    };
                    let typed_ref = !reused
                        && cell
                            .as_ref()
                            .is_some_and(crate::typed_refs::has_type_sources);
                    if typed_ref && pi.ty.is_some() {
                        // `R:` to a value an earlier typed property holds:
                        // the reference must satisfy this type too,
                        // without coercion.
                        let info = cls
                            .props
                            .get(pi.slot as usize)
                            .cloned()
                            .unwrap_or_else(|| Rc::new(pi.clone()));
                        let mut checked = v.clone();
                        vm.verify_prop_assignable_by_ref_strict(&info, &mut checked)?;
                    } else if let Some(ty) = pi.ty.clone() {
                        let mut inner = v.deref();
                        let was_int = matches!(inner, Value::Int(_));
                        let decl = vm.find_class(pi.class.as_bytes());
                        let ctx = TypeCtx {
                            scope: decl.as_ref(),
                            called: Some(&cls),
                            strict: true,
                            internal_arg: None,
                        };
                        if !vm.verify_type(&ty, &mut inner, &ctx)? {
                            let tyname = ty.display(
                                decl.as_ref().map(|c| &c.name),
                                decl.as_ref().and_then(|c| c.parent_name()),
                                None,
                            );
                            return Err(vm.make_throw(
                                "TypeError",
                                &format!(
                                    "Cannot assign {} to property {}::${} of type {tyname}",
                                    given_type(&inner),
                                    object::display_name(&pi.class),
                                    pi.name
                                ),
                            ));
                        }
                        // The strict-mode int -> float coercion is kept.
                        if was_int && matches!(inner, Value::Float(_)) {
                            if let Some(c) = &cell {
                                c.set(inner);
                            }
                        }
                    }
                    let d = object::data(o);
                    let mut slots = d.slots.borrow_mut();
                    if let Some(s) = slots.get_mut(pi.slot as usize) {
                        let old = s.replace(v);
                        drop(slots);
                        if let Some(old) = old.filter(|o| !matches!(o, Value::Reference(_))) {
                            self.push_dtor(Dtor::Keep(old));
                        }
                    } else {
                        drop(slots);
                    }
                    d.set_slot_unset(pi.slot as usize, true);
                    // `ZEND_REF_ADD_TYPE_SOURCE` / `ref_props`: the parse
                    // cell becomes a typed reference if a later `R:`
                    // shares it (a source only counts while the slot
                    // still holds the cell, i.e. after unwrapping only
                    // for shared cells).
                    if let Some(c) = &cell {
                        vm.add_slot_type_source(o, pi.slot as usize, c);
                    }
                }
                Target::Dynamic(k) => {
                    let d = object::data(o);
                    let mut dynp = d.dynamic.borrow_mut();
                    let t = dynp.get_or_insert_with(Box::default);
                    let key = ArrayKey::Str(k.clone());
                    let old = t.get(&key).cloned();
                    t.insert(key, v);
                    drop(dynp);
                    if let Some(old) = old.filter(|o| !matches!(o, Value::Reference(_))) {
                        self.push_dtor(Dtor::Keep(old));
                    }
                }
            }
            if remaining > 0 {
                let prev = self.cur.at(self.cur.pos.wrapping_sub(1));
                if prev != b';' && prev != b'}' {
                    self.cur.pos -= 1;
                    return Ok(false);
                }
            }
            if php_memlimit::is_pending() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Where a serialized property goes (`process_nested_object_data` +
    /// `is_property_visibility_changed`); `None` fails the parse.
    fn prop_target(vm: &mut Vm, o: &Obj, cls: &Rc<Class>, key: &ZStr) -> VmResult<Option<Target>> {
        if let Some(pi) = declared_exact(cls, key.as_bytes()) {
            if pi.is_virtual() {
                vm.docref(
                    E_WARNING,
                    &format!(
                        "Cannot unserialize value for virtual property {}::${}",
                        pi.class, pi.name
                    ),
                )?;
                return Ok(None);
            }
            return Ok(Some(Target::Declared(pi.clone())));
        }
        // Visibility changed since serialization?
        let (c, name) = unmangle_property_name(key.as_bytes());
        let candidate = match c {
            None => true,
            Some(cn) => cn == b"*" || cn.eq_ignore_ascii_case(cls.name.as_bytes()),
        };
        if candidate {
            let n = ZStr::from(name);
            // `ce->properties_info` also holds the private properties a
            // class inherits (under their plain name, nearest declaration
            // first), which `prop_map` leaves out.
            if let Some(pi) = cls
                .prop_map
                .get(&n)
                .filter(|p| p.flags & flags::STATIC == 0)
                .or_else(|| {
                    cls.props
                        .iter()
                        .rev()
                        .find(|p| p.flags & flags::STATIC == 0 && p.name.as_bytes() == name)
                })
            {
                if pi.is_virtual() {
                    vm.docref(
                        E_WARNING,
                        &format!(
                            "Cannot unserialize value for virtual property {}::${}",
                            pi.class,
                            key.to_string_lossy()
                        ),
                    )?;
                    return Ok(None);
                }
                return Ok(Some(Target::Declared((**pi).clone())));
            }
        }
        let exists = object::data(o)
            .dynamic
            .borrow()
            .as_ref()
            .is_some_and(|t| t.get(&ArrayKey::Str(key.clone())).is_some());
        if !exists && !is_incomplete(cls) && cls.lc_name.as_bytes() != b"stdclass" {
            let shown = String::from_utf8_lossy(name).into_owned();
            // A readonly class is `ZEND_ACC_NO_DYNAMIC_PROPERTIES` (GH-9186).
            if cls.flags & (cflags::NO_DYNAMIC_PROPERTIES | cflags::READONLY) != 0 {
                return Err(vm.make_throw(
                    "Error",
                    &format!("Cannot create dynamic property {}::${shown}", cls.name),
                ));
            }
            if cls.flags & cflags::ALLOW_DYNAMIC == 0 {
                vm.error(
                    E_DEPRECATED,
                    &format!(
                        "Creation of dynamic property {}::${shown} is deprecated",
                        cls.name
                    ),
                )?;
            }
        }
        Ok(Some(Target::Dynamic(key.clone())))
    }

    /// `object_custom()` (`C:` entries).
    fn object_custom(
        &mut self,
        vm: &mut Vm,
        cls: &Rc<Class>,
        incomplete: bool,
        name: &ZStr,
        cell: &PhpRef,
    ) -> Step {
        let (datalen, p) = self.cur.parse_iv2(vm, self.cur.pos + 2)?;
        self.cur.pos = p;
        if self.cur.max() - self.cur.pos.min(self.cur.max()) < 2 || self.cur.at(p) != b':' {
            return Ok(None);
        }
        if self.cur.at(p + 1) != b'{' {
            self.cur.pos += 1;
            return Ok(None);
        }
        self.cur.pos += 2;
        let avail = self.cur.max() - self.cur.pos;
        let dlen = usize::try_from(datalen).unwrap_or(usize::MAX);
        if datalen < 0 || avail <= dlen {
            vm.warning(&format!(
                "Insufficient data for unserializing - {datalen} required, {avail} present"
            ))?;
            return Ok(None);
        }
        if self.cur.data[self.cur.pos + dlen] != b'}' {
            self.cur.pos += dlen;
            return Ok(None);
        }
        let payload = ZStr::from(&self.cur.data[self.cur.pos..self.cur.pos + dlen]);
        if incomplete || !has_custom_unserializer(cls) {
            vm.warning(&format!("Class {} has no unserializer", cls.name))?;
            if !is_incomplete(cls) {
                return Ok(None);
            }
            let o = vm.instantiate(cls)?;
            cell.set(Value::Object(o.clone()));
            self.d.borrow_mut().objects.push(o.clone());
            if incomplete {
                vm.write_dynamic_prop(&o, ZStr::from(MAGIC_MEMBER), Value::Str(name.clone()));
            }
        } else {
            let o = vm.instantiate(cls)?;
            cell.set(Value::Object(o.clone()));
            self.d.borrow_mut().objects.push(o.clone());
            // `zend_user_unserialize`: no lock, nested unserialize() calls
            // share this context. An internal class's own `ce->unserialize`
            // (`gmp_unserialize`) is a C callback, not a method call: it
            // pushes no frame, so its exceptions' traces start at
            // `unserialize()` (bug #70284).
            match native_unserializer(cls) {
                Some(b) => {
                    let mut args = Args {
                        values: vec![Value::Str(payload)],
                        passed: vec![true],
                        named: Vec::new(),
                        this: Some(o.clone()),
                        called: Some(cls.clone()),
                        func: Some(b.clone()),
                    };
                    (b.handler)(vm, &mut args)?;
                }
                None => {
                    vm.call_method_name(&o, "unserialize", vec![Value::Str(payload)])?;
                }
            }
        }
        self.cur.pos += dlen + 1;
        Ok(Some(Value::Null))
    }

    /// `"E:" uiv ":" ["]`.
    fn enum_case(&mut self, vm: &mut Vm, start: usize) -> Step {
        if self.cur.at(start + 1) != b':' {
            return Ok(None);
        }
        let Some((len, after)) = self.cur.uiv(start + 2) else {
            return Ok(None);
        };
        if self.cur.at(after) != b':' || self.cur.at(after + 1) != b'"' {
            return Ok(None);
        }
        let body = after + 2;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if self.cur.max().saturating_sub(body) < len || len == 0 {
            self.cur.pos = start + 2;
            return Ok(None);
        }
        let end = body + len;
        if self.cur.at(end) != b'"' {
            self.cur.pos = end;
            return Ok(None);
        }
        if self.cur.at(end + 1) != b';' {
            self.cur.pos = end + 1;
            return Ok(None);
        }
        let text = &self.cur.data[body..end];
        let Some(colon) = text.iter().position(|&c| c == b':') else {
            vm.docref(
                E_WARNING,
                &format!(
                    "Invalid enum name '{}' (missing colon)",
                    String::from_utf8_lossy(text)
                ),
            )?;
            return Ok(None);
        };
        let enum_name = ZStr::from(&text[..colon]);
        let case = ZStr::from(&text[colon + 1..]);
        if !valid_class_name(enum_name.as_bytes()) {
            return Ok(None);
        }
        let Some(cls) = locked(|| vm.lookup_class(&enum_name))? else {
            vm.docref(
                E_WARNING,
                &format!("Class '{}' not found", enum_name.to_string_lossy()),
            )?;
            return Ok(None);
        };
        if !cls.is_enum() {
            vm.docref(
                E_WARNING,
                &format!("Class '{}' is not an enum", enum_name.to_string_lossy()),
            )?;
            return Ok(None);
        }
        self.cur.pos = end + 2;
        let Some(c) = cls.find_const(case.as_bytes()).cloned() else {
            vm.docref(
                E_WARNING,
                &format!(
                    "Undefined constant {}::{}",
                    enum_name.to_string_lossy(),
                    case.to_string_lossy()
                ),
            )?;
            return Ok(None);
        };
        if !c.is_case {
            vm.docref(
                E_WARNING,
                &format!(
                    "{}::{} is not an enum case",
                    enum_name.to_string_lossy(),
                    case.to_string_lossy()
                ),
            )?;
            return Ok(None);
        }
        Ok(Some(locked(|| vm.enum_case(&cls, &case))?))
    }
}

enum Target {
    Declared(PropInfo),
    Dynamic(ZStr),
}

/// `ce->unserialize != NULL`: `Serializable` implementations, and internal
/// classes with a native `C:` unserializer (e.g. GMP).
fn has_custom_unserializer(cls: &Rc<Class>) -> bool {
    cls.instance_of_name(b"serializable")
        || (cls.flags & cflags::INTERNAL != 0 && cls.find_method(b"unserialize").is_some())
}

/// The native `C:` unserializer of an internal class that is not
/// `Serializable` (GMP's `ce->unserialize`), called without a frame.
fn native_unserializer(cls: &Rc<Class>) -> Option<Rc<crate::func::Builtin>> {
    if cls.flags & cflags::INTERNAL == 0 || cls.instance_of_name(b"serializable") {
        return None;
    }
    match &cls.find_method(b"unserialize")?.func {
        crate::func::Func::Builtin(b) => Some(b.clone()),
        crate::func::Func::User(_) => None,
    }
}

/// `ZEND_ACC_NOT_SERIALIZABLE` (inherited; anonymous classes have it).
fn not_serializable(cls: &Rc<Class>) -> bool {
    let mut c = Some(cls);
    while let Some(k) = c {
        if k.flags & (cflags::NOT_SERIALIZABLE | cflags::ANON) != 0 {
            return true;
        }
        c = k.parent.as_ref();
    }
    false
}

fn mark_destructed(o: &Obj) {
    let d = object::data(o);
    d.flags.set(d.flags.get() | object::oflags::DESTRUCTED);
}

/// Decodes the escaped `S:` format.
fn unescape(data: &[u8], body: usize, len: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(len);
    let mut i = body;
    for _ in 0..len {
        let c = *data.get(i)?;
        if c == b'\\' {
            let hi = char::from(*data.get(i + 1)?).to_digit(16)?;
            let lo = char::from(*data.get(i + 2)?).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Some((out, i))
}

// ---------------------------------------------------------------------------
// Unwrapping cells
// ---------------------------------------------------------------------------

/// Replaces cells only their container holds (plus the `var_hash` entry)
/// by their values, recursively.
struct Unwrapper {
    seen_refs: HashSet<usize>,
    seen_objs: HashSet<usize>,
    /// Handles each cell has in the `var_hash` (a reused location is
    /// registered once per value parsed into it).
    slot_counts: HashMap<usize, usize>,
}

/// Drops parse-cell handles without offering them to the cycle collector's
/// possible-root buffer. php-src's `var_hash` holds no refcounts, so its
/// teardown never buffers roots; here a buffered weak handle would also
/// keep the cell's allocation alive until the buffer is compacted, which
/// `memory_get_usage()` sees as a leak (bug #81142). Shared cells are
/// buffered when the user later releases the result.
fn without_root_recording<R>(f: impl FnOnce() -> R) -> R {
    let prev = php_runtime::ref_roots::set_recording(false);
    let r = f();
    php_runtime::ref_roots::set_recording(prev);
    r
}

impl Unwrapper {
    /// An unwrapper for the cells of context `d` (none: only lone cells are
    /// unwrapped), counting each cell's `var_hash` entries.
    fn new(d: Option<&UnserRc>) -> Self {
        let mut slot_counts = HashMap::new();
        if let Some(d) = d {
            for c in d.borrow().slots.iter().flatten() {
                *slot_counts.entry(c.addr()).or_insert(0) += 1;
            }
        }
        Self {
            seen_refs: HashSet::new(),
            seen_objs: HashSet::new(),
            slot_counts,
        }
    }

    /// Unwraps what the delayed calls of `d` will see: the properties of
    /// every object created and the `__unserialize()` data arrays.
    fn pending(&mut self, d: &UnserRc) {
        let objects = std::mem::take(&mut d.borrow_mut().objects);
        for o in &objects {
            self.object(o);
        }
        let mut dtors = std::mem::take(&mut d.borrow_mut().dtors);
        for dt in &mut dtors {
            if let Dtor::Unserialize(_, data) = dt {
                self.value(data);
            }
        }
        d.borrow_mut().dtors = dtors;
    }

    fn value(&mut self, v: &mut Value) {
        match v {
            Value::Reference(r) => {
                let in_table = self.slot_counts.get(&r.addr()).copied().unwrap_or(0);
                if r.refcount() <= in_table + 1 {
                    let mut inner = r.set(Value::Null);
                    self.value(&mut inner);
                    r.set(inner.clone());
                    without_root_recording(|| *v = inner);
                } else if self.seen_refs.insert(r.addr()) {
                    let mut inner = r.set(Value::Null);
                    self.value(&mut inner);
                    r.set(inner);
                }
            }
            Value::Array(a) => {
                let t = a.make_mut();
                for (_, e) in t.iter_mut() {
                    self.value(e);
                }
            }
            Value::Object(o) => self.object(o),
            _ => {}
        }
    }

    fn object(&mut self, o: &Obj) {
        if !self.seen_objs.insert(o.addr()) {
            return;
        }
        let d = object::data(o);
        let n = d.slots.borrow().len();
        for i in 0..n {
            let taken = d.slots.borrow_mut()[i].take();
            if let Some(mut v) = taken {
                self.value(&mut v);
                d.slots.borrow_mut()[i] = Some(v);
            }
        }
        let dynp = d.dynamic.borrow_mut().take();
        if let Some(mut t) = dynp {
            for (_, e) in t.iter_mut() {
                self.value(e);
            }
            *d.dynamic.borrow_mut() = Some(t);
        }
    }
}

// ---------------------------------------------------------------------------
// unserialize()
// ---------------------------------------------------------------------------

/// `php_var_unserialize_init()`; `(data, is_outermost)`.
fn unser_init(vm: &Vm) -> (UnserRc, bool) {
    let fresh = || {
        Rc::new(RefCell::new(UnserData {
            slots: Vec::new(),
            dtors: Vec::new(),
            allowed: None,
            cur_depth: 0,
            max_depth: vm.ini_long("unserialize_max_depth"),
            objects: Vec::new(),
            shared: HashSet::new(),
            cells: HashSet::new(),
        }))
    };
    if is_locked() {
        return (fresh(), true);
    }
    UNSER.with(|u| {
        let mut u = u.borrow_mut();
        if u.0 == 0 || u.1.is_none() {
            let d = fresh();
            *u = (1, Some(d.clone()));
            (d, true)
        } else {
            u.0 += 1;
            (u.1.clone().unwrap_or_else(fresh), false)
        }
    })
}

impl Vm {
    /// Whether a `php_var_unserialize()` made by native code now joins an
    /// active `unserialize()` call's `var_hash` (`PHP_VAR_UNSERIALIZE_INIT`
    /// sharing `BG(unserialize)`): inside one and not under the serialize
    /// lock.
    #[must_use]
    pub fn unserialize_context_active(&self) -> bool {
        !is_locked()
            && UNSER.with(|u| {
                let u = u.borrow();
                u.0 > 0 && u.1.is_some()
            })
    }

    /// The `var_hash` options a `php_var_unserialize()` made by native code
    /// (a native `Serializable::unserialize()` such as `ArrayObject`'s legacy
    /// format, `session_decode()`) runs with: those of the active
    /// `unserialize()` call (`allowed_classes`, `max_depth`, and the depth
    /// reached so far), or the `unserialize_max_depth` ini setting outside
    /// one (or under the serialize lock, where a fresh context is used).
    /// Diagnostics name the active builtin function, like
    /// `php_error_docref()`.
    #[must_use]
    pub fn unserialize_options_in_effect(&self) -> UnserializeOptions {
        let function: std::borrow::Cow<'static, str> =
            std::borrow::Cow::Owned(self.docref_function_name().unwrap_or_default());
        let active = if is_locked() {
            None
        } else {
            UNSER.with(|u| {
                let u = u.borrow();
                let d = u.1.as_ref().filter(|_| u.0 > 0)?.borrow();
                Some(UnserializeOptions {
                    max_depth: UnserializeOptions::max_depth_from_ini(d.max_depth),
                    allowed_classes: d
                        .allowed
                        .clone()
                        .map_or(AllowedClasses::All, AllowedClasses::Only),
                    depth: usize::try_from(d.cur_depth).unwrap_or(0),
                    function: function.clone(),
                    ..UnserializeOptions::default()
                })
            })
        };
        active.unwrap_or_else(|| UnserializeOptions {
            max_depth: UnserializeOptions::max_depth_from_ini(
                self.ini_long("unserialize_max_depth"),
            ),
            function,
            ..UnserializeOptions::default()
        })
    }
}

/// `php_var_unserialize_destroy()`: the outermost call runs the delayed
/// `__wakeup()` / `__unserialize()` calls.
fn unser_destroy(vm: &mut Vm, d: &UnserRc, outermost: bool, run: bool) -> VmResult<()> {
    if !is_locked() {
        UNSER.with(|u| {
            let mut u = u.borrow_mut();
            u.0 = u.0.saturating_sub(1);
            if u.0 == 0 {
                u.1 = None;
            }
        });
    }
    if !outermost {
        return Ok(());
    }
    let dtors = std::mem::take(&mut d.borrow_mut().dtors);
    without_root_recording(|| d.borrow_mut().slots.clear());
    // With an exception pending the calls do not run (and count as failed).
    let mut failed = !run;
    let mut result = Ok(());
    let mut dtors = dtors.into_iter();
    while let Some(dt) = dtors.next() {
        match dt {
            Dtor::Keep(_) => {}
            Dtor::Wakeup(o) => {
                if failed {
                    mark_destructed(&o);
                    continue;
                }
                if php_memlimit::is_pending() {
                    return abandon_for_oom(vm, d, dtors);
                }
                let cls = object::class_of(&o).clone();
                if let Some(m) = cls.magic.wakeup.clone() {
                    if let Err(t) = locked(|| vm.call_method(&o, &m, Vec::new())) {
                        if matches!(t, Throw::Fatal) {
                            d.borrow_mut().dtors.extend(dtors);
                            ABANDONED_UNSER.with(|u| u.borrow_mut().push(d.clone()));
                            return Err(t);
                        }
                        failed = true;
                        mark_destructed(&o);
                        result = Err(t);
                    }
                }
            }
            Dtor::Unserialize(o, data) => {
                if failed {
                    mark_destructed(&o);
                    continue;
                }
                if php_memlimit::is_pending() {
                    return abandon_for_oom(vm, d, dtors);
                }
                let cls = object::class_of(&o).clone();
                let r = if let Some(m) = cls.magic.unserialize.clone() {
                    // Readonly properties may be initialized once more
                    // (`zend_object_set_properties_reinitable`).
                    locked(|| vm.with_reinitable_props(&o, |vm| vm.call_method(&o, &m, vec![data])))
                        .map(|_| ())
                } else if let Some(f) = cls.hooks.as_ref().and_then(|h| h.unserialize) {
                    let Value::Array(a) = data else { continue };
                    locked(|| f(vm, &o, a)).map(|_| ())
                } else {
                    Ok(())
                };
                if let Err(t) = r {
                    if matches!(t, Throw::Fatal) {
                        d.borrow_mut().dtors.extend(dtors);
                        ABANDONED_UNSER.with(|u| u.borrow_mut().push(d.clone()));
                        return Err(t);
                    }
                    failed = true;
                    mark_destructed(&o);
                    result = Err(t);
                }
            }
        }
    }
    result
}

/// A delayed `__wakeup()`/`__unserialize()` call with an OOM pending:
/// `zend_call_function()` raises the fatal instead of calling, and the
/// bailout leaves the rest of the `var_hash` behind (`var_destroy` in
/// `oom_call_function_003.phpt`).
#[cold]
fn abandon_for_oom(vm: &mut Vm, d: &UnserRc, rest: impl Iterator<Item = Dtor>) -> VmResult<()> {
    d.borrow_mut().dtors.extend(rest);
    ABANDONED_UNSER.with(|u| u.borrow_mut().push(d.clone()));
    vm.raise_pending_oom()?;
    Err(Throw::Fatal)
}

/// Parses `options` (`allowed_classes`, `max_depth`); `func` names the
/// caller in the errors.
fn apply_options(vm: &mut Vm, d: &UnserRc, opts: &ZArray, func: &str) -> VmResult<()> {
    if let Some(classes) = opts.get(&ArrayKey::Str(ZStr::from("allowed_classes"))) {
        let classes = classes.deref();
        match &classes {
            Value::Array(list) => {
                let mut set = HashSet::new();
                for (_, e) in list.iter() {
                    let e = e.deref();
                    if !matches!(e, Value::Str(_) | Value::Object(_)) {
                        return Err(vm.make_throw(
                            "TypeError",
                            &format!(
                                "{func}(): Option \"allowed_classes\" must be an array of class names, {} given",
                                given_type(&e)
                            ),
                        ));
                    }
                    let n = vm.to_zstr(&e)?;
                    if !valid_class_name(n.as_bytes()) {
                        return Err(vm.make_throw(
                            "ValueError",
                            &format!(
                                "{func}(): Option \"allowed_classes\" must be an array of class names, \"{}\" given",
                                n.to_string_lossy()
                            ),
                        ));
                    }
                    set.insert(n.as_bytes().to_ascii_lowercase());
                }
                d.borrow_mut().allowed = Some(set);
            }
            Value::Bool(true) => {}
            Value::Bool(false) => d.borrow_mut().allowed = Some(HashSet::new()),
            other => {
                return Err(vm.make_throw(
                    "TypeError",
                    &format!(
                        "{func}(): Option \"allowed_classes\" must be of type array|bool, {} given",
                        given_type(other)
                    ),
                ))
            }
        }
    }
    if let Some(md) = opts.get(&ArrayKey::Str(ZStr::from("max_depth"))) {
        let Value::Int(n) = md.deref() else {
            return Err(vm.make_throw(
                "TypeError",
                &format!(
                    "{func}(): Option \"max_depth\" must be of type int, {} given",
                    given_type(&md.deref())
                ),
            ));
        };
        if n < 0 {
            return Err(vm.make_throw(
                "ValueError",
                &format!("{func}(): Option \"max_depth\" must be greater than or equal to 0"),
            ));
        }
        let mut d = d.borrow_mut();
        d.max_depth = n;
        d.cur_depth = 0;
    }
    Ok(())
}

/// `php_unserialize_with_options()`; `func` names the caller in option
/// errors (diagnostics name the running builtin, like `php_error_docref`).
fn unserialize_with_options(
    vm: &mut Vm,
    buf: &[u8],
    opts: Option<&ZArray>,
    func: &str,
) -> VmResult<Value> {
    let (d, outermost) = unser_init(vm);
    let prev = {
        let b = d.borrow();
        (b.allowed.clone(), b.max_depth, b.cur_depth)
    };
    let restore = |d: &UnserRc| {
        let mut b = d.borrow_mut();
        b.allowed.clone_from(&prev.0);
        b.max_depth = prev.1;
        b.cur_depth = prev.2;
    };
    if let Some(o) = opts {
        if let Err(t) = apply_options(vm, &d, o, func) {
            restore(&d);
            let _ = unser_destroy(vm, &d, outermost, false);
            return Err(t);
        }
    }
    let first_slot = d.borrow().slots.len();
    let mut u = Unserializer {
        cur: Cursor { data: buf, pos: 0 },
        d: d.clone(),
    };
    let parsed = u.value(vm, None);
    if matches!(parsed, Err(Throw::Fatal)) {
        ABANDONED_UNSER.with(|u| u.borrow_mut().push(d));
        return Err(Throw::Fatal);
    }
    let pos = u.cur.pos;
    let result = match parsed {
        Ok(Some(v)) => {
            if pos < buf.len() {
                vm.docref(
                    E_WARNING,
                    &format!("Extra data starting at offset {pos} of {} bytes", buf.len()),
                )?;
            }
            Ok(Some(v))
        }
        Ok(None) => {
            // Forbid later references to the slots of the failed part.
            for s in d.borrow_mut().slots.iter_mut().skip(first_slot) {
                *s = None;
            }
            vm.docref(
                E_WARNING,
                &format!("Error at offset {pos} of {} bytes", buf.len()),
            )?;
            Ok(None)
        }
        Err(t) => {
            for s in d.borrow_mut().slots.iter_mut().skip(first_slot) {
                *s = None;
            }
            Err(t)
        }
    };
    restore(&d);
    // Unwrap cells nobody shares, in the result and in everything the
    // delayed calls will see.
    let mut value = match result {
        Ok(Some(v)) => Some(v),
        Ok(None) => None,
        Err(t) => {
            let _ = unser_destroy(vm, &d, outermost, false);
            return Err(t);
        }
    };
    let mut uw = Unwrapper::new(Some(&d));
    if let Some(v) = value.as_mut() {
        uw.value(v);
    }
    if outermost {
        uw.pending(&d);
    }
    unser_destroy(vm, &d, outermost, true)?;
    Ok(match value {
        Some(v) => v.deref(),
        None => Value::Bool(false),
    })
}

impl Vm {
    /// `php_var_unserialize()` of one value of `data` starting at
    /// `offset`, sharing the active `unserialize()` call's `var_hash`
    /// (back-references `R:`/`r:` cross into and out of a native
    /// `Serializable::unserialize()` payload such as `SplObjectStorage`'s)
    /// and its options. Returns the value and the offset after it, or the
    /// failure offset. Outside an `unserialize()` call a fresh context is
    /// used (and its delayed `__wakeup()`/`__unserialize()` calls run).
    /// `PHP_VAR_UNSERIALIZE_INIT` … `PHP_VAR_UNSERIALIZE_DESTROY` around
    /// `f`: [`Vm::unserialize_nested`] calls inside share one `var_hash`
    /// (a native `Serializable::unserialize()` called directly), and the
    /// delayed `__wakeup()`/`__unserialize()` calls run at the end. Inside
    /// an active `unserialize()` it only joins that call's context.
    pub fn with_unserialize_context<R>(
        &mut self,
        f: impl FnOnce(&mut Vm) -> VmResult<R>,
    ) -> VmResult<R> {
        let (d, outermost) = unser_init(self);
        let r = f(self);
        if matches!(r, Err(Throw::Fatal)) {
            ABANDONED_UNSER.with(|u| u.borrow_mut().push(d));
            return Err(Throw::Fatal);
        }
        let ok = r.is_ok();
        let destroyed = unser_destroy(self, &d, outermost, ok);
        match (r, destroyed) {
            (Err(t), _) | (Ok(_), Err(t)) => Err(t),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// `PHP_VAR_UNSERIALIZE_INIT` for native code that cannot run inside
    /// [`Vm::with_unserialize_context`] (the session decoders, which store
    /// every value as soon as it is parsed): until the matching
    /// [`Vm::end_unserialize_context`], [`Vm::unserialize_nested`] calls
    /// (and `unserialize()` calls made from `Serializable::unserialize()`)
    /// share one `var_hash`.
    pub fn begin_unserialize_context(&mut self) {
        let opened = unser_init(self);
        OPENED_UNSER.with(|o| o.borrow_mut().push(opened));
    }

    /// `PHP_VAR_UNSERIALIZE_DESTROY` of the innermost context from
    /// [`Vm::begin_unserialize_context`]: the outermost one runs the
    /// delayed `__wakeup()`/`__unserialize()` calls when `run` (no
    /// exception is pending). After a fatal error (`bailout`) the context
    /// is abandoned instead, as `zend_bailout()` skips the destroy.
    pub fn end_unserialize_context(&mut self, run: bool, bailout: bool) -> VmResult<()> {
        let Some((d, outermost)) = OPENED_UNSER.with(|o| o.borrow_mut().pop()) else {
            return Ok(());
        };
        if bailout {
            ABANDONED_UNSER.with(|u| u.borrow_mut().push(d));
            return Ok(());
        }
        if outermost {
            // What the delayed calls see is unwrapped first, as for
            // `unserialize()`'s own outermost context.
            Unwrapper::new(Some(&d)).pending(&d);
        }
        unser_destroy(self, &d, outermost, run)
    }

    /// `php_unserialize_with_options()` for native callers such as
    /// `Phar::getMetadata()`: `unserialize()` semantics (options,
    /// delayed `__wakeup()`/`__unserialize()` calls, `false` on failure),
    /// with warnings naming the running builtin and option errors naming
    /// `function_name`.
    pub fn unserialize_with_options(
        &mut self,
        data: &[u8],
        options: Option<&ZArray>,
        function_name: &str,
    ) -> VmResult<Value> {
        unserialize_with_options(self, data, options, function_name)
    }

    pub fn unserialize_nested(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> VmResult<Result<(Value, usize), usize>> {
        unserialize_one(self, data, offset, true)
    }

    /// `php_var_unserialize()` into a `var_tmp_var()` of the shared
    /// `var_hash`: like [`Vm::unserialize_nested`], but the value comes back
    /// as its parse cell (a `Value::Reference`), not yet unwrapped, so `R:`
    /// back-references in later values of the same context turn it, or a
    /// cell inside it, into a true reference in place. The session decoders
    /// store it into `$_SESSION` as PHP stores the `IS_PTR` placeholder and
    /// finish with [`Vm::unwrap_unserialized_cells`] before ending the
    /// context (`php_session_normalize_vars()`).
    pub fn unserialize_nested_cell(
        &mut self,
        data: &[u8],
        offset: usize,
    ) -> VmResult<Result<(Value, usize), usize>> {
        unserialize_one(self, data, offset, false)
    }

    /// Replaces the cells that only `values` (and the active context's
    /// `var_hash`) hold by their values, recursively, as
    /// [`Vm::unserialize_nested`] does for each of its results: a cell an
    /// `R:` back-reference targeted stays a reference, shared by every
    /// value holding it. `values` must hold every handle of a cell outside
    /// the `var_hash` (moved in, not cloned), as the decision counts
    /// handles; without an active context only lone cells are unwrapped.
    pub fn unwrap_unserialized_cells(&mut self, values: &mut [Value]) {
        let active = if is_locked() {
            None
        } else {
            UNSER.with(|u| {
                let u = u.borrow();
                u.1.clone().filter(|_| u.0 > 0)
            })
        };
        let mut uw = Unwrapper::new(active.as_ref());
        for v in values {
            uw.value(v);
        }
    }
}

/// `php_var_unserialize()` of one value of `data` at `offset` in the
/// active context. With `unwrap`, the cells only the value holds are
/// unwrapped and the result dereferenced; otherwise the parse cell itself
/// is returned (see [`Vm::unserialize_nested_cell`]).
fn unserialize_one(
    vm: &mut Vm,
    data: &[u8],
    offset: usize,
    unwrap: bool,
) -> VmResult<Result<(Value, usize), usize>> {
    let (d, outermost) = unser_init(vm);
    let first_slot = d.borrow().slots.len();
    let mut u = Unserializer {
        cur: Cursor { data, pos: offset },
        d: d.clone(),
    };
    let parsed = u.value(vm, None);
    if matches!(parsed, Err(Throw::Fatal)) {
        ABANDONED_UNSER.with(|u| u.borrow_mut().push(d));
        return Err(Throw::Fatal);
    }
    let pos = u.cur.pos;
    let mut value = match parsed {
        Ok(Some(v)) => Some(v),
        Ok(None) => {
            for s in d.borrow_mut().slots.iter_mut().skip(first_slot) {
                *s = None;
            }
            None
        }
        Err(t) => {
            for s in d.borrow_mut().slots.iter_mut().skip(first_slot) {
                *s = None;
            }
            let _ = unser_destroy(vm, &d, outermost, false);
            return Err(t);
        }
    };
    if unwrap {
        if let Some(v) = value.as_mut() {
            Unwrapper::new(Some(&d)).value(v);
        }
    }
    unser_destroy(vm, &d, outermost, true)?;
    Ok(match value {
        Some(v) if unwrap => Ok((v.deref(), pos)),
        Some(v) => Ok((v, pos)),
        None => Err(pos),
    })
}

fn unserialize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = s(a, 0);
    let opts = if a.has(1) { Some(arr(a, 1)) } else { None };
    unserialize_with_options(vm, data.as_bytes(), opts.as_ref(), "unserialize")
}

/// Registers `serialize()` / `unserialize()`.
pub(crate) fn register(vm: &mut Vm) {
    vm.startup_hooks.push(request_startup);
    super::reg(vm, "serialize(mixed $value): string", serialize);
    super::reg(
        vm,
        "unserialize(string $data, array $options = []): mixed",
        unserialize,
    );
}

#[cfg(test)]
mod tests {
    use super::{unescape, valid_class_name};
    use crate::{Vm, VmOptions};
    use php_runtime::{ArrayKey, PhpRef, Value};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn run(src: &str) -> String {
        let sink = Sink::default();
        let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        vm.run_script(src.as_bytes(), "t.php");
        drop(vm);
        let out = sink.0.borrow().clone();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The session decoders' path: values parsed into cells stay subject to
    /// later `R:` back-references until they are unwrapped together.
    #[test]
    fn nested_cells_become_shared_references_when_back_referenced() {
        let mut vm = Vm::new(&VmOptions::default(), Box::new(Sink::default()));
        vm.begin_unserialize_context();
        let data = b"foo|a:3:{i:0;i:1;i:1;i:2;i:2;i:3;}guff|R:1;bar|a:1:{i:0;i:5;}baz|R:6;x|i:7;";
        let parse = |vm: &mut Vm, off: usize| -> (Value, usize) {
            match vm.unserialize_nested_cell(data, off) {
                Ok(Ok(r)) => r,
                other => panic!("parse at {off}: {other:?}"),
            }
        };
        let (foo, end) = parse(&mut vm, 4);
        assert_eq!(end, 34);
        assert!(matches!(foo, Value::Reference(_)));
        let (guff, end) = parse(&mut vm, 39);
        assert_eq!(end, 43);
        let (bar, end) = parse(&mut vm, 47);
        assert_eq!(end, 61);
        let (baz, end) = parse(&mut vm, 65);
        assert_eq!(end, 69);
        let (x, end) = parse(&mut vm, 71);
        assert_eq!(end, 75);
        let mut values = vec![foo, guff, bar, baz, x];
        vm.unwrap_unserialized_cells(&mut values);
        vm.end_unserialize_context(true, false).unwrap();
        let [foo, guff, bar, baz, x] = values.as_slice() else {
            panic!()
        };
        // `guff|R:1;` made foo's cell a reference both entries share.
        let (Value::Reference(f), Value::Reference(g)) = (foo, guff) else {
            panic!("{foo:?} {guff:?}")
        };
        assert!(PhpRef::ptr_eq(f, g));
        assert_eq!(f.refcount(), 2);
        let Value::Array(shared) = f.get() else {
            panic!("{foo:?}")
        };
        assert_eq!(shared.len(), 3);
        // `baz|R:6;` targeted bar's element 0: bar stays a plain array whose
        // element is the reference baz shares.
        let Value::Array(b) = bar else {
            panic!("{bar:?}")
        };
        let (Some(Value::Reference(e)), Value::Reference(z)) = (b.get(&ArrayKey::Int(0)), baz)
        else {
            panic!("{bar:?} {baz:?}")
        };
        assert!(PhpRef::ptr_eq(e, z));
        assert!(matches!(e.get(), Value::Int(5)));
        // Nothing referred to x: a plain value.
        assert!(matches!(x, Value::Int(7)));
    }

    #[test]
    fn callback_func_name_with_nul_is_cut_in_the_reason() {
        let out = run(
            "<?php ini_set('unserialize_callback_func', \"foo\\0butno\");\n\
             function foo($n) {}\n\
             try { unserialize('O:3:\"FOO\":0:{}'); }\n\
             catch (Error $e) { echo $e->getMessage(); }",
        );
        assert_eq!(
            out,
            "Invalid callback foo\0butno, function \"foo\" not found or invalid function name"
        );
    }

    #[test]
    fn callback_func_resolves_parent_in_the_calling_scope() {
        let out = run("<?php ini_set('unserialize_callback_func', 'parent::cb');\n\
             class P { static function cb($n) { echo __CLASS__, \" $n\\n\"; eval('class Foo {}'); } }\n\
             class C extends P { function u($s) { return unserialize($s); } }\n\
             try { unserialize('O:3:\"Bar\":0:{}'); }\n\
             catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
             echo get_class((new C)->u('O:3:\"Foo\":0:{}'));");
        assert!(out.starts_with(
            "Invalid callback parent::cb, cannot access \"parent\" when no class scope is active\n"
        ));
        assert!(out.contains("Use of \"parent\" in callables is deprecated"));
        assert!(out.ends_with("P Foo\nFoo"), "{out}");
    }

    #[test]
    fn incomplete_object_refuses_references_and_method_calls() {
        let out = run("<?php $i = unserialize('O:1:\"C\":1:{s:1:\"p\";i:1;}');\n\
             foreach ([function () use ($i) { $x = 1; $i->p = &$x; }, function () use ($i) { $r = &$i->p; },\n\
             fn() => $i->p[] = 1, fn() => $i->f()] as $f) {\n\
             try { $f(); echo \"ok\\n\"; } catch (Error $e) { echo substr($e->getMessage(), 0, 31), \"\\n\"; } }");
        assert_eq!(
            out,
            "The script tried to modify a pr\n\
             The script tried to modify a pr\n\
             The script tried to modify a pr\n\
             The script tried to call a meth\n"
        );
    }

    #[test]
    fn plain_keys_reach_inherited_private_properties() {
        // `is_property_visibility_changed`: `properties_info` of C holds
        // P's private properties under their plain names.
        let out = run("<?php class P { private ?array $h = null; private $u; }\n\
             #[AllowDynamicProperties] class C extends P {}\n\
             var_dump(unserialize('O:1:\"C\":1:{s:1:\"u\";s:1:\"a\";}'));\n\
             try { unserialize('O:1:\"C\":1:{s:1:\"h\";i:5;}'); }\n\
             catch (TypeError $e) { echo $e->getMessage(), \"\\n\"; }");
        assert_eq!(
            out,
            "object(C)#1 (2) {\n  [\"h\":\"P\":private]=>\n  NULL\n  [\"u\":\"P\":private]=>\n  string(1) \"a\"\n}\n\
             Cannot assign int to property P::$h of type ?array\n"
        );
    }

    #[test]
    fn typed_property_references() {
        let out = run("<?php class A { public int $a; public $b; }\n\
             class D { public int $a; public float $b; }\n\
             $o = unserialize('O:1:\"A\":2:{s:1:\"a\";i:1;s:1:\"b\";R:2;}');\n\
             try { $o->b = 'x'; } catch (TypeError $e) { echo $e->getMessage(), \"\\n\"; }\n\
             var_dump(unserialize('O:1:\"D\":1:{s:1:\"b\";i:1;}')->b);\n\
             var_dump(unserialize('O:1:\"D\":2:{s:1:\"b\";i:1;s:1:\"b\";i:2;}')->b);\n\
             foreach (['O:1:\"D\":2:{s:1:\"a\";i:1;s:1:\"b\";R:2;}',\n\
                       'O:1:\"D\":2:{s:1:\"b\";i:1;s:1:\"a\";R:2;}'] as $s) {\n\
             try { unserialize($s); } catch (TypeError $e) { echo $e->getMessage(), \"\\n\"; } }");
        assert_eq!(
            out,
            "Cannot assign string to reference held by property A::$a of type int\n\
             float(1)\n\
             float(2)\n\
             Reference with value of type int held by property D::$a of type int is not compatible with property D::$b of type float\n\
             Cannot assign float to property D::$a of type int\n"
        );
    }

    #[test]
    fn parse_cell_teardown_buffers_no_possible_roots() {
        use php_runtime::{ref_roots, PhpRef, Value};
        let prev = ref_roots::set_recording(true);
        drop(ref_roots::take());
        let cell = PhpRef::new(Value::empty_array());
        let table_handle = cell.clone();
        super::without_root_recording(|| drop(cell));
        assert_eq!(ref_roots::len(), 0);
        // Recording is restored afterwards.
        let again = table_handle.clone();
        drop(again);
        assert_eq!(ref_roots::len(), 1);
        drop(ref_roots::take());
        drop(table_handle);
        ref_roots::set_recording(prev);
    }

    #[test]
    fn class_names() {
        assert!(valid_class_name(b"Foo\\Bar_1"));
        assert!(!valid_class_name(b"Foo-Bar"));
        assert!(!valid_class_name(b"a b"));
    }

    #[test]
    fn s_format_escapes() {
        assert_eq!(unescape(b"a\\41b", 0, 3), Some((b"aAb".to_vec(), 5)));
        assert_eq!(unescape(b"\\4", 0, 1), None);
    }
}
