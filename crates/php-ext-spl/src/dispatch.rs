// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Generic registration of the SPL classes from
//! [`php_spl::classes::class_table`] and the single native method handler
//! that routes every call to [`php_spl::call_method`].

use crate::host::{rt_throw, spl_state_of, VmHost};
use php_runtime::{Obj, Value, ZStr};
use php_spl::classes::{ClassDef, ClassKind, MethodDef, MethodFlags};
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, VmResult};
use php_vm::func::Func;
use php_vm::opcode::{Op, Operand};
use php_vm::vm::FrameKind;
use php_vm::{Throw, Vm};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::BitOr;
use std::rc::Rc;
use std::sync::OnceLock;

/// What a registered native method stands for.
#[derive(Debug, Clone, Copy)]
struct Target {
    /// The declaring SPL class.
    class: &'static str,
    /// The method declaration.
    def: &'static MethodDef,
    /// Whether the method is `#[\Deprecated]`.
    deprecated: bool,
}

/// A map keyed by `Builtin` addresses (no protection against chosen keys
/// is needed: the keys are the VM's own pointers).
type PtrMap<V> = HashMap<usize, V, rustc_hash::FxBuildHasher>;

thread_local! {
    /// Native method handlers, keyed by the address of their `Builtin`.
    static METHODS: RefCell<PtrMap<Target>> = RefCell::new(PtrMap::default());
}

/// The SPL class table, indexed once per process: the stub signature of
/// every method (the class builder needs `&'static str` signatures), by
/// class position in [`php_spl::classes::class_table`] and method position
/// in [`ClassDef::methods`], and each class's position by name.
struct Tables {
    sigs: Vec<Vec<String>>,
    by_name: HashMap<&'static str, usize>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let table = php_spl::classes::class_table();
        Tables {
            sigs: table
                .iter()
                .map(|c| c.methods.iter().map(signature).collect())
                .collect(),
            by_name: table.iter().enumerate().map(|(i, c)| (c.name, i)).collect(),
        }
    })
}

/// The SPL class declaration named `name` (canonical spelling first, then
/// any case).
fn spl_class(name: &str) -> Option<&'static ClassDef> {
    match tables().by_name.get(name) {
        Some(&i) => php_spl::classes::class_table().get(i),
        None => php_spl::classes::find_class(name),
    }
}

/// Whether `c` is the SPL class `ancestor` or extends it, as
/// [`php_spl::classes::is_subclass_of`] answers for a class `ancestor`:
/// interfaces never lead to a class, and an `extends` chain that leaves
/// the SPL table (into the exception hierarchy) never comes back to it.
fn extends_spl(c: &'static ClassDef, ancestor: &str) -> bool {
    std::iter::successors(Some(c), |d| d.parent.and_then(spl_class))
        .any(|d| d.name.eq_ignore_ascii_case(ancestor))
}

/// Renders a value as a literal the VM's stub parser understands.
fn literal(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let s = format!("{f:?}");
            if s.contains('.') || s.contains('e') {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Str(s) => {
            let t = s.to_string_lossy();
            if t.contains('\'') || t.contains('\n') || t.contains('\t') {
                return None;
            }
            format!("'{}'", t.replace('\\', "\\\\"))
        }
        Value::Array(a) if a.is_empty() => "[]".to_string(),
        _ => return None,
    })
}

