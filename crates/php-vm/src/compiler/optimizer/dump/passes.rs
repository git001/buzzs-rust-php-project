// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The passes php-src runs after its optimizer's main work, applied to the
//! lowered oplines: the variable contractions of the DFA pass
//! (`zend_dfa_optimize_op_array`, `Zend/Optimizer/dfa_pass.c`) on top of a
//! small type inference, NOP removal, `zend_optimizer_compact_vars`
//! (`compact_vars.c`), `zend_optimize_temporary_variables`
//! (`optimize_temp_vars_5.c`) and `zend_calc_live_ranges`
//! (`Zend/zend_opcode.c`).
#![allow(clippy::too_many_lines)]

use super::model::{ZLine, Z};
use php_runtime::Value;
use std::collections::HashMap;

/// Type bits (`MAY_BE_*`).
pub(crate) mod ty {
    pub const UNDEF: u32 = 1 << 0;
    pub const NULL: u32 = 1 << 1;
    pub const FALSE: u32 = 1 << 2;
    pub const TRUE: u32 = 1 << 3;
    pub const LONG: u32 = 1 << 4;
    pub const DOUBLE: u32 = 1 << 5;
    pub const STRING: u32 = 1 << 6;
    pub const ARRAY: u32 = 1 << 7;
    pub const OBJECT: u32 = 1 << 8;
    pub const RESOURCE: u32 = 1 << 9;
    pub const REF: u32 = 1 << 10;
    pub const BOOL: u32 = FALSE | TRUE;
    pub const ANY: u32 = NULL | BOOL | LONG | DOUBLE | STRING | ARRAY | OBJECT | RESOURCE;
    /// Values that need a destructor (`MAY_HAVE_DTOR`-like).
    pub const REFCOUNTED: u32 = STRING | ARRAY | OBJECT | RESOURCE | REF;
    pub const NUM: u32 = LONG | DOUBLE;
}

/// The type of a literal.
fn const_type(v: &Value) -> u32 {
    match v {
        Value::Null => ty::NULL,
        Value::Bool(false) => ty::FALSE,
        Value::Bool(true) => ty::TRUE,
        Value::Int(_) => ty::LONG,
        Value::Float(_) => ty::DOUBLE,
        Value::Str(_) => ty::STRING,
        Value::Array(_) => ty::ARRAY,
        Value::Object(_) => ty::OBJECT,
        Value::Resource(_) => ty::RESOURCE,
        Value::Reference(_) => ty::ANY | ty::REF,
    }
}

/// The type bits of a declared parameter or return type
/// (`crate::types::mask`); `None` (untyped) is anything.
pub(crate) fn decl_type(t: Option<&crate::types::TypeDecl>) -> u32 {
    use crate::types::mask as m;
    let Some(t) = t else {
        return ty::ANY;
    };
    let mut r = 0;
    for (bit, out) in [
        (m::NULL, ty::NULL),
        (m::FALSE, ty::FALSE),
        (m::TRUE, ty::TRUE),
        (m::LONG, ty::LONG),
        (m::DOUBLE, ty::DOUBLE),
        (m::STRING, ty::STRING),
        (m::ARRAY, ty::ARRAY),
        (m::OBJECT, ty::OBJECT),
        (m::RESOURCE, ty::RESOURCE),
        (m::CALLABLE, ty::STRING | ty::ARRAY | ty::OBJECT),
        (m::ITERABLE, ty::ARRAY | ty::OBJECT),
        (m::STATIC, ty::OBJECT),
        (m::VOID, ty::NULL),
    ] {
        if t.mask & bit != 0 {
            r |= out;
        }
    }
    if !t.classes.is_empty() || !t.intersections.is_empty() {
        r |= ty::OBJECT;
    }
    if r == 0 {
        ty::ANY
    } else {
        r
    }
}

/// `ZEND_TYPE_PURE_MASK` of a declared type: its builtin types only.
pub(crate) fn pure_type(t: &crate::types::TypeDecl) -> u32 {
    let classes = !t.classes.is_empty() || !t.intersections.is_empty();
    let all = decl_type(Some(t));
    if classes && t.mask & crate::types::mask::OBJECT == 0 {
        all & !ty::OBJECT
    } else {
        all
    }
}

/// Opcodes after which control does not fall through.
fn ends_flow(op: &str) -> bool {
    matches!(
        op,
        "JMP"
            | "RETURN"
            | "RETURN_BY_REF"
            | "GENERATOR_RETURN"
            | "THROW"
            | "EXIT"
            | "MATCH_ERROR"
            | "MATCH"
            | "FAST_RET"
    )
}

/// The successors of opline `i`.
fn succs(lines: &[ZLine], i: usize) -> Vec<usize> {
    let mut s: Vec<usize> = lines[i]
        .targets()
        .filter_map(|z| match z {
            Z::ZLabel(t) => Some(*t as usize),
            _ => None,
        })
        .collect();
    if !ends_flow(lines[i].op) && i + 1 < lines.len() {
        s.push(i + 1);
    }
    s
}

/// The static opcode name `ASSIGN_OP` prints as its operator.
fn binop_of_ext(ext: &str) -> Option<&'static str> {
    Some(
        match ext.trim().trim_start_matches('(').trim_end_matches(')') {
            "ADD" => "ADD",
            "SUB" => "SUB",
            "MUL" => "MUL",
            "DIV" => "DIV",
            "MOD" => "MOD",
            "POW" => "POW",
            "SL" => "SL",
            "SR" => "SR",
            "BW_OR" => "BW_OR",
            "BW_AND" => "BW_AND",
            "BW_XOR" => "BW_XOR",
            "CONCAT" => "CONCAT",
            _ => return None,
        },
    )
}

