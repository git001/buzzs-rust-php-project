// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ReflectionFunctionAbstract`, `ReflectionFunction` and
//! `ReflectionMethod`.

use super::class::{bind_args, class_arg, method_php_flags, split_args};
use super::{
    class_value, method_value, new_refl, rexc, set_name_props, this, this_refl, zs, FnRef, Refl,
    PRIV, PUB, RO, STAT,
};
use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class};
use crate::func::{flags, Args, Func, Method};
use crate::object;
use crate::types::TypeDecl;
use crate::vm::{PendingCall, Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use std::rc::Rc;

/// Internal methods whose return type is tentative (`@tentative-return-type`
/// in the stubs of the reflection classes).
const TENTATIVE: &[(&str, &str)] = &[
    ("reflectionclassconstant", "getname getvalue ispublic isprivate isprotected getmodifiers getdeclaringclass getdoccomment"),
    ("reflectionclass", "getname isinternal isuserdefined isanonymous isinstantiable iscloneable getfilename getstartline getendline getdoccomment getconstructor hasmethod getmethod getmethods hasproperty getproperty getproperties hasconstant getconstants getreflectionconstants getconstant getreflectionconstant getinterfaces getinterfacenames isinterface gettraits gettraitnames gettraitaliases istrait isabstract isfinal getmodifiers isinstance newinstance newinstancewithoutconstructor newinstanceargs getparentclass issubclassof getstaticproperties getstaticpropertyvalue setstaticpropertyvalue getdefaultproperties isiterable isiterateable implementsinterface getextension getextensionname innamespace getnamespacename getshortname"),
    ("reflectionextension", "getname getversion getfunctions getconstants getinientries getclasses getclassnames getdependencies info ispersistent istemporary"),
    ("reflectionfunctionabstract", "innamespace isclosure isdeprecated isinternal isuserdefined isgenerator isvariadic isstatic getclosurethis getclosurescopeclass getclosurecalledclass getdoccomment getendline getextension getextensionname getfilename getname getnamespacename getnumberofparameters getnumberofrequiredparameters getparameters getshortname getstartline getstaticvariables returnsreference hasreturntype getreturntype"),
    ("reflectionfunction", "isdisabled invoke invokeargs getclosure"),
    ("reflection", "getmodifiernames"),
    ("reflectionmethod", "ispublic isprivate isprotected isabstract isfinal isconstructor isdestructor getclosure getmodifiers invoke invokeargs getdeclaringclass getprototype setaccessible"),
    ("reflectionnamedtype", "getname isbuiltin"),
    ("reflectionparameter", "getname ispassedbyreference canbepassedbyvalue getdeclaringfunction getdeclaringclass getclass hastype gettype isarray iscallable allowsnull getposition isoptional isdefaultvalueavailable getdefaultvalue isdefaultvalueconstant getdefaultvalueconstantname isvariadic"),
    ("reflectionproperty", "getname getvalue setvalue isinitialized ispublic isprivate isprotected isstatic isdefault getmodifiers getdeclaringclass getdoccomment setaccessible gettype hastype getdefaultvalue"),
    ("reflectiontype", "allowsnull"),
    ("reflectionzendextension", "getname getversion getauthor geturl getcopyright"),
];

fn is_tentative(f: &FnRef) -> bool {
    let (Func::Builtin(_), Some(m)) = (&f.func, &f.method) else {
        return false;
    };
    let scope = m.scope();
    let lc = String::from_utf8_lossy(&m.name.to_ascii_lowercase()).into_owned();
    if scope.lc_name.starts_with(b"reflect") {
        return TENTATIVE.iter().any(|(c, ms)| {
            scope.lc_name.as_bytes() == c.as_bytes() && ms.split(' ').any(|x| x == lc)
        });
    }
    // Other internal classes: overridable (non-final, non-private) methods
    // have tentative return types in the php-src stubs; constructors and
    // `__toString` (Stringable) declare real ones, as do the enum
    // interfaces.
    scope.flags & cflags::INTERNAL != 0
        && m.flags & (flags::FINAL | flags::PRIVATE) == 0
        && scope.flags & cflags::FINAL == 0
        && !matches!(
            lc.as_str(),
            "__construct" | "__destruct" | "__tostring" | "__invoke"
        )
        && !matches!(
            scope.lc_name.as_bytes(),
            b"unitenum" | b"backedenum" | b"stringable"
        )
}

/// The return type and whether it is tentative.
pub(crate) fn return_type(_vm: &Vm, f: &FnRef) -> Option<(TypeDecl, bool)> {
    let t = f.func.return_type()?.clone();
    Some((t, is_tentative(f)))
}

/// The doc comment of parameter `idx` (not tracked by the compiler).
pub(crate) fn param_doc_comment(vm: &Vm, f: &FnRef, idx: usize) -> Option<ZStr> {
    let Func::User(u) = &f.func else { return None };
    if u.hook_prop.is_some() && u.params.len() <= idx {
        return None;
    }
    super::source::param_doc_comment(
        vm,
        u.code.filename.as_bytes(),
        u.line_start,
        u.line_end,
        idx,
    )
    .map(ZStr::from)
}

/// The file declaring a user function.
pub(crate) fn fn_file(f: &FnRef) -> Option<ZStr> {
    match &f.func {
        Func::User(u) => Some(u.code.filename.clone()),
        Func::Builtin(_) => None,
    }
}

/// The key of a native function or method in [`Vm::builtin_attributes`] /
/// [`Vm::builtin_doc_comments`]: lowercase `name` or `class::method`.
pub(crate) fn builtin_key(f: &FnRef) -> Option<ZStr> {
    let Func::Builtin(b) = &f.func else {
        return None;
    };
    let name = b.name.as_bytes().to_ascii_lowercase();
    Some(match &f.method {
        Some(m) => {
            let mut k = m.scope().lc_name.as_bytes().to_vec();
            k.extend_from_slice(b"::");
            k.extend_from_slice(&name);
            ZStr::from(k)
        }
        None => ZStr::from(name),
    })
}

/// The doc comment of a native function exposed by its stub.
pub(crate) fn builtin_doc_comment(vm: &Vm, f: &FnRef) -> Option<ZStr> {
    builtin_key(f).and_then(|k| vm.builtin_doc_comments.get(&k).cloned())
}

/// The reflected function.
pub(crate) fn this_fn(vm: &mut Vm, a: &Args) -> VmResult<FnRef> {
    match this_refl(vm, a)? {
        Refl::Function(f) => Ok(f),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// Static variables (closure `use` variables first) with their current
/// values (`op_array.static_variables`).
pub(crate) fn static_vars(vm: &Vm, f: &FnRef) -> Vec<(ZStr, Value)> {
    let Func::User(u) = &f.func else {
        return Vec::new();
    };
    let mut out: Vec<(ZStr, Value)> = Vec::new();
    let cdata = f.closure.as_ref().and_then(|c| vm.closure_data(c));
    if u.is_closure() {
        for us in &u.uses {
            let v = cdata.as_ref().and_then(|d| {
                d.vars
                    .iter()
                    .find(|(cv, _)| *cv == us.cv)
                    .map(|(_, v)| v.deref())
            });
            match v {
                Some(v) => out.push((us.name.clone(), v)),
                None if !us.implicit => out.push((us.name.clone(), Value::Null)),
                None => {}
            }
        }
    }
    let store = match &cdata {
        Some(d) => d.statics.borrow().clone(),
        None => u.statics.borrow().clone(),
    };
    for (idx, decl) in u.code.static_vars.iter().enumerate() {
        let name = decl.name.clone();
        let v = match store.get(idx).cloned().flatten() {
            Some(r) => Value::Reference(r),
            None => decl
                .init
                .clone()
                .or_else(|| static_const_init(vm, f, u, &name))
                .unwrap_or(Value::Null),
        };
        if let Some(e) = out.iter_mut().find(|(n, _)| *n == name) {
            e.1 = v;
        } else {
            out.push((name, v));
        }
    }
    out
}

/// The value of a constant-expression `static` initializer that has not
/// run yet (php-src stores these as constant ASTs in `static_variables`).
fn static_const_init(
    vm: &Vm,
    f: &FnRef,
    u: &crate::func::UserFunction,
    name: &ZStr,
) -> Option<Value> {
    let inits = super::source::static_initializers(
        vm,
        u.code.filename.as_bytes(),
        u.line_start,
        u.line_end,
    );
    let (_, e) = inits
        .into_iter()
        .find(|(n, _)| n.as_slice() == name.as_bytes())?;
    let scope = f.scope(vm);
    const_eval(vm, &e, scope.as_ref())
}

/// Evaluates a side-effect-free constant expression (literals, arrays,
/// global and class constants) without running code.
fn const_eval(vm: &Vm, e: &php_parser::ast::Expr, scope: Option<&Rc<Class>>) -> Option<Value> {
    use php_parser::ast::{ClassRef, ExprKind, MemberName, NameKind};
    if let Some(v) = super::source::literal_value(e) {
        return Some(v);
    }
    match &e.kind {
        ExprKind::Array { items, .. } => {
            let mut t = HashTable::new();
            for it in items {
                let it = it.as_ref()?;
                let v = const_eval(vm, &it.value, scope)?;
                match &it.key {
                    Some(k) => {
                        let key = match const_eval(vm, k, scope)? {
                            Value::Int(i) => ArrayKey::Int(i),
                            Value::Str(s) => ArrayKey::from_zstr(&s),
                            _ => return None,
                        };
                        t.insert(key, v);
                    }
                    None => {
                        let _ = t.push(v);
                    }
                }
            }
            Some(Value::Array(ZArray::from_table(t)))
        }
        ExprKind::Const(n) => {
            let key = ZStr::from(n.name.as_slice());
            vm.constants.get(&key).cloned().or_else(|| {
                let short = n.name.rsplit(|&c| c == b'\\').next()?;
                (n.kind != NameKind::FullyQualified)
                    .then(|| vm.constants.get(&ZStr::from(short)).cloned())
                    .flatten()
            })
        }
        ExprKind::ClassConst { class, name } => {
            let ClassRef::Name(cn) = class.as_ref() else {
                return None;
            };
            let MemberName::Ident(id) = name else {
                return None;
            };
            let lc = cn.name.to_ascii_lowercase();
            let cls = match lc.as_slice() {
                b"self" | b"static" => scope.cloned()?,
                b"parent" => scope?.parent.clone()?,
                _ => vm.find_class(&cn.name)?,
            };
            let cc = cls.find_const(&id.name)?;
            let v = cc.value.borrow().clone();
            match v {
                Some(v) => Some(v),
                None => match &cc.expr {
                    crate::class::ConstExpr::Value(v) => Some(v.clone()),
                    crate::class::ConstExpr::Code(_) => None,
                },
            }
        }
        _ => None,
    }
}

fn rf_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let f = match a.at(0) {
        Value::Object(c) => match vm.closure_data(&c) {
            Some(d) => FnRef::of_closure(&c, &d),
            None => {
                return Err(vm.make_throw(
                    "TypeError",
                    &format!(
                        "ReflectionFunction::__construct(): Argument #1 ($function) must be of type Closure|string, {} given",
                        object::class_of(&c).name
                    ),
                ))
            }
        },
        v => {
            let s = vm.to_zstr(&v)?;
            let n = s.as_bytes().strip_prefix(b"\\").unwrap_or(s.as_bytes());
            match vm.functions.get(n.to_ascii_lowercase().as_slice()).cloned() {
                Some(f) => FnRef::function(f),
                None => return Err(rexc(vm, &format!("Function {}() does not exist", s.to_string_lossy()))),
            }
        }
    };
    let name = f.name();
    set_name_props(&o, Some(&name), None);
    object::set_internal(&o, Refl::Function(f));
    Ok(Value::Null)
}

fn rf_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.class.clone();
    Ok(Value::Str(ZStr::from(super::export::function_string(
        vm,
        &f,
        scope.as_ref(),
        "",
    ))))
}

/// A real (non-fake) closure: `(fn_flags & (CLOSURE|FAKE_CLOSURE)) == CLOSURE`.
fn is_real_closure(f: &FnRef) -> bool {
    f.method.is_none() && f.func.flags() & (flags::CLOSURE | flags::FAKE_CLOSURE) == flags::CLOSURE
}

fn rf_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(this_fn(vm, a)?.name()))
}
fn rf_get_short_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if is_real_closure(&f) {
        return Ok(Value::Str(f.name()));
    }
    Ok(Value::Str(super::short_name(&f.name())))
}
fn rf_get_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if is_real_closure(&f) {
        return Ok(zs(""));
    }
    Ok(Value::Str(super::ns_name(&f.name())))
}
fn rf_in_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if is_real_closure(&f) {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(super::in_ns(&f.name())))
}

