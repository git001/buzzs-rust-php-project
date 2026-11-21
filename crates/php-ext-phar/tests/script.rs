// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the streams, SPL and
//! phar extensions. Expected output comes from `/usr/bin/php` 8.3 (run
//! with `-d phar.readonly=0` where noted).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_runtime::{Value, ZStr};
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

/// Runs `code` (without `<?php`) with the given INI settings and returns
/// its output.
fn run(code: &str, ini: &[(&str, &str)]) -> String {
    run_as(code, ini, None)
}

/// [`run`] under a web SAPI: `PHP_SAPI` is `cli-server`, so
/// `Phar::webPhar()` serves the request the script describes in
/// `$_SERVER`.
fn run_web(code: &str, ini: &[(&str, &str)]) -> String {
    run_as(code, ini, Some("cli-server"))
}

fn run_as(code: &str, ini: &[(&str, &str)], sapi: Option<&str>) -> String {
    let sink = Sink::default();
    let streams = php_ext_streams::Streams;
    let spl = php_ext_spl::Spl;
    let phar = php_ext_phar::Phar;
    let exts: [&dyn Extension; 6] = [
        &php_ext_stdstr::StdStr,
        &php_ext_stdmisc::StdMisc,
        &php_ext_hash::StandardDigests,
        &streams,
        &spl,
        &phar,
    ];
    let opts = VmOptions {
        ini: ini
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    if let Some(sapi) = sapi {
        vm.constants
            .insert(ZStr::from("PHP_SAPI"), Value::Str(ZStr::from(sapi)));
    }
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

const WRITABLE: &[(&str, &str)] = &[("phar.readonly", "0")];

#[test]
fn create_read_and_unlink_a_phar() {
    let out = run(
        r#"$f = sys_get_temp_dir() . '/phpext-phar-a-' . getmypid() . '.phar';
@unlink($f);
$p = new Phar($f, 0, 'a.phar');
$p['a.txt'] = 'hello';
$p->addFromString('dir/b.txt', 'world');
$p->addEmptyDir('empty');
var_dump($p->count(), isset($p['a.txt']), isset($p['nope']), $p->getAlias());
echo file_get_contents("phar://$f/dir/b.txt"), "\n";
echo file_get_contents('phar://a.phar/a.txt'), "\n";
echo $p['a.txt']->getContent(), " ", $p['a.txt']->getCRC32() === crc32('hello') ? 'crc ok' : 'crc bad', "\n";
var_dump($p->getSignature()['hash_type']);
$p->setMetadata(['k' => 1]);
var_dump($p->getMetadata(), $p->hasMetadata());
try { $p['.phar/x'] = 'x'; } catch (BadMethodCallException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
try { $p['missing']; } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
unset($p);
var_dump(Phar::unlinkArchive($f), file_exists($f));"#,
        WRITABLE,
    );
    assert_eq!(
        out,
        "int(3)\nbool(true)\nbool(false)\nstring(6) \"a.phar\"\nworld\nhello\nhello crc ok\n\
         string(7) \"SHA-256\"\narray(1) {\n  [\"k\"]=>\n  int(1)\n}\nbool(true)\n\
         BadMethodCallException: Cannot set any files or directories in magic \".phar\" directory\n\
         Entry missing does not exist\nbool(true)\nbool(false)\n"
    );
}

#[test]
fn phardata_iteration_and_build() {
    let out = run(
        r#"$f = sys_get_temp_dir() . '/phpext-phar-b-' . getmypid() . '.tar';
@unlink($f);
$p = new PharData($f);
$p['a.txt'] = 'hello';
$p->addFromString('dir/b.txt', 'world');
// a directory iterator lists the entries present when it was constructed
$p = new PharData($f);
foreach (new RecursiveIteratorIterator($p) as $name => $info) { echo substr($name, strlen("phar://$f")), ':', get_class($info), ':', $info->getFilename(), "\n"; }
foreach ($p as $name => $info) { echo substr($name, strlen("phar://$f")), "\n"; }
var_dump($p->isFileFormat(Phar::TAR), $p['dir/b.txt']->isCompressed());
$src = $f . '.src';
file_put_contents($src, 'source');
$r = $p->buildFromIterator(new ArrayIterator(['c.txt' => $src]));
foreach ($r as $k => $v) { echo $k, ' <= ', $v === $src ? 'src' : $v, "\n"; }
var_dump($p['c.txt']->getContent());
unlink($src);
try { $p->setStub('x'); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
try { $p->addFromString("a\0b", 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
unset($p);
var_dump(Phar::unlinkArchive($f));
var_dump(Phar::isValidPharFilename('x.phar'), Phar::isValidPharFilename('x.tar', false), Phar::canWrite());
echo Phar::running() === '' ? "not running\n" : "running\n";"#,
        WRITABLE,
    );
    assert_eq!(
        out,
        "/a.txt:PharFileInfo:a.txt\n/dir/b.txt:PharFileInfo:b.txt\n/a.txt\n/dir\nbool(true)\nbool(false)\n\
         c.txt <= src\nstring(6) \"source\"\n\
         A Phar stub cannot be set in a plain tar archive\n\
         PharData::addFromString(): Argument #1 ($localName) must not contain any null bytes\n\
         bool(true)\nbool(true)\nbool(true)\nbool(true)\nnot running\n"
    );
}

#[test]
fn readonly_ini_and_statics() {
    let out = run(
        r#"var_dump(ini_get('phar.readonly'), Phar::canWrite(), ini_set('phar.readonly', 0), ini_get('phar.readonly'));
try { new Phar(sys_get_temp_dir() . '/phpext-phar-c-nonexistent.phar'); } catch (UnexpectedValueException $e) { echo get_class($e), "\n"; }
var_dump(Phar::apiVersion(), Phar::canCompress(Phar::GZ));
try { Phar::mungServer([]); } catch (PharException $e) { echo $e->getMessage(), "\n"; }
try { Phar::mapPhar(); } catch (PharException $e) { echo $e->getMessage(), "\n"; }"#,
        &[],
    );
    assert_eq!(
        out,
        "string(1) \"1\"\nbool(false)\nbool(false)\nstring(1) \"1\"\nUnexpectedValueException\n\
         string(5) \"1.1.1\"\nbool(true)\n\
         No values passed to Phar::mungServer(), expecting an array of any of these strings: PHP_SELF, REQUEST_URI, SCRIPT_FILENAME, SCRIPT_NAME\n\
         __HALT_COMPILER(); must be declared in a phar\n"
    );
}

/// `Phar::webPhar()` under a web SAPI runs a PHP entry of the archive as
/// php-src's `phar_file_action()` does for `PHAR_MIME_PHP`: compiled from
/// its `phar://` URL (`__FILE__`), at the top level with the global
/// variables, registered in `EG(included_files)` (an `include_once` of it
/// is a no-op), resolving its own includes inside the archive; the request
/// ends with it, so the rest of the stub never runs. Oracle: php 8.3
/// `php -S` serving the same archive through a router that includes it.
#[test]
fn webphar_runs_a_php_entry() {
    let out = run_web(
        r#"$f = sys_get_temp_dir() . '/phpext-phar-web-' . getmypid() . '.phar';
@unlink($f);
$p = new Phar($f);
$p['index.php'] = '<?php
echo "index: ", basename(__FILE__), "\n";
include "lib/a.php";
echo isset($g) ? "sees globals\n" : "no globals\n";
include_once __FILE__;
echo "done\n";
';
$p['lib/a.php'] = '<?php echo "lib a\n";';
$p->setStub('<?php
Phar::webPhar();
echo "not reached\n";
__HALT_COMPILER();');
unset($p);
register_shutdown_function(function () use ($f) { @unlink($f); });
$g = 1;
$_SERVER['REQUEST_METHOD'] = 'GET';
$_SERVER['REQUEST_URI'] = '/' . basename($f) . '/index.php';
include $f;
echo "after include\n";"#,
        WRITABLE,
    );
    assert_eq!(out, "index: index.php\nlib a\nsees globals\ndone\n");
}