/// The result type of a binary operator.
fn binop_type(op: &str, t1: u32, t2: u32) -> u32 {
    let t1 = if t1 & ty::UNDEF != 0 {
        t1 | ty::NULL
    } else {
        t1
    };
    let t2 = if t2 & ty::UNDEF != 0 {
        t2 | ty::NULL
    } else {
        t2
    };
    if (t1 | t2) & (ty::OBJECT | ty::REF) != 0 {
        return ty::ANY;
    }
    match op {
        "ADD" | "SUB" | "MUL" | "POW" | "DIV" => {
            let mut r = 0;
            if op == "ADD" && t1 & ty::ARRAY != 0 && t2 & ty::ARRAY != 0 {
                r |= ty::ARRAY;
            }
            if (t1 | t2) & !ty::ARRAY != 0 {
                r |= ty::NUM;
            }
            r
        }
        "MOD" | "SL" | "SR" | "SPACESHIP" => ty::LONG,
        "BW_OR" | "BW_AND" | "BW_XOR" => {
            if t1 & ty::STRING != 0 && t2 & ty::STRING != 0 {
                ty::STRING | ty::LONG
            } else {
                ty::LONG
            }
        }
        "CONCAT" | "FAST_CONCAT" => ty::STRING,
        _ => ty::BOOL,
    }
}

/// Opcodes that only read a CV in `op1`/`op2`.
fn reads_only(op: &str) -> bool {
    matches!(
        op,
        "QM_ASSIGN"
            | "ECHO"
            | "RETURN"
            | "GENERATOR_RETURN"
            | "SEND_VAR"
            | "SEND_VAL"
            | "SEND_VAL_EX"
            | "ADD"
            | "SUB"
            | "MUL"
            | "DIV"
            | "MOD"
            | "POW"
            | "SL"
            | "SR"
            | "BW_OR"
            | "BW_AND"
            | "BW_XOR"
            | "BW_NOT"
            | "CONCAT"
            | "FAST_CONCAT"
            | "BOOL_XOR"
            | "BOOL"
            | "BOOL_NOT"
            | "CAST"
            | "IS_EQUAL"
            | "IS_NOT_EQUAL"
            | "IS_IDENTICAL"
            | "IS_NOT_IDENTICAL"
            | "IS_SMALLER"
            | "IS_SMALLER_OR_EQUAL"
            | "SPACESHIP"
            | "TYPE_CHECK"
            | "ISSET_ISEMPTY_CV"
            | "ISSET_ISEMPTY_DIM_OBJ"
            | "ISSET_ISEMPTY_PROP_OBJ"
            | "FETCH_DIM_R"
            | "FETCH_DIM_IS"
            | "FETCH_OBJ_R"
            | "FETCH_OBJ_IS"
            | "FETCH_LIST_R"
            | "JMPZ"
            | "JMPNZ"
            | "JMPZ_EX"
            | "JMPNZ_EX"
            | "JMP_SET"
            | "COALESCE"
            | "JMP_NULL"
            | "INIT_ARRAY"
            | "ADD_ARRAY_UNPACK"
            | "FE_RESET_R"
            | "STRLEN"
            | "COUNT"
            | "INSTANCEOF"
            | "CLONE"
            | "ROPE_INIT"
            | "ROPE_ADD"
            | "ROPE_END"
            | "INIT_METHOD_CALL"
            | "INIT_DYNAMIC_CALL"
            | "INIT_STATIC_METHOD_CALL"
            | "FRAMELESS_ICALL_1"
            | "FRAMELESS_ICALL_2"
            | "FRAMELESS_ICALL_3"
            | "OP_DATA"
            | "IN_ARRAY"
            | "ARRAY_KEY_EXISTS"
            | "GET_TYPE"
            | "GET_CLASS"
            | "CHECK_VAR"
            | "THROW"
            | "MATCH_ERROR"
            | "MATCH"
            | "TYPE_ASSERT"
            | "EXIT"
            | "YIELD"
            | "YIELD_FROM"
            | "FETCH_CLASS"
            | "NEW"
            | "DECLARE_CONST"
            | "ASSIGN_OBJ"
            | "ASSIGN_OBJ_OP"
            | "PRE_INC_OBJ"
            | "PRE_DEC_OBJ"
            | "POST_INC_OBJ"
            | "POST_DEC_OBJ"
            | "FETCH_OBJ_W"
            | "FETCH_OBJ_RW"
            | "UNSET_OBJ"
            | "ISSET_ISEMPTY_STATIC_PROP"
            | "FETCH_STATIC_PROP_R"
            | "FETCH_STATIC_PROP_IS"
            | "ASSIGN_STATIC_PROP"
    )
}

/// The analysis state of one op array.
pub(crate) struct Types {
    /// CV types at the start of each opline.
    pub cv_in: Vec<Vec<u32>>,
    /// The type of each temporary (over all its definitions).
    pub tmp: HashMap<u32, u32>,
}

/// What the analysis needs to know about the function.
pub(crate) struct FuncShape {
    /// Pseudo-main code: CVs may be anything, including references.
    pub is_main: bool,
    /// The number of CVs.
    pub ncv: usize,
    /// Declared parameter types (by parameter number, 0-based).
    pub params: Vec<u32>,
    /// By-reference parameters.
    pub by_ref: Vec<bool>,
    /// CVs bound at entry (closure `use`), and whether by reference.
    pub bound: Vec<(u32, bool)>,
    /// `ZEND_TYPE_PURE_MASK` of the declared return type, if any.
    pub ret: Option<u32>,
}

fn z_type(z: &Z, cv: &[u32], tmp: &HashMap<u32, u32>) -> u32 {
    match z {
        Z::Cv(c) => cv.get(*c as usize).copied().unwrap_or(ty::ANY | ty::REF),
        Z::Tmp(t) => tmp.get(t).copied().unwrap_or(0),
        Z::Const(v) => const_type(v),
        _ => 0,
    }
}

/// The key types of iterators over literal arrays (`FE_RESET_R` of a
/// constant), by iterator temporary.
fn literal_iter_keys(lines: &[ZLine]) -> HashMap<u32, u32> {
    let mut m = HashMap::new();
    for l in lines {
        if let (Some(t), Z::Const(Value::Array(a))) = (l.res.tmp(), &l.op1) {
            let mut k = 0;
            for (key, _) in a.iter() {
                k |= match key {
                    php_runtime::ArrayKey::Int(_) => ty::LONG,
                    php_runtime::ArrayKey::Str(_) => ty::STRING,
                };
            }
            m.insert(t, k | ty::NULL);
        }
    }
    m
}