fn rf_get_parameters(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let mut out = Vec::new();
    for i in 0..f.func.params().len() {
        out.push(super::param::param_value(vm, &f, i)?);
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

fn rf_num_params(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Int(
        i64::try_from(f.func.params().len()).unwrap_or(0),
    ))
}

fn rf_num_required(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Int(i64::from(f.func.required())))
}

fn rf_get_return_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    match return_type(vm, &f) {
        Some((t, false)) => super::types::type_value(vm, &t, scope.as_ref(), true),
        _ => Ok(Value::Null),
    }
}

fn rf_has_return_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Bool(matches!(return_type(vm, &f), Some((_, false)))))
}

fn rf_get_tentative_return_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    match return_type(vm, &f) {
        Some((t, true)) => super::types::type_value(vm, &t, scope.as_ref(), true),
        _ => Ok(Value::Null),
    }
}

fn rf_has_tentative_return_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Bool(matches!(return_type(vm, &f), Some((_, true)))))
}

macro_rules! rf_bool {
    ($name:ident, |$f:ident| $e:expr) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let $f = this_fn(vm, a)?;
            Ok(Value::Bool($e))
        }
    };
}

rf_bool!(rf_is_variadic, |f| f.func.is_variadic());
rf_bool!(rf_returns_ref, |f| f.func.flags() & flags::RETURN_REF != 0);
rf_bool!(rf_is_generator, |f| f.func.flags() & flags::GENERATOR != 0);
rf_bool!(rf_is_closure, |f| f.method.is_none()
    && (f.func.flags() & flags::CLOSURE != 0 || f.closure.is_some()));
