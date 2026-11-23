// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Function, closure and class declarations; parameters and types.

use super::{CResult, ClassCtx, Compiler, FnCtx, FnKind};
use crate::ast::{self, ClassMemberKind, Expr, Stmt, StmtKind, TypeKind};
use crate::class::{
    cflags, AttributeInfo, ClassDecl, ClassKind, ConstDecl, ConstExpr, MethodDecl, PropDecl,
    PropHooks, TraitRule,
};
use crate::func::{flags, ClosureUse, UserFunction};
use crate::opcode::{Op, Operand, ParamInfo, Place, PlaceBase, Seg};
use crate::types::{mask, TypeDecl};
use php_runtime::{Value, ZStr};
use std::cell::RefCell;
use std::rc::Rc;

/// Source text of an expression (used for `assert()` messages).
pub(crate) fn export_expr_text(source: &[u8], e: &Expr) -> String {
    let s = e.span.start.min(source.len());
    let end = e.span.end.min(source.len()).max(s);
    String::from_utf8_lossy(&source[s..end]).into_owned()
}

/// Collects the class and constant names of an expression that `use`
/// imports resolve differently from the namespace prefix: `(start, end,
/// resolved name)`.
struct ImportedNames<'c, 'f> {
    c: &'c Compiler<'f>,
    out: Vec<(usize, usize, Vec<u8>)>,
}

impl ImportedNames<'_, '_> {
    fn class(&mut self, n: &ast::Name) {
        let special = n.kind == ast::NameKind::Unqualified
            && (n.name.eq_ignore_ascii_case(b"self")
                || n.name.eq_ignore_ascii_case(b"parent")
                || n.name.eq_ignore_ascii_case(b"static"));
        if special
            || !matches!(
                n.kind,
                ast::NameKind::Unqualified | ast::NameKind::Qualified
            )
        {
            return;
        }
        let r = self.c.resolve_class_name(n);
        if r != self.c.ns_prefixed(&n.name) {
            self.out.push((n.span.start, n.span.end, r));
        }
    }

    fn constant(&mut self, n: &ast::Name) {
        let r = match n.kind {
            ast::NameKind::Unqualified => match self.c.const_uses.get(&n.name) {
                Some(i) => i.clone(),
                None => return,
            },
            ast::NameKind::Qualified => {
                let r = self.c.resolve_class_name(n);
                if r == self.c.ns_prefixed(&n.name) {
                    return;
                }
                r
            }
            _ => return,
        };
        self.out.push((n.span.start, n.span.end, r));
    }
}

impl php_parser::visit::Visitor for ImportedNames<'_, '_> {
    fn visit_expr(&mut self, e: &Expr) {
        match &e.kind {
            ast::ExprKind::Closure(_) | ast::ExprKind::ArrowFn(_) => return,
            ast::ExprKind::New { class, .. } => {
                if let ast::NewClass::Name(n) = &**class {
                    self.class(n);
                }
            }
            ast::ExprKind::ClassConst { class, .. }
            | ast::ExprKind::ClassName(class)
            | ast::ExprKind::StaticProp { class, .. }
            | ast::ExprKind::StaticCall { class, .. } => {
                if let ast::ClassRef::Name(n) = &**class {
                    self.class(n);
                }
            }
            ast::ExprKind::Const(n) => self.constant(n),
            _ => {}
        }
        php_parser::visit::walk_expr(self, e);
    }
}

fn modifiers_to_flags(m: ast::Modifiers) -> u32 {
    let mut f = 0;
    if m.contains(ast::Modifiers::PUBLIC) || m.contains(ast::Modifiers::VAR) {
        f |= flags::PUBLIC;
    }
    if m.contains(ast::Modifiers::PROTECTED) {
        f |= flags::PROTECTED;
    }
    if m.contains(ast::Modifiers::PRIVATE) {
        f |= flags::PRIVATE;
    }
    if m.contains(ast::Modifiers::STATIC) {
        f |= flags::STATIC;
    }
    if m.contains(ast::Modifiers::FINAL) {
        f |= flags::FINAL;
    }
    if m.contains(ast::Modifiers::ABSTRACT) {
        f |= flags::ABSTRACT;
    }
    if m.contains(ast::Modifiers::READONLY) {
        f |= flags::READONLY;
    }
    if m.contains(ast::Modifiers::PUBLIC_SET) {
        f |= flags::SET_PUBLIC;
    }
    if m.contains(ast::Modifiers::PROTECTED_SET) {
        f |= flags::SET_PROTECTED;
    }
    if m.contains(ast::Modifiers::PRIVATE_SET) {
        f |= flags::SET_PRIVATE;
    }
    if f & flags::VISIBILITY == 0 {
        f |= flags::PUBLIC;
    }
    f
}

struct BodySpec<'a> {
    kind: FnKind,
    name: ZStr,
    method_name: ZStr,
    params: &'a [ast::Param],
    return_type: Option<&'a ast::Type>,
    by_ref: bool,
    is_generator: bool,
    is_static: bool,
    line_start: u32,
    line_end: u32,
    doc_comment: Option<&'a Vec<u8>>,
    attributes: &'a ast::Attributes,
    is_ctor: bool,
    uses: Vec<(ZStr, bool, u32, bool)>,
    has_body: bool,
}

