// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Zend/zend_builtin_functions.c` and core runtime functions.

use super::util::{arr, b, i, opt, s, sv, value_error};
use super::{alias, constant, reg};
use crate::class::Class;
use crate::func::{flags, Args};
use crate::object;
use crate::vm::{ErrorHandler, FrameKind, PendingCall, Throw, Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, PhpRef, PropertyPurpose, Value, ZArray, ZStr};
use std::rc::Rc;

fn caller_idx(vm: &Vm) -> Option<usize> {
    let n = vm.frames.len();
    if n < 2 {
        return None;
    }
    Some(n - 2)
}

/// `zend_forbid_dynamic_call()` for a call without a previous frame (a
/// shutdown function or other `zend_call_function()` call from outside
/// any frame is `ZEND_CALL_DYNAMIC`).
#[cold]
fn called_dynamically(vm: &mut Vm, name: &str) -> Throw {
    vm.make_throw("Error", &format!("Cannot call {name}() dynamically"))
}

fn func_get_args(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let Some(idx) = caller_idx(vm) else {
        return Err(called_dynamically(vm, "func_get_args"));
    };
    if vm.frames[idx].kind != FrameKind::Function {
        return Err(vm.make_throw(
            "Error",
            "func_get_args() cannot be called from the global scope",
        ));
    }
    Ok(Value::Array(vm.frame_args(idx)))
}

fn func_num_args(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let Some(idx) = caller_idx(vm) else {
        return Err(called_dynamically(vm, "func_num_args"));
    };
    if vm.frames[idx].kind != FrameKind::Function {
        return Err(vm.make_throw(
            "Error",
            "func_num_args() must be called from a function context",
        ));
    }
    let f = &vm.frames[idx];
    let n = f.num_args();
    Ok(Value::Int(i64::try_from(n).unwrap_or(0)))
}

fn func_get_arg(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = i(a, 0);
    let Some(idx) = caller_idx(vm) else {
        if n < 0 {
            return Err(value_error(vm, a, 1, "must be greater than or equal to 0"));
        }
        return Err(called_dynamically(vm, "func_get_arg"));
    };
    if vm.frames[idx].kind != FrameKind::Function {
        return Err(vm.make_throw(
            "Error",
            "func_get_arg() cannot be called from the global scope",
        ));
    }
    if n < 0 {
        return Err(value_error(vm, a, 1, "must be greater than or equal to 0"));
    }
    let args = vm.frame_args(idx);
    match args.get(&ArrayKey::Int(n)) {
        Some(v) => Ok(v.clone()),
        None => Err(value_error(
            vm,
            a,
            1,
            "must be less than the number of the arguments passed to the currently executed function",
        )),
    }
}

#[allow(clippy::unnecessary_wraps)] // builtin signature
fn strlen(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::try_from(s(a, 0).len()).unwrap_or(0)))
}

/// `validate_constant_array_argument()` + `copy_constant_array()`:
/// references are dropped; `None` for a recursive array.
fn constant_array(a: &ZArray, path: &mut Vec<usize>) -> Option<ZArray> {
    if path.contains(&a.addr()) {
        return None;
    }
    path.push(a.addr());
    let mut t = HashTable::new();
    for (k, v) in a.iter() {
        let v = match v.deref() {
            Value::Array(inner) => Value::Array(constant_array(&inner, path)?),
            other => other,
        };
        t.insert(k.clone(), v);
    }
    path.pop();
    Some(ZArray::from_table(t))
}

fn define(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = s(a, 0);
    if a.has(2) {
        vm.deprecated("define(): Argument #3 ($case_insensitive) is ignored and treated as false since declaration of case-insensitive constants is no longer supported, passing the argument explicitly is unnecessary")?;
    }
    if crate::call::find_double_colon(name.as_bytes()).is_some() {
        return Err(value_error(vm, a, 1, "cannot be a class constant"));
    }
    if a.has(2) && b(a, 2) {
        vm.warning("define(): Argument #3 ($case_insensitive) is ignored since declaration of case-insensitive constants is no longer supported, this will be an error in PHP 9.0")?;
    }
    let v = match a.at(1) {
        Value::Array(arr) => match constant_array(&arr, &mut Vec::new()) {
            Some(c) => Value::Array(c),
            None => return Err(value_error(vm, a, 2, "cannot be a recursive array")),
        },
        other => other,
    };
    // `zend_register_constant()`: the namespace part is case-insensitive
    // in the key; the constant keeps the name as given.
    let given = name.clone();
    let bytes = name.as_bytes();
    let name = match bytes.iter().rposition(|&c| c == b'\\') {
        Some(p) => {
            let mut n = bytes[..p].to_ascii_lowercase();
            n.extend_from_slice(&bytes[p..]);
            ZStr::from(n)
        }
        None => name,
    };
    let special = name.as_bytes() == b"__COMPILER_HALT_OFFSET__"
        || [&b"true"[..], b"false", b"null"]
            .iter()
            .any(|k| name.as_bytes().eq_ignore_ascii_case(k));
    if special {
        vm.warning(&format!(
            "Constant {} already defined, this will be an error in PHP 9",
            name.to_string_lossy()
        ))?;
        return Ok(Value::Bool(false));
    }
    let ok = vm.declare_constant(name.clone(), v)?;
    if ok && name != given {
        vm.const_names.insert(name, given);
    }
    Ok(Value::Bool(ok))
}

/// The class part of `defined("C::X")` / `constant("C::X")`
/// (`zend_get_class_constant_ex()`): `self`, `parent` and `static` follow
/// the calling frame; `Ok(None)` is a missing class under `silent`
/// (`ZEND_FETCH_CLASS_SILENT`).
fn constant_class(vm: &mut Vm, name: &[u8], silent: bool) -> VmResult<Option<Rc<Class>>> {
    let name = name.strip_prefix(b"\\").unwrap_or(name);
    if name.eq_ignore_ascii_case(b"self") {
        return match vm.scope() {
            Some(s) => Ok(Some(s)),
            None => Err(vm.make_throw(
                "Error",
                "Cannot access \"self\" when no class scope is active",
            )),
        };
    }
    if name.eq_ignore_ascii_case(b"parent") {
        let Some(scope) = vm.scope() else {
            return Err(vm.make_throw(
                "Error",
                "Cannot access \"parent\" when no class scope is active",
            ));
        };
        return match scope.parent.clone() {
            Some(p) => Ok(Some(p)),
            None => Err(vm.make_throw(
                "Error",
                "Cannot access \"parent\" when current class scope has no parent",
            )),
        };
    }
    if name.eq_ignore_ascii_case(b"static") {
        let called = vm
            .frames
            .iter()
            .rev()
            .find(|f| f.kind != FrameKind::Builtin)
            .and_then(|f| match &f.this {
                Some(t) => Some(object::class_of(t).clone()),
                None => f.called.clone(),
            });
        return match called {
            Some(c) => Ok(Some(c)),
            None => Err(vm.make_throw(
                "Error",
                "Cannot access \"static\" when no class scope is active",
            )),
        };
    }
    let cls = ZStr::from(name);
    if silent {
        vm.lookup_class(&cls)
    } else {
        vm.lookup_class_or_error(&cls).map(Some)
    }
}

/// `zend_verify_const_access()` from the calling scope.
fn const_accessible(vm: &Vm, c: &crate::class::ClassConst) -> bool {
    match c.flags & flags::VISIBILITY {
        flags::PUBLIC => true,
        vis => match (vm.scope(), vm.find_class(c.class.as_bytes())) {
            (Some(s), Some(d)) if vis == flags::PRIVATE => Rc::ptr_eq(&s, &d),
            (Some(s), Some(d)) => s.is_subclass_of(&d) || d.is_subclass_of(&s),
            _ => false,
        },
    }
}

fn defined(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = s(a, 0);
    if let Some(p) = crate::call::find_double_colon(name.as_bytes()) {
        let cn = &name.as_bytes()[p + 2..];
        // Missing, inaccessible and trait constants are all "not defined".
        return Ok(Value::Bool(
            match constant_class(vm, &name.as_bytes()[..p], true)? {
                Some(c) => {
                    !c.is_trait() && c.find_const(cn).is_some_and(|k| const_accessible(vm, k))
                }
                None => false,
            },
        ));
    }
    let n = name
        .as_bytes()
        .strip_prefix(b"\\")
        .unwrap_or(name.as_bytes());
    Ok(Value::Bool(vm.constants.contains_key(n)))
}

fn constant_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = s(a, 0);
    if let Some(p) = crate::call::find_double_colon(name.as_bytes()) {
        let cb = &name.as_bytes()[..p];
        let cn = ZStr::from(&name.as_bytes()[p + 2..]);
        let Some(c) = constant_class(vm, cb, false)? else {
            return Err(vm.make_throw(
                "Error",
                &format!("Class \"{}\" not found", String::from_utf8_lossy(cb)),
            ));
        };
        // Trait constants are only reachable through a using class; the
        // undefined/visibility errors come first.
        if c.is_trait()
            && c.find_const(cn.as_bytes())
                .is_some_and(|k| const_accessible(vm, k))
        {
            return Err(vm.make_throw(
                "Error",
                &format!(
                    "Cannot access trait constant {}::{} directly",
                    String::from_utf8_lossy(cb.strip_prefix(b"\\").unwrap_or(cb)),
                    cn.to_string_lossy()
                ),
            ));
        }
        return vm.fetch_class_constant(&c, &cn);
    }
    let n = name
        .as_bytes()
        .strip_prefix(b"\\")
        .unwrap_or(name.as_bytes());
    match vm.constants.get(n).cloned() {
        Some(v) => {
            vm.constant_deprecation(&ZStr::from(n))?;
            Ok(v)
        }
        None => Err(vm.make_throw(
            "Error",
            &format!("Undefined constant \"{}\"", name.to_string_lossy()),
        )),
    }
}

fn get_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.get(0) {
        Some(Value::Object(o)) => Ok(Value::Str(object::class_of(&o).name.clone())),
        Some(_) => Ok(Value::Bool(false)),
        None => match vm
            .frames
            .get(vm.frames.len().wrapping_sub(2))
            .and_then(|f| f.scope.clone())
        {
            Some(c) => {
                vm.deprecated("Calling get_class() without arguments is deprecated")?;
                Ok(Value::Str(c.name.clone()))
            }
            None => Err(vm.make_throw(
                "Error",
                "get_class() without arguments must be called from within a class",
            )),
        },
    }
}

fn get_parent_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cls = match a.get(0) {
        Some(Value::Object(o)) => Some(object::class_of(&o).clone()),
        // `Z_PARAM_OBJ_OR_CLASS_NAME`.
        Some(v) => {
            let found = match &v {
                Value::Str(s) => vm.lookup_class(s)?,
                _ => None,
            };
            if found.is_none() {
                let given = match &v {
                    Value::Str(s) => format!("\"{}\"", s.to_string_lossy()),
                    other => crate::typecheck::given_type(other),
                };
                return Err(super::util::type_error(
                    vm,
                    a,
                    1,
                    &format!("must be an object or a valid class name, {given} given"),
                ));
            }
            found
        }
        None => {
            vm.deprecated("Calling get_parent_class() without arguments is deprecated")?;
            vm.frames
                .get(vm.frames.len().wrapping_sub(2))
                .and_then(|f| f.scope.clone())
        }
    };
    Ok(match cls.and_then(|c| c.parent.clone()) {
        Some(p) => Value::Str(p.name.clone()),
        None => Value::Bool(false),
    })
}

fn get_called_class(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let c = vm
        .frames
        .get(vm.frames.len().wrapping_sub(2))
        .and_then(|f| f.called.clone());
    match c {
        Some(c) => Ok(Value::Str(c.name.clone())),
        None => Err(vm.make_throw(
            "Error",
            "get_called_class() must be called from within a class",
        )),
    }
}

fn class_arg(vm: &mut Vm, v: &Value, autoload: bool) -> VmResult<Option<Rc<Class>>> {
    match v {
        Value::Object(o) => Ok(Some(object::class_of(o).clone())),
        Value::Str(s) => {
            if autoload {
                vm.lookup_class(s)
            } else {
                Ok(vm.find_class(s.as_bytes()))
            }
        }
        _ => Ok(None),
    }
}

