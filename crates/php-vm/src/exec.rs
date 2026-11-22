// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The interpreter loop (`execute_ex`) and simple instructions.

use crate::func::Func;
use crate::object;
use crate::opcode::{BinKind, CastKind, FetchMode, Op, Operand, PlaceId, UnKind};
use crate::vm::{CvSlot, FinState, Frame, PendingCall, RetTarget, Throw, Vm, VmResult};
use php_runtime::{ops, BinaryOp, ErrorLevel, PhpRef, Value, ZArray, ZStr};
use std::rc::Rc;

#[cfg(test)]
mod dispatch_tests;
mod hot;
mod spec;
pub(crate) mod tape;

/// Upper bound on nested `execute()` runs (internal-to-user callbacks,
/// destructors, `__clone`, magic methods); each one uses native stack.
const MAX_NESTED_EXECUTIONS: u32 = 20000;

/// A number peeked from an operand for the sealed [`Op::Bin`] forms.
#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    #[inline]
    #[allow(clippy::cast_precision_loss)] // php-src's (double) widening
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

/// What an instruction asks the loop to do next.
pub(crate) enum Flow {
    /// Continue with the next instruction of the current frame.
    Next,
    /// The frame stack changed: reload the current frame.
    Reload,
    /// The base frame finished with this value.
    Done(Value),
}

impl Vm {
    /// The current frame.
    #[inline]
    pub(crate) fn frame(&self) -> &Frame {
        self.frames.last().expect("active frame")
    }

    /// The current frame (mutable).
    #[inline]
    pub(crate) fn frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("active frame")
    }

    /// Reads a literal of the current frame.
    #[inline]
    pub(crate) fn lit(&self, i: u32) -> Value {
        self.frame().code.literals[i as usize].clone()
    }

    /// Reads a literal as a string.
    pub(crate) fn lit_str(&self, i: u32) -> ZStr {
        match &self.frame().code.literals[i as usize] {
            Value::Str(s) => s.clone(),
            other => ZStr::from(format!("{other:?}")),
        }
    }

    /// The current numeric value of a CV or literal operand, without
    /// cloning, warnings or reference traversal (`None` sends the op down
    /// the generic path). Always inlined into [`Vm::bin_num_fast`]: as a
    /// call it returns its `Option<Num>` through memory.
    #[inline(always)]
    #[allow(clippy::inline_always)] // as a call, the `Option<Num>` is returned through memory
    fn peek_num(f: &Frame, op: Operand) -> Option<Num> {
        match op {
            // In place, without a borrow guard (`EX_VAR`).
            Operand::Cv(cv) => f.peek_cv(cv, |s| match s {
                Some(Value::Int(i)) => Some(Num::Int(*i)),
                Some(Value::Float(x)) => Some(Num::Float(*x)),
                _ => None,
            }),
            Operand::Lit(i) => match f.code.literals.get(i as usize) {
                Some(Value::Int(v)) => Some(Num::Int(*v)),
                Some(Value::Float(v)) => Some(Num::Float(*v)),
                _ => None,
            },
            // Numbers have no Drop: the slot's value can be left in place
            // (consuming reads leave Null behind, but every later use of a
            // tmp slot is preceded by a write).
            Operand::Tmp(t) | Operand::TmpKeep(t) => match f.tmps.get(t as usize) {
                Some(Value::Int(i)) => Some(Num::Int(*i)),
                Some(Value::Float(x)) => Some(Num::Float(*x)),
                _ => None,
            },
            Operand::Unused => None,
        }
    }

    /// Sealed numeric form of [`Op::Bin`]: the handlers php-src specializes
    /// for `LONG`/`DOUBLE` operand pairs, computed without materializing
    /// `Value` temporaries. `None` when an operand is not a plain defined
    /// number or the result leaves the int domain (overflow → float
    /// promotion is the generic path's job). Mixed int/float pairs use
    /// php-src's `(double)` widening; `===`/`!==` on mixed types is a type
    /// mismatch.
    ///
    /// Always inlined: a hint is not enough inside `Vm::run`, where the
    /// release build of 4e1c2df8 called it (and [`num_bin`]) out of line
    /// from every hot arm (`Bin`, `BinAssignCv`, `AssignOpCv`): 72 + 29
    /// instructions per `$x = $x + $i`, its `Option<Value>` returned
    /// through memory.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above: the hot arms of the dispatch loop
    fn bin_num_fast(&self, op: BinKind, a: Operand, b: Operand) -> Option<Value> {
        let f = self.frame();
        let lhs = Self::peek_num(f, a)?;
        let rhs = Self::peek_num(f, b)?;
        num_bin(op, lhs, rhs)
    }

    /// [`Vm::cmp_simple_fast`] for the operators it decides (`==`, `!=`,
    /// `===`, `!==`), and `None` at once for any other. The call is out of
    /// line: in the release build of 4e1c2df8 its own early `None` still
    /// cost the call and a full prologue and epilogue (about 30
    /// instructions) on every `Bin` that missed the numeric form (`.`,
    /// `**`, arithmetic on strings) and every non-numeric `<`/`<=` branch.
    #[inline(always)]
    #[allow(clippy::inline_always)] // the operator test must stay at the call site
    fn equality_fast(&mut self, op: BinKind, a: Operand, b: Operand) -> Option<bool> {
        if matches!(
            op,
            BinKind::Equal | BinKind::NotEqual | BinKind::Identical | BinKind::NotIdentical
        ) {
            self.cmp_simple_fast(op, a, b)
        } else {
            None
        }
    }
}

/// The arithmetic of [`Vm::bin_num_fast`] on two peeked numbers: `None`
/// when the operator has no sealed numeric form, an int result overflows,
/// or the generic path must throw or convert (a zero divisor, a negative
/// shift or one of 64 bits or more, a float operand of `%`, `<<`, `>>`).
/// Always inlined: as a call, both numbers and the `Option<Value>` result
/// go through memory (here and in the specialised handlers' fast paths,
/// `exec/spec.rs`), and the operator is usually known at the call site's arm.
#[inline(always)]
#[allow(clippy::inline_always)] // part of the hot numeric fast path
fn num_bin(op: BinKind, x: Num, y: Num) -> Option<Value> {
    if let (Num::Float(xf), Num::Float(yf)) = (x, y) {
        return Some(match op {
            BinKind::Arith(BinaryOp::Add) => Value::Float(xf + yf),
            BinKind::Arith(BinaryOp::Sub) => Value::Float(xf - yf),
            BinKind::Arith(BinaryOp::Mul) => Value::Float(xf * yf),
            BinKind::Arith(BinaryOp::Div) if yf != 0.0 => Value::Float(xf / yf),
            BinKind::Equal | BinKind::Identical => Value::Bool(xf == yf),
            BinKind::NotEqual | BinKind::NotIdentical => Value::Bool(xf != yf),
            BinKind::Less => Value::Bool(xf < yf),
            BinKind::LessEqual => Value::Bool(xf <= yf),
            _ => return None,
        });
    }
    if let (Num::Int(x), Num::Int(y)) = (x, y) {
        return Some(match op {
            BinKind::Arith(BinaryOp::Add) => Value::Int(x.checked_add(y)?),
            BinKind::Arith(BinaryOp::Sub) => Value::Int(x.checked_sub(y)?),
            BinKind::Arith(BinaryOp::Mul) => Value::Int(x.checked_mul(y)?),
            BinKind::Arith(BinaryOp::BitOr) => Value::Int(x | y),
            BinKind::Arith(BinaryOp::BitAnd) => Value::Int(x & y),
            BinKind::Arith(BinaryOp::BitXor) => Value::Int(x ^ y),
            // `mod_function`: `checked_rem` refuses a zero divisor (the
            // generic path throws "Modulo by zero") and `PHP_INT_MIN % -1`
            // (the generic path answers 0, as for any `x % -1`).
            BinKind::Arith(BinaryOp::Mod) => Value::Int(x.checked_rem(y)?),
            // `div_function`: an exact quotient stays an int, any other is
            // `(double) x / y`; a zero divisor ("Division by zero") and
            // `PHP_INT_MIN / -1` (a float) are the generic path's.
            BinKind::Arith(BinaryOp::Div) => {
                if x.checked_rem(y)? == 0 {
                    Value::Int(x / y)
                } else {
                    Value::Float(Num::Int(x).as_f64() / Num::Int(y).as_f64())
                }
            }
            // `shift_left_function` / `shift_right_function` by 0..=63 bits
            // (bits shifted out are lost, `>>` is arithmetic); a negative
            // shift (ArithmeticError) or a wider one (0 or -1) goes generic.
            BinKind::Arith(BinaryOp::ShiftLeft) => {
                Value::Int(x.checked_shl(u32::try_from(y).ok()?)?)
            }
            BinKind::Arith(BinaryOp::ShiftRight) => {
                Value::Int(x.checked_shr(u32::try_from(y).ok()?)?)
            }
            BinKind::Equal | BinKind::Identical => Value::Bool(x == y),
            BinKind::NotEqual | BinKind::NotIdentical => Value::Bool(x != y),
            BinKind::Less => Value::Bool(x < y),
            BinKind::LessEqual => Value::Bool(x <= y),
            BinKind::Spaceship => Value::Int(match x.cmp(&y) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }),
            _ => return None,
        });
    }
    let mixed = matches!(
        (x, y),
        (Num::Int(_), Num::Float(_)) | (Num::Float(_), Num::Int(_))
    );
    #[allow(clippy::cast_precision_loss)] // php-src's (double) widening
    let (xf, yf) = (x.as_f64(), y.as_f64());
    Some(match op {
        BinKind::Arith(BinaryOp::Add) => Value::Float(xf + yf),
        BinKind::Arith(BinaryOp::Sub) => Value::Float(xf - yf),
        BinKind::Arith(BinaryOp::Mul) => Value::Float(xf * yf),
        // `div_function` with a double operand; a zero divisor (also -0.0)
        // throws on the generic path.
        BinKind::Arith(BinaryOp::Div) if yf != 0.0 => Value::Float(xf / yf),
        BinKind::Equal => Value::Bool(xf == yf),
        BinKind::NotEqual => Value::Bool(xf != yf),
        BinKind::Identical => Value::Bool(!mixed && xf == yf),
        BinKind::NotIdentical => Value::Bool(mixed || xf != yf),
        BinKind::Less => Value::Bool(xf < yf),
        BinKind::LessEqual => Value::Bool(xf <= yf),
        _ => return None,
    })
}

/// `ZEND_CONCAT`'s string fast path: an empty operand that is not a
/// literal yields the other operand itself (`ZVAL_STR_COPY`, so an interned
/// string stays interned); an empty *literal* does not, and the result is a
/// new string even when it equals the other operand
/// (`$row['label'] . ''` separates; ext/mysqli/tests/mysqli_result_references).
/// With a TMP left operand, a string nothing else holds is extended in
/// place (`zend_string_extend`), not copied.
fn concat_strings(a: Operand, b: Operand, s: ZStr, t: ZStr) -> ZStr {
    if s.is_empty() && !matches!(a, Operand::Lit(_)) {
        return t;
    }
    if t.is_empty() && !matches!(b, Operand::Lit(_)) {
        return s;
    }
    if matches!(a, Operand::Tmp(_)) && !t.is_empty() {
        return s.concat_owned_str(&t);
    }
    ZStr::concat_str(&s, &t)
}

impl Vm {
    /// Generic [`Op::Bin`] evaluation, out of line so the dispatch loop
    /// stays small (the sealed numeric forms handle the hot cases).
    #[inline(never)]
    fn bin_slow(&mut self, op: BinKind, a: Operand, b: Operand, dst: u32) -> VmResult<()> {
        let lhs = self.read(a)?;
        let rhs = self.read(b)?;
        let result = match (op, a, lhs, rhs) {
            // `ZEND_CONCAT` of two strings (see [`concat_strings`]).
            (BinKind::Arith(BinaryOp::Concat), _, Value::Str(s), Value::Str(t)) => {
                Value::Str(concat_strings(a, b, s, t))
            }
            // The same with an integer right operand (`"k" . $i . ...`
            // chains): its digits are appended straight from a stack
            // buffer, as `concat_function` does after `zval_get_string`.
            (BinKind::Arith(BinaryOp::Concat), Operand::Tmp(_), Value::Str(s), Value::Int(i)) => {
                Value::Str(s.concat_owned_long(i))
            }
            (_, _, lhs, rhs) => {
                let result = self.binary(op, &lhs, &rhs)?;
                drop((lhs, rhs));
                result
            }
        };
        self.set_tmp(dst, result);
        Ok(())
    }

    #[inline(never)]
    fn op_abs_slow(&mut self, src: Operand) -> VmResult<Value> {
        let arg = self.read(src)?;
        if let Some(Func::Builtin(b)) = self.functions.get(b"abs".as_slice()).cloned() {
            let mut call = PendingCall::new(Func::Builtin(b.clone()));
            call.args.push(Some(arg));
            let strict = self.frame().code.strict_types;
            self.call_builtin(&b, call, strict)
        } else {
            match arg {
                Value::Int(n) => Ok(if n == i64::MIN {
                    #[allow(clippy::cast_precision_loss)]
                    Value::Float(-(n as f64))
                } else {
                    Value::Int(n.abs())
                }),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                other => {
                    let r = php_runtime::convert::try_convert_to_number(&other, self)
                        .map_err(|e| self.rt_throw(e))?;
                    Ok(match r.unwrap_or(Value::Int(0)) {
                        Value::Int(n) => {
                            if n == i64::MIN {
                                #[allow(clippy::cast_precision_loss)]
                                Value::Float(-(n as f64))
                            } else {
                                Value::Int(n.abs())
                            }
                        }
                        Value::Float(f) => Value::Float(f.abs()),
                        other => other,
                    })
                }
            }
        }
    }

    /// `ZEND_ROPE_INIT` / `ZEND_ROPE_ADD` / `ZEND_ROPE_END`: each part is
    /// read and converted to a string in order (warnings, `__toString`),
    /// then the result is built in ONE exactly sized allocation
    /// (`zend_string_alloc(len)` + `memcpy` per part), with integer parts
    /// formatted straight into it instead of through temporary strings.
    #[inline(never)]
    pub(crate) fn op_rope(&mut self, parts: &[Operand]) -> VmResult<ZStr> {
        // Most ropes have a handful of parts: keep them on the stack.
        const INLINE: usize = 8;
        let mut inline: [Value; INLINE] = Default::default();
        let mut spill: Vec<Value> = Vec::new();
        let vals: &mut [Value] = if parts.len() <= INLINE {
            &mut inline[..parts.len()]
        } else {
            spill.resize(parts.len(), Value::Null);
            spill.as_mut_slice()
        };
        let mut len = 0usize;
        for (slot, p) in vals.iter_mut().zip(parts) {
            let v = match self.read(*p)? {
                v @ (Value::Str(_) | Value::Int(_)) => v,
                v => Value::Str(self.to_zstr(&v)?),
            };
            len = len.saturating_add(match &v {
                Value::Str(s) => s.len(),
                Value::Int(i) => php_runtime::string::long_str_len(*i),
                _ => 0,
            });
            *slot = v;
        }
        let mut out = php_runtime::string::ZStrBuf::with_capacity(len);
        for v in vals.iter() {
            match v {
                Value::Str(s) => out.push_str(s),
                Value::Int(i) => out.push_long(*i),
                _ => {}
            }
        }
        Ok(out.finish())
    }

    /// Reads an operand (dereferenced), warning on undefined variables.
    pub(crate) fn read(&mut self, op: Operand) -> VmResult<Value> {
        match op {
            Operand::Tmp(t) => {
                let v = std::mem::take(&mut self.frame_mut().tmps[t as usize]);
                Ok(match v {
                    Value::Reference(r) => r.get(),
                    v => v,
                })
            }
            Operand::TmpKeep(t) => Ok(self.frame().tmps[t as usize].deref()),
            Operand::Lit(i) => Ok(self.lit(i)),
            Operand::Cv(cv) => self.read_cv(cv),
            Operand::Unused => Ok(Value::Null),
        }
    }

    /// Reads an operand without dereferencing temporaries holding
    /// references.
    pub(crate) fn read_raw(&mut self, op: Operand) -> VmResult<Value> {
        match op {
            Operand::Tmp(t) => Ok(std::mem::take(&mut self.frame_mut().tmps[t as usize])),
            Operand::TmpKeep(t) => Ok(self.frame().tmps[t as usize].clone()),
            other => self.read(other),
        }
    }

    /// Reads a compiled variable (dereferenced), warning when undefined.
    /// The defined case is a borrow-free peek and a clone ([`Frame::peek_cv`]);
    /// the warning is out of line.
    #[inline]
    pub(crate) fn read_cv(&mut self, cv: u32) -> VmResult<Value> {
        match self.read_cv_defined(cv) {
            Some(v) => Ok(v),
            None => self.read_undefined_cv(cv),
        }
    }

