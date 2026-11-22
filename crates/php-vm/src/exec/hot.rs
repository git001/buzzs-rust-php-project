// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The frame-local inner dispatch loop: jumps, smart branches, numeric
//! arithmetic, `++`/`--` and plain assignments of compiled variables and
//! temporaries, run on the current frame and the local `ip` alone.
//!
//! php-src's `execute_ex` keeps `execute_data` and `opline` in machine
//! registers (`ZEND_VM_FP_GLOBAL_REG`, `ZEND_VM_IP_GLOBAL_REG`) and its
//! specialised handlers (`ZEND_PRE_INC_LONG_SPEC_CV`,
//! `ZEND_IS_SMALLER_LONG_SPEC_TMPVARCV_CONST_JMPNZ`,
//! `ZEND_ADD_DOUBLE_SPEC_CV_CONST`, `ZEND_ASSIGN_SPEC_CV_CONST_RETVAL_UNUSED`,
//! `ZEND_JMPZ_SPEC_CV`, ...) touch nothing else. `Vm::run` is one large
//! function whose `ip` and frame live on the stack, and every fast path
//! there looked the current frame up again (the frame stack's bounds, the
//! frame's address). [`step`] runs one such op on a frame borrowed once;
//! `Vm::run` uses it inline for a hot op, and when the op after it is hot
//! as well enters the tape ([`super::tape::run_tape`]), which stays in its
//! own loop until an op it does not handle, an op whose fast path does not
//! apply (nothing changed: `Vm::run` then runs the generic op), or a due
//! safe point (`EG(vm_interrupt)`). The tape runs the ops it has
//! specialised instructions for itself and hands the others here: the core
//! ops (jumps, smart branches, arithmetic, `++`/`--`, plain assignments,
//! the fused loop ops) to [`step`], every other hot op to [`step_ext`]. A
//! frame the tape cannot run on (one with an attached symbol table) has
//! its hot ops run by [`run_steps`].
//!
//! Every fast path here completes its op without diagnostics, user code or
//! frame changes, exactly as the `Vm::run` pre-match did before: it reads
//! only plain values (no references, no undefined variables), and it
//! overwrites only variable slots that hold no refcounted value other than
//! a string (`is_plain_slot`), so releasing the old value has no side
//! effect; `AssignCv` also replaces an array or object, released as
//! `Vm::cv_assign` does (a destructor it queues runs at the safe point
//! after the op, as there). A frame whose symbol table is attached (`Frame::symtab_attached`)
//! has each written variable copied into its `VarStore` slot, out of line
//! ([`sync_cv_cold`]).
//!
//! The array-dimension ops of a variable (`$cv[k]` read, `isset`, `=`,
//! `op=`, `++`/`--`, nested chains) and `.=` on a variable run here as well
//! (`exec/spec/dims.rs`, the pre-match's fast paths moved to the frame):
//! they read an element (a refcount increment), or overwrite a plain
//! element of an array the variable holds, separating a shared one first,
//! and decline, having changed nothing, wherever the generic op could warn
//! or reach an element held by reference, an object or user code.

use super::spec::{
    self, assign_dim_cv_in, assign_dims_cv_in, assign_op_dim_cv_in, assign_op_dims_cv_in,
    cast_string_in, cmp_simple_in, concat_cv_in, concat_in, fe_assign_in, fe_fetch_array_in,
    fetch_dim_cv_in, fetch_dims_cv_in, incdec_dim_cv_in, is_plain_slot, isset_dim_cv_in,
    num_outcome, plain_truth, put_tmp, rope_in, slot_truth,
};
use super::{discard_unrooted, num_bin, release_rooted, Num};
use crate::opcode::{cmp_outcome, BinKind, CastKind, IncDecKind, Op, Operand, UnKind};
use crate::vm::{CvSlot, Frame};
use php_runtime::{BinaryOp, Value, ZStr};

/// The second half of [`super::tape::run_tape`]'s result when no safe
/// point is due: the op at the returned `ip` stopped the loop.
pub(super) const NO_SAFE_POINT: usize = usize::MAX;

/// Whether `op` is a hot op: [`step`] handles it (its fast path may still
/// decline).
#[inline(always)]
#[allow(clippy::inline_always)] // a discriminant test at the top of the dispatch loop
pub(super) fn is_hot(op: &Op) -> bool {
    matches!(
        op,
        Op::Nop
            | Op::Jmp(_)
            | Op::JmpZ(..)
            | Op::JmpNz(..)
            | Op::JmpZEx { .. }
            | Op::JmpNzEx { .. }
            | Op::Unary {
                op: UnKind::Not | UnKind::Bool,
                ..
            }
            | Op::AssignCv { .. }
            | Op::Move { .. }
            | Op::Coalesce { .. }
            | Op::JmpSet { .. }
            | Op::JmpNull { .. }
            | Op::CmpJmp { .. }
            | Op::Bin { .. }
            | Op::BinAssignCv { .. }
            | Op::IncDecCv { .. }
            | Op::IncDecCvCmpJmp { .. }
            | Op::BinAssignCvCmpJmp { .. }
            | Op::AssignOpCvCmpJmp { .. }
            | Op::BinAssignCvJmp { .. }
            | Op::AssignOpCvJmp { .. }
            | Op::AssignOpCv { .. }
            // Run-time cache hits of constants, class constants and static
            // properties (`exec/spec/statics.rs`).
            | Op::FetchConst { fallback: None, .. }
            | Op::FetchConstTo { fallback: None, .. }
            | Op::FetchClassConst {
                name: Operand::Lit(_),
                ..
            }
            | Op::FetchClassConstTo { .. }
            | Op::FetchThis { .. }
            | Op::FetchStaticProp { .. }
            | Op::AssignStaticProp { .. }
            | Op::IssetStaticProp { .. }
            // Object property cache hits (`exec/spec/props.rs`).
            | Op::FetchObjProp { .. }
            | Op::AssignObjProp { .. }
            | Op::AssignOpObjProp { .. }
            | Op::IncDecObjProp { .. }
            | Op::IssetObjProp { .. }
            | Op::New { .. }
            | Op::FuncNumArgs { .. }
            | Op::Strlen { .. }
            | Op::Count { .. }
            | Op::TypeCheck { .. }
            | Op::Isset { .. }
            | Op::Empty { .. }
            | Op::InitFCall { fallback: None, .. }
            | Op::Send { .. }
            | Op::Free(Operand::Tmp(_))
            | Op::FetchDimCv { .. }
            | Op::FetchDimsCv { .. }
            | Op::IssetDimCv { .. }
            | Op::AssignDimCv { .. }
            | Op::AssignDimsCv { .. }
            | Op::AssignOpDimCv { .. }
            | Op::AssignOpDimsCv { .. }
            | Op::IncDecDimCv { .. }
            | Op::ConcatCv { .. }
            | Op::Rope { .. }
            | Op::Cast {
                kind: CastKind::String,
                ..
            }
            | Op::FeFetch { .. }
    )
}

/// Set in the `ip` [`super::tape::run_tape`] returns when a call or a return it made
/// changed the current frame: the `ip` (without this bit) is one of the
/// new current frame's, whose op array `Vm::run` loads first (`LOAD_OPLINE`
/// after `ZEND_VM_ENTER`/`ZEND_VM_LEAVE`). Op indexes never reach it.
pub(super) const SWITCHED: usize = 1 << (usize::BITS - 1);

/// The second half of [`super::tape::run_tape`]'s result when a call or a return it made
/// changed the current frame and the safe point after it is due: the
/// frame's `ip` is stored in it (the callee's first op, or the caller's
/// resume point), and `Vm::run` runs the safe point as after its own fast
/// `DoCall` and `Return` (`Vm::safe_point_after_leave`) before reloading
/// the frame.
pub(super) const LEFT: usize = usize::MAX - 1;

/// The second half of [`super::tape::run_tape`]'s result when a builtin it
/// called threw or exited: the throw is in `Vm::tape_throw`, and the
/// current frame's resume point is stored (`SAVE_OPLINE`), so `Vm::run`
/// propagates it as its own generic `DoCall` would. A sentinel instead of
/// a `Result`: the result stays two registers wide on every return.
pub(super) const THREW: usize = usize::MAX - 2;

/// Whether `op` is a call or a return [`super::tape::run_tape`] makes itself when its
/// fast path applies (`Vm::push_simple_ucall`, `Vm::return_simple`).
#[inline(always)]
#[allow(clippy::inline_always)] // a discriminant test
pub(super) fn is_frame_op(op: &Op) -> bool {
    matches!(op, Op::DoCall { .. } | Op::Return { .. })
}

/// Whether `Vm::run` enters [`super::tape::run_tape`] at `op` after a hot op: a hot op
/// or a call or return it makes itself ([`is_frame_op`]).
#[inline(always)]
#[allow(clippy::inline_always)] // a discriminant test
pub(super) fn enters_hot(op: &Op) -> bool {
    is_hot(op) || is_frame_op(op)
}

/// Whether [`step`] runs `op` in one of its own arms (the core ops); every
/// other hot op goes to [`step_ext`]. The tape hands an op it has no
/// instruction for to the one or the other by this (a wrong answer costs a
/// call, not correctness: `step` passes on what is not its own, and
/// `step_ext` declines it).
pub(super) fn is_core(op: &Op) -> bool {
    matches!(
        op,
        Op::Nop
            | Op::Jmp(_)
            | Op::JmpZ(..)
            | Op::JmpNz(..)
            | Op::JmpZEx { .. }
            | Op::JmpNzEx { .. }
            | Op::Unary {
                op: UnKind::Not | UnKind::Bool,
                ..
            }
            | Op::AssignCv { .. }
            | Op::Move { .. }
            | Op::Coalesce { .. }
            | Op::JmpSet { .. }
            | Op::JmpNull { .. }
            | Op::CmpJmp { .. }
            | Op::Bin { .. }
            | Op::BinAssignCv { .. }
            | Op::IncDecCv { .. }
            | Op::IncDecCvCmpJmp { .. }
            | Op::BinAssignCvCmpJmp { .. }
            | Op::AssignOpCvCmpJmp { .. }
            | Op::BinAssignCvJmp { .. }
            | Op::AssignOpCvJmp { .. }
            | Op::AssignOpCv { .. }
    )
}