fn result_type(
    l: &ZLine,
    cv: &[u32],
    tmp: &HashMap<u32, u32>,
    shape: &FuncShape,
    iter_keys: &HashMap<u32, u32>,
) -> u32 {
    let t1 = z_type(&l.op1, cv, tmp);
    let t2 = z_type(&l.op2, cv, tmp);
    match l.op {
        "QM_ASSIGN" | "JMP_SET" | "COALESCE" | "JMP_NULL" => {
            let t = t1 & !ty::REF;
            if t & ty::UNDEF != 0 {
                (t & !ty::UNDEF) | ty::NULL
            } else {
                t
            }
        }
        "ADD" | "SUB" | "MUL" | "DIV" | "MOD" | "POW" | "SL" | "SR" | "BW_OR" | "BW_AND"
        | "BW_XOR" | "CONCAT" | "FAST_CONCAT" | "SPACESHIP" => binop_type(l.op, t1, t2),
        "BW_NOT" => {
            if t1 & ty::STRING != 0 {
                ty::STRING | ty::LONG
            } else {
                ty::LONG
            }
        }
        "ROPE_END" | "ROPE_INIT" | "ROPE_ADD" | "GET_TYPE" | "GET_CLASS" | "GET_CALLED_CLASS"
        | "FETCH_CLASS_NAME" => ty::STRING,
        "CAST" => match l.ext.as_str() {
            " (long)" => ty::LONG,
            " (double)" => ty::DOUBLE,
            " (string)" => ty::STRING,
            " (array)" => ty::ARRAY,
            " (object)" => ty::OBJECT,
            " (null)" => ty::NULL,
            _ => ty::ANY,
        },
        "BOOL"
        | "BOOL_NOT"
        | "BOOL_XOR"
        | "IS_EQUAL"
        | "IS_NOT_EQUAL"
        | "IS_IDENTICAL"
        | "IS_NOT_IDENTICAL"
        | "IS_SMALLER"
        | "IS_SMALLER_OR_EQUAL"
        | "TYPE_CHECK"
        | "ISSET_ISEMPTY_CV"
        | "ISSET_ISEMPTY_DIM_OBJ"
        | "ISSET_ISEMPTY_PROP_OBJ"
        | "ISSET_ISEMPTY_VAR"
        | "ISSET_ISEMPTY_STATIC_PROP"
        | "ISSET_ISEMPTY_THIS"
        | "INSTANCEOF"
        | "JMPZ_EX"
        | "JMPNZ_EX"
        | "DEFINED"
        | "IN_ARRAY"
        | "ARRAY_KEY_EXISTS"
        | "ASSERT_CHECK" => ty::BOOL,
        "STRLEN" | "COUNT" | "FUNC_NUM_ARGS" => ty::LONG,
        "INIT_ARRAY" | "ADD_ARRAY_ELEMENT" | "ADD_ARRAY_UNPACK" | "FUNC_GET_ARGS"
        | "RECV_VARIADIC" => ty::ARRAY,
        "NEW"
        | "CLONE"
        | "DECLARE_LAMBDA_FUNCTION"
        | "FETCH_THIS"
        | "CALLABLE_CONVERT"
        | "CALLABLE_CONVERT_PARTIAL"
        | "CATCH" => ty::OBJECT,
        "FE_FETCH_R" | "FE_FETCH_RW" => l
            .op1
            .tmp()
            .and_then(|t| iter_keys.get(&t).copied())
            .unwrap_or(ty::LONG | ty::STRING | ty::NULL),
        "RECV" | "RECV_INIT" => {
            let n = match l.op1 {
                Z::Num(n) => n as usize,
                _ => 0,
            };
            let mut t = shape
                .params
                .get(n.wrapping_sub(1))
                .copied()
                .unwrap_or(ty::ANY);
            if shape
                .by_ref
                .get(n.wrapping_sub(1))
                .copied()
                .unwrap_or(false)
            {
                t |= ty::ANY | ty::REF;
            }
            if l.op == "RECV_INIT" {
                t |= t2;
            }
            t
        }
        "FRAMELESS_ICALL_1"
        | "FRAMELESS_ICALL_2"
        | "FRAMELESS_ICALL_3"
        | "DO_ICALL"
        | "DO_UCALL"
        | "DO_FCALL"
        | "DO_FCALL_BY_NAME"
        | "FETCH_CLASS_CONSTANT" => l.ret.unwrap_or(ty::ANY),
        "FETCH_DIM_W"
        | "FETCH_DIM_RW"
        | "FETCH_OBJ_W"
        | "FETCH_OBJ_RW"
        | "FETCH_W"
        | "FETCH_RW"
        | "FETCH_DIM_FUNC_ARG"
        | "FETCH_OBJ_FUNC_ARG"
        | "FETCH_STATIC_PROP_W" => ty::ANY | ty::REF,
        _ => ty::ANY,
    }
}

fn set_cv(cv: &mut [u32], c: u32, t: u32) {
    if let Some(s) = cv.get_mut(c as usize) {
        *s = t;
    }
}

fn get_cv(cv: &[u32], c: u32) -> u32 {
    cv.get(c as usize).copied().unwrap_or(ty::ANY | ty::REF)
}

