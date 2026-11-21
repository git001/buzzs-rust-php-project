// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Fast request shutdown for a process that exits next (ref:
//! Zend/zend_execute_API.c `shutdown_executor()` with `fast_shutdown`,
//! `zend_shutdown_executor_values()`; Zend/zend_hash.c `zend_hash_discard()`).

use crate::class::cflags;
use crate::func::Func;
use crate::vm::Vm;
use php_runtime::Value;

impl Vm {
    /// Releases the VM after [`Vm::shutdown`] the way php-src's fast request
    /// shutdown does, for a process that exits next.
    ///
    /// With the Zend memory manager, `shutdown_executor()` takes its
    /// `fast_shutdown` path: the request heap is freed in one go, so the
    /// function, class and constant tables are not destroyed entry by entry.
    /// `zend_hash_discard()` unlinks their user entries and keeps the
    /// persistent ones (`EG(persistent_functions_count)`, ...), and only
    /// objects with a `free_obj` handler of their own, native payloads whose
    /// release has side effects (a database connection closes, a lock is
    /// released), are freed one by one
    /// (`zend_objects_store_free_object_storage()`).
    ///
    /// Here the user entries (user functions with their `static` variables,
    /// user classes with their static properties and constants, constants
    /// holding an object or a resource) are released, so that whatever they
    /// hold runs its [`Drop`]; the internal entries, which hold no state of
    /// the request, are leaked. Every other part of the VM, the global symbol
    /// table, output, fibers and handlers, drops as usual. The `static`
    /// variables, static properties, constants and property defaults of the
    /// user functions and classes are emptied first, as
    /// `zend_shutdown_executor_values()` empties them, because the function
    /// and class objects themselves may outlive their table entries (the
    /// declaration tables of the compiled units refer to them and they to
    /// the tables).
    ///
    /// The side effects of request shutdown have all run in [`Vm::shutdown`]
    /// already: shutdown functions, destructors, the extensions' request
    /// shutdown hooks (which close every stream and write the session),
    /// output flushing, headers. A host that runs another request on the
    /// same thread (`--repeat`, the built-in web server, a FastCGI worker) must drop
    /// the `Vm` instead.
    pub fn discard(self) {
        let Vm {
            mut functions,
            mut classes,
            mut constants,
            class_order,
            user_constants,
            ini,
            configuration,
            parsed_sources,
            empty_code,
            empty_vars,
            builtin_deprecations,
            builtin_attributes,
            builtin_doc_comments,
            deprecated_constants,
            const_deprecation_filter,
            ct_constants,
            ini_modify_hooks,
            module_info,
            output_handler_aliases,
            output_handler_conflicts,
            output_handler_hooks,
            loaded_extensions,
            units,
            bound_decls,
            call_bufs,
            autoloaders,
            user_opcode_handlers,
            ..
        } = self;
        // `zend_shutdown_executor_values()`: "Release static properties and
        // static variables prior to the final GC run, as they may hold GC
        // roots", and the class constants and property defaults, which "may
        // contain objects as well". They are released by hand rather than
        // with their function or class: a declaration table and the op
        // arrays declared in it refer to each other (`OpArray::decls`), so
        // dropping a user function's table entry does not free it, and
        // php-src frees every object left in the store at this point anyway
        // (`zend_objects_store_free_object_storage()`).
        for f in functions.values() {
            if let Func::User(u) = f {
                u.statics.borrow_mut().clear();
            }
        }
        for c in classes.values() {
            if c.flags & cflags::INTERNAL != 0 {
                continue;
            }
            for m in c.methods.values() {
                if let Func::User(u) = &m.func {
                    u.statics.borrow_mut().clear();
                }
            }
            for p in &c.static_props {
                *p.cell.borrow_mut() = Value::Null;
            }
            for k in &c.consts {
                *k.value.borrow_mut() = None;
            }
            *c.defaults.borrow_mut() = None;
            c.enum_cases.borrow_mut().clear();
        }
        // The user part of each table, which `zend_hash_discard()` unlinks:
        // released here, so that the objects and resources reachable only
        // from static properties, class constants, enum cases, `static`
        // variables or constants are freed as
        // `zend_objects_store_free_object_storage()` frees them.
        functions.retain(|_, f| !f.is_user());
        classes.retain(|_, c| c.flags & cflags::INTERNAL != 0);
        constants.retain(|_, v| !holds_payload(v));
        // The persistent part lives until the process ends (php-src frees it
        // with the arena): about 4 M instructions of `drop_in_place` for the
        // ~1 700 functions and ~320 classes behind `php -r 'echo 1;'`.
        std::mem::forget((functions, classes, constants, class_order, user_constants));
        std::mem::forget((ini, configuration, parsed_sources, empty_code, empty_vars));
        std::mem::forget((
            builtin_deprecations,
            builtin_attributes,
            builtin_doc_comments,
            deprecated_constants,
            const_deprecation_filter,
            ct_constants,
        ));
        std::mem::forget((
            ini_modify_hooks,
            module_info,
            output_handler_aliases,
            output_handler_conflicts,
            output_handler_hooks,
            loaded_extensions,
        ));
        // Bulk VM teardown: compiled ASTs, symbol tables, and call buffers
        // are reclaimed by the OS on process exit without recursive drops.
        std::mem::forget((
            units,
            bound_decls,
            call_bufs,
            autoloaders,
            user_opcode_handlers,
        ));
        // The thread-local registry attributing those internal entries to
        // their extensions goes with them.
        crate::builtins::info::forget_registry();
    }
}

