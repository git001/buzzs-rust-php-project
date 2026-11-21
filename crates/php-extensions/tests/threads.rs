// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! VMs on several threads of one process, the model of a threaded `FastCGI`
//! or HTTP server (php-src ZTS): every worker thread owns its VMs, only
//! request bytes cross threads. A request's output must not depend on
//! what other threads run at the same time, nor on the requests its VM
//! served before (`Vm::request_reset`, php-src's per-request
//! `php_request_shutdown` / `php_request_startup` on a long-lived process).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::{Arc, Barrier};

use php_vm::{Vm, VmOptions};

/// Requests that read and change per-request state: declarations, static
/// variables, autoloaders, resources, INI values, seeded randomness,
/// output buffers, the default time zone, error handlers, destructors.
const REQUESTS: &[&str] = &[
    r#"
class Counter { public static int $n = 0; public static function hit(): int { return ++self::$n; } }
function tick(): int { static $c = 0; return ++$c; }
Counter::hit(); Counter::hit();
tick(); tick(); tick();
echo Counter::$n, ' ', tick(), "\n";
"#,
    r#"
spl_autoload_register(function (string $c): void { echo "load $c\n"; eval("class $c {}"); });
new Lazy();
echo count(spl_autoload_functions()), ' ', class_exists('Counter', false) ? 'leak' : 'clean', "\n";
"#,
    r#"
$f = fopen('php://memory', 'w+');
fwrite($f, "abc");
rewind($f);
echo fread($f, 10), ' ', get_resource_type($f), "\n";
fclose($f);
"#,
    r#"
echo ini_get('precision'), ' ';
ini_set('precision', '5');
echo 1 / 3, ' ', ini_get('precision'), "\n";
"#,
    r#"
mt_srand(42);
echo mt_rand(), ' ', mt_rand(1, 100), ' ';
srand(7);
echo rand(), "\n";
"#,
    r#"
ob_start();
echo "inner";
$s = ob_get_clean();
echo strtoupper($s), ' ', ob_get_level(), "\n";
"#,
    r#"
echo date_default_timezone_get(), ' ';
date_default_timezone_set('Europe/Vienna');
echo date('Y-m-d H:i', 86400 * 365), ' ', date_default_timezone_get(), "\n";
"#,
    r#"
set_error_handler(function (int $no, string $msg): bool { echo "handled: $msg\n"; return true; });
echo $undefined ?? 'null-coalesce', "\n";
trigger_error('custom', E_USER_WARNING);
"#,
    r#"
class Noisy { public function __construct(public string $name) {} public function __destruct() { echo "bye {$this->name}\n"; } }
$a = new Noisy('a');
function gen() { yield 1; yield 2; }
foreach (gen() as $v) { echo $v; }
echo "\n";
unset($a);
$b = new Noisy('b');
"#,
    r#"
$data = ['k' => [1, 2.5, 'x', null, true], 'u' => "\u{e9}"];
echo json_encode($data), ' ', preg_replace('/(\d)/', '<$1>', 'a1b22'), ' ', sprintf('%.2f|%05d', 3.14159, 42), "\n";
echo str_pad(dechex(crc32('buzz')), 8, '0', STR_PAD_LEFT), ' ', md5('buzz'), "\n";
"#,
];

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Sink {
    fn take(&self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut *self.0.borrow_mut())).into_owned()
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A VM with every linked extension writing to `sink`.
fn new_vm(sink: &Sink) -> Vm {
    let opts = VmOptions {
        ini: vec![("display_errors".into(), "1".into())],
    };
    Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL)
}

/// Runs request `i` of [`REQUESTS`] on `vm`; returns its output.
fn request(vm: &mut Vm, sink: &Sink, i: usize) -> String {
    let src = format!("<?php{}", REQUESTS[i]);
    vm.run_script(src.as_bytes(), &format!("/request-{i}.php"));
    sink.take()
}

/// Every request on a fresh VM of the current thread.
fn fresh_outputs() -> Vec<String> {
    (0..REQUESTS.len())
        .map(|i| {
            let sink = Sink::default();
            let mut vm = new_vm(&sink);
            request(&mut vm, &sink, i)
        })
        .collect()
}

/// Runs `f` on a thread with a stack as large as the servers give their
/// request threads.
fn on_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(f)
        .expect("spawn a VM thread")
}

#[test]
fn requests_have_output() {
    let out = on_thread(fresh_outputs).join().expect("thread");
    assert_eq!(out[0], "2 4\n");
    assert_eq!(out[2], "abc stream\n");
    assert_eq!(out[5], "INNER 0\n");
    assert!(out.iter().all(|o| !o.is_empty()));
}

#[test]
fn parallel_vms_match_one_thread() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 3;
    let expected = Arc::new(on_thread(fresh_outputs).join().expect("thread"));
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let expected = Arc::clone(&expected);
            let start = Arc::clone(&start);
            on_thread(move || {
                start.wait();
                for round in 0..ROUNDS {
                    // Each thread walks the requests from another offset, so
                    // different requests run side by side.
                    for k in 0..REQUESTS.len() {
                        let i = (k + t) % REQUESTS.len();
                        let sink = Sink::default();
                        let mut vm = new_vm(&sink);
                        let out = request(&mut vm, &sink, i);
                        assert_eq!(out, expected[i], "thread {t}, round {round}, request {i}");
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("worker thread");
    }
}

#[test]
fn reused_vm_matches_fresh_vms() {
    let (fresh, reused) = on_thread(|| {
        let fresh = fresh_outputs();
        let sink = Sink::default();
        let mut vm = new_vm(&sink);
        let mut reused = Vec::new();
        for _round in 0..2 {
            for i in 0..REQUESTS.len() {
                reused.push(request(&mut vm, &sink, i));
                vm.request_reset();
            }
        }
        (fresh, reused)
    })
    .join()
    .expect("thread");
    for (n, out) in reused.iter().enumerate() {
        let i = n % REQUESTS.len();
        assert_eq!(out, &fresh[i], "request {i}, served {n} requests before");
    }
}

#[test]
fn autoloaders_do_not_outlive_their_request() {
    // Object handles start again at 1 after a reset, so B's closure looks
    // like A's to the duplicate check of `spl_autoload_register()`: a
    // registry left over from A keeps A's closure and reports it.
    let register = |tag: &str| {
        format!(
            r#"<?php
$f = function (string $c): void {{ echo "{tag} loads $c\n"; eval("class $c {{}}"); }};
spl_autoload_register($f);
new Wanted();
$all = spl_autoload_functions();
echo count($all), ' ', $all[0] === $f ? 'own' : 'stale', "\n";
"#
        )
    };
    let out = on_thread(move || {
        let sink = Sink::default();
        let mut out = Vec::new();
        let mut vm = new_vm(&sink);
        for tag in ["A", "B"] {
            vm.run_script(register(tag).as_bytes(), "/autoload.php");
            out.push(sink.take());
            vm.request_reset();
        }
        // A new VM on the same thread.
        drop(vm);
        let mut vm = new_vm(&sink);
        vm.run_script(register("C").as_bytes(), "/autoload.php");
        out.push(sink.take());
        out
    })
    .join()
    .expect("thread");
    assert_eq!(
        out,
        [
            "A loads Wanted\n1 own\n",
            "B loads Wanted\n1 own\n",
            "C loads Wanted\n1 own\n"
        ]
    );
}
