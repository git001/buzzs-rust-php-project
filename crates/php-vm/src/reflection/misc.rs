// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Reflection`, `Reflector`, `ReflectionException`, `ReflectionGenerator`,
//! `ReflectionFiber`, `ReflectionReference`, `ReflectionExtension`,
//! `ReflectionZendExtension`, the `PropertyHookType` enum, and the
//! attribution of internal functions/classes/constants to extensions.

use super::{rexc, set_name_props, str_arg, this, this_refl, zs, FnRef, Refl, PRIV, PUB, RO, STAT};
use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class, ClassConst, ConstExpr, InternalKind, Magic, NameMap, PropInfo};
use crate::func::{flags, Args, Builtin, Func, Method};
use crate::object;
use crate::types::{mask, TypeDecl};
use crate::vm::{Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Obj, Value, ZArray, ZStr};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

// ---------------------------------------------------------------------
// Extension attribution
// ---------------------------------------------------------------------

/// Opens (`begin = true`) or closes the window in which every *new*
/// function, class and constant is attributed to extension `name`
/// (the insert sites report new keys to `builtins::info`'s registry,
/// which also keeps `get_extension_funcs()`'s module attribution: one
/// entry per symbol).
pub(crate) fn record_module(_vm: &Vm, name: &'static str, begin: bool) {
    crate::builtins::info::set_current_module(begin.then_some(name));
}

/// Core (`zend_builtin_functions.c`) functions.
const CORE_FUNCTIONS: &[&str] = &[
    "zend_version",
    "func_num_args",
    "func_get_arg",
    "func_get_args",
    "strlen",
    "strcmp",
    "strncmp",
    "strcasecmp",
    "strncasecmp",
    "error_reporting",
    "define",
    "defined",
    "get_class",
    "get_called_class",
    "get_parent_class",
    "is_subclass_of",
    "is_a",
    "get_class_vars",
    "get_object_vars",
    "get_mangled_object_vars",
    "get_class_methods",
    "method_exists",
    "property_exists",
    "class_exists",
    "interface_exists",
    "trait_exists",
    "enum_exists",
    "function_exists",
    "class_alias",
    "get_included_files",
    "get_required_files",
    "trigger_error",
    "user_error",
    "set_error_handler",
    "restore_error_handler",
    "set_exception_handler",
    "restore_exception_handler",
    "get_exception_handler",
    "get_declared_classes",
    "get_declared_traits",
    "get_declared_interfaces",
    "get_defined_functions",
    "get_defined_vars",
    "get_resource_type",
    "get_resource_id",
    "get_resources",
    "get_loaded_extensions",
    "get_defined_constants",
    "debug_backtrace",
    "debug_print_backtrace",
    "extension_loaded",
    "get_extension_funcs",
    "gc_mem_caches",
    "gc_collect_cycles",
    "gc_enabled",
    "gc_enable",
    "gc_disable",
    "gc_status",
    "clone",
    "exit",
    "die",
];

/// The extension of an internal function.
pub(crate) fn function_module(
    vm: &Vm,
    f: &Func,
    scope: Option<&Rc<Class>>,
    name: &ZStr,
) -> Option<String> {
    if f.is_user() {
        return None;
    }
    if let Func::Builtin(b) = f {
        if b.flags & flags::NO_MODULE != 0 {
            return None;
        }
    }
    if let Some(c) = scope {
        return class_module(vm, c);
    }
    let lc = name.to_ascii_lowercase();
    if let Some(m) = crate::builtins::info::function_origin(lc.as_bytes()) {
        return Some(m.to_string());
    }
    let s = String::from_utf8_lossy(&lc).into_owned();
    // Unattributed functions are the VM's own builtins.
    Some(if CORE_FUNCTIONS.contains(&s.as_str()) {
        "Core".into()
    } else {
        "standard".into()
    })
}

/// ext/standard classes the VM registers itself.
const STANDARD_CLASSES: &[&[u8]] = &[
    b"assertionerror",
    b"directory",
    b"__php_incomplete_class",
    b"php_user_filter",
    b"roundingmode",
    b"sortdirection",
    b"streambucket",
    b"streamerror",
    b"streamerrorcode",
    b"streamerrormode",
    b"streamerrorstore",
    b"streamexception",
    b"streampollhandle",
];

/// The extension of an internal class.
pub(crate) fn class_module(vm: &Vm, c: &Rc<Class>) -> Option<String> {
    if c.flags & cflags::INTERNAL == 0 {
        return None;
    }
    let n = c.lc_name.as_bytes();
    // ext/standard classes some other extension may declare on its behalf
    // (bcmath declares `RoundingMode` when it registers first).
    if STANDARD_CLASSES.contains(&n) {
        return Some("standard".into());
    }
    if let Some(m) = crate::builtins::info::class_origin(n) {
        return Some(m.to_string());
    }
    let _ = vm;
    // Classes registered by the VM itself: the reflection classes and the
    // engine's (`Closure`, `Exception`, ...).
    if n.starts_with(b"reflect") || n == b"propertyhooktype" {
        return Some("Reflection".into());
    }
    if n.starts_with(b"io\\") {
        return Some("standard".into());
    }
    Some("Core".into())
}

/// The extension of an internal constant.
pub(crate) fn constant_module(_vm: &Vm, name: &ZStr) -> String {
    // Unattributed constants are the VM's own (`Core` engine constants,
    // otherwise `standard`), as `get_defined_constants(true)` splits them.
    const PREFIXES: &[&[u8]] = &[
        b"E_",
        b"PHP_",
        b"ZEND_",
        b"DEBUG_BACKTRACE_",
        b"UPLOAD_ERR_",
        b"PEAR_",
    ];
    if let Some(m) = crate::builtins::info::constant_origin(name.as_bytes()) {
        return m.to_string();
    }
    let n = name.as_bytes();
    let core = PREFIXES.iter().any(|p| n.starts_with(p))
        || matches!(
            n,
            b"TRUE"
                | b"FALSE"
                | b"NULL"
                | b"DEFAULT_INCLUDE_PATH"
                | b"STDIN"
                | b"STDOUT"
                | b"STDERR"
                | b"__COMPILER_HALT_OFFSET__"
        );
    if core {
        "Core".into()
    } else {
        "standard".into()
    }
}

