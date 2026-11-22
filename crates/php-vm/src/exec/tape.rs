// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The tape: pre-decoded, specialised instructions for the frame-local loop.
//!
//! php-src decides at compile time which handler runs an instruction:
//! `zend_vm_gen.php` generates one handler per opcode and operand-kind
//! combination (`ZEND_ADD_SPEC_TMPVARCV_CONST`, the smart-branch variants
//! `ZEND_IS_SMALLER_SPEC_TMPVARCV_TMPVARCV_JMPNZ` ...), an operand is a byte
//! offset from the frame (`EX_VAR`), and an instruction costs a handful of
//! machine instructions before its real work (ref: `Zend/zend_vm_def.h`,
//! `Zend/zend_vm_execute.h`, `Zend/zend_execute.h`).
//!
//! [`Op`] is the opposite: one generic instruction per operation, whose
//! operand kinds ([`Operand`]), sub-operation (`++` or `--`, which
//! comparison) and fused successor are data that [`super::hot::step`] decodes
//! on every execution, through bounds-checked slot vectors. Measured on
//! `for ($i = 0; $i < $n; $i++) {}`, one fused op took 95 machine
//! instructions against 30 for php-src's two handlers, 80 of them overhead.
//!
//! The tape is the same program decoded once: [`lower`] turns each `Op` of
//! an op array into an [`Inst`] at the same index, whose opcode ([`TOp`])
//! names the operation *and* the kinds of its operands, and whose operand
//! fields are byte offsets into the frame's compiled-variable or temporary
//! slots (or an immediate integer). [`run_tape`] executes it with a `match`
//! on the opcode and unchecked slot access, polling for a safe point only
//! where a jump is taken.
//!
//! Only what cannot fail is specialised: plain numbers in plain slots. An
//! instruction whose operands are anything else (a string, a reference, an
//! undefined variable, an overflow) has changed nothing when it finds out
//! and hands the op to [`super::hot::step`], the bounds-checked fast path
//! that existed before, which in turn hands what it does not handle to the
//! generic op in `Vm::run`. Ops with no specialised form are [`TOp::Step`]
//! (run by `step`) or [`TOp::Exit`] (not hot ops: back to `Vm::run`), so a
//! tape always describes the whole op array and behaviour never depends on
//! what is specialised.
//!
//! # Families
//!
//! The numeric instructions (smart branches, `+ - *`, `++`/`--`, scalar
//! assignments) are generated here by [`tape_ops!`] from tables of operand
//! kinds. Every other specialised instruction belongs to a family module,
//! which decodes its ops (`lower`) and holds their handlers; a row of the
//! `family` table of the [`tape_ops!`] invocation gives each instruction
//! its opcode, its handler, the number of ops it covers and how it ends:
//!
//! * [`scalar`]: the rest of the frame-local scalar ops (bit operations,
//!   `%`, `/`, comparisons into a temporary, `!`, truth jumps, `??`, `?:`,
//!   the fused assign-and-branch forms);
//! * [`dims`]: array elements of a variable (`$a[k]` read, write, `op=`,
//!   `++`, `isset`, `foreach`);
//! * [`props`]: object and static properties, constants;
//! * [`strs`]: strings and the builtin ops (`.`, `.=`, `strlen()`,
//!   `count()`, type tests, `isset`/`empty`, casts);
//! * [`calls`]: calls and returns of user functions.
//!
//! A handler has the contract of a hot op's fast path
//! ([`super::hot`]): it completes its op without diagnostics, user code or
//! frame changes, or it changes nothing and says so, and the op then runs
//! through [`super::hot::step`]. A handler that may release a value or
//! allocate ends with a poll for a safe point (a queued destructor, a
//! delayed `memory_limit` violation), as every hot op does.
//!
//! # Safety
//!
//! Slot access is unchecked. It is sound because [`lower`] only emits a
//! specialised instruction when every slot offset in it is below the op
//! array's slot counts (`cv_names.len()`, `num_tmps`) or literal count and
//! every jump target is inside the tape, and [`run_tape`] only runs the tape
//! on a frame whose slot vectors are at least that long ([`usable`]). The
//! tape ends in an [`TOp::Exit`] sentinel, so an instruction pointer that
//! advances past the last op stops there.

use super::hot::{self, DECLINED, LEFT, NO_SAFE_POINT, SWITCHED};
use super::Num;
use crate::opcode::{BinKind, ClassSpec, Op, OpArray, Operand};
use crate::vm::{CvSlot, Frame, Vm};
use php_runtime::{BinaryOp, Value};
use std::cell::OnceCell;
use std::rc::Rc;
use std::sync::OnceLock;

/// log2 of the size of a slot (a compiled variable, a temporary, a
/// literal): an operand's byte offset is its index shifted by this.
const SLOT_SHIFT: u32 = 4;

/// One more than the largest slot index a tape operand can name (its byte
/// offset then fits the operand field with room to spare).
const SLOT_LIMIT: u32 = 1 << 27;

// The layout the byte offsets rely on.
const _: () = {
    assert!(std::mem::size_of::<Value>() == 1 << SLOT_SHIFT);
    assert!(std::mem::size_of::<Option<Value>>() == 1 << SLOT_SHIFT);
    assert!(std::mem::size_of::<CvSlot>() == 1 << SLOT_SHIFT);
    assert!(std::mem::align_of::<CvSlot>() == std::mem::align_of::<Option<Value>>());
    assert!(std::mem::size_of::<Inst>() == 32);
};

/// An op array's tape ([`OpArray::tape`]), decoded when the op array first
/// runs in [`run_tape`].
///
/// Its buffer is kept out of `memory_get_usage()`, like the run-time cache
/// (php-src's handlers and operand offsets are part of the op array the
/// compiler allocated; decoding on first use must not show as memory the
/// script allocated between two readings).
#[derive(Default)]
pub(crate) struct Tape(OnceCell<Box<[Inst]>>);

impl Tape {
    /// The instructions, decoded from `code` on first use.
    #[inline]
    fn get(&self, code: &OpArray) -> &[Inst] {
        self.0
            .get_or_init(|| php_memlimit::persistent(|| lower(code)))
    }
}

impl Drop for Tape {
    fn drop(&mut self) {
        if let Some(buf) = self.0.take() {
            php_memlimit::persistent(|| drop(buf));
        }
    }
}

impl std::fmt::Debug for Tape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.get() {
            Some(t) => write!(f, "Tape({} instructions)", t.len()),
            None => f.write_str("Tape(not decoded)"),
        }
    }
}

/// Operand kind: a compiled variable (`a`/`b` is its byte offset among the
/// frame's compiled variables).
const KC: u8 = 0;
/// Operand kind: a temporary (byte offset among the frame's temporaries).
const KT: u8 = 1;
/// Operand kind: an integer literal that fits `i32`, stored in the operand
/// field itself.
const KI: u8 = 2;
/// Operand kind: any other numeric literal (byte offset in the literal
/// table).
const KL: u8 = 3;

/// One pre-decoded instruction (32 bytes, the size of an [`Op`] and of a
/// `zend_op`).
///
/// What the operand fields hold is the instruction's own business (its
/// family's `lower` writes them and its handler reads them), with one
/// rule: an instruction that jumps has its target in `c`, as a signed byte
/// offset from itself ([`rel`]).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inst {
    /// What to do, operand kinds included.
    op: TOp,
    /// The comparison mask of a smart branch
    /// ([`crate::opcode::cmp_outcome`]); free for other instructions.
    k: u8,
    /// A small extra operand.
    #[allow(dead_code)] // for the family modules
    e: u8,
    /// First operand.
    a: u32,
    /// Second operand.
    b: u32,
    /// Result slot (byte offset), or the jump target.
    c: u32,
    /// Fourth operand.
    #[allow(dead_code)] // for the family modules
    d: u32,
    /// Set by [`lower`], not by the families: whether `Vm::run` enters the
    /// tape at this instruction ([`worth_entering`]).
    enter: bool,
    /// Set by [`lower`]: whether the op's old fast path is
    /// `hot::step_ext` (not one of `hot::step`'s own arms), where a
    /// declined instruction hands it.
    ext: bool,
    /// A pointer-sized operand (the address of something that lives as
    /// long as the op array, such as a cache entry the tape owns).
    #[allow(dead_code)] // for the family modules
    p: usize,
}

impl Inst {
    const fn plain(op: TOp) -> Inst {
        Inst {
            op,
            k: 0,
            e: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
            enter: false,
            ext: false,
            p: 0,
        }
    }
}

/// The bases the operand offsets of a frame's instructions are relative to.
#[derive(Clone, Copy)]
struct St {
    /// The frame's compiled variables (`[Option<Value>]`).
    cvs: *mut u8,
    /// The frame's temporaries (`[Value]`).
    tmps: *mut u8,
    /// The op array's literals (`[Value]`).
    lits: *const u8,
    /// The frame itself, for what is not a slot (`$this`, the op array).
    #[allow(dead_code)] // for the family modules
    f: *mut Frame,
}

/// The compiled-variable slot at byte offset `off`.
///
/// # Safety
/// `off` is a slot offset [`lower`] validated for the frame `st` is of.
#[inline(always)]
#[allow(clippy::inline_always, clippy::cast_ptr_alignment)] // a slot offset from a slot vector's base
unsafe fn cv_slot<'a>(st: St, off: u32) -> &'a mut Option<Value> {
    // SAFETY: the caller's contract; a `CvSlot` is a transparent
    // `Option<Value>`.
    unsafe { &mut *st.cvs.add(off as usize).cast::<Option<Value>>() }
}

