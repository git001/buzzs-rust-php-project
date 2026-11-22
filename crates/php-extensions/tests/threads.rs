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
    // `DIRG(default_dir)`: readdir() without a handle uses the last
    // opendir() of this request only.
    r#"
try { var_dump(readdir()); } catch (\Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
$d = opendir('/');
echo readdir() !== false ? "reads\n" : "empty\n";
"#,
    // `FG(default_context)` belongs to the request.
    r#"
echo json_encode(stream_context_get_options(stream_context_get_default())), "\n";
stream_context_set_default(['http' => ['user_agent' => 'left-over']]);
"#,
    // Resource numbers start afresh with each request.
    r#"
$f = fopen('php://memory', 'r');
$g = fopen('php://memory', 'r');
echo (int)$f, ' ', (int)$g, "\n";
"#,
    // get_defined_functions() lists this request's functions in order.
    r#"
function zz_second() {}
function zz_first() {}
echo implode(',', get_defined_functions()['user']), "\n";
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
fn locale_set_default_changes_only_its_own_thread() {
    // `INTL_G(default_locale)` per thread: requests that call
    // `Locale::setDefault()` with different locales run side by side and
    // each reads back its own; the next request on the same thread starts
    // from the configuration again.
    const THREADS: usize = 6;
    const REQUEST: &str = r#"<?php
$own = '@LOCALE@';
$bad = 0;
for ($i = 0; $i < 300; $i++) {
    Locale::setDefault($own);
    $bad += Locale::getDefault() !== $own;
    $bad += ini_get('intl.default_locale') !== $own;
}
echo "$own $bad\n";
"#;
    const AFTER: &str =
        "<?php echo ini_get('intl.default_locale'), '|', Locale::getDefault(), \"\\n\";";
    let initial = on_thread(|| {
        let sink = Sink::default();
        let mut vm = new_vm(&sink);
        vm.run_script(AFTER.as_bytes(), "/after.php");
        sink.take()
    })
    .join()
    .expect("thread");
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let start = Arc::clone(&start);
            let own = ["de_DE", "fr_FR", "ja_JP"][t % 3];
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
        assert_eq!(
            after, initial,
            "{own}: the next request starts from the configuration"
        );
    }
}

/// Serializes the tests that use the process's opcache: one of them
/// resets it, which would drop what another just cached.
static OPCACHE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes [`OPCACHE`] (a failed test does not poison it for the rest).
fn opcache_lock() -> std::sync::MutexGuard<'static, ()> {
    OPCACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A temporary PHP file for an opcache test: its name, and its path.
fn opcache_file() -> (String, std::path::PathBuf) {
    let (file, path) =
        php_streams::temp::create_temporary(None, Some(b"threads-opcache-"), 0, &mut || {})
            .expect("temporary file");
    drop(file);
    let name = String::from_utf8(path).expect("UTF-8 temp path");
    let fs_path = std::path::PathBuf::from(&name);
    (name, fs_path)
}

/// Runs `src` (after `<?php `, with `@P@` standing for `name`) in a new VM
/// on the current thread, with opcache on and no timestamp checks (a
/// changed file is seen only if the cache does not serve the old one),
/// plus `ini`; returns its output.
fn run_opcache(name: &str, ini: &[(&str, &str)], src: &str) -> String {
    let sink = Sink::default();
    let mut all: Vec<(String, String)> = vec![
        ("display_errors".into(), "1".into()),
        ("opcache.enable_cli".into(), "1".into()),
        ("opcache.validate_timestamps".into(), "0".into()),
        // The file was written just now.
        ("opcache.file_update_protection".into(), "0".into()),
    ];
    all.extend(ini.iter().map(|&(k, v)| (k.to_owned(), v.to_owned())));
    let opts = VmOptions { ini: all };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL);
    let src = format!("<?php {}", src.replace("@P@", name));
    vm.run_script(src.as_bytes(), "/opcache.php");
    sink.take()
}

/// [`run_opcache`] on a new thread.
fn run_opcache_on_thread(name: &str, ini: &[(&'static str, &'static str)], src: &str) -> String {
    let (name, ini, src) = (name.to_owned(), ini.to_vec(), src.to_owned());
    on_thread(move || run_opcache(&name, &ini, &src))
        .join()
        .expect("VM thread")
}