impl Compiler<'_> {
    pub(crate) fn compile_function_decl(
        &mut self,
        fd: &ast::FunctionDecl,
        top_level: bool,
    ) -> CResult<()> {
        let name = self.ns_prefixed_pub(&fd.name.name);
        let lc = name.to_ascii_lowercase();
        let known_key = lc.clone();
        // `zend_begin_func_decl`: the unqualified name must not name a
        // different function imported by `use function`.
        if self
            .fn_uses
            .get(&fd.name.name.to_ascii_lowercase())
            .is_some_and(|import| !import.eq_ignore_ascii_case(&lc))
        {
            return self.err(
                format!(
                    "Cannot redeclare function {}() (previously declared as local import)",
                    String::from_utf8_lossy(&name)
                ),
                fd.span.line,
            );
        }
        self.register_seen_symbol(lc.clone(), super::stmt::SYMBOL_FUNCTION);
        if top_level {
            if let Some(&line) = self.declared_functions.get(&lc) {
                return self.err(
                    format!(
                        "Cannot redeclare function {}() (previously declared in {}:{})",
                        String::from_utf8_lossy(&name),
                        self.filename,
                        line
                    ),
                    fd.span.line,
                );
            }
            self.declared_functions.insert(lc, fd.span.line);
        }
        // `zend_begin_func_decl` identifier deprecation.
        let unq = &fd.name.name;
        if unq.eq_ignore_ascii_case(b"readonly")
            || unq.eq_ignore_ascii_case(b"let")
            || unq.eq_ignore_ascii_case(b"is")
        {
            self.warnings.push((
                8192,
                format!(
                    "Calling a function \"{}\" is deprecated since 8.6",
                    String::from_utf8_lossy(unq)
                ),
                fd.span.line,
            ));
        }
        if unq.eq_ignore_ascii_case(b"assert") {
            return self.err(
                "Defining a custom assert() function is not allowed, as the function has special semantics",
                fd.span.line,
            );
        }
        let spec = BodySpec {
            kind: FnKind::Function,
            name: ZStr::from(name),
            method_name: ZStr::empty(),
            params: &fd.params,
            return_type: fd.return_type.as_ref(),
            by_ref: fd.by_ref,
            is_generator: fd.is_generator,
            is_static: false,
            line_start: fd.span.line,
            line_end: fd.end_line,
            doc_comment: fd.doc_comment.as_ref(),
            attributes: &fd.attributes,
            is_ctor: false,
            uses: Vec::new(),
            has_body: true,
        };
        // `zend_compile_func_decl`: a function declared inside a method has
        // no class scope (`CG(active_class_entry) = NULL`), so `__CLASS__`
        // is empty there (`nested_method_and_function.phpt`).
        let classes = std::mem::take(&mut self.classes);
        let uf = self.compile_body(&spec, BodyKind::Stmts(&fd.body));
        self.classes = classes;
        let uf = uf?;
        let idx = self.add_function(uf);
        if top_level {
            self.hoisted_functions.push(idx);
            // `pass_two()` done: later calls in this file know its arginfo.
            let required = fd
                .params
                .iter()
                .rposition(|p| p.default.is_none() && !p.variadic)
                .map_or(0, |i| u32::try_from(i + 1).unwrap_or(u32::MAX));
            let info = super::FunctionInfo::from_flags(
                false,
                required,
                fd.params.iter().map(|p| (p.by_ref, p.variadic)),
            );
            self.known_functions.insert(known_key, info);
        } else {
            self.f().line = fd.span.line;
            self.emit(Op::DeclareFunction(idx));
        }
        Ok(())
    }

    pub(crate) fn closure_name(&self, line: u32) -> ZStr {
        if self.closure_depth == 0 {
            if let Some(n) = &self.opts.closure_name {
                return n.clone();
            }
        }
        let mut f = self.fref();
        if f.kind == FnKind::ConstExpr {
            // `CG(active_op_array)`: the function whose attributes are being
            // compiled, else the code the constant expression appears in.
            if let Some(owner) = &self.attr_owner {
                return ZStr::from(format!("{{closure:{owner}:{line}}}"));
            }
            if let Some(outer) = self.fns.iter().rev().find(|c| c.kind != FnKind::ConstExpr) {
                f = outer;
            }
        }
        let s = match f.kind {
            FnKind::Closure => format!("{{closure:{}:{}}}", f.func_name, line),
            FnKind::Function => format!("{{closure:{}():{}}}", f.func_name, line),
            FnKind::Method => match self.current_class() {
                Some(c) => format!("{{closure:{}::{}():{}}}", c.name, f.method_name, line),
                None => format!("{{closure:{}():{}}}", f.method_name, line),
            },
            FnKind::Main | FnKind::ConstExpr => format!("{{closure:{}:{}}}", self.filename, line),
        };
        ZStr::from(s)
    }

    pub(crate) fn compile_closure(&mut self, c: &ast::Closure) -> CResult<Operand> {
        let mut uses = Vec::new();
        for u in &c.uses {
            if u.name.name == b"this" {
                return self.err("Cannot use $this as lexical variable", u.span.line);
            }
            if is_superglobal_or_globals(&u.name.name) {
                return self.err(
                    format!("Cannot use auto-global as lexical variable"),
                    u.span.line,
                );
            }
            if uses.iter().any(|(n, _, _, _): &(ZStr, bool, u32, bool)| {
                n.as_bytes() == u.name.name.as_slice()
            }) {
                return self.err(
                    format!("Cannot use variable ${} twice", u.name.as_str_lossy()),
                    u.span.line,
                );
            }
            if c.params.iter().any(|p| p.name.name == u.name.name) {
                return self.err(
                    format!(
                        "Cannot use lexical variable ${} as a parameter name",
                        u.name.as_str_lossy()
                    ),
                    u.span.line,
                );
            }
            let pcv = self.cv(&u.name.name);
            uses.push((ZStr::from(u.name.name.as_slice()), u.by_ref, pcv, false));
        }
        let name = self.closure_name(c.span.line);
        let spec = BodySpec {
            kind: FnKind::Closure,
            name,
            method_name: ZStr::empty(),
            params: &c.params,
            return_type: c.return_type.as_ref(),
            by_ref: c.by_ref,
            is_generator: c.is_generator,
            is_static: c.is_static,
            line_start: c.span.line,
            line_end: c.end_line,
            doc_comment: c.doc_comment.as_ref(),
            attributes: &c.attributes,
            is_ctor: false,
            uses,
            has_body: true,
        };
        self.closure_depth += 1;
        let r = self.compile_body(&spec, BodyKind::Stmts(&c.body));
        self.closure_depth -= 1;
        let uf = r?;
        let idx = self.add_function(uf);
        let t = self.tmp();
        self.f().line = c.span.line;
        self.emit(Op::MakeClosure { idx, dst: t });
        Ok(Operand::Tmp(t))
    }

    pub(crate) fn compile_arrow_fn(&mut self, a: &ast::ArrowFn) -> CResult<Operand> {
        let name = self.closure_name(a.span.line);
        let spec = BodySpec {
            kind: FnKind::Closure,
            name,
            method_name: ZStr::empty(),
            params: &a.params,
            return_type: a.return_type.as_ref(),
            by_ref: a.by_ref,
            is_generator: a.is_generator,
            is_static: a.is_static,
            line_start: a.span.line,
            line_end: a.end_line,
            doc_comment: a.doc_comment.as_ref(),
            attributes: &a.attributes,
            is_ctor: false,
            uses: Vec::new(),
            has_body: true,
        };
        self.closure_depth += 1;
        let r = self.compile_body(&spec, BodyKind::Arrow(&a.body));
        self.closure_depth -= 1;
        let mut uf = r?;
        // Implicit by-value capture of every non-parameter variable.
        let nparams = a.params.len();
        let names: Vec<ZStr> = uf.code.cv_names.clone();
        let mut uses = Vec::new();
        for (i, n) in names.iter().enumerate().skip(nparams) {
            let pcv = self.cv(n.as_bytes());
            uses.push(ClosureUse {
                name: n.clone(),
                by_ref: false,
                parent_cv: pcv,
                cv: u32::try_from(i).unwrap_or(0),
                implicit: true,
            });
        }
        uf.uses = uses;
        uf.flags |= flags::ARROW;
        let idx = self.add_function(uf);
        let t = self.tmp();
        self.f().line = a.span.line;
        self.emit(Op::MakeClosure { idx, dst: t });
        Ok(Operand::Tmp(t))
    }

    /// The source text of a parameter default with the names that `use`
    /// imports resolve spliced in fully qualified: the reflection export
    /// re-resolves the text against the namespace only, while php-src
    /// exports the compiled AST (GH-9447).
    fn default_text_with_imports(&self, d: &Expr) -> String {
        let mut names = ImportedNames {
            c: self,
            out: Vec::new(),
        };
        php_parser::visit::Visitor::visit_expr(&mut names, d);
        if names.out.is_empty() {
            return export_expr_text(&self.source, d);
        }
        let s = d.span.start.min(self.source.len());
        let end = d.span.end.min(self.source.len()).max(s);
        names.out.sort_by_key(|r| r.0);
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: linear in the source code"
        )]
        let mut text = Vec::with_capacity(end - s + 16);
        let mut at = s;
        for (a, b, resolved) in names.out {
            if a < at || b > end {
                continue;
            }
            text.extend_from_slice(&self.source[at..a]);
            text.push(b'\\');
            text.extend_from_slice(&resolved);
            at = b;
        }
        text.extend_from_slice(&self.source[at..end]);
        String::from_utf8_lossy(&text).into_owned()
    }

    #[allow(clippy::too_many_lines)]
    fn compile_body(&mut self, spec: &BodySpec<'_>, body: BodyKind<'_>) -> CResult<UserFunction> {
        let saved_hook = if spec.kind != FnKind::Method || !spec.name.as_bytes().starts_with(b"$") {
            self.hook_prop.take()
        } else {
            None
        };
        // Attributes first (`zend_compile_func_decl`).
        let owner = match spec.kind {
            FnKind::Function => Some(format!("{}()", spec.name)),
            FnKind::Method => self
                .current_class()
                .map(|c| format!("{}::{}()", c.name, spec.method_name)),
            _ => None,
        };
        let saved_owner = std::mem::replace(&mut self.attr_owner, owner);
        let attributes = self.attributes_at(spec.attributes, Some(spec.line_start));
        self.attr_owner = saved_owner;
        let attributes = attributes?;
        let tgt = if spec.kind == FnKind::Method {
            super::attrs::target::METHOD
        } else {
            super::attrs::target::FUNCTION
        };
        let in_hook = self.hook_prop.is_some();
        self.validate_attributes(&attributes, tgt, 0, None, in_hook)?;
        let has = |n: &[u8]| attributes.iter().any(|a| a.name.eq_ignore_ascii_case(n));
        let delayed = has(b"DelayedTargetValidation");
        if has(b"NoDiscard") && !in_hook {
            let what = if self.current_class().is_some() {
                "method"
            } else {
                "function"
            };
            if let Some(rt) = spec.return_type {
                let line = spec.line_start;
                if matches!(&rt.kind, ast::TypeKind::Named(n) if n.name.eq_ignore_ascii_case(b"void"))
                {
                    return self.err(
                        format!("A void {what} does not return a value, but #[\\NoDiscard] requires a return value"),
                        line,
                    );
                }
                if matches!(&rt.kind, ast::TypeKind::Named(n) if n.name.eq_ignore_ascii_case(b"never"))
                {
                    return self.err(
                        format!("A never returning {what} does not return a value, but #[\\NoDiscard] requires a return value"),
                        line,
                    );
                }
            }
        }
        let _ = delayed;
        let mut f = FnCtx::new(spec.kind);
        f.line = spec.line_start;
        f.func_name = spec.name.clone();
        f.method_name = spec.method_name.clone();
        f.is_generator = spec.is_generator;
        f.returns_ref = spec.by_ref;
        f.is_static = spec.is_static;
        self.fns.push(f);
        let r = self.compile_body_inner(spec, body);
        let f = self.fns.pop().expect("fn ctx");
        let (params, required, return_type, ctx_uses) = r?;
        let code = self.finish_fn(f)?;
        let mut fl = 0;
        if spec.kind == FnKind::Closure {
            fl |= flags::CLOSURE;
        }
        if spec.is_generator {
            fl |= flags::GENERATOR;
        }
        if spec.by_ref {
            fl |= flags::RETURN_REF;
        }
        if spec.is_static {
            fl |= flags::STATIC;
        }
        if params.last().is_some_and(|p: &ParamInfo| p.variadic) {
            fl |= flags::VARIADIC;
        }
        if !spec.has_body {
            fl |= flags::NO_BODY;
        }
        if attributes
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(b"Deprecated"))
        {
            fl |= flags::DEPRECATED;
        }
        if attributes
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(b"NoDiscard"))
            && !in_hook
        {
            fl |= flags::NODISCARD;
        }
        if saved_hook.is_some() {
            self.hook_prop = saved_hook;
        }
        Ok(UserFunction {
            name: spec.name.clone(),
            params,
            required,
            return_type,
            flags: fl,
            code: Rc::new(code),
            line_start: spec.line_start,
            line_end: spec.line_end,
            doc_comment: self.doc_comment(spec.doc_comment),
            statics: RefCell::new(Vec::new()),
            uses: ctx_uses,
            class_name: self.current_class().map(|c| c.name.clone()),
            attributes,
            hook_prop: None,
            call_shape: crate::func::CallShape::default(),
        })
    }

    #[allow(clippy::type_complexity, clippy::too_many_lines)]
    fn compile_body_inner(
        &mut self,
        spec: &BodySpec<'_>,
        body: BodyKind<'_>,
    ) -> CResult<(Vec<ParamInfo>, u32, Option<TypeDecl>, Vec<ClosureUse>)> {
        // Parameters occupy the first CVs.
        let mut params = Vec::new();
        // Required count: up to the last required parameter.
        let mut required = 0u32;
        for (i, p) in spec.params.iter().enumerate() {
            if p.default.is_none() && !p.variadic {
                required = u32::try_from(i + 1).unwrap_or(0);
            }
        }
        for (i, p) in spec.params.iter().enumerate() {
            if p.name.name == b"this" {
                return self.err("Cannot use $this as parameter", p.span.line);
            }
            if params
                .iter()
                .any(|q: &ParamInfo| q.name.as_bytes() == p.name.name.as_slice())
            {
                return self.err(
                    format!("Redefinition of parameter ${}", p.name.as_str_lossy()),
                    p.span.line,
                );
            }
            if is_superglobal_or_globals(&p.name.name) && p.name.name != b"GLOBALS" {
                return self.err(
                    format!(
                        "Cannot re-assign auto-global variable ${}",
                        p.name.as_str_lossy()
                    ),
                    p.span.line,
                );
            }
            let param_cv = self.cv(&p.name.name);
            self.f().bound_cvs.push((param_cv, p.by_ref));
            self.note_cv_write(&p.name.name);
            let mut ty = match &p.ty {
                Some(t) => Some(self.resolve_type(t)?),
                None => None,
            };
            if !p.by_ref && !p.variadic && ty.as_ref().is_some_and(is_scalar_only) {
                self.f().scalar_params.push(param_cv);
            }
            // `zend_compile_params` compiles the default without constant
            // substitution.
            let saved_subst = std::mem::replace(&mut self.no_const_subst, true);
            let folded_default = p.default.as_ref().and_then(|d| self.try_fold(d));
            self.no_const_subst = saved_subst;
            let mut forced = false;
            if let (Some(t), Some(Value::Null)) = (&mut ty, &folded_default) {
                if p.modifiers.is_empty() && p.hooks.is_none() && !t.allows_null() && !t.is_mixed()
                {
                    if t.mask & mask::VOID != 0 {
                        return self.err("Void can only be used as a standalone type", p.span.line);
                    }
                    if t.mask & mask::NEVER != 0 {
                        return self
                            .err("never can only be used as a standalone type", p.span.line);
                    }
                    t.mask |= mask::NULL;
                    t.pure_intersection = false;
                    forced = true;
                    self.warnings.push((
                        8192,
                        format!(
                            "{}(): Implicitly marking parameter ${} as nullable is deprecated, the explicit nullable type must be used instead",
                            self.fn_display_name(),
                            p.name.as_str_lossy()
                        ),
                        p.span.line,
                    ));
                }
            }
            if let Some(t) = &ty {
                if t.mask & mask::VOID != 0 {
                    return self.err("void cannot be used as a parameter type", p.span.line);
                }
                if t.mask & mask::NEVER != 0 {
                    return self.err("never cannot be used as a parameter type", p.span.line);
                }
                if let Some(value) = &folded_default {
                    if !t.accepts_default(value) {
                        return self.err(
                            format!(
                                "Cannot use {} as default value for parameter ${} of type {}",
                                value.type_name(),
                                p.name.as_str_lossy(),
                                t.to_php_string()
                            ),
                            p.span.line,
                        );
                    }
                }
            }
            if p.variadic && p.default.is_some() {
                return self.err(
                    "Variadic parameter cannot have a default value",
                    p.span.line,
                );
            }
            let promote = if p.modifiers.is_empty() && p.hooks.is_none() {
                0
            } else {
                if !spec.is_ctor {
                    return self.err(
                        "Cannot declare promoted property outside a constructor",
                        p.span.line,
                    );
                }
                if p.variadic {
                    return self.err("Cannot declare variadic promoted property", p.span.line);
                }
                modifiers_to_flags(p.modifiers)
            };
            let mut attrs = self.attributes_at(&p.attributes, Some(spec.line_start))?;
            let promoted_tgt = if promote == 0 {
                0
            } else {
                super::attrs::target::PROPERTY
            };
            self.validate_attributes(
                &attrs,
                super::attrs::target::PARAMETER,
                promoted_tgt,
                None,
                false,
            )?;
            if promoted_tgt != 0 {
                super::attrs::retain_promoted(
                    &mut attrs,
                    super::attrs::target::PARAMETER,
                    promoted_tgt,
                );
            }
            // `Type $p = null` (PHP 5 style, `forced_allow_nullable`) is
            // exempt from the optional-before-required notice.
            if p.default.is_some() && !forced && u32::try_from(i).is_ok_and(|i| i < required) {
                // php-src names the last required parameter.
                let later = (required as usize)
                    .checked_sub(1)
                    .and_then(|r| spec.params.get(r))
                    .map(|q| q.name.as_str_lossy().into_owned())
                    .unwrap_or_default();
                self.warnings.push((
                    8192,
                    format!(
                        "{}(): Optional parameter ${} declared before required parameter ${} is implicitly treated as a required parameter",
                        self.fn_display_name(),
                        p.name.as_str_lossy(),
                        later
                    ),
                    p.span.line,
                ));
            }
            let default_expr = match &p.default {
                Some(d) => {
                    self.check_const_expr(d, true)?;
                    let saved_subst = std::mem::replace(&mut self.no_const_subst, true);
                    let r = self.const_expr(d);
                    self.no_const_subst = saved_subst;
                    Some(r?)
                }
                None => None,
            };
            params.push(ParamInfo {
                name: ZStr::from(p.name.name.as_slice()),
                by_ref: p.by_ref,
                variadic: p.variadic,
                ty,
                has_default: p.default.is_some(),
                default_text: p.default.as_ref().map(|d| {
                    if self.multibyte {
                        String::from_utf8_lossy(&php_parser::export::export_expr(d)).into_owned()
                    } else {
                        self.default_text_with_imports(d)
                    }
                }),
                promote,
                sensitive: attrs
                    .iter()
                    .any(|a| a.name.eq_ignore_ascii_case(b"SensitiveParameter")),
                default_expr,
                attributes: attrs,
            });
        }
        // Closure uses after params.
        let mut uses = Vec::new();
        for (name, by_ref, pcv, implicit) in &spec.uses {
            let cv = self.cv(name.as_bytes());
            self.f().bound_cvs.push((cv, *by_ref));
            uses.push(ClosureUse {
                name: name.clone(),
                by_ref: *by_ref,
                parent_cv: *pcv,
                cv,
                implicit: *implicit,
            });
        }
        let return_type = match spec.return_type {
            Some(t) => {
                let rt = self.resolve_type(t)?;
                Some(rt)
            }
            None => None,
        };
        if let Some(rt) = &return_type {
            if spec.by_ref && rt.is_void() {
                self.warnings.push((
                    8192,
                    format!(
                        "{}(): Returning by reference from a void function is deprecated",
                        self.fn_display_name()
                    ),
                    spec.line_start,
                ));
            }
            if spec.is_generator {
                let ok = rt.mask & (mask::OBJECT | mask::ITERABLE) != 0
                    || rt.is_mixed()
                    || rt.classes.iter().any(|c| {
                        let l = c.to_ascii_lowercase();
                        matches!(l.as_bytes(), b"traversable" | b"iterator" | b"generator")
                    });
                if !ok {
                    return self.err(
                        format!(
                            "Generator return type must be a supertype of Generator, {} given",
                            rt.to_php_string()
                        ),
                        spec.return_type.map_or(spec.line_start, |t| t.span.line),
                    );
                }
            }
        }
        {
            let f = self.f();
            f.has_return_type = return_type.is_some();
            f.return_type.clone_from(&return_type);
        }
        // RECV ops.
        for (i, p) in spec.params.iter().enumerate() {
            let n = u32::try_from(i).unwrap_or(0);
            self.f().line = p.span.line;
            let cv = n;
            // `zend_compile_params`: a default before the last required
            // parameter is dropped (ZEND_RECV), so a named-argument call
            // cannot skip it either.
            let default = p.default.as_ref().filter(|_| n >= required);
            if p.variadic {
                self.emit(Op::RecvVariadic { n, cv });
            } else if let Some(d) = default {
                let at = self.emit(Op::RecvInit { n, cv, skip: 0 });
                let saved = std::mem::replace(&mut self.in_const_default, true);
                let saved_subst = std::mem::replace(&mut self.no_const_subst, true);
                let depth = std::mem::replace(&mut self.inline_const_depth, self.fns.len());
                let v = self.compile_expr(d);
                self.in_const_default = saved;
                self.no_const_subst = saved_subst;
                self.inline_const_depth = depth;
                let v = v?;
                self.f().line = p.span.line;
                self.emit(Op::RecvDefault { n, cv, val: v });
                self.patch_here(at);
            } else {
                self.emit(Op::Recv { n, cv });
            }
        }
        // Constructor promotion.
        if spec.is_ctor {
            for (i, p) in spec.params.iter().enumerate() {
                if !p.modifiers.is_empty() || p.hooks.is_some() {
                    self.f().line = p.span.line;
                    let name = self.lit(Value::Str(ZStr::from(p.name.name.as_slice())));
                    let cv = u32::try_from(i).unwrap_or(0);
                    let place = self.place(Place {
                        base: PlaceBase::This,
                        segs: vec![Seg::Prop(name)],
                    });
                    if p.by_ref {
                        // ZEND_ASSIGN_OBJ_REF for `public T &$x`
                        // (zend_compile_params).
                        let src = self.place(Place {
                            base: PlaceBase::Cv(cv),
                            segs: Vec::new(),
                        });
                        let t = self.tmp();
                        self.emit(Op::MakeRef { place: src, dst: t });
                        self.emit(Op::AssignRef {
                            place,
                            src: Operand::Tmp(t),
                            dst: None,
                        });
                    } else {
                        self.emit(Op::Assign {
                            place,
                            val: Operand::Cv(cv),
                            dst: None,
                        });
                    }
                }
            }
        }
        if spec.is_generator {
            self.f().line = spec.line_start;
            self.emit(Op::GeneratorCreate);
        }
        match body {
            BodyKind::Stmts(stmts) => {
                self.compile_stmts(stmts)?;
                self.f().line = spec.line_end;
                if spec.by_ref && !spec.is_generator {
                    // ZEND_RETURN_BY_REF of the implicit null.
                    self.emit(Op::ReturnByRefValue {
                        val: Operand::Unused,
                    });
                } else {
                    self.emit(Op::Return {
                        val: Operand::Unused,
                    });
                }
            }
            BodyKind::Arrow(e) => {
                self.f().line = e.span.line;
                // The body is `return e;` (`zend_compile_return`): in a
                // by-reference arrow function a variable or a call may not
                // end a nullsafe chain (`zend_assert_not_short_circuited`).
                let by_ref = spec.by_ref && !spec.is_generator;
                if by_ref
                    && (super::stmt::is_ref_expr(e) || super::expr::is_call_expr(e))
                    && super::expr::contains_nullsafe(e)
                {
                    return self.err("Cannot take reference of a nullsafe chain", e.span.line);
                }
                if spec.by_ref && super::stmt::is_ref_expr(e) {
                    let p = self.compile_place(e, false)?;
                    let pid = self.place(p);
                    self.emit(Op::ReturnRef { place: pid });
                } else {
                    let v = if by_ref {
                        self.by_ref_return_operand(e)?
                    } else {
                        self.compile_expr(e)?
                    };
                    if return_type.as_ref().is_some_and(TypeDecl::is_void) {
                        self.free(v);
                        self.emit(Op::Return {
                            val: Operand::Unused,
                        });
                    } else if spec.by_ref && !spec.is_generator {
                        // `fn&() => expr` on a non-variable: ZEND_RETURN_BY_REF
                        // of a value, with its notice (oss_fuzz_58181).
                        self.emit(Op::ReturnByRefValue { val: v });
                    } else {
                        self.emit(Op::Return { val: v });
                    }
                }
            }
            BodyKind::None => {
                self.emit(Op::Return {
                    val: Operand::Unused,
                });
            }
        }
        Ok((params, required, return_type, uses))
    }

    /// Resolves a type declaration.
    pub(crate) fn resolve_type(&mut self, t: &ast::Type) -> CResult<TypeDecl> {
        let mut out = TypeDecl::default();
        self.add_type(&mut out, t, false)?;
        if out.mask & mask::OBJECT != 0 && (out.has_classes() || out.mask & mask::STATIC != 0) {
            return self.err(
                format!(
                    "Type {} contains both object and a class type, which is redundant",
                    out.display(None, None, None)
                ),
                t.span.line,
            );
        }
        if out.is_mixed() && out.mask & mask::NULL != 0 && matches!(t.kind, TypeKind::Nullable(_)) {
            return self.err(
                "Type mixed cannot be marked as nullable since mixed already includes null",
                t.span.line,
            );
        }
        Ok(out)
    }

    fn add_type(&mut self, out: &mut TypeDecl, t: &ast::Type, in_union: bool) -> CResult<()> {
        match &t.kind {
            TypeKind::Array => self.add_mask(out, mask::ARRAY, "array", t.span.line)?,
            TypeKind::Callable => self.add_mask(out, mask::CALLABLE, "callable", t.span.line)?,
            TypeKind::Static => {
                if self.current_class().is_none() && self.closure_depth == 0 {
                    return self.err(
                        "Cannot use \"static\" when no class scope is active",
                        t.span.line,
                    );
                }
                self.add_mask(out, mask::STATIC, "static", t.span.line)?;
            }
            TypeKind::Nullable(inner) => {
                self.add_type(out, inner, in_union)?;
                if let TypeKind::Named(n) = &inner.kind {
                    let lc = n.name.to_ascii_lowercase();
                    if n.kind == ast::NameKind::Unqualified {
                        if lc.as_slice() == b"null" {
                            return self.err("null cannot be marked as nullable", t.span.line);
                        }
                        if lc.as_slice() == b"void" {
                            return self
                                .err("Void can only be used as a standalone type", t.span.line);
                        }
                        if lc.as_slice() == b"never" {
                            return self
                                .err("never can only be used as a standalone type", t.span.line);
                        }
                        if lc.as_slice() == b"mixed" {
                            return self.err(
                                "Type mixed cannot be marked as nullable since mixed already includes null",
                                t.span.line,
                            );
                        }
                    }
                }
                out.mask |= mask::NULL;
            }
            TypeKind::Union(ts) => {
                for x in ts {
                    self.add_type(out, x, true)?;
                }
                if out.mask & mask::VOID != 0 {
                    return self.err("Void can only be used as a standalone type", t.span.line);
                }
                if out.mask & mask::NEVER != 0 {
                    return self.err("never can only be used as a standalone type", t.span.line);
                }
                if out.is_mixed() {
                    return self.err(
                        "Type mixed can only be used as a standalone type",
                        t.span.line,
                    );
                }
            }
            TypeKind::Intersection(ts) => {
                let mut group = TypeDecl::default();
                for p in ts {
                    let single = self.resolve_type(p)?;
                    if single.mask != 0
                        || single.classes.iter().any(|n| {
                            n.eq_ignore_ascii_case(b"self") || n.eq_ignore_ascii_case(b"parent")
                        })
                    {
                        return self.err(
                            format!(
                                "Type {} cannot be part of an intersection type",
                                single.to_php_string()
                            ),
                            p.span.line,
                        );
                    }
                    for name in single.classes {
                        self.add_class_type(&mut group, name, p.span.line)?;
                    }
                }
                let g = group.classes;
                let text = |g: &[ZStr]| {
                    g.iter()
                        .map(ZStr::to_string_lossy)
                        .collect::<Vec<_>>()
                        .join("&")
                };
                for previous in &out.intersections {
                    let (smaller, larger) = if previous.len() < g.len() {
                        (previous, &g)
                    } else {
                        (&g, previous)
                    };
                    if smaller
                        .iter()
                        .all(|n| larger.iter().any(|c| c.eq_ignore_ascii_case(n.as_bytes())))
                    {
                        let message = if smaller.len() == larger.len() {
                            format!(
                                "Type {} is redundant with type {}",
                                text(smaller),
                                text(larger)
                            )
                        } else {
                            format!(
                                "Type {} is redundant as it is more restrictive than type {}",
                                text(larger),
                                text(smaller)
                            )
                        };
                        return self.err(message, t.span.line);
                    }
                }
                for c in &out.classes {
                    if g.iter().any(|n| n.eq_ignore_ascii_case(c.as_bytes())) {
                        return self.err(
                            format!(
                                "Type {} is redundant as it is more restrictive than type {c}",
                                text(&g)
                            ),
                            t.span.line,
                        );
                    }
                }
                out.members.push(crate::types::TypeMember::Intersection(
                    out.intersections.len(),
                ));
                out.intersections.push(g);
                if !in_union {
                    out.pure_intersection = true;
                }
            }
            TypeKind::Named(n) => {
                let lc = n.name.to_ascii_lowercase();
                if n.kind != ast::NameKind::Unqualified && is_builtin_type(&lc) {
                    return self.err(
                        format!(
                            "Type declaration '{}' must be unqualified",
                            String::from_utf8_lossy(&lc)
                        ),
                        t.span.line,
                    );
                }
                if n.kind == ast::NameKind::FullyQualified
                    && matches!(lc.as_slice(), b"self" | b"parent")
                {
                    return self.err(
                        format!(
                            "'\\{}' is an invalid class name",
                            String::from_utf8_lossy(&n.name)
                        ),
                        t.span.line,
                    );
                }
                if n.kind == ast::NameKind::Unqualified {
                    let m = match lc.as_slice() {
                        b"int" => Some((mask::LONG, "int")),
                        b"float" => Some((mask::DOUBLE, "float")),
                        b"string" => Some((mask::STRING, "string")),
                        b"bool" => Some((mask::BOOL, "bool")),
                        b"false" => Some((mask::FALSE, "false")),
                        b"true" => Some((mask::TRUE, "true")),
                        b"null" => Some((mask::NULL, "null")),
                        b"void" => Some((mask::VOID, "void")),
                        b"never" => Some((mask::NEVER, "never")),
                        b"mixed" => Some((mask::ANY, "mixed")),
                        b"object" => Some((mask::OBJECT, "object")),
                        b"iterable" => Some((mask::ITERABLE, "iterable")),
                        b"array" => Some((mask::ARRAY, "array")),
                        b"callable" => Some((mask::CALLABLE, "callable")),
                        b"static" => Some((mask::STATIC, "static")),
                        _ => None,
                    };
                    if let Some((m, nm)) = m {
                        if in_union && m == mask::ANY {
                            return self.err(
                                "Type mixed can only be used as a standalone type",
                                t.span.line,
                            );
                        }
                        return self.add_mask(out, m, nm, t.span.line);
                    }
                    if matches!(lc.as_slice(), b"self" | b"parent") {
                        if self.current_class().is_none() && self.closure_depth == 0 {
                            return self.err(
                                format!(
                                    "Cannot use \"{}\" when no class scope is active",
                                    String::from_utf8_lossy(&lc)
                                ),
                                t.span.line,
                            );
                        }
                        if lc.as_slice() == b"parent"
                            && self
                                .current_class()
                                .is_some_and(|c| !c.is_trait && c.parent.is_none())
                        {
                            return self.err(
                                "Cannot use \"parent\" when current class scope has no parent",
                                t.span.line,
                            );
                        }
                        // `zend_compile_single_typename`: self/parent are not
                        // substituted in traits or anonymous classes (GH-18373).
                        let known = self.current_class().filter(|c| {
                            !c.is_trait
                                && !c.is_anon
                                && !self.opts.closure_name.as_ref().is_some_and(|name| {
                                    name.as_bytes().starts_with(b"{closure:pfa:")
                                })
                        });
                        let resolved = if lc.as_slice() == b"self" {
                            known.map_or_else(|| ZStr::from(n.name.as_slice()), |c| c.name.clone())
                        } else {
                            known
                                .and_then(|c| c.parent.clone())
                                .unwrap_or_else(|| ZStr::from(n.name.as_slice()))
                        };
                        self.add_class_type(out, resolved, t.span.line)?;
                        return Ok(());
                    }
                    if matches!(
                        lc.as_slice(),
                        b"integer" | b"boolean" | b"double" | b"resource"
                    ) && !self.class_uses.contains_key(&lc)
                    {
                        let correct = match lc.as_slice() {
                            b"integer" => Some("int"),
                            b"boolean" => Some("bool"),
                            b"double" => Some("float"),
                            _ => None,
                        };
                        let name = String::from_utf8_lossy(&n.name).into_owned();
                        let class_name =
                            String::from_utf8_lossy(&self.resolve_class_name(n)).into_owned();
                        let extra = if self.namespace.is_empty() {
                            ""
                        } else {
                            " or import the class with \"use\""
                        };
                        let msg = match correct {
                            Some(c) => format!(
                                "\"{name}\" will be interpreted as a class name. Did you mean \"{c}\"? Write \"\\{class_name}\"{extra} to suppress this warning"
                            ),
                            None => format!(
                                "\"{name}\" is not a supported builtin type and will be interpreted as a class name. Write \"\\{class_name}\"{extra} to suppress this warning"
                            ),
                        };
                        self.warnings.push((128, msg, t.span.line));
                    }
                }
                let resolved = ZStr::from(self.resolve_class_name(n));
                let last = resolved
                    .as_bytes()
                    .rsplit(|c| *c == b'\\')
                    .next()
                    .unwrap_or_default();
                if is_reserved_class_name(&last.to_ascii_lowercase()) {
                    return self.err(
                        format!("Cannot use \"{resolved}\" as a type name as it is reserved"),
                        t.span.line,
                    );
                }
                self.add_class_type(out, resolved, t.span.line)?;
            }
        }
        Ok(())
    }

    fn add_class_type(&mut self, out: &mut TypeDecl, name: ZStr, line: u32) -> CResult<()> {
        if name.as_bytes() == b"_" {
            self.warnings.push((
                8192,
                "Using \"_\" as a type name is deprecated since 8.4".to_string(),
                line,
            ));
        }
        if out
            .classes
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name.as_bytes()))
        {
            return self.err(format!("Duplicate type {name} is redundant"), line);
        }
        if out.mask & mask::ITERABLE != 0 && name.eq_ignore_ascii_case(b"Traversable") {
            return self.err(format!("Duplicate type {name} is redundant"), line);
        }
        for g in &out.intersections {
            if g.iter().any(|c| c.eq_ignore_ascii_case(name.as_bytes())) {
                let text = g
                    .iter()
                    .map(ZStr::to_string_lossy)
                    .collect::<Vec<_>>()
                    .join("&");
                return self.err(
                    format!("Type {text} is redundant as it is more restrictive than type {name}"),
                    line,
                );
            }
        }
        out.members
            .push(crate::types::TypeMember::Class(out.classes.len()));
        out.classes.push(name);
        Ok(())
    }

    fn add_mask(&mut self, out: &mut TypeDecl, m: u32, name: &str, line: u32) -> CResult<()> {
        if (m & (mask::ARRAY | mask::ITERABLE) != 0)
            && out.mask & (mask::ARRAY | mask::ITERABLE) != 0
        {
            return self.err("Duplicate type array is redundant", line);
        }
        if m == mask::ITERABLE {
            if out
                .classes
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"Traversable"))
            {
                return self.err("Duplicate type Traversable is redundant", line);
            }
            out.members.push(crate::types::TypeMember::Iterable);
        }
        if out.mask & m == m && m != 0 {
            return self.err(format!("Duplicate type {name} is redundant"), line);
        }
        if (m == mask::FALSE || m == mask::TRUE) && out.mask & mask::BOOL == mask::BOOL {
            return self.err(format!("Duplicate type {name} is redundant"), line);
        }
        if m == mask::BOOL && out.mask & (mask::FALSE | mask::TRUE) != 0 {
            let other = if out.mask & mask::FALSE != 0 {
                "false"
            } else {
                "true"
            };
            return self.err(format!("Duplicate type {other} is redundant"), line);
        }
        if (m == mask::TRUE && out.mask & mask::FALSE != 0)
            || (m == mask::FALSE && out.mask & mask::TRUE != 0)
        {
            return self.err(
                "Type contains both true and false, bool must be used instead",
                line,
            );
        }
        out.mask |= m;
        Ok(())
    }

    pub(crate) fn compile_class_stmt(
        &mut self,
        cd: &ast::ClassDecl,
        top_level: bool,
    ) -> CResult<()> {
        let idx = self.compile_class_decl(cd, false)?;
        if top_level && self.can_early_bind(idx) {
            self.hoisted_classes.push(idx);
        }
        self.f().line = cd.span.line;
        self.emit(Op::DeclareClass(idx));
        Ok(())
    }

    /// `zend_compile_class_decl`: only classes that implement no interface
    /// (enums and non-trait classes with `__toString()` implement one
    /// implicitly) and
    /// use no trait are bound at compile time; the others are declared when
    /// their statement runs.
    fn can_early_bind(&self, idx: u32) -> bool {
        // `idx` is in the table of the op array the class is declared in.
        let Some(table) = self.fref().decls.as_ref() else {
            return false;
        };
        let classes = table.classes.borrow();
        let Some(d) = classes.get(idx as usize) else {
            return false;
        };
        d.kind != ClassKind::Enum
            && d.interfaces.is_empty()
            && d.traits.is_empty()
            && (d.kind == ClassKind::Trait
                || !d
                    .methods
                    .iter()
                    .any(|m| m.name.eq_ignore_ascii_case(b"__tostring")))
    }

    /// Compiles a class-like declaration; returns its index.
    /// `zend_resolve_const_class_name_reference()`: a class reference
    /// (`extends`, `implements`, `use`, trait rules) may not be one of the
    /// `self`/`parent`/`static` fetch keywords.
    fn resolve_class_reference(&self, name: &ast::Name, ty: &str) -> CResult<ZStr> {
        if is_class_fetch_keyword(name) {
            return self.err(
                format!(
                    "Cannot use \"{}\" as {ty}, as it is reserved",
                    String::from_utf8_lossy(&name.name)
                ),
                name.span.line,
            );
        }
        // `zend_resolve_class_name()`: `\static` is not a class name either.
        if name.kind == ast::NameKind::FullyQualified && is_fetch_keyword(&name.name) {
            return self.err(
                format!(
                    "'\\{}' is an invalid class name",
                    String::from_utf8_lossy(&name.name)
                ),
                name.span.line,
            );
        }
        Ok(ZStr::from(self.resolve_class_name(name)))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn compile_class_decl(
        &mut self,
        cd: &ast::ClassDecl,
        is_anon: bool,
    ) -> CResult<u32> {
        let kind = match cd.kind {
            ast::ClassKind::Class => ClassKind::Class,
            ast::ClassKind::Interface => ClassKind::Interface,
            ast::ClassKind::Trait => ClassKind::Trait,
            ast::ClassKind::Enum => ClassKind::Enum,
        };
        let parent = match &cd.extends {
            Some(n) => Some(self.resolve_class_reference(n, "class name")?),
            None => None,
        };
        let name: ZStr = if is_anon {
            self.anon_counter += 1;
            let prefix = parent.as_ref().map_or_else(
                || {
                    cd.implements.first().map_or_else(
                        || "class".to_string(),
                        |i| String::from_utf8_lossy(&self.resolve_class_name(i)).into_owned(),
                    )
                },
                ZStr::to_string_lossy,
            );
            let mut v = format!("{prefix}@anonymous").into_bytes();
            v.push(0);
            v.extend_from_slice(
                format!(
                    "{}:{}${:x}",
                    self.filename,
                    cd.span.line,
                    self.anon_counter - 1
                )
                .as_bytes(),
            );
            ZStr::from(v)
        } else {
            let n = cd.name.as_ref().map(|n| n.name.clone()).unwrap_or_default();
            let lc = n.to_ascii_lowercase();
            if is_reserved_class_name(&lc) {
                return self.err(
                    format!(
                        "Cannot use '{}' as class name as it is reserved",
                        String::from_utf8_lossy(&n)
                    ),
                    cd.span.line,
                );
            }
            // `zend_assert_valid_class_name()` identifier deprecations.
            let ty = match kind {
                ClassKind::Interface => "an interface name",
                ClassKind::Trait => "a trait name",
                ClassKind::Enum => "an enum name",
                ClassKind::Class => "a class name",
            };
            if n.as_slice() == b"_" {
                self.warnings.push((
                    8192,
                    format!("Using \"_\" as {ty} is deprecated since 8.4"),
                    cd.span.line,
                ));
            } else if lc == b"let" {
                self.warnings.push((
                    8192,
                    format!("Using \"let\" as {ty} is deprecated since 8.6"),
                    cd.span.line,
                ));
            } else if lc == b"is" {
                self.warnings.push((
                    8192,
                    format!("Using \"is\" as {ty} is deprecated since 8.6"),
                    cd.span.line,
                ));
            }
            let full = self.ns_prefixed_pub(&n);
            // `zend_compile_class_decl`: the unqualified name must not
            // name a different class imported by `use`.
            if self
                .class_uses
                .get(&lc)
                .is_some_and(|import| !import.eq_ignore_ascii_case(&full))
            {
                return self.err(
                    format!(
                        "Cannot redeclare class {} (previously declared as local import)",
                        String::from_utf8_lossy(&full)
                    ),
                    cd.span.line,
                );
            }
            self.register_seen_symbol(full.to_ascii_lowercase(), super::stmt::SYMBOL_CLASS);
            ZStr::from(full)
        };
        let mut cflag = 0u32;
        match kind {
            ClassKind::Interface => cflag |= cflags::INTERFACE,
            ClassKind::Trait => cflag |= cflags::TRAIT,
            ClassKind::Enum => cflag |= cflags::ENUM | cflags::FINAL,
            ClassKind::Class => {}
        }
        if cd.modifiers.contains(ast::Modifiers::ABSTRACT) {
            cflag |= cflags::ABSTRACT | cflags::EXPLICIT_ABSTRACT;
        }
        if cd.modifiers.contains(ast::Modifiers::FINAL) {
            cflag |= cflags::FINAL;
        }
        if cd.modifiers.contains(ast::Modifiers::READONLY) {
            cflag |= cflags::READONLY;
        }
        if is_anon {
            cflag |= cflags::ANON;
        }
        // `zend_compile_class_decl` compiles the attributes with the new
        // class active: `self` in their arguments is this class.
        self.classes.push(ClassCtx {
            name: name.clone(),
            parent: parent.clone(),
            is_trait: kind == ClassKind::Trait,
            is_interface: kind == ClassKind::Interface,
            is_anon,
        });
        let saved_closure_depth = self.closure_depth;
        self.closure_depth = 0;
        let attributes = self.attributes_at(&cd.attributes, Some(cd.span.line));
        self.closure_depth = saved_closure_depth;
        self.classes.pop();
        let attributes = attributes?;
        self.validate_attributes(
            &attributes,
            super::attrs::target::CLASS,
            0,
            Some(super::attrs::AttrClass {
                name: &name,
                flags: cflag,
            }),
            false,
        )?;
        if attributes
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(b"AllowDynamicProperties"))
        {
            cflag |= cflags::ALLOW_DYNAMIC;
        }
        let interfaces = cd
            .implements
            .iter()
            .map(|n| self.resolve_class_reference(n, "interface name"))
            .collect::<CResult<Vec<ZStr>>>()?;
        self.classes.push(ClassCtx {
            name: name.clone(),
            parent: parent.clone(),
            is_trait: kind == ClassKind::Trait,
            is_interface: kind == ClassKind::Interface,
            is_anon,
        });
        let saved_closure_depth = self.closure_depth;
        self.closure_depth = 0;
        let r = self.compile_class_members(cd, &name, kind, cflag);
        self.closure_depth = saved_closure_depth;
        self.classes.pop();
        let (
            consts,
            props,
            methods,
            traits,
            trait_rules,
            cases,
            case_attributes,
            case_doc_comments,
        ) = r?;
        let backing = match &cd.backing_type {
            Some(t) => {
                let ty = self.resolve_type(t)?;
                if ty.mask == mask::LONG && !ty.has_classes() {
                    Some(mask::LONG)
                } else if ty.mask == mask::STRING && !ty.has_classes() {
                    Some(mask::STRING)
                } else {
                    return self.err(
                        format!(
                            "Enum backing type must be int or string, {} given",
                            ty.to_php_string()
                        ),
                        t.span.line,
                    );
                }
            }
            None => None,
        };
        let decl = ClassDecl {
            name,
            kind,
            flags: cflag,
            parent,
            interfaces,
            traits,
            trait_rules,
            consts,
            props,
            methods,
            cases,
            case_attributes,
            case_doc_comments,
            backing,
            attributes,
            doc_comment: self.doc_comment(cd.doc_comment.as_ref()),
            filename: self.filename.clone(),
            line_start: cd.span.line,
            line_end: cd.end_line,
        };
        Ok(self.add_class(decl))
    }

    #[allow(clippy::type_complexity, clippy::too_many_lines)]
    fn compile_class_members(
        &mut self,
        cd: &ast::ClassDecl,
        class_name: &ZStr,
        kind: ClassKind,
        cflag: u32,
    ) -> CResult<(
        Vec<ConstDecl>,
        Vec<PropDecl>,
        Vec<MethodDecl>,
        Vec<ZStr>,
        Vec<TraitRule>,
        Vec<(ZStr, Option<ConstExpr>, u32)>,
        Vec<Vec<AttributeInfo>>,
        Vec<Option<ZStr>>,
    )> {
        let mut consts = Vec::new();
        let mut props: Vec<PropDecl> = Vec::new();
        let mut methods: Vec<MethodDecl> = Vec::new();
        let mut traits = Vec::new();
        let mut rules = Vec::new();
        let mut cases = Vec::new();
        let mut case_attrs = Vec::new();
        let mut case_docs = Vec::new();
        let display = |n: &ZStr| {
            let b = n.as_bytes();
            match b.iter().position(|&c| c == 0) {
                Some(p) => String::from_utf8_lossy(&b[..p]).into_owned(),
                None => n.to_string_lossy(),
            }
        };
        let cname = display(class_name);
        for m in &cd.members {
            match &m.kind {
                ClassMemberKind::Property(pg) => {
                    // `zend_compile_prop_decl`: a readonly class makes every
                    // property readonly (static ones are then rejected below).
                    let fl = modifiers_to_flags(pg.modifiers)
                        | if cflag & cflags::READONLY != 0 {
                            flags::READONLY
                        } else {
                            0
                        };
                    if kind == ClassKind::Enum {
                        return self.err(
                            format!("Enum {cname} cannot include properties"),
                            m.span.line,
                        );
                    }
                    if pg.modifiers.contains(ast::Modifiers::FINAL)
                        && pg.modifiers.contains(ast::Modifiers::PRIVATE)
                    {
                        return self.err("Property cannot be both final and private", m.span.line);
                    }
                    if kind == ClassKind::Interface {
                        if pg.modifiers.contains(ast::Modifiers::FINAL) {
                            return self.err("Property in interface cannot be final", m.span.line);
                        }
                        if pg
                            .modifiers
                            .intersects(ast::Modifiers::PROTECTED | ast::Modifiers::PRIVATE)
                        {
                            return self.err(
                                "Property in interface cannot be protected or private",
                                m.span.line,
                            );
                        }
                        if pg.modifiers.contains(ast::Modifiers::ABSTRACT) {
                            return self.err("Property in interface cannot be explicitly abstract. All interface members are implicitly abstract", m.span.line);
                        }
                    }
                    if pg.props.iter().any(|p| p.hooks.is_none()) {
                        if kind == ClassKind::Interface {
                            return self
                                .err("Interfaces may only include hooked properties", m.span.line);
                        }
                        if pg.modifiers.contains(ast::Modifiers::ABSTRACT) {
                            return self.err(
                                "Only hooked properties may be declared abstract",
                                m.span.line,
                            );
                        }
                    }
                    let ty = match &pg.ty {
                        Some(t) => {
                            let ty = self.resolve_type(t)?;
                            if ty.mask & mask::VOID != 0 {
                                return self.err(
                                    format!(
                                        "Property {cname}::${} cannot have type void",
                                        pg.props[0].name.as_str_lossy()
                                    ),
                                    t.span.line,
                                );
                            }
                            if ty.mask & mask::NEVER != 0 {
                                return self.err(
                                    format!(
                                        "Property {cname}::${} cannot have type never",
                                        pg.props[0].name.as_str_lossy()
                                    ),
                                    t.span.line,
                                );
                            }
                            if ty.mask & mask::CALLABLE != 0 {
                                return self.err(
                                    format!(
                                        "Property {cname}::${} cannot have type {}",
                                        pg.props[0].name.as_str_lossy(),
                                        ty.to_php_string()
                                    ),
                                    t.span.line,
                                );
                            }
                            Some(ty)
                        }
                        None => None,
                    };
                    // `zend_compile_prop_decl` compiles the default and the
                    // attributes with `active_property_info_name` set
                    // (`__PROPERTY__`).
                    let first = pg.props.first().map(|p| p.name.name.clone());
                    let saved_prop = std::mem::replace(&mut self.hook_prop, first);
                    let attrs = self.attributes(&pg.attributes);
                    self.hook_prop = saved_prop;
                    let attrs = attrs?;
                    self.validate_attributes(
                        &attrs,
                        super::attrs::target::PROPERTY,
                        0,
                        None,
                        false,
                    )?;
                    for (pi, p) in pg.props.iter().enumerate() {
                        self.f().line = p.span.line;
                        if props
                            .iter()
                            .any(|q| q.name.as_bytes() == p.name.name.as_slice())
                        {
                            return self.err(
                                format!("Cannot redeclare {cname}::${}", p.name.as_str_lossy()),
                                p.span.line,
                            );
                        }
                        if fl & flags::READONLY != 0 {
                            if ty.is_none() {
                                return self.err(
                                    format!(
                                        "Readonly property {cname}::${} must have type",
                                        p.name.as_str_lossy()
                                    ),
                                    p.span.line,
                                );
                            }
                            if fl & flags::STATIC != 0 {
                                return self.err(
                                    format!(
                                        "Static property {cname}::${} cannot be readonly",
                                        p.name.as_str_lossy()
                                    ),
                                    p.span.line,
                                );
                            }
                        }
                        let default = match &p.default {
                            Some(d) => {
                                self.check_const_expr(d, false)?;
                                let saved_prop = self.hook_prop.replace(p.name.name.clone());
                                let v = self.const_expr(d);
                                self.hook_prop = saved_prop;
                                Some(v?)
                            }
                            None => {
                                if ty.is_none() {
                                    Some(ConstExpr::Value(Value::Null))
                                } else {
                                    None
                                }
                            }
                        };
                        if let (Some(ty), Some(ConstExpr::Value(value))) = (&ty, &default) {
                            if !ty.accepts_default(value) {
                                let type_name = ty.to_php_string();
                                let message = if value.is_null() && !ty.pure_intersection {
                                    let mut nullable = ty.clone();
                                    nullable.mask |= mask::NULL;
                                    format!("Default value for property of type {type_name} may not be null. Use the nullable type {} to allow null default value", nullable.to_php_string())
                                } else {
                                    format!("Cannot use {} as default value for property {cname}::${} of type {type_name}", value.value_name(), p.name.as_str_lossy())
                                };
                                return self.err(message, p.span.line);
                            }
                        }
                        let mut fl = normalize_set_visibility(
                            self,
                            fl,
                            ty.is_some(),
                            &cname,
                            &p.name.as_str_lossy(),
                            p.span.line,
                        )?;
                        let hooks = match &p.hooks {
                            Some(hs) => {
                                let (h, virt) = self.compile_prop_hooks(
                                    &cname,
                                    &p.name.name,
                                    pg.ty.as_ref(),
                                    hs,
                                    kind == ClassKind::Interface,
                                    fl,
                                    p.span.line,
                                )?;
                                if virt {
                                    fl |= flags::VIRTUAL;
                                }
                                Some(Rc::new(h))
                            }
                            None => None,
                        };
                        let default = if fl & flags::VIRTUAL != 0 && p.default.is_none() {
                            None
                        } else {
                            default
                        };
                        let attributes = if pi == 0 || pg.attributes.is_empty() {
                            attrs.clone()
                        } else {
                            let saved_prop = self.hook_prop.replace(p.name.name.clone());
                            let a = self.attributes(&pg.attributes);
                            self.hook_prop = saved_prop;
                            a?
                        };
                        props.push(PropDecl {
                            name: ZStr::from(p.name.name.as_slice()),
                            default,
                            flags: fl,
                            ty: ty.clone(),
                            doc_comment: self.doc_comment(p.doc_comment.as_ref()),
                            attributes,
                            line: p.span.line,
                            hooks,
                        });
                    }
                }
                ClassMemberKind::Const(cg) => {
                    let fl = modifiers_to_flags(cg.modifiers);
                    if cg.modifiers.contains(ast::Modifiers::STATIC) {
                        return self.err("Cannot use 'static' as constant modifier", m.span.line);
                    }
                    if cg.modifiers.contains(ast::Modifiers::ABSTRACT) {
                        return self.err("Cannot use 'abstract' as constant modifier", m.span.line);
                    }
                    if cg.modifiers.contains(ast::Modifiers::READONLY) {
                        return self.err("Cannot use 'readonly' as constant modifier", m.span.line);
                    }
                    let ty = match &cg.ty {
                        Some(t) => Some(self.resolve_type(t)?),
                        None => None,
                    };
                    let attrs = self.attributes(&cg.attributes)?;
                    self.validate_attributes(
                        &attrs,
                        super::attrs::target::CLASS_CONST,
                        0,
                        None,
                        false,
                    )?;
                    for c in &cg.consts {
                        if c.name.eq_ignore_case("class") {
                            return self.err(
                                "A class constant must not be called 'class'; it is reserved for class name fetching",
                                c.span.line,
                            );
                        }
                        if c.name.eq_ignore_case("namespace") {
                            let object_type = match kind {
                                ClassKind::Class => "class",
                                ClassKind::Interface => "interface",
                                ClassKind::Trait => "trait",
                                ClassKind::Enum => "enum",
                            };
                            self.warnings.push((
                                8192,
                                format!("Declaring {object_type} constant called 'namespace' is deprecated"),
                                c.span.line,
                            ));
                        }
                        if consts
                            .iter()
                            .any(|q: &ConstDecl| q.name.as_bytes() == c.name.name.as_slice())
                        {
                            return self.err(
                                format!(
                                    "Cannot redefine class constant {cname}::{}",
                                    c.name.as_str_lossy()
                                ),
                                c.span.line,
                            );
                        }
                        if fl & flags::PRIVATE != 0 && fl & flags::FINAL != 0 {
                            return self.err(
                                format!("Private constant {cname}::{} cannot be final as it is not visible to other classes", c.name.as_str_lossy()),
                                c.span.line,
                            );
                        }
                        // `zend_declare_typed_class_constant`.
                        if kind == ClassKind::Interface
                            && fl & (flags::PRIVATE | flags::PROTECTED) != 0
                        {
                            return self.err(
                                format!(
                                    "Access type for interface constant {cname}::{} must be public",
                                    c.name.as_str_lossy()
                                ),
                                c.span.line,
                            );
                        }
                        self.f().line = c.span.line;
                        self.check_const_expr(&c.value, false)?;
                        if let Some(ty) = &ty {
                            if ty.mask & (mask::CALLABLE | mask::VOID | mask::NEVER) != 0 {
                                return self.err(
                                    format!(
                                        "Class constant {cname}::{} cannot have type {}",
                                        c.name.as_str_lossy(),
                                        ty.to_php_string()
                                    ),
                                    c.span.line,
                                );
                            }
                        }
                        let value = self.const_expr(&c.value)?;
                        if let (Some(ty), ConstExpr::Value(value)) = (&ty, &value) {
                            if !ty.accepts_default(value) {
                                return self.err(format!("Cannot use {} as value for class constant {cname}::{} of type {}", value.value_name(), c.name.as_str_lossy(), ty.to_php_string()), c.span.line);
                            }
                        }
                        consts.push(ConstDecl {
                            name: ZStr::from(c.name.name.as_slice()),
                            value,
                            flags: fl,
                            ty: ty.clone(),
                            doc_comment: self.doc_comment(c.doc_comment.as_ref()),
                            attributes: attrs.clone(),
                            line: c.span.line,
                            cases_before: u32::try_from(cases.len()).unwrap_or(u32::MAX),
                        });
                    }
                }
                ClassMemberKind::Method(md) => {
                    let mut fl = modifiers_to_flags(md.modifiers);
                    let mname = md.name.as_str_lossy().into_owned();
                    if methods
                        .iter()
                        .any(|q| q.name.eq_ignore_ascii_case(&md.name.name))
                    {
                        return self
                            .err(format!("Cannot redeclare {cname}::{mname}()"), md.span.line);
                    }
                    if kind == ClassKind::Interface {
                        if md.body.is_some() {
                            return self.err(
                                format!(
                                    "Interface function {cname}::{mname}() cannot contain body"
                                ),
                                md.span.line,
                            );
                        }
                        if fl & (flags::PRIVATE | flags::PROTECTED) != 0 {
                            return self.err(
                                format!("Access type for interface method {cname}::{mname}() must be public"),
                                md.span.line,
                            );
                        }
                        if fl & flags::FINAL != 0 {
                            return self.err(
                                format!("Interface method {cname}::{mname}() must not be final"),
                                md.span.line,
                            );
                        }
                        if fl & flags::ABSTRACT != 0 {
                            return self.err(
                                format!("Interface method {cname}::{mname}() must not be abstract"),
                                md.span.line,
                            );
                        }
                        fl |= flags::ABSTRACT;
                    } else if fl & flags::ABSTRACT != 0 {
                        if md.body.is_some() {
                            return self.err(
                                format!("Abstract function {cname}::{mname}() cannot contain body"),
                                md.span.line,
                            );
                        }
                        if fl & flags::PRIVATE != 0 && kind != ClassKind::Trait {
                            return self.err(
                                format!("Abstract function {cname}::{mname}() cannot be declared private"),
                                md.span.line,
                            );
                        }
                        if fl & flags::FINAL != 0 {
                            return self.err(
                                format!("Cannot use the final modifier on an abstract method {cname}::{mname}()"),
                                md.span.line,
                            );
                        }
                        if kind == ClassKind::Class
                            && cflag & cflags::EXPLICIT_ABSTRACT == 0
                            && cflag & cflags::ANON == 0
                        {
                            return self.err(
                                format!("Class {cname} declares abstract method {mname}() and must therefore be declared abstract"),
                                md.span.line,
                            );
                        }
                    } else if md.body.is_none() {
                        return self.err(
                            format!("Non-abstract method {cname}::{mname}() must contain body"),
                            md.span.line,
                        );
                    }
                    // "Private methods cannot be final ..." is a parser
                    // diagnostic (`php_parser::validate`).
                    let is_ctor = md.name.eq_ignore_case("__construct");
                    if !is_ctor
                        && md
                            .params
                            .iter()
                            .any(|p| !p.modifiers.is_empty() || p.hooks.is_some())
                    {
                        return self.err(
                            "Cannot declare promoted property outside a constructor",
                            md.span.line,
                        );
                    }
                    if is_ctor {
                        for p in &md.params {
                            if p.modifiers.is_empty() && p.hooks.is_none() {
                                continue;
                            }
                            if kind == ClassKind::Interface || fl & flags::ABSTRACT != 0 {
                                return self.err(
                                    "Cannot declare promoted property in an abstract constructor",
                                    p.span.line,
                                );
                            }
                            let pfl = modifiers_to_flags(p.modifiers)
                                | if cflag & cflags::READONLY != 0 {
                                    flags::READONLY
                                } else {
                                    0
                                };
                            let ty = match &p.ty {
                                Some(t) => Some(self.resolve_type(t)?),
                                None => None,
                            };
                            if pfl & flags::READONLY != 0 && ty.is_none() {
                                return self.err(
                                    format!(
                                        "Readonly property {cname}::${} must have type",
                                        p.name.as_str_lossy()
                                    ),
                                    p.span.line,
                                );
                            }
                            if props
                                .iter()
                                .any(|q| q.name.as_bytes() == p.name.name.as_slice())
                            {
                                return self.err(
                                    format!("Cannot redeclare {cname}::${}", p.name.as_str_lossy()),
                                    p.span.line,
                                );
                            }
                            // zend_compile_params: a promoted property may not be
                            // `callable` (Zend/zend_compile.c).
                            if let Some(t) = &ty {
                                if t.mask & mask::CALLABLE != 0 {
                                    return self.err(
                                        format!(
                                            "Property {cname}::${} cannot have type {}",
                                            p.name.as_str_lossy(),
                                            t.to_php_string()
                                        ),
                                        p.span.line,
                                    );
                                }
                            }
                            let mut attrs = self.attributes(&p.attributes)?;
                            self.validate_attributes(
                                &attrs,
                                super::attrs::target::PROPERTY,
                                super::attrs::target::PARAMETER,
                                None,
                                false,
                            )?;
                            super::attrs::retain_promoted(
                                &mut attrs,
                                super::attrs::target::PROPERTY,
                                super::attrs::target::PARAMETER,
                            );
                            let mut pfl = normalize_set_visibility(
                                self,
                                pfl,
                                ty.is_some(),
                                &cname,
                                &p.name.as_str_lossy(),
                                p.span.line,
                            )?;
                            // Hooks on a promoted property.
                            let hooks = match &p.hooks {
                                Some(hs) => {
                                    let (h, virt) = self.compile_prop_hooks(
                                        &cname,
                                        &p.name.name,
                                        p.ty.as_ref(),
                                        hs,
                                        kind == ClassKind::Interface,
                                        pfl,
                                        p.span.line,
                                    )?;
                                    if virt {
                                        pfl |= flags::VIRTUAL;
                                    }
                                    Some(Rc::new(h))
                                }
                                None => None,
                            };
                            props.push(PropDecl {
                                name: ZStr::from(p.name.name.as_slice()),
                                default: if ty.is_none() && pfl & flags::VIRTUAL == 0 {
                                    Some(ConstExpr::Value(Value::Null))
                                } else {
                                    None
                                },
                                flags: pfl,
                                ty,
                                doc_comment: self.doc_comment(p.doc_comment.as_ref()),
                                attributes: attrs,
                                line: p.span.line,
                                hooks,
                            });
                        }
                    }
                    let spec = BodySpec {
                        kind: FnKind::Method,
                        name: ZStr::from(md.name.name.as_slice()),
                        method_name: ZStr::from(md.name.name.as_slice()),
                        params: &md.params,
                        return_type: md.return_type.as_ref(),
                        by_ref: md.by_ref,
                        is_generator: md.is_generator,
                        is_static: fl & flags::STATIC != 0,
                        line_start: md.span.line,
                        line_end: md.end_line,
                        doc_comment: md.doc_comment.as_ref(),
                        attributes: &md.attributes,
                        is_ctor,
                        uses: Vec::new(),
                        has_body: md.body.is_some(),
                    };
                    let body = match &md.body {
                        Some(b) => BodyKind::Stmts(b),
                        None => BodyKind::None,
                    };
                    let mut uf = self.compile_body(&spec, body)?;
                    uf.flags |= fl;
                    self.check_magic_method(&cname, &md.name.as_str_lossy(), &uf, md.span.line)?;
                    if md.name.eq_ignore_case("__tostring") && uf.return_type.is_none() {
                        uf.return_type =
                            Some(crate::types::TypeDecl::simple(crate::types::mask::STRING));
                    }
                    if is_ctor {
                        uf.flags |= flags::CTOR;
                    }
                    methods.push(MethodDecl {
                        name: ZStr::from(md.name.name.as_slice()),
                        func: Rc::new(uf),
                        flags: fl,
                    });
                }
                ClassMemberKind::TraitUse(tu) => {
                    if kind == ClassKind::Interface {
                        return self.err(
                            format!(
                                "Cannot use traits inside of interfaces. {} is used in {cname}",
                                String::from_utf8_lossy(&self.resolve_class_name(&tu.traits[0]))
                            ),
                            m.span.line,
                        );
                    }
                    for t in &tu.traits {
                        traits.push(self.resolve_class_reference(t, "trait name")?);
                    }
                    for a in &tu.adaptations {
                        match a {
                            ast::TraitAdaptation::Precedence {
                                method, insteadof, ..
                            } => {
                                let tn = match &method.trait_name {
                                    Some(n) => self.resolve_class_reference(n, "trait name")?,
                                    None => ZStr::default(),
                                };
                                let insteadof = insteadof
                                    .iter()
                                    .map(|n| self.resolve_class_reference(n, "trait name"))
                                    .collect::<CResult<Vec<ZStr>>>()?;
                                rules.push(TraitRule::Precedence {
                                    trait_name: tn,
                                    method: ZStr::from(method.method.name.as_slice()),
                                    insteadof,
                                });
                            }
                            ast::TraitAdaptation::Alias {
                                method,
                                modifiers,
                                alias,
                                ..
                            } => {
                                let f = if modifiers.is_empty() {
                                    0
                                } else {
                                    let mut f = modifiers_to_flags(*modifiers);
                                    if !modifiers.intersects(ast::Modifiers::VISIBILITY) {
                                        f &= !flags::PUBLIC;
                                    }
                                    f
                                };
                                let trait_name = match &method.trait_name {
                                    Some(n) => Some(self.resolve_class_reference(n, "trait name")?),
                                    None => None,
                                };
                                rules.push(TraitRule::Alias {
                                    trait_name,
                                    method: ZStr::from(method.method.name.as_slice()),
                                    flags: f,
                                    alias: alias.as_ref().map(|a| ZStr::from(a.name.as_slice())),
                                });
                            }
                        }
                    }
                }
                ClassMemberKind::EnumCase(ec) => {
                    if kind != ClassKind::Enum {
                        return self.err(format!("Case can only be used in enums"), m.span.line);
                    }
                    let v = match &ec.value {
                        Some(e) => {
                            self.check_const_expr(e, false)?;
                            Some(self.const_expr(e)?)
                        }
                        None => None,
                    };
                    match (&ec.value, &cd.backing_type) {
                        (None, Some(_)) => {
                            return self.err(
                                format!(
                                    "Case {} of backed enum {cname} must have a value",
                                    ec.name.as_str_lossy()
                                ),
                                m.span.line,
                            );
                        }
                        (Some(_), None) => {
                            return self.err(
                                format!(
                                    "Case {} of non-backed enum {cname} must not have a value",
                                    ec.name.as_str_lossy()
                                ),
                                m.span.line,
                            );
                        }
                        _ => {}
                    }
                    // zend_declare_typed_class_constant (via zend_compile_enum_case).
                    if ec.name.eq_ignore_case("namespace") {
                        self.warnings.push((
                            8192,
                            "Declaring enum constant called 'namespace' is deprecated".to_string(),
                            m.span.line,
                        ));
                    }
                    if cases
                        .iter()
                        .any(|(n, _, _): &(ZStr, Option<ConstExpr>, u32)| {
                            n.as_bytes() == ec.name.name.as_slice()
                        })
                        || consts
                            .iter()
                            .any(|c: &ConstDecl| c.name.as_bytes() == ec.name.name.as_slice())
                    {
                        return self.err(
                            format!(
                                "Cannot redefine class constant {cname}::{}",
                                ec.name.as_str_lossy()
                            ),
                            m.span.line,
                        );
                    }
                    let attrs = self.attributes(&ec.attributes)?;
                    self.validate_attributes(
                        &attrs,
                        super::attrs::target::CLASS_CONST,
                        0,
                        None,
                        false,
                    )?;
                    case_attrs.push(attrs);
                    case_docs.push(self.doc_comment(ec.doc_comment.as_ref()));
                    cases.push((ZStr::from(ec.name.name.as_slice()), v, m.span.line));
                }
            }
        }
        Ok((
            consts, props, methods, traits, rules, cases, case_attrs, case_docs,
        ))
    }
}

