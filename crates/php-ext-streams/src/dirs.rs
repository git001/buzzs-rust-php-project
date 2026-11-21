// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/dir.c`: directory handles, `scandir()`, `glob()` and the
//! `Directory` class.

use std::cell::RefCell;
use std::rc::Rc;

use php_runtime::{ArrayKey, Resource, Value, ZArray, ZStr};
use php_streams::open::{self, ScandirSort};
use php_streams::stream::flags;
use php_streams::wrapper::REPORT_ERRORS;
use php_streams::{glob, paths, state};
use php_vm::class::Class;
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::bridge::{self, bytes};
use crate::res::{self, context_arg, fname, path_arg, StreamCell};

thread_local! {
    /// `DIRG(default_dir)`: the last opened directory handle.
    static DEFAULT_DIR: RefCell<Option<Resource>> = const { RefCell::new(None) };
    static DIRECTORY_CLASS: RefCell<Option<Rc<Class>>> = const { RefCell::new(None) };
}

fn set_default_dir(r: Option<Resource>) {
    DEFAULT_DIR.with(|d| *d.borrow_mut() = r);
}

/// Request shutdown: no directory is the default any more (php-src sets
/// `DIRG(default_dir)` to NULL in `PHP_RINIT(dir)`).
pub(crate) fn reset_request() {
    set_default_dir(None);
}

fn is_default_dir(r: &Resource) -> bool {
    DEFAULT_DIR.with(|d| {
        d.borrow()
            .as_ref()
            .is_some_and(|x| x.handle() == r.handle())
    })
}

/// Opens a directory stream; returns the resource.
fn do_opendir(vm: &mut Vm, a: &Args) -> VmResult<Option<Resource>> {
    let path = path_arg(vm, a, 0)?;
    let ctx = context_arg(vm, a, 1, false)?;
    crate::userwrap::clear_last_object();
    let r = bridge::run(vm, || {
        let s = open::opendir(&path, REPORT_ERRORS, ctx.as_ref())?;
        let data = if res::is_user_stream(&s) {
            crate::userwrap::take_last_object()
        } else {
            None
        };
        Some((s, data))
    })?;
    let Some((mut s, data)) = r else {
        return Ok(None);
    };
    s.add_flags(flags::NO_FCLOSE);
    let Value::Resource(r) = res::new_stream(s, data) else {
        return Ok(None);
    };
    set_default_dir(Some(r.clone()));
    Ok(Some(r))
}

fn opendir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(do_opendir(vm, a)?.map_or(Value::Bool(false), Value::Resource))
}

fn dir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let Some(r) = do_opendir(vm, a)? else {
        return Ok(Value::Bool(false));
    };
    let Some(cls) = DIRECTORY_CLASS.with(|c| c.borrow().clone()) else {
        return Ok(Value::Bool(false));
    };
    let o = vm.new_object(&cls)?;
    vm.set_property_raw(&o, "path", Value::Str(a.str(0)));
    vm.set_property_raw(&o, "handle", Value::Resource(r));
    Ok(Value::Object(o))
}

/// `php_dir_get_directory_stream_from_user_arg`.
fn dir_from_arg(vm: &mut Vm, a: &Args) -> VmResult<(Resource, Rc<StreamCell>)> {
    if a.opt(0).is_some() {
        let pair = res::stream_arg_res(vm, a, 0)?;
        return dir_check(vm, a, pair);
    }
    vm.php_deprecated(
        "Passing null is deprecated, instead the last opened directory stream should be provided",
    )?;
    let Some(r) = DEFAULT_DIR.with(|d| d.borrow().clone()) else {
        return Err(vm.throw("TypeError", "No resource supplied"));
    };
    let cell = res::stream_of_resource(vm, a, &r)?;
    dir_check(vm, a, (r, cell))
}

/// Rejects streams that are not directory handles.
fn dir_check(
    vm: &mut Vm,
    a: &Args,
    (r, cell): (Resource, Rc<StreamCell>),
) -> VmResult<(Resource, Rc<StreamCell>)> {
    let is_dir = cell.get().is_some_and(|s| s.has_flag(flags::IS_DIR));
    if !is_dir {
        return Err(a.type_error(vm, 1, "must be a valid Directory resource"));
    }
    Ok((r, cell))
}