/// Whether `v` holds an object or a resource, possibly inside arrays or
/// behind a reference: releasing it may run a native payload's `Drop`.
fn holds_payload(v: &Value) -> bool {
    match v {
        Value::Object(_) | Value::Resource(_) => true,
        Value::Array(a) => a.values().any(holds_payload),
        Value::Reference(r) => holds_payload(&r.borrow()),
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::holds_payload;
    use crate::object;
    use crate::{Vm, VmOptions};
    use php_runtime::{PhpRef, Resource, Value, ZArray, ZStr};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The object handles `var_dump` printed (`object(C)#N (...)`).
    fn dumped_handles(out: &[u8]) -> Vec<usize> {
        String::from_utf8_lossy(out)
            .split('#')
            .skip(1)
            .filter_map(|s| s.split(' ').next()?.parse().ok())
            .collect()
    }

    #[test]
    fn discard_releases_the_objects_the_user_tables_hold() {
        let sink = Sink::default();
        let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        // As the CLI before its last request: the process exits next.
        vm.set_fast_shutdown(true);
        vm.run_script(
            b"<?php
class A { public static $x; }
A::$x = new stdClass;
function f() { static $s; if ($s === null) { $s = new stdClass; } return $s; }
$g = new stdClass; $h = $g;
var_dump(A::$x, f(), $g);",
            "t.php",
        );
        let handles = dumped_handles(&sink.0.borrow());
        assert_eq!(handles.len(), 3, "{:?}", sink.0.borrow());
        // Request shutdown left them alive: a static property, a static
        // variable and a global with two references hold them.
        for &h in &handles {
            assert!(object::live_obj_at(h - 1).is_some(), "#{h} before discard");
        }
        vm.discard();
        for &h in &handles {
            assert!(object::live_obj_at(h - 1).is_none(), "#{h} after discard");
        }
    }

    #[test]
    fn payload_detection_looks_through_arrays_and_references() {
        assert!(!holds_payload(&Value::Null));
        assert!(!holds_payload(&Value::Int(1)));
        assert!(!holds_payload(&Value::Str(ZStr::from("s"))));
        let scalars = Value::Array(ZArray::from_values([
            Value::Int(1),
            Value::Array(ZArray::from_values([Value::Bool(true), Value::Float(2.5)])),
        ]));
        assert!(!holds_payload(&scalars));
        let resource = Value::Resource(Resource::new("test", None));
        assert!(holds_payload(&resource));
        let nested = Value::Array(ZArray::from_values([
            Value::Int(1),
            Value::Array(ZArray::from_values([resource])),
        ]));
        assert!(holds_payload(&nested));
        assert!(holds_payload(&Value::Reference(PhpRef::new(nested))));
        let scalar_ref = Value::Reference(PhpRef::new(Value::Int(0)));
        assert!(!holds_payload(&scalar_ref));
    }
}
