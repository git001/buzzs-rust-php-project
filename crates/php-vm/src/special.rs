// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Handlers of the opcodes that `zend_try_compile_special_func()` compiles
//! builtin calls to (ref: `Zend/zend_compile.c` `zend_compile_func_strlen`,
//! `_count`, `_typecheck`, `_gettype`, `_defined`, `_in_array`,
//! `_array_key_exists`; `Zend/zend_vm_def.h` `ZEND_STRLEN`, `ZEND_COUNT`,
//! `ZEND_TYPE_CHECK`, `ZEND_GET_TYPE`, `ZEND_DEFINED`, `ZEND_IN_ARRAY`,
//! `ZEND_ARRAY_KEY_EXISTS`; `Zend/zend_execute.c` `zend_quick_check_constant`,
//! `zend_array_key_exists_fast`, `zend_array_key_exists_error`).
//!
//! The opcodes run in the caller's frame: no builtin frame is pushed, so
//! a `TypeError` they raise has no `strlen()` line in its trace, exactly as
//! in php-src. The common operand types are handled inline by the run
//! loop; the rest comes here.
//!
//! Also here: frameless internal calls (ref: `Zend/zend_frameless_function.h`,
//! `ZEND_FRAMELESS_ICALL_n` in `Zend/zend_vm_def.h`), which call a handler
//! without pushing its call frame.

use crate::call::forget_unallocated;
use crate::compiler::SPECIAL_BUILTINS;
use crate::func::{flags, Args, Builtin, Func};
use crate::object;
use crate::opcode::{FramelessCall, Operand};
use crate::typecheck::given_type;
use crate::vm::{PendingCall, Vm, VmResult};
use php_runtime::{Value, ZArray, ZStr};
use std::rc::Rc;

impl Vm {
    /// [`crate::compiler::CompileOptions::special_builtins`] for the
    /// current function table: the special builtins registered as
    /// (non-deprecated) internal functions. A disabled function redeclared
    /// in user code keeps its calls, and so does everything under
    /// [`Vm::compile_no_builtins`] (`ZEND_COMPILE_NO_BUILTINS`).
    pub(crate) fn special_builtins(&self) -> u32 {
        if self.compile_no_builtins {
            return 0;
        }
        let mut mask = 0;
        for (i, name) in SPECIAL_BUILTINS.iter().enumerate() {
            let internal = matches!(
                self.functions.get(name.as_bytes()),
                Some(Func::Builtin(b)) if b.flags & (flags::DEPRECATED | flags::NODISCARD) == 0
            );
            if internal {
                mask |= 1 << i;
            }
        }
        mask
    }

    /// `ZEND_STRLEN` for an operand that is not a string: weak mode
    /// coerces scalars and `Stringable` objects (`zend_parse_arg_str_weak`)
    /// and deprecates `null`; anything else is a `TypeError`.
    #[cold]
    #[inline(never)]
    pub(crate) fn op_strlen_slow(&mut self, v: Value) -> VmResult<Value> {
        let mut v = match v {
            Value::Reference(r) => r.get(),
            other => other,
        };
        if let Value::Str(s) = &v {
            return Ok(Value::Int(i64::try_from(s.len()).unwrap_or(i64::MAX)));
        }
        if !self.frame().code.strict_types {
            if v.is_null() {
                self.deprecated(
                    "strlen(): Passing null to parameter #1 ($string) of type string is deprecated",
                )?;
                return Ok(Value::Int(0));
            }
            // Leaves `v` as it was when it cannot be converted.
            if self.weak_scalar_coerce(crate::types::mask::STRING, &mut v)? {
                if let Value::Str(s) = &v {
                    return Ok(Value::Int(i64::try_from(s.len()).unwrap_or(i64::MAX)));
                }
            }
        }
        Err(self.make_throw(
            "TypeError",
            &format!(
                "strlen(): Argument #1 ($string) must be of type string, {} given",
                given_type(&v)
            ),
        ))
    }

    /// `ZEND_COUNT` for an operand that is not an array. Objects run the
    /// `count()` builtin (`count_elements` handlers and `Countable`);
    /// anything else is a `TypeError`.
    #[cold]
    #[inline(never)]
    pub(crate) fn op_count_slow(&mut self, v: Value, sizeof: bool) -> VmResult<Value> {
        let name = if sizeof { "sizeof" } else { "count" };
        let v = match v {
            Value::Reference(r) => r.get(),
            other => other,
        };
        match v {
            Value::Array(a) => Ok(Value::Int(i64::try_from(a.len()).unwrap_or(i64::MAX))),
            v @ Value::Object(_) => self.special_func_fallback(name, v),
            v => Err(self.make_throw(
                "TypeError",
                &format!(
                    "{name}(): Argument #1 ($value) must be of type Countable|array, {} given",
                    given_type(&v)
                ),
            )),
        }
    }

    /// `ZEND_GET_CLASS`: the class name of object `a`, or with no operand
    /// the running function's scope (deprecated).
    #[cold]
    #[inline(never)]
    pub(crate) fn op_get_class(&mut self, a: Operand) -> VmResult<Value> {
        if matches!(a, Operand::Unused) {
            let Some(scope) = self.frame().scope.clone() else {
                return Err(self.make_throw(
                    "Error",
                    "get_class() without arguments must be called from within a class",
                ));
            };
            self.deprecated("Calling get_class() without arguments is deprecated")?;
            return Ok(Value::Str(scope.name.clone()));
        }
        match self.read(a)? {
            Value::Object(o) => Ok(Value::Str(object::class_of(&o).name.clone())),
            v => Err(self.make_throw(
                "TypeError",
                &format!(
                    "get_class(): Argument #1 ($object) must be of type object, {} given",
                    given_type(&v)
                ),
            )),
        }
    }

    /// `ZEND_GET_CALLED_CLASS`: the running function's called scope.
    #[cold]
    #[inline(never)]
    pub(crate) fn op_get_called_class(&mut self) -> VmResult<Value> {
        match self.frame().called.clone() {
            Some(called) => Ok(Value::Str(called.name.clone())),
            None => Err(self.make_throw(
                "Error",
                "get_called_class() must be called from within a class",
            )),
        }
    }

    /// `ZEND_DEFINED`: whether the constant named by literal `name` exists.
    /// Like `zend_quick_check_constant`, only the exact name is looked up:
    /// `true`/`false`/`null` were resolved at compile time, and
    /// `__COMPILER_HALT_OFFSET__` is registered under a mangled name.
    #[inline]
    pub(crate) fn op_defined(&mut self, name: u32) -> VmResult<bool> {
        if let Value::Str(n) = &self.frame().code.literals[name as usize] {
            return Ok(self.constants.contains_key(n));
        }
        self.op_defined_slow(name)
    }

    /// `defined()` of a number literal (`defined(1.5)`): the name is
    /// `zval_get_string` of the literal, formatted with the `precision`
    /// setting in effect.
    #[cold]
    #[inline(never)]
    fn op_defined_slow(&mut self, name: u32) -> VmResult<bool> {
        let v = self.lit(name);
        let n = self.to_zstr(&v)?;
        Ok(self.constants.contains_key(&n))
    }

    /// `ZEND_IN_ARRAY`: whether `needle` is one of the keys of the literal
    /// `set`. A string needle is looked up as a string key and, in strict
    /// mode, an integer as an integer key; in loose mode `null` and `false`
    /// equal only the empty string, and any other needle is compared with
    /// `==` against each key in order.
    pub(crate) fn op_in_array(&mut self, needle: &Value, set: u32, strict: bool) -> VmResult<bool> {
        let keys = {
            let Value::Array(keys) = &self.frame().code.literals[set as usize] else {
                unreachable!("the haystack literal of ZEND_IN_ARRAY is an array");
            };
            match needle {
                Value::Str(s) => return Ok(keys.get_str(s.as_bytes()).is_some()),
                Value::Int(i) if strict => return Ok(keys.get_int(*i).is_some()),
                _ if strict => return Ok(false),
                Value::Null | Value::Bool(false) => return Ok(keys.get_str(b"").is_some()),
                _ => keys.clone(),
            }
        };
        self.in_array_compare(needle, &keys)
    }