/// Applies opline `l` to the CV types; records the type of a TMP result.
fn transfer(
    l: &ZLine,
    cv: &mut [u32],
    tmp: &mut HashMap<u32, u32>,
    shape: &FuncShape,
    iter_keys: &HashMap<u32, u32>,
) -> bool {
    let mut changed = false;
    let rt = result_type(l, cv, tmp, shape, iter_keys);
    match &l.res {
        Z::Tmp(t) => {
            let e = tmp.entry(*t).or_insert(0);
            if *e | rt != *e {
                *e |= rt;
                changed = true;
            }
        }
        Z::Cv(c) => set_cv(cv, *c, rt | (get_cv(cv, *c) & ty::REF)),
        _ => {}
    }
    if let Z::Cv(c) = l.op1 {
        let old = get_cv(cv, c);
        match l.op {
            "ASSIGN" => {
                let mut t = z_type(&l.op2, cv, tmp) & !ty::REF;
                if t & ty::UNDEF != 0 {
                    t = (t & !ty::UNDEF) | ty::NULL;
                }
                if old & ty::REF != 0 {
                    t = ty::ANY | ty::REF;
                }
                set_cv(cv, c, t);
            }
            "ASSIGN_DIM" | "ASSIGN_DIM_OP" | "FETCH_DIM_W" | "FETCH_DIM_RW" | "FETCH_DIM_UNSET"
            | "FETCH_DIM_FUNC_ARG" | "UNSET_DIM" => {
                set_cv(
                    cv,
                    c,
                    (old & (ty::STRING | ty::OBJECT | ty::REF)) | ty::ARRAY,
                );
            }
            "ASSIGN_OP" => {
                let op = binop_of_ext(&l.ext).unwrap_or("ADD");
                let t = binop_type(op, old, z_type(&l.op2, cv, tmp));
                let r = get_cv(cv, c) & ty::REF;
                set_cv(cv, c, t | r);
            }
            "PRE_INC" | "PRE_DEC" | "POST_INC" | "POST_DEC" => {
                let t = ty::NUM
                    | (old
                        & (ty::NULL
                            | ty::BOOL
                            | ty::STRING
                            | ty::ARRAY
                            | ty::OBJECT
                            | ty::RESOURCE
                            | ty::REF));
                set_cv(cv, c, t);
            }
            "UNSET_CV" => set_cv(cv, c, ty::UNDEF),
            op if reads_only(op) => {}
            _ => set_cv(cv, c, old | ty::ANY | ty::REF),
        }
    }
    if let Z::Cv(c) = l.op2 {
        let old = get_cv(cv, c);
        match l.op {
            "FE_FETCH_R" => set_cv(cv, c, ty::ANY | (old & ty::REF)),
            "FE_FETCH_RW" | "ASSIGN_REF" | "BIND_LEXICAL" => {
                if l.op != "BIND_LEXICAL" || l.ext.contains("ref") {
                    set_cv(cv, c, ty::ANY | ty::REF);
                }
            }
            _ => {}
        }
    }
    changed
}