/// Includes the opcache test file and says whether it is cached.
const INCLUDE_CACHED: &str =
    "echo include '@P@', ' ', opcache_is_script_cached('@P@') ? 'cached' : 'not cached', \"\\n\";";

#[test]
fn opcache_is_one_cache_for_the_process() {
    // php-src keeps the cached scripts in shared memory, one cache for
    // all workers: a script compiled by one serves the others, and
    // opcache_invalidate() / opcache_reset() by one act for all. Here the
    // file changes behind the cache (no timestamp checks), so only the
    // shared cache gives a later thread the first version.
    let _cache = opcache_lock();
    let (name, fs_path) = opcache_file();
    let run = |src: &str| run_opcache_on_thread(&name, &[], src);
    let cached = INCLUDE_CACHED;
    std::fs::write(&fs_path, "<?php return 'v1';").expect("write v1");
    assert_eq!(run(cached), "v1 cached\n", "first thread compiles");
    std::fs::write(&fs_path, "<?php return 'v2';").expect("write v2");
    assert_eq!(
        run(cached),
        "v1 cached\n",
        "another thread gets the cached script"
    );
    assert_eq!(
        run("var_dump(opcache_invalidate('@P@', true));"),
        "bool(true)\n"
    );
    assert_eq!(
        run(cached),
        "v2 cached\n",
        "the invalidation holds for every thread"
    );
    assert_eq!(run("var_dump(opcache_reset());"), "bool(true)\n");
    assert_eq!(
        run("var_dump(opcache_is_script_cached('@P@'));"),
        "bool(false)\n",
        "the reset holds for every thread"
    );
    let _ = std::fs::remove_file(&fs_path);
}

#[test]
fn opcache_shares_a_script_across_assertion_modes_that_compile_alike() {
    // `zend.assertions` 1 and 0 compile to the same code (only -1 drops
    // the asserts, `zend_compile_assert`); 0/1 is a runtime switch. A
    // script cached under one serves the other, as php-src's shared
    // memory does.
    let _cache = opcache_lock();
    let (name, fs_path) = opcache_file();
    std::fs::write(&fs_path, "<?php return 'v1';").expect("write v1");
    assert_eq!(
        run_opcache_on_thread(&name, &[("zend.assertions", "1")], INCLUDE_CACHED),
        "v1 cached\n",
        "compiled with assertions on"
    );
    std::fs::write(&fs_path, "<?php return 'v2';").expect("write v2");
    assert_eq!(
        run_opcache_on_thread(&name, &[("zend.assertions", "0")], INCLUDE_CACHED),
        "v1 cached\n",
        "served with assertions off"
    );
    let _ = std::fs::remove_file(&fs_path);
}

#[test]
fn opcache_keys_keep_the_working_directory_of_their_thread() {
    // A relative include is found by a key with the working directory in
    // it (`accel_make_persistent_key`). With one cache for the process,
    // two threads in different directories that include the same
    // relative name must each get their own file.
    let _cache = opcache_lock();
    let tmp = php_streams::temp::temp_dir();
    let dirs: Vec<String> = ["A", "B"]
        .iter()
        .map(|tag| {
            let d = php_streams::temp::tempnam(&tmp, b"opcache-cwd").expect("tempnam");
            let d = String::from_utf8(d).expect("UTF-8 temp path");
            std::fs::remove_file(&d).expect("drop the temp file");
            std::fs::create_dir(&d).expect("a directory in its place");
            std::fs::write(format!("{d}/x.php"), format!("<?php return '{tag}';"))
                .expect("write x.php");
            d
        })
        .collect();
    let both = Arc::new(Barrier::new(2));
    let workers: Vec<_> = dirs
        .iter()
        .cloned()
        .map(|dir| {
            let both = Arc::clone(&both);
            on_thread(move || {
                let sink = Sink::default();
                let opts = VmOptions {
                    ini: vec![
                        ("display_errors".into(), "1".into()),
                        ("opcache.enable_cli".into(), "1".into()),
                        ("opcache.file_update_protection".into(), "0".into()),
                    ],
                };
                let mut vm =
                    Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL);
                let src = format!(
                    "<?php chdir('{dir}'); $a = include 'x.php'; echo $a, ' ', include 'x.php', \"\\n\";"
                );
                both.wait();
                vm.run_script(src.as_bytes(), "/opcache-cwd.php");
                sink.take()
            })
        })
        .collect();
    let out: Vec<String> = workers
        .into_iter()
        .map(|w| w.join().expect("VM thread"))
        .collect();
    for d in &dirs {
        let _ = std::fs::remove_file(format!("{d}/x.php"));
        let _ = std::fs::remove_dir(d);
    }
    assert_eq!(out, ["A A\n", "B B\n"]);
}

