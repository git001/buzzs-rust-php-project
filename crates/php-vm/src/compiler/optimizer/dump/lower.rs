// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Lowering of an [`OpArray`] to the Zend oplines php-src would show for
//! the same code (`zend_compile.c` and the optimizer passes that rewrite
//! call and send opcodes, `Zend/Optimizer/optimize_func_calls.c`).
//!
//! The VM's instruction set is coarser than Zend's: a writable [`Place`]
//! stands for a chain of `FETCH_*_W` oplines, a call's arguments name
//! their source rather than their send mode, and the specialised handlers
//! fold several Zend opcodes into one. Each op is expanded to the oplines
//! `zend_compile_*` emits for it; jump targets are op indices, mapped to
//! oplines afterwards.
#![allow(clippy::too_many_lines)]

use super::model::{StackOf, ZLine, Z};
use crate::class::ClassDecl;
use crate::compiler::{FunctionInfo, FunctionTable};
use crate::func::UserFunction;
use crate::opcode::{
    ArgSrc, BinKind, CastKind, ClassSpec, FetchMode, ForeachKind, IncDecKind, IncludeKind,
    MatchKey, Op, OpArray, Operand, Place, PlaceBase, Seg, UnKind,
};
use crate::types::TypeDecl;
use php_runtime::{BinaryOp, Value, ZStr};
use std::collections::HashMap;
use std::rc::Rc;

/// A function a call resolves to at compile time
/// (`zend_optimizer_get_called_func`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Known {
    /// Send modes.
    pub info: FunctionInfo,
    /// The index of a user function among the dumped op arrays.
    pub user: Option<usize>,
    /// `ZEND_ACC_DEPRECATED` / `ZEND_ACC_NODISCARD` ([`crate::func::flags`]).
    pub flags: u32,
    /// The type bits of the result.
    pub ret: u32,
}

/// What the lowering knows about the script being dumped.
pub(crate) struct Resolver<'a> {
    /// `CG(function_table)`: only internal functions count
    /// (`ZEND_COMPILE_IGNORE_OTHER_FILES`).
    pub table: &'a dyn FunctionTable,
    /// The script's function table (unconditional top-level functions), by
    /// lower-case name, with their index among the dumped op arrays.
    pub functions: HashMap<Vec<u8>, (usize, Rc<UserFunction>)>,
    /// The script's top-level classes by lower-case name, with the dumped
    /// index of each method by lower-case method name.
    pub classes: HashMap<Vec<u8>, (Rc<ClassDecl>, HashMap<Vec<u8>, usize>)>,
    /// The flags and return type of an internal function by lower-case
    /// name.
    pub internal: &'a dyn Fn(&[u8]) -> (u32, Option<TypeDecl>),
    /// `opcache.optimization_level`.
    pub level: u32,
    /// Lower-case names of the classes early-bound at compile time
    /// ([`early_bound_classes`]): the linked classes of
    /// `script->class_table`.
    pub bound: Vec<Vec<u8>>,
    /// Lower-case names declared by more than one class of the script
    /// (conditional declarations): which one a name means is unknown.
    pub ambiguous: Vec<Vec<u8>>,
    /// The dumped op arrays' functions, by dumped index.
    pub funcs: Vec<Rc<UserFunction>>,
}

/// A class constant php-src's optimizer resolves at compile time
/// (`zend_fetch_class_const_info`).
#[derive(Clone, Debug)]
pub(crate) struct ClassConst {
    /// The value, unless it is a constant expression evaluated at run
    /// time (`IS_CONSTANT_AST`).
    pub value: Option<Value>,
    /// The type bits of a typed constant (`zend_convert_type` of
    /// `cc->type`).
    pub ty: Option<u32>,
    /// A `static::` reference that a child class may override
    /// (`is_prototype`).
    pub is_prototype: bool,
}

/// Whether op `i` of `func`'s code is a statement of the file's top-level
/// statement list (`toplevel` in `zend_compile_top_stmt`): in the main
/// code and not crossed by any jump (the jump around or back over a
/// block).
fn is_top_level(func: &UserFunction, i: usize) -> bool {
    if !func.name.is_empty() {
        return false;
    }
    !func.code.ops.iter().enumerate().any(|(at, op)| {
        let mut op = op.clone();
        let mut crossed = false;
        crate::compiler::optimizer::for_each_label_mut(&mut op, |t| {
            let t = *t as usize;
            if at.min(t) < i && i < at.max(t) {
                crossed = true;
            }
        });
        crossed
    })
}

/// Whether class `d` declared by op `i` of the main code is bound at
/// compile time (`zend_compile_class_decl`): a top-level class without
/// interfaces or traits whose parent (if any) is already linked.
fn binds_early(main: &UserFunction, i: usize, d: &ClassDecl, bound: &[Vec<u8>]) -> bool {
    let parent = d.parent.as_ref().map(|p| p.as_bytes().to_ascii_lowercase());
    is_top_level(main, i)
        && d.kind != crate::class::ClassKind::Enum
        && d.interfaces.is_empty()
        && d.traits.is_empty()
        && parent.as_ref().is_none_or(|p| bound.contains(p))
}

/// The lower-case names of the classes the main code `main` binds at
/// compile time, in declaration order.
pub(crate) fn early_bound_classes(main: &UserFunction) -> Vec<Vec<u8>> {
    let mut bound: Vec<Vec<u8>> = Vec::new();
    for (i, op) in main.code.ops.iter().enumerate() {
        let Op::DeclareClass(idx) = generic(op) else {
            continue;
        };
        let Some(d) = main.code.decls.classes.borrow().get(idx as usize).cloned() else {
            continue;
        };
        if binds_early(main, i, &d, &bound) {
            bound.push(d.name.as_bytes().to_ascii_lowercase());
        }
    }
    bound
}

impl Resolver<'_> {
    fn user_info(f: &UserFunction) -> FunctionInfo {
        FunctionInfo::from_flags(
            false,
            f.required,
            f.params.iter().map(|p| (p.by_ref, p.variadic)),
        )
    }

    /// The result type of a call of user function `f`.
    fn user_ret(f: &UserFunction) -> u32 {
        let t = super::passes::decl_type(f.return_type.as_ref());
        if f.flags & crate::func::flags::RETURN_REF != 0 || f.is_generator() {
            super::passes::ty::ANY | super::passes::ty::REF
        } else {
            t
        }
    }

    /// A function called by lower-case name on line `line`: the script's
    /// functions are found by `zend_compile_call` once declared, and by
    /// pass 4 (`optimize_func_calls`) anywhere.
    fn function(&self, lc: &[u8], line: u32) -> Option<Known> {
        if let Some((idx, f)) = self.functions.get(lc) {
            if self.level & crate::compiler::optimizer::PASS_4 != 0 || f.line_start < line {
                return Some(Known {
                    info: Self::user_info(f),
                    user: Some(*idx),
                    flags: f.flags,
                    ret: Self::user_ret(f),
                });
            }
            return None;
        }
        self.table.find(lc).filter(|i| i.internal).map(|info| {
            let (flags, ret) = (self.internal)(lc);
            Known {
                info,
                user: None,
                flags,
                ret: super::passes::decl_type(ret.as_ref()),
            }
        })
    }

    /// The script class declared under lower-case name `lc`, when only one
    /// class of the script has that name.
    fn class_decl(&self, lc: &[u8]) -> Option<&Rc<ClassDecl>> {
        if self.ambiguous.iter().any(|a| a == lc) {
            return None;
        }
        self.classes.get(lc).map(|(d, _)| d)
    }

    /// Whether `lc` names a class early-bound at compile time.
    fn is_bound(&self, lc: &[u8]) -> bool {
        self.bound.iter().any(|b| b == lc)
    }

    /// `zend_hash_find_ptr(&ce->constants_table, name)` of class `d`: its
    /// own constants and, when it is linked, the non-private ones it
    /// inherits from its (linked) parents. Returns the declaring class and
    /// the index of the constant in its `consts`.
    fn find_const(&self, d: &Rc<ClassDecl>, name: &[u8]) -> Option<(Rc<ClassDecl>, usize)> {
        let mut cur = d.clone();
        let mut inherited = false;
        for _ in 0..64 {
            // An enum case is a constant whose value is evaluated at run
            // time.
            if cur.cases.iter().any(|(n, _, _)| n.as_bytes() == name) {
                return None;
            }
            if let Some(k) = cur.consts.iter().position(|c| c.name.as_bytes() == name) {
                if inherited && cur.consts[k].flags & crate::func::flags::PRIVATE != 0 {
                    return None;
                }
                return Some((cur, k));
            }
            let lc = cur.name.as_bytes().to_ascii_lowercase();
            let parent = cur.parent.as_ref()?.as_bytes().to_ascii_lowercase();
            if !self.is_bound(&lc) || !self.is_bound(&parent) {
                return None;
            }
            cur = self.class_decl(&parent)?.clone();
            inherited = true;
        }
        None
    }

    /// `zend_fetch_class_const_info`: constant `name` of the class `class`
    /// names, fetched in a method of class `scope` (lower-case). Only the
    /// script's own classes are known: the early-bound ones by name, the
    /// scope also when it is bound at run time.
    pub(crate) fn class_const(
        &self,
        scope: Option<&[u8]>,
        class: &ClassSpec,
        class_name: Option<&[u8]>,
        name: &[u8],
    ) -> Option<ClassConst> {
        use crate::class::cflags;
        use crate::func::flags;
        let scope_decl = scope
            .and_then(|s| self.class_decl(s))
            .filter(|d| d.flags & cflags::TRAIT == 0);
        let mut is_static = false;
        let ce: Rc<ClassDecl> = match class {
            ClassSpec::Named(_) => {
                let lc = class_name?.to_ascii_lowercase();
                if self.is_bound(&lc) || scope == Some(lc.as_slice()) {
                    self.class_decl(&lc)?.clone()
                } else {
                    return None;
                }
            }
            ClassSpec::SelfClass => scope_decl?.clone(),
            ClassSpec::Static => {
                is_static = true;
                scope_decl?.clone()
            }
            ClassSpec::Parent => {
                let d = scope_decl?;
                if !self.is_bound(scope?) {
                    return None;
                }
                let p = d.parent.as_ref()?.as_bytes().to_ascii_lowercase();
                self.class_decl(&p)?.clone()
            }
            _ => return None,
        };
        if ce.flags & cflags::TRAIT != 0 {
            return None;
        }
        let (decl, k) = self.find_const(&ce, name)?;
        let c = &decl.consts[k];
        let deprecated = c.flags & flags::DEPRECATED != 0
            || c.attributes
                .iter()
                .any(|a| a.name.as_bytes().eq_ignore_ascii_case(b"Deprecated"));
        let public = c.flags & (flags::PRIVATE | flags::PROTECTED) == 0;
        let declared_in_scope = scope.is_some_and(|s| decl.name.as_bytes().eq_ignore_ascii_case(s));
        if deprecated || (!public && !declared_in_scope) {
            return None;
        }
        Some(ClassConst {
            value: match &c.value {
                crate::class::ConstExpr::Value(v) => Some(v.clone()),
                crate::class::ConstExpr::Code(_) => None,
            },
            ty: c.ty.as_ref().map(|t| super::passes::decl_type(Some(t))),
            is_prototype: is_static
                && decl.flags & cflags::FINAL == 0
                && c.flags & flags::FINAL == 0,
        })
    }

    /// `zend_try_inline_call`: the constant a call of the dumped user
    /// function `user` with `sent` arguments is replaced by, when its
    /// code is nothing but `return <literal>;` after receiving its
    /// arguments.
    pub(crate) fn inline_value(
        &self,
        user: usize,
        sent: u32,
        static_call: bool,
        result_used: bool,
    ) -> Option<Value> {
        use crate::func::flags;
        use crate::types::mask as m;
        let f = self.funcs.get(user)?;
        let no_discard = if result_used { 0 } else { flags::NODISCARD };
        if self.level & crate::compiler::optimizer::PASS_16 == 0
            || f.flags
                & (flags::ABSTRACT
                    | flags::DEPRECATED
                    | flags::VARIADIC
                    | flags::RETURN_REF
                    | flags::NO_BODY
                    | no_discard)
                != 0
            || f.is_generator()
            || f.is_closure()
            || sent < f.required
            || (static_call && f.flags & flags::STATIC == 0)
            || f.params
                .iter()
                .any(|p| p.by_ref || p.variadic || p.ty.is_some())
            || !f.code.static_vars.is_empty()
            || f.code.num_statics != 0
            || super::reduced_to_literal(&f.code)
        {
            return None;
        }
        // Missing optional arguments take their defaults, which must not
        // be constant expressions.
        if f.params
            .iter()
            .skip(sent as usize)
            .any(|p| !matches!(p.default_expr, Some(crate::class::ConstExpr::Value(_))))
        {
            return None;
        }
        let mut body = f.code.ops.iter().map(generic).filter(|op| {
            !matches!(
                op,
                Op::Nop | Op::Recv { .. } | Op::RecvInit { .. } | Op::RecvDefault { .. }
            )
        });
        let v = match body.next()? {
            Op::Return {
                val: Operand::Lit(l),
            } => f.code.literals.get(l as usize)?.clone(),
            Op::Return {
                val: Operand::Unused,
            } => Value::Null,
            _ => return None,
        };
        // A literal of another type than the declared return type is
        // checked first (`zend_emit_return_type_check`).
        if let Some(rt) = &f.return_type {
            let mixed =
                rt.classes.is_empty() && rt.intersections.is_empty() && rt.mask & m::ANY == m::ANY;
            let bit = match &v {
                Value::Null => m::NULL | m::VOID,
                Value::Bool(false) => m::FALSE,
                Value::Bool(true) => m::TRUE,
                Value::Int(_) => m::LONG,
                Value::Float(_) => m::DOUBLE,
                Value::Str(_) => m::STRING,
                Value::Array(_) => m::ARRAY,
                _ => 0,
            };
            if !mixed && rt.mask & bit == 0 {
                return None;
            }
        }
        Some(v)
    }

    /// A method of a script class.
    fn method(&self, class_lc: &[u8], method_lc: &[u8]) -> Option<(Known, u32, u32)> {
        let (decl, idx) = self.classes.get(class_lc)?;
        let m = decl
            .methods
            .iter()
            .find(|m| m.name.as_bytes().eq_ignore_ascii_case(method_lc))?;
        Some((
            Known {
                info: Self::user_info(&m.func),
                user: idx.get(method_lc).copied(),
                flags: m.func.flags,
                ret: Self::user_ret(&m.func),
            },
            m.flags,
            decl.flags,
        ))
    }
}