/// Runs the type inference.
pub(crate) fn infer(lines: &[ZLine], shape: &FuncShape) -> Types {
    let n = lines.len();
    let mut entry = vec![ty::UNDEF; shape.ncv];
    if shape.is_main {
        entry = vec![ty::UNDEF | ty::ANY | ty::REF; shape.ncv];
    }
    for &(c, r) in &shape.bound {
        set_cv(&mut entry, c, if r { ty::ANY | ty::REF } else { ty::ANY });
    }
    let mut cv_in: Vec<Vec<u32>> = vec![vec![0; shape.ncv]; n];
    let iter_keys = literal_iter_keys(lines);
    let mut tmp: HashMap<u32, u32> = HashMap::new();
    if n == 0 {
        return Types { cv_in, tmp };
    }
    cv_in[0] = entry;
    let mut reached = vec![false; n];
    reached[0] = true;
    // Round-robin until nothing changes; the lattice is finite.
    for _ in 0..64 {
        let mut changed = false;
        for i in 0..n {
            if !reached[i] {
                continue;
            }
            let mut st = cv_in[i].clone();
            changed |= transfer(&lines[i], &mut st, &mut tmp, shape, &iter_keys);
            for s in succs(lines, i) {
                if s >= n {
                    continue;
                }
                if !reached[s] {
                    reached[s] = true;
                    changed = true;
                }
                for (dst, src) in cv_in[s].iter_mut().zip(&st) {
                    if *dst | *src != *dst {
                        *dst |= *src;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    Types { cv_in, tmp }
}

fn operands(l: &ZLine) -> [&Z; 3] {
    [&l.op1, &l.op2, &l.res]
}

fn uses_cv(l: &ZLine, c: u32) -> bool {
    operands(l).iter().any(|z| z.cv() == Some(c))
}

/// Whether the DFA pass may run at all on this op array.
pub(crate) fn dfa_allowed(lines: &[ZLine]) -> bool {
    !lines.iter().any(|l| {
        matches!(
            l.op,
            "CATCH"
                | "FAST_CALL"
                | "INCLUDE_OR_EVAL"
                | "FETCH_R"
                | "FETCH_W"
                | "FETCH_RW"
                | "FETCH_IS"
                | "FETCH_UNSET"
                | "FETCH_FUNC_ARG"
                | "ISSET_ISEMPTY_VAR"
                | "UNSET_VAR"
                | "GENERATOR_CREATE"
        ) || (l.op == "INIT_FCALL"
            && matches!(&l.op2, Z::Const(Value::Str(s)) if matches!(
                s.as_bytes(),
                b"compact" | b"extract" | b"get_defined_vars" | b"parse_str" | b"mb_parse_str"
            )))
    })
}

/// `opline_supports_assign_contraction`.
fn supports_contraction(lines: &[ZLine], d: usize, c: u32, src_type: u32) -> bool {
    let l = &lines[d];
    let is_c = |z: &Z| z.cv() == Some(c);
    match l.op {
        "NEW" => false,
        "FRAMELESS_ICALL_3" => {
            !(lines.get(d + 1).is_some_and(|n| is_c(&n.op1)) || is_c(&l.op2) || is_c(&l.op1))
        }
        "FRAMELESS_ICALL_2" => !(is_c(&l.op2) || is_c(&l.op1)),
        "FRAMELESS_ICALL_1" => !is_c(&l.op1),
        "DO_ICALL" | "DO_UCALL" | "DO_FCALL" | "DO_FCALL_BY_NAME" => {
            (src_type & ty::ANY) & !(ty::NULL | ty::BOOL | ty::NUM) == 0
        }
        "POST_INC" | "POST_DEC" => !is_c(&l.op1),
        "INIT_ARRAY" => !is_c(&l.op1) && !is_c(&l.op2),
        "CAST" if l.ext == " (array)" || l.ext == " (object)" => !is_c(&l.op1),
        "ASSIGN_OP" | "ASSIGN_OBJ" | "ASSIGN_DIM" | "ASSIGN_OBJ_OP" | "ASSIGN_DIM_OP" => {
            !is_c(&l.op1)
        }
        _ => true,
    }
}

/// Tries to write the temporary `t` that line `i` assigns to CV `c`
/// straight into the CV at its definition.
fn try_contract(lines: &mut [ZLine], types: &Types, i: usize, c: u32, t: u32) -> bool {
    let defs: Vec<usize> = (0..lines.len())
        .filter(|&k| lines[k].res.tmp() == Some(t) && !lines[k].is_nop())
        .collect();
    let uses: Vec<usize> = (0..lines.len())
        .filter(|&k| {
            !lines[k].is_nop() && (lines[k].op1.tmp() == Some(t) || lines[k].op2.tmp() == Some(t))
        })
        .collect();
    let &[d] = defs.as_slice() else {
        return false;
    };
    if d >= i || uses != [i] {
        return false;
    }
    let st = types.tmp.get(&t).copied().unwrap_or(0);
    if st & ty::REF != 0 || st & (ty::UNDEF | ty::ANY) == 0 {
        return false;
    }
    if !supports_contraction(lines, d, c, st) {
        return false;
    }
    if lines[d + 1..i].iter().any(|l| uses_cv(l, c)) {
        return false;
    }
    lines[d].res = Z::Cv(c);
    lines[d].res_var = false;
    lines[i] = ZLine::new("NOP");
    // `$i = $i + 1` / `$i = $i - 1` become `PRE_INC` / `PRE_DEC`.
    let before = get_cv(&types.cv_in[d], c);
    let one = |z: &Z| z.const_int() == Some(1);
    let l = &mut lines[d];
    if l.op == "SUB"
        && l.op1.cv() == Some(c)
        && one(&l.op2)
        && before
            & (ty::UNDEF
                | ty::NULL
                | ty::BOOL
                | ty::STRING
                | ty::ARRAY
                | ty::OBJECT
                | ty::RESOURCE
                | ty::REF)
            == 0
    {
        l.op = "PRE_DEC";
        l.op2 = Z::Unused;
        l.res = Z::Unused;
    } else if l.op == "ADD"
        && before & (ty::BOOL | ty::STRING | ty::ARRAY | ty::OBJECT | ty::RESOURCE | ty::REF) == 0
    {
        if l.op1.cv() == Some(c) && one(&l.op2) {
            l.op = "PRE_INC";
            l.op2 = Z::Unused;
            l.res = Z::Unused;
        } else if l.op2.cv() == Some(c) && one(&l.op1) {
            l.op = "PRE_INC";
            l.op1 = Z::Cv(c);
            l.op2 = Z::Unused;
            l.res = Z::Unused;
        }
    }
    true
}

/// The CV-assignment rewrites of `zend_dfa_optimize_op_array`.
pub(crate) fn dfa(lines: &mut [ZLine], types: &Types, shape: &FuncShape) {
    for i in 0..lines.len() {
        let l = lines[i].clone();
        match l.op {
            // `can_elide_return_type_check`: the returned value already
            // has one of the declared builtin types.
            "VERIFY_RETURN_TYPE" if matches!(l.res, Z::Unused) => {
                let Some(allowed) = shape.ret else { continue };
                let t = match &l.op1 {
                    Z::Tmp(t) => types.tmp.get(t).copied().unwrap_or(0),
                    Z::Cv(c) => get_cv(&types.cv_in[i], *c),
                    _ => continue,
                };
                if t == 0 || t & ty::REF != 0 {
                    continue;
                }
                let t = if t & ty::UNDEF != 0 {
                    (t & !ty::UNDEF) | ty::NULL
                } else {
                    t
                };
                let returned = lines[i + 1..]
                    .iter()
                    .find(|n| !n.is_nop())
                    .is_some_and(|n| {
                        n.op == "RETURN"
                            && match (&n.op1, &l.op1) {
                                (Z::Tmp(a), Z::Tmp(b)) | (Z::Cv(a), Z::Cv(b)) => a == b,
                                _ => false,
                            }
                    });
                if returned && t & !allowed == 0 {
                    lines[i] = ZLine::new("NOP");
                }
            }
            "QM_ASSIGN" => {
                if let (Z::Cv(c), Z::Tmp(t)) = (&l.res, &l.op1) {
                    let new = types.tmp.get(t).copied().unwrap_or(ty::ANY);
                    if new & ty::REFCOUNTED == 0 {
                        try_contract(lines, types, i, *c, *t);
                    }
                }
            }
            "ADD"
            | "SUB"
            | "MUL"
            | "IS_EQUAL"
            | "IS_NOT_EQUAL"
            | "IS_SMALLER"
            | "IS_SMALLER_OR_EQUAL"
                if !matches!(l.res, Z::Unused) =>
            {
                arith_identity(&mut lines[i], types, i);
            }
            "ASSIGN" if matches!(l.res, Z::Unused) => {
                let Z::Cv(c) = l.op1 else { continue };
                let orig = get_cv(&types.cv_in[i], c);
                if orig & ty::REFCOUNTED != 0 {
                    continue;
                }
                if let Z::Tmp(t) = l.op2 {
                    if try_contract(lines, types, i, c, t) {
                        continue;
                    }
                }
                if matches!(l.op2, Z::Const(_) | Z::Cv(_) | Z::Tmp(_)) {
                    let m = &mut lines[i];
                    m.op = "QM_ASSIGN";
                    m.res = Z::Cv(c);
                    m.res_var = false;
                    m.op1 = l.op2.clone();
                    m.op2 = Z::Unused;
                }
            }
            "ASSIGN_OP" => {
                let Z::Cv(c) = l.op1 else { continue };
                let orig = get_cv(&types.cv_in[i], c);
                let op = binop_of_ext(&l.ext);
                let one = l.op2.const_int() == Some(1);
                let m = &mut lines[i];
                if op == Some("ADD")
                    && one
                    && orig
                        & (ty::BOOL | ty::STRING | ty::ARRAY | ty::OBJECT | ty::RESOURCE | ty::REF)
                        == 0
                {
                    m.op = "PRE_INC";
                    m.ext.clear();
                    m.op2 = Z::Unused;
                } else if op == Some("SUB")
                    && one
                    && orig
                        & (ty::UNDEF
                            | ty::NULL
                            | ty::BOOL
                            | ty::STRING
                            | ty::ARRAY
                            | ty::OBJECT
                            | ty::RESOURCE
                            | ty::REF)
                        == 0
                {
                    m.op = "PRE_DEC";
                    m.ext.clear();
                    m.op2 = Z::Unused;
                } else if matches!(l.res, Z::Unused) && orig & ty::REFCOUNTED == 0 {
                    if let Some(name) = op.filter(|o| *o != "CONCAT") {
                        m.op = name;
                        m.ext.clear();
                        m.res = Z::Cv(c);
                        m.res_var = false;
                    }
                }
            }
            _ => {}
        }
    }
}

/// `zend_dfa_optimize_op_array`'s arithmetic rewrites of one operand
/// against a literal: `ADD x, 0` (and `SUB x, 0`, `ADD 0, x`) of a value
/// that is only a long (or only a double, against `0.0`) becomes
/// `QM_ASSIGN x`, and `MUL x, 2` of a number becomes `ADD x, x`. A long
/// literal against a value that is only a double (also of the
/// comparisons) is made a double literal first.
#[allow(clippy::float_cmp, clippy::cast_precision_loss)]
fn arith_identity(l: &mut ZLine, types: &Types, i: usize) {
    const NOT_NUM: u32 = (ty::ANY | ty::UNDEF) & !(ty::LONG | ty::DOUBLE);
    let cv = &types.cv_in[i];
    let (x, lit, lit_first) = match (&l.op1, &l.op2) {
        (Z::Const(_), Z::Const(_)) => return,
        (Z::Const(c), x) => (x.clone(), c.clone(), true),
        (x, Z::Const(c)) => (x.clone(), c.clone(), false),
        _ => return,
    };
    if !matches!(x, Z::Cv(_) | Z::Tmp(_)) {
        return;
    }
    let t = z_type(&x, cv, &types.tmp);
    if t == 0 {
        return;
    }
    let exact = t & (ty::ANY | ty::UNDEF);
    // `ADD long, x [double]`: the literal is converted to a double first.
    let lit = match lit {
        Value::Int(n) if t & ty::ANY == ty::DOUBLE => {
            let d = Value::Float(n as f64);
            if lit_first {
                l.op1 = Z::Const(d.clone());
            } else {
                l.op2 = Z::Const(d.clone());
            }
            d
        }
        v => v,
    };
    let zero = match lit {
        Value::Int(0) => exact == ty::LONG,
        Value::Float(f) => f == 0.0 && exact == ty::DOUBLE,
        _ => false,
    };
    let add_like = l.op == "ADD" || (l.op == "SUB" && !lit_first);
    if add_like && zero {
        l.op = "QM_ASSIGN";
        l.op1 = x;
        l.op2 = Z::Unused;
        return;
    }
    let two = match lit {
        Value::Int(2) => true,
        Value::Float(f) => f == 2.0 && t & ty::LONG == 0,
        _ => false,
    };
    if l.op == "MUL" && t & NOT_NUM == 0 && two {
        l.op = "ADD";
        l.op1 = x.clone();
        l.op2 = x;
    }
}

/// Resolves `Label`s through `first_z` into `ZLabel`s.
pub(crate) fn resolve_labels(lines: &mut [ZLine], first_z: &[u32]) {
    let map = |z: &mut Z| {
        if let Z::Label(l) = *z {
            let at = first_z
                .get(l as usize)
                .or_else(|| first_z.last())
                .copied()
                .unwrap_or(0);
            *z = Z::ZLabel(at);
        }
    };
    for l in lines.iter_mut() {
        for z in l.label_slots_mut() {
            map(z);
        }
    }
}

/// Removes NOPs, retargeting jumps to the next remaining opline; also
/// returns the new index of each old one.
pub(crate) fn remove_nops(lines: Vec<ZLine>) -> (Vec<ZLine>, Vec<u32>) {
    let n = lines.len();
    let mut new_idx = vec![0u32; n + 1];
    let mut k = 0u32;
    for (i, l) in lines.iter().enumerate() {
        new_idx[i] = k;
        if !l.is_nop() {
            k += 1;
        }
    }
    new_idx[n] = k;
    let map = |z: &mut Z| {
        if let Z::ZLabel(t) = *z {
            *z = Z::ZLabel(new_idx[(t as usize).min(n)]);
        }
    };
    let mut out: Vec<ZLine> = lines.into_iter().filter(|l| !l.is_nop()).collect();
    for l in &mut out {
        for z in l.label_slots_mut() {
            map(z);
        }
    }
    (out, new_idx)
}

/// `zend_optimizer_compact_vars`: the CVs still used, renumbered in order.
pub(crate) fn compact_cvs(lines: &mut [ZLine], ncv: usize) -> Vec<u32> {
    let mut used = vec![false; ncv];
    for l in lines.iter() {
        for z in operands(l) {
            if let Some(c) = z.cv() {
                if let Some(u) = used.get_mut(c as usize) {
                    *u = true;
                }
            }
        }
    }
    let mut map = vec![u32::MAX; ncv];
    let mut kept = Vec::new();
    for (c, u) in used.iter().enumerate() {
        if *u {
            map[c] = u32::try_from(kept.len()).unwrap_or(0);
            kept.push(u32::try_from(c).unwrap_or(0));
        }
    }
    let remap = |z: &mut Z| {
        if let Z::Cv(c) = *z {
            if let Some(&m) = map.get(c as usize) {
                *z = Z::Cv(m);
            }
        }
    };
    for l in lines.iter_mut() {
        remap(&mut l.op1);
        remap(&mut l.op2);
        remap(&mut l.res);
    }
    kept
}

/// The number of zval slots a rope of `n` strings takes.
fn rope_slots(n: u32) -> u32 {
    (n * 8).div_ceil(16)
}

/// `zend_optimize_temporary_variables`: renumbers the temporaries in
/// place (from 0) and returns `op_array->T`.
pub(crate) fn compact_tmps(lines: &mut [ZLine], has_finally: bool) -> u32 {
    let total = lines
        .iter()
        .flat_map(|l| operands(l).into_iter().filter_map(Z::tmp))
        .max()
        .map_or(0, |m| m as usize + 1);
    if total == 0 {
        return 0;
    }
    let mut start_of = vec![usize::MAX; total];
    for (i, l) in lines.iter().enumerate().rev() {
        if let Some(t) = l.res.tmp() {
            start_of[t as usize] = i;
        }
    }
    let mut taken: Vec<bool> = vec![false; total * 2 + 8];
    let mut map: Vec<Option<u32>> = vec![None; total];
    let mut max: i64 = -1;
    let get_available = |taken: &mut Vec<bool>, max: &mut i64| -> u32 {
        let i = taken.iter().position(|x| !x).unwrap_or(taken.len());
        if i >= taken.len() {
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: temporaries of the compiled function"
            )]
            taken.resize(i + 1, false);
        }
        taken[i] = true;
        let i64v = i64::try_from(i).unwrap_or(0);
        if i64v > *max {
            *max = i64v;
        }
        u32::try_from(i).unwrap_or(0)
    };
    for i in (0..lines.len()).rev() {
        if let Some(t) = lines[i].op1.tmp() {
            let t = t as usize;
            if lines[i].op == "ROPE_END" {
                let num = rope_slots(lines[i].xv + 1) as usize;
                let mut var = max;
                while var >= 0 && !taken[usize::try_from(var).unwrap_or(0)] {
                    var -= 1;
                }
                let n = i64::try_from(num).unwrap_or(1);
                max = max.max(var + n);
                let var = usize::try_from(var + 1).unwrap_or(0);
                if taken.len() < var + num + 1 {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: temporaries of the compiled function"
                    )]
                    taken.resize(var + num + 1, false);
                }
                map[t] = u32::try_from(var).ok();
                for k in 0..num {
                    taken[var + k] = true;
                }
            } else if map[t].is_none() {
                let mut fresh = false;
                if has_finally
                    && matches!(
                        lines[i].op,
                        "RETURN" | "GENERATOR_RETURN" | "RETURN_BY_REF" | "FREE" | "FE_FREE"
                    )
                {
                    for prev in lines[..i].iter().rev() {
                        if prev.op == "FAST_CALL" {
                            fresh = true;
                            break;
                        }
                        if !matches!(
                            prev.op,
                            "FREE" | "FE_FREE" | "VERIFY_RETURN_TYPE" | "DISCARD_EXCEPTION"
                        ) {
                            break;
                        }
                    }
                }
                let v = if fresh {
                    max += 1;
                    let v = usize::try_from(max).unwrap_or(0);
                    if taken.len() <= v {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "bounded: temporaries of the compiled function"
                        )]
                        taken.resize(v + 1, false);
                    }
                    taken[v] = true;
                    u32::try_from(v).unwrap_or(0)
                } else {
                    get_available(&mut taken, &mut max)
                };
                map[t] = Some(v);
            }
            let m = map[t].unwrap_or(0);
            lines[i].op1 = Z::Tmp(m);
        }
        if let Some(t) = lines[i].op2.tmp() {
            let t = t as usize;
            if map[t].is_none() {
                map[t] = Some(get_available(&mut taken, &mut max));
            }
            lines[i].op2 = Z::Tmp(map[t].unwrap_or(0));
        }
        if let Some(t) = lines[i].res.tmp() {
            let t = t as usize;
            if map[t].is_none() {
                map[t] = Some(get_available(&mut taken, &mut max));
            }
            let m = map[t].unwrap_or(0);
            lines[i].res = Z::Tmp(m);
            if start_of[t] == i {
                if lines[i].op != "FAST_CALL" {
                    taken[m as usize] = false;
                }
                if lines[i].op == "ROPE_INIT" {
                    let num = rope_slots(lines[i].xv);
                    for k in 1..num {
                        if let Some(x) = taken.get_mut((m + k) as usize) {
                            *x = false;
                        }
                    }
                }
            }
        }
    }
    u32::try_from(max + 1).unwrap_or(0)
}