/// Evaluates a stub default (`null`, `0`, `"r"`, `self::X | Y::Z`,
/// `2 * 1024 * 1024`, `PHP_INT_MAX`, ...).
#[must_use]
pub fn eval_default(src: &str, class: &str) -> Option<Value> {
    let s = src.trim();
    for (sep, op) in [
        ('|', i64::bitor as fn(i64, i64) -> i64),
        ('*', i64::saturating_mul),
    ] {
        if let Some((lhs, rhs)) = s.split_once(sep) {
            return match (eval_default(lhs, class)?, eval_default(rhs, class)?) {
                (Value::Int(lhs), Value::Int(rhs)) => Some(Value::Int(op(lhs, rhs))),
                _ => None,
            };
        }
    }
    let is = |name: &str| s.eq_ignore_ascii_case(name);
    if is("null") {
        return Some(Value::Null);
    }
    if is("true") || is("false") {
        return Some(Value::Bool(is("true")));
    }
    if s == "[]" {
        return Some(Value::empty_array());
    }
    if is("php_int_max") {
        return Some(Value::Int(i64::MAX));
    }
    if is("sort_regular") || is("count_normal") {
        return Some(Value::Int(0));
    }
    if is("preg_split_no_empty") {
        return Some(Value::Int(1));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::Int(i));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Some(Value::Float(f));
    }
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        let inner = &s[1..s.len() - 1];
        let mut out = Vec::new();
        let mut it = inner.bytes().peekable();
        while let Some(c) = it.next() {
            if c == b'\\' {
                match it.next() {
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'0') => out.push(0),
                    Some(o) => out.push(o),
                    None => out.push(b'\\'),
                }
            } else {
                out.push(c);
            }
        }
        return Some(Value::Str(ZStr::from(out)));
    }
    if let Some((cls, name)) = s.split_once("::") {
        let cls = if cls.eq_ignore_ascii_case("self") || cls.eq_ignore_ascii_case("static") {
            class
        } else {
            cls
        };
        return php_spl::classes::constant(cls, name).map(Value::Int);
    }
    None
}

/// Whether a declared parameter type is left to `php-spl`'s own checks.
fn loose_type(method: &str, ty: &str) -> bool {
    method.eq_ignore_ascii_case("__construct")
        || ty.contains("callable")
        || matches!(ty, "array|object" | "object|array")
}

/// The VM stub signature of a method.
fn signature(d: &MethodDef) -> String {
    let mut s = String::from(d.name);
    s.push('(');
    for (i, p) in d.params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        // Some parameters are checked by `php-spl` itself, which produces
        // the exact `zend_parse_parameters` messages: callables ("must be
        // a valid callback, ..."), `array|object` ("must be of type
        // array, ...") and constructor arguments. Their types are arginfo
        // only (`@unchecked`): Reflection and inheritance checks see them,
        // the VM does not enforce them.
        if let Some(t) = p.ty {
            if loose_type(d.name, t) {
                s.push_str("@unchecked ");
            }
            s.push_str(t);
            s.push(' ');
        }
        if p.by_ref {
            s.push('&');
        }
        if p.variadic {
            s.push_str("...");
        }
        s.push('$');
        s.push_str(p.name);
        if let Some(def) = p.default {
            s.push_str(" = ");
            match eval_default(def, "").as_ref().and_then(literal) {
                Some(l) => s.push_str(&l),
                None => s.push_str("UNKNOWN"),
            }
        }
    }
    s.push(')');
    if let Some(r) = d.return_type {
        s.push_str(": ");
        s.push_str(r);
    }
    s
}

fn method_flags(d: &MethodDef) -> u32 {
    let mut f = 0;
    if d.flags & MethodFlags::STATIC != 0 {
        f |= acc::STATIC;
    }
    if d.flags & MethodFlags::FINAL != 0 {
        f |= acc::FINAL;
    }
    if d.flags & MethodFlags::PROTECTED != 0 {
        f |= acc::PROTECTED;
    } else if d.flags & MethodFlags::PRIVATE != 0 {
        f |= acc::PRIVATE;
    } else {
        f |= acc::PUBLIC;
    }
    f
}

/// Classes whose handlers have no `clone_obj` (dual iterators, recursive
/// iterators, `SplFileObject`, `GlobIterator`).
fn uncloneable(c: &'static ClassDef) -> bool {
    matches!(
        c.name,
        "GlobIterator" | "SplFileObject" | "SplTempFileObject" | "RecursiveIteratorIterator"
    ) || extends_spl(c, "IteratorIterator")
        || extends_spl(c, "RecursiveIteratorIterator")
}

