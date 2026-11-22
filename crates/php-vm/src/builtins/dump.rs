// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `var_dump()`, `debug_zval_dump()`, `print_r()` and `var_export()`
//! (`ext/standard/var.c`, `Zend/zend.c`).
//!
//! Unlike the context-free printers in `php_runtime::dump`, these use the
//! process-wide recursion guards ([`RecursionGuard`]), so a nested call made
//! from `__debugInfo()` (or from a `jsonSerialize()` it triggers) sees the
//! objects already being dumped, and they stream their output like
//! `php_printf()`: everything produced so far is written before user code
//! (`__debugInfo()`) runs.

use crate::class::ConstExpr;
use crate::vm::{Vm, VmResult};
use php_memlimit::CheckedBuf;
use php_runtime::dump::{export_string, unmangle_property_name};
use php_runtime::float::{format_double, format_double_h};
use php_runtime::ops::{GuardKind, RecursionGuard};
use php_runtime::{
    ArrayKey, ErrorLevel, Obj, ObjectDump, ObjectKind, PropertyPurpose, PropertySlot,
    RuntimeContext, Value,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

thread_local! {
    /// An exception a native `get_properties_for` / `get_debug_info`
    /// handler raised while still returning its table (php-src handlers
    /// leave it in `EG(exception)`, e.g. `dom_get_debug_info` reading a
    /// property of an unconstructed `DOMXPath`): the dump is printed in
    /// full and the dumping function throws it when it returns.
    static DEFERRED_THROW: RefCell<Option<crate::vm::Throw>> = const { RefCell::new(None) };
}

/// Parks `t` for the running dump (see [`DEFERRED_THROW`]); the first
/// exception wins, like a handler that stops reading once
/// `EG(exception)` is set.
pub(crate) fn defer_throw(t: crate::vm::Throw) {
    // A losing exception is dropped outside the borrow (its destructor may
    // run user code that dumps).
    let rejected = DEFERRED_THROW.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_none() {
            *d = Some(t);
            None
        } else {
            Some(t)
        }
    });
    drop(rejected);
}