/// Runs the hot ops of frame `f` from `ip` through [`step`] while their
/// fast paths apply. Returns `(at, NO_SAFE_POINT)` when the op at `at` is
/// not a hot op or declined, having changed nothing, and `(ip, at)` when
/// the safe point after the op at `at` is due, `ip` being where execution
/// continues.
///
/// The loop of a frame the tape cannot run on (one whose symbol table is
/// attached: `step` copies each written variable there). It stays in this
/// one frame: a call or a return is the caller's
/// ([`super::tape::run_tape`]), which then looks at the frame it switched
/// to afresh, so that a callee is not kept off the tape by its caller.
///
/// Never inlined: its own register allocation keeps the frame, `ops`,
/// `lits` and `ip` out of the caller's stack frame. It inlines only
/// `step`'s core ops; the others are one call each ([`step_ext`]).
#[inline(never)]
#[link_section = ".text.hot.php_vm"]
pub(super) fn run_steps(
    f: &mut Frame,
    ops: &[Op],
    lits: &[Value],
    mut ip: usize,
) -> (usize, usize) {
    loop {
        let at = ip;
        ip = step(f, ops, lits, at);
        if ip == DECLINED {
            return (at, NO_SAFE_POINT);
        }
        if crate::interrupt::safe_point_due() {
            return (ip, at);
        }
    }
}

/// [`step`]'s result when the op at `at` is not a hot op or its fast path
/// declined, having changed nothing.
pub(super) const DECLINED: usize = usize::MAX;

/// Runs the hot op at `at` of frame `f` when its fast path applies, and
/// returns the `ip` to continue at; [`DECLINED`] otherwise. The safe point
/// after the op is the caller's.
///
/// The core ops (jumps, smart branches, arithmetic, `++`/`--`, plain
/// assignments and the fused loop ops) run inline; every other hot op
/// ([`is_hot`]) through [`step_ext`], out of line. A new hot op's arm goes
/// into `step_ext` unless it is as small and as frequent as these.
#[inline(always)]
#[allow(clippy::inline_always, clippy::too_many_lines)] // one arm per hot op, like `Vm::run`
pub(super) fn step(f: &mut Frame, ops: &[Op], lits: &[Value], at: usize) -> usize {
    let Some(op) = ops.get(at) else {
        return DECLINED;
    };
    let mut ip = at + 1;
    match *op {
        Op::Nop => {}
        Op::Jmp(t) => ip = t as usize,
        Op::JmpZ(a, t) => match take_bool(f, a) {
            Some(false) => ip = t as usize,
            Some(true) => {}
            None => return DECLINED,
        },
        Op::JmpNz(a, t) => match take_bool(f, a) {
            Some(true) => ip = t as usize,
            Some(false) => {}
            None => return DECLINED,
        },
        // `&&` / `||` (`ZEND_JMPZ_EX`, `ZEND_JMPNZ_EX`): the jump
        // leaves the boolean in `dst`.
        Op::JmpZEx { a, dst, target } => match take_bool(f, a) {
            Some(false) => {
                set_tmp_in(f, dst, Value::Bool(false));
                ip = target as usize;
            }
            Some(true) => {}
            None => return DECLINED,
        },
        Op::JmpNzEx { a, dst, target } => match take_bool(f, a) {
            Some(true) => {
                set_tmp_in(f, dst, Value::Bool(true));
                ip = target as usize;
            }
            Some(false) => {}
            None => return DECLINED,
        },
        // `!a` and `(bool) a` (`ZEND_BOOL_NOT`, `ZEND_BOOL`) of a
        // value whose truth needs no conversion.
        Op::Unary {
            op: un @ (UnKind::Not | UnKind::Bool),
            a,
            dst,
        } => {
            let Some(b) = operand_truth(f, a) else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Bool(b != (un == UnKind::Not)));
        }
        Op::AssignCv { cv, val, dst } => {
            if !assign_cv_in(f, cv, val, dst) {
                return DECLINED;
            }
        }
        Op::Move { src, dst } => {
            if !move_in(f, src, dst) {
                return DECLINED;
            }
        }
        Op::Coalesce { a, dst, target } => match coalesce_in(f, a, dst) {
            Some(true) => ip = target as usize,
            Some(false) => {}
            None => return DECLINED,
        },
        Op::JmpSet { a, dst, target } => match jmp_set_in(f, a, dst) {
            Some(true) => ip = target as usize,
            Some(false) => {}
            None => return DECLINED,
        },
        Op::JmpNull { a, dst, target } => match jmp_null_in(f, a, dst) {
            Some(true) => ip = target as usize,
            Some(false) => {}
            None => return DECLINED,
        },
        // Smart branch (`ZEND_IS_SMALLER_LONG_SPEC` +
        // `ZEND_VM_SMART_BRANCH`): jumps, or skips the jump op after
        // it.
        Op::CmpJmp {
            op: kind,
            a,
            b,
            target,
            jmp_if,
            mask,
            ..
        } => {
            let jump = match cmp_num(f, lits, a, b, mask) {
                Some(j) => j,
                None => match equality(f, kind, a, b) {
                    Some(r) => r == jmp_if,
                    None => return DECLINED,
                },
            };
            if jump {
                ip = target as usize;
            } else {
                ip += 1;
            }
        }
        Op::Bin {
            op: kind,
            a,
            b,
            dst,
        } => {
            if !bin_tmp(f, lits, kind, a, b, dst) {
                // `ZEND_CONCAT` of strings and integers.
                if kind == BinKind::Arith(BinaryOp::Concat) {
                    if f.tmps.len() <= dst as usize {
                        return DECLINED;
                    }
                    let Some(s) = concat_ext(f, lits, a, b) else {
                        return DECLINED;
                    };
                    set_tmp_in(f, dst, Value::Str(s));
                    return ip;
                }
                let Some(r) = equality(f, kind, a, b) else {
                    return DECLINED;
                };
                let Some(slot) = f.tmps.get_mut(dst as usize) else {
                    return DECLINED;
                };
                put_tmp(slot, Value::Bool(r));
            }
        }
        // Stored into the variable: the `AssignCv` after the op is
        // done; otherwise it runs on `dst`.
        Op::BinAssignCv {
            op: kind,
            a,
            b,
            dst,
            cv,
        } => match bin_assign_cv(f, lits, kind, a, b, cv, dst) {
            Some(true) => ip += 1,
            Some(false) => {}
            // `ZEND_CONCAT` of strings and integers: the result goes to
            // `dst`, and the `AssignCv` after the op stores it.
            None if kind == BinKind::Arith(BinaryOp::Concat) => {
                if f.tmps.len() <= dst as usize {
                    return DECLINED;
                }
                let Some(s) = concat_ext(f, lits, a, b) else {
                    return DECLINED;
                };
                set_tmp_in(f, dst, Value::Str(s));
            }
            None => {
                let Some(r) = equality(f, kind, a, b) else {
                    return DECLINED;
                };
                if store_plain(f, cv, Value::Bool(r)) {
                    ip += 1;
                } else if let Some(slot) = f.tmps.get_mut(dst as usize) {
                    put_tmp(slot, Value::Bool(r));
                }
            }
        },
        Op::IncDecCv { cv, kind, dst, .. } => {
            if !incdec_cv(f, cv, kind, dst) {
                return DECLINED;
            }
        }
        // A loop's step and its condition: after the `++`/`--`, the
        // `CmpJmp` at `ip` on plain numbers jumps or skips itself and
        // its jump op; otherwise it runs as the next op.
        // The variable's new value is at hand: an operand naming it is not
        // read back from its slot (which now holds that number).
        Op::IncDecCvCmpJmp { cv, kind, .. } => {
            let Some((_, new)) = incdec_cv_num(f, cv, kind) else {
                return DECLINED;
            };
            if let Some(&Op::CmpJmp {
                a, b, target, mask, ..
            }) = ops.get(ip)
            {
                if let Some(jump) = step_cond(f, lits, cv, new, a, b, mask) {
                    ip = if jump { target as usize } else { ip + 2 };
                }
            }
        }
        Op::BinAssignCvCmpJmp {
            op: kind,
            a,
            b,
            dst,
            cv,
        } => match bin_assign_cv(f, lits, kind, a, b, cv, dst) {
            Some(true) => {
                if let Some(&Op::CmpJmp {
                    a: ca,
                    b: cb,
                    target,
                    mask,
                    ..
                }) = ops.get(ip + 1)
                {
                    match cmp_num(f, lits, ca, cb, mask) {
                        Some(true) => ip = target as usize,
                        Some(false) => ip += 3,
                        None => ip += 1,
                    }
                } else {
                    ip += 1;
                }
            }
            Some(false) => {}
            None => return DECLINED,
        },
        Op::AssignOpCvCmpJmp {
            cv, op: kind, val, ..
        } => {
            if !assign_op_cv(f, lits, cv, kind, val, None) {
                return DECLINED;
            }
            if let Some(&Op::CmpJmp {
                a, b, target, mask, ..
            }) = ops.get(ip)
            {
                match cmp_num(f, lits, a, b, mask) {
                    Some(true) => ip = target as usize,
                    Some(false) => ip += 2,
                    None => {}
                }
            }
        }
        Op::BinAssignCvJmp {
            op: kind,
            a,
            b,
            dst,
            cv,
            target,
            jmp_if,
        } => match bin_assign_cv(f, lits, kind, a, b, cv, dst) {
            Some(true) => match f.peek_cv(cv, slot_truth) {
                Some(cond) if cond == jmp_if => ip = target as usize,
                Some(_) => ip += 2,
                None => ip += 1,
            },
            Some(false) => {}
            None => return DECLINED,
        },
        Op::AssignOpCvJmp {
            cv,
            op: kind,
            val,
            target,
            jmp_if,
            ..
        } => {
            if !assign_op_cv(f, lits, cv, kind, val, None) {
                return DECLINED;
            }
            match f.peek_cv(cv, slot_truth) {
                Some(cond) if cond == jmp_if => ip = target as usize,
                Some(_) => ip += 1,
                None => {}
            }
        }
        Op::AssignOpCv {
            cv,
            op: kind,
            val,
            dst,
            ..
        } => {
            if !assign_op_cv(f, lits, cv, kind, val, dst) {
                return DECLINED;
            }
        }
        // Every other hot op runs out of line ([`step_ext`]); any other
        // op (and a core op whose nested pattern above did not match,
        // such as `Unary` of `-`) is not a hot op.
        _ => {
            return if is_hot(op) {
                step_ext(f, ops, lits, at)
            } else {
                DECLINED
            };
        }
    }
    ip
}

