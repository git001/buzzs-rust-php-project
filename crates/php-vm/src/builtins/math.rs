// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Math functions (`ext/standard/math.c`) and random numbers: fallback
//! bindings for the bare VM. The `php` binary links `php-ext-stdstr`, which
//! re-registers the math.c functions (all but `pow`) with full semantics.

use super::util::{f, i, opt};
use super::{constant, reg};
use crate::func::Args;
use crate::vm::{Vm, VmResult};
use php_runtime::Value;

fn num_arg(vm: &mut Vm, a: &Args, n: usize) -> VmResult<Value> {
    let v = a.at(n);
    match v {
        Value::Int(_) | Value::Float(_) => Ok(v),
        other => {
            let r = php_runtime::convert::try_convert_to_number(&other, vm)
                .map_err(|e| vm.rt_throw(e))?;
            Ok(r.unwrap_or(Value::Int(0)))
        }
    }
}

fn abs(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match num_arg(vm, a, 0)? {
        Value::Int(n) => {
            if n == i64::MIN {
                #[allow(clippy::cast_precision_loss)]
                let f = -(n as f64);
                Value::Float(f)
            } else {
                Value::Int(n.abs())
            }
        }
        Value::Float(x) => Value::Float(x.abs()),
        other => other,
    })
}

fn floor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match num_arg(vm, a, 0)? {
        #[allow(clippy::cast_precision_loss)]
        Value::Int(n) => Value::Float(n as f64),
        Value::Float(x) => Value::Float(x.floor()),
        other => other,
    })
}

fn ceil(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match num_arg(vm, a, 0)? {
        #[allow(clippy::cast_precision_loss)]
        Value::Int(n) => Value::Float(n as f64),
        Value::Float(x) => Value::Float(x.ceil()),
        other => other,
    })
}

fn round(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let v = num_arg(vm, a, 0)?;
    let places = i(a, 1);
    let mode_v = if a.has(2) { i(a, 2) } else { 1 };
    let mode = match mode_v {
        2 => php_support::fmt::RoundingMode::HalfDown,
        3 => php_support::fmt::RoundingMode::HalfEven,
        4 => php_support::fmt::RoundingMode::HalfOdd,
        _ => php_support::fmt::RoundingMode::HalfUp,
    };
    let places = i32::try_from(places.clamp(-1000, 1000)).unwrap_or(0);
    Ok(match v {
        Value::Int(n) => {
            if places >= 0 {
                #[allow(clippy::cast_precision_loss)]
                let r = n as f64;
                Value::Float(r)
            } else {
                #[allow(clippy::cast_precision_loss)]
                Value::Float(php_support::fmt::php_round(n as f64, places, mode))
            }
        }
        Value::Float(x) => Value::Float(php_support::fmt::php_round(x, places, mode)),
        other => other,
    })
}

macro_rules! float_fn {
    ($name:ident, $e:expr) => {
        fn $name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let x = f(a, 0);
            let g: fn(f64) -> f64 = $e;
            Ok(Value::Float(g(x)))
        }
    };
}

float_fn!(sqrt, f64::sqrt);
float_fn!(sin, f64::sin);
float_fn!(cos, f64::cos);
float_fn!(tan, f64::tan);
float_fn!(asin, f64::asin);
float_fn!(acos, f64::acos);
float_fn!(atan, f64::atan);
float_fn!(sinh, f64::sinh);
float_fn!(cosh, f64::cosh);
float_fn!(tanh, f64::tanh);
float_fn!(asinh, f64::asinh);
float_fn!(acosh, f64::acosh);
float_fn!(atanh, f64::atanh);
float_fn!(exp, f64::exp);
float_fn!(expm1, f64::exp_m1);
float_fn!(log10, f64::log10);
float_fn!(log1p, f64::ln_1p);
float_fn!(deg2rad, f64::to_radians);
float_fn!(rad2deg, f64::to_degrees);

fn atan2(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(f(a, 0).atan2(f(a, 1))))
}

