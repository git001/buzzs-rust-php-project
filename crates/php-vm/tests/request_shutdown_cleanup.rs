// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What request shutdown frees after the destructors (`zend_deactivate()`):
//! it calls no destructor itself. Expected outputs are PHP 8.6 (`php -n`).
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
    let _ = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8(out).expect("UTF-8 output")
}

#[test]
fn no_destructor_runs_after_a_fatal_error() {
    // A fatal error that bails out (`zend_objects_store_mark_destructed`):
    // no destructor runs at shutdown, also not for the object in an array
    // cycle that only the collector frees at request end.
    let out = run("<?php
class D { function __destruct() { echo \"dtor\\n\"; } }
$r = [];
$r[0] = &$r;
$r[1] = new D;
$o = new D;
eval('function twice() {} function twice() {}');");
    assert!(out.contains("Cannot redeclare function twice()"), "{out}");
    assert!(!out.lines().any(|l| l == "dtor"), "{out}");
}

#[test]
fn destructors_of_cycles_run_once_at_shutdown() {
    // Without an error the shutdown destructors run, once, for the object
    // in the cycle too; the cleanup afterwards calls none.
    let out = run("<?php
class D { public $self; function __destruct() { echo \"dtor\\n\"; } }
$a = new D;
$a->self = $a;
$r = [];
$r[0] = &$r;
$r[1] = new D;
echo \"end\\n\";");
    assert_eq!(out, "end\ndtor\ndtor\n");
}
