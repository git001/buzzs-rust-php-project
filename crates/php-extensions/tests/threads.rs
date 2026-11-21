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
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use std::sync::{Arc, Barrier};

use php_vm::{Vm, VmOptions};

// The accounting allocator, so that `memory_limit` applies; each VM thread
// takes its own (`on_thread`).
php_memlimit::install_global_allocator!();

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

/// Runs `f` on a thread set up like a server's request thread: as large a
/// stack, its own allocator and its own working directory.
fn on_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            php_memlimit::attach_thread();
            php_streams::sys::own_fs_context().expect("unshare(CLONE_FS)");
            f()
        })
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

#[test]
fn a_timeout_ends_only_its_own_request() {
    // `max_execution_time` per thread (php-src ZTS: `EG(timed_out)` per
    // thread): A spins past its limit while B works through it; the
    // timeout reaches A alone.
    const SPIN: &str = "<?php set_time_limit(1); echo \"A start\\n\"; while (true) {}";
    const WORK: &str = r#"<?php
set_time_limit(5);
$t = microtime(true);
while (microtime(true) - $t < 2.0) {}
echo "B done\n";
"#;
    let start = Arc::new(Barrier::new(2));
    let run = |src: &'static str, start: Arc<Barrier>| {
        on_thread(move || {
            let sink = Sink::default();
            let mut vm = new_vm(&sink);
            start.wait();
            let began = std::time::Instant::now();
            vm.run_script(src.as_bytes(), "/timeout.php");
            (sink.take(), began.elapsed())
        })
    };
    let a = run(SPIN, Arc::clone(&start));
    let b = run(WORK, start);
    let (a_out, a_took) = a.join().expect("thread A");
    let (b_out, b_took) = b.join().expect("thread B");
    assert!(
        a_out.starts_with("A start\n")
            && a_out.contains("Maximum execution time of 1 second exceeded"),
        "{a_out}"
    );
    assert!(
        a_took < std::time::Duration::from_secs(3),
        "A took {a_took:?}"
    );
    assert_eq!(b_out, "B done\n");
    assert!(
        b_took >= std::time::Duration::from_secs(2),
        "B took {b_took:?}"
    );
}

#[test]
fn a_memory_limit_ends_only_its_own_request() {
    // `memory_limit` per thread (php-src ZTS: a heap per thread): A
    // allocates past its 8M while B, with 128M, holds 16M at the same time.
    const HOG: &str = r#"<?php
ini_set('memory_limit', '8M');
$s = str_repeat('x', 4 << 20);
$t = str_repeat('y', 6 << 20);
echo "A survived\n";
"#;
    const WORK: &str = r#"<?php
ini_set('memory_limit', '128M');
$s = str_repeat('x', 16 << 20);
echo strlen($s), ' ', memory_get_usage() > (16 << 20) ? 'counted' : 'missing', "\n";
"#;
    let start = Arc::new(Barrier::new(2));
    let run = |src: &'static str, start: Arc<Barrier>| {
        on_thread(move || {
            let sink = Sink::default();
            let mut vm = new_vm(&sink);
            start.wait();
            vm.run_script(src.as_bytes(), "/memory.php");
            sink.take()
        })
    };
    let a = run(HOG, Arc::clone(&start));
    let b = run(WORK, start);
    let a_out = a.join().expect("thread A");
    let b_out = b.join().expect("thread B");
    assert!(
        a_out.contains("Allowed memory size of 8388608 bytes exhausted")
            && !a_out.contains("A survived"),
        "{a_out}"
    );
    assert_eq!(b_out, "16777216 counted\n");
}

