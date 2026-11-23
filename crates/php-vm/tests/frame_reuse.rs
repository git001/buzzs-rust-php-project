// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Reuse and argument-introspection regressions for pooled user frames.
#![expect(clippy::disallowed_methods, reason = "test code")]

use php_runtime::{Value, ZStr};
use php_vm::vm::{Frame, FrameKind};
use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn run(source: &str) -> String {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let output = sink.0.borrow().clone();
    let output = String::from_utf8(output).expect("UTF-8 output");
    assert_eq!(status, 0, "{output}");
    output
}

#[test]
fn variadic_introspection_excludes_named_but_trace_preserves_them() {
    assert_eq!(
        run(r#"<?php
function f($a, ...$rest) {
    $a = 10;
    echo func_num_args(), ":", serialize(func_get_args()), ":";
    echo serialize(debug_backtrace()[0]["args"]), "\n";
}
f(a: 1, extra: 2);
f(1, 2, 3, extra: 4);
"#), "1:a:1:{i:0;i:10;}:a:2:{i:0;i:10;s:5:\"extra\";i:2;}\n3:a:3:{i:0;i:10;i:1;i:2;i:2;i:3;}:a:4:{i:0;i:10;i:1;i:2;i:2;i:3;s:5:\"extra\";i:4;}\n");
}

#[test]
fn indirect_calls_report_reference_warnings_and_missing_argument_locations() {
    assert_eq!(run(r#"<?php
set_error_handler(function($n, $message) { echo $message, "\n"; });
call_user_func("settype", 1, "int");
function f($a, $b) {}
try { call_user_func("f", 1); } catch (Throwable $e) { echo $e->getMessage(), "\n"; }
"#), "settype(): Argument #1 ($var) must be passed by reference, value given\nToo few arguments to function f(), 1 passed in t.php on line 5 and exactly 2 expected\n");
}

#[test]
fn oversized_frame_buffers_are_released_and_values_dropped() {
    let vm = Vm::new(&VmOptions::default(), Box::new(std::io::sink()));
    let mut frame = Frame::new_pooled(vm.empty_code.clone());
    let value = ZStr::from("held value");
    frame.tmps.resize(1024, Value::Str(value.clone()));
    frame.args.resize(1024, Some(Value::Str(value.clone())));
    frame
        .named
        .resize(1024, (value.clone(), Value::Str(value.clone())));
    frame.iters.resize_with(1024, || None);
    frame.fins.resize(1024, php_vm::vm::FinState::None);
    frame.calls.reserve(1024);
    frame.silence_saved.resize(1024, 1);
    {
        let mut vars = frame.vars.borrow_mut();
        vars.slots.resize(1024, Some(Value::Str(value.clone())));
        vars.names.to_mut().resize(1024, value);
    }
    let class = vm.classes[&ZStr::from("stdclass")].clone();
    let object = php_vm::object::new_object(class, Vec::new());
    frame.this = Some(object.clone());
    frame.closure = Some(object.clone());
    frame.generator = Some(object.clone());
    frame.pending_exc = Some(object.clone());
    frame.tmps.push(Value::Object(object.clone()));
    frame.args.push(Some(Value::Object(object.clone())));
    frame
        .named
        .push((ZStr::from("object"), Value::Object(object.clone())));
    frame
        .vars
        .borrow_mut()
        .slots
        .push(Some(Value::Object(object.clone())));
    assert_eq!(object.refcount(), 9);
    frame.clear_for_pool(&vm.empty_code);
    assert_eq!(object.refcount(), 1);
    assert_eq!(frame.tmps.capacity(), 0);
    assert_eq!(frame.args.capacity(), 0);
    assert_eq!(frame.named.capacity(), 0);
    assert_eq!(frame.iters.capacity(), 0);
    assert_eq!(frame.fins.capacity(), 0);
    assert_eq!(frame.calls.capacity(), 0);
    assert_eq!(frame.silence_saved.capacity(), 0);
    assert_eq!(frame.vars.borrow().slots.capacity(), 0);
    assert_eq!(frame.vars.borrow().names.capacity(), 0);
    frame.reinit_for_user(vm.empty_code.clone(), FrameKind::Function);
    assert!(frame.args.is_empty());
}

#[test]
fn ordinary_buffers_retain_capacity_and_shared_stores_survive() {
    let vm = Vm::new(&VmOptions::default(), Box::new(std::io::sink()));
    let mut frame = Frame::new_pooled(vm.empty_code.clone());
    frame.tmps.push(Value::Int(42));
    let capacity = frame.tmps.capacity();
    frame.vars.borrow_mut().slots.push(Some(Value::Int(42)));
    let shared = frame.vars.clone();
    frame.clear_for_pool(&vm.empty_code);
    assert_eq!(frame.tmps.capacity(), capacity);
    assert!(frame.tmps.is_empty());
    assert!(!Rc::ptr_eq(&shared, &frame.vars));
    assert!(matches!(shared.borrow().slots[0], Some(Value::Int(42))));
    assert!(frame.vars.borrow().slots.is_empty());
}

#[test]
fn nested_error_closure_and_generator_calls_do_not_leak_frame_state() {
    assert_eq!(
        run(r#"<?php
function nested($n) { $local = str_repeat("x", 50); return $n ? nested($n - 1) : func_num_args(); }
function fail(int $a) {}
function gen($a) { yield $a; yield func_num_args(); }
for ($i = 0; $i < 20; $i++) {
    try { fail([]); } catch (TypeError $e) {}
    $f = function($a) use ($i) { return $a + $i; };
    $g = gen($i);
    $sum = 0; foreach ($g as $v) { $sum += $v; }
    if (nested(10) !== 1 || $f(2) !== $i + 2 || $sum !== $i + 1) { echo "bad\n"; }
}
echo "ok\n";
"#),
        "ok\n"
    );
}

#[test]
fn pooled_frames_preserve_captured_references_and_suspended_fibers() {
    assert_eq!(
        run(r#"<?php
class Item { public function __construct(public string $name) {} public function __destruct() { echo $this->name, "\n"; } }
function capture_counter() { $counter = 0; return function() use (&$counter) { return ++$counter; }; }
function return_with_local() { $item = new Item("normal destructor"); }
function throw_with_local() { $item = new Item("exception destructor"); throw new Exception(); }
$next = capture_counter();
echo $next(), ":", $next(), "\n";
return_with_local();
try { throw_with_local(); } catch (Exception $e) {}
$fiber = new Fiber(function() use ($next) { $value = $next(); Fiber::suspend($value); echo $next(), "\n"; });
echo "fiber:", $fiber->start(), "\n";
$fiber->resume();
"#),
        "1:2\nnormal destructor\nexception destructor\nfiber:3\n4\n"
    );
}

#[test]
fn by_ref_void_function_emits_compile_deprecation() {
    assert_eq!(
        run("<?php\nfunction &test(): void {\n}\n$r =& test();\nvar_dump($r);\n"),
        "\nDeprecated: test(): Returning by reference from a void function is deprecated in t.php on line 2\n\nNotice: Only variable references should be returned by reference in t.php on line 3\nNULL\n"
    );
}

#[test]
fn unpacked_assertion_rejects_generated_positional_description() {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(b"<?php assert(...[false, \"explicit\"]);", "t.php");
    assert_eq!(status, 255);
    let output = String::from_utf8(sink.0.borrow().clone()).expect("UTF-8 output");
    assert!(output.contains("Cannot use positional argument after argument unpacking"));
}

fn reference_phpt(path: &str) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/php-src");
    let text = std::fs::read_to_string(root.join(path)).expect("read-only reference PHPT");
    let (_, file) = text.split_once("--FILE--\n").expect("FILE section");
    let (source, expected) = file
        .split_once("--EXPECT--\n")
        .expect("exact EXPECT section");
    assert_eq!(run(source).trim(), expected.trim(), "{path}");
}

#[test]
fn exact_named_variadic_reference_phpt() {
    reference_phpt("Zend/tests/named_params/variadic.phpt");
}

#[test]
fn pfa_reference_default_namespace_and_reference_errors() {
    reference_phpt("Zend/tests/partial_application/default_const_expr.phpt");
    reference_phpt("Zend/tests/partial_application/constexpr_016.phpt");
}

#[test]
fn pfa_reference_magic_and_dynamic_call_restrictions() {
    reference_phpt("Zend/tests/partial_application/magic_scope.phpt");
    reference_phpt("Zend/tests/partial_application/non_dynamic_call_funcs.phpt");
}

#[test]
fn lazy_proxy_magic_runs_before_initialization() {
    reference_phpt("Zend/tests/lazy_objects/gh18038-001.phpt");
}

#[test]
fn lazy_reset_during_initialization_reports_error() {
    reference_phpt("Zend/tests/lazy_objects/reset_as_lazy_while_init_exception.phpt");
}

#[test]
fn hook_iteration_includes_virtual_get_and_rejects_by_value_references() {
    reference_phpt("Zend/tests/property_hooks/foreach_val_to_ref.phpt");
}

#[test]
fn variadic_snapshots_and_magic_trampoline_count_are_stable() {
    assert_eq!(
        run(r#"<?php
function f($a, ...$rest) {
    $a = 5; $rest[0] = 9; $rest[] = 11; $rest["extra"] = 12;
    echo serialize(func_get_args()), "\n";
    echo serialize(debug_backtrace()[0]["args"]), "\n";
}
f(1, 2, extra: 3);
class C { function __call($name, $args) { echo func_num_args(), "\n"; } }
(new C)->f(1, 2, extra: 3);
"#),
        "a:2:{i:0;i:5;i:1;i:2;}\na:3:{i:0;i:5;i:1;i:2;s:5:\"extra\";i:3;}\n2\n"
    );
}

#[test]
fn property_invariance_rejects_narrowing_and_accepts_equivalent_intersections() {
    assert_eq!(
        run(r#"<?php
interface X {} interface Y {}
class A { public X&Y $value; }
class B extends A { public Y&X $value; }
echo "ok\n";
"#),
        "ok\n"
    );
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(b"<?php interface X {} interface Y {} class A { public X&Y $value; } class B extends A { public X $value; }", "t.php");
    assert_eq!(status, 255);
    let output = sink.0.borrow().clone();
    assert!(
        String::from_utf8_lossy(&output).contains("Type of B::$value must be X&Y (as in class A)")
    );
}

#[test]
fn pfa_rebinding_rebinds_only_its_target_closure() {
    assert_eq!(
        run(r#"<?php
class C { function closure() { return function($a, $other) { echo $this === $a ? "same\n" : "different\n"; $other(); }; } }
class D extends C {}
$other = function() { echo "other\n"; };
$partial = (new C)->closure()(?, $other);
$target = new D;
$partial->bindTo($target)($target);
$unbound = function($a) { echo $this === $a ? "bound\n" : "wrong\n"; };
$partial = $unbound(?);
$target = new stdClass;
$partial->bindTo($target)($target);
"#),
        "same\nother\nbound\n"
    );
}

#[test]
fn pfa_methods_preserve_scope_and_reject_unrelated_this() {
    assert_eq!(
        run(r#"<?php
class C { function f($a) { echo static::class, "\n"; } }
class D extends C {}
class Other {}
set_error_handler(function($n, $message) { echo str_contains($message, "Cannot bind method") ? "unrelated\n" : "scope\n"; });
$partial = (new C)->f(?);
$partial->bindTo(new D)(1);
var_dump($partial->bindTo(new Other));
var_dump($partial->bindTo(new C, D::class));
"#),
        "D\nunrelated\nNULL\nscope\nNULL\n"
    );
}

fn reference_fatal_phpt(path: &str) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/php-src");
    let text = std::fs::read_to_string(root.join(path)).expect("read-only reference PHPT");
    let (_, file) = text.split_once("--FILE--\n").expect("FILE section");
    let (source, expected) = file.split_once("--EXPECTF--\n").expect("EXPECTF section");
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(source.as_bytes(), "t.php");
    let output = sink.0.borrow().clone();
    let output = String::from_utf8_lossy(&output);
    assert_eq!(status, 255, "{path}: {output}");
    let prefix = expected.split(" in %s").next().expect("fatal message");
    assert!(output.trim_start().starts_with(prefix), "{path}: {output}");
}

#[test]
fn reference_hook_initialization_and_inheritance_errors() {
    for path in [
        "default_on_virtual.phpt",
        "default_on_virtual_with_inheritance.phpt",
        "get_by_ref_backed.phpt",
        "invalid_abstract_body.phpt",
        "invalid_abstract_indirect.phpt",
        "traits_abstract.phpt",
        "interface.phpt",
        "interface_not_implemented.phpt",
        "get_by_ref_implemented_by_val.phpt",
        "type_compatibility_invalid.phpt",
        "type_compatibility_invalid_2.phpt",
        "set_value_parameter_type_variance_002.phpt",
        "set_value_parameter_type_variance_003.phpt",
        "set_value_parameter_type_variance_007.phpt",
    ] {
        reference_fatal_phpt(&format!("Zend/tests/property_hooks/{path}"));
    }
}

#[test]
fn reference_intersection_and_dnf_property_invariance_errors() {
    for path in [
        "intersection_types/variance/invalid_invariance1.phpt",
        "intersection_types/variance/invalid_invariance2.phpt",
        "dnf_types/variance/invalid_invariance1.phpt",
        "dnf_types/variance/invalid_invariance1_var.phpt",
        "dnf_types/variance/invalid_invariance2.phpt",
        "union_types/variance/invalid_004.phpt",
    ] {
        reference_fatal_phpt(&format!("Zend/tests/type_declarations/{path}"));
    }
}

#[test]
fn indirect_extra_named_reference_arguments_warn_and_remain_separate() {
    assert_eq!(run(r#"<?php
set_error_handler(function($n, $message) { echo $message, "\n"; });
function f(&...$refs) { foreach ($refs as &$ref) { $ref++; } echo func_num_args(), "\n"; }
call_user_func("f", a: 1, b: 2);
"#), "f(): Argument #1 must be passed by reference, value given\nf(): Argument #1 must be passed by reference, value given\n0\n");
}

#[test]
fn pfa_relative_types_and_compilation_warnings_are_preserved() {
    assert_eq!(
        run(r#"<?php
trait T { function f(object $o): self { return $o; } }
class C { use T; }
$c = new C;
$partial = $c->f(?);
echo (new ReflectionFunction($partial))->getReturnType(), "\n";
var_dump($partial($c) === $c);
"#),
        "self\nbool(true)\n"
    );
    let output = run("<?php\nfunction f(_ $a) {}\n$partial = f(?);\n");
    assert!(
        output.contains("Using \"_\" as a type name is deprecated since 8.4 in t.php on line 2")
    );
    assert!(
        output.contains("Using \"_\" as a type name is deprecated since 8.4 in t.php on line 3")
    );
}

#[test]
fn linked_interface_parent_is_recognized() {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink));
    assert_eq!(
        vm.run_script(b"<?php interface X {} interface Y extends X {}", "t.php"),
        0
    );
    let x = &vm.classes[&ZStr::from("x")];
    let y = &vm.classes[&ZStr::from("y")];
    assert!(y.instance_of_name(x.lc_name.as_bytes()), "X={x:?}, Y={y:?}");
}

#[test]
fn pfa_closure_invoke_unknown_default_matches_reference() {
    reference_phpt("Zend/tests/partial_application/errors_005.phpt");
}

#[test]
fn func_num_args_reference_locations_are_exact() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/php-src");
    let text = std::fs::read_to_string(root.join("Zend/tests/func_num_args_basic.phpt"))
        .expect("reference PHPT");
    let (_, file) = text.split_once("--FILE--\n").expect("FILE");
    let (source, expected) = file.split_once("--EXPECTF--\n").expect("EXPECTF");
    let lines: Vec<_> = source
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            (line.trim() == "test2();" || line.trim() == "call_user_func(\"test3\", 1);")
                .then_some(i + 1)
        })
        .collect();
    assert_eq!(lines.len(), 2);
    let expected = expected.replace("%s", "t.php");
    let expected = expected.replacen("%d", &lines[0].to_string(), 1);
    let expected = expected.replacen("%d", &lines[1].to_string(), 1);
    assert_eq!(run(source).trim(), expected.trim());
}

#[test]
fn hooked_foreach_visits_new_dynamic_properties() {
    reference_phpt("Zend/tests/property_hooks/foreach_002.phpt");
}

#[test]
fn named_variadic_literal_reference_error_uses_variadic_position() {
    assert_eq!(
        run(r#"<?php
function f($a, &...$refs) {}
try { f(a: 1, z: 2); } catch (Throwable $e) { echo $e->getMessage(), "\n"; }
"#),
        "f(): Argument #2 could not be passed by reference\n"
    );
}

#[test]
fn pfa_keeps_deferred_nodiscard_attribute_arguments() {
    assert_eq!(
        run(r#"<?php
#[NoDiscard(MESSAGE)]
function f($a) { return $a; }
$partial = f(?);
define("MESSAGE", "payload");
foreach ([new ReflectionFunction("f"), new ReflectionFunction($partial)] as $function) {
    echo serialize($function->getAttributes(NoDiscard::class)[0]->getArguments()), "\n";
}
"#),
        "a:1:{i:0;s:7:\"payload\";}\na:1:{i:0;s:7:\"payload\";}\n"
    );
}

#[test]
fn implicit_hook_defaults_preserve_backed_inheritance_and_promotion() {
    for name in [
        "default_value_inheritance.phpt",
        "no_default_value_untyped_002.phpt",
        "override_plain_set.phpt",
        "parent_get_plain_untyped_uninitialized.phpt",
        "property_promotion.phpt",
    ] {
        reference_phpt(&format!("Zend/tests/property_hooks/{name}"));
    }
    reference_fatal_phpt("Zend/tests/property_hooks/final_prop_promoted_1.phpt");
}

#[test]
fn comparisons_keep_arrays_distinct_from_objects_and_casts_explicit() {
    assert_eq!(
        run(r"<?php
$o = new stdClass;
var_dump($o == [], [] == $o, $o <=> [], [] <=> $o, (array)$o);
"),
        "bool(false)\nbool(false)\nint(1)\nint(-1)\narray(0) {\n}\n"
    );
    reference_phpt("Zend/tests/recursive_array_comparison.phpt");
    assert_eq!(run(r#"<?php
class C { public $next; }
$a = new C; $b = new C; $a->next = $a; $b->next = $b;
for ($i = 0; $i < 2; $i++) {
    try { var_dump($a == $b); } catch (Error $e) { echo $e->getMessage(), "\n"; }
}
var_dump($a == $a, (new C) == (new C));
"#), "Nesting level too deep - recursive dependency?\nNesting level too deep - recursive dependency?\nbool(true)\nbool(true)\n");
}

#[test]
fn plain_slot_fastpath_preserves_reference_visibility_shape_and_hook_slowpaths() {
    assert_eq!(
        run(r#"<?php
#[AllowDynamicProperties]
class Plain { public $p = 1; private $hidden = 2; }
class Typed { public int $p = 1; }
class Hooked { public $p = 1 { get => $this->p; set => $value * 2; } }
class ReadonlySlot { public function __construct(public readonly int $p = 1) {} }
class Child extends Plain {}
$plain = new Plain;
for ($i = 0; $i < 20; $i++) { $plain->p = $i; if ($plain->p !== $i) { echo "bad\n"; } }
$ref = &$plain->p; $plain->p = 21; echo $ref, "\n";
unset($ref); $plain->extra = 4; $plain->p = 22; echo $plain->p, "\n";
foreach ([new Typed, new Hooked, new Child] as $o) { $o->p = 3; echo $o->p, "\n"; }
try { $plain->hidden; } catch (Error $e) { echo "private\n"; }
try { (new ReadonlySlot)->p = 2; } catch (Error $e) { echo "readonly\n"; }
$typed = new Typed; $typedRef = &$typed->p;
try { $typed->p = []; } catch (TypeError $e) { echo "typed-ref\n"; }
echo $typedRef, "\n";
"#),
        "21\n22\n3\n6\n3\nprivate\nreadonly\ntyped-ref\n1\n"
    );
}

#[test]
fn pfa_constant_return_optimization_does_not_capture_or_skip_side_effects() {
    assert_eq!(
        run(r#"<?php
function constant_value() { return 3; }
function effect() { echo "effect\n"; return 4; }
function target($a, $b) { return $a + $b; }
$a = target(constant_value(), ?); $b = target(effect(), ?);
var_dump((new ReflectionFunction($a))->getClosureUsedVariables());
var_dump((new ReflectionFunction($b))->getClosureUsedVariables());
var_dump($a(2), $b(2));
"#),
        "effect\narray(0) {\n}\narray(1) {\n  [\"a\"]=>\n  int(4)\n}\nint(5)\nint(6)\n"
    );
}

#[test]
fn hooked_iteration_and_lazy_magic_keep_scope_and_guards_separate() {
    reference_phpt("Zend/tests/lazy_objects/gh20504-004.phpt");
    reference_phpt("Zend/tests/lazy_objects/gh21478-proxy-get-override.phpt");
    reference_phpt("Zend/tests/property_hooks/property_const_nested.phpt");
}

#[test]
fn plain_interface_implementation_obeys_set_types_and_visibility() {
    reference_fatal_phpt("Zend/tests/property_hooks/gh15140.phpt");
    reference_fatal_phpt("Zend/tests/property_hooks/interface_get_set_readonly.phpt");
}

#[test]
fn proxy_guards_suppress_real_instance_recursion_in_both_directions() {
    reference_phpt("Zend/tests/lazy_objects/clone_creates_object_with_independent_state_002.phpt");
    reference_phpt("Zend/tests/lazy_objects/gh21478-proxy-get-override.phpt");
    assert_eq!(
        run(r#"<?php
class RealInstance {
    public $p;
    function __get($name) { echo "real\n"; global $proxy; return $proxy->$name; }
}
class Proxy extends RealInstance {
    function __get($name) { echo "proxy\n"; return parent::__get($name); }
}
$rc = new ReflectionClass(Proxy::class);
$proxy = $rc->newLazyProxy(function() { return new RealInstance; });
$real = $rc->initializeLazyObject($proxy);
set_error_handler(function($level, $message) { echo str_contains($message, "Undefined property") ? "undefined\n" : "unexpected\n"; });
var_dump($real->x);
"#),
        "real\nundefined\nNULL\n"
    );
}

#[test]
fn proxy_export_cast_and_subclass_reset_preserve_real_storage() {
    assert_eq!(run(r"<?php
class A { public $p = 1; }
class B extends A { public $q = 2; }
$rc = new ReflectionClass(A::class);
$o = new B; $o->p = 3; $o->q = 4;
$rc->resetAsLazyGhost($o, function() {});
var_dump($o->q, $rc->isUninitializedLazyObject($o), $o->p, $o->q);
$proxy = $rc->newLazyProxy(function() { return new A; });
var_dump((array)$proxy);
$proxy->p = 5;
var_dump((array)$proxy);
$export = $rc->newLazyGhost(function($o) { $o->p = 6; });
var_export($export);
"), "int(4)\nbool(true)\nint(1)\nint(4)\narray(0) {\n}\narray(1) {\n  [\"p\"]=>\n  int(5)\n}\n\\A::__set_state(array(\n   \x27p\x27 => 6,\n))");
}

#[test]
fn object_union_rejects_redundant_static_in_both_orders() {
    reference_fatal_phpt(
        "Zend/tests/type_declarations/union_types/redundant_types/object_and_static.phpt",
    );
    reference_fatal_phpt(
        "Zend/tests/type_declarations/union_types/redundant_types/object_and_static2.phpt",
    );
}

#[test]
fn introspection_keeps_positional_references_and_closure_snapshots() {
    assert_eq!(
        run(r#"<?php
function f(&$a, &...$rest) {
    $a = 10; $rest[0] = 20; $rest["named"] = 30;
    echo func_num_args(), ":", serialize(func_get_args()), ":", serialize(func_get_arg(1)), "\n";
}
$a = 1; $b = 2; $c = 3; f($a, $b, named: $c);
echo "$a:$b:$c\n";
$f = function($a, ...$rest) {
    $a = 9; $rest[0] = 8;
    echo func_num_args(), ":", serialize(func_get_args()), "\n";
};
$f(a: 1, extra: 2); $f(1, 2, extra: 3);
"#),
        "2:a:2:{i:0;i:10;i:1;i:20;}:i:20;\n10:20:30\n1:a:1:{i:0;i:9;}\n2:a:2:{i:0;i:9;i:1;i:2;}\n"
    );
}

#[test]
fn named_assertion_and_constructorless_calls_match_reference() {
    reference_phpt("Zend/tests/named_params/assert.phpt");
    reference_phpt("Zend/tests/named_params/ctor_extra_named_args.phpt");
    assert_eq!(
        run(r#"<?php
class C {}
function effect() { echo "effect\n"; return 1; }
new C(effect());
try { new C(...["unknown" => 1]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
echo "ok\n";
"#),
        "effect\nUnknown named parameter $unknown\nok\n"
    );
}

#[test]
fn named_array_unpack_separates_values_but_preserves_existing_references() {
    assert_eq!(
        run(r#"<?php
function f($a = null, &...$rest) { foreach ($rest as &$value) { $value++; } }
$number = 1; $array = ["named" => &$number, 0 => 2];
try { f(...$array); } catch (Error $e) { echo $e->getMessage(), "\n"; }
$array = ["named" => &$number]; $copy = $array;
f(...$array);
echo $number, ":", $array["named"], ":", $copy["named"], "\n";
$array = ["named" => 1]; $copy = $array;
f(...$array);
echo $array["named"], ":", $copy["named"], "\n";
"#),
        "Cannot use positional argument after named argument during unpacking\n2:2:2\n2:1\n"
    );
}

#[test]
fn closure_invoke_preserves_named_gaps_and_reference_arguments() {
    assert_eq!(
        run(r#"<?php
$f = function($a = "a", &$b = null, $c = "c") { $b++; echo "$a:$b:$c\n"; };
$b = 1; $f->__invoke(b: $b, c: "C"); echo "$b\n";
"#),
        "a:2:C\n2\n"
    );
}

#[test]
fn direct_indirect_call_traces_omit_only_compiled_forwarding_frames() {
    assert_eq!(
        run(r#"<?php
function f() {
    foreach (debug_backtrace() as $frame) {
        echo $frame["function"], ":", isset($frame["line"]) ? "line" : "internal", "\n";
    }
}
call_user_func("f"); $call = "call_user_func"; $call("f"); call_user_func_array("f", []);
"#),
        "f:line\nf:internal\ncall_user_func:line\nf:line\n"
    );
}

#[test]
fn variadic_type_errors_omit_the_variadic_parameter_name() {
    assert_eq!(
        run(r#"<?php
function f(int ...$args) {}
try { f(1, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
"#),
        "f(): Argument #2 must be of type int, array given, called in t.php on line 3\n"
    );
}

#[test]
fn foreach_iterators_take_an_object_handle() {
    // `zend_iterator_init`: the iterator of a `foreach` over an `Iterator`
    // or a generator is an object and holds a handle until `FE_FREE`.
    assert_eq!(
        run(r#"<?php
class It implements Iterator {
    private $i = 0;
    function current(): mixed { return 1; }
    function key(): mixed { return 0; }
    function next(): void { $this->i++; }
    function rewind(): void { $this->i = 0; }
    function valid(): bool { return $this->i < 1; }
}
$it = new It;
foreach ($it as $v) { echo spl_object_id(new stdClass), ","; }
echo spl_object_id(new stdClass), ",";
function g() { yield 1; }
foreach (g() as $v) { echo spl_object_id(new stdClass), ","; }
echo spl_object_id(new stdClass), "\n";
"#),
        "3,2,4,3\n"
    );
}

#[test]
fn constant_strlen_folds_without_changing_results() {
    // zend_compile_func_strlen: constant strings fold at compile time; a
    // namespaced unqualified call resolves at run time and is not folded.
    assert_eq!(
        run(r#"<?php
namespace A {
    function strlen($s) { return 42; }
    echo strlen("x"), " ", \strlen("x"), "\n";
}
namespace {
    $s = "abcd";
    echo strlen("hallo"), " ", strlen("a" . "bc"), " ", strlen($s), " ", strlen(""), "\n";
}
"#),
        "42 1\n5 3 4 0\n"
    );
}
