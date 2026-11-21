// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `error_include_args` origins and master's `require` failure Error
//! (`php_verror`, `php_message_handler_for_zend`).

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

fn run(source: &str, ini: &[(&str, &str)]) -> String {
    let sink = Sink::default();
    let options = VmOptions {
        ini: ini
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    };
    let mut vm = Vm::new(&options, Box::new(sink.clone()));
    let _status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8(out).expect("UTF-8 output")
}

const MISSING: &str = "/nonexistent-phprust-dir/missing.php";

#[test]
fn include_origin_without_args() {
    let out = run(&format!("<?php include '{MISSING}'; echo 'done';"), &[]);
    assert!(
        out.contains("Warning: include(): Failed to open stream: No such file or directory"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "Warning: include(): Failed opening '{MISSING}' for inclusion"
        )),
        "{out}"
    );
    assert!(out.ends_with("done"), "{out}");
}

#[test]
fn include_origin_with_args() {
    let out = run(
        "<?php $f = 'no_such_file'; include $f;",
        &[("error_include_args", "1")],
    );
    assert!(
        out.contains("Warning: include('no_such_file'): Failed to open stream"),
        "{out}"
    );
    assert!(
        out.contains("Warning: include('no_such_file'): Failed opening 'no_such_file'"),
        "{out}"
    );
}

#[test]
fn require_failure_throws_error() {
    let out = run(
        &format!(
            "<?php try {{ require '{MISSING}'; }} catch (Error $e) {{ echo get_class($e), ': ', $e->getMessage(); }}"
        ),
        &[],
    );
    assert!(
        out.contains("Warning: require(): Failed to open stream"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "Error: Failed opening required '{MISSING}' (include_path="
        )),
        "{out}"
    );
}

/// `PHP_FUNCTION(assert)` (`ext/standard/assert.c`): dynamic calls honour
/// `zend.assertions` and `assert.active` (php-src
/// `ext/standard/tests/assert/gh18509.phpt`, `assert_basic1.phpt`).
#[test]
fn assert_dynamic_call_and_active() {
    let src = "<?php $c = 'assert'; var_dump($c(false));";
    assert_eq!(run(src, &[("zend.assertions", "0")]), "bool(true)\n");
    let src = "<?php var_dump(assert(false));";
    assert_eq!(run(src, &[("assert.active", "0")]), "bool(true)\n");
}

/// The `AssertionError` code is `E_ERROR`; the startup `assert.callback`
/// gets file, line, `null` and the description; an invalid callback
/// throws "Invalid callback" (`zend_call_function`).
#[test]
fn assert_callback_and_exception() {
    let src = "<?php try { assert(false, 'm'); } catch (AssertionError $e) { echo $e->getMessage(), $e->getCode(); }";
    assert_eq!(run(src, &[]), "m1");
    let src = "<?php function cb($f, $l, $u, $d) { var_dump($f, $l, $u, $d); }\nvar_dump(assert(false, 'd'));";
    assert_eq!(
        run(
            src,
            &[
                ("assert.callback", "cb"),
                ("assert.exception", "0"),
                ("display_errors", "1"),
            ]
        ),
        "string(5) \"t.php\"\nint(2)\nNULL\nstring(1) \"d\"\n\nWarning: assert(): d failed in t.php on line 2\nbool(false)\n"
    );
    let src = "<?php try { var_dump(assert(false)); } catch (Error $e) { echo $e->getMessage(); }";
    assert_eq!(
        run(
            src,
            &[
                ("assert.callback", "nope"),
                ("assert.exception", "0"),
                ("assert.warning", "0"),
            ]
        ),
        "Invalid callback nope, function \"nope\" not found or invalid function name"
    );
}

/// `ini_set()` takes any scalar even under `strict_types` and rejects
/// other types itself (`Z_TYPE_P(new_value) > IS_STRING`, php-src
/// `ext/standard/tests/general_functions/ini_set_types.phpt`).
#[test]
fn ini_set_value_types() {
    let src = "<?php declare(strict_types=1);\nini_set('precision', 6); echo ini_get('precision'), \"\\n\";\ntry { ini_set('foo', []); } catch (TypeError $e) { echo $e->getMessage(); }";
    assert_eq!(
        run(src, &[]),
        "6\nini_set(): Argument #2 ($value) must be of type string|int|float|bool|null"
    );
}