/// The temporary slot at byte offset `off`.
///
/// # Safety
/// As [`cv_slot`].
#[inline(always)]
#[allow(clippy::inline_always, clippy::cast_ptr_alignment)] // a slot offset from a slot vector's base
unsafe fn tmp_slot<'a>(st: St, off: u32) -> &'a mut Value {
    // SAFETY: the caller's contract.
    unsafe { &mut *st.tmps.add(off as usize).cast::<Value>() }
}

/// The slot bases of frame `f`, as [`run_tape`] derives them: what a thin
/// handler (`fam_arm!`'s `_f` modes) reads its operands through.
///
/// # Safety
/// `f` is the current frame, whose slot vectors the tape's offsets fit.
#[inline(always)]
#[allow(clippy::inline_always)] // a few loads at the top of a thin handler
unsafe fn frame_st(f: *mut Frame) -> St {
    // SAFETY: the caller's contract.
    let fr = unsafe { &mut *f };
    St {
        cvs: fr.cvs.as_mut_ptr().cast(),
        tmps: fr.tmps.as_mut_ptr().cast(),
        lits: fr.code.literals.as_ptr().cast(),
        f,
    }
}

/// The number operand `x` of kind `K` holds; `None` when it holds anything
/// else (or is an undefined variable).
///
/// # Safety
/// As [`cv_slot`]; for [`KL`], `x` is a validated literal offset.
#[inline(always)]
#[allow(
    clippy::inline_always,
    clippy::cast_possible_wrap,
    clippy::cast_ptr_alignment // a slot offset from a slot vector's base
)]
unsafe fn num<const K: u8>(st: St, x: u32) -> Option<Num> {
    // SAFETY: the caller's contract: `x` is a validated offset of the kind
    // `K` names.
    let v: &Value = unsafe {
        match K {
            KC => match &*st.cvs.add(x as usize).cast::<Option<Value>>() {
                Some(v) => v,
                None => return None,
            },
            KT => &*st.tmps.add(x as usize).cast::<Value>(),
            KI => return Some(Num::Int(i64::from(x as i32))),
            _ => &*st.lits.add(x as usize).cast::<Value>(),
        }
    };
    match v {
        Value::Int(i) => Some(Num::Int(*i)),
        Value::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

/// Whether a temporary slot holds nothing to release (a store over it is a
/// plain write).
#[inline(always)]
#[allow(clippy::inline_always)]
fn is_scalar(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    )
}

/// The scalar `v` holds, copied; `None` for anything reference-counted.
/// (Where the value is known to be a scalar, copying its 16 bytes with
/// `std::ptr::read` is cheaper: this rebuilds it variant by variant.)
#[inline(always)]
#[allow(clippy::inline_always, dead_code)] // for the family modules
fn scalar_of(v: &Value) -> Option<Value> {
    match v {
        Value::Null => Some(Value::Null),
        Value::Bool(b) => Some(Value::Bool(*b)),
        Value::Int(i) => Some(Value::Int(*i)),
        Value::Float(f) => Some(Value::Float(*f)),
        _ => None,
    }
}

/// Whether two ints compare as a smart branch whose mask has the int
/// outcomes `M` (`LESS` = 1, `EQUAL` = 2, `GREATER` = 4) takes its jump:
/// `(M & int_outcome(x, y)) != 0`, as one comparison.
#[inline(always)]
#[allow(clippy::inline_always)]
fn int_test<const M: u8>(x: i64, y: i64) -> bool {
    match M {
        1 => x < y,
        3 => x <= y,
        4 => x > y,
        6 => x >= y,
        2 => x == y,
        _ => x != y,
    }
}

macro_rules! int_arith {
    (Add, $x:expr, $y:expr) => {
        $x.checked_add($y)
    };
    (Sub, $x:expr, $y:expr) => {
        $x.checked_sub($y)
    };
    (Mul, $x:expr, $y:expr) => {
        $x.checked_mul($y)
    };
}

macro_rules! float_arith {
    (Add, $x:expr, $y:expr) => {
        $x + $y
    };
    (Sub, $x:expr, $y:expr) => {
        $x - $y
    };
    (Mul, $x:expr, $y:expr) => {
        $x * $y
    };
}

/// Why [`run_core`] stopped: the op at `ip` is not specialised, or its
/// operands are not what its instruction handles (nothing changed): it runs
/// through [`hot::step`].
const WHY_STEP: u8 = 0;
/// Why [`run_core`] stopped: the op at `ip` is not a hot op.
const WHY_EXIT: u8 = 1;
/// Why [`run_core`] stopped: the op at `ip` is a hot op outside
/// [`hot::step`]'s core arms: it runs through [`hot::step_ext`].
const WHY_EXT: u8 = 3;
/// Why [`run_core`] stopped: the op at `ip` starts a call or passes an
/// argument ([`calls`]), which [`run_tape`] runs with the VM at hand.
const WHY_CALL: u8 = 4;
/// Why [`run_core`] stopped: the jump at `at` was taken (to `ip`) and a safe
/// point is due.
const WHY_POLL: u8 = 2;

/// Where and why [`run_core`] stopped.
struct Out {
    ip: *const Inst,
    at: *const Inst,
    why: u8,
}

#[inline(always)]
#[allow(clippy::inline_always)]
fn step_out(ip: *const Inst) -> Out {
    Out {
        ip,
        at: ip,
        why: WHY_STEP,
    }
}

/// The instruction after a fused loop step that did not jump back (its
/// smart branch and jump op skipped): out of line and cold, so that the
/// compiler lays the loop's jump back out as the straight path (inline, it
/// put the jump out of line: one more instruction per iteration).
#[cold]
#[inline(never)]
fn loop_exit(ip: *const Inst) -> *const Inst {
    ip.wrapping_add(3)
}

/// Takes the jump of the instruction at `$ip` and polls for a safe point,
/// as `ZEND_VM_JMP` does (`ZEND_VM_INTERRUPT_CHECK`): the instructions
/// between two taken jumps are specialised ones, which queue no destructor
/// and allocate nothing, so only an asynchronous request (a signal, a
/// timeout) can be pending, and it is seen here.
macro_rules! take_jump {
    ($ip:ident, $inst:ident) => {{
        let at = $ip;
        #[allow(clippy::cast_possible_wrap)]
        {
            $ip = $ip.byte_offset($inst.c as i32 as isize);
        }
        if crate::interrupt::safe_point_due() {
            return Out {
                ip: $ip,
                at,
                why: WHY_POLL,
            };
        }
    }};
}

/// What a family handler that may jump decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Br {
    /// Its operands are not what it handles; nothing changed.
    Decline,
    /// Done; execution goes on after the ops the instruction covers.
    Fall,
    /// Done; execution goes on at the instruction's jump target.
    Jump,
}

/// The arm of a `family` row in [`run_core`]: `$h` is the handler, `$n`
/// the number of ops the instruction covers, and the first argument how it
/// ends:
///
/// * `plain`: `unsafe fn(St, &Inst) -> bool` (`false`: declined, nothing
///   changed); for handlers that neither release a value nor allocate;
/// * `poll`: the same, followed by a poll for a safe point (anything that
///   may release a value or allocate);
/// * `branch`: `unsafe fn(St, &Inst) -> Br`; a taken jump polls, as every
///   jump does;
/// * `branch_poll`: the same, polling on the fall-through path as well.
macro_rules! fam_arm {
    // The thin modes: the handler is called with the frame alone (`st.f`)
    // and reads any slot base from it (`frame_st`). Passed all four bases,
    // the calls raise their weight in the loop's register allocation: three
    // more such arms put the variables' base of every op on the stack.
    (plain_f, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        if !($h)($st.f, $inst) {
            return step_out($ip);
        }
        $ip = $ip.add($n);
    }};
    (poll_f, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        if !($h)($st.f, $inst) {
            return step_out($ip);
        }
        let at = $ip;
        $ip = $ip.add($n);
        if crate::interrupt::safe_point_due() {
            return Out {
                ip: $ip,
                at,
                why: WHY_POLL,
            };
        }
    }};
    (branch_f, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        match ($h)($st.f, $inst) {
            Br::Decline => return step_out($ip),
            Br::Fall => $ip = $ip.add($n),
            Br::Jump => take_jump!($ip, $inst),
        }
    }};
    (branch_poll_f, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        match ($h)($st.f, $inst) {
            Br::Decline => return step_out($ip),
            Br::Fall => {
                let at = $ip;
                $ip = $ip.add($n);
                if crate::interrupt::safe_point_due() {
                    return Out {
                        ip: $ip,
                        at,
                        why: WHY_POLL,
                    };
                }
            }
            Br::Jump => take_jump!($ip, $inst),
        }
    }};
    (plain, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        if !($h)($st, $inst) {
            return step_out($ip);
        }
        $ip = $ip.add($n);
    }};
    (poll, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        if !($h)($st, $inst) {
            return step_out($ip);
        }
        let at = $ip;
        $ip = $ip.add($n);
        if crate::interrupt::safe_point_due() {
            return Out {
                ip: $ip,
                at,
                why: WHY_POLL,
            };
        }
    }};
    (branch, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        match ($h)($st, $inst) {
            Br::Decline => return step_out($ip),
            Br::Fall => $ip = $ip.add($n),
            Br::Jump => take_jump!($ip, $inst),
        }
    }};
    (branch_poll, $h:expr, $n:literal, $st:ident, $inst:ident, $ip:ident) => {{
        match ($h)($st, $inst) {
            Br::Decline => return step_out($ip),
            Br::Fall => {
                let at = $ip;
                $ip = $ip.add($n);
                if crate::interrupt::safe_point_due() {
                    return Out {
                        ip: $ip,
                        at,
                        why: WHY_POLL,
                    };
                }
            }
            Br::Jump => take_jump!($ip, $inst),
        }
    }};
}