/// [`step`] for the hot ops outside its core: the constant, class-constant,
/// static- and object-property cache hits, `new`, the builtin ops
/// (`strlen()`, `count()`, `is_*()`, `isset`/`empty`, `func_num_args()`),
/// `INIT_FCALL`/`SEND`, `FREE`, the array-dimension ops of a variable,
/// `.=`, ropes, `(string)` and `foreach`. Same contract as `step`: the
/// `ip` to continue at, or [`DECLINED`] having changed nothing.
///
/// Never inlined, so that `step` stays small: every one of these arms
/// inlines its fast path (`exec/spec/*.rs`, `call.rs`), and with them in
/// the loop of the hot ops that function was 48 KB, and its op list (pointer
/// and length) and the next `ip` lived on the stack: 4 of the 17
/// instructions of each op's dispatch were spill loads and stores, and the
/// core arms reloaded the frame and its slot vectors from the stack too.
/// These ops pay a call instead (their fast paths are each several times
/// its cost), and the loop of the core ops (jumps, smart branches,
/// arithmetic, `++`/`--`, assignments and the fused ops) has the registers
/// for its state.
#[inline(never)]
#[allow(clippy::too_many_lines)] // one arm per hot op, like `step`
pub(super) fn step_ext(f: &mut Frame, ops: &[Op], lits: &[Value], at: usize) -> usize {
    let Some(op) = ops.get(at) else {
        return DECLINED;
    };
    let mut ip = at + 1;
    match *op {
        // `ZEND_FETCH_CONSTANT`, `ZEND_FETCH_CLASS_CONSTANT`,
        // `ZEND_FETCH_STATIC_PROP_R`/`_IS`, `ZEND_ASSIGN_STATIC_PROP` and
        // `ZEND_ISSET_ISEMPTY_STATIC_PROP` on a run-time cache hit; the
        // `To` forms skip the folded `AssignCv` when they stored into its
        // variable.
        Op::FetchConst { name, dst, .. } => {
            if spec::fetch_const_in(f, name, dst, None).is_none() {
                return DECLINED;
            }
        }
        Op::FetchConstTo { name, dst, to, .. } => {
            match spec::fetch_const_in(f, name, dst, Some(to)) {
                Some(true) => ip += 1,
                Some(false) => {}
                None => return DECLINED,
            }
        }
        Op::FetchClassConst {
            ref class,
            name: Operand::Lit(name),
            dst,
        } => {
            if spec::fetch_class_const_in(f, class, name, dst, None).is_none() {
                return DECLINED;
            }
        }
        Op::FetchClassConstTo {
            ref class,
            name,
            dst,
            to,
        } => match spec::fetch_class_const_in(f, class, name, dst, Some(to)) {
            Some(true) => ip += 1,
            Some(false) => {}
            None => return DECLINED,
        },
        // `ZEND_FETCH_THIS` in a method (without `$this`, the generic op
        // throws).
        Op::FetchThis { dst } => {
            let Some(this) = f.this.clone() else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Object(this));
        }
        Op::FetchStaticProp {
            place,
            dst,
            to,
            mode,
        } => match spec::fetch_static_prop_in(f, place, dst, to, mode) {
            Some(true) => ip += 1,
            Some(false) => {}
            None => return DECLINED,
        },
        Op::AssignStaticProp { place, val, dst } => {
            if !spec::assign_static_prop_in(f, place, val, dst) {
                return DECLINED;
            }
        }
        Op::IssetStaticProp { place, dst, empty } => {
            if !spec::isset_static_prop_in(f, place, dst, empty) {
                return DECLINED;
            }
        }
        Op::FetchObjProp { .. }
        | Op::AssignObjProp { .. }
        | Op::AssignOpObjProp { .. }
        | Op::IncDecObjProp { .. }
        | Op::IssetObjProp { .. }
        | Op::New { .. } => {
            if !spec::prop_op_in(f, op, &mut ip) {
                return DECLINED;
            }
        }
        // `ZEND_FUNC_NUM_ARGS`.
        Op::FuncNumArgs { dst } => {
            if let Some(d) = dst {
                let n = i64::try_from(f.num_args()).unwrap_or(i64::MAX);
                set_tmp_in(f, d, Value::Int(n));
            }
        }
        // `ZEND_STRLEN` of a string, `ZEND_COUNT` of an array.
        Op::Strlen { a, dst } => {
            let Some(n) = strlen_in(f, lits, a) else {
                return DECLINED;
            };
            set_dst_in(f, dst, Value::Int(n));
        }
        Op::Count { a, dst, .. } => {
            let Some(n) = count_in(f, lits, a) else {
                return DECLINED;
            };
            set_dst_in(f, dst, Value::Int(n));
        }
        // `ZEND_TYPE_CHECK` (`is_int()` & co.) of a value read without
        // a diagnostic.
        Op::TypeCheck { a, mask, dst } => {
            let Some(r) = type_check_in(f, lits, a, mask) else {
                return DECLINED;
            };
            set_dst_in(f, dst, Value::Bool(r));
        }
        // `ZEND_ISSET_ISEMPTY_CV`.
        Op::Isset { place, dst } => {
            let Some(r) = isset_cv_in(f, place, false) else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Bool(r));
        }
        Op::Empty { place, dst } => {
            let Some(r) = isset_cv_in(f, place, true) else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Bool(r));
        }
        // `ZEND_INIT_FCALL` on a run-time cache hit and `ZEND_SEND_VAL` /
        // `ZEND_SEND_VAR` of a positional by-value argument: the pending
        // call lives in the frame (`Vm::init_fcall_fast`, `Vm::send_fast`).
        Op::InitFCall {
            name,
            fallback: None,
            ..
        } => {
            if !crate::call::init_fcall_in(f, name) {
                return DECLINED;
            }
        }
        Op::Send { ref src } => {
            if !crate::call::send_in(f, src) {
                return DECLINED;
            }
        }
        // `ZEND_FREE` of a temporary whose release has no side effect.
        Op::Free(Operand::Tmp(t)) => {
            let Some(slot) = f.tmps.get_mut(t as usize) else {
                return DECLINED;
            };
            if !is_plain_slot(Some(&*slot)) {
                return DECLINED;
            }
            discard_unrooted(Some(std::mem::take(slot)));
        }
        // Array dimensions of a variable and `.=` on one
        // (`exec/spec/dims.rs`). With `to`, the `AssignCv` after a
        // dimension fetch is done too (`Some(true)`: skip it).
        Op::FetchDimCv {
            cv, dim, dst, to, ..
        } => match fetch_dim_cv_in(f, cv, dim, dst, to) {
            Some(true) => ip += 1,
            Some(false) => {}
            None => return DECLINED,
        },
        Op::FetchDimsCv {
            cv, place, dst, to, ..
        } => match fetch_dims_cv_in(f, cv, place, dst, to) {
            Some(true) => ip += 1,
            Some(false) => {}
            None => return DECLINED,
        },
        Op::IssetDimCv {
            cv,
            dim,
            dst,
            empty,
            ..
        } => {
            if !isset_dim_cv_in(f, cv, dim, dst, empty) {
                return DECLINED;
            }
        }
        Op::AssignDimCv { cv, dim, val, .. } => {
            if !assign_dim_cv_in(f, cv, dim, val) {
                return DECLINED;
            }
        }
        Op::AssignDimsCv { cv, val, place } => {
            if !assign_dims_cv_in(f, cv, place, val) {
                return DECLINED;
            }
        }
        Op::AssignOpDimCv {
            cv,
            dim,
            op: kind,
            val,
            ..
        } => {
            if !assign_op_dim_cv_in(f, cv, dim, kind, val) {
                return DECLINED;
            }
        }
        Op::AssignOpDimsCv {
            cv,
            op: kind,
            val,
            place,
        } => {
            if !assign_op_dims_cv_in(f, cv, kind, place, val) {
                return DECLINED;
            }
        }
        Op::IncDecDimCv {
            cv, dim, kind, dst, ..
        } => {
            let Some(r) = incdec_dim_cv_in(f, cv, dim, kind) else {
                return DECLINED;
            };
            // The result is an int or a float: an unused one needs no
            // release.
            if let Some(d) = dst {
                set_tmp_in(f, d, r);
            }
        }
        Op::ConcatCv { cv, val, dst, .. } => {
            if !concat_cv_in(f, cv, val, dst) {
                return DECLINED;
            }
        }
        // `(string)` of a string or an integer.
        Op::Cast {
            kind: CastKind::String,
            a,
            dst,
        } => {
            if f.tmps.len() <= dst as usize {
                return DECLINED;
            }
            let Some(s) = cast_string_in(f, lits, a) else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Str(s));
        }
        // `ZEND_ROPE_END` of strings and integers.
        Op::Rope { ref parts, dst } => {
            if f.tmps.len() <= dst as usize {
                return DECLINED;
            }
            let Some(s) = rope_in(f, lits, parts) else {
                return DECLINED;
            };
            set_tmp_in(f, dst, Value::Str(s));
        }
        // `foreach` by value over an array: the next element, or the
        // jump out of the loop at the end.
        Op::FeFetch {
            iter,
            end,
            val,
            key,
        } => match fe_fetch_array_in(f, iter, val, key) {
            // The assignments of the value and key that follow run here
            // when they are plain `AssignCv`s (`fe_assign_in`).
            Some(true) => {
                let after = fe_assign_in(f, ops, ip, val);
                if after != ip {
                    if let Some(k) = key {
                        ip = fe_assign_in(f, ops, after, k);
                    } else {
                        ip = after;
                    }
                }
            }
            Some(false) => ip = end as usize,
            None => return DECLINED,
        },
        _ => return DECLINED,
    }
    ip
}

