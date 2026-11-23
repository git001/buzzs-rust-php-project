// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The user-call path: arguments bound into CVs when the frame is pushed
//! (`i_init_func_execute_data`), `RECV` skipping and in-place verification,
//! the `ZEND_INIT_METHOD_CALL` run-time cache and closure variable binding.
//! Expected outputs are php-src's (`/usr/bin/php -n`).
#![expect(clippy::disallowed_methods, reason = "test code")]

use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::fmt::Write as _;
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
    // A new request's object store (`zend_objects_store_init`), as each
    // script gets in its own `php` process: the store is per thread, and a
    // test running several scripts would otherwise reuse the handles the
    // previous one freed, in the order it freed them.
    php_vm::object::reset_store();
    let sink = Sink::default();
    let mut vm = Vm::new(&VmOptions::default(), Box::new(sink.clone()));
    let status = vm.run_script(source.as_bytes(), "t.php");
    drop(vm);
    let output = sink.0.borrow().clone();
    let output = String::from_utf8(output).expect("UTF-8 output");
    assert_eq!(status, 0, "{output}");
    output
}

/// What `arguments_bound_at_frame_push_match_recv_semantics` prints in
/// php-src (`/usr/bin/php -n`).
const BOUND_ARGS_EXPECTED: &str = concat!(
    r#"1:a:1:{i:0;i:1;}:a:3:{i:0;i:1;i:1;i:2;i:2;a:1:{i:0;i:3;}}
2:a:2:{i:0;i:1;i:1;i:5;}:a:3:{i:0;i:1;i:1;i:5;i:2;a:1:{i:0;i:3;}}
4:a:4:{i:0;i:1;i:1;i:5;i:2;i:6;i:3;i:7;}:a:3:{i:0;i:1;i:1;i:5;i:2;i:6;}
3:a:3:{i:0;i:0;i:1;i:2;i:2;i:9;}:a:3:{i:0;i:0;i:1;i:2;i:2;i:9;}
Too few arguments to function plain(), 0 passed in t.php on line 4 and at least 1 expected
gap(): Argument #2 ($b) not passed
a:2:{i:0;s:7:"changed";i:1;i:2;}
int(5)
string(1) "d"
NULL
int(7)
string(1) "8"
NULL
int(1)
string(1) "x"
array(1) {
  [0]=>
  int(1)
}
typed(): Argument #1 ($a) must be of type int, string given, called in t.php on line 11
a:1:{i:0;s:1:"x";}
typed(): Argument #2 ($b) must be of type string, array given, called in t.php on line 12
a:2:{i:0;i:1;i:1;a:0:{}}
array(3) {
  [0]=>
  int(1)
  [1]=>
  int(2)
  [2]=>
  int(3)
}
int(1)
array(0) {
}
int(1)
int(1)
array(2) {
  [0]=>
  int(2)
  [1]=>
  int(3)
}
int(3)
int(1)
array(1) {
  ["x"]=>
  int(2)
}
int(1)
abc
[toString]string(3) "str"
int(1)
"#,
    // `echo $v, " "` ends both generator lines in a space: spelled out
    // so that no editor strips it.
    "10 2 1 \n10 20 2 \n",
    r#"a:1:{i:0;i:0;}abc01
a:1:{i:0;i:1;}abc0111
a:1:{i:0;i:2;}abc011121
69
3
2
O:1:"P":2:{s:1:"x";i:1;s:1:"y";i:3;}O:1:"P":2:{s:1:"x";s:1:"a";s:1:"y";i:4;}
20
1275
"#
);

