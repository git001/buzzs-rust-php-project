// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/bcmath` for the phprust VM: binds the `php-bcmath` library (the
//! procedural `bc*()` functions and `BcMath\Number`) through the
//! `php_vm::ext` extension API.
//!
//! The `bcmath.scale` INI entry is the single source of truth for the
//! default scale: every call reads it, and `bcscale()` alters it like
//! `zend_alter_ini_entry()`.
//!
//! The extension also declares the `RoundingMode` enum (ext/standard in
//! php-src master) when no other extension has done so.

// Native handlers share one signature even when they cannot fail.
#![allow(clippy::unnecessary_wraps)]

mod number;

use php_bcmath::{BcError, BcMath, ErrorKind, RoundingMode};
use php_runtime::{Obj, Value, ZArray, ZStr};
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{Args, ClassBuilder, Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

/// The `bcmath` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bcmath;

/// `E_DEPRECATED`.
pub(crate) const E_DEPRECATED: i64 = 8192;

/// Converts a library error into the PHP exception (or the fatal
/// out-of-memory error).
pub(crate) fn throw(vm: &mut Vm, e: &BcError) -> Throw {
    if e.kind == ErrorKind::OutOfMemory {
        let tried = e
            .message
            .rsplit("allocate ")
            .next()
            .and_then(|s| s.split(' ').next())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        return out_of_memory(vm, tried);
    }
    vm.throw(e.kind.class_name(), &e.message)
}

/// The `emalloc(bytes)` of a `bc_num` that cannot fit (bcmath's
/// allocations are not fallible; GH-17398 for the hard ceiling).
#[cold]
fn out_of_memory(vm: &mut Vm, bytes: usize) -> Throw {
    match php_memlimit::emalloc_check(bytes) {
        Err(over) => Throw::from(over),
        Ok(()) => vm.out_of_memory(bytes),
    }
}

/// Maps a library result to a VM result.
pub(crate) fn lift<T>(vm: &mut Vm, r: Result<T, BcError>) -> VmResult<T> {
    r.map_err(|e| throw(vm, &e))
}

/// The library state for the current `bcmath.scale` value.
fn state(vm: &Vm) -> BcMath {
    let mut bc = BcMath::new();
    bc.set_ini_scale(vm.ini_long("bcmath.scale"));
    bc
}

/// A `?int $scale = null` argument.
pub(crate) fn scale_arg(a: &Args, n: usize) -> Option<i64> {
    match a.opt(n) {
        Some(Value::Int(i)) => Some(i),
        _ => None,
    }
}

fn str_value(s: &str) -> Value {
    Value::Str(ZStr::from(s))
}

/// Reads a `RoundingMode` enum case argument (default `HalfAwayFromZero`).
pub(crate) fn rounding_mode(vm: &mut Vm, v: Option<Value>) -> VmResult<RoundingMode> {
    let Some(Value::Object(o)) = v else {
        return Ok(RoundingMode::HalfAwayFromZero);
    };
    let name = vm.get_property(&o, "name")?;
    let name = vm.value_to_string(&name)?;
    Ok(RoundingMode::from_name(&name.to_string_lossy()).unwrap_or(RoundingMode::HalfAwayFromZero))
}

type Binary = fn(&BcMath, &[u8], &[u8], Option<i64>) -> Result<String, BcError>;

fn binary(vm: &mut Vm, a: &Args, f: Binary) -> VmResult<Value> {
    let bc = state(vm);
    let r = f(
        &bc,
        a.str(0).as_bytes(),
        a.str(1).as_bytes(),
        scale_arg(a, 2),
    );
    lift(vm, r).map(|s| str_value(&s))
}

fn bcadd(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcadd)
}

fn bcsub(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcsub)
}

fn bcmul(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcmul)
}

fn bcdiv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcdiv)
}

fn bcmod(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcmod)
}

fn bcpow(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    binary(vm, a, BcMath::bcpow)
}

fn bcdivmod(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let bc = state(vm);
    let r = bc.bcdivmod(a.str(0).as_bytes(), a.str(1).as_bytes(), scale_arg(a, 2));
    let (q, r) = lift(vm, r)?;
    Ok(Value::Array(ZArray::from_values([
        str_value(&q),
        str_value(&r),
    ])))
}

fn bcpowmod(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let bc = state(vm);
    let r = bc.bcpowmod(
        a.str(0).as_bytes(),
        a.str(1).as_bytes(),
        a.str(2).as_bytes(),
        scale_arg(a, 3),
    );
    lift(vm, r).map(|s| str_value(&s))
}