rf_bool!(rf_is_internal, |f| !f.is_user());
rf_bool!(rf_is_user, |f| f.is_user());
rf_bool!(rf_is_deprecated, |f| f.func.flags() & flags::DEPRECATED
    != 0);
rf_bool!(rf_is_static, |f| f.flags() & flags::STATIC != 0);
rf_bool!(rf_is_anonymous, |f| is_real_closure(&f));

fn rf_get_doc_comment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if f.is_trampoline() {
        return Ok(Value::Bool(false));
    }
    Ok(match &f.func {
        Func::User(u) => u.doc_comment.clone().map_or(Value::Bool(false), Value::Str),
        Func::Builtin(_) => builtin_doc_comment(vm, &f).map_or(Value::Bool(false), Value::Str),
    })
}

fn rf_get_start_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if f.is_trampoline() {
        return Ok(Value::Bool(false));
    }
    Ok(match &f.func {
        Func::User(u) => Value::Int(i64::from(u.line_start)),
        Func::Builtin(_) => Value::Bool(false),
    })
}

fn rf_get_end_line(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if f.is_trampoline() {
        return Ok(Value::Bool(false));
    }
    Ok(match &f.func {
        Func::User(u) => Value::Int(i64::from(u.line_end)),
        Func::Builtin(_) => Value::Bool(false),
    })
}