/// `php_dir_get_directory_stream_from_this`.
fn dir_from_this(vm: &mut Vm, a: &Args) -> VmResult<(Resource, Rc<StreamCell>)> {
    let Some(this) = a.this.clone() else {
        return Err(vm.throw("Error", "Internal directory stream has been altered"));
    };
    // An uninitialized `$handle` (instance made without `dir()`) reads as
    // "altered" (Z_DIRECTORY_HANDLE_P is not IS_RESOURCE).
    let handle = vm.get_property(&this, "handle").map(|v| v.deref());
    let Ok(Value::Resource(r)) = handle else {
        return Err(vm.throw("Error", "Internal directory stream has been altered"));
    };
    let cell = match res::cell_of(&r) {
        Some(c) if !r.is_closed() && !c.is_closed() => c,
        _ => {
            return Err(vm.throw(
                "TypeError",
                &format!(
                    "Directory::{}(): cannot use Directory resource after it has been closed",
                    fname(a)
                ),
            ))
        }
    };
    if !cell.get().is_some_and(|s| s.has_flag(flags::IS_DIR)) {
        return Err(vm.throw("Error", "Internal directory stream has been altered"));
    }
    Ok((r, cell))
}

fn close_dir(vm: &mut Vm, r: &Resource, cell: &StreamCell) -> VmResult<Value> {
    if let Some(mut s) = cell.take() {
        r.close();
        bridge::run(vm, || s.close())?;
    } else {
        r.close();
    }
    if is_default_dir(r) {
        set_default_dir(None);
    }
    Ok(Value::Null)
}

fn read_dir(vm: &mut Vm, cell: &StreamCell) -> VmResult<Value> {
    let r = res::with_stream(vm, cell, php_streams::stream::Stream::readdir)?.flatten();
    Ok(r.map_or(Value::Bool(false), bytes))
}

fn rewind_dir(vm: &mut Vm, cell: &StreamCell) -> VmResult<Value> {
    res::with_stream(vm, cell, php_streams::stream::Stream::rewinddir)?;
    Ok(Value::Null)
}

fn closedir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (r, cell) = dir_from_arg(vm, a)?;
    close_dir(vm, &r, &cell)
}

fn readdir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cell) = dir_from_arg(vm, a)?;
    read_dir(vm, &cell)
}

fn rewinddir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cell) = dir_from_arg(vm, a)?;
    rewind_dir(vm, &cell)
}

fn m_close(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (r, cell) = dir_from_this(vm, a)?;
    close_dir(vm, &r, &cell)
}

fn m_read(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cell) = dir_from_this(vm, a)?;
    read_dir(vm, &cell)
}

fn m_rewind(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cell) = dir_from_this(vm, a)?;
    rewind_dir(vm, &cell)
}

/// `dir_class_get_constructor`: `Directory` declares no constructor (so
/// `ReflectionClass::getConstructor()` is null and PDO's `FETCH_CLASS`
/// instantiates it), but `new Directory()` throws.
fn get_constructor(vm: &mut Vm, _obj: &php_runtime::Obj) -> VmResult<()> {
    Err(vm.throw(
        "Error",
        "Cannot directly construct Directory, use dir() instead",
    ))
}

fn scandir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    if path.is_empty() {
        return Err(a.value_error(vm, 1, "must not be empty"));
    }
    let sort = match a.int(1) {
        0 => ScandirSort::Ascending,
        1 => ScandirSort::Descending,
        2 => ScandirSort::None,
        _ => {
            return Err(a.value_error(
                vm,
                2,
                "must be one of the SCANDIR_SORT_ASCENDING, SCANDIR_SORT_DESCENDING, or SCANDIR_SORT_NONE constants",
            ))
        }
    };
    let ctx = if a.opt(2).is_some() {
        context_arg(vm, a, 2, false)?
    } else {
        None
    };
    let r = bridge::run(vm, || {
        let r = open::scandir(&path, sort, ctx.as_ref());
        (r, std::io::Error::last_os_error())
    })?;
    match r {
        (Some(names), _) => {
            let mut arr = ZArray::with_capacity(names.len());
            for n in names {
                let _ = arr.make_mut().push(bytes(n));
            }
            Ok(Value::Array(arr))
        }
        (None, err) => {
            let errno = err.raw_os_error().unwrap_or(0);
            vm.php_warning(&format!(
                "(errno {errno}): {}",
                php_streams::sys::strerror(errno)
            ))?;
            Ok(Value::Bool(false))
        }
    }
}

fn glob_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let pattern = path_arg(vm, a, 0)?;
    let flags = a.int(1);
    let r = bridge::run(vm, || {
        let restricted = !state::settings().open_basedir.is_empty();
        glob::glob_with_basedir(&pattern, flags, |p| {
            !restricted || paths::check_open_basedir_ex(p, false)
        })
    })?;
    match r {
        Ok(list) => {
            let mut arr = ZArray::with_capacity(list.len());
            for p in list {
                let _ = arr.make_mut().push(bytes(p));
            }
            Ok(Value::Array(arr))
        }
        Err(e) => {
            if let Some(w) = e.warning() {
                vm.php_warning(&w)?;
            }
            Ok(Value::Bool(false))
        }
    }
}

