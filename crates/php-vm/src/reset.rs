// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Request reset restoring clean VM state across request boundaries
//! without rebuilding the VM (for persistent worker SAPIs such as mod_php).

use crate::class::cflags;
use crate::func::Func;
use crate::vm::Vm;
use php_runtime::{Value, ZStr};
use std::cell::RefCell;
use std::rc::Rc;

impl Vm {
    /// Restores clean start-of-request state on a persistent VM instance:
    /// drops user functions, classes and constants, resets superglobals,
    /// statics, autoloaders, error handlers, output buffers, headers, INI,
    /// handle counter and extension state.
    pub fn request_reset(&mut self) {
        // Ensure request shutdown hooks ran if not already completed.
        if !self.in_shutdown {
            self.in_shutdown = true;
            for h in self.shutdown_hooks.clone() {
                h(self);
            }
            self.in_shutdown = false;
        }

        // 1. User functions: clear static variables, then retain only internal functions.
        for f in self.functions.values() {
            if let Func::User(u) = f {
                u.statics.borrow_mut().clear();
            }
        }
        self.functions.retain(|_, f| !f.is_user());

        // 2. Classes: clear user class static props, constants, defaults, enum cases,
        // and retain only internal classes.
        for c in self.classes.values() {
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
        self.classes.retain(|_, c| c.flags & cflags::INTERNAL != 0);
        self.class_order.retain(|name| {
            self.classes
                .get(name)
                .is_some_and(|c| c.flags & cflags::INTERNAL != 0)
        });

        // Reset internal class static properties and constants to initial defaults.
        for c in self.classes.values() {
            if c.flags & cflags::INTERNAL != 0 {
                for p in &c.static_props {
                    if let Some(crate::class::ConstExpr::Value(init)) = &p.default {
                        *p.cell.borrow_mut() = init.clone();
                    } else {
                        *p.cell.borrow_mut() = Value::Null;
                    }
                    p.initialized.set(true);
                }
                for k in &c.consts {
                    *k.value.borrow_mut() = None;
                }
                *c.defaults.borrow_mut() = None;
            }
        }

        // Clear bound class declarations, units, parsed sources, anonymous counter.
        self.bound_decls.clear();
        self.units.clear();
        self.parsed_sources.borrow_mut().clear();
        self.anon_counter = 0;

        // 3. User constants: remove all user-defined constants.
        for name in std::mem::take(&mut self.user_constants) {
            self.constants.remove(&name);
            self.const_attributes.remove(&name);
            self.const_files.remove(&name);
            self.const_names.remove(&name);
        }
        self.const_deprecation_guard.clear();
        self.ct_constants = None;
        *self.const_deprecation_filter.borrow_mut() = (0, Vec::new());

        // 4. Superglobals: fresh symbol table.
        self.globals = Rc::new(RefCell::new(crate::vm::VarStore::default()));

        // 5. Autoloaders.
        self.autoloaders.clear();
        self.autoloading.clear();

        // 6. Error & exception handlers.
        self.error_handlers.clear();
        self.exception_handlers.clear();
        self.in_error_handler = 0;
        self.last_error = None;
        self.last_fatal_error_backtrace = None;
        self.error_msg_html_ready = false;
        self.reporting_oom = false;

        // 7. Output buffers.
        self.output = php_support::output::OutputStack::new();
        self.output_handler_threw = false;

        // 8. Headers.
        self.headers_sent = false;
        self.response_mimetype = None;
        self.send_default_content_type = true;
        crate::builtins::headers::reset();

        // 9. INI settings: revert alterations back to server configuration.
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        self.ini.deactivate(&mut sink);
        for n in ["error_reporting", "precision", "serialize_precision"] {
            self.sync_ini(n);
        }

        // 10. Handle counter & object store.
        crate::object::reset_store();

        // 11. Extensions and runtime subsystems.
        crate::auto_globals::reset();
        crate::weakref::reset();
        crate::lazy::reset();
        crate::typed_refs::reset();
        crate::builtins::dump::reset();
        self.clear_interrupt();
        crate::gc::request_startup(self.ini.get_bool("zend.enable_gc"));
        crate::object::set_destructors_enabled(true);

        // 12. Execution state.
        self.frames.clear();
        self.included.clear();
        self.included_set.clear();
        self.attribute_call_sites.clear();
        self.exit_status = 0;
        self.shutdown_functions.clear();
        self.pending_throw = None;
        self.script_path = ZStr::empty();
        self.argv.clear();
        self.depth = 0;
        self.once_flags.clear();
        self.tick_functions.clear();
        self.user_ticks.clear();
        self.ticks_count = 0;
        self.fiber_switch_block = 0;
        self.timeout_seconds = 0;
        self.rfc1867_uploaded_files.clear();
        self.fibers.clear();
        self.pending_call_dst = None;
        self.builtin_deprecation_reported = false;
        self.dim_ctx = crate::ext::DimensionCtx::Read;
        self.prop_ctx = crate::ext::PropCtx::Read;
        self.ini_stage = crate::ext::IniStage::Runtime;
        self.request_info = php_sapi::RequestInfo::default();
        self.magic_call_name = None;
        self.main_return = Value::Null;
        self.call_bufs = crate::vm::CallBufs::prewarmed(&self.empty_code);
        self.prop_guards.clear();
        self.observer = crate::observer::ObserverState::default();
        self.user_opcode_handlers.clear();
        self.ext = crate::builtins::ExtState::default();
    }
}

#[cfg(test)]
mod tests {
    use crate::{Vm, VmOptions};
    use php_runtime::ZStr;
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

    #[test]
    fn request_reset_drops_user_declarations_and_resets_state() {
        let sink = Sink::default();
        let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
        vm.run_script(
            b"<?php
            function test_user_fn() { static $s = 1; return ++$s; }
            class TestUserClass { public static $prop = 42; }
            define('TEST_USER_CONST', 'hello');
            $test_global = 'foo';
            $obj = new stdClass;
            ini_set('precision', '20');
            ",
            "req1.php",
        );

        assert!(vm.functions.get(&ZStr::from("test_user_fn")).is_some());
        assert!(vm.classes.get(&ZStr::from("testuserclass")).is_some());
        assert!(vm.constants.get(&ZStr::from("TEST_USER_CONST")).is_some());
        assert_eq!(vm.precision, 20);

        vm.request_reset();

        assert!(
            vm.functions.get(&ZStr::from("test_user_fn")).is_none(),
            "user function must be dropped"
        );
        assert!(
            vm.classes.get(&ZStr::from("testuserclass")).is_none(),
            "user class must be dropped"
        );
        assert!(
            vm.constants.get(&ZStr::from("TEST_USER_CONST")).is_none(),
            "user constant must be dropped"
        );
        assert_eq!(vm.precision, 14, "INI must be restored");
        assert!(
            vm.globals.borrow().slots.is_empty(),
            "globals must be empty"
        );

        // Run a second script on the same persistent VM: object handle starts at #1
        sink.0.borrow_mut().clear();
        vm.run_script(
            b"<?php
            $o = new stdClass;
            var_dump($o);
            ",
            "req2.php",
        );
        let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        assert!(
            out.contains("object(stdClass)#1"),
            "handle counter must start at #1, got: {out}"
        );
    }
}
