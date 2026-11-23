// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Pass 1, 2, 4: Local optimizations, strength reduction, and function call optimization:
//! - Pass 1 (ZEND_OPTIMIZER_PASS_1): Local constant folding, constant branches, echo folding.
//! - Pass 2 (ZEND_OPTIMIZER_PASS_2): Constant conversion and typecheck strength reduction.
//! - Pass 4 (ZEND_OPTIMIZER_PASS_4): Function call optimization on pure built-ins with constant arguments.

use super::{add_or_find_literal, PASS_2, PASS_4};
use crate::opcode::{type_mask, ArgSrc, BinKind, CastKind, Op, OpArray, Operand, UnKind};
use php_runtime::error::CollectingContext;
use php_runtime::{BinaryOp, Value, ZStr};

/// Runs Pass 1 (and Pass 2) on `op_array`. Returns `true` if any instruction was changed.
pub(crate) fn run_pass1(op_array: &mut OpArray, level: u32) -> bool {
    let mut changed = false;
    let len = op_array.ops.len();

    for i in 0..len {
        let op = &op_array.ops[i];
        let mut replacement = None;

        match op {
            Op::Bin {
                op: bin_op,
                a,
                b,
                dst,
            } => {
                if let (Operand::Lit(la), Operand::Lit(lb)) = (a, b) {
                    let va = &op_array.literals[*la as usize];
                    let vb = &op_array.literals[*lb as usize];
                    if let Some(val) = fold_bin_op(*bin_op, va, vb) {
                        let lit = add_or_find_literal(&mut op_array.literals, val);
                        replacement = Some(Op::Move {
                            src: Operand::Lit(lit),
                            dst: *dst,
                        });
                    }
                } else if *bin_op == BinKind::Arith(BinaryOp::Concat) {
                    // String concat identity: "" . $b -> (string)$b, $a . "" -> (string)$a
                    if let Operand::Lit(la) = a {
                        if let Some(Value::Str(s)) = op_array.literals.get(*la as usize) {
                            if s.is_empty() {
                                replacement = Some(Op::Cast {
                                    kind: CastKind::String,
                                    a: *b,
                                    dst: *dst,
                                });
                            }
                        }
                    } else if let Operand::Lit(lb) = b {
                        if let Some(Value::Str(s)) = op_array.literals.get(*lb as usize) {
                            if s.is_empty() {
                                replacement = Some(Op::Cast {
                                    kind: CastKind::String,
                                    a: *a,
                                    dst: *dst,
                                });
                            }
                        }
                    }
                } else if (level & PASS_2) != 0 {
                    // Strength reduction: $x === null -> TypeCheck(NULL)
                    if *bin_op == BinKind::Identical {
                        if let Operand::Lit(lb) = b {
                            if matches!(op_array.literals.get(*lb as usize), Some(Value::Null)) {
                                replacement = Some(Op::TypeCheck {
                                    a: *a,
                                    mask: type_mask::NULL,
                                    dst: Some(*dst),
                                });
                            }
                        } else if let Operand::Lit(la) = a {
                            if matches!(op_array.literals.get(*la as usize), Some(Value::Null)) {
                                replacement = Some(Op::TypeCheck {
                                    a: *b,
                                    mask: type_mask::NULL,
                                    dst: Some(*dst),
                                });
                            }
                        }
                    }
                }
            }

            Op::Unary { op: un_op, a, dst } => {
                if let Operand::Lit(la) = a {
                    let va = &op_array.literals[*la as usize];
                    if let Some(val) = fold_unary_op(*un_op, va) {
                        let lit = add_or_find_literal(&mut op_array.literals, val);
                        replacement = Some(Op::Move {
                            src: Operand::Lit(lit),
                            dst: *dst,
                        });
                    }
                }
            }

            Op::Cast { kind, a, dst } => {
                if (level & PASS_2) != 0 && *kind == CastKind::Bool {
                    replacement = Some(Op::Unary {
                        op: UnKind::Bool,
                        a: *a,
                        dst: *dst,
                    });
                } else if let Operand::Lit(la) = a {
                    let va = &op_array.literals[*la as usize];
                    if let Some(val) = fold_cast(*kind, va) {
                        let lit = add_or_find_literal(&mut op_array.literals, val);
                        replacement = Some(Op::Move {
                            src: Operand::Lit(lit),
                            dst: *dst,
                        });
                    }
                }
            }

            Op::Strlen { a, dst } => {
                if let Operand::Lit(la) = a {
                    let len = if let Some(Value::Str(s)) = op_array.literals.get(*la as usize) {
                        Some(s.len() as i64)
                    } else {
                        None
                    };
                    if let Some(len) = len {
                        let lit = add_or_find_literal(&mut op_array.literals, Value::Int(len));
                        replacement = Some(match dst {
                            Some(d) => Op::Move {
                                src: Operand::Lit(lit),
                                dst: *d,
                            },
                            None => Op::Nop,
                        });
                    }
                }
            }

            Op::Count { a, dst, .. } => {
                if let Operand::Lit(la) = a {
                    let count_opt = match op_array.literals.get(*la as usize) {
                        Some(Value::Array(arr)) => Some(arr.len() as i64),
                        _ => None,
                    };
                    if let Some(count) = count_opt {
                        let lit = add_or_find_literal(&mut op_array.literals, Value::Int(count));
                        replacement = Some(match dst {
                            Some(d) => Op::Move {
                                src: Operand::Lit(lit),
                                dst: *d,
                            },
                            None => Op::Nop,
                        });
                    }
                }
            }

            Op::TypeCheck { a, mask, dst } => {
                if let Operand::Lit(la) = a {
                    if let Some(va) = op_array.literals.get(*la as usize) {
                        let res = type_mask::matches(*mask, va);
                        let lit = add_or_find_literal(&mut op_array.literals, Value::Bool(res));
                        replacement = Some(match dst {
                            Some(d) => Op::Move {
                                src: Operand::Lit(lit),
                                dst: *d,
                            },
                            None => Op::Nop,
                        });
                    }
                }
            }

            Op::JmpZ(cond, target) => {
                if let Operand::Lit(lc) = cond {
                    if let Some(vc) = op_array.literals.get(*lc as usize) {
                        if let Some(truthy) = crate::compiler::expr::const_truthiness(vc) {
                            if !truthy {
                                // Falsy condition: branch is unconditionally taken
                                replacement = Some(Op::Jmp(*target));
                            } else {
                                // Truthy condition: branch is never taken
                                replacement = Some(Op::Nop);
                            }
                        }
                    }
                }
            }

            Op::JmpNz(cond, target) => {
                if let Operand::Lit(lc) = cond {
                    if let Some(vc) = op_array.literals.get(*lc as usize) {
                        if let Some(truthy) = crate::compiler::expr::const_truthiness(vc) {
                            if truthy {
                                // Truthy condition: branch is unconditionally taken
                                replacement = Some(Op::Jmp(*target));
                            } else {
                                // Falsy condition: branch is never taken
                                replacement = Some(Op::Nop);
                            }
                        }
                    }
                }
            }

            Op::JmpZEx {
                a,
                dst: _,
                target: _,
            } => {
                if let Operand::Lit(la) = a {
                    if let Some(va) = op_array.literals.get(*la as usize) {
                        if crate::compiler::expr::const_truthiness(va) == Some(true) {
                            // Truthy: never taken and dst is not modified; falls through
                            replacement = Some(Op::Nop);
                        }
                    }
                }
            }

            Op::JmpNzEx {
                a,
                dst: _,
                target: _,
            } => {
                if let Operand::Lit(la) = a {
                    if let Some(va) = op_array.literals.get(*la as usize) {
                        if crate::compiler::expr::const_truthiness(va) == Some(false) {
                            // Falsy: never taken and dst is not modified; falls through
                            replacement = Some(Op::Nop);
                        }
                    }
                }
            }

            Op::Echo(op) => {
                if let Operand::Lit(l) = op {
                    enum EchoAction {
                        Nop,
                        Preconvert(Value),
                    }
                    let action = match op_array.literals.get(*l as usize) {
                        Some(Value::Str(s)) if s.is_empty() => Some(EchoAction::Nop),
                        Some(Value::Null) => Some(EchoAction::Nop),
                        Some(Value::Bool(b)) => {
                            Some(EchoAction::Preconvert(Value::Str(ZStr::from(if *b {
                                "1"
                            } else {
                                ""
                            }))))
                        }
                        Some(Value::Int(n)) => Some(EchoAction::Preconvert(Value::Str(
                            ZStr::from(n.to_string()),
                        ))),
                        _ => None,
                    };
                    match action {
                        Some(EchoAction::Nop) => replacement = Some(Op::Nop),
                        Some(EchoAction::Preconvert(v)) => {
                            let lit = add_or_find_literal(&mut op_array.literals, v);
                            replacement = Some(Op::Echo(Operand::Lit(lit)));
                        }
                        None => {}
                    }
                }
            }

            _ => {}
        }

        if let Some(new_op) = replacement {
            op_array.ops[i] = new_op;
            changed = true;
        }
    }

    if (level & PASS_2) != 0 {
        changed |= run_pass2(op_array);
    }
    if (level & PASS_4) != 0 {
        changed |= run_pass4(op_array);
    }

    changed
}

