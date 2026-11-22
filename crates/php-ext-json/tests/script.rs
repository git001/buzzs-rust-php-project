// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `json` extension.

#![allow(clippy::needless_raw_string_hashes)]

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

fn run(code: &str) -> String {
    run_configured(code, |_| {})
}

fn run_configured(code: &str, configure: impl FnOnce(&mut Vm)) -> String {
    // Its own counter: the tests run in parallel and set different
    // `memory_limit`s.
    php_memlimit::attach_thread();
    let sink = Sink::default();
    let ext = php_ext_json::Json;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    configure(&mut vm);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn encode_flags_and_constants() {
    let out = run(
        r#"echo json_encode(["a" => 1, "b" => [1.0, "é/<"]], JSON_PRESERVE_ZERO_FRACTION | JSON_UNESCAPED_SLASHES | JSON_HEX_TAG);"#,
    );
    assert_eq!(out, r#"{"a":1,"b":[1.0,"\u00e9/\u003C"]}"#);
}

#[test]
fn decode_objects_and_last_error() {
    let out = run(r#"$o = json_decode('{"x":{"y":[1,2]},"":3}');
        echo $o->x->y[1], $o->{""}, json_last_error(), '|';
        var_dump(json_decode('[1', true), json_last_error(), json_last_error_msg());"#);
    assert_eq!(
        out,
        "230|NULL\nint(4)\nstring(30) \"Syntax error near location 1:3\"\n"
    );
}

#[test]
fn throw_on_error_uses_json_exception_with_code() {
    let out = run(
        r#"try { json_decode("{", false, 512, JSON_THROW_ON_ERROR); }
        catch (JsonException $e) { echo get_class($e), ':', $e->getCode(), ':', $e->getMessage(), ':', $e instanceof Exception ? 'E' : '-'; }"#,
    );
    assert_eq!(out, "JsonException:4:Syntax error near location 1:2:E");
}

#[test]
fn value_errors_for_depth() {
    let out = run(
        r#"try { json_decode("1", false, 0); } catch (ValueError $e) { echo $e->getMessage(), '|'; }
        try { json_validate("1", 1, 5); } catch (ValueError $e) { echo $e->getMessage(); }"#,
    );
    assert_eq!(
        out,
        "json_decode(): Argument #3 ($depth) must be greater than 0|json_validate(): Argument #3 ($flags) must be a valid flag (allowed flags: JSON_INVALID_UTF8_IGNORE)"
    );
}

#[test]
fn json_serializable_and_enums() {
    let out = run(
        r#"class P implements JsonSerializable { function __construct(private $v) {} function jsonSerialize(): mixed { return ['v' => $this->v]; } }
        enum S: string { case A = 'a'; }
        class Q { public $pub = 1; protected $pro = 2; private $pri = 3; }
        echo json_encode([new P(5), S::A, new Q]);"#,
    );
    assert_eq!(out, r#"[{"v":5},"a",{"pub":1}]"#);
}

#[test]
fn hooked_properties_use_get_hooks() {
    let out = run(
        r#"class C { public $a = 1; public $h { get => 5; } public $v { set { } } }
        echo json_encode(new C);"#,
    );
    assert_eq!(out, r#"{"a":1,"h":5}"#);
}

#[test]
fn lazy_objects_with_hooks_are_initialized() {
    let out = run(r#"#[AllowDynamicProperties]
        class C {
            public int $a;
            public int $b { get => $this->b * 10; set(int $v) { $this->b = $v; } }
            public int $c { get => $this->a + 1; }
            protected int $p = 7;
            private int $q { get => 8; }
            public function __construct() { echo 'ctor|'; $this->a = 1; $this->b = 2; $this->d = 4; }
        }
        $r = new ReflectionClass(C::class);
        $g = $r->newLazyGhost(function ($o) { echo 'ghost|'; $o->__construct(); });
        echo json_encode($g), '|';
        $p = $r->newLazyProxy(function ($o) { echo 'proxy|'; return new C; });
        echo json_encode($p), '|';
        echo json_encode($p);"#);
    assert_eq!(
        out,
        r#"ghost|ctor|{"a":1,"b":20,"c":2,"d":4}|proxy|ctor|{"a":1,"b":20,"c":2,"d":4}|{"a":1,"b":20,"c":2,"d":4}"#
    );
}

#[test]
fn lazy_initializer_exception_aborts_encoding() {
    let out = run(r#"class C { public $b = 0; public $a { get => $this->b; } }
        $o = (new ReflectionClass(C::class))->newLazyProxy(function () { throw new Exception('init'); });
        try { var_dump(json_encode($o)); } catch (Exception $e) { echo get_class($e), ':', $e->getMessage(); }"#);
    assert_eq!(out, "Exception:init");
}

#[test]
fn hooked_non_public_and_uninitialized_properties_are_absent() {
    let out = run(r#"class C {
            public int $u;
            public $a = 1 { get => $this->a + 1; set { $this->a = $value; } }
            protected $b { get => 3; }
            private $c { get => 4; }
            public function enc() { return json_encode($this); }
        }
        echo json_encode(new C), (new C)->enc();"#);
    assert_eq!(out, r#"{"a":2}{"a":2}"#);
}

#[test]
fn recursion_is_detected() {
    let out = run(r#"$a = ['x' => 1]; $a['self'] = &$a;
        var_dump(json_encode($a), json_last_error_msg());
        echo json_encode($a, JSON_PARTIAL_OUTPUT_ON_ERROR);"#);
    assert_eq!(
        out,
        "bool(false)\nstring(18) \"Recursion detected\"\n{\"x\":1,\"self\":null}"
    );
}

#[test]
fn native_stack_exhaustion_is_a_json_depth_error() {
    let out = run_configured(
        r#"echo json_encode(1), '|';
        var_dump(json_encode([]), json_last_error(), json_last_error_msg());
        echo json_encode(new stdClass, JSON_PARTIAL_OUTPUT_ON_ERROR | JSON_PRETTY_PRINT), '|';
        json_decode("\xFF");
        try { json_encode([], JSON_THROW_ON_ERROR); }
        catch (JsonException $e) { echo $e->getCode(), ':', $e->getMessage(), '|', json_last_error(), '|'; }
        echo json_encode([], JSON_THROW_ON_ERROR | JSON_PARTIAL_OUTPUT_ON_ERROR), '|', json_last_error();"#,
        |vm| vm.set_native_stack_size(0),
    );
    assert_eq!(
        out,
        "1|bool(false)\nint(1)\nstring(28) \"Maximum stack depth exceeded\"\nnull|1:Maximum stack depth exceeded|5|null|1"
    );
}

#[test]
fn native_stack_allows_encoding_beyond_default_nesting_limit() {
    const STACK_SIZE: usize = 256 << 20;
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(|| {
            let out = run_configured(
                r#"$a = 1;
                for ($i = 0; $i < 3001; $i++) { $a = [$a]; }
                echo json_encode($a, 0, 4000), '|', json_last_error();"#,
                |vm| vm.set_native_stack_size(STACK_SIZE),
            );
            assert_eq!(out, format!("{}1{}|0", "[".repeat(3001), "]".repeat(3001)));
        })
        .expect("spawn")
        .join()
        .expect("JSON stack thread");
}

#[test]
fn memory_limit_stops_json_output_growth() {
    for code in [
        r#"ini_set("memory_limit", "8M"); $subject = str_repeat("\x01", 2 * 1024 * 1024); json_encode($subject); echo "not reached";"#,
        r#"ini_set("memory_limit", "8M"); $subject = []; $item = str_repeat("\x01", 20000); for ($i = 0; $i < 200; $i++) { $subject[] = $item; } json_encode($subject, JSON_PARTIAL_OUTPUT_ON_ERROR); echo "not reached";"#,
    ] {
        let out = run(code);
        assert!(
            out.contains("Allowed memory size of 8388608 bytes exhausted"),
            "{out}"
        );
        assert!(!out.contains("not reached"));
    }
}

#[test]
fn memory_limit_is_modelled() {
    let out = run(
        r#"ini_set('memory_limit', '4M'); json_encode(str_repeat("\x01", 1 << 20)); echo "not reached";"#,
    );
    assert!(
        out.contains("Allowed memory size of 4194304 bytes exhausted"),
        "{out}"
    );
    assert!(!out.contains("not reached"));
}
