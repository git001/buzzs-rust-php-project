// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `pack.c`, `scanf.c`, `uuencode.c`, `quot_print.c`, `versioning.c`,
//! `soundex.c`, `metaphone.c` and `levenshtein.c`.

use php_runtime::{Value, ZArray};
use std::cell::RefCell;

use php_stdstr::error::{Notices, StdResult};
use php_stdstr::pack::{self, PackValue, UnpackKey, UnpackValue};
use php_stdstr::scanf::{self, ScanValue, SscanfResult};
use php_stdstr::{analysis, quoted_printable, uuencode, versioning};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::{assoc, bytes, check, emit, mem, opt_bytes, to_str};

/// The conversion `pack()` applies to an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conv {
    Long,
    Double,
    Bytes,
}

/// First-pass `pack()` argument: records the conversions the format asks
/// for (the library's [`PackValue`] has no access to the VM, and PHP's
/// conversions may warn or call `__toString()`).
struct PackRecorder<'a> {
    idx: usize,
    log: &'a RefCell<Vec<(usize, Conv)>>,
}

impl PackValue for PackRecorder<'_> {
    fn to_long(&self) -> i64 {
        self.log.borrow_mut().push((self.idx, Conv::Long));
        0
    }
    fn to_double(&self) -> f64 {
        self.log.borrow_mut().push((self.idx, Conv::Double));
        0.0
    }
    fn to_bytes(&self) -> StdResult<Vec<u8>> {
        self.log.borrow_mut().push((self.idx, Conv::Bytes));
        Ok(Vec::new())
    }
}

/// Second-pass `pack()` argument with the VM-converted values.
#[derive(Default)]
struct PackConverted {
    long: i64,
    double: f64,
    bytes: Vec<u8>,
}

impl PackValue for PackConverted {
    fn to_long(&self) -> i64 {
        self.long
    }
    fn to_double(&self) -> f64 {
        self.double
    }
    fn to_bytes(&self) -> StdResult<Vec<u8>> {
        Ok(self.bytes.clone())
    }
}

fn pack_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let format = a.str(0);
    let values: Vec<Value> = a.values.iter().skip(1).map(Value::deref).collect();
    let log = RefCell::new(Vec::new());
    {
        let rec: Vec<PackRecorder<'_>> = (0..values.len())
            .map(|idx| PackRecorder { idx, log: &log })
            .collect();
        let mut sink = Notices::new();
        // Format errors are reported by the second pass.
        let _ = pack::pack(format.as_bytes(), &rec, &mut sink);
    }
    let mut conv: Vec<PackConverted> = values.iter().map(|_| PackConverted::default()).collect();
    for (idx, kind) in log.into_inner() {
        let v = &values[idx];
        match kind {
            Conv::Long => conv[idx].long = vm.value_to_int(v)?,
            Conv::Double => conv[idx].double = vm.value_to_float(v)?,
            Conv::Bytes => conv[idx].bytes = to_str(vm, v)?.as_bytes().to_vec(),
        }
    }
    let mut n = Notices::new();
    let r = pack::pack(format.as_bytes(), &conv, &mut n);
    emit(vm, n)?;
    Ok(bytes(check(vm, r)?))
}

fn unpack_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (format, data) = (a.str(0), a.str(1));
    // `C*`/`c*` (one element per remaining byte) is the format that can
    // turn a script-sized string into a huge array.
    if format.windows(2).any(|w| matches!(w, [b'c' | b'C', b'*'])) {
        mem::check(data.len().saturating_mul(mem::elem(0)))?;
    }
    let mut n = Notices::new();
    let r = pack::unpack(format.as_bytes(), data.as_bytes(), a.int(2), &mut n);
    emit(vm, n)?;
    let r = check(vm, r)?;
    Ok(match r {
        None => Value::Bool(false),
        Some(items) => assoc(items.into_iter().map(|(k, v)| {
            let key = match k {
                UnpackKey::Int(i) => php_runtime::ArrayKey::Int(i),
                UnpackKey::Str(s) => php_runtime::ArrayKey::Str(s.into()),
            };
            let val = match v {
                UnpackValue::Int(i) => Value::Int(i),
                UnpackValue::Float(f) => Value::Float(f),
                UnpackValue::Str(s) => bytes(s),
            };
            (key, val)
        })),
    })
}