#[test]
fn every_thread_starts_with_the_process_lc_ctype() {
    // `zend_reset_lc_ctype_locale()` at process start (the standard
    // extension's `module_startup`): "C.UTF-8", so `escapeshellarg()`
    // keeps multibyte characters (bug74589_utf8.phpt), on every thread.
    const REQUEST: &str = "<?php echo setlocale(LC_CTYPE, 0), ' ', escapeshellarg('a_新建');";
    let workers: Vec<_> = (0..4)
        .map(|_| {
            on_thread(|| {
                let sink = Sink::default();
                let mut vm = new_vm(&sink);
                vm.run_script(REQUEST.as_bytes(), "/ctype.php");
                sink.take()
            })
        })
        .collect();
    for w in workers {
        assert_eq!(w.join().expect("worker thread"), "C.UTF-8 'a_新建'");
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

/// A temporary file of the test's own, and how many descriptors of this
/// process point at it: other tests running meanwhile open files of their
/// own, so only these count.
fn own_file() -> (Vec<u8>, impl Fn() -> usize) {
    let (file, path) =
        php_streams::temp::create_temporary(None, Some(b"threads-fds-"), 0, &mut || {})
            .expect("temporary file");
    drop(file);
    let target = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path));
    let count = move || {
        std::fs::read_dir("/proc/self/fd").map_or(0, |dir| {
            dir.filter_map(Result::ok)
                .filter(|e| std::fs::read_link(e.path()).is_ok_and(|l| l == target))
                .count()
        })
    };
    (path, count)
}

#[test]
fn cycles_do_not_outlive_their_request() {
    // `zend_objects_store_free_object_storage()` and the global symbol
    // table's destruction at request shutdown: an object cycle holding a
    // megabyte and an open file, and an array that holds a reference to
    // itself, are freed when the request ends, so a VM serving request
    // after request neither grows nor keeps descriptors open.
    const CYCLES: &str = "<?php
        $r = [];
        $r[0] = &$r;
        $r[1] = str_repeat('y', 1 << 20);
        $a = new stdClass;
        $b = new stdClass;
        $a->b = $b;
        $b->a = $a;
        $a->f = fopen('@FILE@', 'r');
        $a->s = str_repeat('x', 1 << 20);
        echo 'ok';";
    let (path, open_fds) = own_file();
    let script = CYCLES.replace("@FILE@", &String::from_utf8_lossy(&path));
    let (grown, fds_before, fds_after, out) = on_thread(move || {
        let sink = Sink::default();
        let mut vm = new_vm(&sink);
        vm.run_script(script.as_bytes(), "/cycles.php");
        vm.request_reset();
        let fds_before = open_fds();
        let first = php_memlimit::current().raw_usage();
        for _ in 0..20 {
            vm.run_script(script.as_bytes(), "/cycles.php");
            vm.request_reset();
        }
        let grown = php_memlimit::current().raw_usage().saturating_sub(first);
        (grown, fds_before, open_fds(), sink.take())
    })
    .join()
    .expect("worker thread");
    let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&path));
    assert_eq!(out, "ok".repeat(21));
    assert_eq!(fds_after, fds_before, "descriptors left open");
    // Each request left 2 MiB behind before.
    assert!(grown < 64 << 10, "{grown} bytes left behind by 20 requests");
}

/// A test extension whose `buzz_test_panic()` panics, as a bug in Buzz
/// would: the request cannot end normally.
struct PanicProbe;