/// Runs Pass 2: Constant conversion and typecheck strength reduction (ZEND_OPTIMIZER_PASS_2).
///
/// - Fold type checks on constants (Op::TypeCheck on literals, and is_null/is_int/etc. built-in calls on constants).
/// - Strength reduction: $x === null -> TypeCheck(NULL), null === $x -> TypeCheck(NULL).
/// - Constant conversion: (bool)$x -> Unary::Bool, literal casts folded.
pub(crate) fn run_pass2(op_array: &mut OpArray) -> bool {
    let mut changed = false;
    let len = op_array.ops.len();

    for i in 0..len {
        let op = &op_array.ops[i];
        let mut replacement = None;

        match op {
            Op::TypeCheck { a, mask, dst } => {
                if let Operand::Lit(la) = a {
                    if let Some(va) = op_array.literals.get(*la as usize) {
                        let res = type_mask::matches(*mask, va);
                        let lit = add_or_find_literal(&mut op_array.literals, Value::Bool(res));
                        replacement = Some(match dst {
                            Some(d) => Op::Move {
                                src: Operand::Lit(lit),
                                dst: *d,
                            },
                            None => Op::Nop,
                        });
                    }
                }
            }

            Op::Bin {
                op: bin_op,
                a,
                b,
                dst,
            } => {
                // Strength reduction: $x === null -> TypeCheck(NULL)
                if *bin_op == BinKind::Identical {
                    if let Operand::Lit(lb) = b {
                        if matches!(op_array.literals.get(*lb as usize), Some(Value::Null)) {
                            replacement = Some(Op::TypeCheck {
                                a: *a,
                                mask: type_mask::NULL,
                                dst: Some(*dst),
                            });
                        }
                    } else if let Operand::Lit(la) = a {
                        if matches!(op_array.literals.get(*la as usize), Some(Value::Null)) {
                            replacement = Some(Op::TypeCheck {
                                a: *b,
                                mask: type_mask::NULL,
                                dst: Some(*dst),
                            });
                        }
                    }
                }
            }

            Op::Cast { kind, a, dst } => {
                if let Operand::Lit(la) = a {
                    let va = &op_array.literals[*la as usize];
                    if let Some(val) = fold_cast(*kind, va) {
                        let lit = add_or_find_literal(&mut op_array.literals, val);
                        replacement = Some(Op::Move {
                            src: Operand::Lit(lit),
                            dst: *dst,
                        });
                    }
                } else if *kind == CastKind::Bool {
                    replacement = Some(Op::Unary {
                        op: UnKind::Bool,
                        a: *a,
                        dst: *dst,
                    });
                }
            }

            _ => {}
        }

        if let Some(new_op) = replacement {
            op_array.ops[i] = new_op;
            changed = true;
        }
    }

    // Fold type check function calls (e.g. is_null, is_int, is_float, is_string, is_bool, is_array) on constants
    changed |= fold_builtin_calls(op_array, is_typecheck_fn);

    changed
}