/// Whether `zend_compile` gives `op` a result temporary that `zend_do_free`
/// drops when the result is unused (`zend_op_may_elide_result`, plus
/// `ASSIGN_REF`'s `IS_VAR` result): the number stays allocated.
fn elided_result(op: &str) -> bool {
    matches!(
        op,
        "ASSIGN"
            | "ASSIGN_DIM"
            | "ASSIGN_OBJ"
            | "ASSIGN_STATIC_PROP"
            | "ASSIGN_OP"
            | "ASSIGN_DIM_OP"
            | "ASSIGN_OBJ_OP"
            | "ASSIGN_STATIC_PROP_OP"
            | "ASSIGN_REF"
            | "ASSIGN_OBJ_REF"
            | "ASSIGN_STATIC_PROP_REF"
            | "PRE_INC_STATIC_PROP"
            | "PRE_DEC_STATIC_PROP"
            | "PRE_INC_OBJ"
            | "PRE_DEC_OBJ"
            | "PRE_INC"
            | "PRE_DEC"
            | "DO_FCALL"
            | "DO_ICALL"
            | "DO_UCALL"
            | "DO_FCALL_BY_NAME"
            | "YIELD"
            | "YIELD_FROM"
            | "INCLUDE_OR_EVAL"
    )
}

/// Without pass 9 the temporaries keep the numbers `zend_compile` gave
/// them, in order of definition, including the results it allocated and
/// then dropped as unused (`zend_do_free`); returns `op_array->T`.
pub(crate) fn number_tmps(lines: &mut [ZLine]) -> u32 {
    let mut map: HashMap<u32, u32> = HashMap::new();
    let mut next: u32 = 0;
    for l in lines.iter() {
        for z in [&l.res, &l.op1, &l.op2] {
            if let Some(t) = z.tmp() {
                map.entry(t).or_insert_with(|| {
                    next += 1;
                    next - 1
                });
            }
        }
        if matches!(l.res, Z::Unused) && elided_result(l.op) {
            next += 1;
        }
    }
    let fix = |z: &mut Z| {
        if let Z::Tmp(t) = z {
            if let Some(&n) = map.get(t) {
                *t = n;
            }
        }
    };
    for l in lines.iter_mut() {
        fix(&mut l.res);
        fix(&mut l.op1);
        fix(&mut l.op2);
    }
    next
}