/// Defines [`TOp`], the opcode lookups [`lower`] uses and [`run_core`] from
/// the tables of specialised instruction families.
macro_rules! tape_ops {
    (
        cmp { $( $cn:ident = ($cka:ident, $ckb:ident, $cm:literal), )* }
        inccmp { $( $in:ident = ($iinc:literal, $ikb:ident, $im:literal), )* }
        arith_tmp { $( $an:ident = ($aop:ident, $aka:ident, $akb:ident), )* }
        arith_cv { $( $vn:ident = ($vop:ident, $vka:ident, $vkb:ident), )* }
        assign_op { $( $on:ident = ($oop:ident, $okb:ident), )* }
        family { $( $fname:ident = ($fh:expr, $fskip:literal, $fmode:ident), )* }
    ) => {
        /// A tape opcode: the operation and the kinds of its operands
        /// (`C` compiled variable, `T` temporary, `I` small integer
        /// literal, `L` other numeric literal).
        #[repr(u16)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(clippy::upper_case_acronyms)]
        pub(crate) enum TOp {
            /// Not a hot op (or the end of the tape): back to `Vm::run`.
            Exit,
            /// A core hot op with no specialised form: [`hot::step`] runs
            /// it.
            Step,
            /// Any other hot op with no specialised form:
            /// [`hot::step_ext`] runs it.
            Ext,
            /// `Op::InitFCall` without a namespace fallback ([`calls`]).
            CallInit,
            /// `Op::Send` of a value ([`calls`]).
            CallSend,
            /// `Op::Jmp`.
            Jmp,
            /// `Op::JmpZ` of a temporary holding a bool.
            JmpZT,
            /// `Op::JmpNz` of a temporary holding a bool.
            JmpNzT,
            /// `++$cv` / `$cv++`, result unused.
            IncC,
            /// `--$cv` / `$cv--`, result unused.
            DecC,
            /// `++$cv` into a temporary.
            PreIncT,
            /// `--$cv` into a temporary.
            PreDecT,
            /// `$cv++` into a temporary.
            PostIncT,
            /// `$cv--` into a temporary.
            PostDecT,
            /// `$cv = $cv2`, result unused.
            AssignC,
            /// `$cv = TMP`, result unused.
            AssignT,
            /// `$cv = <small int literal>`, result unused.
            AssignI,
            /// `$cv = <scalar literal>`, result unused.
            AssignL,
            $(
                /// Smart branch (`Op::CmpJmp`).
                $cn,
            )*
            $(
                /// `++`/`--` of a compiled variable fused with the smart
                /// branch on it (`Op::IncDecCvCmpJmp` + `Op::CmpJmp`).
                $in,
            )*
            $(
                /// `Op::Bin` (`+`, `-`, `*`) into a temporary.
                $an,
            )*
            $(
                /// `Op::BinAssignCv` (`$cv = a op b`).
                $vn,
            )*
            $(
                /// `Op::AssignOpCv` (`$cv op= b`), result unused.
                $on,
            )*
            $(
                /// An instruction of a family module (see the `family`
                /// table).
                $fname,
            )*
        }

        impl TOp {
            fn cmp(ka: u8, kb: u8, m: u8) -> Option<TOp> {
                $( if ka == $cka && kb == $ckb && m == $cm { return Some(TOp::$cn); } )*
                None
            }
            fn inccmp(inc: bool, kb: u8, m: u8) -> Option<TOp> {
                $( if inc == $iinc && kb == $ikb && m == $im { return Some(TOp::$in); } )*
                None
            }
            fn arith_tmp(op: BinaryOp, ka: u8, kb: u8) -> Option<TOp> {
                $( if matches!(op, BinaryOp::$aop) && ka == $aka && kb == $akb { return Some(TOp::$an); } )*
                None
            }
            fn arith_cv(op: BinaryOp, ka: u8, kb: u8) -> Option<TOp> {
                $( if matches!(op, BinaryOp::$vop) && ka == $vka && kb == $vkb { return Some(TOp::$vn); } )*
                None
            }
            fn assign_op(op: BinaryOp, kb: u8) -> Option<TOp> {
                $( if matches!(op, BinaryOp::$oop) && kb == $okb { return Some(TOp::$on); } )*
                None
            }
        }

        /// Runs specialised instructions from `ip` until one stops it
        /// ([`Out`]): the loop of the tape. Inlined into [`run_tape`].
        ///
        /// # Safety
        /// `ip` points into a tape [`lower`] built for the op array the
        /// frame of `st` runs, and that frame is [`usable`].
        #[inline(always)]
        #[allow(
            clippy::inline_always,
            clippy::too_many_lines,
            clippy::float_arithmetic,
            // `x = x op y` comes out of `float_arith!` for every operator.
            clippy::assign_op_pattern,
            // Slot offsets are multiples of the 16-byte slot size from the
            // bases of `Vec<Value>` and `Vec<CvSlot>` buffers.
            clippy::cast_ptr_alignment
        )]
        unsafe fn run_core(st: St, mut ip: *const Inst) -> Out {
            // SAFETY (the whole loop): `ip` stays inside the tape (every
            // instruction advances it by what `lower` validated: the next
            // op, the ops a fused instruction covers, or a jump target, and
            // the sentinel stops a run past the last op), and every slot
            // offset an instruction carries was validated by `lower` for
            // the slot vectors `st` points to.
            unsafe { loop {
                let inst = &*ip;
                match inst.op {
                    TOp::Exit => {
                        return Out { ip, at: ip, why: WHY_EXIT };
                    }
                    TOp::Step => return step_out(ip),
                    TOp::Ext => {
                        return Out { ip, at: ip, why: WHY_EXT };
                    }
                    TOp::CallInit | TOp::CallSend => {
                        return Out { ip, at: ip, why: WHY_CALL };
                    }
                    TOp::Jmp => take_jump!(ip, inst),
                    TOp::JmpZT => {
                        let s = tmp_slot(st, inst.a);
                        let Value::Bool(b) = *s else {
                            return step_out(ip);
                        };
                        // Consumed, as `hot::take_bool` does (a bool:
                        // nothing to release).
                        std::ptr::write(s, Value::Null);
                        if b {
                            ip = ip.add(1);
                        } else {
                            take_jump!(ip, inst);
                        }
                    }
                    TOp::JmpNzT => {
                        let s = tmp_slot(st, inst.a);
                        let Value::Bool(b) = *s else {
                            return step_out(ip);
                        };
                        std::ptr::write(s, Value::Null);
                        if b {
                            take_jump!(ip, inst);
                        } else {
                            ip = ip.add(1);
                        }
                    }
                    TOp::IncC => {
                        match cv_slot(st, inst.a) {
                            Some(Value::Int(i)) => {
                                let Some(n) = i.checked_add(1) else {
                                    return step_out(ip);
                                };
                                *i = n;
                            }
                            Some(Value::Float(x)) => *x += 1.0,
                            _ => return step_out(ip),
                        }
                        ip = ip.add(1);
                    }
                    TOp::DecC => {
                        match cv_slot(st, inst.a) {
                            Some(Value::Int(i)) => {
                                let Some(n) = i.checked_sub(1) else {
                                    return step_out(ip);
                                };
                                *i = n;
                            }
                            Some(Value::Float(x)) => *x -= 1.0,
                            _ => return step_out(ip),
                        }
                        ip = ip.add(1);
                    }
                    TOp::PreIncT | TOp::PostIncT | TOp::PreDecT | TOp::PostDecT => {
                        let inc = matches!(inst.op, TOp::PreIncT | TOp::PostIncT);
                        let post = matches!(inst.op, TOp::PostIncT | TOp::PostDecT);
                        let d = tmp_slot(st, inst.c);
                        // An old value to release is the slow path's (it
                        // may queue a destructor, which wants its poll).
                        if !is_scalar(d) {
                            return step_out(ip);
                        }
                        match cv_slot(st, inst.a) {
                            Some(Value::Int(i)) => {
                                let old = *i;
                                let n = if inc { old.checked_add(1) } else { old.checked_sub(1) };
                                let Some(n) = n else {
                                    return step_out(ip);
                                };
                                *i = n;
                                std::ptr::write(d, Value::Int(if post { old } else { n }));
                            }
                            Some(Value::Float(x)) => {
                                let old = *x;
                                let n = if inc { old + 1.0 } else { old - 1.0 };
                                *x = n;
                                std::ptr::write(d, Value::Float(if post { old } else { n }));
                            }
                            _ => return step_out(ip),
                        }
                        ip = ip.add(1);
                    }
                    // The scalar assignments copy the 16 bytes of the value
                    // (a scalar owns nothing, so its bits are its copy);
                    // rebuilding it variant by variant made the compiler
                    // shuffle it through the stack in pieces.
                    TOp::AssignC => {
                        let s = st.cvs.add(inst.b as usize).cast::<Option<Value>>();
                        let d = st.cvs.add(inst.a as usize).cast::<Option<Value>>();
                        if !matches!(&*s, Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)))
                            || !matches!(&*d, None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)))
                        {
                            return step_out(ip);
                        }
                        // The destination is undefined or a scalar: nothing
                        // to release. `copy`, not `copy_nonoverlapping`:
                        // `$x = $x` names one slot.
                        std::ptr::copy(s, d, 1);
                        ip = ip.add(1);
                    }
                    TOp::AssignT => {
                        let s = st.tmps.add(inst.b as usize).cast::<Value>();
                        let d = st.cvs.add(inst.a as usize).cast::<Option<Value>>();
                        if !is_scalar(&*s)
                            || !matches!(&*d, None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)))
                        {
                            return step_out(ip);
                        }
                        std::ptr::write(d, Some(std::ptr::read(s)));
                        // The temporary is consumed (`std::mem::take` in
                        // `hot::assign_cv_in`).
                        std::ptr::write(s, Value::Null);
                        ip = ip.add(1);
                    }
                    TOp::AssignI => {
                        let d = cv_slot(st, inst.a);
                        if !matches!(d, None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_))) {
                            return step_out(ip);
                        }
                        #[allow(clippy::cast_possible_wrap)]
                        std::ptr::write(d, Some(Value::Int(i64::from(inst.b as i32))));
                        ip = ip.add(1);
                    }
                    TOp::AssignL => {
                        // A scalar literal (`lower` emits this for nothing
                        // else).
                        let s = st.lits.add(inst.b as usize).cast::<Value>();
                        debug_assert!(is_scalar(&*s));
                        let d = cv_slot(st, inst.a);
                        if !matches!(d, None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_))) {
                            return step_out(ip);
                        }
                        std::ptr::write(d, Some(std::ptr::read(s)));
                        ip = ip.add(1);
                    }
                    $(
                        TOp::$cn => {
                            let jump = match num::<{ $cka }>(st, inst.a) {
                                Some(Num::Int(x)) => match num::<{ $ckb }>(st, inst.b) {
                                    Some(Num::Int(y)) => int_test::<$cm>(x, y),
                                    Some(y) => hot::jump_of(Num::Int(x), y, inst.k),
                                    None => return step_out(ip),
                                },
                                Some(x) => match num::<{ $ckb }>(st, inst.b) {
                                    Some(y) => hot::jump_of(x, y, inst.k),
                                    None => return step_out(ip),
                                },
                                None => return step_out(ip),
                            };
                            if jump {
                                take_jump!(ip, inst);
                            } else {
                                // Past the jump op the comparison decided.
                                ip = ip.add(2);
                            }
                        }
                    )*
                    $(
                        TOp::$in => {
                            // Every test before the store: a declined
                            // instruction has changed nothing.
                            let Some(Num::Int(y)) = num::<{ $ikb }>(st, inst.b) else {
                                return step_out(ip);
                            };
                            let Some(Value::Int(i)) = cv_slot(st, inst.a) else {
                                return step_out(ip);
                            };
                            let n = if $iinc { i.checked_add(1) } else { i.checked_sub(1) };
                            let Some(n) = n else {
                                return step_out(ip);
                            };
                            *i = n;
                            if int_test::<$im>(n, y) {
                                take_jump!(ip, inst);
                            } else {
                                // Past the smart branch and its jump op: the
                                // loop's exit, once per loop (the jump back
                                // is the common way; see `loop_exit`).
                                ip = loop_exit(ip);
                            }
                        }
                    )*
                    $(
                        TOp::$an => {
                            // The result is stored where it is computed
                            // (a tag and a payload store): merged into one
                            // `Value` first, it travelled in pieces. The
                            // destination is tested up front for that; it
                            // is addressed by raw pointer because it may be
                            // one of the operands.
                            let d = st.tmps.add(inst.c as usize).cast::<Value>();
                            if !is_scalar(&*d) {
                                return step_out(ip);
                            }
                            // Two ints and two floats each on a path of
                            // their own; an int with a float is widened
                            // (php-src's `(double)`). An int overflow is
                            // `num_bin`'s (a float).
                            #[allow(clippy::cast_precision_loss)]
                            match num::<{ $aka }>(st, inst.a) {
                                Some(Num::Int(x)) => match num::<{ $akb }>(st, inst.b) {
                                    Some(Num::Int(y)) => {
                                        let Some(r) = int_arith!($aop, x, y) else {
                                            return step_out(ip);
                                        };
                                        std::ptr::write(d, Value::Int(r));
                                    }
                                    Some(Num::Float(y)) => {
                                        std::ptr::write(d, Value::Float(float_arith!($aop, x as f64, y)));
                                    }
                                    None => return step_out(ip),
                                },
                                Some(Num::Float(x)) => match num::<{ $akb }>(st, inst.b) {
                                    Some(Num::Float(y)) => {
                                        std::ptr::write(d, Value::Float(float_arith!($aop, x, y)));
                                    }
                                    Some(Num::Int(y)) => {
                                        std::ptr::write(d, Value::Float(float_arith!($aop, x, y as f64)));
                                    }
                                    None => return step_out(ip),
                                },
                                None => return step_out(ip),
                            }
                            ip = ip.add(1);
                        }
                    )*
                    $(
                        TOp::$vn => {
                            // As for the temporary result above.
                            #[allow(clippy::cast_precision_loss)]
                            let n = match num::<{ $vka }>(st, inst.a) {
                                Some(Num::Int(x)) => match num::<{ $vkb }>(st, inst.b) {
                                    Some(Num::Int(y)) => {
                                        let Some(r) = int_arith!($vop, x, y) else {
                                            return step_out(ip);
                                        };
                                        Num::Int(r)
                                    }
                                    Some(Num::Float(y)) => Num::Float(float_arith!($vop, x as f64, y)),
                                    None => return step_out(ip),
                                },
                                Some(Num::Float(x)) => match num::<{ $vkb }>(st, inst.b) {
                                    Some(Num::Float(y)) => Num::Float(float_arith!($vop, x, y)),
                                    Some(Num::Int(y)) => Num::Float(float_arith!($vop, x, y as f64)),
                                    None => return step_out(ip),
                                },
                                None => return step_out(ip),
                            };
                            // Into the variable when it holds no array,
                            // object or reference (the `AssignCv` after the
                            // op is then done); else into the temporary,
                            // for that `AssignCv`.
                            let d = cv_slot(st, inst.c);
                            match d {
                                None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)) => {
                                    std::ptr::write(d, Some(match n {
                                        Num::Int(i) => Value::Int(i),
                                        Num::Float(x) => Value::Float(x),
                                    }));
                                    ip = ip.add(2);
                                }
                                _ => return step_out(ip),
                            }
                        }
                    )*
                    $(
                        TOp::$on => {
                            let Some(y) = num::<{ $okb }>(st, inst.b) else {
                                return step_out(ip);
                            };
                            let slot = cv_slot(st, inst.a);
                            match (&mut *slot, y) {
                                (Some(Value::Int(i)), Num::Int(y)) => {
                                    // An overflow is `num_bin`'s (a float).
                                    let Some(r) = int_arith!($oop, *i, y) else {
                                        return step_out(ip);
                                    };
                                    *i = r;
                                }
                                (Some(Value::Float(x)), Num::Float(y)) => {
                                    *x = float_arith!($oop, *x, y);
                                }
                                (Some(Value::Float(x)), Num::Int(y)) => {
                                    #[allow(clippy::cast_precision_loss)] // php-src's (double) widening
                                    let y = y as f64;
                                    *x = float_arith!($oop, *x, y);
                                }
                                // An int variable and a float: the variable
                                // becomes a float (an int: nothing to
                                // release).
                                (Some(Value::Int(i)), Num::Float(y)) => {
                                    #[allow(clippy::cast_precision_loss)] // php-src's (double) widening
                                    let r = float_arith!($oop, *i as f64, y);
                                    std::ptr::write(slot, Some(Value::Float(r)));
                                }
                                _ => return step_out(ip),
                            }
                            ip = ip.add(1);
                        }
                    )*
                    $(
                        TOp::$fname => fam_arm!($fmode, $fh, $fskip, st, inst, ip),
                    )*
                }
            } }
        }
    };
}