impl Compiler<'_> {
    /// Compiles the hooks of a property; returns the hooks and whether the
    /// property is virtual.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn compile_prop_hooks(
        &mut self,
        cname: &str,
        prop: &[u8],
        prop_ty: Option<&ast::Type>,
        hooks: &[ast::PropertyHook],
        is_interface: bool,
        prop_flags: u32,
        line: u32,
    ) -> CResult<(PropHooks, bool)> {
        let pname = String::from_utf8_lossy(prop).into_owned();
        if prop_flags & flags::READONLY != 0 {
            return self.err("Hooked properties cannot be readonly", line);
        }
        if hooks.is_empty() {
            return self.err("Property hook list must not be empty", line);
        }
        if prop_flags & flags::STATIC != 0 {
            return self.err("Cannot declare hooks for static property", line);
        }
        let mut out = PropHooks {
            class: self
                .current_class()
                .map(|c| c.name.clone())
                .unwrap_or_default(),
            ..PropHooks::default()
        };
        let mut is_virtual = true;
        if is_interface {
            is_virtual = true;
        }
        for h in hooks {
            let hname = h.name.name.to_ascii_lowercase();
            let is_get = match hname.as_slice() {
                b"get" => true,
                b"set" => false,
                _ => {
                    return self.err(
                        format!(
                            "Unknown hook \"{}\" for property {cname}::${pname}, expected \"get\" or \"set\"",
                            h.name.as_str_lossy()
                        ),
                        h.span.line,
                    )
                }
            };
            let hm = modifiers_to_flags(h.modifiers);
            let is_final = h.modifiers.contains(ast::Modifiers::FINAL);
            if is_final && prop_flags & flags::PRIVATE != 0 {
                return self.err(
                    "Property hook cannot be both final and private",
                    h.span.line,
                );
            }
            for (m, n) in [
                (ast::Modifiers::PUBLIC, "public"),
                (ast::Modifiers::PROTECTED, "protected"),
                (ast::Modifiers::PRIVATE, "private"),
                (ast::Modifiers::STATIC, "static"),
                (ast::Modifiers::READONLY, "readonly"),
            ] {
                if h.modifiers.contains(m) {
                    return self.err(
                        format!("Cannot use the {n} modifier on a property hook"),
                        h.span.line,
                    );
                }
            }
            let _ = hm;
            let has_body = !matches!(h.body, ast::HookBody::None);
            let is_abstract = is_interface || (prop_flags & flags::ABSTRACT != 0 && !has_body);
            if is_abstract {
                if has_body {
                    return self.err("Abstract property hook cannot have body", h.span.line);
                }
                if prop_flags & flags::PRIVATE != 0 {
                    return self.err(
                        "Property hook cannot be both abstract and private",
                        h.span.line,
                    );
                }
                if is_final {
                    return self.err(
                        "Property hook cannot be both abstract and final",
                        h.span.line,
                    );
                }
            } else if !has_body {
                return self.err("Non-abstract property hook must have a body", h.span.line);
            }
            if (is_get && out.get.is_some()) || (!is_get && out.set.is_some()) {
                return self.err(
                    format!(
                        "Cannot redeclare property hook \"{}\"",
                        h.name.as_str_lossy()
                    ),
                    h.span.line,
                );
            }
            // Parameters.
            let implicit_param;
            let params: &[ast::Param] = if is_get {
                if h.params.is_some() {
                    return self.err(
                        format!(
                            "get hook of property {cname}::${pname} must not have a parameter list"
                        ),
                        h.span.line,
                    );
                }
                &[]
            } else if let Some(ps) = &h.params {
                if ps.len() != 1 {
                    return self.err(
                        format!("set hook of property {cname}::${pname} must accept exactly one parameters"),
                        h.span.line,
                    );
                }
                let p = &ps[0];
                let n = p.name.as_str_lossy();
                if p.by_ref {
                    return self.err(
                        format!("Parameter ${n} of set hook {cname}::${pname} must not be pass-by-reference"),
                        h.span.line,
                    );
                }
                if p.variadic {
                    return self.err(
                        format!(
                            "Parameter ${n} of set hook {cname}::${pname} must not be variadic"
                        ),
                        h.span.line,
                    );
                }
                if p.default.is_some() {
                    return self.err(
                        format!("Parameter ${n} of set hook {cname}::${pname} must not have a default value"),
                        h.span.line,
                    );
                }
                if prop_ty.is_some() != p.ty.is_some() {
                    return self.err(
                        format!("Type of parameter ${n} of hook {cname}::${pname}::set must be compatible with property type"),
                        h.span.line,
                    );
                }
                ps
            } else {
                implicit_param = vec![ast::Param {
                    attributes: Vec::new(),
                    modifiers: ast::Modifiers::NONE,
                    ty: prop_ty.cloned(),
                    by_ref: false,
                    variadic: false,
                    name: ast::Ident::new(b"value".to_vec(), h.span),
                    default: None,
                    doc_comment: None,
                    hooks: None,
                    span: h.span,
                }];
                &implicit_param
            };
            // Body.
            let body_stmts: Vec<ast::Stmt>;
            let body = match &h.body {
                ast::HookBody::None => BodyKind::None,
                ast::HookBody::Block(b) => BodyKind::Stmts(b),
                ast::HookBody::Expr(e) => {
                    let sp = e.span;
                    let st = if is_get {
                        ast::Stmt {
                            kind: ast::StmtKind::Return(Some((**e).clone())),
                            span: sp,
                        }
                    } else {
                        is_virtual = false;
                        let target = ast::Expr::new(
                            ast::ExprKind::Prop {
                                object: Box::new(ast::Expr::new(
                                    ast::ExprKind::Var(ast::VarName::Name(ast::Ident::new(
                                        b"this".to_vec(),
                                        sp,
                                    ))),
                                    sp,
                                )),
                                name: ast::MemberName::Ident(ast::Ident::new(prop.to_vec(), sp)),
                                nullsafe: false,
                            },
                            sp,
                        );
                        ast::Stmt {
                            kind: ast::StmtKind::Expr(ast::Expr::new(
                                ast::ExprKind::Assign {
                                    target: Box::new(target),
                                    value: e.clone(),
                                },
                                sp,
                            )),
                            span: sp,
                        }
                    };
                    body_stmts = vec![st];
                    BodyKind::Stmts(&body_stmts)
                }
            };
            let void_ty;
            let ret: Option<&ast::Type> = if is_get {
                prop_ty
            } else {
                void_ty = ast::Type {
                    kind: ast::TypeKind::Named(ast::Name {
                        kind: ast::NameKind::Unqualified,
                        name: b"void".to_vec(),
                        span: h.span,
                    }),
                    span: h.span,
                };
                Some(&void_ty)
            };
            let fname = ZStr::from(format!("${pname}::{}", if is_get { "get" } else { "set" }));
            let spec = BodySpec {
                kind: FnKind::Method,
                name: fname.clone(),
                method_name: fname,
                params,
                return_type: ret,
                by_ref: h.by_ref,
                is_generator: h.is_generator,
                is_static: false,
                line_start: h.span.line,
                line_end: h.end_line,
                doc_comment: h.doc_comment.as_ref(),
                attributes: &h.attributes,
                is_ctor: false,
                uses: Vec::new(),
                has_body,
            };
            let saved = self.hook_prop.replace(prop.to_vec());
            let r = self.compile_body(&spec, body);
            self.hook_prop = saved;
            let mut uf = r?;
            uf.hook_prop = Some(ZStr::from(prop));
            let vis = if prop_flags & flags::PRIVATE != 0 {
                flags::PRIVATE
            } else {
                flags::PUBLIC
            };
            uf.flags |= vis
                | if is_final { flags::FINAL } else { 0 }
                | if is_abstract { flags::ABSTRACT } else { 0 };
            // Backing store usage: `$this->prop` inside the hook body.
            if has_body && uses_backing(&uf.code, prop) {
                is_virtual = false;
            }
            let uf = Rc::new(uf);
            if is_get {
                out.get = Some(uf);
                out.final_get = is_final;
                out.abstract_get = is_abstract;
            } else {
                out.set = Some(uf);
                out.final_set = is_final;
                out.abstract_set = is_abstract;
            }
        }
        if prop_flags & flags::ABSTRACT != 0 && !out.abstract_get && !out.abstract_set {
            return self.err(
                format!(
                    "Abstract property {}::${} must specify at least one abstract hook",
                    cname,
                    String::from_utf8_lossy(prop)
                ),
                line,
            );
        }
        Ok((out, is_virtual))
    }
}

