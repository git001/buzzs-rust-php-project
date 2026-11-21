// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP snippets run on a VM with this extension.

use super::Pdo;
use php_runtime::{Value, ZStr};
use php_vm::ext::{Args, Extension, VmResult};
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
    let sink = Sink::default();
    let streams = php_ext_streams::Streams;
    let ext = Pdo;
    let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &streams, &ext];
    let opts = VmOptions { ini: Vec::new() };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    let src = format!("<?php\n{code}");
    vm.run_script(src.as_bytes(), "test.php");
    // The streams extension's STDIN/STDOUT/STDERR resources close the
    // process descriptors when dropped, which would take the test
    // harness's output with them: keep the VM alive.
    std::mem::forget(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn pdo_query_and_fetch_modes() {
    let out = run(r#"
        $db = new PDO('sqlite::memory:');
        $db->exec('CREATE TABLE t(id int, name text)');
        $db->exec("INSERT INTO t VALUES(1,'a'),(2,'b')");
        foreach ($db->query('SELECT * FROM t', PDO::FETCH_NUM) as $k => $r) {
            echo $k, ':', implode(',', $r), ';';
        }
        $s = $db->prepare('SELECT name FROM t WHERE id = ?');
        $s->execute([2]);
        echo $s->fetchColumn(), ';', $s->queryString;
    "#);
    assert_eq!(out, "0:1,a;1:2,b;b;SELECT name FROM t WHERE id = ?");
}

#[test]
fn pdo_exception_carries_error_info() {
    let out = run(r#"
        $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
        try { $db->query('SELECT * FROM nope'); }
        catch (PDOException $e) {
            echo get_class($e), '|', $e->getCode(), '|', $e->errorInfo[2], '|', $e instanceof RuntimeException ? 'rt' : 'x';
        }
    "#);
    assert_eq!(out, "PDOException|HY000|no such table: nope|rt");
}

#[test]
fn bound_column_and_param_references() {
    let out = run(r#"
        $db = new Pdo\Sqlite('sqlite::memory:');
        $db->exec('CREATE TABLE t(v int)');
        $ins = $db->prepare('INSERT INTO t VALUES(?)');
        $ins->bindParam(1, $x);
        foreach ([3, 4] as $x) { $ins->execute(); }
        $s = $db->query('SELECT v FROM t');
        $s->bindColumn(1, $v);
        while ($s->fetch(PDO::FETCH_BOUND)) { echo $v; }
        echo get_class($db), PDO::getAvailableDrivers()[0];
    "#);
    assert_eq!(out, "34Pdo\\Sqlitemysql");
}

#[test]
fn sqlite_udf_and_clone_refusal() {
    let out = run(r#"
        $db = new Pdo\Sqlite('sqlite::memory:');
        $db->createFunction('twice', fn($v) => $v * 2);
        echo $db->query('SELECT twice(21)')->fetchColumn(), ';';
        try { clone $db; } catch (Error $e) { echo $e->getMessage(); }
    "#);
    assert_eq!(
        out,
        "42;Trying to clone an uncloneable object of class Pdo\\Sqlite"
    );
}

#[test]
fn sqlite3_classes() {
    let out = run(r#"
        $db = new SQLite3(':memory:');
        $db->exec('CREATE TABLE x(a)');
        $st = $db->prepare('INSERT INTO x VALUES(:a)');
        $st->bindValue(':a', 7, SQLITE3_INTEGER);
        $st->execute();
        $r = $db->query('SELECT a FROM x');
        echo $r->fetchArray(SQLITE3_NUM)[0], ';', $db->querySingle('SELECT count(*) FROM x'), ';';
        $db->enableExceptions(true);
        try { $db->exec('bogus'); } catch (SQLite3Exception $e) { echo get_class($e); }
    "#);
    assert_eq!(out, "7;1;SQLite3Exception");
}

#[test]
fn pdorow_cannot_be_constructed() {
    let out = run(r#"
        try { new PDORow(); } catch (PDOException $e) { echo $e->getMessage(); }
    "#);
    assert_eq!(out, "You may not create a PDORow manually");
}

/// pdo_035: `&$row[0]` reads the column with the "Indirect modification"
/// notice; `&$row[]` throws from `row_dim_read`.
#[test]
fn pdorow_dimension_fetch_by_reference() {
    let out = run(r#"
        set_error_handler(function ($no, $msg) { echo $no, ':', $msg, ';'; return true; });
        $db = new PDO('sqlite::memory:');
        $row = $db->query('SELECT 1 AS a')->fetch(PDO::FETCH_LAZY);
        try { $r = &$row[0]; echo 'ok;'; } catch (Error $e) { echo $e->getMessage(), ';'; }
        try { $r = &$row[]; } catch (Error $e) { echo $e->getMessage(), ';'; }
        echo $row[0];
    "#);
    assert_eq!(
        out,
        "8:Indirect modification of overloaded element of PDORow has no effect;ok;\
         Cannot append to PDORow offset;1"
    );
}

/// pdo_fetch_func_001: `zend_is_callable()` on the `FETCH_FUNC` callable
/// raises the relative-callable deprecation once, in the caller's scope.
#[test]
fn fetch_func_relative_callable_deprecation() {
    let out = run(r#"
        set_error_handler(function ($no, $msg) { echo $no, ':', $msg, ';'; return true; });
        class Foo { public function m($x) { return __METHOD__ . "($x)"; } }
        class Bar extends Foo {
            public function run($db) {
                $st = $db->query('SELECT 1 UNION ALL SELECT 2');
                echo implode(',', $st->fetchAll(PDO::FETCH_FUNC, [$this, 'parent::m']));
            }
        }
        (new Bar)->run(new PDO('sqlite::memory:'));
    "#);
    assert_eq!(
        out,
        "8192:Callables of the form [\"Bar\", \"parent::m\"] are deprecated;Foo::m(1),Foo::m(2)"
    );
}

#[test]
fn blob_stream_resource() {
    let out = run(r#"
        $db = new SQLite3(':memory:');
        $db->exec("CREATE TABLE b(d BLOB); INSERT INTO b VALUES('hello')");
        $s = $db->openBlob('b', 'd', 1);
        echo get_resource_type($s), ':', stream_get_contents($s);
    "#);
    assert_eq!(out, "stream:hello");
}

/// `dbh_method_get`: the legacy `sqlite*` methods come from the driver's
/// method table, so a `PDO` without an `SQLite` connection has no such
/// method ("Call to undefined method", as spelled, with no deprecation
/// first), while an `SQLite` one reports the 8.5 deprecation and runs it.
#[test]
fn legacy_sqlite_methods_need_an_sqlite_connection() {
    let out = run(r#"
        set_error_handler(function ($no, $msg) { echo $no, ':', $msg, ';'; return true; });
        class P extends PDO { function __construct() {} }
        try { (new P)->sqliteCreateFunction('f', 'strlen'); }
        catch (Error $e) { echo get_class($e), ':', $e->getMessage(), ';'; }
        try { (new P)->SQLITECREATECOLLATION('c', 'strcmp'); }
        catch (Error $e) { echo $e->getMessage(), ';'; }
        $db = new PDO('sqlite::memory:');
        var_dump($db->sqliteCreateFunction('f', 'strlen'));
    "#);
    assert_eq!(
        out,
        "Error:Call to undefined method P::sqliteCreateFunction();\
         Call to undefined method P::SQLITECREATECOLLATION();\
         8192:Method PDO::sqliteCreateFunction() is deprecated since 8.5, \
         use Pdo\\Sqlite::createFunction() instead;bool(true)\n"
    );
}

/// `pdo_hash_methods`: the legacy `sqlite*` methods live in the driver's
/// method table, not in `PDO`'s: absent from `get_class_methods()` and
/// `method_exists('PDO', …)`, found by `method_exists()` and
/// `is_callable()` only on an `SQLite` connection, refused by callable
/// checks elsewhere, and free for a subclass to declare with any
/// signature.
#[test]
fn legacy_sqlite_methods_are_driver_methods() {
    let out = run(r#"
        set_error_handler(function ($no, $msg) { echo $no, ':', $msg, ';'; return true; });
        class P extends PDO { function __construct() {} }
        class Q extends PDO { function __construct() {} function sqliteCreateFunction($x) { return "mine $x"; } }
        class R extends PDO { function __call($n, $a) { return "call $n"; } }
        $p = new P;
        $s = new PDO('sqlite::memory:');
        echo (int) method_exists('PDO', 'sqliteCreateFunction'),
            (int) method_exists($p, 'sqliteCreateFunction'),
            (int) method_exists($s, 'sqliteCreateFunction'),
            (int) method_exists(new R('sqlite::memory:'), 'sqliteCreateFunction'),
            (int) is_callable([$p, 'sqliteCreateFunction']),
            (int) is_callable([$s, 'SQLITECREATECOLLATION']), ';';
        foreach (get_class_methods($s) as $m) {
            if ($m === 'sqliteCreateFunction') { echo 'listed;'; }
        }
        echo (new Q)->sqliteCreateFunction(1), ';';
        echo (new R('sqlite::memory:'))->sqliteCreateFunction(1), ';';
        try { call_user_func([$p, 'sqliteCreateFunction'], 'f', 'strlen'); }
        catch (TypeError $e) { echo $e->getMessage(), ';'; }
        var_dump(call_user_func([$s, 'sqliteCreateCollation'], 'c', 'strcmp'));
    "#);
    assert_eq!(
        out,
        "001001;mine 1;call sqliteCreateFunction;\
         call_user_func(): Argument #1 ($callback) must be a valid callback, \
         class P does not have a method \"sqliteCreateFunction\";\
         8192:Method PDO::sqliteCreateCollation() is deprecated since 8.5, \
         use Pdo\\Sqlite::createCollation() instead;bool(true)\n"
    );
}

/// `pdo_hash_methods` builds the driver's method table per connection
/// (`dbh->cls_methods`), not per class: two `SQLite` connections hold
/// distinct method objects (an observer reports `init` for each of them),
/// one connection always hands out the same, and a `PDO` without an
/// `SQLite` connection has none.
#[test]
fn driver_methods_are_built_per_connection() {
    fn probe(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
        let Value::Object(o) = a.at(0) else {
            return Ok(Value::Int(-1));
        };
        let m = super::pdo::pdo_driver_method(vm, &o, &ZStr::from("sqliteCreateFunction"));
        let id = m.map_or(0, |m| Rc::as_ptr(&m) as usize);
        Ok(Value::Int(i64::try_from(id).unwrap_or(-1)))
    }
    let sink = Sink::default();
    let streams = php_ext_streams::Streams;
    let ext = Pdo;
    let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &streams, &ext];
    let opts = VmOptions { ini: Vec::new() };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    vm.register_function("driver_method_id(object $pdo): int", probe);
    vm.run_script(
        br"<?php
        class P extends PDO { function __construct() {} }
        $a = new PDO('sqlite::memory:');
        $b = new PDO('sqlite::memory:');
        $ia = driver_method_id($a);
        var_dump($ia !== 0, $ia === driver_method_id($a), $ia !== driver_method_id($b), driver_method_id(new P));
        ",
        "test.php",
    );
    // See `run`: keep the VM alive.
    std::mem::forget(vm);
    let out = sink.0.borrow().clone();
    assert_eq!(
        String::from_utf8_lossy(&out),
        "bool(true)\nbool(true)\nbool(true)\nint(0)\n"
    );
}