tape_ops! {
    cmp {
        CmpLtCC = (KC, KC, 1), CmpLeCC = (KC, KC, 3), CmpGtCC = (KC, KC, 4), CmpGeCC = (KC, KC, 6), CmpEqCC = (KC, KC, 2), CmpNeCC = (KC, KC, 5),
        CmpLtCT = (KC, KT, 1), CmpLeCT = (KC, KT, 3), CmpGtCT = (KC, KT, 4), CmpGeCT = (KC, KT, 6), CmpEqCT = (KC, KT, 2), CmpNeCT = (KC, KT, 5),
        CmpLtCI = (KC, KI, 1), CmpLeCI = (KC, KI, 3), CmpGtCI = (KC, KI, 4), CmpGeCI = (KC, KI, 6), CmpEqCI = (KC, KI, 2), CmpNeCI = (KC, KI, 5),
        CmpLtCL = (KC, KL, 1), CmpLeCL = (KC, KL, 3), CmpGtCL = (KC, KL, 4), CmpGeCL = (KC, KL, 6), CmpEqCL = (KC, KL, 2), CmpNeCL = (KC, KL, 5),
        CmpLtTC = (KT, KC, 1), CmpLeTC = (KT, KC, 3), CmpGtTC = (KT, KC, 4), CmpGeTC = (KT, KC, 6), CmpEqTC = (KT, KC, 2), CmpNeTC = (KT, KC, 5),
        CmpLtTT = (KT, KT, 1), CmpLeTT = (KT, KT, 3), CmpGtTT = (KT, KT, 4), CmpGeTT = (KT, KT, 6), CmpEqTT = (KT, KT, 2), CmpNeTT = (KT, KT, 5),
        CmpLtTI = (KT, KI, 1), CmpLeTI = (KT, KI, 3), CmpGtTI = (KT, KI, 4), CmpGeTI = (KT, KI, 6), CmpEqTI = (KT, KI, 2), CmpNeTI = (KT, KI, 5),
        CmpLtTL = (KT, KL, 1), CmpLeTL = (KT, KL, 3), CmpGtTL = (KT, KL, 4), CmpGeTL = (KT, KL, 6), CmpEqTL = (KT, KL, 2), CmpNeTL = (KT, KL, 5),
    }
    inccmp {
        IncLtC = (true, KC, 1), IncLeC = (true, KC, 3), IncGtC = (true, KC, 4), IncGeC = (true, KC, 6), IncEqC = (true, KC, 2), IncNeC = (true, KC, 5),
        IncLtI = (true, KI, 1), IncLeI = (true, KI, 3), IncGtI = (true, KI, 4), IncGeI = (true, KI, 6), IncEqI = (true, KI, 2), IncNeI = (true, KI, 5),
        DecLtC = (false, KC, 1), DecLeC = (false, KC, 3), DecGtC = (false, KC, 4), DecGeC = (false, KC, 6), DecEqC = (false, KC, 2), DecNeC = (false, KC, 5),
        DecLtI = (false, KI, 1), DecLeI = (false, KI, 3), DecGtI = (false, KI, 4), DecGeI = (false, KI, 6), DecEqI = (false, KI, 2), DecNeI = (false, KI, 5),
    }
    arith_tmp {
        AddCC = (Add, KC, KC), AddCT = (Add, KC, KT), AddCI = (Add, KC, KI), AddCL = (Add, KC, KL), AddTC = (Add, KT, KC), AddTT = (Add, KT, KT), AddTI = (Add, KT, KI), AddTL = (Add, KT, KL), AddIC = (Add, KI, KC), AddIT = (Add, KI, KT), AddLC = (Add, KL, KC), AddLT = (Add, KL, KT),
        SubCC = (Sub, KC, KC), SubCT = (Sub, KC, KT), SubCI = (Sub, KC, KI), SubCL = (Sub, KC, KL), SubTC = (Sub, KT, KC), SubTT = (Sub, KT, KT), SubTI = (Sub, KT, KI), SubTL = (Sub, KT, KL), SubIC = (Sub, KI, KC), SubIT = (Sub, KI, KT), SubLC = (Sub, KL, KC), SubLT = (Sub, KL, KT),
        MulCC = (Mul, KC, KC), MulCT = (Mul, KC, KT), MulCI = (Mul, KC, KI), MulCL = (Mul, KC, KL), MulTC = (Mul, KT, KC), MulTT = (Mul, KT, KT), MulTI = (Mul, KT, KI), MulTL = (Mul, KT, KL), MulIC = (Mul, KI, KC), MulIT = (Mul, KI, KT), MulLC = (Mul, KL, KC), MulLT = (Mul, KL, KT),
    }
    arith_cv {
        AddCCV = (Add, KC, KC), AddCTV = (Add, KC, KT), AddCIV = (Add, KC, KI), AddCLV = (Add, KC, KL), AddTCV = (Add, KT, KC), AddTTV = (Add, KT, KT), AddTIV = (Add, KT, KI), AddTLV = (Add, KT, KL), AddICV = (Add, KI, KC), AddITV = (Add, KI, KT), AddLCV = (Add, KL, KC), AddLTV = (Add, KL, KT),
        SubCCV = (Sub, KC, KC), SubCTV = (Sub, KC, KT), SubCIV = (Sub, KC, KI), SubCLV = (Sub, KC, KL), SubTCV = (Sub, KT, KC), SubTTV = (Sub, KT, KT), SubTIV = (Sub, KT, KI), SubTLV = (Sub, KT, KL), SubICV = (Sub, KI, KC), SubITV = (Sub, KI, KT), SubLCV = (Sub, KL, KC), SubLTV = (Sub, KL, KT),
        MulCCV = (Mul, KC, KC), MulCTV = (Mul, KC, KT), MulCIV = (Mul, KC, KI), MulCLV = (Mul, KC, KL), MulTCV = (Mul, KT, KC), MulTTV = (Mul, KT, KT), MulTIV = (Mul, KT, KI), MulTLV = (Mul, KT, KL), MulICV = (Mul, KI, KC), MulITV = (Mul, KI, KT), MulLCV = (Mul, KL, KC), MulLTV = (Mul, KL, KT),
    }
    assign_op {
        AddEqC = (Add, KC), AddEqT = (Add, KT), AddEqI = (Add, KI), AddEqL = (Add, KL),
        SubEqC = (Sub, KC), SubEqT = (Sub, KT), SubEqI = (Sub, KI), SubEqL = (Sub, KL),
        MulEqC = (Mul, KC), MulEqT = (Mul, KT), MulEqI = (Mul, KI), MulEqL = (Mul, KL),
    }
    // One row per instruction: `Opcode = (handler, ops covered, ending),`
    // where the ending is `plain`, `poll`, `branch` or `branch_poll`
    // (see `fam_arm!`). Each family adds its rows under its own heading.
    family {
        // ---- scalar (exec/tape/scalar.rs) ----
        Nop = (scalar::nop, 1, plain),
        JmpZC = (scalar::jmpz_cv, 1, branch),
        JmpNzC = (scalar::jmpnz_cv, 1, branch),
        NumBinT = (scalar::num_bin_tmp, 1, plain_f),
        NumBinC = (scalar::num_bin_cv, 2, plain_f),
        NumAssignOpC = (scalar::num_assign_op_cv, 1, plain_f),

        // ---- end of scalar ----
        //
        //
        //
        // ---- dims (exec/tape/dims.rs) ----
        FetchDimC = (dims::fetch_dim, 1, branch_poll_f),
        FetchDimIC = (dims::fetch_dim_int, 1, branch_poll_f),
        FetchDimSC = (dims::fetch_dim_str, 1, branch_poll_f),
        FetchDimsC = (dims::fetch_dims, 1, branch_poll_f),
        IssetDimC = (dims::isset_dim, 1, poll_f),
        AssignDimC = (dims::assign_dim, 1, poll_f),
        AssignDimIC = (dims::assign_dim_int, 1, poll_f),
        AssignDimsC = (dims::assign_dims, 1, poll_f),
        AssignOpDimC = (dims::assign_op_dim, 1, poll_f),
        AssignOpDimsC = (dims::assign_op_dims, 1, poll_f),
        IncDecDimC = (dims::incdec_dim, 1, poll_f),
        ConcatCv = (dims::concat_cv, 1, poll_f),

        // ---- end of dims ----
        //
        //
        //
        // ---- props (exec/tape/props.rs) ----
        FetchObjProp = (props::fetch_prop, 1, poll_f),
        FetchObjPropTo = (props::fetch_prop_to, 1, branch_poll_f),
        AssignObjProp = (props::prop_in, 1, poll_f),
        AssignOpObjProp = (props::prop_in, 1, poll_f),
        IncDecObjProp = (props::prop_in, 1, poll_f),
        IssetObjProp = (props::prop_in, 1, poll_f),

        // ---- end of props ----
        //
        //
        //
        // ---- strs (exec/tape/strs.rs) ----
        StrlenC = (strs::strlen_cv, 1, plain_f),
        CountC = (strs::count_cv, 1, plain_f),
        TypeCheckC = (strs::type_check_cv, 1, plain_f),
        TypeCheckJmpC = (strs::type_check_jmp_cv, 2, branch_f),
        IssetC = (strs::isset_cv, 1, plain_f),
        IssetJmpC = (strs::isset_jmp_cv, 2, branch_f),

        // ---- end of strs ----
        //
        //
        //
        // ---- calls (exec/tape/calls.rs) ----


        // ---- end of calls ----
    }
}