/// Whether a global constant is deprecated (internal deprecation table or
/// `#[\Deprecated]` on a user constant).
pub(crate) fn constant_deprecated(vm: &Vm, name: &ZStr) -> bool {
    vm.deprecated_constants.contains_key(name)
        || name.as_bytes() == b"E_STRICT"
        || vm
            .const_attributes
            .get(name)
            .is_some_and(|a| a.iter().any(|x| x.name.eq_ignore_ascii_case(b"Deprecated")))
}

/// The file a user constant was declared in.
pub(crate) fn constant_file(vm: &Vm, name: &ZStr) -> Option<ZStr> {
    vm.const_files.get(name).cloned()
}

/// All extension names, as `get_loaded_extensions()` reports them.
fn module_names(vm: &Vm) -> Vec<String> {
    let mut v: Vec<String> = crate::LOADED_EXTENSIONS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    v.extend(vm.loaded_extensions.iter().cloned());
    v
}

/// `reflection_extension_factory`.
pub(crate) fn extension_value(vm: &mut Vm, name: &str) -> VmResult<Value> {
    let n = ZStr::from(name);
    super::new_refl(
        vm,
        "ReflectionExtension",
        Refl::Extension(n.clone()),
        Some(&n),
        None,
    )
}

// ---------------------------------------------------------------------
// Reflection
// ---------------------------------------------------------------------

