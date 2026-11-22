// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/file.c`: opening, reading and writing streams, whole-file
//! helpers, and the path functions that do not stat.

use std::rc::Rc;

use php_runtime::{Value, ZArray};
use php_streams::context::ContextRef;
use php_streams::file::{self as lib, ArgError, PutData};
use php_streams::stream::Stream;
use php_streams::wrapper::{REPORT_ERRORS, USE_PATH};
use php_streams::{open, pathname, php_wrapper, plain, temp};
use php_vm::ext::{Args, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

use crate::bridge::{self, bytes};
use crate::res::{
    self, context_arg, fname, path_arg, str_arg, stream_arg, with_stream, StreamCell,
};

/// Turns a library `ArgError` into the PHP `ValueError`.
pub fn arg_error(vm: &mut Vm, a: &Args, e: &ArgError) -> Throw {
    let msg = e
        .message
        .strip_prefix('(')
        .and_then(|m| m.split_once(") "))
        .map_or(e.message.as_str(), |(_, rest)| rest);
    a.value_error(vm, e.arg as usize, msg)
}

/// Opens `path` through the wrappers; throws what `php://` URLs raise.
pub fn open_stream(
    vm: &mut Vm,
    a: &Args,
    path: &[u8],
    mode: &str,
    options: u32,
    ctx: Option<&ContextRef>,
) -> VmResult<Option<(Stream, Option<Value>)>> {
    if path.is_empty() {
        return Err(vm.throw("ValueError", open::EMPTY_PATH_ERROR));
    }
    let mut opened = None;
    crate::userwrap::clear_last_object();
    let s = bridge::run(vm, || {
        let s = open::open_wrapper(path, mode, options, &mut opened, ctx)?;
        let data = if res::is_user_stream(&s) {
            crate::userwrap::take_last_object()
        } else {
            None
        };
        Some((s, data))
    })?;
    let Some((s, data)) = s else {
        if let Some(e) = php_wrapper::take_exception() {
            return Err(php_url_exception(vm, a, &e));
        }
        return Ok(None);
    };
    Ok(Some((s, data)))
}

/// The exception for a failed `php://` open.
pub fn php_url_exception(vm: &mut Vm, a: &Args, e: &php_wrapper::PhpUrlException) -> Throw {
    match e {
        php_wrapper::PhpUrlException::NegativeMaxMemory => {
            a.value_error(vm, 2, "must be greater than or equal to 0")
        }
        php_wrapper::PhpUrlException::NoResource => vm.throw("Error", "No URL resource specified"),
    }
}

/// Flushes the VM's buffered standard output before writing to a stream
/// on file descriptor 1 or 2, so output keeps its order.
pub fn sync_stdio(vm: &mut Vm, cell: &StreamCell) {
    let is_std = cell.get().is_some_and(|s| {
        s.ops_as::<plain::StdioOps>()
            .is_some_and(|o| matches!(o.fd(), 1 | 2))
    });
    if is_std {
        vm.flush_stdout();
    }
}

fn opt_int(a: &Args, n: usize) -> Option<i64> {
    a.opt(n).map(|_| a.int(n))
}

fn int_value(n: i64) -> Value {
    Value::Int(n)
}

fn str_or_false(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Bool(false), bytes)
}

fn fopen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let mode = a.str(1).to_string_lossy();
    let use_path = a.bool(2);
    let ctx = context_arg(vm, a, 3, false)?;
    let opts = REPORT_ERRORS | if use_path { USE_PATH } else { 0 };
    match open_stream(vm, a, &path, &mode, opts, ctx.as_ref())? {
        Some((s, data)) => Ok(res::new_stream(s, data)),
        None => Ok(Value::Bool(false)),
    }
}

/// Whether `handle` is one of the CLI's `STDIN`/`STDOUT`/`STDERR`
/// resources (handles 1–3, registered with `NO_CLOSE`).
fn is_cli_std_handle(handle: i64) -> bool {
    (1..=3).contains(&handle)
}