/// The lowered form of one op array.
pub(crate) struct Lowered {
    /// The oplines.
    pub lines: Vec<ZLine>,
    /// For each op (and one past the end), the index of its first opline.
    pub first_z: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CallKind {
    /// `INIT_FCALL` (resolved).
    FCall,
    /// `INIT_FCALL_BY_NAME`.
    ByName,
    /// `INIT_NS_FCALL_BY_NAME`.
    NsByName,
    /// `INIT_DYNAMIC_CALL`.
    Dynamic,
    /// `INIT_METHOD_CALL`.
    Method,
    /// `INIT_STATIC_METHOD_CALL`.
    Static,
    /// `NEW`.
    New,
    /// `INIT_PARENT_PROPERTY_HOOK_CALL`.
    Hook,
}

struct Call {
    kind: CallKind,
    known: Option<Known>,
    /// Frameless call: the function name and the variant's arity.
    frameless: Option<(Vec<u8>, u32)>,
    args: Vec<Z>,
    sent: u32,
    /// The `INIT_*` opline of a call to a known function that is not a
    /// prototype, which `zend_try_inline_call` may replace.
    inline: Option<usize>,
}

/// A fetch mode of a container chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    R,
    Is,
    W,
    Rw,
    FuncArg,
    Unset,
    List,
}

impl Mode {
    fn is_var(self) -> bool {
        !matches!(self, Mode::R | Mode::Is | Mode::List)
    }
}

/// The opcode name of a binary operator.
pub(crate) fn bin_name(op: BinKind) -> &'static str {
    match op {
        BinKind::Arith(a) => arith_name(a),
        BinKind::Equal => "IS_EQUAL",
        BinKind::NotEqual => "IS_NOT_EQUAL",
        BinKind::Identical => "IS_IDENTICAL",
        BinKind::NotIdentical => "IS_NOT_IDENTICAL",
        BinKind::Less => "IS_SMALLER",
        BinKind::LessEqual => "IS_SMALLER_OR_EQUAL",
        BinKind::Spaceship => "SPACESHIP",
    }
}

/// The opcode name of an arithmetic operator.
pub(crate) fn arith_name(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "ADD",
        BinaryOp::Sub => "SUB",
        BinaryOp::Mul => "MUL",
        BinaryOp::Div => "DIV",
        BinaryOp::Mod => "MOD",
        BinaryOp::Pow => "POW",
        BinaryOp::ShiftLeft => "SL",
        BinaryOp::ShiftRight => "SR",
        BinaryOp::BitOr => "BW_OR",
        BinaryOp::BitAnd => "BW_AND",
        BinaryOp::BitXor => "BW_XOR",
        BinaryOp::Concat => "CONCAT",
        BinaryOp::BoolXor => "BOOL_XOR",
        BinaryOp::BitNot => "BW_NOT",
        BinaryOp::BoolNot => "BOOL_NOT",
    }
}

/// `zend_dump_class_fetch_type` of `self`/`parent`/`static` with `flags`
/// already formatted.
fn fetch_type(c: &ClassSpec, flags: &str) -> Option<String> {
    let t = match c {
        ClassSpec::SelfClass => "(self)",
        ClassSpec::Parent => "(parent)",
        ClassSpec::Static => "(static)",
        _ => return None,
    };
    Some(format!("{t}{flags}"))
}

/// The type names `zend_dump_type_info` prints for the `1 << IS_*` bits
/// of a `TYPE_CHECK`.
fn type_mask_text(mask: u8) -> String {
    use crate::opcode::type_mask as m;
    let single = match mask {
        x if x == m::NULL => Some("(null)"),
        x if x == m::BOOL => Some("(bool)"),
        x if x == m::INT => Some("(long)"),
        x if x == m::FLOAT => Some("(double)"),
        x if x == m::STRING => Some("(string)"),
        x if x == m::ARRAY => Some("(array)"),
        x if x == m::OBJECT => Some("(object)"),
        x if x == m::RESOURCE => Some("(resource)"),
        _ => None,
    };
    if let Some(s) = single {
        return format!(" {s}");
    }
    let mut names = Vec::new();
    for (bit, name) in [
        (m::NULL, "null"),
        (m::BOOL, "bool"),
        (m::INT, "long"),
        (m::FLOAT, "double"),
        (m::STRING, "string"),
        (m::ARRAY, "array"),
        (m::OBJECT, "object"),
        (m::RESOURCE, "resource"),
    ] {
        if mask & bit != 0 {
            names.push(name);
        }
    }
    format!(" TYPE [{}]", names.join(", "))
}

/// The lowering of one op array.
pub(crate) struct Lower<'a> {
    oa: &'a OpArray,
    /// `oa`'s ops as compiled: each `FramelessICall` expanded back into
    /// its call ops (`specialise::unfuse_frameless_calls`).
    ops: std::borrow::Cow<'a, [Op]>,
    func: &'a UserFunction,
    res: &'a Resolver<'a>,
    /// Lower-case name of the class whose method this is.
    scope: Option<Vec<u8>>,
    /// This op array's dynamic function definitions (decl indices).
    dyn_defs: &'a [u32],
    lines: Vec<ZLine>,
    next_tmp: u32,
    /// Temporaries replaced by another operand.
    alias: HashMap<u32, Z>,
    calls: Vec<Call>,
    /// The temporary of each foreach iterator slot.
    iters: HashMap<u32, u32>,
    /// Ops already lowered together with an earlier one.
    skip: Vec<bool>,
    /// `INIT_ARRAY`s waiting for their first element: op index of the
    /// element, then the array, the printed size and the element's key and
    /// value.
    pending_init: HashMap<usize, (u32, String, Option<Operand>, Operand)>,
    /// Temporaries that are another one in php-src's code.
    rename: HashMap<u32, u32>,
    /// Lower-case names of the classes bound at compile time so far
    /// (early binding in `zend_compile_class_decl`).
    early_bound: Vec<Vec<u8>>,
    /// The results of `DECLARE_ANON_CLASS` (a class operand as it is).
    anon: Vec<u32>,
    /// A `VERIFY_RETURN_TYPE` was lowered for the next return (the VM's
    /// explicit check before finally blocks).
    verified: bool,
    /// The results of inlined calls (`T = QM_ASSIGN <literal>`, which the
    /// block pass folds into a `VERIFY_RETURN_TYPE` reading them).
    inlined: Vec<u32>,
}

impl<'a> Lower<'a> {
    /// Prepares the lowering of `func`'s code.
    pub(crate) fn new(
        func: &'a UserFunction,
        scope: Option<Vec<u8>>,
        dyn_defs: &'a [u32],
        res: &'a Resolver<'a>,
    ) -> Self {
        let oa: &'a OpArray = &func.code;
        Lower {
            oa,
            ops: crate::compiler::specialise::unfuse_frameless_calls(&oa.ops),
            func,
            res,
            scope,
            dyn_defs,
            lines: Vec::new(),
            next_tmp: oa.num_tmps,
            alias: HashMap::new(),
            calls: Vec::new(),
            iters: HashMap::new(),
            skip: vec![false; oa.ops.len()],
            pending_init: HashMap::new(),
            rename: HashMap::new(),
            early_bound: Vec::new(),
            anon: Vec::new(),
            verified: false,
            inlined: Vec::new(),
        }
    }

    fn tmp(&mut self) -> u32 {
        let t = self.next_tmp;
        self.next_tmp += 1;
        t
    }

    fn push(&mut self, l: ZLine) {
        self.lines.push(l);
    }

    fn lit(&self, l: u32) -> Value {
        self.oa
            .literals
            .get(l as usize)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn lit_bytes(&self, l: u32) -> Vec<u8> {
        match self.oa.literals.get(l as usize) {
            Some(Value::Str(s)) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }

    fn z(&self, o: &Operand) -> Z {
        match *o {
            Operand::Cv(c) => Z::Cv(c),
            Operand::Tmp(t) | Operand::TmpKeep(t) => {
                self.alias.get(&t).cloned().unwrap_or(Z::Tmp(t))
            }
            Operand::Lit(l) => Z::Const(self.lit(l)),
            Operand::Unused => Z::Unused,
        }
    }

    /// An operand a call consumes: a temporary that stays alive for later
    /// reads ([`Operand::TmpKeep`], the callback of an `array_map()`
    /// foreach) is copied first (`ZEND_COPY_TMP` in
    /// `zend_compile_func_array_map`).
    fn copy_kept(&mut self, o: &Operand) -> Z {
        let z = self.z(o);
        if matches!(o, Operand::TmpKeep(_)) && matches!(z, Z::Tmp(_)) {
            let t = self.tmp();
            self.push(ZLine::new("COPY_TMP").t(t).a(z));
            return Z::Tmp(t);
        }
        z
    }

    fn zs(&self, s: &[u8]) -> Z {
        Z::Const(Value::Str(ZStr::from(s)))
    }

    /// A class operand of an opline: a literal name or a fetch type, with
    /// `flags` for the fetch type (`zend_compile_class_ref`).
    fn class_z(&mut self, c: &ClassSpec, flags: &str) -> Z {
        match c {
            ClassSpec::Named(l) => Z::Const(self.lit(*l)),
            ClassSpec::Dyn(op) => {
                let z = self.copy_kept(op);
                if z.is_const() || matches!(z, Z::Tmp(t) if self.anon.contains(&t)) {
                    return z;
                }
                let t = self.tmp();
                self.push(
                    ZLine::new("FETCH_CLASS")
                        .v(t)
                        .a(Z::Text(flags.trim().to_string()))
                        .b(z),
                );
                Z::Tmp(t)
            }
            other => Z::Text(fetch_type(other, flags).unwrap_or_default()),
        }
    }

    /// Lowers the whole op array.
    pub(crate) fn run(mut self) -> Lowered {
        let n = self.ops.len();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: one slot per compiled op"
        )]
        let mut first_z = Vec::with_capacity(n + 1);
        let mut bound = self.func.uses.is_empty();
        for i in 0..n {
            first_z.push(u32::try_from(self.lines.len()).unwrap_or(u32::MAX));
            if self.skip[i] {
                continue;
            }
            if let Some((arr, size, key, val)) = self.pending_init.remove(&i) {
                let v = self.z(&val);
                let k = key.as_ref().map_or(Z::Text("NEXT".into()), |k| self.z(k));
                self.push(ZLine::new("INIT_ARRAY").t(arr).x(size).a(v).b(k));
                continue;
            }
            let op = generic(&self.ops[i]);
            if !bound
                && !matches!(
                    op,
                    Op::Recv { .. }
                        | Op::RecvInit { .. }
                        | Op::RecvDefault { .. }
                        | Op::RecvVariadic { .. }
                        | Op::GeneratorCreate
                        | Op::Nop
                )
            {
                bound = true;
                self.bind_uses();
            }
            self.lower_op(i, &op);
        }
        first_z.push(u32::try_from(self.lines.len()).unwrap_or(u32::MAX));
        if !self.rename.is_empty() {
            let rename = &self.rename;
            let fix = |z: &mut Z| {
                if let Z::Tmp(t) = z {
                    let mut x = *t;
                    for _ in 0..8 {
                        match rename.get(&x) {
                            Some(&n) => x = n,
                            None => break,
                        }
                    }
                    *t = x;
                }
            };
            for l in &mut self.lines {
                fix(&mut l.res);
                fix(&mut l.op1);
                fix(&mut l.op2);
            }
        }
        Lowered {
            lines: self.lines,
            first_z,
        }
    }