#[test]
fn arguments_bound_at_frame_push_match_recv_semantics() {
    assert_eq!(
        run(r#"<?php
function plain($a, $b = 2, $c = [3]) { echo func_num_args(), ":", serialize(func_get_args()), ":", serialize([$a, $b, $c]), "\n"; }
plain(1); plain(1, 5); plain(1, 5, 6, 7); plain(c: 9, a: 0);
try { plain(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
function gap($a, $b, $c = 3) { echo $a, $b, $c, "\n"; }
try { gap(1, c: 4); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
function trace($a, $b) { $a = "changed"; return debug_backtrace()[0]["args"]; }
echo serialize(trace(1, 2)), "\n";
function typed(int $a, string $b = "d", ?array $c = null) { var_dump($a, $b, $c); }
typed("5"); typed(7, 8); typed(1.0, "x", [1]);
try { typed("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n", serialize($e->getTrace()[0]["args"]), "\n"; }
try { typed(1, []); } catch (TypeError $e) { echo $e->getMessage(), "\n", serialize($e->getTrace()[0]["args"]), "\n"; }
function mixed_($a, int $b, $c) { var_dump(func_get_args()); }
mixed_(1, "2", 3);
function variadic($a, ...$rest) { var_dump($a, $rest, func_num_args()); }
variadic(1); variadic(1, 2, 3); variadic(1, x: 2);
function byref(&$a, $b) { $a .= $b; }
$s = "a"; byref($s, "b"); byref($s, "c"); echo $s, "\n";
class S { function __toString(): string { echo "[toString]"; return "str"; } }
function strparam(string $s, $t) { var_dump($s, $t); }
strparam(new S, 1);
function gen($a, $b = 2) { yield $a; yield $b; yield func_num_args(); }
foreach (gen(10) as $v) echo $v, " "; echo "\n";
foreach (gen(10, 20) as $v) echo $v, " "; echo "\n";
$f = function ($x, $y = 1) use (&$s) { $s .= $x . $y; return func_get_args(); };
for ($i = 0; $i < 3; $i++) { echo serialize($f($i)), $s, "\n"; }
$g = fn($x) => $x * $i;
echo $g(2), $g(3), "\n";
function counter() { static $n = 0; return ++$n; }
counter(); counter(); echo counter(), "\n";
$h = function () { static $k = 0; return ++$k; };
$h(); echo $h(), "\n";
class P { public function __construct(public $x, public int $y = 3) {} }
echo serialize(new P(1)), serialize(new P("a", "4")), "\n";
function many($a, $b) { return $a + $b; }
$t = 0; for ($i = 0; $i < 5; $i++) { $t += many($i, $i); } echo $t, "\n";
function rec($n) { return $n ? $n + rec($n - 1) : 0; }
echo rec(50), "\n";
"#),
        BOUND_ARGS_EXPECTED
    );
}

/// What `return_releases_in_zend_leave_helper_order` prints in php-src
/// (`/usr/bin/php -n`).
const LEAVE_ORDER_EXPECTED: &str = r#"in f
d:arg-a
d:cv-b
d:cv-c
d:extra1
d:extra2
after f
in m
d:m-arg
d:m-cv-b
d:m-extra
M gone
after m
d:g-cv
d:g-extra
after g
d:g-arg
in closure
d:cl-arg
d:cl-cv
d:cl-extra
after closure call
d:use-x
in v
d:v-a
d:v-cv
d:v-r1
d:v-r2
after v
in n
d:n-a
d:n-c
d:n-cv
after n
leave 0
d:rec-0
leave 1
d:rec-1
leave 2
d:rec-2
leave 3
d:rec-3
leave 4
d:rec-4
leave 5
d:rec-5
leave 6
d:rec-6
leave 7
d:rec-7
leave 8
d:rec-8
leave 9
d:rec-9
after rec
d:t-cv
d:mid-cv
caught
d:t-arg
d:t-extra
d:mid-arg
d:mid-extra
after catch
got
gen resumed
d:gen-cv
d:gen-extra
d:gen-arg
after gen
in __call undefined
d:call-a
d:call-b
d:call-cv
end
"#;

#[test]
fn return_releases_in_zend_leave_helper_order() {
    // `zend_leave_helper`: a returning frame releases its compiled
    // variables in order (`i_free_compiled_variables`), then the extra
    // named and the extra positional arguments, then `$this` or the
    // closure; a frame left by an exception, a frame dropped beyond the
    // pool (`rec`) and a generator's frame release in the same order.
    assert_eq!(
        run(r#"<?php
class D { function __construct(public $n) {} function __destruct() { echo "d:{$this->n}\n"; } }
function f($a) { $b = new D('cv-b'); $c = new D('cv-c'); echo "in f\n"; }
f(new D('arg-a'), new D('extra1'), new D('extra2'));
echo "after f\n";
class M { function m($a) { $b = new D('m-cv-b'); echo "in m\n"; } function __destruct() { echo "M gone\n"; } }
(new M)->m(new D('m-arg'), new D('m-extra'));
echo "after m\n";
function g($a) { $b = new D('g-cv'); return $a; }
$r = g(new D('g-arg'), new D('g-extra'));
echo "after g\n";
$r = null;
$x = new D('use-x');
$cl = function ($p) use ($x) { $y = new D('cl-cv'); echo "in closure\n"; };
$x = null;
$cl(new D('cl-arg'), new D('cl-extra'));
echo "after closure call\n";
$cl = null;
function v($a, ...$rest) { $b = new D('v-cv'); echo "in v\n"; }
v(new D('v-a'), new D('v-r1'), new D('v-r2'));
echo "after v\n";
function n($a, $b = null, $c = null) { $z = new D('n-cv'); echo "in n\n"; }
n(c: new D('n-c'), a: new D('n-a'));
echo "after n\n";
function rec($n) { $o = new D("rec-$n"); if ($n > 0) rec($n - 1); echo "leave $n\n"; }
rec(9);
echo "after rec\n";
function thrower($a) { $b = new D('t-cv'); throw new Exception('x'); }
function mid($a) { $m = new D('mid-cv'); thrower(new D('t-arg'), new D('t-extra')); }
try { mid(new D('mid-arg'), new D('mid-extra')); } catch (Exception $e) { echo "caught\n"; }
$e = null;
echo "after catch\n";
function gen($a) { $b = new D('gen-cv'); yield $a; echo "gen resumed\n"; }
$g = gen(new D('gen-arg'), new D('gen-extra'));
foreach ($g as $v) { echo "got\n"; }
$v = null;
$g = null;
echo "after gen\n";
class C { function __call($name, $args) { $l = new D('call-cv'); echo "in __call $name\n"; } }
(new C)->undefined(new D('call-a'), new D('call-b'));
echo "end\n";
"#),
        LEAVE_ORDER_EXPECTED
    );
}

/// What `unpacking_sends_each_element_as_the_iterator_yields_it` prints in
/// php-src (`/usr/bin/php -n`).
const UNPACK_ORDER_EXPECTED: &str = r#"a

Warning: Cannot pass by-reference argument 1 of byref() by unpacking a Traversable, passing by-value instead in t.php on line 5
b

Warning: Cannot pass by-reference argument 2 of byref() by unpacking a Traversable, passing by-value instead in t.php on line 5
c
byref
1
2
Cannot use positional argument after named argument during unpacking
d:before
d:y1
d:y2
caught
a
b
c
2
x
Keys must be of type int|string during array unpacking
d:before
d:y1
d:y2
caught
d:one
Cannot add element to the array as the next element is already occupied
array(3) {
  ["a"]=>
  int(1)
  [0]=>
  int(2)
  [1]=>
  int(3)
}
array(2) {
  [0]=>
  int(1)
  [1]=>
  int(2)
}
end
"#;

#[test]
fn unpacking_sends_each_element_as_the_iterator_yields_it() {
    // `ZEND_SEND_UNPACK` / `ZEND_ADD_ARRAY_UNPACK` of a Traversable: each
    // element is sent (or added) before the iterator moves on, so its
    // by-reference warning or key error comes first and an error stops
    // the iterator; when an exception ends the walk, the iterator, then
    // the operand, then the arguments sent (or the array built) so far are
    // released.
    assert_eq!(
        run(r#"<?php
class D { function __construct(public $n) {} function __destruct() { echo "d:{$this->n}\n"; } }
function g() { echo "a\n"; yield 1; echo "b\n"; yield 2; echo "c\n"; }
function byref(&$x, &$y) { echo "byref\n"; }
byref(...g());
function h() { echo "1\n"; yield 'k' => 1; echo "2\n"; yield 5; echo "3\n"; }
function q($k = 0, $z = 0) { echo "q\n"; }
try { q(...h()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
function y() { yield new D('y1'); yield new D('y2'); throw new Exception('x'); }
function r($a, $b, $c = null) { echo "r\n"; }
try { r(new D('before'), ...y()); } catch (Exception $e) { echo "caught\n"; }
$a = [...g()];
echo count($a), "\n";
function kf() { yield 1; echo "x\n"; yield 1.5 => 2; echo "y\n"; yield 3; }
try { $b = [...kf()]; } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { $c = [new D('before'), ...y()]; } catch (Exception $e) { echo "caught\n"; }
function one() { yield new D('one'); echo "after one\n"; }
try { $w = [PHP_INT_MAX => 0, ...one()]; } catch (Error $e) { echo $e->getMessage(), "\n"; }
function ki() { yield 'a' => 1; yield '7' => 2; yield 3; }
var_dump([...ki()]);
var_dump(iterator_to_array((function () { yield 1; yield 2; })()));
echo "end\n";
"#),
        UNPACK_ORDER_EXPECTED
    );
    // The handles: the generator #1, the iterator's #2, the yielded #3;
    // freed in the order #2, #1, #3, so the next object takes #3.
    let handles = "object(stdClass)#3 (0) {\n}\nobject(stdClass)#3 (0) {\n}\n";
    assert_eq!(
        run(r#"<?php
function o() { yield new stdClass; throw new Exception("x"); }
function f($a, $b) { var_dump(new stdClass); }
try { f(...o()); } catch (Exception $e) { var_dump(new stdClass); }
var_dump(new stdClass);
"#),
        handles
    );
    assert_eq!(
        run(r#"<?php
function o() { yield new stdClass; throw new Exception("x"); }
try { $z = [...o()]; } catch (Exception $e) { var_dump(new stdClass); }
var_dump(new stdClass);
"#),
        handles
    );
}

#[test]
fn method_cache_respects_class_scope_and_visibility() {
    assert_eq!(
        run(r#"<?php
class A {
    private function f() { return "A::f"; }
    protected function p() { return "A::p"; }
    public function callF(A $o) { return $o->f(); }
    public function callP(A $o) { return $o->p(); }
    public function __call($n, $a) { return "A::__call($n)"; }
}
class B extends A {
    public function f() { return "B::f"; }
    public function F2() { return "B::F2"; }
}
class C { public function f() { return "C::f"; } private function priv() {} 
          public static function st() { return isset($this) ? "this" : "static"; } }
$a = new A; $b = new B; $c = new C;
for ($i = 0; $i < 3; $i++) {
    echo $a->callF($b), " ", $b->f(), " ", $b->callF($b), " ", $a->callF($a), " ", $c->f(), "\n";
    echo $a->f(), " ", $a->p(), " ", $b->callP($b), " ", $b->f2(), " ", $b->F2(), " ", $b->F2(), " ", $c->st(), "\n";
    try { $c->priv(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
}
abstract class Base { abstract function m(); function run() { return $this->m(); } }
class X1 extends Base { function m() { return "X1"; } }
class X2 extends Base { function m() { return "X2"; } }
foreach ([new X1, new X2, new X1, new X2] as $o) echo $o->run(); echo "\n";
$cl = function () { return $this->f(); };
echo Closure::bind($cl, $b, A::class)(), " ", Closure::bind($cl, $b, B::class)(), " ", Closure::bind($cl, $b, A::class)(), "\n";
$m = "F"; echo $b->$m(), $b->{"f"}(), "\n";
class LongNames { function thisIsAVeryLongMethodNameThatExceedsTheCacheLimitForSure() { return "long"; } }
$l = new LongNames; echo $l->thisIsAVeryLongMethodNameThatExceedsTheCacheLimitForSure(), $l->THISISAVERYLONGMETHODNAMETHATEXCEEDSTHECACHELIMITFORSURE(), "\n";
"#),
        r"A::f B::f A::f A::f C::f
A::__call(f) A::__call(p) A::p B::F2 B::F2 B::F2 static
Call to private method C::priv() from global scope
A::f B::f A::f A::f C::f
A::__call(f) A::__call(p) A::p B::F2 B::F2 B::F2 static
Call to private method C::priv() from global scope
A::f B::f A::f A::f C::f
A::__call(f) A::__call(p) A::p B::F2 B::F2 B::F2 static
Call to private method C::priv() from global scope
X1X2X1X2
A::f B::f A::f
B::fB::f
longlong
"
    );
}

#[test]
fn named_argument_gaps_stop_recv_skipping() {
    // A gap left by named arguments stops the skipping: the default of a
    // later parameter is still evaluated, and a missing one is reported.
    assert_eq!(
        run(r#"<?php
function d() { echo "[d]"; return 7; }
function f($a, $b = D, $c = 3) { return "$a/$b/$c"; }
const D = 5;
echo f(1), " ", f(1, 2), " ", f(1, c: 9), " ", f(c: 0, a: 4), "\n";
function g($a, $b) { return $a . $b; }
try { echo g(b: 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
"#),
        "1/5/3 1/2/3 1/5/9 4/5/0\ng(): Argument #1 ($a) not passed\n"
    );
}

#[test]
fn method_cache_evictions_keep_lookups_exact() {
    // More (class, scope, name) triples than the cache holds: every lookup
    // must still resolve as `zend_std_get_method` would, including private
    // methods reached from their own scope and refused from another one.
    let classes = (0..100).fold(String::new(), |mut out, i| {
        writeln!(
            out,
            "class K{i} {{ function m() {{ return {i}; }} private function p() {{ return -{i}; }} function q($o) {{ return $o->p(); }} }}"
        )
        .expect("String write");
        out
    });
    let src = format!(
        r#"<?php
{classes}$objs = [];
for ($i = 0; $i < 100; $i++) {{ $c = "K$i"; $objs[] = new $c; }}
$sum = 0;
for ($r = 0; $r < 3; $r++) {{ foreach ($objs as $o) {{ $sum += $o->m() + $o->M() + $o->q($o); }} }}
echo $sum, "\n";
try {{ $objs[0]->p(); }} catch (Error $e) {{ echo $e->getMessage(), "\n"; }}
try {{ $objs[1]->q($objs[2]); }} catch (Error $e) {{ echo $e->getMessage(), "\n"; }}
"#
    );
    assert_eq!(
        run(&src),
        "14850\nCall to private method K0::p() from global scope\nCall to private method K2::p() from scope K1\n"
    );
}

/// The in-place call and return paths (`push_simple_ucall`,
/// `return_simple`) and the cases that must leave them for the general
/// path: defaults skipped by `RECV_INIT`, extra and counted arguments,
/// recursion, by-reference returns, return-type coercion, undefined return
/// variables, methods, static methods, closures with bound variables and
/// `$this`, first-class callables, type and count errors, generators,
/// destructor order of released frames, and calls from internal code.
/// Expected output is php-src's (`/usr/bin/php -n`).
#[test]
fn in_place_calls_and_returns_match_the_general_path() {
    // The bare test VM has no ext/standard or ext/json: `lst()` builds
    // json_encode()'s output for int lists, and loops replace
    // array_sum()/array_map()/count() (kept on existing lines so the
    // expected line numbers still hold).
    let source = r#"<?php
function id($x) { return $x; }
function add(int $a, int $b = 10): int { return $a + $b; }
function lit() { return 5; }
function lst($a) { $s = '['; $sep = ''; foreach ($a as $v) { $s .= $sep . $v; $sep = ','; } return $s . ']'; } function cnt($a, $b = 2) { return func_num_args() . ':' . lst(func_get_args()); }
function fib($n) { return $n < 2 ? $n : fib($n - 1) + fib($n - 2); }
function &refret(array &$a) { return $a[0]; }
function f2(?string $s): string { return $s ?? 'null'; }
function coerce(int $i): string { return $i; }
function undef() { return $nope; }
class Obj {
    public $v = 1;
    function get($x) { return $x + $this->v; }
    static function st($x) { return $x * 2; }
    function __destruct() { echo "destruct {$this->v}\n"; }
}
echo id(1), id("s"), id(1.5), "\n";
echo add(1), ' ', add(1, 2), "\n";
echo lit(), "\n";
echo cnt(1), ' ', cnt(1, 2, 3), "\n";
echo fib(15), "\n";
$arr = [1, 2]; $r = &refret($arr); $r = 9; echo $arr[0], "\n";
echo f2(null), f2("x"), "\n";
var_dump(coerce(3));
var_dump(undef());
$o = new Obj; echo $o->get(1), ' ', Obj::st(4), "\n";
$k = 3;
$c = function ($x) use ($k) { return $x + $k; };
echo $c(1), "\n";
$m = new Obj; $m->v = 7;
$b = Closure::bind(function ($y) { return $this->v + $y; }, $m, Obj::class);
echo $b(1), "\n";
$fc = id(...); echo $fc(42), "\n";
try { add("x"); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { add(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
function gen() { yield 1; return 2; }
$g = gen(); foreach ($g as $v) echo $v; echo $g->getReturn(), "\n";
function tmp_obj() { return new Obj; }
tmp_obj();
echo "after\n";
function keep() { $x = new Obj; $x->v = 3; return 1; }
echo keep(), "\n";
function deep($n) { return $n ? deep($n - 1) : 'bottom'; }
echo deep(100), "\n";
$sum = 0; $dbl = fn($x) => id($x) * 2; foreach ([1, 2, 3] as $x) { $sum += $dbl($x); } echo $sum, "\n";
function nested($a) { return id(id($a)); }
echo nested(8), "\n";
function strict_arr(array $a): array { return $a; }
$n = 0; foreach (strict_arr([1,2]) as $_) { $n++; } echo $n, "\n";
echo "end\n";
"#;
    let expected = r#"1s1.5
11 3
5
1:[1] 3:[1,2,3]
610
9
nullx
string(1) "3"

Warning: Undefined variable $nope in t.php on line 10
NULL
2 8
4
8
42
TypeError: add(): Argument #1 ($a) must be of type int, string given, called in t.php on line 34
Too few arguments to function add(), 0 passed in t.php on line 35 and at least 1 expected
12
destruct 1
after
destruct 3
1
bottom
12
8
2
end
destruct 7
destruct 1
"#;
    assert_eq!(run(source), expected);
}

/// Sends and returns that must leave the in-place paths (`send_fast`,
/// `return_simple`) for the general ones: by-reference parameters, more
/// arguments than the inline buffer holds, named arguments, an undefined
/// variable argument, frames with an attached symbol table, references,
/// `void`/nullable returns, implicit returns with and without a return
/// type, return-type coercion; and namespaced call
/// sites with a global fallback (`init_fcall_fast` declines them).
/// Expected output is php-src's (`/usr/bin/php -n`).
#[test]
fn sends_and_returns_outside_the_in_place_paths() {
    let source = r#"<?php
namespace NS;
function local($x) { return strlen($x); }
echo local('abcd'), \strlen('ab'), strtoupper('q'), "\n";
function byref(&$a, $b) { $a .= $b; return $a; }
function many($a, $b, $c, $d, $e, $f, $g, $h) { return "$a$b$c$d$e$f$g$h"; }
function named($a, $b = 'B', $c = 'C') { return $a . $b . $c; }
function show($x) { var_dump($x); return $x; }
$s = 'x';
echo byref($s, 'y'), ' ', $s, "\n";
echo many(1, 2, 3, 4, 5, 6, 7, 8), "\n";
echo named('a', c: 'z'), "\n";
show($undefined_var);
$GLOBALS['gv'] = 5;
extract(['ev' => 6]);
echo show($gv) + show($ev), "\n";
$r = 10; $ref = &$r;
echo show($ref), "\n";
function ret_ref_cv() { $a = 1; $b = &$a; $b = 2; return $a; }
echo ret_ref_cv(), "\n";
function ret_void(): void { return; }
var_dump(ret_void());
function ret_nullable(): ?int { return null; }
var_dump(ret_nullable());
function ret_bad(): int { return "12"; }
var_dump(ret_bad());
function ret_float(): float { return 3; }
var_dump(ret_float());
function nothing($a) {}
var_dump(nothing(1));
function noret(): int { if (false) { return 1; } }
try { noret(); } catch (\TypeError $e) { echo $e->getMessage(), "\n"; }
function nullable_noret(): ?int { if (false) { return 1; } }
try { var_dump(nullable_noret()); } catch (\TypeError $e) { echo $e->getMessage(), "\n"; }
"#;
    let expected = r"42Q
xy xy
12345678
aBz

Warning: Undefined variable $undefined_var in t.php on line 13
NULL
int(5)
int(6)
11
int(10)
10
2
NULL
NULL
int(12)
float(3)
NULL
NS\noret(): Return value must be of type int, none returned
NS\nullable_noret(): Return value must be of type ?int, none returned
";
    assert_eq!(run(source), expected);
}

/// `return_simple` moving a returned compiled variable out of the released
/// frame (strings, numbers) or copying it (arrays, references), and storing
/// the result straight into the caller's temporary: used, discarded,
/// reused in a loop and passed on as an argument. Expected output is
/// php-src's (`/usr/bin/php -n`).
#[test]
fn returned_variables_move_into_the_callers_result() {
    let source = r#"<?php
function s($x) { $y = $x . "!"; return $y; }
function keep($x) { $copy = $x; return $x; }
function arr($n) { $a = [$n, $n + 1]; return $a; }
function refd() { $v = "r"; $f = function () use (&$v) { $v .= "x"; }; $f(); return $v; }
function typed(string $s): string { return $s; }
function fl(): float { $f = 1.5; return $f; }
$a = s("a"); $b = s("b");
echo $a, $b, s("c"), "\n";
s("discarded");
echo keep("k"), keep(3), "\n";
$x = arr(1); $y = arr(5); echo $x[0], $x[1], $y[0], $y[1], "\n";
echo refd(), "\n";
echo typed("t") . typed("u"), "\n";
var_dump(fl());
for ($i = 0; $i < 3; $i++) { $t = s((string) $i); echo $t; }
echo "\n";
echo typed(s("abc")), "\n";
var_dump(s(1) === "1!");
"#;
    let expected = "a!b!c!\nk3\n1256\nrx\ntu\nfloat(1.5)\n0!1!2!\nabc!\nbool(true)\n";
    assert_eq!(run(source), expected);
}

/// `return_simple` releasing the returning frame in place (the frame's
/// variables, then the caller's result slot): destructor timing of a
/// local object, of a returned object kept and replaced, deep recursion
/// that grows and trims the frame pool, and nested calls as arguments.
/// Expected output is php-src's (`/usr/bin/php -n`).
#[test]
fn frames_released_in_place_on_return_keep_destructor_order() {
    let source = r#"<?php
class D { function __construct(public $n) {} function __destruct() { echo "~", $this->n; } }
function local($n) { $d = new D($n); echo "in$n"; return $n; }
function give($n) { $d = new D($n); return $d; }
function depth($n) { return $n === 0 ? 0 : 1 + depth($n - 1); }
function m($a) { return $a; }
echo local(1), "|\n";
$r = local(2); echo "|", $r, "\n";
$o = give(3); echo "|kept\n"; $o = give(4); echo "|replaced\n"; unset($o);
echo "\n", depth(300), " ", depth(5), "\n";
$t = new D(5); echo m($t)->n, "\n"; $t = null; echo "|\n";
function caller() { return m(m(7)) + m(8); }
echo caller(), "\n";
"#;
    let expected = "in1~11|\nin2~2|2\n|kept\n~3|replaced\n~4\n300 5\n5\n~5|\n15\n";
    assert_eq!(run(source), expected);
}

/// The plain-call test reads the function's cached declaration shape
/// (`UserFunction::call_shape`, worked out on the first call): each kind of
/// declaration it rules out (by-reference and variadic parameters, static
/// variables, generators, foreach and finally slots, `#[\Deprecated]`) and
/// typed parameters (checked per argument) behave on the second and third
/// call as on the first. Expected output is php-src's (`/usr/bin/php8.5 -n`;
/// `#[\Deprecated]` is PHP 8.4+).
#[test]
fn cached_call_shape_keeps_every_kind_of_declaration_exact() {
    let source = r#"<?php
function u($a, $b = 2) { return $a . "-" . $b; }
function m(mixed $a) { return gettype($a); }
function t(int $a, $b = null) { return var_export($a, true) . "/" . gettype($b); }
function r(&$a) { $a++; return $a; }
function v($a, ...$rest) { return $a + count($rest); }
function s($a) { static $n = 0; $n += $a; return $n; }
function g($a) { yield $a; }
function fe($a) { $s = 0; foreach ($a as $x) { $s += $x; } return $s; }
function fin($a) { try { return $a; } finally { echo "f$a "; } }
#[\Deprecated]
function d($a) { return $a * 2; }
for ($i = 0; $i < 3; $i++) {
    echo u($i), " ", u($i, "x"), " ", m($i), " ", m("s"), " ", t($i), " ", t("5", $i), " ";
    $x = $i; echo r($x), $x, " ", v($i), v($i, 1, 2), " ", s($i), " ";
    foreach (g($i) as $y) echo $y;
    echo " ", fe([$i, 1]), " ", fin($i), " ", @d($i), "\n";
}
try { t("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
try { u(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
echo d(4), "\n";
"#;
    let expected = concat!(
        "0-2 0-x integer string 0/NULL 5/integer 11 02 0 0 1 f0 0 0\n",
        "1-2 1-x integer string 1/NULL 5/integer 22 13 1 1 2 f1 1 2\n",
        "2-2 2-x integer string 2/NULL 5/integer 33 24 3 2 3 f2 2 4\n",
        "t(): Argument #1 ($a) must be of type int, string given, called in t.php on line 19\n",
        "Too few arguments to function u(), 0 passed in t.php on line 20 and at least 1 expected\n",
        "\nDeprecated: Function d() is deprecated in t.php on line 21\n",
        "8\n",
    );
    assert_eq!(run(source), expected);
}

/// A `static` closure declared in a method or function caches its first
/// object in its function's static-variable store
/// (`ZEND_DECLARE_LAMBDA_FUNCTION`'s cache slot), which the plain-call path
/// no longer turns down: calls of such closures (direct, with defaults,
/// typed, with a static variable, extra arguments, a backtrace line,
/// through `array_map`, recursive, with bound variables) behave as on the
/// general path. Expected output is php-src's (`/usr/bin/php -n`, the same
/// with `/usr/bin/php8.5 -n`); the bare test VM has no `array_sum()`, so a
/// loop sums `array_map()`'s results (on the same line: the backtrace line
/// above is part of the output).
#[test]
fn static_closures_with_a_cached_object_take_the_plain_call_path() {
    let source = r#"<?php
class K {
    function mk() { return static function ($a, $b = 1) { return $a * $b; }; }
    function mk2() { return static fn($a) => static::class . $a; }
    function typed() { return static function (int $a): int { return $a + 1; }; }
    function counter() { return static function () { static $n = 0; return ++$n; }; }
    function args() { return static function ($a) { return implode("|", func_get_args()) . func_num_args(); }; }
    function line() { return static function ($a) { return debug_backtrace()[0]['line'] + $a; }; }
}
class L extends K {}
$k = new K; $l = new L;
$f = $k->mk(); $g = $l->mk();
$s = 0;
for ($i = 0; $i < 5; $i++) { $s += $f($i) + $g($i, 2) + $k->mk()($i); }
echo $s, "\n";
$h = $l->mk2(); echo $h(1), " ", $h(2), " ", $k->mk2()(3), "\n";
$t = $k->typed(); echo $t(1), $t(2), " ";
try { $t("x"); } catch (TypeError $e) { echo get_class($e), "\n"; }
$c = $k->counter(); echo $c(), $c(), $c(), "\n";
$a = $k->args(); echo $a(1), " ", $a(2), " ", $a(3, 4), "\n";
$ln = $k->line(); echo $ln(0), " ", $ln(100), "\n";
$m = 0; foreach (array_map($f, [1, 2, 3]) as $x) { $m += $x; } echo $m, " ", implode(",", array_map($g, [1, 2], [3, 4])), "\n";
function outer() { return static function ($x) { return $x + 1; }; }
$o = outer(); echo $o(1), $o(2), outer()(3), "\n";
var_dump((new ReflectionFunction($f))->getStaticVariables());
function deep($n) { $r = static function ($m) use ($n) { return $m + $n; }; return $r($n); }
echo deep(1), deep(2), "\n";
function rec($n) { $r = static function ($m) { return $m <= 0 ? 0 : $m + rec($m - 1); }; return $r($n); }
echo rec(4), "\n";
"#;
    let expected =
        "40\nL1 L2 K3\n23 TypeError\n123\n11 21 3|42\n21 121\n6 3,8\n234\narray(0) {\n}\n24\n10\n";
    assert_eq!(run(source), expected);
}

/// The first op of a plain call is read from a table worked out once per
/// passed-argument count (`UserFunction::plain_first_op`) instead of the
/// per-call `RECV` walk: every count from none to all six inline
/// arguments, with defaults computed by `RECV_INIT` code, starts where the
/// walk started. Expected output is php-src's (`/usr/bin/php -n`); the
/// bare test VM has no ext/json, so the array default is printed by hand
/// as `json_encode()` prints it.
#[test]
fn precomputed_first_op_matches_every_argument_count() {
    let source = r#"<?php
function d($a = 1, $b = [2], $c = PHP_INT_SIZE, $d = "x" . "y", $e = null, $f = 6) {
    return implode(",", [$a, is_array($b) ? "[" . implode(",", $b) . "]" : $b, $c, $d, var_export($e, true), $f]);
}
function r($a, $b, $c = 3) { return "$a$b$c"; }
for ($i = 0; $i < 2; $i++) {
    echo d(), " ", d(9), " ", d(9, 8), " ", d(9, 8, 7), " ", d(9, 8, 7, 6), " ", d(9, 8, 7, 6, 5), " ", d(9, 8, 7, 6, 5, 4), "\n";
    echo r(1, 2), r(1, 2, 0), "\n";
}
"#;
    let line = "1,[2],8,xy,NULL,6 9,[2],8,xy,NULL,6 9,8,8,xy,NULL,6 9,8,7,xy,NULL,6 9,8,7,6,NULL,6 9,8,7,6,5,6 9,8,7,6,5,4\n123120\n";
    assert_eq!(run(source), line.repeat(2));
}