/// `get_mangled_object_vars()`: the raw property table (mangled names, no
/// scope rules, no lazy initialization) as a symbol table.
fn get_mangled_object_vars(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Value::Object(o) = a.at(0) else {
        return Ok(Value::Array(ZArray::new()));
    };
    let mut t = HashTable::new();
    for (k, v, _) in vm.object_prop_table(&o, false) {
        let Some(v) = v else { continue };
        let k = match k {
            ArrayKey::Str(s) => match php_runtime::numeric::handle_numeric_str(s.as_bytes()) {
                Some(i) => ArrayKey::Int(i),
                None => ArrayKey::Str(s),
            },
            k => k,
        };
        let v = match v {
            Value::Reference(r) if r.refcount() == 1 => r.get(),
            other => other,
        };
        t.insert(k, v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

/// `zend_check_property_access()` for a declared instance property of
/// `cls` seen from `scope`.
fn property_visible(
    vm: &Vm,
    cls: &Rc<Class>,
    pi: &crate::class::PropInfo,
    scope: Option<&Rc<Class>>,
) -> bool {
    match vm.lookup_prop(cls, &pi.name, scope) {
        crate::props::PropLookup::Found(found) if found.slot == pi.slot => true,
        // A parent's private property from the parent's scope.
        _ => {
            pi.visibility() == flags::PRIVATE
                && scope.is_some_and(|s| s.name.eq_ignore_ascii_case(pi.class.as_bytes()))
        }
    }
}

fn get_object_vars(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Value::Object(o) = a.at(0) else {
        return Ok(Value::Null);
    };
    let o = vm.lazy_object_for(&o, None)?;
    if let Some(f) = object::class_of(&o)
        .hooks
        .as_ref()
        .and_then(|h| h.properties_for)
    {
        if let Some(properties) = f(vm, &o, PropertyPurpose::GetObjectVars)? {
            let scope = vm.scope();
            let cls = object::class_of(&o).clone();
            let mut t = HashTable::new();
            for (key, value) in properties.iter() {
                let mut value = value.clone();
                let key = match key {
                    ArrayKey::Str(key) => {
                        let (declaring, name) =
                            php_runtime::dump::unmangle_property_name(key.as_bytes());
                        let name = ZStr::from(name);
                        let declared = if declaring.is_some() {
                            // A mangled key names one declared property:
                            // `zend_check_property_access()` on that slot.
                            let Some(pi) = cls
                                .props
                                .iter()
                                .find(|pi| crate::props::mangle(pi) == ArrayKey::Str(key.clone()))
                                .cloned()
                            else {
                                let key = ArrayKey::from_zstr(key);
                                if !t.contains_key(&key) {
                                    t.insert(key, value);
                                }
                                continue;
                            };
                            if !property_visible(vm, &cls, &pi, scope.as_ref()) {
                                continue;
                            }
                            Some(pi)
                        } else {
                            match vm.lookup_prop(&cls, &name, scope.as_ref()) {
                                crate::props::PropLookup::Found(pi) => {
                                    if pi.visibility() != flags::PUBLIC {
                                        continue;
                                    }
                                    Some(pi)
                                }
                                crate::props::PropLookup::Inaccessible(_) => continue,
                                crate::props::PropLookup::NotFound => None,
                            }
                        };
                        if let Some(pi) = declared {
                            if let Some(h) = &pi.hooks {
                                if pi.is_virtual() && h.get.is_none() {
                                    continue;
                                }
                                value =
                                    vm.read_property(&o, &name, crate::opcode::FetchMode::Is)?;
                            }
                        }
                        if declaring.is_some() {
                            ArrayKey::Str(name)
                        } else {
                            ArrayKey::from_zstr(key)
                        }
                    }
                    key @ ArrayKey::Int(_) => key.clone(),
                };
                if !t.contains_key(&key) {
                    t.insert(key, value);
                }
            }
            return Ok(Value::Array(ZArray::from_table(t)));
        }
    }
    if let Some(f) = object::class_of(&o)
        .hooks
        .as_ref()
        .and_then(|h| h.get_properties)
    {
        if let Some(t) = f(vm, &o)? {
            return Ok(Value::Array(t));
        }
    }
    // Like php-src: the raw property table filtered by
    // `zend_check_property_access()`, except hooked properties, which are
    // re-read through their get hook (`zend_read_property_ex`); virtual
    // hooked properties without a get hook are skipped. No `__get`.
    let scope = vm.scope();
    let cls = object::class_of(&o).clone();
    let mut t = object::PropTableBuilder::new(object::data(&o));
    crate::lazy::mark_props_built(object::data(&o));
    let stale = crate::lazy::stale_slots(object::data(&o));
    for pi in cls.props.clone() {
        let name = pi.name.clone();
        if !property_visible(vm, &cls, &pi, scope.as_ref()) {
            continue;
        }
        let v = if let Some(h) = &pi.hooks {
            if pi.is_virtual() && h.get.is_none() {
                continue;
            }
            vm.read_property(&o, &name, crate::opcode::FetchMode::Is)?
        } else if let Some(frozen) = &stale {
            // Frozen copies in the properties table (`lazy::stale_slots`).
            match frozen.get(pi.slot as usize).cloned().flatten() {
                Some(v) => v,
                None => continue,
            }
        } else {
            let d = object::data(&o);
            let slot = d
                .slots
                .borrow()
                .get(pi.slot as usize)
                .and_then(|s| s.as_ref().map(unref_singly_held));
            match slot {
                Some(v) => v,
                None => continue, // uninitialized typed property
            }
        };
        t.push(Some(pi.slot), ArrayKey::from_zstr(&name), v);
    }
    if let Some(dyn_t) = object::data(&o).dynamic.borrow().as_ref() {
        let declared = !cls.props.is_empty();
        for (k, v) in dyn_t.iter() {
            let key = match k {
                ArrayKey::Str(s) => {
                    // A dynamic property shadowed by a declared one the
                    // scope sees (`foreach_shadowed_dyn_property`).
                    if declared && !dynamic_property_visible(vm, &cls, s, scope.as_ref()) {
                        continue;
                    }
                    ArrayKey::from_zstr(s)
                }
                k => k.clone(),
            };
            t.push_renamed(None, k, key, unref_singly_held(v));
        }
    }
    let t = t.finish();
    Ok(Value::Array(ZArray::from_table(t)))
}

/// `get_object_vars()` copies a reference nothing else holds as its value
/// (`Z_ISREF_P(value) && Z_REFCOUNT_P(value) == 1`), e.g. the one left by
/// passing the property by reference.
fn unref_singly_held(v: &Value) -> Value {
    match v {
        Value::Reference(r) if r.refcount() == 1 => r.get(),
        v => v.clone(),
    }
}

/// `zend_check_property_access(zobj, key, is_dynamic = 1)`: a dynamic
/// property is hidden when `zend_get_property_info()` resolves its name to
/// a declared non-public property (or denies access to one); a parent's
/// private property seen from outside its class counts as dynamic.
fn dynamic_property_visible(
    vm: &Vm,
    cls: &Rc<Class>,
    name: &ZStr,
    scope: Option<&Rc<Class>>,
) -> bool {
    if name.as_bytes().first() == Some(&0) {
        return true;
    }
    match vm.lookup_prop(cls, name, scope) {
        crate::props::PropLookup::NotFound => true,
        crate::props::PropLookup::Found(pi) => pi.visibility() == flags::PUBLIC,
        crate::props::PropLookup::Inaccessible(pi) => {
            pi.visibility() == flags::PRIVATE && !pi.class.eq_ignore_ascii_case(cls.name.as_bytes())
        }
    }
}

/// A `properties_info` entry: (name, declaring class (`prop_info->ce`),
/// flags, instance slot or `None` for a static property).
type InfoEntry = (ZStr, ZStr, u32, Option<u32>);

/// The entry `c` itself has for `name`: a property it declares, or one a
/// trait bound into it (declared on `c` as well).
fn own_info(c: &Class, name: &[u8]) -> Option<InfoEntry> {
    let mine = |decl: &ZStr| decl.eq_ignore_ascii_case(&c.name);
    if let Some(p) = c
        .props
        .iter()
        .find(|p| p.name.as_bytes() == name && mine(&p.class))
    {
        return Some((p.name.clone(), p.class.clone(), p.flags, Some(p.slot)));
    }
    c.static_props
        .iter()
        .find(|sp| sp.name.as_bytes() == name && mine(&sp.class))
        .map(|sp| (sp.name.clone(), sp.class.clone(), sp.flags, None))
}

/// `ce->properties_info` in php-src order. A user class declares its own
/// properties at compile time in declaration order (an enum's `name` and
/// `value` first, promoted constructor parameters where the constructor
/// is); linking then appends the parent's entries it does not redeclare
/// (`zend_do_inheritance`, inherited private ones included) and the
/// properties of its traits (`zend_do_traits_property_binding`; one the
/// parent already has keeps the parent's entry). An internal class
/// inherits first and declares its own properties afterwards
/// (`zend_register_internal_class_ex`; a redeclared name is deleted and
/// added again by `zend_declare_typed_property`).
fn properties_info(c: &Rc<Class>, out: &mut Vec<InfoEntry>) {
    fn push(out: &mut Vec<InfoEntry>, e: InfoEntry) {
        if !out.iter().any(|x| x.0 == e.0) {
            out.push(e);
        }
    }
    let user = c.decl.as_deref();
    if let Some(decl) = user {
        if c.is_enum() {
            for name in [b"name".as_slice(), b"value".as_slice()] {
                if let Some(e) = own_info(c, name) {
                    push(out, e);
                }
            }
        }
        for p in &decl.props {
            if let Some(e) = own_info(c, p.name.as_bytes()) {
                push(out, e);
            }
        }
    }
    if let Some(parent) = &c.parent {
        let mut inherited = Vec::new();
        properties_info(parent, &mut inherited);
        for e in inherited {
            if user.is_none() && own_info(c, e.0.as_bytes()).is_some() {
                continue;
            }
            push(out, e);
        }
    }
    if user.is_some() {
        for t in &c.traits {
            let mut bound = Vec::new();
            properties_info(t, &mut bound);
            for (name, ..) in bound {
                if let Some(e) = own_info(c, name.as_bytes()) {
                    push(out, e);
                }
            }
        }
    }
    // The rest of what `c` declares (all of an internal class's own).
    let mine = |decl: &ZStr| decl.eq_ignore_ascii_case(&c.name);
    for p in c.props.iter().filter(|p| mine(&p.class)) {
        push(
            out,
            (p.name.clone(), p.class.clone(), p.flags, Some(p.slot)),
        );
    }
    for sp in c.static_props.iter().filter(|sp| mine(&sp.class)) {
        push(out, (sp.name.clone(), sp.class.clone(), sp.flags, None));
    }
}

/// `get_class_vars()` (`add_class_vars` twice: instance properties, then
/// static ones), in `properties_info` order, filtered by the caller's
/// scope.
fn get_class_vars(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = s(a, 0);
    let Some(cls) = vm.lookup_class(&name)? else {
        return Err(super::util::type_error(
            vm,
            a,
            1,
            &format!(
                "must be a valid class name, \"{}\" given",
                name.to_string_lossy()
            ),
        ));
    };
    let scope = vm
        .frames
        .get(vm.frames.len().wrapping_sub(2))
        .and_then(|f| f.scope.clone());
    // `zend_update_class_constants`.
    let defaults = vm.class_defaults(&cls)?;
    let mut info = Vec::new();
    properties_info(&cls, &mut info);
    let visible = |decl: &ZStr, fl: u32| -> bool {
        if fl & flags::VIRTUAL != 0 {
            return false;
        }
        let decl_cls = vm.find_class(decl.as_bytes());
        if fl & flags::PRIVATE != 0 {
            return scope
                .as_ref()
                .is_some_and(|s| s.name.eq_ignore_ascii_case(decl));
        }
        if fl & flags::PROTECTED != 0 {
            let (Some(s), Some(d)) = (scope.as_ref(), decl_cls.as_ref()) else {
                return false;
            };
            return s.is_subclass_of(d) || d.is_subclass_of(s);
        }
        true
    };
    let shown: Vec<InfoEntry> = info
        .into_iter()
        .filter(|(_, d, fl, _)| visible(d, *fl))
        .collect();
    let mut t = HashTable::new();
    for (n, _, _, slot) in &shown {
        if let Some(slot) = slot {
            // Uninitialized typed properties are reported as null.
            let v = defaults
                .get(*slot as usize)
                .cloned()
                .flatten()
                .unwrap_or_default();
            t.insert(ArrayKey::Str(n.clone()), v);
        }
    }
    // Static properties report their declared defaults
    // (`ce->default_static_members_table`), not their current values; a
    // constant expression is evaluated on a copy, in the declaring class
    // (where `ConstExpr` code runs).
    for (n, decl, _, slot) in &shown {
        if slot.is_some() {
            continue;
        }
        let Some(dc) = vm.find_class(decl.as_bytes()) else {
            continue;
        };
        let default = dc
            .static_props
            .iter()
            .find(|sp| sp.name == *n && sp.class.eq_ignore_ascii_case(decl))
            .and_then(|sp| sp.default.clone());
        let v = match default {
            Some(e) => vm.eval_const_expr(&e, Some(&dc))?,
            None => Value::Null,
        };
        t.insert(ArrayKey::Str(n.clone()), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn get_class_methods(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(cls) = class_arg(vm, &a.at(0), true)? else {
        // `zend_wrong_parameter_type_error` quotes a string argument.
        let given = match a.at(0) {
            Value::Str(s) => format!("\"{}\"", s.to_string_lossy()),
            other => crate::typecheck::given_type(&other),
        };
        return Err(super::util::type_error(
            vm,
            a,
            1,
            &format!("must be an object or a valid class name, {given} given"),
        ));
    };
    let scope = vm
        .frames
        .get(vm.frames.len().wrapping_sub(2))
        .and_then(|f| f.scope.clone());
    let mut out = HashTable::new();
    // `Closure::__invoke` is not in the method table (GH-17126).
    if cls.lc_name.as_bytes() == b"closure" && cls.find_method(b"__invoke").is_none() {
        let _ = out.push(sv("__invoke"));
    }
    for lc in &cls.method_order {
        let m = &cls.methods[lc];
        if vm.can_access_method(m, scope.as_ref()) {
            let _ = out.push(Value::Str(m.name.clone()));
        }
    }
    Ok(Value::Array(ZArray::from_table(out)))
}

fn method_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let cls = match &v {
        Value::Object(o) => Some(object::class_of(o).clone()),
        Value::Str(s) => vm.lookup_class(s)?,
        _ => {
            return Err(super::util::type_error(
                vm,
                a,
                1,
                &format!(
                    "must be of type object|string, {} given",
                    crate::typecheck::given_type(&v)
                ),
            ))
        }
    };
    let m = s(a, 1);
    let Some(cls) = cls else {
        return Ok(Value::Bool(false));
    };
    if let Some(f) = cls.find_method(m.as_bytes()) {
        // `ZEND_FUNCTION(method_exists)`: a private method inherited from a
        // parent is a shadow when checking a class name.
        return Ok(Value::Bool(
            matches!(v, Value::Object(_))
                || f.flags & flags::PRIVATE == 0
                || std::ptr::eq(f.scope.as_ptr(), Rc::as_ptr(&cls)),
        ));
    }
    // `zend_get_closure_invoke_method`: Closure::__invoke exists for the
    // class and its instances although it is not in the method table.
    if cls.lc_name.as_bytes() == b"closure" && m.eq_ignore_ascii_case(b"__invoke") {
        return Ok(Value::Bool(true));
    }
    if let Value::Object(o) = &v {
        // `get_method` of the object: methods outside the class table
        // (PDO's driver methods) exist for it.
        if vm.extra_method(o, &cls, &m).is_some() {
            return Ok(Value::Bool(true));
        }
        return Ok(Value::Bool(forwarded_method_exists(vm, o, &m)?));
    }
    Ok(Value::Bool(false))
}

/// `method_exists()` on an object asks its `get_method` handler, which for
/// `IteratorIterator` & co. (`spl_dual_it_get_method`) finds the inner
/// iterator's real methods (its `__call` trampoline does not count).
#[cold]
fn forwarded_method_exists(vm: &mut Vm, o: &php_runtime::Obj, m: &ZStr) -> VmResult<bool> {
    let mut cur = o.clone();
    // Bounded: a chain of wrappers cannot loop, but stay defensive.
    for _ in 0..64 {
        let Some(forward) = object::class_of(&cur)
            .hooks
            .as_ref()
            .and_then(|h| h.forward_methods_to)
        else {
            return Ok(false);
        };
        let Some(inner) = forward(vm, &cur)? else {
            return Ok(false);
        };
        if object::class_of(&inner).find_method(m.as_bytes()).is_some() {
            return Ok(true);
        }
        cur = inner;
    }
    Ok(false)
}

fn property_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let p = s(a, 1);
    let cls = match &v {
        Value::Object(o) => object::class_of(o).clone(),
        Value::Str(s) => match vm.lookup_class(s)? {
            Some(c) => c,
            None => return Ok(Value::Bool(false)),
        },
        _ => {
            return Err(super::util::type_error(
                vm,
                a,
                1,
                &format!(
                    "must be of type object|string, {} given",
                    crate::typecheck::given_type(&v)
                ),
            ))
        }
    };
    if cls.prop_map.contains_key(&p)
        || cls.static_map.contains_key(&p)
        || cls.props.iter().any(|x| {
            // `property_exists()`: an inherited private property is not
            // the class's own (`property_info->ce == ce`).
            x.name == p
                && (x.visibility() != crate::func::flags::PRIVATE
                    || x.class.as_bytes().eq_ignore_ascii_case(cls.name.as_bytes()))
        })
    {
        return Ok(Value::Bool(true));
    }
    if let Value::Object(o) = &v {
        // `has_property(ZEND_PROPERTY_EXISTS)` of native classes.
        if let Some(f) = object::class_of(o)
            .hooks
            .as_ref()
            .and_then(|h| h.has_property)
        {
            let saved = std::mem::replace(&mut vm.prop_ctx, crate::ext::PropCtx::Exists);
            let r = f(vm, o, &p, false);
            vm.prop_ctx = saved;
            if let Some(r) = r? {
                return Ok(Value::Bool(r));
            }
        }
        // `zend_std_has_property(ZEND_PROPERTY_EXISTS)` from the calling
        // scope: an ancestor's private property counts when called from
        // that ancestor and its slot is initialized (bug50810).
        if let Some(scope) = vm.scope() {
            let data = object::data(o);
            let found = cls.props.iter().any(|x| {
                x.name == p
                    && x.visibility() == crate::func::flags::PRIVATE
                    && x.class
                        .as_bytes()
                        .eq_ignore_ascii_case(scope.name.as_bytes())
                    && data
                        .slots
                        .borrow()
                        .get(x.slot as usize)
                        .is_some_and(Option::is_some)
            });
            if found {
                return Ok(Value::Bool(true));
            }
        }
        let d = object::data(o).dynamic.borrow();
        if d.as_ref()
            .is_some_and(|t| t.contains_key(&ArrayKey::Str(p.clone())))
        {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn class_exists_kind(vm: &mut Vm, a: &Args, pred: fn(&Class) -> bool) -> VmResult<Value> {
    let name = s(a, 0);
    let autoload = !a.has(1) || b(a, 1);
    let c = if autoload {
        vm.lookup_class(&name)?
    } else {
        vm.find_class(name.as_bytes())
    };
    Ok(Value::Bool(c.is_some_and(|c| pred(&c))))
}

fn class_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // Enums are classes (`ZEND_ACC_INTERFACE | ZEND_ACC_TRAIT` excluded).
    class_exists_kind(vm, a, |c| !c.is_interface() && !c.is_trait())
}
fn interface_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    class_exists_kind(vm, a, Class::is_interface)
}
fn trait_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    class_exists_kind(vm, a, Class::is_trait)
}
fn enum_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    class_exists_kind(vm, a, Class::is_enum)
}

fn function_exists(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = s(a, 0);
    let n = n.as_bytes().strip_prefix(b"\\").unwrap_or(n.as_bytes());
    Ok(Value::Bool(
        vm.functions.contains_key(n.to_ascii_lowercase().as_slice()),
    ))
}

fn is_a_impl(vm: &mut Vm, a: &Args, subclass_only: bool) -> VmResult<Value> {
    let v = a.at(0);
    let allow_string = a.has(2) && b(a, 2) || (subclass_only && !a.has(2));
    let cls = match &v {
        Value::Object(o) => object::class_of(o).clone(),
        Value::Str(s) if allow_string => match vm.lookup_class(s)? {
            Some(c) => c,
            None => return Ok(Value::Bool(false)),
        },
        // `is_a_impl` (`Zend/zend_builtin_functions.c`).
        Value::Str(_) => {
            vm.deprecated(if subclass_only {
                "Calling is_subclass_of() with a string when $allow_string is false"
            } else {
                "Calling is_a() with a string when $allow_string is false"
            })?;
            return Ok(Value::Bool(false));
        }
        _ => return Ok(Value::Bool(false)),
    };
    let target = s(a, 1);
    let Some(t) = vm.find_class(target.as_bytes()) else {
        return Ok(Value::Bool(false));
    };
    if subclass_only && Rc::ptr_eq(&cls, &t) {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(cls.is_subclass_of(&t)))
}

fn is_a(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    is_a_impl(vm, a, false)
}
fn is_subclass_of(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    is_a_impl(vm, a, true)
}

fn spl_autoload_register(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if let Some(cb) = opt(a, 0) {
        if !vm.is_callable_value(&cb) {
            return Err(super::util::type_error(
                vm,
                a,
                1,
                "must be a valid callback or null, no array or string given",
            ));
        }
        let prepend = b(a, 2);
        let fcc = vm.zend_is_callable(&cb, true)?.ok();
        let loader = crate::vm::ClassLoader { callable: cb, fcc };
        if !vm.autoloaders.iter().any(|x| x.is_same(&loader)) {
            if prepend {
                vm.autoloaders.insert(0, loader);
            } else {
                vm.autoloaders.push(loader);
            }
        }
    }
    Ok(Value::Bool(true))
}

fn spl_autoload_unregister(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let before = vm.autoloaders.len();
    vm.autoloaders
        .retain(|x| !php_runtime::ops::is_identical(&x.callable, &cb).unwrap_or(false));
    Ok(Value::Bool(vm.autoloaders.len() != before))
}

fn spl_autoload_functions(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Array(ZArray::from_values(
        vm.autoloaders.iter().map(|l| l.callable.clone()),
    )))
}

fn spl_autoload_call(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = s(a, 0);
    vm.lookup_class(&n)?;
    Ok(Value::Null)
}

fn spl_object_id(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.at(0) {
        Value::Object(o) => Ok(Value::Int(i64::from(o.handle()))),
        _ => Ok(Value::Int(0)),
    }
}

fn spl_object_hash(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.at(0) {
        Value::Object(o) => Ok(sv(format!("{:032x}", o.handle()))),
        _ => Ok(sv("")),
    }
}

fn set_error_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    if !cb.is_null() && !vm.is_callable_value(&cb) {
        let desc = vm.callable_error(&cb);
        return Err(super::util::type_error(
            vm,
            a,
            1,
            &format!("must be a valid callback or null, {desc}"),
        ));
    }
    let mask = if a.has(1) { i(a, 1) } else { 30719 };
    let old = vm
        .error_handlers
        .last()
        .map(|h| h.callback.clone())
        .unwrap_or_default();
    vm.error_handlers.push(ErrorHandler { callback: cb, mask });
    Ok(old)
}

fn restore_error_handler(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.error_handlers.pop();
    Ok(Value::Bool(true))
}

/// `get_error_handler()` (`Zend/zend_builtin_functions.c`): the current
/// `EG(user_error_handler)`, or null.
#[allow(clippy::unnecessary_wraps)] // builtin handler signature
fn get_error_handler(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(vm
        .error_handlers
        .last()
        .map(|h| h.callback.clone())
        .unwrap_or_default())
}

fn set_exception_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    if !cb.is_null() && !vm.is_callable_value(&cb) {
        let desc = vm.callable_error(&cb);
        return Err(super::util::type_error(
            vm,
            a,
            1,
            &format!("must be a valid callback or null, {desc}"),
        ));
    }
    let old = vm.exception_handlers.last().cloned().unwrap_or_default();
    vm.exception_handlers.push(cb);
    Ok(old)
}