fn fclose(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (r, cell) = res::stream_arg_res(vm, a, 0)?;
    if res::is_enclosed(&r) {
        // The memory stream inside php://temp (GH-21221):
        // php_stream_encloses() set PHP_STREAM_FLAG_NO_FCLOSE.
        vm.php_warning("cannot close the provided stream, as it must not be manually closed")?;
        return Ok(Value::Bool(false));
    }
    // A stream in use by one of its own callbacks cannot be closed
    // (PHP_STREAM_FLAG_NO_FCLOSE is set around user-space calls).
    let busy = cell.get().is_none();
    if busy && !php_streams::user::in_guarded_call(cell.id()) {
        // In use by an operation whose callback (e.g. `stream_flush`) may
        // close it: the resource goes now, the stream once the operation
        // ends.
        r.close();
        cell.request_close();
        return Ok(Value::Bool(true));
    }
    let no_fclose = busy
        || cell
            .get()
            .is_some_and(|s| s.has_flag(php_streams::stream::flags::NO_FCLOSE));
    if no_fclose {
        vm.php_warning("cannot close the provided stream, as it must not be manually closed")?;
        return Ok(Value::Bool(false));
    }
    let mut result = 0;
    sync_stdio(vm, &cell);
    if let Some(mut s) = cell.take() {
        r.close();
        if is_cli_std_handle(r.handle()) {
            // GH-8827: an explicit fclose(STDIN/STDOUT/STDERR) closes the
            // descriptor; only dropping the constant's resource keeps it.
            s.clear_flags(php_streams::stream::flags::NO_CLOSE);
        }
        let id = s.id();
        result = res::fclosing(id, || bridge::run(vm, || s.close()))?;
        res::release_enclosed(id);
        crate::process::reap(cell.id());
    } else {
        r.close();
    }
    Ok(Value::Bool(result == 0))
}

fn fread(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let len = a.int(1);
    match with_stream(vm, &cell, |s| lib::fread(s, len))? {
        Some(Ok(v)) => Ok(str_or_false(v)),
        Some(Err(e)) => Err(arg_error(vm, a, &e)),
        None => Ok(Value::Bool(false)),
    }
}

fn fwrite(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let data = str_arg(a, 1);
    let len = opt_int(a, 2);
    sync_stdio(vm, &cell);
    let r = with_stream(vm, &cell, |s| lib::fwrite(s, &data, len))?.flatten();
    Ok(r.map_or(Value::Bool(false), int_value))
}

fn fgets(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let len = opt_int(a, 1);
    match with_stream(vm, &cell, |s| lib::fgets(s, len))? {
        Some(Ok(v)) => Ok(str_or_false(v)),
        Some(Err(e)) => Err(arg_error(vm, a, &e)),
        None => Ok(Value::Bool(false)),
    }
}

fn fgetc(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let c = with_stream(vm, &cell, Stream::getc)?.flatten();
    Ok(c.map_or(Value::Bool(false), |c| bytes([c])))
}

fn feof(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let r = with_stream(vm, &cell, Stream::eof)?;
    // A stream whose user filter is running reports its EOF flag.
    Ok(Value::Bool(r.unwrap_or_else(|| {
        php_streams::filter::user::filtering_eof(cell.id()).unwrap_or(true)
    })))
}

fn fflush(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    sync_stdio(vm, &cell);
    let r = with_stream(vm, &cell, Stream::flush)?;
    Ok(Value::Bool(r == Some(0)))
}

fn ftell(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let r = with_stream(vm, &cell, |s| lib::ftell(s))?.flatten();
    Ok(r.map_or(Value::Bool(false), int_value))
}

fn fseek(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let off = a.int(1);
    let whence = a.int(2);
    let r = with_stream(vm, &cell, |s| lib::fseek(s, off, whence))?;
    Ok(Value::Int(r.unwrap_or(-1)))
}

fn rewind(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let r = with_stream(vm, &cell, Stream::rewind)?;
    Ok(Value::Bool(r.is_some_and(|r| r != -1)))
}

fn ftruncate(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let size = a.int(1);
    match with_stream(vm, &cell, |s| lib::ftruncate(s, size))? {
        Some(Ok(b)) => Ok(Value::Bool(b)),
        Some(Err(e)) => Err(arg_error(vm, a, &e)),
        None => Ok(Value::Bool(false)),
    }
}

fn flock(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let op = a.int(1);
    if a.has(2) {
        a.set_ref(2, Value::Int(0));
    }
    match with_stream(vm, &cell, |s| lib::flock(s, op))? {
        Some(Ok((ok, wouldblock))) => {
            if wouldblock && a.has(2) {
                a.set_ref(2, Value::Int(1));
            }
            Ok(Value::Bool(ok))
        }
        Some(Err(e)) => Err(arg_error(vm, a, &e)),
        None => Ok(Value::Bool(false)),
    }
}

fn fstat(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let r = with_stream(vm, &cell, lib::fstat)?.flatten();
    Ok(r.map_or(Value::Bool(false), |sb| bridge::from_lib(&sb.to_value())))
}