/// [`hot::step`] out of line: the core loop must not carry its code.
#[inline(never)]
fn step_slow(f: &mut Frame, ops: &[Op], lits: &[Value], at: usize) -> usize {
    hot::step(f, ops, lits, at)
}

/// The kind and operand field of a numeric operand; `None` when it has no
/// specialised form (not a number literal, or out of range).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn src(code: &OpArray, o: Operand) -> Option<(u8, u32)> {
    match o {
        Operand::Cv(i) => Some((KC, cv_off(code, i)?)),
        Operand::Tmp(t) | Operand::TmpKeep(t) => Some((KT, tmp_off(code, t)?)),
        Operand::Lit(l) => match code.literals.get(l as usize)? {
            Value::Int(v) => match i32::try_from(*v) {
                Ok(small) => Some((KI, small as u32)),
                Err(_) => Some((KL, lit_off(l)?)),
            },
            Value::Float(_) => Some((KL, lit_off(l)?)),
            _ => None,
        },
        Operand::Unused => None,
    }
}

/// The byte offset of a compiled variable; `None` when out of range.
fn cv_off(code: &OpArray, cv: u32) -> Option<u32> {
    ((cv as usize) < code.cv_names.len() && cv < SLOT_LIMIT).then_some(cv << SLOT_SHIFT)
}

/// The byte offset of a temporary; `None` when out of range.
fn tmp_off(code: &OpArray, t: u32) -> Option<u32> {
    (t < code.num_tmps && t < SLOT_LIMIT).then_some(t << SLOT_SHIFT)
}

/// The byte offset of a literal the caller found in the literal table.
fn lit_off(l: u32) -> Option<u32> {
    (l < SLOT_LIMIT).then_some(l << SLOT_SHIFT)
}

/// The jump from instruction `from` to op index `to`, as a byte offset;
/// `None` when the target is outside the tape.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn rel(code: &OpArray, from: usize, to: u32) -> Option<u32> {
    if to as usize > code.ops.len() {
        return None;
    }
    let delta = (i64::from(to) - from as i64) * std::mem::size_of::<Inst>() as i64;
    i32::try_from(delta).ok().map(|d| d as u32)
}

/// The mask of a smart branch with its operands exchanged: `LESS` and
/// `GREATER` trade places; equal, unordered and the identity bits are
/// symmetric ([`crate::opcode::cmp_outcome`]).
const fn mirrored(mask: u8) -> u8 {
    use crate::opcode::cmp_outcome::{GREATER, LESS};
    let mut m = mask & !(LESS | GREATER);
    if mask & LESS != 0 {
        m |= GREATER;
    }
    if mask & GREATER != 0 {
        m |= LESS;
    }
    m
}