fn scan_value(v: ScanValue) -> Value {
    match v {
        ScanValue::Int(i) => Value::Int(i),
        ScanValue::Float(f) => Value::Float(f),
        ScanValue::Str(s) => bytes(s),
    }
}

fn sscanf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let num_vars = a.values.len().saturating_sub(2);
    let r = scanf::sscanf(a.str(0).as_bytes(), a.str(1).as_bytes(), num_vars);
    Ok(match check(vm, r)? {
        SscanfResult::Array(None) => Value::Null,
        SscanfResult::Array(Some(vals)) => Value::Array(ZArray::from_values(
            vals.into_iter().map(|v| v.map_or(Value::Null, scan_value)),
        )),
        SscanfResult::Assigned { values, count } => {
            for (i, v) in values.into_iter().enumerate() {
                if let Some(v) = v {
                    a.set_ref(i + 2, scan_value(v));
                }
            }
            Value::Int(count)
        }
    })
}

fn convert_uuencode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(uuencode::convert_uuencode(a.str(0).as_bytes())))
}

fn convert_uudecode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = uuencode::convert_uudecode(a.str(0).as_bytes(), &mut n);
    emit(vm, n)?;
    Ok(opt_bytes(r))
}

fn quoted_printable_decode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(quoted_printable::quoted_printable_decode(
        a.str(0).as_bytes(),
    )))
}

fn quoted_printable_encode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(quoted_printable::quoted_printable_encode(
        a.str(0).as_bytes(),
    )))
}

fn version_compare(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (v1, v2) = (a.str(0), a.str(1));
    match a.opt(2) {
        None => Ok(Value::Int(i64::from(versioning::version_compare(
            v1.as_bytes(),
            v2.as_bytes(),
        )))),
        Some(op) => {
            let op = to_str(vm, &op)?;
            let r = versioning::version_compare_op(v1.as_bytes(), v2.as_bytes(), op.as_bytes());
            Ok(Value::Bool(check(vm, r)?))
        }
    }
}

fn soundex(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(analysis::soundex(a.str(0).as_bytes())))
}

fn metaphone(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = analysis::metaphone(a.str(0).as_bytes(), a.int(1));
    Ok(bytes(check(vm, r)?))
}

fn levenshtein(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cost = |n: usize| if a.has(n) { a.int(n) } else { 1 };
    let (s1, s2) = (a.str(0), a.str(1));
    if !s1.is_empty() && !s2.is_empty() {
        // The first of the two workspace rows.
        let row = (s2.len() + 1).saturating_mul(8);
        mem::check(row)?;
    }
    let r = analysis::levenshtein(s1.as_bytes(), s2.as_bytes(), cost(2), cost(3), cost(4));
    Ok(Value::Int(r))
}

/// Registers the functions of this module.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("pack(string $format, mixed ...$values): string", pack_fn)
        .function_static("unpack(string $format, string $string, int $offset = 0): array|false", unpack_fn)
        .function_static("sscanf(string $string, string $format, mixed &...$vars): array|int|null", sscanf)
        .function_static("convert_uuencode(string $string): string", convert_uuencode)
        .function_static("convert_uudecode(string $string): string|false", convert_uudecode)
        .function_static("quoted_printable_decode(string $string): string", quoted_printable_decode)
        .function_static("quoted_printable_encode(string $string): string", quoted_printable_encode)
        .function_static("version_compare(string $version1, string $version2, ?string $operator = null): int|bool", version_compare)
        .function_static("soundex(string $string): string", soundex)
        .function_static("metaphone(string $string, int $max_phonemes = 0): string", metaphone)
        .deprecated("8.6", "use a userland phonetic matching library instead")
        .function_static("levenshtein(string $string1, string $string2, int $insertion_cost = 1, int $replacement_cost = 1, int $deletion_cost = 1): int", levenshtein);
}
