// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Master NaN coercion diagnostics and array/object cast callback regressions.

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

fn run(source: &str) -> String {
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let output = String::from_utf8(sink.0.borrow().clone()).expect("UTF-8 output");
    assert_eq!(status, 0, "{output}");
    output
}

#[test]
fn nan_to_other_master_baseline() {
    let output = run(r#"<?php
$nan = fdiv(0, 0);
var_dump($nan);
function implicit_to_bool(bool $v) { var_dump($v); }
function implicit_to_string(string $v) { var_dump($v); }
implicit_to_bool($nan);
implicit_to_string($nan);
var_dump((int) $nan);
var_dump((bool) $nan);
var_dump((string) $nan);
var_dump((array) $nan);
var_dump((object) $nan);
foreach (['null', 'bool', 'int', 'string', 'array', 'object'] as $type) {
    $nan = fdiv(0, 0);
    settype($nan, $type);
    var_dump($nan);
}
"#);
    let normalized = output
        .lines()
        .map(|line| {
            if let Some((message, number)) = line.split_once(" in t.php on line ") {
                number.parse::<u32>().expect("warning line number");
                format!("{message} in t.php on line LINE")
            } else if let Some(rest) = line.strip_prefix("object(stdClass)#") {
                let (handle, properties) = rest.split_once(' ').expect("object dump");
                handle.parse::<u32>().expect("object handle");
                format!("object(stdClass)#ID {properties}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        normalized,
        r#"float(NAN)

Warning: unexpected NAN value was coerced to bool in t.php on line LINE
bool(true)

Warning: unexpected NAN value was coerced to string in t.php on line LINE
string(3) "NAN"

Warning: The float NAN is not representable as an int, cast occurred in t.php on line LINE
int(0)

Warning: unexpected NAN value was coerced to bool in t.php on line LINE
bool(true)

Warning: unexpected NAN value was coerced to string in t.php on line LINE
string(3) "NAN"

Warning: unexpected NAN value was coerced to array in t.php on line LINE
array(1) {
  [0]=>
  float(NAN)
}

Warning: unexpected NAN value was coerced to object in t.php on line LINE
object(stdClass)#ID (1) {
  ["scalar"]=>
  float(NAN)
}

Warning: unexpected NAN value was coerced to null in t.php on line LINE
NULL

Warning: unexpected NAN value was coerced to bool in t.php on line LINE
bool(true)

Warning: The float NAN is not representable as an int, cast occurred in t.php on line LINE
int(0)

Warning: unexpected NAN value was coerced to string in t.php on line LINE
string(3) "NAN"

Warning: unexpected NAN value was coerced to array in t.php on line LINE
array(1) {
  [0]=>
  float(NAN)
}

Warning: unexpected NAN value was coerced to object in t.php on line LINE
object(stdClass)#ID (1) {
  ["scalar"]=>
  float(NAN)
}"#
    );
}

#[test]
fn cast_callbacks_wrap_replacements_without_restarting_conversion() {
    assert_eq!(
        run(r#"<?php
foreach ([null, "changed", [1], (object) ["nested" => 1]] as $replacement) {
    set_error_handler(function ($level, $message) use ($replacement) {
        global $v;
        $v = $replacement;
        echo $level, ":", $message, "\n";
    });
    $v = NAN; $a = (array) $v; echo serialize($a), "\n";
    $v = NAN; $o = (object) $v; echo serialize($o), "\n";
    $v = NAN; settype($v, "array"); echo serialize($v), "\n";
    $v = NAN; settype($v, "object"); echo serialize($v), "\n";
}
"#),
        concat!(
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;N;}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";N;}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;N;}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";N;}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;s:7:\"changed\";}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";s:7:\"changed\";}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;s:7:\"changed\";}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";s:7:\"changed\";}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;a:1:{i:0;i:1;}}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";a:1:{i:0;i:1;}}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;a:1:{i:0;i:1;}}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";a:1:{i:0;i:1;}}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;O:8:\"stdClass\":1:{s:6:\"nested\";i:1;}}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";O:8:\"stdClass\":1:{s:6:\"nested\";i:1;}}\n",
            "2:unexpected NAN value was coerced to array\n",
            "a:1:{i:0;O:8:\"stdClass\":1:{s:6:\"nested\";i:1;}}\n",
            "2:unexpected NAN value was coerced to object\n",
            "O:8:\"stdClass\":1:{s:6:\"scalar\";O:8:\"stdClass\":1:{s:6:\"nested\";i:1;}}\n",
        )
    );
}

