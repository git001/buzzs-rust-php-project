// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of every php-src `ext/bcmath/tests/**/*.phpt` script.
//!
//! The original test files are vendored under `tests/phpt/` (read-only
//! copies). Each script is transcribed to Rust (`tests/phpt_port/`) on top of
//! a tiny emulation of the PHP features it uses, and its output is compared
//! with the original `--EXPECT--`/`--EXPECTF--` section. Scripts that only
//! exercise engine behaviour (class inheritance, casts to int, dynamic
//! properties, memory limits, 32-bit builds) are listed in [`ENGINE_ONLY`].

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::too_many_lines)]

#[path = "phpt_port/functions.rs"]
mod functions;
#[path = "phpt_port/harness.rs"]
mod harness;
#[path = "phpt_port/number.rs"]
mod number;
#[path = "phpt_port/runner.rs"]
mod runner;

use std::path::Path;

use harness::Php;
use runner::Phpt;

type Port = fn(&mut Php, &Phpt);

const PORTED: &[(&str, Port)] = &[
    ("GHSA-x692-q9x7-8c3f.phpt", functions::ghsa_x692),
    (
        "bc_is_zero_for_scale_clamp.phpt",
        functions::bc_is_zero_for_scale_clamp,
    ),
    ("bcadd.phpt", functions::run_bcmath_tests),
    ("bcadd_error.phpt", functions::bcadd_error),
    ("bcadd_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcadd_zero.phpt", functions::run_bcmath_tests),
    ("bcceil.phpt", functions::bcfloor_ceil),
    ("bcceil_error.phpt", functions::bcfloor_ceil_error),
    ("bccomp.phpt", functions::bccomp),
    ("bccomp_error.phpt", functions::bccomp_error),
    ("bccomp_variation001.phpt", functions::bccomp_variations),
    ("bccomp_variation002.phpt", functions::bccomp_variations),
    ("bcdiv.phpt", functions::run_bcmath_tests),
    ("bcdiv_by_one.phpt", functions::bcdiv_by_one),
    ("bcdiv_by_pow_10.phpt", functions::bcdiv_by_pow_10),
    ("bcdiv_error1.phpt", functions::bcdiv_error1),
    ("bcdiv_error2.phpt", functions::bcdiv_error2),
    ("bcdiv_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcdiv_zero.phpt", functions::run_bcmath_tests),
    ("bcdivmod.phpt", functions::bcdivmod),
    ("bcdivmod_by_zero.phpt", functions::bcdivmod_by_zero),
    ("bcfloor.phpt", functions::bcfloor_ceil),
    ("bcfloor_error.phpt", functions::bcfloor_ceil_error),
    ("bcmod.phpt", functions::run_bcmath_tests),
    ("bcmod_error2.phpt", functions::bcmod_error2),
    ("bcmod_error3.phpt", functions::bcmod_error3),
    ("bcmod_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcmod_zero.phpt", functions::run_bcmath_tests),
    ("bcmul.phpt", functions::run_bcmath_tests),
    ("bcmul_check_overflow.phpt", functions::bcmul_check_overflow),
    ("bcmul_error.phpt", functions::bcmul_error),
    ("bcmul_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcmul_zero.phpt", functions::run_bcmath_tests),
    ("bcpow.phpt", functions::run_bcmath_tests),
    ("bcpow_div_by_zero.phpt", functions::bcpow_div_by_zero),
    ("bcpow_error1.phpt", functions::bcpow_error1),
    ("bcpow_error2.phpt", functions::bcpow_error2),
    ("bcpow_error3.phpt", functions::bcpow_error3),
    ("bcpow_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcpow_zero.phpt", functions::run_bcmath_tests),
    ("bcpowmod.phpt", functions::bcpowmod),
    ("bcpowmod_error.phpt", functions::bcpowmod_error),
    (
        "bcpowmod_negative_exponent.phpt",
        functions::bcpowmod_negative_exponent,
    ),
    ("bcpowmod_with_mod_1.phpt", functions::bcpowmod_with_mod_1),
    (
        "bcpowmod_zero_modulus.phpt",
        functions::bcpowmod_zero_modulus,
    ),
    ("bcround_all.phpt", functions::bcround_all),
    ("bcround_away_from_zero.phpt", functions::run_round_test),
    ("bcround_ceiling.phpt", functions::run_round_test),
    ("bcround_early_return.phpt", functions::bcround_early_return),
    ("bcround_error.phpt", functions::bcround_error),
    ("bcround_floor.phpt", functions::run_round_test),
    ("bcround_half_down.phpt", functions::run_round_test),
    ("bcround_half_even.phpt", functions::run_round_test),
    ("bcround_half_odd.phpt", functions::run_round_test),
    ("bcround_half_up.phpt", functions::run_round_test),
    (
        "bcround_precision_bounds.phpt",
        functions::bcround_precision_bounds,
    ),
    ("bcround_toward_zero.phpt", functions::run_round_test),
    ("bcscale.phpt", functions::bcscale),
    ("bcscale_variation001.phpt", functions::bcscale_variation001),
    ("bcscale_variation002.phpt", functions::bcscale_variation002),
    ("bcscale_variation003.phpt", functions::bcscale_variation003),
    ("bcsqrt.phpt", functions::bcsqrt),
    ("bcsqrt_error1.phpt", functions::bcsqrt_error1),
    ("bcsqrt_error2.phpt", functions::bcsqrt_error2),
    ("bcsub.phpt", functions::run_bcmath_tests),
    ("bcsub_error.phpt", functions::bcsub_error),
    ("bcsub_large_numbers.phpt", functions::run_bcmath_tests),
    ("bcsub_zero.phpt", functions::run_bcmath_tests),
    ("bug44995.phpt", functions::bug44995),
    ("bug46781.phpt", functions::bug46781),
    ("bug54598.phpt", functions::bug54598),
    ("bug60377.phpt", functions::bug60377),
    ("bug66364.phpt", functions::bug66364),
    ("bug72093.phpt", functions::bug72093),
    ("bug75178.phpt", functions::bug75178),
    ("bug78238.phpt", functions::bug78238),
    ("bug78878.phpt", functions::bug78878),
    ("bug80545.phpt", functions::bug80545),
    ("gh15968.phpt", number::gh15968),
    ("gh16262.phpt", functions::gh16262),
    ("gh16265.phpt", functions::gh16265),
    ("gh16978.phpt", functions::gh16978),
    ("gh17275.phpt", functions::gh17275),
    ("gh20006.phpt", functions::gh20006),
    ("negative_scale.phpt", functions::negative_scale),
    ("scale.phpt", functions::scale),
    ("scale_ini.phpt", functions::scale),
    (
        "sec_embedded_null_truncation.phpt",
        functions::sec_embedded_null_truncation,
    ),
    ("str2num_formatting.phpt", functions::str2num_formatting),
    ("number/cast.phpt", number::cast),
    ("number/clone.phpt", number::clone),
    ("number/construct_64bit.phpt", number::construct_64bit),
    ("number/construct_error.phpt", number::construct_error),
    ("number/construct_float.phpt", number::construct_float),
    ("number/gh18641.phpt", number::gh18641),
    (
        "number/properties_isset_empty_exists.phpt",
        number::properties_isset_empty_exists,
    ),
    ("number/properties_read.phpt", number::properties_read),
    (
        "number/properties_unset.phpt",
        number::properties_write_unset,
    ),
    (
        "number/properties_write_error.phpt",
        number::properties_write_unset,
    ),
    ("number/serialize.phpt", number::serialize),
    ("number/unserialize.phpt", number::unserialize),
    ("number/unserialize_error.phpt", number::unserialize_error),
    ("number/methods/add.phpt", number::method_basic),
    (
        "number/methods/add_with_scale.phpt",
        number::method_with_scale,
    ),
    (
        "number/methods/calc_methods_num_arg_error.phpt",
        number::calc_methods_num_arg_error,
    ),
    (
        "number/methods/calc_methods_scale_arg_error.phpt",
        number::calc_methods_scale_arg_error,
    ),
    (
        "number/methods/calc_methods_zero_result_sign.phpt",
        number::calc_methods_zero_result_sign,
    ),
    ("number/methods/ceil.phpt", number::floor_ceil),
    ("number/methods/compare.phpt", number::compare),
    (
        "number/methods/compare_arg_error.phpt",
        number::compare_arg_error,
    ),
    (
        "number/methods/compare_near_zero.phpt",
        number::compare_near_zero,
    ),
    (
        "number/methods/compare_with_scale.phpt",
        number::compare_with_scale,
    ),
    ("number/methods/div.phpt", number::method_basic),
    (
        "number/methods/div_mod_by_zero.phpt",
        number::div_mod_by_zero,
    ),
    (
        "number/methods/div_with_scale.phpt",
        number::method_with_scale,
    ),
    ("number/methods/divmod.phpt", number::divmod),
    (
        "number/methods/divmod_with_scale.phpt",
        number::divmod_with_scale,
    ),
    ("number/methods/floor.phpt", number::floor_ceil),
    ("number/methods/mod.phpt", number::method_basic),
    (
        "number/methods/mod_with_scale.phpt",
        number::method_with_scale,
    ),
    ("number/methods/mul.phpt", number::method_basic),
    (
        "number/methods/mul_with_scale.phpt",
        number::method_with_scale,
    ),
    ("number/methods/pow.phpt", number::pow),
    (
        "number/methods/pow_div_by_zero.phpt",
        number::pow_div_by_zero,
    ),
    ("number/methods/pow_with_scale.phpt", number::pow_with_scale),
    ("number/methods/powmod.phpt", number::powmod),
    (
        "number/methods/powmod_arg_error.phpt",
        number::powmod_arg_error,
    ),
    (
        "number/methods/powmod_with_scale.phpt",
        number::powmod_with_scale,
    ),
    ("number/methods/round.phpt", number::round),
    (
        "number/methods/round_has_trailing_zeros.phpt",
        number::round_has_trailing_zeros,
    ),
    ("number/methods/sqrt.phpt", number::sqrt),
    (
        "number/methods/sqrt_with_scale.phpt",
        number::sqrt_with_scale,
    ),
    ("number/methods/sub.phpt", number::method_basic),
    (
        "number/methods/sub_with_scale.phpt",
        number::method_with_scale,
    ),
    ("number/operators/add_int.phpt", number::operator_values),
    ("number/operators/add_object.phpt", number::operator_values),
    ("number/operators/add_string.phpt", number::operator_values),
    (
        "number/operators/calc_array.phpt",
        number::operator_bad_operand,
    ),
    (
        "number/operators/calc_float.phpt",
        number::operator_bad_operand,
    ),
    (
        "number/operators/calc_non_numeric_string.phpt",
        number::operator_bad_operand,
    ),
    (
        "number/operators/calc_null.phpt",
        number::operator_bad_operand,
    ),
    (
        "number/operators/calc_other_class.phpt",
        number::operator_bad_operand,
    ),
    (
        "number/operators/calc_undef.phpt",
        number::operator_bad_operand,
    ),
    ("number/operators/compare.phpt", number::operator_compare),
    (
        "number/operators/compare_with_invalid_types.phpt",
        number::operator_compare_invalid,
    ),
    (
        "number/operators/compound_assignment.phpt",
        number::compound_assignment,
    ),
    ("number/operators/decrement.phpt", number::inc_dec),
    (
        "number/operators/div_by_zero.phpt",
        number::operator_by_zero,
    ),
    ("number/operators/div_int.phpt", number::operator_values),
    ("number/operators/div_object.phpt", number::operator_values),
    ("number/operators/div_string.phpt", number::operator_values),
    ("number/operators/increment.phpt", number::inc_dec),
    (
        "number/operators/mod_by_zero.phpt",
        number::operator_by_zero,
    ),
    ("number/operators/mod_int.phpt", number::operator_values),
    ("number/operators/mod_object.phpt", number::operator_values),
    ("number/operators/mod_string.phpt", number::operator_values),
    ("number/operators/mul_int.phpt", number::operator_values),
    ("number/operators/mul_object.phpt", number::operator_values),
    ("number/operators/mul_string.phpt", number::operator_values),
    (
        "number/operators/pow_div_by_zero.phpt",
        number::pow_div_by_zero,
    ),
    ("number/operators/pow_int.phpt", number::operator_pow),
    ("number/operators/pow_object.phpt", number::operator_pow),
    ("number/operators/pow_string.phpt", number::operator_pow),
    ("number/operators/sub_int.phpt", number::operator_values),
    ("number/operators/sub_object.phpt", number::operator_values),
    ("number/operators/sub_string.phpt", number::operator_values),
];