fn fsync_impl(vm: &mut Vm, a: &Args, data_only: bool) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let r = with_stream(vm, &cell, |s| lib::fsync(s, data_only))?;
    Ok(Value::Bool(r.unwrap_or(false)))
}

fn fsync(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    fsync_impl(vm, a, false)
}

fn fdatasync(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    fsync_impl(vm, a, true)
}

fn fpassthru(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let mut buf = vec![0u8; php_streams::stream::CHUNK_SIZE];
    let mut count = 0usize;
    // `_php_stream_passthru()`: PHPWRITE each chunk as it is read.
    while let Some(Ok(n @ 1..)) = with_stream(vm, &cell, |s| s.read(&mut buf))? {
        vm.output(&buf[..n]);
        count += n;
    }
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn readfile(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let use_path = a.bool(1);
    let ctx = context_arg(vm, a, 2, false)?;
    let opts = REPORT_ERRORS | if use_path { USE_PATH } else { 0 };
    let Some((mut s, _)) = open_stream(vm, a, &path, "rb", opts, ctx.as_ref())? else {
        return Ok(Value::Bool(false));
    };
    let mut buf = vec![0u8; php_streams::stream::CHUNK_SIZE];
    let mut count = 0usize;
    // `php_stream_passthru()`: PHPWRITE each chunk as it is read, so large
    // files never sit in memory.
    while let Ok(n @ 1..) = bridge::run_stream(vm, || s.read(&mut buf))? {
        vm.output(&buf[..n]);
        count += n;
    }
    bridge::run(vm, || s.close())?;
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn file_get_contents(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let use_path = a.bool(1);
    let offset = a.int(3);
    let maxlen = opt_int(a, 4);
    if let Some(m) = maxlen {
        if m < 0 {
            return Err(a.value_error(vm, 5, "must be greater than or equal to 0"));
        }
    }
    let ctx = context_arg(vm, a, 2, false)?;
    let opts = REPORT_ERRORS | if use_path { USE_PATH } else { 0 };
    let Some((mut s, _)) = open_stream(vm, a, &path, "rb", opts, ctx.as_ref())? else {
        return Ok(Value::Bool(false));
    };
    let r = bridge::run(vm, || {
        if s.ops_label() == "STDIO" {
            s.set_option(&mut php_streams::stream::StreamOption::ReadBuffer(
                php_streams::stream::BufferMode::None,
                None,
            ));
        }
        if offset != 0 {
            let whence = if offset > 0 {
                php_streams::stream::SEEK_SET
            } else {
                php_streams::stream::SEEK_END
            };
            if s.seek(offset, whence) < 0 {
                php_streams::diag::warning(format!(
                    "Failed to seek to position {offset} in the stream"
                ));
                s.close();
                return None;
            }
        }
        let max = maxlen.map(|m| usize::try_from(m).unwrap_or(usize::MAX));
        let c = s.copy_to_mem(max).unwrap_or_default();
        s.close();
        Some(c)
    })?;
    Ok(str_or_false(r))
}

/// `file_put_contents()` data, converted before opening.
enum Owned {
    Bytes(Vec<u8>),
    Chunks(Vec<Vec<u8>>),
    Invalid,
    Stream,
}

fn file_put_contents(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let data = a.at(1);
    let flags = a.int(2);
    let src = match &data {
        Value::Resource(r) => Some(res::stream_of_resource(vm, a, r)?),
        _ => None,
    };
    let ctx = context_arg(vm, a, 3, flags & lib::FILE_NO_DEFAULT_CONTEXT != 0)?;
    if path.is_empty() {
        return Err(vm.throw("ValueError", open::EMPTY_PATH_ERROR));
    }
    let owned = match &data {
        Value::Resource(_) => Owned::Stream,
        Value::Array(arr) => {
            let mut chunks = Vec::with_capacity(arr.len());
            for (_, v) in arr.iter() {
                chunks.push(vm.value_to_string(v)?.as_bytes().to_vec());
            }
            Owned::Chunks(chunks)
        }
        Value::Object(o) => {
            let cls = php_vm::object::class_of(o).clone();
            if cls.magic.to_string.is_some() {
                Owned::Bytes(vm.value_to_string(&data)?.as_bytes().to_vec())
            } else {
                Owned::Invalid
            }
        }
        other => Owned::Bytes(vm.value_to_string(other)?.as_bytes().to_vec()),
    };
    crate::userwrap::clear_last_object();
    let r = bridge::run(vm, || {
        let put = match &owned {
            Owned::Bytes(b) => PutData::Bytes(b),
            Owned::Chunks(c) => PutData::Chunks(c),
            Owned::Invalid => PutData::Invalid,
            Owned::Stream => {
                let cell = src.as_ref()?;
                let mut s = cell.get()?;
                return lib::file_put_contents(&path, PutData::Stream(&mut s), flags, ctx.as_ref());
            }
        };
        lib::file_put_contents(&path, put, flags, ctx.as_ref())
    })?;
    if r.is_none() {
        if let Some(e) = php_wrapper::take_exception() {
            return Err(php_url_exception(vm, a, &e));
        }
    }
    Ok(r.map_or(Value::Bool(false), int_value))
}

fn file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let flags = a.int(1);
    let ctx = context_arg(vm, a, 2, flags & lib::FILE_NO_DEFAULT_CONTEXT != 0)?;
    if path.is_empty() && flags & !0x17 == 0 {
        return Err(vm.throw("ValueError", open::EMPTY_PATH_ERROR));
    }
    let r = bridge::run(vm, || lib::file(&path, flags, ctx.as_ref()))?;
    match r {
        Ok(Some(lines)) => {
            let mut arr = ZArray::with_capacity(lines.len());
            for l in lines {
                let _ = arr.make_mut().push(bytes(l));
            }
            Ok(Value::Array(arr))
        }
        Ok(None) => {
            if let Some(e) = php_wrapper::take_exception() {
                return Err(php_url_exception(vm, a, &e));
            }
            Ok(Value::Bool(false))
        }
        Err(e) => Err(arg_error(vm, a, &e)),
    }
}

fn tempnam(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let dir = path_arg(vm, a, 0)?;
    let prefix = path_arg(vm, a, 1)?;
    let r = bridge::run(vm, || temp::tempnam(&dir, &prefix))?;
    Ok(str_or_false(r))
}

fn tmpfile(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = bridge::run(vm, plain::tmpfile)?;
    Ok(r.map_or(Value::Bool(false), |s| res::new_stream(s, None)))
}

fn sys_get_temp_dir(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let r = bridge::run(vm, temp::temp_dir)?;
    Ok(bytes(r))
}

fn umask(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(lib::umask(opt_int(a, 0))))
}

