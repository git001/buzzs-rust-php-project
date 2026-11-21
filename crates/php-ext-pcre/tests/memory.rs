// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Replacement memory parity with the real request allocator installed.

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

php_memlimit::install_global_allocator!();

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

#[test]
fn bug81243_exact_reference_output_with_live_allocations() {
    // ext/pcre/tests/bug81243.phpt, including both unmodified comparisons.
    let source = br"<?php
$test_string = str_repeat('Eins zwei drei', 2000);

$replaced = preg_replace('/\s/', '-', $test_string);
$mem0 = memory_get_usage();
$replaced = str_repeat($replaced, 1);
$mem1 = memory_get_usage();
var_dump($mem0 == $mem1);

$replaced = preg_replace_callback('/\s/', function ($_) {return '-';}, $test_string);
$mem0 = memory_get_usage();
$replaced = str_repeat($replaced, 1);
$mem1 = memory_get_usage();
var_dump($mem0 == $mem1);
?>
";
    let sink = Sink::default();
    let opts = VmOptions::default();
    let ext = php_ext_pcre::Pcre;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    let status = vm.run_script(source, "bug81243.php");
    drop(vm);
    assert_eq!(status, 0);
    assert_eq!(&*sink.0.borrow(), b"bool(true)\nbool(true)\n");
}