fn restore_exception_handler(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.exception_handlers.pop();
    Ok(Value::Bool(true))
}

/// `get_exception_handler()` (`Zend/zend_builtin_functions.c)`: the active
/// user exception handler, or null.
#[allow(clippy::unnecessary_wraps)] // builtin handler signature
fn get_exception_handler(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(vm.exception_handlers.last().cloned().unwrap_or_default())
}

fn trigger_error(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let msg = s(a, 0);
    let level = if a.has(1) { i(a, 1) } else { 1024 };
    match level {
        256 | 512 | 1024 | 16384 => {}
        _ => {
            return Err(value_error(
                vm,
                a,
                2,
                "must be one of E_USER_ERROR, E_USER_WARNING, E_USER_NOTICE, or E_USER_DEPRECATED",
            ))
        }
    }
    if level == 256 {
        vm.deprecated("Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead")?;
    }
    vm.error_bytes(level, msg.as_bytes())?;
    Ok(Value::Bool(true))
}

fn error_reporting(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let old = vm.error_reporting;
    if let Some(v) = opt(a, 0) {
        let n = vm.to_long(&v)?;
        vm.error_reporting = n;
        let mut sink = Vec::new();
        vm.ini.alter(
            "error_reporting",
            n.to_string().as_bytes(),
            php_support::ini::Access::USER,
            php_support::ini::Stage::Runtime,
            true,
            &mut sink,
        );
    }
    Ok(Value::Int(old))
}