    /// The op at `i` if it exists.
    fn op_at(&self, i: usize) -> Option<Op> {
        self.ops.get(i).map(generic)
    }

    /// Whether op `i` reads temporary `t` (through any operand).
    fn reads_tmp(op: &Op, t: u32) -> bool {
        let mut found = false;
        let mut op = op.clone();
        crate::compiler::optimizer::for_each_operand_mut(&mut op, |o| {
            if matches!(o, Operand::Tmp(x) | Operand::TmpKeep(x) if *x == t) {
                found = true;
            }
        });
        found
    }

    /// Scans the arguments of the call started at op `i`: the number of
    /// arguments, and whether every one is a plain positional value or
    /// variable (what a frameless call can take).
    fn scan_call(&self, i: usize) -> (u32, bool) {
        let mut depth = 0u32;
        let mut n = 0u32;
        let mut simple = true;
        for op in self.ops.iter().skip(i + 1) {
            match generic(op) {
                Op::InitFCall { .. }
                | Op::InitDynCall { .. }
                | Op::InitMethodCall { .. }
                | Op::InitStaticCall { .. }
                | Op::InitParentHook { .. }
                | Op::New { .. } => depth += 1,
                Op::DoCall { .. } | Op::CallableConvert { .. } | Op::CallablePartial { .. } => {
                    if depth == 0 {
                        return (n, simple);
                    }
                    depth -= 1;
                }
                Op::Abs { .. } if depth == 0 => simple = false,
                Op::Send { src } if depth == 0 => {
                    n += 1;
                    match src {
                        ArgSrc::Val(_) | ArgSrc::CallResult(_) => {}
                        ArgSrc::Place(p) | ArgSrc::TempPlace(p) => {
                            let pl = &self.oa.places[p as usize];
                            if !(pl.segs.is_empty() && matches!(pl.base, PlaceBase::Cv(_))) {
                                simple = false;
                            }
                        }
                    }
                }
                Op::SendNamed { .. } | Op::SendPlaceholder { .. } if depth == 0 => {
                    n += 1;
                    simple = false;
                }
                Op::SendUnpack { .. } if depth == 0 => simple = false,
                _ => {}
            }
        }
        (n, false)
    }