/// [`concat_in`] (`ZEND_CONCAT` of strings and integers, for the `Bin`
/// and `BinAssignCv` arms of [`step`]) out of line: it allocates, so a call
/// costs it little, and inlined twice into the loop of the core ops it would
/// take registers from the numeric arms.
#[inline(never)]
fn concat_ext(f: &mut Frame, lits: &[Value], a: Operand, b: Operand) -> Option<ZStr> {
    concat_in(f, lits, a, b)
}

/// Stores the scalar result `v` of an op into its result temporary when
/// the result is used ([`set_tmp_in`]); an unused int or bool owns nothing.
#[inline(always)]
#[allow(clippy::inline_always)]
fn set_dst_in(f: &mut Frame, dst: Option<u32>, v: Value) {
    if let Some(t) = dst {
        set_tmp_in(f, t, v);
    }
}

/// `strlen()` of an operand holding a string, read as `Vm::read` reads it
/// (a temporary is consumed and its string released): a defined compiled
/// variable, a literal or a temporary. `None` for anything else (a
/// reference, an undefined variable, other types: the generic op).
#[inline(always)]
#[allow(clippy::inline_always)]
fn strlen_in(f: &mut Frame, lits: &[Value], a: Operand) -> Option<i64> {
    let n = match a {
        Operand::Cv(cv) => match f.cvs.get(cv as usize).and_then(CvSlot::get)? {
            Value::Str(s) => s.len(),
            _ => return None,
        },
        Operand::Lit(i) => match lits.get(i as usize)? {
            Value::Str(s) => s.len(),
            _ => return None,
        },
        Operand::TmpKeep(t) => match f.tmps.get(t as usize)? {
            Value::Str(s) => s.len(),
            _ => return None,
        },
        Operand::Tmp(t) => {
            let slot = f.tmps.get_mut(t as usize)?;
            let Value::Str(s) = slot else {
                return None;
            };
            let n = s.len();
            drop(std::mem::take(slot));
            n
        }
        Operand::Unused => return None,
    };
    Some(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `count()` / `sizeof()` of an operand holding an array, read as
/// `Vm::read` reads it (a temporary is consumed and its array released,
/// as `Vm::op_count_slow` releases it). `None` for anything else.
#[inline(always)]
#[allow(clippy::inline_always)]
fn count_in(f: &mut Frame, lits: &[Value], a: Operand) -> Option<i64> {
    let n = match a {
        Operand::Cv(cv) => match f.cvs.get(cv as usize).and_then(CvSlot::get)? {
            Value::Array(arr) => arr.len(),
            _ => return None,
        },
        Operand::Lit(i) => match lits.get(i as usize)? {
            Value::Array(arr) => arr.len(),
            _ => return None,
        },
        Operand::TmpKeep(t) => match f.tmps.get(t as usize)? {
            Value::Array(arr) => arr.len(),
            _ => return None,
        },
        Operand::Tmp(t) => {
            let slot = f.tmps.get_mut(t as usize)?;
            let Value::Array(arr) = slot else {
                return None;
            };
            let n = arr.len();
            drop(std::mem::take(slot));
            n
        }
        Operand::Unused => return None,
    };
    Some(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Whether the type of operand `a` is one of the `mask` bits
/// ([`crate::opcode::type_mask::matches`]), read without a diagnostic: a
/// defined compiled variable of a frame without an attached symbol table,
/// a literal, or a temporary (consumed, as `Vm::read` consumes it).
/// `None` for an undefined variable (the generic op warns).
#[inline(always)]
#[allow(clippy::inline_always)]
fn type_check_in(f: &mut Frame, lits: &[Value], a: Operand, mask: u8) -> Option<bool> {
    use crate::opcode::type_mask::matches;
    match a {
        Operand::Cv(cv) if !f.symtab_attached => {
            Some(matches(mask, f.cvs.get(cv as usize).and_then(CvSlot::get)?))
        }
        Operand::Lit(i) => Some(matches(mask, lits.get(i as usize)?)),
        Operand::TmpKeep(t) => Some(matches(mask, f.tmps.get(t as usize)?)),
        Operand::Tmp(t) => {
            let v = std::mem::take(f.tmps.get_mut(t as usize)?);
            let r = matches(mask, &v);
            discard_unrooted(Some(v));
            Some(r)
        }
        Operand::Cv(_) | Operand::Unused => None,
    }
}

/// `isset($cv)` / `empty($cv)` (`ZEND_ISSET_ISEMPTY_CV`, which never
/// warns) of a plain compiled variable of a frame without an attached
/// symbol table, as `Vm::place_isset` decides it: undefined is not set
/// and empty; a value (behind a reference: the referenced one) is set
/// unless null, and empty when [`plain_truth`] says false. `None` for any
/// other place and for a value whose truth needs a conversion (objects:
/// the generic op).
#[inline(always)]
#[allow(clippy::inline_always)]
fn isset_cv_in(f: &Frame, place: crate::opcode::PlaceId, empty: bool) -> Option<bool> {
    if f.symtab_attached {
        return None;
    }
    let p = f.code.places.get(place as usize)?;
    let crate::opcode::PlaceBase::Cv(cv) = &p.base else {
        return None;
    };
    if !p.segs.is_empty() {
        return None;
    }
    let decide = |v: &Value| {
        if empty {
            plain_truth(v).map(|b| !b)
        } else {
            Some(!matches!(v, Value::Null))
        }
    };
    match f.cvs.get(*cv as usize)?.get() {
        None => Some(empty),
        Some(Value::Reference(r)) => decide(&*r.try_borrow()?),
        Some(v) => decide(v),
    }
}

/// Copies compiled variable `cv`'s new value into the attached symbol
/// table (`Frame::sync_cv_to_symtab`), out of line: callers test
/// `f.symtab_attached` first, so the hot path builds no `Value` for it.
#[cold]
#[inline(never)]
fn sync_cv_cold(f: &Frame, cv: u32) {
    if let Some(v) = f.cvs.get(cv as usize).and_then(CvSlot::get_clone) {
        f.sync_cv_to_symtab(cv, &v);
    }
}

/// A copy of `v`: a scalar inline, anything else through `Value::clone`
/// (out of line).
#[inline(always)]
#[allow(clippy::inline_always)]
fn scalar_copy(v: &Value) -> Value {
    match v {
        Value::Int(i) => Value::Int(*i),
        Value::Float(x) => Value::Float(*x),
        Value::Bool(b) => Value::Bool(*b),
        Value::Null => Value::Null,
        other => other.clone(),
    }
}

/// Stores `v` into temporary `t` of `f` ([`put_tmp`]); an out-of-range
/// slot is ignored, as the pre-match did.
#[inline(always)]
#[allow(clippy::inline_always)]
fn set_tmp_in(f: &mut Frame, t: u32, v: Value) {
    if let Some(slot) = f.tmps.get_mut(t as usize) {
        put_tmp(slot, v);
    }
}

/// The boolean of a jump operand without conversion (`Vm::take_tmp_bool`):
/// a temporary holding a bool (consumed), or a compiled variable holding a
/// plain value (tested in place). `None` for the generic `jump_cond`.
#[inline(always)]
#[allow(clippy::inline_always)]
fn take_bool(f: &mut Frame, op: Operand) -> Option<bool> {
    match op {
        Operand::Tmp(t) => {
            let slot = f.tmps.get_mut(t as usize)?;
            if let Value::Bool(b) = *slot {
                // A bool has nothing to drop: overwrite it without the
                // out-of-line `drop_in_place`.
                std::mem::forget(std::mem::take(slot));
                Some(b)
            } else {
                None
            }
        }
        Operand::Cv(cv) => f.peek_cv(cv, slot_truth),
        _ => None,
    }
}

/// The truth of an operand that needs neither a diagnostic nor a
/// conversion (`!a`, `(bool) a`): a temporary holding null, a bool, an int
/// or a non-NaN float (consumed, as `read` does: nothing to release), a
/// kept temporary, a defined compiled variable or a literal holding a value
/// [`plain_truth`] decides. `None` for the generic read and `to_bool`.
#[inline(always)]
#[allow(clippy::inline_always)]
fn operand_truth(f: &mut Frame, op: Operand) -> Option<bool> {
    match op {
        Operand::Tmp(t) => {
            let slot = f.tmps.get_mut(t as usize)?;
            let b = match *slot {
                Value::Null => false,
                Value::Bool(b) => b,
                Value::Int(i) => i != 0,
                Value::Float(x) if !x.is_nan() => x != 0.0,
                _ => return None,
            };
            // A scalar: nothing to drop.
            std::mem::forget(std::mem::take(slot));
            Some(b)
        }
        Operand::TmpKeep(t) => plain_truth(f.tmps.get(t as usize)?),
        Operand::Cv(cv) => f.peek_cv(cv, slot_truth),
        Operand::Lit(i) => plain_truth(f.code.literals.get(i as usize)?),
        Operand::Unused => None,
    }
}

/// The number an operand holds (`Vm::peek_num`), with the literals at
/// hand: a compiled variable, temporary or literal holding an int or a
/// float, read in place.
#[inline(always)]
#[allow(clippy::inline_always)]
fn num_of(f: &Frame, lits: &[Value], op: Operand) -> Option<Num> {
    // A variable or a literal (nearly every numeric operand) by tag tests
    // and predicted branches, not through the operand kind's jump table.
    let v = if let Operand::Cv(cv) = op {
        f.cvs.get(cv as usize).and_then(CvSlot::get)?
    } else if let Operand::Lit(i) = op {
        lits.get(i as usize)?
    } else {
        match op {
            Operand::Tmp(t) | Operand::TmpKeep(t) => f.tmps.get(t as usize)?,
            Operand::Cv(_) | Operand::Lit(_) | Operand::Unused => return None,
        }
    };
    match v {
        Value::Int(i) => Some(Num::Int(*i)),
        Value::Float(x) => Some(Num::Float(*x)),
        _ => None,
    }
}

/// The int an operand holds when it is a compiled variable or a literal
/// holding an int, read in place; `None` for anything else (the caller's
/// general [`num_of`] path).
#[inline(always)]
#[allow(clippy::inline_always)]
fn int_of(f: &Frame, lits: &[Value], op: Operand) -> Option<i64> {
    let v = if let Operand::Cv(cv) = op {
        f.cvs.get(cv as usize).and_then(CvSlot::get)?
    } else if let Operand::Lit(i) = op {
        lits.get(i as usize)?
    } else {
        return None;
    };
    if let Value::Int(i) = *v {
        Some(i)
    } else {
        None
    }
}

/// The [`cmp_outcome`] bit of comparing two ints (`LESS`, `EQUAL` or
/// `GREATER`; the identity flag makes no difference between two ints), as
/// [`num_outcome`] computes it.
#[inline(always)]
#[allow(clippy::inline_always)]
fn int_outcome(x: i64, y: i64) -> u8 {
    1 << (u8::from(x > y) + u8::from(x >= y))
}

/// The jump decision of the [`Op::CmpJmp`] (operands `a`, `b`, `mask`)
/// right after an [`Op::IncDecCvCmpJmp`] that left `new` in compiled
/// variable `cv`; `None` when an operand is not a plain number (the
/// `CmpJmp` then runs as its own op). The usual loop condition (`$i < $n`,
/// `$i <= 100`: the variable on the left, ints on both sides) is decided
/// by one integer comparison, without [`Num`]'s kind dispatch; anything
/// else as [`jump_of`] decides it. `b` naming `cv` itself reads the slot,
/// which already holds `new`.
#[inline(always)]
#[allow(clippy::inline_always, clippy::many_single_char_names)]
fn step_cond(
    f: &Frame,
    lits: &[Value],
    cv: u32,
    new: Num,
    a: Operand,
    b: Operand,
    mask: u8,
) -> Option<bool> {
    let known = Operand::Cv(cv);
    if let Num::Int(x) = new {
        if a == known {
            if let Some(y) = int_of(f, lits, b) {
                return Some((mask & int_outcome(x, y)) != 0);
            }
        }
    }
    let x = if a == known { new } else { num_of(f, lits, a)? };
    let y = if b == known { new } else { num_of(f, lits, b)? };
    Some(jump_of(x, y, mask))
}

/// `+`, `-`, `*` and `/` (by anything but zero) of two floats or of an
/// int and a float (php-src's `(double)` widening of the int), and `+`,
/// `-`, `*` of two ints whose result does not overflow: the operators of
/// nearly all numeric loops, chosen by direct tests of the operator
/// instead of [`num_bin`]'s two-level dispatch over [`BinKind`] and
/// [`BinaryOp`]. The same results as `num_bin`; `None` for every other
/// case (an overflow, a zero divisor, an int `/`, other operators), which
/// `num_bin` then decides (out of line: [`bin_tmp_other`] & co.).
///
/// The result is a [`Num`], not a `Value`: the callers store it with
/// [`put_num_tmp`] / [`put_num_cv`], a tag and a payload store into the
/// destination slot, where a `Value` result merged with `num_bin`'s (whose
/// booleans keep their payload in another byte) was assembled in a stack
/// temporary and copied into the slot as 16 bytes.
#[inline(always)]
#[allow(clippy::inline_always)]
fn arith_num(op: BinKind, x: Num, y: Num) -> Option<Num> {
    #[allow(clippy::cast_precision_loss)] // php-src's (double) widening
    let (p, q) = match (x, y) {
        (Num::Float(p), Num::Float(q)) => (p, q),
        (Num::Int(p), Num::Int(q)) => {
            return if matches!(op, BinKind::Arith(BinaryOp::Add)) {
                p.checked_add(q).map(Num::Int)
            } else if matches!(op, BinKind::Arith(BinaryOp::Sub)) {
                p.checked_sub(q).map(Num::Int)
            } else if matches!(op, BinKind::Arith(BinaryOp::Mul)) {
                p.checked_mul(q).map(Num::Int)
            } else {
                None
            };
        }
        (Num::Int(p), Num::Float(q)) => (p as f64, q),
        (Num::Float(p), Num::Int(q)) => (p, q as f64),
    };
    if matches!(op, BinKind::Arith(BinaryOp::Mul)) {
        Some(Num::Float(p * q))
    } else if matches!(op, BinKind::Arith(BinaryOp::Add)) {
        Some(Num::Float(p + q))
    } else if matches!(op, BinKind::Arith(BinaryOp::Sub)) {
        Some(Num::Float(p - q))
    } else if matches!(op, BinKind::Arith(BinaryOp::Div)) && q != 0.0 {
        // `div_function` with a double operand; a zero divisor (also
        // -0.0) throws "Division by zero" on the generic path.
        Some(Num::Float(p / q))
    } else {
        None
    }
}

/// Stores number `n` into temporary slot `slot`, as [`put_tmp`] does: over
/// an old null, bool, int or float (nothing to release; nearly always, a
/// temporary being consumed by its reader) by the tag and payload stores
/// alone, else through `put_tmp` out of line.
#[inline(always)]
#[allow(clippy::inline_always)]
fn put_num_tmp(slot: &mut Value, n: Num) {
    if matches!(
        *slot,
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    ) {
        let old = match n {
            Num::Int(i) => std::mem::replace(slot, Value::Int(i)),
            Num::Float(x) => std::mem::replace(slot, Value::Float(x)),
        };
        // A scalar: nothing to drop.
        std::mem::forget(old);
    } else {
        put_tmp_other(slot, num_value(n));
    }
}

/// [`put_tmp`] over an old value that may own something (out of line).
#[inline(never)]
fn put_tmp_other(slot: &mut Value, v: Value) {
    put_tmp(slot, v);
}

/// Stores number `n` into compiled variable slot `slot` when that is
/// undefined or holds a plain value ([`is_plain_slot`]), releasing the old
/// value as `discard_unrooted` does: `true`. An undefined or scalar old
/// value is overwritten by the tag and payload stores alone; an old string
/// is released out of line. `false`, nothing changed, for any other slot.
#[inline(always)]
#[allow(clippy::inline_always)]
fn put_num_cv(slot: &mut Option<Value>, n: Num) -> bool {
    match *slot {
        None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)) => {
            let old = match n {
                Num::Int(i) => slot.replace(Value::Int(i)),
                Num::Float(x) => slot.replace(Value::Float(x)),
            };
            // Undefined or a scalar: nothing to drop.
            std::mem::forget(old);
            true
        }
        Some(Value::Str(_)) => {
            replace_plain_cv(slot, num_value(n));
            true
        }
        _ => false,
    }
}

/// Replaces the plain value of a compiled variable slot (a string: released
/// at once), out of line.
#[inline(never)]
fn replace_plain_cv(slot: &mut Option<Value>, v: Value) {
    discard_unrooted(slot.replace(v));
}

/// The jump decision of a smart branch on two plain numbers: the
/// comparison's [`cmp_outcome`] bit against the op's mask.
#[inline(always)]
#[allow(clippy::inline_always, clippy::many_single_char_names)]
fn cmp_num(f: &Frame, lits: &[Value], a: Operand, b: Operand, mask: u8) -> Option<bool> {
    let x = num_of(f, lits, a)?;
    let y = num_of(f, lits, b)?;
    Some(jump_of(x, y, mask))
}

/// Whether a smart branch with `mask` jumps on the comparison of `x` with
/// `y`.
#[inline(always)]
#[allow(clippy::inline_always)]
pub(super) fn jump_of(x: Num, y: Num, mask: u8) -> bool {
    let identity = (mask & cmp_outcome::IDENTITY) != 0;
    (mask & num_outcome(x, y, identity)) != 0
}

/// `Vm::equality_fast` on frame `f`: `==`, `!=`, `===`, `!==` decided
/// without conversions (out of line); `None` at once for other operators.
#[inline(always)]
#[allow(clippy::inline_always)]
fn equality(f: &mut Frame, op: BinKind, a: Operand, b: Operand) -> Option<bool> {
    if matches!(
        op,
        BinKind::Equal | BinKind::NotEqual | BinKind::Identical | BinKind::NotIdentical
    ) {
        cmp_simple_in(f, op, a, b)
    } else {
        None
    }
}

/// [`Op::Bin`] on two plain numbers, the result written into temporary
/// `dst` (`ZEND_ADD_LONG_SPEC` & co.). `false`, nothing changed, when an
/// operand is not a plain number or the operator has no numeric form.
#[inline(always)]
#[allow(clippy::inline_always, clippy::many_single_char_names)]
fn bin_tmp(f: &mut Frame, lits: &[Value], op: BinKind, a: Operand, b: Operand, dst: u32) -> bool {
    let Some(x) = num_of(f, lits, a) else {
        return false;
    };
    let Some(y) = num_of(f, lits, b) else {
        return false;
    };
    // Reading the operands changed nothing: a missing slot can decline
    // before the result is computed.
    let Some(slot) = f.tmps.get_mut(dst as usize) else {
        return false;
    };
    match arith_num(op, x, y) {
        Some(n) => {
            put_num_tmp(slot, n);
            true
        }
        None => bin_tmp_other(slot, op, x, y),
    }
}

/// [`bin_tmp`] for what [`arith_num`] leaves to [`num_bin`] (other
/// operators, mixed kinds, an int overflow), out of line: `num_bin` is two
/// jump tables over the operator and many arms, which inline would grow
/// the loop of the core ops and the register pressure of that loop for every op.
#[inline(never)]
fn bin_tmp_other(slot: &mut Value, op: BinKind, x: Num, y: Num) -> bool {
    let Some(r) = num_bin(op, x, y) else {
        return false;
    };
    put_tmp(slot, r);
    true
}

/// Stores the scalar `v` into compiled variable `cv` when that is
/// undefined or holds a plain value ([`is_plain_slot`]): `true`. `false`,
/// nothing changed (`v` is a scalar: nothing to give back), otherwise.
#[inline(always)]
#[allow(clippy::inline_always)]
fn store_plain(f: &mut Frame, cv: u32, v: Value) -> bool {
    let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) else {
        return false;
    };
    if !is_plain_slot(slot.as_ref()) {
        return false;
    }
    discard_unrooted(slot.replace(v));
    if f.symtab_attached {
        sync_cv_cold(f, cv);
    }
    true
}

/// [`Op::BinAssignCv`] on two plain numbers: the result goes straight
/// into compiled variable `cv` when that holds no refcounted value (`Some(true)`), else into `dst` for the `AssignCv`
/// after the op (`Some(false)`). `None`, nothing changed, as for
/// [`bin_tmp`].
#[inline(always)]
#[allow(clippy::inline_always, clippy::many_single_char_names)]
fn bin_assign_cv(
    f: &mut Frame,
    lits: &[Value],
    op: BinKind,
    a: Operand,
    b: Operand,
    cv: u32,
    dst: u32,
) -> Option<bool> {
    let x = num_of(f, lits, a)?;
    let y = num_of(f, lits, b)?;
    let Some(n) = arith_num(op, x, y) else {
        return bin_assign_cv_other(f, op, x, y, cv, dst);
    };
    if let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) {
        if put_num_cv(slot, n) {
            if f.symtab_attached {
                sync_cv_cold(f, cv);
            }
            return Some(true);
        }
    }
    let slot = f.tmps.get_mut(dst as usize)?;
    put_num_tmp(slot, n);
    Some(false)
}