impl php_vm::ext::Extension for PanicProbe {
    fn name(&self) -> &'static str {
        "panic_probe"
    }

    fn register(&self, m: &mut php_vm::ext::ModuleBuilder<'_>) {
        m.function_static("buzz_test_panic(): void", |_, _| panic!("probe panicked"));
    }
}

#[test]
fn a_panic_ends_only_its_own_request_and_leaves_nothing_behind() {
    // The request holds open files, an object cycle and an array cycle of
    // a megabyte each when it panics. The panic comes back as a value; the
    // request's state is released without running PHP code
    // (`Vm::finalize`: the destructor below must not run), so the thread's
    // descriptors and memory are what they were before the request.
    // Requests on other threads carry on meanwhile.
    const PANICS: &str = "<?php
        class D { public $self; public $data; function __destruct() { echo \"dtor\\n\"; } }
        $files = [fopen('@FILE@', 'r'), fopen('@FILE@', 'r')];
        $d = new D;
        $d->self = $d;
        $d->data = str_repeat('x', 1 << 20);
        $r = [];
        $r[0] = &$r;
        $r[1] = str_repeat('y', 1 << 20);
        echo 'before ';
        buzz_test_panic();
        echo 'unreachable';";
    let (path, open_fds) = own_file();
    let script = PANICS.replace("@FILE@", &String::from_utf8_lossy(&path));
    let start = Arc::new(Barrier::new(2));
    let neighbour = {
        let start = Arc::clone(&start);
        on_thread(move || {
            start.wait();
            (0..REQUESTS.len())
                .map(|i| {
                    let sink = Sink::default();
                    let mut vm = new_vm(&sink);
                    request(&mut vm, &sink, i)
                })
                .collect::<Vec<_>>()
        })
    };
    let panicking = on_thread(move || {
        let sink = Sink::default();
        let exts: Vec<&dyn php_vm::ext::Extension> = php_extensions::ALL
            .iter()
            .copied()
            .chain(std::iter::once(&PanicProbe as &dyn php_vm::ext::Extension))
            .collect();
        let opts = VmOptions {
            ini: vec![("display_errors".into(), "1".into())],
        };
        let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
        vm.run_script(b"<?php echo 'warm';", "/warm.php");
        vm.request_reset();
        let _ = sink.take();
        let fds_before = open_fds();
        let mem_before = php_memlimit::current().raw_usage();
        start.wait();
        let caught = php_sapi::panic::guard(|| vm.run_script(script.as_bytes(), "/panics.php"))
            .expect_err("the request panics");
        let finalized = php_sapi::panic::guard(|| vm.finalize());
        let out = sink.take();
        // Measured with the VM still alive: the request's state is gone
        // already (dropping the VM frees its tables, which would hide a
        // leak).
        let fds_after = open_fds();
        let mem_after = php_memlimit::current().raw_usage();
        php_sapi::panic::discard(vm);
        (
            caught.message,
            finalized.is_ok(),
            out,
            fds_before,
            fds_after,
            mem_before,
            mem_after,
        )
    });
    let (message, finalized, out, fds_before, fds_after, mem_before, mem_after) =
        panicking.join().expect("the panic stays inside its guard");
    let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&path));
    assert_eq!(message, "probe panicked");
    assert!(finalized, "finalize itself did not panic");
    assert_eq!(
        out, "before ",
        "no destructor, shutdown function or output after the panic"
    );
    assert_eq!(fds_after, fds_before, "descriptors left open");
    // The request built 2 MiB of cycles. What is left is the compiled
    // script with its class, which the VM keeps until it is dropped right
    // after (about 120 KiB).
    let left = mem_after.saturating_sub(mem_before);
    assert!(left < 512 << 10, "{left} bytes left behind");
    assert_eq!(
        neighbour.join().expect("neighbour thread"),
        fresh_outputs(),
        "the neighbour's requests are unaffected"
    );
    // A new thread with a new VM works.
    let again = on_thread(|| {
        let sink = Sink::default();
        let mut vm = new_vm(&sink);
        request(&mut vm, &sink, 0)
    })
    .join()
    .expect("new thread");
    assert_eq!(again, fresh_outputs()[0]);
}