/// Whether the instructions of family `name` are switched off for this
/// process: the environment variable `PHPRUST_TAPE_OFF` lists families
/// (`numeric`, `scalar`, `dims`, `props`, `strs`, `calls`, or `all`),
/// separated by commas. A debugging aid, read once: it tells which family a
/// wrong result comes from without a rebuild (every op of a family that is
/// off runs through `hot::step`, as it did before the family existed).
fn switched_off(name: &str) -> bool {
    static OFF: OnceLock<Vec<String>> = OnceLock::new();
    OFF.get_or_init(|| {
        std::env::var("PHPRUST_TAPE_OFF")
            .map(|list| list.split(',').map(|n| n.trim().to_owned()).collect())
            .unwrap_or_default()
    })
    .iter()
    .any(|off| off == name || off == "all")
}

/// The specialised instruction of the op at `i`, if it has one: a numeric
/// one of this module, else one of a family's.
fn lower_op(code: &OpArray, i: usize) -> Option<Inst> {
    let family = |name: &str, lower: fn(&OpArray, usize) -> Option<Inst>| {
        if switched_off(name) {
            None
        } else {
            lower(code, i)
        }
    };
    family("numeric", lower_numeric)
        .or_else(|| family("scalar", scalar::lower))
        .or_else(|| family("dims", dims::lower))
        .or_else(|| family("props", props::lower))
        .or_else(|| family("strs", strs::lower))
        .or_else(|| family("calls", calls::lower))
}