fn unlink(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let ctx = context_arg(vm, a, 1, false)?;
    let r = bridge::run(vm, || lib::unlink(&path, ctx.as_ref()))?;
    Ok(Value::Bool(r))
}

fn rename(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let from = path_arg(vm, a, 0)?;
    let to = path_arg(vm, a, 1)?;
    let ctx = context_arg(vm, a, 2, false)?;
    let r = bridge::run(vm, || lib::rename(&from, &to, ctx.as_ref()))?;
    Ok(Value::Bool(r))
}

fn copy(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let from = path_arg(vm, a, 0)?;
    let to = path_arg(vm, a, 1)?;
    let ctx = context_arg(vm, a, 2, false)?;
    let r = bridge::run(vm, || lib::copy(&from, &to, ctx.as_ref()))?;
    Ok(Value::Bool(r))
}

/// Whether `path` is, byte for byte, one of the request's uploads
/// (`zend_hash_exists(SG(rfc1867_uploaded_files), path)`).
fn is_upload(vm: &Vm, path: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    vm.rfc1867_uploaded_files
        .iter()
        .any(|f| f.as_os_str().as_bytes() == path)
}

/// `is_uploaded_file()` (ext/standard/basic_functions.c).
fn is_uploaded_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    Ok(Value::Bool(is_upload(vm, &path)))
}

/// `move_uploaded_file()` (ext/standard/basic_functions.c): renames the
/// upload (then applies `0666 & ~umask`), or copies it and unlinks the
/// original; the moved file stops being an upload.
fn move_uploaded_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let from = path_arg(vm, a, 0)?;
    let to = path_arg(vm, a, 1)?;
    if !is_upload(vm, &from) {
        return Ok(Value::Bool(false));
    }
    if !bridge::run(vm, || php_streams::paths::check_open_basedir(&to))? {
        return Ok(Value::Bool(false));
    }
    let from_p = std::path::Path::new(std::ffi::OsStr::from_bytes(&from));
    let to_p = std::path::Path::new(std::ffi::OsStr::from_bytes(&to));
    let moved = if std::fs::rename(from_p, to_p).is_ok() {
        let mask = php_streams::sys::current_umask();
        if let Err(e) =
            std::fs::set_permissions(to_p, std::fs::Permissions::from_mode(0o666 & !mask))
        {
            vm.php_warning(&php_streams::sys::error_text(&e))?;
        }
        true
    } else if bridge::run(vm, || {
        lib::copy_file_ctx(
            &from,
            &to,
            php_streams::wrapper::STREAM_DISABLE_OPEN_BASEDIR,
            None,
        )
    })? {
        let _ = std::fs::remove_file(from_p);
        true
    } else {
        false
    };
    if moved {
        vm.rfc1867_uploaded_files
            .retain(|f| f.as_os_str().as_bytes() != from.as_slice());
    } else {
        let mut msg = b"Unable to move \"".to_vec();
        msg.extend_from_slice(&from);
        msg.extend_from_slice(b"\" to \"");
        msg.extend_from_slice(&to);
        msg.push(b'"');
        vm.php_warning_bytes(&msg)?;
    }
    Ok(Value::Bool(moved))
}

