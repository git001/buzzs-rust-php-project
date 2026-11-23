// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/fileinfo` for the phprust VM: binds the `php-fileinfo` library (the
//! system libmagic) through the `php_vm::ext` extension API.
//!
//! Registers the `finfo` class (`__construct`, `file`, `buffer`,
//! `set_flags`), the procedural `finfo_*` functions, `mime_content_type()`
//! and the `FILEINFO_*` constants.  Paths are opened through the streams
//! layer of `php-ext-streams` (wrappers, contexts, `open_basedir`), plain
//! files are then handed to libmagic by descriptor and anything else is
//! read into memory.
//!
//! A `finfo` object carries its libmagic cookie as native state; an object
//! whose constructor failed (or was never run) has none and every method
//! throws `Error: Invalid finfo object`.

// Native handlers share one `fn(&mut Vm, &mut Args) -> VmResult<Value>`
// signature, even those that cannot fail.
#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "test code"))]
#![allow(clippy::unnecessary_wraps)]

mod cdf;

use std::fs::File;

use php_ext_streams::{bridge, files, res};
use php_fileinfo::{CallStyle, Finfo, Thrown, Warning};
use php_runtime::{Obj, Value, ZStr};
use php_streams::context::ContextRef;
use php_streams::diag::{self, Severity};
use php_streams::plain::StdioOps;
use php_streams::stream::Stream;
use php_streams::wrapper::REPORT_ERRORS;
use php_vm::ext::{
    acc, class_flags, Args, ClassBuilder, Extension, InfoItem, ModuleBuilder, Throw, VmResult,
};
use php_vm::Vm;

/// The `fileinfo` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fileinfo;

/// The native state of a `finfo` object (`None` once a constructor call
/// failed).
struct State(Option<Finfo>);

/// `SEEK_SET`.
const SEEK_SET: i32 = 0;

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Where warnings go: emitted normally, or — inside `finfo::__construct()`,
/// which runs with `EH_THROW` — the first one becomes an `Exception`.
struct Reporter {
    throw_first: bool,
    thrown: Option<Throw>,
}

impl Reporter {
    fn new(throw_first: bool) -> Self {
        Self {
            throw_first,
            thrown: None,
        }
    }

    /// Reports one `E_WARNING` docref message (without the prefix).
    fn warn(&mut self, vm: &mut Vm, msg: &str) -> VmResult<()> {
        if !self.throw_first {
            return vm.php_warning(msg);
        }
        if self.thrown.is_none() {
            let t = vm.throw("Exception", &format!("finfo::__construct(): {msg}"));
            self.thrown = Some(t);
        }
        Ok(())
    }

    /// Reports the library's collected warnings.
    fn warnings(&mut self, vm: &mut Vm, list: Vec<Warning>) -> VmResult<()> {
        for w in list {
            self.warn(vm, &w.message)?;
        }
        Ok(())
    }

    /// Reports the diagnostics queued by the streams library.
    fn stream_diags(&mut self, vm: &mut Vm) -> VmResult<()> {
        for d in diag::take() {
            let text = d.text();
            match d.severity {
                Severity::Warning => self.warn(vm, &text)?,
                Severity::Notice => vm.php_notice(&text)?,
                Severity::Deprecated => vm.php_deprecated(&text)?,
                Severity::Error => vm.raise(1, &text)?,
            }
        }
        Ok(())
    }
}

/// Runs a library call collecting its warnings.
fn collect<T>(f: impl FnOnce(&mut dyn FnMut(Warning)) -> T) -> (T, Vec<Warning>) {
    let mut list = Vec::new();
    let r = f(&mut |w| list.push(w));
    (r, list)
}

/// Throws a library-built exception.
fn thrown(vm: &mut Vm, t: Thrown) -> Throw {
    match t {
        Thrown::ValueError(m) => vm.throw("ValueError", &m),
        Thrown::TypeError(m) => vm.throw("TypeError", &m),
        Thrown::Error(m) => vm.throw("Error", &m),
    }
}

/// An `int` parameter whose stub default is `FILEINFO_NONE`.
fn flags_arg(a: &Args, n: usize) -> i64 {
    if a.has(n) {
        a.int(n)
    } else {
        php_fileinfo::FILEINFO_NONE
    }
}