thread_local! {
    /// Nesting depth of the dumping builtins (`var_dump()` called from a
    /// `__debugInfo()` of an object being dumped).
    static DUMP_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Marks a running dumping builtin; see [`begin_dump`].
pub(crate) struct DumpScope(());

impl Drop for DumpScope {
    fn drop(&mut self) {
        DUMP_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Enters a dumping builtin. The outermost one drops an exception parked
/// outside any dump (a handler called for another purpose), so it does
/// not surface from an unrelated call.
#[must_use]
pub(crate) fn begin_dump() -> DumpScope {
    let outermost = DUMP_DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n == 0
    });
    if outermost {
        clear_deferred_throw();
    }
    DumpScope(())
}

/// Drops the parked exception (the dump failed with its own).
pub(crate) fn clear_deferred_throw() {
    if let Some(t) = DEFERRED_THROW.with(|d| d.borrow_mut().take()) {
        drop(t);
    }
}

/// Throws the exception parked during the dump that just ended, if any.
pub(crate) fn take_deferred_throw() -> VmResult<()> {
    match DEFERRED_THROW.with(|d| d.borrow_mut().take()) {
        Some(t) => Err(t),
        None => Ok(()),
    }
}

/// Output of a dump: streamed through the output layer, or collected
/// (`print_r($v, true)`, `var_export($v, true)`).
/// Bytes a streamed `var_dump()` collects before it writes them.
const STREAM_CHUNK: usize = 8192;

pub(crate) struct Out {
    /// Never grows past `memory_limit`: a refused growth is recorded and
    /// the rest of the dump dropped ([`Out::check`], [`Out::finish`]).
    buf: CheckedBuf,
    stream: bool,
}

impl Out {
    /// Output written with `echo` semantics.
    pub(crate) fn stream() -> Self {
        Out {
            buf: CheckedBuf::new(),
            stream: true,
        }
    }

    /// Output collected into a string.
    pub(crate) fn collect() -> Self {
        Out {
            buf: CheckedBuf::new(),
            stream: false,
        }
    }

    fn put(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn spaces(&mut self, n: usize) {
        self.buf.extend(std::iter::repeat_n(b' ', n));
    }

    /// Writes what has been produced so far (before user code runs).
    pub(crate) fn flush(&mut self, vm: &mut Vm) {
        if self.stream && !self.buf.is_empty() {
            let b = std::mem::take(&mut self.buf);
            vm.echo(&b);
        }
    }

    /// Whether the output stopped at `memory_limit`: the walk stops then.
    pub(crate) fn check(&self) -> VmResult<()> {
        Ok(self.buf.check()?)
    }

    /// Per element of `var_dump()`, `debug_zval_dump()` and `print_r()`: a
    /// streamed dump writes what it has, so the output never collects in
    /// memory (in chunks of [`STREAM_CHUNK`] bytes, to keep the writes few;
    /// only earlier, so the order with warnings and user code stays).
    /// php-src's `php_printf` writes each piece of `var_dump()` at once;
    /// its `print_r()` builds the whole string first and fails at
    /// `memory_limit` only after printing it (the over-limit allocation is
    /// granted), so streaming prints the same bytes without the fatal.
    /// `var_export()` stays buffered: php-src prints its warnings before
    /// the export, and the export stops at the limit with the fatal
    /// ([`check`](Self::check)).
    pub(crate) fn step(&mut self, vm: &mut Vm) -> VmResult<()> {
        if self.stream && self.buf.len() >= STREAM_CHUNK {
            self.flush(vm);
        }
        self.check()
    }

    /// The collected bytes (streamed output is flushed instead), or the
    /// memory-limit fatal when the output stopped growing.
    pub(crate) fn finish(mut self, vm: &mut Vm) -> VmResult<Vec<u8>> {
        self.flush(vm);
        Ok(self.buf.into_vec()?)
    }
}

/// Bytes up to the first NUL (`%s` semantics).
fn cstr(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |p| &b[..p])
}

fn object_dump(
    vm: &mut Vm,
    out: &mut Out,
    o: &Obj,
    purpose: PropertyPurpose,
) -> VmResult<ObjectDump> {
    out.flush(vm);
    if purpose == PropertyPurpose::Debug {
        if let Some(d) = debug_info(vm, o)? {
            return Ok(d);
        }
    }
    vm.object_dump(o, purpose).map_err(|e| vm.rt_throw(e))
}

/// `zend_std_get_debug_info()` for classes with a user `__debugInfo()`:
/// a returned reference is unwrapped, `null` is deprecated (empty table),
/// anything else is fatal.
fn debug_info(vm: &mut Vm, o: &Obj) -> VmResult<Option<ObjectDump>> {
    let cls = crate::object::class_of(o).clone();
    if cls
        .hooks
        .as_ref()
        .is_some_and(|h| h.properties_for.is_some())
    {
        return Ok(None);
    }
    let Some(m) = cls.magic.debug_info.clone() else {
        return Ok(None);
    };
    let table = match vm.call_method(o, &m, Vec::new())?.deref() {
        Value::Array(a) => a,
        Value::Null => {
            vm.deprecated(&format!(
                "Returning null from {}::__debugInfo() is deprecated, return an empty array instead",
                cls.name
            ))?;
            php_runtime::ZArray::new()
        }
        _ => return Err(vm.fatal("__debugInfo() must return an array")),
    };
    let kind = if cls.is_enum() {
        ObjectKind::Enum {
            case_name: vm.enum_case_name(o).unwrap_or_default(),
            backing_type: cls.backing.map(|b| {
                if b == crate::types::mask::LONG {
                    "int"
                } else {
                    "string"
                }
            }),
            has_debug_info: true,
        }
    } else if cls.lc_name.as_bytes() == b"stdclass" {
        ObjectKind::StdClass
    } else {
        ObjectKind::Regular {
            lazy_prefix: vm.lazy_prefix(o),
        }
    };
    let properties = table
        .iter()
        .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
        .collect();
    Ok(Some(ObjectDump {
        kind,
        properties: Some(properties),
    }))
}

/// The property table of an enum case (`name`, `value`) for printers that
/// show it (`print_r`, `debug_zval_dump`).
fn enum_props(vm: &mut Vm, o: &Obj) -> VmResult<Vec<(ArrayKey, PropertySlot)>> {
    let t = vm
        .object_properties(o, PropertyPurpose::Debug)
        .map_err(|e| vm.rt_throw(e))?;
    Ok(t.map(|t| {
        t.iter()
            .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
            .collect()
    })
    .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Native stack limit (`ZEND_CHECK_STACK_LIMIT`)
// ---------------------------------------------------------------------------

/// Approximate C stack bytes PHP uses per nesting level of the printers.
const FRAME_BYTES: i64 = 128;
/// The stack PHP assumes when `zend.max_allowed_stack_size` is 0 (auto).
const DEFAULT_STACK: i64 = 8 << 20;

thread_local! {
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// See [`set_resource_refs_hook`].
    static RESOURCE_REFS: std::cell::Cell<Option<crate::ext::ResourceRefsFn>> =
        const { std::cell::Cell::new(None) };
}

/// Installs the callback reporting the references an extension's internal
/// structures hold to a resource.
pub(crate) fn set_resource_refs_hook(f: crate::ext::ResourceRefsFn) {
    RESOURCE_REFS.with(|h| h.set(Some(f)));
}

/// References to `r` held outside PHP values (`GC_ADDREF` on the
/// `zend_resource` by its extension, e.g. `stream->ctx`).
fn resource_internal_refs(r: &php_runtime::Resource) -> usize {
    RESOURCE_REFS.with(std::cell::Cell::get).map_or(0, |f| f(r))
}

/// The usable stack in bytes (`zend.max_allowed_stack_size -
/// zend.reserved_stack_size`).
fn stack_budget(vm: &Vm) -> i64 {
    let max = vm.ini_long("zend.max_allowed_stack_size");
    let max = if max <= 0 { DEFAULT_STACK } else { max };
    (max - vm.ini_long("zend.reserved_stack_size").max(0)).max(FRAME_BYTES)
}

/// One nesting level of a printer; `None` when PHP's C stack would have
/// overflowed (`zend_call_stack_overflowed()`).
pub(super) struct Depth;

impl Depth {
    pub(super) fn enter(vm: &Vm) -> Option<Depth> {
        let limit = usize::try_from(stack_budget(vm) / FRAME_BYTES).unwrap_or(usize::MAX);
        DEPTH.with(|d| {
            if d.get() >= limit {
                return None;
            }
            d.set(d.get() + 1);
            Some(Depth)
        })
    }
}

impl Drop for Depth {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// `zend_call_stack_size_error()`.
pub(crate) fn stack_size_error(vm: &mut Vm) -> crate::vm::Throw {
    let size = stack_budget(vm);
    vm.make_throw(
        "Error",
        &format!(
            "Maximum call stack size of {size} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
        ),
    )
}

// ---------------------------------------------------------------------------
// var_dump / debug_zval_dump
// ---------------------------------------------------------------------------

/// Refcount bookkeeping for `debug_zval_dump()` (`None` for `var_dump()`):
/// the number of handles held by the dumper/VM beyond the ones PHP counts.
#[derive(Clone, Copy)]
pub(crate) struct Zval {
    extra: usize,
}

fn put_key(out: &mut Out, k: &ArrayKey, level: usize) {
    out.spaces(level + 1);
    match k {
        ArrayKey::Int(i) => out.put(format!("[{i}]=>\n").as_bytes()),
        ArrayKey::Str(s) => {
            out.put(b"[\"");
            out.put(s.as_bytes());
            out.put(b"\"]=>\n");
        }
    }
}

fn put_property_key(out: &mut Out, k: &ArrayKey, level: usize) {
    out.spaces(level + 1);
    match k {
        ArrayKey::Int(i) => out.put(format!("[{i}]=>\n").as_bytes()),
        ArrayKey::Str(s) => {
            out.put(b"[\"");
            match unmangle_property_name(s.as_bytes()) {
                (Some(class), prop) => {
                    out.put(cstr(prop));
                    if class.first() == Some(&b'*') {
                        out.put(b"\":protected");
                    } else {
                        out.put(b"\":\"");
                        out.put(cstr(class));
                        out.put(b"\":private");
                    }
                }
                (None, prop) => {
                    out.put(prop);
                    out.put(b"\"");
                }
            }
            out.put(b"]=>\n");
        }
    }
}

fn rc(count: usize, z: Zval) -> usize {
    count.saturating_sub(z.extra).max(1)
}

/// Per-call state of `debug_zval_dump()`: where this VM's ownership of a
/// refcounted value differs from php-src's, by value identity.
struct DzState {
    /// Handles kept by compiled class metadata that php-src does not
    /// have (see [`class_handles`]).
    hidden: HashMap<usize, usize>,
    /// Handles php-src's class entries hold that this VM has not
    /// materialised yet (see [`class_handles`]).
    missing: HashMap<usize, usize>,
    /// Identities of the values in the property lists that the dumper
    /// frames hold (each list holds a handle to each of its values, where
    /// php-src walks the object's own table), innermost last.
    held: Vec<usize>,
    /// Occurrence counts of `held`.
    held_count: HashMap<usize, usize>,
}

thread_local! {
    static DZ_STATE: RefCell<Option<DzState>> = const { RefCell::new(None) };
}

/// Resets dumping recursion and exception state for a new request.
pub(crate) fn reset() {
    DEFERRED_THROW.with(|d| *d.borrow_mut() = None);
    DUMP_DEPTH.with(|d| d.set(0));
    DEPTH.with(|d| d.set(0));
    DZ_STATE.with(|d| *d.borrow_mut() = None);
}

/// The identity of a refcounted value (`None` for scalars, resources and
/// interned or empty strings).
fn identity(v: &Value) -> Option<usize> {
    match v {
        Value::Array(a) => Some(a.addr()),
        Value::Str(s) if !s.is_interned() && !s.is_empty() => Some(s.as_bytes().as_ptr() as usize),
        Value::Object(o) => Some(o.addr()),
        Value::Reference(r) => Some(r.addr()),
        _ => None,
    }
}

/// `rc()` for the value with identity `id`, corrected for the handles
/// php-src does not have (class metadata, the dumper's property lists)
/// and for the ones it has that this VM does not (see [`DzState`]).
fn rc_of(count: usize, z: Zval, id: usize) -> usize {
    let (hidden, missing) = DZ_STATE.with(|s| {
        s.borrow().as_ref().map_or((0, 0), |st| {
            let at = |m: &HashMap<usize, usize>| m.get(&id).copied().unwrap_or(0);
            (at(&st.hidden) + at(&st.held_count), at(&st.missing))
        })
    });
    rc(
        count + missing,
        Zval {
            extra: z.extra + hidden,
        },
    )
}

/// Handles to the refcounted literals of class declarations that this
/// VM's metadata keeps and php-src does not (`hidden`), and the ones
/// php-src's class entries keep that this VM has not materialised yet
/// (`missing`), by value identity.
struct ClassHandles {
    hidden: HashMap<usize, usize>,
    missing: HashMap<usize, usize>,
}

/// Counts one handle in `map` when `e` is a refcounted literal.
fn note(map: &mut HashMap<usize, usize>, e: Option<&ConstExpr>) {
    if let Some(ConstExpr::Value(v)) = e {
        if let Some(id) = identity(v) {
            *map.entry(id).or_insert(0) += 1;
        }
    }
}

/// Where this VM's class metadata and php-src's class entries own the
/// literals of declarations differently, over every declared class (an
/// alias is the same class).
///
/// Held here and not by php-src:
/// - an instance property default: `PropDecl::default` and
///   `PropInfo::default` (each once, however many classes share them);
///   php-src keeps it only in `default_properties_table`;
/// - a static property default: `PropDecl::default`, and the copies of
///   the `StaticProp` that inheriting classes get; php-src's
///   `default_static_members_table` entry is the declaring (or
///   trait-using) class's `StaticProp::default`, an inheriting class
///   holds `IS_INDIRECT`, and the storage cell is `static_members_table`;
/// - a class constant: `ConstDecl::value` and `ClassConst::expr`;
///   php-src keeps `zend_class_constant.value` only (our
///   `ClassConst::value`), one per declaring class shared with its
///   subclasses and copied per trait-using class, as `Rc<ClassConst>`s
///   are.
///
/// Held by php-src and not here yet:
/// - `default_properties_table` is filled when a class is linked, for
///   every class of a hierarchy (abstract classes and traits included);
///   `Class::defaults` is built on first instantiation, so a class never
///   instantiated holds none of its literal defaults;
/// - `zend_class_constant.value` exists from the declaration on; the
///   `ClassConst::value` of a typed constant or of a trait's copy is set
///   on first use.
fn class_handles(vm: &Vm) -> ClassHandles {
    let mut seen_classes = HashSet::new();
    let mut seen_props = HashSet::new();
    let mut seen_decls = HashSet::new();
    let mut seen_consts = HashSet::new();
    let mut seen_cells = HashSet::new();
    let mut hidden: HashMap<usize, usize> = HashMap::new();
    let mut missing: HashMap<usize, usize> = HashMap::new();
    for cls in vm.classes.values() {
        if !seen_classes.insert(Rc::as_ptr(cls)) {
            continue;
        }
        for pi in cls.props.iter().chain(cls.prop_map.values()) {
            if seen_props.insert(Rc::as_ptr(pi)) {
                note(&mut hidden, pi.default.as_ref());
            }
        }
        if cls.defaults.try_borrow().is_ok_and(|d| d.is_none()) {
            for pi in &cls.props {
                note(&mut missing, pi.default.as_ref());
            }
        }
        for sp in &cls.static_props {
            if !seen_cells.insert(Rc::as_ptr(&sp.cell)) {
                note(&mut hidden, sp.default.as_ref());
            }
        }
        for c in &cls.consts {
            if seen_consts.insert(Rc::as_ptr(c)) {
                note(&mut hidden, Some(&c.expr));
                if c.value.try_borrow().is_ok_and(|v| v.is_none()) {
                    note(&mut missing, Some(&c.expr));
                }
            }
        }
        if let Some(decl) = &cls.decl {
            if seen_decls.insert(Rc::as_ptr(decl)) {
                for pd in &decl.props {
                    note(&mut hidden, pd.default.as_ref());
                }
                for cd in &decl.consts {
                    note(&mut hidden, Some(&cd.value));
                }
            }
        }
    }
    for f in vm.frames.iter() {
        if !f.symtab_attached {
            continue;
        }
        let Ok(mut st) = f.vars.try_borrow_mut() else {
            continue;
        };
        for (i, name) in f.code.cv_names.iter().enumerate() {
            let Some(cv_id) = f
                .cvs
                .get(i)
                .and_then(crate::vm::CvSlot::get)
                .and_then(identity)
            else {
                continue;
            };
            let Some(slot) = st.find(name.as_bytes()) else {
                continue;
            };
            let Some(sym_id) = st
                .slots
                .get(slot as usize)
                .and_then(Option::as_ref)
                .and_then(identity)
            else {
                continue;
            };
            if cv_id == sym_id {
                *hidden.entry(cv_id).or_insert(0) += 1;
            }
        }
    }
    ClassHandles { hidden, missing }
}

/// A dumper frame's entries in [`DzState::held`], removed when dropped.
struct HeldMark(usize);

impl HeldMark {
    fn new() -> Self {
        HeldMark(DZ_STATE.with(|s| s.borrow().as_ref().map_or(0, |st| st.held.len())))
    }
}

/// Records that the current dumper frame holds a handle to `v` (until its
/// [`HeldMark`] drops).
fn hold(v: &Value) {
    let Some(id) = identity(v) else {
        return;
    };
    DZ_STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.held.push(id);
            *st.held_count.entry(id).or_insert(0) += 1;
        }
    });
}

impl Drop for HeldMark {
    fn drop(&mut self) {
        let _ = DZ_STATE.try_with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                while st.held.len() > self.0 {
                    let Some(id) = st.held.pop() else {
                        break;
                    };
                    if let Some(c) = st.held_count.get_mut(&id) {
                        *c -= 1;
                        if *c == 0 {
                            st.held_count.remove(&id);
                        }
                    }
                }
            }
        });
    }
}

