// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/zlib` and `ext/bz2` for the phprust VM: binds the `php-zlib` and
//! `php-bz2` libraries through the `php_vm::ext` extension API.
//!
//! * [`codec`] — the one-shot `gz*`/`zlib_*` codecs;
//! * [`context`] — `DeflateContext`/`InflateContext` and
//!   `deflate_*`/`inflate_*`;
//! * [`files`] — `gzopen()`/`gzfile()`/`readgzfile()`, the `gz*` stream
//!   aliases, the `compress.zlib://` wrapper and `zlib.*` filters;
//! * [`output`] — `ob_gzhandler()`, `zlib_get_coding_type()` and the
//!   `zlib.output_*` INI settings;
//! * [`bz2`] — all of `ext/bz2` (registered as a second loaded extension).
//!
//! Stream resources are those of `php-ext-streams`, so the regular file
//! functions operate on `gzopen()`/`bzopen()` results.

pub mod bz2;
pub mod codec;
pub mod context;
pub mod files;
pub mod output;

use php_runtime::{Value, ZStr};
use php_vm::ext::{Extension, ModuleBuilder, VmResult};
use php_vm::Vm;

/// The `zlib` extension (also registers `bz2`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Zlib;

impl Extension for Zlib {
    fn name(&self) -> &'static str {
        "zlib"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for (name, v) in php_zlib::constants() {
            let v = match v {
                php_zlib::ConstValue::Int(i) => Value::Int(i),
                php_zlib::ConstValue::Str(s) => Value::Str(ZStr::from(s)),
            };
            m.constant(name, v);
        }
        codec::register(m);
        context::register(m);
        files::register(m);
        output::register(m);
        bz2::register(m);
    }
}

/// Maps a `php-zlib` failure to PHP: a warning (with the running
/// function's prefix) and `false`, or a thrown exception.
pub(crate) fn zlib_error(vm: &mut Vm, e: &php_zlib::Error) -> VmResult<Value> {
    use php_zlib::Error as E;
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

/// A PHP string value from bytes.
pub(crate) fn bytes(b: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(b.as_ref()))
}

/// Registers `signature` with the handler of the already registered
/// native function `target` (PHP's `ZEND_FALIAS`, but keeping this
/// function's own parameter names for error messages).
pub(crate) fn falias(m: &mut ModuleBuilder<'_>, signature: &str, target: &str) {
    if let Some(b) = m.vm().native_function(target) {
        m.function(signature, b.handler);
    }
}
