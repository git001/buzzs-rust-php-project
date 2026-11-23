// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `serialize()` / `unserialize()` (`ext/standard/var.c`,
//! `ext/standard/var_unserializer.re`).
//!
//! Scalars, arrays and references are handled here; objects go through the
//! [`RuntimeContext`] hooks ([`RuntimeContext::serialize_object`],
//! [`RuntimeContext::unserialize_object_create`] and friends) because they
//! involve `__serialize`, `__sleep`, `__wakeup`, `Serializable` and class
//! lookup, which live in the VM.

use crate::array::{ArrayKey, HashTable, ZArray};
use crate::error::{ErrorLevel, RtResult, RuntimeContext, RuntimeError};
use crate::float::zend_gcvt;
use crate::numeric::{handle_numeric_str, strtod};
use crate::string::ZStr;
use crate::value::{PhpRef, Value};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// How an object serializes itself (decided by the VM).
#[derive(Debug, Clone)]
pub enum SerializedObject {
    /// `O:len:"Class":n:{...}` with the given (mangled-key) property table
    /// (from `__serialize`, `__sleep` or the default property table).
    Object {
        /// Class name written to the output.
        class: ZStr,
        /// Properties / `__serialize` data.
        props: ZArray,
    },
    /// `E:len:"Class:Case";` for enum cases.
    Enum {
        /// Enum class name.
        class: ZStr,
        /// Case name.
        case: ZStr,
    },
    /// `C:len:"Class":n:{data}` for `Serializable` implementations.
    Custom {
        /// Class name.
        class: ZStr,
        /// Payload returned by `serialize()`.
        data: Vec<u8>,
    },
    /// Serialization failed without exception: written as `N;` and later
    /// references to the object also become `N;`.
    Null,
}

// ---------------------------------------------------------------------------
// serialize
// ---------------------------------------------------------------------------

/// State shared across nested serialization calls within the same context.
#[derive(Default, Debug)]
pub struct SerContext {
    pub n: i64,
    /// Identity -> `n` of the first occurrence (-1: failed Serializable).
    pub seen: HashMap<usize, i64>,
    /// Keeps registered values alive so their addresses stay unique.
    pub keep: Vec<Value>,
}

type SerRc = Rc<RefCell<SerContext>>;