    fn lower_op(&mut self, i: usize, op: &Op) {
        match op {
            Op::Nop
            | Op::Rethrow
            | Op::UserOpcode { .. }
            | Op::ConstDimCheck { .. }
            | Op::InitStatic { .. }
            | Op::Diag { .. } => {}
            Op::Move { src, dst } => {
                let a = self.z(src);
                self.push(ZLine::new("QM_ASSIGN").t(*dst).a(a));
            }
            Op::Bin { op, a, b, dst } => {
                let (a, b) = (self.z(a), self.z(b));
                self.push(ZLine::new(bin_name(*op)).t(*dst).a(a).b(b));
            }
            Op::Unary { op, a, dst } => {
                let a = self.z(a);
                let l = match op {
                    UnKind::Neg => ZLine::new("MUL").a(a).b(Z::Const(Value::Int(-1))),
                    UnKind::Plus => ZLine::new("MUL").a(a).b(Z::Const(Value::Int(1))),
                    UnKind::Not => ZLine::new("BOOL_NOT").a(a),
                    UnKind::BitNot => ZLine::new("BW_NOT").a(a),
                    UnKind::Bool => ZLine::new("BOOL").a(a),
                };
                self.push(l.t(*dst));
            }
            Op::Cast { kind, a, dst } => {
                let a = self.z(a);
                let l = match kind {
                    CastKind::Bool => ZLine::new("BOOL").a(a),
                    CastKind::Int => ZLine::new("CAST").x(" (long)").a(a),
                    CastKind::Float => ZLine::new("CAST").x(" (double)").a(a),
                    CastKind::String => ZLine::new("CAST").x(" (string)").a(a),
                    CastKind::Array => ZLine::new("CAST").x(" (array)").a(a),
                    CastKind::Object => ZLine::new("CAST").x(" (object)").a(a),
                    CastKind::Null => ZLine::new("CAST").x(" (null)").a(a),
                };
                self.push(l.t(*dst));
            }
            Op::Rope { parts, dst } => self.lower_rope(parts, *dst),
            Op::InitArray { dst } => self.lower_init_array(i, *dst),
            Op::ArrayAdd { arr, key, val } => {
                let v = self.z(val);
                let k = key.as_ref().map_or(Z::Text("NEXT".into()), |k| self.z(k));
                self.push(ZLine::new("ADD_ARRAY_ELEMENT").t(*arr).a(v).b(k));
            }
            Op::ArrayAddRef { arr, key, place } => {
                let v = self.place_ref(*place);
                let k = key.as_ref().map_or(Z::Text("NEXT".into()), |k| self.z(k));
                self.push(
                    ZLine::new("ADD_ARRAY_ELEMENT")
                        .t(*arr)
                        .x(" (ref)")
                        .a(v)
                        .b(k),
                );
            }
            Op::ArrayUnpack { arr, src } | Op::ConstArrayUnpack { arr, src } => {
                let s = self.z(src);
                self.push(ZLine::new("ADD_ARRAY_UNPACK").t(*arr).a(s));
            }
            Op::Jmp(l) => self.push(ZLine::new("JMP").a(Z::Label(*l))),
            Op::JmpZ(c, l) => {
                let c = self.z(c);
                self.push(ZLine::new("JMPZ").a(c).b(Z::Label(*l)));
            }
            Op::JmpNz(c, l) => {
                let c = self.z(c);
                self.push(ZLine::new("JMPNZ").a(c).b(Z::Label(*l)));
            }
            Op::JmpZEx { a, dst, target } => {
                let a = self.z(a);
                self.short_circuit_result(&a, *dst);
                self.push(ZLine::new("JMPZ_EX").t(*dst).a(a).b(Z::Label(*target)));
            }
            Op::JmpNzEx { a, dst, target } => {
                let a = self.z(a);
                self.short_circuit_result(&a, *dst);
                self.push(ZLine::new("JMPNZ_EX").t(*dst).a(a).b(Z::Label(*target)));
            }
            Op::JmpSet { a, dst, target } => {
                let a = self.z(a);
                self.push(ZLine::new("JMP_SET").t(*dst).a(a).b(Z::Label(*target)));
            }
            Op::Coalesce { a, dst, target } => {
                let a = self.z(a);
                self.push(ZLine::new("COALESCE").t(*dst).a(a).b(Z::Label(*target)));
            }
            Op::JmpNull { a, dst, target } => {
                let a = self.z(a);
                self.push(ZLine::new("JMP_NULL").t(*dst).a(a).b(Z::Label(*target)));
            }
            Op::AssignCv { cv, val, dst } => {
                let v = self.z(val);
                self.push(ZLine::new("ASSIGN").ot(*dst).a(Z::Cv(*cv)).b(v));
            }
            Op::Assign { place, val, dst } => self.lower_assign(*place, val, *dst),
            Op::AssignOp {
                place,
                op,
                val,
                dst,
            } => self.lower_assign_op(*place, *op, val, *dst),
            Op::IncDec { place, kind, dst } => self.lower_incdec(*place, *kind, *dst),
            Op::AssignRef { place, src, dst } => {
                let s = self.z(src);
                let pl = self.oa.places[*place as usize].clone();
                if pl.segs.is_empty() {
                    let target = match &pl.base {
                        PlaceBase::Cv(c) => Z::Cv(*c),
                        _ => self.container(&pl, 0, Mode::W),
                    };
                    self.push(ZLine::new("ASSIGN_REF").ot(*dst).a(target).b(s));
                } else {
                    let last = pl.segs.len() - 1;
                    let c = self.container(&pl, last, Mode::W);
                    match &pl.segs[last] {
                        Seg::Prop(n) => {
                            let n = self.z(n);
                            self.push(ZLine::new("ASSIGN_OBJ_REF").ot(*dst).a(c).b(n));
                        }
                        seg => {
                            let k = self.seg_key(seg);
                            let t = self.tmp();
                            self.push(ZLine::new("FETCH_DIM_W").v(t).a(c).b(k));
                            self.push(ZLine::new("ASSIGN_REF").ot(*dst).a(Z::Tmp(t)).b(s));
                            return;
                        }
                    }
                    self.push(ZLine::new("OP_DATA").a(s));
                }
            }
            Op::MakeRef { place, dst }
            | Op::ListFetchRef {
                place,
                dst: Some(dst),
            } => {
                let z = self.place_ref(*place);
                self.alias.insert(*dst, z);
            }
            Op::ListFetchRef { place, dst: None } => {
                let _ = self.place_ref(*place);
            }
            Op::Fetch { place, mode, dst } => {
                let m = match mode {
                    FetchMode::Read => Mode::R,
                    FetchMode::Is => Mode::Is,
                    FetchMode::List => Mode::List,
                };
                self.lower_fetch(*place, m, *dst);
            }
            Op::Isset { place, dst } => self.lower_isset(*place, *dst, false),
            Op::Empty { place, dst } => self.lower_isset(*place, *dst, true),
            Op::Unset { place } => self.lower_unset(*place),
            Op::Global { cv, name } => {
                let n = Z::Const(self.lit(*name));
                self.push(ZLine::new("BIND_GLOBAL").a(Z::Cv(*cv)).b(n));
            }
            Op::BindStatic { cv, .. } => {
                self.push(ZLine::new("BIND_STATIC").x(" (ref)").a(Z::Cv(*cv)));
            }
            // `zend_compile_method_call` fetches a `$this` that may be
            // missing (a closure, a static context) as a VAR; a guaranteed
            // one is the `THIS` operand of `INIT_METHOD_CALL` itself.
            Op::FetchThis { dst } => {
                let later = self
                    .ops
                    .iter()
                    .skip(i + 1)
                    .map(generic)
                    .find(|o| Self::reads_tmp(o, *dst));
                if matches!(later, Some(Op::InitMethodCall { obj: Operand::Tmp(t), .. }) if t == *dst)
                {
                    self.push(ZLine::new("FETCH_THIS").v(*dst));
                } else {
                    self.push(ZLine::new("FETCH_THIS").t(*dst));
                }
            }
            Op::InitFCall {
                name,
                fallback,
                display,
            } => self.lower_init_fcall(i, *name, *fallback, *display),
            Op::InitDynCall { callee } => {
                let c = self.copy_kept(callee);
                let (n, _) = self.scan_call(i);
                self.push(ZLine::new("INIT_DYNAMIC_CALL").x(format!(" {n}")).b(c));
                self.calls.push(Call {
                    kind: CallKind::Dynamic,
                    known: None,
                    frameless: None,
                    args: Vec::new(),
                    sent: 0,
                    inline: None,
                });
            }
            Op::InitMethodCall { obj, name } => {
                let o = match obj {
                    Operand::Unused => Z::Text("THIS".into()),
                    _ => self.z(obj),
                };
                let nm = self.z(name);
                let (n, _) = self.scan_call(i);
                // `zend_optimizer_get_called_func`: a closure's op array has
                // no scope there.
                let closure = self.func.flags & crate::func::flags::CLOSURE != 0;
                let found = match (&o, &nm, &self.scope) {
                    (Z::Text(t), Z::Const(Value::Str(m)), Some(cls)) if t == "THIS" && !closure => {
                        let mlc = m.as_bytes().to_ascii_lowercase();
                        self.res.method(cls, &mlc)
                    }
                    _ => None,
                };
                let known = found.map(|(k, _, _)| k);
                // `zend_optimizer_get_called_func`: a method that no child
                // class can override (private, final, or of a final class)
                // is not a prototype, so its calls may be inlined.
                let fixed = found.is_some_and(|(k, mf, cf)| {
                    use crate::class::cflags;
                    use crate::func::flags;
                    k.user.is_some()
                        && cf & cflags::TRAIT == 0
                        && (mf & (flags::PRIVATE | flags::FINAL) != 0 || cf & cflags::FINAL != 0)
                });
                let init_at = self.lines.len();
                self.push(ZLine::new("INIT_METHOD_CALL").x(format!(" {n}")).a(o).b(nm));
                self.calls.push(Call {
                    kind: CallKind::Method,
                    known,
                    frameless: None,
                    args: Vec::new(),
                    sent: 0,
                    inline: fixed.then_some(init_at),
                });
            }
            Op::InitStaticCall { class, name } => {
                let nm = self.z(name);
                let (n, _) = self.scan_call(i);
                let class_lc = match class {
                    ClassSpec::Named(l) => Some(self.lit_bytes(*l).to_ascii_lowercase()),
                    ClassSpec::SelfClass => self.scope.clone(),
                    _ => None,
                };
                let known = match (&class_lc, &nm) {
                    (Some(c), Z::Const(Value::Str(m))) => {
                        let mlc = m.as_bytes().to_ascii_lowercase();
                        self.res.method(c, &mlc).and_then(|(k, mf, _)| {
                            use crate::func::flags;
                            let same = self.scope.as_deref() == Some(c.as_slice());
                            (mf & flags::ABSTRACT == 0
                                && (mf & (flags::PRIVATE | flags::PROTECTED) == 0 || same))
                                .then_some(k)
                        })
                    }
                    _ => None,
                };
                // `zend_optimizer_get_class_entry_from_op1`: the script's
                // early-bound classes by name, and the method's own class.
                let resolved = class_lc.as_ref().is_some_and(|c| {
                    let scope = self.scope.as_deref() == Some(c.as_slice());
                    (scope || self.res.is_bound(c))
                        && self
                            .res
                            .class_decl(c)
                            .is_some_and(|d| d.flags & crate::class::cflags::TRAIT == 0)
                });
                let fixed = resolved && known.is_some_and(|k| k.user.is_some());
                let c = self.class_z(class, " (exception)");
                let init_at = self.lines.len();
                self.push(
                    ZLine::new("INIT_STATIC_METHOD_CALL")
                        .x(format!(" {n}"))
                        .a(c)
                        .b(nm),
                );
                self.calls.push(Call {
                    kind: CallKind::Static,
                    known,
                    frameless: None,
                    args: Vec::new(),
                    sent: 0,
                    inline: fixed.then_some(init_at),
                });
            }
            Op::InitParentHook { prop, set } => {
                let (n, _) = self.scan_call(i);
                let p = Z::Const(self.lit(*prop));
                self.push(
                    ZLine::new("INIT_PARENT_PROPERTY_HOOK_CALL")
                        .x(format!(" {n}"))
                        .a(p)
                        .b(Z::Num(u32::from(*set))),
                );
                self.calls.push(Call {
                    kind: CallKind::Hook,
                    known: None,
                    frameless: None,
                    args: Vec::new(),
                    sent: 0,
                    inline: None,
                });
            }
            Op::New { class, dst, .. } => {
                let (n, _) = self.scan_call(i);
                let known = match class {
                    ClassSpec::Named(l) => {
                        let lc = self.lit_bytes(*l).to_ascii_lowercase();
                        self.res.method(&lc, b"__construct").map(|(k, _, _)| k)
                    }
                    ClassSpec::SelfClass => self
                        .scope
                        .clone()
                        .and_then(|c| self.res.method(&c, b"__construct"))
                        .map(|(k, _, _)| k),
                    _ => None,
                };
                let c = self.class_z(class, " (exception)");
                self.push(ZLine::new("NEW").t(*dst).x(format!(" {n}")).a(c));
                self.calls.push(Call {
                    kind: CallKind::New,
                    known,
                    frameless: None,
                    args: Vec::new(),
                    sent: 0,
                    inline: None,
                });
            }
            Op::Send { src } => self.lower_send(src, None),
            Op::SendNamed { name, src } => {
                let n = self.lit(*name);
                self.lower_send(src, Some(n));
            }
            Op::SendUnpack { src } => {
                let s = self.z(src);
                if let Some(c) = self.calls.last_mut() {
                    c.frameless = None;
                }
                self.push(ZLine::new("SEND_UNPACK").a(s));
            }
            Op::SendPlaceholder { name } => {
                let sent = self.calls.last_mut().map_or(1, |c| {
                    c.sent += 1;
                    c.sent
                });
                let b = match name {
                    Some(n) => Z::Const(self.lit(*n)),
                    None => Z::Num(sent),
                };
                self.push(ZLine::new("SEND_PLACEHOLDER").b(b));
            }
            Op::DoCall { dst } => self.lower_do_call(*dst),
            Op::CallableConvert { dst, .. } => {
                self.calls.pop();
                self.push(ZLine::new("CALLABLE_CONVERT").t(*dst).x(" 0"));
            }
            Op::CallablePartial { dst, name, .. } => {
                self.calls.pop();
                let n = Z::Const(self.lit(*name));
                self.push(ZLine::new("CALLABLE_CONVERT_PARTIAL").t(*dst).x(" 0").b(n));
            }
            Op::Recv { n, cv } => {
                self.push(ZLine::new("RECV").r(Z::Cv(*cv)).a(Z::Num(n + 1)));
            }
            Op::RecvInit { n, cv, skip } => {
                // The default's code up to `skip` is php-src's
                // `CONSTANT_AST` literal of `RECV_INIT`.
                let end = (*skip as usize).clamp(i + 1, self.ops.len());
                let default = match self.op_at(i + 1) {
                    Some(Op::RecvDefault {
                        val: v @ Operand::Lit(_),
                        ..
                    }) if end == i + 2 => self.z(&v),
                    _ => Z::Text("zval(type=11)".into()),
                };
                self.skip[i + 1..end].fill(true);
                self.push(
                    ZLine::new("RECV_INIT")
                        .r(Z::Cv(*cv))
                        .a(Z::Num(n + 1))
                        .b(default),
                );
            }
            Op::RecvDefault { .. } => {}
            Op::RecvVariadic { n, cv } => {
                self.push(ZLine::new("RECV_VARIADIC").r(Z::Cv(*cv)).a(Z::Num(n + 1)));
            }
            Op::VerifyReturn { val } => {
                self.verified = true;
                let v = self.z(val);
                if v.is_const() {
                    // `zend_emit_return_type_check` gives a literal a TMP
                    // result, which the return then reads.
                    let t = self.tmp();
                    self.push(ZLine::new("VERIFY_RETURN_TYPE").t(t).a(v));
                    if let Some(Op::Return {
                        val: r @ Operand::Lit(_),
                    }) = self.op_at(i + 1)
                    {
                        if r == *val {
                            self.skip[i + 1] = true;
                            self.push(ZLine::new("RETURN").a(Z::Tmp(t)));
                        }
                    }
                } else {
                    self.push(ZLine::new("VERIFY_RETURN_TYPE").a(v));
                }
            }
            Op::Return { val } => {
                let mut v = match self.z(val) {
                    Z::Unused => Z::Const(Value::Null),
                    z => z,
                };
                if !std::mem::take(&mut self.verified) {
                    if let Some(t) = self.return_type_check(val) {
                        v = t;
                    }
                }
                let name = if self.func.is_generator() {
                    "GENERATOR_RETURN"
                } else {
                    "RETURN"
                };
                self.push(ZLine::new(name).a(v));
            }
            Op::ReturnRef { place } => {
                let z = self.place_ref(*place);
                self.push(ZLine::new("RETURN_BY_REF").x(" (function)").a(z));
            }
            Op::ReturnByRefValue { val } => {
                let v = match self.z(val) {
                    Z::Unused => Z::Const(Value::Null),
                    z => z,
                };
                self.push(ZLine::new("RETURN_BY_REF").x(" (value)").a(v));
            }
            Op::GeneratorCreate => self.push(ZLine::new("GENERATOR_CREATE")),
            Op::Yield { key, val, dst } => {
                let v = val.as_ref().map_or(Z::Unused, |v| self.z(v));
                let k = key.as_ref().map_or(Z::Unused, |k| self.z(k));
                self.push(ZLine::new("YIELD").ot(*dst).a(v).b(k));
            }
            Op::YieldRef { key, place, dst } => {
                let v = self.place_ref(*place);
                let k = key.as_ref().map_or(Z::Unused, |k| self.z(k));
                self.push(ZLine::new("YIELD").ot(*dst).a(v).b(k));
            }
            Op::YieldByRefValue { key, val, dst, .. } => {
                let v = self.z(val);
                let k = key.as_ref().map_or(Z::Unused, |k| self.z(k));
                self.push(ZLine::new("YIELD").ot(*dst).a(v).b(k));
            }
            Op::YieldFrom { src, dst } => {
                let s = self.z(src);
                self.push(ZLine::new("YIELD_FROM").ot(*dst).a(s));
            }
            Op::DeclareFunction(idx) => {
                let f = self.oa.decls.functions.borrow().get(*idx as usize).cloned();
                let name = f
                    .map(|f| f.name.as_bytes().to_ascii_lowercase())
                    .unwrap_or_default();
                let n = self.dyn_index(*idx);
                let z = self.zs(&name);
                self.push(ZLine::new("DECLARE_FUNCTION").a(z).b(Z::Num(n)));
            }
            Op::DeclareClass(idx) => {
                let Some(d) = self.oa.decls.classes.borrow().get(*idx as usize).cloned() else {
                    return;
                };
                let name = d.name.as_bytes().to_ascii_lowercase();
                let parent = d.parent.as_ref().map(|p| p.as_bytes().to_ascii_lowercase());
                // `zend_compile_class_decl`: a top-level class without
                // interfaces or traits whose parent (if any) is already
                // linked is bound at compile time and emits nothing.
                if binds_early(self.func, i, &d, &self.early_bound) {
                    self.early_bound.push(name);
                    return;
                }
                let z = self.zs(&name);
                let p = parent.map_or(Z::Unused, |p| self.zs(&p));
                self.push(ZLine::new("DECLARE_CLASS").a(z).b(p));
            }
            Op::DeclareAnonClass { idx, dst } => {
                let d = self.oa.decls.classes.borrow().get(*idx as usize).cloned();
                let name = d
                    .map(|d| d.name.as_bytes().to_ascii_lowercase())
                    .unwrap_or_default();
                let z = self.zs(&name);
                self.anon.push(*dst);
                self.push(ZLine::new("DECLARE_ANON_CLASS").v(*dst).a(z));
            }
            Op::MakeClosure { idx, dst } => {
                let n = self.dyn_index(*idx);
                let f = self.oa.decls.functions.borrow().get(*idx as usize).cloned();
                // `extended_value`: a cache slot pair for a static closure
                // without static variables declared in a function, else -1.
                let cached = f.as_ref().is_some_and(|f| {
                    f.flags & crate::func::flags::STATIC != 0
                        && f.code.num_statics == 0
                        && !self.func.name.is_empty()
                });
                let x = if cached { 0 } else { u32::MAX };
                self.push(
                    ZLine::new("DECLARE_LAMBDA_FUNCTION")
                        .t(*dst)
                        .x(format!(" {x}"))
                        .b(Z::Num(n)),
                );
                if let Some(f) = f {
                    for u in &f.uses {
                        let l = ZLine::new("BIND_LEXICAL")
                            .x(if u.by_ref { " (ref)" } else { "" })
                            .a(Z::Tmp(*dst))
                            .b(Z::Cv(u.parent_cv));
                        self.push(l);
                    }
                }
            }
            Op::DeclareConst { name, val, .. } => {
                let n = Z::Const(self.lit(*name));
                let v = self.z(val);
                self.push(ZLine::new("DECLARE_CONST").a(n).b(v));
            }
            Op::FetchConst {
                name,
                fallback,
                dst,
            } => {
                let a = if fallback.is_some() {
                    Z::Text("(unqualified-in-namespace)".into())
                } else {
                    Z::Unused
                };
                let n = Z::Const(self.lit(*name));
                self.push(ZLine::new("FETCH_CONSTANT").t(*dst).a(a).b(n));
            }
            Op::FetchClassConst { class, name, dst } => {
                let n = self.z(name);
                let found = match &n {
                    Z::Const(Value::Str(s)) => {
                        let class_name = match class {
                            ClassSpec::Named(l) => Some(self.lit_bytes(*l)),
                            _ => None,
                        };
                        self.res.class_const(
                            self.scope.as_deref(),
                            class,
                            class_name.as_deref(),
                            s.as_bytes(),
                        )
                    }
                    _ => None,
                };
                // Pass 1 replaces the fetch of a constant no child class
                // can override by its value (`replace_by_const_or_qm_assign`).
                if let Some(ClassConst {
                    value: Some(v),
                    is_prototype: false,
                    ..
                }) = &found
                {
                    if self.res.level & crate::compiler::optimizer::PASS_1 != 0 {
                        self.alias.insert(*dst, Z::Const(v.clone()));
                        return;
                    }
                }
                let c = self.class_z(class, " (exception)");
                let mut l = ZLine::new("FETCH_CLASS_CONSTANT").t(*dst).a(c).b(n);
                // The type of a typed constant (`zend_update_type_info`).
                l.ret = found.and_then(|cc| cc.ty);
                self.push(l);
            }
            Op::FetchClassName { class, dst, .. } => {
                let c = self.class_z(class, "");
                self.push(ZLine::new("FETCH_CLASS_NAME").t(*dst).a(c));
            }
            Op::InstanceOf { a, class, dst } => {
                let a = self.z(a);
                let c = self.class_z(class, " (no-autoload) (silent) (exception)");
                self.push(ZLine::new("INSTANCEOF").t(*dst).a(a).b(c));
            }
            Op::Clone { a, dst } => {
                let a = self.z(a);
                self.push(ZLine::new("CLONE").t(*dst).a(a));
            }
            Op::FuncNumArgs { dst } => self.push(ZLine::new("FUNC_NUM_ARGS").ot(*dst)),
            Op::FuncGetArgs { dst } => self.push(ZLine::new("FUNC_GET_ARGS").ot(*dst)),
            Op::Strlen { a, dst } => {
                let a = self.z(a);
                self.push(ZLine::new("STRLEN").ot(*dst).a(a));
            }
            Op::Count { a, dst, .. } => {
                let a = self.z(a);
                self.push(ZLine::new("COUNT").ot(*dst).a(a));
            }
            Op::TypeCheck { a, mask, dst } => {
                let a = self.z(a);
                self.push(
                    ZLine::new("TYPE_CHECK")
                        .ot(*dst)
                        .x(type_mask_text(*mask))
                        .a(a),
                );
            }
            Op::GetType { a, dst } => {
                let a = self.z(a);
                self.push(ZLine::new("GET_TYPE").ot(*dst).a(a));
            }
            Op::GetClass { a, dst } => {
                let a = self.z(a);
                self.push(ZLine::new("GET_CLASS").ot(*dst).a(a));
            }
            Op::GetCalledClass { dst } => self.push(ZLine::new("GET_CALLED_CLASS").ot(*dst)),
            Op::Defined { name, dst } => {
                let n = Z::Const(self.lit(*name));
                self.push(ZLine::new("DEFINED").ot(*dst).a(n));
            }
            Op::InArray {
                a,
                set,
                strict,
                dst,
            } => {
                let a = self.z(a);
                let s = Z::Const(self.lit(*set));
                self.push(
                    ZLine::new("IN_ARRAY")
                        .ot(*dst)
                        .x(format!(" {}", u32::from(*strict)))
                        .a(a)
                        .b(s),
                );
            }
            Op::KeyExists { key, array, dst } => {
                let k = self.z(key);
                let a = self.z(array);
                self.push(ZLine::new("ARRAY_KEY_EXISTS").ot(*dst).a(k).b(a));
            }
            Op::ArrayKeyExists { dst, .. } => self.lower_do_call(*dst),
            Op::Echo(a) => {
                let folded = matches!(a, Operand::Tmp(_) | Operand::TmpKeep(_));
                let a = match self.z(a) {
                    // A literal that replaced a temporary is converted to a
                    // string, and an empty one echoes nothing
                    // (`zend_optimizer_update_op1_const`).
                    Z::Const(v) if folded => match v {
                        Value::Int(n) => Z::Const(Value::Str(ZStr::from(n.to_string().as_bytes()))),
                        Value::Bool(true) => Z::Const(Value::Str(ZStr::from(&b"1"[..]))),
                        Value::Bool(false) | Value::Null => return,
                        Value::Str(s) if s.as_bytes().is_empty() => return,
                        other => Z::Const(other),
                    },
                    z => z,
                };
                self.push(ZLine::new("ECHO").a(a));
            }
            Op::Exit(a) => {
                let a = self.z(a);
                self.push(ZLine::new("EXIT").a(a));
            }
            Op::Free(a) => match self.z(a) {
                Z::Cv(c) => self.push(ZLine::new("CHECK_VAR").a(Z::Cv(c))),
                z @ Z::Tmp(_) => {
                    let mut l = ZLine::new("FREE").a(z);
                    l.free_on_return = self.before_return(i);
                    self.push(l);
                }
                _ => {}
            },
            Op::Throw(a) => {
                let a = self.z(a);
                self.push(ZLine::new("THROW").a(a));
            }
            Op::Catch { classes, cv, next } => {
                let n = classes.len();
                for (k, c) in classes.iter().enumerate() {
                    let cz = Z::Const(self.lit(*c));
                    let last = k + 1 == n;
                    let mut l = ZLine::new("CATCH").a(cz).r(cv.map_or(Z::Unused, Z::Cv));
                    if last {
                        // `ZEND_LAST_CATCH` has no target.
                        if !matches!(self.op_at(*next as usize), Some(Op::Rethrow) | None) {
                            l = l.b(Z::Label(*next));
                        }
                    } else {
                        // The next CATCH of the same clause.
                        let at = u32::try_from(self.lines.len() + 1).unwrap_or(u32::MAX);
                        l = l.b(Z::ZLabel(at));
                    }
                    self.push(l);
                }
            }
            Op::FastCall { fin, target } => {
                let t = self.fin_tmp(*fin);
                self.push(ZLine::new("FAST_CALL").t(t).a(Z::Label(*target)));
            }
            Op::FastRet { fin } => {
                let t = self.fin_tmp(*fin);
                self.push(ZLine::new("FAST_RET").a(Z::Tmp(t)));
            }
            Op::DiscardFinally { fin } => {
                let t = self.fin_tmp(*fin);
                self.push(ZLine::new("DISCARD_EXCEPTION").a(Z::Tmp(t)));
            }
            Op::BeginSilence { dst } => self.push(ZLine::new("BEGIN_SILENCE").t(*dst)),
            Op::EndSilence { saved } => {
                let s = self.z(saved);
                self.push(ZLine::new("END_SILENCE").a(s));
            }
            Op::Include { kind, a, dst } => {
                let a = self.z(a);
                let k = match kind {
                    IncludeKind::Include => " (include)",
                    IncludeKind::IncludeOnce => " (include_once)",
                    IncludeKind::Require => " (require)",
                    IncludeKind::RequireOnce => " (require_once)",
                    IncludeKind::Eval => " (eval)",
                };
                self.push(ZLine::new("INCLUDE_OR_EVAL").t(*dst).x(k).a(a));
            }
            Op::FeReset {
                subject,
                place,
                kind,
                iter,
                end,
            } => {
                let t = self.tmp();
                self.iters.insert(*iter, t);
                // `zend_compile_foreach` gives a by-value iterator a TMP;
                // the loop of `zend_compile_func_array_map` (after its
                // `TYPE_ASSERT` and `INIT_ARRAY`) keeps a VAR.
                let array_map = i >= 2
                    && matches!(self.op_at(i - 1), Some(Op::InitArray { .. }))
                    && matches!(self.op_at(i - 2), Some(Op::TypeAssertArray { .. }));
                let (name, s, var) = match (kind, place) {
                    (ForeachKind::Ref, Some(p)) => ("FE_RESET_RW", self.place_ref(*p), true),
                    _ => ("FE_RESET_R", self.z(subject), array_map),
                };
                let l = ZLine::new(name).a(s).b(Z::Label(*end));
                self.push(if var { l.v(t) } else { l.t(t) });
            }
            Op::FeFetch {
                iter,
                end,
                val,
                key,
            } => self.lower_fe_fetch(i, *iter, *end, *val, *key),
            Op::FeFree { iter } => {
                let t = self.iter_tmp(*iter);
                self.push(ZLine::new("FE_FREE").a(Z::Tmp(t)));
            }
            Op::FeFreeOnReturn { iter, .. } => {
                let t = self.iter_tmp(*iter);
                let mut l = ZLine::new("FE_FREE").a(Z::Tmp(t));
                l.free_on_return = true;
                self.push(l);
            }
            Op::MatchError { subject } => {
                let s = self.z(subject);
                self.push(ZLine::new("MATCH_ERROR").a(s));
            }
            Op::Match { subject, table } => {
                let s = self.z(subject);
                let mut l = ZLine::new("MATCH").a(s).j(Z::Label(table.default));
                l.jt = Some(
                    table
                        .entries
                        .iter()
                        .map(|(k, t)| {
                            let k = match k {
                                MatchKey::Int(i) => Value::Int(*i),
                                MatchKey::Str(b) => Value::Str(ZStr::from(&b[..])),
                            };
                            (k, Z::Label(*t))
                        })
                        .collect(),
                );
                self.push(l);
            }
            Op::TypeAssertArray { val, msg } => {
                // "name(): Argument #N ($x) must be of type array".
                let m = self.lit_bytes(*msg);
                let fname = m
                    .iter()
                    .position(|&c| c == b'(')
                    .map_or(&m[..0], |p| &m[..p])
                    .to_vec();
                let argn = m
                    .iter()
                    .position(|&c| c == b'#')
                    .map(|p| {
                        m[p + 1..]
                            .iter()
                            .take_while(|c| c.is_ascii_digit())
                            .fold(0u32, |acc, c| acc * 10 + u32::from(c - b'0'))
                    })
                    .unwrap_or(0);
                let v = self.z(val);
                let f = self.zs(&fname);
                self.push(
                    ZLine::new("TYPE_ASSERT")
                        .x(format!(" {}", (argn << 16) | 7))
                        .a(f)
                        .b(v),
                );
            }
            Op::Ticks(n) => self.push(ZLine::new("TICKS").x(format!(" {n}"))),
            Op::AssertCheck { dst, skip } => {
                self.push(ZLine::new("ASSERT_CHECK").t(*dst).a(Z::Label(*skip)));
            }
            Op::ConstPropCheck { name, dst, .. } => {
                let n = self.z(name);
                self.push(ZLine::new("QM_ASSIGN").t(*dst).a(n));
            }
            Op::Abs { src, dst, .. } => {
                // `InitFCall("abs")` + `Send` + `DoCall`, specialised.
                let s = self.z(src);
                let name = self.zs(b"abs");
                let mut init = ZLine::new("INIT_FCALL").x(" 1").a(Z::Num(0)).b(name);
                init.stack = Some(StackOf::Internal(1));
                self.push(init);
                let send = if s.cv().is_some() {
                    "SEND_VAR"
                } else {
                    "SEND_VAL"
                };
                self.push(ZLine::new(send).a(s).b(Z::Num(1)));
                self.push(ZLine::new("DO_ICALL").ot(*dst));
            }
            // Every specialised form was mapped back by `generic`.
            _ => {}
        }
    }

