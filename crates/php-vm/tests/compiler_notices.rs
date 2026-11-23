// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Compile-time decisions with observable notices and errors: by-reference
//! returns and yields (`zend_compile_return`, `zend_compile_yield`), the
//! argument fetch mode of `zend_compile_args` (`Cannot use [] for reading`)
//! and the declaration order of enum members (ref: `Zend/zend_compile.c`).
#![expect(clippy::disallowed_methods, reason = "test code")]

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

/// Runs `source` as `t.php`, returning the exit status and the output.
fn run_status(source: &str) -> (i32, String) {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let output = sink.0.borrow().clone();
    (status, String::from_utf8(output).expect("UTF-8 output"))
}

/// Runs `source`, asserting a clean exit, and returns its output.
fn run(source: &str) -> String {
    let (status, output) = run_status(source);
    assert_eq!(status, 0, "{output}");
    output
}

fn notice(message: &str, line: u32) -> String {
    format!("\nNotice: {message} in t.php on line {line}\n")
}

const RETURNED: &str = "Only variable references should be returned by reference";
const YIELDED: &str = "Only variable references should be yielded by reference";

/// `return $a =& $b;` in a by-reference function: php-src master compiles
/// the assignment for reading, a TMP holding the dereferenced value
/// (GH-20628), so `ZEND_RETURN_BY_REF` notices and returns a new reference
/// that is not bound to `$b`.
#[test]
fn by_ref_return_of_assign_ref_notices() {
    let out = run("<?php\n\
         function &c() {\n\
         \x20   static $b = 1;\n\
         \x20   return $a =& $b;\n\
         }\n\
         $x =& c();\n\
         $x++;\n\
         var_dump(c());\n");
    assert_eq!(out, format!("{0}{0}int(1)\n", notice(RETURNED, 4)));
}

/// `fn&() => expr` is `return expr;`: `@f()` is compiled for reading (a
/// TMP, dereferenced), a call is fetched for writing and keeps the
/// reference a by-reference function returned.
#[test]
fn by_ref_arrow_function_returns_like_return() {
    let out = run("<?php\n\
         function &r() { static $v = 1; return $v; }\n\
         $f = fn&() => @r();\n\
         $x =& $f();\n\
         $x++;\n\
         var_dump(r());\n\
         $g = fn&() => r();\n\
         $y =& $g();\n\
         $y++;\n\
         var_dump(r());\n");
    assert_eq!(out, format!("{}int(1)\nint(2)\n", notice(RETURNED, 3)));
}

/// A by-reference call result returned from inside a `foreach` or a `try`
/// with `finally` keeps its reference through the loop free and the
/// finally block (no copy, no notice).
#[test]
fn by_ref_call_result_survives_return_unwinding() {
    let out = run("<?php\n\
         function &r() { static $v = 1; return $v; }\n\
         function &a() { foreach ([1] as $_) { return r(); } }\n\
         function &b() { try { return r(); } finally { echo \"fin\\n\"; } }\n\
         $x =& a();\n\
         $x++;\n\
         $y =& b();\n\
         $y++;\n\
         var_dump(r());\n");
    assert_eq!(out, "fin\nint(3)\n");
}

/// A by-value result returned from a by-reference function inside a loop
/// still notices once.
#[test]
fn by_value_call_result_in_loop_notices() {
    let out = run("<?php\n\
         function v() { return 5; }\n\
         function &a() {\n\
         \x20   foreach ([1] as $_) {\n\
         \x20       return v();\n\
         \x20   }\n\
         }\n\
         $x =& a();\n\
         var_dump($x);\n");
    assert_eq!(out, format!("{}int(5)\n", notice(RETURNED, 5)));
}

/// `foreach` by reference over a by-reference generator binds a yielded
/// constant or a bare `yield` silently (`ZEND_FE_FETCH_RW` wraps the
/// generator's value in a reference): only `ZEND_YIELD` notices.
#[test]
fn by_ref_foreach_over_yielded_values_is_silent() {
    let out = run("<?php\n\
         function &gen() {\n\
         \x20   $b = 1;\n\
         \x20   yield 5;\n\
         \x20   yield;\n\
         \x20   yield $b;\n\
         }\n\
         foreach (gen() as &$v) {\n\
         \x20   var_dump($v);\n\
         }\n");
    assert_eq!(out, format!("{}int(5)\nNULL\nint(1)\n", notice(YIELDED, 4)));
}