    /// The loose-comparison loop of `ZEND_IN_ARRAY`: `zend_compare` of the
    /// needle with each string key, in order, until the first match.
    #[cold]
    #[inline(never)]
    fn in_array_compare(&mut self, needle: &Value, keys: &ZArray) -> VmResult<bool> {
        for key in keys.keys() {
            let key = key.to_value();
            if php_runtime::ops::is_equal(needle, &key, self).map_err(|e| self.rt_throw(e))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `ZEND_ARRAY_KEY_EXISTS`: both operands are read without the
    /// undefined-variable warning first (`GET_OP*_ZVAL_PTR_UNDEF`); the
    /// warnings come from the lookup or the type error, in php-src's order.
    pub(crate) fn op_key_exists(&mut self, key: Operand, array: Operand) -> VmResult<bool> {
        let k = self.read_quiet(key)?;
        let subject = self.read_quiet(array)?;
        match subject {
            Some(Value::Array(a)) => match k {
                Some(Value::Str(s)) => Ok(a.get_sym(s.as_bytes()).is_some()),
                Some(Value::Int(i)) => Ok(a.get_int(i).is_some()),
                k => self.key_exists_slow(key, k, &a),
            },
            subject => self.key_exists_error(key, k.is_none(), array, subject.as_ref()),
        }
    }

    /// `zend_array_key_exists_fast` for the key types the fast path leaves
    /// out: an undefined variable (warning, then `""`), `null` (deprecated,
    /// then `""`), floats (converted with their diagnostics while the array
    /// is kept alive), booleans, resources (warning) and arrays or objects
    /// (`zend_illegal_array_offset_access`).
    #[cold]
    #[inline(never)]
    fn key_exists_slow(
        &mut self,
        key_op: Operand,
        key: Option<Value>,
        a: &ZArray,
    ) -> VmResult<bool> {
        match key {
            None => {
                self.warn_undefined_cv(key_op)?;
                Ok(a.get_str(b"").is_some())
            }
            Some(v @ (Value::Array(_) | Value::Object(_))) => Err(self.make_throw(
                "TypeError",
                &format!("Cannot access offset of type {} on array", given_type(&v)),
            )),
            Some(v @ Value::Float(_)) => {
                // php-src addrefs the array while the conversion warns: a
                // handler that drops the last other owner makes the lookup
                // fail (`GC_DELREF` reaching zero destroys the array).
                let had_owner = a.refcount() > 1;
                let found = php_stdarr::basic::array_key_exists(self, "array_key_exists", &v, a)
                    .map_err(|e| self.rt_throw(e))?;
                Ok(found && !(had_owner && a.refcount() == 1))
            }
            Some(v) => php_stdarr::basic::array_key_exists(self, "array_key_exists", &v, a)
                .map_err(|e| self.rt_throw(e)),
        }
    }

    /// `zend_array_key_exists_error`: the undefined-variable warnings for
    /// the key and then the subject, then the `TypeError` (which an
    /// exception thrown by a warning handler pre-empts).
    #[cold]
    #[inline(never)]
    fn key_exists_error(
        &mut self,
        key_op: Operand,
        key_undefined: bool,
        array_op: Operand,
        subject: Option<&Value>,
    ) -> VmResult<bool> {
        if key_undefined {
            self.warn_undefined_cv(key_op)?;
        }
        if subject.is_none() {
            self.warn_undefined_cv(array_op)?;
        }
        let given = subject.map_or_else(|| "null".to_string(), given_type);
        Err(self.make_throw(
            "TypeError",
            &format!(
                "array_key_exists(): Argument #2 ($array) must be of type array, {given} given"
            ),
        ))
    }

    /// Reads an operand without the undefined-variable warning; `None` is
    /// an undefined variable (`GET_OP1_ZVAL_PTR_UNDEF`).
    fn read_quiet(&mut self, op: Operand) -> VmResult<Option<Value>> {
        match op {
            Operand::Cv(cv) => Ok(self.read_cv_defined(cv)),
            other => self.read(other).map(Some),
        }
    }

    /// `ZVAL_UNDEFINED_OP*()` for a compiled-variable operand.
    fn warn_undefined_cv(&mut self, op: Operand) -> VmResult<()> {
        if let Operand::Cv(cv) = op {
            let name = self.frame().code.cv_names[cv as usize].clone();
            self.warning(&crate::exec::undefined_var_msg(&name))?;
        }
        Ok(())
    }

    /// Runs special builtin `name` as a regular internal call with
    /// argument `v` (the operand kinds an opcode leaves to the builtin).
    /// Like the opcode, the call is hidden from observers.
    #[cold]
    #[inline(never)]
    fn special_func_fallback(&mut self, name: &str, v: Value) -> VmResult<Value> {
        let Some(Func::Builtin(b)) = self.functions.get(name.as_bytes()).cloned() else {
            return Err(self.make_throw("Error", &format!("Call to undefined function {name}()")));
        };
        let mut call = PendingCall::new(Func::Builtin(b.clone()));
        call.args.push(Some(v));
        call.direct = true;
        let strict = self.frame().code.strict_types;
        if self.observer.enabled {
            self.observer.quiet_builtin = true;
        }
        self.call_builtin(&b, call, strict)
    }
}

/// The frameless-call classes, kept in bits 27..=31 of a builtin
/// function's flags ([`crate::func::flags::FRAMELESS`]): php-src's
/// frameless functions (`ZEND_FRAMELESS_FUNCTION` in ext/standard) plus
/// `abs()`, `chr()`, `ord()` and the math functions. Each class names the
/// argument shapes for which its handler has no diagnostic, exception,
/// callback, frame inspection or immediate memory-limit fatal (see
/// [`frameless_args`]); for those shapes no frame can be observed, so
/// whether php-src has a frameless variant does not matter.
mod fl {
    /// `abs(int|float)`.
    pub(super) const ABS: u32 = 1;
    /// `dechex(int)`.
    pub(super) const DECHEX: u32 = 2;
    /// `strtolower(string)`, `strtoupper(string)`.
    pub(super) const CASE: u32 = 3;
    /// `trim()`, `ltrim()`, `rtrim()` of a string, with a character list
    /// without a `..` range (`php_charmask()` warns about invalid ones).
    pub(super) const TRIM: u32 = 4;
    /// `dirname(string[, int])` with at least one level (else a
    /// `ValueError`).
    pub(super) const DIRNAME: u32 = 5;
    /// `is_numeric()` of a scalar.
    pub(super) const IS_NUMERIC: u32 = 6;
    /// `ord()` of a one-byte string (master deprecates other lengths).
    pub(super) const ORD: u32 = 7;
    /// `chr()` of 0..=255 (master deprecates other values).
    pub(super) const CHR: u32 = 8;
    /// `strpos(string, string[, int])` with an offset inside the haystack
    /// (else a `ValueError`).
    pub(super) const STRPOS: u32 = 9;
    /// `str_contains()`, `str_starts_with()`, `str_ends_with()`.
    pub(super) const STR_TEST: u32 = 10;
    /// `max()`, `min()` of two scalars.
    pub(super) const MIN_MAX: u32 = 11;
    /// `substr(string, int[, ?int])`.
    pub(super) const SUBSTR: u32 = 12;
    /// `strstr(string, string[, bool])`.
    pub(super) const STRSTR: u32 = 13;
    /// `in_array()` of a scalar in an array, strict, or loose over a short
    /// haystack without objects or resources (whose comparisons can warn
    /// or call `__toString()`).
    pub(super) const IN_ARRAY: u32 = 14;
    /// `str_replace(string, string, string)` with a replacement no longer
    /// than the search string (a growing result first reserves its memory,
    /// which can fail with the memory-limit fatal).
    pub(super) const STR_REPLACE: u32 = 15;
    /// `strtr(string, string, string)`.
    pub(super) const STRTR: u32 = 16;
    /// The one-argument float functions (`sqrt()`, `sin()`, `exp()`, ...,
    /// `floor()`, `ceil()`, `is_nan()`) of an int or a float: zpp widens an
    /// int silently in both modes (`zend_parse_arg_double_slow`), and the
    /// handlers read it as a float either way.
    pub(super) const FLOAT: u32 = 17;
    /// The two-argument float functions (`atan2()`, `hypot()`, `fmod()`,
    /// `fdiv()`, `fpow()`) of ints or floats.
    pub(super) const FLOAT2: u32 = 18;
    /// `log()` of an int or a float without a base (a base can be invalid).
    pub(super) const LOG: u32 = 19;
    /// `round(int|float[, int])` without a mode (a mode can be invalid).
    pub(super) const ROUND: u32 = 20;
    /// `intdiv(int, int)` neither by zero nor of `PHP_INT_MIN` by -1.
    pub(super) const INTDIV: u32 = 21;
    /// `pi()`.
    pub(super) const PI: u32 = 22;
}

/// The frameless-call class bits ([`flags::FRAMELESS`]) of the builtin
/// function named `lc` (lower case), set when it is registered. Aliases
/// copy them with the other flags.
pub(crate) fn frameless_flags(lc: &[u8]) -> u32 {
    let class = match lc {
        b"abs" => fl::ABS,
        b"dechex" => fl::DECHEX,
        b"strtolower" | b"strtoupper" => fl::CASE,
        b"trim" | b"ltrim" | b"rtrim" => fl::TRIM,
        b"dirname" => fl::DIRNAME,
        b"is_numeric" => fl::IS_NUMERIC,
        b"ord" => fl::ORD,
        b"chr" => fl::CHR,
        b"strpos" => fl::STRPOS,
        b"str_contains" | b"str_starts_with" | b"str_ends_with" => fl::STR_TEST,
        b"max" | b"min" => fl::MIN_MAX,
        b"substr" => fl::SUBSTR,
        b"strstr" => fl::STRSTR,
        b"in_array" => fl::IN_ARRAY,
        b"str_replace" => fl::STR_REPLACE,
        b"strtr" => fl::STRTR,
        b"sqrt" | b"exp" | b"expm1" | b"log10" | b"log1p" | b"sin" | b"cos" | b"tan" | b"asin"
        | b"acos" | b"atan" | b"sinh" | b"cosh" | b"tanh" | b"asinh" | b"acosh" | b"atanh"
        | b"deg2rad" | b"rad2deg" | b"floor" | b"ceil" | b"is_nan" | b"is_finite"
        | b"is_infinite" => fl::FLOAT,
        b"atan2" | b"hypot" | b"fmod" | b"fdiv" | b"fpow" => fl::FLOAT2,
        b"log" => fl::LOG,
        b"round" => fl::ROUND,
        b"intdiv" => fl::INTDIV,
        b"pi" => fl::PI,
        _ => return 0,
    };
    class << flags::FRAMELESS_SHIFT
}

/// Whether `call` of builtin `b` runs frameless. php-src's frameless
/// internal calls (`ZEND_FRAMELESS_ICALL_n`) skip the call frame and
/// synthesise one from the opline when the function reports an error
/// (`zend_fetch_debug_backtrace`, `zend_active_function_ex`). Here the
/// choice is made per call instead: a plain call ([`plain_call`]) of a
/// function with a frameless class runs without a frame only for
/// arguments with which nothing can see the frame ([`frameless_args`]:
/// accepted types, so zpp coerces nothing, and values the handler cannot
/// report about). Every other call, including each one that could warn or
/// throw, pushes the real builtin frame, which is the frame php-src would
/// synthesise: traces still show `#0 file(line): strpos(...)`.
#[inline]
pub(crate) fn frameless_call(b: &Builtin, call: &PendingCall) -> bool {
    let class = (b.flags & flags::FRAMELESS) >> flags::FRAMELESS_SHIFT;
    class != 0
        && b.flags & (flags::DEPRECATED | flags::NODISCARD) == 0
        && plain_call(call)
        && frameless_args(class, &call.args)
}

/// A call by name with positional arguments only and nothing bound to it:
/// what `ZEND_INIT_FCALL` and the `SEND` ops make of `f($a, $b)`.
/// Always inline: a few field tests, made out of line (~30 instructions
/// a frameless call) when merely `#[inline]`.
#[inline(always)]
#[allow(clippy::inline_always)]
fn plain_call(call: &PendingCall) -> bool {
    !call.has_named
        && !call.dynamic
        && !call.is_ctor
        && call.named.is_empty()
        && call.magic_name.is_none()
        && call.this.is_none()
        && call.scope.is_none()
        && call.called.is_none()
        && call.closure.is_none()
        && call.pfa.is_none()
}

/// Whether the handler of frameless class `class` ([`fl`]) runs for
/// `args` (as sent: by value, all passed) without anything that could
/// observe its frame: the values already have the declared types (no
/// coercion, deprecation or `TypeError`), and none of the class's error
/// conditions holds.
pub(crate) fn frameless_args(class: u32, args: &[Option<Value>]) -> bool {
    use Value::{Array, Bool, Float, Int, Null, Str};
    match (class, args) {
        (fl::ABS | fl::FLOAT | fl::LOG | fl::ROUND, [Some(Int(_) | Float(_))])
        | (fl::FLOAT2, [Some(Int(_) | Float(_)), Some(Int(_) | Float(_))])
        | (fl::ROUND, [Some(Int(_) | Float(_)), Some(Int(_))])
        | (fl::PI, [])
        | (fl::DECHEX, [Some(Int(_))])
        | (fl::CASE | fl::TRIM | fl::DIRNAME, [Some(Str(_))])
        | (fl::STRPOS | fl::STR_TEST | fl::STRSTR, [Some(Str(_)), Some(Str(_))])
        | (fl::STRSTR, [Some(Str(_)), Some(Str(_)), Some(Bool(_))])
        | (
            fl::SUBSTR,
            [Some(Str(_)), Some(Int(_))] | [Some(Str(_)), Some(Int(_)), Some(Int(_) | Null)],
        )
        | (fl::STRTR, [Some(Str(_)), Some(Str(_)), Some(Str(_))]) => true,
        (fl::TRIM, [Some(Str(_)), Some(Str(chars))]) => !has_char_range(chars.as_bytes()),
        (fl::DIRNAME, [Some(Str(_)), Some(Int(levels))]) => *levels >= 1,
        (fl::IS_NUMERIC, [Some(v)]) => is_scalar(v),
        (fl::ORD, [Some(Str(s))]) => s.len() == 1,
        (fl::CHR, [Some(Int(c))]) => (0..=255).contains(c),
        (fl::STRPOS, [Some(Str(haystack)), Some(Str(_)), Some(Int(offset))]) => {
            offset_within(haystack.len(), *offset)
        }
        (fl::MIN_MAX, [Some(a), Some(b)]) => is_scalar(a) && is_scalar(b),
        (fl::IN_ARRAY, [Some(needle), Some(Array(haystack))]) => {
            is_scalar(needle) && loose_comparisons_silent(haystack)
        }
        (fl::IN_ARRAY, [Some(needle), Some(Array(haystack)), Some(Bool(strict))]) => {
            is_scalar(needle) && (*strict || loose_comparisons_silent(haystack))
        }
        (fl::STR_REPLACE, [Some(Str(search)), Some(Str(replace)), Some(Str(_))]) => {
            replace.len() <= search.len()
        }
        (fl::INTDIV, [Some(Int(dividend)), Some(Int(divisor))]) => {
            *divisor != 0 && !(*dividend == i64::MIN && *divisor == -1)
        }
        _ => false,
    }
}

/// Whether `v` is null or a scalar: a value whose comparisons and
/// conversions run no PHP code and report nothing.
#[inline]
fn is_scalar(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
    )
}

/// Whether `php_charmask()` reads a `..` range in the character list
/// `chars` (and may warn that it is invalid).
fn has_char_range(chars: &[u8]) -> bool {
    chars.windows(2).any(|w| w == b"..")
}

/// Whether `offset` is inside a haystack of `len` bytes as `strpos()`
/// requires (`-len <= offset <= len`).
fn offset_within(len: usize, offset: i64) -> bool {
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    (-len..=len).contains(&offset)
}

/// The longest haystack a loose frameless `in_array()` checks up front;
/// longer ones take the framed call rather than being walked twice.
const LOOSE_SCAN_MAX: usize = 32;

/// Whether the loose comparisons (`==`) of a scalar with each element of
/// `haystack` are silent: no element is an object (conversion warnings,
/// `__toString()`) or a resource, also behind a reference.
fn loose_comparisons_silent(haystack: &ZArray) -> bool {
    let silent = |v: &Value| !matches!(v, Value::Object(_) | Value::Resource(_));
    haystack.len() <= LOOSE_SCAN_MAX
        && haystack.values().all(|v| match v {
            Value::Reference(r) => r.try_borrow().is_some_and(|inner| silent(&inner)),
            v => silent(v),
        })
}

impl Vm {
    /// A [`frameless_call`]: the handler runs on the caller's frame with
    /// the arguments moved out of the pending call (and the declared
    /// defaults of omitted trailing parameters, as for a framed call).
    /// The call is a [`plain_call`], so only the function handle (moved
    /// into the handler's arguments) and the argument buffer (back to the
    /// pool) are left to release.
    pub(crate) fn call_builtin_frameless(
        &mut self,
        b: &Rc<Builtin>,
        call: PendingCall,
    ) -> VmResult<Value> {
        if self.magic_call_name.is_some() {
            self.magic_call_name = None;
        }
        let PendingCall {
            func,
            args: mut sent,
            named,
            ..
        } = call;
        forget_unallocated(named);
        let handle = match func {
            Func::Builtin(handle) => handle,
            Func::User(_) => b.clone(),
        };
        let nparams = b.params.len();
        let mut values = self.call_bufs.values.pop().unwrap_or_default();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        values.reserve(nparams);
        let mut passed = self.call_bufs.passed.pop().unwrap_or_default();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        passed.reserve(nparams);
        for arg in &mut sent {
            values.push(arg.take().unwrap_or_default());
            passed.push(true);
        }
        while values.len() < nparams {
            let i = values.len();
            match self.builtin_default(b, i) {
                Some(d) if i >= b.required as usize => {
                    values.push(d);
                    passed.push(false);
                }
                _ => break,
            }
        }
        let mut args = Args {
            values,
            passed,
            named: Vec::new(),
            this: None,
            called: None,
            func: Some(handle),
        };
        let mut r = (b.handler)(self, &mut args);
        r = self.deliver_oom(r);
        // `ZEND_VM_FCALL_INTERRUPT_CHECK`, after the call.
        if r.is_ok() {
            if let Err(e) = self.check_interrupt() {
                r = Err(e);
            }
        }
        let Args {
            values,
            passed,
            named,
            ..
        } = args;
        forget_unallocated(named);
        self.put_val_vec(values);
        self.put_bool_vec(passed);
        self.put_arg_vec(sent);
        r
    }
}

/// The result of a one-argument frameless call that needs no handler:
/// `abs()` of an int other than `PHP_INT_MIN` or of a float, `ord()` of a
/// one-byte string, `chr()` of 0..=255 (the shared one-character string,
/// `ZSTR_CHAR`, as the handler returns it), and the one-argument math
/// functions of classes `FLOAT`, `LOG` and `ROUND` (function `name`; see
/// [`crate::call::float_builtin_eval`]). `arg` already has the shape
/// [`frameless_args`] accepts for `class`; `None` for every other call.
#[inline]
fn frameless_inline(name: &[u8], class: u32, arg: Option<&Value>) -> Option<Value> {
    match (class, arg?) {
        (fl::ABS, Value::Int(i)) if *i != i64::MIN => Some(Value::Int(i.abs())),
        (fl::ABS, Value::Float(x)) => Some(Value::Float(x.abs())),
        (fl::ORD, Value::Str(s)) => s.as_bytes().first().map(|&c| Value::Int(i64::from(c))),
        (fl::CHR, Value::Int(c)) => u8::try_from(*c)
            .ok()
            .map(|c| Value::Str(ZStr::from_byte(c))),
        (fl::FLOAT | fl::LOG | fl::ROUND, v) => crate::call::float_builtin_eval(name, v),
        _ => None,
    }
}

impl Vm {
    /// `ZEND_FRAMELESS_ICALL_n` / `ZEND_DO_ICALL` of the pending call when it
    /// is a [`frameless_call`] whose arguments are all in the call's inline
    /// buffer: the call is read where it lies in the caller's `calls`
    /// (not moved out, as [`Vm::call_builtin`] takes it, three times), its
    /// arguments move into the handler's `Args`, and the rest of it is
    /// released before the handler runs, as [`Vm::call_builtin_frameless`]
    /// releases it. `abs()`, `ord()` and `chr()` of one argument need no
    /// handler at all ([`frameless_inline`]). Each step is the one the
    /// general path takes, in the same order: the handler, the interrupt
    /// check after it, the buffers back to their pools.
    ///
    /// Returns `None`, having changed nothing, for any other pending call
    /// and while observers are enabled (they see every internal call).
    /// `dst` is the call's result temporary (`pending_call_dst`).
    #[inline]
    pub(crate) fn frameless_icall_in_place(&mut self, dst: Option<u32>) -> Option<VmResult<Value>> {
        if self.observer.enabled {
            return None;
        }
        let frame = self.frames.last_mut()?;
        let call = frame.calls.last_mut()?;
        let Func::Builtin(b) = &call.func else {
            return None;
        };
        let class = (b.flags & flags::FRAMELESS) >> flags::FRAMELESS_SHIFT;
        if class == 0 || b.flags & (flags::DEPRECATED | flags::NODISCARD) != 0 {
            return None;
        }
        let n = usize::from(call.inline_count);
        let sent = call.inline_args.get(..n)?;
        if !call.args.is_empty() || !plain_call(call) || !frameless_args(class, sent) {
            return None;
        }
        let quick = if n == 1 {
            frameless_inline(b.name.as_bytes(), class, sent[0].as_ref())
        } else {
            None
        };
        let (mut values, mut passed) = if quick.is_none() {
            let nparams = b.params.len();
            let mut values = self.call_bufs.values.pop().unwrap_or_default();
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: number of arguments of the call"
            )]
            values.reserve(nparams.max(n));
            let mut passed = self.call_bufs.passed.pop().unwrap_or_default();
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: number of arguments of the call"
            )]
            passed.reserve(nparams.max(n));
            for arg in &mut call.inline_args[..n] {
                values.push(arg.take().unwrap_or_default());
                passed.push(true);
            }
            (values, passed)
        } else {
            (Vec::new(), Vec::new())
        };
        // An inline result's argument (the others moved into `values`):
        // every inline slot is empty from here on.
        let held = if quick.is_some() {
            call.inline_args[0].take()
        } else {
            None
        };
        // The call is popped and taken apart field by field rather than
        // dropped where it lies (`truncate`), whose drop glue made an
        // out-of-line `drop_in_place` call per field (~140 instructions a
        // call) for fields that hold nothing: a [`plain_call`] has no
        // `$this`, scopes, closure, `__call` name, partial application or
        // named arguments, and its inline arguments were all taken above.
        // What it owns is its function handle (moved into the handler's
        // `Args`, where the clone of it was) and its argument buffer
        // (empty: back to the pool). Nothing is leaked: the forgotten
        // fields are `None` or unallocated.
        let PendingCall {
            func,
            args: spare,
            named,
            inline_args,
            this,
            scope,
            called,
            closure,
            magic_name,
            pfa,
            ..
        } = frame.calls.pop().expect("pending call");
        debug_assert!(
            this.is_none()
                && scope.is_none()
                && called.is_none()
                && closure.is_none()
                && magic_name.is_none()
                && pfa.is_none()
                && named.is_empty()
                && spare.is_empty()
                && inline_args.iter().all(Option::is_none)
        );
        std::mem::forget((this, scope, called, closure, magic_name, pfa, inline_args));
        forget_unallocated(named);
        let spare = if spare.capacity() > 0 {
            Some(spare)
        } else {
            std::mem::forget(spare);
            None
        };
        let b = match func {
            Func::Builtin(b) => b,
            // The pending call's function was matched as a builtin above.
            Func::User(_) => unreachable!("frameless call of a user function"),
        };
        if let Some(v) = quick {
            // What the old in-place drop released: the function handle and
            // the argument (a scalar or a string: no side effect).
            drop(b);
            drop(held);
            // `fast_builtin_call`'s order: the interrupt check, then the
            // argument buffer back to its pool.
            let r = self.check_interrupt().map(|()| v);
            if let Some(args) = spare {
                self.put_arg_vec(args);
            }
            return Some(r);
        }
        self.pending_call_dst = dst;
        if self.magic_call_name.is_some() {
            self.magic_call_name = None;
        }
        // The declared defaults of omitted trailing parameters, as for a
        // framed call ([`Vm::builtin_default`]); a literal default, the
        // common case, is copied here without the out-of-line call. The
        // signature is read once (each `b.field` goes through its
        // `OnceCell`).
        let sig = b.signature();
        let (nparams, required) = (sig.params.len(), sig.required as usize);
        while values.len() < nparams {
            let i = values.len();
            if let (true, Some(Some(d))) = (i >= required, sig.defaults.get(i)) {
                values.push(crate::call::clone_value(d));
                passed.push(false);
                continue;
            }
            match self.builtin_default(&b, i) {
                Some(d) if i >= required => {
                    values.push(d);
                    passed.push(false);
                }
                _ => break,
            }
        }
        let handler = b.handler;
        let mut args = Args {
            values,
            passed,
            named: Vec::new(),
            this: None,
            called: None,
            func: Some(b),
        };
        let mut r = handler(self, &mut args);
        r = self.deliver_oom(r);
        // `ZEND_VM_FCALL_INTERRUPT_CHECK`, after the call.
        if r.is_ok() {
            if let Err(e) = self.check_interrupt() {
                r = Err(e);
            }
        }
        let Args {
            values,
            passed,
            named,
            ..
        } = args;
        forget_unallocated(named);
        self.put_val_vec(values);
        self.put_bool_vec(passed);
        if let Some(args) = spare {
            self.put_arg_vec(args);
        }
        Some(r)
    }
}

