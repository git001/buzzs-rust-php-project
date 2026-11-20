// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `zend_test.*` INI directives (`PHP_INI_BEGIN()` of test.c and
//! observer.c, in registration order).

use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{ModuleBuilder, VmResult};
use php_vm::Vm;

const MODULE: &str = "zend_test";

/// `(name, default, access, handler)`.
const DIRECTIVES: &[(&str, &str, Access, ModifyHandler)] = &[
    (
        "zend_test.register_system_entropy",
        "0",
        Access::SYSTEM,
        ModifyHandler::None,
    ),
    (
        "zend_test.replace_zend_execute_ex",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.register_passes",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.rearm_vm_interrupt",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.print_stderr_mshutdown",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.limit_copy_file_range",
        "-1",
        Access::ALL,
        ModifyHandler::Long,
    ),
    (
        "zend_test.quantity_value",
        "0",
        Access::ALL,
        ModifyHandler::Long,
    ),
    ("zend_test.str_test", "", Access::ALL, ModifyHandler::String),
    (
        "zend_test.not_empty_str_test",
        "val",
        Access::ALL,
        ModifyHandler::StrNotEmpty,
    ),
    (
        "zend_test.observe_opline_in_zendmm",
        "0",
        Access::ALL,
        ModifyHandler::Bool,
    ),
    // observer.c
    (
        "zend_test.observer.enabled",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.show_output",
        "1",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.observe_all",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.observe_includes",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.observe_functions",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.observe_declaring",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.observe_function_names",
        "",
        Access::ALL,
        ModifyHandler::None,
    ),
    (
        "zend_test.observer.show_return_type",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.show_return_value",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.show_init_backtrace",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.show_opcode",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.show_opcode_in_user_handler",
        "",
        Access::SYSTEM,
        ModifyHandler::String,
    ),
    (
        "zend_test.observer.set_vm_interrupt_on_begin",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.set_vm_interrupt_on_begin_without_callback",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.set_vm_interrupt_on_end",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.fiber_init",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.fiber_switch",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.fiber_destroy",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.execute_internal",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    (
        "zend_test.observer.reserve_op_array_handle",
        "0",
        Access::SYSTEM,
        ModifyHandler::Bool,
    ),
    // zend_mm_custom_handlers.c (hooks in `crate::mm`)
    (crate::mm::DIRECTIVE, "0", Access::USER, ModifyHandler::Bool),
];

/// `zend_test_observer_OnUpdateCommaList`: the function list can only be
/// changed while the observer is enabled.
fn on_observe_function_names(vm: &mut Vm, _value: &[u8]) -> VmResult<bool> {
    Ok(vm.ini_bool("zend_test.observer.enabled"))
}

/// `[zend_test] MSHUTDOWN` on stderr (the CLI runs one request per
/// module lifetime).
fn print_mshutdown(vm: &mut Vm) {
    if vm.ini_bool("zend_test.print_stderr_mshutdown") {
        eprintln!("[zend_test] MSHUTDOWN");
    }
}

pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, default, access, handler) in DIRECTIVES {
        m.ini(IniDirective::new(
            name,
            Some(default),
            *access,
            *handler,
            MODULE,
        ));
    }
    m.ini_on_modify(
        "zend_test.observer.observe_function_names",
        on_observe_function_names,
    );
    m.on_request_shutdown(print_mshutdown);
}

#[cfg(test)]
mod tests {
    #[test]
    fn directives_are_namespaced() {
        assert!(super::DIRECTIVES
            .iter()
            .all(|(n, ..)| n.starts_with("zend_test.")));
    }
}