fn log(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let x = f(a, 0);
    if let Some(b) = opt(a, 1) {
        let base = vm.to_double(&b)?;
        if base <= 0.0 {
            return Err(super::util::value_error(vm, a, 2, "must be greater than 0"));
        }
        if (base - 1.0).abs() < f64::EPSILON {
            return Ok(Value::Float(f64::NAN));
        }
        return Ok(Value::Float(x.ln() / base.ln()));
    }
    Ok(Value::Float(x.ln()))
}

fn pi(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(std::f64::consts::PI))
}

fn fmod(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(php_runtime::ops::fmod(f(a, 0), f(a, 1))))
}

fn fdiv(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(f(a, 0) / f(a, 1)))
}

fn intdiv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = php_runtime::ops::intdiv(i(a, 0), i(a, 1));
    r.map(Value::Int).map_err(|e| vm.rt_throw(e))
}

fn pow(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let x = a.at(0);
    let y = a.at(1);
    php_runtime::ops::pow(&x, &y, vm).map_err(|e| vm.rt_throw(e))
}

fn hypot(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(f(a, 0).hypot(f(a, 1))))
}

fn is_nan(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(f(a, 0).is_nan()))
}
fn is_finite(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(f(a, 0).is_finite()))
}
fn is_infinite(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(f(a, 0).is_infinite()))
}

fn mt_srand(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let seed = opt(a, 0).map(|_| i(a, 0));
    let mode = i(a, 1);
    let w = vm.random_globals().mt_srand(seed, mode);
    if let Some(w) = w {
        vm.deprecated(w)?;
    }
    Ok(Value::Null)
}

fn mt_rand(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if !a.has(0) {
        return Ok(Value::Int(vm.random_globals().mt_rand()));
    }
    if !a.has(1) {
        return Err(vm.make_throw(
            "ArgumentCountError",
            "mt_rand() expects exactly 2 arguments, 1 given",
        ));
    }
    let (min, max) = (i(a, 0), i(a, 1));
    match vm.random_globals().mt_rand_range(min, max) {
        Ok(v) => Ok(Value::Int(v)),
        Err(e) => Err(vm.make_throw("ValueError", &e.to_string())),
    }
}

fn rand(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if !a.has(0) {
        return Ok(Value::Int(vm.random_globals().mt_rand()));
    }
    if !a.has(1) {
        return Err(vm.make_throw(
            "ArgumentCountError",
            "rand() expects exactly 2 arguments, 1 given",
        ));
    }
    let (min, max) = (i(a, 0), i(a, 1));
    Ok(Value::Int(vm.random_globals().rand_range(min, max)))
}

fn mt_getrandmax(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(2_147_483_647))
}

fn random_int(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (min, max) = (i(a, 0), i(a, 1));
    if min > max {
        return Err(super::util::value_error(
            vm,
            a,
            1,
            "must be less than or equal to argument #2 ($max)",
        ));
    }
    match php_random::csprng::random_int(min, max) {
        Ok(v) => Ok(Value::Int(v)),
        Err(e) => Err(vm.make_throw("Exception", &e.to_string())),
    }
}

fn random_bytes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = i(a, 0);
    if n < 1 {
        return Err(super::util::value_error(vm, a, 1, "must be greater than 0"));
    }
    match php_random::csprng::random_bytes(n) {
        Ok(buf) => Ok(Value::Str(php_runtime::ZStr::from(buf))),
        Err(e) => Err(vm.make_throw("Exception", &e.to_string())),
    }
}

/// Registers the math functions only the VM provides.
pub(crate) fn register(vm: &mut Vm) {
    reg(
        vm,
        "pow(mixed $num, mixed $exponent): object|int|float",
        pow,
    );
}

