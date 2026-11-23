// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `exif_read_data()`, `exif_thumbnail()`, `exif_imagetype()` and
//! `exif_tagname()` (ref: `ext/exif/exif.c`).

use php_exif::{ExifConfig, ReadOptions, StreamOrigin, ThumbnailResult};
use php_ext_streams::{bridge, res};
use php_runtime::{Value, ZStr};
use php_streams::wrapper::{REPORT_ERRORS, STREAM_MUST_SEEK};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::convert::MbConverter;
use crate::emit;
use crate::io::{origin_of, StreamIo};

/// The `exif.*` settings in effect (validated by the `on_modify`
/// handlers; a value that still fails, e.g. from `-d`, keeps the default).
fn config(vm: &Vm) -> ExifConfig {
    let mut cfg = ExifConfig::default();
    for (name, _) in php_exif::exif::INI_ENTRIES {
        if let Some(v) = vm.ini_string(name) {
            let _ = cfg.set(name, &String::from_utf8_lossy(&v), &MbConverter);
        }
    }
    cfg
}

/// The `$file` argument of `exif_read_data()` / `exif_thumbnail()`.
enum Source {
    /// An open stream resource.
    Stream(std::rc::Rc<res::StreamCell>),
    /// A path to open with `STREAM_MUST_SEEK | IGNORE_PATH`.
    Path(Vec<u8>),
}

/// Validates argument 1: a stream resource, or a non-empty path without NUL
/// bytes (after `try_convert_to_string()`).
fn source_arg(vm: &mut Vm, a: &Args) -> VmResult<Source> {
    let v = a.at(0);
    if let Value::Resource(r) = &v {
        return Ok(Source::Stream(res::stream_of_resource(vm, a, r)?));
    }
    let path = vm.value_to_string(&v)?.as_bytes().to_vec();
    if path.is_empty() {
        return Err(a.value_error(vm, 1, "must not be empty"));
    }
    if path.contains(&0) {
        return Err(a.value_error(vm, 1, "must not contain any null bytes"));
    }
    Ok(Source::Path(path))
}

/// Runs `f` on the image stream (opened from a path, or the passed resource)
/// with its [`StreamOrigin`]. `Ok(None)` after the "Unable to open file"
/// warning.
fn with_source<R>(
    vm: &mut Vm,
    src: &Source,
    f: impl FnOnce(StreamIo<'_>, &StreamOrigin) -> R,
) -> VmResult<Option<R>> {
    match src {
        Source::Stream(cell) => res::with_stream(vm, cell, |s| {
            let origin = origin_of(s);
            f(StreamIo(s), &origin)
        }),
        Source::Path(path) => {
            let out = bridge::run(vm, || {
                let mut opened = None;
                let mut s = php_streams::open::open_wrapper(
                    path,
                    "rb",
                    STREAM_MUST_SEEK,
                    &mut opened,
                    None,
                )?;
                let origin = origin_of(&s);
                let r = f(StreamIo(&mut s), &origin);
                s.close();
                Some(r)
            })?;
            if out.is_none() {
                vm.php_warning(php_exif::exif::UNABLE_TO_OPEN)?;
            }
            Ok(out)
        }
    }
}

/// `exif_read_data($file, ?string $required_sections = null, bool $as_arrays
/// = false, bool $read_thumbnail = false): array|false`.
fn exif_read_data(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let src = source_arg(vm, a)?;
    let opts = ReadOptions {
        required_sections: a.opt(1).map(|v| match v {
            Value::Str(s) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }),
        as_arrays: a.bool(2),
        read_thumbnail: a.bool(3),
    };
    let cfg = config(vm);
    let out = with_source(vm, &src, |io, origin| {
        php_exif::exif_read_data(io, origin, &opts, &cfg, &MbConverter)
    })?;
    let Some((value, diags)) = out else {
        return Ok(Value::Bool(false));
    };
    emit(vm, &diags)?;
    Ok(value)
}

/// `exif_thumbnail($file, &$width = null, &$height = null, &$image_type =
/// null): string|false`.
fn exif_thumbnail(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let src = source_arg(vm, a)?;
    let want_dimensions = a.has(1) || a.has(2);
    let cfg = config(vm);
    let out = with_source(vm, &src, |io, origin| {
        php_exif::exif_thumbnail(io, origin, want_dimensions, &cfg, &MbConverter)
    })?;
    let Some((thumb, diags)) = out else {
        return Ok(Value::Bool(false));
    };
    emit(vm, &diags)?;
    let Some(ThumbnailResult {
        data,
        width,
        height,
        image_type,
    }) = thumb
    else {
        return Ok(Value::Bool(false));
    };
    for (n, v) in [(1, width), (2, height), (3, image_type)] {
        if a.has(n) {
            a.set_ref(n, Value::Int(v));
        }
    }
    Ok(Value::Str(ZStr::from(data)))
}

/// `exif_imagetype(string $filename): int|false`.
fn exif_imagetype(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = res::path_arg(vm, a, 0)?;
    if path.is_empty() {
        return Err(vm.throw("ValueError", php_streams::open::EMPTY_PATH_ERROR));
    }
    let out = bridge::run(vm, || {
        let mut opened = None;
        let mut s = php_streams::open::open_wrapper(&path, "rb", REPORT_ERRORS, &mut opened, None)?;
        let r = php_exif::exif_imagetype(StreamIo(&mut s), &path);
        s.close();
        Some(r)
    })?;
    let Some((ty, diags)) = out else {
        return Ok(Value::Bool(false));
    };
    emit(vm, &diags)?;
    Ok(ty.map_or(Value::Bool(false), Value::Int))
}

/// `exif_tagname(int $index): string|false`.
#[allow(clippy::unnecessary_wraps)] // the native-function signature
fn exif_tagname(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(php_exif::exif_tagname(a.int(0)).map_or(Value::Bool(false), Value::string))
}

/// Registers the exif functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("exif_tagname(int $index): string|false", exif_tagname)
        .function_static(
            "exif_read_data($file, ?string $required_sections = null, bool $as_arrays = false, bool $read_thumbnail = false): array|false",
            exif_read_data,
        )
        .function_static(
            "exif_thumbnail($file, &$width = null, &$height = null, &$image_type = null): string|false",
            exif_thumbnail,
        )
        .function_static("exif_imagetype(string $filename): int|false", exif_imagetype);
}