fn error_get_last(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let Some(e) = vm.last_error.clone() else {
        return Ok(Value::Null);
    };
    let mut t = HashTable::new();
    t.insert(ArrayKey::Str(ZStr::from("type")), Value::Int(e.level));
    t.insert(ArrayKey::Str(ZStr::from("message")), sv(e.message));
    t.insert(ArrayKey::Str(ZStr::from("file")), Value::Str(e.file));
    t.insert(
        ArrayKey::Str(ZStr::from("line")),
        Value::Int(i64::from(e.line)),
    );
    // `EG(last_fatal_error_backtrace)` (`fatal_error_backtraces`).
    if let Some(trace) = &vm.last_fatal_error_backtrace {
        t.insert(
            ArrayKey::Str(ZStr::from("trace")),
            Value::Array(trace.clone()),
        );
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn error_clear_last(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.last_error = None;
    vm.last_fatal_error_backtrace = None;
    Ok(Value::Null)
}

fn ini_get(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = s(a, 0);
    let name = n.to_string_lossy();
    Ok(match vm.ini.get(&name) {
        Some(v) => ini_value_str(v),
        None => Value::Bool(false),
    })
}

/// Longest INI value [`ini_get`] hands out as a shared interned string.
const INI_SHARED_MAX: usize = 4096;

/// `ZVAL_SET_INI_STR`: php-src's `ini_get()` never allocates. It returns
/// the entry's own string: an interned one for a startup value (and for
/// one set from a literal), else a refcounted copy of the string
/// `zend_alter_ini_entry` stored, which was counted when it was set. The
/// registry here keeps the bytes (counted when set), so the string handed
/// out is a permanent interned copy made outside the counted heap: reading
/// a directive twice, or dropping a value read before it changed, moves
/// `memory_get_usage()` by nothing (Symfony's
/// `testCreatingManyInstancesDoesNotLeakMemory` keeps
/// `ini_get('session.save_handler')`, "files" then "user"). Very long
/// values get an ordinary string, so that the permanent table stays small.
fn ini_value_str(v: &[u8]) -> Value {
    if v.len() > INI_SHARED_MAX {
        return sv(v.to_vec());
    }
    Value::Str(php_memlimit::persistent(|| ZStr::intern_permanent(v)))
}

fn ini_set(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = s(a, 0).to_string_lossy();
    let v = a.at(1);
    // `Z_PARAM_ZVAL(new_value)` + `Z_TYPE_P(new_value) > IS_STRING`.
    if matches!(
        v,
        Value::Array(_) | Value::Object(_) | Value::Resource(_) | Value::Reference(_)
    ) {
        return Err(super::util::type_error(
            vm,
            a,
            2,
            "must be of type string|int|float|bool|null",
        ));
    }
    let val: Vec<u8> = match &v {
        Value::Bool(b) => {
            if *b {
                b"1".to_vec()
            } else {
                b"".to_vec()
            }
        }
        Value::Null => Vec::new(),
        other => vm.to_zstr(other)?.as_bytes().to_vec(),
    };
    let mut sink = Vec::new();
    if n == "memory_limit" || n == "max_memory_limit" {
        vm.ini.memory_usage = u64::try_from(php_memlimit::real_usage()).unwrap_or(u64::MAX);
    }
    if let Some(h) = vm.ini_modify_hooks.get(n.as_str()).copied() {
        vm.ini_stage = crate::ext::IniStage::Runtime;
        if vm.ini.get(&n).is_some() && !h(vm, &val)? {
            return Ok(Value::Bool(false));
        }
    }
    let r = vm.ini.ini_set(&n, &val, &mut sink);
    for d in sink {
        let lvl = i64::from(d.level.0);
        if d.docref {
            vm.docref(lvl, &d.message)?;
        } else {
            vm.error(lvl, &d.message)?;
        }
    }
    vm.sync_ini(&n);
    Ok(match r {
        Some(old) => sv(old),
        None => Value::Bool(false),
    })
}

fn ini_restore(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = s(a, 0).to_string_lossy();
    let mut sink = Vec::new();
    if n == "memory_limit" || n == "max_memory_limit" {
        vm.ini.memory_usage = u64::try_from(php_memlimit::real_usage()).unwrap_or(u64::MAX);
    }
    // `zend_restore_ini_entry_cb`: the handler sees the original value at
    // the runtime stage before it is restored; if it refuses (opcache.enable
    // once disabled), the directive keeps its value.
    if let Some(h) = vm.ini_modify_hooks.get(n.as_str()).copied() {
        if let Some(orig) = vm.ini.runtime_restore_value(&n) {
            vm.ini_stage = crate::ext::IniStage::Runtime;
            if !h(vm, &orig)? {
                return Ok(Value::Null);
            }
        }
    }
    vm.ini.ini_restore(&n, &mut sink);
    vm.sync_ini(&n);
    Ok(Value::Null)
}

fn set_include_path(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = s(a, 0);
    let mut sink = Vec::new();
    let old = vm.ini.ini_set("include_path", v.as_bytes(), &mut sink);
    Ok(match old {
        Some(o) => sv(o),
        None => Value::Bool(false),
    })
}

fn get_include_path(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(vm
        .ini
        .ini_get("include_path")
        .map_or(Value::Bool(false), sv))
}

fn debug_backtrace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let opts = if a.has(0) { i(a, 0) } else { 1 };
    let limit = usize::try_from(i(a, 1).max(0)).unwrap_or(0);
    let t = vm.backtrace(true, opts & 2 != 0, opts & 1 != 0, limit);
    Ok(Value::Array(t))
}

fn debug_print_backtrace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let opts = i(a, 0);
    let limit = usize::try_from(i(a, 1).max(0)).unwrap_or(0);
    let t = vm.backtrace(true, opts & 2 != 0, false, limit);
    let s = vm.trace_to_string(&t, false);
    vm.echo(s.as_bytes());
    Ok(Value::Null)
}

/// `PHP_FUNCTION(assert)` (`ext/standard/assert.c`).
fn assert_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // `!ASSERTG(active) || EG(assertions) <= 0`: only dynamic calls reach
    // here with assertions disabled (compiled calls skip the call).
    if !vm.ini.get_bool("assert.active") || vm.ini.get_long("zend.assertions") <= 0 {
        return Ok(Value::Bool(true));
    }
    let v = a.at(0);
    if vm.to_bool(&v)? {
        return Ok(Value::Bool(true));
    }
    let desc = a.get(1).unwrap_or(Value::Null);
    if let Value::Object(o) = &desc {
        if vm.instance_of(o, b"throwable") {
            return Err(Throw::Exception(o.clone()));
        }
    }
    let msg = if desc.is_null() {
        None
    } else {
        Some(vm.to_zstr(&desc)?)
    };
    assert_failed(vm, msg)
}

/// The failure path of `assert()`: the `ASSERT_CALLBACK` call, then the
/// `AssertionError` / warning and `assert.bail` handling.
#[cold]
fn assert_failed(vm: &mut Vm, msg: Option<ZStr>) -> VmResult<Value> {
    // `ASSERTG(callback)`, else the startup `assert.callback` (`ASSERTG(cb)`).
    let callback = vm.ext.assert_callback.clone().or_else(|| {
        vm.ini
            .config_value("assert.callback")
            .filter(|c| !c.is_empty())
            .map(sv)
    });
    // An exception the callback left pending (`EG(exception)`).
    let mut pending: Option<php_runtime::Obj> = None;
    if let Some(cb) = callback {
        let (file, line) = vm.current_location();
        let mut args = vec![Value::Str(file), Value::Int(i64::from(line)), Value::Null];
        if let Some(m) = &msg {
            args.push(Value::Str(m.clone()));
        }
        // `call_user_function()` → `zend_call_function()`.
        let r = match vm.zend_is_callable(&cb, false)? {
            Ok(mut call) => {
                call.args = args.into_iter().map(Some).collect();
                vm.call_pending(call)
            }
            Err(err) => {
                let name = vm.callable_name(&cb);
                Err(vm.make_throw("Error", &format!("Invalid callback {name}, {err}")))
            }
        };
        match r {
            Ok(_) => {}
            Err(Throw::Exception(e)) => pending = Some(e),
            Err(t) => return Err(t),
        }
    }
    let bail = vm.ini.get_bool("assert.bail");
    if vm.ini.get_bool("assert.exception") {
        let text = msg.map(|m| m.to_string_lossy()).unwrap_or_default();
        // `zend_throw_exception(assertion_error_ce, ..., E_ERROR)`.
        let e = vm.create_exception("AssertionError", &text, crate::errors::level::ERROR)?;
        if let Some(prev) = &pending {
            crate::exceptions::chain_previous(&e, prev);
        }
        if bail {
            // `zend_exception_error(EG(exception), E_ERROR)`: not catchable.
            vm.report_uncaught(&e);
            return Err(Throw::Fatal);
        }
        return Err(Throw::Exception(e));
    }
    if vm.ini.get_bool("assert.warning") {
        let m = msg.map_or_else(|| "Assertion".to_string(), |m| m.to_string_lossy());
        vm.docref(crate::errors::level::WARNING, &format!("{m} failed"))?;
    }
    if bail {
        if let Some(e) = pending {
            // The callback threw: `zend_exception_error(EG(exception),
            // E_WARNING)`, then `zend_throw_unwind_exit()`.
            uncaught_warning(vm, &e)?;
        }
        return Err(Throw::Exit);
    }
    match pending {
        Some(e) => Err(Throw::Exception(e)),
        None => Ok(Value::Bool(false)),
    }
}

/// `zend_exception_error(ex, E_WARNING)`: reports `e` as an "Uncaught
/// ..." warning, straight to the error callback like every report of
/// `zend_exception_error` (`zend_error_va`): a user error handler never
/// sees it. An exception its `__toString()` threw stays pending
/// (`EG(exception)`, so `assert()` skips `zend_throw_unwind_exit()`) and is
/// returned as the throw.
#[cold]
fn uncaught_warning(vm: &mut Vm, e: &php_runtime::Obj) -> VmResult<()> {
    match vm.exception_error(e, crate::errors::level::WARNING) {
        Some(inner) => Err(Throw::Exception(inner)),
        None => Ok(()),
    }
}

fn class_alias(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let orig = s(a, 0);
    let alias = s(a, 1);
    let autoload = !a.has(2) || b(a, 2);
    let c = if autoload {
        vm.lookup_class(&orig)?
    } else {
        vm.find_class(orig.as_bytes())
    };
    let Some(c) = c else {
        vm.warning(&format!("Class \"{}\" not found", orig.to_string_lossy()))?;
        return Ok(Value::Bool(false));
    };
    let an = alias
        .as_bytes()
        .strip_prefix(b"\\")
        .unwrap_or(alias.as_bytes());
    let lc = ZStr::from(an.to_ascii_lowercase());
    if vm.classes.contains_key(&lc) {
        // `zend_class_redeclaration_error_ex(E_WARNING, alias_name, ce)`:
        // the message names the aliased class's kind and location.
        let msg = crate::classlink::class_redeclaration_message(&alias, &c);
        vm.warning(&msg)?;
        return Ok(Value::Bool(false));
    }
    if matches!(
        lc.as_bytes()
            .rsplit(|c| *c == b'\\')
            .next()
            .unwrap_or_default(),
        b"self"
            | b"parent"
            | b"static"
            | b"int"
            | b"float"
            | b"bool"
            | b"string"
            | b"true"
            | b"false"
            | b"null"
            | b"void"
            | b"never"
            | b"iterable"
            | b"object"
            | b"mixed"
            | b"array"
            | b"callable"
    ) {
        return Err(vm.fatal(&format!(
            "Cannot use \"{}\" as a class alias as it is reserved",
            lc.to_string_lossy()
        )));
    }
    // `zend_assert_valid_class_name(lcname, "a class alias")`.
    if lc.as_bytes() == b"_" || lc.as_bytes().ends_with(b"\\_") {
        vm.deprecated("Using \"_\" as a class alias is deprecated since 8.4")?;
    } else if lc.as_bytes() == b"let" || lc.as_bytes().ends_with(b"\\let") {
        vm.deprecated("Using \"let\" as a class alias is deprecated since 8.6")?;
    } else if lc.as_bytes() == b"is" || lc.as_bytes().ends_with(b"\\is") {
        vm.deprecated("Using \"is\" as a class alias is deprecated since 8.6")?;
    }
    vm.class_order.push(lc.clone());
    vm.classes.insert(lc, c);
    Ok(Value::Bool(true))
}

fn getcwd(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(match std::env::current_dir() {
        Ok(p) => {
            use std::os::unix::ffi::OsStrExt;
            sv(p.as_os_str().as_bytes())
        }
        Err(_) => Value::Bool(false),
    })
}