fn rf_get_file_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if f.is_trampoline() {
        return Ok(Value::Bool(false));
    }
    Ok(match &f.func {
        Func::User(u) => Value::Str(u.code.filename.clone()),
        Func::Builtin(_) => Value::Bool(false),
    })
}

fn rf_get_attributes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    let t = if scope.is_some() && !is_real_closure(&f) {
        super::target::METHOD
    } else {
        super::target::FUNCTION
    };
    // `zend_get_call_trampoline_func` copies the attributes of the
    // `__call`/`__callStatic` method the trampoline stands for.
    let trampoline_target = f
        .closure
        .as_ref()
        .and_then(|c| vm.closure_data(c))
        .filter(|d| d.magic_name.is_some())
        .map(|d| d.func);
    let attrs = match trampoline_target.as_ref().unwrap_or(&f.func) {
        Func::User(u) => u.attributes.clone(),
        Func::Builtin(_) => builtin_key(&f)
            .and_then(|k| vm.builtin_attributes.get(&k))
            .map(|v| (**v).clone())
            .unwrap_or_default(),
    };
    let hook = matches!(&f.func, Func::User(u) if u.name.as_bytes().starts_with(b"$"));
    super::attribute::attributes_array_decl(vm, &attrs, t, scope.as_ref(), fn_file(&f), hook, a)
}

fn rf_get_static_variables(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let mut t = HashTable::new();
    for (k, v) in static_vars(vm, &f) {
        t.insert(ArrayKey::Str(k), v);
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rf_get_closure_used_variables(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let mut t = HashTable::new();
    let (Some(c), Func::User(u)) = (&f.closure, &f.func) else {
        return Ok(Value::Array(ZArray::new()));
    };
    if let Some(d) = vm.closure_data(c) {
        for us in &u.uses {
            if let Some((_, v)) = d.vars.iter().find(|(cv, _)| *cv == us.cv) {
                t.insert(ArrayKey::Str(us.name.clone()), v.clone());
            }
        }
    }
    Ok(Value::Array(ZArray::from_table(t)))
}

fn rf_get_closure_this(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if let Some(c) = &f.closure {
        if let Some(d) = vm.closure_data(c) {
            return Ok(d.this.map_or(Value::Null, Value::Object));
        }
    }
    Ok(Value::Null)
}

fn rf_get_closure_scope_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if let Some(c) = &f.closure {
        if let Some(s) = vm.closure_data(c).and_then(|d| d.scope) {
            return class_value(vm, &s);
        }
    }
    Ok(Value::Null)
}

fn rf_get_closure_called_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if let Some(c) = &f.closure {
        if let Some(d) = vm.closure_data(c) {
            let cls = d
                .this
                .as_ref()
                .map(|t| object::class_of(t).clone())
                .or(d.called)
                .or(d.scope);
            if let Some(s) = cls {
                return class_value(vm, &s);
            }
        }
    }
    Ok(Value::Null)
}

fn call_function(
    vm: &mut Vm,
    f: &FnRef,
    args: Vec<Value>,
    named: Vec<(ZStr, Value)>,
) -> VmResult<Value> {
    let mut call = match &f.closure {
        Some(c) => vm
            .closure_call(c)
            .unwrap_or_else(|| PendingCall::new(f.func.clone())),
        None => PendingCall::new(f.func.clone()),
    };
    bind_args(vm, &mut call, args, named)?;
    Ok(vm.call_pending(call)?.deref())
}

fn rf_invoke(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let args = a.values.clone();
    let named = std::mem::take(&mut a.named);
    call_function(vm, &f, args, named)
}

fn rf_invoke_args(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let (args, named) = match a.at(0) {
        Value::Array(arr) => split_args(vm, &arr)?,
        _ => (Vec::new(), Vec::new()),
    };
    call_function(vm, &f, args, named)
}

