// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `random` extension.
//! Expected values come from php-src (`/usr/bin/php`).

const EXT: php_ext_random::Random = php_ext_random::Random;

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
fn legacy_functions() {
    assert_eq!(
        run(r"mt_srand(42); echo mt_rand(), ' ', mt_rand(1, 100), ' ', rand(10, 1);"),
        "804318771 68 7"
    );
    assert_eq!(
        run(r"try { mt_rand(5, 1); } catch (ValueError $e) { echo $e->getMessage(); }"),
        "mt_rand(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)"
    );
}

#[test]
fn engines() {
    assert_eq!(
        run(
            r"$p = new Random\Engine\PcgOneseq128XslRr64(5); echo bin2hex($p->generate()), ' ';
            $x = new Random\Engine\Xoshiro256StarStar(5); $x->jump(); echo bin2hex($x->generate());"
        ),
        "61506b66398b07b2 038cac77ef8f3c29"
    );
}

#[test]
fn randomizer_methods() {
    assert_eq!(
        run(r"$r = new Random\Randomizer(new Random\Engine\Mt19937(1));
            echo $r->getInt(0, 10), ' ', bin2hex($r->getBytes(5)), ' ', $r->nextInt();"),
        "0 eb8047ff8c 2002651684"
    );
    assert_eq!(
        run(
            r"$r = new Random\Randomizer(new Random\Engine\Xoshiro256StarStar(3));
            echo implode(',', $r->shuffleArray([1,2,3,4,5])), ' ', $r->shuffleBytes('abcdef'), ' ',
                implode(',', $r->pickArrayKeys(['a'=>1,'b'=>2,'c'=>3], 2)), ' ',
                $r->getFloat(0, 1, Random\IntervalBoundary::ClosedClosed);"
        ),
        "2,1,5,3,4 adbcfe a,c 0.95575796270422"
    );
}

#[test]
fn randomizer_serialization_shares_sequence() {
    assert_eq!(
        run(
            r"$r = new Random\Randomizer(new Random\Engine\Xoshiro256StarStar(3));
            $r2 = unserialize(serialize($r)); echo $r->getInt(1, 1000), ' ', $r2->getInt(1, 1000);"
        ),
        "169 169"
    );
}

#[test]
fn user_engine_errors() {
    assert_eq!(
        run(
            r"final class E implements Random\Engine { public function generate(): string { return ''; } }
            try { (new Random\Randomizer(new E))->getInt(0, 10); }
            catch (Random\BrokenRandomEngineError $e) { echo $e->getMessage(); }"
        ),
        "A random engine must return a non-empty string"
    );
}

#[test]
fn get_bytes_from_string_wrapper_call_counts() {
    assert_eq!(
        run(r"final class Wrapper implements Random\Engine {
                public int $calls = 0;
                public function __construct(private Random\Engine $engine) {}
                public function generate(): string {
                    $this->calls++;
                    return $this->engine->generate();
                }
            }
            for ($size = 1; $size <= 256; $size *= 2) {
                $engine = new Wrapper(new Random\Engine\Xoshiro256StarStar(99));
                $r = new Random\Randomizer($engine);
                $source = '';
                for ($i = 0; $i < $size; $i++) { $source .= chr($i); }
                $result = $r->getBytesFromString($source, 17);
                echo strlen($result), ':', $engine->calls, ' ';
                $r->getBytesFromString($source, 1);
                echo $engine->calls, ';';
            }
            $r->getBytesFromString($source . $source, 17);
            echo $engine->calls;"),
        "17:3 4;17:3 4;17:3 4;17:3 4;17:3 4;17:3 4;17:3 4;17:3 4;17:3 4;21"
    );
}

#[test]
fn get_bytes_from_string_user_output() {
    assert_eq!(
        run(r"final class E implements Random\Engine {
                public int $calls = 0;
                public function generate(): string {
                    $this->calls++;
                    if ($this->calls === 1) { return chr(1); }
                    if ($this->calls === 2) { return chr(2) . chr(3); }
                    return chr(0) . chr(1) . chr(2) . chr(3)
                        . chr(0) . chr(1) . chr(2) . chr(3) . chr(255);
                }
            }
            $engine = new E;
            $r = new Random\Randomizer($engine);
            echo $r->getBytesFromString('abcd', 12), ':', $engine->calls, ' ';
            echo $r->getBytesFromString('abcd', 1), ':', $engine->calls;"),
        "bcdabcdabcda:4 a:5"
    );
    assert_eq!(
        run(r"final class E implements Random\Engine {
                public int $calls = 0;
                public function generate(): string {
                    if (++$this->calls === 1) { return 'a'; }
                    throw new Exception('engine stopped');
                }
            }
            try { (new Random\Randomizer(new E))->getBytesFromString('a', 2); }
            catch (Exception $e) { echo $e->getMessage(); }"),
        "engine stopped"
    );
    assert_eq!(
        run(r"final class E implements Random\Engine {
                public function generate(): string { return ''; }
            }
            try { (new Random\Randomizer(new E))->getBytesFromString('a', 1); }
            catch (Random\BrokenRandomEngineError $e) { echo $e->getMessage(); }"),
        "A random engine must return a non-empty string"
    );
}