/// `php_var_dump()` (`zv == None`) / `php_debug_zval_dump()` of one value.
#[allow(clippy::too_many_lines)]
pub(crate) fn var_dump(
    vm: &mut Vm,
    out: &mut Out,
    value: &Value,
    level: usize,
    zv: Option<Zval>,
) -> VmResult<()> {
    if level > 1 {
        out.spaces(level - 1);
    }
    let mut common: &[u8] = b"";
    let deref;
    let value = match value {
        Value::Reference(r) => {
            if let Some(z) = zv {
                let n = rc_of(r.refcount(), z, r.addr());
                out.put(format!("reference refcount({n}) {{\n").as_bytes());
                let inner = r.get();
                var_dump(vm, out, &inner, level + 2, Some(Zval { extra: 1 }))?;
                if level > 1 {
                    out.spaces(level - 1);
                }
                out.put(b"}\n");
                return Ok(());
            }
            if r.refcount() > 1 {
                common = b"&";
            }
            deref = r.get();
            &deref
        }
        v => v,
    };
    match value {
        Value::Null => {
            out.put(common);
            out.put(b"NULL\n");
        }
        Value::Bool(b) => {
            out.put(common);
            out.put(if *b {
                b"bool(true)\n"
            } else {
                b"bool(false)\n"
            });
        }
        Value::Int(i) => {
            out.put(common);
            out.put(format!("int({i})\n").as_bytes());
        }
        Value::Float(f) => {
            out.put(common);
            let s = format_double_h(*f, vm.serialize_precision());
            out.put(format!("float({s})\n").as_bytes());
        }
        Value::Str(s) => {
            out.put(common);
            out.put(format!("string({}) \"", s.len()).as_bytes());
            out.put(s.as_bytes());
            match zv {
                Some(z) if !s.is_interned() => {
                    let n = match identity(value) {
                        Some(id) => rc_of(s.refcount(), z, id),
                        None => rc(s.refcount(), z),
                    };
                    out.put(format!("\" refcount({n})\n").as_bytes());
                }
                Some(_) => out.put(b"\" interned\n"),
                None => out.put(b"\"\n"),
            }
        }
        Value::Array(a) => {
            let Some(_guard) = RecursionGuard::try_enter(GuardKind::Debug, a.addr()) else {
                out.put(b"*RECURSION*\n");
                return Ok(());
            };
            out.put(common);
            match zv {
                Some(z) => {
                    let packed = if a.is_packed() { "packed " } else { "" };
                    if a.is_empty() {
                        out.put(b"array(0) interned {\n");
                    } else {
                        let n = rc_of(a.refcount(), z, a.addr());
                        out.put(format!("array({}) {packed}refcount({n}){{\n", a.len()).as_bytes());
                    }
                }
                None => out.put(format!("array({}) {{\n", a.len()).as_bytes()),
            }
            let a = a.clone();
            for (k, v) in a.iter() {
                out.step(vm)?;
                let Some(_depth) = Depth::enter(vm) else {
                    out.spaces(level + 1);
                    out.put(b"nesting level too deep");
                    continue;
                };
                put_key(out, k, level);
                var_dump(vm, out, v, level + 2, zv.map(|_| Zval { extra: 0 }))?;
            }
            if level > 1 {
                out.spaces(level - 1);
            }
            out.put(b"}\n");
        }
        Value::Object(o) => var_dump_object(vm, out, o, common, level, zv)?,
        Value::Resource(r) => {
            out.put(common);
            let t = r
                .type_name()
                .map_or_else(|| b"Unknown".to_vec(), |t| t.as_bytes().to_vec());
            out.put(format!("resource({}) of type (", r.handle()).as_bytes());
            out.put(&t);
            match zv {
                // Resources do not expose their handle count: report the
                // usual "variable + argument" (or "container") count, plus
                // the references the owning extension holds (GH-10885: a
                // stream keeps its context).
                Some(_) => {
                    let base = if level == 1 { 2 } else { 1 };
                    let n = base + resource_internal_refs(r);
                    out.put(format!(") refcount({n})\n").as_bytes());
                }
                None => out.put(b")\n"),
            }
        }
        Value::Reference(_) => out.put(b"UNKNOWN:0\n"),
    }
    Ok(())
}