    /// Whether the frees after op `i` lead to a return: a `FREE` there is
    /// `zend_handle_loops_and_finally`'s `ZEND_FREE_ON_RETURN`.
    fn before_return(&self, i: usize) -> bool {
        for op in self.ops.iter().skip(i + 1).map(generic) {
            match op {
                Op::Free(_) | Op::FeFreeOnReturn { .. } | Op::VerifyReturn { .. } => {}
                Op::Return { .. } | Op::ReturnRef { .. } | Op::ReturnByRefValue { .. } => {
                    return true;
                }
                _ => return false,
            }
        }
        false
    }

    /// `zend_emit_return_type_check` (and `zend_emit_final_return` for the
    /// implicit return, an unused `val`): the `VERIFY_RETURN_TYPE` the VM
    /// folds into its return. Returns the operand the `RETURN` then reads
    /// when the check copied a literal to a TMP.
    fn return_type_check(&mut self, val: &Operand) -> Option<Z> {
        use crate::types::mask as m;
        if self.func.is_generator() {
            return None;
        }
        let rt = self.func.return_type.as_ref()?;
        if rt.mask & m::VOID != 0 {
            return None;
        }
        if matches!(val, Operand::Unused) {
            if rt.mask & m::NEVER != 0 {
                self.push(ZLine::new("VERIFY_NEVER_TYPE"));
            } else {
                self.push(ZLine::new("VERIFY_RETURN_TYPE"));
            }
            return None;
        }
        let v = self.z(val);
        // `mixed`: no run-time check.
        let mixed =
            rt.classes.is_empty() && rt.intersections.is_empty() && rt.mask & m::ANY == m::ANY;
        if mixed {
            return None;
        }
        // An inlined call's literal reaches the check through its
        // `QM_ASSIGN`, which the block pass folds into the check
        // (`T = VERIFY_RETURN_TYPE <literal>`) whatever its type.
        if v.is_const()
            && matches!(val, Operand::Tmp(t) | Operand::TmpKeep(t) if self.inlined.contains(t))
        {
            let t = self.tmp();
            self.push(ZLine::new("VERIFY_RETURN_TYPE").t(t).a(v));
            return Some(Z::Tmp(t));
        }
        if let Z::Const(c) = &v {
            let bit = match c {
                Value::Null => m::NULL,
                Value::Bool(false) => m::FALSE,
                Value::Bool(true) => m::TRUE,
                Value::Int(_) => m::LONG,
                Value::Float(_) => m::DOUBLE,
                Value::Str(_) => m::STRING,
                Value::Array(_) => m::ARRAY,
                _ => 0,
            };
            // `ZEND_TYPE_CONTAINS_CODE`: `bool` contains both `true` and
            // `false`; `callable` and `iterable` are not builtin codes of
            // a literal.
            if bit != 0 && rt.mask & bit != 0 {
                return None;
            }
            let t = self.tmp();
            self.push(ZLine::new("VERIFY_RETURN_TYPE").t(t).a(v));
            return Some(Z::Tmp(t));
        }
        self.push(ZLine::new("VERIFY_RETURN_TYPE").a(v));
        None
    }