/// Runs Pass 4: Function call optimization (ZEND_OPTIMIZER_PASS_4).
///
/// - Evaluate pure built-in calls at compile-time when all arguments are known constants
///   (e.g. strlen, chr, ord, abs, min, max, is_null, is_string, etc.).
pub(crate) fn run_pass4(op_array: &mut OpArray) -> bool {
    let mut changed = false;

    // 1. Fold specialized Abs on literal operands if present
    for i in 0..op_array.ops.len() {
        if let Op::Abs { src, dst, to } = &op_array.ops[i] {
            if let Operand::Lit(la) = src {
                if let Some(val) = op_array.literals.get(*la as usize) {
                    if let Some(res) = evaluate_pure_builtin(b"abs", &[val]) {
                        let res_lit = add_or_find_literal(&mut op_array.literals, res);
                        if let Some(cv) = to {
                            op_array.ops[i] = Op::AssignCv {
                                cv: *cv,
                                val: Operand::Lit(res_lit),
                                dst: *dst,
                            };
                        } else {
                            op_array.ops[i] = match dst {
                                Some(d) => Op::Move {
                                    src: Operand::Lit(res_lit),
                                    dst: *d,
                                },
                                None => Op::Nop,
                            };
                        }
                        changed = true;
                    }
                }
            }
        }
    }

    // 2. Fold pure built-in function calls
    changed |= fold_builtin_calls(op_array, |_| true);

    changed
}