/// Classes whose objects own a directory or file stream, closed by their
/// `dtor_obj` (`spl_filesystem_object_destroy_object`).
fn owns_stream(c: &'static ClassDef) -> bool {
    extends_spl(c, "DirectoryIterator") || extends_spl(c, "SplFileObject")
}

/// Registers one SPL class or interface; `idx` is its position in
/// [`php_spl::classes::class_table`].
fn register_class(vm: &mut Vm, idx: usize, c: &'static ClassDef) {
    let sigs = tables().sigs.get(idx).map_or(&[][..], Vec::as_slice);
    let mut b = ClassBuilder::new(c.name);
    match c.kind {
        ClassKind::Interface => b = b.flags(class_flags::INTERFACE),
        ClassKind::Abstract => b = b.flags(class_flags::ABSTRACT),
        ClassKind::Class => {}
    }
    if c.is_final {
        b = b.flags(class_flags::FINAL);
    }
    if c.name == "SplFileInfo" {
        b = b.flags(class_flags::NOT_SERIALIZABLE);
    }
    if uncloneable(c) {
        b = b.flags(class_flags::NO_CLONE);
    }
    if let Some(p) = c.parent {
        b = b.extends(vm, p);
    }
    b = b.implements(vm, c.interfaces);
    for (n, v) in c.constants {
        b = b.constant(n, Value::Int(*v));
    }
    for p in c.properties {
        let def = p.default.and_then(|d| eval_default(d, c.name));
        b = b.prop(p.name, acc::PUBLIC, p.ty, def);
    }
    for (d, sig) in c.methods.iter().zip(sigs) {
        let fl = method_flags(d);
        if c.kind == ClassKind::Interface || d.flags & MethodFlags::ABSTRACT != 0 {
            b = b.abstract_method(sig, fl);
        } else {
            b = b.method(sig, fl, method_handler);
        }
    }
    if c.kind != ClassKind::Interface {
        let mut hooks = if owns_stream(c) {
            crate::hooks::filesystem_class_hooks()
        } else {
            crate::hooks::class_hooks()
        };
        if matches!(c.name, "IteratorIterator" | "RecursiveIteratorIterator") {
            hooks.forward_methods_to = Some(forward_methods_to);
        }
        b = b.hooks(hooks);
    }
    if matches!(c.name, "IteratorIterator" | "RecursiveIteratorIterator") {
        // `spl_dual_it_get_method` / `spl_recursive_it_get_method` forward
        // calls of unknown methods to the inner iterator; the VM has no
        // `get_method` hook, so this is modelled with `__call`.
        b = b.method("__call(...$arguments)", acc::PUBLIC, forward_call);
    }
    let cls = b.register(vm);
    METHODS.with(|m| {
        let mut m = m.borrow_mut();
        m.reserve(c.methods.len());
        for d in c.methods {
            let lc = d.name.to_ascii_lowercase();
            if let Some(meth) = cls.methods.get(lc.as_bytes()) {
                if let Func::Builtin(bi) = &meth.func {
                    m.insert(
                        Rc::as_ptr(bi) as usize,
                        Target {
                            class: c.name,
                            def: d,
                            deprecated: d.deprecated.is_some(),
                        },
                    );
                }
            }
        }
    });
}

/// Registers every SPL class the VM does not provide yet, parents and
/// interfaces first. Returns the names of the classes that were already
/// registered (by the VM or another extension).
pub fn register_classes(vm: &mut Vm) -> Vec<&'static str> {
    crate::hooks::register_gc();
    let mut skipped = Vec::new();
    let mut pending: Vec<(usize, &'static ClassDef)> = Vec::new();
    for (idx, c) in php_spl::classes::class_table().iter().enumerate() {
        if vm.find_class(c.name.as_bytes()).is_some() {
            skipped.push(c.name);
        } else {
            pending.push((idx, c));
        }
    }
    while !pending.is_empty() {
        let before = pending.len();
        let mut rest = Vec::new();
        for (idx, c) in pending {
            let ready = c
                .parent
                .iter()
                .chain(c.interfaces.iter())
                .all(|n| vm.find_class(n.as_bytes()).is_some());
            if ready {
                register_class(vm, idx, c);
            } else {
                rest.push((idx, c));
            }
        }
        pending = rest;
        if pending.len() == before {
            // Unresolvable dependencies: register anyway.
            for (idx, c) in pending.drain(..) {
                register_class(vm, idx, c);
            }
        }
    }
    skipped
}

