// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Partial function application (`f($a, ?, ...)`), after php-src's
//! `Zend/zend_partial.c`.
//!
//! Like php-src, the Closure is produced by generating the source of an
//! equivalent closure and compiling it:
//!
//! ```php
//! $g = f($a, ?);   // static function($b) use ($a) { return \f($a, $b); }
//! ```
//!
//! Bound (non-literal) arguments become lexical variables, placeholders
//! become parameters with the target's names, types and defaults, and a
//! `...` placeholder adds the remaining parameters (and forwards extra
//! arguments when the target is not variadic).

use std::rc::Rc;

use php_runtime::{Value, ZStr};

use crate::func::{flags, Func};
use crate::include::CompileOutcome;
use crate::object;
use crate::opcode::ParamInfo;
use crate::vm::{Frame, FrameKind, PendingCall, Vm, VmResult};

/// Placeholder bookkeeping of a pending partial call.
#[derive(Clone, Debug, Default)]
pub struct PfaArgs {
    /// Argument offsets holding a placeholder.
    pub placeholders: Vec<bool>,
    /// Named placeholders in syntax order (`named_positions`).
    pub named_order: Vec<ZStr>,
    /// Named placeholders that matched no parameter (variadic targets).
    pub unknown_named: Vec<ZStr>,
}

impl PfaArgs {
    fn is_placeholder(&self, i: usize) -> bool {
        self.placeholders.get(i).copied().unwrap_or(false)
    }
}

/// Generated variable names (`zp_names`).
struct Names {
    params: Vec<Option<String>>,
    variadic: Option<String>,
    extra_named: Option<String>,
    inner: Option<String>,
}

impl Names {
    fn exists(&self, n: &str) -> bool {
        self.params.iter().flatten().any(|p| p == n)
            || self.variadic.as_deref() == Some(n)
            || self.extra_named.as_deref() == Some(n)
            || self.inner.as_deref() == Some(n)
    }
}

/// What an argument slot holds.
#[derive(Clone)]
enum Slot {
    Placeholder,
    /// Not passed (skipped by named arguments).
    Undef,
    /// A literal burned into the generated code.
    Const(Value),
    /// A bound value passed as a lexical variable.
    Bound(Value),
}

fn param_name(params: &[ParamInfo], num_args: usize, i: usize) -> String {
    params
        .get(i.min(num_args))
        .map_or_else(String::new, |p| p.name.to_string_lossy())
}

impl Slot {
    fn is_ph(&self) -> bool {
        matches!(self, Slot::Placeholder)
    }
    fn is_undef(&self) -> bool {
        matches!(self, Slot::Undef)
    }
}