fn rf_get_closure(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    match &f.closure {
        Some(c) => Ok(Value::Object(c.clone())),
        None => vm.make_closure_from_call(PendingCall::new(f.func.clone())),
    }
}

fn rf_get_extension(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    match super::misc::function_module(vm, &f.func, scope.as_ref(), &f.name()) {
        Some(m) if !f.func.is_user() => super::misc::extension_value(vm, &m),
        _ => Ok(Value::Null),
    }
}

fn rf_get_extension_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    match super::misc::function_module(vm, &f.func, scope.as_ref(), &f.name()) {
        Some(m) if !f.func.is_user() => Ok(zs(&m)),
        _ => Ok(Value::Bool(false)),
    }
}

fn rf_is_disabled(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.deprecated(
        "Method ReflectionFunction::isDisabled() is deprecated since 8.0, as ReflectionFunction can no longer be constructed for disabled functions",
    )?;
    Ok(Value::Bool(false))
}

// ---------------------------------------------------------------------
// ReflectionMethod
// ---------------------------------------------------------------------

fn instantiate_method(vm: &mut Vm, a: &Args, is_ctor: bool) -> VmResult<(Rc<Class>, FnRef)> {
    let arg1 = a.at(0);
    let arg2 = if is_ctor {
        a.get(1).filter(|v| !v.is_null())
    } else {
        None
    };
    let (cls, mname, orig) = match (&arg1, arg2) {
        (Value::Object(o), Some(m)) => (object::class_of(o).clone(), vm.to_zstr(&m)?, Some(o.clone())),
        (Value::Object(_), None) => {
            return Err(vm.make_throw(
                "ValueError",
                "ReflectionMethod::__construct(): Argument #2 ($method) cannot be null when argument #1 ($objectOrMethod) is an object",
            ))
        }
        (v, Some(m)) => {
            let c = class_arg(vm, v, "Class")?;
            (c, vm.to_zstr(&m)?, None)
        }
        (v, None) => {
            let s = vm.to_zstr(v)?;
            let Some(p) = crate::call::find_double_colon(s.as_bytes()) else {
                let msg = if is_ctor {
                    "ReflectionMethod::__construct(): Argument #1 ($objectOrMethod) must be a valid method name"
                } else {
                    "ReflectionMethod::createFromMethodName(): Argument #1 ($method) must be a valid method name"
                };
                return Err(rexc(vm, msg));
            };
            let cn = ZStr::from(&s.as_bytes()[..p]);
            let c = class_arg(vm, &Value::Str(cn), "Class")?;
            (c, ZStr::from(&s.as_bytes()[p + 2..]), None)
        }
    };
    if cls.lc_name.as_bytes() == b"closure" && mname.eq_ignore_ascii_case(b"__invoke") {
        if let Some(o) = &orig {
            if let Some(d) = vm.closure_data(o) {
                let m = Rc::new(Method {
                    name: ZStr::from("__invoke"),
                    func: d.func.clone(),
                    flags: flags::PUBLIC,
                    scope: Rc::downgrade(&cls),
                    proto_scope: Rc::downgrade(&cls),
                });
                return Ok((
                    cls.clone(),
                    FnRef {
                        func: d.func,
                        method: Some(m),
                        class: Some(cls.clone()),
                        closure: Some(o.clone()),
                    },
                ));
            }
        }
    }
    match cls.find_method(mname.as_bytes()).cloned() {
        Some(m) => Ok((cls.clone(), FnRef::method(&m, &cls))),
        None => Err(rexc(
            vm,
            &format!(
                "Method {}::{}() does not exist",
                cls.name,
                mname.to_string_lossy()
            ),
        )),
    }
}

fn rm_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if !a.has(1) {
        vm.deprecated(
            "Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead",
        )?;
    }
    let (_, f) = instantiate_method(vm, a, true)?;
    let o = this(a);
    let scope_name = f.scope(vm).map(|c| c.name.clone()).unwrap_or_default();
    set_name_props(&o, Some(&f.name()), Some(&scope_name));
    object::set_internal(&o, Refl::Function(f));
    Ok(Value::Null)
}

fn rm_create_from_method_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, f) = instantiate_method(vm, a, false)?;
    if let Some(c) = &a.called {
        if c.flags & (cflags::ABSTRACT | cflags::EXPLICIT_ABSTRACT) != 0 {
            return Err(vm.make_throw(
                "Error",
                &format!("Cannot instantiate abstract class {}", c.name),
            ));
        }
    }
    let called = a.called.clone().map_or_else(
        || "ReflectionMethod".to_string(),
        |c| c.name.to_string_lossy(),
    );
    let scope_name = f.scope(vm).map(|c| c.name.clone()).unwrap_or_default();
    let n = f.name();
    new_refl(vm, &called, Refl::Function(f), Some(&n), Some(&scope_name))
}

