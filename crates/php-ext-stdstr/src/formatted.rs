// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/formatted_print.c`: `sprintf()`, `printf()`,
//! `vsprintf()`, `vprintf()` over `php_support::fmt::php_formatted_print`.
//!
//! PHP converts each argument only when a conversion consumes it
//! (`zval_get_long()` for `%d`, `zval_get_tmp_string()` for `%s`, ...), and
//! those conversions can warn ("Array to string conversion", "The float
//! ... is not representable as an int") or call `__toString()`. The
//! formatting engine has no access to the VM, so formatting runs twice: a
//! first pass with side-effect-free placeholder values records which
//! conversions happen in which order; the VM then performs exactly those
//! conversions, and the second pass formats with the real values.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use php_runtime::{Value, ZStr};
use php_support::fmt::{php_formatted_print, ArgSource, FormatArg, FormatError, FormatOptions};
use php_support::Diagnostic;
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::{bytes, to_str};

/// The conversion a format directive applies to its argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Float,
    Str,
}

/// First-pass argument: records conversions, returns placeholders.
struct Recorder<'a> {
    idx: usize,
    exact: Option<i64>,
    log: &'a RefCell<Vec<(usize, Kind)>>,
}

impl Recorder<'_> {
    fn note(&self, k: Kind) {
        self.log.borrow_mut().push((self.idx, k));
    }
}

impl FormatArg for Recorder<'_> {
    fn as_int(&self) -> i64 {
        self.note(Kind::Int);
        self.exact.unwrap_or(0)
    }
    fn as_float(&self) -> f64 {
        self.note(Kind::Float);
        0.0
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        self.note(Kind::Str);
        Cow::Borrowed(b"")
    }
    fn exact_int(&self) -> Option<i64> {
        self.exact
    }
}

/// Second-pass argument with the VM-converted values.
#[derive(Default)]
struct Converted {
    int: i64,
    float: f64,
    string: Option<ZStr>,
    exact: Option<i64>,
}

impl FormatArg for Converted {
    fn as_int(&self) -> i64 {
        self.int
    }
    fn as_float(&self) -> f64 {
        self.float
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.string.as_ref().map_or(&[][..], ZStr::as_bytes))
    }
    fn exact_int(&self) -> Option<i64> {
        self.exact
    }
}

/// Single-pass argument for values whose conversions have no side
/// effects and need no VM state (integers, booleans, null, strings used as
/// strings, floats used as floats). Any other conversion sets `bail`, and
/// the caller discards the result and takes the two-pass path.
struct Plain<'a> {
    v: &'a Value,
    bail: &'a Cell<bool>,
}

impl Plain<'_> {
    fn give_up<T: Default>(&self) -> T {
        self.bail.set(true);
        T::default()
    }
}

