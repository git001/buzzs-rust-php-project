// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `standard` misc
//! extension.

#![allow(clippy::needless_raw_string_hashes)]

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_runtime::{Value, ZArray};
use php_vm::ext::{Extension, ParseBodyOptions};
use php_vm::{Vm, VmOptions, VmResult};

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
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    run_with(&VmOptions::default(), &exts, code)
}

fn run_with(opts: &VmOptions, exts: &[&dyn Extension], code: &str) -> String {
    let sink = Sink::default();
    let mut vm = Vm::with_extensions(opts, Box::new(sink.clone()), exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn crypt_and_password() {
    let out = run(r#"echo crypt("rasmuslerdorf", '$1$rasmusle$'), '|';
        var_dump(password_verify("x", '$2y$04$usesomesillystringfore7hnbRJHxXVLeakoG8K30oukPsA.ztMG'));
        echo password_get_info('$2y$10$abcdefghijklmnopqrstuuG1eFXJp.XcXo5hn7rRYoB5S6Swl8yFG')['algoName'], '|', PASSWORD_BCRYPT;"#);
    assert_eq!(
        out,
        "$1$rasmusle$rISCgZzpwk3UhDidwXvin0|bool(false)\nbcrypt|2y"
    );
}

/// `crypt()`, `password_hash()` and `password_verify()` mark their password
/// parameter `#[\SensitiveParameter]` (ext/standard/basic_functions.stub.php),
/// so backtraces show `Object(SensitiveParameterValue)` instead of it.
#[test]
fn password_parameters_are_sensitive() {
    let out = run(
        r#"try { password_hash("foo", "Invalid"); } catch (ValueError $e) { echo $e->getTraceAsString(), "|"; }
try { password_verify("foo"); } catch (ArgumentCountError $e) { echo $e->getTraceAsString(), "|"; }
try { crypt("foo"); } catch (ArgumentCountError $e) { echo $e->getTraceAsString(); }"#,
    );
    assert_eq!(
        out,
        "#0 test.php(1): password_hash(Object(SensitiveParameterValue), 'Invalid')\n#1 {main}|\
         #0 test.php(2): password_verify(Object(SensitiveParameterValue))\n#1 {main}|\
         #0 test.php(3): crypt(Object(SensitiveParameterValue))\n#1 {main}"
    );
}

#[test]
fn shell_escaping_and_exec() {
    let out = run(
        r#"echo escapeshellarg("it's"), '|', escapeshellcmd("a;b"), '|';
        $r = exec("printf 'a\nb  \n'", $lines, $code);
        echo $r, '|', implode(',', $lines), '|', $code, '|';
        system("echo sys", $c2); echo $c2;"#,
    );
    assert_eq!(out, "'it'\\''s'|a\\;b|b|a,b|0|sys\n0");
}

#[test]
fn query_strings() {
    let out = run(
        r#"echo http_build_query(['a' => [1, 2], 'b' => 'x y'], 'n', ';', PHP_QUERY_RFC3986), '|';
        parse_str("x[]=1&x[]=2&y=a+b", $r); echo $r['x'][1], $r['y'];"#,
    );
    assert_eq!(out, "a%5B0%5D=1;a%5B1%5D=2;b=x%20y|2a b");
}

#[test]
fn network_conversions() {
    let out = run(
        r#"var_dump(ip2long("10.0.0.1"), long2ip(167772161), inet_ntop(inet_pton("::1")), ip2long("1.2.3"));"#,
    );
    assert_eq!(
        out,
        "int(167772161)\nstring(8) \"10.0.0.1\"\nstring(3) \"::1\"\nbool(false)\n"
    );
}

#[test]
fn environment() {
    let out = run(r#"putenv("PHPRUST_T=1"); echo getenv("PHPRUST_T"), '|';
        try { putenv("=x"); } catch (ValueError $e) { echo $e->getMessage(), '|'; }
        echo php_uname('s'), '|', is_int(getmypid()) ? 'pid' : '-';"#);
    assert_eq!(
        out,
        "1|putenv(): Argument #1 ($assignment) must have a valid syntax|Linux|pid"
    );
}

#[test]
fn highlighting() {
    let out = run(r#"echo highlight_string('<?php echo 1; ?>', true);"#);
    assert!(
        out.starts_with(
            "<pre><code style=\"color: #000000\"><span style=\"color: #0000BB\">&lt;?php "
        ),
        "{out}"
    );
    assert!(out.ends_with("</code></pre>"), "{out}");
}

#[test]
fn setlocale_validates_all_scalar_args_before_trying_locales() {
    let out = run(r#"
        setlocale(LC_ALL, 0, '0');
        setlocale(LC_ALL, '0', 0);
        foreach (['C', 'zz_ZZ.nope'] as $first) {
            try { setlocale(LC_ALL, $first, ['C']); }
            catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        }
        class LocaleName {
            function __toString(): string { echo "converted\n"; return 'C'; }
        }
        echo setlocale(LC_ALL, 'C', new LocaleName), "\n";
        try { setlocale(LC_ALL, 'C', "bad\0locale"); }
        catch (ValueError $e) { echo $e->getMessage(), "\n"; }
        try { setlocale(LC_ALL, ['C'], 'C'); }
        catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
        echo setlocale(LC_ALL, ['zz_ZZ.nope', new LocaleName]), "\n";
        class ThrowingLocale {
            function __toString(): string { throw new Error('conversion failed'); }
        }
        try { setlocale(LC_ALL, 'C', new ThrowingLocale); }
        catch (Error $e) { echo $e->getMessage(), "\n"; }
    "#);
    assert_eq!(
        out,
        "setlocale(): Argument #3 must be of type ?string, array given\n\
         setlocale(): Argument #3 must be of type ?string, array given\n\
         converted\nC\nsetlocale(): Argument #3 must not contain any null bytes\n\
         setlocale() expects exactly 2 arguments when argument #2 ($locales) is an array, 3 given\n\
         converted\nC\nconversion failed\n"
    );
}

#[test]
fn source_return_buffers_obey_the_output_handler_lock() {
    for (name, call) in [
        ("php_strip_whitespace", "php_strip_whitespace(__FILE__)"),
        ("highlight_file", "highlight_file(__FILE__, true)"),
        ("highlight_string", "highlight_string(__FILE__, true)"),
    ] {
        // As run-tests.php runs the .phpt tests: no fatal error backtrace.
        let opts = VmOptions {
            ini: vec![("fatal_error_backtraces".to_string(), "0".to_string())],
        };
        let ext = php_ext_stdmisc::StdMisc;
        let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
        let out = run_with(
            &opts,
            &exts,
            &format!(
                "ob_start(function($s) {{\n{call};\nreturn $s;\n}});\necho 'discarded';\nob_end_flush();"
            ),
        );
        assert_eq!(
            out,
            format!(
                "\nFatal error: {name}(): Cannot use output buffering in output buffering display handlers in test.php on line 2\n"
            )
        );
    }
}

#[test]
fn returned_object_destructor_runs_inside_the_output_handler() {
    let out = run(r#"
        class OutputValue {
            function __toString() { return 'discarded'; }
            function __destruct() { highlight_string('x', true); }
        }
        ob_start(function($s) { return new OutputValue; });
        echo 'buffered';
        ob_end_flush();
    "#);
    assert!(
        out.contains(
            "Fatal error: highlight_string(): Cannot use output buffering in output buffering display handlers"
        ),
        "{out}"
    );
    assert!(!out.contains("discarded"), "{out}");
}

#[test]
fn gh20906_destructors_and_captured_callbacks_obey_the_output_lock() {
    for (name, call) in [
        ("php_strip_whitespace", "php_strip_whitespace(__FILE__)"),
        ("highlight_file", "highlight_file(__FILE__, true)"),
        ("highlight_string", "highlight_string(__FILE__, true)"),
    ] {
        let out = run(&format!(
            "class A {{
                function __destruct() {{
                    {call};
                    echo 'x';
                    $c = new A;
                    ob_start(function () use ($c) {{ return '/'; }}, 1);
                    ob_start(function () use (&$c) {{ $c = new A; return '/'; }}, 1);
                }}
            }}
            new A;"
        ));
        assert!(
            out.contains(&format!(
                "Fatal error: {name}(): Cannot use output buffering in output buffering display handlers"
            )),
            "{out}"
        );
    }
}

#[test]
fn source_return_buffers_capture_open_warnings_and_restore_the_stack() {
    let out = run(r#"ob_start();
        set_error_handler(function($n, $m) { var_dump(ob_get_level()); return true; });
        var_dump(highlight_file('/nonexistent-gh20906.php', true), ob_get_level());
        var_dump(php_strip_whitespace('/nonexistent-gh20906.php'), ob_get_level());"#);
    assert_eq!(
        out,
        "int(2)\nint(2)\nbool(false)\nint(1)\nint(2)\nstring(0) \"\"\nint(1)\n"
    );
}

#[test]
fn source_return_buffers_are_removed_on_an_open_exception() {
    let out = run(r#"ob_start();
        set_error_handler(function($n, $m) { throw new Exception($m); });
        try { highlight_file('/nonexistent-gh20906.php', true); } catch (Exception $e) {}
        var_dump(ob_get_level());
        try { php_strip_whitespace('/nonexistent-gh20906.php'); } catch (Exception $e) {}
        var_dump(ob_get_level());"#);
    assert_eq!(out, "int(1)\nint(1)\n");
}

#[test]
fn highlight_return_buffer_does_not_invoke_the_outer_handler() {
    let out = run(r#"ob_start(function($s) { return strtoupper($s); }, 1);
        $s = highlight_string('x', true);
        $level = ob_get_level();
        ob_end_clean();
        var_dump($level, str_contains($s, '<code'));"#);
    assert_eq!(out, "int(1)\nbool(true)\n");
}

#[test]
fn setlocale_strict_scalar_args_and_weak_array_elements() {
    let out = run(r#"declare(strict_types=1);
        try { setlocale(LC_ALL, 0, '0'); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        try { setlocale(LC_ALL, '0', 0); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        class LocaleName {
            function __toString(): string { echo "converted\n"; return 'C'; }
        }
        try { setlocale(LC_ALL, new LocaleName); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        try { setlocale(LC_ALL, 'C', new LocaleName); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        echo setlocale(LC_ALL, [new LocaleName]), "\n";
        setlocale(LC_ALL, 'C', null);
        echo "nullable\n";
    "#);
    assert_eq!(
        out,
        "setlocale(): Argument #2 ($locales) must be of type array|string|null, int given\n\
         setlocale(): Argument #3 must be of type ?string, int given\n\
         setlocale(): Argument #2 ($locales) must be of type array|string|null, LocaleName given\n\
         setlocale(): Argument #3 must be of type ?string, LocaleName given\n\
         converted\nC\nnullable\n"
    );
}

#[test]
fn scanner_open_failures_use_php_stream_errors() {
    let out = run(r#"
        set_error_handler(function($n, $s) { echo $s, "\n"; });
        var_dump(php_strip_whitespace(str_repeat('A', 300)));
        var_dump(ob_get_contents());
        try { php_strip_whitespace(''); }
        catch (ValueError $e) { echo $e->getMessage(), "\n"; }
        try { php_strip_whitespace("bad\0path"); }
        catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    "#);
    assert_eq!(
        out,
        "php_strip_whitespace(): Failed to open stream: File name too long\n\
         string(0) \"\"\nbool(false)\nPath must not be empty\n\
         php_strip_whitespace(): Argument #1 ($filename) must not contain any null bytes\n"
    );
}

#[test]
fn scanner_reads_wrappers_and_include_path() {
    let misc = php_ext_stdmisc::StdMisc;
    let streams = php_ext_streams::Streams;
    let opts = VmOptions {
        ini: vec![("allow_url_include".to_string(), "1".to_string())],
    };
    let out = run_with(
        &opts,
        &[&php_ext_stdstr::StdStr, &streams, &misc],
        r#"
        class SourceWrapper {
            public $context;
            private $source = '<?php /* comment */ echo 1; ?>';
            private $position = 0;
            function stream_open($path, $mode, $options, &$opened_path) {
                echo $path, "\n";
                $opened_path = $path;
                return true;
            }
            function stream_read($length) {
                $piece = substr($this->source, $this->position, 3);
                $this->position += strlen($piece);
                return $piece;
            }
            function stream_eof() { return $this->position >= strlen($this->source); }
            function stream_stat() { return ['mode' => 0100644, 'size' => strlen($this->source)]; }
            function url_stat($path, $flags) { return $this->stream_stat(); }
        }
        stream_wrapper_register('source', SourceWrapper::class);
        set_include_path('source://root');
        echo php_strip_whitespace('file.php'), "\n";
        set_include_path('.');
        echo php_strip_whitespace('data:text/plain,%3C%3Fphp%20echo%202%3B%20%3F%3E'), "\n";
        class ThrowingSource {
            public $context;
            function stream_open($path, $mode, $options, &$opened_path) {
                throw new Error('open failed');
            }
        }
        stream_wrapper_register('throwing', ThrowingSource::class);
        try { php_strip_whitespace('throwing://file.php'); }
        catch (Error $e) { echo $e->getMessage(), "\n"; }
    "#,
    );
    assert_eq!(
        out,
        "source://root/file.php\n<?php  echo 1; ?>\n<?php echo 2; ?>\nopen failed\n"
    );
    let out = run_with(
        &VmOptions::default(),
        &[&php_ext_stdstr::StdStr, &streams, &misc],
        r#"
        $warnings = [];
        set_error_handler(function($n, $s) use (&$warnings) { $warnings[] = $s; });
        var_dump(php_strip_whitespace('data:text/plain,%3C%3Fphp%20echo%202%3B') === '');
        var_dump(strpos(implode('|', $warnings), 'allow_url_include=0') !== false);
    "#,
    );
    assert_eq!(out, "bool(true)\nbool(true)\n");
}

#[test]
fn scanner_distinguishes_read_failure_from_empty_source() {
    let misc = php_ext_stdmisc::StdMisc;
    let streams = php_ext_streams::Streams;
    let out = run_with(
        &VmOptions::default(),
        &[&php_ext_stdstr::StdStr, &streams, &misc],
        r#"
        class ReadSource {
            public $context;
            private $mode;
            private $read = false;
            function stream_open($path, $mode, $options, &$opened_path) {
                $this->mode = $path;
                return true;
            }
            function stream_read($length) {
                if ($this->mode === 'readsource://throw') {
                    throw new Error('read failed');
                }
                if ($this->mode === 'readsource://empty') { return ''; }
                if ($this->mode === 'readsource://partial' && !$this->read) {
                    $this->read = true;
                    return '<?php echo 1; ?>';
                }
                return false;
            }
            function stream_eof() { return $this->mode === 'readsource://empty'; }
            function stream_stat() { return ['mode' => 0100644, 'size' => 0]; }
            function stream_set_option($option, $arg1, $arg2) { return true; }
        }
        stream_wrapper_register('readsource', ReadSource::class);
        set_error_handler(function($n, $s) { echo $s, "\n"; });
        var_dump(highlight_file('readsource://failed', true));
        var_dump(highlight_file('readsource://partial', true));
        var_dump(is_string(highlight_file('readsource://empty', true)));
        var_dump(php_strip_whitespace('readsource://empty'));
        try { php_strip_whitespace('readsource://throw'); }
        catch (Error $e) { echo $e->getMessage(), "\n"; }
    "#,
    );
    assert_eq!(
        out,
        "highlight_file(): Failed opening 'readsource://failed' for highlighting\nbool(false)\n\
         highlight_file(): Failed opening 'readsource://partial' for highlighting\nbool(false)\n\
         bool(true)\nstring(0) \"\"\nread failed\n"
    );
}

#[test]
fn setlocale_direct_forwarding_is_strict_and_dynamic_forwarding_is_weak() {
    let out = run(r#"declare(strict_types=1);
        try { call_user_func('setlocale', LC_ALL, 0); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        try { call_user_func_array('setlocale', [LC_ALL, '0', 0]); }
        catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        $forward = 'call_user_func';
        $forward('setlocale', LC_ALL, 0);
        $forward = 'call_user_func_array';
        $forward('setlocale', [LC_ALL, '0', 0]);
        echo "weak forwarding\n";
    "#);
    assert_eq!(
        out,
        "setlocale(): Argument #2 ($locales) must be of type array|string|null, int given\n\
         setlocale(): Argument #3 must be of type ?string, int given\nweak forwarding\n"
    );
}

#[test]
fn ini_functions() {
    let out = run(
        r#"$a = parse_ini_string("x = 1\n[s]\ny[] = on\n", true, INI_SCANNER_TYPED);
        var_dump($a['x'], $a['s']['y'][0]);
        echo isset(ini_get_all(null, false)['precision']) ? 'ok' : 'missing';"#,
    );
    assert_eq!(out, "int(1)\nbool(true)\nok");
}

#[test]
fn ini_get_all_sorts_the_shared_reflection_table() {
    struct IniFixture;

    impl Extension for IniFixture {
        fn name(&self) -> &'static str {
            "ini_sort_test"
        }

        fn register(&self, m: &mut php_vm::ext::ModuleBuilder<'_>) {
            use php_support::ini::{Access, IniDirective, ModifyHandler};

            for name in ["fixture.zeta", "fixture.Beta", "fixture.alpha"] {
                m.ini(IniDirective::new(
                    name,
                    Some("1"),
                    Access::ALL,
                    ModifyHandler::Long,
                    "ini_sort_test",
                ));
            }
            m.ini(IniDirective::new(
                "fixture.other",
                Some("1"),
                Access::ALL,
                ModifyHandler::Long,
                "other_sort_test",
            ));
        }
    }

    for query in [
        "ini_get_all('ini_sort_test', false);",
        "ini_get_all(null, true);",
        "@ini_get_all('missing_sort_fixture', false);",
        "ini_get_all('other_sort_test', true);",
    ] {
        let sink = Sink::default();
        let ext = php_ext_stdmisc::StdMisc;
        let fixture = IniFixture;
        let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &ext, &fixture];
        let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
        let code = format!(
            "<?php
            $r = new ReflectionExtension('ini_sort_test');
            foreach ($r->getINIEntries() as $k => $v) {{ echo $k, ';'; }}
            echo \"\\n\", (string)$r, \"\\nAFTER\\n\";
            {query}
            foreach ($r->getINIEntries() as $k => $v) {{ echo $k, ';'; }}
            echo \"\\n\", (string)$r;"
        );
        vm.run_script(code.as_bytes(), "test.php");
        drop(vm);
        let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        let (before, after) = out.split_once("\nAFTER\n").expect("reflection output");
        assert!(
            before.starts_with("fixture.zeta;fixture.Beta;fixture.alpha;\n"),
            "{query}: {before}"
        );
        assert!(
            after.starts_with("fixture.alpha;fixture.Beta;fixture.zeta;\n"),
            "{query}: {after}"
        );
        for (text, names) in [
            (before, ["fixture.zeta", "fixture.Beta", "fixture.alpha"]),
            (after, ["fixture.alpha", "fixture.Beta", "fixture.zeta"]),
        ] {
            let positions = names.map(|n| {
                text.find(&format!("Entry [ {n} <ALL> ]"))
                    .expect("INI entry in extension string")
            });
            assert!(positions[0] < positions[1] && positions[1] < positions[2]);
        }
    }
}

#[test]
fn credits_and_info() {
    let out = run(r#"phpcredits(CREDITS_GROUP); phpinfo(INFO_LICENSE);"#);
    assert!(out.starts_with("PHP Credits\n"), "{out}");
    assert!(out.contains(" PHP Group "), "{out}");
    assert!(out.contains("PHP is free software."), "{out}");
}

#[test]
fn getopt_without_argv_options() {
    let out = run(r#"var_dump(getopt("a:", ["long"], $rest), $rest);"#);
    assert!(out.ends_with("int(1)\n"), "{out}");
}

#[test]
fn proc_open_validates_command() {
    // Only the `standard` misc extension is loaded here, so no stream
    // functions are available; the argument checks still run.
    let out =
        run(r#"try { proc_open([], [], $p); } catch (ValueError $e) { echo $e->getMessage(); }"#);
    assert_eq!(out, "proc_open(): Argument #1 ($command) must not be empty");
}

#[test]
fn setcookie_validation() {
    let out = run(
        r#"var_dump(setcookie("a", "b", ["path" => "/", "samesite" => "Lax"]));
        try { setcookie("a", "b", ["bogus" => 1]); } catch (ValueError $e) { echo $e->getMessage(), '|'; }
        try { setrawcookie("a", "b c"); } catch (ValueError $e) { echo $e->getMessage(); }"#,
    );
    assert_eq!(
        out,
        "bool(true)\nsetcookie(): option \"bogus\" is invalid|setrawcookie(): Argument #2 ($value) cannot contain \",\", \";\", \" \", \"\\t\", \"\\r\", \"\\n\", \"\\013\", or \"\\014\""
    );
}

#[test]
fn ini_parse_quantity_warns() {
    let out = run(r#"var_dump(ini_parse_quantity("2K"), ini_parse_quantity("1Q"));"#);
    assert!(out.contains("Invalid quantity \"1Q\""), "{out}");
    assert!(out.ends_with("int(2048)\nint(1)\n"), "{out}");
}

#[test]
fn get_current_user_without_script_is_empty() {
    assert_eq!(run(r#"var_dump(get_current_user());"#), "string(0) \"\"\n");
}

#[test]
fn ini_access_constants() {
    // ext/standard/basic_functions.stub.php: `ZEND_INI_*` access levels,
    // also resolved from inside a namespace (Zend/tests/namespaces/ns_044).
    let out = run(r#"namespace test\ns1;
        echo INI_USER, INI_PERDIR, INI_SYSTEM, INI_ALL, \INI_ALL, constant("INI_ALL");"#);
    assert_eq!(out, "124777");
}

/// GH-20583: under `zend.max_allowed_stack_size`, deeply nested data makes
/// `http_build_query()` throw (`php_url_check_stack_limit`).
#[test]
fn http_build_query_honours_the_stack_limit() {
    // The default limit is derived from the main thread's stack and the
    // explicit one assumes release-build frames; the test harness threads
    // are smaller, so run both on a thread sized like a CLI's.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(http_build_query_stack_limit_cases)
        .expect("spawn")
        .join()
        .expect("join");
}

fn http_build_query_stack_limit_cases() {
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let code = r#"$a = null;
        for ($i = 0; $i < 3000; $i++) { $a = [$i => $a]; }
        try { http_build_query($a, 'p'); } catch (Throwable $e) { echo $e::class, ": ", $e->getMessage(), "|"; }
        $b = null;
        for ($i = 0; $i < 100; $i++) { $b = [$i => $b]; }
        var_dump(http_build_query($b));"#;
    let opts = VmOptions {
        ini: vec![(
            "zend.max_allowed_stack_size".to_string(),
            "256K".to_string(),
        )],
    };
    assert_eq!(
        run_with(&opts, &exts, code),
        "Error: Maximum call stack size reached.|string(0) \"\"\n"
    );
    assert_eq!(run(code), "string(0) \"\"\n");
}

/// `request_parse_body()` (`ext/standard/http.c`) validates `$options`
/// (`cache_request_parse_body_options`); a CLI request has no content
/// type.
#[test]
fn request_parse_body_validates_options() {
    let out = run(
        r#"foreach ([['foo' => 1], [42 => 1], ['max_input_vars' => []], ['upload_max_filesize' => '1GB'], ['POST_MAX_SIZE' => 5], null] as $o) { try { request_parse_body($o); } catch (Throwable $e) { echo $e::class, ': ', $e->getMessage(), "\n"; } }"#,
    );
    assert_eq!(
        out,
        "ValueError: Invalid key \"foo\" in $options argument\n\
         ValueError: Invalid integer key in $options argument\n\
         ValueError: Invalid array value in $options argument\n\
         \nWarning: Invalid quantity \"1GB\": unknown multiplier \"B\", interpreting as \"1\" for backwards compatibility in test.php on line 1\n\
         RequestParseBodyException: Request does not provide a content type\n\
         RequestParseBodyException: Request does not provide a content type\n\
         RequestParseBodyException: Request does not provide a content type\n"
    );
}

thread_local! {
    /// The options [`fake_parse_body`] received.
    static PARSE_BODY_CALLS: RefCell<Vec<ParseBodyOptions>> = const { RefCell::new(Vec::new()) };
}

/// A web SAPI's body parse ([`Vm::parse_body_hook`]): records the options,
/// throws for `max_input_vars` 0, else returns fixed arrays.
fn fake_parse_body(vm: &mut Vm, options: &ParseBodyOptions) -> VmResult<(ZArray, ZArray)> {
    PARSE_BODY_CALLS.with(|c| c.borrow_mut().push(*options));
    if options.max_input_vars == Some(0) {
        return Err(vm.throw(
            "RequestParseBodyException",
            "Input variables exceeded 0. To increase the limit change max_input_vars in php.ini.",
        ));
    }
    let mut post = ZArray::new();
    post.make_mut().insert("a", Value::from("b"));
    let mut file = ZArray::new();
    file.make_mut().insert("error", Value::Int(4));
    let mut files = ZArray::new();
    files.make_mut().insert("f", Value::Array(file));
    Ok((post, files))
}

/// `request_parse_body()` under a web SAPI: the options reach the SAPI's
/// parse as limits (quantity strings parsed, keys in any case), its
/// arrays come back as `[$post, $files]` and its exceptions propagate.
#[test]
fn request_parse_body_runs_the_sapi_parse() {
    let sink = Sink::default();
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.parse_body_hook = Some(fake_parse_body);
    vm.run_script(
        br#"<?php
var_dump(request_parse_body(['POST_MAX_SIZE' => '1K', 'upload_max_filesize' => 2, 'Max_File_Uploads' => '3']));
try { request_parse_body(['max_input_vars' => 0]); } catch (RequestParseBodyException $e) { echo $e->getMessage(), "\n"; }"#,
        "test.php",
    );
    drop(vm);
    let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
    assert_eq!(
        out,
        "array(2) {\n  [0]=>\n  array(1) {\n    [\"a\"]=>\n    string(1) \"b\"\n  }\n  [1]=>\n  array(1) {\n    [\"f\"]=>\n    array(1) {\n      [\"error\"]=>\n      int(4)\n    }\n  }\n}\n\
         Input variables exceeded 0. To increase the limit change max_input_vars in php.ini.\n"
    );
    let calls = PARSE_BODY_CALLS.with(|c| c.take());
    assert_eq!(
        calls,
        vec![
            ParseBodyOptions {
                max_file_uploads: Some(3),
                post_max_size: Some(1024),
                upload_max_filesize: Some(2),
                ..ParseBodyOptions::default()
            },
            ParseBodyOptions {
                max_input_vars: Some(0),
                ..ParseBodyOptions::default()
            },
        ]
    );
}

/// `ext/standard/assert.c`: the startup values of the `assert.*`
/// directives raise their deprecations, and the startup
/// `assert.callback` (`ASSERTG(cb)`) is called with four arguments.
#[test]
fn assert_startup_deprecations_and_ini_callback() {
    let opts = VmOptions {
        ini: [
            ("assert.warning", "0"),
            ("assert.callback", "f1"),
            ("assert.bail", "0"),
            ("assert.exception", "0"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect(),
    };
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let out = run_with(
        &opts,
        &exts,
        r#"function f1($f, $l, $u, $d) { echo "f1 $l $d\n"; } var_dump(assert(0));"#,
    );
    assert_eq!(
        out,
        "\nDeprecated: PHP Startup: assert.warning INI setting is deprecated in Unknown on line 0\n\
         \nDeprecated: PHP Startup: assert.callback INI setting is deprecated in Unknown on line 0\n\
         \nDeprecated: PHP Startup: assert.exception INI setting is deprecated in Unknown on line 0\n\
         f1 1 assert(0)\nbool(false)\n"
    );
}

/// `assert_options(ASSERT_CALLBACK)` keeps any callable (`ASSERTG(callback)`)
/// apart from the INI value; `ini_set('assert.callback')` replaces it and
/// is deprecated; the `ASSERT_*` constants are deprecated.
#[test]
fn assert_options_callback_state() {
    let out = run(
        r#"class C { static function cb($f, $l, $u, $d) { echo "C $d\n"; } }
assert_options(ASSERT_CALLBACK, ['C', 'cb']);
echo ini_get('assert.callback'), "|\n";
try { assert(false); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; }
ini_set('assert.callback', 'nope');
try { assert(false); } catch (Error $e) { echo $e::class, ':', $e->getPrevious()->getMessage(), "\n"; }"#,
    );
    let dep = "\nDeprecated: Constant ASSERT_CALLBACK is deprecated since 8.3, as assert_options() is deprecated in test.php on line 2\n\
               \nDeprecated: Function assert_options() is deprecated since 8.3 in test.php on line 2\n";
    assert_eq!(
        out,
        format!(
            "{dep}|\nC assert(false)\nassert(false)\n\
             \nDeprecated: ini_set(): assert.callback INI setting is deprecated in test.php on line 5\n\
             AssertionError:Invalid callback nope, function \"nope\" not found or invalid function name\n"
        )
    );
}

/// `get_browser()` (`ext/standard/browscap.c`): the best wildcard match,
/// its `Parent` chain, boolean normalization, the object form and the
/// missing `HTTP_USER_AGENT` warning.
#[test]
fn get_browser_matches_browscap_sections() {
    let ini = std::env::temp_dir().join(format!("phprust-browscap-{}.ini", std::process::id()));
    std::fs::write(
        &ini,
        "[*]\nBrowser=\"Default Browser\"\nFrames=true\n\n[Opera*]\nParent=Op\nPlatform=WinXP\n\n[Op]\nBrowser=Opera\nFrames=off\n",
    )
    .expect("write browscap fixture");
    let opts = VmOptions {
        ini: [("browscap".to_string(), ini.display().to_string())]
            .into_iter()
            .collect(),
    };
    let ext = php_ext_stdmisc::StdMisc;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let out = run_with(
        &opts,
        &exts,
        r#"var_dump(get_browser('Opera/7', true));
$o = get_browser('x'); echo get_class($o), ' ', $o->browser, ' ', $o->frames, "\n";
var_dump(get_browser(null));"#,
    );
    let _ = std::fs::remove_file(&ini);
    assert_eq!(
        out,
        "array(6) {\n  [\"browser_name_regex\"]=>\n  string(11) \"~^opera.*$~\"\n  \
         [\"browser_name_pattern\"]=>\n  string(6) \"Opera*\"\n  [\"parent\"]=>\n  string(2) \"Op\"\n  \
         [\"platform\"]=>\n  string(5) \"WinXP\"\n  [\"browser\"]=>\n  string(5) \"Opera\"\n  \
         [\"frames\"]=>\n  string(0) \"\"\n}\nstdClass Default Browser 1\n\
         \nWarning: get_browser(): HTTP_USER_AGENT variable is not set, cannot determine user agent name in test.php on line 3\n\
         bool(false)\n"
    );
}