macro_rules! rm_bool {
    ($name:ident, |$f:ident| $e:expr) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let $f = this_fn(vm, a)?.flags();
            Ok(Value::Bool($e))
        }
    };
}

rm_bool!(rm_is_public, |f| f & flags::PUBLIC != 0);
rm_bool!(rm_is_private, |f| f & flags::PRIVATE != 0);
rm_bool!(rm_is_protected, |f| f & flags::PROTECTED != 0);
rm_bool!(rm_is_abstract, |f| f & flags::ABSTRACT != 0);
rm_bool!(rm_is_final, |f| f & flags::FINAL != 0);

fn rm_is_ctor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let (Some(m), Some(c)) = (&f.method, &f.class) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(
        m.name.eq_ignore_ascii_case(b"__construct")
            && c.magic
                .construct
                .as_ref()
                .is_some_and(|ct| Rc::ptr_eq(&ct.scope(), &m.scope())),
    ))
}

fn rm_is_dtor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(
        this_fn(vm, a)?.name().eq_ignore_ascii_case(b"__destruct"),
    ))
}

fn rm_get_declaring_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    match f.scope(vm) {
        Some(s) => class_value(vm, &s),
        None => Ok(Value::Null),
    }
}

fn rm_get_modifiers(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Int(match &f.method {
        Some(m) => method_php_flags(m) & 0x77,
        None => i64::from(f.flags() & 0x77),
    }))
}

fn rm_has_prototype(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    Ok(Value::Bool(method_prototype(&f).is_some()))
}

/// `common.prototype` of the reflected method as seen from the reflected
/// class: a method inherited from a parent that satisfies an interface the
/// child implements is copied into the child with that interface method as
/// its prototype (`do_inheritance_check_on_method`).
fn method_prototype(f: &FnRef) -> Option<Rc<Method>> {
    let m = f.method.as_ref()?;
    match &f.class {
        Some(c) => super::order::prototype_in(m, c),
        None => super::order::prototype(m),
    }
}

fn rm_get_prototype(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    match method_prototype(&f) {
        Some(p) => {
            let s = p.scope();
            method_value(vm, &p, &s)
        }
        None => {
            let cn = f.class.as_ref().map(|c| c.name.clone()).unwrap_or_default();
            Err(rexc(
                vm,
                &format!("Method {}::{}() does not have a prototype", cn, f.name()),
            ))
        }
    }
}

