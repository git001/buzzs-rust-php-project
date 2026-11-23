// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ReflectionClass`, `ReflectionObject` and `ReflectionEnum`.

use super::order::{self, PropMeta};
use super::{
    class_value, method_value, new_refl, rexc, set_name_props, str_arg, this, this_refl,
    with_scope, zs, FnRef, PropRef, Refl, PRIV, PUB, RO,
};
use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class, ClassConst, TraitRule};
use crate::func::{flags, Args, Func, Method};
use crate::object;
use crate::types::{mask, TypeDecl};
use crate::vm::{PendingCall, Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Obj, Value, ZArray, ZStr};
use std::rc::Rc;

/// The reflected class (and object for `ReflectionObject`).
pub(crate) fn this_class_obj(vm: &mut Vm, a: &Args) -> VmResult<(Rc<Class>, Option<Obj>)> {
    match this_refl(vm, a)? {
        Refl::Class { cls, obj } => Ok((cls, obj)),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn this_class(vm: &mut Vm, a: &Args) -> VmResult<Rc<Class>> {
    Ok(this_class_obj(vm, a)?.0)
}

/// `ce->interfaces` in php-src order.
pub(crate) fn interfaces_order(vm: &Vm, c: &Rc<Class>) -> Vec<Rc<Class>> {
    let Some(decl) = &c.decl else {
        return c.interfaces.clone();
    };
    let mut out: Vec<Rc<Class>> = c
        .parent
        .as_ref()
        .map(|p| interfaces_order(vm, p))
        .unwrap_or_default();
    let mut direct: Vec<Rc<Class>> = decl
        .interfaces
        .iter()
        .filter_map(|n| vm.find_class(n.as_bytes()))
        .collect();
    if c.is_enum() {
        if let Some(u) = vm.find_class(b"UnitEnum") {
            direct.push(u);
        }
        if c.backing.is_some() {
            if let Some(b) = vm.find_class(b"BackedEnum") {
                direct.push(b);
            }
        }
    }
    let nparent = out.len();
    for i in direct {
        if !out.iter().any(|x| Rc::ptr_eq(x, &i)) {
            out.push(i);
        }
    }
    // `zend_do_inherit_interfaces`: each new interface's own interfaces are
    // appended in reverse order.
    let ndirect = out.len();
    for k in nparent..ndirect {
        let iface = out[k].clone();
        let ce_num = out.len();
        for sub in interfaces_order(vm, &iface).into_iter().rev() {
            if !out[..ce_num].iter().any(|x| Rc::ptr_eq(x, &sub)) {
                out.push(sub);
            }
        }
    }
    for i in &c.interfaces {
        if !out.iter().any(|x| Rc::ptr_eq(x, i)) {
            out.push(i.clone());
        }
    }
    out
}

/// Names of the traits used by `c` (as written).
fn trait_names(c: &Class) -> Vec<ZStr> {
    match &c.decl {
        Some(d) => d
            .traits
            .iter()
            .map(|n| ZStr::from(n.as_bytes().strip_prefix(b"\\").unwrap_or(n.as_bytes())))
            .collect(),
        None => c.traits.iter().map(|t| t.name.clone()).collect(),
    }
}

/// Resolves a `ReflectionClass|string` argument.
pub(crate) fn class_arg(vm: &mut Vm, v: &Value, what: &str) -> VmResult<Rc<Class>> {
    match v {
        Value::Object(o) => match super::refl(o) {
            Some(Refl::Class { cls, .. }) => Ok(cls),
            _ => Err(vm.make_throw(
                "Error",
                "Internal error: Failed to retrieve the argument's reflection object",
            )),
        },
        _ => {
            let s = vm.to_zstr(v)?;
            match vm.lookup_class(&s)? {
                Some(c) => Ok(c),
                None => Err(rexc(
                    vm,
                    &format!("{what} \"{}\" does not exist", s.to_string_lossy()),
                )),
            }
        }
    }
}

/// Whether a typed static property holds no value yet (`IS_UNDEF`); the VM
/// keeps `null` in the slot of a typed static property without default.
pub(crate) fn static_uninit(c: &Rc<Class>, name: &[u8], cell: &Value) -> bool {
    c.find_static(name)
        .is_some_and(|sp| sp.ty.is_some() && sp.default.is_none() && cell.is_null())
}

/// `zend_update_class_constants`: evaluates default property values.
pub(crate) fn update_class_constants(vm: &mut Vm, c: &Rc<Class>) -> VmResult<()> {
    vm.class_defaults(c)?;
    let names: Vec<ZStr> = c.static_props.iter().map(|s| s.name.clone()).collect();
    for n in names {
        with_scope(vm, Some(c.clone()), |vm| vm.static_prop_cell(c, &n))?;
    }
    Ok(())
}

fn rc_construct_impl(vm: &mut Vm, a: &Args, is_object: bool) -> VmResult<Value> {
    let o = this(a);
    let arg = a.at(0);
    let (cls, obj) = match &arg {
        Value::Object(x) => (
            object::class_of(x).clone(),
            if is_object { Some(x.clone()) } else { None },
        ),
        _ => {
            let s = vm.to_zstr(&arg)?;
            match vm.lookup_class(&s)? {
                Some(c) => (c, None),
                None => {
                    let e = vm.make_throw(
                        "ReflectionException",
                        &format!("Class \"{}\" does not exist", s.to_string_lossy()),
                    );
                    if let crate::vm::Throw::Exception(ex) = &e {
                        crate::exceptions::set_base_prop(ex, "code", Value::Int(-1));
                    }
                    return Err(e);
                }
            }
        }
    };
    set_name_props(&o, Some(&cls.name), None);
    object::set_internal(&o, Refl::Class { cls, obj });
    Ok(Value::Null)
}

fn rc_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    rc_construct_impl(vm, a, false)
}

fn ro_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    rc_construct_impl(vm, a, true)
}