fn mkdir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let dir = path_arg(vm, a, 0)?;
    let mode = a.int(1);
    let recursive = a.bool(2);
    let ctx = context_arg(vm, a, 3, false)?;
    let r = bridge::run(vm, || lib::mkdir(&dir, mode, recursive, ctx.as_ref()))?;
    Ok(Value::Bool(r))
}

fn rmdir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let dir = path_arg(vm, a, 0)?;
    let ctx = context_arg(vm, a, 1, false)?;
    let r = bridge::run(vm, || lib::rmdir(&dir, ctx.as_ref()))?;
    Ok(Value::Bool(r))
}

fn realpath(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = path_arg(vm, a, 0)?;
    let r = bridge::run(vm, || lib::realpath(&p))?;
    Ok(str_or_false(r))
}

fn readlink(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = path_arg(vm, a, 0)?;
    let r = bridge::run(vm, || lib::readlink(&p))?;
    Ok(str_or_false(r))
}

fn linkinfo(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = path_arg(vm, a, 0)?;
    if p.is_empty() {
        return Err(a.value_error(vm, 1, "must not be empty"));
    }
    let r = bridge::run(vm, || lib::linkinfo(&p))?;
    Ok(Value::Int(r))
}

fn symlink(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let t = path_arg(vm, a, 0)?;
    let l = path_arg(vm, a, 1)?;
    let r = bridge::run(vm, || lib::symlink(&t, &l))?;
    Ok(Value::Bool(r))
}

fn link(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let t = path_arg(vm, a, 0)?;
    let l = path_arg(vm, a, 1)?;
    let r = bridge::run(vm, || lib::link(&t, &l))?;
    Ok(Value::Bool(r))
}

fn fnmatch(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let pat = path_arg(vm, a, 0)?;
    let name = path_arg(vm, a, 1)?;
    let flags = i32::try_from(a.int(2)).unwrap_or(0);
    let r = bridge::run(vm, || lib::fnmatch(&pat, &name, flags))?;
    Ok(Value::Bool(r))
}

fn basename(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = str_arg(a, 0);
    let suffix = str_arg(a, 1);
    Ok(bytes(pathname::basename(&p, &suffix)))
}

fn dirname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = str_arg(a, 0);
    let levels = a.int(1);
    if levels == 1 {
        return Ok(bytes(pathname::dirname(&p)));
    }
    match pathname::dirname_levels(&p, levels) {
        Some(d) => Ok(bytes(d)),
        None => Err(a.value_error(vm, 2, "must be greater than or equal to 1")),
    }
}

fn pathinfo(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = str_arg(a, 0);
    let opt = if a.has(1) {
        a.int(1)
    } else {
        pathname::PATHINFO_ALL
    };
    if opt == pathname::PATHINFO_ALL {
        let info = pathname::pathinfo(&p);
        let mut arr = ZArray::new();
        if let Some(d) = info.dirname {
            arr.make_mut().insert("dirname", bytes(d));
        }
        arr.make_mut().insert("basename", bytes(info.basename));
        if let Some(e) = info.extension {
            arr.make_mut().insert("extension", bytes(e));
        }
        arr.make_mut().insert("filename", bytes(info.filename));
        return Ok(Value::Array(arr));
    }
    match pathname::pathinfo_part(&p, opt) {
        Ok(s) => Ok(bytes(s)),
        Err(e) => Err(a.value_error(vm, 2, e.message())),
    }
}

// ---------------------------------------------------------------------------
// CSV, scanf, printf
// ---------------------------------------------------------------------------