/// The `finfo` object (`$this`, or argument #1 procedurally) and the index
/// of the first argument after it.
fn target(a: &Args) -> (Obj, usize, CallStyle) {
    match &a.this {
        Some(o) => (o.clone(), 0, CallStyle::Method),
        None => match a.at(0) {
            Value::Object(o) => (o, 1, CallStyle::Function),
            // ZPP (`finfo $finfo`) guarantees an object.
            _ => unreachable!("finfo argument is not an object"),
        },
    }
}

/// Runs `f` on the object's cookie, or throws "Invalid finfo object".
fn with_finfo<R>(vm: &mut Vm, o: &Obj, f: impl FnOnce(&mut Finfo) -> R) -> VmResult<R> {
    match php_vm::object::internal::<State, _>(o, |s| s.0.as_mut().map(f)) {
        Some(Some(r)) => Ok(r),
        _ => Err(vm.throw("Error", php_fileinfo::INVALID_OBJECT)),
    }
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// `finfo_open()` / `finfo::__construct()` minus the object handling:
/// argument checks, `open_basedir`, path expansion and loading.
fn open(vm: &mut Vm, a: &Args, style: CallStyle, rep: &mut Reporter) -> VmResult<Option<Finfo>> {
    let flags = flags_arg(a, 0);
    let database = a.opt(1).map(|v| match v {
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    });
    if let Some(db) = &database {
        php_fileinfo::check_magic_database(style, db).map_err(|t| thrown(vm, t))?;
    }
    let mut resolved = None;
    if let Some(db) = database.filter(|d| !d.is_empty()) {
        let allowed = bridge::enter(vm, || php_streams::paths::check_open_basedir(&db));
        rep.stream_diags(vm)?;
        if !allowed {
            return Ok(None);
        }
        let cwd = std::env::current_dir()
            .map(|p| std::os::unix::ffi::OsStringExt::into_vec(p.into_os_string()))
            .unwrap_or_default();
        match php_fileinfo::expand_filepath(&db, &cwd) {
            Some(p) => resolved = Some(p),
            None => return Ok(None),
        }
    }
    let (finfo, warnings) = collect(|w| Finfo::open(flags, resolved.as_deref(), w));
    rep.warnings(vm, warnings)?;
    Ok(finfo)
}

fn finfo_open(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut rep = Reporter::new(false);
    let Some(finfo) = open(vm, a, CallStyle::Function, &mut rep)? else {
        return Ok(Value::Bool(false));
    };
    let class = vm
        .find_class(b"finfo")
        .ok_or_else(|| vm.throw("Error", "Class \"finfo\" not found"))?;
    let o = vm.new_object(&class)?;
    php_vm::object::set_internal(&o, State(Some(finfo)));
    Ok(Value::Object(o))
}

fn construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    // A second constructor call first drops the old cookie.
    php_vm::object::internal::<State, _>(&o, |s| s.0 = None);
    let mut rep = Reporter::new(true);
    let finfo = open(vm, a, CallStyle::Method, &mut rep)?;
    if let Some(t) = rep.thrown.take() {
        return Err(t);
    }
    match finfo {
        Some(f) => {
            php_vm::object::set_internal(&o, State(Some(f)));
            Ok(Value::Null)
        }
        None => Err(vm.throw("Exception", php_fileinfo::CONSTRUCTOR_FAILED)),
    }
}

// ---------------------------------------------------------------------------
// Methods / functions
// ---------------------------------------------------------------------------

fn finfo_close(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(true))
}

fn set_flags(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, base, _) = target(a);
    let flags = a.int(base);
    with_finfo(vm, &o, |f| f.set_flags(flags))?;
    Ok(Value::Bool(true))
}