fn fold_builtin_calls(op_array: &mut OpArray, filter: impl Fn(&[u8]) -> bool) -> bool {
    let mut changed = false;
    let len = op_array.ops.len();

    for i in 0..len {
        // Only a call that resolves to the global function at compile time
        // is evaluated: an unqualified call in a namespace (one with a
        // fallback name) may reach a namespaced function of the same name
        // declared later (`zend_compile_call` compiles it as
        // `INIT_NS_FCALL_BY_NAME`, which php-src never evaluates).
        let fn_name_bytes = match &op_array.ops[i] {
            Op::InitFCall {
                name,
                fallback: None,
                ..
            } => match op_array.literals.get(*name as usize) {
                Some(Value::Str(s)) if !s.as_bytes().contains(&b'\\') => s.clone(),
                _ => continue,
            },
            _ => continue,
        };
        let name_bytes = fn_name_bytes.as_bytes().to_ascii_lowercase();

        if !filter(&name_bytes) {
            continue;
        }

        // Collect argument literals and find Op::DoCall
        let mut arg_lits = Vec::new();
        let mut send_indices = Vec::new();
        let mut docall_info = None;
        let mut valid = true;

        for j in (i + 1)..len {
            match &op_array.ops[j] {
                Op::Nop => {}
                Op::Send { src } => match src {
                    ArgSrc::Val(Operand::Lit(l)) | ArgSrc::CallResult(Operand::Lit(l)) => {
                        arg_lits.push(*l);
                        send_indices.push(j);
                    }
                    _ => {
                        valid = false;
                        break;
                    }
                },
                Op::DoCall { dst } => {
                    docall_info = Some((j, *dst));
                    break;
                }
                _ => {
                    valid = false;
                    break;
                }
            }
        }

        if !valid {
            continue;
        }

        let Some((call_idx, dst)) = docall_info else {
            continue;
        };

        // Get literal values for arguments
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        let mut arg_values = Vec::with_capacity(arg_lits.len());
        let mut all_found = true;
        for &lit_idx in &arg_lits {
            if let Some(val) = op_array.literals.get(lit_idx as usize) {
                arg_values.push(val);
            } else {
                all_found = false;
                break;
            }
        }

        if !all_found {
            continue;
        }

        if let Some(result) = evaluate_pure_builtin(&name_bytes, &arg_values) {
            let res_lit = add_or_find_literal(&mut op_array.literals, result);
            op_array.ops[i] = Op::Nop;
            for s_idx in send_indices {
                op_array.ops[s_idx] = Op::Nop;
            }
            op_array.ops[call_idx] = match dst {
                Some(d) => Op::Move {
                    src: Operand::Lit(res_lit),
                    dst: d,
                },
                None => Op::Nop,
            };
            changed = true;
        }
    }

    changed
}