#[test]
fn cast_callbacks_preserve_temporary_and_detached_reference_operands() {
    assert_eq!(
        run(r#"<?php
set_error_handler(function ($level, $message) {
    global $v;
    $v = "changed";
    echo $message, "\n";
});
$a = (array) fdiv(0, 0); var_dump($a[0]);
$o = (object) fdiv(0, 0); var_dump($o->scalar);
function &nan_reference() { global $v; return $v; }
$v = NAN; $a = (array) nan_reference(); var_dump($a[0]);
$v = NAN; $o = (object) nan_reference(); var_dump($o->scalar);
set_error_handler(function ($level, $message) {
    global $v;
    unset($v);
    echo $message, "\n";
});
$v = NAN; $alias =& $v; $a = (array) $v; var_dump($a[0]);
$v = NAN; $alias =& $v; $o = (object) $v; var_dump($o->scalar);
"#),
        concat!(
            "unexpected NAN value was coerced to array\nfloat(NAN)\n",
            "unexpected NAN value was coerced to object\nfloat(NAN)\n",
            "unexpected NAN value was coerced to array\nstring(7) \"changed\"\n",
            "unexpected NAN value was coerced to object\nstring(7) \"changed\"\n",
            "unexpected NAN value was coerced to array\nfloat(NAN)\n",
            "unexpected NAN value was coerced to object\nfloat(NAN)\n",
        )
    );
}

#[test]
fn cast_warning_handler_locations_and_suppression() {
    assert_eq!(
        run(r#"<?php
set_error_handler(function ($level, $message, $file, $line) {
    echo $level, ":", $message, ":", $file, ":", $line, "\n";
});
$v = NAN;
$a = (array) $v;
$o = @(object) $v;
restore_error_handler();
$a = @(array) $v;
$o = @(object) $v;
var_dump($a[0], $o->scalar);
"#),
        concat!(
            "2:unexpected NAN value was coerced to array:t.php:6\n",
            "2:unexpected NAN value was coerced to object:t.php:7\n",
            "float(NAN)\nfloat(NAN)\n",
        )
    );
}

#[test]
fn throwing_cast_warning_handler_preserves_exception_and_skips_assignment() {
    assert_eq!(
        run(r#"<?php
$exception = new Exception("nan warning");
set_error_handler(function ($level, $message) use ($exception) {
    global $v;
    $v = "changed";
    throw $exception;
});
$v = NAN; $result = "original";
try { $result = (array) $v; } catch (Exception $caught) {
    var_dump($caught === $exception, $v, $result);
}
$v = NAN;
try { $result = (object) $v; } catch (Exception $caught) {
    var_dump($caught === $exception, $v, $result);
}
"#),
        concat!(
            "bool(true)\nstring(7) \"changed\"\nstring(8) \"original\"\n",
            "bool(true)\nstring(7) \"changed\"\nstring(8) \"original\"\n",
        )
    );
}

#[test]
fn finite_and_infinite_container_casts_do_not_warn() {
    assert_eq!(
        run(r#"<?php
set_error_handler(function ($level, $message) { echo $message, "\n"; });
foreach ([0.0, -0.0, 1.5, INF, -INF] as $v) {
    $a = (array) $v;
    $o = (object) $v;
    var_dump($a[0] === $v, $o->scalar === $v);
}
"#),
        "bool(true)\n".repeat(10)
    );
}