fn chdir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = s(a, 0);
    use std::os::unix::ffi::OsStrExt;
    match std::env::set_current_dir(std::ffi::OsStr::from_bytes(d.as_bytes())) {
        Ok(()) => Ok(Value::Bool(true)),
        Err(e) => {
            vm.docref(
                2,
                &format!("{} (errno {})", e.kind(), e.raw_os_error().unwrap_or(0)),
            )?;
            Ok(Value::Bool(false))
        }
    }
}

/// `ZEND_FUNCTION(exit)` (`Zend/zend_builtin_functions.c`). Since PHP 8.4
/// `exit`/`die` compile to an ordinary call of `\exit()`, so the argument
/// goes through `Z_PARAM_STR_OR_LONG` (null and float deprecations,
/// `TypeError`s, `strict_types`) and the function can be named as a
/// callable or called with a named argument.
fn exit_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.at(0) {
        Value::Str(s) => {
            if !s.is_empty() {
                vm.echo(s.as_bytes());
                // An exception might be emitted by an output handler.
                vm.take_pending_throw()?;
            }
        }
        Value::Int(n) => {
            // `EG(exit_status)` is a C `int`.
            #[allow(clippy::cast_possible_truncation)]
            let status = n as i32;
            vm.exit_status = status;
        }
        _ => {}
    }
    Err(Throw::Exit)
}

fn clone_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let r = vm.clone_value(v)?;
    if let (Value::Object(o), Some(Value::Array(props))) = (&r, a.get(1)) {
        for (k, pv) in props.iter() {
            let name = match k {
                ArrayKey::Str(s) => s.clone(),
                ArrayKey::Int(i) => ZStr::from(i.to_string()),
            };
            // `zend_objects_clone_obj_with`: a reference held only by the
            // array is unwrapped; a shared one cannot be assigned.
            let pv = match pv {
                Value::Reference(r) if r.refcount() > 1 => {
                    return Err(vm.make_throw(
                        "Error",
                        "Cannot assign by reference when cloning with updated properties",
                    ));
                }
                pv => pv.deref(),
            };
            let d = crate::object::data(o);
            d.flags.set(d.flags.get() | crate::object::oflags::CLONING);
            let res = vm.write_property(o, &name, pv);
            d.flags.set(d.flags.get() & !crate::object::oflags::CLONING);
            res?;
        }
    }
    Ok(r)
}

fn ret_zero(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(0))
}
fn ret_false(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(false))
}
fn ret_null(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Null)
}

fn memory_get_usage(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.frames.drop_pool(&vm.empty_vars);
    // The call protocol's buffer pools, like the frame pool: a buffer that
    // moved into a long-lived frame is replaced by a fresh one on the next
    // call, so a reading must not depend on which calls came before.
    vm.call_bufs.args.clear();
    vm.call_bufs.values.clear();
    vm.call_bufs.passed.clear();
    let real = a.has(0) && a.bool(0);
    Ok(Value::Int(
        i64::try_from(php_memlimit::memory_get_usage(real)).unwrap_or(i64::MAX),
    ))
}

fn memory_get_peak_usage(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let real = a.has(0) && a.bool(0);
    Ok(Value::Int(
        i64::try_from(php_memlimit::memory_get_peak_usage(real)).unwrap_or(i64::MAX),
    ))
}

fn memory_reset_peak_usage(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    php_memlimit::reset_peak();
    Ok(Value::Null)
}

fn gc_mem_caches(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(
        i64::try_from(php_memlimit::gc_mem_caches()).unwrap_or(0),
    ))
}

fn php_sapi_name(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(sv("cli"))
}

fn phpversion(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if a.has(0) && !a.at(0).is_null() {
        let e = s(a, 0).to_ascii_lowercase();
        return Ok(match e.as_bytes() {
            b"core" | b"standard" | b"date" | b"pcre" | b"spl" | b"json" | b"random"
            | b"reflection" | b"hash" => sv(crate::PHP_VERSION),
            _ => Value::Bool(false),
        });
    }
    Ok(sv(crate::PHP_VERSION))
}

fn zend_version(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(sv(crate::ZEND_VERSION))
}

fn extension_loaded(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let e = s(a, 0).to_ascii_lowercase();
    let e = e.to_string_lossy();
    Ok(Value::Bool(
        crate::LOADED_EXTENSIONS
            .iter()
            .any(|x| x.eq_ignore_ascii_case(&e))
            || _vm
                .loaded_extensions
                .iter()
                .any(|x| x.eq_ignore_ascii_case(&e)),
    ))
}

fn get_loaded_extensions(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // `$zend_extensions = true`: the Zend extensions instead of the modules.
    if a.has(0) && a.bool(0) {
        return Ok(Value::Array(ZArray::from_values(
            crate::reflection::misc::zend_extensions(vm)
                .into_iter()
                .map(|z| sv(z.name)),
        )));
    }
    let mut names: Vec<&str> = crate::LOADED_EXTENSIONS.to_vec();
    for e in &vm.loaded_extensions {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(e)) {
            names.push(e.as_str());
        }
    }
    Ok(Value::Array(ZArray::from_values(names.into_iter().map(sv))))
}

fn call_user_func(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let args: Vec<Value> = a.values.iter().skip(1).map(Value::deref).collect();
    let named = std::mem::take(&mut a.named);
    cuf_call(vm, &cb, args, named, "call_user_func")
}

fn call_user_func_array(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let (args, named) = cufa_args(vm, &arr(a, 1))?;
    cuf_call(vm, &cb, args, named, "call_user_func_array")
}

/// Splits a `call_user_func_array()` argument array into positional and
/// named arguments (`zend_fcall_info.named_params`).
#[allow(clippy::type_complexity)]
fn cufa_args(vm: &mut Vm, arr: &ZArray) -> VmResult<(Vec<Value>, Vec<(ZStr, Value)>)> {
    let mut args = Vec::new();
    let mut named = Vec::new();
    for (k, v) in arr.iter() {
        match k {
            ArrayKey::Int(_) => {
                if !named.is_empty() {
                    return Err(vm.make_throw(
                        "Error",
                        "Cannot use positional argument after named argument",
                    ));
                }
                args.push(v.clone());
            }
            ArrayKey::Str(s) => named.push((s.clone(), v.clone())),
        }
    }
    Ok((args, named))
}

/// `PHP_FUNCTION(forward_static_call)` (`ext/standard/basic_functions.c`):
/// like `call_user_func()`, but only from a class scope, and the callee
/// keeps the caller's called scope (`static::`) when it derives from the
/// callback's class.
fn forward_static_call(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let args: Vec<Value> = a.values.iter().skip(1).map(Value::deref).collect();
    let named = std::mem::take(&mut a.named);
    let call = fsc_resolve(vm, &cb)?;
    let caller_has_scope = vm
        .frames
        .len()
        .checked_sub(2)
        .and_then(|i| vm.frames.get(i))
        .is_some_and(|f| f.scope.is_some());
    if !caller_has_scope {
        return Err(vm.make_throw(
            "Error",
            "Cannot call forward_static_call() when no class scope is active",
        ));
    }
    let call = fsc_forward_called_scope(vm, call);
    vm.call_resolved_named(call, args, named)
}

/// `PHP_FUNCTION(forward_static_call_array)`: [`forward_static_call`] with
/// the arguments in an array, and without the class scope check.
fn forward_static_call_array(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = a.at(0);
    let call = fsc_resolve(vm, &cb)?;
    let (args, named) = cufa_args(vm, &arr(a, 1))?;
    let call = fsc_forward_called_scope(vm, call);
    vm.call_resolved_named(call, args, named)
}

/// `Z_PARAM_FUNC` of the `forward_static_call*()` functions.
fn fsc_resolve(vm: &mut Vm, cb: &Value) -> VmResult<PendingCall> {
    match vm.zend_is_callable(cb, false)? {
        Ok(call) => Ok(call),
        Err(reason) => Err(vm.make_throw("TypeError", &reason)),
    }
}

/// `zend_get_called_scope(execute_data)` (`Zend/zend_execute_API.c`) seen
/// from the builtin's frame replaces the callback's called scope when it
/// is an instance of the callback's class (`fci_cache.calling_scope`).
/// A callback bound to an object keeps the object's class.
fn fsc_forward_called_scope(vm: &Vm, mut call: PendingCall) -> PendingCall {
    if call.this.is_some() {
        return call;
    }
    let Some(calling) = call.called.clone().or_else(|| call.scope.clone()) else {
        return call;
    };
    let mut called = None;
    for f in vm.frames.iter().rev() {
        if let Some(o) = &f.this {
            called = Some(object::class_of(o).clone());
            break;
        }
        if let Some(c) = &f.called {
            called = Some(c.clone());
            break;
        }
        if f.kind != FrameKind::Builtin || f.scope.is_some() {
            break;
        }
    }
    if let Some(c) = called {
        if c.is_subclass_of(&calling) {
            call.called = Some(c);
        }
    }
    call
}

/// Calls the callback of `call_user_func[_array]()`. A direct call compiles
/// to `ZEND_INIT_USER_CALL` (`zend_compile_func_cuf`, `Zend/zend_compile.c`):
/// when an error handler throws on the "Use of "self" in callables"
/// deprecation, `zend_is_callable_check_class` (`Zend/zend_API.c`) fails
/// with "cannot use "self"/"parent"/"static" in callables" and the opcode
/// throws that `TypeError` chained to the pending exception. Through zpp
/// (a dynamic call) the pending exception propagates alone.
fn cuf_call(
    vm: &mut Vm,
    cb: &Value,
    args: Vec<Value>,
    named: Vec<(ZStr, Value)>,
    fname: &str,
) -> VmResult<Value> {
    // A direct call compiles to `ZEND_INIT_USER_CALL` + `ZEND_DO_FCALL`
    // (frameless here), whose unused result triggers `#[\NoDiscard]`.
    let discarded =
        vm.pending_call_dst.is_none() && vm.frames.last().is_some_and(|f| f.builtin_name.is_none());
    let call = match vm.zend_is_callable(cb, false) {
        Ok(Ok(call)) => call,
        Ok(Err(reason)) => return Err(vm.make_throw("TypeError", &reason)),
        Err(Throw::Exception(prev))
            if vm.frames.last().is_some_and(|f| f.builtin_name.is_none())
                && names_callable_keyword(cb) =>
        {
            return Err(cuf_keyword_error(vm, fname, &prev));
        }
        Err(t) => return Err(t),
    };
    if discarded && call.func.flags() & flags::NODISCARD != 0 {
        vm.check_nodiscard_call(&call)?;
    }
    vm.call_resolved_named(call, args, named)
}

/// Whether the callable's class part is `self`, `parent` or `static`.
fn names_callable_keyword(cb: &Value) -> bool {
    let class = match cb.deref() {
        Value::Str(s) => {
            let b = s.as_bytes();
            match b.windows(2).rposition(|w| w == b"::") {
                Some(p) => b[..p].to_ascii_lowercase(),
                None => return false,
            }
        }
        Value::Array(a) => match a.get(&ArrayKey::Int(0)).map(Value::deref) {
            Some(Value::Str(c)) => c.as_bytes().to_ascii_lowercase(),
            _ => return false,
        },
        _ => return false,
    };
    matches!(class.as_slice(), b"self" | b"parent" | b"static")
}

#[cold]
fn cuf_keyword_error(vm: &mut Vm, fname: &str, prev: &php_runtime::Obj) -> Throw {
    let e = vm.make_throw(
        "TypeError",
        &format!(
            "{fname}(): Argument #1 ($callback) must be a valid callback, \
             cannot use \"self\"/\"parent\"/\"static\" in callables"
        ),
    );
    if let Throw::Exception(o) = &e {
        crate::exceptions::chain_previous(o, prev);
    }
    e
}

fn is_callable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let syntax_only = b(a, 1);
    let r = if syntax_only {
        // `IS_CALLABLE_CHECK_SYNTAX_ONLY`: `[0 => object|string, 1 => string]`;
        // objects still need a closure (`get_closure`).
        match &v {
            Value::Str(_) => true,
            Value::Object(_) => vm.is_callable_value(&v),
            Value::Array(x) => {
                x.len() == 2
                    && matches!(
                        x.get(&ArrayKey::Int(0)).map(Value::deref),
                        Some(Value::Object(_) | Value::Str(_))
                    )
                    && matches!(
                        x.get(&ArrayKey::Int(1)).map(Value::deref),
                        Some(Value::Str(_))
                    )
            }
            _ => false,
        }
    } else {
        // `zend_is_callable_ex(var, NULL, 0, ...)`: deprecations (`"static"`
        // in callables) are emitted and exceptions propagate.
        vm.zend_is_callable(&v, false)?.is_ok()
    };
    if a.has(2) {
        let name = vm.callable_name(&v);
        a.set_ref(2, sv(name));
    }
    Ok(Value::Bool(r))
}