    /// The undefined-variable path of [`Vm::read_cv`]: the warning, then null.
    #[cold]
    #[inline(never)]
    fn read_undefined_cv(&mut self, cv: u32) -> VmResult<Value> {
        let name = self.frame().code.cv_names[cv as usize].clone();
        self.warning(&crate::exec::undefined_var_msg(&name))?;
        Ok(Value::Null)
    }

    /// [`Vm::read_cv`] for a defined variable (dereferenced); `None` when
    /// undefined, leaving the warning to the caller.
    ///
    /// Always inlined: as a call (what the inline hint got in `Vm::read`,
    /// release build of 4e1c2df8) the call, the argument set-up and the
    /// `Option<Value>` returned through memory are about a fifth of its ~45
    /// instructions, paid for every compiled variable an instruction reads.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above: every CV operand read
    pub(crate) fn read_cv_defined(&self, cv: u32) -> Option<Value> {
        self.frame().peek_cv(cv, |s| match s {
            Some(Value::Reference(r)) => Some(r.get()),
            Some(v) => Some(v.clone()),
            None => None,
        })
    }

    /// The `OP_DATA` read of [`Op::Assign`] from a compiled variable:
    /// [`Vm::read_cv_defined`], except that a reference to null, false or
    /// an array is left to the place walk, which reads it after fetching
    /// the containers ([`Vm::place_assign_cv_ref`]).
    ///
    /// Always inlined: `Vm::run`'s `Assign` arm called it out of line in
    /// the release build of 4e1c2df8, the `CvOpData` returned through
    /// memory, on every `$obj->p = $v` / `$a[$k] = $v`.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above
    pub(crate) fn read_cv_op_data(&self, cv: u32) -> CvOpData {
        self.frame().peek_cv(cv, |s| match s {
            Some(Value::Reference(r)) => {
                let inner = r.borrow();
                if matches!(*inner, Value::Null | Value::Bool(false) | Value::Array(_)) {
                    CvOpData::Deferred
                } else {
                    CvOpData::Value(inner.clone())
                }
            }
            Some(v) => CvOpData::Value(v.clone()),
            None => CvOpData::Undefined,
        })
    }

    /// The undefined compiled variable `val` of `place op= val` when the
    /// walk reads it: `place` ends in a dimension
    /// ([`Vm::place_assign_op_undef_cv`]).
    pub(crate) fn assign_op_undef_dim_cv(&self, place: PlaceId, val: Operand) -> Option<u32> {
        let Operand::Cv(cv) = val else {
            return None;
        };
        (self.frame().peek_cv(cv, |s: Option<&Value>| s.is_none())
            && self.assign_op_reads_value_late(place))
        .then_some(cv)
    }

    /// Reads a compiled variable without warning (`None` if undefined).
    #[inline]
    pub(crate) fn cv_get(&self, cv: u32) -> Option<Value> {
        self.frame()
            .peek_cv(cv, |slot: Option<&Value>| slot.cloned())
    }

    /// Assigns to a compiled variable (through references). A typed
    /// reference is handed back with the value, unassigned: the caller
    /// assigns through it with [`Vm::assign_ref`].
    ///
    /// The common case, a slot that is undefined or holds no reference,
    /// array or object (nothing to assign through, no possible GC root to
    /// buffer), is a plain store; the old value is dropped without the
    /// out-of-line drop call when it is a scalar.
    #[must_use]
    #[inline]
    pub(crate) fn cv_assign(&mut self, cv: u32, v: Value) -> Option<(PhpRef, Value)> {
        let f = self.frame_mut();
        let idx = cv as usize;
        let slot = f.cvs.get_mut(idx)?;
        if matches!(
            slot.get(),
            Some(Value::Reference(_) | Value::Array(_) | Value::Object(_))
        ) {
            return self.cv_assign_slow(cv, v);
        }
        let old = slot.set(Some(v.clone()));
        f.sync_cv_to_symtab(cv, &v);
        discard_unrooted(old);
        None
    }

    /// [`Vm::cv_assign`] when the slot holds a reference (assigned
    /// through), an array or an object (a possible GC root once released).
    #[must_use]
    #[inline(never)]
    fn cv_assign_slow(&mut self, cv: u32, v: Value) -> Option<(PhpRef, Value)> {
        let f = self.frame_mut();
        let idx = cv as usize;
        let slot = f.cvs.get(idx)?;
        let is_ref = matches!(slot.get(), Some(Value::Reference(_)));
        if !is_ref && f.symtab_attached {
            f.unsync_cv_from_symtab(cv);
        }
        let old = match slot.get() {
            Some(Value::Reference(r)) if crate::typed_refs::has_type_sources(r) => {
                return Some((r.clone(), v));
            }
            Some(Value::Reference(r)) => Some(r.set(v.clone())),
            _ => slot.set(Some(v.clone())),
        };
        if !is_ref {
            f.sync_cv_to_symtab(cv, &v);
        }
        if let Some(old) = &old {
            crate::gc::possible_root(old);
        }
        drop(old);
        None
    }

    /// Fast path for reading places directly: direct auto-globals (`$_GET`)
    /// without breaking to the generic place fetcher.
    #[inline(always)]
    fn fetch_place_fast(&mut self, place: PlaceId) -> Option<Value> {
        enum FastKind {
            Get,
            AutoGlobal(usize),
            None,
        }
        let kind = {
            let f = self.frames.last()?;
            let p = f.code.places.get(place as usize)?;
            if !p.segs.is_empty() {
                FastKind::None
            } else {
                match &p.base {
                    crate::opcode::PlaceBase::Global(crate::opcode::Operand::Lit(lit)) => {
                        let val = f.code.literals.get(*lit as usize)?;
                        if let Value::Str(s) = val {
                            let b = s.as_bytes();
                            if b == b"_GET" {
                                FastKind::Get
                            } else if b.first() == Some(&b'_') {
                                match crate::auto_globals::index(b) {
                                    Some(idx) => FastKind::AutoGlobal(idx),
                                    None => FastKind::None,
                                }
                            } else {
                                FastKind::None
                            }
                        } else {
                            FastKind::None
                        }
                    }
                    _ => FastKind::None,
                }
            }
        };
        let (idx, val) = match kind {
            FastKind::Get => (0, self.get_auto_global_get()),
            FastKind::AutoGlobal(idx) => (idx, self.get_auto_global(idx)),
            FastKind::None => return None,
        };
        if val.is_null() {
            if let Some(slot) = self.ext.auto_globals.cached_slots[idx] {
                if self
                    .globals
                    .borrow()
                    .slots
                    .get(slot as usize)
                    .map_or(true, |s| s.is_none())
                {
                    return None;
                }
            } else {
                return None;
            }
        }
        Some(val)
    }

    /// `ZEND_ASSIGN` with a CV result operand (`$cv = val`, the
    /// [`Op::AssignCv`] instruction, `zend_assign_to_variable`) in place
    /// on the current frame (`hot::assign_cv_in`): php-src's
    /// `ZEND_ASSIGN_SPEC_CV_{CONST,TMP,CV}` fast case: the source is a
    /// literal, a temporary or a defined compiled variable whose value
    /// needs no dereferencing work with side effects, and the target slot
    /// is undefined or holds a value whose release needs no GC root (no
    /// reference to assign through, no array or object). One store into
    /// the slot, no intermediate `VmResult`, no drop call for a scalar old
    /// value. Returns `false`, having changed nothing, when the generic
    /// path must run.
    #[inline(always)]
    #[allow(clippy::inline_always)] // the point is to avoid the call and the Value moves through memory
    fn assign_cv_fast(&mut self, cv: u32, val: Operand, dst: Option<u32>) -> bool {
        self.frames
            .last_mut()
            .is_some_and(|f| hot::assign_cv_in(f, cv, val, dst))
    }

    /// The generic `AssignCv` (reads that warn, references on either side,
    /// old values that are possible GC roots), after
    /// [`Vm::assign_cv_fast`] did not apply.
    #[inline(never)]
    fn assign_cv_generic(&mut self, cv: u32, val: Operand, dst: Option<u32>) -> VmResult<()> {
        let v = self.read(val)?;
        let v = self.copy_on_assign(v);
        if let Some(d) = dst {
            self.set_tmp(d, v.clone());
        }
        if let Some((r, v)) = self.cv_assign(cv, v) {
            let v = self.assign_ref(&r, v)?;
            if let Some(d) = dst {
                self.set_tmp(d, v);
            }
        }
        Ok(())
    }

    /// Binds a compiled variable to a reference.
    ///
    /// Also the store of `RECV`: the slot is then undefined, and nothing
    /// is released.
    #[inline]
    pub(crate) fn cv_bind(&mut self, cv: u32, v: Value) {
        let f = self.frame_mut();
        let idx = cv as usize;
        if let Some(slot) = f.cvs.get_mut(idx) {
            let old = slot.set(Some(v.clone()));
            f.sync_cv_to_symtab(cv, &v);
            release_slot_value(old);
        }
    }

    /// Stores a temporary.
    ///
    /// Always inlined: `Vm::run` called it out of line from 17 arms in the
    /// release build of 4e1c2df8 (the inline hint was not taken there),
    /// ~25 instructions for a 16-byte store and a tag test.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above: the result store of most instructions
    pub(crate) fn set_tmp(&mut self, t: u32, v: Value) {
        let slot = &mut self.frame_mut().tmps[t as usize];
        if let (Value::Float(ref mut dst_f), Value::Float(rf)) = (&mut *slot, &v) {
            *dst_f = *rf;
            return;
        }
        let old = std::mem::replace(slot, v);
        // The slot almost always holds a scalar (or the `Null` a consuming
        // read left behind): skip the out-of-line `drop_in_place` call.
        if matches!(
            old,
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
        ) {
            std::mem::forget(old);
        }
    }

    /// The boolean held by temporary `op`, consumed, if it holds one: the
    /// `IS_TRUE`/`IS_FALSE` fast path of php-src's `ZEND_JMPZ`/`ZEND_JMPNZ`
    /// handlers (comparison results), without the out-of-line read,
    /// conversion and drop of the generic path. A compiled variable holding
    /// a plain value is tested in place (`if ($found)`), without the clone;
    /// anything else (undefined: warning, references, objects, resources)
    /// takes the generic path.
    ///
    /// Always inlined: the hot `JmpZ`/`JmpNz` arms of `Vm::run` called it
    /// out of line in the release build of 4e1c2df8.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above: the jump fast path
    fn take_tmp_bool(&mut self, op: Operand) -> Option<bool> {
        let t = match op {
            Operand::Tmp(t) => t,
            // In place (`ZEND_JMPZ_SPEC_CV`'s type switch): null, bool,
            // int, non-NaN float, string, array; undefined (warning),
            // references, objects and resources go generic.
            Operand::Cv(cv) => return self.peek_cv_bool(cv),
            _ => return None,
        };
        let slot = self.frame_mut().tmps.get_mut(t as usize)?;
        if let Value::Bool(b) = *slot {
            // A bool has nothing to drop: overwrite it without the
            // out-of-line `drop_in_place` a plain assignment calls.
            std::mem::forget(std::mem::take(slot));
            Some(b)
        } else {
            None
        }
    }

    /// The truth value of a jump operand: a boolean temporary (consumed),
    /// a compiled variable holding a plain value, or the generic read and
    /// conversion (`ZEND_JMPZ`/`ZEND_JMPNZ` with their inline type switch).
    #[inline]
    fn jump_cond(&mut self, op: Operand) -> VmResult<bool> {
        if let Some(b) = self.take_tmp_bool(op) {
            return Ok(b);
        }
        let v = self.read(op)?;
        self.to_bool(&v)
    }

    /// Stores an optional result. Always inlined, like [`Vm::set_tmp`]
    /// (five arms of `Vm::run` called it out of line in the release build
    /// of 4e1c2df8).
    #[inline(always)]
    #[allow(clippy::inline_always)] // see `set_tmp`
    pub(crate) fn set_dst(&mut self, dst: Option<u32>, v: Value) {
        if let Some(t) = dst {
            self.set_tmp(t, v);
        } else if matches!(
            v,
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
        ) {
            // An unused scalar result (`$i++;`): skip the out-of-line
            // `drop_in_place` call, as `set_tmp` does for the old value.
            std::mem::forget(v);
        }
    }

    /// Converts a runtime error into a throw (creating the exception).
    pub(crate) fn rt_throw(&mut self, e: php_runtime::RuntimeError) -> Throw {
        use php_runtime::RuntimeError as E;
        if let Some(t) = self.pending_throw.take() {
            return t;
        }
        match e {
            E::Thrown(Value::Object(o)) => Throw::Exception(o),
            E::Thrown(_) => Throw::Fatal,
            // Recorded and pending: reported where it leaves the builtin.
            E::MemoryLimit(_) => Throw::Fatal,
            E::Fatal(m) => {
                let _ = self.fatal(&m);
                Throw::Fatal
            }
            E::Error(m) => self.make_throw("Error", &m),
            E::TypeError(m) => self.make_throw("TypeError", &m),
            E::ValueError(m) => self.make_throw("ValueError", &m),
            E::ArithmeticError(m) => self.make_throw("ArithmeticError", &m),
            E::DivisionByZeroError(m) => self.make_throw("DivisionByZeroError", &m),
            E::ArgumentCountError(m) => self.make_throw("ArgumentCountError", &m),
            E::Custom { class, message } => {
                let c = class.to_string_lossy();
                self.make_throw(&c, &message)
            }
        }
    }

    /// Runs the frames from `base` until the base frame returns.
    pub fn execute(&mut self, base: usize) -> VmResult<Value> {
        // `zend_call_stack_overflowed()`: nested executions (callbacks,
        // destructors, __clone, ...) use the native stack.
        if self.native_stack_exhausted() {
            self.observer_unwind(base, None);
            self.frames.truncate(base);
            return Err(crate::builtins::dump::stack_size_error(self));
        }
        // `zend_call_function`: a pending OOM is raised before user code
        // called from a builtin runs.
        if php_memlimit::is_pending() {
            self.observer_unwind(base, None);
            self.frames.truncate(base);
            self.raise_pending_oom()?;
            return Err(Throw::Fatal);
        }
        if self.depth >= MAX_NESTED_EXECUTIONS {
            self.observer_unwind(base, None);
            // A backstop for native stacks whose size we were not told.
            self.frames.truncate(base);
            return Err(crate::builtins::dump::stack_size_error(self));
        }
        self.depth += 1;
        let guard = crate::interrupt::ExecutionGuard::enter();
        let r = self.execute_inner(base);
        drop(guard);
        self.depth -= 1;
        r
    }

    fn execute_inner(&mut self, base: usize) -> VmResult<Value> {
        loop {
            match self.run(base) {
                Ok(v) => return Ok(v),
                Err(Throw::Exception(e)) => {
                    if let Err(t) = self.handle_exception(e, base) {
                        return Err(t);
                    }
                }
                Err(t) => {
                    self.observer_unwind(base, Some(&t));
                    // Before dropping the frames being unwound, persist any
                    // global CVs (e.g. `$fiber` in the main frame) into the
                    // shared globals store so values it uniquely owns survive
                    // the frame's destruction and remain visible to `register_shutdown_function`
                    // handlers and object destructors (matching zend's
                    // `zend_attach_symbol_table` semantics on the main frame).
                    self.attach_globals_symbol_table();
                    self.frames.truncate(base);
                    return Err(t);
                }
            }
        }
    }