/// The arguments as `php-spl` expects them: only the passed ones, with
/// parameters skipped by named arguments filled with their defaults.
fn collect_args(args: &Args, target: Target) -> Vec<Value> {
    let variadic = target.def.params.last().is_some_and(|p| p.variadic);
    let count = if variadic {
        args.values.len()
    } else {
        args.passed.iter().rposition(|&p| p).map_or(0, |i| i + 1)
    };
    (0..count)
        .map(|i| {
            let value = args.values.get(i).cloned().unwrap_or_default();
            if args.passed.get(i).copied().unwrap_or(true) {
                return value;
            }
            target
                .def
                .params
                .get(i)
                .and_then(|p| p.default)
                .and_then(|d| eval_default(d, target.class))
                .unwrap_or(value)
        })
        .collect()
}

/// `spl_array_method`: the `ArrayObject`/`ArrayIterator` sort methods
/// call the array function of the same name, which may be disabled.
fn disabled_sort(vm: &mut Vm, t: Target) -> Option<Throw> {
    const SORTS: &[&str] = &[
        "asort",
        "ksort",
        "uasort",
        "uksort",
        "natsort",
        "natcasesort",
    ];
    if !matches!(t.class, "ArrayObject" | "ArrayIterator") {
        return None;
    }
    let name = SORTS.iter().find(|s| s.eq_ignore_ascii_case(t.def.name))?;
    let list = vm.ini_string("disable_functions")?;
    let disabled = list
        .split(|&b| b == b',' || b == b' ')
        .any(|f| f.eq_ignore_ascii_case(name.as_bytes()));
    disabled.then(|| {
        vm.throw(
            "Error",
            &format!("Cannot call method {name} when function {name} is disabled"),
        )
    })
}

/// `new IteratorIterator($aggregate, 'Class')`: `spl_dual_it_construct`
/// calls the *named* class's `getIterator()` (`zend_call_method` with an
/// explicit scope), which `php-spl` cannot express through
/// `SplHost::call_method` (normal dispatch would re-enter a subclass
/// override that wraps `$this` again). Resolves the aggregate here and
/// hands `php-spl` the resulting iterator.
fn downcast_aggregate(vm: &mut Vm, args: &mut Vec<Value>) -> VmResult<()> {
    let (Some(Value::Object(o)), Some(Value::Str(cn))) = (
        args.first().map(Value::deref),
        args.get(1).map(Value::deref),
    ) else {
        return Ok(());
    };
    let ocls = php_vm::object::class_of(&o).clone();
    if ocls.instance_of_name(b"iterator") || !ocls.instance_of_name(b"iteratoraggregate") {
        return Ok(());
    }
    let Some(named) = vm.find_class(cn.as_bytes()) else {
        return Ok(());
    };
    if !named.instance_of_name(b"traversable") || !ocls.instance_of_name(named.lc_name.as_bytes()) {
        // php-spl reports the downcast error.
        return Ok(());
    }
    let Some(m) = named.find_method(b"getiterator") else {
        return Ok(());
    };
    let decl = m.scope();
    if Rc::ptr_eq(
        &decl,
        &ocls
            .find_method(b"getiterator")
            .map_or_else(|| decl.clone(), |x| x.scope()),
    ) {
        // No override between the object's class and the named class.
        return Ok(());
    }
    let callable = Value::Array(php_runtime::ZArray::from_values(vec![
        Value::Object(o),
        Value::Str(ZStr::from(format!(
            "{}::getIterator",
            decl.name.to_string_lossy()
        ))),
    ]));
    let it = vm.call_value(&callable, Vec::new())?;
    match it.deref() {
        Value::Object(i) if php_vm::object::class_of(&i).instance_of_name(b"traversable") => {
            args.truncate(1);
            args[0] = Value::Object(i);
            Ok(())
        }
        _ => Err(vm.throw(
            "LogicException",
            &format!(
                "{}::getIterator() must return an object that implements Traversable",
                named.name.to_string_lossy()
            ),
        )),
    }
}