fn rc_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let s = super::export::class_string(vm, &c, obj.as_ref(), "")?;
    Ok(Value::Str(ZStr::from(s)))
}

fn rc_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(this_class(vm, a)?.name.clone()))
}
fn rc_get_short_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(super::short_name(&this_class(vm, a)?.name)))
}
fn rc_get_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(super::ns_name(&this_class(vm, a)?.name)))
}
fn rc_in_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(super::in_ns(&this_class(vm, a)?.name)))
}

macro_rules! rc_bool {
    ($name:ident, |$c:ident| $e:expr) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let $c = this_class(vm, a)?;
            Ok(Value::Bool($e))
        }
    };
}

/// `ZEND_ACC_IMPLICIT_ABSTRACT_CLASS | ZEND_ACC_EXPLICIT_ABSTRACT_CLASS`.
pub(crate) fn is_abstract(c: &Class) -> bool {
    c.flags & cflags::EXPLICIT_ABSTRACT != 0
        || (c.flags & cflags::ABSTRACT != 0 && !c.is_interface())
        || c.methods.values().any(|m| m.is_abstract())
}

rc_bool!(rc_is_interface, |c| c.is_interface());
rc_bool!(rc_is_trait, |c| c.is_trait());
rc_bool!(rc_is_enum, |c| c.is_enum());
rc_bool!(rc_is_abstract, |c| is_abstract(&c));
rc_bool!(rc_is_final, |c| c.flags & cflags::FINAL != 0);
rc_bool!(rc_is_readonly, |c| c.flags & cflags::READONLY != 0);
rc_bool!(rc_is_anon, |c| c.flags & cflags::ANON != 0);
rc_bool!(rc_is_internal, |c| c.flags & cflags::INTERNAL != 0);
rc_bool!(rc_is_user, |c| c.flags & cflags::INTERNAL == 0);
rc_bool!(rc_is_instantiable, |c| !c.is_interface()
    && !c.is_trait()
    && !c.is_enum()
    && !is_abstract(&c)
    && c.magic
        .construct
        .as_ref()
        .is_none_or(|m| m.visibility() == flags::PUBLIC));
rc_bool!(rc_is_iterable, |c| !c.is_interface()
    && !c.is_trait()
    && !is_abstract(&c)
    && c.instance_of_name(b"traversable"));

fn rc_is_cloneable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    if c.is_interface() || c.is_trait() || c.is_enum() || is_abstract(&c) {
        return Ok(Value::Bool(false));
    }
    if let Some(m) = &c.magic.clone {
        return Ok(Value::Bool(m.visibility() == flags::PUBLIC));
    }
    let uncloneable = c.flags & cflags::NO_CLONE != 0
        || matches!(
            c.lc_name.as_bytes(),
            b"generator" | b"fiber" | b"weakreference" | b"weakmap"
        )
        || c.instance_of_name(b"reflector")
        || c.instance_of_name(b"reflectiontype");
    Ok(Value::Bool(!uncloneable))
}

fn rc_is_instance(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(Value::Bool(match a.at(0) {
        Value::Object(o) => object::class_of(&o).is_subclass_of(&c),
        _ => false,
    }))
}

fn rc_is_subclass_of(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let o = class_arg(vm, &a.at(0), "Class")?;
    Ok(Value::Bool(!Rc::ptr_eq(&c, &o) && c.is_subclass_of(&o)))
}

fn rc_implements(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let i = class_arg(vm, &a.at(0), "Interface")?;
    if !i.is_interface() {
        return Err(rexc(vm, &format!("{} is not an interface", i.name)));
    }
    Ok(Value::Bool(c.is_subclass_of(&i)))
}

fn rc_get_parent(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    match &c.parent {
        Some(p) => class_value(vm, &p.clone()),
        None => Ok(Value::Bool(false)),
    }
}

fn rc_get_interface_names(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let l = interfaces_order(vm, &c);
    Ok(Value::Array(ZArray::from_values(
        l.iter().map(|i| Value::Str(i.name.clone())),
    )))
}