fn get_modifier_names(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = a.at(0).as_int().unwrap_or(0);
    let mut out = Vec::new();
    if m & 64 != 0 {
        out.push(zs("abstract"));
    }
    if m & 32 != 0 {
        out.push(zs("final"));
    }
    if m & 512 != 0 {
        out.push(zs("virtual"));
    }
    match m & 7 {
        1 => out.push(zs("public")),
        4 => out.push(zs("private")),
        2 => out.push(zs("protected")),
        _ => {}
    }
    match m & 0x1c00 {
        0x800 => out.push(zs("protected(set)")),
        0x1000 => out.push(zs("private(set)")),
        _ => {}
    }
    if m & 16 != 0 {
        out.push(zs("static"));
    }
    if m & (128 | 65536) != 0 {
        out.push(zs("readonly"));
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

/// Registers `ReflectionException`, `Reflection` and `Reflector`.
pub(crate) fn register_base(vm: &mut Vm) {
    ClassBuilder::new("ReflectionException")
        .extends(vm, "Exception")
        .register(vm);
    ClassBuilder::new("Reflection")
        .method(
            "getModifierNames(int $modifiers): array",
            STAT,
            get_modifier_names,
        )
        .register(vm);
    ClassBuilder::new("Reflector")
        .flags(cflags::INTERFACE | cflags::ABSTRACT)
        .implements(vm, &["Stringable"])
        .register(vm);
}

fn clone_error(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(rexc(vm, "Cannot clone object using __clone()"))
}

// ---------------------------------------------------------------------
// ReflectionGenerator
// ---------------------------------------------------------------------

fn this_gen(vm: &mut Vm, a: &Args) -> VmResult<Obj> {
    match this_refl(vm, a)? {
        Refl::Generator(g, _) => Ok(g),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// Information about the executing frame of a generator.
struct GenFrame {
    file: ZStr,
    line: u32,
    func: Option<Func>,
    this: Option<Obj>,
    closure: Option<Obj>,
    scope: Option<Rc<Class>>,
    args: Vec<Value>,
}

impl GenFrame {
    fn of(f: &crate::vm::Frame) -> Self {
        GenFrame {
            file: f.code.filename.clone(),
            line: f.line(),
            func: f.func.clone(),
            this: f.this.clone(),
            closure: f.closure.clone(),
            scope: f.scope.clone(),
            args: f
                .args
                .iter()
                .map(|a| a.clone().map(|v| v.deref()).unwrap_or_default())
                .collect(),
        }
    }
}

thread_local! {
    /// Generator handle → function, remembered while the frame was seen
    /// (a frame lent to a running delegate is not marked with its
    /// generator).
    static GEN_FUNCS: RefCell<HashMap<u32, Func>> = RefCell::new(HashMap::new());
}

/// Request reset: object handles start again at 1, so the remembered
/// generators' functions would be found for other generators.
pub(crate) fn reset_request() {
    GEN_FUNCS.with(|m| m.borrow_mut().clear());
}

fn gen_frame(vm: &Vm, g: &Obj) -> Option<GenFrame> {
    let suspended = object::internal::<crate::generator::GenData, _>(g, |d| {
        if d.state == crate::generator::GenState::Finished {
            return Some(None);
        }
        d.frame.as_ref().map(|f| Some(GenFrame::of(f)))
    })
    .flatten();
    let found = match suspended {
        Some(x) => x,
        None => {
            let own = vm.frames.iter().rev().find(|f| {
                f.generator
                    .as_ref()
                    .is_some_and(|x| x.handle() == g.handle())
            });
            match own {
                Some(f) => Some(GenFrame::of(f)),
                None => {
                    // Lent to a delegate: find the frame running its function.
                    let func = GEN_FUNCS.with(|m| m.borrow().get(&g.handle()).cloned());
                    func.and_then(|func| {
                        vm.frames
                            .iter()
                            .rev()
                            .find(|f| {
                                f.generator.is_none()
                                    && f.func.as_ref().is_some_and(|x| Func::ptr_eq(x, &func))
                            })
                            .map(GenFrame::of)
                    })
                }
            }
        }
    };
    if let Some(f) = found.as_ref().and_then(|f| f.func.clone()) {
        GEN_FUNCS.with(|m| m.borrow_mut().insert(g.handle(), f));
    }
    found
}

fn gen_closed(vm: &mut Vm) -> crate::vm::Throw {
    rexc(vm, "Cannot fetch information from a closed Generator")
}

fn rg_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let Value::Object(g) = a.at(0) else {
        return Ok(Value::Null);
    };
    let f = gen_frame(vm, &g).and_then(|f| gen_fnref(&f));
    object::set_internal(&o, Refl::Generator(g, f));
    Ok(Value::Null)
}

fn rg_get_executing_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    match gen_frame(vm, &g) {
        Some(f) => Ok(Value::Int(i64::from(f.line))),
        None => Err(gen_closed(vm)),
    }
}

fn rg_get_executing_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    match gen_frame(vm, &g) {
        Some(f) => Ok(Value::Str(f.file)),
        None => Err(gen_closed(vm)),
    }
}

fn rg_get_this(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    match gen_frame(vm, &g) {
        Some(f) => Ok(f.this.map_or(Value::Null, Value::Object)),
        None => Err(gen_closed(vm)),
    }
}

/// The reflected function of a generator frame.
fn gen_fnref(f: &GenFrame) -> Option<FnRef> {
    let func = f.func.clone()?;
    if let Some(c) = &f.closure {
        return Some(FnRef {
            func,
            method: None,
            class: None,
            closure: Some(c.clone()),
        });
    }
    if let Some(s) = &f.scope {
        if let Some(m) = s.find_method(func.name().as_bytes()).cloned() {
            return Some(FnRef::method(&m, s));
        }
    }
    Some(FnRef::function(func))
}

/// The function of a finished generator, found from the name its debug
/// info reports (`Generator::__debugInfo()` keeps `func_name`).
fn gen_fnref_by_name(vm: &mut Vm, g: &Obj) -> Option<FnRef> {
    let props = vm
        .props_for(g, php_runtime::PropertyPurpose::Debug)
        .ok()??;
    let name = match props.get(&ArrayKey::from("function"))? {
        Value::Str(s) => s.clone(),
        _ => return None,
    };
    if let Some(p) = crate::call::find_double_colon(name.as_bytes()) {
        let c = vm.find_class(&name.as_bytes()[..p])?;
        let m = c.find_method(&name.as_bytes()[p + 2..])?.clone();
        return Some(FnRef::method(&m, &c));
    }
    if let Some(f) = vm.functions.get(&name.to_ascii_lowercase()).cloned() {
        return Some(FnRef::function(f));
    }
    // A closure: find its compiled body by name.
    vm.units.iter().find_map(|u| {
        u.decls
            .functions
            .borrow()
            .iter()
            .find(|f| f.name == name)
            .cloned()
            .map(|f| FnRef::function(Func::User(f)))
    })
}

fn rg_get_function(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Refl::Generator(g, saved) = this_refl(vm, a)? else {
        return Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let f = gen_frame(vm, &g)
        .and_then(|f| gen_fnref(&f))
        .or(saved)
        .or_else(|| gen_fnref_by_name(vm, &g));
    let Some(f) = f else {
        return Err(gen_closed(vm));
    };
    match (&f.method, &f.class) {
        (Some(m), Some(c)) => super::method_value(vm, &m.clone(), &c.clone()),
        _ => super::function_value(vm, f),
    }
}

fn rg_get_executing_generator(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    if gen_frame(vm, &g).is_none() {
        return Err(gen_closed(vm));
    }
    let cur = vm.gen_get_current(&g)?;
    Ok(Value::Object(cur))
}

fn rg_is_closed(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    Ok(Value::Bool(gen_frame(vm, &g).is_none()))
}

fn rg_get_trace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let g = this_gen(vm, a)?;
    let options = match a.get(0) {
        Some(Value::Int(i)) => i,
        _ => 1,
    };
    if gen_frame(vm, &g).is_none() {
        return Err(gen_closed(vm));
    }
    // The delegation chain: `g` delegates to its `parent`, down to the
    // executing generator; the trace lists the executing one first.
    let mut chain = vec![g.clone()];
    let mut cur = g.clone();
    while let Some(p) =
        object::internal::<crate::generator::GenData, _>(&cur, |d| d.parent.clone()).flatten()
    {
        if chain.iter().any(|x| Obj::ptr_eq(x, &p)) {
            break;
        }
        chain.push(p.clone());
        cur = p;
    }
    chain.reverse();
    let frames: Vec<Option<GenFrame>> = chain.iter().map(|x| gen_frame(vm, x)).collect();
    let mut out = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let Some(f) = f else { continue };
        let mut e = HashTable::new();
        if let Some(Some(caller)) = frames.get(i + 1) {
            e.insert(ArrayKey::from("file"), Value::Str(caller.file.clone()));
            e.insert(ArrayKey::from("line"), Value::Int(i64::from(caller.line)));
        }
        if let Some(func) = &f.func {
            e.insert(ArrayKey::from("function"), Value::Str(func.name().clone()));
            if f.closure.is_none() {
                if let Some(s) = &f.scope {
                    e.insert(ArrayKey::from("class"), Value::Str(s.name.clone()));
                    if let (Some(o), true) = (&f.this, options & 1 != 0) {
                        e.insert(ArrayKey::from("object"), Value::Object(o.clone()));
                    }
                    e.insert(
                        ArrayKey::from("type"),
                        zs(if f.this.is_some() { "->" } else { "::" }),
                    );
                }
            }
        }
        if options & 2 == 0 {
            e.insert(
                ArrayKey::from("args"),
                Value::Array(ZArray::from_values(f.args.clone())),
            );
        }
        out.push(Value::Array(ZArray::from_table(e)));
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

/// Registers `ReflectionGenerator`.
pub(crate) fn register_generator(vm: &mut Vm) {
    ClassBuilder::new("ReflectionGenerator")
        .flags(cflags::FINAL | cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method("__construct(Generator $generator)", PUB, rg_construct)
        .method("getExecutingLine(): int", PUB, rg_get_executing_line)
        .method("getExecutingFile(): string", PUB, rg_get_executing_file)
        .method(
            "getTrace(int $options = DEBUG_BACKTRACE_PROVIDE_OBJECT): array",
            PUB,
            rg_get_trace,
        )
        .method(
            "getFunction(): ReflectionFunctionAbstract",
            PUB,
            rg_get_function,
        )
        .method("getThis(): ?object", PUB, rg_get_this)
        .method(
            "getExecutingGenerator(): Generator",
            PUB,
            rg_get_executing_generator,
        )
        .method("isClosed(): bool", PUB, rg_is_closed)
        .register(vm);
}

// ---------------------------------------------------------------------
// ReflectionFiber
// ---------------------------------------------------------------------

fn this_fiber(vm: &mut Vm, a: &Args) -> VmResult<Obj> {
    match this_refl(vm, a)? {
        Refl::Fiber(f) => Ok(f),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn fiber_state(f: &Obj) -> Option<crate::fiber::FiberState> {
    object::internal::<crate::fiber::FiberData, _>(f, |d| d.state)
}

fn fiber_location(vm: &Vm, f: &Obj) -> Option<(ZStr, u32)> {
    let st = fiber_state(f)?;
    match st {
        crate::fiber::FiberState::Suspended => {
            object::internal::<crate::fiber::FiberData, _>(f, |d| {
                d.frames
                    .iter()
                    .rev()
                    .find(|fr| fr.kind != crate::vm::FrameKind::Builtin)
                    .map(|fr| (fr.code.filename.clone(), fr.line()))
            })
            .flatten()
        }
        crate::fiber::FiberState::Running => {
            let fr = vm
                .frames
                .iter()
                .rev()
                .find(|fr| fr.kind != crate::vm::FrameKind::Builtin)?;
            Some((fr.code.filename.clone(), fr.line()))
        }
        _ => None,
    }
}

fn fiber_check(vm: &mut Vm, f: &Obj) -> VmResult<()> {
    match fiber_state(f) {
        Some(crate::fiber::FiberState::Suspended | crate::fiber::FiberState::Running) => Ok(()),
        _ => Err(vm.make_throw(
            "Error",
            "Cannot fetch information from a fiber that has not been started or is terminated",
        )),
    }
}

fn rfb_construct(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    if let Value::Object(f) = a.at(0) {
        object::set_internal(&o, Refl::Fiber(f));
    }
    Ok(Value::Null)
}

fn rfb_get_fiber(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Object(this_fiber(vm, a)?))
}

fn rfb_get_executing_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fiber(vm, a)?;
    fiber_check(vm, &f)?;
    Ok(fiber_location(vm, &f).map_or(Value::Null, |(file, _)| Value::Str(file)))
}

fn rfb_get_executing_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fiber(vm, a)?;
    fiber_check(vm, &f)?;
    Ok(fiber_location(vm, &f).map_or(Value::Null, |(_, l)| Value::Int(i64::from(l))))
}

fn rfb_get_callable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fiber(vm, a)?;
    if matches!(fiber_state(&f), Some(crate::fiber::FiberState::Dead) | None) {
        return Err(vm.make_throw(
            "Error",
            "Cannot fetch the callable from a fiber that has terminated",
        ));
    }
    Ok(crate::fiber::fiber_callable(&f).unwrap_or_default())
}

fn rfb_get_trace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fiber(vm, a)?;
    fiber_check(vm, &f)?;
    let options = match a.get(0) {
        Some(Value::Int(i)) => i,
        _ => 1,
    };
    let (ignore_args, provide_object) = (options & 2 != 0, options & 1 != 0);
    match fiber_state(&f) {
        Some(crate::fiber::FiberState::Running) => {
            // Only the fiber's own frames: hide the frames below its base.
            let base = object::internal::<crate::fiber::FiberData, _>(&f, |d| d.base)
                .unwrap_or(0)
                .min(vm.frames.len());
            // A fiber that resumed another one ends where that one starts.
            let end = vm
                .fibers
                .iter()
                .filter(|x| {
                    !Obj::ptr_eq(x, &f) && fiber_state(x) == Some(crate::fiber::FiberState::Running)
                })
                .filter_map(|x| object::internal::<crate::fiber::FiberData, _>(x, |d| d.base))
                .filter(|b| *b > base)
                .min()
                .unwrap_or(vm.frames.len())
                .min(vm.frames.len());
            let above = vm.frames.split_off(end);
            let head: Vec<crate::vm::Frame> = vm.frames.drain(..base).collect();
            let t = vm.backtrace(false, ignore_args, provide_object, 0);
            let mid = std::mem::take(&mut vm.frames);
            vm.frames = head.into();
            vm.frames.extend(mid);
            vm.frames.extend(above);
            Ok(Value::Array(t))
        }
        _ => {
            // Suspended: trace the fiber's saved frames.
            let saved = object::internal::<crate::fiber::FiberData, _>(&f, |d| {
                std::mem::take(&mut d.frames)
            })
            .unwrap_or_default();
            let outer = std::mem::replace(&mut vm.frames, saved.into());
            let t = vm.backtrace(false, ignore_args, provide_object, 0);
            let saved = std::mem::replace(&mut vm.frames, outer);
            object::internal::<crate::fiber::FiberData, _>(&f, |d| d.frames = saved.into_vec());
            Ok(Value::Array(t))
        }
    }
}