thread_local! {
    static SER_LOCK: Cell<u32> = const { Cell::new(0) };
    static SER_CTX: RefCell<(u32, Option<SerRc>)> = const { RefCell::new((0, None)) };
    static ACTIVE_OBJECTS: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

/// Runs `f` with `BG(serialize_lock)` raised (nested user callbacks get a fresh context).
pub fn serialize_locked<R>(f: impl FnOnce() -> R) -> R {
    SER_LOCK.with(|l| l.set(l.get() + 1));
    struct LockGuard;
    impl Drop for LockGuard {
        fn drop(&mut self) {
            SER_LOCK.with(|l| l.set(l.get().saturating_sub(1)));
        }
    }
    let _guard = LockGuard;
    f()
}

/// Runs `f` with an isolated serializer context and lock held.
pub fn serialize_isolated<R>(f: impl FnOnce() -> R) -> R {
    let saved_lock = SER_LOCK.with(|l| l.replace(1));
    let saved_ctx = SER_CTX.with(|c| std::mem::replace(&mut *c.borrow_mut(), (0, None)));
    struct IsoGuard {
        lock: u32,
        ctx: (u32, Option<SerRc>),
    }
    impl Drop for IsoGuard {
        fn drop(&mut self) {
            SER_LOCK.with(|l| l.set(self.lock));
            SER_CTX.with(|c| *c.borrow_mut() = self.ctx.clone());
        }
    }
    let _guard = IsoGuard {
        lock: saved_lock,
        ctx: saved_ctx,
    };
    f()
}

fn is_locked() -> bool {
    SER_LOCK.with(Cell::get) > 0
}

fn ser_ctx_init() -> (SerRc, bool) {
    if is_locked() {
        return (Rc::new(RefCell::new(SerContext::default())), false);
    }
    SER_CTX.with(|s| {
        let mut s = s.borrow_mut();
        if s.0 == 0 || s.1.is_none() {
            let d = Rc::new(RefCell::new(SerContext::default()));
            *s = (1, Some(d.clone()));
            (d, true)
        } else {
            s.0 += 1;
            (s.1.clone().unwrap_or_default(), false)
        }
    })
}

fn ser_ctx_destroy(is_owner: bool) {
    if is_locked() {
        return;
    }
    SER_CTX.with(|s| {
        let mut s = s.borrow_mut();
        s.0 = s.0.saturating_sub(1);
        if s.0 == 0 || is_owner {
            s.0 = 0;
            s.1 = None;
        }
    });
}

struct Serializer {
    out: Vec<u8>,
    context: SerRc,
    active_arrays: Vec<usize>,
}

/// `serialize($value)`.
pub fn serialize(value: &Value, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Vec<u8>> {
    let (d, is_owner) = ser_ctx_init();
    struct CtxGuard {
        is_owner: bool,
    }
    impl Drop for CtxGuard {
        fn drop(&mut self) {
            ser_ctx_destroy(self.is_owner);
        }
    }
    let _guard = CtxGuard { is_owner };
    let mut s = Serializer {
        out: Vec::new(),
        context: d,
        active_arrays: Vec::new(),
    };
    s.value(value, ctx)?;
    Ok(s.out)
}

fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(format!("s:{}:\"", s.len()).as_bytes());
    out.extend_from_slice(s);
    out.extend_from_slice(b"\";");
}

fn write_key(out: &mut Vec<u8>, k: &ArrayKey) {
    match k {
        ArrayKey::Int(i) => out.extend_from_slice(format!("i:{i};").as_bytes()),
        ArrayKey::Str(s) => write_string(out, s.as_bytes()),
    }
}

impl Serializer {
    fn value(&mut self, v: &Value, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<()> {
        let key = match v {
            Value::Reference(r) => match &*r.borrow() {
                Value::Object(o) => Some(o.addr()),
                _ => Some(r.addr()),
            },
            Value::Object(o) => Some(o.addr()),
            _ => None,
        };
        {
            let mut c = self.context.borrow_mut();
            c.n += 1;
            if let Some(key) = key {
                if let Some(&id) = c.seen.get(&key) {
                    if v.is_reference() && id != -1 {
                        c.n -= 1;
                    }
                    drop(c);
                    if id == -1 {
                        self.out.extend_from_slice(b"N;");
                    } else if v.is_reference() {
                        self.out.extend_from_slice(format!("R:{id};").as_bytes());
                    } else {
                        self.out.extend_from_slice(format!("r:{id};").as_bytes());
                    }
                    return Ok(());
                }
                let n = c.n;
                c.seen.insert(key, n);
                c.keep.push(v.clone());
            }
        }
        let v = v.deref();
        match &v {
            Value::Null | Value::Resource(_) => {
                // Resources serialize as integer 0 in PHP ("i:0;").
                if matches!(v, Value::Resource(_)) {
                    self.out.extend_from_slice(b"i:0;");
                } else {
                    self.out.extend_from_slice(b"N;");
                }
            }
            Value::Bool(b) => self
                .out
                .extend_from_slice(if *b { b"b:1;" } else { b"b:0;" }),
            Value::Int(i) => self.out.extend_from_slice(format!("i:{i};").as_bytes()),
            Value::Float(d) => {
                let p = i32::try_from(ctx.serialize_precision().clamp(-1, 1000)).unwrap_or(17);
                self.out.extend_from_slice(b"d:");
                self.out
                    .extend_from_slice(zend_gcvt(*d, p, b'.', b'E').as_bytes());
                self.out.push(b';');
            }
            Value::Str(s) => write_string(&mut self.out, s.as_bytes()),
            Value::Array(a) => self.table(b"a", None, a, ctx)?,
            Value::Object(o) => {
                let is_recursive = ACTIVE_OBJECTS.with(|ao| !ao.borrow_mut().insert(o.addr()));
                if is_recursive {
                    self.context.borrow_mut().seen.insert(o.addr(), -1);
                    self.out.extend_from_slice(b"N;");
                    return Ok(());
                }
                struct ObjGuard(usize);
                impl Drop for ObjGuard {
                    fn drop(&mut self) {
                        ACTIVE_OBJECTS.with(|ao| {
                            ao.borrow_mut().remove(&self.0);
                        });
                    }
                }
                let _guard = ObjGuard(o.addr());
                match ctx.serialize_object(o)? {
                    SerializedObject::Object { class, props } => {
                        self.table(b"O", Some(&class), &props, ctx)?;
                    }
                    SerializedObject::Enum { class, case } => {
                        self.out.extend_from_slice(
                            format!("E:{}:\"", class.len() + 1 + case.len()).as_bytes(),
                        );
                        self.out.extend_from_slice(class.as_bytes());
                        self.out.push(b':');
                        self.out.extend_from_slice(case.as_bytes());
                        self.out.extend_from_slice(b"\";");
                    }
                    SerializedObject::Custom { class, data } => {
                        self.out
                            .extend_from_slice(format!("C:{}:\"", class.len()).as_bytes());
                        self.out.extend_from_slice(class.as_bytes());
                        self.out
                            .extend_from_slice(format!("\":{}:{{", data.len()).as_bytes());
                        self.out.extend_from_slice(&data);
                        self.out.push(b'}');
                    }
                    SerializedObject::Null => {
                        self.context.borrow_mut().seen.insert(o.addr(), -1);
                        self.out.extend_from_slice(b"N;");
                    }
                }
            }
            Value::Reference(_) => unreachable!("dereferenced"),
        }
        Ok(())
    }

    fn table(
        &mut self,
        tag: &[u8],
        class: Option<&ZStr>,
        t: &HashTable,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<()> {
        self.out.extend_from_slice(tag);
        self.out.push(b':');
        if let Some(c) = class {
            self.out
                .extend_from_slice(format!("{}:\"", c.len()).as_bytes());
            self.out.extend_from_slice(c.as_bytes());
            self.out.extend_from_slice(b"\":");
        }
        self.out
            .extend_from_slice(format!("{}:{{", t.len()).as_bytes());
        // `php_var_serialize_nested_data`: an array element that is the
        // array being serialized (`$t = &$a; $a[] = $t;`) is written as
        // `N;` (counted like any value).
        let own = (tag == b"a").then(|| std::ptr::from_ref(t) as usize);
        if let Some(addr) = own {
            self.active_arrays.push(addr);
        }
        let res = self.table_body(t, own, ctx);
        if own.is_some() {
            self.active_arrays.pop();
        }
        res
    }

    fn table_body(
        &mut self,
        t: &HashTable,
        own: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<()> {
        for (k, v) in t {
            write_key(&mut self.out, k);
            let is_own = match v {
                Value::Reference(r) if r.refcount() == 1 => {
                    let inner = r.get();
                    matches!(inner, Value::Array(ref e) if Some(e.addr()) == own || self.active_arrays.contains(&e.addr()))
                }
                Value::Array(e) => Some(e.addr()) == own || self.active_arrays.contains(&e.addr()),
                _ => false,
            };
            if is_own {
                self.context.borrow_mut().n += 1;
                self.out.extend_from_slice(b"N;");
            } else {
                match v {
                    Value::Reference(r) if r.refcount() == 1 => {
                        let inner = r.get();
                        self.value(&inner, ctx)?;
                    }
                    other => self.value(other, ctx)?,
                }
            }
        }
        self.out.push(b'}');
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// unserialize
// ---------------------------------------------------------------------------

/// The class objects of unknown or disallowed classes are created as.
pub const INCOMPLETE_CLASS: &str = "__PHP_Incomplete_Class";
/// The property of an incomplete object holding the original class name
/// (`MAGIC_MEMBER`).
pub const INCOMPLETE_CLASS_NAME_MEMBER: &str = "__PHP_Incomplete_Class_Name";
/// The property holding the raw `C:` payload of an object unserialized with
/// [`UnserializeOptions::incomplete_classes_only`] (`MAGIC_MEMBER_SERIALIZED`).
pub const INCOMPLETE_SERIALIZED_MEMBER: &str = "__PHP_Incomplete_Class_Serialized";
/// The property holding the case name of an `E:` value unserialized with
/// [`UnserializeOptions::incomplete_classes_only`] (`MAGIC_MEMBER_ENUM_CASE`).
pub const INCOMPLETE_ENUM_CASE_MEMBER: &str = "__PHP_Incomplete_Enum_Case";

/// `HT_MAX_SIZE` (64-bit): the largest element count an `a:` header may give.
const HT_MAX_SIZE: i64 = 0x4000_0000;
/// `MAX_LENGTH_OF_LONG - 1`: significant digits `parse_iv2()` accepts.
const MAX_LONG_DIGITS: usize = 19;

/// The `allowed_classes` option of `unserialize()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AllowedClasses {
    /// `true` (the default): every class may be instantiated.
    #[default]
    All,
    /// `false` or an array of names: only these classes (stored lowercase)
    /// may be instantiated; an empty set allows none. Objects of other
    /// classes become `__PHP_Incomplete_Class`.
    Only(HashSet<Vec<u8>>),
}

impl AllowedClasses {
    /// `allowed_classes => false`.
    #[must_use]
    pub fn none() -> Self {
        Self::Only(HashSet::new())
    }

    /// `allowed_classes => [names...]` (matched case-insensitively).
    pub fn only<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        Self::Only(
            names
                .into_iter()
                .map(|n| n.as_ref().to_ascii_lowercase())
                .collect(),
        )
    }

    /// `unserialize_allowed_class()`: whether objects of `class` may be
    /// instantiated.
    #[must_use]
    pub fn allows(&self, class: &[u8]) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => !set.is_empty() && set.contains(&class.to_ascii_lowercase()),
        }
    }
}

/// Options for [`unserialize`] / [`unserialize_partial`] (the state
/// `php_unserialize_with_options()` stores in the `var_hash`).
#[derive(Debug, Clone)]
pub struct UnserializeOptions {
    /// `max_depth` (0 = unlimited). Defaults to the `unserialize_max_depth`
    /// ini default, [`UnserializeOptions::DEFAULT_MAX_DEPTH`]; a negative ini
    /// value means unlimited (see [`UnserializeOptions::max_depth_from_ini`]).
    pub max_depth: usize,
    /// `allowed_classes`.
    pub allowed_classes: AllowedClasses,
    /// The nesting depth already reached when this call starts (`cur_depth`
    /// of a shared `var_hash`): a nested `unserialize()` made from
    /// `Serializable::unserialize()` continues the outer depth, so the
    /// combined depth is limited.
    pub depth: usize,
    /// The function named in "`%s(): `" diagnostics (`php_error_docref`).
    /// Empty: no prefix.
    pub function: Cow<'static, str>,
    /// `php_var_unserialize_set_incomplete_classes_only()`: no class is
    /// looked up at all; every `O:`/`C:`/`E:` value becomes an incomplete
    /// object that keeps its raw payload (`C:`) or case name (`E:`).
    pub incomplete_classes_only: bool,
}

impl UnserializeOptions {
    /// The `unserialize_max_depth` ini default.
    pub const DEFAULT_MAX_DEPTH: usize = 4096;

    /// Converts an `unserialize_max_depth` ini value (non-positive values
    /// disable the limit).
    #[must_use]
    pub fn max_depth_from_ini(value: i64) -> usize {
        usize::try_from(value).unwrap_or(0)
    }
}

impl Default for UnserializeOptions {
    fn default() -> Self {
        Self {
            max_depth: Self::DEFAULT_MAX_DEPTH,
            allowed_classes: AllowedClasses::All,
            depth: 0,
            function: Cow::Borrowed("unserialize"),
            incomplete_classes_only: false,
        }
    }
}

/// What [`RuntimeContext::unserialize_enum_case`] found for an `E:` value.
#[derive(Debug, Clone)]
pub enum EnumCaseLookup {
    /// The case object.
    Found(Value),
    /// No such class ("Class '%s' not found").
    ClassNotFound,
    /// The class is not an enum ("Class '%s' is not an enum").
    NotAnEnum,
    /// The enum has no constant of that name ("Undefined constant `%s::%s`").
    UndefinedCase,
    /// The constant is not a case ("`%s::%s` is not an enum case").
    NotACase,
}

/// `zend_is_valid_class_name()`.
fn valid_class_name(name: &[u8]) -> bool {
    name.iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'\\' || c >= 0x80)
}

enum Node {
    Scalar(Value),
    Array(Vec<(ArrayKey, usize)>),
    /// `R:` — reference to another node.
    Ref(usize),
    /// `r:` — the same object as another node.
    ObjRef(usize),
    Object {
        class: ZStr,
        props: Vec<(ArrayKey, usize)>,
        /// Created as `__PHP_Incomplete_Class` with these properties set
        /// first (the original class name, raw payloads).
        incomplete: Option<Vec<(&'static str, Value)>>,
    },
    /// A resolved enum case.
    Enum(Value),
    Custom {
        class: ZStr,
        data: Vec<u8>,
    },
}

/// Slot of a value whose node does not exist yet (the value being parsed).
const UNFILLED: usize = usize::MAX;

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    nodes: Vec<Node>,
    /// `var_hash` slots: slot id - 1 → node index.
    slots: Vec<usize>,
    /// Node index → its `var_hash` slot (nodes made by `R:` have none).
    node_slot: Vec<Option<usize>>,
    opts: &'a UnserializeOptions,
    depth: usize,
    /// An exception raised by a hook or a diagnostic handler (a pending
    /// `EG(exception)`): parsing fails without the offset warning.
    error: Option<RuntimeError>,
}

/// Result of a failed parse step: the C code's `return 0` (with `*p` set).
struct Fail;

impl<'a> Parser<'a> {
    fn new(data: &'a [u8], pos: usize, opts: &'a UnserializeOptions) -> Self {
        Self {
            data,
            pos,
            nodes: Vec::new(),
            slots: Vec::new(),
            node_slot: Vec::new(),
            opts,
            depth: opts.depth,
            error: None,
        }
    }

    fn at(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0)
    }

    fn max(&self) -> usize {
        self.data.len()
    }

    /// Reports a diagnostic; an exception thrown by the handler becomes the
    /// pending error.
    fn emit(&mut self, ctx: &mut (impl RuntimeContext + ?Sized), level: ErrorLevel, msg: &str) {
        if let Err(e) = ctx.diagnostic(level, msg) {
            self.error.get_or_insert(e);
        }
    }

    /// `php_error_docref()`: prefixed with the calling function.
    fn docref(&mut self, ctx: &mut (impl RuntimeContext + ?Sized), level: ErrorLevel, msg: &str) {
        if self.opts.function.is_empty() {
            self.emit(ctx, level, msg);
        } else {
            let full = format!("{}(): {msg}", self.opts.function);
            self.emit(ctx, level, &full);
        }
    }

    /// `uiv = [0-9]+` starting at `i`; returns (value, end).
    fn uiv(&self, i: usize) -> Option<(u64, usize)> {
        let mut j = i;
        let mut v: u64 = 0;
        while j < self.max() && self.data[j].is_ascii_digit() {
            v = v
                .wrapping_mul(10)
                .wrapping_add(u64::from(self.data[j] - b'0'));
            j += 1;
        }
        (j > i).then_some((v, j))
    }

    /// `parse_iv2(p, &q)`: optional sign, leading zeros, digits (possibly
    /// none) starting at `i`; returns the value and the end position, with
    /// the "Numerical result out of range" warning (and saturation) on
    /// overflow.
    fn parse_iv2(&mut self, i: usize, ctx: &mut (impl RuntimeContext + ?Sized)) -> (i64, usize) {
        let mut j = i;
        let neg = self.at(j) == b'-';
        if matches!(self.at(j), b'+' | b'-') {
            j += 1;
        }
        while j < self.max() && self.data[j] == b'0' {
            j += 1;
        }
        let digits_start = j;
        let mut v: u64 = 0;
        while j < self.max() && self.data[j].is_ascii_digit() {
            v = v
                .wrapping_mul(10)
                .wrapping_add(u64::from(self.data[j] - b'0'));
            j += 1;
        }
        let limit = if neg { 1u64 << 63 } else { (1u64 << 63) - 1 };
        if j - digits_start > MAX_LONG_DIGITS || v > limit {
            self.docref(ctx, ErrorLevel::Warning, "Numerical result out of range");
            return (if neg { i64::MIN } else { i64::MAX }, j);
        }
        #[allow(clippy::cast_possible_wrap)]
        let r = if neg {
            v.wrapping_neg() as i64
        } else {
            v as i64
        };
        (r, j)
    }

    fn push_node(&mut self, n: Node, slot: Option<usize>) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(n);
        self.node_slot.push(slot);
        if let Some(s) = slot {
            self.slots[s] = idx;
        }
        idx
    }

    /// `php_var_unserialize_internal()`. `as_key` means `var_hash == NULL`
    /// (array keys); `overwriting` is the node currently stored at the key
    /// being written (a duplicate key), which back-references must not
    /// target.
    fn value(
        &mut self,
        as_key: bool,
        overwriting: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<usize, Fail> {
        let start = self.pos;
        if start >= self.max() {
            return Err(Fail);
        }
        let tag = self.data[start];
        let slot = if !as_key && tag != b'R' {
            self.slots.push(UNFILLED);
            Some(self.slots.len() - 1)
        } else {
            None
        };
        let colon = self.at(start + 1) == b':';
        match tag {
            b'N' if self.at(start + 1) == b';' => {
                self.pos = start + 2;
                Ok(self.push_node(Node::Scalar(Value::Null), slot))
            }
            b'b' if colon
                && matches!(self.at(start + 2), b'0' | b'1')
                && self.at(start + 3) == b';' =>
            {
                self.pos = start + 4;
                let b = self.at(start + 2) == b'1';
                Ok(self.push_node(Node::Scalar(Value::Bool(b)), slot))
            }
            b'i' if colon => {
                let mut j = start + 2;
                if matches!(self.at(j), b'+' | b'-') {
                    j += 1;
                }
                let (_, end) = self.uiv(j).ok_or(Fail)?;
                if self.at(end) != b';' {
                    return Err(Fail);
                }
                self.pos = end + 1;
                let (v, _) = self.parse_iv2(start + 2, ctx);
                Ok(self.push_node(Node::Scalar(Value::Int(v)), slot))
            }
            b'd' if colon => {
                let rest = &self.data[start + 2..];
                for (lit, v) in [
                    (&b"NAN;"[..], f64::NAN),
                    (&b"INF;"[..], f64::INFINITY),
                    (&b"-INF;"[..], f64::NEG_INFINITY),
                ] {
                    if rest.starts_with(lit) {
                        self.pos = start + 2 + lit.len();
                        return Ok(self.push_node(Node::Scalar(Value::Float(v)), slot));
                    }
                }
                let end = self.float_end(start + 2).ok_or(Fail)?;
                if self.at(end) != b';' {
                    return Err(Fail);
                }
                let (v, _) = strtod(&self.data[start + 2..end]);
                self.pos = end + 1;
                Ok(self.push_node(Node::Scalar(Value::Float(v)), slot))
            }
            b's' | b'S' if colon => {
                let s = self.string(start, tag == b'S', ctx)?;
                Ok(self.push_node(Node::Scalar(Value::Str(ZStr::new(s))), slot))
            }
            b'a' if colon => self.array(start, as_key, slot, ctx),
            b'R' | b'r' if colon => self.back_ref(start, tag == b'R', as_key, overwriting, slot),
            b'O' | b'C' if colon && !as_key => self.object(start, tag == b'C', slot, ctx),
            b'E' if colon && !as_key => self.enum_case(start, slot, ctx),
            b'}' => {
                self.docref(
                    ctx,
                    ErrorLevel::Warning,
                    "Unexpected end of serialized data",
                );
                Err(Fail)
            }
            _ => Err(Fail),
        }
    }

    /// `"s:" uiv ":" ["]` and `"S:" uiv ":" ["]`: the string's bytes.
    fn string(
        &mut self,
        start: usize,
        escaped: bool,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<Vec<u8>, Fail> {
        let (len, after) = self.uiv(start + 2).ok_or(Fail)?;
        if self.at(after) != b':' || self.at(after + 1) != b'"' {
            return Err(Fail);
        }
        let body = after + 2;
        let maxlen = self.max() - body.min(self.max());
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if maxlen < len {
            self.pos = start + 2;
            return Err(Fail);
        }
        let (bytes, end) = if escaped {
            self.unescape_s(body, len).ok_or(Fail)?
        } else {
            (self.data[body..body + len].to_vec(), body + len)
        };
        if self.at(end) != b'"' {
            self.pos = end;
            return Err(Fail);
        }
        if self.at(end + 1) != b';' {
            self.pos = end + 1;
            return Err(Fail);
        }
        self.pos = end + 2;
        if escaped {
            self.docref(
                ctx,
                ErrorLevel::Deprecated,
                "Unserializing the 'S' format is deprecated",
            );
        }
        Ok(bytes)
    }

    /// `"a:" uiv ":" "{"`.
    fn array(
        &mut self,
        start: usize,
        as_key: bool,
        slot: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<usize, Fail> {
        // The rule is `"a:" uiv ":" "{"`: no sign allowed.
        let (_, end) = self.uiv(start + 2).ok_or(Fail)?;
        if self.at(end) != b':' || self.at(end + 1) != b'{' {
            return Err(Fail);
        }
        // `parse_iv()` (with its range warning) runs before the key check.
        let (elements, _) = self.parse_iv2(start + 2, ctx);
        self.pos = end + 2;
        if as_key {
            return Err(Fail);
        }
        let room = i64::try_from((self.max() - self.pos) / 2).unwrap_or(i64::MAX);
        if !(0..HT_MAX_SIZE).contains(&elements) || elements > room {
            return Err(Fail);
        }
        let idx = self.push_node(Node::Array(Vec::new()), slot);
        if elements > 0 {
            let items = self.nested(elements, false, ctx)?;
            self.nodes[idx] = Node::Array(items);
        }
        self.finish_nested()?;
        Ok(idx)
    }

    /// `"R:" uiv ";"` / `"r:" uiv ";"`.
    fn back_ref(
        &mut self,
        start: usize,
        is_ref: bool,
        as_key: bool,
        overwriting: Option<usize>,
        slot: Option<usize>,
    ) -> Result<usize, Fail> {
        let (id, end) = self.uiv(start + 2).ok_or(Fail)?;
        if self.at(end) != b';' {
            return Err(Fail);
        }
        self.pos = end + 1;
        if as_key {
            return Err(Fail);
        }
        let id = usize::try_from(id).unwrap_or(0);
        if id == 0 || id > self.slots.len() {
            return Err(Fail);
        }
        let target = self.slots[id - 1];
        // `rval_ref == rval`: the value being written (its own slot, or the
        // slot of the location a duplicate key overwrites).
        if target == UNFILLED || Some(target) == overwriting {
            return Err(Fail);
        }
        if is_ref {
            return Ok(self.push_node(Node::Ref(target), slot));
        }
        let mut t = target;
        while let Node::Ref(next) | Node::ObjRef(next) = self.nodes[t] {
            t = next;
        }
        if !matches!(
            self.nodes[t],
            Node::Object { .. } | Node::Enum(_) | Node::Custom { .. }
        ) {
            return Err(Fail);
        }
        Ok(self.push_node(Node::ObjRef(t), slot))
    }

    /// End of `d:` payload matching `iv | nv | nvexp`.
    fn float_end(&self, i: usize) -> Option<usize> {
        let mut j = i;
        if matches!(self.at(j), b'+' | b'-') {
            j += 1;
        }
        let int_start = j;
        while j < self.max() && self.data[j].is_ascii_digit() {
            j += 1;
        }
        let int_digits = j - int_start;
        if self.at(j) == b'.' {
            let f = j + 1;
            let mut k = f;
            while k < self.max() && self.data[k].is_ascii_digit() {
                k += 1;
            }
            if int_digits == 0 && k == f {
                return None;
            }
            j = k;
        } else if int_digits == 0 {
            return None;
        }
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

    /// `unserialize_str()`: decodes the escaped `S:` format (`\xx` hex
    /// escapes).
    fn unescape_s(&self, body: usize, len: usize) -> Option<(Vec<u8>, usize)> {
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: at most the length of the input string"
        )]
        let mut out = Vec::with_capacity(len.min(self.max()));
        let mut i = body;
        for _ in 0..len {
            if i >= self.max() {
                return None;
            }
            if self.data[i] == b'\\' {
                let hi = (*self.data.get(i + 1)? as char).to_digit(16)?;
                let lo = (*self.data.get(i + 2)? as char).to_digit(16)?;
                #[allow(clippy::cast_possible_truncation)]
                out.push((hi * 16 + lo) as u8);
                i += 3;
            } else {
                out.push(self.data[i]);
                i += 1;
            }
        }
        Some((out, i))
    }

    /// `process_nested_array_data()` / `process_nested_object_data()`.
    fn nested(
        &mut self,
        elements: i64,
        object: bool,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<Vec<(ArrayKey, usize)>, Fail> {
        let max_depth = self.opts.max_depth;
        if max_depth > 0 && self.depth >= max_depth {
            self.docref(
                ctx,
                ErrorLevel::Warning,
                &format!(
                    "Maximum depth of {max_depth} exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting"
                ),
            );
            return Err(Fail);
        }
        self.depth += 1;
        let r = self.nested_items(elements, object, ctx);
        self.depth -= 1;
        r
    }

    fn nested_items(
        &mut self,
        elements: i64,
        object: bool,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<Vec<(ArrayKey, usize)>, Fail> {
        let mut items: Vec<(ArrayKey, usize)> = Vec::new();
        // Position of each key in `items`, for O(1) duplicate handling. (The
        // cached hash inside `ZStr` is interior mutability that does not
        // affect `Hash`/`Eq`.)
        #[allow(clippy::mutable_key_type)]
        let mut index: HashMap<ArrayKey, usize> = HashMap::new();
        let mut remaining = elements;
        while remaining > 0 {
            remaining -= 1;
            let k = self.value(true, None, ctx)?;
            // Keys keep symbol-table semantics (numeric strings are integers)
            // for objects too: that is what `__unserialize()` receives, and
            // the host converts them to property names when filling
            // properties.
            let key = match &self.nodes[k] {
                Node::Scalar(Value::Int(i)) => ArrayKey::Int(*i),
                Node::Scalar(Value::Str(s)) => {
                    if object && self.opts.incomplete_classes_only && is_reserved_member(s) {
                        return Err(Fail);
                    }
                    match handle_numeric_str(s.as_bytes()) {
                        Some(i) => ArrayKey::Int(i),
                        None => ArrayKey::Str(s.clone()),
                    }
                }
                _ => return Err(Fail),
            };
            let existing = index.get(&key).copied();
            let v = self.value(false, existing.map(|p| items[p].1), ctx)?;
            if let Some(pos) = existing {
                // The overwritten value's `var_hash` slot pointed at the
                // location, which now holds the new value.
                let old = items[pos].1;
                if let Some(s) = self.node_slot[old] {
                    self.slots[s] = v;
                }
                items[pos].1 = v;
            } else {
                index.insert(key.clone(), items.len());
                items.push((key, v));
            }
            if remaining > 0 {
                let prev = self.at(self.pos.wrapping_sub(1));
                if prev != b';' && prev != b'}' {
                    self.pos -= 1;
                    return Err(Fail);
                }
            }
        }
        Ok(items)
    }

    /// `finish_nested_data()`.
    fn finish_nested(&mut self) -> Result<(), Fail> {
        if self.pos >= self.max() || self.data[self.pos] != b'}' {
            return Err(Fail);
        }
        self.pos += 1;
        Ok(())
    }

    /// `object ":" uiv ":" ["]` (`O:` and `C:` entries).
    fn object(
        &mut self,
        start: usize,
        custom: bool,
        slot: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<usize, Fail> {
        let (len, after) = self.uiv(start + 2).ok_or(Fail)?;
        if self.at(after) != b':' || self.at(after + 1) != b'"' {
            return Err(Fail);
        }
        let body = after + 2;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if self.max() - body.min(self.max()) < len || len == 0 {
            self.pos = start + 2;
            return Err(Fail);
        }
        let end = body + len;
        if self.at(end) != b'"' {
            self.pos = end;
            return Err(Fail);
        }
        if self.at(end + 1) != b':' {
            self.pos = end + 1;
            return Err(Fail);
        }
        let name = &self.data[body..end];
        // Runtime definition keys and names starting with a namespace
        // separator are rejected, as are names that cannot be classes (the
        // C code checks the latter only for names missing from the class
        // table, which such names always are).
        if name[0] == 0 || name[0] == b'\\' || !valid_class_name(name) {
            return Err(Fail);
        }
        let class = ZStr::from(name);
        let incomplete =
            self.opts.incomplete_classes_only || !self.opts.allowed_classes.allows(name);
        self.pos = end;
        if custom {
            return self.object_custom(class, incomplete, slot, ctx);
        }
        if end + 2 >= self.max() {
            self.emit(ctx, ErrorLevel::Warning, "Bad unserialize data");
            return Err(Fail);
        }
        // O:len:"Class":n:{props}
        let (elements, n_end) = self.parse_iv2(end + 2, ctx);
        self.pos = n_end;
        let room = i64::try_from((self.max() - end) / 2).unwrap_or(i64::MAX);
        if elements < 0 || elements > room {
            return Err(Fail);
        }
        if self.at(n_end) != b':' {
            return Err(Fail);
        }
        if self.at(n_end + 1) != b'{' {
            self.pos = n_end + 1;
            return Err(Fail);
        }
        self.pos = n_end + 2;
        let incomplete =
            incomplete.then(|| vec![(INCOMPLETE_CLASS_NAME_MEMBER, Value::Str(class.clone()))]);
        let idx = self.push_node(
            Node::Object {
                class: class.clone(),
                props: Vec::new(),
                incomplete: None,
            },
            slot,
        );
        let props = self.nested(elements, true, ctx)?;
        self.nodes[idx] = Node::Object {
            class,
            props,
            incomplete,
        };
        self.finish_nested()?;
        Ok(idx)
    }

    /// `object_custom()`: `C:len:"Class":datalen:{data}` after the class
    /// name.
    fn object_custom(
        &mut self,
        class: ZStr,
        incomplete: bool,
        slot: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<usize, Fail> {
        let (datalen, dl_end) = self.parse_iv2(self.pos + 2, ctx);
        self.pos = dl_end;
        if self.max() - self.pos.min(self.max()) < 2 || self.at(self.pos) != b':' {
            return Err(Fail);
        }
        if self.at(self.pos + 1) != b'{' {
            self.pos += 1;
            return Err(Fail);
        }
        self.pos += 2;
        let avail = self.max() - self.pos;
        let dlen = usize::try_from(datalen).unwrap_or(usize::MAX);
        if datalen < 0 || avail <= dlen {
            self.emit(
                ctx,
                ErrorLevel::Warning,
                &format!(
                    "Insufficient data for unserializing - {datalen} required, {avail} present"
                ),
            );
            return Err(Fail);
        }
        if self.data[self.pos + dlen] != b'}' {
            self.pos += dlen;
            return Err(Fail);
        }
        let data = self.data[self.pos..self.pos + dlen].to_vec();
        let node = if self.opts.incomplete_classes_only {
            // Keep the payload verbatim; the class name is stored after it.
            Node::Object {
                class: class.clone(),
                props: Vec::new(),
                incomplete: Some(vec![
                    (INCOMPLETE_SERIALIZED_MEMBER, Value::Str(ZStr::new(data))),
                    (INCOMPLETE_CLASS_NAME_MEMBER, Value::Str(class)),
                ]),
            }
        } else if incomplete {
            self.emit(
                ctx,
                ErrorLevel::Warning,
                &format!("Class {INCOMPLETE_CLASS} has no unserializer"),
            );
            Node::Object {
                class: class.clone(),
                props: Vec::new(),
                incomplete: Some(vec![(INCOMPLETE_CLASS_NAME_MEMBER, Value::Str(class))]),
            }
        } else {
            Node::Custom { class, data }
        };
        self.pos += dlen + 1;
        Ok(self.push_node(node, slot))
    }

    /// `"E:" uiv ":" ["]`: the enum case is resolved while parsing, so that
    /// failures happen at php-src's offsets.
    fn enum_case(
        &mut self,
        start: usize,
        slot: Option<usize>,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> Result<usize, Fail> {
        let (len, after) = self.uiv(start + 2).ok_or(Fail)?;
        if self.at(after) != b':' || self.at(after + 1) != b'"' {
            return Err(Fail);
        }
        let body = after + 2;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if self.max() - body.min(self.max()) < len || len == 0 {
            self.pos = start + 2;
            return Err(Fail);
        }
        let end = body + len;
        if self.at(end) != b'"' {
            self.pos = end;
            return Err(Fail);
        }
        if self.at(end + 1) != b';' {
            self.pos = end + 1;
            return Err(Fail);
        }
        let text = &self.data[body..end];
        let Some(colon) = text.iter().position(|&c| c == b':') else {
            let msg = format!(
                "Invalid enum name '{}' (missing colon)",
                String::from_utf8_lossy(text)
            );
            self.docref(ctx, ErrorLevel::Warning, &msg);
            return Err(Fail);
        };
        let enum_name = ZStr::from(&text[..colon]);
        let case = ZStr::from(&text[colon + 1..]);
        if !valid_class_name(enum_name.as_bytes()) {
            return Err(Fail);
        }
        if self.opts.incomplete_classes_only {
            self.pos = end + 2;
            let node = Node::Object {
                class: enum_name.clone(),
                props: Vec::new(),
                incomplete: Some(vec![
                    (INCOMPLETE_CLASS_NAME_MEMBER, Value::Str(enum_name)),
                    (INCOMPLETE_ENUM_CASE_MEMBER, Value::Str(case)),
                ]),
            };
            return Ok(self.push_node(node, slot));
        }
        if self.error.is_some() {
            return Err(Fail);
        }
        let found = match ctx.unserialize_enum_case(&enum_name, &case) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return Err(Fail);
            }
        };
        let (e, c) = (enum_name.to_string_lossy(), case.to_string_lossy());
        match found {
            EnumCaseLookup::ClassNotFound => {
                self.docref(ctx, ErrorLevel::Warning, &format!("Class '{e}' not found"));
                Err(Fail)
            }
            EnumCaseLookup::NotAnEnum => {
                self.docref(
                    ctx,
                    ErrorLevel::Warning,
                    &format!("Class '{e}' is not an enum"),
                );
                Err(Fail)
            }
            EnumCaseLookup::UndefinedCase => {
                self.pos = end + 2;
                self.docref(
                    ctx,
                    ErrorLevel::Warning,
                    &format!("Undefined constant {e}::{c}"),
                );
                Err(Fail)
            }
            EnumCaseLookup::NotACase => {
                self.pos = end + 2;
                self.docref(
                    ctx,
                    ErrorLevel::Warning,
                    &format!("{e}::{c} is not an enum case"),
                );
                Err(Fail)
            }
            EnumCaseLookup::Found(v) => {
                self.pos = end + 2;
                Ok(self.push_node(Node::Enum(v), slot))
            }
        }
    }
}

/// The members `incomplete_classes_only` mode writes itself; data carrying
/// one of them is rejected (`is_incomplete_class_reserved_member()`).
fn is_reserved_member(name: &ZStr) -> bool {
    [
        INCOMPLETE_CLASS_NAME_MEMBER,
        INCOMPLETE_SERIALIZED_MEMBER,
        INCOMPLETE_ENUM_CASE_MEMBER,
    ]
    .iter()
    .any(|m| name.as_bytes() == m.as_bytes())
}

/// `unserialize($data, $options)`. Returns `Ok(None)` (PHP's `false`) on
/// malformed input after emitting "`unserialize()`: Error at offset X of Y
/// bytes"; emits "`unserialize()`: Extra data starting at offset X of Y
/// bytes" for trailing bytes after a valid value. Exceptions (from hooks or
/// from diagnostic handlers) are returned as `Err` without the offset
/// warning.
pub fn unserialize(
    data: &[u8],
    options: &UnserializeOptions,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<Option<Value>> {
    if data.is_empty() {
        return Ok(None);
    }
    let prefix = if options.function.is_empty() {
        String::new()
    } else {
        format!("{}(): ", options.function)
    };
    match unserialize_partial(data, 0, options, ctx)? {
        Err(offset) => {
            ctx.diagnostic(
                ErrorLevel::Warning,
                &format!("{prefix}Error at offset {offset} of {} bytes", data.len()),
            )?;
            Ok(None)
        }
        Ok((v, end)) => {
            if end < data.len() {
                ctx.diagnostic(
                    ErrorLevel::Warning,
                    &format!(
                        "{prefix}Extra data starting at offset {end} of {} bytes",
                        data.len()
                    ),
                )?;
            }
            Ok(Some(v))
        }
    }
}

/// `php_var_unserialize()`: unserializes the one value starting at `start`
/// in `data` (which may continue after it). Returns the value and the
/// offset just past it, or `Err(offset)` with the failure offset (relative
/// to the start of `data`, like `p - buf`) when the input is malformed; no
/// "Error at offset" / "Extra data" warning is emitted (other warnings are).
/// Exceptions are returned as the outer `Err`.
pub fn unserialize_partial(
    data: &[u8],
    start: usize,
    options: &UnserializeOptions,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<Result<(Value, usize), usize>> {
    let mut p = Parser::new(data, start, options);
    let parsed = p.value(false, None, ctx);
    if let Some(e) = p.error.take() {
        return Err(e);
    }
    let Ok(root) = parsed else {
        return Ok(Err(p.pos));
    };
    let mut b = Builder {
        nodes: &p.nodes,
        referenced: vec![false; p.nodes.len()],
        refs: HashMap::new(),
        objects: HashMap::new(),
    };
    for n in &p.nodes {
        if let Node::Ref(t) = n {
            b.referenced[*t] = true;
        }
    }
    let mut v = b.build(root, ctx)?;
    v.unwrap_reference();
    Ok(Ok((v, p.pos)))
}

struct Builder<'a> {
    nodes: &'a [Node],
    referenced: Vec<bool>,
    refs: HashMap<usize, PhpRef>,
    objects: HashMap<usize, Value>,
}

impl Builder<'_> {
    fn build(&mut self, idx: usize, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Value> {
        if self.referenced[idx] {
            if let Some(r) = self.refs.get(&idx) {
                return Ok(Value::Reference(r.clone()));
            }
            let r = PhpRef::new(Value::Null);
            self.refs.insert(idx, r.clone());
            let v = self.build_raw(idx, ctx)?;
            r.set(v);
            return Ok(Value::Reference(r));
        }
        self.build_raw(idx, ctx)
    }

    fn build_table(
        &mut self,
        preset: &[(&'static str, Value)],
        items: &[(ArrayKey, usize)],
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<HashTable> {
        // PHP's unserializer uses `array_finit_size_mixed`: non-empty arrays
        // are always hash tables, empty ones are the (packed) empty array.
        let n = preset.len() + items.len();
        let mut t = if n == 0 {
            HashTable::new()
        } else {
            HashTable::with_capacity_mixed(n)
        };
        for (k, v) in preset {
            t.insert_str_raw(ZStr::from(*k), v.clone());
        }
        for (k, v) in items {
            let val = self.build(*v, ctx)?;
            match k {
                ArrayKey::Str(s) => {
                    t.insert_str_raw(s.clone(), val);
                }
                ArrayKey::Int(i) => {
                    t.insert(*i, val);
                }
            }
        }
        Ok(t)
    }

    fn build_raw(
        &mut self,
        idx: usize,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<Value> {
        let nodes = self.nodes;
        match &nodes[idx] {
            Node::Scalar(v) | Node::Enum(v) => Ok(v.clone()),
            Node::Array(items) => Ok(Value::Array(self.build_table(&[], items, ctx)?.into())),
            Node::Ref(t) => self.build(*t, ctx),
            Node::ObjRef(t) => {
                let v = self.build(*t, ctx)?;
                Ok(v.deref())
            }
            Node::Object {
                class,
                props,
                incomplete,
            } => {
                if let Some(o) = self.objects.get(&idx) {
                    return Ok(o.clone());
                }
                let (obj, preset) = match incomplete {
                    Some(preset) => (
                        ctx.unserialize_object_create(&ZStr::from(INCOMPLETE_CLASS))?,
                        preset.as_slice(),
                    ),
                    None => (ctx.unserialize_object_create(class)?, &[][..]),
                };
                self.objects.insert(idx, obj.clone());
                let table = self.build_table(preset, props, ctx)?;
                ctx.unserialize_object_finish(&obj, ZArray::from_table(table))?;
                Ok(obj)
            }
            Node::Custom { class, data } => {
                if let Some(o) = self.objects.get(&idx) {
                    return Ok(o.clone());
                }
                let v = ctx.unserialize_custom(class, data)?;
                self.objects.insert(idx, v.clone());
                Ok(v)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::error::CollectingContext;
    use crate::value::Obj;

    fn ser(v: &Value) -> String {
        let mut c = CollectingContext::new();
        String::from_utf8(serialize(v, &mut c).unwrap()).unwrap()
    }

    fn unser(s: &str) -> (Option<Value>, Vec<String>) {
        let mut c = CollectingContext::new();
        let v = unserialize(s.as_bytes(), &UnserializeOptions::default(), &mut c).unwrap();
        (v, c.messages())
    }

    #[test]
    fn scalars() {
        assert_eq!(ser(&Value::Null), "N;");
        assert_eq!(ser(&Value::Bool(true)), "b:1;");
        assert_eq!(ser(&Value::Int(-5)), "i:-5;");
        assert_eq!(ser(&Value::Float(0.1)), "d:0.1;");
        assert_eq!(ser(&Value::Float(1.0)), "d:1;");
        assert_eq!(ser(&Value::Float(1e100)), "d:1.0E+100;");
        assert_eq!(ser(&Value::Float(-0.0)), "d:-0;");
        assert_eq!(ser(&Value::Float(f64::NAN)), "d:NAN;");
        assert_eq!(ser(&Value::Float(f64::NEG_INFINITY)), "d:-INF;");
        assert_eq!(ser(&Value::from("a\"b")), "s:3:\"a\"b\";");
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn arrays_and_references() {
        // $x = 1; $a = [&$x, &$x, "k" => [1.5]];
        let x = PhpRef::new(Value::Int(1));
        let mut t = HashTable::new();
        t.push(Value::Reference(x.clone())).unwrap();
        t.push(Value::Reference(x.clone())).unwrap();
        t.insert("k", Value::Array(ZArray::from_values([Value::Float(1.5)])));
        let a = Value::Array(t.into());
        let s = ser(&a);
        assert_eq!(s, "a:3:{i:0;i:1;i:1;R:2;s:1:\"k\";a:1:{i:0;d:1.5;}}");
        let (v, msgs) = unser(&s);
        assert!(msgs.is_empty());
        let v = v.unwrap();
        let arr = v.as_array().unwrap();
        let (Some(Value::Reference(r0)), Some(Value::Reference(r1))) =
            (arr.get_int(0), arr.get_int(1))
        else {
            panic!("expected references")
        };
        assert!(PhpRef::ptr_eq(r0, r1));
        r0.set(Value::Int(9));
        assert_eq!(r1.get().as_int(), Some(9));
        // Re-serializing gives the same string back.
        drop(x);
        assert_eq!(ser(&v).replace("i:9;", "i:1;"), s);
    }

    #[test]
    fn self_containing_array_serializes_null() {
        // $t = &$a; $a = [1]; $a[] = $t; serialize($a) (leaks the cycle)
        let mut a = ZArray::from_values([Value::Int(1), Value::Null]);
        assert!(a.store_self_at(&[None]).is_some());
        assert_eq!(ser(&Value::Array(a.clone())), "a:2:{i:0;i:1;i:1;N;}");
        // the element still counts for back-references
        let o = PhpRef::new(Value::Int(3));
        let mut b = ZArray::from_values([Value::Null, Value::Reference(o.clone())]);
        b.make_mut().push(Value::Reference(o)).unwrap();
        assert!(b.store_self_at(&[Some(ArrayKey::Int(0))]).is_some());
        assert_eq!(
            ser(&Value::Array(b.clone())),
            "a:3:{i:0;N;i:1;i:3;i:2;R:3;}"
        );
    }

    #[test]
    fn nested_serializable_joins_context_bug79031() {
        // Simulates Serializable::serialize() calling serialize() on an inner value.
        // It must share the outer seen table and slot numbering.
        struct SerializableMockContext {
            inner_val: Value,
        }
        impl RuntimeContext for SerializableMockContext {
            fn diagnostic(&mut self, _level: ErrorLevel, _message: &str) -> RtResult<()> {
                Ok(())
            }
            fn serialize_object(&mut self, _obj: &Obj) -> RtResult<SerializedObject> {
                // Serializable::serialize() does NOT lock, so nested serialize shares context.
                let mut inner_ctx = CollectingContext::new();
                let inner_bytes = serialize(&self.inner_val, &mut inner_ctx)?;
                Ok(SerializedObject::Custom {
                    class: ZStr::from("A"),
                    data: inner_bytes,
                })
            }
        }

        let shared_ref = PhpRef::new(Value::Int(42));
        let obj = Obj::dummy("A");
        let mut t = HashTable::new();
        t.push(Value::Reference(shared_ref.clone())).unwrap();
        t.push(Value::Object(obj.clone())).unwrap();
        t.push(Value::Reference(shared_ref.clone())).unwrap();

        let outer = Value::Array(t.into());
        // The nested call finds the reference as the outer #2. A
        // `C:` payload is `<length>:{<data>}` with no `;` after it
        // (`php_var_serialize_class_name` + the `Serializable` branch of
        // `php_var_serialize_intern`): "R:2;" is 4 bytes.
        let mut ctx = SerializableMockContext {
            inner_val: Value::Reference(shared_ref.clone()),
        };
        let out = serialize(&outer, &mut ctx).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "a:3:{i:0;i:42;i:1;C:1:\"A\":4:{R:2;}i:2;R:2;}");
        // What PHP code can pass (`serialize([&$x])` inside `serialize()`
        // of `$arr = [&$x, new A, &$x]`), as `/usr/bin/php -n` prints it.
        let mut ctx = SerializableMockContext {
            inner_val: Value::Array(ZArray::from_values([Value::Reference(shared_ref.clone())])),
        };
        let out = serialize(&outer, &mut ctx).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "a:3:{i:0;i:42;i:1;C:1:\"A\":14:{a:1:{i:0;R:2;}}i:2;R:2;}"
        );
    }

    #[test]
    fn nested_serialize_locked_isolates_context() {
        // Simulates __serialize() calling serialize() inside serialize_locked.
        // The nested serialize() has an isolated var table.
        struct MagicSerializeMockContext {
            inner_val: Value,
        }
        impl RuntimeContext for MagicSerializeMockContext {
            fn diagnostic(&mut self, _level: ErrorLevel, _message: &str) -> RtResult<()> {
                Ok(())
            }
            fn serialize_object(&mut self, _obj: &Obj) -> RtResult<SerializedObject> {
                // __serialize() runs with serialize_locked
                let nested_data = serialize_locked(|| {
                    let mut inner_ctx = CollectingContext::new();
                    serialize(&self.inner_val, &mut inner_ctx).unwrap()
                });
                let mut props = ZArray::new();
                props
                    .make_mut()
                    .insert("data", Value::Str(ZStr::from(nested_data)));
                Ok(SerializedObject::Object {
                    class: ZStr::from("B"),
                    props,
                })
            }
        }

        let shared_ref = PhpRef::new(Value::Int(99));
        let obj = Obj::dummy("B");
        let mut t = HashTable::new();
        t.push(Value::Reference(shared_ref.clone())).unwrap();
        t.push(Value::Object(obj.clone())).unwrap();
        t.push(Value::Reference(shared_ref.clone())).unwrap();

        let mut ctx = MagicSerializeMockContext {
            inner_val: Value::Reference(shared_ref.clone()),
        };
        let out = serialize(&Value::Array(t.into()), &mut ctx).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("O:1:\"B\""));
        assert!(s.ends_with("i:2;R:2;}"));
    }

    #[test]
    fn object_recursion_guard_emits_null() {
        struct RecursiveMockContext;
        impl RuntimeContext for RecursiveMockContext {
            fn diagnostic(&mut self, _level: ErrorLevel, _message: &str) -> RtResult<()> {
                Ok(())
            }
            fn serialize_object(&mut self, obj: &Obj) -> RtResult<SerializedObject> {
                // Directly attempts to serialize the same object again (recursion)
                let _ = serialize(&Value::Object(obj.clone()), self);
                Ok(SerializedObject::Object {
                    class: ZStr::from("Rec"),
                    props: ZArray::new(),
                })
            }
        }

        let obj = Obj::dummy("Rec");
        let mut ctx = RecursiveMockContext;
        let out = serialize(&Value::Object(obj), &mut ctx).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "O:3:\"Rec\":0:{}");
    }

    #[test]
    fn duplicate_objects_consume_slot_duplicate_references_do_not() {
        let obj = Obj::dummy("Dummy");
        let o_val = Value::Object(obj);
        let mut t = HashTable::new();
        t.push(o_val.clone()).unwrap();
        t.push(o_val.clone()).unwrap();
        let mut c = CollectingContext::new();
        let out = serialize(&Value::Array(t.into()), &mut c).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "a:2:{i:0;O:5:\"Dummy\":0:{}i:1;r:2;}"
        );
    }

    #[test]
    fn errors_and_offsets() {
        assert_eq!(unser("").0.map(|_| ()), None);
        assert!(unser("").1.is_empty());
        let (v, m) = unser("i:5");
        assert!(v.is_none());
        assert_eq!(m, ["Warning: unserialize(): Error at offset 0 of 3 bytes"]);
        let (_, m) = unser("s:5:\"abc\";");
        assert_eq!(
            m,
            ["Warning: unserialize(): Error at offset 10 of 10 bytes"]
        );
        let (_, m) = unser("s:50:\"abc\";");
        assert_eq!(m, ["Warning: unserialize(): Error at offset 2 of 11 bytes"]);
        let (_, m) = unser("a:1:{i:0;i:1;");
        assert_eq!(
            m,
            ["Warning: unserialize(): Error at offset 13 of 13 bytes"]
        );
        let (_, m) = unser("b:2;");
        assert_eq!(m, ["Warning: unserialize(): Error at offset 0 of 4 bytes"]);
        // a:1:{i:0;R:1;} is an array containing a reference to itself.
        let (v, m) = unser("a:1:{i:0;R:1;}");
        assert!(m.is_empty());
        let v = v.unwrap();
        assert!(v
            .as_array()
            .unwrap()
            .get_int(0)
            .is_some_and(Value::is_reference));
        let (_, m) = unser("a:2:{i:0;i:1;}");
        assert_eq!(
            m,
            [
                "Warning: unserialize(): Unexpected end of serialized data",
                "Warning: unserialize(): Error at offset 13 of 14 bytes"
            ]
        );
        let (v, m) = unser("i:5;xyz");
        assert_eq!(v.and_then(|v| v.as_int()), Some(5));
        assert_eq!(
            m,
            ["Warning: unserialize(): Extra data starting at offset 4 of 7 bytes"]
        );
        let (v, _) = unser("a:1:{s:1:\"5\";b:1;}");
        assert!(v.unwrap().as_array().unwrap().get_int(5).is_some());
        // Unserialized arrays are hash tables, like PHP's.
        let (v, _) = unser("a:2:{i:0;i:1;i:1;i:2;}");
        let v = v.unwrap();
        assert!(!v.as_array().unwrap().is_packed());
        assert!(unser("a:0:{}").0.unwrap().as_array().unwrap().is_packed());
        let (v, _) = unser("d:1.5e3;");
        assert_eq!(v.and_then(|v| v.as_float()), Some(1500.0));
    }

    // -----------------------------------------------------------------
    // Oracle comparisons (expectations generated with php 8.3 / master:
    // `unserialize($in, $opts)` with an error handler printing
    // "Level: message", the result printed with `print_r`).
    // -----------------------------------------------------------------

    /// A context with an enum `Foo { case Bar; const Baz = Foo::Bar; }`
    /// and a plain class `NotEnum`; other names are not classes.
    struct EnumCtx {
        inner: CollectingContext,
        bar: Obj,
    }

    impl EnumCtx {
        fn new() -> Self {
            Self {
                inner: CollectingContext::new(),
                bar: crate::plain::PlainObject::create(
                    "Foo",
                    ZArray::from_pairs([("name", Value::from("Bar"))]),
                ),
            }
        }
    }

    impl RuntimeContext for EnumCtx {
        fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
            self.inner.diagnostic(level, message)
        }
        fn object_properties(
            &mut self,
            obj: &Obj,
            purpose: crate::error::PropertyPurpose,
        ) -> RtResult<Option<ZArray>> {
            self.inner.object_properties(obj, purpose)
        }
        fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
            self.inner.unserialize_object_create(class)
        }
        fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
            self.inner.unserialize_object_finish(obj, props)
        }
        fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
            self.inner.unserialize_custom(class, data)
        }
        fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
            self.inner.property_write(obj, name, value)
        }
        fn unserialize_enum_case(&mut self, class: &ZStr, case: &ZStr) -> RtResult<EnumCaseLookup> {
            Ok(match class.as_bytes().to_ascii_lowercase().as_slice() {
                b"foo" => match case.as_bytes() {
                    b"Bar" => EnumCaseLookup::Found(Value::Object(self.bar.clone())),
                    b"Baz" => EnumCaseLookup::NotACase,
                    _ => EnumCaseLookup::UndefinedCase,
                },
                b"notenum" | b"stdclass" => EnumCaseLookup::NotAnEnum,
                b"boom" => return Err(RuntimeError::Error("autoload failed".into())),
                _ => EnumCaseLookup::ClassNotFound,
            })
        }
    }

    /// `print_r`-like one-line rendering: `[k=>v,...]`, `Class{k=>v,...}`.
    fn render(v: &Value) -> String {
        fn table(t: &HashTable) -> String {
            t.iter()
                .map(|(k, v)| {
                    let k = match k {
                        ArrayKey::Int(i) => i.to_string(),
                        ArrayKey::Str(s) => s.to_string_lossy(),
                    };
                    format!("{k}=>{}", render(v))
                })
                .collect::<Vec<_>>()
                .join(",")
        }
        match v.deref() {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format!("{f}"),
            Value::Str(s) => s.to_string_lossy(),
            Value::Array(a) => format!("[{}]", table(&a)),
            Value::Object(o) => {
                let props = o
                    .downcast_ref::<crate::plain::PlainObject>()
                    .map(|p| table(&p.props()))
                    .unwrap_or_default();
                format!("{}{{{props}}}", o.class_name().to_string_lossy())
            }
            _ => "?".into(),
        }
    }

    fn run(input: &str, opts: &UnserializeOptions) -> (Vec<String>, Option<String>) {
        let mut c = EnumCtx::new();
        let v = unserialize(input.as_bytes(), opts, &mut c).unwrap();
        (c.inner.messages(), v.as_ref().map(render))
    }

    fn opts_allowed(a: AllowedClasses) -> UnserializeOptions {
        UnserializeOptions {
            allowed_classes: a,
            ..UnserializeOptions::default()
        }
    }

    fn opts_depth(d: usize) -> UnserializeOptions {
        UnserializeOptions {
            max_depth: d,
            ..UnserializeOptions::default()
        }
    }

    fn check(input: &str, opts: &UnserializeOptions, msgs: &[&str], result: Option<&str>) {
        let (m, r) = run(input, opts);
        assert_eq!(m, msgs, "messages for {input:?}");
        assert_eq!(r.as_deref(), result, "result for {input:?}");
    }

    fn fails_at(input: &str, opts: &UnserializeOptions, warnings: &[&str], offset: usize) {
        let mut msgs: Vec<String> = warnings.iter().map(|w| format!("Warning: {w}")).collect();
        msgs.push(format!(
            "Warning: unserialize(): Error at offset {offset} of {} bytes",
            input.len()
        ));
        let (m, r) = run(input, opts);
        assert_eq!(m, msgs, "messages for {input:?}");
        assert_eq!(r, None, "result for {input:?}");
    }

    #[test]
    fn enum_cases_and_failure_offsets() {
        let d = UnserializeOptions::default();
        check("E:7:\"Foo:Bar\";", &d, &[], Some("Foo{name=>Bar}"));
        fails_at(
            "E:7:\"Foo:Baz\";",
            &d,
            &["unserialize(): Foo::Baz is not an enum case"],
            14,
        );
        fails_at(
            "E:7:\"Foo:Qux\";",
            &d,
            &["unserialize(): Undefined constant Foo::Qux"],
            14,
        );
        fails_at(
            "E:9:\"NotEnum:A\";",
            &d,
            &["unserialize(): Class 'NotEnum' is not an enum"],
            0,
        );
        fails_at(
            "E:7:\"Bar:Baz\";",
            &d,
            &["unserialize(): Class 'Bar' not found"],
            0,
        );
        fails_at(
            "E:6:\"FooBar\";",
            &d,
            &["unserialize(): Invalid enum name 'FooBar' (missing colon)"],
            0,
        );
        fails_at("E:0:\"\";", &d, &[], 2);
        fails_at("E:8:\"Foo:Bar\";", &d, &[], 13);
        fails_at("E:7:\"Foo:Bar\"", &d, &[], 13);
        fails_at("E:50:\"Foo:Bar\";", &d, &[], 2);
        fails_at(
            "a:2:{i:0;E:7:\"Foo:Qux\";i:1;i:1;}",
            &d,
            &["unserialize(): Undefined constant Foo::Qux"],
            23,
        );
        // An enum as an array key.
        fails_at("a:1:{E:7:\"Foo:Bar\";i:1;}", &d, &[], 5);
        // An invalid class name fails silently at the start.
        fails_at("E:8:\"1Fo-:Bar\";", &d, &[], 0);
        fails_at(
            "E:4:\":Bar\";",
            &d,
            &["unserialize(): Class '' not found"],
            0,
        );
        // `r:` to an enum case is the same case.
        check(
            "a:2:{i:0;E:7:\"Foo:Bar\";i:1;r:2;}",
            &d,
            &[],
            Some("[0=>Foo{name=>Bar},1=>Foo{name=>Bar}]"),
        );
        // Enums are not subject to allowed_classes.
        check(
            "E:7:\"Foo:Bar\";",
            &opts_allowed(AllowedClasses::none()),
            &[],
            Some("Foo{name=>Bar}"),
        );
        // A hook error (an autoloader exception) propagates without the
        // offset warning.
        let mut c = EnumCtx::new();
        let r = unserialize(b"a:1:{i:0;E:6:\"Boom:A\";}", &d, &mut c);
        assert!(matches!(r, Err(RuntimeError::Error(m)) if m == "autoload failed"));
        assert!(c.inner.messages().is_empty());
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn allowed_classes() {
        check(
            "O:7:\"NotEnum\":1:{s:1:\"a\";i:1;}",
            &opts_allowed(AllowedClasses::none()),
            &[],
            Some("__PHP_Incomplete_Class{__PHP_Incomplete_Class_Name=>NotEnum,a=>1}"),
        );
        check(
            "O:7:\"NotEnum\":0:{}",
            &opts_allowed(AllowedClasses::only(["NOTENUM"])),
            &[],
            Some("NotEnum{}"),
        );
        check(
            "O:7:\"NotEnum\":0:{}",
            &opts_allowed(AllowedClasses::only(["other"])),
            &[],
            Some("__PHP_Incomplete_Class{__PHP_Incomplete_Class_Name=>NotEnum}"),
        );
        check(
            "C:7:\"NotEnum\":3:{abc}",
            &opts_allowed(AllowedClasses::none()),
            &["Warning: Class __PHP_Incomplete_Class has no unserializer"],
            Some("__PHP_Incomplete_Class{__PHP_Incomplete_Class_Name=>NotEnum}"),
        );
        fails_at(
            "O:3:\"a-b\":0:{}",
            &opts_allowed(AllowedClasses::none()),
            &[],
            0,
        );
        fails_at("O:3:\"a-b\":0:{}", &UnserializeOptions::default(), &[], 0);
        assert!(AllowedClasses::All.allows(b"X"));
        assert!(!AllowedClasses::none().allows(b"X"));
        assert!(AllowedClasses::only(["Foo\\Bar"]).allows(b"fOO\\bar"));
        // Back-references to incomplete objects keep their identity.
        let mut c = EnumCtx::new();
        let v = unserialize(
            b"a:2:{i:0;O:1:\"A\":0:{}i:1;r:2;}",
            &opts_allowed(AllowedClasses::none()),
            &mut c,
        )
        .unwrap()
        .unwrap();
        let a = v.as_array().unwrap();
        let (Some(Value::Object(x)), Some(Value::Object(y))) = (a.get_int(0), a.get_int(1)) else {
            panic!("expected objects");
        };
        assert_eq!(x.addr(), y.addr());
    }

    #[test]
    fn max_depth() {
        check(
            "a:1:{i:0;a:1:{i:0;a:0:{}}}",
            &opts_depth(2),
            &[],
            Some("[0=>[0=>[]]]"),
        );
        let depth2 = "unserialize(): Maximum depth of 2 exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting";
        fails_at(
            "a:1:{i:0;a:1:{i:0;a:1:{i:0;i:1;}}}",
            &opts_depth(2),
            &[depth2],
            23,
        );
        // Objects count even when empty.
        fails_at(
            "a:1:{i:0;O:8:\"stdClass\":0:{}}",
            &opts_depth(1),
            &[&depth2.replace("of 2", "of 1")],
            27,
        );
        check(
            "a:1:{i:0;a:1:{i:0;i:1;}}",
            &opts_depth(0),
            &[],
            Some("[0=>[0=>1]]"),
        );
        // A nested call continues the outer depth.
        let nested = UnserializeOptions {
            max_depth: 3,
            depth: 2,
            ..UnserializeOptions::default()
        };
        fails_at(
            "a:1:{i:0;a:1:{i:0;i:1;}}",
            &nested,
            &[&depth2.replace("of 2", "of 3")],
            14,
        );
        assert_eq!(UnserializeOptions::default().max_depth, 4096);
        assert_eq!(UnserializeOptions::max_depth_from_ini(-1), 0);
        assert_eq!(UnserializeOptions::max_depth_from_ini(17), 17);
        // The default limit stops runaway nesting (the parser recurses, so
        // this runs on a thread with a VM-sized stack).
        let deep = "a:1:{i:0;".repeat(5000) + "N;" + &"}".repeat(5000);
        let (m, r) = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || run(&deep, &UnserializeOptions::default()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(r, None);
        assert!(m[0].contains("Maximum depth of 4096 exceeded"), "{m:?}");
    }

    #[test]
    fn syntax_failure_offsets() {
        let d = UnserializeOptions::default();
        let eod = "unserialize(): Unexpected end of serialized data";
        fails_at("}", &d, &[eod], 0);
        fails_at("a:2:{i:0;i:1;}", &d, &[eod], 13);
        fails_at("a:1:{a:0:{}i:0;}", &d, &[], 10);
        fails_at("a:1:{R:1;i:0;}", &d, &[], 9);
        fails_at("O:8:\"stdClass\":", &d, &["Bad unserialize data"], 13);
        fails_at("O:8:\"stdClass\":1", &d, &[], 16);
        fails_at("a:3:{i:0;i:1;i:0;R:2;i:5;i:5;}", &d, &[], 21);
        fails_at("a:2:{i:0;O:8:\"stdClass\":0:{}i:0;r:2;}", &d, &[], 36);
        fails_at(
            "a:99999999999999999999:{}",
            &d,
            &["unserialize(): Numerical result out of range"],
            24,
        );
        fails_at("a:5:{", &d, &[], 5);
        fails_at("O:8:\"stdClass\":5:{}", &d, &[], 16);
        fails_at("O:8:\"stdClass\":-1:{}", &d, &[], 17);
        fails_at("a:2:{i:0;b:1i:1;i:2;}", &d, &[], 9);
        fails_at("a:2:{i:0;a:0:{}xi:1;i:2;}", &d, &[], 15);
        fails_at(
            "C:8:\"stdClass\":-1:{}",
            &d,
            &["Insufficient data for unserializing - -1 required, 1 present"],
            19,
        );
        fails_at(
            "C:8:\"stdClass\":9:{}",
            &d,
            &["Insufficient data for unserializing - 9 required, 1 present"],
            18,
        );
        fails_at("C:8:\"stdClass\":1:{xx", &d, &[], 19);
        fails_at("O:9:\"stdClass\":0:{}", &d, &[], 14);
        fails_at("O:0:\"\":0:{}", &d, &[], 2);
        fails_at("O:9:\"\\stdClass\":0:{}", &d, &[], 0);
        fails_at("a:1:{i:0;r:1;}", &d, &[], 13);
        fails_at("a:1:{i:0;r:3;}", &d, &[], 13);
        fails_at("r:1;", &d, &[], 4);
        fails_at("a:1:{N;i:0;}", &d, &[], 7);
        fails_at("d:e5;", &d, &[], 0);
        fails_at("b:2;", &d, &[], 0);
        fails_at("x", &d, &[], 0);
    }

    #[test]
    fn scalars_and_duplicates_match_oracle() {
        let d = UnserializeOptions::default();
        // The overwritten value's slot now designates the new value.
        check(
            "a:3:{i:0;s:1:\"a\";i:0;i:5;i:1;R:2;}",
            &d,
            &[],
            Some("[0=>5,1=>5]"),
        );
        check(
            "i:99999999999999999999;",
            &d,
            &["Warning: unserialize(): Numerical result out of range"],
            Some("9223372036854775807"),
        );
        check(
            "i:-9223372036854775808;",
            &d,
            &[],
            Some("-9223372036854775808"),
        );
        check("i:000000000000000000000000001;", &d, &[], Some("1"));
        check(
            "S:3:\"\\61bc\";",
            &d,
            &["Deprecated: unserialize(): Unserializing the 'S' format is deprecated"],
            Some("abc"),
        );
        check("d:.5;", &d, &[], Some("0.5"));
        check("d:5.;", &d, &[], Some("5"));
        check("d:-INF;", &d, &[], Some("-inf"));
        check(
            "a:1:{s:1:\"0\";O:8:\"stdClass\":1:{i:5;N;}}",
            &d,
            &[],
            Some("[0=>stdClass{5=>null}]"),
        );
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn partial_and_prefix() {
        let d = UnserializeOptions::default();
        let mut c = EnumCtx::new();
        let buf = b"xxi:5;s:1:\"a\";";
        let (v, end) = unserialize_partial(buf, 2, &d, &mut c).unwrap().unwrap();
        assert_eq!(v.as_int(), Some(5));
        assert_eq!(end, 6);
        let (v, end) = unserialize_partial(buf, end, &d, &mut c).unwrap().unwrap();
        assert_eq!(render(&v), "a");
        assert_eq!(end, buf.len());
        // Failure offsets are relative to the buffer; no offset warning.
        assert_eq!(
            unserialize_partial(b"x:a:1:{i:0;", 2, &d, &mut c)
                .unwrap()
                .err(),
            Some(11)
        );
        assert!(c.inner.messages().is_empty());
        // Diagnostics name the calling function.
        let o = UnserializeOptions {
            function: Cow::Borrowed("session_decode"),
            ..UnserializeOptions::default()
        };
        let (m, _) = run("a:1:{}}", &o);
        assert_eq!(
            m,
            [
                "Warning: session_decode(): Unexpected end of serialized data",
                "Warning: session_decode(): Error at offset 5 of 7 bytes"
            ]
        );
        let o = UnserializeOptions {
            function: Cow::Borrowed(""),
            ..UnserializeOptions::default()
        };
        assert_eq!(run("x", &o).0, ["Warning: Error at offset 0 of 1 bytes"]);
    }

    #[test]
    fn incomplete_classes_only() {
        let o = UnserializeOptions {
            incomplete_classes_only: true,
            ..UnserializeOptions::default()
        };
        check(
            "a:3:{i:0;O:7:\"NotEnum\":1:{s:1:\"a\";i:1;}i:1;C:1:\"X\":3:{abc}i:2;E:7:\"Foo:Bar\";}",
            &o,
            &[],
            Some(
                "[0=>__PHP_Incomplete_Class{__PHP_Incomplete_Class_Name=>NotEnum,a=>1},\
                 1=>__PHP_Incomplete_Class{__PHP_Incomplete_Class_Serialized=>abc,__PHP_Incomplete_Class_Name=>X},\
                 2=>__PHP_Incomplete_Class{__PHP_Incomplete_Class_Name=>Foo,__PHP_Incomplete_Enum_Case=>Bar}]",
            ),
        );
        // Data carrying a reserved member is rejected (after the key).
        fails_at(
            "O:1:\"A\":1:{s:27:\"__PHP_Incomplete_Class_Name\";s:1:\"B\";}",
            &o,
            &[],
            46,
        );
        fails_at("O:3:\"a-b\":0:{}", &o, &[], 0);
    }
}