/// The `error_log` file timestamp is `d-M-Y H:i:s e` in the default
/// timezone, which ext/date sets (php-src
/// `ext/standard/tests/general_functions/bug60723.phpt`); without it, UTC.
#[test]
fn error_log_file_dates_in_utc_without_ext_date() {
    let log = std::env::temp_dir().join(format!("phprust-log-date-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let sink = Sink::default();
    let options = VmOptions {
        ini: [
            ("log_errors".to_string(), "1".to_string()),
            ("display_errors".to_string(), "0".to_string()),
            ("error_log".to_string(), log.display().to_string()),
        ]
        .into_iter()
        .collect(),
    };
    let mut vm = Vm::new(&options, Box::new(sink.clone()));
    let _status = vm.run_script(b"<?php echo $undefined;", "t.php");
    drop(vm);
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let (date, rest) = logged.split_once("] ").expect("dated line");
    assert_eq!(
        rest,
        "PHP Warning:  Undefined variable $undefined in t.php on line 1\n"
    );
    assert_eq!(date.len(), "[21-Nov-2026 17:11:51 UTC".len(), "{logged}");
    assert!(date.starts_with('[') && date.ends_with(" UTC"), "{logged}");
}

/// Shutdown functions run without a previous frame: `func_*_args()` see a
/// dynamic call and `forward_static_call()` has no class scope (php-src
/// `register_shutdown_functions_without_previous_call_frame_0[123].phpt`).
#[test]
fn builtins_called_without_a_previous_frame() {
    for f in ["func_get_args", "func_num_args"] {
        let out = run(
            &format!("<?php register_shutdown_function('{f}'); echo 'Done';"),
            &[("display_errors", "1")],
        );
        assert!(
            out.starts_with("Done")
                && out.contains(&format!("Uncaught Error: Cannot call {f}() dynamically")),
            "{out}"
        );
    }
    let src = "<?php class A { static function t() { return forward_static_call('strlen', 'abc'); } }\necho A::t();\ntry { forward_static_call('strlen', 'x'); } catch (Error $e) { echo $e->getMessage(); }";
    assert_eq!(
        run(src, &[]),
        "3Cannot call forward_static_call() when no class scope is active"
    );
}

/// `unregister_tick_function()` compares resolved callables
/// (`zend_fcc_equals`): a first-class callable equals the function,
/// method, trampoline or `__invoke` it wraps (php-src
/// `ext/standard/tests/general_functions/gh22118.phpt`).
#[test]
fn unregister_tick_function_matches_first_class_callables() {
    let src = r#"<?php
declare(ticks=1);
$n = 0;
function t(): void { global $n; $n++; }
class T {
    public int $a = 0;
    public int $m = 0;
    public int $i = 0;
    public function a(): void { $this->a++; }
    public function __invoke(): void { $this->i++; }
    public function __call(string $name, array $args): void { $this->m++; }
    public function run(): void {
        register_tick_function($this->a(...));
        unregister_tick_function([$this, 'a']);
        register_tick_function($this->missing(...));
        unregister_tick_function($this->missing(...));
        register_tick_function($this(...));
        unregister_tick_function($this);
        echo "{$this->a} {$this->m} {$this->i}\n";
    }
}
register_tick_function(t(...));
unregister_tick_function('t');
echo "$n\n";
(new T())->run();
"#;
    assert_eq!(run(src, &[]), "1\n1 1 1\n");
}

#[test]
fn html_errors_link_include_warnings_to_the_manual() {
    let out = run(
        "<?php include_once 'nonexistent.php';",
        &[
            ("html_errors", "1"),
            ("docref_root", "/"),
            ("docref_ext", ".html"),
        ],
    );
    assert!(
        out.contains(
            "<b>Warning</b>:  include_once() [<a href='/function.include-once.html'>\
             function.include-once.html</a>]: Failed to open stream: No such file or directory \
             in <b>t.php</b>"
        ),
        "{out}"
    );
}

#[test]
fn html_errors_escape_docref_messages_once() {
    let out = run(
        "<?php include '<x&y>';",
        &[("html_errors", "1"), ("docref_ext", ".html")],
    );
    // No docref_root: no link, and the message is escaped exactly once.
    assert!(
        out.contains("<b>Warning</b>:  include(): Failed opening '&lt;x&amp;y&gt;' for inclusion"),
        "{out}"
    );
}