/// The loop variable is bound to the generator's current value, as
/// `ZEND_FE_FETCH_RW` makes it a reference in place.
#[test]
fn by_ref_foreach_binds_the_generator_value() {
    let out = run("<?php\n\
         function &gen() {\n\
         \x20   yield 1;\n\
         }\n\
         $g = gen();\n\
         foreach ($g as &$v) {\n\
         \x20   $v = 7;\n\
         \x20   var_dump($g->current());\n\
         }\n");
    assert_eq!(out, format!("{}int(7)\n", notice(YIELDED, 3)));
}

const READING: &str = "Fatal error: Cannot use [] for reading in t.php on line 3";

/// Asserts that `source` fails to compile with `fatal` (no output from the
/// script itself).
fn assert_compile_error(source: &str, fatal: &str) {
    let (status, out) = run_status(source);
    assert_ne!(status, 0, "{out}");
    assert!(out.contains(fatal), "{out}");
    assert!(!out.contains("started"), "{out}");
}

/// `zend_compile_args`: an internal function, and a function declared
/// before the file is compiled, is a known `fbc`, so `[]` passed for a
/// by-value parameter is compiled for reading: a compile error.
#[test]
fn append_for_a_known_by_value_parameter_is_a_compile_error() {
    assert_compile_error("<?php\necho \"started\\n\";\nvar_dump($a[]);\n", READING);
    assert_compile_error(
        "<?php\necho \"started\\n\";\nstr_replace($a[], 1, 2);\n",
        READING,
    );
    assert_compile_error(
        "<?php\nnamespace X;\n\\trim($a[]);\necho \"started\\n\";\n",
        READING,
    );
    assert_compile_error(
        "<?php\nfunction f($v) {}\necho \"started\\n\"; f($a[]->b);\n",
        READING,
    );
    // A function of code that already ran is in the function table too.
    let (status, out) = run_status(
        "<?php\n\
         function g($v) {}\n\
         eval('echo \"started\\n\"; g($a[][0]);');\n\
         echo \"after\\n\";\n",
    );
    assert_ne!(status, 0, "{out}");
    assert!(
        out.contains(
            "Fatal error: Cannot use [] for reading in t.php(3) : eval()'d code on line 1"
        ),
        "{out}"
    );
    assert!(!out.contains("started") && !out.contains("after"), "{out}");
}

/// A by-reference parameter of a known function fetches `[]` for writing
/// (the element is appended), and an unknown callee decides at run time
/// (`BP_VAR_FUNC_ARG`): `f($b[]->c)` throws before appending when `f`
/// takes the argument by value.
#[test]
fn append_for_a_by_ref_or_unknown_parameter_runs() {
    let out = run("<?php\n\
         var_dump(str_replace(\"a\", \"b\", \"aa\", $c[]));\n\
         var_dump($c);\n\
         echo \"started\\n\";\n\
         try { f($b[]->c); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         try { g($b[]->c); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         var_dump($b);\n\
         function f($v) {}\n\
         function g(&$v) {}\n");
    assert_eq!(
        out,
        "string(2) \"bb\"\n\
         array(1) {\n  [0]=>\n  int(2)\n}\n\
         started\n\
         Cannot use [] for reading\n\
         Attempt to modify property \"c\" on null\n\
         array(1) {\n  [0]=>\n  NULL\n}\n"
    );
}