/// Registers `ReflectionFiber`.
pub(crate) fn register_fiber(vm: &mut Vm) {
    ClassBuilder::new("ReflectionFiber")
        .flags(cflags::FINAL | cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method("__construct(Fiber $fiber)", PUB, rfb_construct)
        .method("getFiber(): Fiber", PUB, rfb_get_fiber)
        .method("getExecutingFile(): ?string", PUB, rfb_get_executing_file)
        .method("getExecutingLine(): ?int", PUB, rfb_get_executing_line)
        .method("getCallable(): callable", PUB, rfb_get_callable)
        .method(
            "getTrace(int $options = DEBUG_BACKTRACE_PROVIDE_OBJECT): array",
            PUB,
            rfb_get_trace,
        )
        .register(vm);
}

// ---------------------------------------------------------------------
// ReflectionReference
// ---------------------------------------------------------------------

thread_local! {
    static REF_KEY: Cell<u64> = const { Cell::new(0) };
}

fn rr_from_array_element(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Value::Array(arr) = a.at(0) else {
        return Ok(Value::Null);
    };
    let key = match a.at(1) {
        Value::Int(i) => ArrayKey::Int(i),
        Value::Str(s) => ArrayKey::from_zstr(&s),
        _ => return Ok(Value::Null),
    };
    match arr.get(&key) {
        None => Err(rexc(vm, "Array key not found")),
        Some(Value::Reference(r)) => {
            let self_ref = matches!(&*r.borrow(), Value::Array(x) if ZArray::ptr_eq(x, &arr));
            if r.refcount() == 1 && !self_ref {
                return Ok(Value::Null);
            }
            super::new_refl(
                vm,
                "ReflectionReference",
                Refl::Reference(r.clone()),
                None,
                None,
            )
        }
        Some(_) => Ok(Value::Null),
    }
}

fn rr_get_id(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Refl::Reference(r) = this_refl(vm, a)? else {
        return Err(rexc(vm, "Corrupted ReflectionReference object"));
    };
    let key = REF_KEY.with(|k| {
        if k.get() == 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos());
            k.set(u64::try_from(now & 0xffff_ffff_ffff).unwrap_or(1) | 1);
        }
        k.get()
    });
    let mut data = key.to_le_bytes().to_vec();
    data.extend_from_slice(&(r.addr() as u64).to_le_bytes());
    let digest = php_hash::sha1(&data);
    Ok(Value::Str(ZStr::from(digest.to_vec())))
}