fn bcsqrt(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let bc = state(vm);
    let r = bc.bcsqrt(a.str(0).as_bytes(), scale_arg(a, 1));
    lift(vm, r).map(|s| str_value(&s))
}

fn bccomp(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let bc = state(vm);
    let r = bc.bccomp(a.str(0).as_bytes(), a.str(1).as_bytes(), scale_arg(a, 2));
    lift(vm, r).map(Value::Int)
}

fn bcscale(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut bc = state(vm);
    let new = scale_arg(a, 0);
    let old = lift(vm, bc.bcscale(new))?;
    if let Some(s) = new {
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        vm.ini.alter(
            "bcmath.scale",
            s.to_string().as_bytes(),
            Access::USER,
            php_support::ini::Stage::Runtime,
            false,
            &mut sink,
        );
    }
    Ok(Value::Int(old))
}

fn bcfloor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = state(vm).bcfloor(a.str(0).as_bytes());
    lift(vm, r).map(|s| str_value(&s))
}

fn bcceil(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = state(vm).bcceil(a.str(0).as_bytes());
    lift(vm, r).map(|s| str_value(&s))
}

fn bcround(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let precision = if a.has(1) { a.int(1) } else { 0 };
    let mode = rounding_mode(vm, a.opt(2))?;
    let r = state(vm).bcround(a.str(0).as_bytes(), precision, mode);
    lift(vm, r).map(|s| str_value(&s))
}

/// Declares the internal `enum RoundingMode` (ext/standard's
/// `math.stub.php` in php-src master) unless another extension already has.
fn register_rounding_mode(vm: &mut Vm) {
    if vm.find_class(b"RoundingMode").is_some() {
        return;
    }
    let mut b = ClassBuilder::new("RoundingMode").unit_enum();
    for m in RoundingMode::ALL {
        b = b.case(m.name(), None);
    }
    b.register(vm);
}

/// Whether `o` is a `BcMath\Number`.
pub(crate) fn is_number(o: &Obj) -> bool {
    php_vm::object::has_internal::<number::State>(o)
}

impl Extension for Bcmath {
    fn name(&self) -> &'static str {
        "bcmath"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        register_rounding_mode(m.vm());
        m.ini(IniDirective::new(
            "bcmath.scale",
            Some("0"),
            Access::ALL,
            ModifyHandler::LongGEZero,
            "bcmath",
        ));
        m.function_static(
            "bcadd(string $num1, string $num2, ?int $scale = null): string",
            bcadd,
        )
        .function_static(
            "bcsub(string $num1, string $num2, ?int $scale = null): string",
            bcsub,
        )
        .function_static(
            "bcmul(string $num1, string $num2, ?int $scale = null): string",
            bcmul,
        )
        .function_static(
            "bcdiv(string $num1, string $num2, ?int $scale = null): string",
            bcdiv,
        )
        .function_static(
            "bcmod(string $num1, string $num2, ?int $scale = null): string",
            bcmod,
        )
        .function_static(
            "bcdivmod(string $num1, string $num2, ?int $scale = null): array",
            bcdivmod,
        )
        .function_static(
            "bcpowmod(string $num, string $exponent, string $modulus, ?int $scale = null): string",
            bcpowmod,
        )
        .function_static(
            "bcpow(string $num, string $exponent, ?int $scale = null): string",
            bcpow,
        )
        .function_static("bcsqrt(string $num, ?int $scale = null): string", bcsqrt)
        .function_static(
            "bccomp(string $num1, string $num2, ?int $scale = null): int",
            bccomp,
        )
        .function_static("bcscale(?int $scale = null): int", bcscale)
        .function_static("bcfloor(string $num): string", bcfloor)
        .function_static("bcceil(string $num): string", bcceil)
        .function_static(
            "bcround(string $num, int $precision = 0, RoundingMode $mode = RoundingMode::HalfAwayFromZero): string",
            bcround,
        );
        number::register(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_arg_ignores_null() {
        let a = Args {
            values: vec![Value::Null, Value::Int(3)],
            passed: vec![true, true],
            named: Vec::new(),
            this: None,
            called: None,
            func: None,
        };
        assert_eq!(scale_arg(&a, 0), None);
        assert_eq!(scale_arg(&a, 1), Some(3));
        assert_eq!(scale_arg(&a, 2), None);
    }
}