#[test]
fn a_chdir_moves_only_its_own_thread() {
    // The working directory per thread (php-src ZTS: `VCWD_*`): A and B
    // each `chdir()` into their own directory and only then, once both
    // have moved, resolve relative paths: PHP's file functions, `include`
    // and a child process must all see the thread's own directory.
    const SETUP: &str = r#"<?php
chdir('@DIR@');
file_put_contents('tag.txt', '@TAG@');
file_put_contents('inc.php', '<?php return "inc-@TAG@";');
"#;
    const READ: &str = r#"<?php
$f = fopen('tag.txt', 'r');
echo getcwd() === realpath('@DIR@') ? 'cwd' : 'other cwd', ' ',
    file_get_contents('tag.txt'), ' ', fread($f, 10), ' ', include 'inc.php', ' ',
    file_exists('../' . basename('@DIR@') . '/tag.txt') ? 'found' : 'missing', ' ',
    trim(shell_exec('pwd')) === realpath('@DIR@') ? 'child' : 'other child', "\n";
"#;
    let process_cwd = std::env::current_dir().expect("cwd");
    let tmp = php_streams::temp::temp_dir();
    let dirs: Vec<Vec<u8>> = (0..2)
        .map(|_| {
            let d = php_streams::temp::tempnam(&tmp, b"cwd").expect("tempnam");
            let p = std::path::Path::new(std::ffi::OsStr::from_bytes(&d));
            std::fs::remove_file(p).expect("drop the temp file");
            std::fs::create_dir(p).expect("a directory in its place");
            d
        })
        .collect();
    let moved = Arc::new(Barrier::new(2));
    let workers: Vec<_> = ["A", "B"]
        .into_iter()
        .zip(dirs.iter().cloned())
        .map(|(tag, dir)| {
            let moved = Arc::clone(&moved);
            on_thread(move || {
                let sink = Sink::default();
                let mut vm = new_vm(&sink);
                let fill = |src: &str| {
                    src.replace("@TAG@", tag)
                        .replace("@DIR@", &String::from_utf8_lossy(&dir))
                };
                vm.run_script(fill(SETUP).as_bytes(), "/setup.php");
                let setup = sink.take();
                vm.request_reset();
                moved.wait();
                vm.run_script(fill(READ).as_bytes(), "/read.php");
                (setup, sink.take())
            })
        })
        .collect();
    let out: Vec<(String, String)> = workers
        .into_iter()
        .map(|w| w.join().expect("worker thread"))
        .collect();
    for d in &dirs {
        let _ = std::fs::remove_dir_all(std::path::Path::new(std::ffi::OsStr::from_bytes(d)));
    }
    assert_eq!(
        out[0],
        (String::new(), "cwd A A inc-A found child\n".to_string())
    );
    assert_eq!(
        out[1],
        (String::new(), "cwd B B inc-B found child\n".to_string())
    );
    assert_eq!(
        std::env::current_dir().expect("cwd"),
        process_cwd,
        "the process's directory stays"
    );
}

#[test]
fn putenv_in_parallel_requests_stays_consistent() {
    // The environment is the process's, as in php-src ZTS, where
    // `tsrm_env_lock` serialises every access: requests that `putenv()`,
    // `getenv()`, list the environment and start children at the same time
    // must each see their own variable, and nothing may read an
    // environment array another thread is replacing.
    const THREADS: usize = 8;
    const REQUEST: &str = r#"<?php
$bad = 0;
for ($i = 0; $i < 400; $i++) {
    putenv("BUZZ_ENV_@T@=$i");
    $bad += getenv("BUZZ_ENV_@T@") !== "$i";
    $bad += (getenv()["BUZZ_ENV_@T@"] ?? null) !== "$i";
    if ($i % 40 === 0) {
        $bad += trim(shell_exec('printenv BUZZ_ENV_@T@')) !== "$i";
    }
}
echo "@T@ $bad\n";
"#;
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let start = Arc::clone(&start);
            on_thread(move || {
                let sink = Sink::default();
                let mut vm = new_vm(&sink);
                start.wait();
                let src = REQUEST.replace("@T@", &t.to_string());
                vm.run_script(src.as_bytes(), "/env.php");
                let out = sink.take();
                vm.request_reset();
                (out, std::env::var_os(format!("BUZZ_ENV_{t}")))
            })
        })
        .collect();
    for (t, w) in workers.into_iter().enumerate() {
        let (out, left) = w.join().expect("worker thread");
        assert_eq!(out, format!("{t} 0\n"));
        assert_eq!(left, None, "the request end restores the environment");
    }
}