/// [`bin_assign_cv`] for what [`arith_num`] leaves to [`num_bin`], out of
/// line (as [`bin_tmp_other`]).
#[inline(never)]
fn bin_assign_cv_other(
    f: &mut Frame,
    op: BinKind,
    x: Num,
    y: Num,
    cv: u32,
    dst: u32,
) -> Option<bool> {
    let r = num_bin(op, x, y)?;
    if let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) {
        if is_plain_slot(slot.as_ref()) {
            discard_unrooted(slot.replace(r));
            if f.symtab_attached {
                sync_cv_cold(f, cv);
            }
            return Some(true);
        }
    }
    let slot = f.tmps.get_mut(dst as usize)?;
    put_tmp(slot, r);
    Some(false)
}

/// `++`/`--` of compiled variable `cv` holding an int that does not
/// overflow, or a float (`ZEND_PRE_INC_LONG_SPEC_CV` & co.), in place; the
/// expression's value goes to `dst` when used. `false`, nothing changed,
/// for the generic path.
#[inline(always)]
#[allow(clippy::inline_always)]
fn incdec_cv(f: &mut Frame, cv: u32, kind: IncDecKind, dst: Option<u32>) -> bool {
    let Some((old, new)) = incdec_cv_num(f, cv, kind) else {
        return false;
    };
    // An unused result is never built (an int or a float: nothing to drop).
    if let Some(t) = dst {
        set_tmp_in(f, t, num_value(if kind.is_post() { old } else { new }));
    }
    true
}