/// Scripts whose expectations are produced by the engine, not by bcmath.
const ENGINE_ONLY: &[(&str, &str)] = &[
    (
        "gh17398.phpt",
        "hard memory limit fatal error (the crate reports ErrorKind::OutOfMemory)",
    ),
    (
        "number/cast_warning.phpt",
        "(int)/(float) cast warnings of objects",
    ),
    (
        "number/class_extends_error.phpt",
        "final class inheritance compile error",
    ),
    ("number/construct_32bit.phpt", "32-bit builds only (SKIPIF)"),
    (
        "number/properties_unknown.phpt",
        "undefined/dynamic property errors of a strict-properties class",
    ),
];

#[test]
fn phpt_ports_match_expectations() {
    let mut failures = Vec::new();
    for &(name, port) in PORTED {
        let t = Phpt::load(name);
        let mut php = Php {
            bc: functions::state_for(&t),
            ..Php::default()
        };
        port(&mut php, &t);
        if let Err(d) = t.check(&php.out) {
            failures.push(format!("{name}: {d}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} ported scripts fail:\n{}",
        failures.len(),
        PORTED.len(),
        failures.join("\n")
    );
}

#[test]
fn every_phpt_is_accounted_for() {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if p.extension().is_some_and(|x| x == "phpt") {
                out.push(
                    p.strip_prefix(base)
                        .expect("prefix")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/phpt");
    let mut all = Vec::new();
    walk(&base, &base, &mut all);
    all.sort();
    let missing: Vec<&String> = all
        .iter()
        .filter(|n| !PORTED.iter().any(|(p, _)| p == n) && !ENGINE_ONLY.iter().any(|(p, _)| p == n))
        .collect();
    assert!(missing.is_empty(), "unported phpt files: {missing:?}");
    assert_eq!(all.len(), PORTED.len() + ENGINE_ONLY.len());
    assert_eq!(all.len(), 171);
}