/// Registers the bare VM's math and random functions and constants
/// (`Vm::new`): the SAPIs get them from php-ext-stdstr and php-ext-random.
pub(crate) fn register_bare(vm: &mut Vm) {
    reg(vm, "abs(int|float $num): int|float", abs);
    reg(vm, "floor(int|float $num): float", floor);
    reg(vm, "ceil(int|float $num): float", ceil);
    reg(
        vm,
        "round(int|float $num, int $precision = 0, $mode = PHP_ROUND_HALF_UP): float",
        round,
    );
    reg(vm, "sqrt(float $num): float", sqrt);
    reg(vm, "sin(float $num): float", sin);
    reg(vm, "cos(float $num): float", cos);
    reg(vm, "tan(float $num): float", tan);
    reg(vm, "asin(float $num): float", asin);
    reg(vm, "acos(float $num): float", acos);
    reg(vm, "atan(float $num): float", atan);
    reg(vm, "atan2(float $y, float $x): float", atan2);
    reg(vm, "sinh(float $num): float", sinh);
    reg(vm, "cosh(float $num): float", cosh);
    reg(vm, "tanh(float $num): float", tanh);
    reg(vm, "asinh(float $num): float", asinh);
    reg(vm, "acosh(float $num): float", acosh);
    reg(vm, "atanh(float $num): float", atanh);
    reg(vm, "exp(float $num): float", exp);
    reg(vm, "expm1(float $num): float", expm1);
    reg(vm, "log(float $num, float $base = M_E): float", log);
    reg(vm, "log10(float $num): float", log10);
    reg(vm, "log1p(float $num): float", log1p);
    reg(vm, "deg2rad(float $num): float", deg2rad);
    reg(vm, "rad2deg(float $num): float", rad2deg);
    reg(vm, "pi(): float", pi);
    reg(vm, "fmod(float $num1, float $num2): float", fmod);
    reg(vm, "fdiv(float $num1, float $num2): float", fdiv);
    reg(vm, "intdiv(int $num1, int $num2): int", intdiv);
    reg(vm, "hypot(float $x, float $y): float", hypot);
    reg(vm, "is_nan(float $num): bool", is_nan);
    reg(vm, "is_finite(float $num): bool", is_finite);
    reg(vm, "is_infinite(float $num): bool", is_infinite);
    reg(
        vm,
        "mt_srand(?int $seed = null, int $mode = MT_RAND_MT19937): void",
        mt_srand,
    );
    reg(
        vm,
        "srand(?int $seed = null, int $mode = MT_RAND_MT19937): void",
        mt_srand,
    );
    reg(
        vm,
        "mt_rand(int $min = UNKNOWN, int $max = UNKNOWN): int",
        mt_rand,
    );
    reg(
        vm,
        "rand(int $min = UNKNOWN, int $max = UNKNOWN): int",
        rand,
    );
    reg(vm, "mt_getrandmax(): int", mt_getrandmax);
    reg(vm, "getrandmax(): int", mt_getrandmax);
    reg(vm, "random_int(int $min, int $max): int", random_int);
    reg(vm, "random_bytes(int $length): string", random_bytes);
    for (n, v) in [
        ("M_PI", std::f64::consts::PI),
        ("M_E", std::f64::consts::E),
        ("M_LOG2E", std::f64::consts::LOG2_E),
        ("M_LOG10E", std::f64::consts::LOG10_E),
        ("M_LN2", std::f64::consts::LN_2),
        ("M_LN10", std::f64::consts::LN_10),
        ("M_PI_2", std::f64::consts::FRAC_PI_2),
        ("M_PI_4", std::f64::consts::FRAC_PI_4),
        ("M_1_PI", std::f64::consts::FRAC_1_PI),
        ("M_2_PI", std::f64::consts::FRAC_2_PI),
        ("M_SQRTPI", 1.772_453_850_905_516),
        ("M_2_SQRTPI", std::f64::consts::FRAC_2_SQRT_PI),
        ("M_SQRT2", std::f64::consts::SQRT_2),
        ("M_SQRT3", 1.732_050_807_568_877_2),
        ("M_SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ("M_LNPI", 1.144_729_885_849_400_2),
        ("M_EULER", 0.577_215_664_901_532_9),
    ] {
        constant(vm, n, Value::Float(v));
    }
    for (n, v) in [
        ("PHP_ROUND_HALF_UP", 1),
        ("PHP_ROUND_HALF_DOWN", 2),
        ("PHP_ROUND_HALF_EVEN", 3),
        ("PHP_ROUND_HALF_ODD", 4),
        ("MT_RAND_MT19937", 0),
        ("MT_RAND_PHP", 1),
    ] {
        constant(vm, n, Value::Int(v));
    }
}