/// `register_shutdown_function()` (`ext/standard/basic_functions.c`): the
/// callback is resolved by `Z_PARAM_FUNC` in the registering scope, so
/// `"self::m"` raises its deprecation here and still names the class at
/// shutdown, when no scope is active.
fn register_shutdown_function(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cb = match vm.resolve_callable_value(&a.at(0))? {
        Ok(cb) => cb,
        Err(desc) => {
            return Err(super::util::type_error(
                vm,
                a,
                1,
                &format!("must be a valid callback, {desc}"),
            ))
        }
    };
    let args: Vec<Value> = a.values.iter().skip(1).map(Value::deref).collect();
    vm.shutdown_functions.push((cb, args));
    Ok(Value::Null)
}

fn get_defined_vars(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let Some(idx) = caller_idx(vm) else {
        return Ok(Value::Array(ZArray::new()));
    };
    vm.frames[idx].attach_symbol_table();
    vm.frames[idx].sync_cvs_to_symtab();
    let frame = &vm.frames[idx];
    let vars = frame.vars.borrow();
    // `zend_array_dup`: check ownership before copying the symbol-table slots.
    // The frame's CV of the same name mirrors its slot (php-src's slot is
    // `IS_INDIRECT`), so a reference held by both counts once.
    let mut out = HashTable::new();
    for (name, slot) in vars.names.iter().zip(&vars.slots) {
        let Some(v) = slot else { continue };
        let v = match v {
            Value::Reference(r) => {
                let mirrored = frame
                    .code
                    .cv_names
                    .iter()
                    .position(|n| n.as_bytes() == name.as_bytes())
                    .and_then(|i| frame.cvs.get(i).and_then(crate::vm::CvSlot::get))
                    .is_some_and(|cv| matches!(cv, Value::Reference(c) if PhpRef::ptr_eq(c, r)));
                if r.refcount() <= 1 + usize::from(mirrored) {
                    r.get()
                } else {
                    v.clone()
                }
            }
            other => other.clone(),
        };
        out.insert(php_runtime::ArrayKey::Str(name.clone()), v);
    }
    Ok(Value::Array(ZArray::from_table(out)))
}

fn compact(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(idx) = caller_idx(vm) else {
        return Ok(Value::Array(ZArray::new()));
    };
    vm.frames[idx].attach_symbol_table();
    let mut out = HashTable::new();
    let vars = vm.frames[idx].vars.clone();
    fn add(
        vm: &mut Vm,
        vars: &std::rc::Rc<std::cell::RefCell<crate::vm::VarStore>>,
        v: &Value,
        out: &mut HashTable,
        depth: u32,
    ) -> VmResult<()> {
        match v {
            Value::Str(n) => {
                let val = {
                    let mut st = vars.borrow_mut();
                    st.find(n.as_bytes())
                        .and_then(|i| st.slots[i as usize].clone())
                };
                match val {
                    Some(val) => {
                        out.insert(ArrayKey::from_zstr(n), val.deref());
                    }
                    None => {
                        if n.as_bytes() == b"this" {
                            if let Some(t) =
                                vm.frames.iter().rev().nth(1).and_then(|f| f.this.clone())
                            {
                                out.insert(ArrayKey::Str(n.clone()), Value::Object(t));
                                return Ok(());
                            }
                        }
                        vm.warning(&format!(
                            "compact(): Undefined variable ${}",
                            n.to_string_lossy()
                        ))?;
                    }
                }
            }
            Value::Array(arr) => {
                if depth > 64 {
                    return Err(vm.make_throw("Error", "Recursion detected"));
                }
                for (_, x) in arr.iter() {
                    add(vm, vars, &x.deref(), out, depth + 1)?;
                }
            }
            Value::Reference(r) => add(vm, vars, &r.get(), out, depth)?,
            other => {
                vm.warning(&format!(
                    "compact(): Argument #1 must be string or array of strings, {} given",
                    other.type_name()
                ))?;
            }
        }
        Ok(())
    }
    let vals: Vec<Value> = a.values.iter().map(Value::deref).collect();
    for v in vals {
        add(vm, &vars, &v, &mut out, 0)?;
    }
    Ok(Value::Array(ZArray::from_table(out)))
}

fn extract(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(idx) = caller_idx(vm) else {
        return Ok(Value::Int(0));
    };
    let src = match a.values.first() {
        Some(Value::Reference(r)) => r.get(),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    let Value::Array(arr) = src else {
        return Ok(Value::Int(0));
    };
    let flags_v = if a.has(1) { i(a, 1) } else { 0 };
    let prefix = if a.has(2) { s(a, 2) } else { ZStr::empty() };
    let refs = flags_v & 256 != 0;
    let mode = flags_v & 0xff;
    vm.frames[idx].attach_symbol_table();
    let vars = vm.frames[idx].vars.clone();
    let mut n = 0;
    for (k, v) in arr.iter() {
        let ArrayKey::Str(name) = k else {
            if mode == 2 || mode == 3 {
                // EXTR_PREFIX_ALL/INVALID with numeric keys.
                let nm = format!(
                    "{}_{}",
                    prefix,
                    match k {
                        ArrayKey::Int(i) => i.to_string(),
                        _ => String::new(),
                    }
                );
                let mut st = vars.borrow_mut();
                let i = st.find_or_add(nm.as_bytes());
                st.slots[i as usize] = Some(v.deref());
                n += 1;
            }
            continue;
        };
        let valid = !name.is_empty()
            && (name.as_bytes()[0].is_ascii_alphabetic()
                || name.as_bytes()[0] == b'_'
                || name.as_bytes()[0] >= 0x80)
            && name
                .as_bytes()
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80);
        // One borrow: `find` needs it mutably (it may build the name
        // index), and the slot test reads through the same guard.
        let exists = {
            let mut st = vars.borrow_mut();
            st.find(name.as_bytes())
                .is_some_and(|i| st.slots[i as usize].is_some())
        };
        let target = match mode {
            1 => {
                if exists {
                    continue;
                }
                name.clone()
            }
            2 => {
                if exists {
                    ZStr::from(format!("{prefix}_{name}"))
                } else {
                    name.clone()
                }
            }
            3 => ZStr::from(format!("{prefix}_{name}")),
            6 => {
                if !exists {
                    continue;
                }
                name.clone()
            }
            _ => name.clone(),
        };
        if !valid && mode != 3 {
            continue;
        }
        if target.as_bytes() == b"this" {
            continue;
        }
        if target.as_bytes() == b"GLOBALS" {
            continue;
        }
        let mut st = vars.borrow_mut();
        let i = st.find_or_add(target.as_bytes()) as usize;
        if refs {
            let _ = v;
        }
        match &mut st.slots[i] {
            Some(Value::Reference(r)) if !refs => {
                r.set(v.deref());
            }
            slot => *slot = Some(v.deref()),
        }
        n += 1;
    }
    vm.frames[idx].sync_from_symbol_table();
    Ok(Value::Int(n))
}

/// Internal classes php-src does not have (engine helpers).
const HIDDEN_CLASSES: &[&[u8]] = &[b"fibergracefulexit"];

/// `get_declared_class_impl()`: the class table in declaration order;
/// aliases (`class_alias()`) are listed by their lowercase key.
fn declared(vm: &Vm, pred: fn(&Class) -> bool) -> Value {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for lc in &vm.class_order {
        if !seen.insert(lc.clone()) || HIDDEN_CLASSES.contains(&lc.as_bytes()) {
            continue;
        }
        let Some(c) = vm.classes.get(lc) else {
            continue;
        };
        if !pred(c) {
            continue;
        }
        if c.lc_name == *lc {
            out.push(Value::Str(c.name.clone()));
        } else {
            out.push(Value::Str(lc.clone()));
        }
    }
    Value::Array(ZArray::from_values(out))
}

fn get_declared_classes(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(declared(vm, |c| !c.is_interface() && !c.is_trait()))
}

fn get_declared_interfaces(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(declared(vm, Class::is_interface))
}

fn get_declared_traits(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(declared(vm, Class::is_trait))
}

fn get_defined_functions(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // Disabled functions are never registered, so the flag is a no-op
    // (`ZEND_NUM_ARGS() == 1` deprecation in zend_builtin_functions.c).
    if a.has(0) {
        vm.deprecated(
            "get_defined_functions(): The $exclude_disabled parameter has no effect since PHP 8.0",
        )?;
    }
    // `EG(function_table)` order: internal functions as registered, then
    // user functions as declared.
    let internal: Vec<Value> = super::info::internal_functions(vm)
        .into_iter()
        .map(Value::Str)
        .collect();
    let user: Vec<Value> = super::info::user_functions(vm)
        .into_iter()
        .map(Value::Str)
        .collect();
    let mut t = HashTable::new();
    t.insert(
        ArrayKey::Str(ZStr::from("internal")),
        Value::Array(ZArray::from_values(internal)),
    );
    t.insert(
        ArrayKey::Str(ZStr::from("user")),
        Value::Array(ZArray::from_values(user)),
    );
    Ok(Value::Array(ZArray::from_table(t)))
}