fn uses_backing(code: &crate::opcode::OpArray, prop: &[u8]) -> bool {
    code.places.iter().any(|p| {
        matches!(p.base, crate::opcode::PlaceBase::This)
            && matches!(p.segs.first(), Some(crate::opcode::Seg::Prop(crate::opcode::Operand::Lit(i)))
                if matches!(&code.literals[*i as usize], php_runtime::Value::Str(s) if s.as_bytes() == prop))
    })
}

impl Compiler<'_> {
    /// `zend_check_magic_method_implementation`.
    #[allow(clippy::too_many_lines)]
    fn check_magic_method(
        &mut self,
        cname: &str,
        name: &str,
        uf: &UserFunction,
        line: u32,
    ) -> CResult<()> {
        let lc = name.to_ascii_lowercase();
        if !lc.starts_with("__") {
            return Ok(());
        }
        let nargs = uf.params.iter().filter(|p| !p.variadic).count();
        let is_static = uf.flags & flags::STATIC != 0;
        let has_ret = uf.return_type.is_some();
        let args = |c: &Compiler, n: usize| -> CResult<()> {
            if nargs != n {
                let msg = match n {
                    0 => format!("Method {cname}::{name}() cannot take arguments"),
                    1 => format!("Method {cname}::{name}() must take exactly 1 argument"),
                    _ => format!("Method {cname}::{name}() must take exactly {n} arguments"),
                };
                return c.err(msg, line);
            }
            if uf.params.iter().take(n).any(|p| p.by_ref) {
                return c.err(
                    format!("Method {cname}::{name}() cannot take arguments by reference"),
                    line,
                );
            }
            Ok(())
        };
        let non_static = |c: &Compiler| -> CResult<()> {
            if is_static {
                return c.err(format!("Method {cname}::{name}() cannot be static"), line);
            }
            Ok(())
        };
        let must_static = |c: &Compiler| -> CResult<()> {
            if !is_static {
                return c.err(format!("Method {cname}::{name}() must be static"), line);
            }
            Ok(())
        };
        let arg_type = |c: &Compiler, i: usize, m: u32, tn: &str| -> CResult<()> {
            if let Some(p) = uf.params.get(i) {
                if let Some(t) = &p.ty {
                    if t.mask & m == 0 && !t.is_mixed() {
                        return c.err(
                            format!("{cname}::{name}(): Parameter #{} (${}) must be of type {tn} when declared", i + 1, p.name),
                            line,
                        );
                    }
                }
            }
            Ok(())
        };
        let ret_type = |c: &Compiler, m: u32, tn: &str| -> CResult<()> {
            let Some(t) = &uf.return_type else {
                return Ok(());
            };
            if t.mask & crate::types::mask::NEVER != 0 {
                return Ok(());
            }
            let mut extra = t.mask & !m & !crate::types::mask::ITERABLE;
            if t.mask & crate::types::mask::ITERABLE != 0 && m & crate::types::mask::ARRAY == 0 {
                extra |= crate::types::mask::ARRAY;
            }
            let complex = t.has_classes() || extra & crate::types::mask::STATIC != 0;
            extra &= !crate::types::mask::STATIC;
            if extra != 0 || (complex && m != crate::types::mask::OBJECT) {
                return c.err(
                    format!("{cname}::{name}(): Return type must be {tn} when declared"),
                    line,
                );
            }
            Ok(())
        };
        let public = |c: &mut Compiler| {
            if uf.flags & flags::PUBLIC == 0 {
                c.warnings.push((
                    2,
                    format!("The magic method {cname}::{name}() must have public visibility"),
                    line,
                ));
            }
        };
        let no_ret = |c: &Compiler| -> CResult<()> {
            if has_ret {
                return c.err(
                    format!("Method {cname}::{name}() cannot declare a return type"),
                    line,
                );
            }
            Ok(())
        };
        // `zend_check_magic_method_no_return_type` /
        // `zend_check_magic_method_return_type(MAY_BE_VOID)`: a magic method
        // that cannot return a value cannot be `#[\NoDiscard]`.
        let no_discard = |c: &Compiler| -> CResult<()> {
            if uf.flags & flags::NODISCARD != 0 {
                return c.err(
                    format!("Method {cname}::{name} cannot be #[\\NoDiscard]"),
                    line,
                );
            }
            Ok(())
        };
        use crate::types::mask as m;
        match lc.as_str() {
            "__construct" => {
                non_static(self)?;
                no_ret(self)?;
                no_discard(self)?;
            }
            "__destruct" => {
                args(self, 0)?;
                non_static(self)?;
                no_ret(self)?;
                no_discard(self)?;
            }
            "__clone" => {
                args(self, 0)?;
                non_static(self)?;
                ret_type(self, m::VOID, "void")?;
                no_discard(self)?;
            }
            "__get" => {
                args(self, 1)?;
                non_static(self)?;
                public(self);
                arg_type(self, 0, m::STRING, "string")?;
            }
            "__set" => {
                args(self, 2)?;
                non_static(self)?;
                public(self);
                arg_type(self, 0, m::STRING, "string")?;
                ret_type(self, m::VOID, "void")?;
                no_discard(self)?;
            }
            "__unset" => {
                args(self, 1)?;
                non_static(self)?;
                public(self);
                arg_type(self, 0, m::STRING, "string")?;
                ret_type(self, m::VOID, "void")?;
                no_discard(self)?;
            }
            "__isset" => {
                args(self, 1)?;
                non_static(self)?;
                public(self);
                arg_type(self, 0, m::STRING, "string")?;
                ret_type(self, m::BOOL, "bool")?;
            }
            "__call" | "__callstatic" => {
                args(self, 2)?;
                if lc == "__call" {
                    non_static(self)?;
                } else {
                    must_static(self)?;
                }
                public(self);
                arg_type(self, 0, m::STRING, "string")?;
                arg_type(self, 1, m::ARRAY, "array")?;
            }
            "__tostring" => {
                args(self, 0)?;
                non_static(self)?;
                public(self);
                ret_type(self, m::STRING, "string")?;
            }
            "__debuginfo" => {
                args(self, 0)?;
                non_static(self)?;
                public(self);
                ret_type(self, m::ARRAY | m::NULL, "?array")?;
                if uf.return_type.as_ref().is_some_and(TypeDecl::allows_null) {
                    self.warnings.push((8192, format!("Returning null from {cname}::__debugInfo() is deprecated, make the return type non-nullable and return an empty array instead"), line));
                }
            }
            "__serialize" | "__sleep" => {
                args(self, 0)?;
                non_static(self)?;
                public(self);
                ret_type(self, m::ARRAY, "array")?;
            }
            "__unserialize" => {
                args(self, 1)?;
                non_static(self)?;
                public(self);
                arg_type(self, 0, m::ARRAY, "array")?;
                ret_type(self, m::VOID, "void")?;
                no_discard(self)?;
            }
            "__set_state" => {
                args(self, 1)?;
                must_static(self)?;
                public(self);
                arg_type(self, 0, m::ARRAY, "array")?;
                ret_type(self, m::OBJECT, "object")?;
            }
            "__invoke" => {
                non_static(self)?;
                public(self);
            }
            "__wakeup" => {
                args(self, 0)?;
                non_static(self)?;
                public(self);
                ret_type(self, m::VOID, "void")?;
                no_discard(self)?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// `zend_declare_typed_property` set-visibility normalization.
fn normalize_set_visibility(
    c: &Compiler,
    fl: u32,
    has_type: bool,
    cname: &str,
    pname: &str,
    line: u32,
) -> CResult<u32> {
    let mut fl = fl;
    if fl & flags::PUBLIC != 0 && fl & flags::READONLY != 0 && fl & flags::SET_MASK == 0 {
        fl |= flags::SET_PROTECTED;
    } else if fl & flags::SET_MASK != 0 {
        if !has_type {
            return c.err(
                format!("Property with asymmetric visibility {cname}::${pname} must have type"),
                line,
            );
        }
        let rank = |v: u32| {
            if v & (flags::PUBLIC | flags::SET_PUBLIC) != 0 {
                0
            } else if v & (flags::PROTECTED | flags::SET_PROTECTED) != 0 {
                1
            } else {
                2
            }
        };
        if rank(fl & flags::VISIBILITY) > rank(fl & flags::SET_MASK) {
            return c.err(
                format!("Visibility of property {cname}::${pname} must not be weaker than set visibility"),
                line,
            );
        }
        if (fl & flags::PUBLIC != 0 && fl & flags::SET_PUBLIC != 0)
            || (fl & flags::PROTECTED != 0 && fl & flags::SET_PROTECTED != 0)
            || (fl & flags::PRIVATE != 0 && fl & flags::SET_PRIVATE != 0)
        {
            fl &= !flags::SET_MASK;
        }
        if fl & flags::SET_PRIVATE != 0 {
            fl |= flags::FINAL;
        }
    }
    Ok(fl)
}

enum BodyKind<'a> {
    Stmts(&'a [Stmt]),
    Arrow(&'a Expr),
    None,
}

pub(super) fn is_superglobal_or_globals(n: &[u8]) -> bool {
    super::expr::is_superglobal(n) || n == b"GLOBALS"
}

fn is_builtin_type(lc: &[u8]) -> bool {
    matches!(
        lc,
        b"int"
            | b"float"
            | b"string"
            | b"bool"
            | b"false"
            | b"true"
            | b"null"
            | b"void"
            | b"never"
            | b"mixed"
            | b"object"
            | b"iterable"
            | b"array"
            | b"callable"
            | b"static"
    )
}

/// `zend_get_class_fetch_type_ast() != ZEND_FETCH_CLASS_DEFAULT`: an
/// unqualified (or `namespace\`-relative) `self`, `parent` or `static`.
fn is_class_fetch_keyword(name: &ast::Name) -> bool {
    matches!(
        name.kind,
        ast::NameKind::Unqualified | ast::NameKind::Relative
    ) && is_fetch_keyword(&name.name)
}

/// `self`, `parent` or `static` (case-insensitive).
fn is_fetch_keyword(n: &[u8]) -> bool {
    n.eq_ignore_ascii_case(b"self")
        || n.eq_ignore_ascii_case(b"parent")
        || n.eq_ignore_ascii_case(b"static")
}

pub(super) fn is_reserved_class_name(lc: &[u8]) -> bool {
    matches!(
        lc,
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
    )
}

#[allow(dead_code)]
fn unused(_: StmtKind) {}

/// Whether `t` admits only scalars and null (no array, object, resource,
/// callable, iterable or class name): a value of it has no destructor.
fn is_scalar_only(t: &crate::types::TypeDecl) -> bool {
    use crate::types::mask;
    let scalars = mask::NULL | mask::BOOL | mask::LONG | mask::DOUBLE | mask::STRING;
    t.mask != 0 && t.mask & !scalars == 0 && t.classes.is_empty() && t.intersections.is_empty()
}