/// Stack budget when `zend.max_allowed_stack_size` is 0 (auto): what is
/// left of the default 8 MiB main-thread stack after the VM's own use.
const DEFAULT_STACK_BUDGET: usize = 6 * 1024 * 1024;

thread_local! {
    /// Approximate stack address of the outermost SPL method call and the
    /// nesting depth (`zend_call_stack_overflowed` emulation: SPL methods
    /// recurse natively, e.g. an `AppendIterator` appended to itself).
    static STACK: RefCell<(usize, usize)> = const { RefCell::new((0, 0)) };
}

/// Leaves one SPL call level on drop.
struct StackLevel;

impl Drop for StackLevel {
    fn drop(&mut self) {
        STACK.with(|s| {
            let mut s = s.borrow_mut();
            s.1 = s.1.saturating_sub(1);
        });
    }
}

/// Enters one SPL call level; `Err` with PHP's "Maximum call stack size"
/// error when the native stack used since the outermost SPL call exceeds
/// `zend.max_allowed_stack_size - zend.reserved_stack_size`.
fn enter_stack(vm: &mut Vm) -> VmResult<StackLevel> {
    let marker = 0u8;
    let here = std::ptr::from_ref(&marker) as usize;
    let used = STACK.with(|s| {
        let mut s = s.borrow_mut();
        if s.1 == 0 {
            s.0 = here;
        }
        s.1 += 1;
        s.0.saturating_sub(here)
    });
    let level = StackLevel;
    let max = vm.ini_long("zend.max_allowed_stack_size");
    let reserved = usize::try_from(vm.ini_long("zend.reserved_stack_size")).unwrap_or(0);
    let budget = usize::try_from(max)
        .ok()
        .filter(|&m| m > 0)
        .unwrap_or(DEFAULT_STACK_BUDGET)
        .saturating_sub(reserved);
    if used > budget {
        return Err(vm.throw(
            "Error",
            &format!(
                "Maximum call stack size of {budget} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
            ),
        ));
    }
    Ok(level)
}