/// The in-place `++`/`--` of [`incdec_cv`] without its result store:
/// `(old, new)` values of compiled variable `cv` (written back, and
/// copied into an attached symbol table). `None`, nothing changed, when
/// `cv` holds anything but an int that does not overflow or a float.
#[inline(always)]
#[allow(clippy::inline_always)]
fn incdec_cv_num(f: &mut Frame, cv: u32, kind: IncDecKind) -> Option<(Num, Num)> {
    let slot = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut)?;
    let r = match slot.as_mut() {
        Some(Value::Int(i)) => {
            let old = *i;
            let new = if kind.is_inc() {
                old.checked_add(1)
            } else {
                old.checked_sub(1)
            }?;
            *i = new;
            (Num::Int(old), Num::Int(new))
        }
        Some(Value::Float(x)) => {
            let old = *x;
            let new = if kind.is_inc() { old + 1.0 } else { old - 1.0 };
            *x = new;
            (Num::Float(old), Num::Float(new))
        }
        _ => return None,
    };
    if f.symtab_attached {
        sync_cv_cold(f, cv);
    }
    Some(r)
}

/// The value of number `n`.
#[inline(always)]
#[allow(clippy::inline_always)]
fn num_value(n: Num) -> Value {
    match n {
        Num::Int(i) => Value::Int(i),
        Num::Float(x) => Value::Float(x),
    }
}

/// `$cv op= val` in place when both hold plain numbers and the result has
/// a numeric form (`ZEND_ASSIGN_OP_SPEC_CV_*` with the `IS_LONG` /
/// `IS_DOUBLE` paths of `zend_binary_op`), the result also written to
/// `dst` when used. `false`, nothing changed, for
/// the generic `AssignOp`.
#[inline(always)]
#[allow(clippy::inline_always)]
fn assign_op_cv(
    f: &mut Frame,
    lits: &[Value],
    cv: u32,
    op: BinaryOp,
    val: Operand,
    dst: Option<u32>,
) -> bool {
    // The operand first: it may be this very variable (`$x += $x`).
    let Some(y) = num_of(f, lits, val) else {
        return false;
    };
    let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) else {
        return false;
    };
    let x = match *slot {
        Some(Value::Int(i)) => Num::Int(i),
        Some(Value::Float(v)) => Num::Float(v),
        _ => return false,
    };
    let Some(n) = arith_num(BinKind::Arith(op), x, y) else {
        return assign_op_cv_other(f, cv, op, x, y, dst);
    };
    // The old value is an int or a float: nothing to release.
    let old = match n {
        Num::Int(i) => slot.replace(Value::Int(i)),
        Num::Float(v) => slot.replace(Value::Float(v)),
    };
    std::mem::forget(old);
    if f.symtab_attached {
        sync_cv_cold(f, cv);
    }
    if let Some(t) = dst {
        if let Some(s) = f.tmps.get_mut(t as usize) {
            put_num_tmp(s, n);
        }
    }
    true
}

/// [`assign_op_cv`] for what [`arith_num`] leaves to [`num_bin`], out of
/// line (as [`bin_tmp_other`]); `x` is the number compiled variable `cv`
/// holds.
#[inline(never)]
fn assign_op_cv_other(
    f: &mut Frame,
    cv: u32,
    op: BinaryOp,
    x: Num,
    y: Num,
    dst: Option<u32>,
) -> bool {
    let Some(r) = num_bin(BinKind::Arith(op), x, y) else {
        return false;
    };
    let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) else {
        return false;
    };
    let keep = dst.map(|_| scalar_copy(&r));
    // The old value is an int or a float: nothing to release.
    discard_unrooted(slot.replace(r));
    if f.symtab_attached {
        sync_cv_cold(f, cv);
    }
    if let (Some(t), Some(k)) = (dst, keep) {
        set_tmp_in(f, t, k);
    }
    true
}

/// `$cv = val` in place (`Vm::assign_cv_fast`, `ZEND_ASSIGN_SPEC_CV_*`):
/// `val` is a literal, a temporary or a defined compiled variable, not a
/// reference (a variable's reference is dereferenced and copied,
/// `ZVAL_COPY_DEREF`), and the variable is undefined or holds no
/// reference. An old array or object is released as `Vm::cv_assign` does
/// (a possible GC root, out of line), unless the frame's symbol table is
/// attached (the generic assignment detaches the slot first). `false`,
/// nothing changed, for the generic assignment.
#[inline(always)]
#[allow(clippy::inline_always)]
pub(super) fn assign_cv_in(f: &mut Frame, cv: u32, val: Operand, dst: Option<u32>) -> bool {
    let rooted = match f.cvs.get(cv as usize).map(CvSlot::get) {
        Some(Some(Value::Array(_) | Value::Object(_))) if !f.symtab_attached => true,
        Some(Some(Value::Reference(_) | Value::Array(_) | Value::Object(_))) | None => {
            return false;
        }
        Some(_) => false,
    };
    let v = match val {
        // Consumed, as `read` does; a reference held by the temporary is
        // left to the generic path (it derefs and releases it).
        Operand::Tmp(t) => match f.tmps.get_mut(t as usize) {
            Some(s) if !matches!(s, Value::Reference(_)) => std::mem::take(s),
            _ => return false,
        },
        Operand::TmpKeep(t) => match f.tmps.get(t as usize) {
            Some(s) if !matches!(s, Value::Reference(_)) => scalar_copy(s),
            _ => return false,
        },
        Operand::Lit(i) => match f.code.literals.get(i as usize) {
            Some(l) if !matches!(l, Value::Reference(_)) => scalar_copy(l),
            _ => return false,
        },
        Operand::Cv(src) => match f.cvs.get(src as usize).and_then(CvSlot::get) {
            Some(Value::Reference(r)) => r.get(),
            Some(s) => scalar_copy(s),
            None => return false,
        },
        Operand::Unused => return false,
    };
    // The result first, as the generic assignment stores it before the
    // variable (an old value released there may queue a destructor).
    if let Some(d) = dst {
        set_tmp_in(f, d, scalar_copy(&v));
    }
    let Some(slot) = f.cvs.get_mut(cv as usize).map(CvSlot::as_mut) else {
        return false;
    };
    let old = slot.replace(v);
    if f.symtab_attached {
        sync_cv_cold(f, cv);
    }
    if rooted {
        release_rooted(old);
    } else {
        discard_unrooted(old);
    }
    true
}

/// [`Op::Move`] when its read needs no diagnostic (`ZEND_QM_ASSIGN`): a
/// literal, a defined compiled variable (dereferenced and cloned), a
/// temporary not holding a reference (moved, or cloned when kept).
#[inline(always)]
#[allow(clippy::inline_always)]
fn move_in(f: &mut Frame, src: Operand, dst: u32) -> bool {
    let v = match src {
        Operand::Lit(i) => match f.code.literals.get(i as usize) {
            Some(v) if !matches!(v, Value::Reference(_)) => scalar_copy(v),
            _ => return false,
        },
        Operand::Cv(cv) => {
            let v = f.peek_cv(cv, |slot| match slot {
                Some(Value::Reference(r)) => Some(r.get()),
                Some(v) => Some(scalar_copy(v)),
                None => None,
            });
            let Some(v) = v else {
                return false;
            };
            v
        }
        Operand::Tmp(t) => match f.tmps.get_mut(t as usize) {
            Some(slot) if !matches!(slot, Value::Reference(_)) => std::mem::take(slot),
            _ => return false,
        },
        Operand::TmpKeep(t) => match f.tmps.get(t as usize) {
            Some(slot) if !matches!(slot, Value::Reference(_)) => scalar_copy(slot),
            _ => return false,
        },
        Operand::Unused => Value::Null,
    };
    put_tmp(&mut f.tmps[dst as usize], v);
    true
}