    /// `zend_compile_short_circuiting`: `JMPZ_EX`/`JMPNZ_EX` of a TMP
    /// stores its result in that TMP.
    fn short_circuit_result(&mut self, a: &Z, dst: u32) {
        if let Z::Tmp(x) = a {
            if *x != dst {
                self.rename.insert(dst, *x);
            }
        }
    }

    /// `zend_compile_closure_uses`: a closure body starts by binding its
    /// `use` variables (`BIND_STATIC`), after the parameters.
    fn bind_uses(&mut self) {
        let f = self.func;
        for u in &f.uses {
            let x = if u.by_ref { " (ref)" } else { "" };
            self.push(ZLine::new("BIND_STATIC").x(x).a(Z::Cv(u.cv)));
        }
    }

    fn fin_tmp(&mut self, fin: u32) -> u32 {
        // Finally blocks get stable synthetic temporaries.
        let key = u32::MAX - 1 - fin;
        if let Some(Z::Tmp(t)) = self.alias.get(&key) {
            return *t;
        }
        let t = self.tmp();
        self.alias.insert(key, Z::Tmp(t));
        t
    }

    fn iter_tmp(&mut self, iter: u32) -> u32 {
        if let Some(t) = self.iters.get(&iter) {
            return *t;
        }
        let t = self.tmp();
        self.iters.insert(iter, t);
        t
    }

    /// The position of decl `idx` among this op array's dynamic function
    /// definitions (`DECLARE_LAMBDA_FUNCTION`'s `op1.num`).
    fn dyn_index(&self, idx: u32) -> u32 {
        self.dyn_defs
            .iter()
            .position(|&d| d == idx)
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(0)
    }

    fn lower_rope(&mut self, parts: &[Operand], dst: u32) {
        let zs: Vec<Z> = parts.iter().map(|p| self.z(p)).collect();
        match zs.len() {
            0 => self.push(
                ZLine::new("QM_ASSIGN")
                    .t(dst)
                    .a(Z::Const(Value::Str(ZStr::empty()))),
            ),
            1 => {
                let a = zs[0].clone();
                self.push(ZLine::new("CAST").t(dst).x(" (string)").a(a));
            }
            2 => {
                let (a, b) = (zs[0].clone(), zs[1].clone());
                self.push(ZLine::new("FAST_CONCAT").t(dst).a(a).b(b));
            }
            n => {
                let r = self.tmp();
                let mut init = ZLine::new("ROPE_INIT")
                    .t(r)
                    .x(format!(" {n}"))
                    .b(zs[0].clone());
                init.xv = u32::try_from(n).unwrap_or(0);
                self.push(init);
                for (k, z) in zs.iter().enumerate().take(n - 1).skip(1) {
                    let mut l = ZLine::new("ROPE_ADD")
                        .t(r)
                        .x(format!(" {k}"))
                        .a(Z::Tmp(r))
                        .b(z.clone());
                    l.xv = u32::try_from(k).unwrap_or(0);
                    self.push(l);
                }
                let mut end = ZLine::new("ROPE_END")
                    .t(dst)
                    .x(format!(" {}", n - 1))
                    .a(Z::Tmp(r))
                    .b(zs[n - 1].clone());
                end.xv = u32::try_from(n - 1).unwrap_or(0);
                self.push(end);
            }
        }
    }

    /// `INIT_ARRAY`: emitted with the first element (`zend_compile_array`).
    fn lower_init_array(&mut self, i: usize, dst: u32) {
        let mut count = 0u32;
        let mut packed = true;
        let mut first: Option<usize> = None;
        let mut used_before_first = false;
        for (j, op) in self.ops.iter().enumerate().skip(i + 1) {
            match generic(op) {
                Op::ArrayAdd { arr, key, .. } | Op::ArrayAddRef { arr, key, .. } if arr == dst => {
                    count += 1;
                    packed &= key.is_none();
                    first.get_or_insert(j);
                }
                Op::ArrayUnpack { arr, .. } | Op::ConstArrayUnpack { arr, .. } if arr == dst => {
                    count += 1;
                    packed = false;
                    first.get_or_insert(j);
                }
                Op::InitArray { dst: d } if d == dst => break,
                // A loop filling the array (`array_map()` inlined by
                // `zend_compile_func_array_map`) rather than its literal.
                Op::FeReset { .. } | Op::FeFetch { .. } => break,
                Op::Jmp(t) if t as usize <= j => break,
                o => {
                    if Self::reads_tmp(&o, dst) {
                        if first.is_none() {
                            used_before_first = true;
                        }
                        break;
                    }
                }
            }
        }
        let size = format!(" {count}{}", if packed { " (packed)" } else { "" });
        match first {
            Some(j) if !used_before_first => {
                if let Some(Op::ArrayAdd { key, val, .. }) = self.op_at(j) {
                    // Lowered here, when the first element is reached.
                    self.pending_init.insert(j, (dst, size, key, val));
                    return;
                }
                self.push(
                    ZLine::new("INIT_ARRAY")
                        .t(dst)
                        .x(size)
                        .b(Z::Text("NEXT".into())),
                );
            }
            _ => {
                self.push(
                    ZLine::new("INIT_ARRAY")
                        .t(dst)
                        .x(size)
                        .b(Z::Text("NEXT".into())),
                );
            }
        }
    }
}

/// The name of a fetch opcode: `kind` is `b'D'` (dimension), `b'O'`
/// (property), `b'S'` (static property) or `b'V'` (variable by name).
fn fetch_name(kind: u8, mode: Mode) -> &'static str {
    match (kind, mode) {
        (b'D', Mode::R) => "FETCH_DIM_R",
        (b'D', Mode::Is) => "FETCH_DIM_IS",
        (b'D', Mode::W) => "FETCH_DIM_W",
        (b'D', Mode::Rw) => "FETCH_DIM_RW",
        (b'D', Mode::FuncArg) => "FETCH_DIM_FUNC_ARG",
        (b'D', Mode::Unset) => "FETCH_DIM_UNSET",
        (b'D', Mode::List) => "FETCH_LIST_R",
        (b'O', Mode::R | Mode::List) => "FETCH_OBJ_R",
        (b'O', Mode::Is) => "FETCH_OBJ_IS",
        (b'O', Mode::W) => "FETCH_OBJ_W",
        (b'O', Mode::Rw) => "FETCH_OBJ_RW",
        (b'O', Mode::FuncArg) => "FETCH_OBJ_FUNC_ARG",
        (b'O', Mode::Unset) => "FETCH_OBJ_UNSET",
        (b'S', Mode::R | Mode::List) => "FETCH_STATIC_PROP_R",
        (b'S', Mode::Is) => "FETCH_STATIC_PROP_IS",
        (b'S', Mode::W) => "FETCH_STATIC_PROP_W",
        (b'S', Mode::Rw) => "FETCH_STATIC_PROP_RW",
        (b'S', Mode::FuncArg) => "FETCH_STATIC_PROP_FUNC_ARG",
        (b'S', Mode::Unset) => "FETCH_STATIC_PROP_UNSET",
        (_, Mode::R | Mode::List) => "FETCH_R",
        (_, Mode::Is) => "FETCH_IS",
        (_, Mode::W) => "FETCH_W",
        (_, Mode::Rw) => "FETCH_RW",
        (_, Mode::FuncArg) => "FETCH_FUNC_ARG",
        (_, Mode::Unset) => "FETCH_UNSET",
    }
}

fn incdec_name(kind: IncDecKind, suffix: &str) -> &'static str {
    match (kind, suffix) {
        (IncDecKind::PreInc, "") => "PRE_INC",
        (IncDecKind::PreDec, "") => "PRE_DEC",
        (IncDecKind::PostInc, "") => "POST_INC",
        (IncDecKind::PostDec, "") => "POST_DEC",
        (IncDecKind::PreInc, "OBJ") => "PRE_INC_OBJ",
        (IncDecKind::PreDec, "OBJ") => "PRE_DEC_OBJ",
        (IncDecKind::PostInc, "OBJ") => "POST_INC_OBJ",
        (IncDecKind::PostDec, "OBJ") => "POST_DEC_OBJ",
        (IncDecKind::PreInc, _) => "PRE_INC_STATIC_PROP",
        (IncDecKind::PreDec, _) => "PRE_DEC_STATIC_PROP",
        (IncDecKind::PostInc, _) => "POST_INC_STATIC_PROP",
        (IncDecKind::PostDec, _) => "POST_DEC_STATIC_PROP",
    }
}