fn buffer(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, base, _) = target(a);
    // ZPP (`r!`) runs before the deprecation.
    if let Some(v) = a.get(base + 2) {
        if !matches!(v, Value::Null | Value::Resource(_)) {
            return Err(context_type_error(vm, a, base + 2, &v));
        }
    }
    if a.has(base + 2) {
        vm.php_deprecated(php_fileinfo::buffer_context_deprecation())?;
    }
    let data = a.str(base);
    let flags = flags_arg(a, base + 1);
    let effective = effective_flags(vm, &o, flags)?;
    reserve_cdf(vm, effective, data.as_bytes())?;
    let (r, warnings) = with_finfo(vm, &o, |f| collect(|w| f.buffer(data.as_bytes(), flags, w)))?;
    Reporter::new(false).warnings(vm, warnings)?;
    Ok(r.map_or(Value::Bool(false), |d| Value::Str(ZStr::from(d))))
}

/// The display name of `finfo_<name>()` / `finfo::<name>()`.
fn func_name(style: CallStyle, name: &str) -> String {
    match style {
        CallStyle::Function => format!("finfo_{name}"),
        CallStyle::Method => format!("finfo::{name}"),
    }
}

/// ZPP's `TypeError` for a `$context` (0-based argument `n`) that is
/// neither a resource nor `null`.
fn context_type_error(vm: &mut Vm, a: &Args, n: usize, v: &Value) -> Throw {
    a.type_error(
        vm,
        n + 1,
        &format!(
            "must be of type resource or null, {} given",
            bridge::given_type(v)
        ),
    )
}

fn file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, base, style) = target(a);
    let path = a.str(base);
    let path = path.as_bytes();
    let check = php_fileinfo::check_filename(style, path);
    if path.contains(&0) {
        if let Err(t) = check {
            return Err(thrown(vm, t));
        }
    }
    with_finfo(vm, &o, |_| ())?;
    check.map_err(|t| thrown(vm, t))?;
    let ctx = context_arg(vm, a, base + 2, style)?;
    let flags = flags_arg(a, base + 1);
    let effective = effective_flags(vm, &o, flags)?;
    let r = from_path(vm, a, path, ctx.as_ref(), effective, &mut |vm, run| {
        with_finfo(vm, &o, |f| collect(|w| run(f, flags, w)))
    })?;
    Ok(r.map_or(Value::Bool(false), |d| Value::Str(ZStr::from(d))))
}

/// `php_stream_context_from_zval()` on the `$context` argument `n`
/// (`null` is the default context), with the messages naming
/// `finfo_file()` or `finfo::file()`.
fn context_arg(vm: &mut Vm, a: &Args, n: usize, style: CallStyle) -> VmResult<Option<ContextRef>> {
    match a.at(n) {
        Value::Null => Ok(res::ctx_cell_of(&res::default_context()).map(|c| c.ctx.clone())),
        Value::Resource(r) => match res::ctx_cell_of(&r) {
            Some(c) => Ok(Some(c.ctx.clone())),
            None => Err(vm.throw(
                "TypeError",
                &format!(
                    "{}(): supplied resource is not a valid Stream-Context resource",
                    func_name(style, "file")
                ),
            )),
        },
        other => Err(context_type_error(vm, a, n, &other)),
    }
}

/// How [`from_path`] reaches a cookie: `get(vm, run)` calls `run` on it with
/// the per-call flags and a warning sink.
type Identify<'a> = dyn FnMut(
        &mut Vm,
        &mut dyn FnMut(&mut Finfo, i64, &mut dyn FnMut(Warning)) -> Option<Vec<u8>>,
    ) -> VmResult<(Option<Vec<u8>>, Vec<Warning>)>
    + 'a;

/// What libmagic should examine once the stream is open.
enum Source {
    /// A plain file, re-opened by descriptor.
    File(File),
    /// Any other stream's content (up to `MAGIC_PARAM_BYTES_MAX`).
    Bytes(Vec<u8>),
    /// A directory.
    Directory,
}

