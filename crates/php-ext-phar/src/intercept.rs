// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Phar::interceptFileFuncs()` (`func_interceptors.c`).
//!
//! Like PHP, the interceptors are installed at module startup by wrapping
//! the current handlers of the file functions; each wrapper consults
//! `php_phar::intercept` (which is inert until `interceptFileFuncs()` was
//! called and a phar is executing) and falls back to the original handler.

use std::cell::RefCell;
use std::collections::HashMap;

use php_ext_streams::bridge;
use php_phar::intercept::{self as ic, InterceptStat, StatKind};
use php_runtime::Value;
use php_streams::stat::StatBuf;
use php_vm::ext::{Args, ModuleBuilder, NativeFn, VmResult};
use php_vm::Vm;

use crate::util::{bytes, executed};

thread_local! {
    static ORIG: RefCell<HashMap<&'static str, NativeFn>> = RefCell::new(HashMap::new());
}

fn orig(name: &'static str, vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match ORIG.with(|o| o.borrow().get(name).copied()) {
        Some(f) => f(vm, a),
        None => Ok(Value::Bool(false)),
    }
}

/// Replaces the handler of `name`, keeping the original for [`orig`].
fn wrap(vm: &mut Vm, name: &'static str, handler: NativeFn) {
    if let Some(original) = vm.replace_native_handler(name, handler) {
        ORIG.with(|o| o.borrow_mut().insert(name, original));
    }
}

/// The first argument as a path, when it is a string.
fn path_of(a: &Args) -> Option<Vec<u8>> {
    match a.at(0) {
        Value::Str(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

fn replace_path(a: &mut Args, p: Vec<u8>) {
    if let Some(slot) = a.values.get_mut(0) {
        *slot = bytes(p);
    }
}

macro_rules! path_fn {
    ($fn:ident, $name:literal, $use_inc:expr) => {
        fn $fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            if php_phar::registry::intercepted() {
                if let Some(p) = path_of(a) {
                    #[allow(clippy::redundant_closure_call)]
                    let use_inc = ($use_inc)(&*a);
                    let ex = executed(vm);
                    let inc = vm.ini_string("include_path").unwrap_or_default();
                    if let Some(np) = ic::relative_path(&p, use_inc, ex.as_deref(), &inc) {
                        replace_path(a, np);
                    }
                }
            }
            orig($name, vm, a)
        }
    };
}

path_fn!(fopen, "fopen", |a: &Args| a.has(2)
    && matches!(a.at(2), Value::Bool(true)));
path_fn!(file_get_contents, "file_get_contents", |a: &Args| a.has(1)
    && matches!(a.at(1), Value::Bool(true)));
path_fn!(readfile, "readfile", |a: &Args| a.has(1)
    && matches!(a.at(1), Value::Bool(true)));

fn opendir(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if php_phar::registry::intercepted() {
        if let Some(p) = path_of(a) {
            let ex = executed(vm);
            if let Some(np) = ic::opendir_path(&p, ex.as_deref()) {
                replace_path(a, np);
            }
        }
    }
    orig("opendir", vm, a)
}

fn is_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if php_phar::registry::intercepted() {
        if let Some(p) = path_of(a) {
            let ex = executed(vm);
            if let Some(b) = ic::is_file(&p, ex.as_deref()) {
                return Ok(Value::Bool(b));
            }
        }
    }
    orig("is_file", vm, a)
}

fn is_link(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if php_phar::registry::intercepted() {
        if let Some(p) = path_of(a) {
            let ex = executed(vm);
            if let Some(b) = ic::is_link(&p, ex.as_deref()) {
                return Ok(Value::Bool(b));
            }
        }
    }
    orig("is_link", vm, a)
}

/// `phar_fancy_stat()`.
fn fancy_stat(sb: &StatBuf, kind: StatKind) -> Value {
    use php_streams::stat::mode as m;
    let uid = i64::from(php_streams::sys::uid());
    let gid = i64::from(php_streams::sys::gid());
    let (mut r, mut w, mut x) = (0o004, 0o002, 0o001);
    if sb.uid == uid {
        (r, w, x) = (0o400, 0o200, 0o100);
    } else if sb.gid == gid {
        (r, w, x) = (0o040, 0o020, 0o010);
    }
    match kind {
        StatKind::Perms => Value::Int(sb.mode),
        StatKind::Inode => Value::Int(sb.ino),
        StatKind::Size => Value::Int(sb.size),
        StatKind::Owner => Value::Int(sb.uid),
        StatKind::Group => Value::Int(sb.gid),
        StatKind::Atime => Value::Int(sb.atime),
        StatKind::Mtime => Value::Int(sb.mtime),
        StatKind::Ctime => Value::Int(sb.ctime),
        StatKind::Type => {
            let t = match sb.mode & m::S_IFMT {
                x if x == m::S_IFDIR => "dir",
                x if x == m::S_IFLNK => "link",
                _ => "file",
            };
            bytes(t)
        }
        StatKind::IsWritable => Value::Bool(sb.mode & w != 0),
        StatKind::IsReadable => Value::Bool(sb.mode & r != 0),
        StatKind::IsExecutable => Value::Bool(sb.mode & x != 0),
        StatKind::Exists => Value::Bool(true),
        StatKind::IsDir => Value::Bool(sb.mode & m::S_IFMT == m::S_IFDIR),
        StatKind::Lstat | StatKind::Stat => bridge::from_lib(&sb.to_value()),
    }
}

fn stat_common(name: &'static str, kind: StatKind, vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if php_phar::registry::intercepted() {
        if let Some(p) = path_of(a) {
            let ex = executed(vm);
            match ic::file_stat(&p, kind, ex.as_deref()) {
                InterceptStat::Skip => {}
                InterceptStat::Bool(b) => return Ok(Value::Bool(b)),
                InterceptStat::Stat(sb) => return Ok(fancy_stat(&sb, kind)),
                InterceptStat::Warn(w) => {
                    vm.php_warning(&w)?;
                    return Ok(Value::Bool(false));
                }
            }
        }
    }
    orig(name, vm, a)
}