fn var_dump_object(
    vm: &mut Vm,
    out: &mut Out,
    o: &Obj,
    common: &[u8],
    level: usize,
    zv: Option<Zval>,
) -> VmResult<()> {
    let cls = crate::object::class_of(o).clone();
    if zv.is_none() && cls.is_enum() && cls.magic.debug_info.is_none() {
        let case = vm.enum_case_name(o).unwrap_or_default();
        out.put(common);
        out.put(b"enum(");
        out.put(cls.name.as_bytes());
        out.put(b"::");
        out.put(case.as_bytes());
        out.put(b")\n");
        return Ok(());
    }
    // Protect before `__debugInfo()` runs (GH-8044).
    let Some(_guard) = RecursionGuard::try_enter(GuardKind::Debug, o.addr()) else {
        out.put(b"*RECURSION*\n");
        return Ok(());
    };
    let refs_before = o.refcount();
    let ObjectDump { kind, properties } = object_dump(vm, out, o, PropertyPurpose::Debug)?;
    // `php_debug_zval_dump` prints the refcount after
    // `zend_get_properties_for()`: a table a native handler builds
    // (`SplFixedArray`) holds its own references to the values, the
    // standard property table does not (GH-8044).
    let refs = if crate::ext::hooks_of(o).is_some_and(|h| h.properties_for.is_some()) {
        o.refcount()
    } else {
        refs_before
    };
    let properties = match (&kind, properties) {
        (ObjectKind::Enum { .. }, None) if zv.is_some() => Some(enum_props(vm, o)?),
        (_, p) => p,
    };
    let mut count = properties.as_ref().map_or(0, |p| {
        p.iter()
            .filter(|(_, s)| matches!(s, PropertySlot::Value(_)))
            .count()
    });
    // `zend_array_count` of the object's own table while an unset of one
    // of its declared properties runs destructors (see
    // `object::begin_stale_count`).
    if cls.magic.debug_info.is_none()
        && !crate::ext::hooks_of(o).is_some_and(|h| h.properties_for.is_some())
    {
        count += crate::object::stale_count(o.handle());
    }
    out.put(common);
    // `%pS`: the full name (anonymous classes include `\0file:line$n`).
    let class = cls.name.clone();
    if let (ObjectKind::Enum { case_name, .. }, None) = (&kind, zv) {
        out.put(b"enum(");
        out.put(class.as_bytes());
        out.put(b"::");
        out.put(case_name.as_bytes());
        out.put(format!(") ({count}) {{\n").as_bytes());
    } else {
        if let ObjectKind::Regular { lazy_prefix } = &kind {
            out.put(lazy_prefix.as_bytes());
        }
        out.put(b"object(");
        out.put(class.as_bytes());
        out.put(format!(")#{} ({count}) ", o.handle()).as_bytes());
        match zv {
            Some(z) => out.put(format!("refcount({}){{\n", rc_of(refs, z, o.addr())).as_bytes()),
            None => out.put(b"{\n"),
        }
    }
    // Our copy of the property table holds a handle to each value until
    // the dump of this object ends; php-src walks the object's own table.
    // (The synthetic proxy instance table owns a PHP-visible reference.)
    let _held = zv.map(|_| HeldMark::new());
    if zv.is_some()
        && !crate::object::data(o)
            .lazy
            .borrow()
            .as_ref()
            .is_some_and(|info| info.proxy && info.initialized)
    {
        for (_, slot) in properties.iter().flatten() {
            if let PropertySlot::Value(v) = slot {
                hold(v);
            }
        }
    }
    for (k, slot) in properties.iter().flatten() {
        out.step(vm)?;
        let Some(_depth) = Depth::enter(vm) else {
            out.spaces(level + 1);
            out.put(b"nesting level too deep");
            continue;
        };
        put_property_key(out, k, level);
        match slot {
            PropertySlot::Value(v) => {
                var_dump(vm, out, v, level + 2, zv.map(|_| Zval { extra: 0 }))?;
            }
            PropertySlot::Uninitialized(t) => {
                out.spaces(level + 1);
                out.put(b"uninitialized(");
                out.put(t.as_bytes());
                out.put(b")\n");
            }
        }
    }
    if level > 1 {
        out.spaces(level - 1);
    }
    out.put(b"}\n");
    Ok(())
}

