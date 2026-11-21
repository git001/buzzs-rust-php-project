// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `__toString()` renderers of the reflection classes (`_class_string`,
//! `_function_string`, `_parameter_string`, `_property_string`,
//! `_class_const_string`, `_enum_case_string`, `_const_string`), and the
//! rendering of default values (`format_default_value`).

use super::order::{self, PropMeta};
use super::types::ZType;
use super::FnRef;
use crate::class::{cflags, Class, ClassConst, ConstExpr};
use crate::func::{flags, Func, Method};
use crate::object;
use crate::opcode::ParamInfo;
use crate::vm::{Vm, VmResult};
use php_parser::ast::{self, ArgKind, ClassRef, Expr, ExprKind, NameKind, NewClass};
use php_runtime::{ArrayKey, Obj, Value, ZStr};
use std::rc::Rc;

/// A byte buffer (`smart_str`).
#[derive(Default)]
pub(crate) struct Buf(pub Vec<u8>);

impl Buf {
    fn s(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    fn b(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn z(&mut self, z: &ZStr) -> &mut Self {
        self.0.extend_from_slice(z.as_bytes());
        self
    }
    fn c(&mut self, c: u8) -> &mut Self {
        self.0.push(c);
        self
    }
}

/// `smart_str_append_escaped`.
pub(crate) fn escaped(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        if c < 32 || c == b'\\' || c > 126 {
            out.push(b'\\');
            match c {
                b'\n' => out.push(b'n'),
                b'\r' => out.push(b'r'),
                b'\t' => out.push(b't'),
                0x0c => out.push(b'f'),
                0x0b => out.push(b'v'),
                b'\\' => out.push(b'\\'),
                0x1b => out.push(b'e'),
                _ => {
                    out.push(b'x');
                    out.extend_from_slice(format!("{c:02X}").as_bytes());
                }
            }
        } else {
            out.push(c);
        }
    }
}

/// A float as `smart_str_append_double(.., precision, false)` prints it.
pub(crate) fn fmt_double(vm: &Vm, d: f64) -> String {
    php_runtime::float::format_double(d, vm.precision, false)
}

/// `smart_str_append_scalar` / `smart_str_append_zval`; `None` for arrays
/// and non-enum objects.
fn scalar_repr(vm: &Vm, v: &Value) -> Option<Vec<u8>> {
    Some(match v {
        Value::Null => b"NULL".to_vec(),
        Value::Bool(true) => b"true".to_vec(),
        Value::Bool(false) => b"false".to_vec(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Float(d) => fmt_double(vm, *d).into_bytes(),
        Value::Str(s) => {
            let mut o = vec![b'\''];
            escaped(&mut o, s.as_bytes());
            o.push(b'\'');
            o
        }
        Value::Object(o) if object::class_of(o).is_enum() => {
            let cls = object::class_of(o).clone();
            let mut out = cls.name.as_bytes().to_vec();
            out.extend_from_slice(b"::");
            if let Some(n) = vm.enum_case_name(o) {
                out.extend_from_slice(n.as_bytes());
            }
            out
        }
        Value::Reference(r) => return scalar_repr(vm, &r.get()),
        _ => return None,
    })
}

/// `format_default_value` for an evaluated value.
pub(crate) fn format_default_value(vm: &Vm, v: &Value) -> Vec<u8> {
    if let Some(s) = scalar_repr(vm, v) {
        return s;
    }
    match v {
        Value::Array(a) => {
            let is_list = a.iter().enumerate().all(
                |(i, (k, _))| matches!(k, ArrayKey::Int(n) if usize::try_from(*n).ok() == Some(i)),
            );
            let mut out = vec![b'['];
            for (i, (k, val)) in a.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                if !is_list {
                    match k {
                        ArrayKey::Str(s) => {
                            out.push(b'\'');
                            escaped(&mut out, s.as_bytes());
                            out.push(b'\'');
                        }
                        ArrayKey::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
                    }
                    out.extend_from_slice(b" => ");
                }
                out.extend_from_slice(&format_default_value(vm, val));
            }
            out.push(b']');
            out
        }
        Value::Object(o) => {
            let mut out = b"object(".to_vec();
            out.extend_from_slice(object::class_of(o).name.as_bytes());
            out.push(b')');
            out
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------
// Constant expression export (`zend_ast_export` of compiled ASTs)
// ---------------------------------------------------------------------

fn resolve_name(ns: &[u8], n: &ast::Name) -> Vec<u8> {
    match n.kind {
        NameKind::FullyQualified => n.name.clone(),
        NameKind::Relative | NameKind::Qualified | NameKind::Unqualified => {
            if ns.is_empty() {
                n.name.clone()
            } else {
                let mut v = ns.to_vec();
                v.push(b'\\');
                v.extend_from_slice(&n.name);
                v
            }
        }
    }
}

fn is_special_class(n: &[u8]) -> bool {
    n.eq_ignore_ascii_case(b"self")
        || n.eq_ignore_ascii_case(b"parent")
        || n.eq_ignore_ascii_case(b"static")
}

/// Resolves class and constant names like the compiler does for constant
/// expressions (relative to namespace `ns`).
fn resolve_expr(e: &mut Expr, ns: &[u8]) {
    resolve_expr_in(e, ns, None);
}

/// Like [`resolve_expr`], folding `self::class` to the class name like the
/// compiler does when the class is known.
fn resolve_expr_in(e: &mut Expr, ns: &[u8], self_name: Option<&[u8]>) {
    if let (ExprKind::ClassName(class), Some(sn)) = (&e.kind, self_name) {
        if let ClassRef::Name(n) = class.as_ref() {
            if n.kind == NameKind::Unqualified && n.name.eq_ignore_ascii_case(b"self") {
                e.kind = ExprKind::String(sn.to_vec());
                return;
            }
        }
    }
    match &mut e.kind {
        ExprKind::ClassConst { class, .. }
        | ExprKind::ClassName(class)
        | ExprKind::StaticProp { class, .. } => {
            if let ClassRef::Name(n) = class.as_mut() {
                if n.kind != NameKind::FullyQualified
                    && !(n.kind == NameKind::Unqualified && is_special_class(&n.name))
                {
                    let r = resolve_name(ns, n);
                    if r != n.name || n.kind == NameKind::Relative {
                        n.name = r;
                        n.kind = NameKind::FullyQualified;
                    }
                }
            } else if let ClassRef::Expr(x) = class.as_mut() {
                resolve_expr(x, ns);
            }
        }
        ExprKind::StaticCall { class, args, .. } => {
            if let ClassRef::Name(n) = class.as_mut() {
                if n.kind != NameKind::FullyQualified
                    && !(n.kind == NameKind::Unqualified && is_special_class(&n.name))
                {
                    n.name = resolve_name(ns, n);
                    n.kind = NameKind::FullyQualified;
                }
            }
            resolve_args(args, ns);
        }
        ExprKind::New { class, args } => {
            if let NewClass::Name(n) = class.as_mut() {
                if !(n.kind == NameKind::Unqualified && is_special_class(&n.name)) {
                    n.name = resolve_name(ns, n);
                    n.kind = NameKind::FullyQualified;
                }
            }
            resolve_args(args, ns);
        }
        ExprKind::Const(n) => {
            let lc = n.name.to_ascii_lowercase();
            if n.kind == NameKind::Unqualified
                && matches!(lc.as_slice(), b"true" | b"false" | b"null")
            {
                n.name = lc;
                return;
            }
            if n.kind != NameKind::FullyQualified {
                n.name = resolve_name(ns, n);
            }
            n.kind = NameKind::Unqualified;
        }
        ExprKind::Array { items, .. } => {
            for it in items.iter_mut().flatten() {
                if let Some(k) = &mut it.key {
                    resolve_expr(k, ns);
                }
                resolve_expr(&mut it.value, ns);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            resolve_expr(left, ns);
            resolve_expr(right, ns);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => resolve_expr(expr, ns),
        ExprKind::Conditional {
            cond, then, else_, ..
        } => {
            resolve_expr(cond, ns);
            if let Some(t) = then {
                resolve_expr(t, ns);
            }
            resolve_expr(else_, ns);
        }
        ExprKind::Dim { base, index } => {
            resolve_expr(base, ns);
            if let Some(i) = index {
                resolve_expr(i, ns);
            }
        }
        ExprKind::Prop { object, .. } => resolve_expr(object, ns),
        ExprKind::Call { callee, args } => {
            // zend_compile_const_expr_fcc(): the function name is resolved
            // (not fully qualified unless written so).
            if let ast::Callee::Name(n) = callee.as_mut() {
                if n.kind != NameKind::FullyQualified {
                    n.name = resolve_name(ns, n);
                    n.kind = NameKind::Qualified;
                }
            }
            resolve_args(args, ns);
        }
        _ => {}
    }
}

/// `zend_ast_export` of an attribute argument: class, constant and
/// function names resolved relative to `ns`, and each closure (compiled
/// to a `ZEND_AST_OP_ARRAY`) printed as `Closure(name)` with the names of
/// the compiled closures, in source order.
pub(crate) fn export_attr_arg(mut e: Expr, ns: &[u8], closures: &[ZStr]) -> Vec<u8> {
    resolve_expr(&mut e, ns);
    let mut names = closures.iter();
    name_closures(&mut e, &mut names);
    php_parser::export::export_expr(&e)
}

fn name_closures(e: &mut Expr, names: &mut std::slice::Iter<'_, ZStr>) {
    let name_args = |args: &mut ast::ArgList, names: &mut std::slice::Iter<'_, ZStr>| {
        for a in &mut args.args {
            if let ArgKind::Positional(x) | ArgKind::Named { value: x, .. } = &mut a.kind {
                name_closures(x, names);
            }
        }
    };
    if matches!(e.kind, ExprKind::Closure(_)) {
        if let Some(n) = names.next() {
            let mut text = b"Closure(".to_vec();
            text.extend_from_slice(n.as_bytes());
            text.push(b')');
            e.kind = ExprKind::Const(ast::Name {
                kind: NameKind::Unqualified,
                name: text,
                span: e.span,
            });
        }
        return;
    }
    match &mut e.kind {
        ExprKind::Array { items, .. } => {
            for it in items.iter_mut().flatten() {
                if let Some(k) = &mut it.key {
                    name_closures(k, names);
                }
                name_closures(&mut it.value, names);
            }
        }
        ExprKind::New { args, .. }
        | ExprKind::Call { args, .. }
        | ExprKind::StaticCall { args, .. } => name_args(&mut **args, names),
        _ => {}
    }
}

fn resolve_args(args: &mut ast::ArgList, ns: &[u8]) {
    for a in &mut args.args {
        match &mut a.kind {
            ArgKind::Positional(x) | ArgKind::Unpack(x) | ArgKind::Named { value: x, .. } => {
                resolve_expr(x, ns);
            }
            _ => {}
        }
    }
}

/// Parses a constant expression's source text.
pub(crate) fn parse_const_expr(text: &str) -> Option<Expr> {
    let src = format!("<?php ({text});");
    let prog = php_parser::parse(src.as_bytes(), &php_parser::ParseOptions::default()).ok()?;
    let st = prog.statements.into_iter().next()?;
    match st.kind {
        ast::StmtKind::Expr(e) => Some(strip_parens(e)),
        _ => None,
    }
}

fn strip_parens(e: Expr) -> Expr {
    e
}

/// `zend_ast_export` of a not yet evaluated constant expression, given its
/// source text, the namespace it was compiled in and the class `self`
/// refers to.
pub(crate) fn export_const_expr_in(text: &str, ns: &[u8], self_name: Option<&[u8]>) -> Vec<u8> {
    match parse_const_expr(text) {
        Some(mut e) => {
            resolve_expr_in(&mut e, ns, self_name);
            php_parser::export::export_expr(&e)
        }
        None => text.as_bytes().to_vec(),
    }
}

/// The namespace a function or class was declared in.
pub(crate) fn namespace_of(name: &ZStr) -> Vec<u8> {
    let b = name.as_bytes();
    let b = match b.iter().position(|&c| c == 0) {
        Some(p) => &b[..p],
        None => b,
    };
    match b.iter().rposition(|&c| c == b'\\') {
        Some(p) => b[..p].to_vec(),
        None => Vec::new(),
    }
}

/// The namespace of a reflected function (from its name or class).
pub(crate) fn fn_namespace(vm: &Vm, f: &FnRef) -> Vec<u8> {
    if let Some(s) = f.scope(vm) {
        return namespace_of(&s.name);
    }
    if let Func::User(u) = &f.func {
        if u.is_closure() {
            if let Some(cn) = &u.class_name {
                return namespace_of(cn);
            }
            let n = &u.name;
            // `{closure:NS\fn():3}` style names.
            if let Some(inner) = n.as_bytes().strip_prefix(b"{closure:") {
                if let Some(end) = inner.iter().position(|&c| c == b'(' || c == b':') {
                    return namespace_of(&ZStr::from(&inner[..end]));
                }
            }
            return Vec::new();
        }
    }
    namespace_of(f.func.name())
}

/// The default value of user parameter `p` rendered for `__toString`.
pub(crate) fn param_default_repr(vm: &Vm, f: &FnRef, p: &ParamInfo) -> Option<Vec<u8>> {
    match &p.default_expr {
        Some(ConstExpr::Value(v)) => Some(format_default_value(vm, v)),
        Some(ConstExpr::Code(_)) => {
            let text = p.default_text.clone().unwrap_or_default();
            let declared_in_trait = match &f.func {
                Func::User(u) => u
                    .class_name
                    .as_ref()
                    .and_then(|n| vm.find_class(n.as_bytes()))
                    .is_some_and(|c| c.is_trait()),
                Func::Builtin(_) => false,
            };
            let scope = f
                .scope(vm)
                .filter(|c| !c.is_trait() && f.closure.is_none() && !declared_in_trait);
            let sn = scope.as_ref().map(|c| c.name.as_bytes().to_vec());
            Some(export_const_expr_in(
                &text,
                &fn_namespace(vm, f),
                sn.as_deref(),
            ))
        }
        None => None,
    }
}

// ---------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------

/// `_parameter_string`.
pub(crate) fn parameter_string(vm: &Vm, f: &FnRef, idx: usize, required: bool) -> Vec<u8> {
    let mut b = Buf::default();
    let params = f.func.params();
    let Some(p) = params.get(idx) else { return b.0 };
    b.s(&format!("Parameter #{idx} [ "));
    b.s(if required {
        "<required> "
    } else {
        "<optional> "
    });
    if let Some(t) = &p.ty {
        b.s(&ZType::from_decl(t).to_php()).c(b' ');
    }
    if p.by_ref {
        b.c(b'&');
    }
    if p.variadic {
        b.s("...");
    }
    b.c(b'$').z(&p.name);
    if !required && !p.variadic {
        match &f.func {
            Func::User(_) if f.is_trampoline() => {
                b.s(" = <default>");
            }
            Func::Builtin(_) => {
                b.s(" = ");
                match p.default_text.as_deref() {
                    Some(t) if t != "UNKNOWN" => b.s(t),
                    _ => b.s("<default>"),
                };
            }
            Func::User(_) => {
                if let Some(d) =
                    param_default_repr(vm, f, p).filter(|_| idx >= f.func.required() as usize)
                {
                    b.s(" = ").b(&d);
                }
            }
        }
    }
    b.s(" ]");
    b.0
}

fn method_in(c: &Class, name: &[u8]) -> Option<Rc<Method>> {
    c.methods.get(name.to_ascii_lowercase().as_slice()).cloned()
}

/// `_function_string`.
#[allow(clippy::too_many_lines)]
pub(crate) fn function_string(
    vm: &mut Vm,
    f: &FnRef,
    scope: Option<&Rc<Class>>,
    indent: &str,
) -> Vec<u8> {
    let mut b = Buf::default();
    let fl = f.flags();
    let fscope = f.scope(vm);
    if let (Func::User(u), true) = (&f.func, f.is_user()) {
        if let Some(d) = &u.doc_comment {
            b.s(indent).z(d).c(b'\n');
        }
    } else if let Some(d) = super::function::builtin_doc_comment(vm, f) {
        b.s(indent).z(&d).c(b'\n');
    }
    // zend_create_fake_closure() adds ZEND_ACC_CLOSURE to the copied
    // function, so `f(...)` reflects as a closure too.
    let is_closure = (fl & flags::CLOSURE != 0 || f.closure.is_some()) && f.method.is_none();
    b.s(indent);
    b.s(if is_closure {
        "Closure [ "
    } else if fscope.is_some() {
        "Method [ "
    } else {
        "Function [ "
    });
    b.s(if f.is_user() { "<user" } else { "<internal" });
    if fl & flags::DEPRECATED != 0 {
        b.s(", deprecated");
    }
    if let Func::Builtin(bf) = &f.func {
        if let Some(m) = super::misc::function_module(vm, &f.func, fscope.as_ref(), &bf.name) {
            b.s(":").s(&m);
        }
    }
    if let (Some(scope), Some(fs)) = (scope, &fscope) {
        if !Rc::ptr_eq(fs, scope) {
            b.s(", inherits ").z(&fs.name);
        } else if let Some(parent) = &fs.parent {
            if let Some(ov) = method_in(parent, f.name().as_bytes()) {
                let ovs = ov.scope();
                if !Rc::ptr_eq(&ovs, fs) && ov.visibility() != flags::PRIVATE {
                    b.s(", overwrites ").z(&ovs.name);
                }
            }
        }
    }
    if let Some(m) = &f.method {
        let proto = match scope {
            Some(sc) => order::prototype_in(m, sc),
            None => order::prototype(m),
        };
        if let Some(p) = proto {
            b.s(", prototype ").z(&p.scope().name);
        }
    }
    let is_ctor = fscope.is_some() && f.name().eq_ignore_ascii_case(b"__construct") && !is_closure;
    if is_ctor {
        b.s(", ctor");
    }
    b.s("> ");
    if fl & flags::ABSTRACT != 0 {
        b.s("abstract ");
    }
    if fl & flags::FINAL != 0 {
        b.s("final ");
    }
    if fl & flags::STATIC != 0 {
        b.s("static ");
    }
    if fscope.is_some() {
        b.s(match fl & flags::VISIBILITY {
            flags::PRIVATE => "private ",
            flags::PROTECTED => "protected ",
            _ => "public ",
        });
        b.s("method ");
    } else {
        b.s("function ");
    }
    if fl & flags::RETURN_REF != 0 {
        b.c(b'&');
    }
    b.z(&f.name()).s(" ] {\n");
    if let (Func::User(u), true) = (&f.func, f.is_user()) {
        b.s(indent)
            .s("  @@ ")
            .z(&u.code.filename)
            .s(&format!(" {} - {}\n", u.line_start, u.line_end));
    }
    let pind = format!("{indent}  ");
    if is_closure {
        let vars = super::function::static_vars(vm, f);
        if !vars.is_empty() {
            b.c(b'\n');
            b.s(&pind)
                .s(&format!("- Bound Variables [{}] {{\n", vars.len()));
            for (i, (k, _)) in vars.iter().enumerate() {
                b.s(&pind)
                    .s(&format!("    Variable #{i} [ $"))
                    .z(k)
                    .s(" ]\n");
            }
            b.s(&pind).s("}\n");
        }
    }
    let params = f.func.params();
    let required = f.func.required() as usize;
    // `_function_parameter_string()` prints nothing without arginfo: user
    // code (including the `Closure::__invoke` trampoline, which borrows the
    // closure's arginfo) has none when it declares no parameters and no
    // return type; internal functions always have it.
    if !params.is_empty() || !f.func.is_user() || f.func.return_type().is_some() {
        b.c(b'\n');
        b.s(&pind)
            .s(&format!("- Parameters [{}] {{\n", params.len()));
        for i in 0..params.len() {
            b.s(&pind).s("  ");
            let ps = parameter_string(vm, f, i, i < required);
            b.b(&ps);
            b.c(b'\n');
        }
        b.s(&pind).s("}\n");
    }
    if let Some((rt, tentative)) = super::function::return_type(vm, f) {
        b.s(&format!(
            "  {indent}- {} [ ",
            if tentative {
                "Tentative return"
            } else {
                "Return"
            }
        ));
        b.s(&ZType::from_decl(&rt).to_php()).c(b' ');
        b.s("]\n");
    }
    b.s(indent).s("}\n");
    b.0
}

/// The default value of a property for `__toString` (`None`: no default).
fn prop_default_repr(vm: &mut Vm, p: &PropMeta, cls: &Rc<Class>) -> Option<Vec<u8>> {
    if p.flags & flags::VIRTUAL != 0 {
        return None;
    }
    if p.is_static {
        let decl = vm
            .find_class(p.class.as_bytes())
            .unwrap_or_else(|| cls.clone());
        let cell = decl
            .find_static(p.name.as_bytes())
            .map(|s| (s.cell.clone(), s.initialized.get(), s.default.clone()));
        return match cell {
            Some((_, _, Some(e))) => Some(const_expr_repr(vm, &e, &p.class, &p.name, false)),
            Some((_, _, None)) => {
                if p.ty.is_some() {
                    None
                } else {
                    Some(b"NULL".to_vec())
                }
            }
            None => None,
        };
    }
    let e = p.default.clone()?;
    let decl = vm
        .find_class(p.class.as_bytes())
        .unwrap_or_else(|| cls.clone());
    if let (Some(pi), Some(defs)) = (&p.info, decl.defaults.borrow().as_ref()) {
        if let Some(Some(v)) = defs.get(pi.slot as usize) {
            return Some(format_default_value(vm, v));
        }
    }
    Some(const_expr_repr(vm, &e, &p.class, &p.name, false))
}

/// `zend_ast_export` of a compiled constant expression of class `class`
/// (re-parsed from the declaring file).
pub(crate) fn const_expr_repr(
    vm: &Vm,
    e: &ConstExpr,
    class: &ZStr,
    member: &ZStr,
    is_const: bool,
) -> Vec<u8> {
    match e {
        ConstExpr::Value(v) => format_default_value(vm, v),
        ConstExpr::Code(_) => {
            let found = vm.find_class(class.as_bytes()).and_then(|c| {
                let file = c.filename.clone()?;
                let lines = [c.line_start, c.line_end];
                lines.iter().find_map(|&l| {
                    if is_const {
                        super::source::const_value(vm, file.as_bytes(), l, member.as_bytes())
                    } else {
                        super::source::prop_default(vm, file.as_bytes(), l, member.as_bytes())
                    }
                })
            });
            match found {
                Some((mut ex, ns)) => {
                    resolve_expr(&mut ex, &ns);
                    php_parser::export::export_expr(&ex)
                }
                None => b"<default>".to_vec(),
            }
        }
    }
}

/// `_property_string`.
pub(crate) fn property_string(
    vm: &mut Vm,
    p: Option<&PropMeta>,
    name: &ZStr,
    cls: &Rc<Class>,
    indent: &str,
) -> Vec<u8> {
    let mut b = Buf::default();
    let Some(p) = p else {
        b.s(indent)
            .s("Property [ <dynamic> public $")
            .z(name)
            .s(" ]\n");
        return b.0;
    };
    if let Some(d) = &p.doc {
        b.s(indent).z(d).c(b'\n');
    }
    b.s(indent).s("Property [ ");
    let fl = super::property::php_flags(p);
    if fl & super::acc::ABSTRACT != 0 {
        b.s("abstract ");
    }
    if fl & super::acc::FINAL != 0 {
        b.s("final ");
    }
    b.s(match p.flags & flags::VISIBILITY {
        flags::PRIVATE => "private ",
        flags::PROTECTED => "protected ",
        _ => "public ",
    });
    if fl & super::acc::PRIVATE_SET != 0 {
        b.s("private(set) ");
    } else if fl & super::acc::PROTECTED_SET != 0 {
        b.s("protected(set) ");
    }
    if p.is_static {
        b.s("static ");
    }
    if p.flags & flags::READONLY != 0 {
        b.s("readonly ");
    }
    if p.flags & flags::VIRTUAL != 0 {
        b.s("virtual ");
    }
    if let Some(t) = &p.ty {
        b.s(&ZType::from_decl(t).to_php()).c(b' ');
    }
    b.c(b'$').z(name);
    if let Some(d) = prop_default_repr(vm, p, cls) {
        b.s(" = ").b(&d);
    }
    if let Some(h) = &p.hooks {
        b.s(" {");
        if h.get.is_some() || h.abstract_get {
            b.s(if h.final_get { " final get;" } else { " get;" });
        }
        if h.set.is_some() || h.abstract_set {
            b.s(if h.final_set { " final set;" } else { " set;" });
        }
        b.s(" }");
    }
    b.s(" ]\n");
    b.0
}

/// The name of a value's type (`zend_zval_type_name`).
pub(crate) fn zval_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Resource(_) => "resource",
        Value::Reference(r) => zval_type_name(&r.get()),
    }
}

fn const_value_text(vm: &mut Vm, v: &Value) -> VmResult<Vec<u8>> {
    Ok(match v {
        Value::Array(_) => b"Array".to_vec(),
        Value::Object(_) => b"Object".to_vec(),
        Value::Float(d) => fmt_double(vm, *d).into_bytes(),
        Value::Bool(false) => b"false".to_vec(),
        Value::Bool(true) => b"true".to_vec(),
        other => vm.to_zstr(other)?.as_bytes().to_vec(),
    })
}

/// `_class_const_string`.
pub(crate) fn class_const_string(
    vm: &mut Vm,
    cls: &Rc<Class>,
    c: &Rc<ClassConst>,
    indent: &str,
) -> VmResult<Vec<u8>> {
    let v = super::constant::const_value_of(vm, cls, c)?;
    let mut b = Buf::default();
    if let Some(d) = &c.doc_comment {
        b.s(indent).z(d).c(b'\n');
    }
    let vis = match c.flags & flags::VISIBILITY {
        flags::PRIVATE => "private",
        flags::PROTECTED => "protected",
        _ => "public",
    };
    let fin = if c.flags & flags::FINAL != 0 {
        "final "
    } else {
        ""
    };
    let ty = match &c.ty {
        Some(t) => ZType::from_decl(t).to_php(),
        None => zval_type_name(&v).to_string(),
    };
    b.s(indent)
        .s(&format!("Constant [ {fin}{vis} {ty} "))
        .z(&c.name)
        .s(" ] { ");
    let t = const_value_text(vm, &v)?;
    b.b(&t).s(" }\n");
    Ok(b.0)
}

/// `_enum_case_string`.
pub(crate) fn enum_case_string(
    vm: &mut Vm,
    cls: &Rc<Class>,
    c: &Rc<ClassConst>,
    indent: &str,
) -> VmResult<Vec<u8>> {
    let v = vm.enum_case(cls, &c.name)?;
    let mut b = Buf::default();
    if let Some(d) = &super::constant::const_doc_comment(c) {
        b.s(indent).z(d).c(b'\n');
    }
    b.s(indent).s("Case ").z(&c.name);
    if cls.backing.is_none() {
        b.c(b'\n');
    } else {
        let bv = super::constant::backing_value(&v);
        let t = vm.to_zstr(&bv)?;
        b.s(" = ").z(&t).c(b'\n');
    }
    Ok(b.0)
}

/// `_const_string` (global constants).
pub(crate) fn const_string(
    vm: &mut Vm,
    name: &ZStr,
    v: &Value,
    cflags_: &[&str],
    indent: &str,
) -> VmResult<Vec<u8>> {
    let mut b = Buf::default();
    b.s(indent).s("Constant [ ");
    if !cflags_.is_empty() {
        b.s("<").s(&cflags_.join(", ")).s("> ");
    }
    b.s(zval_type_name(v)).c(b' ').z(name).s(" ] { ");
    let t = match v {
        Value::Array(_) => b"Array".to_vec(),
        Value::Str(s) => s.as_bytes().to_vec(),
        Value::Float(d) => fmt_double(vm, *d).into_bytes(),
        Value::Bool(false) => b"false".to_vec(),
        Value::Bool(true) => b"true".to_vec(),
        other => vm.to_zstr(other)?.as_bytes().to_vec(),
    };
    b.b(&t).s(" }\n");
    Ok(b.0)
}

/// Whether instances of `c` are iterated by an iterator (`ce->get_iterator`).
fn has_iterator(c: &Rc<Class>) -> bool {
    if c.hooks.as_ref().is_some_and(|h| h.get_iterator.is_some()) {
        return true;
    }
    if c.lc_name.as_bytes() == b"generator" {
        return true;
    }
    if c.props.iter().any(|p| p.hooks.is_some())
        || c.decl
            .as_ref()
            .is_some_and(|d| d.props.iter().any(|p| p.hooks.is_some()))
    {
        return true;
    }
    c.interfaces
        .iter()
        .any(|i| matches!(i.lc_name.as_bytes(), b"iterator" | b"iteratoraggregate"))
        || matches!(c.lc_name.as_bytes(), b"iterator" | b"iteratoraggregate")
}

/// `_class_string`.
#[allow(clippy::too_many_lines)]
pub(crate) fn class_string(
    vm: &mut Vm,
    c: &Rc<Class>,
    obj: Option<&Obj>,
    indent: &str,
) -> VmResult<Vec<u8>> {
    let mut b = Buf::default();
    if let Some(d) = &c.doc_comment {
        b.s(indent).z(d).c(b'\n');
    }
    let user = c.flags & cflags::INTERNAL == 0;
    if obj.is_some() {
        b.s(indent).s("Object of class [ ");
    } else {
        let kind = if c.is_interface() {
            "Interface"
        } else if c.is_trait() {
            "Trait"
        } else if c.is_enum() {
            "Enum"
        } else {
            "Class"
        };
        b.s(indent).s(kind).s(" [ ");
    }
    b.s(if user { "<user" } else { "<internal" });
    if !user {
        if let Some(m) = super::misc::class_module(vm, c) {
            b.s(":").s(&m);
        }
    }
    b.s("> ");
    if has_iterator(c) {
        b.s("<iterateable> ");
    }
    if c.is_interface() {
        b.s("interface ");
    } else if c.is_trait() {
        b.s("trait ");
    } else if c.is_enum() {
        b.s("enum ");
    } else {
        if super::class::is_abstract(c) {
            b.s("abstract ");
        }
        if c.flags & cflags::FINAL != 0 {
            b.s("final ");
        }
        if c.flags & cflags::READONLY != 0 {
            b.s("readonly ");
        }
        b.s("class ");
    }
    b.z(&c.name);
    if let Some(p) = &c.parent {
        b.s(" extends ").z(&p.name);
    }
    if c.is_enum() {
        if let Some(m) = c.backing {
            b.s(if m == crate::types::mask::STRING {
                ": string"
            } else {
                ": int"
            });
        }
    }
    let ifaces = super::class::interfaces_order(vm, c);
    if !ifaces.is_empty() {
        b.s(if c.is_interface() {
            " extends "
        } else {
            " implements "
        });
        for (i, x) in ifaces.iter().enumerate() {
            if i > 0 {
                b.s(", ");
            }
            b.z(&x.name);
        }
    }
    b.s(" ] {\n");
    if user {
        let file = c.filename.clone().unwrap_or_default();
        b.s(indent)
            .s("  @@ ")
            .z(&file)
            .s(&format!(" {}-{}\n", c.line_start, c.line_end));
    }
    let sub = format!("{indent}    ");
    // Constants and enum cases.
    let mut cases = Vec::new();
    let mut consts = Vec::new();
    let mut ncases = 0;
    let mut nconsts = 0;
    for cc in order::consts(c) {
        if cc.is_case {
            cases.extend(enum_case_string(vm, c, &cc, &sub)?);
            ncases += 1;
        } else {
            consts.extend(class_const_string(vm, c, &cc, &sub)?);
            nconsts += 1;
        }
    }
    if ncases > 0 {
        b.c(b'\n');
        b.s(indent).s(&format!("  - Enum cases [{ncases}] {{\n"));
        b.b(&cases);
        b.s(indent).s("  }\n");
    }
    b.c(b'\n');
    b.s(indent).s(&format!("  - Constants [{nconsts}] {{\n"));
    b.b(&consts);
    b.s(indent).s("  }\n");
    // Static properties.
    let props = order::props(vm, c);
    let statics: Vec<&PropMeta> = props.iter().filter(|p| p.is_static).collect();
    b.s(&format!(
        "\n{indent}  - Static properties [{}] {{\n",
        statics.len()
    ));
    for p in &statics {
        let s = property_string(vm, Some(p), &p.name, c, &sub);
        b.b(&s);
    }
    b.s(indent).s("  }\n");
    // Static methods.
    let methods: Vec<Rc<Method>> = order::methods(c)
        .into_iter()
        .filter(|m| m.visibility() != flags::PRIVATE || Rc::ptr_eq(&m.scope(), c))
        .collect();
    let smethods: Vec<&Rc<Method>> = methods.iter().filter(|m| m.is_static()).collect();
    b.s(&format!(
        "\n{indent}  - Static methods [{}] {{",
        smethods.len()
    ));
    if smethods.is_empty() {
        b.c(b'\n');
    }
    for m in &smethods {
        b.c(b'\n');
        let s = function_string(vm, &FnRef::method(m, c), Some(c), &sub);
        b.b(&s);
    }
    b.s(indent).s("  }\n");
    // Properties.
    let inst: Vec<&PropMeta> = props.iter().filter(|p| !p.is_static).collect();
    b.s(&format!("\n{indent}  - Properties [{}] {{\n", inst.len()));
    for p in &inst {
        let s = property_string(vm, Some(p), &p.name, c, &sub);
        b.b(&s);
    }
    b.s(indent).s("  }\n");
    // Dynamic properties.
    if let Some(o) = obj {
        let mut dynp = Vec::new();
        let mut n = 0;
        let names: Vec<ZStr> = object::data(o)
            .dynamic
            .borrow()
            .as_ref()
            .map(|t| {
                t.iter()
                    .map(|(k, _)| match k {
                        ArrayKey::Str(s) => s.clone(),
                        ArrayKey::Int(i) => ZStr::from(i.to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        for name in names {
            if name.is_empty() || name.as_bytes()[0] == 0 {
                continue;
            }
            if c.props.iter().any(|p| p.name == name) || c.find_static(name.as_bytes()).is_some() {
                continue;
            }
            n += 1;
            dynp.extend(property_string(vm, None, &name, c, &sub));
        }
        b.s(&format!("\n{indent}  - Dynamic properties [{n}] {{\n"));
        b.b(&dynp);
        b.s(indent).s("  }\n");
    }
    // Methods.
    let imethods: Vec<&Rc<Method>> = methods.iter().filter(|m| !m.is_static()).collect();
    b.s(&format!("\n{indent}  - Methods [{}] {{", imethods.len()));
    if imethods.is_empty() {
        b.c(b'\n');
    }
    for m in &imethods {
        b.c(b'\n');
        let mut fr = FnRef::method(m, c);
        if let (Some(o), true) = (
            obj,
            c.lc_name.as_bytes() == b"closure" && m.name.eq_ignore_ascii_case(b"__invoke"),
        ) {
            if let Some(d) = vm.closure_data(o) {
                fr = FnRef {
                    func: d.func.clone(),
                    method: None,
                    class: None,
                    closure: Some(o.clone()),
                };
            }
        }
        let s = function_string(vm, &fr, Some(c), &sub);
        b.b(&s);
    }
    b.s(indent).s("  }\n");
    b.s(indent).s("}\n");
    Ok(b.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_smart_str() {
        let mut o = Vec::new();
        escaped(&mut o, b"a\nb\\c\x01'");
        assert_eq!(o, b"a\\nb\\\\c\\x01'".to_vec());
    }

    #[test]
    fn exports_namespaced_constant_expressions() {
        assert_eq!(
            export_const_expr_in("new Bar()", b"App", None),
            b"new \\App\\Bar()".to_vec()
        );
        assert_eq!(
            export_const_expr_in("self::BAR", b"App", None),
            b"self::BAR".to_vec()
        );
        assert_eq!(
            export_const_expr_in("Bar::BAZ", b"App", None),
            b"\\App\\Bar::BAZ".to_vec()
        );
        assert_eq!(
            export_const_expr_in("foo::BAR", b"", None),
            b"foo::BAR".to_vec()
        );
        assert_eq!(
            export_const_expr_in("PHP_SELF + 1", b"", None),
            b"PHP_SELF + 1".to_vec()
        );
        assert_eq!(
            export_const_expr_in("self::class", b"", Some(b"F")),
            b"'F'".to_vec()
        );
        assert_eq!(
            export_const_expr_in("new self()", b"App", None),
            b"new self()".to_vec()
        );
    }

    #[test]
    fn namespace_of_names() {
        assert_eq!(namespace_of(&ZStr::from("A\\B\\C")), b"A\\B".to_vec());
        assert_eq!(namespace_of(&ZStr::from("C")), Vec::<u8>::new());
    }
}