/// The native handler of every SPL method.
fn method_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(f) = a.func.clone() else {
        return Err(vm.throw("Error", "SPL method called without a function"));
    };
    let _level = enter_stack(vm)?;
    let key = Rc::as_ptr(&f) as usize;
    let Some(t) = METHODS.with(|m| m.borrow().get(&key).copied()) else {
        return Err(vm.throw(
            "Error",
            &format!("Unknown SPL method {}()", f.name.to_string_lossy()),
        ));
    };
    if t.deprecated {
        if let Some(msg) = php_spl::classes::deprecation_message(t.class, t.def.name) {
            vm.raise(8192, &msg)?;
        }
    }
    if let Some(e) = disabled_sort(vm, t) {
        return Err(e);
    }
    if t.class == "SplFixedArray"
        && t.def.name == "offsetGet"
        && a.at(0).is_null()
        && !is_direct_call(vm, "offsetGet")
    {
        // `$b = &$fa[]` / `f($fa[])`: the VM calls the `read_dimension`
        // hook only for intermediate appends and falls back to
        // `offsetGet(null)` for a final one.
        return Err(vm.throw("Error", "[] operator not supported for SplFixedArray"));
    }
    let mut args = collect_args(a, t);
    let this = a.this.clone();
    if t.class == "IteratorIterator" && t.def.name == "__construct" {
        downcast_aggregate(vm, &mut args)?;
    }
    let callback_resolved = resolve_filter_callback(vm, t, &mut args)?;
    if t.class == "SplFixedArray" && t.def.name == "getIterator" && args.is_empty() {
        if let Some((obj, st)) = this
            .as_ref()
            .and_then(|o| Some((o.clone(), spl_state_of(o)?)))
        {
            // `zend_create_internal_iterator_zval` over the class's own
            // iterator: `php-spl` resolves it through `get_iterator`, which
            // re-enters a user `getIterator()` override (infinite recursion).
            // `spl_fixedarray_get_iterator` takes its own object handle
            // (`zend_iterator_init`) before the `InternalIterator` object.
            let it = php_spl::iter::with_handle(
                Box::new(php_spl::fixedarray::FixedArrayIter::new(obj, st)),
                Box::new(php_vm::object::HandleGuard::reserve()),
            );
            return crate::iter::new_internal_iterator(vm, it);
        }
    }
    // The "object not initialized" checks of `$obj->m()` run in the
    // `get_method` hook (see `hooks::get_method`).
    let _entry = crate::files::enter();
    let file_obj = this
        .as_ref()
        .and_then(spl_state_of)
        .is_some_and(|st| crate::files::is_file_state(&st));
    if file_obj {
        // Streams of SplFileObject & co. may be user wrappers: run with
        // the VM lent to the stream layer.
        return crate::files::run_bridged(vm, |host| {
            php_spl::call_method(host, t.class, t.def.name, this.as_ref(), &args)
        });
    }
    if t.def.name == "unserialize" {
        // `Serializable::unserialize()`: all parts of the payload share one
        // var_hash (`PHP_VAR_UNSERIALIZE_INIT`), joined with the active
        // `unserialize()` call's when there is one.
        return vm.with_unserialize_context(|vm| {
            let r = {
                let mut host = VmHost::new(vm);
                php_spl::call_method(&mut host, t.class, t.def.name, this.as_ref(), &args)
            };
            crate::iter::finish(vm, r)
        });
    }
    let r = {
        let mut host = if callback_resolved {
            VmHost::callable_checked(vm)
        } else {
            VmHost::new(vm)
        };
        php_spl::call_method(&mut host, t.class, t.def.name, this.as_ref(), &args)
    };
    crate::iter::finish(vm, r)
}

/// `spl_dual_it_construct()` for `CallbackFilterIterator` and
/// `RecursiveCallbackFilterIterator` (`"OF"`): once the inner iterator is
/// accepted, `zend_fcall_info_init()` resolves the callback in the
/// constructing scope, raising its deprecations, and `accept()` calls that
/// resolution (`zend_call_known_fcc`) wherever the iteration runs, as do
/// the children `getChildren()` constructs with it. A callback whose
/// resolution depends on the constructing scope (`'parent::m'`, a private
/// method) reaches `php-spl` as the fake closure
/// [`Vm::resolve_kept_callable`] makes of it. Returns whether the callback
/// was resolved here, so that `php-spl`'s own check only validates.
fn resolve_filter_callback(vm: &mut Vm, t: Target, args: &mut [Value]) -> VmResult<bool> {
    let inner: &[u8] = match (t.class, t.def.name) {
        ("CallbackFilterIterator", "__construct") => b"iterator",
        ("RecursiveCallbackFilterIterator", "__construct") => b"recursiveiterator",
        _ => return Ok(false),
    };
    let inner_ok = matches!(
        args.first().map(Value::deref),
        Some(Value::Object(o)) if php_vm::object::class_of(&o).instance_of_name(inner)
    );
    if !inner_ok || args.len() != 2 {
        // `php-spl` reports the argument error first.
        return Ok(false);
    }
    if let Ok(kept) = vm.resolve_kept_callable(&args[1])? {
        args[1] = kept;
    }
    Ok(true)
}

/// The VM stub signature of an SPL function.
#[must_use]
pub fn function_signature(d: &MethodDef) -> String {
    signature(d)
}