impl Lower<'_> {
    fn seg_key(&self, seg: &Seg) -> Z {
        match seg {
            Seg::Dim(o) | Seg::Prop(o) => self.z(o),
            Seg::Append => Z::Text("NEXT".into()),
        }
    }

    fn res_line(l: ZLine, t: u32, mode: Mode) -> ZLine {
        if mode.is_var() {
            l.v(t)
        } else {
            l.t(t)
        }
    }

    /// Fetches the base of a place that is not a plain variable or value
    /// (`$$name`, `$GLOBALS['x']`, `A::$x`) into `dst` (a new temporary
    /// when `None`); `dim_write` marks a fetch for a dimension write.
    fn base_fetch(&mut self, base: &PlaceBase, mode: Mode, dst: Option<u32>, dim_write: bool) -> Z {
        match base {
            PlaceBase::Cv(c) => Z::Cv(*c),
            PlaceBase::This => Z::Text("THIS".into()),
            PlaceBase::Value(o) => self.z(o),
            PlaceBase::GlobalsArray => {
                let t = dst.unwrap_or_else(|| self.tmp());
                self.push(ZLine::new("FETCH_GLOBALS").t(t));
                Z::Tmp(t)
            }
            PlaceBase::VarVar(o) | PlaceBase::Global(o) => {
                let n = self.z(o);
                let t = dst.unwrap_or_else(|| self.tmp());
                let scope = if matches!(base, PlaceBase::Global(_)) {
                    " (global)"
                } else {
                    " (local)"
                };
                let l = ZLine::new(fetch_name(b'V', mode)).x(scope).a(n);
                self.push(Self::res_line(l, t, mode));
                Z::Tmp(t)
            }
            PlaceBase::StaticProp { class, name } => {
                let n = self.z(name);
                let c = self.class_z(class, " (exception)");
                let t = dst.unwrap_or_else(|| self.tmp());
                let mut l = ZLine::new(fetch_name(b'S', mode)).a(n).b(c);
                if dim_write && mode == Mode::W {
                    l = l.x(" (dim write)");
                }
                self.push(Self::res_line(l, t, mode));
                Z::Tmp(t)
            }
        }
    }

    /// Applies one segment to `cur` in `mode`, into `dst`.
    fn apply_seg(&mut self, cur: Z, seg: &Seg, next: Option<&Seg>, mode: Mode, dst: u32) {
        let cur = match (&cur, seg) {
            (Z::Text(t), Seg::Dim(_) | Seg::Append) if t == "THIS" => {
                let t = self.tmp();
                self.push(ZLine::new("FETCH_THIS").t(t));
                Z::Tmp(t)
            }
            _ => cur,
        };
        let key = self.seg_key(seg);
        let l = match seg {
            Seg::Dim(_) => ZLine::new(fetch_name(b'D', mode)),
            Seg::Append => ZLine::new(fetch_name(b'D', Mode::W)),
            Seg::Prop(_) => {
                let mut l = ZLine::new(fetch_name(b'O', mode));
                if mode == Mode::W && matches!(next, Some(Seg::Dim(_) | Seg::Append)) {
                    l = l.x(" (dim write)");
                } else if mode == Mode::FuncArg && next.is_none() {
                    // `zend_compile_var(..., by_ref)` of an argument sets
                    // `ZEND_FETCH_REF` on the property fetch.
                    l = l.x(" (ref)");
                }
                l
            }
        };
        let mode = if matches!(seg, Seg::Append) {
            Mode::W
        } else {
            mode
        };
        self.push(Self::res_line(l.a(cur).b(key), dst, mode));
    }

    /// The container of the place's segment `upto`: the base and the
    /// segments before it, fetched in `mode`.
    fn container(&mut self, pl: &Place, upto: usize, mode: Mode) -> Z {
        let dim_write = matches!(pl.segs.first(), Some(Seg::Dim(_) | Seg::Append));
        let mut cur = self.base_fetch(&pl.base, mode, None, dim_write);
        for k in 0..upto {
            let t = self.tmp();
            self.apply_seg(cur, &pl.segs[k], pl.segs.get(k + 1), mode, t);
            cur = Z::Tmp(t);
        }
        cur
    }

    /// The place fetched for writing (`BP_VAR_W`): a variable or a VAR.
    fn place_ref(&mut self, p: u32) -> Z {
        let pl = self.oa.places[p as usize].clone();
        if pl.segs.is_empty() {
            return self.base_fetch(&pl.base, Mode::W, None, false);
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::W);
        let t = self.tmp();
        self.apply_seg(c, &pl.segs[last], None, Mode::W, t);
        Z::Tmp(t)
    }

    /// Reads the place into `dst` in `mode`.
    fn read_place(&mut self, pl: &Place, mode: Mode, dst: u32) {
        if pl.segs.is_empty() {
            match &pl.base {
                PlaceBase::Cv(c) => self.push(ZLine::new("QM_ASSIGN").t(dst).a(Z::Cv(*c))),
                PlaceBase::This => self.push(ZLine::new("FETCH_THIS").t(dst)),
                PlaceBase::Value(o) => {
                    let z = self.z(o);
                    self.push(ZLine::new("QM_ASSIGN").t(dst).a(z));
                }
                b => {
                    let _ = self.base_fetch(b, mode, Some(dst), false);
                }
            }
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(pl, last, mode);
        self.apply_seg(c, &pl.segs[last], None, mode, dst);
    }

    fn lower_fetch(&mut self, p: u32, mode: Mode, dst: u32) {
        let pl = self.oa.places[p as usize].clone();
        self.read_place(&pl, mode, dst);
    }

    fn lower_isset(&mut self, p: u32, dst: u32, empty: bool) {
        let pl = self.oa.places[p as usize].clone();
        let x = if empty { " (empty)" } else { " (isset)" };
        if pl.segs.is_empty() {
            let l = match &pl.base {
                PlaceBase::Cv(c) => ZLine::new("ISSET_ISEMPTY_CV").a(Z::Cv(*c)),
                PlaceBase::This => ZLine::new("ISSET_ISEMPTY_THIS"),
                PlaceBase::VarVar(o) => {
                    let n = self.z(o);
                    ZLine::new("ISSET_ISEMPTY_VAR").a(n)
                }
                PlaceBase::Global(o) => {
                    let n = self.z(o);
                    return self.push(
                        ZLine::new("ISSET_ISEMPTY_VAR")
                            .t(dst)
                            .x(format!(" (global){x}"))
                            .a(n),
                    );
                }
                PlaceBase::StaticProp { class, name } => {
                    let n = self.z(name);
                    let c = self.class_z(class, " (exception)");
                    ZLine::new("ISSET_ISEMPTY_STATIC_PROP").a(n).b(c)
                }
                PlaceBase::Value(o) => {
                    let z = self.z(o);
                    ZLine::new("TYPE_CHECK").a(z)
                }
                PlaceBase::GlobalsArray => ZLine::new("ISSET_ISEMPTY_VAR"),
            };
            self.push(l.t(dst).x(x));
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::Is);
        let key = self.seg_key(&pl.segs[last]);
        let name = if matches!(pl.segs[last], Seg::Prop(_)) {
            "ISSET_ISEMPTY_PROP_OBJ"
        } else {
            "ISSET_ISEMPTY_DIM_OBJ"
        };
        self.push(ZLine::new(name).t(dst).x(x).a(c).b(key));
    }

    fn lower_unset(&mut self, p: u32) {
        let pl = self.oa.places[p as usize].clone();
        if pl.segs.is_empty() {
            let l = match &pl.base {
                PlaceBase::Cv(c) => ZLine::new("UNSET_CV").a(Z::Cv(*c)),
                PlaceBase::VarVar(o) => {
                    let n = self.z(o);
                    ZLine::new("UNSET_VAR").x(" (local)").a(n)
                }
                PlaceBase::Global(o) => {
                    let n = self.z(o);
                    ZLine::new("UNSET_VAR").x(" (global)").a(n)
                }
                PlaceBase::StaticProp { class, name } => {
                    let n = self.z(name);
                    let c = self.class_z(class, " (exception)");
                    ZLine::new("UNSET_STATIC_PROP").a(n).b(c)
                }
                _ => return,
            };
            self.push(l);
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::Unset);
        let key = self.seg_key(&pl.segs[last]);
        let name = if matches!(pl.segs[last], Seg::Prop(_)) {
            "UNSET_OBJ"
        } else {
            "UNSET_DIM"
        };
        self.push(ZLine::new(name).a(c).b(key));
    }

    fn lower_assign(&mut self, p: u32, val: &Operand, dst: Option<u32>) {
        let pl = self.oa.places[p as usize].clone();
        let v = self.z(val);
        if pl.segs.is_empty() {
            match &pl.base {
                PlaceBase::Cv(c) => self.push(ZLine::new("ASSIGN").ot(dst).a(Z::Cv(*c)).b(v)),
                PlaceBase::StaticProp { class, name } => {
                    let n = self.z(name);
                    let c = self.class_z(class, " (exception)");
                    self.push(ZLine::new("ASSIGN_STATIC_PROP").ot(dst).a(n).b(c));
                    self.push(ZLine::new("OP_DATA").a(v));
                }
                b => {
                    let c = self.base_fetch(b, Mode::W, None, false);
                    self.push(ZLine::new("ASSIGN").ot(dst).a(c).b(v));
                }
            }
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::W);
        let key = self.seg_key(&pl.segs[last]);
        let name = if matches!(pl.segs[last], Seg::Prop(_)) {
            "ASSIGN_OBJ"
        } else {
            "ASSIGN_DIM"
        };
        let c = match (&c, name) {
            (Z::Text(t), "ASSIGN_DIM") if t == "THIS" => {
                let t = self.tmp();
                self.push(ZLine::new("FETCH_THIS").t(t));
                Z::Tmp(t)
            }
            _ => c,
        };
        self.push(ZLine::new(name).ot(dst).a(c).b(key));
        self.push(ZLine::new("OP_DATA").a(v));
    }

    fn lower_assign_op(&mut self, p: u32, op: BinaryOp, val: &Operand, dst: Option<u32>) {
        let pl = self.oa.places[p as usize].clone();
        let v = self.z(val);
        let x = format!(" ({})", arith_name(op));
        if pl.segs.is_empty() {
            match &pl.base {
                PlaceBase::Cv(c) => {
                    self.push(ZLine::new("ASSIGN_OP").ot(dst).x(x).a(Z::Cv(*c)).b(v));
                }
                PlaceBase::StaticProp { class, name } => {
                    let n = self.z(name);
                    let c = self.class_z(class, " (exception)");
                    self.push(ZLine::new("ASSIGN_STATIC_PROP_OP").ot(dst).x(x).a(n).b(c));
                    self.push(ZLine::new("OP_DATA").a(v));
                }
                b => {
                    let c = self.base_fetch(b, Mode::Rw, None, false);
                    self.push(ZLine::new("ASSIGN_OP").ot(dst).x(x).a(c).b(v));
                }
            }
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::Rw);
        let key = self.seg_key(&pl.segs[last]);
        let name = if matches!(pl.segs[last], Seg::Prop(_)) {
            "ASSIGN_OBJ_OP"
        } else {
            "ASSIGN_DIM_OP"
        };
        self.push(ZLine::new(name).ot(dst).x(x).a(c).b(key));
        self.push(ZLine::new("OP_DATA").a(v));
    }

    fn lower_incdec(&mut self, p: u32, kind: IncDecKind, dst: Option<u32>) {
        // `zend_do_free` compiles an unused `$i++` as `++$i`.
        let kind = match (kind, dst) {
            (IncDecKind::PostInc, None) => IncDecKind::PreInc,
            (IncDecKind::PostDec, None) => IncDecKind::PreDec,
            _ => kind,
        };
        let pl = self.oa.places[p as usize].clone();
        if pl.segs.is_empty() {
            match &pl.base {
                PlaceBase::Cv(c) => {
                    self.push(ZLine::new(incdec_name(kind, "")).ot(dst).a(Z::Cv(*c)))
                }
                PlaceBase::StaticProp { class, name } => {
                    let n = self.z(name);
                    let c = self.class_z(class, " (exception)");
                    self.push(ZLine::new(incdec_name(kind, "STATIC")).ot(dst).a(n).b(c));
                }
                b => {
                    let c = self.base_fetch(b, Mode::Rw, None, false);
                    self.push(ZLine::new(incdec_name(kind, "")).ot(dst).a(c));
                }
            }
            return;
        }
        let last = pl.segs.len() - 1;
        let c = self.container(&pl, last, Mode::Rw);
        if let Seg::Prop(n) = &pl.segs[last] {
            let n = self.z(n);
            self.push(ZLine::new(incdec_name(kind, "OBJ")).ot(dst).a(c).b(n));
            return;
        }
        let t = self.tmp();
        self.apply_seg(c, &pl.segs[last], None, Mode::Rw, t);
        self.push(ZLine::new(incdec_name(kind, "")).ot(dst).a(Z::Tmp(t)));
    }

    fn lower_init_fcall(&mut self, i: usize, name: u32, fallback: Option<u32>, display: u32) {
        let lc = self.lit_bytes(name);
        let (n, simple) = self.scan_call(i);
        let line = self.oa.line(i);
        let known = self.res.function(&lc, line);
        let frameless = match known {
            Some(k) if fallback.is_none() && k.user.is_none() && simple && n <= 3 => {
                let ok = crate::compiler::frameless_arities(&lc)
                    .unwrap_or(&[])
                    .iter()
                    .any(|&a| a == n && k.info.required <= n && (!k.info.variadic || a == n));
                ok.then(|| (lc.clone(), n))
            }
            _ => None,
        };
        if frameless.is_some() {
            self.calls.push(Call {
                kind: CallKind::FCall,
                known,
                frameless,
                args: Vec::new(),
                sent: 0,
                inline: None,
            });
            return;
        }
        let init_at = self.lines.len();
        let kind = match (known, fallback) {
            (Some(k), _) => {
                let mut l = ZLine::new("INIT_FCALL")
                    .x(format!(" {n}"))
                    .a(Z::Num(0))
                    .b(self.zs(&lc));
                l.stack = Some(match k.user {
                    Some(u) => StackOf::User(u, n),
                    None => StackOf::Internal(n),
                });
                self.push(l);
                CallKind::FCall
            }
            (None, Some(_)) => {
                let d = Z::Const(self.lit(display));
                self.push(ZLine::new("INIT_NS_FCALL_BY_NAME").x(format!(" {n}")).b(d));
                CallKind::NsByName
            }
            (None, None) => {
                let d = Z::Const(self.lit(display));
                self.push(ZLine::new("INIT_FCALL_BY_NAME").x(format!(" {n}")).b(d));
                CallKind::ByName
            }
        };
        let user = known.is_some_and(|k| k.user.is_some());
        self.calls.push(Call {
            kind,
            known,
            frameless: None,
            args: Vec::new(),
            sent: 0,
            inline: (kind == CallKind::FCall && user).then_some(init_at),
        });
    }

    fn lower_send(&mut self, src: &ArgSrc, name: Option<Value>) {
        let (known, frameless, argn) = match self.calls.last_mut() {
            Some(c) => {
                c.sent += 1;
                (c.known, c.frameless.is_some(), c.sent)
            }
            None => (None, false, 1),
        };
        if frameless {
            let z = match src {
                ArgSrc::Val(o) | ArgSrc::CallResult(o) => self.z(o),
                ArgSrc::Place(p) | ArgSrc::TempPlace(p) => match self.oa.places[*p as usize].base {
                    PlaceBase::Cv(c) => Z::Cv(c),
                    _ => Z::Unused,
                },
            };
            if let Some(c) = self.calls.last_mut() {
                c.args.push(z);
            }
            return;
        }
        let pos = match &name {
            Some(n) => Z::Const(n.clone()),
            None => Z::Num(argn),
        };
        let by_ref = if name.is_some() {
            None
        } else {
            known.map(|k| k.info.sends_by_ref(argn as usize - 1))
        };
        match src {
            ArgSrc::Val(o) => {
                let v = self.z(o);
                let op = if by_ref.is_some() {
                    "SEND_VAL"
                } else {
                    "SEND_VAL_EX"
                };
                self.push(ZLine::new(op).a(v).b(pos));
            }
            ArgSrc::CallResult(o) => {
                let v = self.z(o);
                let op = match by_ref {
                    Some(true) => "SEND_VAR_NO_REF",
                    Some(false) => "SEND_VAL",
                    None => "SEND_VAL_EX",
                };
                self.push(ZLine::new(op).a(v).b(pos));
            }
            ArgSrc::TempPlace(p) => {
                let pl = self.oa.places[*p as usize].clone();
                let t = self.tmp();
                self.read_place(&pl, Mode::R, t);
                let op = if by_ref.is_some() {
                    "SEND_VAL"
                } else {
                    "SEND_VAL_EX"
                };
                self.push(ZLine::new(op).a(Z::Tmp(t)).b(pos));
            }
            ArgSrc::Place(p) => {
                let pl = self.oa.places[*p as usize].clone();
                if let (PlaceBase::Cv(c), true) = (&pl.base, pl.segs.is_empty()) {
                    let op = match by_ref {
                        Some(true) => "SEND_REF",
                        Some(false) => "SEND_VAR",
                        None => "SEND_VAR_EX",
                    };
                    self.push(ZLine::new(op).a(Z::Cv(*c)).b(pos));
                    return;
                }
                match by_ref {
                    Some(true) => {
                        let z = self.place_ref(*p);
                        // The by-reference fetch of the argument
                        // (`ZEND_FETCH_REF`).
                        if let Some(l) = self.lines.last_mut() {
                            if matches!(l.op, "FETCH_OBJ_W" | "FETCH_STATIC_PROP_W")
                                && l.ext.is_empty()
                                && l.res.tmp().is_some()
                                && l.res.tmp() == z.tmp()
                            {
                                l.ext = " (ref)".into();
                            }
                        }
                        self.push(ZLine::new("SEND_REF").a(z).b(pos));
                    }
                    Some(false) => {
                        let t = self.tmp();
                        self.read_place(&pl, Mode::R, t);
                        self.push(ZLine::new("SEND_VAL").a(Z::Tmp(t)).b(pos));
                    }
                    None => {
                        self.push(ZLine::new("CHECK_FUNC_ARG").b(pos.clone()));
                        let t = self.tmp();
                        if pl.segs.is_empty() {
                            let _ = self.base_fetch(&pl.base, Mode::FuncArg, Some(t), false);
                            if let Some(l) = self.lines.last_mut() {
                                if l.op == "FETCH_STATIC_PROP_FUNC_ARG" {
                                    l.ext = " (ref)".into();
                                }
                            }
                        } else {
                            let last = pl.segs.len() - 1;
                            let c = self.container(&pl, last, Mode::FuncArg);
                            self.apply_seg(c, &pl.segs[last], None, Mode::FuncArg, t);
                        }
                        self.push(ZLine::new("SEND_FUNC_ARG").a(Z::Tmp(t)).b(pos));
                    }
                }
            }
        }
    }

    fn lower_do_call(&mut self, dst: Option<u32>) {
        let Some(call) = self.calls.pop() else {
            self.push(ZLine::new("DO_FCALL").ot(dst));
            return;
        };
        if let Some((name, n)) = call.frameless {
            let op = match n {
                0 => "FRAMELESS_ICALL_0",
                1 => "FRAMELESS_ICALL_1",
                2 => "FRAMELESS_ICALL_2",
                _ => "FRAMELESS_ICALL_3",
            };
            let mut args = call.args.into_iter();
            let a = args.next().unwrap_or(Z::Unused);
            let b = args.next().unwrap_or(Z::Unused);
            let mut l = ZLine::new(op).ot(dst).a(a).b(b);
            l.suffix = format!("({})", String::from_utf8_lossy(&name));
            l.ret = call.known.map(|k| k.ret);
            self.push(l);
            if n == 3 {
                let c = args.next().unwrap_or(Z::Unused);
                self.push(ZLine::new("OP_DATA").a(c));
            }
            return;
        }
        if self.try_inline(&call, dst) {
            return;
        }
        // `zend_get_call_op`: deprecated functions, and `#[\NoDiscard]` ones
        // whose result is unused, keep `DO_FCALL_BY_NAME`.
        let by_name = call.known.is_some_and(|k| {
            use crate::func::flags;
            k.flags & flags::DEPRECATED != 0 || (k.flags & flags::NODISCARD != 0 && dst.is_none())
        });
        let op = match (call.kind, call.known) {
            (CallKind::FCall | CallKind::Method | CallKind::Static, Some(_)) if by_name => {
                "DO_FCALL_BY_NAME"
            }
            (CallKind::FCall, Some(k)) => {
                if k.user.is_some() {
                    "DO_UCALL"
                } else {
                    "DO_ICALL"
                }
            }
            (CallKind::ByName | CallKind::NsByName, _) => "DO_FCALL_BY_NAME",
            (CallKind::Method | CallKind::Static, Some(k)) if k.user.is_some() => "DO_UCALL",
            _ => "DO_FCALL",
        };
        let mut l = ZLine::new(op).ot(dst);
        if call.kind != CallKind::New {
            l.ret = call.known.map(|k| k.ret);
        }
        self.push(l);
    }

    /// `zend_try_inline_call`: a call of a function whose code only
    /// returns a literal becomes that literal (`QM_ASSIGN`, which the
    /// later passes propagate into the result's readers); its `INIT_*`
    /// opline goes and the arguments it sent are freed
    /// (`zend_delete_call_instructions`).
    fn try_inline(&mut self, call: &Call, dst: Option<u32>) -> bool {
        let (Some(init), Some(Known { user: Some(u), .. })) = (call.inline, call.known) else {
            return false;
        };
        let static_call = call.kind == CallKind::Static;
        let Some(v) = self
            .res
            .inline_value(u, call.sent, static_call, dst.is_some())
        else {
            return false;
        };
        if self
            .lines
            .get(init)
            .is_none_or(|l| !super::passes::is_init_call(l.op))
        {
            return false;
        }
        // The call's own arguments: the sends outside nested calls. Only
        // positional `SEND_VAL`/`SEND_VAR` are freed by php-src.
        let mut depth = 0u32;
        let mut sends = Vec::new();
        for (k, l) in self.lines.iter().enumerate().skip(init + 1) {
            if super::passes::is_init_call(l.op) {
                depth += 1;
            } else if super::passes::is_do_call(l.op) || l.op.starts_with("CALLABLE_CONVERT") {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && (l.op.starts_with("SEND_") || l.op == "CHECK_FUNC_ARG") {
                if !matches!(l.op, "SEND_VAL" | "SEND_VAR") || !matches!(l.op2, Z::Num(_)) {
                    return false;
                }
                sends.push(k);
            }
        }
        self.lines[init] = ZLine::new("NOP");
        for k in sends {
            let a = std::mem::replace(&mut self.lines[k].op1, Z::Unused);
            self.lines[k] = match a {
                Z::Cv(c) => ZLine::new("CHECK_VAR").a(Z::Cv(c)),
                z @ Z::Tmp(_) => ZLine::new("FREE").a(z),
                _ => ZLine::new("NOP"),
            };
        }
        if let Some(d) = dst {
            self.alias.insert(d, Z::Const(v));
            self.inlined.push(d);
        }
        true
    }

    fn lower_fe_fetch(&mut self, i: usize, iter: u32, end: u32, val: u32, key: Option<u32>) {
        let it = self.iter_tmp(iter);
        let rw = matches!(
            self.ops
                .iter()
                .take(i)
                .rev()
                .map(generic)
                .find(|o| matches!(o, Op::FeReset { iter: x, .. } if *x == iter)),
            Some(Op::FeReset {
                kind: ForeachKind::Ref,
                ..
            })
        );
        let v = match self.op_at(i + 1) {
            Some(Op::AssignCv {
                cv,
                val: Operand::Tmp(t),
                dst: None,
            }) if t == val && !rw => {
                self.skip[i + 1] = true;
                Z::Cv(cv)
            }
            Some(Op::AssignRef {
                place,
                src: Operand::Tmp(t),
                dst: None,
            }) if t == val && rw => {
                let pl = &self.oa.places[place as usize];
                if let (PlaceBase::Cv(c), true) = (&pl.base, pl.segs.is_empty()) {
                    self.skip[i + 1] = true;
                    Z::Cv(*c)
                } else {
                    Z::Tmp(val)
                }
            }
            _ => Z::Tmp(val),
        };
        let name = if rw { "FE_FETCH_RW" } else { "FE_FETCH_R" };
        self.push(ZLine::new(name).ot(key).a(Z::Tmp(it)).b(v).j(Z::Label(end)));
    }
}

/// The generic op a specialised handler (`compiler/specialise.rs`) stands
/// for; any other op unchanged.
pub(crate) fn generic(op: &Op) -> Op {
    match op {
        Op::CmpJmp { op, a, b, dst, .. }
        | Op::BinAssignCv { op, a, b, dst, .. }
        | Op::BinAssignCvCmpJmp { op, a, b, dst, .. }
        | Op::BinAssignCvJmp { op, a, b, dst, .. } => Op::Bin {
            op: *op,
            a: *a,
            b: *b,
            dst: *dst,
        },
        Op::IncDecCv {
            kind, dst, place, ..
        }
        | Op::IncDecDimCv {
            kind, dst, place, ..
        }
        | Op::IncDecObjProp { place, kind, dst }
        | Op::IncDecObjPropDim {
            place, kind, dst, ..
        } => Op::IncDec {
            place: *place,
            kind: *kind,
            dst: *dst,
        },
        Op::IncDecCvCmpJmp { kind, place, .. } => Op::IncDec {
            place: *place,
            kind: *kind,
            dst: None,
        },
        Op::AssignOpCv {
            op,
            val,
            dst,
            place,
            ..
        }
        | Op::AssignOpStaticProp {
            place,
            op,
            val,
            dst,
        }
        | Op::AssignOpObjProp {
            place,
            op,
            val,
            dst,
        }
        | Op::AssignOpObjPropDim {
            place,
            op,
            val,
            dst,
            ..
        } => Op::AssignOp {
            place: *place,
            op: *op,
            val: *val,
            dst: *dst,
        },
        Op::AssignOpDimCv { op, val, place, .. }
        | Op::AssignOpDimsCv { op, val, place, .. }
        | Op::AssignOpCvCmpJmp { op, val, place, .. }
        | Op::AssignOpCvJmp { op, val, place, .. } => Op::AssignOp {
            place: *place,
            op: *op,
            val: *val,
            dst: None,
        },
        Op::ConcatCv {
            val, dst, place, ..
        } => Op::AssignOp {
            place: *place,
            op: BinaryOp::Concat,
            val: *val,
            dst: *dst,
        },
        Op::FetchDimCv {
            place, dst, mode, ..
        }
        | Op::FetchDimsCv {
            place, dst, mode, ..
        }
        | Op::FetchStaticProp {
            place, dst, mode, ..
        }
        | Op::FetchGlobal {
            place, dst, mode, ..
        }
        | Op::FetchObjProp {
            place, dst, mode, ..
        }
        | Op::FetchObjPropDim {
            place, dst, mode, ..
        } => Op::Fetch {
            place: *place,
            mode: *mode,
            dst: *dst,
        },
        Op::IssetDimCv {
            place, dst, empty, ..
        }
        | Op::IssetStaticProp { place, dst, empty }
        | Op::IssetObjProp { place, dst, empty }
        | Op::IssetObjPropDim {
            place, dst, empty, ..
        } => {
            if *empty {
                Op::Empty {
                    place: *place,
                    dst: *dst,
                }
            } else {
                Op::Isset {
                    place: *place,
                    dst: *dst,
                }
            }
        }
        Op::AssignDimCv { val, place, .. } | Op::AssignDimsCv { val, place, .. } => Op::Assign {
            place: *place,
            val: *val,
            dst: None,
        },
        Op::AssignStaticProp { place, val, dst }
        | Op::AssignObjProp { place, val, dst }
        | Op::AssignObjPropDim {
            place, val, dst, ..
        } => Op::Assign {
            place: *place,
            val: *val,
            dst: *dst,
        },
        Op::FetchConstTo {
            name,
            fallback,
            dst,
            ..
        } => Op::FetchConst {
            name: *name,
            fallback: *fallback,
            dst: *dst,
        },
        Op::FetchClassConstTo {
            class, name, dst, ..
        } => Op::FetchClassConst {
            class: class.clone(),
            name: Operand::Lit(*name),
            dst: *dst,
        },
        other => other.clone(),
    }
}