/// `php_fileinfo_from_path()`: opens `path` through the wrappers (which
/// report open failures), answers `"directory"` for directories and
/// otherwise lets libmagic examine the content.
fn from_path(
    vm: &mut Vm,
    a: &Args,
    path: &[u8],
    ctx: Option<&ContextRef>,
    flags: i64,
    get: &mut Identify<'_>,
) -> VmResult<Option<Vec<u8>>> {
    let Some((mut stream, _)) = files::open_stream(vm, a, path, "rb", REPORT_ERRORS, ctx)? else {
        return Ok(None);
    };
    let source = bridge::run(vm, || source_of(&mut stream))?;
    bridge::run(vm, || stream.close())?;
    let (r, warnings) = match source {
        Source::Directory => return Ok(Some(php_fileinfo::DIRECTORY.to_vec())),
        Source::File(file) => {
            reserve_cdf(vm, flags, &file)?;
            get(vm, &mut |f, flags, w| f.file_handle(&file, flags, w))?
        }
        Source::Bytes(data) => {
            reserve_cdf(vm, flags, data.as_slice())?;
            get(vm, &mut |f, flags, w| f.buffer(&data, flags, w))?
        }
    };
    Reporter::new(false).warnings(vm, warnings)?;
    Ok(r)
}

/// Classifies an open stream (see [`Source`]).
fn source_of(stream: &mut Stream) -> Source {
    if stream.stat().is_some_and(|s| s.is_dir()) {
        return Source::Directory;
    }
    if !stream.is_filtered() && stream.unread_bytes() == 0 {
        if let Some(fd) = stream.ops_as::<StdioOps>().map(StdioOps::fd) {
            if let Ok(file) = File::open(format!("/proc/self/fd/{fd}")) {
                return Source::File(file);
            }
        }
    }
    Source::Bytes(read_head(stream))
}

/// Up to `MAGIC_PARAM_BYTES_MAX` (7 MiB) bytes from the stream's position.
fn read_head(stream: &mut Stream) -> Vec<u8> {
    let max = 7 * 1024 * 1024;
    let mut data = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while data.len() < max {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buf[..n.min(max - data.len())]),
        }
    }
    data
}

/// The flags libmagic runs with: the per-call `$flags` when non-zero,
/// otherwise the object's.
fn effective_flags(vm: &mut Vm, o: &Obj, flags: i64) -> VmResult<i64> {
    if flags != 0 {
        return Ok(flags);
    }
    with_finfo(vm, o, |f| i64::from(f.flags()))
}

/// Charges the memory the bundled libmagic's CDF reader would `emalloc()`
/// for `src` (see [`cdf`]) against `memory_limit`; over it, the fatal
/// "Allowed memory size of N bytes exhausted" PHP raises.  `file_trycdf()`
/// is skipped for `MAGIC_APPLE`/`MAGIC_EXTENSION`.
fn reserve_cdf(vm: &mut Vm, flags: i64, src: &(impl cdf::Source + ?Sized)) -> VmResult<()> {
    if flags & (php_fileinfo::FILEINFO_APPLE | php_fileinfo::FILEINFO_EXTENSION) != 0 {
        return Ok(());
    }
    for bytes in cdf::allocations(src) {
        if let Err(over) = php_memlimit::try_reserve(bytes) {
            // Raised here; keep the VM's post-call poll from repeating it.
            let _ = php_memlimit::take();
            return Err(vm.fatal(&over.to_string()));
        }
    }
    Ok(())
}

fn mime_content_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = match a.at(0) {
        Value::Str(s) => {
            let path = s.as_bytes();
            php_fileinfo::check_mime_content_type_filename(path).map_err(|t| thrown(vm, t))?;
            let (finfo, warnings) =
                collect(|w| Finfo::open(php_fileinfo::FILEINFO_MIME_TYPE, None, w));
            Reporter::new(false).warnings(vm, warnings)?;
            let Some(finfo) = finfo else {
                return Ok(Value::Bool(false));
            };
            let mut finfo = Some(finfo);
            let ctx = res::ctx_cell_of(&res::default_context()).map(|c| c.ctx.clone());
            from_path(
                vm,
                a,
                path,
                ctx.as_ref(),
                php_fileinfo::FILEINFO_MIME_TYPE,
                &mut |_vm, run| {
                    let f = finfo.as_mut().ok_or(Throw::Fatal)?;
                    Ok(collect(|w| run(f, 0, w)))
                },
            )?
        }
        Value::Resource(r) => {
            let cell = res::stream_of_resource(vm, a, &r)?;
            // Rewind, read, restore the position (`magic_stream()`).
            let data = bridge::run(vm, || {
                cell.get().map(|mut s| {
                    let pos = s.tell();
                    s.seek(0, SEEK_SET);
                    let data = read_head(&mut s);
                    s.seek(pos, SEEK_SET);
                    data
                })
            })?
            .unwrap_or_default();
            reserve_cdf(vm, php_fileinfo::FILEINFO_MIME_TYPE, data.as_slice())?;
            let (finfo, warnings) =
                collect(|w| Finfo::open(php_fileinfo::FILEINFO_MIME_TYPE, None, w));
            Reporter::new(false).warnings(vm, warnings)?;
            let Some(mut finfo) = finfo else {
                return Ok(Value::Bool(false));
            };
            let (out, warnings) = collect(|w| finfo.buffer(&data, 0, w));
            Reporter::new(false).warnings(vm, warnings)?;
            out
        }
        other => {
            let name = bridge::given_type(&other);
            return Err(thrown(
                vm,
                php_fileinfo::mime_content_type_type_error(&name),
            ));
        }
    };
    Ok(r.map_or(Value::Bool(false), |d| Value::Str(ZStr::from(d))))
}