/// The method name of the user-code call being executed, recovered from
/// the calling frame's pending `InitMethodCall` (matched by call
/// nesting); `None` for calls made by the engine.
fn pending_method_name(vm: &Vm) -> Option<ZStr> {
    let f = vm
        .frames
        .iter()
        .rev()
        .find(|f| f.kind != FrameKind::Builtin)?;
    let ops = &f.code.ops;
    // The interpreter advances `ip` before executing an instruction.
    let mut i = f.ip.checked_sub(1)?;
    if !matches!(ops.get(i)?, Op::DoCall { .. }) {
        return None;
    }
    let mut depth = 0usize;
    loop {
        match &ops[i] {
            Op::DoCall { .. } | Op::ArrayKeyExists { .. } | Op::CallableConvert { .. } => {
                depth += 1
            }
            Op::InitMethodCall { name, .. } => {
                depth -= 1;
                if depth == 0 {
                    return match name {
                        Operand::Lit(n) => match f.code.literals.get(*n as usize) {
                            Some(Value::Str(s)) => Some(s.clone()),
                            _ => None,
                        },
                        _ => None,
                    };
                }
            }
            Op::InitFCall { .. }
            | Op::InitDynCall { .. }
            | Op::InitStaticCall { .. }
            | Op::InitParentHook { .. }
            | Op::New { .. } => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
            }
            _ => {}
        }
        i = i.checked_sub(1)?;
    }
}

/// Whether `method` is being called directly by user code (`$o->m()`), as
/// opposed to by the engine (`ArrayAccess` dispatch, `count()`,
/// `foreach`) or through a constructor call.
fn is_direct_call(vm: &Vm, method: &str) -> bool {
    pending_method_name(vm).is_some_and(|n| n.eq_ignore_ascii_case(method.as_bytes()))
}

/// Forwards an unknown method call to the inner (current sub-) iterator.
/// A native `__call` receives the call's own arguments (unpacked); the
/// method name comes from [`Vm::magic_call_name`].
/// `ClassHooks::forward_methods_to` of the dual and recursive iterators:
/// the iterator whose methods `spl_dual_it_get_method` /
/// `spl_recursive_it_get_method` resolve (so `method_exists()` sees them).
fn forward_methods_to(vm: &mut Vm, this: &Obj) -> VmResult<Option<Obj>> {
    let target = {
        let _entry = crate::files::enter();
        let mut host = VmHost::new(vm);
        php_spl::handlers::method_target(&mut host, this)
    };
    target.map_err(|e| rt_throw(vm, e))
}

fn forward_call(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(this) = a.this.clone() else {
        return Ok(Value::Null);
    };
    let name = vm.magic_call_name();
    let call_args: Vec<Value> = a.values.iter().map(Value::deref).collect();
    let target = {
        let _entry = crate::files::enter();
        let mut host = VmHost::new(vm);
        php_spl::handlers::method_target(&mut host, &this)
    };
    let target = target.map_err(|e| rt_throw(vm, e))?;
    if let (Some(name), Some(inner)) = (&name, target) {
        let lc = name.to_ascii_lowercase();
        let icls = php_vm::object::class_of(&inner);
        if icls.find_method(lc.as_bytes()).is_some() || icls.magic.call.is_some() {
            return vm.call_method_by_name(&inner, &name.to_string_lossy(), call_args);
        }
    }
    let shown = name.map_or_else(|| "__call".to_string(), |n| n.to_string_lossy());
    let t = vm.throw(
        "Error",
        &format!(
            "Call to undefined method {}::{shown}()",
            this.class_name().to_string_lossy()
        ),
    );
    if let Throw::Exception(e) = &t {
        drop_top_trace_frame(e);
    }
    Err(t)
}

