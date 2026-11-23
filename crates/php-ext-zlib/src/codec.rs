// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The one-shot codecs: `gzcompress()`/`gzuncompress()`,
//! `gzdeflate()`/`gzinflate()`, `gzencode()`/`gzdecode()` and
//! `zlib_encode()`/`zlib_decode()`.

use php_runtime::Value;
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;
use php_zlib::constants::{ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_RAW};

use crate::{bytes, zlib_error};

/// Integer argument `n`, or `default` when it was not passed.
pub(crate) fn int_or(a: &Args, n: usize, default: i64) -> i64 {
    if a.has(n) {
        a.int(n)
    } else {
        default
    }
}

/// Converts a codec result to the PHP return value.
fn result(vm: &mut Vm, r: php_zlib::Result<Vec<u8>>) -> VmResult<Value> {
    match r {
        Ok(v) => Ok(bytes(v)),
        Err(e) => zlib_error(vm, &e),
    }
}

/// Runs a codec inside the streams bridge (option conversions report
/// through the library's diagnostics queue).
fn run(vm: &mut Vm, f: impl FnOnce() -> php_zlib::Result<Vec<u8>>) -> VmResult<Value> {
    let r = php_ext_streams::bridge::run(vm, f)?;
    result(vm, r)
}

macro_rules! encoder {
    ($name:ident, $lib:path, $default:expr) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let data = a.str(0);
            let level = int_or(a, 1, -1);
            let encoding = int_or(a, 2, $default);
            run(vm, || $lib(data.as_bytes(), level, encoding))
        }
    };
}

macro_rules! decoder {
    ($name:ident, $lib:path) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            let data = a.str(0);
            let max = int_or(a, 1, 0);
            run(vm, || $lib(data.as_bytes(), max))
        }
    };
}

encoder!(gzcompress, php_zlib::gzcompress, ZLIB_ENCODING_DEFLATE);
encoder!(gzdeflate, php_zlib::gzdeflate, ZLIB_ENCODING_RAW);
encoder!(gzencode, php_zlib::gzencode, ZLIB_ENCODING_GZIP);
decoder!(gzuncompress, php_zlib::gzuncompress);
decoder!(gzinflate, php_zlib::gzinflate);
decoder!(gzdecode, php_zlib::gzdecode);
decoder!(zlib_decode, php_zlib::zlib_decode);

fn zlib_encode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(0);
    let encoding = a.int(1);
    let level = int_or(a, 2, -1);
    run(vm, || {
        php_zlib::zlib_encode(data.as_bytes(), encoding, level)
    })
}

/// Registers the codec functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "zlib_encode(string $data, int $encoding, int $level = -1): string|false",
        zlib_encode,
    )
    .function_static(
        "zlib_decode(string $data, int $max_length = 0): string|false",
        zlib_decode,
    )
    .function_static(
        "gzdeflate(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_RAW): string|false",
        gzdeflate,
    )
    .function_static(
        "gzencode(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_GZIP): string|false",
        gzencode,
    )
    .function_static(
        "gzcompress(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_DEFLATE): string|false",
        gzcompress,
    )
    .function_static(
        "gzinflate(string $data, int $max_length = 0): string|false",
        gzinflate,
    )
    .function_static(
        "gzdecode(string $data, int $max_length = 0): string|false",
        gzdecode,
    )
    .function_static(
        "gzuncompress(string $data, int $max_length = 0): string|false",
        gzuncompress,
    );
}
