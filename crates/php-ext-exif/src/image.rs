// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! ext/standard's `image.c` and `iptc.c`: `getimagesize()`,
//! `getimagesizefromstring()`, `image_type_to_mime_type()`,
//! `image_type_to_extension()`, `iptcparse()` and `iptcembed()`.

use php_exif::imagesize;
use php_exif::ImageSizeOutcome;
use php_ext_streams::{bridge, res};
use php_runtime::{Value, ZArray, ZStr};
use php_streams::wrapper::{REPORT_ERRORS, STREAM_MUST_SEEK};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::emit;
use crate::io::StreamIo;

/// Finishes `php_getimagesize_from_stream()`: fills `$image_info`, raises
/// the diagnostics and returns the result array (or `false`).
fn finish(vm: &mut Vm, a: &Args, out: &ImageSizeOutcome) -> VmResult<Value> {
    if let Some(app) = &out.app_markers {
        a.set_ref(1, Value::Array(ZArray::from_table(app.clone())));
    }
    emit(vm, &out.diagnostics)?;
    Ok(out.to_value())
}

/// `getimagesize(string $filename, &$image_info = null): array|false`.
fn getimagesize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = res::path_arg(vm, a, 0)?;
    let want_info = a.has(1);
    if want_info {
        // `zend_try_array_init()` happens before the file is opened; a
        // destructor of the old value that throws leaves the exception
        // pending, so the open reports nothing (GH-17162).
        vm.try_array_init_arg(a, 1)?;
    }
    if path.is_empty() {
        return Err(vm.throw("ValueError", php_streams::open::EMPTY_PATH_ERROR));
    }
    let out = bridge::run(vm, || {
        let mut opened = None;
        let mut s = php_streams::open::open_wrapper(
            &path,
            "rb",
            STREAM_MUST_SEEK | REPORT_ERRORS,
            &mut opened,
            None,
        )?;
        let r = imagesize::getimagesize(StreamIo(&mut s), &path, want_info);
        s.close();
        Some(r)
    })?;
    match out {
        Some(out) => finish(vm, a, &out),
        None => Ok(Value::Bool(false)),
    }
}

/// `getimagesizefromstring(string $string, &$image_info = null): array|false`.
fn getimagesizefromstring(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(0);
    let want_info = a.has(1);
    if want_info {
        vm.try_array_init_arg(a, 1)?;
    }
    let out = imagesize::getimagesizefromstring(data.as_bytes(), want_info);
    finish(vm, a, &out)
}

/// `image_type_to_mime_type(int $image_type): string`.
#[allow(clippy::unnecessary_wraps)] // the native-function signature
fn image_type_to_mime_type(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::string(php_exif::image_type_to_mime_type(a.int(0))))
}

/// `image_type_to_extension(int $image_type, bool $include_dot = true):
/// string|false`.
#[allow(clippy::unnecessary_wraps)] // the native-function signature
fn image_type_to_extension(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(php_exif::image_type_to_extension(a.int(0), a.bool(1))
        .map_or(Value::Bool(false), Value::string))
}

/// `iptcparse(string $iptc_block): array|false`.
#[allow(clippy::unnecessary_wraps)] // the native-function signature
fn iptcparse(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(php_exif::iptc::iptcparse(a.str(0).as_bytes()))
}

/// `iptcembed(string $iptc_data, string $filename, int $spool = 0):
/// string|bool`.
fn iptcembed(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(0);
    let path = res::path_arg(vm, a, 1)?;
    let spool = a.int(2);
    if !bridge::run(vm, || php_streams::paths::check_open_basedir(&path))? {
        return Ok(Value::Bool(false));
    }
    // `VCWD_FOPEN(jpeg_file, "rb")`: a plain file, no wrappers.
    let Some((jpeg, st_size)) = read_plain_file(&path) else {
        let mut msg = b"Unable to open ".to_vec();
        msg.extend_from_slice(&path);
        vm.php_warning_bytes(&msg)?;
        return Ok(Value::Bool(false));
    };
    let mut out = php_exif::iptc::iptcembed(data.as_bytes(), &jpeg, spool);
    // php-exif sizes the result buffer from the data it was given; C sizes
    // it from `fstat()`, which is smaller for FIFOs and the like: reading
    // then stops at the end of that buffer.
    if spool < 2 && st_size < jpeg.len() {
        let cap = data.len() + 29 + 1024 + 1 + st_size;
        if let Ok(Some(bytes)) = out.result.as_mut() {
            bytes.truncate(cap);
        }
    }
    for b in &out.spooled {
        // `PUTC()` writes byte by byte, so output handlers with a chunk
        // size see every byte (and may re-enter).
        vm.output(std::slice::from_ref(b));
    }
    Ok(match out.result {
        Ok(Some(bytes)) => Value::Str(ZStr::from(bytes)),
        Ok(None) => Value::Bool(true),
        Err(()) => Value::Bool(false),
    })
}

/// Reads a file with `fopen()` + `fstat()` semantics: the contents and
/// `st_size`.
fn read_plain_file(path: &[u8]) -> Option<(Vec<u8>, usize)> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let mut f = std::fs::File::open(std::ffi::OsStr::from_bytes(path)).ok()?;
    let st_size = f
        .metadata()
        .ok()
        .and_then(|m| usize::try_from(m.len()).ok())
        .unwrap_or(0);
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some((buf, st_size))
}

/// Registers the ext/standard image functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "image_type_to_mime_type(int $image_type): string",
        image_type_to_mime_type,
    )
    .function_static(
        "image_type_to_extension(int $image_type, bool $include_dot = true): string|false",
        image_type_to_extension,
    )
    .function_static(
        "getimagesize(string $filename, &$image_info = null): array|false",
        getimagesize,
    )
    .function_static(
        "getimagesizefromstring(string $string, &$image_info = null): array|false",
        getimagesizefromstring,
    )
    .function_static(
        "iptcembed(string $iptc_data, string $filename, int $spool = 0): string|bool",
        iptcembed,
    )
    .function_static("iptcparse(string $iptc_block): array|false", iptcparse);
}