/// The numeric instruction ([`tape_ops!`]'s generated families) of the op
/// at `i`, if it has one.
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn lower_numeric(code: &OpArray, i: usize) -> Option<Inst> {
    let plain = Inst::plain;
    Some(match *code.ops.get(i)? {
        Op::Jmp(t) => Inst {
            c: rel(code, i, t)?,
            ..plain(TOp::Jmp)
        },
        Op::JmpZ(Operand::Tmp(t), target) => Inst {
            a: tmp_off(code, t)?,
            c: rel(code, i, target)?,
            ..plain(TOp::JmpZT)
        },
        Op::JmpNz(Operand::Tmp(t), target) => Inst {
            a: tmp_off(code, t)?,
            c: rel(code, i, target)?,
            ..plain(TOp::JmpNzT)
        },
        Op::CmpJmp {
            a, b, target, mask, ..
        } => {
            let (ka, fa) = src(code, a)?;
            let (kb, fb) = src(code, b)?;
            // A literal on the left (`$x > 5` is compiled as `5 < $x`, as
            // php-src does): the mirrored comparison of the slot with it.
            let (ka, fa, kb, fb, mask) = if matches!(ka, KI | KL) {
                (kb, fb, ka, fa, mirrored(mask))
            } else {
                (ka, fa, kb, fb, mask)
            };
            Inst {
                k: mask,
                a: fa,
                b: fb,
                c: rel(code, i, target)?,
                ..plain(TOp::cmp(ka, kb, mask & 7)?)
            }
        }
        Op::IncDecCvCmpJmp { cv, kind, .. } => {
            let Some(&Op::CmpJmp {
                a, b, target, mask, ..
            }) = code.ops.get(i + 1)
            else {
                return None;
            };
            // The variable against something else, which the fused form
            // reads before the store. `$i > 0` and `$n > $i` have the
            // variable on the right (compiled as `0 < $i`, `$i < $n`
            // mirrored): the mirrored comparison.
            let known = Operand::Cv(cv);
            let (other, mask) = match (a == known, b == known) {
                (true, false) => (b, mask),
                (false, true) => (a, mirrored(mask)),
                _ => return None,
            };
            let (kb, fb) = src(code, other)?;
            Inst {
                k: mask,
                a: cv_off(code, cv)?,
                b: fb,
                c: rel(code, i, target)?,
                ..plain(TOp::inccmp(kind.is_inc(), kb, mask & 7)?)
            }
        }
        Op::IncDecCv {
            cv,
            kind,
            dst: None,
            ..
        } => Inst {
            a: cv_off(code, cv)?,
            ..plain(if kind.is_inc() { TOp::IncC } else { TOp::DecC })
        },
        Op::IncDecCv {
            cv,
            kind,
            dst: Some(t),
            ..
        } => Inst {
            a: cv_off(code, cv)?,
            c: tmp_off(code, t)?,
            ..plain(match (kind.is_inc(), kind.is_post()) {
                (true, false) => TOp::PreIncT,
                (true, true) => TOp::PostIncT,
                (false, false) => TOp::PreDecT,
                (false, true) => TOp::PostDecT,
            })
        },
        Op::Bin {
            op: BinKind::Arith(op),
            a,
            b,
            dst,
        } => {
            let (ka, fa) = src(code, a)?;
            let (kb, fb) = src(code, b)?;
            Inst {
                a: fa,
                b: fb,
                c: tmp_off(code, dst)?,
                ..plain(TOp::arith_tmp(op, ka, kb)?)
            }
        }
        Op::BinAssignCv {
            op: BinKind::Arith(op),
            a,
            b,
            cv,
            ..
        } => {
            let (ka, fa) = src(code, a)?;
            let (kb, fb) = src(code, b)?;
            Inst {
                a: fa,
                b: fb,
                c: cv_off(code, cv)?,
                ..plain(TOp::arith_cv(op, ka, kb)?)
            }
        }
        Op::AssignOpCv {
            cv,
            op,
            val,
            dst: None,
            ..
        } => {
            let (kb, fb) = src(code, val)?;
            Inst {
                a: cv_off(code, cv)?,
                b: fb,
                ..plain(TOp::assign_op(op, kb)?)
            }
        }
        Op::AssignCv { cv, val, dst: None } => {
            let a = cv_off(code, cv)?;
            match val {
                Operand::Cv(s) => Inst {
                    a,
                    b: cv_off(code, s)?,
                    ..plain(TOp::AssignC)
                },
                Operand::Tmp(t) => Inst {
                    a,
                    b: tmp_off(code, t)?,
                    ..plain(TOp::AssignT)
                },
                Operand::Lit(l) => match code.literals.get(l as usize)? {
                    Value::Int(v) => match i32::try_from(*v) {
                        #[allow(clippy::cast_sign_loss)]
                        Ok(small) => Inst {
                            a,
                            b: small as u32,
                            ..plain(TOp::AssignI)
                        },
                        Err(_) => Inst {
                            a,
                            b: lit_off(l)?,
                            ..plain(TOp::AssignL)
                        },
                    },
                    Value::Null | Value::Bool(_) | Value::Float(_) => Inst {
                        a,
                        b: lit_off(l)?,
                        ..plain(TOp::AssignL)
                    },
                    _ => return None,
                },
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Decodes `code` into its tape: one [`Inst`] per op, at the op's index,
/// and an [`TOp::Exit`] sentinel after the last one.
pub(crate) fn lower(code: &OpArray) -> Box<[Inst]> {
    let mut tape = Vec::with_capacity(code.ops.len() + 1);
    for (i, op) in code.ops.iter().enumerate() {
        tape.push(lower_op(code, i).unwrap_or_else(|| {
            Inst::plain(if !hot::is_hot(op) {
                TOp::Exit
            } else if hot::is_core(op) {
                TOp::Step
            } else {
                TOp::Ext
            })
        }));
    }
    tape.push(Inst::plain(TOp::Exit));
    // Entering the tape costs a call, a look at the frame and the way back
    // (about what one unspecialised hot op costs): worth it at a
    // specialised instruction when the tape also runs what follows it.
    for i in 0..code.ops.len() {
        let follows = likely_next(code, i);
        tape[i].ext = !hot::is_core(&code.ops[i]);
        tape[i].enter = !matches!(tape[i].op, TOp::Exit | TOp::Step | TOp::Ext)
            && tape.get(follows).is_some_and(|next| next.op != TOp::Exit);
    }
    tape.into_boxed_slice()
}

/// The op that most likely runs after the op at `i`: the target of a
/// backward jump (a loop goes round), the fall-through of a forward one,
/// the op after the ops a fused op covers.
fn likely_next(code: &OpArray, i: usize) -> usize {
    let after = |target: u32, fall: usize| {
        if target as usize <= i {
            target as usize
        } else {
            fall
        }
    };
    match code.ops.get(i) {
        Some(&Op::Jmp(t)) => t as usize,
        Some(&(Op::JmpZ(_, t) | Op::JmpNz(_, t))) => after(t, i + 1),
        Some(&Op::CmpJmp { target, .. }) => after(target, i + 2),
        Some(Op::IncDecCvCmpJmp { .. } | Op::AssignOpCvCmpJmp { .. }) => {
            match code.ops.get(i + 1) {
                Some(&Op::CmpJmp { target, .. }) => after(target, i + 3),
                _ => i + 1,
            }
        }
        Some(Op::BinAssignCvCmpJmp { .. }) => match code.ops.get(i + 2) {
            Some(&Op::CmpJmp { target, .. }) => after(target, i + 4),
            _ => i + 1,
        },
        Some(Op::BinAssignCv { .. }) => i + 2,
        _ => i + 1,
    }
}

/// Whether `Vm::run` enters the tape at the op at `at` of `code` at once
/// (the tape is decoded on first use): it has a specialised instruction
/// and so has the op that most likely follows it ([`lower`] works that
/// out). Any other hot op `Vm::run` runs itself first (`hot::step`), and
/// it enters the tape when the op after that one is a hot op too.
#[inline]
pub(super) fn worth_entering(code: &OpArray, at: usize) -> bool {
    code.tape.get(code).get(at).is_some_and(|inst| inst.enter)
}

/// Whether the tape of `code` may run on frame `f`: its slots are where
/// the tape's offsets point (see the module's safety notes), and no symbol
/// table mirrors its compiled variables (a write would have to be copied
/// there: `hot::step` does that).
#[inline(always)]
#[allow(clippy::inline_always)] // three tests on the way into the loop
fn usable(f: &Frame, code: &OpArray) -> bool {
    !f.symtab_attached
        && f.cvs.len() >= code.cv_names.len()
        && f.tmps.len() >= code.num_tmps as usize
}

/// Runs the hot ops of the current frame (the one `code` belongs to) from
/// `ip`, and the plain user calls and returns among them
/// (`ZEND_DO_UCALL`, `ZEND_RETURN`: [`Vm::push_simple_ucall`],
/// [`Vm::return_simple`]), going on in the callee's or the caller's frame
/// in place of returning to `Vm::run` to reload it, and the plain builtin
/// calls by name (`ZEND_DO_ICALL`: `calls::call_native`). Specialised
/// instructions run in [`run_core`]; a hot op without one, or whose
/// instruction declined, runs through [`hot::step`] or
/// [`hot::step_ext`]; a frame the tape cannot run on ([`usable`]) has its
/// hot ops run by [`hot::run_steps`]. Returns:
///
/// * `(ip, NO_SAFE_POINT)` when the op at `ip` must run in `Vm::run` (not
///   a hot op, or its fast path declined, having changed nothing; a
///   `DoCall` or `Return` here was declined by its fast path);
/// * `(ip, at)` when the safe point after the op at `at` is due, `ip`
///   being where execution continues;
/// * either with [`SWITCHED`] set in `ip` when the current frame changed
///   since the call (`ip` and `at` are then the new frame's);
/// * `(_, LEFT)` when a call or return changed the frame and the safe
///   point after it is due ([`LEFT`]);
/// * `(_, THREW)` when a builtin it called (`calls::call_native`) threw or
///   exited ([`hot::THREW`]): the throw is in `Vm::tape_throw`, the
///   current frame's resume point stored as for `Vm::run`'s generic
///   `DoCall` (`SAVE_OPLINE`), so the exception unwinds from the call.
///
/// `base` is `Vm::run`'s: the frame at that index is never returned from
/// here (`Vm::return_simple` declines it). The calls and returns are
/// exactly what `Vm::run`'s fast `DoCall` and `Return` arms do, in the
/// same order: those push or pop only plain user frames (no observer,
/// generator, by-reference return, attached symbol table, named or spread
/// arguments, coercion, deep recursion: anything else is declined and runs
/// as the generic op), and run no user code; the frame's `ip` is stored as
/// there, and a due safe point is run by `Vm::run` right after the call or
/// return, as there. Every frame entered is looked at afresh: a callee
/// runs on the tape whatever its caller ran on.
///
/// Never inlined: the loop's state (instruction pointer, slot bases) lives
/// in its own registers, which is also why it is one function.
#[inline(never)]
#[allow(clippy::too_many_lines)]
// The hot functions of the interpreter (this one, `run_call`,
// `calls::ready_frame`, `calls::enter`, `hot::run_steps`,
// `Vm::return_simple`, `Vm::return_generic`) share one section: the linker
// puts `.text.hot.*` together at the start of `.text`, so code added
// elsewhere no longer moves them relative to each other. Byte-identical,
// 64-byte aligned functions otherwise land in other L1i and branch
// predictor sets: calls_bench took 8 % longer with fetch latency 3.5 % ->
// 8.2 % (TopdownL2) after an unrelated change (22 Nov 2026).
#[link_section = ".text.hot.php_vm"]
pub(super) fn run_tape(vm: &mut Vm, base: usize, code: &OpArray, ip: usize) -> (usize, usize) {
    let mut switched = 0;
    let mut code: *const OpArray = code;
    let mut ip = ip;
    // The call `calls::prepare` readied in the current frame, until its
    // `DoCall` enters it. Turned back into the generic pending call
    // (`flush`) before anything else could see the caller's calls: an op
    // handed to the generic code that passes an argument, and every way
    // out of this function.
    let mut prep: Option<calls::Prepared> = None;
    // Whether the current frame was just pushed by a call made below: its
    // slots are sized for its code and no symbol table is attached
    // (`usable` holds without the test).
    let mut fresh = false;
    'frames: loop {
        // SAFETY: as in `Vm::run`: the op array is owned by `Rc`s, one of
        // them held by the current frame, so its heap block stays where it
        // is while that frame is current. Until the next call or return
        // below (after which it is reloaded from the new current frame),
        // only hot ops run, which neither push nor pop frames nor replace
        // a frame's `code`; and `Vm::run`, which gets this frame's `ip`
        // back with `SWITCHED`, reloads the op array from the frame itself.
        let c: &OpArray = unsafe { &*code };
        let ops: &[Op] = &c.ops;
        let lits: &[Value] = &c.literals;
        let Some(f) = vm.frames.last_mut() else {
            flush(vm, &mut prep);
            return (ip | switched, NO_SAFE_POINT);
        };
        let at = if fresh || usable(f, c) {
            fresh = false;
            let tape: &[Inst] = c.tape.get(c);
            debug_assert_eq!(tape.len(), ops.len() + 1);
            let tbase = tape.as_ptr();
            // SAFETY: `ip` is an op index of this op array (`Vm::run`'s, a
            // frame's stored resume point, or what a hot op continued at),
            // at most `ops.len()`: inside the tape, whose last element is
            // the sentinel.
            let mut p = unsafe { tbase.add(ip.min(ops.len())) };
            // The current frame by raw pointer: the call instructions need
            // the VM (the frame stack), and the frame is looked up again
            // after each of them.
            let mut fp: *mut Frame = f;
            loop {
                // SAFETY: `fp` is the current frame (`vm.frames`' last
                // live one), derived after the last use of the VM.
                let f = unsafe { &mut *fp };
                // The slot vectors are read again after every hot op run
                // out of the core (none of them resizes one, but none is
                // bound not to replace its buffer).
                let st = St {
                    cvs: f.cvs.as_mut_ptr().cast(),
                    tmps: f.tmps.as_mut_ptr().cast(),
                    lits: lits.as_ptr().cast(),
                    f: std::ptr::from_mut(f),
                };
                // SAFETY: the frame is `usable` for this tape (tested
                // above; a hot op that attaches a symbol table sends the
                // frame back through that test below), and `p` points into
                // the tape.
                let out = unsafe { run_core(st, p) };
                // SAFETY: both point into the tape.
                #[allow(clippy::cast_sign_loss)]
                let at = unsafe { out.ip.offset_from(tbase) } as usize;
                let next = match out.why {
                    WHY_CALL => {
                        let next = run_call(vm, &mut prep, ops, lits, at);
                        if next == ENTERED {
                            // The call was entered: the tape goes on in the
                            // callee's frame, as after an entered `DoCall`
                            // below (sized for its code, no symbol table).
                            fresh = true;
                            switched = SWITCHED;
                            if crate::interrupt::safe_point_due() {
                                return (0, LEFT);
                            }
                            let Some(f) = vm.frames.last() else {
                                return (0, LEFT);
                            };
                            ip = f.ip;
                            code = Rc::as_ptr(&f.code);
                            continue 'frames;
                        }
                        let Some(f) = vm.frames.last_mut() else {
                            flush(vm, &mut prep);
                            return (0, NO_SAFE_POINT);
                        };
                        fp = f;
                        if next & (CALLED | CALL_THREW) == 0 || next == DECLINED {
                            next
                        } else if next & CALL_THREW != 0 {
                            // As after the `DoCall` arm's call below.
                            return ((next & !CALL_THREW) | switched, hot::THREW);
                        } else {
                            // The builtin call was made: the safe point
                            // after its `DoCall`, as the arm below; then
                            // the op after it (the loop looks at the frame
                            // afresh: the handler may have attached its
                            // symbol table).
                            let d = next & !CALLED;
                            if crate::interrupt::safe_point_due() {
                                return ((d + 1) | switched, d);
                            }
                            d + 1
                        }
                    }
                    WHY_EXT => {
                        // An argument the generic `Send` passes goes to the
                        // newest pending call: the prepared one becomes it.
                        if prep.is_some() && matches!(ops.get(at), Some(Op::Send { .. })) {
                            flush(vm, &mut prep);
                        }
                        // SAFETY: `fp` is current (`flush` only changed
                        // the frame's pending calls).
                        hot::step_ext(unsafe { &mut *fp }, ops, lits, at)
                    }
                    // A declined instruction: the op's old fast path, by
                    // the shortest route (a family op is not one of
                    // `hot::step`'s own arms; `ext`, worked out by
                    // `lower`).
                    // SAFETY: `out.ip` points into the tape (as above).
                    WHY_STEP if unsafe { (*out.ip).ext } => hot::step_ext(f, ops, lits, at),
                    WHY_STEP => step_slow(f, ops, lits, at),
                    WHY_POLL => {
                        flush(vm, &mut prep);
                        // SAFETY: as above.
                        #[allow(clippy::cast_sign_loss)]
                        let last = unsafe { out.at.offset_from(tbase) } as usize;
                        return (at | switched, last);
                    }
                    _ => break at,
                };
                if next == DECLINED {
                    break at;
                }
                if crate::interrupt::safe_point_due() {
                    flush(vm, &mut prep);
                    return (next | switched, at);
                }
                // SAFETY: as above; `fp` is current.
                if unsafe { (*fp).symtab_attached } {
                    // The op attached a symbol table: the frame leaves the
                    // tape.
                    flush(vm, &mut prep);
                    ip = next;
                    continue 'frames;
                }
                // SAFETY: a hot op continues at an op index of this op
                // array (the next op or a jump target).
                p = unsafe { tbase.add(next.min(ops.len())) };
            }
        } else {
            let (next, last) = hot::run_steps(f, ops, lits, ip);
            if last != NO_SAFE_POINT {
                flush(vm, &mut prep);
                return (next | switched, last);
            }
            next
        };
        // `ZEND_DO_UCALL` / `ZEND_RETURN` of a plain user function: the
        // prepared call entered directly (`calls::enter`), or the fast
        // arms of `Vm::run`, which store `at + 1` as the frame's resume
        // point (`SAVE_OPLINE`).
        let changed = match ops.get(at) {
            Some(&Op::DoCall { dst }) => {
                fresh = true;
                match prep.take() {
                    // `ZEND_DO_ICALL` of the prepared builtin call: the
                    // handler runs here, and the caller goes on after the
                    // call, looked at afresh (the handler may have attached
                    // its symbol table), with the safe point after the call
                    // when it is due, as after `Vm::run`'s generic `DoCall`.
                    // Tested before `enter`, which would only hand the call
                    // back: called for builtins too, it was taken out of
                    // line and cost each builtin call ~75 instructions.
                    Some(call) if call.is_native() => {
                        fresh = false;
                        match calls::call_native(vm, call, dst, at + 1) {
                            Ok(Ok(())) => {
                                if crate::interrupt::safe_point_due() {
                                    return ((at + 1) | switched, at);
                                }
                                ip = at + 1;
                                continue 'frames;
                            }
                            Ok(Err(t)) => {
                                vm.tape_throw = Some(t);
                                return (at | switched, hot::THREW);
                            }
                            Err(call) => {
                                calls::materialize(vm, call);
                                false
                            }
                        }
                    }
                    Some(call) => match calls::enter(vm, call, dst, at + 1) {
                        Ok(()) => true,
                        Err(call) => {
                            calls::materialize(vm, call);
                            vm.push_simple_ucall(dst, at + 1)
                        }
                    },
                    None => vm.push_simple_ucall(dst, at + 1),
                }
            }
            Some(&Op::Return { val }) => {
                flush(vm, &mut prep);
                fresh = false;
                vm.return_simple(val, base, at + 1).is_some()
            }
            _ => false,
        };
        if !changed {
            flush(vm, &mut prep);
            return (at | switched, NO_SAFE_POINT);
        }
        // The frame's op array may be freed with it: `ops`, `lits` and
        // the op are not read again before they are reloaded above.
        switched = SWITCHED;
        if crate::interrupt::safe_point_due() {
            return (0, LEFT);
        }
        // `LOAD_OPLINE()` of the new current frame, as `Vm::run` does.
        let Some(f) = vm.frames.last() else {
            return (0, LEFT);
        };
        ip = f.ip;
        code = Rc::as_ptr(&f.code);
    }
}

/// Turns a prepared call back into the generic pending call
/// ([`calls::materialize`]).
#[inline(always)]
#[allow(clippy::inline_always)] // a test on every way out of the loop
fn flush(vm: &mut Vm, prep: &mut Option<calls::Prepared>) {
    // Tested first: `take` would store `None` back on every way out.
    if prep.is_some() {
        flush_cold(vm, prep);
    }
}

/// [`flush`] of a prepared call, out of line.
#[cold]
#[inline(never)]
fn flush_cold(vm: &mut Vm, prep: &mut Option<calls::Prepared>) {
    if let Some(call) = prep.take() {
        calls::materialize(vm, call);
    }
}

/// Runs the call instruction at `at` ([`WHY_CALL`]) with the VM:
/// `InitFCall` prepares the call ([`calls::prepare`]), `Send` writes into
/// the prepared call ([`calls::send`]); when they do not apply (the call
/// is first turned back into a pending call), the op's generic fast path
/// (`hot::step_ext`: `init_fcall_in`, `send_in`). Returns where execution
/// continues, or [`DECLINED`].
#[inline(never)]
#[link_section = ".text.hot.php_vm"]
fn run_call(
    vm: &mut Vm,
    prep: &mut Option<calls::Prepared>,
    ops: &[Op],
    lits: &[Value],
    at: usize,
) -> usize {
    match ops.get(at) {
        Some(&Op::InitFCall { name, .. }) => {
            flush(vm, prep);
            if let Some(call) = calls::prepare(vm, name) {
                return send_and_enter(vm, prep, call, ops, at);
            }
        }
        Some(&Op::InitMethodCall {
            obj,
            name: Operand::Lit(lit),
        }) => {
            flush(vm, prep);
            if let Some(call) = calls::prepare_method(vm, obj, lit, at) {
                return send_and_enter(vm, prep, call, ops, at);
            }
            // Not a hot op: its generic fast path (`Vm::run`'s pre-match
            // arm), else the generic op in `Vm::run`.
            return if vm.init_method_call_fast(obj, lit, at) {
                at + 1
            } else {
                DECLINED
            };
        }
        Some(&Op::New {
            class: ClassSpec::Named(lit),
            dst,
            skip,
        }) => {
            flush(vm, prep);
            match calls::new_object(vm, lit, dst, skip) {
                Some(calls::NewDone::Skip(target)) => return target,
                Some(calls::NewDone::Prepared(call)) => {
                    return send_and_enter(vm, prep, call, ops, at);
                }
                Some(calls::NewDone::Pending) => return at + 1,
                // The generic `New` (`hot::step_ext`, then `Vm::run`).
                None => {}
            }
        }
        Some(&Op::InitStaticCall { .. }) => {
            flush(vm, prep);
            // Not a hot op: else the generic op in `Vm::run`.
            return match calls::prepare_static(vm, at) {
                Some(call) => send_and_enter(vm, prep, call, ops, at),
                None => DECLINED,
            };
        }
        Some(&Op::InitDynCall {
            callee: Operand::Cv(cv),
        }) => {
            flush(vm, prep);
            if let Some(call) = calls::prepare_closure(vm, cv) {
                return send_and_enter(vm, prep, call, ops, at);
            }
            // Not a hot op: its generic fast path (`Vm::run`'s pre-match
            // arm), else the generic op in `Vm::run`.
            return if vm.init_closure_call_fast(cv) {
                at + 1
            } else {
                DECLINED
            };
        }
        Some(Op::Send { src }) => {
            if let Some(call) = prep.as_mut() {
                if calls::send(vm, call, src) {
                    return at + 1;
                }
            }
            flush(vm, prep);
        }
        _ => {}
    }
    let Some(f) = vm.frames.last_mut() else {
        return DECLINED;
    };
    hot::step_ext(f, ops, lits, at)
}

/// [`run_call`]'s answer when it entered the call it prepared: the
/// callee's frame is the current one.
const ENTERED: usize = usize::MAX - 1;

/// Set in [`run_call`]'s answer (with the `DoCall`'s index) when it made
/// the builtin call it prepared ([`calls::call_native`]): the caller goes
/// on after the `DoCall`.
const CALLED: usize = 1 << (usize::BITS - 2);

/// Set in [`run_call`]'s answer (with the `DoCall`'s index) when the
/// builtin call it made threw ([`calls::call_native`]; the throw is in
/// `Vm::tape_throw`).
const CALL_THREW: usize = 1 << (usize::BITS - 3);

/// The `Send`s after the op at `at` that started `call`
/// ([`send_following`]), and the call itself when its `DoCall` comes right
/// after them and it is a user one (`ZEND_DO_UCALL`: [`calls::enter`]),
/// without another round trip through the loop: [`ENTERED`]. Otherwise the
/// call is kept in `prep` for its `DoCall` (a builtin's, one `enter`
/// declines, or one whose `Send`s were not all passed here), and the op
/// where the tape goes on is returned.
#[inline(always)]
#[allow(clippy::inline_always)] // one copy per kind of call start
fn send_and_enter(
    vm: &mut Vm,
    prep: &mut Option<calls::Prepared>,
    mut call: calls::Prepared,
    ops: &[Op],
    at: usize,
) -> usize {
    let next = send_following(vm, &mut call, ops, at);
    if let Some(&Op::DoCall { dst }) = ops.get(next) {
        if call.is_native() {
            // `ZEND_DO_ICALL` at once (as the loop's `DoCall` arm runs it).
            match calls::call_native(vm, call, dst, next + 1) {
                Ok(Ok(())) => return next | CALLED,
                Ok(Err(t)) => {
                    vm.tape_throw = Some(t);
                    return next | CALL_THREW;
                }
                Err(back) => call = back,
            }
        } else {
            match calls::enter(vm, call, dst, next + 1) {
                Ok(()) => return ENTERED,
                Err(back) => call = back,
            }
        }
    }
    *prep = Some(call);
    next
}

/// The `Send`s right after the op at `at`, which started the prepared
/// `call`, passed at once ([`calls::send`]): they are its arguments (an
/// argument's own call would start with its own op), and nothing runs in
/// between, so each loop round trip saved changes nothing else. Returns
/// where the tape goes on: the op after the last one passed (the `DoCall`,
/// or a `Send` `calls::send` declined, which then runs as before).
#[inline]
fn send_following(vm: &mut Vm, call: &mut calls::Prepared, ops: &[Op], at: usize) -> usize {
    let mut next = at + 1;
    while let Some(Op::Send { src }) = ops.get(next) {
        if !calls::send(vm, call, src) {
            break;
        }
        next += 1;
    }
    next
}

mod calls;
mod dims;
mod props;
mod scalar;
mod strs;

#[cfg(test)]
mod tests;
