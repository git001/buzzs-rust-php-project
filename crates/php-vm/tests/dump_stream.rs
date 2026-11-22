// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `var_dump()` and `print_r()` write a large dump as they walk it, so it
//! never collects in memory: under a 16M `memory_limit` a 500000-element
//! array is dumped in full, byte for byte what php 8.6 prints (whose
//! buffered `print_r()` then fails at the limit). One test: the memory
//! accounting is process-wide.

use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

php_memlimit::install_global_allocator!();

/// Counts the output and keeps its end only: a sink that kept 32 MB would
/// count against the script's own limit.
#[derive(Clone, Default)]
struct Tail(Rc<RefCell<(usize, Vec<u8>)>>);

impl Write for Tail {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut t = self.0.borrow_mut();
        t.0 += bytes.len();
        t.1.extend_from_slice(bytes);
        let excess = t.1.len().saturating_sub(256);
        t.1.drain(..excess);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn large_dumps_are_written_while_walking() {
    let sink = Tail::default();
    let options = VmOptions {
        ini: [("display_errors".to_string(), "1".to_string())]
            .into_iter()
            .collect(),
    };
    let mut vm = Vm::new(&options, Box::new(sink.clone()));
    let status = vm.run_script(
        br#"<?php
ini_set("memory_limit", "16M");
$a = [];
for ($i = 0; $i < 500000; $i++) {
    $a[] = "abcdefghij";
}
var_dump($a);
print_r($a);
echo "\nreached ", memory_get_peak_usage() < (16 << 20) ? "under" : "over", "\n";
"#,
        "t.php",
    );
    drop(vm);
    let (len, tail) = sink.0.borrow().clone();
    let tail = String::from_utf8_lossy(&tail).into_owned();
    assert_eq!(status, 0, "{tail}");
    // php 8.6.1: 32777808 bytes of dump (`php -n`, same script).
    assert_eq!(len, 32_777_808 + "\nreached under\n".len(), "{tail}");
    assert!(
        tail.ends_with("[499999] => abcdefghij\n)\n\nreached under\n"),
        "{tail}"
    );
}