/// `??` on a compiled variable (`ZEND_COALESCE_SPEC_CV`), which never
/// warns: `Some(true)` (jump) with the dereferenced value cloned into
/// `dst` when it is not null, `Some(false)` when undefined or null. `None`
/// for any other operand.
#[inline(always)]
#[allow(clippy::inline_always)]
fn coalesce_in(f: &mut Frame, a: Operand, dst: u32) -> Option<bool> {
    let Operand::Cv(cv) = a else {
        return None;
    };
    let v = f.peek_cv(cv, |slot| match slot {
        None | Some(Value::Null) => None,
        Some(Value::Reference(r)) => {
            let inner = r.get();
            (!matches!(inner, Value::Null)).then_some(inner)
        }
        Some(v) => Some(v.clone()),
    });
    let Some(v) = v else {
        return Some(false);
    };
    put_tmp(&mut f.tmps[dst as usize], v);
    Some(true)
}

/// `?:` on a defined compiled variable whose truth needs no conversion
/// (`ZEND_JMP_SET_SPEC_CV`): `Some(true)` (jump) with the value cloned
/// into `dst` when truthy, `Some(false)` when falsy. `None` otherwise.
#[inline(always)]
#[allow(clippy::inline_always)]
fn jmp_set_in(f: &mut Frame, a: Operand, dst: u32) -> Option<bool> {
    let Operand::Cv(cv) = a else {
        return None;
    };
    let found = f.peek_cv(cv, |slot| {
        let v = slot?;
        Some(plain_truth(v)?.then(|| v.clone()))
    })?;
    let Some(v) = found else {
        return Some(false);
    };
    put_tmp(&mut f.tmps[dst as usize], v);
    Some(true)
}

/// `?->`'s null test on a defined compiled variable
/// (`ZEND_JMP_NULL_SPEC_CV`): `Some(true)` (jump, `dst` null) when it holds
/// null, directly or through a reference, `Some(false)` otherwise. `None`
/// for an undefined variable and any other operand.
#[inline(always)]
#[allow(clippy::inline_always)]
fn jmp_null_in(f: &mut Frame, a: Operand, dst: u32) -> Option<bool> {
    let Operand::Cv(cv) = a else {
        return None;
    };
    let null = f.peek_cv(cv, |slot| match slot? {
        Value::Null => Some(true),
        Value::Reference(r) => Some(matches!(&*r.try_borrow()?, Value::Null)),
        _ => Some(false),
    })?;
    if null {
        put_tmp(&mut f.tmps[dst as usize], Value::Null);
    }
    Some(null)
}

#[cfg(test)]
mod tests {
    use super::{
        arith_num, int_outcome, jump_of, num_bin, num_outcome, num_value, put_num_cv, put_num_tmp,
        Num,
    };
    use crate::opcode::{cmp_outcome, BinKind};
    use php_runtime::{BinaryOp, Value};

    /// The tag and bits of a numeric result, for exact comparisons (a
    /// float's sign and NaN included).
    fn bits(v: Option<Value>) -> Option<(u8, u64)> {
        match v? {
            Value::Int(i) => Some((0, u64::from_ne_bytes(i.to_ne_bytes()))),
            Value::Float(x) => Some((1, x.to_bits())),
            Value::Bool(b) => Some((2, u64::from(b))),
            _ => Some((3, 0)),
        }
    }

    // php -r 'var_dump(PHP_INT_MAX + 1, 3 * 4, 0.1 + 0.2, -0.0 * 1.0, 2 - 5);'
    #[test]
    fn arith_num_matches_num_bin() {
        let nums = [
            Num::Int(0),
            Num::Int(3),
            Num::Int(-5),
            Num::Int(i64::MAX),
            Num::Int(i64::MIN),
            Num::Float(0.1),
            Num::Float(-0.0),
            Num::Float(f64::NAN),
            Num::Float(f64::INFINITY),
        ];
        let ops = [
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Mod,
            BinaryOp::BitOr,
        ];
        for x in nums {
            for y in nums {
                for op in ops {
                    let kind = BinKind::Arith(op);
                    let slow = bits(num_bin(kind, x, y));
                    // The fast path answers only where num_bin does, and the
                    // same; `+`, `-`, `*` and `/` by a non-zero divisor of a
                    // float and a float or an int always.
                    let floats = matches!(x, Num::Float(_)) || matches!(y, Num::Float(_));
                    match arith_num(kind, x, y) {
                        Some(r) => assert_eq!(bits(Some(num_value(r))), slow),
                        None => assert!(
                            !(floats
                                && (matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                                    || (op == BinaryOp::Div && y.as_f64() != 0.0))),
                            "{op:?} of a float declined"
                        ),
                    }
                }
                for kind in [BinKind::Less, BinKind::Equal, BinKind::Identical] {
                    assert!(arith_num(kind, x, y).is_none());
                }
            }
        }
        // `PHP_INT_MAX + 1` overflows: the generic op makes it a float.
        assert!(arith_num(
            BinKind::Arith(BinaryOp::Add),
            Num::Int(i64::MAX),
            Num::Int(1)
        )
        .is_none());
        assert!(matches!(
            arith_num(BinKind::Arith(BinaryOp::Mul), Num::Int(3), Num::Int(4)),
            Some(Num::Int(12))
        ));
    }

    // The direct number stores: over a scalar or an undefined variable by
    // the tag and payload alone, over a string by releasing it, and never
    // over an array (the generic assignment's).
    #[test]
    fn number_stores() {
        let mut tmp = Value::Null;
        put_num_tmp(&mut tmp, Num::Int(7));
        assert!(matches!(tmp, Value::Int(7)));
        put_num_tmp(&mut tmp, Num::Float(-0.0));
        assert!(matches!(tmp, Value::Float(x) if x.to_bits() == (-0.0_f64).to_bits()));
        put_num_tmp(&mut tmp, Num::Int(i64::MIN));
        assert!(matches!(tmp, Value::Int(i64::MIN)));
        let mut tmp = Value::from("left in a temporary");
        put_num_tmp(&mut tmp, Num::Float(2.5));
        assert!(matches!(tmp, Value::Float(x) if x.to_bits() == 2.5_f64.to_bits()));
        let mut tmp = Value::empty_array();
        put_num_tmp(&mut tmp, Num::Int(1));
        assert!(matches!(tmp, Value::Int(1)));

        let mut cv: Option<Value> = None;
        assert!(put_num_cv(&mut cv, Num::Int(3)));
        assert!(matches!(cv, Some(Value::Int(3))));
        assert!(put_num_cv(&mut cv, Num::Float(0.5)));
        assert!(matches!(cv, Some(Value::Float(x)) if x.to_bits() == 0.5_f64.to_bits()));
        let mut cv = Some(Value::Bool(true));
        assert!(put_num_cv(&mut cv, Num::Int(-1)));
        assert!(matches!(cv, Some(Value::Int(-1))));
        let mut cv = Some(Value::from("a string"));
        assert!(put_num_cv(&mut cv, Num::Int(4)));
        assert!(matches!(cv, Some(Value::Int(4))));
        let mut cv = Some(Value::empty_array());
        assert!(!put_num_cv(&mut cv, Num::Int(4)));
        assert!(matches!(cv, Some(Value::Array(_))));
    }

    #[test]
    fn int_outcome_matches_num_outcome() {
        let ints = [i64::MIN, -1, 0, 1, 2, i64::MAX];
        for x in ints {
            for y in ints {
                for identity in [false, true] {
                    assert_eq!(
                        int_outcome(x, y),
                        num_outcome(Num::Int(x), Num::Int(y), identity)
                    );
                }
            }
        }
    }

    fn mask(op: BinKind, jmp_if: bool) -> u8 {
        cmp_outcome::for_branch(op, jmp_if).expect("a smart-branch comparison")
    }

    // `for ($i = 0; $i < 3; $i++)`: php -r 'var_dump(2 < 3, 3 < 3, 1.5 < 2, NAN < 1);'
    #[test]
    fn smart_branch_on_numbers() {
        let less = mask(BinKind::Less, true);
        assert!(jump_of(Num::Int(2), Num::Int(3), less));
        assert!(!jump_of(Num::Int(3), Num::Int(3), less));
        assert!(jump_of(Num::Float(1.5), Num::Int(2), less));
        assert!(!jump_of(Num::Float(f64::NAN), Num::Int(1), less));
        // `!=` jumps on a NaN; `1 !== 1.0` is true (a type mismatch).
        assert!(jump_of(
            Num::Float(f64::NAN),
            Num::Int(1),
            mask(BinKind::NotEqual, true)
        ));
        assert!(jump_of(
            Num::Int(1),
            Num::Float(1.0),
            mask(BinKind::NotIdentical, true)
        ));
        assert!(!jump_of(
            Num::Int(1),
            Num::Float(1.0),
            mask(BinKind::Identical, true)
        ));
        assert!(jump_of(
            Num::Int(1),
            Num::Float(1.0),
            mask(BinKind::Equal, true)
        ));
    }

    #[test]
    fn number_values() {
        assert!(matches!(num_value(Num::Int(-4)), Value::Int(-4)));
        assert!(
            matches!(num_value(Num::Float(0.5)), Value::Float(x) if x.to_bits() == 0.5_f64.to_bits())
        );
    }
}

/// Scripts through both routes of [`step`]: its inline core ops and the
/// hot ops it hands to [`step_ext`], in the same loops, and ops whose fast
/// path declines on either route. Expected output from
/// `/usr/bin/php -n` (PHP 8.3), paths normalised to `t.php`.
#[cfg(test)]
mod route_tests {
    #![allow(clippy::needless_raw_string_hashes)]