fn is_typecheck_fn(name: &[u8]) -> bool {
    matches!(
        name,
        b"is_null"
            | b"is_bool"
            | b"is_int"
            | b"is_integer"
            | b"is_long"
            | b"is_float"
            | b"is_double"
            | b"is_string"
            | b"is_array"
            | b"is_object"
            | b"is_resource"
            | b"is_scalar"
            | b"is_numeric"
    )
}

fn is_php_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\0' | 0x0B)
}

fn trim_bytes<'a>(mut s: &'a [u8], left: bool, right: bool) -> &'a [u8] {
    if left {
        while let Some((&first, rest)) = s.split_first() {
            if is_php_whitespace(first) {
                s = rest;
            } else {
                break;
            }
        }
    }
    if right {
        while let Some((&last, rest)) = s.split_last() {
            if is_php_whitespace(last) {
                s = rest;
            } else {
                break;
            }
        }
    }
    s
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex2bin(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: linear in a compile-time literal"
    )]
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.chunks_exact(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn bin2hex(s: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: linear in a compile-time literal"
    )]
    let mut out = Vec::with_capacity(s.len() * 2);
    for &b in s {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0f) as usize]);
    }
    out
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn evaluate_pure_builtin(name: &[u8], args: &[&Value]) -> Option<Value> {
    if super::is_disabled_function(name) {
        return None;
    }
    match name {
        b"strlen" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Int(s.len() as i64));
                }
            }
            None
        }
        // `@compile-time-eval` in `zend_builtin_functions.stub.php`.
        b"zend_version" => args
            .is_empty()
            .then(|| Value::Str(ZStr::from(crate::ZEND_VERSION.as_bytes()))),
        // sccp.c `can_ct_eval_func_call` evaluates `dirname` on POSIX; a
        // level below 1 throws, which discards the evaluation.
        b"dirname" => match args {
            [Value::Str(p)] => Some(Value::Str(ZStr::from(
                php_stdstr::transform::dirname(p.as_bytes(), 1).ok()?,
            ))),
            [Value::Str(p), Value::Int(levels)] if *levels >= 1 => Some(Value::Str(ZStr::from(
                php_stdstr::transform::dirname(p.as_bytes(), *levels).ok()?,
            ))),
            _ => None,
        },
        b"chr" => {
            if args.len() == 1 {
                if let Value::Int(n) = args[0] {
                    return Some(Value::Str(ZStr::from(vec![(*n & 0xff) as u8])));
                }
            }
            None
        }
        b"ord" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    let b = if s.is_empty() {
                        0
                    } else {
                        s.as_bytes()[0] as i64
                    };
                    return Some(Value::Int(b));
                }
            }
            None
        }
        b"abs" => {
            if args.len() == 1 {
                match args[0] {
                    Value::Int(n) => {
                        return Some(if *n == i64::MIN {
                            Value::Float(-(*n as f64))
                        } else {
                            Value::Int(n.abs())
                        });
                    }
                    Value::Float(f) => return Some(Value::Float(f.abs())),
                    _ => {}
                }
            }
            None
        }
        b"min" => {
            if args.len() >= 2 {
                let mut min_val = args[0];
                for arg in &args[1..] {
                    let mut ctx = CollectingContext::new();
                    if let Ok(true) = php_runtime::ops::is_smaller(arg, min_val, &mut ctx) {
                        if ctx.diagnostics.is_empty() {
                            min_val = arg;
                        } else {
                            return None;
                        }
                    } else if !ctx.diagnostics.is_empty() {
                        return None;
                    }
                }
                return Some((*min_val).clone());
            }
            None
        }
        b"max" => {
            if args.len() >= 2 {
                let mut max_val = args[0];
                for arg in &args[1..] {
                    let mut ctx = CollectingContext::new();
                    if let Ok(true) = php_runtime::ops::is_smaller(max_val, arg, &mut ctx) {
                        if ctx.diagnostics.is_empty() {
                            max_val = arg;
                        } else {
                            return None;
                        }
                    } else if !ctx.diagnostics.is_empty() {
                        return None;
                    }
                }
                return Some((*max_val).clone());
            }
            None
        }
        b"is_null" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Null)))
            } else {
                None
            }
        }
        b"is_bool" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Bool(_))))
            } else {
                None
            }
        }
        b"is_int" | b"is_integer" | b"is_long" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Int(_))))
            } else {
                None
            }
        }
        b"is_float" | b"is_double" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Float(_))))
            } else {
                None
            }
        }
        b"is_string" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Str(_))))
            } else {
                None
            }
        }
        b"is_array" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Array(_))))
            } else {
                None
            }
        }
        b"is_object" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Object(_))))
            } else {
                None
            }
        }
        b"is_resource" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(args[0], Value::Resource(_))))
            } else {
                None
            }
        }
        b"is_scalar" => {
            if args.len() == 1 {
                Some(Value::Bool(matches!(
                    args[0],
                    Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
                )))
            } else {
                None
            }
        }
        b"is_numeric" => {
            if args.len() == 1 {
                let is_num = match args[0] {
                    Value::Int(_) | Value::Float(_) => true,
                    Value::Str(s) => php_runtime::numeric::is_numeric(s.as_bytes()),
                    _ => false,
                };
                Some(Value::Bool(is_num))
            } else {
                None
            }
        }
        b"boolval" => {
            if args.len() == 1 {
                fold_cast(CastKind::Bool, args[0])
            } else {
                None
            }
        }
        b"intval" => {
            if args.len() == 1 {
                fold_cast(CastKind::Int, args[0])
            } else {
                None
            }
        }
        b"floatval" | b"doubleval" => {
            if args.len() == 1 {
                fold_cast(CastKind::Float, args[0])
            } else {
                None
            }
        }
        b"strval" => {
            if args.len() == 1 {
                fold_cast(CastKind::String, args[0])
            } else {
                None
            }
        }
        b"strtolower" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(s.as_bytes().to_ascii_lowercase())));
                }
            }
            None
        }
        b"strtoupper" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(s.as_bytes().to_ascii_uppercase())));
                }
            }
            None
        }
        b"trim" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(trim_bytes(s.as_bytes(), true, true))));
                }
            }
            None
        }
        b"ltrim" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(trim_bytes(
                        s.as_bytes(),
                        true,
                        false,
                    ))));
                }
            }
            None
        }
        b"rtrim" | b"chop" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(trim_bytes(
                        s.as_bytes(),
                        false,
                        true,
                    ))));
                }
            }
            None
        }
        b"bin2hex" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    return Some(Value::Str(ZStr::from(bin2hex(s.as_bytes()))));
                }
            }
            None
        }
        b"hex2bin" => {
            if args.len() == 1 {
                if let Value::Str(s) = args[0] {
                    if let Some(decoded) = hex2bin(s.as_bytes()) {
                        return Some(Value::Str(ZStr::from(decoded)));
                    }
                }
            }
            None
        }
        b"substr" => {
            if args.len() == 2 || args.len() == 3 {
                if let (Value::Str(st), Value::Int(offset)) = (args[0], args[1]) {
                    let len = if args.len() == 3 {
                        if let Value::Int(l) = args[2] {
                            Some(*l)
                        } else {
                            return None;
                        }
                    } else {
                        None
                    };
                    let sub = php_stdstr::search::substr(st.as_bytes(), *offset, len);
                    return Some(Value::Str(ZStr::from(sub)));
                }
            }
            None
        }
        b"str_repeat" => {
            if args.len() == 2 {
                if let (Value::Str(s), Value::Int(times)) = (args[0], args[1]) {
                    // As php-src (`Zend/Optimizer/sccp.c`, "avoid creating
                    // overly large strings at compile-time"): only results
                    // below 64 KiB; a larger one is left to the run-time
                    // call, which checks it against `memory_limit`.
                    const FOLD_MAX: usize = 64 * 1024;
                    if usize::try_from(*times)
                        .ok()
                        .and_then(|t| s.len().checked_mul(t))
                        .is_some_and(|n| n < FOLD_MAX)
                    {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "bounded: below FOLD_MAX bytes"
                        )]
                        let repeated = s.as_bytes().repeat(*times as usize);
                        return Some(Value::Str(ZStr::from(repeated)));
                    }
                }
            }
            None
        }
        b"sqrt" => {
            if args.len() == 1 {
                match args[0] {
                    Value::Int(n) if *n >= 0 => {
                        return Some(Value::Float((*n as f64).sqrt()));
                    }
                    Value::Float(f) if *f >= 0.0 => return Some(Value::Float(f.sqrt())),
                    _ => {}
                }
            }
            None
        }
        b"floor" => {
            if args.len() == 1 {
                match args[0] {
                    Value::Int(n) => {
                        return Some(Value::Float(*n as f64));
                    }
                    Value::Float(f) => return Some(Value::Float(f.floor())),
                    _ => {}
                }
            }
            None
        }
        b"ceil" => {
            if args.len() == 1 {
                match args[0] {
                    Value::Int(n) => {
                        return Some(Value::Float(*n as f64));
                    }
                    Value::Float(f) => return Some(Value::Float(f.ceil())),
                    _ => {}
                }
            }
            None
        }
        b"round" => {
            if args.len() == 1 {
                match args[0] {
                    Value::Int(n) => {
                        return Some(Value::Float(*n as f64));
                    }
                    Value::Float(f) => return Some(Value::Float(f.round())),
                    _ => {}
                }
            }
            None
        }
        b"intdiv" => {
            if args.len() == 2 {
                if let (Value::Int(a), Value::Int(b)) = (args[0], args[1]) {
                    if *b != 0 && !(*a == i64::MIN && *b == -1) {
                        return Some(Value::Int(*a / *b));
                    }
                }
            }
            None
        }
        b"pi" => {
            if args.is_empty() {
                return Some(Value::Float(std::f64::consts::PI));
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn fold_bin_op(op: BinKind, a: &Value, b: &Value) -> Option<Value> {
    // Never fold compound types, references, or resources
    if matches!(
        (a, b),
        (Value::Array(_), _)
            | (_, Value::Array(_))
            | (Value::Object(_), _)
            | (_, Value::Object(_))
            | (Value::Reference(_), _)
            | (_, Value::Reference(_))
            | (Value::Resource(_), _)
            | (_, Value::Resource(_))
    ) {
        return None;
    }

    match op {
        BinKind::Arith(bop) => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::binary_op(bop, a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(res);
                }
            }
            None
        }
        BinKind::Equal => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::is_equal(a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Bool(res));
                }
            }
            None
        }
        BinKind::NotEqual => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::is_not_equal(a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Bool(res));
                }
            }
            None
        }
        BinKind::Identical => {
            if let Ok(res) = php_runtime::ops::is_identical(a, b) {
                Some(Value::Bool(res))
            } else {
                None
            }
        }
        BinKind::NotIdentical => {
            if let Ok(res) = php_runtime::ops::is_identical(a, b) {
                Some(Value::Bool(!res))
            } else {
                None
            }
        }
        BinKind::Less => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::is_smaller(a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Bool(res));
                }
            }
            None
        }
        BinKind::LessEqual => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::is_smaller_or_equal(a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Bool(res));
                }
            }
            None
        }
        BinKind::Spaceship => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::spaceship(a, b, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(res);
                }
            }
            None
        }
    }
}

