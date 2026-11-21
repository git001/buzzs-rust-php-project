// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Builtin functions and classes.

pub mod classbuilder;
mod core;
pub(crate) mod dump;
pub(crate) mod headers;
pub(crate) mod info;
mod serial;
pub(crate) use self::core::enum_methods;
mod array;
mod classes;
mod math;
mod output;
pub mod sig;
mod string;
pub(crate) mod util;
mod var;

use crate::func::{flags, Builtin, BuiltinFn, Func};
use crate::vm::Vm;
use php_runtime::{Value, ZStr};
use std::collections::HashMap;
use std::rc::Rc;

/// Per-request state of builtin extensions.
#[derive(Debug, Default)]
pub struct ExtState {
    /// `__COMPILER_HALT_OFFSET__` per file.
    pub halt_offsets: HashMap<ZStr, i64>,
    /// Random number generator globals (`mt_rand`, `rand`, `lcg_value`).
    pub random: Option<php_random::RandomGlobals>,
    /// `ASSERTG(callback)` (`ext/standard/assert.c`): the callback set by
    /// `assert_options(ASSERT_CALLBACK)` or a runtime `assert.callback`
    /// change; `None` (`IS_UNDEF`) falls back to the startup INI value.
    pub assert_callback: Option<Value>,
    /// The JIT auto-globals still armed (`CG(auto_globals)`).
    pub(crate) auto_globals: crate::auto_globals::AutoGlobals,
}

/// Registers a builtin function from a stub signature.
pub(crate) fn reg(vm: &mut Vm, sig: &'static str, f: BuiltinFn) -> ZStr {
    let (lc, is_new) = insert_builtin(vm, sig, None, f);
    if is_new {
        info::note_new_function(&lc);
    }
    lc
}

/// [`reg`] for a signature built at run time.
pub(crate) fn reg_dynamic(vm: &mut Vm, sig: &str, f: BuiltinFn) -> ZStr {
    let (lc, is_new) = insert_builtin(vm, "", Some(sig), f);
    if is_new {
        info::note_new_function(&lc);
    }
    lc
}

/// [`reg`] for the extension API: attributes the function to `module` in
/// the same registry update that records the new key for Reflection.
pub(crate) fn reg_module(
    vm: &mut Vm,
    sig: &'static str,
    f: BuiltinFn,
    module: &'static str,
) -> ZStr {
    let (lc, is_new) = insert_builtin(vm, sig, None, f);
    info::note_function(&lc, module, is_new);
    lc
}

/// [`reg_module`] for a signature built at run time.
pub(crate) fn reg_module_dynamic(
    vm: &mut Vm,
    sig: &str,
    f: BuiltinFn,
    module: &'static str,
) -> ZStr {
    let (lc, is_new) = insert_builtin(vm, "", Some(sig), f);
    info::note_function(&lc, module, is_new);
    lc
}

/// Inserts the builtin function of stub signature `sig`; returns its
/// lower-case name (the table key) and whether the name was new. Only the
/// name and flags are scanned now; the rest of the signature is parsed on
/// first use (from `sig`, or from `owned` when the text is not static).
fn insert_builtin(
    vm: &mut Vm,
    sig: &'static str,
    owned: Option<&str>,
    f: BuiltinFn,
) -> (ZStr, bool) {
    let text = owned.unwrap_or(sig);
    let s = sig::sig_head(text);
    let mut fl = 0;
    if s.variadic {
        fl |= flags::VARIADIC;
    }
    if s.by_ref_return {
        fl |= flags::RETURN_REF;
    }
    let name = ZStr::from(s.name);
    // Shares `name`'s buffer when it is already lower case, as method
    // names do (`ClassBuilder::register`).
    let lc = name.to_ascii_lowercase();
    fl |= crate::special::frameless_flags(lc.as_bytes());
    let b = match owned {
        Some(t) => Builtin::from_stub(name, t, fl, f),
        None => Builtin::from_static_stub(name, sig, fl, f),
    };
    let is_new = vm
        .functions
        .insert(lc.clone(), Func::Builtin(Rc::new(b)))
        .is_none();
    (lc, is_new)
}

/// Registers an alias for an existing function.
pub(crate) fn alias(vm: &mut Vm, alias: &str, target: &str) {
    if let Some(Func::Builtin(b)) = vm.functions.get(&ZStr::from(target)).cloned() {
        let nb = b.renamed(ZStr::from(alias));
        let lc = ZStr::from(alias.to_ascii_lowercase());
        if vm
            .functions
            .insert(lc.clone(), Func::Builtin(Rc::new(nb)))
            .is_none()
        {
            info::note_new_function(&lc);
        }
    }
}

/// Registers every builtin function and class.
pub(crate) fn register_all(vm: &mut Vm) {
    classes::register(vm);
    crate::weakref::register(vm);
    core::register(vm);
    crate::gc::register(vm);
    crate::gc::add_root_provider(crate::gc::RootProvider {
        take: php_runtime::ref_roots::take,
        len: php_runtime::ref_roots::len,
        set_recording: php_runtime::ref_roots::set_recording,
    });
    php_runtime::ref_roots::set_notify(Some(crate::gc::note_possible_root));
    headers::register(vm);
    var::register(vm);
    output::register(vm);
    string::register(vm);
    math::register(vm);
    info::register(vm);
}

/// Registers what the bare VM (`Vm::new`) has on top of [`register_all`]:
/// bindings of ext/standard's string, math, array and output functions
/// (and `count()`/`sizeof()`) for scripts and tests without extensions.
/// [`Vm::with_extensions`](crate::Vm::with_extensions) does not register
/// them: there they come only from the extensions that provide them
/// (php-ext-stdstr, php-ext-stdarr, php-ext-random, php-ext-session, …).
pub(crate) fn register_bare(vm: &mut Vm) {
    string::register_bare(vm);
    math::register_bare(vm);
    output::register_bare(vm);
    array::register_bare_count(vm);
    array::register_bare_array_fns(vm);
}

/// Defines a constant.
pub(crate) fn constant(vm: &mut Vm, name: &str, v: Value) {
    let n = ZStr::from(name);
    if vm.constants.insert(n.clone(), v).is_none() {
        info::note_new_constant(&n);
    }
}