fn get_defined_constants(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut t = HashTable::new();
    if b(a, 0) {
        let mut u = HashTable::new();
        for n in &vm.user_constants {
            if let Some(v) = vm.constants.get(n) {
                u.insert(ArrayKey::Str(n.clone()), v.clone());
            }
        }
        t.insert(
            ArrayKey::Str(ZStr::from("user")),
            Value::Array(ZArray::from_table(u)),
        );
        return Ok(Value::Array(ZArray::from_table(t)));
    }
    for (k, v) in &vm.constants {
        t.insert(ArrayKey::Str(k.clone()), v.clone());
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn iterator_to_array(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let preserve = !a.has(1) || b(a, 1);
    let pairs = vm.iterate_to_vec(&v)?;
    Ok(Value::Array(vm.pairs_to_array(pairs, preserve)?))
}

fn iterator_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = a.at(0);
    let pairs = vm.iterate_to_vec(&v)?;
    Ok(Value::Int(i64::try_from(pairs.len()).unwrap_or(0)))
}

fn iterator_apply(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Value::Object(o) = a.at(0) else {
        return Ok(Value::Int(0));
    };
    let cb = a.at(1);
    let args: Vec<Value> = match a.get(2) {
        Some(Value::Array(x)) => x.iter().map(|(_, v)| v.clone()).collect(),
        _ => Vec::new(),
    };
    let it = vm.get_iterator(&o)?;
    vm.call_method_name(&it, "rewind", Vec::new())?;
    let mut n = 0;
    loop {
        let valid = vm.call_method_name(&it, "valid", Vec::new())?;
        if !vm.to_bool(&valid)? {
            break;
        }
        n += 1;
        let r = vm.call_value(&cb, args.clone())?;
        if !vm.to_bool(&r)? {
            break;
        }
        vm.call_method_name(&it, "next", Vec::new())?;
    }
    Ok(Value::Int(n))
}

fn class_implements(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(c) = class_arg(vm, &a.at(0), true)? else {
        return Ok(Value::Bool(false));
    };
    let mut t = HashTable::new();
    for i in &c.interfaces {
        t.insert(ArrayKey::Str(i.name.clone()), Value::Str(i.name.clone()));
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn class_parents(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(c) = class_arg(vm, &a.at(0), true)? else {
        return Ok(Value::Bool(false));
    };
    let mut t = HashTable::new();
    let mut p = c.parent.clone();
    while let Some(x) = p {
        t.insert(ArrayKey::Str(x.name.clone()), Value::Str(x.name.clone()));
        p = x.parent.clone();
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn class_uses(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(c) = class_arg(vm, &a.at(0), true)? else {
        return Ok(Value::Bool(false));
    };
    let mut t = HashTable::new();
    for x in &c.traits {
        t.insert(ArrayKey::Str(x.name.clone()), Value::Str(x.name.clone()));
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn getenv(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if !a.has(0) || a.at(0).is_null() {
        let mut t = HashTable::new();
        for (k, v) in std::env::vars_os() {
            use std::os::unix::ffi::OsStrExt;
            t.insert(ArrayKey::from_bytes(k.as_bytes()), sv(v.as_bytes()));
        }
        return Ok(Value::Array(ZArray::from_table(t)));
    }
    let n = s(a, 0);
    use std::os::unix::ffi::OsStrExt;
    match std::env::var_os(std::ffi::OsStr::from_bytes(n.as_bytes())) {
        Some(v) => Ok(sv(v.as_bytes())),
        None => Ok(Value::Bool(false)),
    }
}

fn putenv(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let st = s(a, 0).to_string_lossy();
    match st.split_once('=') {
        // SAFETY-free: std::env::set_var is safe on this toolchain edition.
        Some((k, v)) => std::env::set_var(k, v),
        None => std::env::remove_var(st),
    }
    Ok(Value::Bool(true))
}

fn usleep(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    std::thread::sleep(std::time::Duration::from_micros(
        u64::try_from(i(a, 0).max(0)).unwrap_or(0),
    ));
    Ok(Value::Null)
}

fn sleep(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    std::thread::sleep(std::time::Duration::from_secs(
        u64::try_from(i(a, 0).max(0)).unwrap_or(0),
    ));
    Ok(Value::Int(0))
}

fn time(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(Value::Int(i64::try_from(t).unwrap_or(0)))
}

fn microtime(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    if b(a, 0) {
        return Ok(Value::Float(d.as_secs_f64()));
    }
    Ok(sv(format!(
        "{:.8} {}",
        f64::from(d.subsec_micros()) / 1e6,
        d.as_secs()
    )))
}

fn hrtime(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let st = START.get_or_init(std::time::Instant::now);
    let e = st.elapsed();
    if b(a, 0) {
        return Ok(Value::Int(
            i64::try_from(e.as_nanos()).unwrap_or(0) + 1_000_000_000,
        ));
    }
    Ok(Value::Array(ZArray::from_values([
        Value::Int(i64::try_from(e.as_secs()).unwrap_or(0) + 1),
        Value::Int(i64::from(e.subsec_nanos())),
    ])))
}

fn version_compare(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v1 = s(a, 0);
    let v2 = s(a, 1);
    let c = php_version_compare(v1.as_bytes(), v2.as_bytes());
    if a.has(2) && !a.at(2).is_null() {
        let op = s(a, 2).to_string_lossy();
        let r = match op.as_str() {
            "<" | "lt" => c < 0,
            "<=" | "le" => c <= 0,
            ">" | "gt" => c > 0,
            ">=" | "ge" => c >= 0,
            "==" | "eq" => c == 0,
            "!=" | "<>" | "ne" => c != 0,
            _ => {
                return Err(value_error(vm, a, 3, "must be a valid comparison operator"));
            }
        };
        return Ok(Value::Bool(r));
    }
    Ok(Value::Int(i64::from(c)))
}

fn canon_version(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: Option<u8> = None;
    for &c in v {
        let kind = |x: u8| {
            if x.is_ascii_digit() {
                1
            } else if x == b'.' || x == b'-' || x == b'_' || x == b'+' {
                2
            } else {
                3
            }
        };
        if kind(c) == 2 {
            if out.last() != Some(&b'.') {
                out.push(b'.');
            }
        } else {
            if let Some(p) = prev {
                if kind(p) != 2 && kind(p) != kind(c) && out.last() != Some(&b'.') {
                    out.push(b'.');
                }
            }
            out.push(c);
        }
        prev = Some(c);
    }
    out
}

fn special_order(s: &[u8]) -> i32 {
    let forms: [(&[u8], i32); 9] = [
        (b"dev", 0),
        (b"alpha", 1),
        (b"a", 1),
        (b"beta", 2),
        (b"b", 2),
        (b"RC", 3),
        (b"rc", 3),
        (b"#", 4),
        (b"pl", 5),
    ];
    for (f, o) in forms {
        if s.starts_with(f) {
            return o;
        }
    }
    -6
}

/// `php_version_compare`.
pub(crate) fn php_version_compare(a: &[u8], b: &[u8]) -> i32 {
    let ca = canon_version(a);
    let cb = canon_version(b);
    let pa: Vec<&[u8]> = ca.split(|&c| c == b'.').filter(|p| !p.is_empty()).collect();
    let pb: Vec<&[u8]> = cb.split(|&c| c == b'.').filter(|p| !p.is_empty()).collect();
    let n = pa.len().max(pb.len());
    for idx in 0..n {
        let x = pa.get(idx);
        let y = pb.get(idx);
        let r = match (x, y) {
            (Some(x), Some(y)) => {
                let xd = x[0].is_ascii_digit();
                let yd = y[0].is_ascii_digit();
                match (xd, yd) {
                    (true, true) => {
                        let xi: i64 = std::str::from_utf8(x)
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        let yi: i64 = std::str::from_utf8(y)
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        (xi > yi) as i32 - (xi < yi) as i32
                    }
                    (false, false) => {
                        let (o1, o2) = (special_order(x), special_order(y));
                        (o1 > o2) as i32 - (o1 < o2) as i32
                    }
                    (true, false) => {
                        let o = special_order(y);
                        (4 > o) as i32 - (4 < o) as i32
                    }
                    (false, true) => {
                        let o = special_order(x);
                        (o > 4) as i32 - (o < 4) as i32
                    }
                }
            }
            (Some(x), None) => {
                if x[0].is_ascii_digit() {
                    1
                } else {
                    let o = special_order(x);
                    (o > 4) as i32 - (o < 4) as i32
                }
            }
            (None, Some(y)) => {
                if y[0].is_ascii_digit() {
                    -1
                } else {
                    let o = special_order(y);
                    (4 > o) as i32 - (4 < o) as i32
                }
            }
            (None, None) => 0,
        };
        if r != 0 {
            return r;
        }
    }
    0
}

fn get_resource_type(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.at(0) {
        Value::Resource(r) => Ok(sv(r
            .type_name()
            .map_or_else(|| "Unknown".to_string(), |t| t.to_string_lossy()))),
        _ => Ok(Value::Bool(false)),
    }
}

fn get_resource_id(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match a.at(0) {
        Value::Resource(r) => Ok(Value::Int(r.handle())),
        _ => Ok(Value::Int(0)),
    }
}

fn enum_cases(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let called = a.called.clone().expect("enum class");
    vm.enum_cases_array(&called)
}

fn enum_from(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let called = a.called.clone().expect("enum class");
    vm.enum_from(&called, a.at(0), false)
}

fn enum_try_from(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let called = a.called.clone().expect("enum class");
    vm.enum_from(&called, a.at(0), true)
}

pub(crate) fn enum_methods() -> [(&'static str, u32, crate::func::BuiltinFn); 3] {
    [
        ("cases(): array", flags::PUBLIC | flags::STATIC, enum_cases),
        (
            "from(@unchecked int|string $value): static",
            flags::PUBLIC | flags::STATIC,
            enum_from,
        ),
        (
            "tryFrom(@unchecked int|string $value): ?static",
            flags::PUBLIC | flags::STATIC,
            enum_try_from,
        ),
    ]
}

/// Registers the core functions and constants.
#[allow(clippy::too_many_lines)]
pub(crate) fn register(vm: &mut Vm) {
    reg(vm, "zend_version(): string", zend_version);
    reg(
        vm,
        "assert(mixed $assertion, Throwable|string|null $description = null): bool",
        assert_fn,
    );
    reg(
        vm,
        "class_alias(string $class, string $alias, bool $autoload = true): bool",
        class_alias,
    );
    reg(vm, "getcwd(): string|false", getcwd);
    reg(vm, "chdir(string $directory): bool", chdir);
    reg(
        vm,
        "clone(object $object, array $withProperties = []): object",
        clone_fn,
    );
    reg(vm, "exit(string|int $status = 0): never", exit_fn);
    alias(vm, "die", "exit");
    reg(vm, "func_num_args(): int", func_num_args);
    reg(vm, "func_get_arg(int $position): mixed", func_get_arg);
    reg(vm, "func_get_args(): array", func_get_args);
    reg(vm, "strlen(string $string): int", strlen);
    reg(
        vm,
        "define(string $constant_name, mixed $value, bool $case_insensitive = false): bool",
        define,
    );
    reg(vm, "defined(string $constant_name): bool", defined);
    reg(vm, "constant(string $name): mixed", constant_fn);
    reg(vm, "get_class(object $object = UNKNOWN): string", get_class);
    reg(vm, "get_called_class(): string", get_called_class);
    reg(
        vm,
        "get_parent_class($object_or_class = UNKNOWN): string|false",
        get_parent_class,
    );
    reg(
        vm,
        "is_subclass_of(mixed $object_or_class, string $class, bool $allow_string = true): bool",
        is_subclass_of,
    );
    reg(
        vm,
        "is_a(mixed $object_or_class, string $class, bool $allow_string = false): bool",
        is_a,
    );
    reg(
        vm,
        "get_class_vars(string $class): array|false",
        get_class_vars,
    );
    reg(
        vm,
        "get_object_vars(object $object): array",
        get_object_vars,
    );
    reg(
        vm,
        "get_mangled_object_vars(object $object): array",
        get_mangled_object_vars,
    );
    reg(
        vm,
        "get_class_methods(@unchecked object|string $object_or_class): array",
        get_class_methods,
    );
    reg(
        vm,
        "method_exists($object_or_class, string $method): bool",
        method_exists,
    );
    reg(
        vm,
        "property_exists($object_or_class, string $property): bool",
        property_exists,
    );
    reg(
        vm,
        "class_exists(string $class, bool $autoload = true): bool",
        class_exists,
    );
    reg(
        vm,
        "interface_exists(string $interface, bool $autoload = true): bool",
        interface_exists,
    );
    reg(
        vm,
        "trait_exists(string $trait, bool $autoload = true): bool",
        trait_exists,
    );
    reg(
        vm,
        "enum_exists(string $enum, bool $autoload = true): bool",
        enum_exists,
    );
    reg(
        vm,
        "function_exists(string $function): bool",
        function_exists,
    );
    reg(vm, "get_declared_classes(): array", get_declared_classes);
    reg(vm, "get_declared_traits(): array", get_declared_traits);
    reg(
        vm,
        "get_declared_interfaces(): array",
        get_declared_interfaces,
    );
    reg(
        vm,
        "get_defined_functions(bool $exclude_disabled = true): array",
        get_defined_functions,
    );
    reg(vm, "get_defined_vars(): array", get_defined_vars);
    reg(
        vm,
        "get_defined_constants(bool $categorize = false): array",
        get_defined_constants,
    );
    reg(
        vm,
        "trigger_error(string $message, int $error_level = E_USER_NOTICE): true",
        trigger_error,
    );
    alias(vm, "user_error", "trigger_error");
    reg(
        vm,
        "set_error_handler(?callable $callback, int $error_levels = E_ALL)",
        set_error_handler,
    );
    reg(vm, "restore_error_handler(): true", restore_error_handler);
    reg(vm, "get_error_handler(): ?callable", get_error_handler);
    reg(
        vm,
        "set_exception_handler(?callable $callback)",
        set_exception_handler,
    );
    reg(
        vm,
        "restore_exception_handler(): true",
        restore_exception_handler,
    );
    reg(
        vm,
        "get_exception_handler(): ?callable",
        get_exception_handler,
    );
    reg(
        vm,
        "error_reporting(?int $error_level = null): int",
        error_reporting,
    );
    reg(vm, "error_get_last(): ?array", error_get_last);
    reg(vm, "error_clear_last(): void", error_clear_last);
    reg(vm, "ini_get(string $option): string|false", ini_get);
    reg(
        vm,
        "ini_set(string $option, mixed $value): string|false",
        ini_set,
    );
    alias(vm, "ini_alter", "ini_set");
    reg(vm, "ini_restore(string $option): void", ini_restore);
    reg(
        vm,
        "set_include_path(string $include_path): string|false",
        set_include_path,
    );
    reg(vm, "get_include_path(): string|false", get_include_path);
    reg(
        vm,
        "debug_backtrace(int $options = 1, int $limit = 0): array",
        debug_backtrace,
    );
    reg(
        vm,
        "debug_print_backtrace(int $options = 0, int $limit = 0): void",
        debug_print_backtrace,
    );
    // gc_collect_cycles/gc_enabled/gc_enable/gc_disable/gc_status: the
    // collector (crate::gc::register).
    reg(vm, "gc_mem_caches(): int", gc_mem_caches);
    reg(
        vm,
        "memory_get_usage(bool $real_usage = false): int",
        memory_get_usage,
    );
    reg(
        vm,
        "memory_get_peak_usage(bool $real_usage = false): int",
        memory_get_peak_usage,
    );
    reg(
        vm,
        "memory_reset_peak_usage(): void",
        memory_reset_peak_usage,
    );
    reg(
        vm,
        "set_time_limit(int $seconds): bool",
        crate::interrupt::set_time_limit,
    );
    reg(
        vm,
        "register_tick_function(callable $callback, mixed ...$args): bool",
        crate::interrupt::register_tick_function,
    );
    reg(
        vm,
        "unregister_tick_function(callable $callback): void",
        crate::interrupt::unregister_tick_function,
    );
    reg(vm, "ignore_user_abort(?bool $enable = null): int", ret_zero);
    reg(vm, "connection_status(): int", ret_zero);
    reg(vm, "connection_aborted(): int", ret_zero);
    reg(vm, "php_sapi_name(): string|false", php_sapi_name);
    reg(
        vm,
        "phpversion(?string $extension = null): string|false",
        phpversion,
    );
    reg(
        vm,
        "extension_loaded(string $extension): bool",
        extension_loaded,
    );
    reg(
        vm,
        "get_loaded_extensions(bool $zend_extensions = false): array",
        get_loaded_extensions,
    );
    reg(
        vm,
        "call_user_func(callable $callback, mixed ...$args): mixed",
        call_user_func,
    );
    reg(
        vm,
        "call_user_func_array(callable $callback, array $args): mixed",
        call_user_func_array,
    );
    reg(
        vm,
        "forward_static_call(callable $callback, mixed ...$args): mixed",
        forward_static_call,
    );
    reg(
        vm,
        "forward_static_call_array(callable $callback, array $args): mixed",
        forward_static_call_array,
    );
    reg(
        vm,
        "is_callable(mixed $value, bool $syntax_only = false, &$callable_name = null): bool",
        is_callable,
    );
    reg(
        vm,
        "register_shutdown_function(callable $callback, mixed ...$args): void",
        register_shutdown_function,
    );
    reg(vm, "compact($var_name, ...$var_names): array", compact);
    reg(
        vm,
        "extract(@prefer-ref array &$array, int $flags = 0, string $prefix = \"\"): int",
        extract,
    );
    reg(vm, "spl_autoload_register(?callable $callback = null, bool $throw = true, bool $prepend = false): bool", spl_autoload_register);
    reg(
        vm,
        "spl_autoload_unregister(callable $callback): bool",
        spl_autoload_unregister,
    );
    reg(
        vm,
        "spl_autoload_functions(): array",
        spl_autoload_functions,
    );
    reg(
        vm,
        "spl_autoload_call(string $class): void",
        spl_autoload_call,
    );
    reg(vm, "spl_object_id(object $object): int", spl_object_id);
    reg(
        vm,
        "spl_object_hash(object $object): string",
        spl_object_hash,
    );
    reg(
        vm,
        "class_implements($object_or_class, bool $autoload = true): array|false",
        class_implements,
    );
    reg(
        vm,
        "class_parents($object_or_class, bool $autoload = true): array|false",
        class_parents,
    );
    reg(
        vm,
        "class_uses($object_or_class, bool $autoload = true): array|false",
        class_uses,
    );
    reg(
        vm,
        "iterator_to_array(Traversable|array $iterator, bool $preserve_keys = true): array",
        iterator_to_array,
    );
    reg(
        vm,
        "iterator_count(Traversable|array $iterator): int",
        iterator_count,
    );
    reg(
        vm,
        "iterator_apply(Traversable $iterator, callable $callback, ?array $args = null): int",
        iterator_apply,
    );
    reg(
        vm,
        "getenv(?string $name = null, bool $local_only = false): array|string|false",
        getenv,
    );
    reg(vm, "putenv(string $assignment): bool", putenv);
    reg(vm, "usleep(int $microseconds): void", usleep);
    reg(vm, "sleep(int $seconds): int", sleep);
    reg(vm, "time(): int", time);
    reg(
        vm,
        "microtime(bool $as_float = false): string|float",
        microtime,
    );
    reg(
        vm,
        "hrtime(bool $as_number = false): array|int|float|false",
        hrtime,
    );
    reg(
        vm,
        "version_compare(string $version1, string $version2, ?string $operator = null): int|bool",
        version_compare,
    );
    reg(
        vm,
        "get_resource_type($resource): string",
        get_resource_type,
    );
    reg(vm, "get_resource_id($resource): int", get_resource_id);
    reg(vm, "flush(): void", ret_null);
    reg(
        vm,
        "headers_sent(&$filename = null, &$line = null): bool",
        ret_false,
    );
    reg(
        vm,
        "header(string $header, bool $replace = true, int $response_code = 0): void",
        ret_null,
    );
    reg(vm, "headers_list(): array", |_, _| {
        Ok(Value::Array(ZArray::new()))
    });
    reg(vm, "php_ini_loaded_file(): string|false", ret_false);
    reg(vm, "php_ini_scanned_files(): string|false", ret_false);
    reg(
        vm,
        "setlocale(int $category, $locales, ...$rest): string|false",
        |_, _| Ok(sv("C")),
    );
    reg(vm, "gethostname(): string|false", |_, _| {
        Ok(sv("localhost"))
    });
    reg(vm, "getmypid(): int|false", |_, _| {
        Ok(Value::Int(i64::from(std::process::id())))
    });
    reg(vm, "sys_get_temp_dir(): string", |_, _| {
        Ok(sv(std::env::temp_dir().to_string_lossy().into_owned()))
    });
    reg(vm, "php_uname(string $mode = \"a\"): string", |_, a| {
        let m = s(a, 0);
        Ok(sv(match m.as_bytes() {
            b"s" => "Linux",
            b"n" => "localhost",
            b"r" => "6.0.0",
            b"m" => "x86_64",
            _ => "Linux localhost 6.0.0 #1 SMP x86_64",
        }))
    });

    for (n, v) in [
        ("E_ERROR", 1),
        ("E_WARNING", 2),
        ("E_PARSE", 4),
        ("E_NOTICE", 8),
        ("E_CORE_ERROR", 16),
        ("E_CORE_WARNING", 32),
        ("E_COMPILE_ERROR", 64),
        ("E_COMPILE_WARNING", 128),
        ("E_USER_ERROR", 256),
        ("E_USER_WARNING", 512),
        ("E_USER_NOTICE", 1024),
        ("E_STRICT", 2048),
        ("E_RECOVERABLE_ERROR", 4096),
        ("E_DEPRECATED", 8192),
        ("E_USER_DEPRECATED", 16384),
        ("E_ALL", 30719),
        ("DEBUG_BACKTRACE_PROVIDE_OBJECT", 1),
        ("DEBUG_BACKTRACE_IGNORE_ARGS", 2),
        ("EXTR_OVERWRITE", 0),
        ("EXTR_SKIP", 1),
        ("EXTR_PREFIX_SAME", 2),
        ("EXTR_PREFIX_ALL", 3),
        ("EXTR_PREFIX_INVALID", 4),
        ("EXTR_PREFIX_IF_EXISTS", 5),
        ("EXTR_IF_EXISTS", 6),
        ("EXTR_REFS", 256),
        ("PHP_MAJOR_VERSION", 8),
        ("PHP_MINOR_VERSION", 7),
        ("PHP_RELEASE_VERSION", 0),
        ("PHP_VERSION_ID", 80700),
        ("PHP_DEBUG", 0),
        ("PHP_ZTS", 0),
        ("PHP_MAXPATHLEN", 4096),
        ("PHP_INT_SIZE", 8),
        ("PHP_FLOAT_DIG", 15),
        ("ZEND_THREAD_SAFE", 0),
        ("ZEND_DEBUG_BUILD", 0),
    ] {
        constant(vm, n, Value::Int(v));
    }
    // `#[\Deprecated(since: '8.4', message: 'the error level was removed')]`
    // on `E_STRICT` in Zend/zend_constants.stub.php.
    vm.deprecated_constants.insert(
        ZStr::from("E_STRICT"),
        crate::attrs::format_suffix(
            Some(&ZStr::from("8.4")),
            Some(&ZStr::from("the error level was removed")),
        ),
    );
    constant(vm, "ZEND_THREAD_SAFE", Value::Bool(false));
    constant(vm, "ZEND_DEBUG_BUILD", Value::Bool(false));
    constant(vm, "PHP_INT_MAX", Value::Int(i64::MAX));
    constant(vm, "PHP_INT_MIN", Value::Int(i64::MIN));
    constant(vm, "PHP_FLOAT_EPSILON", Value::Float(f64::EPSILON));
    constant(vm, "PHP_FLOAT_MAX", Value::Float(f64::MAX));
    constant(vm, "PHP_FLOAT_MIN", Value::Float(f64::MIN_POSITIVE));
    constant(vm, "NAN", Value::Float(f64::NAN));
    constant(vm, "INF", Value::Float(f64::INFINITY));
    constant(vm, "PHP_VERSION", sv(crate::PHP_VERSION));
    constant(vm, "PHP_EXTRA_VERSION", sv("-dev"));
    constant(vm, "PHP_OS", sv("Linux"));
    constant(vm, "PHP_OS_FAMILY", sv("Linux"));
    constant(vm, "PHP_EOL", sv("\n"));
    constant(vm, "DIRECTORY_SEPARATOR", sv("/"));
    constant(vm, "PATH_SEPARATOR", sv(":"));
    constant(vm, "PHP_SHLIB_SUFFIX", sv("so"));
    constant(
        vm,
        "PHP_BINARY",
        sv(std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()),
    );
    constant(vm, "PHP_SAPI", sv("cli"));
    constant(vm, "PHP_PREFIX", sv("/usr/local"));
    constant(vm, "PHP_BINDIR", sv("/usr/local/bin"));
    constant(vm, "PHP_LIBDIR", sv("/usr/local/lib/php"));
    constant(vm, "PHP_DATADIR", sv("/usr/local/share/php"));
    constant(vm, "PHP_EXTENSION_DIR", sv("/usr/local/lib/php/extensions"));
    constant(vm, "DEFAULT_INCLUDE_PATH", sv(".:/usr/local/lib/php"));
    constant(vm, "PEAR_INSTALL_DIR", sv("/usr/local/lib/php"));
    constant(
        vm,
        "PEAR_EXTENSION_DIR",
        sv("/usr/local/lib/php/extensions"),
    );
    constant(vm, "PHP_CONFIG_FILE_PATH", sv("/usr/local/etc"));
    constant(vm, "PHP_CONFIG_FILE_SCAN_DIR", sv(""));
    constant(vm, "PHP_MANDIR", sv("/usr/local/share/man"));
    constant(vm, "PHP_SYSCONFDIR", sv("/usr/local/etc"));
    constant(vm, "PHP_LOCALSTATEDIR", sv("/usr/local/var"));
    constant(vm, "PHP_FD_SETSIZE", Value::Int(1024));
    constant(vm, "PHP_OUTPUT_HANDLER_START", Value::Int(1));
    constant(vm, "PHP_OUTPUT_HANDLER_WRITE", Value::Int(0));
    constant(vm, "PHP_OUTPUT_HANDLER_FLUSH", Value::Int(4));
    constant(vm, "PHP_OUTPUT_HANDLER_CLEAN", Value::Int(2));
    constant(vm, "PHP_OUTPUT_HANDLER_FINAL", Value::Int(8));
    constant(vm, "PHP_OUTPUT_HANDLER_CONT", Value::Int(0));
    constant(vm, "PHP_OUTPUT_HANDLER_END", Value::Int(8));
    constant(vm, "PHP_OUTPUT_HANDLER_CLEANABLE", Value::Int(16));
    constant(vm, "PHP_OUTPUT_HANDLER_FLUSHABLE", Value::Int(32));
    constant(vm, "PHP_OUTPUT_HANDLER_REMOVABLE", Value::Int(64));
    constant(vm, "PHP_OUTPUT_HANDLER_STDFLAGS", Value::Int(112));
    constant(vm, "PHP_OUTPUT_HANDLER_STARTED", Value::Int(4096));
    constant(vm, "PHP_OUTPUT_HANDLER_DISABLED", Value::Int(8192));
    constant(vm, "PHP_OUTPUT_HANDLER_PROCESSED", Value::Int(16384));
    for (name, value) in php_sapi::UPLOAD_ERR_CONSTANTS {
        constant(vm, name, Value::Int(*value));
    }
    let _ = (Throw::Fatal,);
}

#[cfg(test)]
mod ini_get_tests {
    use super::{ini_value_str, INI_SHARED_MAX};
    use php_runtime::Value;

    /// `ini_get()` shares one interned string per value, as php-src's
    /// `ZVAL_SET_INI_STR` (`debug_zval_dump(ini_get('precision'))` prints
    /// `string(2) "14" interned` under `/usr/bin/php8.5 -n`).
    #[test]
    fn ini_get_values_are_shared_interned_strings() {
        let (Value::Str(a), Value::Str(b)) = (ini_value_str(b"files"), ini_value_str(b"files"))
        else {
            panic!("ini_get value is not a string");
        };
        assert!(a.is_interned());
        assert!(php_runtime::ZStr::ptr_eq(&a, &b));
        let Value::Str(long) = ini_value_str(&vec![b'x'; INI_SHARED_MAX + 1]) else {
            panic!("ini_get value is not a string");
        };
        assert!(!long.is_interned());
        assert_eq!(long.len(), INI_SHARED_MAX + 1);
    }
}