impl Vm {
    /// [`Op::FramelessICall`](crate::opcode::Op::FramelessICall) in
    /// place (`ZEND_FRAMELESS_ICALL_n`): when the call site resolved to a
    /// builtin with a frameless class (its run-time cache entry, filled by
    /// the first, generic run of the op) and the arguments (literals, and
    /// defined variables of a frame without an attached symbol table) have
    /// shapes [`frameless_args`] accepts, the handler runs on the caller's
    /// frame with the arguments read straight from their operands: no
    /// pending call, no `SEND`s, no `DO_ICALL`. Each step is the one
    /// [`Vm::frameless_icall_in_place`] takes for the same call, in the
    /// same order: the inline result or the handler with the declared
    /// defaults of omitted parameters, the interrupt check, the buffers
    /// back to their pools, the result stored in `dst`. `ip` (the op after
    /// this one) is saved first, as `SAVE_OPLINE()`.
    ///
    /// Returns `None`, having changed nothing, for any other call and
    /// while observers are enabled: the op then runs the call's ops
    /// (`INIT_FCALL`, `SEND`s, `DO_ICALL`) one after the other.
    #[inline]
    pub(crate) fn frameless_icall_op(
        &mut self,
        fc: &FramelessCall,
        ip: usize,
    ) -> Option<VmResult<()>> {
        if self.observer.enabled {
            return None;
        }
        let frame = self.frames.last_mut()?;
        if frame.symtab_attached {
            return None;
        }
        let cached = frame
            .code
            .fcall_cache
            .borrow()
            .get(fc.name as usize)
            .and_then(|c| c.as_ref().and_then(crate::opcode::CachedFunc::get));
        let Some(Func::Builtin(b)) = cached else {
            return None;
        };
        let class = (b.flags & flags::FRAMELESS) >> flags::FRAMELESS_SHIFT;
        if class == 0 || b.flags & (flags::DEPRECATED | flags::NODISCARD) != 0 {
            return None;
        }
        let n = usize::from(fc.argc).min(3);
        // What `SEND_VAL` / `SEND_VAR` would put in the call (`send_in`):
        // a literal's copy, a variable's value (a reference's target).
        let mut sent: [Option<Value>; 3] = [None, None, None];
        for (slot, operand) in sent.iter_mut().zip(&fc.vals[..n]) {
            *slot = Some(match *operand {
                Operand::Lit(l) => crate::call::clone_value(frame.code.literals.get(l as usize)?),
                Operand::Cv(cv) => match frame
                    .cvs
                    .get(cv as usize)
                    .and_then(crate::vm::CvSlot::get)?
                {
                    Value::Reference(r) => r.get(),
                    v => crate::call::clone_value(v),
                },
                _ => return None,
            });
        }
        if !frameless_args(class, &sent[..n]) {
            return None;
        }
        frame.ip = ip;
        let quick = if n == 1 {
            frameless_inline(b.name.as_bytes(), class, sent[0].as_ref())
        } else {
            None
        };
        if let Some(v) = quick {
            drop(b);
            drop(sent);
            if let Err(e) = self.check_interrupt() {
                return Some(Err(e));
            }
            if let Some(d) = fc.dst {
                self.set_tmp(d, v);
            }
            return Some(Ok(()));
        }
        self.pending_call_dst = fc.dst;
        if self.magic_call_name.is_some() {
            self.magic_call_name = None;
        }
        // The declared defaults of omitted trailing parameters, read as
        // in [`Vm::frameless_icall_in_place`].
        let sig = b.signature();
        let (nparams, required) = (sig.params.len(), sig.required as usize);
        let mut values = self.call_bufs.values.pop().unwrap_or_default();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        values.reserve(nparams.max(n));
        let mut passed = self.call_bufs.passed.pop().unwrap_or_default();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: number of arguments of the call"
        )]
        passed.reserve(nparams.max(n));
        for arg in sent.iter_mut().take(n) {
            values.push(arg.take().unwrap_or_default());
            passed.push(true);
        }
        while values.len() < nparams {
            let i = values.len();
            if let (true, Some(Some(d))) = (i >= required, sig.defaults.get(i)) {
                values.push(crate::call::clone_value(d));
                passed.push(false);
                continue;
            }
            match self.builtin_default(&b, i) {
                Some(d) if i >= required => {
                    values.push(d);
                    passed.push(false);
                }
                _ => break,
            }
        }
        let handler = b.handler;
        let mut args = Args {
            values,
            passed,
            named: Vec::new(),
            this: None,
            called: None,
            func: Some(b),
        };
        let mut r = handler(self, &mut args);
        r = self.deliver_oom(r);
        // `ZEND_VM_FCALL_INTERRUPT_CHECK`, after the call.
        if r.is_ok() {
            if let Err(e) = self.check_interrupt() {
                r = Err(e);
            }
        }
        let Args {
            values,
            passed,
            named,
            ..
        } = args;
        forget_unallocated(named);
        self.put_val_vec(values);
        self.put_bool_vec(passed);
        Some(r.map(|v| {
            if let Some(d) = fc.dst {
                self.set_tmp(d, v);
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{fl, frameless_args, frameless_call, frameless_flags, frameless_inline};
    use crate::compiler::{special_builtin_bit, SPECIAL_BUILTINS};
    use crate::func::Func;
    use crate::opcode::type_mask;
    use crate::vm::{PendingCall, Vm};
    use php_runtime::{Value, ZStr};

    #[test]
    fn frameless_calls_need_error_free_arities_and_accepted_scalars() {
        let vm = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        let call = |name: &str, args: Vec<Value>| {
            let Some(Func::Builtin(b)) = vm.functions.get(&ZStr::from(name)).cloned() else {
                panic!("{name} is a core builtin");
            };
            let mut call = PendingCall::new(Func::Builtin(b.clone()));
            call.args = args.into_iter().map(Some).collect();
            (b, call)
        };
        let frameless = |name: &str, args: Vec<Value>| {
            let (b, call) = call(name, args);
            frameless_call(&b, &call)
        };
        let s = |v: &str| Value::Str(ZStr::from(v));
        assert!(frameless("abs", vec![Value::Int(-1)]));
        assert!(frameless("abs", vec![Value::Float(-1.5)]));
        // zpp would coerce (or deprecate): the frame is needed.
        assert!(!frameless("abs", vec![s("1")]));
        assert!(!frameless("abs", vec![Value::Bool(true)]));
        assert!(!frameless("abs", vec![Value::Null]));
        assert!(frameless("ord", vec![s("a")]));
        assert!(!frameless("ord", vec![s("")]));
        assert!(!frameless("ord", vec![s("ab")]));
        assert!(frameless("chr", vec![Value::Int(65)]));
        assert!(!frameless("chr", vec![Value::Int(256)]));
        assert!(frameless("strpos", vec![s("abc"), s("b")]));
        // An offset inside the haystack cannot fail; one outside throws
        // the `ValueError` whose trace shows the frame.
        assert!(frameless("strpos", vec![s("abc"), s("b"), Value::Int(1)]));
        assert!(frameless("strpos", vec![s("abc"), s("b"), Value::Int(-3)]));
        assert!(!frameless("strpos", vec![s("abc"), s("b"), Value::Int(4)]));
        assert!(!frameless("strpos", vec![s("abc"), s("b"), Value::Int(-4)]));
        assert!(frameless("substr", vec![s("abc"), Value::Int(1)]));
        assert!(frameless(
            "substr",
            vec![s("abc"), Value::Int(1), Value::Null]
        ));
        assert!(!frameless("substr", vec![s("abc"), s("1")]));
        assert!(frameless("trim", vec![s(" a ")]));
        assert!(!frameless("trim", vec![s(" a "), s("a..b")]));
        assert!(!frameless("str_repeat", vec![s("a"), Value::Int(2)]));
        let arr = Value::Array(php_runtime::ZArray::new());
        assert!(!frameless("substr", vec![arr, Value::Int(1)]));
        let (b, mut dynamic) = call("abs", vec![Value::Int(1)]);
        dynamic.dynamic = true;
        assert!(!frameless_call(&b, &dynamic));
        let (b, mut named) = call("abs", vec![Value::Int(1)]);
        named.has_named = true;
        assert!(!frameless_call(&b, &named));
        let (b, mut ctor) = call("abs", vec![Value::Int(1)]);
        ctor.is_ctor = true;
        assert!(!frameless_call(&b, &ctor));
        let (b, mut magic) = call("abs", vec![Value::Int(1)]);
        magic.magic_name = Some(ZStr::from("abs"));
        assert!(!frameless_call(&b, &magic));
    }

    #[test]
    fn inline_frameless_results_are_the_handlers() {
        let s = |v: &str| Value::Str(ZStr::from(v));
        let int = |v: Option<Value>| match v {
            Some(Value::Int(i)) => Some(i),
            _ => None,
        };
        assert_eq!(
            int(frameless_inline(b"abs", fl::ABS, Some(&Value::Int(-7)))),
            Some(7)
        );
        assert_eq!(
            int(frameless_inline(b"abs", fl::ABS, Some(&Value::Int(7)))),
            Some(7)
        );
        // abs(PHP_INT_MIN) is a float: the handler's.
        assert!(frameless_inline(b"abs", fl::ABS, Some(&Value::Int(i64::MIN))).is_none());
        assert!(matches!(
            frameless_inline(b"abs", fl::ABS, Some(&Value::Float(-2.5))),
            Some(Value::Float(x)) if x.to_bits() == 2.5f64.to_bits()
        ));
        assert_eq!(
            int(frameless_inline(b"ord", fl::ORD, Some(&s("A")))),
            Some(65)
        );
        match frameless_inline(b"chr", fl::CHR, Some(&Value::Int(66))) {
            // `ZSTR_CHAR`: the shared one-character string.
            Some(Value::Str(z)) => assert!(ZStr::ptr_eq(&z, &ZStr::from_byte(b'B'))),
            other => panic!("chr(66) gave {other:?}"),
        }
        assert!(frameless_inline(b"chr", fl::CHR, Some(&Value::Int(256))).is_none());
        assert!(frameless_inline(b"substr", fl::SUBSTR, Some(&s("abc"))).is_none());
        assert!(frameless_inline(b"abs", fl::ABS, None).is_none());
    }

    /// The one-argument math functions run without their handlers, with
    /// the handlers' results (`/usr/bin/php -n`: `var_dump()` of each).
    #[test]
    #[allow(clippy::float_cmp)]
    fn inline_math_results_are_the_handlers() {
        let float = |name: &str, class: u32, v: Value| match frameless_inline(
            name.as_bytes(),
            class,
            Some(&v),
        ) {
            Some(Value::Float(x)) => x,
            other => panic!("{name}({v:?}) gave {other:?}"),
        };
        assert_eq!(
            float("sqrt", fl::FLOAT, Value::Int(2)),
            1.414_213_562_373_095_1
        );
        assert_eq!(
            float("sin", fl::FLOAT, Value::Int(1)),
            0.841_470_984_807_896_5
        );
        assert_eq!(float("log", fl::LOG, Value::Int(10)), 2.302_585_092_994_046);
        assert_eq!(float("round", fl::ROUND, Value::Float(2.5)), 3.0);
        assert_eq!(float("round", fl::ROUND, Value::Float(-2.5)), -3.0);
        assert_eq!(float("round", fl::ROUND, Value::Float(1.955)), 2.0);
        assert_eq!(float("round", fl::ROUND, Value::Int(7)), 7.0);
        assert_eq!(float("floor", fl::FLOAT, Value::Int(-7)), -7.0);
        assert!(float("log", fl::LOG, Value::Int(-1)).is_nan());
        assert!(matches!(
            frameless_inline(b"is_nan", fl::FLOAT, Some(&Value::Float(f64::NAN))),
            Some(Value::Bool(true))
        ));
        // Not one of the evaluated names: the handler runs.
        assert!(frameless_inline(b"sqrtx", fl::FLOAT, Some(&Value::Int(4))).is_none());
    }

    #[test]
    fn registration_stores_the_frameless_class_in_the_flags() {
        use crate::func::flags;
        let mut vm = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        vm.register_function(
            "in_array(mixed $needle, array $haystack, bool $strict = false): bool",
            |_vm, _a| Ok(Value::Bool(false)),
        );
        vm.register_function("unrelated(int $n): int", |_vm, _a| Ok(Value::Int(0)));
        let class_of = |name: &str| match vm.functions.get(&ZStr::from(name)) {
            Some(Func::Builtin(b)) => (b.flags & flags::FRAMELESS) >> flags::FRAMELESS_SHIFT,
            _ => panic!("{name} is registered"),
        };
        assert_eq!(class_of("in_array"), fl::IN_ARRAY);
        assert_eq!(class_of("abs"), fl::ABS);
        assert_eq!(class_of("strtolower"), fl::CASE);
        assert_eq!(class_of("unrelated"), 0);
        assert_eq!(class_of("strlen"), 0);
        // Distinct, non-zero classes that fit the five flag bits.
        let names: &[&[u8]] = &[
            b"abs",
            b"dechex",
            b"strtolower",
            b"trim",
            b"dirname",
            b"is_numeric",
            b"ord",
            b"chr",
            b"strpos",
            b"str_contains",
            b"max",
            b"substr",
            b"strstr",
            b"in_array",
            b"str_replace",
            b"strtr",
            b"sqrt",
            b"fmod",
            b"log",
            b"round",
            b"intdiv",
            b"pi",
        ];
        let mut seen = Vec::new();
        for name in names {
            let bits = frameless_flags(name);
            assert_ne!(bits, 0, "{}", String::from_utf8_lossy(name));
            assert_eq!(bits & !flags::FRAMELESS, 0);
            assert!(!seen.contains(&bits));
            seen.push(bits);
        }
        assert_eq!(frameless_flags(b"STRPOS"), 0);
        assert_eq!(frameless_flags(b"chop"), 0);
        assert_eq!(frameless_flags(b"implode"), 0);
        assert_eq!(frameless_flags(b"preg_match"), 0);
        assert_eq!(frameless_flags(b"ltrim"), frameless_flags(b"rtrim"));
        assert_eq!(frameless_flags(b"sqrt"), frameless_flags(b"floor"));
        assert_eq!(frameless_flags(b"pow"), 0);
        assert_eq!(frameless_flags(b"mt_rand"), 0);
        assert_eq!(frameless_flags(b"min"), frameless_flags(b"max"));
    }

    #[test]
    fn frameless_argument_shapes_exclude_every_error_path() {
        let s = |v: &str| Some(Value::Str(ZStr::from(v)));
        let i = |n: i64| Some(Value::Int(n));
        let ok = |class: u32, args: &[Option<Value>]| frameless_args(class, args);
        // trim(): character lists with a `..` range can warn.
        assert!(ok(fl::TRIM, &[s(" a ")]));
        assert!(ok(fl::TRIM, &[s("xay"), s("xy")]));
        assert!(ok(fl::TRIM, &[s("xay"), s("")]));
        assert!(ok(fl::TRIM, &[s("xay"), s("a.b")]));
        assert!(!ok(fl::TRIM, &[s("xay"), s("a..z")]));
        assert!(!ok(fl::TRIM, &[s("xay"), s("..")]));
        assert!(!ok(fl::TRIM, &[s("xay"), i(1)]));
        // dirname(): levels below 1 throw.
        assert!(ok(fl::DIRNAME, &[s("/a/b")]));
        assert!(ok(fl::DIRNAME, &[s("/a/b"), i(2)]));
        assert!(!ok(fl::DIRNAME, &[s("/a/b"), i(0)]));
        assert!(!ok(fl::DIRNAME, &[s("/a/b"), s("2")]));
        // strpos(): the offset must be inside the haystack.
        assert!(ok(fl::STRPOS, &[s(""), s(""), i(0)]));
        assert!(!ok(fl::STRPOS, &[s(""), s(""), i(1)]));
        assert!(!ok(fl::STRPOS, &[s("ab"), s("b"), i(i64::MIN)]));
        assert!(!ok(fl::STRPOS, &[s("ab"), s("b"), Some(Value::Null)]));
        // in_array(): scalar needles; loose comparisons only with
        // silent elements of a short haystack, strict ones always.
        let mut vm = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        let std_class = vm.find_class(b"stdClass").expect("stdClass");
        let instance = vm.new_object(&std_class).expect("an instance");
        let obj = || Value::Object(instance.clone());
        let arr = |vals: Vec<Value>| Some(Value::Array(php_runtime::ZArray::from_values(vals)));
        assert!(ok(
            fl::IN_ARRAY,
            &[i(2), arr(vec![Value::Int(1), Value::Int(2)])]
        ));
        assert!(ok(
            fl::IN_ARRAY,
            &[s("a"), arr(vec![Value::Array(php_runtime::ZArray::new())])]
        ));
        assert!(!ok(fl::IN_ARRAY, &[i(2), arr(vec![obj()])]));
        assert!(ok(
            fl::IN_ARRAY,
            &[i(2), arr(vec![obj()]), Some(Value::Bool(true))]
        ));
        assert!(!ok(
            fl::IN_ARRAY,
            &[i(2), arr(vec![obj()]), Some(Value::Bool(false))]
        ));
        let by_ref = Value::Reference(php_runtime::PhpRef::new(obj()));
        assert!(!ok(fl::IN_ARRAY, &[i(2), arr(vec![by_ref])]));
        let scalar_ref = Value::Reference(php_runtime::PhpRef::new(Value::Int(2)));
        assert!(ok(fl::IN_ARRAY, &[i(2), arr(vec![scalar_ref])]));
        let long = arr((0..=32).map(Value::Int).collect());
        assert!(!ok(fl::IN_ARRAY, &[i(2), long.clone()]));
        assert!(ok(fl::IN_ARRAY, &[i(2), long, Some(Value::Bool(true))]));
        assert!(!ok(fl::IN_ARRAY, &[arr(vec![]), arr(vec![])]));
        assert!(!ok(fl::IN_ARRAY, &[i(2), arr(vec![]), i(1)]));
        // str_replace(): a growing result reserves memory first.
        assert!(ok(fl::STR_REPLACE, &[s("ab"), s("c"), s("xabx")]));
        assert!(ok(fl::STR_REPLACE, &[s("ab"), s("cd"), s("xabx")]));
        assert!(!ok(fl::STR_REPLACE, &[s("a"), s("cd"), s("xax")]));
        assert!(!ok(fl::STR_REPLACE, &[s("a"), s("b"), arr(vec![])]));
        // max()/min() of two scalars; strtr() of three strings.
        assert!(ok(fl::MIN_MAX, &[i(1), s("x")]));
        assert!(!ok(fl::MIN_MAX, &[i(1), arr(vec![])]));
        assert!(!ok(fl::MIN_MAX, &[i(1), i(2), i(3)]));
        assert!(ok(fl::STRTR, &[s("abc"), s("a"), s("b")]));
        assert!(!ok(fl::STRTR, &[s("abc"), arr(vec![])]));
        // Math: ints are widened silently; log() bases, round() modes and
        // intdiv() by zero or overflowing can fail.
        let f = |x: f64| Some(Value::Float(x));
        assert!(ok(fl::FLOAT, &[i(4)]));
        assert!(ok(fl::FLOAT, &[f(-1.0)]));
        assert!(!ok(fl::FLOAT, &[s("4")]));
        assert!(!ok(fl::FLOAT, &[Some(Value::Null)]));
        assert!(!ok(fl::FLOAT, &[i(4), i(4)]));
        assert!(ok(fl::FLOAT2, &[i(4), f(2.0)]));
        assert!(!ok(fl::FLOAT2, &[i(4)]));
        assert!(ok(fl::LOG, &[i(8)]));
        assert!(!ok(fl::LOG, &[i(8), i(2)]));
        assert!(ok(fl::ROUND, &[f(1.5)]));
        assert!(ok(fl::ROUND, &[f(1.55), i(1)]));
        assert!(!ok(fl::ROUND, &[f(1.55), f(1.0)]));
        assert!(!ok(fl::ROUND, &[f(1.55), i(1), i(2)]));
        assert!(ok(fl::INTDIV, &[i(7), i(2)]));
        assert!(ok(fl::INTDIV, &[i(i64::MIN), i(1)]));
        assert!(!ok(fl::INTDIV, &[i(7), i(0)]));
        assert!(!ok(fl::INTDIV, &[i(i64::MIN), i(-1)]));
        assert!(!ok(fl::INTDIV, &[f(7.0), i(2)]));
        assert!(ok(fl::PI, &[]));
        assert!(!ok(fl::PI, &[i(1)]));
        // Unknown classes and gaps never run frameless.
        assert!(!ok(0, &[i(1)]));
        assert!(!ok(fl::ABS, &[None]));
        assert!(!ok(fl::ABS, &[]));
    }

    #[test]
    fn frameless_calls_return_their_buffers_to_the_pools() {
        let mut vm = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        let pools = |vm: &Vm| {
            let mut sizes: Vec<_> = vm.call_bufs.args.iter().map(Vec::capacity).collect();
            sizes.sort_unstable();
            (sizes, vm.call_bufs.values.len(), vm.call_bufs.passed.len())
        };
        let before = pools(&vm);
        let status = vm.run_script(
            br#"<?php
$s = 'abc';
for ($i = 0; $i < 3; $i++) {
    strpos($s, 'b'); strpos($s, 'b', 1); trim($s); trim($s, 'ac'); substr($s, 1);
    abs(-$i); str_replace('a', 'b', $s); strtr($s, 'a', 'b'); dirname('/a/b', 2);
}
"#,
            "pools.php",
        );
        assert_eq!(status, 0);
        assert_eq!(pools(&vm), before);
        assert!(vm.call_bufs.args.iter().all(Vec::is_empty));
        assert!(vm.call_bufs.values.iter().all(Vec::is_empty));
    }

    /// Frameless calls made in place (`Vm::frameless_icall_in_place`):
    /// handler calls with and without literal defaults, the inline
    /// results of `abs()`, `ord()` and `chr()`, and arguments taken from
    /// variables, literals and temporaries, which the calls release (the
    /// variables are reused afterwards). Expected output from
    /// `/usr/bin/php -n`.
    #[test]
    fn frameless_calls_in_place_match_the_handlers() {
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
        let sink = Sink::default();
        let mut vm = Vm::new(&crate::VmOptions::default(), Box::new(sink.clone()));
        let status = vm.run_script(
            br#"<?php
$s = "hello world";
for ($i = 0; $i < 2; $i++) {
    echo strpos($s, "o"), " ", strpos($s, "o", 5), " ", ord("A"), " ", abs(-3), " ",
        chr(66), " ", substr($s, 6), " ", trim("  x  "), " ", round(2.5), " ",
        in_array(2, [1, 2]) ? "y" : "n", " ", str_replace("o", "0", $s), "\n";
}
function f($t) { return ord($t) + strpos($t . "z", "z"); }
echo f("a"), " ", $s, "\n";
"#,
            "t.php",
        );
        assert_eq!(status, 0);
        drop(vm);
        let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        assert_eq!(
            out,
            "4 7 65 3 B world x 3 y hell0 w0rld\n4 7 65 3 B world x 3 y hell0 w0rld\n98 hello world\n"
        );
    }

    /// `Op::FramelessICall` (`Vm::frameless_icall_op`): calls run in
    /// place, and declined ones (a reference, a coerced or invalid
    /// argument, an undefined variable, a `ValueError`) run as the call
    /// ops they replaced, with the builtin's frame in the trace. Expected
    /// output from `/usr/bin/php -n`.
    #[test]
    fn fused_frameless_calls_match_the_call_ops() {
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
        let run = |src: &str| {
            let sink = Sink::default();
            let mut vm = Vm::new(&crate::VmOptions::default(), Box::new(sink.clone()));
            let status = vm.run_script(src.as_bytes(), "t.php");
            assert_eq!(status, 0);
            drop(vm);
            let bytes = sink.0.borrow().clone();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let out = run(r#"<?php
function t($s, $n) {
    $r = [];
    $r[] = strpos($s, "o");
    $r[] = strpos($s, "o", 5);
    $r[] = substr($s, 6);
    $r[] = substr($s, 1, 3);
    $r[] = str_contains($s, "wor") ? "y" : "n";
    $r[] = trim("  x  ");
    $r[] = strtoupper($s);
    $r[] = in_array($n, [1, 2, 3]) ? "y" : "n";
    $r[] = max($n, 2);
    $r[] = str_replace("o", "0", $s);
    $ref = &$s;
    $r[] = strpos($s, "w");
    $r[] = strpos($n, "2");
    $r[] = var_export(@strpos($undef, "x"), true);
    echo implode(",", $r), "\n";
}
t("hello world", 2);
t("hello world", 2);
try { strpos([], "x"); } catch (TypeError $e) { echo $e->getMessage(), " ", $e->getLine(), "\n", $e->getTraceAsString(), "\n"; }
try { substr("abc", "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
try { strpos("abc", "c", 9); } catch (ValueError $e) { echo $e->getMessage(), " ", $e->getLine(), "\n"; }
"#);
        assert_eq!(
            out,
            "4,7,world,ell,y,x,HELLO WORLD,y,2,hell0 w0rld,6,0,false\n\
             4,7,world,ell,y,x,HELLO WORLD,y,2,hell0 w0rld,6,0,false\n\
             strpos(): Argument #1 ($haystack) must be of type string, array given 22\n\
             #0 t.php(22): strpos(Array, 'x')\n\
             #1 {main}\n\
             substr(): Argument #2 ($offset) must be of type int, string given\n\
             strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack) 24\n"
        );
        let out = run(r#"<?php declare(strict_types=1);
$s = "abc"; $n = 5;
echo strpos($s, "b"), "\n";
try { strpos($n, "5"); } catch (TypeError $e) { echo $e->getMessage(), " ", $e->getLine(), "\n"; }
"#);
        assert_eq!(
            out,
            "1\nstrpos(): Argument #1 ($haystack) must be of type string, int given 4\n"
        );
    }

    #[test]
    fn special_builtin_bits_are_distinct_and_lowercase() {
        let mut seen = 0u32;
        for name in SPECIAL_BUILTINS {
            assert_eq!(name.to_ascii_lowercase(), *name);
            let bit = special_builtin_bit(name.as_bytes()).expect("listed");
            assert_eq!(seen & bit, 0, "{name}");
            seen |= bit;
        }
        assert_eq!(special_builtin_bit(b"is_integer"), None);
        assert_eq!(special_builtin_bit(b"doubleval"), None);
        assert_eq!(special_builtin_bit(b"STRLEN"), None);
    }

    #[test]
    fn type_mask_matches_like_zend_type_check() {
        let s = Value::Str(ZStr::from("x"));
        assert!(type_mask::matches(type_mask::STRING, &s));
        assert!(type_mask::matches(type_mask::SCALAR, &s));
        assert!(!type_mask::matches(type_mask::ARRAY, &s));
        assert!(type_mask::matches(type_mask::NULL, &Value::Null));
        assert!(!type_mask::matches(type_mask::SCALAR, &Value::Null));
        assert!(type_mask::matches(type_mask::BOOL, &Value::Bool(false)));
        assert!(type_mask::matches(type_mask::INT, &Value::Int(0)));
        assert!(!type_mask::matches(type_mask::FLOAT, &Value::Int(0)));
        assert!(type_mask::matches(type_mask::FLOAT, &Value::Float(0.5)));
        let r = Value::Reference(php_runtime::PhpRef::new(Value::Int(3)));
        assert!(type_mask::matches(type_mask::INT, &r));
        assert!(!type_mask::matches(type_mask::STRING, &r));
    }
}
