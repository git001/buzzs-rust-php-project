// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `filter` extension.
//! Expected values come from php-src (`/usr/bin/php`).

const EXT: php_ext_filter::Filter = php_ext_filter::Filter;

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

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let ext = EXT;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn validate_and_sanitize() {
    assert_eq!(
        run(
            r"var_dump(filter_var('123', FILTER_VALIDATE_INT), filter_var('abc', FILTER_VALIDATE_INT, FILTER_NULL_ON_FAILURE),
            filter_var('a@b.co', FILTER_VALIDATE_EMAIL), filter_var('<b>', FILTER_SANITIZE_SPECIAL_CHARS));"
        ),
        "int(123)\nNULL\nstring(6) \"a@b.co\"\nstring(11) \"&#60;b&#62;\"\n"
    );
}

#[test]
fn regexp_and_callback() {
    assert_eq!(
        run(
            r"var_dump(filter_var('abc', FILTER_VALIDATE_REGEXP, ['options' => ['regexp' => '/^a/']]),
            filter_var('abc', FILTER_CALLBACK, ['options' => fn ($s) => strrev($s)]));"
        ),
        "string(3) \"abc\"\nstring(3) \"cba\"\n"
    );
}

/// `php_filter_callback()` (master, c7da728 "Check callback validity
/// once") checks the option with `IS_CALLABLE_SUPPRESS_DEPRECATIONS` and
/// calls that resolution: `'parent::m'` resolves in the calling scope
/// without the deprecation PHP 8.3 raised through `call_user_function()`.
#[test]
fn callback_option_resolves_in_the_calling_scope_without_deprecations() {
    assert_eq!(
        run(r#"
class foo { public function fc($v) { return "fc:$v"; } }
class bar extends foo {
    function go() {
        var_dump(filter_var('v', FILTER_CALLBACK, ['options' => 'parent::fc']));
        var_dump(filter_var('w', FILTER_CALLBACK, ['options' => [$this, 'parent::fc']]));
    }
}
(new bar)->go();
try { filter_var('v', FILTER_CALLBACK, ['options' => 'parent::fc']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
"#),
        "string(4) \"fc:v\"\nstring(4) \"fc:w\"\nfilter_var(): Option must be a valid callback\n"
    );
}

#[test]
fn arrays() {
    assert_eq!(
        run(
            r"var_dump(filter_var_array(['a' => '1', 'b' => 'x'], ['a' => FILTER_VALIDATE_INT, 'b' => FILTER_VALIDATE_INT, 'c' => FILTER_DEFAULT]));"
        ),
        "array(3) {\n  [\"a\"]=>\n  int(1)\n  [\"b\"]=>\n  bool(false)\n  [\"c\"]=>\n  NULL\n}\n"
    );
}

#[test]
fn lists_and_inputs() {
    assert_eq!(
        run(
            r"var_dump(filter_id('int'), filter_list()[20], filter_input(INPUT_GET, 'x'), filter_has_var(INPUT_GET, 'x'));"
        ),
        "int(257)\nstring(8) \"callback\"\nNULL\nbool(false)\n"
    );
}

#[test]
fn throw_on_failure() {
    assert_eq!(
        run(
            r"try { filter_var('x', FILTER_VALIDATE_INT, FILTER_THROW_ON_FAILURE); }
            catch (Filter\FilterFailedException $e) { echo get_class($e), ': ', $e->getMessage(); }"
        ),
        "Filter\\FilterFailedException: filter validation failed: filter int not satisfied by 'x'"
    );
}