/// `debug_zval_dump()` of one argument; `extra` is the number of handles
/// held beyond PHP's own (the variable and the argument slot). A nested
/// call (from `__debugInfo()`) shares the enclosing call's [`DzState`].
pub(crate) fn debug_zval_dump(
    vm: &mut Vm,
    out: &mut Out,
    value: &Value,
    extra: usize,
) -> VmResult<()> {
    let outermost = DZ_STATE.with(|s| s.borrow().is_none());
    if outermost {
        let ClassHandles { hidden, missing } = class_handles(vm);
        let state = DzState {
            hidden,
            missing,
            held: Vec::new(),
            held_count: HashMap::new(),
        };
        DZ_STATE.with(|s| *s.borrow_mut() = Some(state));
    }
    let r = var_dump(vm, out, value, 1, Some(Zval { extra }));
    if outermost {
        DZ_STATE.with(|s| *s.borrow_mut() = None);
    }
    r
}

// ---------------------------------------------------------------------------
// print_r
// ---------------------------------------------------------------------------

/// `zend_print_zval_r_to_buf()`.
pub(crate) fn print_r(vm: &mut Vm, out: &mut Out, value: &Value, indent: usize) -> VmResult<()> {
    match value {
        Value::Array(a) => {
            out.put(b"Array\n");
            let Some(_guard) = RecursionGuard::try_enter(GuardKind::Debug, a.addr()) else {
                out.put(b" *RECURSION*");
                return Ok(());
            };
            // A shared handle, not a copy of every entry (changes made by
            // user code meanwhile copy the array).
            let a = a.clone();
            let entries = a
                .iter()
                .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())));
            print_hash(vm, out, entries, indent, false)
        }
        Value::Object(o) => {
            let cls = crate::object::class_of(o).clone();
            out.put(cstr(o.class_name().as_bytes()));
            if cls.is_enum() {
                out.put(b" Enum");
                if let Some(b) = cls.backing {
                    out.put(if b == crate::types::mask::LONG {
                        b":int"
                    } else {
                        b":string"
                    });
                }
                out.put(b"\n");
            } else {
                out.put(b" Object\n");
            }
            let Some(_guard) = RecursionGuard::try_enter(GuardKind::Debug, o.addr()) else {
                out.put(b" *RECURSION*");
                return Ok(());
            };
            let info = object_dump(vm, out, o, PropertyPurpose::Debug)?;
            let entries = match (&info.kind, info.properties) {
                (ObjectKind::Enum { .. }, None) => enum_props(vm, o)?,
                (_, p) => p.unwrap_or_default(),
            };
            print_hash(vm, out, entries, indent, true)
        }
        Value::Float(f) => {
            out.put(format_double(*f, vm.precision(), false).as_bytes());
            Ok(())
        }
        Value::Reference(r) => {
            let v = r.get();
            print_r(vm, out, &v, indent)
        }
        other => {
            let s = vm.to_zstr(other)?;
            out.put(s.as_bytes());
            Ok(())
        }
    }
}