impl FormatArg for Plain<'_> {
    fn as_int(&self) -> i64 {
        match self.v {
            Value::Int(i) => *i,
            Value::Bool(b) => i64::from(*b),
            Value::Null => 0,
            _ => self.give_up(),
        }
    }
    #[allow(clippy::cast_precision_loss)]
    fn as_float(&self) -> f64 {
        match self.v {
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Null => 0.0,
            _ => self.give_up(),
        }
    }
    fn as_str(&self) -> Cow<'_, [u8]> {
        match self.v {
            Value::Str(s) => Cow::Borrowed(s.as_bytes()),
            Value::Int(i) => Cow::Owned(i.to_string().into_bytes()),
            Value::Bool(true) => Cow::Borrowed(b"1"),
            Value::Bool(false) | Value::Null => Cow::Borrowed(b""),
            _ => {
                self.bail.set(true);
                Cow::Borrowed(b"")
            }
        }
    }
    fn exact_int(&self) -> Option<i64> {
        match self.v {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
}

/// Maps the engine's result to the VM: diagnostics first, then the
/// thrown error, as `php_formatted_print()` does.
fn finish(
    vm: &mut Vm,
    r: Result<Vec<u8>, FormatError>,
    diags: Vec<Diagnostic>,
) -> VmResult<Vec<u8>> {
    emit_diags(vm, diags)?;
    match r {
        Ok(v) => Ok(v),
        Err(FormatError::ValueError(m)) => Err(vm.throw("ValueError", &m)),
        Err(FormatError::ArgumentCountError(m)) => Err(vm.throw("ArgumentCountError", &m)),
        Err(FormatError::Fatal(m)) => Err(vm.fatal(&m)),
        Err(FormatError::MemoryLimit(over)) => Err(over.into()),
    }
}

fn emit_diags(vm: &mut Vm, diags: Vec<Diagnostic>) -> VmResult<()> {
    for d in diags {
        let level = i64::from(d.level.0);
        if d.docref {
            match level {
                crate::E_WARNING => vm.php_warning(&d.message)?,
                crate::E_NOTICE => vm.php_notice(&d.message)?,
                _ => vm.php_deprecated(&d.message)?,
            }
        } else {
            vm.raise(level, &d.message)?;
        }
    }
    Ok(())
}

/// `php_formatted_print()` with lazy, VM-performed argument conversions:
/// the engine behind `sprintf()` and friends, exported for `fprintf()` /
/// `vfprintf()` in the stream binding (`source` is
/// `ArgSource::Variadic(2)` / `ArgSource::Array` there).
///
/// # Errors
///
/// Throws `ValueError`/`ArgumentCountError` (or a fatal error) exactly
/// like PHP, and propagates exceptions from argument conversions.
pub fn format(vm: &mut Vm, fmt: &[u8], values: &[Value], source: ArgSource) -> VmResult<Vec<u8>> {
    let opts = FormatOptions {
        source,
        decimal_point: php_support::fmt::locale::decimal_point(),
    };
    // Fast path: one formatting pass when no conversion can have a side
    // effect (the common `%d`/`%s`/`%.2f` over ints, strings and floats).
    if !values.iter().any(|v| matches!(v, Value::Reference(_))) {
        let bail = Cell::new(false);
        let plain: Vec<Plain<'_>> = values.iter().map(|v| Plain { v, bail: &bail }).collect();
        let refs: Vec<&dyn FormatArg> = plain.iter().map(|a| a as &dyn FormatArg).collect();
        let mut diags: Vec<Diagnostic> = Vec::new();
        let r = php_formatted_print(fmt, &refs, opts, &mut diags);
        if !bail.get() {
            return finish(vm, r, diags);
        }
    }
    let values: Vec<Value> = values.iter().map(Value::deref).collect();
    let exact = |v: &Value| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    };
    let log = RefCell::new(Vec::new());
    {
        let rec: Vec<Recorder<'_>> = values
            .iter()
            .enumerate()
            .map(|(idx, v)| Recorder {
                idx,
                exact: exact(v),
                log: &log,
            })
            .collect();
        let refs: Vec<&dyn FormatArg> = rec.iter().map(|a| a as &dyn FormatArg).collect();
        let mut sink: Vec<Diagnostic> = Vec::new();
        // Errors surface again (identically) in the second pass.
        let _ = php_formatted_print(fmt, &refs, opts, &mut sink);
    }
    let mut conv: Vec<Converted> = values
        .iter()
        .map(|v| Converted {
            exact: exact(v),
            ..Converted::default()
        })
        .collect();
    for (idx, kind) in log.into_inner() {
        let v = &values[idx];
        match kind {
            Kind::Int => conv[idx].int = vm.value_to_int(v)?,
            Kind::Float => conv[idx].float = vm.value_to_float(v)?,
            Kind::Str => conv[idx].string = Some(to_str(vm, v)?),
        }
    }
    let refs: Vec<&dyn FormatArg> = conv.iter().map(|a| a as &dyn FormatArg).collect();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let r = php_formatted_print(fmt, &refs, opts, &mut diags);
    finish(vm, r, diags)
}

fn array_values(a: &Args, n: usize) -> Vec<Value> {
    a.array(n).iter().map(|(_, v)| v.clone()).collect()
}

fn len_value(out: &[u8]) -> Value {
    Value::Int(i64::try_from(out.len()).unwrap_or(i64::MAX))
}

fn sprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = a.str(0);
    let out = format(
        vm,
        f.as_bytes(),
        a.values.get(1..).unwrap_or(&[]),
        ArgSource::Variadic(1),
    )?;
    Ok(bytes(out))
}

fn printf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = a.str(0);
    let out = format(
        vm,
        f.as_bytes(),
        a.values.get(1..).unwrap_or(&[]),
        ArgSource::Variadic(1),
    )?;
    vm.output(&out);
    Ok(len_value(&out))
}

fn vsprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = a.str(0);
    let out = format(vm, f.as_bytes(), &array_values(a, 1), ArgSource::Array)?;
    Ok(bytes(out))
}

fn vprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let f = a.str(0);
    let out = format(vm, f.as_bytes(), &array_values(a, 1), ArgSource::Array)?;
    vm.output(&out);
    Ok(len_value(&out))
}

/// Registers the `formatted_print.c` functions (`fprintf`/`vfprintf`
/// belong to the stream binding).
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("sprintf(string $format, mixed ...$values): string", sprintf)
        .function_static("printf(string $format, mixed ...$values): int", printf)
        .function_static("vprintf(string $format, array $values): int", vprintf)
        .function_static("vsprintf(string $format, array $values): string", vsprintf);
}