fn rr_construct(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(vm.make_throw(
        "Error",
        "Call to private ReflectionReference::__construct() from global scope",
    ))
}

/// Registers `ReflectionReference`.
pub(crate) fn register_reference(vm: &mut Vm) {
    // Not `NO_NEW`: `new ReflectionReference()` reaches the private
    // constructor and fails its visibility check, as in php-src.
    ClassBuilder::new("ReflectionReference")
        .flags(cflags::FINAL | cflags::NOT_SERIALIZABLE)
        .method(
            "fromArrayElement(array $array, int|string $key): ?ReflectionReference",
            STAT,
            rr_from_array_element,
        )
        .method("getId(): string", PUB, rr_get_id)
        .method("__clone(): void", PRIV, clone_error)
        .method("__construct()", PRIV, rr_construct)
        .register(vm);
}

// ---------------------------------------------------------------------
// ReflectionExtension / ReflectionZendExtension
// ---------------------------------------------------------------------

fn this_ext(vm: &mut Vm, a: &Args) -> VmResult<String> {
    match this_refl(vm, a)? {
        Refl::Extension(n) => Ok(n.to_string_lossy()),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn re_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let n = str_arg(a, 0).to_string_lossy();
    let Some(m) = module_names(vm)
        .into_iter()
        .find(|m| m.eq_ignore_ascii_case(&n))
    else {
        return Err(rexc(vm, &format!("Extension \"{n}\" does not exist")));
    };
    let z = ZStr::from(m.as_str());
    set_name_props(&o, Some(&z), None);
    object::set_internal(&o, Refl::Extension(z));
    Ok(Value::Null)
}

fn re_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(zs(&this_ext(vm, a)?))
}

fn re_get_version(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _ = this_ext(vm, a)?;
    Ok(zs(crate::PHP_VERSION))
}