fn rc_get_interfaces(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut t = HashTable::new();
    for i in interfaces_order(vm, &c) {
        let v = class_value(vm, &i)?;
        t.insert(ArrayKey::Str(i.name.clone()), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rc_get_trait_names(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(Value::Array(ZArray::from_values(
        trait_names(&c).into_iter().map(Value::Str),
    )))
}

fn rc_get_traits(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut t = HashTable::new();
    for (n, tc) in trait_names(&c).into_iter().zip(c.traits.clone()) {
        let v = class_value(vm, &tc)?;
        t.insert(ArrayKey::Str(n), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rc_get_trait_aliases(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut t = HashTable::new();
    if let Some(d) = &c.decl {
        for r in &d.trait_rules {
            if let TraitRule::Alias {
                trait_name,
                method,
                alias: Some(alias),
                ..
            } = r
            {
                let cn = match trait_name {
                    Some(tn) => vm
                        .find_class(tn.as_bytes())
                        .map_or_else(|| tn.clone(), |x| x.name.clone()),
                    None => {
                        let lc = method.to_ascii_lowercase();
                        c.traits
                            .iter()
                            .find(|t| t.methods.contains_key(lc.as_bytes()))
                            .map(|t| t.name.clone())
                            .unwrap_or_default()
                    }
                };
                let mut v = cn.as_bytes().to_vec();
                v.extend_from_slice(b"::");
                v.extend_from_slice(method.as_bytes());
                t.insert(ArrayKey::Str(alias.clone()), Value::Str(ZStr::from(v)));
            }
        }
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

/// The php-src method flags (`fn_flags`) used by `getMethods()` filters.
pub(crate) fn method_php_flags(m: &Method) -> i64 {
    let mut f = i64::from(m.flags & 0x7f);
    if m.func.flags() & flags::DEPRECATED != 0 {
        f |= super::acc::DEPRECATED;
    }
    f
}

fn filter_arg(a: &Args, default: i64) -> i64 {
    match a.get(0) {
        Some(Value::Int(i)) => i,
        _ => default,
    }
}

fn invoke_closure(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(o) = a.this.clone() else {
        return Ok(Value::Null);
    };
    let Some(mut call) = vm.closure_call(&o) else {
        return Ok(Value::Null);
    };
    let args = a.values.clone();
    let named = std::mem::take(&mut a.named);
    bind_args(vm, &mut call, args, named)?;
    vm.call_pending(call)
}

/// The `Closure::__invoke` handler method (`zend_get_closure_invoke_method`
/// of an object-less closure).
pub(crate) fn closure_invoke_method(c: &Rc<Class>) -> Rc<Method> {
    let s = crate::builtins::sig::parse_sig_borrowed("__invoke(mixed ...$args): mixed");
    let b = crate::func::Builtin::new(
        ZStr::from("__invoke"),
        crate::func::Signature {
            params: s.params,
            defaults: s.defaults,
            required: 0,
            return_type: None,
        },
        flags::PUBLIC | flags::VARIADIC,
        invoke_closure,
    );
    Rc::new(Method {
        name: ZStr::from("__invoke"),
        func: Func::Builtin(Rc::new(b)),
        flags: flags::PUBLIC,
        scope: Rc::downgrade(c),
        proto_scope: Rc::downgrade(c),
    })
}

/// The `__invoke` method of a closure object as a `FnRef`.
fn closure_invoke(vm: &Vm, c: &Rc<Class>, o: &Obj) -> Option<FnRef> {
    let d = vm.closure_data(o)?;
    let m = Rc::new(Method {
        name: ZStr::from("__invoke"),
        func: d.func.clone(),
        flags: flags::PUBLIC,
        scope: Rc::downgrade(c),
        proto_scope: Rc::downgrade(c),
    });
    Some(FnRef {
        func: d.func,
        method: Some(m),
        class: Some(c.clone()),
        closure: None,
    })
}

fn rc_get_methods(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let filter = filter_arg(a, 0x7f);
    let mut out = Vec::new();
    for m in order::methods(&c) {
        if m.visibility() == flags::PRIVATE && !Rc::ptr_eq(&m.scope(), &c) {
            continue;
        }
        if method_php_flags(&m) & filter != 0 {
            out.push(method_value(vm, &m, &c)?);
        }
    }
    if c.lc_name.as_bytes() == b"closure" && c.find_method(b"__invoke").is_none() && filter & 1 != 0
    {
        match obj.as_ref().and_then(|o| closure_invoke(vm, &c, o)) {
            Some(f) => {
                let name = ZStr::from("__invoke");
                out.push(new_refl(
                    vm,
                    "ReflectionMethod",
                    Refl::Function(f),
                    Some(&name),
                    Some(&c.name),
                )?);
            }
            None => {
                let m = closure_invoke_method(&c);
                out.push(method_value(vm, &m, &c)?);
            }
        }
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

fn rc_get_method(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let n = str_arg(a, 0);
    if c.lc_name.as_bytes() == b"closure" && n.eq_ignore_ascii_case(b"__invoke") {
        if let Some(o) = &obj {
            if let Some(f) = closure_invoke(vm, &c, o) {
                let name = ZStr::from("__invoke");
                return new_refl(
                    vm,
                    "ReflectionMethod",
                    Refl::Function(f),
                    Some(&name),
                    Some(&c.name),
                );
            }
        }
    }
    match c.find_method(n.as_bytes()).cloned() {
        Some(m) => method_value(vm, &m, &c),
        None if c.lc_name.as_bytes() == b"closure" && n.eq_ignore_ascii_case(b"__invoke") => {
            let m = closure_invoke_method(&c);
            method_value(vm, &m, &c)
        }
        None => Err(rexc(
            vm,
            &format!(
                "Method {}::{}() does not exist",
                c.name,
                n.to_string_lossy()
            ),
        )),
    }
}

fn rc_has_method(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    Ok(Value::Bool(
        c.find_method(n.as_bytes()).is_some()
            || (c.lc_name.as_bytes() == b"closure" && n.eq_ignore_ascii_case(b"__invoke")),
    ))
}

fn rc_get_constructor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    match c.magic.construct.clone() {
        Some(m) => method_value(vm, &m, &c),
        None => Ok(Value::Null),
    }
}

/// `reflection_property_factory`.
pub(crate) fn prop_value(
    vm: &mut Vm,
    class: &Rc<Class>,
    name: &ZStr,
    meta: Option<PropMeta>,
) -> VmResult<Value> {
    let decl = meta.as_ref().map_or_else(
        || class.name.clone(),
        |p| {
            vm.find_class(p.class.as_bytes())
                .map_or_else(|| p.class.clone(), |c| c.name.clone())
        },
    );
    let r = PropRef {
        class: class.clone(),
        name: name.clone(),
        meta: meta.map(Rc::new),
    };
    new_refl(
        vm,
        "ReflectionProperty",
        Refl::Prop(r),
        Some(name),
        Some(&decl),
    )
}

/// Whether `o` has a dynamic property `name`.
pub(crate) fn has_dynamic(o: &Obj, name: &ZStr) -> bool {
    object::data(o).dynamic.borrow().as_ref().is_some_and(|t| {
        t.contains_key(&ArrayKey::from_zstr(name)) || t.contains_key(&ArrayKey::Str(name.clone()))
    })
}

fn rc_get_properties(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let filter = filter_arg(a, 0x17);
    let mut out = Vec::new();
    for p in order::props(vm, &c) {
        if super::property::php_flags(&p) & filter != 0 {
            let n = p.name.clone();
            out.push(prop_value(vm, &c, &n, Some(p))?);
        }
    }
    if let (Some(o), true) = (obj, filter & 1 != 0) {
        let names: Vec<ZStr> = object::data(&o)
            .dynamic
            .borrow()
            .as_ref()
            .map(|t| {
                t.iter()
                    .filter_map(|(k, _)| match k {
                        ArrayKey::Str(s) => Some(s.clone()),
                        ArrayKey::Int(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for n in names {
            out.push(prop_value(vm, &c, &n, None)?);
        }
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

fn rc_get_property(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let n = str_arg(a, 0);
    if let Some(p) = order::find_prop(vm, &c, n.as_bytes()) {
        return prop_value(vm, &c, &n, Some(p));
    }
    if let Some(o) = &obj {
        if has_dynamic(o, &n) {
            return prop_value(vm, &c, &n, None);
        }
    }
    if let Some(pos) = crate::call::find_double_colon(n.as_bytes()) {
        let cn = ZStr::from(&n.as_bytes()[..pos]);
        let pn = ZStr::from(&n.as_bytes()[pos + 2..]);
        let Some(c2) = vm.lookup_class(&cn)? else {
            let e = rexc(
                vm,
                &format!("Class \"{}\" does not exist", cn.to_string_lossy()),
            );
            set_code(&e, -1);
            return Err(e);
        };
        if !c.is_subclass_of(&c2) {
            let e = rexc(
                vm,
                &format!(
                    "Fully qualified property name {}::${} does not specify a base class of {}",
                    c2.name,
                    pn.to_string_lossy(),
                    c.name
                ),
            );
            set_code(&e, -1);
            return Err(e);
        }
        if let Some(p) = order::find_prop(vm, &c2, pn.as_bytes()) {
            return prop_value(vm, &c2, &pn, Some(p));
        }
        return Err(rexc(
            vm,
            &format!(
                "Property {}::${} does not exist",
                c2.name,
                pn.to_string_lossy()
            ),
        ));
    }
    Err(rexc(
        vm,
        &format!(
            "Property {}::${} does not exist",
            c.name,
            n.to_string_lossy()
        ),
    ))
}

/// Sets the `code` of a thrown exception.
pub(crate) fn set_code(e: &crate::vm::Throw, code: i64) {
    if let crate::vm::Throw::Exception(ex) = e {
        crate::exceptions::set_base_prop(ex, "code", Value::Int(code));
    }
}

fn rc_has_property(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, obj) = this_class_obj(vm, a)?;
    let n = str_arg(a, 0);
    if order::find_prop(vm, &c, n.as_bytes()).is_some() {
        return Ok(Value::Bool(true));
    }
    if let Some(o) = &obj {
        if has_dynamic(o, &n) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

/// Evaluates every constant of `c` (`zend_update_class_constant`).
fn eval_consts(vm: &mut Vm, c: &Rc<Class>) -> VmResult<Vec<(Rc<ClassConst>, Value)>> {
    let mut out = Vec::new();
    for cc in order::consts(c) {
        let v = super::constant::const_value_of(vm, c, &cc)?;
        out.push((cc, v));
    }
    Ok(out)
}

fn const_flag_ok(cc: &ClassConst, filter: i64) -> bool {
    i64::from(cc.flags & 0x27) & filter != 0
}

fn rc_get_constants(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let filter = filter_arg(a, 7);
    let mut t = HashTable::new();
    for (cc, v) in eval_consts(vm, &c)? {
        if const_flag_ok(&cc, filter) {
            t.insert(ArrayKey::Str(cc.name.clone()), v);
        }
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rc_get_constant(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    let all = eval_consts(vm, &c)?;
    match all.into_iter().find(|(cc, _)| cc.name == n) {
        Some((_, v)) => Ok(v),
        None => {
            vm.deprecated(
                "ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists",
            )?;
            Ok(Value::Bool(false))
        }
    }
}

fn rc_has_constant(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    Ok(Value::Bool(c.find_const(n.as_bytes()).is_some()))
}

fn rc_get_reflection_constants(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let filter = filter_arg(a, 7);
    let mut out = Vec::new();
    for cc in order::consts(&c) {
        if const_flag_ok(&cc, filter) {
            out.push(super::constant::class_constant_value(vm, &c, &cc, false)?);
        }
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

fn rc_get_reflection_constant(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    match c.find_const(n.as_bytes()).cloned() {
        Some(cc) => super::constant::class_constant_value(vm, &c, &cc, false),
        None => Ok(Value::Bool(false)),
    }
}

/// Binds positional and named arguments to a pending call.
pub(crate) fn bind_args(
    vm: &mut Vm,
    call: &mut PendingCall,
    args: Vec<Value>,
    named: Vec<(ZStr, Value)>,
) -> VmResult<()> {
    call.args = args.into_iter().map(Some).collect();
    for (k, v) in named {
        let pos = call
            .func
            .params()
            .iter()
            .position(|p| p.name == k && !p.variadic);
        match pos {
            Some(p) => {
                if call.args.len() <= p {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: position of a declared parameter"
                    )]
                    call.args.resize(p + 1, None);
                } else if call.args[p].is_some() {
                    return Err(vm.make_throw(
                        "Error",
                        &format!(
                            "Named parameter ${} overwrites previous argument",
                            k.to_string_lossy()
                        ),
                    ));
                }
                call.args[p] = Some(v);
            }
            None => {
                if !call.func.is_variadic() {
                    return Err(vm.make_throw(
                        "Error",
                        &format!("Unknown named parameter ${}", k.to_string_lossy()),
                    ));
                }
                call.named.push((k, v));
            }
        }
        call.has_named = true;
    }
    Ok(())
}

/// Splits an argument array into positional and named arguments.
pub(crate) fn split_args(vm: &mut Vm, arr: &ZArray) -> VmResult<(Vec<Value>, Vec<(ZStr, Value)>)> {
    let mut pos = Vec::new();
    let mut named: Vec<(ZStr, Value)> = Vec::new();
    for (k, v) in arr.iter() {
        match k {
            ArrayKey::Int(_) => {
                if !named.is_empty() {
                    return Err(vm.make_throw(
                        "Error",
                        "Cannot use positional argument after named argument during unpacking",
                    ));
                }
                pos.push(v.clone());
            }
            ArrayKey::Str(s) => named.push((s.clone(), v.clone())),
        }
    }
    Ok((pos, named))
}

fn new_instance(
    vm: &mut Vm,
    c: &Rc<Class>,
    args: Vec<Value>,
    named: Vec<(ZStr, Value)>,
) -> VmResult<Value> {
    let o = vm.instantiate(c)?;
    match c.magic.construct.clone() {
        Some(m) => {
            if m.visibility() != flags::PUBLIC {
                mark_ctor_failed(&o);
                return Err(rexc(
                    vm,
                    &format!("Access to non-public constructor of class {}", c.name),
                ));
            }
            let mut call = PendingCall {
                this: Some(o.clone()),
                scope: Some(m.scope()),
                called: Some(c.clone()),
                ..PendingCall::new(m.func.clone())
            };
            if let Err(e) = bind_args(vm, &mut call, args, named) {
                mark_ctor_failed(&o);
                return Err(e);
            }
            if let Err(e) = vm.call_pending(call) {
                mark_ctor_failed(&o);
                return Err(e);
            }
        }
        None => {
            if !args.is_empty() || !named.is_empty() {
                return Err(rexc(
                    vm,
                    &format!("Class {} does not have a constructor, so you cannot pass any constructor arguments", c.name),
                ));
            }
        }
    }
    Ok(Value::Object(o))
}

/// `zend_object_store_ctor_failed`: the destructor must not run.
pub(crate) fn mark_ctor_failed(o: &Obj) {
    let d = object::data(o);
    d.flags.set(d.flags.get() | object::oflags::DESTRUCTED);
}

fn rc_new_instance(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let args = a.values.iter().map(Value::deref).collect();
    let named = std::mem::take(&mut a.named);
    new_instance(vm, &c, args, named)
}

fn rc_new_instance_args(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let (args, named) = match a.get(0) {
        Some(Value::Array(arr)) => split_args(vm, &arr)?,
        _ => (Vec::new(), Vec::new()),
    };
    new_instance(vm, &c, args, named)
}

fn rc_new_instance_without_ctor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    if c.flags & cflags::INTERNAL != 0
        && c.flags & cflags::FINAL != 0
        && c.hooks.as_ref().is_some_and(|h| h.create.is_some())
        || matches!(
            c.lc_name.as_bytes(),
            b"generator"
                | b"closure"
                | b"fiber"
                | b"weakreference"
                | b"weakmap"
                // `ext/standard/io_poll.c` gives these final classes a
                // `create_object`; their binding sets the state up in the
                // constructor instead.
                | b"io\\poll\\context"
                | b"io\\poll\\watcher"
                | b"io\\poll\\streampollhandle"
        )
    {
        return Err(rexc(
            vm,
            &format!(
                "Class {} is an internal class marked as final that cannot be instantiated without invoking its constructor",
                c.name
            ),
        ));
    }
    let o = vm.instantiate(&c)?;
    Ok(Value::Object(o))
}

fn rc_get_attributes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut attrs = c.attributes.clone();
    if c.flags & cflags::INTERNAL != 0 && attrs.is_empty() {
        // Internal attribute classes carry `#[Attribute(flags)]`.
        if let Some(fl) = super::attribute::attribute_flags(vm, &c)? {
            attrs.push(crate::class::AttributeInfo {
                name: ZStr::from("Attribute"),
                args: vec![(None, crate::class::ConstExpr::Value(Value::Int(fl)))],
                line: 0,
                strict: false,
            });
        }
    }
    super::attribute::attributes_array_in(
        vm,
        &attrs,
        super::target::CLASS,
        Some(&c),
        c.filename.clone(),
        a,
    )
}

fn rc_get_doc_comment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(c.doc_comment.clone().map_or(Value::Bool(false), Value::Str))
}

fn rc_get_file_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(if c.flags & cflags::INTERNAL == 0 {
        c.filename.clone().map_or(Value::Bool(false), Value::Str)
    } else {
        Value::Bool(false)
    })
}

fn rc_get_start_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(if c.flags & cflags::INTERNAL == 0 {
        Value::Int(i64::from(c.line_start))
    } else {
        Value::Bool(false)
    })
}

fn rc_get_end_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    Ok(if c.flags & cflags::INTERNAL == 0 {
        Value::Int(i64::from(c.line_end))
    } else {
        Value::Bool(false)
    })
}

fn rc_get_static_property_value(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    update_class_constants(vm, &c)?;
    let sp = c.find_static(n.as_bytes()).cloned();
    let visible = sp.as_ref().is_some_and(|s| {
        s.flags & flags::PRIVATE == 0 || s.class.eq_ignore_ascii_case(c.name.as_bytes())
    });
    if visible {
        let cell = with_scope(vm, Some(c.clone()), |vm| vm.static_prop_cell(&c, &n))?;
        let v = cell.borrow().deref();
        if static_uninit(&c, n.as_bytes(), &v) {
            if a.has(1) {
                return Ok(a.at(1));
            }
            return Err(vm.make_throw(
                "Error",
                &format!(
                    "Typed property {}::${} must not be accessed before initialization",
                    c.name,
                    n.to_string_lossy()
                ),
            ));
        }
        return Ok(v);
    }
    if a.has(1) {
        return Ok(a.at(1));
    }
    Err(rexc(
        vm,
        &format!(
            "Property {}::${} does not exist",
            c.name,
            n.to_string_lossy()
        ),
    ))
}

fn rc_set_static_property_value(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    update_class_constants(vm, &c)?;
    let Some(sp) = c.find_static(n.as_bytes()).cloned() else {
        return Err(rexc(
            vm,
            &format!(
                "Property {}::${} does not exist",
                c.name,
                n.to_string_lossy()
            ),
        ));
    };
    let cell = with_scope(vm, Some(c.clone()), |vm| vm.static_prop_cell(&c, &n))?;
    let mut v = a.at(1);
    if let Some(ty) = &sp.ty {
        v = super::property::coerce_static(vm, &sp, ty, v)?;
    }
    let r = cell.borrow().clone();
    match r {
        // `zend_assign_to_variable()`: a reference with typed sources
        // checks the value against every one of them (non-strict).
        Value::Reference(r) => {
            vm.assign_ref_ex(&r, v, Some(false))?;
        }
        _ => *cell.borrow_mut() = v,
    }
    Ok(Value::Null)
}

fn rc_get_static_properties(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    update_class_constants(vm, &c)?;
    let mut t = HashTable::new();
    for p in order::props(vm, &c) {
        if !p.is_static {
            continue;
        }
        let cell = with_scope(vm, Some(c.clone()), |vm| vm.static_prop_cell(&c, &p.name))?;
        let v = cell.borrow().deref();
        if static_uninit(&c, p.name.as_bytes(), &v) {
            continue;
        }
        t.insert(ArrayKey::Str(p.name.clone()), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rc_get_default_properties(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    update_class_constants(vm, &c)?;
    let props = order::props(vm, &c);
    let mut t = HashTable::new();
    for p in props.iter().filter(|p| p.is_static) {
        let cell = with_scope(vm, Some(c.clone()), |vm| vm.static_prop_cell(&c, &p.name))?;
        if p.ty.is_some() && cell.borrow().is_null() && p.default.is_none() {
            continue;
        }
        t.insert(ArrayKey::Str(p.name.clone()), cell.borrow().deref());
    }
    let defs = vm.class_defaults(&c)?;
    for p in props.iter().filter(|p| !p.is_static) {
        if p.flags & flags::VIRTUAL != 0 {
            continue;
        }
        let Some(pi) = &p.info else { continue };
        if let Some(Some(v)) = defs.get(pi.slot as usize) {
            t.insert(ArrayKey::Str(p.name.clone()), v.deref());
        }
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rc_get_modifiers(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut m = 0;
    if c.flags & cflags::EXPLICIT_ABSTRACT != 0 {
        m |= super::acc::EXPLICIT_ABSTRACT_CLASS;
    }
    if c.flags & cflags::FINAL != 0 {
        m |= super::acc::FINAL;
    }
    if c.flags & cflags::READONLY != 0 {
        m |= super::acc::READONLY_CLASS;
    }
    Ok(Value::Int(m))
}

fn rc_get_extension(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    match super::misc::class_module(vm, &c) {
        Some(m) if c.flags & cflags::INTERNAL != 0 => super::misc::extension_value(vm, &m),
        _ => Ok(Value::Null),
    }
}

fn rc_get_extension_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    match super::misc::class_module(vm, &c) {
        Some(m) if c.flags & cflags::INTERNAL != 0 => Ok(zs(&m)),
        _ => Ok(Value::Bool(false)),
    }
}

fn clone_error(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(rexc(vm, "Cannot clone object using __clone()"))
}

// ---------------------------------------------------------------------
// ReflectionEnum
// ---------------------------------------------------------------------

fn re_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    rc_construct(vm, a)?;
    let c = this_class(vm, a)?;
    if !c.is_enum() {
        let e = rexc(vm, &format!("Class \"{}\" is not an enum", c.name));
        set_code(&e, -1);
        return Err(e);
    }
    Ok(Value::Null)
}

fn re_get_cases(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let mut out = Vec::new();
    for cc in order::consts(&c) {
        if cc.is_case {
            out.push(super::constant::class_constant_value(vm, &c, &cc, true)?);
        }
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

fn re_get_case(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    match c.find_const(n.as_bytes()).cloned() {
        Some(cc) if cc.is_case => super::constant::class_constant_value(vm, &c, &cc, true),
        Some(_) => Err(rexc(
            vm,
            &format!("{}::{} is not a case", c.name, n.to_string_lossy()),
        )),
        None => Err(rexc(
            vm,
            &format!("Case {}::{} does not exist", c.name, n.to_string_lossy()),
        )),
    }
}

fn re_has_case(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    let n = str_arg(a, 0);
    Ok(Value::Bool(
        c.find_const(n.as_bytes()).is_some_and(|cc| cc.is_case),
    ))
}

fn re_is_backed(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(this_class(vm, a)?.backing.is_some()))
}

fn re_get_backing_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_class(vm, a)?;
    match c.backing {
        Some(m) => super::types::type_value(
            vm,
            &TypeDecl::simple(m & (mask::LONG | mask::STRING)),
            None,
            false,
        ),
        None => Ok(Value::Null),
    }
}

/// `Func` of a reflected function (for the lazy-object helpers).
#[allow(dead_code)]
pub(crate) fn func_name(f: &Func) -> ZStr {
    f.name().clone()
}

/// Registers `ReflectionClass` and `ReflectionObject`.
#[allow(clippy::too_many_lines)]
pub(crate) fn register(vm: &mut Vm) {
    use super::lazy as lz;
    ClassBuilder::new("ReflectionClass")
        .implements(vm, &["Reflector"])
        .hooks(super::constant::hooks())
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .prop("name", RO, Some("string"), None)
        .constant("IS_IMPLICIT_ABSTRACT", Value::Int(16))
        .constant("IS_EXPLICIT_ABSTRACT", Value::Int(64))
        .constant("IS_FINAL", Value::Int(32))
        .constant("IS_READONLY", Value::Int(65536))
        .constant("SKIP_INITIALIZATION_ON_SERIALIZE", Value::Int(8))
        .constant("SKIP_DESTRUCTOR", Value::Int(16))
        .method("__clone(): void", PRIV, clone_error)
        .method(
            "__construct(object|string $objectOrClass)",
            PUB,
            rc_construct,
        )
        .method("__toString(): string", PUB, rc_to_string)
        .method("getName(): string", PUB, rc_get_name)
        .method("isInternal(): bool", PUB, rc_is_internal)
        .method("isUserDefined(): bool", PUB, rc_is_user)
        .method("isAnonymous(): bool", PUB, rc_is_anon)
        .method("isInstantiable(): bool", PUB, rc_is_instantiable)
        .method("isCloneable(): bool", PUB, rc_is_cloneable)
        .method("getFileName(): string|false", PUB, rc_get_file_name)
        .method("getStartLine(): int|false", PUB, rc_get_start_line)
        .method("getEndLine(): int|false", PUB, rc_get_end_line)
        .method("getDocComment(): string|false", PUB, rc_get_doc_comment)
        .method(
            "getConstructor(): ?ReflectionMethod",
            PUB,
            rc_get_constructor,
        )
        .method("hasMethod(string $name): bool", PUB, rc_has_method)
        .method(
            "getMethod(string $name): ReflectionMethod",
            PUB,
            rc_get_method,
        )
        .method(
            "getMethods(?int $filter = null): array",
            PUB,
            rc_get_methods,
        )
        .method("hasProperty(string $name): bool", PUB, rc_has_property)
        .method(
            "getProperty(string $name): ReflectionProperty",
            PUB,
            rc_get_property,
        )
        .method(
            "getProperties(?int $filter = null): array",
            PUB,
            rc_get_properties,
        )
        .method("hasConstant(string $name): bool", PUB, rc_has_constant)
        .method(
            "getConstants(?int $filter = null): array",
            PUB,
            rc_get_constants,
        )
        .method(
            "getReflectionConstants(?int $filter = null): array",
            PUB,
            rc_get_reflection_constants,
        )
        .method("getConstant(string $name): mixed", PUB, rc_get_constant)
        .method(
            "getReflectionConstant(string $name): ReflectionClassConstant|false",
            PUB,
            rc_get_reflection_constant,
        )
        .method("getInterfaces(): array", PUB, rc_get_interfaces)
        .method("getInterfaceNames(): array", PUB, rc_get_interface_names)
        .method("isInterface(): bool", PUB, rc_is_interface)
        .method("getTraits(): array", PUB, rc_get_traits)
        .method("getTraitNames(): array", PUB, rc_get_trait_names)
        .method("getTraitAliases(): array", PUB, rc_get_trait_aliases)
        .method("isTrait(): bool", PUB, rc_is_trait)
        .method("isEnum(): bool", PUB, rc_is_enum)
        .method("isAbstract(): bool", PUB, rc_is_abstract)
        .method("isFinal(): bool", PUB, rc_is_final)
        .method("isReadOnly(): bool", PUB, rc_is_readonly)
        .method("getModifiers(): int", PUB, rc_get_modifiers)
        .method("isInstance(object $object): bool", PUB, rc_is_instance)
        .method("newInstance(mixed ...$args): object", PUB, rc_new_instance)
        .method(
            "newInstanceWithoutConstructor(): object",
            PUB,
            rc_new_instance_without_ctor,
        )
        .method(
            "newInstanceArgs(array $args = []): ?object",
            PUB,
            rc_new_instance_args,
        )
        .method(
            "newLazyGhost(callable $initializer, int $options = 0): object",
            PUB,
            lz::rc_new_lazy_ghost,
        )
        .method(
            "newLazyProxy(callable $factory, int $options = 0): object",
            PUB,
            lz::rc_new_lazy_proxy,
        )
        .method(
            "resetAsLazyGhost(object $object, callable $initializer, int $options = 0): void",
            PUB,
            lz::rc_reset_as_lazy_ghost,
        )
        .method(
            "resetAsLazyProxy(object $object, callable $factory, int $options = 0): void",
            PUB,
            lz::rc_reset_as_lazy_proxy,
        )
        .method(
            "initializeLazyObject(object $object): object",
            PUB,
            lz::rc_initialize_lazy_object,
        )
        .method(
            "isUninitializedLazyObject(object $object): bool",
            PUB,
            lz::rc_is_uninitialized_lazy_object,
        )
        .method(
            "markLazyObjectAsInitialized(object $object): object",
            PUB,
            lz::rc_mark_lazy_object_as_initialized,
        )
        .method(
            "getLazyInitializer(object $object): ?callable",
            PUB,
            lz::rc_get_lazy_initializer,
        )
        .method(
            "getParentClass(): ReflectionClass|false",
            PUB,
            rc_get_parent,
        )
        .method(
            "isSubclassOf(ReflectionClass|string $class): bool",
            PUB,
            rc_is_subclass_of,
        )
        .method(
            "getStaticProperties(): array",
            PUB,
            rc_get_static_properties,
        )
        .method(
            "getStaticPropertyValue(string $name, mixed $default = UNKNOWN): mixed",
            PUB,
            rc_get_static_property_value,
        )
        .method(
            "setStaticPropertyValue(string $name, mixed $value): void",
            PUB,
            rc_set_static_property_value,
        )
        .method(
            "getDefaultProperties(): array",
            PUB,
            rc_get_default_properties,
        )
        .method("isIterable(): bool", PUB, rc_is_iterable)
        .method("isIterateable(): bool", PUB, rc_is_iterable)
        .method(
            "implementsInterface(ReflectionClass|string $interface): bool",
            PUB,
            rc_implements,
        )
        .method(
            "getExtension(): ?ReflectionExtension",
            PUB,
            rc_get_extension,
        )
        .method(
            "getExtensionName(): string|false",
            PUB,
            rc_get_extension_name,
        )
        .method("inNamespace(): bool", PUB, rc_in_ns)
        .method("getNamespaceName(): string", PUB, rc_get_ns)
        .method("getShortName(): string", PUB, rc_get_short_name)
        .method(
            "getAttributes(?string $name = null, int $flags = 0): array",
            PUB,
            rc_get_attributes,
        )
        .register(vm);
    ClassBuilder::new("ReflectionObject")
        .extends(vm, "ReflectionClass")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method("__construct(object $object)", PUB, ro_construct)
        .register(vm);
}

/// Registers `ReflectionEnum`.
pub(crate) fn register_enum(vm: &mut Vm) {
    ClassBuilder::new("ReflectionEnum")
        .extends(vm, "ReflectionClass")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method(
            "__construct(object|string $objectOrClass)",
            PUB,
            re_construct,
        )
        .method("hasCase(string $name): bool", PUB, re_has_case)
        .method(
            "getCase(string $name): ReflectionEnumUnitCase",
            PUB,
            re_get_case,
        )
        .method("getCases(): array", PUB, re_get_cases)
        .method("isBacked(): bool", PUB, re_is_backed)
        .method(
            "getBackingType(): ?ReflectionNamedType",
            PUB,
            re_get_backing_type,
        )
        .register(vm);
}