impl Vm {
    /// `ZEND_SEND_PLACEHOLDER`.
    pub(crate) fn op_send_placeholder(&mut self, name: Option<ZStr>) -> VmResult<()> {
        self.pooled_args();
        let pos = match &name {
            Some(n) => self.arg_position(n)?,
            None => Some(self.frame().calls.last().map_or(0, |c| c.args.len())),
        };
        let call = self.frame_mut().calls.last_mut().expect("pending call");
        let pfa = call.pfa.get_or_insert_with(Box::default);
        match pos {
            Some(p) => {
                if call.args.len() <= p {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: number of arguments of the call"
                    )]
                    call.args.resize(p + 1, None);
                }
                if pfa.placeholders.len() <= p {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: number of arguments of the call"
                    )]
                    pfa.placeholders.resize(p + 1, false);
                }
                if call.args[p].is_some() || pfa.placeholders[p] {
                    let n = name
                        .as_ref()
                        .map_or_else(String::new, ZStr::to_string_lossy);
                    let what = if pfa.placeholders[p] {
                        "placeholder"
                    } else {
                        "argument"
                    };
                    return Err(self.make_throw(
                        "Error",
                        &format!("Named parameter ${n} overwrites previous {what}"),
                    ));
                }
                call.args[p] = Some(Value::Null);
                pfa.placeholders[p] = true;
                if let Some(n) = name {
                    pfa.named_order.push(n);
                    call.has_named = true;
                }
            }
            None => {
                if let Some(n) = name {
                    pfa.unknown_named.push(n);
                }
            }
        }
        Ok(())
    }

    /// `ZEND_CALLABLE_CONVERT_PARTIAL` (`zend_partial_create`).
    #[allow(clippy::too_many_lines)]
    pub(crate) fn make_partial(
        &mut self,
        mut call: PendingCall,
        variadic_ph: bool,
        const_args: u32,
        pfa_name: ZStr,
    ) -> VmResult<Value> {
        call.materialize_args();
        let pfa = call.pfa.take().map(|b| *b).unwrap_or_default();
        let func = call.func.clone();
        if !func.is_user()
            && matches!(
                func.name().as_bytes(),
                b"func_get_arg"
                    | b"func_get_args"
                    | b"func_num_args"
                    | b"compact"
                    | b"extract"
                    | b"get_defined_vars"
            )
        {
            return Err(self.make_throw(
                "Error",
                &format!("Cannot call {}() dynamically", func.name()),
            ));
        }

        // A `__call`/`__callStatic` trampoline: one variadic `$arguments`.
        let is_magic = call.is_magic_trampoline();
        let magic = call.magic_name.clone();
        if is_magic {
            call.scope = self.find_class(b"Closure");
        }
        let is_closure_invoke = call
            .magic_name
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(b"__invoke"))
            || (matches!(&func, Func::Builtin(b) if b.name.as_bytes() == b"__invoke")
                && call
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.lc_name.as_bytes() == b"closure"));
        if is_closure_invoke {
            let target = call.closure.take().or_else(|| call.this.clone());
            call.this = target;
            call.scope = self.find_class(b"Closure");
            call.called = self.find_class(b"Closure");
            call.magic_name = Some(ZStr::from("__invoke"));
        }
        let params: Vec<ParamInfo> = if is_magic {
            vec![ParamInfo {
                name: ZStr::from("arguments"),
                by_ref: false,
                variadic: true,
                ty: Some(crate::types::TypeDecl::mixed()),
                has_default: false,
                default_text: None,
                promote: 0,
                sensitive: false,
                default_expr: None,
                attributes: Vec::new(),
            }]
        } else {
            func.params().to_vec()
        };
        let fn_variadic = is_magic || func.flags() & flags::VARIADIC != 0;
        let num_args = if fn_variadic {
            params.len().saturating_sub(1)
        } else {
            params.len()
        };
        let required = if is_magic {
            0
        } else {
            func.required() as usize
        };
        let fname = if is_closure_invoke {
            "Closure::__invoke".to_string()
        } else {
            match &magic {
                Some(m) => match &call.scope {
                    Some(sc) => format!(
                        "{}::{}",
                        object::display_name(&sc.name),
                        m.to_string_lossy()
                    ),
                    None => m.to_string_lossy(),
                },
                None => self.func_display(&func, call.scope.as_ref()),
            }
        };

        // `zp_args_check`.
        if let Some(n) = pfa.unknown_named.first() {
            return Err(self.make_throw(
                "Error",
                &format!(
                    "Cannot use named placeholder for unknown or variadic parameter ${}",
                    n.to_string_lossy()
                ),
            ));
        }
        let bind_constants = call.closure.is_some() || call.this.is_some();
        let argc = call.args.len();

        if argc < required && !variadic_ph {
            let limit = if num_args <= required {
                "exactly"
            } else {
                "at least"
            };
            return Err(self.make_throw(
                "ArgumentCountError",
                &format!("Partial application of {fname}() expects {limit} {required} arguments, {argc} given"),
            ));
        }
        if argc > num_args && !fn_variadic {
            return Err(self.make_throw(
                "ArgumentCountError",
                &format!("Partial application of {fname}() expects at most {num_args} arguments, {argc} given"),
            ));
        }

        // Slots.
        let new_argc = if variadic_ph {
            argc.max(num_args)
        } else {
            argc
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        let mut slots: Vec<Slot> = Vec::with_capacity(new_argc);
        for i in 0..new_argc {
            let s = match call.args.get(i) {
                _ if pfa.is_placeholder(i) => Slot::Placeholder,
                Some(Some(v)) => {
                    let lit = !bind_constants
                        && i < 32
                        && const_args & (1 << i) != 0
                        && !matches!(v, Value::Reference(_));
                    if lit {
                        Slot::Const(v.clone())
                    } else {
                        Slot::Bound(v.clone())
                    }
                }
                _ => Slot::Undef,
            };
            slots.push(s);
        }

        // Parameter order: positional placeholders, then named ones in
        // syntax order, then implicit ones added by `...`.
        let mut param_offset: Vec<Option<usize>> = vec![None; new_argc];
        let mut num_params = 0usize;
        let mut num_positional = 0usize;
        for i in 0..argc {
            if !slots[i].is_ph() {
                continue;
            }
            num_params += 1;
            let pname = params.get(i.min(num_args)).map(|p| p.name.clone());
            let named_pos = pname.and_then(|n| pfa.named_order.iter().position(|x| *x == n));
            param_offset[i] = Some(match named_pos {
                Some(np) => num_positional + np,
                None => {
                    num_positional += 1;
                    num_positional - 1
                }
            });
        }
        let mut num_required = num_params;
        if variadic_ph {
            for i in 0..new_argc {
                if i < argc && !slots[i].is_undef() {
                    continue;
                }
                slots[i] = Slot::Placeholder;
                num_params += 1;
                let off = num_params - 1;
                param_offset[i] = Some(off);
                // `zp_compute_num_required`.
                if i < num_args {
                    if i < required {
                        num_required = num_required.max(off + 1);
                    }
                } else {
                    num_required = off + 1;
                }
            }
        }

        // Names (`zp_assign_names`).
        let mut names = Names {
            params: vec![None; new_argc],
            variadic: None,
            extra_named: None,
            inner: None,
        };
        for i in 0..new_argc.min(num_args) {
            if slots[i].is_ph() {
                names.params[i] = Some(param_name(&params, num_args, i));
            }
        }
        if variadic_ph && fn_variadic {
            names.variadic = Some(param_name(&params, num_args, num_args));
        }
        for i in num_args..new_argc {
            if !slots[i].is_ph() {
                continue;
            }
            let orig = param_name(&params, num_args, num_args);
            let mut n = 0;
            let name = loop {
                let cand = format!("{orig}{n}");
                if !names.exists(&cand) {
                    break cand;
                }
                n += 1;
            };
            names.params[i] = Some(name);
        }
        for i in 0..new_argc {
            if !matches!(slots[i], Slot::Bound(_)) {
                continue;
            }
            let orig = param_name(&params, num_args, i);
            let mut cand = orig.clone();
            let mut n = 2;
            while names.exists(&cand) {
                cand = format!("{orig}{n}");
                n += 1;
            }
            names.params[i] = Some(cand);
        }
        let extra_named: Vec<(ZStr, Value)> = std::mem::take(&mut call.named);
        if !extra_named.is_empty() {
            let mut cand = "extra_named_params".to_string();
            let mut n = 2;
            while names.exists(&cand) {
                cand = format!("extra_named_params{n}");
                n += 1;
            }
            names.extra_named = Some(cand);
        }
        let inner_closure = call.closure.clone();
        if inner_closure.is_some() {
            let mut cand = "fn".to_string();
            let mut n = 2;
            while names.exists(&cand) {
                cand = format!("fn{n}");
                n += 1;
            }
            names.inner = Some(cand);
        }

        // Type-check bound and literal arguments (`zp_bind`).
        let strict = self.frame().code.strict_types;
        for i in 0..new_argc {
            let v = match &mut slots[i] {
                Slot::Bound(v) | Slot::Const(v) => v,
                _ => continue,
            };
            let p = if i < num_args {
                params.get(i)
            } else if fn_variadic {
                params.last()
            } else {
                None
            };
            let Some(p) = p else { continue };
            let Some(ty) = p.ty.clone() else { continue };
            if p.by_ref {
                continue;
            }
            let ctx = crate::typecheck::TypeCtx {
                scope: call.scope.as_ref(),
                called: call.called.as_ref(),
                strict,
                internal_arg: None,
            };
            let mut val = v.deref();
            if !self.verify_type(&ty, &mut val, &ctx)? {
                let scope = call.scope.clone();
                let need = ty.display(
                    scope.as_ref().map(|s| &s.name),
                    scope.as_ref().and_then(|s| s.parent_name()),
                    None,
                );
                return Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "{fname}(): Argument #{} (${}) must be of type {need}, {} given",
                        i + 1,
                        p.name.to_string_lossy(),
                        crate::typecheck::given_type(&val)
                    ),
                ));
            }
            if !matches!(v, Value::Reference(_)) {
                *v = val;
            }
        }

        // Generate the closure source.
        let type_src = |ty: &crate::types::TypeDecl| -> String {
            let mut ty = ty.clone();
            for name in ty
                .classes
                .iter_mut()
                .chain(ty.intersections.iter_mut().flatten())
            {
                if !name.eq_ignore_ascii_case(b"self") && !name.eq_ignore_ascii_case(b"parent") {
                    *name = ZStr::from(format!("\\{name}"));
                }
            }
            ty.to_php_string()
        };
        let mut src = String::new();
        let mut uses: Vec<String> = Vec::new();
        if let Some(n) = &names.inner {
            uses.push(format!("${n}"));
        }
        let mut param_src: Vec<String> = vec![String::new(); num_params];
        let mut defaults_needed: Vec<(usize, usize)> = Vec::new();
        for i in 0..new_argc {
            match &slots[i] {
                Slot::Placeholder => {
                    let off = param_offset[i].unwrap_or(0);
                    let p = params.get(i.min(num_args));
                    let mut s = String::new();
                    if p.is_some_and(|p| p.sensitive) {
                        s.push_str("#[\\SensitiveParameter] ");
                    }
                    if let Some(t) = p.and_then(|p| p.ty.as_ref()) {
                        s.push_str(&type_src(t));
                        s.push(' ');
                    }
                    if !is_magic && func.arg_by_ref(i) {
                        s.push('&');
                    }
                    s.push('$');
                    s.push_str(names.params[i].as_deref().unwrap_or("arg"));
                    if off >= num_required {
                        defaults_needed.push((i, off));
                    }
                    param_src[off] = s;
                }
                Slot::Bound(_) => {
                    let n = names.params[i].clone().unwrap_or_default();
                    uses.push(if !is_magic && func.arg_by_ref(i) {
                        format!("&${n}")
                    } else {
                        format!("${n}")
                    });
                }
                _ => {}
            }
        }
        for (i, off) in defaults_needed {
            let d = self.partial_default_src(
                &func,
                &params,
                i,
                call.scope.as_ref(),
                is_closure_invoke,
            )?;
            param_src[off].push_str(" = ");
            param_src[off].push_str(&d);
        }
        if let Some(n) = &names.extra_named {
            uses.push(format!("${n}"));
        }
        if variadic_ph && fn_variadic {
            let p = params.last();
            let mut s = String::new();
            if p.is_some_and(|p| p.sensitive) {
                s.push_str("#[\\SensitiveParameter] ");
            }
            if let Some(t) = p.and_then(|p| p.ty.as_ref()) {
                s.push_str(&type_src(t));
                s.push(' ');
            }
            if p.is_some_and(|p| p.by_ref) {
                s.push('&');
            }
            s.push_str("...$");
            s.push_str(names.variadic.as_deref().unwrap_or("args"));
            param_src.push(s);
        }
        let ret_ty = if is_magic {
            None
        } else {
            func.return_type().cloned()
        };
        let is_void = ret_ty
            .as_ref()
            .is_some_and(|t| t.mask & crate::types::mask::VOID != 0);
        let this_obj = call.this.clone();
        let static_closure = this_obj.is_none()
            && !(func.flags() & flags::CLOSURE != 0 && func.flags() & flags::STATIC == 0);
        if func.is_user() {
            if let Func::User(uf) = &func {
                if uf
                    .attributes
                    .iter()
                    .any(|a| a.name.eq_ignore_ascii_case(b"NoDiscard"))
                {
                    src.push_str("#[\\NoDiscard] ");
                }
            }
        }
        if static_closure {
            src.push_str("static ");
        }
        src.push_str("function ");
        if func.flags() & flags::RETURN_REF != 0 {
            src.push('&');
        }
        src.push('(');
        src.push_str(&param_src.join(", "));
        src.push(')');
        if !uses.is_empty() {
            src.push_str(" use (");
            src.push_str(&uses.join(", "));
            src.push(')');
        }
        if let Some(t) = &ret_ty {
            src.push_str(": ");
            src.push_str(&type_src(t));
        }
        src.push_str(" { ");
        let call_src = |vm: &mut Vm, forward: bool| -> VmResult<String> {
            let mut args: Vec<String> = Vec::new();
            let is_assert = func.name().eq_ignore_ascii_case(b"assert") && !func.is_user();
            if is_assert {
                args.push("'assert'".into());
            }
            for i in 0..new_argc {
                match &slots[i] {
                    Slot::Undef => {
                        if i < required {
                            let pn = param_name(&params, num_args, i);
                            return Err(vm.make_throw(
                                "ArgumentCountError",
                                &format!("{fname}(): Argument #{} (${pn}) not passed", i + 1),
                            ));
                        }
                        args.push(vm.partial_default_src(
                            &func,
                            &params,
                            i,
                            call.scope.as_ref(),
                            is_closure_invoke,
                        )?);
                    }
                    Slot::Const(v) => args.push(literal_src(vm, v)?),
                    _ => args.push(format!("${}", names.params[i].as_deref().unwrap_or("arg"))),
                }
            }
            if let Some(n) = &names.extra_named {
                args.push(format!("...${n}"));
            }
            if variadic_ph {
                if fn_variadic {
                    args.push(format!(
                        "...${}",
                        names.variadic.as_deref().unwrap_or("args")
                    ));
                } else if forward {
                    args.push(format!("...\\array_slice(\\func_get_args(), {num_params})"));
                }
            }
            let a = args.join(", ");
            let fname_src = match &magic {
                Some(m) => m.to_string_lossy(),
                None => func.name().to_string_lossy(),
            };
            Ok(if is_assert {
                format!("\\call_user_func({a})")
            } else if let Some(n) = &names.inner {
                format!("${n}({a})")
            } else if this_obj.is_some() {
                format!("$this->{fname_src}({a})")
            } else if call.called.is_some() && call.scope.is_some() {
                format!("static::{fname_src}({a})")
            } else {
                format!("\\{fname_src}({a})")
            })
        };
        let ret = |c: String| {
            if is_void {
                format!("{c};")
            } else {
                format!("return {c};")
            }
        };
        if variadic_ph && !fn_variadic {
            let plain = call_src(self, false)?;
            let fwd = call_src(self, true)?;
            src.push_str(&format!(
                "if (\\func_num_args() <= {num_params}) {{ {} }} else {{ {} }}",
                ret(plain),
                ret(fwd)
            ));
        } else {
            let c = call_src(self, false)?;
            src.push_str(&ret(c));
        }
        src.push_str(" }");

        // Compile `return <closure>;` at the declaring line.
        let (file, line) = self.current_location();
        let mut code = String::from("<?php ");
        let declaring_name = call.scope.as_ref().map_or_else(|| func.name(), |c| &c.name);
        if let Some((namespace, _)) = declaring_name.to_string_lossy().rsplit_once('\\') {
            code.push_str("namespace ");
            code.push_str(namespace);
            code.push_str("; ");
        }

        for _ in 1..line {
            code.push('\n');
        }
        code.push_str("return ");
        code.push_str(&src);
        code.push(';');
        let fscope = if is_magic && this_obj.is_none() {
            None
        } else {
            call.scope.clone()
        };
        let unit =
            match self.compile_pfa(code.as_bytes(), &file, fscope.as_ref(), &pfa_name, strict)? {
                CompileOutcome::Ok(u) => u,
                CompileOutcome::ParseError(e) => return Err(crate::vm::Throw::Exception(e)),
                CompileOutcome::Fatal => return Err(crate::vm::Throw::Fatal),
            };

        // Run it with the bound values as variables.
        let mut f = Frame::new(unit.main.code.clone(), FrameKind::Eval);
        f.observer_hidden = true;
        {
            let set = |f: &mut Frame, name: &str, v: Value| {
                if let Some(idx) = f
                    .code
                    .cv_names
                    .iter()
                    .position(|n| n.as_bytes() == name.as_bytes())
                {
                    f.cvs[idx].set(Some(v.clone()));
                }
                let mut vars = f.vars.borrow_mut();
                if let Some(idx) = vars
                    .names
                    .iter()
                    .position(|n| n.as_bytes() == name.as_bytes())
                {
                    vars.slots[idx] = Some(v);
                }
            };
            if let (Some(n), Some(c)) = (&names.inner, &inner_closure) {
                set(&mut f, n, Value::Object(c.clone()));
            }
            for i in 0..new_argc {
                if let Slot::Bound(v) = &slots[i] {
                    let v = if !is_magic && func.arg_by_ref(i) {
                        match v {
                            Value::Reference(r) => Value::Reference(r.clone()),
                            other => Value::Reference(php_runtime::PhpRef::new(other.clone())),
                        }
                    } else {
                        v.deref()
                    };
                    set(&mut f, names.params[i].as_deref().unwrap_or(""), v);
                }
            }
            if let Some(n) = &names.extra_named {
                let mut t = php_runtime::HashTable::new();
                for (k, v) in &extra_named {
                    t.insert(php_runtime::ArrayKey::Str(k.clone()), v.deref());
                }
                set(&mut f, n, Value::Array(php_runtime::ZArray::from_table(t)));
            }
        }
        f.this.clone_from(&this_obj);
        f.scope.clone_from(&fscope);
        f.called = match &this_obj {
            Some(o) => Some(object::class_of(o).clone()),
            None => call.called.clone(),
        };
        self.declare_hoisted(&unit)?;
        self.frames.push(f);
        let base = self.frames.len() - 1;
        let r = self.execute(base);
        self.frames.truncate(base);
        if let Ok(Value::Object(closure)) = &r {
            object::internal::<crate::closure::ClosureData, _>(closure, |data| {
                if let Func::User(function) = &data.func {
                    if let Some(inner_name) = &names.inner {
                        data.partial_inner = function
                            .uses
                            .iter()
                            .find(|capture| capture.name.as_bytes() == inner_name.as_bytes())
                            .map(|capture| capture.cv);
                    }
                    if let Func::User(original) = &func {
                        let attributes: Vec<_> = original
                            .attributes
                            .iter()
                            .filter(|attribute| attribute.name.eq_ignore_ascii_case(b"NoDiscard"))
                            .cloned()
                            .collect();
                        if !attributes.is_empty() {
                            let statics = function.statics.borrow().clone();
                            data.func = Func::User(Rc::new(crate::func::UserFunction {
                                name: function.name.clone(),
                                params: function.params.clone(),
                                required: function.required,
                                return_type: function.return_type.clone(),
                                flags: function.flags,
                                code: function.code.clone(),
                                line_start: function.line_start,
                                line_end: function.line_end,
                                doc_comment: function.doc_comment.clone(),
                                statics: std::cell::RefCell::new(statics),
                                uses: function.uses.clone(),
                                class_name: function.class_name.clone(),
                                attributes,
                                hook_prop: function.hook_prop.clone(),
                                call_shape: crate::func::CallShape::default(),
                            }));
                        }
                    }
                }
            });
        }
        r
    }

    /// Source text of the default value of parameter `i`
    /// (`zp_get_param_default_value`).
    fn partial_default_src(
        &mut self,
        func: &Func,
        params: &[ParamInfo],
        i: usize,
        scope: Option<&Rc<crate::class::Class>>,
        is_closure_invoke: bool,
    ) -> VmResult<String> {
        let Some(p) = params.get(i) else {
            return Ok("null".into());
        };
        if is_closure_invoke
            || (matches!(func, Func::Builtin(b) if b.name.as_bytes() == b"__invoke")
                && scope.is_some_and(|scope| scope.lc_name.as_bytes() == b"closure"))
        {
            return Err(self.make_throw("ArgumentCountError", &format!(
                "Closure::__invoke(): Argument #{} (${}) must be passed explicitly, because the default value is not known",
                i + 1, p.name
            )));
        }
        if let Some(e) = &p.default_expr {
            let e = e.clone();
            if let Ok(v) = self.eval_const_expr(&e, scope) {
                if !matches!(v, Value::Object(_)) {
                    return literal_src(self, &v);
                }
            }
        }
        if let Func::Builtin(b) = func {
            if let Some(v) = b.defaults.get(i).cloned().flatten() {
                return literal_src(self, &v);
            }
            // `zp_get_param_default_value` for internal functions: an
            // UNKNOWN (or non-literal) default cannot be reproduced.
            let known = p
                .default_text
                .as_deref()
                .map(str::trim)
                .is_some_and(|t| !t.is_empty() && t != "UNKNOWN");
            if !known {
                let fname = self.func_display(func, scope);
                return Err(self.make_throw(
                    "ArgumentCountError",
                    &format!(
                        "{fname}(): Argument #{} (${}) must be passed explicitly, because the default value is not known",
                        i + 1,
                        p.name.to_string_lossy()
                    ),
                ));
            }
        }
        Ok(p.default_text.clone().unwrap_or_else(|| "null".into()))
    }

    fn compile_pfa(
        &mut self,
        src: &[u8],
        file: &ZStr,
        scope: Option<&Rc<crate::class::Class>>,
        name: &ZStr,
        strict: bool,
    ) -> VmResult<CompileOutcome> {
        let program = match php_parser::parse(src, &php_parser::ParseOptions::default()) {
            Ok(p) => p,
            Err(e) => {
                return Err(
                    self.make_throw("Error", &format!("Partial application: {}", e.message))
                );
            }
        };
        let opts = crate::compiler::CompileOptions {
            filename: file.clone(),
            is_eval: false,
            eval_class: scope.map(|s| s.name.clone()),
            assertions: self.ini.get_long("zend.assertions"),
            eval_parent: scope.and_then(|s| s.parent_name().cloned()),
            closure_name: Some(name.clone()),
            strict_types: strict,
            sprintf_builtin: matches!(
                self.functions.get(&ZStr::from("sprintf")),
                Some(Func::Builtin(_))
            ),
            array_map_builtin: matches!(
                self.functions.get(&ZStr::from("array_map")),
                Some(Func::Builtin(_))
            ),
            strlen_builtin: matches!(
                self.functions.get(&ZStr::from("strlen")),
                Some(Func::Builtin(_))
            ),
            special_builtins: self.special_builtins(),
            anon_base: self.anon_counter,
            ct_constants: Some(self.ct_constants()),
            stack_limit: None,
            user_opcodes: Vec::new(),
            optimization_level: 0,
            strip_doc_comments: false,
        };
        let compiled = crate::compiler::compile(&program, Rc::from(src), opts);
        self.activate_compiled_auto_globals();
        match compiled {
            Ok(u) => {
                self.anon_counter = u.anon_end;
                for (level, message, line) in &u.warnings {
                    self.error_at(*level, message, file, *line)?;
                }
                Ok(CompileOutcome::Ok(Rc::new(u)))
            }

            Err(e) => {
                for (level, message, line) in &e.warnings {
                    self.error_at(*level, message, file, *line)?;
                }
                self.report_compile_error(&e.message, file, e.line, e.is_parse);
                Ok(CompileOutcome::Fatal)
            }
        }
    }
}

/// PHP source for a literal value.
fn literal_src(vm: &mut Vm, v: &Value) -> VmResult<String> {
    match v.deref() {
        Value::Null => Ok("null".into()),
        Value::Bool(b) => Ok(if b { "true".into() } else { "false".into() }),
        Value::Int(i) => Ok(if i == i64::MIN {
            "\\PHP_INT_MIN".into()
        } else {
            i.to_string()
        }),
        other => {
            let out = php_runtime::dump::var_export(&other, vm).map_err(|e| vm.rt_throw(e))?;
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
    }
}