/// One `zend_live_range`.
pub(crate) struct LiveRange {
    /// The temporary (already renumbered).
    pub var: u32,
    /// `ZEND_LIVE_*` kind as printed.
    pub kind: &'static str,
    /// First opline.
    pub start: u32,
    /// The opline that ends it.
    pub end: u32,
}

fn is_fake_def(op: &str) -> bool {
    matches!(op, "ROPE_ADD" | "ADD_ARRAY_ELEMENT" | "ADD_ARRAY_UNPACK")
}

fn keeps_op1_alive(op: &str) -> bool {
    matches!(
        op,
        "CASE"
            | "CASE_STRICT"
            | "SWITCH_LONG"
            | "SWITCH_STRING"
            | "MATCH"
            | "MATCH_ERROR"
            | "FETCH_LIST_R"
            | "FETCH_LIST_W"
            | "COPY_TMP"
            | "EXT_STMT"
    )
}

pub(crate) fn is_init_call(op: &str) -> bool {
    matches!(
        op,
        "INIT_FCALL"
            | "INIT_FCALL_BY_NAME"
            | "INIT_NS_FCALL_BY_NAME"
            | "INIT_DYNAMIC_CALL"
            | "INIT_USER_CALL"
            | "INIT_METHOD_CALL"
            | "INIT_STATIC_METHOD_CALL"
            | "INIT_PARENT_PROPERTY_HOOK_CALL"
            | "NEW"
    )
}