    use crate::{Vm, VmOptions};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn run(src: &str) -> String {
        let sink = Sink::default();
        let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        vm.run_script(src.as_bytes(), "t.php");
        drop(vm);
        let out = sink.0.borrow().clone();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Jumps, smart branches, arithmetic (an overflow to float included), `&&`/`||`,
    /// `??`, `?:`, `!`, `(bool)`, `.` and `++`/`--`: the inline core arms.
    #[test]
    fn core_ops_inline_in_the_loop() {
        let src = r#"<?php
$s = 0; $f = 0.5; $t = "";
for ($i = 0; $i < 6; $i++) {
    $s = $s + $i * 3 - 1;
    $f *= 1.5;
    $b = !($i % 2) && $i > 1 || $i == 5;
    $c = (bool) $i;
    $n = $i > 3 ? null : $i;
    $d = $n ?? -1;
    $e = $n ?: 'z';
    $t = $t . $i . ",";
    $u = "a" . $i;
    if ($b) { $s += 100; } else { $s -= 1; }
    $j = $i;
    $j--;
    ++$j;
    while ($j > 4) { $j -= 2; }
    echo $s, " ", $f, " ", var_export($b, true), " ", var_export($c, true), " ", $d, " ", $e, " ", $u, " ", $j, "\n";
}
echo $t, "\n";
$k = PHP_INT_MAX - 2;
for ($m = 0; $m < 4; $m++) { $k = $k + 1; }
var_dump($k);
$x = 10;
do { $x = $x - 3; } while ($x > 0);
var_dump($x, $x === -2, $x != -2.0, 1.5 <= $x);
"#;
        assert_eq!(
            run(src),
            r#"-2 0.75 false false 0 z a0 0
-1 1.125 false true 1 1 a1 1
104 1.6875 true true 2 2 a2 2
111 2.53125 false true 3 3 a3 3
222 3.796875 true true -1 z a4 4
336 5.6953125 true true -1 z a5 3
0,1,2,3,4,5,
float(9.223372036854776E+18)
int(-2)
bool(true)
bool(false)
bool(false)
"#
        );
    }

    /// Constants, class constants, static and object properties, `new`, calls
    /// and their arguments, `strlen()`, `count()`, `is_int()`, `isset`/`empty`,
    /// array dimensions, `.=`, ropes, `(string)` and `foreach` (`step_ext`),
    /// interleaved with the core ops of their loops.
    #[test]
    fn out_of_line_hot_ops_between_core_ops() {
        let src = r#"<?php
const LIMIT = 3;
class P {
    const STEP = 2;
    public static $count = 0;
    public $v = 1;
    public $items = [];
    function run() {
        for ($i = 0; $i < LIMIT; $i++) {
            $this->v += self::STEP;
            $this->v++;
            self::$count += $this->v;
            $this->items[] = $i;
            if (isset($this->v)) { echo "set "; }
        }
        return $this->v . "/" . self::$count . "/" . count($this->items) . "/" . func_num_args();
    }
}
function add($a, $b) { return $a + $b; }
$p = new P;
echo $p->run(), "\n";
$arr = [];
$str = "";
for ($i = 0; $i < 5; $i++) {
    $arr[$i] = add($i, 10);
    $arr[$i] += 1;
    $arr[$i]++;
    $m[$i % 2][$i] = $i;
    $str .= "x";
    $len = strlen($str);
    $cnt = count($arr);
    $isI = is_int($arr[$i]);
    $has = isset($arr[$i + 1]);
    $emp = empty($arr[$i]);
    $q = isset($zz);
    $r = empty($str);
    $cast = (string) $i;
    echo "{$arr[$i]}:$len:$cnt:", var_export($isI, true), ":", var_export($has, true),
        ":", var_export($emp, true), ":", var_export($q, true), ":", var_export($r, true), ":$cast\n";
    add($i, $i);
}
$sum = 0;
foreach ($arr as $k => $v) { $sum += $k * $v; }
foreach ($m as $row) { foreach ($row as $v) { echo $v; } }
echo "\n", $sum, " ", P::$count, " ", P::STEP, " ", LIMIT, "\n";
"#;
        assert_eq!(
            run(src),
            r#"set set set 10/21/3/0
12:1:1:true:false:false:false:false:0
13:2:2:true:false:false:false:false:1
14:3:3:true:false:false:false:false:2
15:4:4:true:false:false:false:false:3
16:5:5:true:false:false:false:false:4
02413
150 21 2 3
"#
        );
    }

    /// Fast paths that decline on both routes (an undefined variable, a
    /// numeric string, `strlen()` of an int, a missing property): the generic
    /// op runs, with its warning on its own line.
    #[test]
    fn declined_ops_on_both_routes_warn_on_their_line() {
        let src = r#"<?php
for ($i = 0; $i < 3; $i++) {
    $x = $i + $undef1;
    $y = $arr2[$i] ?? 'd';
    $z = strlen($i);
    $w = $nope[$i];
    echo $x, $y, $z, "\n";
}
$a = "5";
for ($i = 0; $i < 2; $i++) { $a = $a + 1; $a .= "1"; }
var_dump($a);
$o = new stdClass;
for ($i = 0; $i < 2; $i++) { $o->p = $i; echo $o->p, $o->missing ?? "-", "\n"; }
echo $o->absent;
echo "\ndone\n";
"#;
        assert_eq!(
            run(src),
            r#"
Warning: Undefined variable $undef1 in t.php on line 3

Warning: Undefined variable $nope in t.php on line 6

Warning: Trying to access array offset on null in t.php on line 6
0d1

Warning: Undefined variable $undef1 in t.php on line 3

Warning: Undefined variable $nope in t.php on line 6

Warning: Trying to access array offset on null in t.php on line 6
1d1

Warning: Undefined variable $undef1 in t.php on line 3

Warning: Undefined variable $nope in t.php on line 6

Warning: Trying to access array offset on null in t.php on line 6
2d1
string(3) "621"
0-
1-

Warning: Undefined property: stdClass::$absent in t.php on line 14

done
"#
        );
    }

    /// Plain user calls and returns made in the tape's loop (the callee's
    /// and the caller's frames switched there): recursion, an empty body,
    /// a frame pool grown by a deep recursion.
    #[test]
    fn calls_and_returns_in_the_loop() {
        assert_eq!(
            run(r#"<?php
function fibo($n) { return ($n < 2) ? 1 : fibo($n - 2) + fibo($n - 1); }
function ack($m, $n) {
    if ($m == 0) return $n + 1;
    if ($n == 0) return ack($m - 1, 1);
    return ack($m - 1, ack($m, ($n - 1)));
}
function depth($n) { return $n == 0 ? 0 : 1 + depth($n - 1); }
function id($a) { return $a; }
function none() {}
$s = 0;
for ($i = 0; $i < 50; $i++) { $s += id($i); none(); }
echo fibo(15), " ", ack(2, 3), " ", depth(3000), " ", $s, "\n";
"#),
            "987 9 3000 1225\n"
        );
    }

    /// An exception thrown two frames below a hot loop (its trace and
    /// line), caught there; generators called from a hot loop (declined:
    /// the generic call).
    #[test]
    fn exceptions_and_generators_across_switched_frames() {
        assert_eq!(
            run(r#"<?php
function inner($i) {
    if ($i == 3) throw new RuntimeException("at $i");
    return $i * 2;
}
function outer($i) { $x = $i + 1; return inner($x - 1) + 1; }
$t = 0;
for ($i = 0; $i < 6; $i++) {
    try {
        $t += outer($i);
    } catch (RuntimeException $e) {
        echo $e->getMessage(), " line ", $e->getLine(), "\n", $e->getTraceAsString(), "\n";
    }
}
echo $t, "\n";
function gen($n) { for ($k = 0; $k < $n; $k++) yield $k => $k * 10; }
$sum = 0;
for ($i = 1; $i < 4; $i++) {
    foreach (gen($i) as $k => $v) { $sum += $k + $v; }
}
echo $sum, "\n";
"#),
            r#"at 3 line 3
#0 t.php(6): inner(3)
#1 t.php(10): outer(3)
#2 {main}
29
44
"#
        );
    }

    /// Destructors of a callee's locals run at its return, before the
    /// caller goes on; `debug_backtrace()`, `func_get_args()` and return
    /// type errors in functions called from a hot loop.
    #[test]
    fn destructors_backtraces_and_return_types_after_switches() {
        assert_eq!(
            run(r#"<?php
class D {
    public function __construct(public int $n) {}
    public function __destruct() { echo "dtor {$this->n}\n"; }
}
function make($i) { $o = new D($i); return $i + 1; }
function keep($i) { return new D($i); }
for ($i = 0; $i < 3; $i++) {
    $r = make($i);
    echo "after $r\n";
}
for ($i = 5; $i < 7; $i++) {
    $o = keep($i);
    echo "kept {$o->n}\n";
}
unset($o);
function where($a, $b = 2) {
    $bt = debug_backtrace();
    return $bt[0]['line'] . ":" . $bt[0]['function'] . ":" . implode(",", $bt[0]['args']) . ":" . implode(",", func_get_args()) . ":" . func_num_args();
}
function via($i) { return where($i); }
for ($i = 0; $i < 2; $i++) {
    echo where($i), " ", via($i + 10), "\n";
}
function typed(int $a): int { return $a; }
function bad($a): int { return $a; }
for ($i = 0; $i < 3; $i++) {
    echo typed($i);
    try { echo bad($i == 2 ? "x" : $i); } catch (TypeError $e) { echo "\n", $e->getMessage(), " @", $e->getLine(), "\n"; }
}
echo "\n";
"#),
            r#"dtor 0
after 1
dtor 1
after 2
dtor 2
after 3
kept 5
dtor 5
kept 6
dtor 6
23:where:0:0:1 21:where:10:10:1
23:where:1:1:1 21:where:11:11:1
00112
bad(): Return value must be of type int, string returned @26

"#
        );
    }

    /// Calls from a callback's frame (the base frame of a nested run, which
    /// returns through the generic op), static closures, ticks between
    /// plain calls, and an exception's trace through recursive calls.
    #[test]
    fn nested_runs_ticks_and_recursive_traces() {
        assert_eq!(
            run(r#"<?php
function id($a) { return $a; }
function sq($a) { return $a * $a; }
echo implode(",", array_map(function ($x) { return id($x) + sq($x); }, [1, 2, 3])), "\n";
$f = static fn($x) => id($x) * 2;
$s = 0;
for ($i = 0; $i < 5; $i++) { $s += $f($i) + id($i); }
echo $s, "\n";
function tick() { echo "tick\n"; }
register_tick_function('tick');
declare(ticks=1) {
    for ($i = 0; $i < 2; $i++) { $v = id($i); }
}
unregister_tick_function('tick');
function rec($n) { if ($n > 0) { return rec($n - 1); } throw new LogicException("bottom"); }
try { rec(3); } catch (LogicException $e) { echo $e->getMessage(), " ", count($e->getTrace()), " ", $e->getTrace()[3]['line'], "\n"; }
"#),
            r#"2,6,12
30
tick
tick
tick
bottom 4 16
"#
        );
    }
}