pub(crate) fn fold_unary_op(op: UnKind, a: &Value) -> Option<Value> {
    match op {
        UnKind::Not => crate::compiler::expr::const_truthiness(a).map(|t| Value::Bool(!t)),
        UnKind::Bool => crate::compiler::expr::const_truthiness(a).map(Value::Bool),
        UnKind::BitNot => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::bit_not(a, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(res);
                }
            }
            None
        }
        UnKind::Neg => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::sub(&Value::Int(0), a, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(res);
                }
            }
            None
        }
        UnKind::Plus => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = php_runtime::ops::add(&Value::Int(0), a, &mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(res);
                }
            }
            None
        }
    }
}

pub(crate) fn fold_cast(kind: CastKind, a: &Value) -> Option<Value> {
    match kind {
        CastKind::Bool => crate::compiler::expr::const_truthiness(a).map(Value::Bool),
        CastKind::Int => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = a.to_long(&mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Int(res));
                }
            }
            None
        }
        CastKind::Float => {
            let mut ctx = CollectingContext::new();
            if let Ok(res) = a.to_double(&mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Float(res));
                }
            }
            None
        }
        CastKind::String => {
            // Avoid folding float to string to avoid precision / locale differences
            if matches!(a, Value::Float(_)) {
                return None;
            }
            let mut ctx = CollectingContext::new();
            if let Ok(res) = a.to_zstr(&mut ctx) {
                if ctx.diagnostics.is_empty() {
                    return Some(Value::Str(res));
                }
            }
            None
        }
        CastKind::Null => Some(Value::Null),
        CastKind::Array | CastKind::Object => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GH-17733: a function removed by `disable_functions` is not in the
    /// function table, so php-src never evaluates a call to it at compile
    /// time (`str_repeat("ab", 3)` then throws "Call to undefined function").
    #[test]
    fn disabled_function_is_not_evaluated() {
        let s = Value::Str(ZStr::from(&b"ab"[..]));
        let n = Value::Int(3);
        assert!(evaluate_pure_builtin(b"str_repeat", &[&s, &n]).is_some());
        super::super::set_disabled_functions([b"str_repeat".to_vec()]);
        assert!(evaluate_pure_builtin(b"str_repeat", &[&s, &n]).is_none());
        assert!(evaluate_pure_builtin(b"strlen", &[&s]).is_some());
        super::super::set_disabled_functions(std::iter::empty());
        assert!(evaluate_pure_builtin(b"str_repeat", &[&s, &n]).is_some());
    }

    /// `str_repeat()` is evaluated at compile time only below 64 KiB, as
    /// php-src's sccp.c: a larger result would be allocated by the compiler,
    /// outside any `memory_limit` check.
    #[test]
    fn str_repeat_is_folded_below_64_kib() {
        let s = Value::Str(ZStr::from(&b"abcd"[..]));
        let fold = |n: i64| evaluate_pure_builtin(b"str_repeat", &[&s, &Value::Int(n)]);
        assert!(fold(16 * 1024 - 1).is_some());
        assert!(fold(16 * 1024).is_none());
        assert!(fold(100_000_000).is_none());
        assert!(fold(-1).is_none());
    }

    /// `zend_version()` and `dirname()` are evaluated at compile time
    /// (ext/opcache/tests/zend_version.phpt, ct_eval_frameless_001.phpt);
    /// expectations from `/usr/bin/php`.
    #[test]
    fn zend_version_and_dirname_are_evaluated() {
        assert!(matches!(
            evaluate_pure_builtin(b"zend_version", &[]),
            Some(Value::Str(s)) if s.as_bytes() == crate::ZEND_VERSION.as_bytes()
        ));
        let p = Value::Str(ZStr::from(&b"/a/b/c/"[..]));
        let dir = |args: &[&Value]| match evaluate_pure_builtin(b"dirname", args) {
            Some(Value::Str(s)) => Some(s.as_bytes().to_vec()),
            _ => None,
        };
        assert_eq!(dir(&[&p]), Some(b"/a/b".to_vec()));
        assert_eq!(dir(&[&p, &Value::Int(2)]), Some(b"/a".to_vec()));
        assert_eq!(dir(&[&p, &Value::Int(9)]), Some(b"/".to_vec()));
        assert_eq!(
            dir(&[&Value::Str(ZStr::from(&b"abc"[..]))]),
            Some(b".".to_vec())
        );
        // `dirname($p, 0)` throws a ValueError at run time.
        assert_eq!(dir(&[&p, &Value::Int(0)]), None);
        assert_eq!(dir(&[&Value::Int(1)]), None);
    }
}
