// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP snippets run on a VM with this extension.

use super::StdArray;
use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

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

/// Runs `code` (without the `<?php` tag) and returns its output.
fn run(code: &str) -> String {
    run_with(&[], code)
}

/// [`run`] with `-d` INI settings.
fn run_with(ini: &[(&str, &str)], code: &str) -> String {
    let sink = Sink::default();
    let ext = StdArray;
    let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &php_ext_random::Random, &ext];
    let opts = VmOptions {
        ini: ini
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    let src = format!("<?php\n{code}");
    vm.run_script(src.as_bytes(), "test.php");
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn sorts_through_the_reference() {
    assert_eq!(
        run("$a = [3, 1, 2]; sort($a); echo implode(',', $a);"),
        "1,2,3"
    );
    assert_eq!(
        run("$a = ['b' => 1, 'a' => 2]; ksort($a); echo implode(',', array_keys($a));"),
        "a,b"
    );
}

#[test]
fn user_sort_and_callback_errors() {
    assert_eq!(
        run("$a = [1, 3, 2]; usort($a, fn($x, $y) => $y <=> $x); echo implode(',', $a);"),
        "3,2,1"
    );
    assert_eq!(
        run("$a = [1]; try { usort($a, 'nope'); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "usort(): Argument #2 ($callback) must be a valid callback, function \"nope\" not found or invalid function name"
    );
    assert_eq!(
        run("try { array_map('nope', []); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "array_map(): Argument #1 ($callback) must be a valid callback or null, function \"nope\" not found or invalid function name"
    );
}

#[test]
fn u_diff_variants_validate_trailing_callbacks() {
    assert_eq!(
        run("try { array_udiff([1]); } catch (ArgumentCountError $e) { echo $e->getMessage(); }"),
        "array_udiff() expects at least 2 arguments, 1 given"
    );
    assert_eq!(
        run("try { array_udiff_uassoc([1], [2], 'strcmp', 'x'); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "array_udiff_uassoc(): Argument #4 must be a valid callback, function \"x\" not found or invalid function name"
    );
    assert_eq!(
        run("echo implode(',', array_udiff_uassoc([1, 5], [1], fn($a, $b) => $a <=> $b, fn($a, $b) => $a <=> $b));"),
        "5"
    );
    assert_eq!(
        run("try { array_diff([1], 2); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "array_diff(): Argument #2 must be of type array, int given"
    );
}

#[test]
fn internal_pointer_mutates_the_variable() {
    assert_eq!(
        run("$a = [1, 2, 3]; next($a); next($a); echo current($a), key($a); reset($a); echo current($a);"),
        "321"
    );
    assert_eq!(run("$a = [1, 2, 3]; echo end($a), key($a);"), "32");
}

#[test]
fn extract_and_compact_use_the_callers_scope() {
    assert_eq!(
        run("function f() { $src = ['x' => 1, 'y' => 2]; $n = extract($src); return [$n, $x + $y]; } echo implode(',', f());"),
        "2,3"
    );
    assert_eq!(
        run("function g() { $a = 1; $b = 'z'; return compact('a', ['b']); } echo implode(',', g());"),
        "1,z"
    );
    assert_eq!(
        run("$a = 1; $b = 2; echo implode(',', compact('a', 'b'));"),
        "1,2"
    );
    assert_eq!(
        run("$arr = ['v' => 1]; extract($arr, EXTR_REFS); $v = 5; echo $arr['v'];"),
        "5"
    );
    assert_eq!(
        run("try { call_user_func('extract', ['a' => 1]); } catch (Error $e) { echo $e->getMessage(); }"),
        "Cannot call extract() dynamically"
    );
}

#[test]
fn extract_negative_keys_preserves_the_callers_snapshot() {
    assert_eq!(
        run(
            "$a = [-5 => 'hello', 'world', 2 => 'positive', 'check' => 'extracted'];
            function foo(array $a) {
                extract($a, EXTR_PREFIX_ALL, 'prefix');
                var_dump(get_defined_vars());
            }
            foo($a);"
        ),
        concat!(
            "array(3) {\n",
            "  [\"a\"]=>\n",
            "  array(4) {\n",
            "    [-5]=>\n",
            "    string(5) \"hello\"\n",
            "    [-4]=>\n",
            "    string(5) \"world\"\n",
            "    [2]=>\n",
            "    string(8) \"positive\"\n",
            "    [\"check\"]=>\n",
            "    string(9) \"extracted\"\n",
            "  }\n",
            "  [\"prefix_2\"]=>\n",
            "  string(8) \"positive\"\n",
            "  [\"prefix_check\"]=>\n",
            "  string(9) \"extracted\"\n",
            "}\n"
        )
    );
}

#[test]
fn defined_vars_snapshot_preserves_visibility_and_reference_ownership() {
    assert_eq!(
        run("function snapshot($a) {
                $unset = 1; unset($unset);
                $null = null;
                $shared = [1]; $alias =& $shared;
                $single = [2]; $gone =& $single; unset($gone);
                $dynamic = 'dynamic'; $$dynamic = 3;
                extract($a);
                $vars = get_defined_vars();
                echo implode(',', array_keys($vars)), '|';
                $vars['a']['x'] = 9;
                $vars['single'][0] = 9;
                $vars['shared'][0] = 9;
                $vars['x'] = 9;
                echo $a['x'], ',', $single[0], ',', $shared[0], ',', $alias[0], ',', $x,
                    '|', (int) array_key_exists('null', $vars),
                    '|', (int) array_key_exists('unset', $vars),
                    '|', (int) array_key_exists('vars', $vars);
            }
            snapshot(['x' => 1]);"),
        "a,null,shared,alias,single,dynamic,x|1,2,9,9,1|1|0|0"
    );
}

#[test]
fn rng_shares_mt_srand_state() {
    let a = run("mt_srand(42); $x = range(1, 10); shuffle($x); echo implode(',', $x);");
    let b = run("mt_srand(42); $x = range(1, 10); shuffle($x); echo implode(',', $x);");
    assert_eq!(a, b);
    assert_eq!(run("mt_srand(1); echo strlen(str_shuffle('abcdef'));"), "6");
}

#[test]
fn constants_and_misc() {
    assert_eq!(
        run("echo SORT_STRING, COUNT_RECURSIVE, EXTR_REFS, ARRAY_FILTER_USE_KEY;"),
        "212562"
    );
    assert_eq!(run("echo count([1, [2, 3]], COUNT_RECURSIVE);"), "4");
    assert_eq!(run("echo max(1, 5, 3), min([4, 2]);"), "52");
    assert_eq!(
        run("try { array_key_exists([], []); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "Cannot access offset of type array on array"
    );
    assert_eq!(run("echo implode(',', array_filter([1, 0, 2]));"), "1,2");
}

#[test]
fn clamp_preserves_values_and_validates_bounds() {
    assert_eq!(
        run(
            "echo clamp(2, 1.3, 3.4), '|', clamp(0, 1.3, 3.4), '|', clamp('d', 'c', 'g');
            $f = clamp(...); echo '|', $f(value: 'a', min: 'c', max: 'g');
            echo '|', (int) is_int(clamp(2, 1.3, 3.4)), '|', (int) is_nan(clamp(NAN, 4, 6));"
        ),
        "2|1.3|d|c|1|1"
    );
    assert_eq!(
        run("foreach ([[4, NAN, NAN], [7, 6, NAN], [1, 3, 2]] as $args) {
            try { clamp(...$args); } catch (ValueError $e) { echo $e->getMessage(), '|'; }
        }"),
        "clamp(): Argument #2 ($min) must not be NAN|clamp(): Argument #3 ($max) must not be NAN|clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|"
    );
}

#[test]
fn clamp_source_matrix_matches_direct_and_dynamic_calls() {
    assert_eq!(
        run("declare(strict_types=1);
            $f = clamp(...);
            $cases = [
                [2, 1, 3, 2], [0, 1, 3, 1], [6, 1, 3, 3],
                [2, 1.3, 3.4, 2], [2.5, 1, 3, 2.5],
                [2.5, 1.3, 3.4, 2.5], [0, 1.3, 3.4, 1.3],
                [M_PI, -INF, INF, M_PI], [NAN, 4, 6, NAN],
                ['a', 'c', 'g', 'c'], ['d', 'c', 'g', 'd'],
                ['2025-08-01', '2025-08-15', '2025-09-15', '2025-08-15'],
                ['2025-08-20', '2025-08-15', '2025-09-15', '2025-08-20'],
                [null, -1, 1, -1], [null, 1, 3, 1], [null, -3, -1, -3],
                [-9999, null, 10, -9999], [12, null, 10, 10]
            ];
            foreach ($cases as [$value, $min, $max, $expected]) {
                $direct = clamp($value, $min, $max);
                $dynamic = $f($value, $min, $max);
                if (is_float($expected) && is_nan($expected)) {
                    if (!is_nan($direct) || !is_nan($dynamic)) { echo 'bad NAN'; }
                } elseif ($direct !== $expected || $dynamic !== $expected) {
                    echo 'bad value';
                }
            }
            $a = new InvalidArgumentException('a');
            $b = new RuntimeException('b');
            $c = new LogicException('c');
            foreach ([[$a, $b, $c], [$b, $a, $c], [$c, $a, $b]] as [$value, $min, $max]) {
                if (clamp($value, $min, $max) !== $value || $f($value, $min, $max) !== $value) {
                    echo 'bad object';
                }
            }
            echo 'done';"),
        "done"
    );
}

#[test]
fn clamp_source_errors_match_direct_and_dynamic_calls() {
    assert_eq!(
        run("$f = clamp(...);
            foreach ([[4, NAN, 6], [7, 6, NAN], [1, 3, 2], [-9999, 5, null], [12, -5, null]]
                as [$value, $min, $max]) {
                try { clamp($value, $min, $max); }
                catch (ValueError $e) { echo $e->getMessage(), '|'; }
                try { $f($value, $min, $max); }
                catch (ValueError $e) { echo $e->getMessage(), '|'; }
            }"),
        concat!(
            "clamp(): Argument #2 ($min) must not be NAN|",
            "clamp(): Argument #2 ($min) must not be NAN|",
            "clamp(): Argument #3 ($max) must not be NAN|",
            "clamp(): Argument #3 ($max) must not be NAN|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
            "clamp(): Argument #2 ($min) must be smaller than or equal to argument #3 ($max)|",
        )
    );
}

#[test]
fn globals_prefer_ref_uses_a_snapshot() {
    assert_eq!(
        run("$marker = 1;
            function detached() {
                extract($GLOBALS, EXTR_REFS);
                $marker = 4;
                echo $GLOBALS['marker'], ',', $marker, '|';
            }
            detached();
            $ref = &$marker;
            function shared() {
                extract($GLOBALS, EXTR_REFS);
                $marker = 8;
                echo $GLOBALS['marker'], ',';
                $GLOBALS['marker'] = 9;
                echo $marker;
            }
            shared();"),
        "1,4|8,9"
    );
}

#[test]
fn extract_checks_typed_references_before_later_entries() {
    assert_eq!(
        run("declare(strict_types=1);
            class T { public int $i = 0; public string $s = ''; }
            $o = new T; $i = &$o->i; $s = &$o->s;
            try { extract(['i' => 'foo', 's' => 42]); }
            catch (TypeError $e) { echo $e->getMessage(), '|'; }
            echo $o->i, ',', strlen($o->s), '|';
            echo extract(['i' => '7', 's' => 42]), '|', $o->i, ',', $o->s;"),
        "Cannot assign string to reference held by property T::$i of type int|0,0|2|7,42"
    );
}

#[test]
fn globals_rejects_mutating_array_arguments() {
    assert_eq!(
        run("$marker = 42;
            try { array_splice($GLOBALS, 0, count($GLOBALS)); }
            catch (Error $e) { echo $e->getMessage(), '|'; }
            try { sort($GLOBALS); }
            catch (Error $e) { echo $e->getMessage(), '|'; }
            echo $marker;
            $GLOBALS['items'] = [3, 1, 2];
            sort($GLOBALS['items']); echo '|', implode(',', $items);"),
        "array_splice(): Argument #1 ($array) could not be passed by reference|sort(): Argument #1 ($array) could not be passed by reference|42|1,2,3"
    );
}

#[test]
fn array_map_self_reference_cleanup() {
    assert_eq!(
        run("$a = ['k1' => 'v1', 'k2' => 'v2'];
            $a[] = &$a['k1']; $a[] = &$a;
            $mapped = array_map(fn($v) => [$v], $a);
            $same = array_map(null, $a);
            $zip = array_map(null, $a, $a);
            $a[0] = null; $a[1] = null;
            echo count($mapped), ',', count($same), ',', count($zip), '|', (int) is_null($a), '|done';"),
        "4,4,4|1|done"
    );
}

#[test]
fn merge_recursive_self_reference_cleanup() {
    assert_eq!(
        run("$a = ['key1' => ['key2' => []]];
            $a['key1']['key2']['key3'] = &$a;
            $b = ['key1' => ['key2' => []]];
            $b['key1']['key2']['key3'] = &$b;
            try { array_merge_recursive($a, $b); }
            catch (Error $e) { echo $e->getMessage(), '|'; }
            $a['key1']['key2']['key3'] = null;
            $b['key1']['key2']['key3'] = null;
            echo (int) is_null($a), (int) is_null($b), '|done';"),
        "Recursion detected|11|done"
    );
}

#[test]
fn walk_typed_reference_writeback() {
    assert_eq!(
        run("class T { public int $prop = 42; }
            $o = new T;
            try { @array_walk($o, function (&$v) { $v = []; }); }
            catch (TypeError $e) { echo $e->getMessage(), '|'; }
            echo $o->prop;
            @array_walk($o, function (&$v) { $v = '7'; });
            echo '|', $o->prop, '|', (int) is_int($o->prop);"),
        "Cannot assign array to reference held by property T::$prop of type int|42|7|1"
    );
}

#[test]
fn locale_sort_constant_is_deprecated() {
    let out = run("$a = ['b', 'a']; sort($a, SORT_LOCALE_STRING); echo implode(',', $a);");
    assert!(out.contains(
        "Constant SORT_LOCALE_STRING is deprecated since 8.6, use one of the Collator::*sort*() methods instead"
    ));
    assert!(out.ends_with("a,b"));
}

#[test]
fn object_internal_pointer_persists() {
    let out = run("class T { public int $a; public $b = 1; public $c = 2; }
        $o = new T;
        echo @reset($o), @next($o), @key($o), var_export(@next($o), true), var_export(@key($o), true);");
    // master semantics (GH-16905): uninitialized properties are skipped.
    assert_eq!(out, "12cfalseNULL");
}

#[test]
fn array_splice_detects_destructor_modification() {
    let out = run(
        "class C { function __destruct() { global $arr; $arr[] = 0; } }
        $arr = ['1', new C, '2'];
        try { array_splice($arr, 1, 2); echo 'no'; } catch (Error $e) { echo $e->getMessage(); }",
    );
    assert_eq!(out, "Array was modified during array_splice operation");
    assert_eq!(
        run("$a = [1, 2, 3]; $r = array_splice($a, 1, 1); echo implode(',', $a), '|', implode(',', $r);"),
        "1,3|2"
    );
}

#[test]
fn memory_and_stack_limits() {
    // `memory_limit` is process-global in php-memlimit, so it is exercised
    // by the oom_*.phpt tests rather than by these parallel unit tests.
    assert!(run("array_fill(0, 2147483647, 1);")
        .contains("Possible integer overflow in memory allocation"));
    // Counting (and dropping) a 10000-level array recurses natively: give
    // the script the CLI's large stack.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let deep = "$a = []; for ($i = 0; $i < 10000; $i++) { $a = [$a, []]; }";
            assert_eq!(
                run(&format!("{deep} echo count($a, COUNT_RECURSIVE);")),
                "20000"
            );
            // 128K leaves 80 KiB after the default 48 KiB reservation:
            // enough to compile the script (php-src fails to compile
            // `echo 1;` with 16K or 64K) and far less than 10000 levels of
            // `php_count_recursive`.
            let limited = run_with(
                &[("zend.max_allowed_stack_size", "128K")],
                &format!("{deep} try {{ count($a, COUNT_RECURSIVE); }} catch (Error $e) {{ echo $e->getMessage(); }}"),
            );
            assert_eq!(
                limited,
                "Maximum call stack size of 81920 bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
            );
        })
        .expect("spawn test thread")
        .join()
        .expect("test thread");
}

/// `Z_PARAM_FUNC`: method callables are resolved in the calling scope,
/// with the `self`/`parent`/`static` deprecations raised once per call of
/// the array function.
#[test]
fn keyword_callables_resolve_in_calling_scope() {
    let out = run(r#"
class A { public function foo($x) { echo static::class, $x, "\n"; return $x; } }
class B extends A {
    function t() {
        array_map("parent::foo", [1, 2]);
        array_map(["self", "foo"], [3]);
        array_map([$this, "A::foo"], [4]);
    }
}
(new B)->t();
"#);
    assert_eq!(
        out,
        "\nDeprecated: Use of \"parent\" in callables is deprecated in test.php on line 6\nB1\nB2\n\
         \nDeprecated: Use of \"self\" in callables is deprecated in test.php on line 7\nB3\n\
         \nDeprecated: Callables of the form [\"B\", \"A::foo\"] are deprecated in test.php on line 8\nB4\n"
    );
}