#[test]
fn setlocale_changes_only_its_own_thread() {
    // `setlocale()` per thread, as each FPM process has its own locale:
    // requests that switch to a decimal-comma locale run alongside ones
    // that stay in "C", and each formats with its own decimal point, and
    // the next request on the same thread starts in "C" again.
    const THREADS: usize = 6;
    const REQUEST: &str = r#"<?php
$own = '@LOCALE@';
$bad = 0;
for ($i = 0; $i < 300; $i++) {
    if ($own !== 'C') {
        $bad += setlocale(LC_ALL, $own) !== $own;
    }
    $dp = $own === 'C' ? '.' : ',';
    $bad += sprintf('%.1f', 0.5) !== "0{$dp}5";
    $bad += localeconv()['decimal_point'] !== $dp;
    $bad += nl_langinfo(RADIXCHAR) !== $dp;
    $bad += setlocale(LC_NUMERIC, 0) !== $own;
}
echo "$own $bad\n";
"#;
    const AFTER: &str = "<?php echo setlocale(LC_NUMERIC, 0), ' ', sprintf('%.1f', 0.5), \"\\n\";";
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let start = Arc::clone(&start);
            let own = if t % 2 == 0 { "de_DE.UTF-8" } else { "C" };
            on_thread(move || {
                let sink = Sink::default();
                let mut vm = new_vm(&sink);
                start.wait();
                vm.run_script(REQUEST.replace("@LOCALE@", own).as_bytes(), "/locale.php");
                let out = sink.take();
                vm.request_reset();
                vm.run_script(AFTER.as_bytes(), "/after.php");
                (own, out, sink.take())
            })
        })
        .collect();
    for w in workers {
        let (own, out, after) = w.join().expect("worker thread");
        assert_eq!(out, format!("{own} 0\n"));
        assert_eq!(after, "C 0.5\n", "{own}: the next request starts in C");
    }
}

#[test]
fn umask_comes_back_at_request_end() {
    // A request's umask() lasts until its end (`BG(umask)` restored in
    // `PHP_RSHUTDOWN(basic)`) and, with each worker's own file-system
    // context, never reaches a request on another thread.
    const SET: &str = "<?php echo decoct(umask(0077)), ' ', decoct(umask()), \"\\n\";";
    const READ: &str = "<?php echo decoct(umask()), \"\\n\";";
    let set = Arc::new(Barrier::new(2));
    let read = Arc::new(Barrier::new(2));
    let a = {
        let (set, read) = (Arc::clone(&set), Arc::clone(&read));
        on_thread(move || {
            let sink = Sink::default();
            let mut vm = new_vm(&sink);
            vm.run_script(b"<?php umask(0022);", "/init.php");
            vm.request_reset();
            vm.run_script(SET.as_bytes(), "/set.php");
            let during = sink.take();
            set.wait();
            read.wait();
            vm.request_reset();
            vm.run_script(READ.as_bytes(), "/read.php");
            (during, sink.take())
        })
    };
    let b = on_thread(move || {
        let sink = Sink::default();
        let mut vm = new_vm(&sink);
        set.wait();
        vm.run_script(READ.as_bytes(), "/read.php");
        read.wait();
        sink.take()
    });
    let (during, after) = a.join().expect("thread A");
    let other = b.join().expect("thread B");
    assert_eq!(during, "22 77\n");
    assert_eq!(after, "22\n", "the request's umask is gone");
    assert_eq!(other, format!("{:o}\n", php_streams::sys::current_umask()));
}