fn std_error(vm: &mut Vm, a: &Args, e: &php_stdstr::error::StdError) -> Throw {
    let mut msg = if e.func.is_some() {
        format!("{}(): {}", fname(a), e.message)
    } else {
        e.message.clone()
    };
    // `%c` of a NUL byte: the message keeps the NUL (zend_string).
    if msg.ends_with("conversion character \"") {
        msg.push_str("\0\"");
    }
    vm.throw(e.kind.class_name(), &msg)
}

fn notices(vm: &mut Vm, ns: Vec<php_stdstr::error::Notice>) -> VmResult<()> {
    use php_stdstr::error::Level;
    for n in ns {
        match n.level {
            Level::Warning => vm.php_warning(&n.message)?,
            Level::Notice => vm.php_notice(&n.message)?,
            Level::Deprecated => vm.php_deprecated(&n.message)?,
        }
    }
    Ok(())
}

fn csv_opts(vm: &mut Vm, a: &Args, first: usize) -> VmResult<php_stdstr::csv::CsvOptions> {
    let name: &'static str = if fname(a) == "fputcsv" {
        "fputcsv"
    } else {
        "fgetcsv"
    };
    let sep = a.get(first).map(|_| str_arg(a, first));
    let enc = a.get(first + 1).map(|_| str_arg(a, first + 1));
    let esc = a.get(first + 2).map(|_| str_arg(a, first + 2));
    let mut ns = Vec::new();
    let arg_no = u32::try_from(first + 1).unwrap_or(0);
    let r = php_stdstr::csv::csv_options(
        name,
        arg_no,
        sep.as_deref(),
        enc.as_deref(),
        esc.as_deref(),
        &mut ns,
    );
    notices(vm, ns)?;
    r.map_err(|e| std_error(vm, a, &e))
}

fn fgetcsv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let length = opt_int(a, 1);
    if let Err(e) = php_stdstr::csv::fgetcsv_length(length) {
        return Err(std_error(vm, a, &e));
    }
    let opts = csv_opts(vm, a, 2)?;
    let r = with_stream(vm, &cell, |s| {
        lib::fgetcsv(s, length, |line, more| {
            php_stdstr::csv::fgetcsv_parse(line, &opts, more)
        })
    })?;
    match r {
        Some(Ok(Some(fields))) => {
            let mut arr = ZArray::new();
            match fields {
                Some(f) => {
                    for x in f {
                        let _ = arr.make_mut().push(bytes(x));
                    }
                }
                None => {
                    let _ = arr.make_mut().push(Value::Null);
                }
            }
            Ok(Value::Array(arr))
        }
        Some(Err(e)) => Err(arg_error(vm, a, &e)),
        _ => Ok(Value::Bool(false)),
    }
}

fn fputcsv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let fields = a.array(1);
    let opts = csv_opts(vm, a, 2)?;
    let eol = a.get(5).map(|_| str_arg(a, 5));
    let mut strs = Vec::with_capacity(fields.len());
    for (_, v) in fields.iter() {
        strs.push(vm.value_to_string(v)?.as_bytes().to_vec());
    }
    let line = php_stdstr::csv::fputcsv_line(&strs, &opts, eol.as_deref());
    sync_stdio(vm, &cell);
    let r = with_stream(vm, &cell, |s| lib::fputcsv_write(s, &line))?.flatten();
    Ok(r.map_or(Value::Bool(false), int_value))
}

fn scan_value(v: php_stdstr::scanf::ScanValue) -> Value {
    use php_stdstr::scanf::ScanValue;
    match v {
        ScanValue::Int(i) => Value::Int(i),
        ScanValue::Float(f) => Value::Float(f),
        ScanValue::Str(s) => bytes(s),
    }
}

fn fscanf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    use php_stdstr::scanf::{sscanf, SscanfResult};
    // fscanf() still fetches the stream as a "File-Handle" resource.
    let r = res::resource_arg(vm, a, 0)?;
    let cell = match res::cell_of(&r) {
        Some(c) if !r.is_closed() && !c.is_closed() => c,
        _ => {
            return Err(vm.throw(
                "TypeError",
                "fscanf(): supplied resource is not a valid File-Handle resource",
            ))
        }
    };
    let format = str_arg(a, 1);
    let nvars = a.values.len().saturating_sub(2);
    let line = with_stream(vm, &cell, |s| s.get_line(None))?.flatten();
    let Some(line) = line else {
        return Ok(Value::Bool(false));
    };
    match sscanf(&line, &format, nvars) {
        Ok(SscanfResult::Array(None)) => Ok(Value::Null),
        Ok(SscanfResult::Array(Some(vals))) => {
            let mut arr = ZArray::new();
            for v in vals {
                let _ = arr.make_mut().push(v.map_or(Value::Null, scan_value));
            }
            Ok(Value::Array(arr))
        }
        Ok(SscanfResult::Assigned { values, count }) => {
            for (i, v) in values.into_iter().enumerate() {
                if let Some(v) = v {
                    if let Some(Value::Reference(r)) = a.values.get(i + 2) {
                        r.set(scan_value(v));
                    }
                }
            }
            Ok(Value::Int(count))
        }
        Err(e) => Err(std_error(vm, a, &e)),
    }
}