/// An unqualified call in a namespace is resolved at run time: no `fbc`.
/// php-src master compiles a frameless branch beside it when the global
/// function is internal with a frameless handler for that many arguments
/// (`zend_compile_ns_call`), and that branch compiles the arguments for
/// reading; not for a call nested in such a branch's arguments, nor for
/// an arity without a handler (`substr()` needs two arguments, a
/// four-argument `str_replace()` has none).
#[test]
fn namespaced_calls_compile_append_by_their_frameless_branch() {
    assert_compile_error(
        "<?php\nnamespace X;\ntrim($a[]);\necho \"started\\n\";\n",
        READING,
    );
    assert_compile_error(
        "<?php\nnamespace X;\nstr_replace(1, 2, $a[]);\necho \"started\\n\";\n",
        READING,
    );
    let out = run("<?php\n\
         namespace X;\n\
         echo \"started\\n\";\n\
         try { strlen($a[]); } catch (\\Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         try { trim(strtolower($a[])); } catch (\\Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         try { substr($a[]); } catch (\\Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         function f($v) {}\n\
         try { f($a[]); } catch (\\Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         var_dump(str_replace(\"a\", \"b\", \"aa\", $c[]), $c);\n");
    assert_eq!(
        out,
        format!(
            "started\n{}string(2) \"bb\"\narray(1) {{\n  [0]=>\n  int(2)\n}}\n",
            "Cannot use [] for reading\n".repeat(4)
        )
    );
}

/// `zend_compile_class_decl` adds an enum's constants and cases to the
/// constants table in declaration order, also when they share a line
/// (`ConstDecl::cases_before`); `cases()` keeps the cases' own order.
#[test]
fn enum_constants_and_cases_keep_declaration_order() {
    let out = run("<?php\n\
         enum E { const X = 1; case A; const Y = 2; case B; }\n\
         enum F { case A; const X = 1; case B; const Y = 2; const Z = 3; case C; }\n\
         foreach ([E::class, F::class] as $class) {\n\
         \x20   foreach ((new ReflectionClass($class))->getReflectionConstants() as $c) {\n\
         \x20       echo $c->getName(), $c->isEnumCase() ? '*' : '', ' ';\n\
         \x20   }\n\
         \x20   echo \"\\n\";\n\
         }\n\
         foreach ((new ReflectionClass('E'))->getConstants() as $k => $_) { echo $k, ' '; }\n\
         echo \"\\n\";\n\
         foreach (F::cases() as $case) { echo $case->name, ' '; }\n\
         echo \"\\n\";\n");
    assert_eq!(out, "X A* Y B* \nA* X B* Y Z C* \nX A Y B \nA B C \n");
}

/// Each op array holds only the declarations nested in it, so the run-time
/// `DECLARE_FUNCTION` / `DECLARE_CLASS` of a function body find theirs in
/// its own table, and dropping the VM frees every function and class the
/// script compiled: no declared function's code holds the table that holds
/// the function (the `Rc` cycle leaked every user declaration of each
/// request under `php -S` and `--repeat`).
#[test]
fn dropping_the_vm_frees_the_scripts_declarations() {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(
        b"<?php
function outer() {
    function inner() { return 1; }
    if (false) { $f = function () { return fn() => 2; }; }
}
outer();
echo inner(), \"\\n\";
function mk() {
    class K { static function sm() { return 3; } function m() { return function () {}; } }
}
mk();
echo K::sm(), \"\\n\";
if (false) { $g = function () {}; }
",
        "t.php",
    );
    assert_eq!(status, 0);
    assert_eq!(sink.0.borrow().as_slice(), b"1\n3\n");
    let mut functions = Vec::new();
    let mut classes = Vec::new();
    for unit in vm
        .units
        .iter()
        .filter(|u| u.main.code.filename.as_bytes() == b"t.php")
    {
        functions.extend(unit.decls.functions.borrow().iter().map(Rc::downgrade));
        classes.extend(unit.decls.classes.borrow().iter().map(Rc::downgrade));
    }
    // inner, the arrow function, $f, outer, K::m's closure, mk, $g; K.
    assert_eq!((functions.len(), classes.len()), (7, 1));
    drop(vm);
    for (i, f) in functions.iter().enumerate() {
        assert!(f.upgrade().is_none(), "function #{i} outlived the VM");
    }
    assert!(classes[0].upgrade().is_none(), "class K outlived the VM");
}