fn invoke_method(
    vm: &mut Vm,
    f: &FnRef,
    obj: Value,
    args: Vec<Value>,
    named: Vec<(ZStr, Value)>,
    variadic: bool,
) -> VmResult<Value> {
    let Some(scope) = f.scope(vm) else {
        return Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let fl = f.flags();
    if fl & flags::ABSTRACT != 0 {
        return Err(rexc(
            vm,
            &format!(
                "Trying to invoke abstract method {}::{}()",
                scope.name,
                f.name()
            ),
        ));
    }
    let this = if fl & flags::STATIC != 0 {
        if !obj.is_null() {
            let mn = if variadic {
                "ReflectionMethod::invoke"
            } else {
                "ReflectionMethod::invokeArgs"
            };
            vm.deprecated(&format!(
                "Calling {mn}() for static method {}::{}() does not need an object parameter",
                scope.name,
                f.name()
            ))?;
        }
        None
    } else {
        match obj {
            Value::Object(o) => {
                if !object::class_of(&o).is_subclass_of(&scope) {
                    return Err(rexc(
                        vm,
                        "Given object is not an instance of the class this method was declared in",
                    ));
                }
                if let Some(orig) = &f.closure {
                    if !php_runtime::Obj::ptr_eq(orig, &o) {
                        let same = match (vm.closure_data(orig), vm.closure_data(&o)) {
                            (Some(x), Some(y)) => Func::ptr_eq(&x.func, &y.func),
                            _ => false,
                        };
                        if !same {
                            return Err(rexc(
                                vm,
                                "Given Closure is not the same as the reflected Closure",
                            ));
                        }
                    }
                    let mut call = vm
                        .closure_call(&o)
                        .unwrap_or_else(|| PendingCall::new(f.func.clone()));
                    bind_args(vm, &mut call, args, named)?;
                    return Ok(vm.call_pending(call)?.deref());
                }
                Some(o)
            }
            _ => {
                return Err(rexc(
                    vm,
                    &format!(
                        "Trying to invoke non static method {}::{}() without an object",
                        scope.name,
                        f.name()
                    ),
                ));
            }
        }
    };
    let called = match &this {
        Some(t) => object::class_of(t).clone(),
        None => f.class.clone().unwrap_or_else(|| scope.clone()),
    };
    let mut call = PendingCall {
        this,
        scope: Some(scope),
        called: Some(called),
        ..PendingCall::new(f.func.clone())
    };
    bind_args(vm, &mut call, args, named)?;
    Ok(vm.call_pending(call)?.deref())
}

/// `Z_PARAM_OBJECT_OR_NULL` for argument #1 of `invoke`/`invokeArgs`.
fn object_or_null(vm: &mut Vm, f: &FnRef, a: &Args, method: &str) -> VmResult<Value> {
    if f.flags() & flags::ABSTRACT != 0 {
        let scope = f.scope(vm).map(|c| c.name.clone()).unwrap_or_default();
        return Err(rexc(
            vm,
            &format!("Trying to invoke abstract method {}::{}()", scope, f.name()),
        ));
    }
    let v = a.at(0);
    if !matches!(v, Value::Null | Value::Object(_)) {
        return Err(vm.make_throw(
            "TypeError",
            &format!(
                "ReflectionMethod::{method}(): Argument #1 ($object) must be of type ?object, {} given",
                crate::typecheck::given_type(&v)
            ),
        ));
    }
    Ok(v)
}

fn rm_invoke(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if !a.has(0) {
        return Err(vm.make_throw(
            "ArgumentCountError",
            "ReflectionMethod::invoke() expects at least 1 argument, 0 given",
        ));
    }
    let obj = object_or_null(vm, &f, a, "invoke")?;
    let args: Vec<Value> = a.values.iter().skip(1).cloned().collect();
    let named = std::mem::take(&mut a.named);
    invoke_method(vm, &f, obj, args, named, true)
}

fn rm_invoke_args(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    if f.flags() & flags::ABSTRACT == 0 && !a.has(1) {
        let n = usize::from(a.has(0));
        return Err(vm.make_throw(
            "ArgumentCountError",
            &format!("ReflectionMethod::invokeArgs() expects exactly 2 arguments, {n} given"),
        ));
    }
    let obj = object_or_null(vm, &f, a, "invokeArgs")?;
    let (args, named) = match a.at(1) {
        Value::Array(x) => split_args(vm, &x)?,
        v => {
            return Err(vm.make_throw(
                "TypeError",
                &format!(
                    "ReflectionMethod::invokeArgs(): Argument #2 ($args) must be of type array, {} given",
                    crate::typecheck::given_type(&v)
                ),
            ))
        }
    };
    invoke_method(vm, &f, obj, args, named, false)
}

fn rm_get_closure(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = this_fn(vm, a)?;
    let scope = f.scope(vm);
    if f.flags() & flags::STATIC != 0 {
        let call = PendingCall {
            scope: scope.clone(),
            called: scope,
            ..PendingCall::new(f.func.clone())
        };
        return vm.make_closure_from_call(call);
    }
    let Some(Value::Object(o)) = a.get(0) else {
        return Err(vm.make_throw(
            "ValueError",
            "ReflectionMethod::getClosure(): Argument #1 ($object) cannot be null for non-static methods",
        ));
    };
    if let Some(s) = &scope {
        if !object::class_of(&o).is_subclass_of(s) {
            return Err(rexc(
                vm,
                "Given object is not an instance of the class this method was declared in",
            ));
        }
    }
    if f.closure.is_some() && object::class_of(&o).lc_name.as_bytes() == b"closure" {
        return Ok(Value::Object(o));
    }
    let called = object::class_of(&o).clone();
    let call = PendingCall {
        this: Some(o),
        scope,
        called: Some(called),
        ..PendingCall::new(f.func.clone())
    };
    vm.make_closure_from_call(call)
}

fn rm_set_accessible(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    vm.deprecated("Method ReflectionMethod::setAccessible() is deprecated since 8.5, as it has no effect since PHP 8.1")?;
    Ok(Value::Null)
}

fn clone_error(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(rexc(vm, "Cannot clone object using __clone()"))
}

/// Registers `ReflectionFunctionAbstract`.
pub(crate) fn register_abstract(vm: &mut Vm) {
    ClassBuilder::new("ReflectionFunctionAbstract")
        .hooks(super::constant::hooks())
        .flags(
            cflags::ABSTRACT
                | cflags::EXPLICIT_ABSTRACT
                | cflags::NOT_SERIALIZABLE
                | cflags::NO_CLONE,
        )
        .implements(vm, &["Reflector"])
        .prop("name", RO, Some("string"), None)
        .method("__clone(): void", PRIV, clone_error)
        .method("inNamespace(): bool", PUB, rf_in_ns)
        .method("isClosure(): bool", PUB, rf_is_closure)
        .method("isDeprecated(): bool", PUB, rf_is_deprecated)
        .method("isInternal(): bool", PUB, rf_is_internal)
        .method("isUserDefined(): bool", PUB, rf_is_user)
        .method("isGenerator(): bool", PUB, rf_is_generator)
        .method("isVariadic(): bool", PUB, rf_is_variadic)
        .method("isStatic(): bool", PUB, rf_is_static)
        .method("getClosureThis(): ?object", PUB, rf_get_closure_this)
        .method(
            "getClosureScopeClass(): ?ReflectionClass",
            PUB,
            rf_get_closure_scope_class,
        )
        .method(
            "getClosureCalledClass(): ?ReflectionClass",
            PUB,
            rf_get_closure_called_class,
        )
        .method(
            "getClosureUsedVariables(): array",
            PUB,
            rf_get_closure_used_variables,
        )
        .method("getDocComment(): string|false", PUB, rf_get_doc_comment)
        .method("getEndLine(): int|false", PUB, rf_get_end_line)
        .method(
            "getExtension(): ?ReflectionExtension",
            PUB,
            rf_get_extension,
        )
        .method(
            "getExtensionName(): string|false",
            PUB,
            rf_get_extension_name,
        )
        .method("getFileName(): string|false", PUB, rf_get_file_name)
        .method("getName(): string", PUB, rf_get_name)
        .method("getNamespaceName(): string", PUB, rf_get_ns)
        .method("getNumberOfParameters(): int", PUB, rf_num_params)
        .method("getNumberOfRequiredParameters(): int", PUB, rf_num_required)
        .method("getParameters(): array", PUB, rf_get_parameters)
        .method("getShortName(): string", PUB, rf_get_short_name)
        .method("getStartLine(): int|false", PUB, rf_get_start_line)
        .method("getStaticVariables(): array", PUB, rf_get_static_variables)
        .method("returnsReference(): bool", PUB, rf_returns_ref)
        .method("hasReturnType(): bool", PUB, rf_has_return_type)
        .method("getReturnType(): ?ReflectionType", PUB, rf_get_return_type)
        .method(
            "hasTentativeReturnType(): bool",
            PUB,
            rf_has_tentative_return_type,
        )
        .method(
            "getTentativeReturnType(): ?ReflectionType",
            PUB,
            rf_get_tentative_return_type,
        )
        .method(
            "getAttributes(?string $name = null, int $flags = 0): array",
            PUB,
            rf_get_attributes,
        )
        .register(vm);
}

/// Registers `ReflectionFunction`.
pub(crate) fn register_function(vm: &mut Vm) {
    ClassBuilder::new("ReflectionFunction")
        .extends(vm, "ReflectionFunctionAbstract")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .constant("IS_DEPRECATED", Value::Int(super::acc::DEPRECATED))
        .method("__construct(Closure|string $function)", PUB, rf_construct)
        .method("__toString(): string", PUB, rf_to_string)
        .method("isAnonymous(): bool", PUB, rf_is_anonymous)
        .method("isDisabled(): bool", PUB, rf_is_disabled)
        .method("invoke(mixed ...$args): mixed", PUB, rf_invoke)
        .method("invokeArgs(array $args): mixed", PUB, rf_invoke_args)
        .method("getClosure(): Closure", PUB, rf_get_closure)
        .register(vm);
}

/// Registers `ReflectionMethod`.
pub(crate) fn register_method(vm: &mut Vm) {
    ClassBuilder::new("ReflectionMethod")
        .extends(vm, "ReflectionFunctionAbstract")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .prop("class", RO, Some("string"), None)
        .constant("IS_STATIC", Value::Int(16))
        .constant("IS_PUBLIC", Value::Int(1))
        .constant("IS_PROTECTED", Value::Int(2))
        .constant("IS_PRIVATE", Value::Int(4))
        .constant("IS_ABSTRACT", Value::Int(64))
        .constant("IS_FINAL", Value::Int(32))
        .method(
            "__construct(object|string $objectOrMethod, ?string $method = null)",
            PUB,
            rm_construct,
        )
        .method(
            "createFromMethodName(string $method): static",
            STAT,
            rm_create_from_method_name,
        )
        .method("__toString(): string", PUB, rf_to_string)
        .method("isPublic(): bool", PUB, rm_is_public)
        .method("isPrivate(): bool", PUB, rm_is_private)
        .method("isProtected(): bool", PUB, rm_is_protected)
        .method("isAbstract(): bool", PUB, rm_is_abstract)
        .method("isFinal(): bool", PUB, rm_is_final)
        .method("isConstructor(): bool", PUB, rm_is_ctor)
        .method("isDestructor(): bool", PUB, rm_is_dtor)
        .method(
            "getClosure(?object $object = null): Closure",
            PUB,
            rm_get_closure,
        )
        .method("getModifiers(): int", PUB, rm_get_modifiers)
        .method(
            "invoke(mixed $object = UNKNOWN, mixed ...$args): mixed",
            PUB,
            rm_invoke,
        )
        .method(
            "invokeArgs(mixed $object = UNKNOWN, mixed $args = UNKNOWN): mixed",
            PUB,
            rm_invoke_args,
        )
        .method(
            "getDeclaringClass(): ReflectionClass",
            PUB,
            rm_get_declaring_class,
        )
        .method("getPrototype(): ReflectionMethod", PUB, rm_get_prototype)
        .method("hasPrototype(): bool", PUB, rm_has_prototype)
        .method(
            "setAccessible(bool $accessible): void",
            PUB,
            rm_set_accessible,
        )
        .register(vm);
}
