// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `error_include_args` on warnings the library already prefixes with the
//! function name (`php_verror` builds the origin with the arguments).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};

/// A `Write` sink shared with the test.
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

fn run(code: &str, include_args: bool) -> String {
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let opts = VmOptions {
        ini: vec![(
            "error_include_args".to_string(),
            if include_args { "1" } else { "0" }.to_string(),
        )],
    };
    let sink = Sink::default();
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

const SALT: &str =
    r#"password_hash("test", PASSWORD_BCRYPT, ["salt" => str_repeat("1", 22), "cost" => 4]);"#;

#[test]
fn prefixed_library_warning_gets_arguments() {
    let out = run(SALT, true);
    assert!(out.contains("Warning: password_hash("), "{out}");
    assert!(
        out.contains(", '2y', Array): The \"salt\" option has been ignored"),
        "{out}"
    );
}

#[test]
fn prefixed_library_warning_without_arguments() {
    let out = run(SALT, false);
    assert!(
        out.contains("Warning: password_hash(): The \"salt\" option has been ignored"),
        "{out}"
    );
}