pub(crate) fn is_do_call(op: &str) -> bool {
    matches!(
        op,
        "DO_FCALL" | "DO_FCALL_BY_NAME" | "DO_ICALL" | "DO_UCALL"
    )
}

/// `emit_live_range` (with the optimizer's `needs_live_range` check).
fn emit_live_range(
    lines: &[ZLine],
    out: &mut Vec<LiveRange>,
    var: u32,
    start: usize,
    end: usize,
    needs: &dyn Fn(usize) -> bool,
) {
    let def = lines[start].op;
    let (kind, start) = match def {
        "ADD_ARRAY_ELEMENT" | "ADD_ARRAY_UNPACK" | "ROPE_ADD" | "JMPZ_EX" | "JMPNZ_EX" | "BOOL"
        | "BOOL_NOT" | "FETCH_CLASS" | "DECLARE_ANON_CLASS" | "FAST_CALL" => return,
        "BEGIN_SILENCE" => ("silence", start + 1),
        "ROPE_INIT" => ("rope", start),
        "FE_RESET_R" | "FE_RESET_RW" => ("loop", start + 1),
        "NEW" => {
            let orig = start;
            let mut s = start;
            let mut level = 0;
            while s + 1 < end {
                s += 1;
                let op = lines[s].op;
                if is_init_call(op) {
                    level += 1;
                } else if is_do_call(op) {
                    if level == 0 {
                        break;
                    }
                    level -= 1;
                }
            }
            out.push(LiveRange {
                var,
                kind: "new",
                start: u32::try_from(orig + 1).unwrap_or(0),
                end: u32::try_from(s + 1).unwrap_or(0),
            });
            if s + 1 == end {
                return;
            }
            if !needs(orig) {
                return;
            }
            ("tmp/var", s + 1)
        }
        _ => {
            if !needs(start) {
                return;
            }
            ("tmp/var", start + 1)
        }
    };
    out.push(LiveRange {
        var,
        kind,
        start: u32::try_from(start).unwrap_or(0),
        end: u32::try_from(end).unwrap_or(0),
    });
}

/// `zend_calc_live_ranges`; also fills in `FE_FREE`'s `loop-end(+N)`.
pub(crate) fn live_ranges(
    lines: &mut [ZLine],
    ntmps: u32,
    needs: &dyn Fn(usize) -> bool,
) -> Vec<LiveRange> {
    let mut last_use: Vec<Option<usize>> = vec![None; ntmps as usize + 1];
    let mut out = Vec::new();
    let mut loop_end: Vec<(usize, u32)> = Vec::new();
    for opnum in (0..lines.len()).rev() {
        let l = &lines[opnum];
        if let Some(t) = l.res.tmp() {
            let t = t as usize;
            if !is_fake_def(l.op) {
                if let Some(u) = last_use.get(t).copied().flatten() {
                    if opnum + 1 != u {
                        emit_live_range(lines, &mut out, t as u32, opnum, u, needs);
                    }
                    last_use[t] = None;
                }
            }
        }
        if let Some(t) = l.op1.tmp() {
            let t = t as usize;
            if t < last_use.len() {
                match last_use[t] {
                    None => {
                        if !keeps_op1_alive(l.op) {
                            last_use[t] = Some(opnum - usize::from(l.op == "OP_DATA"));
                        }
                    }
                    Some(u) if matches!(l.op, "FREE" | "FE_FREE") && l.free_on_return => {
                        let mut jump = 1;
                        while lines
                            .get(opnum + jump)
                            .is_some_and(|n| matches!(n.op, "FREE" | "FE_FREE") && n.free_on_return)
                        {
                            jump += 1;
                        }
                        if lines
                            .get(opnum + jump)
                            .is_some_and(|n| n.op == "VERIFY_RETURN_TYPE")
                        {
                            jump += 1;
                        }
                        if opnum + jump + 1 != u {
                            out.push(LiveRange {
                                var: t as u32,
                                kind: if l.op == "FE_FREE" { "loop" } else { "tmp/var" },
                                start: u32::try_from(opnum + jump + 1).unwrap_or(0),
                                end: u32::try_from(u).unwrap_or(0),
                            });
                        }
                        last_use[t] = Some(opnum);
                        loop_end.push((opnum, u32::try_from(u - opnum).unwrap_or(0)));
                    }
                    Some(_) => {}
                }
            }
        }
        if let Some(t) = l.op2.tmp() {
            let t = t as usize;
            if t < last_use.len() {
                if matches!(l.op, "FE_FETCH_R" | "FE_FETCH_RW") {
                    if let Some(u) = last_use[t] {
                        if opnum + 1 != u {
                            emit_live_range(lines, &mut out, t as u32, opnum, u, needs);
                        }
                        last_use[t] = None;
                    }
                } else if last_use[t].is_none() {
                    last_use[t] = Some(opnum);
                }
            }
        }
    }
    for (at, n) in loop_end {
        lines[at].op2 = Z::Text(format!("loop-end(+{n})"));
    }
    out.reverse();
    out.sort_by_key(|r| r.start);
    out
}
