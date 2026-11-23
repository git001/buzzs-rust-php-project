// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/bz2`: `bzcompress()`/`bzdecompress()`, `bzopen()` and the bz2
//! stream functions, the `compress.bzip2://` wrapper and the `bzip2.*`
//! filters. Registered by [`crate::Zlib`], which also reports `bz2` as a
//! loaded extension.

use std::rc::Rc;

use php_bz2::filter::Bz2FilterFactory;
use php_bz2::wrapper::{bz_error, bzopen_path, bzopen_stream, bzread_length, Bz2Wrapper};
use php_bz2::Output;
use php_ext_streams::{bridge, res};
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::bytes;

/// Maps a `php-bz2` failure to PHP (warning + `false`, or an exception).
fn bz2_error(vm: &mut Vm, e: &php_bz2::Error) -> VmResult<Value> {
    use php_bz2::Error as E;
    match e {
        E::Warning(m) => {
            vm.php_warning(m)?;
            Ok(Value::Bool(false))
        }
        E::ValueError(m) => Err(vm.throw("ValueError", m)),
        E::TypeError(m) => Err(vm.throw("TypeError", m)),
        E::Error(m) => Err(vm.throw("Error", m)),
    }
}

fn output(vm: &mut Vm, r: php_bz2::Result<Output>) -> VmResult<Value> {
    match r {
        Ok(Output::Data(d)) => Ok(bytes(d)),
        Ok(Output::Code(c)) => Ok(Value::Int(c)),
        Ok(Output::False) => Ok(Value::Bool(false)),
        Err(e) => bz2_error(vm, &e),
    }
}

fn bzcompress(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(0);
    let block = if a.has(1) { a.int(1) } else { 4 };
    let work = if a.has(2) { a.int(2) } else { 0 };
    let r = php_bz2::bzcompress(data.as_bytes(), block, work);
    output(vm, r)
}

fn bzdecompress(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(0);
    let small = a.bool(1);
    let r = php_bz2::bzdecompress(data.as_bytes(), small);
    output(vm, r)
}

fn bzopen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = a.str(1);
    let m = mode.as_bytes();
    if m.len() != 1 || (m[0] != b'r' && m[0] != b'w') {
        return Err(a.value_error(vm, 2, "must be either \"r\" or \"w\""));
    }
    let stream = match a.at(0) {
        Value::Str(file) => {
            let r = bridge::run(vm, || bzopen_path(file.as_bytes(), m, None))?;
            match r {
                Ok(s) => s,
                Err(e) => return bz2_error(vm, &e),
            }
        }
        Value::Resource(r) => {
            let cell = res::stream_of_resource(vm, a, &r)?;
            let r = res::with_stream(vm, &cell, |s| bzopen_stream(s, m))?;
            match r {
                Some(Ok(s)) => s,
                Some(Err(e)) => return bz2_error(vm, &e),
                None => None,
            }
        }
        other => {
            let given = bridge::given_type(&other);
            return Err(a.type_error(
                vm,
                1,
                &format!("must be of type string or file-resource, {given} given"),
            ));
        }
    };
    Ok(stream.map_or(Value::Bool(false), |s| res::new_stream(s, None)))
}

fn bzread(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = res::stream_arg(vm, a, 0)?;
    let len = if a.has(1) { a.int(1) } else { 1024 };
    let len = match bzread_length(len) {
        Ok(l) => l,
        Err(e) => return bz2_error(vm, &e),
    };
    let r = res::with_stream(vm, &cell, |s| s.read_vec(len))?.flatten();
    Ok(r.map_or(Value::Bool(false), bytes))
}

/// `php_bz2_error()`: `(errno, errstr)` of the bz2 stream argument.
fn error_info(vm: &mut Vm, a: &Args, func: &str) -> VmResult<Result<(i64, &'static str), Value>> {
    let cell = res::stream_arg(vm, a, 0)?;
    let r = res::with_stream(vm, &cell, |s| bz_error(func, s))?;
    match r {
        Some(Ok(info)) => Ok(Ok(info)),
        Some(Err(e)) => bz2_error(vm, &e).map(Err),
        None => Ok(Err(Value::Bool(false))),
    }
}

fn bzerrno(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match error_info(vm, a, "bzerrno")? {
        Ok((n, _)) => Value::Int(n),
        Err(v) => v,
    })
}

fn bzerrstr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match error_info(vm, a, "bzerrstr")? {
        Ok((_, s)) => bytes(s),
        Err(v) => v,
    })
}

fn bzerror(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(match error_info(vm, a, "bzerror")? {
        Ok((n, s)) => {
            let mut arr = ZArray::new();
            arr.make_mut().insert(ZStr::from("errno"), Value::Int(n));
            arr.make_mut().insert(ZStr::from("errstr"), bytes(s));
            Value::Array(arr)
        }
        Err(v) => v,
    })
}

/// Registers `ext/bz2`.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    php_streams::wrapper::register_global(b"compress.bzip2", Rc::new(Bz2Wrapper));
    php_streams::wrapper::register_filter_global("bzip2.*", Rc::new(Bz2FilterFactory));
    m.function_static("bzopen($file, string $mode)", bzopen)
        .function_static("bzread($bz, int $length = 1024): string|false", bzread)
        .function_static("bzerrno($bz): int", bzerrno)
        .function_static("bzerrstr($bz): string", bzerrstr)
        .function_static("bzerror($bz): array", bzerror)
        .function_static(
            "bzcompress(string $data, int $block_size = 4, int $work_factor = 0): string|int",
            bzcompress,
        )
        .function_static(
            "bzdecompress(string $data, bool $use_less_memory = false): string|int|false",
            bzdecompress,
        );
    for (sig, target) in [
        (
            "bzwrite($bz, string $data, ?int $length = null): int|false",
            "fwrite",
        ),
        ("bzflush($bz): bool", "fflush"),
        ("bzclose($bz): bool", "fclose"),
    ] {
        crate::falias(m, sig, target);
    }
    m.vm().loaded_extensions.push("bz2".to_string());
}