fn print_hash(
    vm: &mut Vm,
    out: &mut Out,
    entries: impl IntoIterator<Item = (ArrayKey, PropertySlot)>,
    indent: usize,
    is_object: bool,
) -> VmResult<()> {
    out.spaces(indent);
    out.put(b"(\n");
    let inner = indent + 4;
    for (k, slot) in entries {
        out.step(vm)?;
        let PropertySlot::Value(v) = slot else {
            continue;
        };
        out.spaces(inner);
        out.put(b"[");
        match &k {
            ArrayKey::Int(i) => out.put(i.to_string().as_bytes()),
            ArrayKey::Str(s) if is_object => match unmangle_property_name(s.as_bytes()) {
                (Some(class), prop) => {
                    out.put(prop);
                    if class.first() == Some(&b'*') {
                        out.put(b":protected");
                    } else {
                        out.put(b":");
                        out.put(cstr(class));
                        out.put(b":private");
                    }
                }
                (None, prop) => out.put(prop),
            },
            ArrayKey::Str(s) => out.put(s.as_bytes()),
        }
        out.put(b"] => ");
        print_r(vm, out, &v, inner + 4)?;
        out.put(b"\n");
    }
    out.spaces(indent);
    out.put(b")\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// var_export
// ---------------------------------------------------------------------------

fn circular(vm: &mut Vm, out: &mut Out) -> VmResult<()> {
    vm.diagnostic(
        ErrorLevel::Warning,
        "var_export does not handle circular references",
    )
    .map_err(|e| vm.rt_throw(e))?;
    out.put(b"NULL");
    Ok(())
}

/// `php_var_export_ex()`.
pub(crate) fn var_export(vm: &mut Vm, out: &mut Out, value: &Value, level: usize) -> VmResult<()> {
    let Some(_depth) = Depth::enter(vm) else {
        return Err(stack_size_error(vm));
    };
    match value {
        Value::Null | Value::Resource(_) => out.put(b"NULL"),
        Value::Bool(b) => out.put(if *b { b"true" } else { b"false" }),
        Value::Int(i) => {
            if *i == i64::MIN {
                out.put(b"-9223372036854775807-1");
            } else {
                out.put(i.to_string().as_bytes());
            }
        }
        Value::Float(f) => out.put(format_double(*f, vm.serialize_precision(), true).as_bytes()),
        Value::Str(s) => out.put(&export_string(s.as_bytes())),
        Value::Array(a) => {
            let Some(_guard) = RecursionGuard::try_enter(GuardKind::Export, a.addr()) else {
                return circular(vm, out);
            };
            if level > 1 {
                out.put(b"\n");
                out.spaces(level - 1);
            }
            out.put(b"array (\n");
            let a = a.clone();
            for (k, v) in a.iter() {
                out.check()?;
                out.spaces(level + 1);
                match k {
                    ArrayKey::Int(i) => out.put(i.to_string().as_bytes()),
                    ArrayKey::Str(s) => out.put(&export_string(s.as_bytes())),
                }
                out.put(b" => ");
                var_export(vm, out, v, level + 2)?;
                out.put(b",\n");
            }
            if level > 1 {
                out.spaces(level - 1);
            }
            out.put(b")");
        }
        Value::Object(o) => return var_export_object(vm, out, o, level),
        Value::Reference(r) => {
            let v = r.get();
            return var_export(vm, out, &v, level);
        }
    }
    Ok(())
}

fn var_export_object(vm: &mut Vm, out: &mut Out, o: &Obj, level: usize) -> VmResult<()> {
    let Some(_guard) = RecursionGuard::try_enter(GuardKind::Export, o.addr()) else {
        return circular(vm, out);
    };
    let info = object_dump(vm, out, o, PropertyPurpose::VarExport)?;
    if level > 1 {
        out.put(b"\n");
        out.spaces(level - 1);
    }
    let class = o.class_name();
    match &info.kind {
        ObjectKind::StdClass => out.put(b"(object) array(\n"),
        ObjectKind::Enum { case_name, .. } => {
            out.put(b"\\");
            out.put(class.as_bytes());
            out.put(b"::");
            out.put(case_name.as_bytes());
            return Ok(());
        }
        ObjectKind::Regular { .. } => {
            out.put(b"\\");
            out.put(class.as_bytes());
            out.put(b"::__set_state(array(\n");
        }
    }
    for (k, slot) in info.properties.iter().flatten() {
        out.check()?;
        let PropertySlot::Value(v) = slot else {
            continue;
        };
        out.spaces(level + 2);
        match k {
            ArrayKey::Int(i) => out.put(i.to_string().as_bytes()),
            ArrayKey::Str(s) => {
                let (_, prop) = unmangle_property_name(s.as_bytes());
                out.put(&export_string(prop));
            }
        }
        out.put(b" => ");
        var_export(vm, out, v, level + 2)?;
        out.put(b",\n");
    }
    if level > 1 {
        out.spaces(level - 1);
    }
    match info.kind {
        ObjectKind::StdClass => out.put(b")"),
        _ => out.put(b"))"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{cstr, rc, Zval};
    use crate::{Vm, VmOptions};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[test]
    fn cstr_stops_at_nul() {
        assert_eq!(cstr(b"ab\0cd"), b"ab");
        assert_eq!(cstr(b"abc"), b"abc");
    }

    #[test]
    fn refcounts_subtract_own_handles() {
        assert_eq!(rc(5, Zval { extra: 2 }), 3);
        assert_eq!(rc(1, Zval { extra: 2 }), 1);
    }

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

    /// php-src fills every class's `default_properties_table` at link
    /// time: the literal of an inherited property is held once per class
    /// of the hierarchy (abstract parents and traits included), whether
    /// or not the class was ever instantiated.
    #[test]
    fn property_defaults_are_held_by_every_class_of_the_hierarchy() {
        let out = run("<?php
abstract class P { public $p = [1]; }
class Q extends P {}
class R extends Q {}
trait T { public $t = [2]; }
class U { use T; }
class V { use T; }
$q = new Q;
debug_zval_dump($q);
$r = new R;
debug_zval_dump($q);
$u = new U;
debug_zval_dump($u);
");
        assert_eq!(
            out,
            "object(Q)#1 (1) refcount(2){\n  [\"p\"]=>\n  array(1) packed refcount(4){\n    [0]=>\n    int(1)\n  }\n}\n\
             object(Q)#1 (1) refcount(2){\n  [\"p\"]=>\n  array(1) packed refcount(5){\n    [0]=>\n    int(1)\n  }\n}\n\
             object(U)#3 (1) refcount(2){\n  [\"t\"]=>\n  array(1) packed refcount(4){\n    [0]=>\n    int(2)\n  }\n}\n"
        );
    }

    /// A static property default is held by the declaring class's
    /// `default_static_members_table` and by its `static_members_table`;
    /// an inheriting class holds `IS_INDIRECT`, a trait-using class a
    /// copy of both.
    #[test]
    fn static_property_defaults_are_held_once_per_declaring_class() {
        let out = run("<?php
class H { public static $s = [1]; }
class I extends H {}
class J extends H { public static $s = [2]; }
trait T { public static $t = [3]; }
class U { use T; }
debug_zval_dump(H::$s);
debug_zval_dump(I::$s);
debug_zval_dump(J::$s);
debug_zval_dump(U::$t);
");
        assert_eq!(
            out,
            "array(1) packed refcount(3){\n  [0]=>\n  int(1)\n}\n\
             array(1) packed refcount(3){\n  [0]=>\n  int(1)\n}\n\
             array(1) packed refcount(3){\n  [0]=>\n  int(2)\n}\n\
             array(1) packed refcount(4){\n  [0]=>\n  int(3)\n}\n"
        );
    }

    /// A class constant's value is held by its `zend_class_constant`
    /// alone (shared with subclasses, copied per trait-using class) plus
    /// the fetched temporary; fetches php-src cannot substitute at compile
    /// time (class declared later, `static::`, a variable class name, a
    /// trait user) show exactly that.
    #[test]
    fn class_constants_are_held_once_per_class_constant() {
        let out = run("<?php
debug_zval_dump(W::X);
class W { const X = [5]; }
class C { const X = [1]; const array Y = [2]; }
class D extends C { static function f() { debug_zval_dump(static::X); } }
trait T { const K = [3]; }
class U { use T; }
class V { use T; }
D::f();
$c = 'C';
debug_zval_dump($c::Y);
debug_zval_dump(V::K);
");
        assert_eq!(
            out,
            "array(1) packed refcount(2){\n  [0]=>\n  int(5)\n}\n\
             array(1) packed refcount(2){\n  [0]=>\n  int(1)\n}\n\
             array(1) packed refcount(2){\n  [0]=>\n  int(2)\n}\n\
             array(1) packed refcount(4){\n  [0]=>\n  int(3)\n}\n"
        );
    }

    /// An alias is the same class entry (one default table); a promoted
    /// constructor property has no default in the class.
    #[test]
    fn aliases_and_promoted_properties_add_no_handles() {
        let out = run("<?php
class A { public $a = [1]; }
class_alias('A', 'AA');
class B extends A {}
$b = new B;
debug_zval_dump($b);
class P { function __construct(public $a = [7]) {} }
$p = new P;
debug_zval_dump($p);
");
        assert_eq!(
            out,
            "object(B)#1 (1) refcount(2){\n  [\"a\"]=>\n  array(1) packed refcount(3){\n    [0]=>\n    int(1)\n  }\n}\n\
             object(P)#2 (1) refcount(2){\n  [\"a\"]=>\n  array(1) packed refcount(2){\n    [0]=>\n    int(7)\n  }\n}\n"
        );
    }
}