/// A pre-converted printf argument.
struct PArg {
    int: i64,
    float: f64,
    s: Vec<u8>,
    exact: Option<i64>,
}

impl php_support::fmt::printf::FormatArg for PArg {
    fn as_int(&self) -> i64 {
        self.int
    }
    fn as_float(&self) -> f64 {
        self.float
    }
    fn as_str(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.s)
    }
    fn exact_int(&self) -> Option<i64> {
        self.exact
    }
}

fn format(vm: &mut Vm, fmt: &[u8], vals: &[Value], variadic: bool) -> VmResult<Vec<u8>> {
    use php_support::fmt::printf::{php_formatted_print, ArgSource, FormatError, FormatOptions};
    let mut args = Vec::with_capacity(vals.len());
    for v in vals {
        let v = v.deref();
        let s = match &v {
            Value::Array(_) => b"Array".to_vec(),
            _ => vm.value_to_string(&v)?.as_bytes().to_vec(),
        };
        args.push(PArg {
            int: vm.value_to_int(&v)?,
            float: vm.value_to_float(&v)?,
            s,
            exact: v.as_int(),
        });
    }
    let refs: Vec<&dyn php_support::fmt::printf::FormatArg> = args
        .iter()
        .map(|a| a as &dyn php_support::fmt::printf::FormatArg)
        .collect();
    let mut diags: Vec<php_support::Diagnostic> = Vec::new();
    let opts = FormatOptions {
        source: if variadic {
            ArgSource::Variadic(2)
        } else {
            ArgSource::Array
        },
        decimal_point: php_support::fmt::locale::decimal_point(),
    };
    let r = php_formatted_print(fmt, &refs, opts, &mut diags);
    for d in diags {
        let lvl = i64::from(d.level.0);
        if d.docref {
            match lvl {
                2 => vm.php_warning(&d.message)?,
                8 => vm.php_notice(&d.message)?,
                _ => vm.php_deprecated(&d.message)?,
            }
        } else {
            vm.raise(lvl, &d.message)?;
        }
    }
    match r {
        Ok(v) => Ok(v),
        Err(FormatError::ValueError(m)) => Err(vm.throw("ValueError", &m)),
        Err(FormatError::ArgumentCountError(m)) => Err(vm.throw("ArgumentCountError", &m)),
        Err(FormatError::Fatal(m)) => {
            vm.raise(1, &m)?;
            Ok(Vec::new())
        }
        Err(FormatError::MemoryLimit(over)) => Err(over.into()),
    }
}

fn fprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let f = str_arg(a, 1);
    let vals: Vec<Value> = a.values.get(2..).map(<[Value]>::to_vec).unwrap_or_default();
    let out = format(vm, &f, &vals, true)?;
    write_formatted(vm, &cell, &out)
}

fn vfprintf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cell = stream_arg(vm, a, 0)?;
    let f = str_arg(a, 1);
    let vals: Vec<Value> = a.array(2).values().cloned().collect();
    let out = format(vm, &f, &vals, false)?;
    write_formatted(vm, &cell, &out)
}

fn write_formatted(vm: &mut Vm, cell: &Rc<StreamCell>, out: &[u8]) -> VmResult<Value> {
    sync_stdio(vm, cell);
    let _ = with_stream(vm, cell, |s| s.write(out))?;
    Ok(Value::Int(i64::try_from(out.len()).unwrap_or(i64::MAX)))
}

fn get_meta_tags(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let use_path = a.bool(1);
    let opts = REPORT_ERRORS | if use_path { USE_PATH } else { 0 };
    let Some((mut s, _)) = open_stream(vm, a, &path, "rb", opts, None)? else {
        return Ok(Value::Bool(false));
    };
    let tags = bridge::run(vm, || {
        let t = crate::meta::meta_tags(&mut s);
        s.close();
        t
    })?;
    let mut arr = ZArray::new();
    for (k, v) in tags {
        arr.make_mut()
            .insert(php_runtime::ArrayKey::Str(k.as_slice().into()), bytes(v));
    }
    Ok(Value::Array(arr))
}