    /// Finds a handler for `exc` in the frames above `base`. Returns
    /// `Ok(())` when execution can continue, or the throw that escapes.
    pub(crate) fn handle_exception(
        &mut self,
        mut exc: php_runtime::Obj,
        base: usize,
    ) -> VmResult<()> {
        loop {
            if self.frames.len() <= base {
                // Thrown after the frames of this execution were left
                // (e.g. by a destructor run on the way out).
                return Err(Throw::Exception(exc));
            }
            let fidx = self.frames.len() - 1;
            let f = self.frames.last_mut().expect("frame");
            if !f.silence_saved.is_empty() {
                // `cleanup_live_vars` for ZEND_LIVE_SILENCE ranges
                // (Zend/zend_execute.c): each pending `@` restores its
                // saved mask only while the current one is still silenced.
                let saved = std::mem::take(&mut f.silence_saved);
                for old in saved.into_iter().rev() {
                    self.error_reporting = silence_restore(self.error_reporting, old);
                }
            }
            if !self.frame().code.live_tmps.is_empty() {
                exc = match self.release_live_tmps(exc) {
                    Ok(e) => e,
                    Err(t) => return Err(self.abandon_unwinding(t, base)),
                };
            }
            let f = self.frames.last_mut().expect("frame");
            let throw_ip = f.ip.saturating_sub(1) as u32;
            let code = f.code.clone();
            for entry in &code.try_table {
                let in_try = throw_ip >= entry.try_start && throw_ip < entry.try_end;
                if in_try {
                    if let Some(cs) = entry.catch_start {
                        exc = match self.free_left_iterators(&code, throw_ip, cs, exc) {
                            Ok(e) => e,
                            Err(t) => return Err(self.abandon_unwinding(t, base)),
                        };
                        let f = self.frame_mut();
                        f.ip = cs as usize;
                        ctor_failed_unfinished(&f.calls);
                        f.calls.clear();
                        // `cleanup_live_vars`: the try body's temporaries
                        // (e.g. the left operand of `new A + new B()`) are
                        // released, and destructed, before the catch runs.
                        let (ts, te) = entry.try_tmps;
                        let dead: Vec<Value> = f
                            .tmps
                            .get_mut(ts as usize..te as usize)
                            .map(|t| t.iter_mut().map(std::mem::take).collect())
                            .unwrap_or_default();
                        drop(dead);
                        let exc = match self.destruct_while_unwinding(exc) {
                            Ok(e) => e,
                            Err(t) => return Err(self.abandon_unwinding(t, base)),
                        };
                        self.frame_mut().pending_exc = Some(exc);
                        return Ok(());
                    }
                }
                if let Some((fin, fs, fe)) = entry.finally {
                    if throw_ip >= entry.try_start && throw_ip < entry.catch_end {
                        exc = match self.free_left_iterators(&code, throw_ip, fs, exc) {
                            Ok(e) => e,
                            Err(t) => return Err(self.abandon_unwinding(t, base)),
                        };
                        let f = self.frame_mut();
                        f.fins[fin as usize] = FinState::Exception(exc);
                        f.ip = fs as usize;
                        ctor_failed_unfinished(&f.calls);
                        f.calls.clear();
                        return Ok(());
                    }
                    if throw_ip >= fs && throw_ip < fe {
                        let f = self.frame_mut();
                        let prev = std::mem::take(&mut f.fins[fin as usize]);
                        if let FinState::Exception(p) = prev {
                            if crate::fiber::is_graceful_exit(&exc)
                                && crate::generator::is_close_marker(&exc)
                            {
                                // A force-closed generator rethrows it.
                                exc = p;
                            } else {
                                crate::exceptions::chain_previous(&exc, &p);
                            }
                        }
                    }
                }
            }
            // Unwind this frame (observers see its end first). Then
            // `cleanup_unfinished_calls` + `cleanup_live_vars`: its
            // unfinished calls and live temporaries (foreach iterators,
            // intermediate results) are released while it is still the
            // current frame.
            if self.observer.enabled {
                self.observer_end_frame(fidx, None, Some(&Throw::Exception(exc.clone())));
            }
            self.release_frame_temporaries();
            exc = match self.destruct_while_unwinding(exc) {
                Ok(e) => e,
                Err(t) => return Err(self.abandon_unwinding(t, base)),
            };
            if fidx <= base && self.frames[fidx].kind == crate::vm::FrameKind::Main {
                return Err(Throw::Exception(exc));
            }
            let popped = self.frames.pop();
            if let Some(p) = &popped {
                // `i_free_compiled_variables`: released values may be
                // cycle roots (and may fill the root buffer).
                crate::gc::frame_released(p, self.frames.last().and_then(|c| c.this.as_ref()));
                if let Some(g) = &p.generator {
                    crate::generator::mark_finished(g);
                }
                // `zend_object_store_ctor_failed`: an object whose
                // constructor threw is never destructed.
                if p.is_ctor {
                    if let Some(t) = &p.this {
                        let d = object::data(t);
                        d.flags.set(d.flags.get() | object::oflags::DESTRUCTED);
                    }
                }
            }
            // `zend_leave_helper`: the compiled variables (and `$this`) are
            // released with the caller current.
            drop(popped);
            exc = match self.destruct_while_unwinding(exc) {
                Ok(e) => e,
                Err(t) => return Err(self.abandon_unwinding(t, base)),
            };
            if fidx <= base {
                return Err(Throw::Exception(exc));
            }
        }
    }

    /// `cleanup_live_vars` (`Zend/zend_execute.c)`: releases the temporaries
    /// live at the throwing op, in order, before a handler of this frame
    /// runs; their destructors see `exc` in flight.
    #[cold]
    fn release_live_tmps(&mut self, exc: php_runtime::Obj) -> Result<php_runtime::Obj, Throw> {
        let f = self.frame_mut();
        #[allow(clippy::cast_possible_truncation)]
        let throw_ip = f.ip.saturating_sub(1) as u32;
        let code = f.code.clone();
        let mut dead = Vec::new();
        for l in &code.live_tmps {
            if (l.start..l.end).contains(&throw_ip) {
                if let Some(v) = f.tmps.get_mut(l.tmp as usize) {
                    dead.push(std::mem::take(v));
                }
            }
        }
        if dead.is_empty() {
            return Ok(exc);
        }
        drop(dead);
        self.destruct_while_unwinding(exc)
    }

    /// `cleanup_live_vars(op_num, catch_op_num)` for foreach iterators: an
    /// exception handled in the frame it was thrown in first releases the
    /// iterators of the loops it leaves (outermost first, the order of
    /// the live-range table). Their destructors, and those of the operands
    /// the throwing op freed, run with `exc` in flight before the handler
    /// (`zend_objects_destroy_object`), so their exceptions chain to it.
    #[cold]
    fn free_left_iterators(
        &mut self,
        code: &crate::opcode::OpArray,
        throw_ip: u32,
        target: u32,
        exc: php_runtime::Obj,
    ) -> Result<php_runtime::Obj, Throw> {
        if self.frame().iters.iter().any(Option::is_some) {
            for (at, op) in code.ops.iter().enumerate() {
                if let Op::FeReset { iter, end, .. } = op {
                    // Live from after FE_RESET up to (excluding) the FE_FREE.
                    let start = u32::try_from(at).unwrap_or(u32::MAX);
                    let live = |ip: u32| start < ip && ip < *end;
                    if live(throw_ip) && !live(target) {
                        self.op_fe_free(*iter);
                    }
                }
            }
        }
        self.destruct_while_unwinding(exc)
    }

    /// Releases the temporaries, foreach iterators and unfinished calls of
    /// the current frame (which an exception is about to leave).
    fn release_frame_temporaries(&mut self) {
        // `cleanup_unfinished_calls` first, innermost call first, each
        // releasing its arguments before `$this`; then `cleanup_live_vars`.
        // (The order decides which object handles are reused first.)
        let mut calls = std::mem::take(&mut self.frame_mut().calls);
        // An abandoned `new` is not destructed (`zend_object_store_ctor_failed`).
        ctor_failed_unfinished(&calls);
        while let Some(mut call) = calls.pop() {
            for slot in &mut call.inline_args[..call.inline_count as usize] {
                *slot = None;
            }
            call.inline_count = 0;
            call.args.clear();
            call.named.clear();
            drop(call);
        }
        let n = u32::try_from(self.frame().iters.len()).unwrap_or(u32::MAX);
        for i in 0..n {
            self.op_fe_free(i);
        }
        let f = self.frame_mut();
        let tmps: Vec<Value> = f.tmps.iter_mut().map(std::mem::take).collect();
        drop(tmps);
    }

    /// Runs the destructors of objects released while `exc` unwinds the
    /// stack (`zend_objects_destroy_object` with an exception in flight):
    /// an exception a destructor throws gets `exc` as its previous one and
    /// continues the unwinding in its place.
    fn destruct_while_unwinding(
        &mut self,
        exc: php_runtime::Obj,
    ) -> Result<php_runtime::Obj, Throw> {
        if !object::has_pending_destructors() {
            if crate::gc::wants_run() {
                return self.collect_while_unwinding(exc);
            }
            return Ok(exc);
        }
        let exc = match self.run_pending_destructors() {
            Ok(()) => exc,
            Err(Throw::Exception(new)) if !self.is_executing() => {
                self.destructor_threw_without_frame(new)?;
                exc
            }
            Err(Throw::Exception(new)) => {
                crate::exceptions::chain_previous(&new, &exc);
                new
            }
            Err(t) => return Err(t),
        };
        if crate::gc::wants_run() {
            return self.collect_while_unwinding(exc);
        }
        Ok(exc)
    }

    /// The values released while `exc` unwinds filled the root buffer
    /// (`gc_possible_root_when_full` inside `i_free_compiled_variables`):
    /// the collection runs now, before a handler is found. An exception
    /// thrown by a garbage destructor gets `exc` as its previous one and
    /// continues the unwinding in its place (`zend_objects_destroy_object`,
    /// `remember_prev_exception` in a fiber).
    #[cold]
    #[inline(never)]
    fn collect_while_unwinding(
        &mut self,
        exc: php_runtime::Obj,
    ) -> Result<php_runtime::Obj, Throw> {
        match self.gc_safe_point() {
            Ok(()) => Ok(exc),
            Err(Throw::Exception(new)) => {
                crate::exceptions::chain_previous(&new, &exc);
                Ok(new)
            }
            Err(t) => Err(t),
        }
    }

    /// A destructor run while an exception leaves the outermost frame
    /// (`zend_leave_helper` makes the caller, here none, current before
    /// freeing the compiled variables) threw: `zend_call_function` with no
    /// `EG(current_execute_data)` reports it at once
    /// (`zend_throw_exception_internal`: the user exception handler, else
    /// the uncaught fatal and a bailout), without chaining it to the
    /// exception in flight. `Ok` (handled by the user handler) keeps that
    /// one unwinding (`zend_objects_destroy_object` restores it).
    #[cold]
    #[inline(never)]
    fn destructor_threw_without_frame(&mut self, new: php_runtime::Obj) -> VmResult<()> {
        if let Err(t) = self.uncaught_exception(new) {
            object::set_destructors_enabled(false);
            return Err(t);
        }
        Ok(())
    }

    /// A destructor run during unwinding exited or bailed out: the frames
    /// of this execution are abandoned.
    fn abandon_unwinding(&mut self, t: Throw, base: usize) -> Throw {
        self.observer_unwind(base, Some(&t));
        self.frames.truncate(base);
        t
    }