/// `PHP_MINFO(fileinfo)`: the `phpinfo()` section.
fn info(_vm: &mut Vm) -> Vec<InfoItem> {
    vec![
        InfoItem::TableStart,
        InfoItem::Row(vec!["fileinfo support".into(), "enabled".into()]),
        InfoItem::Row(vec!["libmagic".into(), php_fileinfo::libmagic_version()]),
        InfoItem::TableEnd,
    ]
}

impl Extension for Fileinfo {
    fn name(&self) -> &'static str {
        "fileinfo"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for &(name, value) in php_fileinfo::CONSTANTS {
            m.constant(name, Value::Int(value));
        }
        ClassBuilder::new("finfo")
            .flags(class_flags::NO_CLONE | class_flags::NOT_SERIALIZABLE)
            .method(
                "__construct(int $flags = FILEINFO_NONE, ?string $magic_database = null)",
                acc::PUBLIC,
                construct,
            )
            .method(
                "file(string $filename, int $flags = FILEINFO_NONE, $context = null): string|false",
                acc::PUBLIC,
                file,
            )
            .method(
                "buffer(string $string, int $flags = FILEINFO_NONE, $context = null): string|false",
                acc::PUBLIC,
                buffer,
            )
            .method("set_flags(int $flags): true", acc::PUBLIC, set_flags)
            .register(m.vm());
        m.function_static(
            "finfo_open(int $flags = FILEINFO_NONE, ?string $magic_database = null): finfo|false",
            finfo_open,
        )
        .function_static("finfo_close(finfo $finfo): true", finfo_close)
        .deprecated("8.5", "as finfo objects are freed automatically")
        .function_static("finfo_set_flags(finfo $finfo, int $flags): true", set_flags)
        .function_static(
            "finfo_file(finfo $finfo, string $filename, int $flags = FILEINFO_NONE, $context = null): string|false",
            file,
        )
        .function_static(
            "finfo_buffer(finfo $finfo, string $string, int $flags = FILEINFO_NONE, $context = null): string|false",
            buffer,
        )
        .function_static("mime_content_type($filename): string|false", mime_content_type)
        .info(info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(func_name(CallStyle::Function, "file"), "finfo_file");
        assert_eq!(func_name(CallStyle::Method, "buffer"), "finfo::buffer");
    }

    #[test]
    fn flags_default_to_none() {
        let a = Args::default();
        assert_eq!(flags_arg(&a, 1), php_fileinfo::FILEINFO_NONE);
    }

    #[test]
    fn plain_files_are_handed_over_by_descriptor() {
        let dir = std::env::temp_dir().join(format!("php-ext-fileinfo-{}", std::process::id()));
        assert!(std::fs::create_dir_all(&dir).is_ok());
        let file = dir.join("x.txt");
        assert!(std::fs::write(&file, b"hello").is_ok());
        let open = |p: &std::path::Path| {
            php_streams::plain::fopen(p.as_os_str().as_encoded_bytes(), "rb", 0, &mut None).ok()
        };
        if let Some(mut s) = open(&file) {
            assert!(matches!(source_of(&mut s), Source::File(_)));
        }
        if let Some(mut s) = open(&dir) {
            assert!(matches!(source_of(&mut s), Source::Directory));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