macro_rules! stat_fns {
    ($($fn:ident => ($name:literal, $kind:ident)),* $(,)?) => {
        $(fn $fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            stat_common($name, StatKind::$kind, vm, a)
        })*

        const STAT_FUNCS: &[(&str, NativeFn)] = &[$(($name, $fn)),*];
    };
}

stat_fns! {
    file_exists => ("file_exists", Exists),
    fileperms => ("fileperms", Perms),
    fileinode => ("fileinode", Inode),
    filesize => ("filesize", Size),
    fileowner => ("fileowner", Owner),
    filegroup => ("filegroup", Group),
    fileatime => ("fileatime", Atime),
    filemtime => ("filemtime", Mtime),
    filectime => ("filectime", Ctime),
    filetype => ("filetype", Type),
    is_writable => ("is_writable", IsWritable),
    is_readable => ("is_readable", IsReadable),
    is_executable => ("is_executable", IsExecutable),
    is_dir => ("is_dir", IsDir),
    lstat => ("lstat", Lstat),
    stat => ("stat", Stat),
}

/// Installs the interceptors (module startup).
pub fn install(m: &mut ModuleBuilder<'_>) {
    let vm = m.vm();
    let plain: [(&'static str, NativeFn); 6] = [
        ("fopen", fopen),
        ("file_get_contents", file_get_contents),
        ("readfile", readfile),
        ("opendir", opendir),
        ("is_file", is_file),
        ("is_link", is_link),
    ];
    for (name, f) in plain.into_iter().chain(STAT_FUNCS.iter().copied()) {
        wrap(vm, name, f);
    }
    debug_assert!(ic::INTERCEPTED_FUNCTIONS.len() == plain.len() + STAT_FUNCS.len());
}