fn chdir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = path_arg(vm, a, 0)?;
    let allowed = bridge::run(vm, || paths::check_open_basedir(&path))?;
    if !allowed {
        return Ok(Value::Bool(false));
    }
    if let Err(e) = std::env::set_current_dir(php_streams::util::os_path(&path)) {
        let errno = e.raw_os_error().unwrap_or(0);
        vm.php_warning(&format!(
            "{} (errno {errno})",
            php_streams::sys::strerror(errno)
        ))?;
        return Ok(Value::Bool(false));
    }
    state::clear_stat_cache();
    Ok(Value::Bool(true))
}

fn realpath_cache_get(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let mut arr = ZArray::new();
    for b in php_streams::vcwd::cache_entries() {
        let mut entry = ZArray::new();
        let e = entry.make_mut();
        // `bucket->key` is unsigned: a double beyond ZEND_LONG_MAX.
        #[allow(clippy::cast_precision_loss)]
        let key = i64::try_from(b.key).map_or(Value::Float(b.key as f64), Value::Int);
        e.insert(ArrayKey::Str(ZStr::from("key")), key);
        e.insert(ArrayKey::Str(ZStr::from("is_dir")), Value::Bool(b.is_dir));
        e.insert(ArrayKey::Str(ZStr::from("realpath")), bytes(b.realpath));
        e.insert(ArrayKey::Str(ZStr::from("expires")), Value::Int(b.expires));
        arr.make_mut()
            .insert_str_raw(ZStr::from(b.path), Value::Array(entry));
    }
    Ok(Value::Array(arr))
}

fn realpath_cache_size(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(php_streams::vcwd::cache_size()))
}

/// `php_execute_script()` resolves the primary script with
/// `expand_filepath()`, which leaves its directories in the realpath cache.
fn prime_realpath_cache(vm: &mut Vm) {
    let script = vm.script_path.as_bytes().to_vec();
    if script.is_empty() || !php_streams::util::os_path(&script).is_file() {
        return;
    }
    let _ = bridge::run(vm, || {
        let cwd = php_streams::sys::getcwd().unwrap_or_default();
        php_streams::vcwd::virtual_file_ex(&cwd, &script, php_streams::vcwd::Resolve::FilePath)
    });
}

/// Registers the functions, constants and the `Directory` class.
pub fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("opendir(string $directory, $context = null)", opendir)
        .function_static("dir(string $directory, $context = null): Directory|false", dir)
        .function_static("closedir($dir_handle = null): void", closedir)
        .function_static("rewinddir($dir_handle = null): void", rewinddir)
        .function_static("readdir($dir_handle = null): string|false", readdir)
        .function_static(
            "scandir(string $directory, int $sorting_order = SCANDIR_SORT_ASCENDING, $context = null): array|false",
            scandir,
        )
        .function_static("glob(string $pattern, int $flags = 0): array|false", glob_fn)
        .function_static("chdir(string $directory): bool", chdir)
        .function_static("realpath_cache_get(): array", realpath_cache_get)
        .function_static("realpath_cache_size(): int", realpath_cache_size)
        .on_request_startup(prime_realpath_cache);
    for (name, v) in [
        ("GLOB_BRACE", glob::GLOB_BRACE),
        ("GLOB_ERR", glob::GLOB_ERR),
        ("GLOB_MARK", glob::GLOB_MARK),
        ("GLOB_NOCHECK", glob::GLOB_NOCHECK),
        ("GLOB_NOESCAPE", glob::GLOB_NOESCAPE),
        ("GLOB_NOSORT", glob::GLOB_NOSORT),
        ("GLOB_ONLYDIR", glob::GLOB_ONLYDIR),
        ("GLOB_AVAILABLE_FLAGS", glob::GLOB_AVAILABLE_FLAGS),
    ] {
        m.constant(name, Value::Int(i64::from(v)));
    }
    m.constant("SCANDIR_SORT_ASCENDING", Value::Int(0))
        .constant("SCANDIR_SORT_DESCENDING", Value::Int(1))
        .constant("SCANDIR_SORT_NONE", Value::Int(2));
    let cls = ClassBuilder::new("Directory")
        .flags(class_flags::FINAL | class_flags::NOT_SERIALIZABLE | class_flags::NO_CLONE)
        .prop("path", acc::PUBLIC | acc::READONLY, Some("string"), None)
        .prop("handle", acc::PUBLIC | acc::READONLY, Some("mixed"), None)
        .hooks(php_vm::ext::ClassHooks {
            get_constructor: Some(get_constructor),
            ..php_vm::ext::ClassHooks::default()
        })
        .method("close(): void", acc::PUBLIC, m_close)
        .method("rewind(): void", acc::PUBLIC, m_rewind)
        .method("read(): string|false", acc::PUBLIC, m_read)
        .register(m.vm());
    DIRECTORY_CLASS.with(|c| *c.borrow_mut() = Some(cls));
}
