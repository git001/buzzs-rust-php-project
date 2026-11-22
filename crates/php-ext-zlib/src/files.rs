// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `gzopen()`, `gzfile()`, `readgzfile()`, the `gz*` stream function
//! aliases, and registration of the `compress.zlib://` wrapper and the
//! `zlib.*` stream filters.
//!
//! `gzopen()` returns an ordinary `php-ext-streams` stream resource whose
//! ops are `php-zlib`'s gzip stream, so `gzread()` & co. are plain aliases
//! of the `f*` functions, exactly like PHP's `ZEND_FALIAS` entries.

use std::rc::Rc;

use php_ext_streams::{bridge, res};
use php_runtime::{Value, ZArray};
use php_streams::context::ContextRef;
use php_streams::stream::{Stream, CHUNK_SIZE};
use php_streams::wrapper::{REPORT_ERRORS, USE_PATH};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;
use php_zlib::filter::ZlibFilterFactory;
use php_zlib::wrapper::{gzopen as lib_gzopen, ZlibWrapper};

use crate::{bytes, mem};

/// `sizeof(zval)`: one element of a packed array.
const PACKED_SLOT: usize = 16;

/// `php_stream_gets()` buffer size used by `gzfile()` (`char buf[8192]`,
/// read with `sizeof(buf) - 1`).
const GZFILE_LINE: usize = 8191;

// `readgzfile()` and `gzpassthru()` write each chunk as it is read
// (`_php_stream_passthru()`, `char buf[8192]`). With output buffering the
// buffer can grow past `memory_limit`: the allocation is granted and the
// violation recorded, and the loop stops at the next chunk
// (`ZEND_HAS_OOM()`), so the VM raises "Allowed memory size of N bytes
// exhausted" (discarding the buffers) right after the call instead of the
// output growing towards the hard limit.

/// `php_stream_context_from_zval(NULL, false)`: the default context.
fn default_context() -> Option<ContextRef> {
    res::ctx_cell_of(&res::default_context()).map(|c| Rc::clone(&c.ctx))
}

/// `php_stream_gzopen(NULL, filename, mode, REPORT_ERRORS [| USE_PATH])`.
fn open(vm: &mut Vm, a: &Args, mode: &str, use_path: bool) -> VmResult<Option<Stream>> {
    let path = res::path_arg(vm, a, 0)?;
    let ctx = default_context();
    let options = REPORT_ERRORS | if use_path { USE_PATH } else { 0 };
    bridge::run(vm, || {
        let mut opened = None;
        lib_gzopen(false, &path, mode, options, &mut opened, ctx.as_ref()).ok()
    })
}

fn gzopen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = a.str(1).to_string_lossy();
    let use_path = a.bool(2);
    Ok(match open(vm, a, &mode, use_path)? {
        Some(s) => res::new_stream(s, None),
        None => Value::Bool(false),
    })
}

fn gzfile(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let use_path = a.bool(1);
    let Some(mut s) = open(vm, a, "rb", use_path)? else {
        return Ok(Value::Bool(false));
    };
    // Stop once PHP's array would exceed memory_limit.
    let limit = mem::limit(vm);
    let (lines, oom) = bridge::run(vm, || {
        let mut lines = Vec::new();
        let mut size = 0usize;
        let mut oom = None;
        while let Some(l) = s.get_line(Some(GZFILE_LINE)) {
            size = size.saturating_add(mem::zstr(l.len()) + PACKED_SLOT);
            lines.push(l);
            if lines.len() % 1024 == 0 && mem::exceeds(limit, size) {
                oom = Some(size);
                break;
            }
        }
        s.close();
        (lines, oom)
    })?;
    if let Some(size) = oom {
        drop(lines);
        mem::check(vm, size)?;
        return Ok(Value::Bool(false));
    }
    let mut arr = ZArray::with_capacity(lines.len());
    for l in lines {
        let _ = arr.make_mut().push(bytes(l));
    }
    Ok(Value::Array(arr))
}

fn readgzfile(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let use_path = a.bool(1);
    let Some(mut s) = open(vm, a, "rb", use_path)? else {
        return Ok(Value::Bool(false));
    };
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut count = 0usize;
    // `php_stream_passthru()`: PHPWRITE each chunk as it is read.
    while let Ok(n @ 1..) = bridge::run_stream(vm, || s.read(&mut buf))? {
        if php_memlimit::is_pending() {
            break;
        }
        vm.output(&buf[..n]);
        count += n;
    }
    bridge::run(vm, || {
        s.close();
    })?;
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

/// `gzpassthru()` (`fpassthru()`): `php_stream_passthru()` on the stream.
fn gzpassthru(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = res::stream_arg(vm, a, 0)?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut count = 0usize;
    while let Some(Ok(n @ 1..)) = res::with_stream(vm, &cell, |s| s.read(&mut buf))? {
        if php_memlimit::is_pending() {
            break;
        }
        vm.output(&buf[..n]);
        count += n;
    }
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

/// `gzgets()` (`fgets()`, with the memory check on the line read).
fn gzgets(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(fgets) = vm.native_function("fgets") else {
        return Ok(Value::Bool(false));
    };
    let v = (fgets.handler)(vm, a)?;
    mem::check_str(vm, &v)?;
    Ok(v)
}

/// Registers the wrapper, the filters and the stream functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    php_streams::wrapper::register_global(b"compress.zlib", Rc::new(ZlibWrapper));
    php_streams::wrapper::register_filter_global("zlib.*", Rc::new(ZlibFilterFactory));
    m.function_static(
        "gzfile(string $filename, bool $use_include_path = false): array|false",
        gzfile,
    )
    .function_static(
        "gzopen(string $filename, string $mode, bool $use_include_path = false)",
        gzopen,
    )
    .function_static(
        "readgzfile(string $filename, bool $use_include_path = false): int|false",
        readgzfile,
    )
    .function_static("gzpassthru($stream): int", gzpassthru)
    .function_static("gzgets($stream, ?int $length = null): string|false", gzgets);
    for (sig, target) in [
        (
            "gzwrite($stream, string $data, ?int $length = null): int|false",
            "fwrite",
        ),
        (
            "gzputs($stream, string $data, ?int $length = null): int|false",
            "fwrite",
        ),
        ("gzrewind($stream): bool", "rewind"),
        ("gzclose($stream): bool", "fclose"),
        ("gzeof($stream): bool", "feof"),
        ("gzgetc($stream): string|false", "fgetc"),
        (
            "gzseek($stream, int $offset, int $whence = SEEK_SET): int",
            "fseek",
        ),
        ("gztell($stream): int|false", "ftell"),
        ("gzread($stream, int $length): string|false", "fread"),
    ] {
        crate::falias(m, sig, target);
    }
}
