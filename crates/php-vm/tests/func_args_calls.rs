// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `func_get_args()`, `func_num_args()`, `func_get_arg()` and the other
//! scope-introspection functions called other than directly: php-src first
//! refuses a caller that is code rather than a function (the global
//! scope), then a dynamic call (`$f()`, a callback through
//! `zend_call_function()`, a shutdown function). Expected outputs are
//! PHP 8.6 (`php -n`).
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
    let options = VmOptions {
        ini: vec![("display_errors".into(), "1".into())],
    };
    let mut vm = Vm::new(&options, Box::new(sink.clone()));
    let _status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8(out).expect("UTF-8 output")
}

const CASES: &str = r#"<?php
function show(callable $f) {
    try { $r = $f(); echo "[", implode(",", $r), "]\n"; }
    catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
}
function f() { $g = 'func_get_args'; return $g(); }
show(fn() => f(1, 2));
function h2() { $n = 'func_num_args'; return call_user_func($n); }
show(fn() => h2(1));
function k() { return array_map('func_get_arg', [0]); }
show(fn() => k(7));
function k2() { return array_map('func_get_arg', [-1]); }
show(fn() => k2(7));
function k3() { $g = 'func_get_arg'; return $g(-1); }
show(fn() => k3(7));
show(function () { return func_get_args(); });
function q() { $a = 1; $n = 'compact'; return call_user_func($n, 'a'); }
show(fn() => q());
function t() { $a = 1; return array_map('extract', [['b' => 2]]); }
show(fn() => t());
function m($a) { $a = 5; return func_get_args(); }
show(fn() => m(1));
show(fn() => func_get_args());
try { func_get_args(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { $g = 'func_get_args'; $g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { eval('return func_num_args();'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#;

#[test]
fn indirect_calls_fail_as_in_php() {
    assert_eq!(
        run(CASES),
        "Error: Cannot call func_get_args() dynamically
Error: Cannot call func_num_args() dynamically
Error: Cannot call func_get_arg() dynamically
ValueError: func_get_arg(): Argument #1 ($position) must be greater than or equal to 0
ValueError: func_get_arg(): Argument #1 ($position) must be greater than or equal to 0
[]
Error: Cannot call compact() dynamically
Error: Cannot call extract() dynamically
[5]
[]
func_get_args() cannot be called from the global scope
func_get_args() cannot be called from the global scope
func_num_args() must be called from a function context
"
    );
}