/// The functions of extension `m` in registration order
/// (`get_extension_funcs()`).
fn ext_functions(vm: &mut Vm, m: &str) -> Vec<(ZStr, Func)> {
    let names = match vm.call_function("get_extension_funcs", vec![Value::Str(ZStr::from(m))]) {
        Ok(Value::Array(a)) => a
            .values()
            .filter_map(|v| v.as_str().cloned())
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .filter_map(|n| {
            let lc = n.to_ascii_lowercase();
            vm.functions.get(&lc).cloned().map(|f| (lc, f))
        })
        .collect()
}

fn re_get_functions(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let mut t = HashTable::new();
    for (k, f) in ext_functions(vm, &m) {
        let name = f.name().clone();
        let v = super::function_value(vm, FnRef::function(f))?;
        let _ = k;
        t.insert(ArrayKey::Str(name), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

/// The constants of extension `m` in registration order
/// (`get_defined_constants(true)[$m]`).
fn ext_constants(vm: &mut Vm, m: &str) -> Vec<(ZStr, Value)> {
    let Ok(Value::Array(all)) = vm.call_function("get_defined_constants", vec![Value::Bool(true)])
    else {
        return Vec::new();
    };
    let group = all.iter().find_map(|(k, v)| match (k, v) {
        (ArrayKey::Str(s), Value::Array(a)) if s.eq_ignore_ascii_case(m.as_bytes()) => {
            Some(a.clone())
        }
        _ => None,
    });
    group
        .map(|g| {
            g.iter()
                .filter_map(|(k, v)| match k {
                    ArrayKey::Str(s) => Some((s.clone(), v.clone())),
                    ArrayKey::Int(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn re_get_constants(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let mut t = HashTable::new();
    for (k, v) in ext_constants(vm, &m) {
        t.insert(ArrayKey::Str(k), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn re_get_ini_entries(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let mut t = HashTable::new();
    if let Some(entries) = module_ini(vm, &m) {
        for e in entries {
            let v = e
                .local_value
                .map_or(Value::Null, |x| Value::Str(ZStr::from(x)));
            t.insert(ArrayKey::Str(ZStr::from(e.name.as_str())), v);
        }
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn ext_classes(vm: &Vm, m: &str) -> Vec<Rc<Class>> {
    vm.class_order
        .iter()
        .filter_map(|lc| vm.classes.get(lc).cloned())
        .filter(|c| c.flags & cflags::INTERNAL != 0)
        .filter(|c| class_module(vm, c).is_some_and(|x| x.eq_ignore_ascii_case(m)))
        .collect()
}

fn re_get_classes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let mut t = HashTable::new();
    for c in ext_classes(vm, &m) {
        let v = super::class_value(vm, &c)?;
        t.insert(ArrayKey::Str(c.name.clone()), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn re_get_class_names(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    Ok(Value::Array(ZArray::from_values(
        ext_classes(vm, &m)
            .into_iter()
            .map(|c| Value::Str(c.name.clone())),
    )))
}

/// `zend_module_entry.deps` of the bundled extensions (`ZEND_MOD_*`).
fn module_deps(m: &str) -> &'static [(&'static str, &'static str)] {
    match m.to_ascii_lowercase().as_str() {
        "dom" => &[
            ("libxml", "Required"),
            ("lexbor", "Required"),
            ("domxml", "Conflicts"),
        ],
        "exif" => &[("standard", "Required"), ("mbstring", "Optional")],
        "libxml" | "mysqlnd" | "openssl" | "sodium" => &[("standard", "Required")],
        "mbstring" | "pgsql" | "zip" => &[("pcre", "Required")],
        "mysqli" => &[("spl", "Required"), ("mysqlnd", "Required")],
        "pdo" | "snmp" => &[("spl", "Required")],
        "pdo_dblib" | "pdo_firebird" | "pdo_odbc" | "pdo_pgsql" | "pdo_sqlite" => {
            &[("pdo", "Required")]
        }
        "pdo_mysql" => &[("pdo", "Required"), ("mysqlnd", "Required")],
        "phar" => &[
            ("apc", "Optional"),
            ("bz2", "Optional"),
            ("openssl", "Optional"),
            ("zlib", "Optional"),
            ("standard", "Optional"),
            ("hash", "Required"),
            ("spl", "Required"),
        ],
        "simplexml" => &[("libxml", "Required"), ("spl", "Required")],
        "soap" => &[
            ("date", "Required"),
            ("libxml", "Required"),
            ("session", "Optional"),
        ],
        "spl" => &[("json", "Required")],
        "standard" => &[
            ("random", "Required"),
            ("uri", "Required"),
            ("session", "Optional"),
        ],
        "uri" => &[("lexbor", "Required")],
        "xml" | "xmlwriter" => &[("libxml", "Required")],
        "xmlreader" => &[("dom", "Required"), ("libxml", "Required")],
        "xsl" => &[("libxml", "Required"), ("dom", "Required")],
        _ => &[],
    }
}

fn re_get_dependencies(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let mut t = HashTable::new();
    for (k, v) in module_deps(&m) {
        t.insert(ArrayKey::from(*k), zs(v));
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

/// The INI entries of extension `m`: those registered with its module
/// name, plus `m.*` directives the VM registers under `Core` on behalf of
/// bundled extensions (`pcre.*`, `date.*`, ...).
fn module_ini(vm: &Vm, m: &str) -> Option<Vec<php_support::ini::IniEntryInfo>> {
    let own: Vec<String> = vm
        .ini
        .ini_entries_registered(Some(m))
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.name)
        .collect();
    let prefix = format!("{}.", m.to_ascii_lowercase());
    let borrowed: Vec<String> = if m.eq_ignore_ascii_case("Core") {
        Vec::new()
    } else {
        vm.ini
            .ini_entries_registered(Some("Core"))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.name.to_ascii_lowercase().starts_with(&prefix))
            .map(|e| e.name)
            .collect()
    };
    // Registration order (`EG(ini_directives)`).
    let all = vm.ini.ini_entries_registered(None).unwrap_or_default();
    Some(
        all.into_iter()
            .filter(|e| own.contains(&e.name) || borrowed.contains(&e.name))
            .collect(),
    )
}

/// `display_ini_entries()` (text mode) for module `m`.
fn ini_block(vm: &Vm, m: &str, out: &mut Vec<u8>) {
    let Some(entries) = module_ini(vm, m) else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    let show = |v: Option<&[u8]>| -> Vec<u8> {
        match v {
            Some(x) if !x.is_empty() => x.to_vec(),
            _ => b"no value".to_vec(),
        }
    };
    out.extend_from_slice(b"\nDirective => Local Value => Master Value\n");
    for e in entries {
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(b" => ");
        out.extend(show(e.local_value.as_deref()));
        out.extend_from_slice(b" => ");
        out.extend(show(e.global_value.as_deref()));
        out.push(b'\n');
    }
}

/// `php_info_print_module()` in text mode.
fn module_info_text(vm: &mut Vm, m: &str) -> Vec<u8> {
    use crate::ext::InfoItem;
    let mut out = format!("\n{m}\n").into_bytes();
    if m.eq_ignore_ascii_case("Reflection") {
        // `PHP_MINFO_FUNCTION(reflection)`.
        out.extend_from_slice(b"\nReflection => enabled\n");
        return out;
    }
    let cell = |c: &str| {
        if c.is_empty() {
            " ".to_string()
        } else {
            c.to_string()
        }
    };
    match vm.module_info(m) {
        Some(items) => {
            for it in items {
                match it {
                    InfoItem::TableStart => out.push(b'\n'),
                    InfoItem::TableEnd => {}
                    InfoItem::Header(cols) | InfoItem::Row(cols) => {
                        let v: Vec<String> = cols.iter().map(|c| cell(c)).collect();
                        out.extend_from_slice(v.join(" => ").as_bytes());
                        out.push(b'\n');
                    }
                    InfoItem::ColspanHeader(text) => {
                        let spaces = 74i64 - i64::try_from(text.len()).unwrap_or(0);
                        let pad = usize::try_from((spaces / 2).max(1)).unwrap_or(1);
                        out.extend_from_slice(
                            format!("{:>pad$}{text}{:>pad$}\n", " ", " ").as_bytes(),
                        );
                    }
                    InfoItem::IniEntries => ini_block(vm, m, &mut out),
                }
            }
        }
        None => {
            out.extend_from_slice(format!("\nVersion => {}\n", crate::PHP_VERSION).as_bytes());
            ini_block(vm, m, &mut out);
        }
    }
    out
}

fn re_info(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let out = module_info_text(vm, &m);
    vm.echo(&out);
    Ok(Value::Null)
}

fn re_is_persistent(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _ = this_ext(vm, a)?;
    Ok(Value::Bool(true))
}

fn re_is_temporary(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _ = this_ext(vm, a)?;
    Ok(Value::Bool(false))
}

fn re_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let m = this_ext(vm, a)?;
    let num = module_names(vm)
        .iter()
        .position(|x| x.eq_ignore_ascii_case(&m))
        .map_or(0, |i| i + 1);
    let mut out = format!(
        "Extension [ <persistent> extension #{num} {m} version {} ] {{\n",
        crate::PHP_VERSION
    )
    .into_bytes();
    let deps = module_deps(&m);
    if !deps.is_empty() {
        out.extend_from_slice(b"\n  - Dependencies {\n");
        for (n, k) in deps {
            out.extend_from_slice(format!("    Dependency [ {n} ({k}) ]\n").as_bytes());
        }
        out.extend_from_slice(b"  }\n");
    }
    if let Some(entries) = module_ini(vm, &m) {
        if !entries.is_empty() {
            out.extend_from_slice(b"\n  - INI {\n");
            for e in entries {
                let acc = match e.access {
                    7 => "ALL".to_string(),
                    x => {
                        let mut p = Vec::new();
                        if x & 1 != 0 {
                            p.push("USER");
                        }
                        if x & 2 != 0 {
                            p.push("PERDIR");
                        }
                        if x & 4 != 0 {
                            p.push("SYSTEM");
                        }
                        p.join(",")
                    }
                };
                out.extend_from_slice(
                    format!("    Entry [ {} <{acc}> ]\n      Current = '", e.name).as_bytes(),
                );
                out.extend_from_slice(e.local_value.as_deref().unwrap_or_default());
                out.extend_from_slice(b"'\n");
                if e.local_value != e.global_value {
                    out.extend_from_slice(b"      Default = '");
                    out.extend_from_slice(e.global_value.as_deref().unwrap_or_default());
                    out.extend_from_slice(b"'\n");
                }
                out.extend_from_slice(b"    }\n");
            }
            out.extend_from_slice(b"  }\n");
        }
    }
    let consts = ext_constants(vm, &m);
    if !consts.is_empty() {
        out.extend_from_slice(format!("\n  - Constants [{}] {{\n", consts.len()).as_bytes());
        for (k, v) in consts {
            out.extend(super::export::const_string(
                vm,
                &k,
                &v,
                &["persistent"],
                "    ",
            )?);
        }
        out.extend_from_slice(b"  }\n");
    }
    let funcs = ext_functions(vm, &m);
    if !funcs.is_empty() {
        out.extend_from_slice(b"\n  - Functions {\n");
        for (_, f) in funcs {
            out.extend(super::export::function_string(
                vm,
                &FnRef::function(f),
                None,
                "    ",
            ));
        }
        out.extend_from_slice(b"  }\n");
    }
    let classes = ext_classes(vm, &m);
    if !classes.is_empty() {
        out.extend_from_slice(format!("\n  - Classes [{}] {{", classes.len()).as_bytes());
        for c in classes {
            out.push(b'\n');
            out.extend(super::export::class_string(vm, &c, None, "    ")?);
        }
        out.extend_from_slice(b"  }\n");
    }
    out.extend_from_slice(b"}\n");
    Ok(Value::Str(ZStr::from(out)))
}

/// A loaded Zend extension (`zend_extension` in `zend_extensions.h`).
pub(crate) struct ZendExtension {
    pub(crate) name: &'static str,
    author: &'static str,
    url: &'static str,
    copyright: &'static str,
}

/// opcache's `zend_extension_entry` (`ext/opcache/ZendAccelerator.c`);
/// its version is `PHP_VERSION`.
const OPCACHE_ZEND_EXTENSION: ZendExtension = ZendExtension {
    name: "Zend OPcache",
    author: "Zend by Perforce",
    url: "https://www.zend.com/",
    copyright: "Copyright \u{a9}",
};

/// The loaded Zend extensions (`zend_extensions`), in load order: opcache
/// when its module is loaded.
pub(crate) fn zend_extensions(vm: &Vm) -> Vec<&'static ZendExtension> {
    let mut out = Vec::new();
    if vm
        .loaded_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(OPCACHE_ZEND_EXTENSION.name))
    {
        out.push(&OPCACHE_ZEND_EXTENSION);
    }
    out
}

fn zend_extension_named(vm: &Vm, name: &str) -> Option<&'static ZendExtension> {
    zend_extensions(vm)
        .into_iter()
        .find(|z| z.name.eq_ignore_ascii_case(name))
}

/// `ReflectionZendExtension::__construct` (`zend_get_extension`).
fn rze_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let n = str_arg(a, 0).to_string_lossy();
    let Some(z) = zend_extension_named(vm, &n) else {
        return Err(rexc(vm, &format!("Zend Extension \"{n}\" does not exist")));
    };
    let name = ZStr::from(z.name);
    set_name_props(&o, Some(&name), None);
    object::set_internal(&o, Refl::Extension(name));
    Ok(Value::Null)
}

fn this_zend_ext(vm: &mut Vm, a: &Args) -> VmResult<&'static ZendExtension> {
    let n = this_ext(vm, a)?;
    match zend_extension_named(vm, &n) {
        Some(z) => Ok(z),
        None => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn rze_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(zs(this_zend_ext(vm, a)?.name))
}

fn rze_get_version(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let _ = this_zend_ext(vm, a)?;
    Ok(zs(crate::PHP_VERSION))
}

fn rze_get_author(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(zs(this_zend_ext(vm, a)?.author))
}

fn rze_get_url(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(zs(this_zend_ext(vm, a)?.url))
}

fn rze_get_copyright(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(zs(this_zend_ext(vm, a)?.copyright))
}

/// `_zend_extension_string`.
fn rze_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let z = this_zend_ext(vm, a)?;
    Ok(zs(&format!(
        "Zend Extension [ {} {} {} by {} <{}> ]\n",
        z.name,
        crate::PHP_VERSION,
        z.copyright,
        z.author,
        z.url
    )))
}

/// Registers `ReflectionExtension` and `ReflectionZendExtension`.
pub(crate) fn register_extension(vm: &mut Vm) {
    ClassBuilder::new("ReflectionExtension")
        .implements(vm, &["Reflector"])
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .hooks(super::constant::hooks())
        .prop("name", RO, Some("string"), None)
        .method("__clone(): void", PRIV, clone_error)
        .method("__construct(string $name)", PUB, re_construct)
        .method("__toString(): string", PUB, re_to_string)
        .method("getName(): string", PUB, re_get_name)
        .method("getVersion(): ?string", PUB, re_get_version)
        .method("getFunctions(): array", PUB, re_get_functions)
        .method("getConstants(): array", PUB, re_get_constants)
        .method("getINIEntries(): array", PUB, re_get_ini_entries)
        .method("getClasses(): array", PUB, re_get_classes)
        .method("getClassNames(): array", PUB, re_get_class_names)
        .method("getDependencies(): array", PUB, re_get_dependencies)
        .method("info(): void", PUB, re_info)
        .method("isPersistent(): bool", PUB, re_is_persistent)
        .method("isTemporary(): bool", PUB, re_is_temporary)
        .register(vm);
    ClassBuilder::new("ReflectionZendExtension")
        .implements(vm, &["Reflector"])
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .hooks(super::constant::hooks())
        .prop("name", RO, Some("string"), None)
        .method("__clone(): void", PRIV, clone_error)
        .method("__construct(string $name)", PUB, rze_construct)
        .method("__toString(): string", PUB, rze_to_string)
        .method("getName(): string", PUB, rze_get_name)
        .method("getVersion(): string", PUB, rze_get_version)
        .method("getAuthor(): string", PUB, rze_get_author)
        .method("getURL(): string", PUB, rze_get_url)
        .method("getCopyright(): string", PUB, rze_get_copyright)
        .register(vm);
}

// ---------------------------------------------------------------------
// PropertyHookType
// ---------------------------------------------------------------------

/// Registers the internal backed enum `PropertyHookType`.
pub(crate) fn register_hook_type(vm: &mut Vm) {
    let name = ZStr::from("PropertyHookType");
    let ifaces: Vec<Rc<Class>> = ["UnitEnum", "BackedEnum"]
        .iter()
        .filter_map(|n| vm.find_class(n.as_bytes()))
        .collect();
    let cls = Rc::new_cyclic(|weak| {
        let mk_const = |n: &str, v: &str| {
            Rc::new(ClassConst {
                name: ZStr::from(n),
                value: RefCell::new(None),
                expr: ConstExpr::Value(zs(v)),
                flags: flags::PUBLIC,
                class: name.clone(),
                ty: None,
                is_case: true,
                evaluating: Cell::new(false),
                doc_comment: None,
                attributes: Vec::new(),
            })
        };
        let consts = vec![mk_const("Get", "get"), mk_const("Set", "set")];
        let const_map: NameMap<usize> = consts
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        let mk_prop = |n: &str, slot: u32| {
            Rc::new(PropInfo {
                name: ZStr::from(n),
                slot,
                flags: flags::PUBLIC | flags::READONLY,
                ty: Some(TypeDecl::simple(mask::STRING)),
                class: name.clone(),
                default: None,
                doc_comment: None,
                attributes: Vec::new(),
                hooks: None,
            })
        };
        let props = vec![mk_prop("name", 0), mk_prop("value", 1)];
        let prop_map: NameMap<Rc<PropInfo>> =
            props.iter().map(|p| (p.name.clone(), p.clone())).collect();
        let mut methods: NameMap<Rc<Method>> = NameMap::default();
        let mut order = Vec::new();
        for (sig, fl, h) in crate::builtins::enum_methods() {
            let s = crate::builtins::sig::parse_sig_borrowed(sig);
            let lc = ZStr::from(s.name.to_ascii_lowercase());
            let name = s.name;
            let b = Builtin::new(ZStr::from(name), s.into(), fl, h);
            order.push(lc.clone());
            methods.insert(
                lc,
                Rc::new(Method {
                    name: ZStr::from(name),
                    func: Func::Builtin(Rc::new(b)),
                    flags: fl,
                    scope: weak.clone(),
                    proto_scope: weak.clone(),
                }),
            );
        }
        for i in &ifaces {
            for lc in &i.method_order {
                if !methods.contains_key(lc) {
                    if let Some(m) = i.methods.get(lc) {
                        order.push(lc.clone());
                        methods.insert(lc.clone(), m.clone());
                    }
                }
            }
        }
        let magic: Magic = crate::classlink::build_magic(&methods);
        Class {
            name: name.clone(),
            lc_name: ZStr::from("propertyhooktype"),
            flags: cflags::INTERNAL | cflags::ENUM | cflags::FINAL | cflags::NO_NEW,
            parent: None,
            interfaces: ifaces.clone(),
            traits: Vec::new(),
            consts,
            const_map,
            props,
            prop_map,
            static_props: Vec::new(),
            static_map: NameMap::default(),
            methods,
            method_order: order,
            magic,
            defaults: RefCell::new(None),
            backing: Some(mask::STRING),
            enum_cases: RefCell::new(NameMap::default()),
            decl: None,
            internal: InternalKind::None,
            attributes: Vec::new(),
            filename: None,
            line_start: 0,
            line_end: 0,
            doc_comment: None,
            linked: Cell::new(true),
            hooks: None,
        }
    });
    vm.register_class(cls);
}