    /// The interpreter loop proper (`execute_ex`).
    ///
    /// Like php-src's VM, which keeps `opline` in a machine register and
    /// stores it to `EX(opline)` only where a handler may observe it
    /// (`SAVE_OPLINE()`), the loop runs on a local `ip` (the index of the
    /// next instruction) and the op array's instruction slice held in
    /// locals, instead of looking up the current frame and writing its
    /// `ip` back for every instruction:
    ///
    /// * Hot instructions whose fast path cannot observe the frame's `ip`
    ///   (no warning, no throw, no user code: jumps on a boolean temporary,
    ///   the sealed numeric [`Op::Bin`] forms) run first, on the local `ip`
    ///   alone (`ZEND_VM_NEXT_OPCODE` / `ZEND_VM_SET_OPCODE`).
    /// * Every other instruction is dispatched after storing the local
    ///   `ip` into the frame (`SAVE_OPLINE`). Within one instruction that
    ///   returns [`Flow::Next`], only this loop moves the current frame's
    ///   `ip` (jumps write the local), so the local stays authoritative and
    ///   is not reloaded. [`Flow::Reload`] (a frame was pushed or popped)
    ///   reloads the frame's code and `ip` (`LOAD_OPLINE` after
    ///   `ZEND_VM_ENTER`/`ZEND_VM_LEAVE`).
    /// * The safe point after an instruction ([`Vm::safe_point_due`]) is
    ///   one inline flag test; its work, which may report the current
    ///   line, stores the local `ip` first ([`Vm::safe_point`]).
    #[allow(clippy::too_many_lines)]
    fn run(&mut self, base: usize) -> VmResult<Value> {
        'frames: loop {
            // `LOAD_OPLINE()`: one lookup of the current frame, its `ip`
            // and its op array (`EX(func)->op_array`), borrowed without a
            // reference count: two calls re-enter here twice each (call
            // and return), and an `Rc` clone and drop on each was a
            // measurable part of a call.
            let top = self.frame();
            let mut ip = top.ip;
            // SAFETY: the op array is owned by `Rc`s, one of them held by
            // the current frame, so its heap block does not move when the
            // frame does (the frame stack's vector growing, a fiber or a
            // generator moving its frames out and back). The block is
            // only released when the last `Rc` is dropped, which for the
            // current frame's handle happens only when the frame is popped
            // or recycled (`FrameStack::release_top`, a generator or fiber
            // dropping its frames) - the instructions that do that return
            // `Flow::Reload` or `Flow::Done`, or are the fast `DoCall` and
            // `Return` arms, which all leave this iteration of `'frames`
            // without reading `code`, `ops` or `op` again (their safe
            // point gets no instruction: `safe_point_after_leave`), or
            // throw, which returns from `run`. Within one instruction
            // that returns `Flow::Next` the current frame stays on the
            // stack with the same op array (nested `run`s for callbacks
            // and destructors push and pop frames above it only), so
            // every use of `code`, `ops` and `op` below happens while
            // that frame's handle keeps the block alive.
            //
            // `tape::run_tape` makes plain calls and returns itself: when it
            // comes back in another frame (`hot::SWITCHED`), `code` and
            // `ops` are reloaded from that frame below, before any use, as
            // here (the frame it left may have been freed with its op
            // array; `op` is not read again either).
            let mut code: &crate::opcode::OpArray = unsafe { &*Rc::as_ptr(&top.code) };
            let mut ops: &[Op] = &code.ops;
            'ops: loop {
                let at = ip;
                let mut op = &ops[at];
                ip = at + 1;
                // Hot instructions on the local `ip` (no frame store): their
                // fast paths cannot warn, throw, run user code or change the
                // frame stack. A specialised op (`exec/spec.rs`) whose fast
                // path does not apply has changed nothing and breaks to the
                // big match, which runs the generic op it replaced.
                'slow: {
                    // The frame-local hot ops (`exec/hot.rs`): one runs here
                    // (`hot::step`, inline); when the op after it is hot as
                    // well, they run in `tape::run_tape`'s own loop, on the
                    // frame and `ip` in registers, until an op it does not
                    // handle or whose fast path declined (that op then runs
                    // here: a declined hot op as its generic op, any other
                    // through this pre-match), or a due safe point. Plain
                    // user calls and returns among them run there as well,
                    // which then goes on in the callee's or the caller's
                    // frame (`hot::SWITCHED`: reloaded here).
                    if hot::is_hot(op) {
                        // An op with a specialised instruction that is
                        // followed by another starts on the tape; any other
                        // hot op runs here first, and the tape takes over
                        // when the op after it is a hot op too (a lone one
                        // is not worth the call).
                        let start = if tape::worth_entering(code, at) {
                            at
                        } else {
                            let Some(f) = self.frames.last_mut() else {
                                break 'slow;
                            };
                            let lits: &[Value] = &code.literals;
                            let next = hot::step(f, ops, lits, at);
                            if next == hot::DECLINED {
                                break 'slow;
                            }
                            ip = next;
                            if Self::safe_point_due() {
                                ip = self.safe_point(ip, op)?;
                                continue 'ops;
                            }
                            if !ops.get(ip).is_some_and(hot::enters_hot) {
                                continue 'ops;
                            }
                            ip
                        };
                        let (mut next, last) = tape::run_tape(self, base, code, start);
                        if last == hot::LEFT {
                            // A call or return there, with the safe point
                            // after it due (as after the fast `DoCall` and
                            // `Return` arms below).
                            self.safe_point_after_leave()?;
                            continue 'frames;
                        }
                        if next & hot::SWITCHED != 0 {
                            // `LOAD_OPLINE()` of the frame it went on in.
                            next &= !hot::SWITCHED;
                            // SAFETY: as for `code` above; the current
                            // frame holds this op array.
                            code = unsafe { &*Rc::as_ptr(&self.frame().code) };
                            ops = &code.ops;
                        }
                        if last != hot::NO_SAFE_POINT {
                            // A builtin called on the tape threw: the frame's
                            // resume point is stored (`SAVE_OPLINE`), and the
                            // exception unwinds from there.
                            if last == hot::THREW {
                                return Err(self.tape_throw.take().unwrap_or(Throw::Fatal));
                            }
                            ip = self.safe_point(next, &ops[last])?;
                            continue 'ops;
                        }
                        ip = next;
                        // A hot op whose fast path declined, or a call or
                        // return `run_tape` did not make: the generic op.
                        if hot::enters_hot(&ops[next]) {
                            op = &ops[next];
                            ip = next + 1;
                            break 'slow;
                        }
                        continue 'ops;
                    }
                    match op {
                        Op::Abs { src, dst, to } => {
                            if let Some(val) = self.abs_fast(*src) {
                                if let Some(cv) = *to {
                                    if let Some(d) = *dst {
                                        if self.store_fetched(d, Some(cv), val) {
                                            ip += 3;
                                        } else {
                                            ip += 2;
                                        }
                                    } else if self.cv_store_plain(cv, val).is_ok() {
                                        ip += 3;
                                    } else {
                                        ip += 2;
                                    }
                                } else {
                                    if let Some(d) = *dst {
                                        self.set_tmp(d, val);
                                    }
                                    ip += 2;
                                }
                            } else {
                                break 'slow;
                            }
                        }
                        // The array-dimension ops, `.=` on a variable and the
                        // static-property and constant cache hits are hot ops
                        // (`exec/hot.rs`): a declined one runs as its generic op.
                        Op::Fetch { place, mode, dst } => {
                            if *mode == FetchMode::Read {
                                if let Some(v) = self.fetch_place_fast(*place) {
                                    self.set_tmp(*dst, v);
                                } else {
                                    break 'slow;
                                }
                            } else {
                                break 'slow;
                            }
                        }
                        Op::FetchGlobal {
                            place,
                            dst,
                            to,
                            mode,
                        } => match self.fetch_global_fast(*place, *mode) {
                            Some(v) => {
                                if self.store_fetched(*dst, *to, v) {
                                    ip += 1;
                                }
                            }
                            None => break 'slow,
                        },
                        Op::AssignOpStaticProp {
                            place,
                            op: kind,
                            val,
                            dst,
                        } => {
                            if !self.assign_op_static_prop_fast(*place, *kind, *val, *dst) {
                                break 'slow;
                            }
                        }
                        // Object properties on a cache hit (`exec/spec/props.rs`).
                        Op::FetchObjProp {
                            place,
                            dst,
                            to,
                            mode,
                        } => match self.fetch_obj_prop_fast(*place, *dst, *to, *mode) {
                            Some(true) => ip += 1,
                            Some(false) => {}
                            None => break 'slow,
                        },
                        Op::FetchObjPropDim {
                            place,
                            dim,
                            dst,
                            to,
                            mode,
                        } => match self.fetch_obj_prop_dim_fast(*place, *dim, *dst, *to, *mode) {
                            Some(true) => ip += 1,
                            Some(false) => {}
                            None => break 'slow,
                        },
                        Op::AssignObjProp { place, val, dst } => {
                            if !self.assign_obj_prop_fast(*place, *val, *dst) {
                                break 'slow;
                            }
                        }
                        Op::AssignObjPropDim {
                            place,
                            dim,
                            val,
                            dst,
                        } => {
                            if !self.assign_obj_prop_dim_fast(*place, *dim, *val, *dst) {
                                break 'slow;
                            }
                        }
                        Op::AssignOpObjProp {
                            place,
                            op: kind,
                            val,
                            dst,
                        } => {
                            if !self.assign_op_obj_prop_fast(*place, *kind, *val, *dst) {
                                break 'slow;
                            }
                        }
                        Op::AssignOpObjPropDim {
                            place,
                            dim,
                            op: kind,
                            val,
                            dst,
                        } => {
                            if !self.assign_op_obj_prop_dim_fast(*place, *dim, *kind, *val, *dst) {
                                break 'slow;
                            }
                        }
                        Op::IncDecObjProp { place, kind, dst } => {
                            match self.incdec_obj_prop_fast(*place, *kind) {
                                Some(r) => self.set_dst(*dst, r),
                                None => break 'slow,
                            }
                        }
                        Op::IncDecObjPropDim {
                            place,
                            dim,
                            kind,
                            dst,
                        } => match self.incdec_obj_prop_dim_fast(*place, *dim, *kind) {
                            Some(r) => self.set_dst(*dst, r),
                            None => break 'slow,
                        },
                        // `$x = isset($o->p)`: the `AssignCv` of the result
                        // is done here when the variable holds a plain value
                        // (as `FetchObjProp`'s `to`), and skipped.
                        Op::IssetObjProp { place, dst, empty } => {
                            match self.isset_obj_prop_fast(*place, *empty) {
                                Some(r) => {
                                    let folded = match ops.get(ip) {
                                        Some(Op::AssignCv {
                                            cv,
                                            val: Operand::Tmp(t),
                                            dst: None,
                                        }) if *t == *dst => {
                                            self.cv_store_plain(*cv, Value::Bool(r)).is_ok()
                                        }
                                        _ => false,
                                    };
                                    if folded {
                                        ip += 1;
                                    } else {
                                        self.set_tmp(*dst, Value::Bool(r));
                                    }
                                }
                                None => break 'slow,
                            }
                        }
                        Op::IssetObjPropDim {
                            place,
                            dim,
                            dst,
                            empty,
                        } => match self.isset_obj_prop_dim_fast(*place, *dim, *empty) {
                            Some(r) => self.set_tmp(*dst, Value::Bool(r)),
                            None => break 'slow,
                        },
                        // A constant fetch with a namespace fallback (its
                        // cached global needs the constant table's size; the
                        // others are hot ops, `exec/hot.rs`).
                        Op::FetchConst {
                            name,
                            fallback,
                            dst,
                        } => match self.const_hit(*name, *fallback) {
                            Some(v) => self.set_tmp(*dst, v),
                            None => break 'slow,
                        },
                        // With the `AssignCv` after it folded in
                        // (`Some(true)` from `store_fetched`: skip it).
                        Op::FetchConstTo {
                            name,
                            fallback,
                            dst,
                            to,
                        } => match self.const_hit(*name, *fallback) {
                            Some(v) => {
                                if self.store_fetched(*dst, Some(*to), v) {
                                    ip += 1;
                                }
                            }
                            None => break 'slow,
                        },
                        // `ZEND_FRAMELESS_ICALL_n` in place
                        // (`Vm::frameless_icall_op`), past the `Nop`s of the
                        // call ops it replaced. Declined, the generic arm
                        // below runs those ops.
                        Op::FramelessICall(fc) => match self.frameless_icall_op(fc, ip) {
                            Some(r) => {
                                r?;
                                ip = at + usize::from(fc.argc) + 2;
                            }
                            None => break 'slow,
                        },
                        // A call site resolved before (`Vm::init_fcall_fast`).
                        Op::InitFCall { name, fallback, .. } => {
                            if !self.init_fcall_fast(*name, *fallback) {
                                break 'slow;
                            }
                        }
                        // A by-value argument into the call's inline buffer
                        // (`Vm::send_fast`).
                        Op::Send { src } => {
                            if !self.send_fast(src) {
                                break 'slow;
                            }
                        }
                        // `$this` of a method frame: its object, copied
                        // (the generic arm's fast case; no `$this` throws
                        // there).
                        Op::FetchThis { dst } => {
                            let Some(this) = self.frames.last().and_then(|f| f.this.clone()) else {
                                break 'slow;
                            };
                            self.set_tmp(*dst, Value::Object(this));
                        }
                        // `$closure()` (`Vm::init_closure_call_fast`).
                        Op::InitDynCall {
                            callee: Operand::Cv(cv),
                        } => {
                            if !self.init_closure_call_fast(*cv) {
                                break 'slow;
                            }
                        }
                        // `$obj->name()` on a method cache hit
                        // (`Vm::init_method_call_fast`).
                        Op::InitMethodCall {
                            obj,
                            name: Operand::Lit(lit),
                        } => {
                            if !self.init_method_call_fast(*obj, *lit, at) {
                                break 'slow;
                            }
                        }
                        // `ZEND_DO_UCALL` of a plain user function
                        // (`Vm::push_simple_ucall`, which stores `ip` as the
                        // caller's resume point): the callee frame is
                        // pushed and the loop goes on in it, as after
                        // `Flow::Reload`. Declined, the generic `DoCall`
                        // below runs (`Vm::op_do_call`).
                        Op::DoCall { dst } => {
                            if !self.push_simple_ucall(*dst, ip) {
                                break 'slow;
                            }
                            if Self::safe_point_due() {
                                self.safe_point_after_leave()?;
                            }
                            continue 'frames;
                        }
                        // `ZEND_RETURN` on its common path
                        // (`Vm::return_simple`): the frame is gone and the
                        // loop goes on in the caller, as after
                        // `Flow::Reload`. Declined, the generic `Return`
                        // below runs (`Vm::op_return`).
                        Op::Return { val } => {
                            if self.return_simple(*val, base, ip).is_none() {
                                break 'slow;
                            }
                            // `op` may be freed with the frame's op array.
                            if Self::safe_point_due() {
                                self.safe_point_after_leave()?;
                            }
                            continue 'frames;
                        }
                        _ => break 'slow,
                    }
                    if Self::safe_point_due() {
                        ip = self.safe_point(ip, op)?;
                    }
                    continue 'ops;
                }
                // `SAVE_OPLINE()`: the handler may report the current line,
                // throw, call out, or push a frame.
                self.frame_mut().ip = ip;
                let flow = match op {
                    Op::Nop => Flow::Next,
                    Op::Move { src, dst } => {
                        let v = self.read(*src)?;
                        self.set_tmp(*dst, v);
                        Flow::Next
                    }
                    Op::Bin { op, a, b, dst } => {
                        // Sealed numeric fast path (php-src's specialized
                        // long/double handlers): operands are CVs or
                        // literals currently holding plain numbers.
                        // Anything else - references, undefined variables,
                        // strings, overflow, zero divisors - takes the
                        // out-of-line generic path with its exact
                        // semantics.
                        if let Some(r) = self.bin_num_fast(*op, *a, *b) {
                            self.set_tmp(*dst, r);
                        } else if let Some(r) = self.equality_fast(*op, *a, *b) {
                            self.set_tmp(*dst, Value::Bool(r));
                        } else {
                            self.bin_slow(*op, *a, *b, *dst)?;
                        }
                        Flow::Next
                    }
                    Op::Unary { op, a, dst } => {
                        let x = self.read(*a)?;
                        let r =
                            match op {
                                UnKind::Not => Value::Bool(!self.to_bool(&x)?),
                                UnKind::Bool => Value::Bool(self.to_bool(&x)?),
                                UnKind::BitNot => {
                                    ops::bit_not(&x, self).map_err(|e| self.rt_throw(e))?
                                }
                                UnKind::Neg => ops::mul(&x, &Value::Int(-1), self)
                                    .map_err(|e| self.rt_throw(e))?,
                                UnKind::Plus => ops::mul(&x, &Value::Int(1), self)
                                    .map_err(|e| self.rt_throw(e))?,
                            };
                        self.set_tmp(*dst, r);
                        Flow::Next
                    }
                    Op::Cast { kind, a, dst } => {
                        let x = if matches!(*kind, CastKind::Array | CastKind::Object) {
                            self.read_raw(*a)?
                        } else {
                            self.read(*a)?
                        };
                        let r = self.cast(*kind, *a, x)?;
                        self.set_tmp(*dst, r);
                        Flow::Next
                    }
                    Op::Rope { parts, dst } => {
                        let s = self.op_rope(parts)?;
                        self.set_tmp(*dst, Value::Str(s));
                        Flow::Next
                    }
                    Op::InitArray { dst } => {
                        self.set_tmp(*dst, Value::Array(ZArray::new()));
                        Flow::Next
                    }
                    Op::ArrayAdd { arr, key, val } => {
                        let v = self.read(*val)?;
                        let k = match key {
                            Some(k) => Some(self.read(*k)?),
                            None => None,
                        };
                        self.array_literal_add(*arr, k, v)?;
                        Flow::Next
                    }
                    Op::ArrayAddRef { arr, key, place } => {
                        let k = match key {
                            Some(k) => Some(self.read(*k)?),
                            None => None,
                        };
                        let r = self.place_make_ref(*place)?;
                        self.array_literal_add(*arr, k, Value::Reference(r))?;
                        Flow::Next
                    }
                    Op::ArrayUnpack { arr, src } => {
                        let v = self.read(*src)?;
                        self.array_unpack(*arr, v)?;
                        Flow::Next
                    }
                    Op::ConstArrayUnpack { arr, src } => {
                        let v = self.read(*src)?;
                        self.const_array_unpack(*arr, v)?;
                        Flow::Next
                    }
                    Op::Jmp(t) => {
                        ip = *t as usize;
                        Flow::Next
                    }
                    Op::JmpZ(a, t) => {
                        if !self.jump_cond(*a)? {
                            ip = *t as usize;
                        }
                        Flow::Next
                    }
                    Op::JmpNz(a, t) => {
                        if self.jump_cond(*a)? {
                            ip = *t as usize;
                        }
                        Flow::Next
                    }
                    Op::JmpZEx { a, dst, target } => {
                        let b = self.jump_cond(*a)?;
                        if !b {
                            self.set_tmp(*dst, Value::Bool(false));
                            ip = *target as usize;
                        }
                        Flow::Next
                    }
                    Op::JmpNzEx { a, dst, target } => {
                        let b = self.jump_cond(*a)?;
                        if b {
                            self.set_tmp(*dst, Value::Bool(true));
                            ip = *target as usize;
                        }
                        Flow::Next
                    }
                    Op::JmpSet { a, dst, target } => {
                        let v = self.read(*a)?;
                        if self.to_bool(&v)? {
                            self.set_tmp(*dst, v);
                            ip = *target as usize;
                        }
                        Flow::Next
                    }
                    Op::Coalesce { a, dst, target } => {
                        let v = match a {
                            Operand::Cv(cv) => self.read_cv_defined(*cv).unwrap_or_default(),
                            o => self.read(*o)?,
                        };
                        if !v.is_null() {
                            self.set_tmp(*dst, v);
                            ip = *target as usize;
                        }
                        Flow::Next
                    }
                    Op::JmpNull { a, dst, target } => {
                        let v = match a {
                            Operand::Cv(cv) => match self.read_cv_defined(*cv) {
                                Some(v) => v,
                                None => self.read(*a)?,
                            },
                            o => self.read(*o)?,
                        };
                        if v.is_null() {
                            if let Operand::TmpKeep(t) = a {
                                self.set_tmp(*t, Value::Null);
                            }
                            self.set_tmp(*dst, Value::Null);
                            ip = *target as usize;
                        }
                        Flow::Next
                    }
                    // The pre-match's `assign_cv_fast` did not apply.
                    Op::AssignCv { cv, val, dst } => {
                        self.assign_cv_generic(*cv, *val, *dst)?;
                        Flow::Next
                    }
                    // Also the generic form of `AssignStaticProp`.
                    Op::Assign { place, val, dst } | Op::AssignStaticProp { place, val, dst } => {
                        let r = match *val {
                            Operand::Cv(cv) => match self.read_cv_op_data(cv) {
                                CvOpData::Value(v) => self.place_assign(*place, v)?,
                                CvOpData::Deferred => self.place_assign_cv_ref(*place, cv)?,
                                CvOpData::Undefined => self.place_assign_undef_cv(*place, cv)?,
                            },
                            other => {
                                let v = self.read(other)?;
                                let v = self.copy_on_assign(v);
                                self.place_assign(*place, v)?
                            }
                        };
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    // Also the generic form of `AssignOpStaticProp`.
                    Op::AssignOp {
                        place,
                        op,
                        val,
                        dst,
                    }
                    | Op::AssignOpStaticProp {
                        place,
                        op,
                        val,
                        dst,
                    } => {
                        let concat = *op == BinaryOp::Concat;
                        if let Some(cv) = self.assign_op_undef_dim_cv(*place, *val) {
                            let r = self.place_assign_op_undef_cv(*place, *op, cv)?;
                            self.set_dst(*dst, r);
                        } else if !(concat
                            && (self.concat_assign_cv_lit(*place, *val, *dst)
                                || (dst.is_none() && self.concat_dims_cv_fast(*place, *val))))
                        {
                            let v = self.read(*val)?;
                            if !(concat && self.concat_assign_cv(*place, &v, *dst)) {
                                let r = self.place_assign_op(*place, *op, v)?;
                                self.set_dst(*dst, r);
                            }
                        }
                        Flow::Next
                    }
                    // The generic `IncDec` of the specialised
                    // `IncDecCvCmpJmp` (its `CmpJmp` then runs as its own
                    // op).
                    Op::IncDecCvCmpJmp { kind, place, .. } => {
                        let r = self.place_incdec(*place, *kind)?;
                        self.set_dst(None, r);
                        Flow::Next
                    }
                    Op::AssignOpCvCmpJmp { place, op, val, .. }
                    | Op::AssignOpCvJmp { place, op, val, .. } => {
                        let v = self.read(*val)?;
                        let r = self.place_assign_op(*place, *op, v)?;
                        self.set_dst(None, r);
                        Flow::Next
                    }
                    // Also the generic form of the specialised `IncDecCv` /
                    // `IncDecDimCv` whose fast path did not apply.
                    Op::IncDec { place, kind, dst }
                    | Op::IncDecCv {
                        kind, dst, place, ..
                    }
                    | Op::IncDecDimCv {
                        kind, dst, place, ..
                    } => {
                        let r = self.place_incdec(*place, *kind)?;
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::AssignRef { place, src, dst } => {
                        let v = self.read_raw(*src)?;
                        let r = self.place_assign_ref(*place, v)?;
                        if dst.is_some() {
                            self.set_dst(*dst, r);
                        } else {
                            // An unused result is a duplicate handle of the
                            // reference just bound: not a GC possible root.
                            crate::gc::release_duplicate(r);
                        }
                        Flow::Next
                    }
                    Op::MakeRef { place, dst } => {
                        let r = self.place_make_ref(*place)?;
                        self.set_tmp(*dst, Value::Reference(r));
                        Flow::Next
                    }
                    Op::ListFetchRef { place, dst } => {
                        self.list_fetch_ref(*place, *dst)?;
                        Flow::Next
                    }
                    // Also the generic form of the specialised `FetchDimCv` /
                    // `FetchDimsCv` whose fast path did not apply (the
                    // `AssignCv` folded into them then runs as its own op).
                    Op::Fetch { place, mode, dst }
                    | Op::FetchDimCv {
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
                    } => {
                        let v = if *mode == FetchMode::Read {
                            if let Some(v) = self.fetch_place_fast(*place) {
                                v
                            } else {
                                self.place_fetch(*place, *mode)?
                            }
                        } else {
                            self.place_fetch(*place, *mode)?
                        };
                        self.set_tmp(*dst, v);
                        Flow::Next
                    }
                    Op::Isset { place, dst } => {
                        let v = self.place_isset(*place, false)?;
                        self.set_tmp(*dst, Value::Bool(v));
                        Flow::Next
                    }
                    Op::Empty { place, dst } => {
                        let v = self.place_isset(*place, true)?;
                        self.set_tmp(*dst, Value::Bool(v));
                        Flow::Next
                    }
                    Op::ArrayKeyExists { array, dst } => {
                        if self.op_array_key_exists(array, *dst)? {
                            Flow::Reload
                        } else {
                            Flow::Next
                        }
                    }
                    Op::Unset { place } => {
                        self.place_unset(*place)?;
                        Flow::Next
                    }
                    Op::Global { cv, name } => {
                        let n = self.lit_str(*name);
                        let r = self.global_ref(n.as_bytes());
                        self.cv_bind(*cv, Value::Reference(r));
                        Flow::Next
                    }
                    Op::BindStatic { cv, idx, skip } => {
                        if let Some(r) = self.static_slot(*idx) {
                            self.cv_bind(*cv, Value::Reference(r));
                            ip = *skip as usize;
                        }
                        Flow::Next
                    }
                    Op::InitStatic { cv, idx, val } => {
                        let v = self.read(*val)?;
                        let r = self.static_init(*idx, v);
                        self.cv_bind(*cv, Value::Reference(r));
                        Flow::Next
                    }
                    Op::FetchThis { dst } => {
                        let t = self.frame().this.clone();
                        match t {
                            Some(o) => self.set_tmp(*dst, Value::Object(o)),
                            None => {
                                return Err(self
                                    .make_throw("Error", "Using $this when not in object context"))
                            }
                        }
                        Flow::Next
                    }
                    Op::InitFCall {
                        name,
                        fallback,
                        display,
                    } => {
                        self.op_init_fcall(*name, *fallback, *display)?;
                        Flow::Next
                    }
                    Op::InitDynCall { callee } => {
                        let c = self.read(*callee)?;
                        self.op_init_dyn_call(c)?;
                        Flow::Next
                    }
                    Op::InitMethodCall { obj, name } => {
                        let o = self.method_object(*obj)?;
                        self.op_init_method_call_op(o, *name)?;
                        Flow::Next
                    }
                    Op::InitStaticCall { class, name } => {
                        self.op_init_static_call(class, *name)?;
                        Flow::Next
                    }
                    Op::InitParentHook { prop, set } => {
                        let p = self.lit_str(*prop);
                        self.op_init_parent_hook(p, *set)?;
                        Flow::Next
                    }
                    Op::New { class, dst, skip } => {
                        let has_ctor = self.op_new(class, *dst)?;
                        if !has_ctor && *skip != 0 {
                            ip = *skip as usize;
                        }
                        Flow::Next
                    }
                    Op::Send { src } => {
                        self.op_send(src, None)?;
                        Flow::Next
                    }
                    Op::SendNamed { name, src } => {
                        let n = self.lit_str(*name);
                        self.op_send(src, Some(n))?;
                        Flow::Next
                    }
                    // The call ops an `Op::FramelessICall` replaced, one
                    // after the other (the `Nop`s after it follow).
                    Op::FramelessICall(fc) => {
                        self.op_init_fcall(fc.name, None, fc.display)?;
                        for src in &fc.args[..usize::from(fc.argc).min(3)] {
                            self.op_send(src, None)?;
                        }
                        if self.op_do_call(fc.dst)? {
                            Flow::Reload
                        } else {
                            Flow::Next
                        }
                    }
                    Op::SendUnpack { src } => {
                        self.op_send_unpack(*src)?;
                        Flow::Next
                    }
                    Op::DoCall { dst } => {
                        if self.op_do_call(*dst)? {
                            Flow::Reload
                        } else {
                            Flow::Next
                        }
                    }
                    Op::SendPlaceholder { name } => {
                        let n = name.map(|n| self.lit_str(n));
                        self.op_send_placeholder(n)?;
                        Flow::Next
                    }
                    Op::CallablePartial {
                        dst,
                        variadic,
                        const_args,
                        name,
                    } => {
                        let call = self.frame_mut().calls.pop().expect("pending call");
                        let n = self.lit_str(*name);
                        let c = self.make_partial(call, *variadic, *const_args, n)?;
                        self.set_tmp(*dst, c);
                        Flow::Next
                    }
                    Op::CallableConvert { dst, const_expr } => {
                        let call = self.frame_mut().calls.pop().expect("pending call");
                        if *const_expr && call.magic_name.is_some() {
                            return Err(self.make_throw(
                                "Error",
                                "Creating a callable for the magic __callStatic() method is not supported in constant expressions",
                            ));
                        }
                        let c = self.make_closure_from_call(call)?;
                        self.set_tmp(*dst, c);
                        Flow::Next
                    }
                    Op::Recv { n, cv } => {
                        self.op_recv(*n, *cv)?;
                        Flow::Next
                    }
                    Op::RecvInit { n, cv, skip } => {
                        if self.op_recv_init(*n, *cv)? {
                            ip = *skip as usize;
                        }
                        Flow::Next
                    }
                    Op::RecvDefault { n, cv, val } => {
                        let v = self.read(*val)?;
                        self.op_recv_default(*n, *cv, v)?;
                        Flow::Next
                    }
                    Op::RecvVariadic { n, cv } => {
                        self.op_recv_variadic(*n, *cv)?;
                        Flow::Next
                    }
                    Op::VerifyReturn { val } => {
                        if let Operand::Tmp(t) = *val {
                            let v = self.read(*val)?;
                            let v = self.verify_return(Some(v))?;
                            self.set_tmp(t, v);
                        }
                        Flow::Next
                    }
                    // `Vm::return_simple` declined above.
                    Op::Return { val } => {
                        let v = match val {
                            Operand::Unused => None,
                            Operand::Cv(cv) => match self.return_undefined_typed_cv(*cv)? {
                                Some(v) => Some(v),
                                None => Some(self.read(*val)?),
                            },
                            o => Some(self.read(*o)?),
                        };
                        self.op_return(v, base)?
                    }
                    Op::ReturnByRefValue { val } => {
                        let unused = *val == Operand::Unused;
                        let raw = if unused {
                            Value::Null
                        } else {
                            self.read_raw(*val)?
                        };
                        let v = match raw {
                            Value::Reference(r) => self.verify_return(Some(Value::Reference(r)))?,
                            other => {
                                let checked = self.verify_return((!unused).then_some(other))?;
                                self.notice(
                                    "Only variable references should be returned by reference",
                                )?;
                                Value::Reference(php_runtime::PhpRef::new(checked))
                            }
                        };
                        self.op_return_raw(Some(v), base)?
                    }
                    Op::ReturnRef { place } => {
                        let by_ref = self
                            .frame()
                            .ufunc
                            .as_ref()
                            .is_some_and(|u| u.flags & crate::func::flags::RETURN_REF != 0);
                        let v = if by_ref {
                            Value::Reference(self.place_make_ref(*place)?)
                        } else {
                            self.place_fetch(*place, crate::opcode::FetchMode::Read)?
                        };
                        self.op_return(Some(v), base)?
                    }
                    Op::GeneratorCreate => self.op_generator_create(base)?,
                    Op::Yield { key, val, dst } => {
                        let k = match key {
                            Some(k) => Some(self.read(*k)?),
                            None => None,
                        };
                        let v = match val {
                            Some(v) => Some(self.read_raw(*v)?),
                            None => None,
                        };
                        self.op_yield(k, v, *dst, base)?
                    }
                    Op::YieldRef { key, place, dst } => {
                        let k = match key {
                            Some(k) => Some(self.read(*k)?),
                            None => None,
                        };
                        let r = self.place_make_ref(*place)?;
                        self.op_yield(k, Some(Value::Reference(r)), *dst, base)?
                    }
                    Op::YieldByRefValue {
                        key,
                        val,
                        call,
                        dst,
                    } => {
                        let k = match key {
                            Some(k) => Some(self.read(*k)?),
                            None => None,
                        };
                        let v = self.read_raw(*val)?;
                        self.op_yield_by_ref_value(k, v, *call, *dst, base)?
                    }
                    Op::YieldFrom { src, dst } => {
                        let v = self.read_raw(*src)?;
                        self.op_yield_from(v, *dst, base)?
                    }
                    Op::DeclareFunction(idx) => {
                        let f = code.decls.functions.borrow()[*idx as usize].clone();
                        self.declare_function(&f)?;
                        Flow::Next
                    }
                    Op::DeclareClass(idx) => {
                        let d = code.decls.classes.borrow()[*idx as usize].clone();
                        self.declare_class_stmt(&d)?;
                        Flow::Next
                    }
                    Op::DeclareAnonClass { idx, dst } => {
                        let d = code.decls.classes.borrow()[*idx as usize].clone();
                        let c = self.declare_class(&d, true)?;
                        self.set_tmp(*dst, Value::Str(c.name.clone()));
                        Flow::Next
                    }
                    Op::MakeClosure { idx, dst } => {
                        let f = code.decls.functions.borrow()[*idx as usize].clone();
                        let c = self.make_closure(&f)?;
                        self.set_tmp(*dst, c);
                        Flow::Next
                    }
                    Op::DeclareConst { name, val, attrs } => {
                        let n = self.lit_str(*name);
                        let v = self.read(*val)?;
                        if self.declare_constant(n.clone(), v)? && *attrs != u32::MAX {
                            if let Some(a) =
                                code.decls.const_attributes.borrow().get(*attrs as usize)
                            {
                                self.const_attributes.insert(n, a.clone());
                            }
                        }
                        Flow::Next
                    }
                    Op::FetchConst {
                        name,
                        fallback,
                        dst,
                    } => {
                        let v = self.fetch_constant_cached(*name, *fallback)?;
                        self.set_tmp(*dst, v);
                        Flow::Next
                    }
                    Op::FetchClassConst { class, name, dst } => {
                        let v = self.fetch_class_const_cached(class, *name)?;
                        self.set_tmp(*dst, v);
                        Flow::Next
                    }
                    // A miss of the specialised forms: the generic fetch,
                    // then the store of the folded `AssignCv` (which runs as
                    // its own op when the variable holds a refcounted value).
                    Op::FetchConstTo {
                        name,
                        fallback,
                        dst,
                        to,
                    } => {
                        let v = self.fetch_constant_cached(*name, *fallback)?;
                        if self.store_fetched(*dst, Some(*to), v) {
                            ip += 1;
                        }
                        Flow::Next
                    }
                    Op::FetchClassConstTo {
                        class,
                        name,
                        dst,
                        to,
                    } => {
                        let v = self.fetch_class_const_cached(class, Operand::Lit(*name))?;
                        if self.store_fetched(*dst, Some(*to), v) {
                            ip += 1;
                        }
                        Flow::Next
                    }
                    Op::FetchClassName {
                        class,
                        dst,
                        const_expr,
                    } => {
                        let v = self.op_fetch_class_name(class, *const_expr)?;
                        self.set_tmp(*dst, v);
                        Flow::Next
                    }
                    Op::InstanceOf { a, class, dst } => {
                        let v = self.read(*a)?;
                        let r = self.op_instanceof(&v, class)?;
                        self.set_tmp(*dst, Value::Bool(r));
                        Flow::Next
                    }
                    Op::Clone { a, dst } => {
                        let v = self.read(*a)?;
                        let r = self.clone_value(v)?;
                        self.set_tmp(*dst, r);
                        Flow::Next
                    }
                    Op::FuncNumArgs { dst } => {
                        let n = self.frame().num_args();
                        self.set_dst(*dst, Value::Int(i64::try_from(n).unwrap_or(i64::MAX)));
                        Flow::Next
                    }
                    Op::FuncGetArgs { dst } => {
                        let idx = self.frames.len() - 1;
                        let args = self.frame_args(idx);
                        self.set_dst(*dst, Value::Array(args));
                        Flow::Next
                    }
                    Op::Strlen { a, dst } => {
                        let len = match a {
                            Operand::Cv(cv) => self.frame().peek_cv(*cv, |s| match s {
                                Some(Value::Str(z)) => Some(z.len()),
                                _ => None,
                            }),
                            Operand::Lit(i) => match self.frame().code.literals.get(*i as usize) {
                                Some(Value::Str(s)) => Some(s.len()),
                                _ => None,
                            },
                            _ => None,
                        };
                        let r = match len {
                            Some(n) => Value::Int(i64::try_from(n).unwrap_or(i64::MAX)),
                            None => {
                                let v = self.read(*a)?;
                                self.op_strlen_slow(v)?
                            }
                        };
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::Count { a, dst, sizeof } => {
                        let len = match a {
                            Operand::Cv(cv) => self.frame().peek_cv(*cv, |s| match s {
                                Some(Value::Array(arr)) => Some(arr.len()),
                                _ => None,
                            }),
                            Operand::Lit(i) => match self.frame().code.literals.get(*i as usize) {
                                Some(Value::Array(arr)) => Some(arr.len()),
                                _ => None,
                            },
                            _ => None,
                        };
                        let r = match len {
                            Some(n) => Value::Int(i64::try_from(n).unwrap_or(i64::MAX)),
                            None => {
                                let v = self.read(*a)?;
                                self.op_count_slow(v, *sizeof)?
                            }
                        };
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::TypeCheck { a, mask, dst } => {
                        let r = match a {
                            Operand::Cv(cv) => {
                                let f = self.frame();
                                if !f.symtab_attached {
                                    f.cvs
                                        .get(*cv as usize)
                                        .and_then(CvSlot::get)
                                        .map(|v| crate::opcode::type_mask::matches(*mask, v))
                                } else {
                                    None
                                }
                            }
                            Operand::Lit(i) => self
                                .frame()
                                .code
                                .literals
                                .get(*i as usize)
                                .map(|v| crate::opcode::type_mask::matches(*mask, v)),
                            _ => None,
                        };
                        let r = match r {
                            Some(matched) => matched,
                            None => {
                                let v = self.read(*a)?;
                                crate::opcode::type_mask::matches(*mask, &v)
                            }
                        };
                        self.set_dst(*dst, Value::Bool(r));
                        Flow::Next
                    }
                    Op::GetType { a, dst } => {
                        let v = self.read(*a)?;
                        let t = v.legacy_type_name();
                        self.set_dst(*dst, Value::Str(ZStr::from(t)));
                        Flow::Next
                    }
                    Op::GetClass { a, dst } => {
                        let r = self.op_get_class(*a)?;
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::GetCalledClass { dst } => {
                        let r = self.op_get_called_class()?;
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::Defined { name, dst } => {
                        let r = self.op_defined(*name)?;
                        self.set_dst(*dst, Value::Bool(r));
                        Flow::Next
                    }
                    Op::InArray {
                        a,
                        set,
                        strict,
                        dst,
                    } => {
                        let needle = self.read(*a)?;
                        let r = self.op_in_array(&needle, *set, *strict)?;
                        self.set_dst(*dst, Value::Bool(r));
                        Flow::Next
                    }
                    Op::KeyExists { key, array, dst } => {
                        let r = self.op_key_exists(*key, *array)?;
                        self.set_dst(*dst, Value::Bool(r));
                        Flow::Next
                    }
                    Op::Echo(a) => {
                        let fast = match a {
                            Operand::Cv(cv) => self.frame().peek_cv(*cv, |s| match s {
                                Some(Value::Int(i)) => Some(Ok(*i)),
                                Some(Value::Str(s)) => Some(Err(s.clone())),
                                _ => None,
                            }),
                            Operand::Lit(i) => match self.frame().code.literals.get(*i as usize) {
                                Some(Value::Int(i)) => Some(Ok(*i)),
                                Some(Value::Str(s)) => Some(Err(s.clone())),
                                _ => None,
                            },
                            _ => None,
                        };
                        match fast {
                            Some(Ok(i)) => {
                                let mut buf = [0u8; php_runtime::string::LONG_MAX_LEN];
                                self.echo(php_runtime::string::format_long(i, &mut buf));
                            }
                            Some(Err(s)) => {
                                self.echo(s.as_bytes());
                            }
                            None => {
                                let v = self.read(*a)?;
                                match &v {
                                    Value::Str(s) => self.echo(s.as_bytes()),
                                    // `zend_long_to_str` digits from a stack buffer:
                                    // no string is materialised for an echoed int.
                                    Value::Int(i) => {
                                        let mut buf = [0u8; php_runtime::string::LONG_MAX_LEN];
                                        self.echo(php_runtime::string::format_long(*i, &mut buf));
                                    }
                                    _ => {
                                        let s = self.to_zstr(&v)?;
                                        self.echo(s.as_bytes());
                                    }
                                }
                            }
                        }
                        self.take_pending_throw()?;
                        Flow::Next
                    }
                    Op::Exit(a) => {
                        let v = self.read(*a)?;
                        self.op_exit(v)?;
                        return Err(Throw::Exit);
                    }
                    Op::Free(a) => {
                        let v = self.read_raw(*a)?;
                        drop(v);
                        Flow::Next
                    }
                    Op::Throw(a) => {
                        let v = self.read(*a)?;
                        return Err(self.op_throw(v));
                    }
                    Op::Catch { classes, cv, next } => {
                        let exc = self.frame().pending_exc.clone();
                        let Some(exc) = exc else {
                            ip = *next as usize;
                            continue;
                        };
                        let cls = object::class_of(&exc).clone();
                        let mut matched = false;
                        for c in classes.iter() {
                            if self.catch_class_matches(&cls, *c) {
                                matched = true;
                                break;
                            }
                        }
                        if matched {
                            self.frame_mut().pending_exc = None;
                            if let Some(cv) = cv {
                                if let Some((r, v)) = self.cv_assign(*cv, Value::Object(exc)) {
                                    self.assign_ref_strict(&r, v)?;
                                }
                            }
                        } else {
                            ip = *next as usize;
                        }
                        Flow::Next
                    }
                    Op::Rethrow => {
                        let exc = self.frame_mut().pending_exc.take();
                        match exc {
                            Some(e) => return Err(Throw::Exception(e)),
                            None => Flow::Next,
                        }
                    }
                    Op::FastCall { fin, target } => {
                        self.frame_mut().fins[*fin as usize] = FinState::Return(ip);
                        ip = *target as usize;
                        Flow::Next
                    }
                    Op::FastRet { fin } => {
                        let st = std::mem::take(&mut self.frame_mut().fins[*fin as usize]);
                        match st {
                            FinState::Return(ret) => {
                                ip = ret;
                                Flow::Next
                            }
                            FinState::Exception(e) => return Err(Throw::Exception(e)),
                            FinState::None => Flow::Next,
                        }
                    }
                    Op::DiscardFinally { fin } => {
                        self.frame_mut().fins[*fin as usize] = FinState::None;
                        Flow::Next
                    }
                    Op::BeginSilence { dst } => {
                        let old = self.error_reporting;
                        self.frame_mut().silence_saved.push(old);
                        self.set_tmp(*dst, Value::Int(old));
                        // ZEND_BEGIN_SILENCE: fatal errors are never silenced,
                        // and an already-silenced mask is left alone.
                        if !has_only_fatal_errors(old) {
                            self.error_reporting = old & E_FATAL_ERRORS;
                        }
                        Flow::Next
                    }
                    Op::EndSilence { saved } => {
                        let v = self.read(*saved)?;
                        self.frame_mut().silence_saved.pop();
                        if let Value::Int(old) = v {
                            self.error_reporting = silence_restore(self.error_reporting, old);
                        }
                        Flow::Next
                    }
                    Op::Include { kind, a, dst } => {
                        let v = self.read(*a)?;
                        if self.op_include(*kind, v, *dst)? {
                            Flow::Reload
                        } else {
                            Flow::Next
                        }
                    }
                    Op::FeReset {
                        subject,
                        place,
                        kind,
                        iter,
                        end,
                    } => {
                        let has = self.op_fe_reset(*subject, *place, *kind, *iter)?;
                        if !has {
                            ip = *end as usize;
                        }
                        Flow::Next
                    }
                    Op::FeFetch {
                        iter,
                        end,
                        val,
                        key,
                    } => {
                        let has = self.op_fe_fetch(*iter, *val, *key)?;
                        if !has {
                            ip = *end as usize;
                        }
                        Flow::Next
                    }
                    Op::FeFree { iter } => {
                        self.op_fe_free_and_destruct(*iter)?;
                        Flow::Next
                    }
                    Op::FeFreeOnReturn { iter, loop_at } => {
                        if let Err(e) = self.op_fe_free_and_destruct(*iter) {
                            if *loop_at != u32::MAX {
                                self.frame_mut().ip = *loop_at as usize + 1;
                            }
                            return Err(e);
                        }
                        if object::has_pending_destructors() {
                            if let Err(e) = self.run_pending_destructors() {
                                if *loop_at != u32::MAX {
                                    self.frame_mut().ip = *loop_at as usize + 1;
                                }
                                return Err(e);
                            }
                        }
                        Flow::Next
                    }
                    Op::Match { subject, table } => {
                        // `ZEND_MATCH`: an undefined CV warns once
                        // (`ZVAL_UNDEFINED_OP1`), then takes the default.
                        let v = self.read(*subject)?;
                        ip = table.target(&v) as usize;
                        Flow::Next
                    }
                    Op::MatchError { subject } => {
                        // `GET_OP1_ZVAL_PTR_UNDEF`: an undefined CV is
                        // reported as null without a warning.
                        let v = match *subject {
                            Operand::Cv(cv) => self.read_cv_defined(cv).unwrap_or(Value::Null),
                            o => self.read(o)?,
                        };
                        return Err(self.match_error(&v));
                    }
                    Op::TypeAssertArray { val, msg } => {
                        self.op_type_assert_array(*val, *msg)?;
                        Flow::Next
                    }
                    Op::Ticks(n) => {
                        self.op_ticks(*n)?;
                        Flow::Next
                    }
                    Op::Diag { level, msg } => {
                        let m = self.lit_str(*msg).to_string_lossy();
                        self.error(i64::from(*level), &m)?;
                        Flow::Next
                    }
                    Op::ConstDimCheck { obj } => {
                        if matches!(self.read(*obj)?, Value::Object(_)) {
                            return Err(self.const_dim_on_object());
                        }
                        Flow::Next
                    }
                    Op::UserOpcode { opcode } => {
                        self.run_user_opcode_handler(*opcode);
                        Flow::Next
                    }
                    Op::ConstPropCheck { obj, name, dst } => {
                        let n = self.read(*name)?;
                        let n = self.to_zstr(&n)?;
                        self.set_tmp(*dst, Value::Str(n));
                        if let Value::Object(o) = self.read(*obj)? {
                            if !object::class_of(&o).is_enum() {
                                return Err(self.non_enum_const_prop());
                            }
                        }
                        Flow::Next
                    }
                    // The specialised ops whose fast path did not apply
                    // (the hot pre-match changed nothing): the generic op
                    // each one replaced (`IncDec*` and `FetchDim*` share the
                    // `IncDec` and `Fetch` arms above). The op after a
                    // `CmpJmp` or `BinAssignCv` then runs as its own op.
                    Op::CmpJmp { op, a, b, dst, .. }
                    | Op::BinAssignCv { op, a, b, dst, .. }
                    | Op::BinAssignCvCmpJmp { op, a, b, dst, .. }
                    | Op::BinAssignCvJmp { op, a, b, dst, .. } => {
                        self.bin_slow(*op, *a, *b, *dst)?;
                        Flow::Next
                    }
                    Op::Abs { src, dst, to } => {
                        let v = self.op_abs_slow(*src)?;
                        if let Some(cv) = *to {
                            if let Some(d) = *dst {
                                if !self.store_fetched(d, Some(cv), v.clone()) {
                                    self.assign_cv_fast(cv, Operand::Tmp(d), None);
                                }
                            } else if self.cv_store_plain(cv, v.clone()).is_err() {
                                let _ = self.cv_assign(cv, v.clone());
                            }
                        } else if let Some(d) = *dst {
                            self.set_tmp(d, v);
                        }
                        Flow::Next
                    }
                    Op::AssignOpCv {
                        op,
                        val,
                        dst,
                        place,
                        ..
                    } => {
                        let v = self.read(*val)?;
                        let r = self.place_assign_op(*place, *op, v)?;
                        self.set_dst(*dst, r);
                        Flow::Next
                    }
                    Op::AssignOpDimCv { op, val, place, .. }
                    | Op::AssignOpDimsCv { op, val, place, .. } => {
                        if let Some(cv) = self.assign_op_undef_dim_cv(*place, *val) {
                            self.place_assign_op_undef_cv(*place, *op, cv)?;
                        } else {
                            let v = self.read(*val)?;
                            self.place_assign_op(*place, *op, v)?;
                        }
                        Flow::Next
                    }
                    Op::IssetDimCv {
                        place, dst, empty, ..
                    }
                    | Op::IssetStaticProp { place, dst, empty } => {
                        let v = self.place_isset(*place, *empty)?;
                        self.set_tmp(*dst, Value::Bool(v));
                        Flow::Next
                    }
                    Op::AssignDimCv { val, place, .. } | Op::AssignDimsCv { val, place, .. } => {
                        self.assign_place_unused(*place, *val)?;
                        Flow::Next
                    }
                    // The `.=` of `AssignOp` (the literal form is part of
                    // the fast path that did not apply).
                    Op::ConcatCv {
                        val, dst, place, ..
                    } => {
                        let v = self.read(*val)?;
                        if !self.concat_assign_cv(*place, &v, *dst) {
                            let r = self.place_assign_op(*place, BinaryOp::Concat, v)?;
                            self.set_dst(*dst, r);
                        }
                        Flow::Next
                    }
                    Op::AssertCheck { dst, skip } => {
                        let a = self.ini.get_long("zend.assertions");
                        if a != 1 {
                            self.set_tmp(*dst, Value::Bool(true));
                            ip = *skip as usize;
                        }
                        Flow::Next
                    }
                    // Handlers for the dedicated property ops (`exec/spec/props.rs`).
                    Op::FetchObjProp {
                        place,
                        dst,
                        to,
                        mode,
                    } => self.op_fetch_obj_prop(*place, *dst, *to, *mode, &mut ip)?,
                    Op::FetchObjPropDim {
                        place,
                        dim,
                        dst,
                        to,
                        mode,
                    } => self.op_fetch_obj_prop_dim(*place, *dim, *dst, *to, *mode, &mut ip)?,
                    Op::AssignObjProp { place, val, dst } => {
                        self.op_assign_obj_prop(*place, *val, *dst)?
                    }
                    Op::AssignObjPropDim {
                        place,
                        dim,
                        val,
                        dst,
                    } => self.op_assign_obj_prop_dim(*place, *dim, *val, *dst)?,
                    Op::AssignOpObjProp {
                        place,
                        op: kind,
                        val,
                        dst,
                    } => self.op_assign_op_obj_prop(*place, *kind, *val, *dst)?,
                    Op::AssignOpObjPropDim {
                        place,
                        dim,
                        op: kind,
                        val,
                        dst,
                    } => self.op_assign_op_obj_prop_dim(*place, *dim, *kind, *val, *dst)?,
                    Op::IncDecObjProp { place, kind, dst } => {
                        self.op_incdec_obj_prop(*place, *kind, *dst)?
                    }
                    Op::IncDecObjPropDim {
                        place,
                        dim,
                        kind,
                        dst,
                    } => self.op_incdec_obj_prop_dim(*place, *dim, *kind, *dst)?,
                    Op::IssetObjProp { place, dst, empty } => {
                        self.op_isset_obj_prop(*place, *dst, *empty)?
                    }
                    Op::IssetObjPropDim {
                        place,
                        dim,
                        dst,
                        empty,
                    } => self.op_isset_obj_prop_dim(*place, *dim, *dst, *empty)?,
                };
                match flow {
                    Flow::Next => {
                        if Self::safe_point_due() {
                            ip = self.safe_point(ip, op)?;
                        }
                    }
                    // The frame stack changed: the safe point runs on the
                    // new top frame, whose `ip` is its own.
                    // The frame stack changed: `op` may be freed with a
                    // popped frame's op array (see `LOAD_OPLINE` above).
                    Flow::Reload => {
                        if Self::safe_point_due() {
                            self.safe_point_after_leave()?;
                        }
                        continue 'frames;
                    }
                    Flow::Done(v) => {
                        if Self::safe_point_due() {
                            self.safe_point_after_leave()?;
                        }
                        return Ok(v);
                    }
                }
            }
        }
    }

    /// Whether the safe point after an instruction has work to do
    /// (`EG(vm_interrupt)`): one flag says whether destructors, a
    /// collection, a delayed `memory_limit` violation (php-memlimit raises
    /// the flag through its pending hook, see [`crate::interrupt`]) or
    /// interrupt handlers wait. One inline flag test; the work itself is
    /// out of line ([`Vm::safe_point_work`]), where each source is polled
    /// on its own.
    #[inline(always)]
    #[allow(clippy::inline_always)] // the per-instruction test of the dispatch loop
    fn safe_point_due() -> bool {
        crate::interrupt::safe_point_due()
    }

    /// The safe point after an instruction of the current frame that
    /// continues at `ip`: stores `ip` into the frame (the work may report
    /// the current line, run user code or throw from this frame), does the
    /// work, and returns the frame's `ip` to continue at.
    #[cold]
    #[inline(never)]
    fn safe_point(&mut self, ip: usize, op: &Op) -> VmResult<usize> {
        self.frame_mut().ip = ip;
        self.safe_point_work(op)?;
        Ok(self.frame().ip)
    }

    /// The work waiting for the end of instruction `op`, in php-src's
    /// order: destructors of released objects, the collector when the root
    /// buffer is full (`gc_possible_root_when_full`), a delayed
    /// `memory_limit` violation (`zend_mm_delayed_error`), then the
    /// interrupt handlers (`zend_interrupt_helper`). Each source is checked
    /// on its own (the flag that led here may be stale); afterwards the
    /// flag is cleared unless a source is still set, also when the work
    /// threw.
    #[cold]
    #[inline(never)]
    fn safe_point_work(&mut self, op: &Op) -> VmResult<()> {
        let r = self.safe_point_checks(defers_oom(op));
        crate::interrupt::resync_safe_point();
        r
    }

    /// [`Vm::safe_point_work`] after an instruction that pushed or popped
    /// a frame (`Flow::Reload`, `Flow::Done`, the fast `DoCall` and
    /// `Return`), which may have freed the instruction with its op array.
    /// None of those instructions [defers](defers_oom) a pending OOM
    /// (`ZEND_VM_ENTER`/`ZEND_VM_LEAVE` are interrupt points), so the
    /// instruction itself is not needed.
    #[cold]
    #[inline(never)]
    fn safe_point_after_leave(&mut self) -> VmResult<()> {
        let r = self.safe_point_checks(false);
        crate::interrupt::resync_safe_point();
        r
    }

    /// The four checks of [`Vm::safe_point_work`]; `defers` is whether
    /// the instruction just run [defers](defers_oom) a pending OOM.
    fn safe_point_checks(&mut self, defers: bool) -> VmResult<()> {
        if object::has_pending_destructors() {
            self.run_pending_destructors()?;
        }
        // Root buffer reached the threshold (`gc_possible_root_when_full`).
        if crate::gc::wants_run() {
            self.gc_safe_point()?;
        }
        // Delayed memory_limit violation (`zend_mm_delayed_error`).
        if php_memlimit::is_pending() {
            self.poll_pending_oom(defers)?;
        }
        self.check_interrupt()
    }

    /// Applies a binary operator.
    pub(crate) fn binary(&mut self, op: BinKind, a: &Value, b: &Value) -> VmResult<Value> {
        let r = match op {
            BinKind::Arith(o) => match (o, a, b) {
                (BinaryOp::Add, Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
                    Some(r) => Ok(Value::Int(r)),
                    None => ops::add(a, b, self),
                },
                (BinaryOp::Sub, Value::Int(x), Value::Int(y)) => match x.checked_sub(*y) {
                    Some(r) => Ok(Value::Int(r)),
                    None => ops::sub(a, b, self),
                },
                (BinaryOp::Concat, Value::Str(x), Value::Str(y)) => {
                    Ok(Value::Str(ZStr::concat_str(x, y)))
                }
                _ => ops::binary_op(o, a, b, self),
            },
            BinKind::Identical => ops::is_identical(a, b).map(Value::Bool),
            BinKind::NotIdentical => ops::is_identical(a, b).map(|r| Value::Bool(!r)),
            BinKind::Equal => match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x == y)),
                _ => ops::is_equal(a, b, self).map(Value::Bool),
            },
            BinKind::NotEqual => ops::is_not_equal(a, b, self).map(Value::Bool),
            BinKind::Less => match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x < y)),
                _ => ops::is_smaller(a, b, self).map(Value::Bool),
            },
            BinKind::LessEqual => match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x <= y)),
                _ => ops::is_smaller_or_equal(a, b, self).map(Value::Bool),
            },
            BinKind::Spaceship => ops::spaceship(a, b, self),
        };
        r.map_err(|e| self.rt_throw(e))
    }

    /// `zend_is_true`.
    pub(crate) fn to_bool(&mut self, v: &Value) -> VmResult<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Int(i) => Ok(*i != 0),
            Value::Null => Ok(false),
            _ => v.to_bool(self).map_err(|e| self.rt_throw(e)),
        }
    }

    /// `zval_get_string`.
    pub(crate) fn to_zstr(&mut self, v: &Value) -> VmResult<ZStr> {
        match v {
            Value::Str(s) => Ok(s.clone()),
            _ => v.to_zstr(self).map_err(|e| self.rt_throw(e)),
        }
    }

    /// `zval_get_long`.
    pub(crate) fn to_long(&mut self, v: &Value) -> VmResult<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            _ => v.to_long(self).map_err(|e| self.rt_throw(e)),
        }
    }

    /// `zval_get_double`.
    pub(crate) fn to_double(&mut self, v: &Value) -> VmResult<f64> {
        match v {
            Value::Float(f) => Ok(*f),
            _ => v.to_double(self).map_err(|e| self.rt_throw(e)),
        }
    }

    /// Value copy semantics for assignment: references are unwrapped.
    /// Always inlined: a call passes and returns the `Value` through memory
    /// (`Vm::run`'s `Assign` arm made one in the release build of 4e1c2df8)
    /// for what is a tag test in the common case.
    #[inline(always)]
    #[allow(clippy::inline_always)] // see above
    pub(crate) fn copy_on_assign(&self, v: Value) -> Value {
        match v {
            Value::Reference(r) => r.get(),
            v => v,
        }
    }

    fn cast(&mut self, kind: CastKind, source: Operand, raw: Value) -> VmResult<Value> {
        let (x, reference) = match raw {
            Value::Reference(r) => (r.get(), Some(r)),
            other => (other, None),
        };
        Ok(match kind {
            CastKind::Int => Value::Int(self.to_long(&x)?),
            CastKind::Float => Value::Float(self.to_double(&x)?),
            // A string is its own cast (`zval_get_string` adds a reference);
            // the rope compiler emits this for `"$str"`.
            CastKind::String => match x {
                Value::Str(_) => x,
                _ => Value::Str(self.to_zstr(&x)?),
            },
            CastKind::Bool => Value::Bool(self.to_bool(&x)?),
            CastKind::Null => Value::Null,
            CastKind::Array => match x {
                Value::Array(_) => x,
                Value::Object(o) => Value::Array(self.object_to_array(&o)?),
                Value::Null => Value::Array(ZArray::new()),
                other => {
                    let other = if matches!(other, Value::Float(f) if f.is_nan()) {
                        let held = match source {
                            Operand::Cv(cv) => self.cv_get(cv),
                            _ => reference.map(Value::Reference),
                        };
                        self.warning("unexpected NAN value was coerced to array")?;
                        // Zend wraps the value after the callback, without re-dispatching.
                        match held {
                            Some(Value::Reference(r)) => r.get(),
                            _ => match source {
                                Operand::Cv(cv) => self.cv_get(cv).unwrap_or(Value::Null).deref(),
                                _ => other,
                            },
                        }
                    } else {
                        other
                    };
                    Value::Array(ZArray::from_values([other]))
                }
            },
            CastKind::Object => match x {
                Value::Object(_) => x,
                Value::Array(a) => self.array_to_object(&a)?,
                Value::Null => self.create_std_object_vm()?,
                other => {
                    let o = self.create_std_object_vm()?;
                    let other = if matches!(other, Value::Float(f) if f.is_nan()) {
                        let held = match source {
                            Operand::Cv(cv) => self.cv_get(cv),
                            _ => reference.map(Value::Reference),
                        };
                        self.warning("unexpected NAN value was coerced to object")?;
                        match held {
                            Some(Value::Reference(r)) => r.get(),
                            _ => match source {
                                Operand::Cv(cv) => self.cv_get(cv).unwrap_or(Value::Null).deref(),
                                _ => other,
                            },
                        }
                    } else {
                        other
                    };
                    if let Value::Object(obj) = &o {
                        self.write_dynamic_prop(obj, ZStr::from("scalar"), other);
                    }
                    o
                }
            },
        })
    }

    fn array_literal_add(&mut self, arr: u32, key: Option<Value>, v: Value) -> VmResult<()> {
        let mut a = std::mem::take(&mut self.frame_mut().tmps[arr as usize]);
        let res = (|| -> VmResult<()> {
            let Value::Array(za) = &mut a else {
                return Ok(());
            };
            match key {
                None => {
                    if za.make_mut().push(v).is_err() {
                        return Err(self.make_throw(
                            "Error",
                            "Cannot add element to the array as the next element is already occupied",
                        ));
                    }
                }
                Some(k) => {
                    let key = self.array_key(&k, KeyCtx::Literal)?;
                    if let Some(key) = key {
                        za.make_mut().insert(key, v);
                    }
                }
            }
            Ok(())
        })();
        self.frame_mut().tmps[arr as usize] = a;
        res
    }

    /// `ZEND_FETCH_LIST_W` on a VAR container that is not a reference (a
    /// by-value call result, `Zend/zend_vm_def.h)`: notices and falls back
    /// to `ZEND_FETCH_LIST_R`; the element then becomes a fresh reference.
    #[cold]
    fn list_fetch_ref(&mut self, pid: crate::opcode::PlaceId, dst: Option<u32>) -> VmResult<()> {
        let referenceable = match self.frame().code.places[pid as usize].base {
            crate::opcode::PlaceBase::Value(
                crate::opcode::Operand::Tmp(t) | crate::opcode::Operand::TmpKeep(t),
            ) => matches!(self.frame().tmps[t as usize], Value::Reference(_)),
            _ => true,
        };
        if referenceable {
            let r = self.place_make_ref(pid)?;
            if let Some(d) = dst {
                self.set_tmp(d, Value::Reference(r));
            }
            return Ok(());
        }
        self.notice("Attempting to set reference to non referenceable value")?;
        let v = self.place_fetch(pid, crate::opcode::FetchMode::List)?;
        if let Some(d) = dst {
            self.set_tmp(d, Value::Reference(PhpRef::new(v)));
        }
        Ok(())
    }

    /// `zend_ast_add_unpacked_element` (`Zend/zend_ast.c)`: a constant
    /// expression spreads arrays only, never Traversables.
    #[cold]
    #[allow(clippy::needless_pass_by_value)] // the operand value is owned by the caller's read
    fn const_array_unpack(&mut self, arr: u32, src: Value) -> VmResult<()> {
        let src = src.deref();
        if !matches!(src, Value::Array(_)) {
            return Err(self.make_throw(
                "Error",
                "Only arrays can be unpacked in constant expression",
            ));
        }
        self.array_unpack(arr, src)
    }

    fn array_unpack(&mut self, arr: u32, src: Value) -> VmResult<()> {
        match src {
            Value::Array(s) => {
                let mut a = std::mem::take(&mut self.frame_mut().tmps[arr as usize]);
                if let Value::Array(za) = &mut a {
                    let t = za.make_mut();
                    for (k, v) in s.iter() {
                        let v = v.deref();
                        match k {
                            php_runtime::ArrayKey::Int(_) => {
                                if t.push(v).is_err() {
                                    self.frame_mut().tmps[arr as usize] = a;
                                    return Err(self.make_throw(
                                        "Error",
                                        "Cannot add element to the array as the next element is already occupied",
                                    ));
                                }
                            }
                            php_runtime::ArrayKey::Str(_) => {
                                t.insert(k.clone(), v);
                            }
                        }
                    }
                }
                self.frame_mut().tmps[arr as usize] = a;
                Ok(())
            }
            Value::Object(o) if self.instance_of(&o, b"traversable") => {
                // Each element goes into the array as the iterator yields it
                // (`ZEND_ADD_ARRAY_UNPACK`): the array under construction
                // stays in its temporary, so when an exception ends the walk
                // the iterator, then the operand (`o`), then the array with
                // the elements added so far (`cleanup_live_vars`) are
                // released.
                self.iterate_each(&o, &mut |vm: &mut Vm, key: Value, v: Value| {
                    vm.array_unpack_add(arr, key, v)
                })
            }
            other => Err(self.make_throw(
                "Error",
                &format!(
                    "Only arrays and Traversables can be unpacked, {} given",
                    crate::typecheck::given_type(&other)
                ),
            )),
        }
    }

    /// Adds the element `v` with key `k` of an unpacked `Traversable` to the
    /// array being built in temporary `arr` (one step of
    /// `ZEND_ADD_ARRAY_UNPACK`): a string key that is not numeric
    /// (`ZEND_HANDLE_NUMERIC`) is kept, an integer or numeric string key
    /// appends.
    fn array_unpack_add(&mut self, arr: u32, k: Value, v: Value) -> VmResult<()> {
        let key = match k {
            Value::Int(_) => None,
            Value::Str(s) => match php_runtime::ArrayKey::from_zstr(&s) {
                k @ php_runtime::ArrayKey::Str(_) => Some(k),
                php_runtime::ArrayKey::Int(_) => None,
            },
            _ => {
                return Err(self.make_throw(
                    "Error",
                    "Keys must be of type int|string during array unpacking",
                ));
            }
        };
        let Some(Value::Array(za)) = self.frame_mut().tmps.get_mut(arr as usize) else {
            return Ok(());
        };
        let t = za.make_mut();
        match key {
            Some(k) => {
                t.insert(k, v);
            }
            None => {
                if let Err(v) = t.push(v) {
                    // `zend_cannot_add_element()`, then the element is
                    // released.
                    let e = self.make_throw(
                        "Error",
                        "Cannot add element to the array as the next element is already occupied",
                    );
                    drop(v);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// `ZEND_THROW` + `zend_throw_exception_object`.
    fn op_throw(&mut self, v: Value) -> Throw {
        match v {
            Value::Object(o) if self.instance_of(&o, b"throwable") => Throw::Exception(o),
            Value::Object(_) => self.make_throw(
                "Error",
                "Cannot throw objects that do not implement Throwable",
            ),
            _ => self.make_throw("Error", "Can only throw objects"),
        }
    }

    fn op_exit(&mut self, v: Value) -> VmResult<()> {
        match v {
            Value::Str(s) => {
                self.echo(s.as_bytes());
            }
            Value::Int(i) => {
                #[allow(clippy::cast_possible_truncation)]
                let st = i as i32;
                self.exit_status = st;
            }
            Value::Null => {}
            Value::Bool(b) => {
                self.exit_status = i32::from(b);
            }
            Value::Float(f) => {
                #[allow(clippy::cast_possible_truncation)]
                let st = f as i32;
                self.exit_status = st;
            }
            other => {
                return Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "exit(): Argument #1 ($status) must be of type string|int, {} given",
                        crate::typecheck::given_type(&other)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Runs the destructors of objects released since the last check.
    pub(crate) fn run_pending_destructors(&mut self) -> VmResult<()> {
        loop {
            let pending = object::take_pending_destructors();
            if pending.is_empty() {
                return Ok(());
            }
            // `zend_objects_destroy_object`: a destructor runs with the
            // pending exception set aside; a new exception gets the pending
            // one as its innermost `previous`.
            let mut pending_err: Option<Throw> = None;
            for o in pending {
                if let Err(e) = self.call_destructor(&o) {
                    pending_err = Some(match (pending_err.take(), e) {
                        (Some(Throw::Exception(old)), Throw::Exception(new)) => {
                            crate::exceptions::chain_previous(&new, &old);
                            Throw::Exception(new)
                        }
                        (Some(old), Throw::Exception(_)) => old,
                        (_, e) => e,
                    });
                }
                // Freed here unless the destructor stored it somewhere;
                // either way the handles of the objects whose release
                // queued it are free now.
                let h = o.handle();
                drop(o);
                object::release_deferred_handles(h);
            }
            if let Some(e) = pending_err {
                return Err(e);
            }
        }
    }

    /// Calls `__destruct` on an object.
    pub(crate) fn call_destructor(&mut self, o: &php_runtime::Obj) -> VmResult<()> {
        let cls = object::class_of(o).clone();
        let Some(m) = cls.magic.destruct.clone() else {
            // Native destructor hook without a `__destruct` method.
            if let Some(f) = cls.hooks.as_ref().and_then(|h| h.destruct) {
                return f(self, o);
            }
            return Ok(());
        };
        if m.visibility() != crate::func::flags::PUBLIC && !self.destructor_accessible(&cls, &m)? {
            return Ok(());
        }
        self.call_method(o, &m, Vec::new())?;
        // A native destructor hook also runs after a user `__destruct`
        // (the internal free path of the native parent).
        if let Some(f) = cls.hooks.as_ref().and_then(|h| h.destruct) {
            f(self, o)?;
        }
        Ok(())
    }

    /// The visibility check of `zend_objects_destroy_object` for a
    /// private/protected `__destruct`: while code executes, an inaccessible
    /// destructor throws an `Error`; with nothing executing (request
    /// shutdown) it is skipped with a warning. `Ok(false)` skips the call.
    #[cold]
    fn destructor_accessible(
        &mut self,
        cls: &Rc<crate::class::Class>,
        m: &crate::func::Method,
    ) -> VmResult<bool> {
        let vis = if m.visibility() == crate::func::flags::PRIVATE {
            "private"
        } else {
            "protected"
        };
        if !self.is_executing() {
            self.warning(&format!(
                "Call to {vis} {}::__destruct() from global scope during shutdown ignored",
                cls.name
            ))?;
            return Ok(false);
        }
        let scope = self.scope();
        if self.can_access_method(m, scope.as_ref()) {
            return Ok(true);
        }
        Err(self.make_throw(
            "Error",
            &format!(
                "Call to {vis} {}::__destruct() from {}",
                cls.name,
                scope_display(scope.as_ref())
            ),
        ))
    }

    /// The type of `ZEND_RETURN`: finishes the current frame.
    pub(crate) fn op_return(&mut self, v: Option<Value>, base: usize) -> VmResult<Flow> {
        let v = self.verify_return(v)?;
        self.op_return_raw(Some(v), base)
    }

    pub(crate) fn op_return_raw(&mut self, v: Option<Value>, base: usize) -> VmResult<Flow> {
        let v = v.unwrap_or_default();
        let fidx = self.frames.len() - 1;
        self.observer_end_frame(fidx, Some(&v), None);
        if self.frames[fidx].generator.is_some() {
            let frame = self.frames.pop().expect("frame");
            if let Some(g) = &frame.generator {
                crate::generator::finish_with(g, v);
            }
            drop(frame);
            if fidx <= base {
                return Ok(Flow::Done(Value::Null));
            }
            return Ok(Flow::Reload);
        }
        let is_main = Rc::ptr_eq(&self.frames[fidx].vars, &self.globals)
            || self.frames[fidx].kind == crate::vm::FrameKind::Main;
        if is_main && fidx <= base {
            self.frames[fidx].attach_symbol_table();
            self.frames[fidx].sync_cvs_to_symtab();
        } else if self.frames[fidx].symtab_attached {
            self.frames[fidx].sync_cvs_to_symtab();
            if let Some(prev) = fidx.checked_sub(1) {
                if self.frames[prev].symtab_attached {
                    self.frames[prev].sync_from_symbol_table();
                    self.frames[prev].sync_cvs_to_symtab();
                }
            }
        }
        let (ret, by_ref) = {
            let frame = &self.frames[fidx];
            let caller_this = fidx
                .checked_sub(1)
                .and_then(|i| self.frames[i].this.as_ref());
            crate::gc::frame_released(frame, caller_this);
            let by_ref = frame
                .ufunc
                .as_ref()
                .is_some_and(|u| u.flags & crate::func::flags::RETURN_REF != 0);
            (frame.ret, by_ref)
        };
        self.frames.release_top(&self.empty_code);
        let v = if by_ref { v } else { self.copy_on_assign(v) };
        if fidx <= base {
            return Ok(Flow::Done(v));
        }
        if let RetTarget::Tmp(t) = ret {
            self.set_tmp(t, v);
        }
        Ok(Flow::Reload)
    }

    /// `Fetching properties on non-enums in constant expressions is not
    /// allowed` (`zend_ast_evaluate_ex`, `ZEND_AST_PROP`).
    #[cold]
    fn non_enum_const_prop(&mut self) -> Throw {
        self.make_throw(
            "Error",
            "Fetching properties on non-enums in constant expressions is not allowed",
        )
    }

    /// `Cannot use [] on objects in constant expression`
    /// (`zend_ast_evaluate_ex`, `ZEND_AST_DIM`).
    #[cold]
    fn const_dim_on_object(&mut self) -> Throw {
        self.make_throw("Error", "Cannot use [] on objects in constant expression")
    }

    /// Creates an exception of an internal class and returns the throw.
    pub(crate) fn make_throw(&mut self, class: &str, msg: &str) -> Throw {
        match self.create_exception(class, msg, 0) {
            Ok(o) => Throw::Exception(o),
            Err(t) => t,
        }
    }

    /// Reports an `E_WARNING`.
    pub(crate) fn warning(&mut self, msg: &str) -> VmResult<()> {
        self.error(ErrorLevel::Warning.bits(), msg)
    }

    /// Reports an `E_NOTICE`.
    pub(crate) fn notice(&mut self, msg: &str) -> VmResult<()> {
        self.error(ErrorLevel::Notice.bits(), msg)
    }

    /// Reports an `E_DEPRECATED`.
    pub(crate) fn deprecated(&mut self, msg: &str) -> VmResult<()> {
        self.error(ErrorLevel::Deprecated.bits(), msg)
    }

    /// The current class scope.
    pub(crate) fn scope(&self) -> Option<Rc<crate::class::Class>> {
        self.frames
            .iter()
            .rev()
            .find(|f| f.kind != crate::vm::FrameKind::Builtin)
            .and_then(|f| f.scope.clone())
    }

    /// `ZEND_TYPE_ASSERT` (`Zend/zend_vm_def.h`) for an array parameter: the
    /// operand is read without an undefined-variable warning.
    fn op_type_assert_array(&mut self, val: Operand, msg: u32) -> VmResult<()> {
        let v = if let Operand::Cv(cv) = val {
            self.read_cv_defined(cv).unwrap_or(Value::Null)
        } else {
            self.read_raw(val)?
        };
        if matches!(v, Value::Array(_)) {
            return Ok(());
        }
        Err(self.type_assert_error(&v, msg))
    }

    #[cold]
    #[inline(never)]
    fn type_assert_error(&mut self, v: &Value, msg: u32) -> Throw {
        let prefix = self.lit_str(msg);
        let m = format!(
            "{}, {} given",
            prefix.to_string_lossy(),
            crate::typecheck::given_type(v)
        );
        self.make_throw("TypeError", &m)
    }
}

/// Releases the old value of a variable slot that holds no possible GC
/// root (no array, object or reference: `gc::possible_root` would ignore
/// it). Scalars own nothing and are forgotten, which skips the
/// out-of-line `drop_in_place` call; strings and resources are dropped.
#[inline(always)]
#[allow(clippy::inline_always)] // a call would defeat the point
fn discard_unrooted(old: Option<Value>) {
    match old {
        None | Some(Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)) => {
            std::mem::forget(old);
        }
        other => drop(other),
    }
}

/// Releases the old value of a variable slot (`i_zval_ptr_dtor` /
/// `zval_ptr_dtor` of a variable): an array, object or reference may
/// become a GC possible root ([`crate::gc::possible_root`], out of line);
/// anything else is released at once by [`discard_unrooted`].
#[inline(always)]
#[allow(clippy::inline_always)] // the common case is an empty or scalar slot
fn release_slot_value(old: Option<Value>) {
    if matches!(
        old,
        Some(Value::Array(_) | Value::Object(_) | Value::Reference(_))
    ) {
        release_rooted(old);
    } else {
        discard_unrooted(old);
    }
}

/// [`release_slot_value`] of an array, object or reference.
#[inline(never)]
fn release_rooted(old: Option<Value>) {
    if let Some(old) = &old {
        crate::gc::possible_root(old);
    }
    drop(old);
}

/// A compiled variable read as the value of an assignment to a place
/// ([`Vm::read_cv_op_data`]).
pub(crate) enum CvOpData {
    /// The (dereferenced) value.
    Value(Value),
    /// A reference to null, false or an array, possibly a container of the
    /// place: read by the walk ([`Vm::place_assign_cv_ref`]).
    Deferred,
    /// Undefined.
    Undefined,
}

/// How a key is used (affects error messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyCtx {
    /// Array literal.
    Literal,
    /// Read access.
    Read,
    /// Write access.
    Write,
    /// isset/empty.
    Isset,
    /// unset.
    Unset,
}

/// `E_FATAL_ERRORS` (`Zend/zend_errors.h`): `E_ERROR | E_CORE_ERROR |
/// E_COMPILE_ERROR | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE`.
const E_FATAL_ERRORS: i64 = 1 | 16 | 64 | 256 | 4096 | 4;

/// `E_HAS_ONLY_FATAL_ERRORS(mask)` (`Zend/zend_errors.h`).
const fn has_only_fatal_errors(mask: i64) -> bool {
    mask & !E_FATAL_ERRORS == 0
}

/// The mask after leaving an `@` whose saved mask is `old`
/// (`ZEND_END_SILENCE` / `ZEND_LIVE_SILENCE` cleanup): restored only when the
/// silenced expression did not set a non-silenced mask of its own.
pub(crate) const fn silence_restore(current: i64, old: i64) -> i64 {
    if has_only_fatal_errors(current) && !has_only_fatal_errors(old) {
        old
    } else {
        current
    }
}

/// "scope X" / "global scope" for messages.
pub(crate) fn scope_display(scope: Option<&Rc<crate::class::Class>>) -> String {
    match scope {
        Some(c) => format!("scope {}", object::display_name(&c.name)),
        None => "global scope".to_string(),
    }
}

/// "Undefined variable $x" — or "Undefined global variable $x" for the
/// auto-globals, which php-src fetches with `ZEND_FETCH_GLOBAL_LOCK`.
pub(crate) fn undefined_var_msg(name: &impl std::fmt::Display) -> String {
    let n = name.to_string();
    let global = matches!(
        n.as_str(),
        "_GET" | "_POST" | "_COOKIE" | "_SERVER" | "_ENV" | "_REQUEST" | "_FILES" | "_SESSION"
    );
    format!(
        "Undefined {}variable ${n}",
        if global { "global " } else { "" }
    )
}

thread_local! {
    /// Where the pending `memory_limit` violation was first seen
    /// (`EG(delayed_oom).filename`/`lineno`, recorded by
    /// `zend_mm_delayed_verror` at allocation time): the fatal is reported
    /// there even when it is raised at a later interrupt point.
    static OOM_SITE: std::cell::RefCell<Option<(ZStr, u32)>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether a pending OOM may wait past this instruction. php-src only acts
/// on `EG(vm_interrupt)` at interrupt points (`ZEND_VM_ENTER`, taken jumps,
/// after internal calls: `ZEND_VM_FCALL_INTERRUPT_CHECK`), so the plain
/// data instructions after the allocation that crossed the limit still run
/// (`oom_after_main_script.phpt` prints "Done" before the fatal).
fn defers_oom(op: &Op) -> bool {
    matches!(
        op,
        Op::Nop
            | Op::Move { .. }
            | Op::Bin { .. }
            | Op::Unary { .. }
            | Op::Cast { .. }
            | Op::Rope { .. }
            | Op::InitArray { .. }
            | Op::ArrayAdd { .. }
            | Op::AssignCv { .. }
            | Op::Assign { .. }
            | Op::AssignOp { .. }
            | Op::BinAssignCv { .. }
            | Op::AssignOpCv { .. }
            | Op::AssignOpDimCv { .. }
            | Op::AssignDimCv { .. }
            | Op::AssignDimsCv { .. }
            | Op::AssignOpDimsCv { .. }
            | Op::AssignStaticProp { .. }
            | Op::AssignOpStaticProp { .. }
            | Op::AssignObjProp { .. }
            | Op::AssignObjPropDim { .. }
            | Op::AssignOpObjProp { .. }
            | Op::AssignOpObjPropDim { .. }
            | Op::ConcatCv { .. }
            | Op::Abs { .. }
            | Op::Echo(_)
            | Op::Free(_)
    )
}

impl Vm {
    /// The per-instruction check of a pending `memory_limit` violation:
    /// raises it unless the instruction just run [defers](defers_oom) it (`defers`), in which case only
    /// the location is remembered.
    #[cold]
    #[inline(never)]
    fn poll_pending_oom(&mut self, defers: bool) -> VmResult<()> {
        if defers {
            if OOM_SITE.with(|s| s.borrow().is_none()) {
                let site = self.current_location();
                OOM_SITE.with(|s| *s.borrow_mut() = Some(site));
            }
            return Ok(());
        }
        self.raise_pending_oom()
    }

    /// Raises a recorded `memory_limit` violation as PHP's fatal error
    /// (`zend_handle_delayed_oom`), at the location it was first seen.
    pub(crate) fn raise_pending_oom(&mut self) -> VmResult<()> {
        let site = OOM_SITE.with(|s| s.borrow_mut().take());
        if let Some(over) = php_memlimit::take() {
            let (file, line) = site.unwrap_or_else(|| self.current_location());
            // `php_error_cb` (main/main.c) while the memory-limit error is
            // reported: the output buffers are discarded, unless a handler
            // is running, so the message reaches the SAPI and the buffered
            // output is neither printed nor copied again.
            self.reporting_oom = true;
            if self.output.status() & php_support::output::status::LOCKED == 0 {
                php_support::output::discard_all(self);
            }
            let r = self.error_at(crate::errors::level::ERROR, &over.to_string(), &file, line);
            self.reporting_oom = false;
            r?;
        }
        Ok(())
    }

    /// Where a builtin's result leaves it: a [`Throw::Fatal`] that is a
    /// still pending `memory_limit` violation (an [`OverLimit`] returned
    /// with `?`) is reported here, at the caller's line, before it unwinds
    /// further. Any other result passes unchanged.
    ///
    /// [`OverLimit`]: php_memlimit::OverLimit
    #[inline]
    pub(crate) fn deliver_oom<T>(&mut self, r: VmResult<T>) -> VmResult<T> {
        if matches!(r, Err(Throw::Fatal)) && php_memlimit::is_pending() {
            self.raise_pending_oom()?;
        }
        r
    }

    /// Forgets a pending violation and its location (start of request
    /// shutdown: `EG(delayed_oom).state = ZEND_OOM_STATE_NONE`).
    pub(crate) fn clear_pending_oom() {
        php_memlimit::clear();
        OOM_SITE.with(|s| *s.borrow_mut() = None);
    }
}

impl Vm {
    /// Whether the native stack used since the first execution exceeds the
    /// usable budget (`zend.max_allowed_stack_size` minus
    /// `zend.reserved_stack_size`, or the thread's stack minus a safety
    /// margin for the interpreter's own frames).
    /// Extensions may use this check without raising the VM's stack error.
    pub fn native_stack_exhausted(&mut self) -> bool {
        let marker = 0u8;
        let here = std::ptr::addr_of!(marker) as usize;
        if self.native_stack_base == 0 || here > self.native_stack_base {
            self.native_stack_base = here;
            return false;
        }
        let used = self.native_stack_base - here;
        // Both INIs are INI_SYSTEM: read them once. A reservation of 0 is
        // php-src's default of 48 KiB (`OnUpdateReservedStackSize`:
        // `ZEND_ALLOCA_MAX_SIZE + 16K`).
        let (max, reserved) = match self.stack_limit_ini {
            Some(t) => t,
            None => {
                let reserved = match self.ini.get_long("zend.reserved_stack_size") {
                    r if r <= 0 => 48 << 10,
                    r => usize::try_from(r).unwrap_or(usize::MAX),
                };
                let t = (self.ini.get_long("zend.max_allowed_stack_size"), reserved);
                self.stack_limit_ini = Some(t);
                t
            }
        };
        if max < 0 {
            return false;
        }
        // The real thread stack must never be smashed, whatever the INI says.
        let hard = self
            .native_stack_size
            .saturating_sub((self.native_stack_size / 8).max(256 << 10));
        let budget = if max > 0 {
            // `zend.max_allowed_stack_size` is sized for php-src's frames;
            // the interpreter's are ~4x larger (same scale as fiber stacks).
            usize::try_from(max)
                .unwrap_or(usize::MAX)
                .saturating_sub(reserved)
                .saturating_mul(crate::fiber::STACK_SCALE)
        } else {
            hard.saturating_sub(reserved)
        };
        used > budget.min(hard)
    }

    /// Declares the native stack size of the thread running the VM (the
    /// CLI's large VM stack); the default assumes 8 MiB.
    pub fn set_native_stack_size(&mut self, bytes: usize) {
        self.native_stack_size = bytes;
    }
}

/// `cleanup_unfinished_calls` + `ZEND_LIVE_NEW`: the object of a `new`
/// whose constructor call an exception abandons before it was made is
/// never destructed (`zend_object_store_ctor_failed`).
#[cold]
pub(crate) fn ctor_failed_unfinished(calls: &[crate::vm::PendingCall]) {
    for c in calls {
        if c.is_ctor {
            if let Some(t) = &c.this {
                let d = object::data(t);
                d.flags.set(d.flags.get() | object::oflags::DESTRUCTED);
            }
        }
    }
}

impl Vm {
    /// `return $cv;` of an undefined CV in a function with a checked return
    /// type: php-src's `ZEND_VERIFY_RETURN_TYPE` reads the CV first (one
    /// "Undefined variable" warning) and `ZEND_RETURN` reads it again (a
    /// second one) when null passes the type. Returns the value to return
    /// when the second read must not happen (null fails the type, so the
    /// check throws before `ZEND_RETURN`); `None` when the ordinary read
    /// follows.
    #[cold]
    #[inline(never)]
    fn return_undefined_typed_cv(&mut self, cv: u32) -> VmResult<Option<Value>> {
        let f = self.frame();
        let defined = f.cvs.get(cv as usize).and_then(CvSlot::get).is_some();
        let Some(uf) = f.ufunc.as_ref() else {
            return Ok(None);
        };
        if defined || uf.flags & crate::func::flags::GENERATOR != 0 {
            return Ok(None);
        }
        let Some(rt) = uf.return_type.as_ref() else {
            return Ok(None);
        };
        if rt.is_mixed() {
            return Ok(None);
        }
        let accepts_null = crate::call::accepts_without_coercion(rt.mask, &Value::Null);
        let v = self.read(Operand::Cv(cv))?;
        Ok((!accepts_null).then_some(v))
    }
}

#[cfg(test)]
mod leave_tests {
    use super::defers_oom;
    use crate::opcode::{Op, Operand};

    /// The instructions that push or pop a frame (`Flow::Reload`,
    /// `Flow::Done`, the fast `DoCall`/`Return` arms) run their safe point
    /// as `Vm::safe_point_after_leave`, which assumes none of them defers
    /// a pending OOM.
    #[test]
    fn frame_changing_instructions_do_not_defer_oom() {
        let ops = [
            Op::DoCall { dst: None },
            Op::DoCall { dst: Some(0) },
            Op::Return {
                val: Operand::Unused,
            },
            Op::Return {
                val: Operand::Tmp(0),
            },
            Op::ReturnByRefValue {
                val: Operand::Cv(0),
            },
            Op::GeneratorCreate,
            Op::Yield {
                key: None,
                val: None,
                dst: None,
            },
            Op::YieldFrom {
                src: Operand::Tmp(0),
                dst: None,
            },
        ];
        for op in &ops {
            assert!(!defers_oom(op), "{op:?}");
        }
        // Plain data instructions do defer it (`oom_after_main_script.phpt`).
        assert!(defers_oom(&Op::Nop));
    }
}