/// Registers the functions of this module.
pub fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "fopen(string $filename, string $mode, bool $use_include_path = false, $context = null)",
        fopen,
    )
    .function_static("fclose($stream): bool", fclose)
    .function_static("fread($stream, int $length): string|false", fread)
    .function_static("fwrite($stream, string $data, ?int $length = null): int|false", fwrite)
    .function_static("fputs($stream, string $data, ?int $length = null): int|false", fwrite)
    .function_static("fgets($stream, ?int $length = null): string|false", fgets)
    .function_static("fgetc($stream): string|false", fgetc)
    .function_static("feof($stream): bool", feof)
    .function_static("fflush($stream): bool", fflush)
    .function_static("ftell($stream): int|false", ftell)
    .function_static("fseek($stream, int $offset, int $whence = SEEK_SET): int", fseek)
    .function_static("rewind($stream): bool", rewind)
    .function_static("ftruncate($stream, int $size): bool", ftruncate)
    .function_static("flock($stream, int $operation, &$would_block = null): bool", flock)
    .function_static("fstat($stream): array|false", fstat)
    .function_static("fsync($stream): bool", fsync)
    .function_static("fdatasync($stream): bool", fdatasync)
    .function_static("fpassthru($stream): int", fpassthru)
    .function_static(
        "readfile(string $filename, bool $use_include_path = false, $context = null): int|false",
        readfile,
    )
    .function_static(
        "file_get_contents(string $filename, bool $use_include_path = false, $context = null, int $offset = 0, ?int $length = null): string|false",
        file_get_contents,
    )
    .function_static(
        "file_put_contents(string $filename, mixed $data, int $flags = 0, $context = null): int|false",
        file_put_contents,
    )
    .function_static("file(string $filename, int $flags = 0, $context = null): array|false", file)
    .function_static("tempnam(string $directory, string $prefix): string|false", tempnam)
    .function_static("tmpfile()", tmpfile)
    .function_static("sys_get_temp_dir(): string", sys_get_temp_dir)
    .function_static("umask(?int $mask = null): int", umask)
    .function_static("unlink(string $filename, $context = null): bool", unlink)
    .function_static("rename(string $from, string $to, $context = null): bool", rename)
    .function_static("copy(string $from, string $to, $context = null): bool", copy)
    .function_static("is_uploaded_file(string $filename): bool", is_uploaded_file)
    .function_static(
        "move_uploaded_file(string $from, string $to): bool",
        move_uploaded_file,
    )
    .function_static(
        "mkdir(string $directory, int $permissions = 511, bool $recursive = false, $context = null): bool",
        mkdir,
    )
    .function_static("rmdir(string $directory, $context = null): bool", rmdir)
    .function_static("realpath(string $path): string|false", realpath)
    .function_static("readlink(string $path): string|false", readlink)
    .function_static("linkinfo(string $path): int|false", linkinfo)
    .function_static("symlink(string $target, string $link): bool", symlink)
    .function_static("link(string $target, string $link): bool", link)
    .function_static("fnmatch(string $pattern, string $filename, int $flags = 0): bool", fnmatch)
    .function_static("basename(string $path, string $suffix = \"\"): string", basename)
    .function_static("dirname(string $path, int $levels = 1): string", dirname)
    .function_static("pathinfo(string $path, int $flags = PATHINFO_ALL): array|string", pathinfo)
    .function_static(
        "fgetcsv($stream, ?int $length = null, string $separator = UNKNOWN, string $enclosure = UNKNOWN, string $escape = UNKNOWN): array|false",
        fgetcsv,
    )
    .function_static(
        "fputcsv($stream, array $fields, string $separator = UNKNOWN, string $enclosure = UNKNOWN, string $escape = UNKNOWN, string $eol = UNKNOWN): int|false",
        fputcsv,
    )
    .function_static(
        "fscanf($stream, string $format, mixed &...$vars): array|int|false|null",
        fscanf,
    )
    .function_static(
        "get_meta_tags(string $filename, bool $use_include_path = false): array|false",
        get_meta_tags,
    )
    .function_static("fprintf($stream, string $format, mixed ...$values): int", fprintf)
    .function_static("vfprintf($stream, string $format, array $values): int", vfprintf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_arg() {
        use php_support::fmt::printf::FormatArg;
        let p = PArg {
            int: 3,
            float: 3.5,
            s: b"x".to_vec(),
            exact: Some(3),
        };
        assert_eq!(p.as_int(), 3);
        assert_eq!(&*p.as_str(), b"x");
    }
}