/// Removes the innermost frame (our `__call`) from an exception's trace:
/// PHP raises "Call to undefined method" while resolving the call, before
/// any frame is pushed.
fn drop_top_trace_frame(e: &Obj) {
    let cls = php_vm::object::class_of(e).clone();
    let Some(pi) = cls.props.iter().find(|p| p.name.as_bytes() == b"trace") else {
        return;
    };
    let d = php_vm::object::data(e);
    let mut slots = d.slots.borrow_mut();
    if let Some(Some(Value::Array(frames))) = slots.get(pi.slot as usize) {
        let rest: Vec<Value> = frames.values().skip(1).cloned().collect();
        slots[pi.slot as usize] = Some(Value::Array(php_runtime::ZArray::from_values(rest)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(src: &str, class: &str) -> Option<String> {
        eval_default(src, class).as_ref().and_then(literal)
    }

    #[test]
    fn defaults_evaluate() {
        assert_eq!(lit("null", "").as_deref(), Some("null"));
        assert_eq!(lit("2 * 1024 * 1024", "").as_deref(), Some("2097152"));
        assert_eq!(
            lit("self::LEAVES_ONLY", "RecursiveIteratorIterator").as_deref(),
            Some("0")
        );
        assert_eq!(lit("\"\\\"\"", "").as_deref(), Some("'\"'"));
        assert_eq!(lit("\"\\\\\"", "").as_deref(), Some("'\\\\'"));
        assert_eq!(
            lit("PHP_INT_MAX", "").as_deref(),
            Some("9223372036854775807")
        );
    }

    #[test]
    fn spl_ancestry_matches_the_class_table() {
        for c in php_spl::classes::class_table() {
            for a in [
                "IteratorIterator",
                "RecursiveIteratorIterator",
                "DirectoryIterator",
                "SplFileObject",
            ] {
                assert_eq!(
                    extends_spl(c, a),
                    php_spl::classes::is_subclass_of(c.name, a),
                    "{} / {a}",
                    c.name
                );
            }
        }
        let glob = spl_class("GlobIterator").expect("GlobIterator");
        assert!(uncloneable(glob) && owns_stream(glob));
        let temp = spl_class("splTEMPfileobject").expect("any case");
        assert!(uncloneable(temp) && owns_stream(temp));
        assert!(!uncloneable(
            spl_class("ArrayIterator").expect("ArrayIterator")
        ));
    }

    #[test]
    fn signature_table_follows_the_class_table() {
        let t = tables();
        for (i, c) in php_spl::classes::class_table().iter().enumerate() {
            assert_eq!(t.by_name.get(c.name), Some(&i));
            assert_eq!(t.sigs[i].len(), c.methods.len());
            for (d, sig) in c.methods.iter().zip(&t.sigs[i]) {
                assert_eq!(*sig, signature(d));
            }
        }
    }

    #[test]
    fn named_defaults_ignore_case() {
        assert_eq!(lit("NULL", "").as_deref(), Some("null"));
        assert_eq!(lit("True", "").as_deref(), Some("true"));
        assert_eq!(lit("false", "").as_deref(), Some("false"));
        assert_eq!(lit("[]", "").as_deref(), Some("[]"));
        assert_eq!(lit("Sort_Regular", "").as_deref(), Some("0"));
        assert_eq!(lit("COUNT_NORMAL", "").as_deref(), Some("0"));
        assert_eq!(lit("PREG_SPLIT_NO_EMPTY", "").as_deref(), Some("1"));
    }

    #[test]
    fn signatures_render() {
        let (_, m) = php_spl::classes::find_method("SplFileObject", "fgetcsv").expect("method");
        assert_eq!(
            signature(m),
            "fgetcsv(string $separator = ',', string $enclosure = '\"', string $escape = '\\\\'): array|false"
        );
        let (_, m) =
            php_spl::classes::find_method("CallbackFilterIterator", "__construct").expect("ctor");
        assert_eq!(
            signature(m),
            "__construct(@unchecked Iterator $iterator, @unchecked callable $callback)"
        );
        let (_, m) = php_spl::classes::find_method("ArrayObject", "exchangeArray").expect("method");
        assert_eq!(
            signature(m),
            "exchangeArray(@unchecked array|object $array): array"
        );
        let (_, m) = php_spl::classes::find_method("ArrayIterator", "__construct").expect("ctor");
        assert_eq!(
            signature(m),
            "__construct(@unchecked array|object $array = [], @unchecked int $flags = 0)"
        );
    }
}
