// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `memory_limit` as a bound per request in a threaded server (plan 3e,
//! review round 4): each case grows a script-sized result past the soft
//! limit inside one builtin, on a worker thread with its own allocator,
//! and must
//!
//! 1. stop near the limit: the peak stays within [`OVERRUN`] above it
//!    (the overrun is bounded, not "until the hard ceiling");
//! 2. leave the process alive and the VM reusable: no hard-ceiling exit,
//!    the next request on the same VM runs and the usage falls back to
//!    the baseline (no unwinding through half-updated state);
//! 3. end like php-src: the "Allowed memory size" fatal, exit status 255,
//!    shutdown functions still run.
//!
//! Every case runs in a child process (the hard ceiling ends it). Growth
//! paths not yet checked against the limit are listed in [`KNOWN_GAPS`]
//! with what goes wrong: the test fails when a new path breaks *and* when
//! a listed one is fixed, so the list only shrinks (branch
//! `memlimit-fallible`, stage 4 brings it to empty).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::process::Command;
use std::rc::Rc;

use php_vm::{Vm, VmOptions};

php_memlimit::install_global_allocator!();

/// The soft limit the growth runs under (`ini_set` before the call).
const LIMIT: usize = 16 << 20;
/// How far above [`LIMIT`] the peak may go: one checked growth step.
const OVERRUN: usize = 2 << 20;

const CASE_ENV: &str = "PHPRUST_MEMLIMIT_P0_CASE";

/// `(name, setup, growth)`: `setup` builds the input under 256M, `growth`
/// is the one statement that grows past [`LIMIT`].
const CASES: &[(&str, &str, &str)] = &[
    // Controls: checked today.
    ("str_repeat", "", "$r = str_repeat('ab', 50000000);"),
    ("str_pad", "", "$r = str_pad('x', 100000000);"),
    (
        "opcode_loop_append",
        "",
        "$r = []; for ($i = 0; ; $i++) { $r[] = $i; }",
    ),
    (
        "opcode_loop_concat",
        "",
        "$r = ''; for (;;) { $r .= str_repeat('x', 65536); }",
    ),
    // Builtins that grow in a loop without a safe point.
    ("array_fill", "", "$r = array_fill(0, 5000000, 0);"),
    ("range", "", "$r = range(1, 5000000);"),
    (
        "explode",
        "$s = str_repeat('a,', 3000000);",
        "$r = explode(',', $s);",
    ),
    (
        "str_split",
        "$s = str_repeat('a', 3000000);",
        "$r = str_split($s);",
    ),
    (
        "mb_str_split",
        "$s = str_repeat('a', 3000000);",
        "$r = mb_str_split($s);",
    ),
    (
        "preg_match_all",
        "$s = str_repeat('a', 3000000);",
        "preg_match_all('/a/', $s, $r);",
    ),
    (
        "preg_split",
        "$s = str_repeat('a', 3000000);",
        "$r = preg_split('//', $s);",
    ),
    (
        "json_decode",
        "$s = '[' . str_repeat('1,', 3000000) . '1]';",
        "$r = json_decode($s);",
    ),
    (
        "unserialize",
        "$s = serialize(array_fill(0, 300000, 'abcdefghij'));",
        "$r = unserialize($s);",
    ),
    (
        "array_merge",
        "$a = range(1, 200000);",
        "$r = array_merge($a, $a, $a, $a, $a);",
    ),
    (
        "array_map",
        "$a = range(1, 200000);",
        "$r = array_map(fn ($v) => [$v], $a);",
    ),
    // String builders that grow one result without a safe point.
    (
        "json_encode",
        "$a = array_fill(0, 500000, 'abcdefghijklmnopqrstuvwxyz0123456789');",
        "$r = json_encode($a);",
    ),
    (
        "implode",
        "$a = array_fill(0, 500000, 'abcdefghijklmnopqrstuvwxyz0123456789');",
        "$r = implode(',', $a);",
    ),
    (
        "serialize",
        "$a = array_fill(0, 500000, 'abcdefghijklmnopqrstuvwxyz0123456789');",
        "$r = serialize($a);",
    ),
    (
        "var_export",
        "$a = array_fill(0, 500000, 'abcdefghijklmnopqrstuvwxyz0123456789');",
        "$r = var_export($a, true);",
    ),
    (
        "http_build_query",
        "$a = array_fill(0, 500000, 'abcdefghijklmnopqrstuvwxyz0123456789');",
        "$r = http_build_query($a);",
    ),
    (
        "str_replace",
        "$s = str_repeat('a', 3000000);",
        "$r = str_replace('a', 'abcdefgh', $s);",
    ),
    // Stage 4 probes.
    (
        "file_get_contents",
        "$p = tempnam(sys_get_temp_dir(), 'p0'); register_shutdown_function(fn () => unlink($p)); \
         $f = fopen($p, 'w'); for ($i = 0; $i < 640; $i++) { fwrite($f, str_repeat(\"abcdefgh\\n\", 6554)); } fclose($f);",
        "$r = file_get_contents($p);",
    ),
    (
        "file_lines",
        "$p = tempnam(sys_get_temp_dir(), 'p0'); register_shutdown_function(fn () => unlink($p)); \
         $f = fopen($p, 'w'); for ($i = 0; $i < 640; $i++) { fwrite($f, str_repeat(\"abcdefgh\\n\", 6554)); } fclose($f);",
        "$r = file($p);",
    ),
    (
        "fread_large",
        "$p = tempnam(sys_get_temp_dir(), 'p0'); register_shutdown_function(fn () => unlink($p)); \
         $f = fopen($p, 'w'); for ($i = 0; $i < 640; $i++) { fwrite($f, str_repeat(\"abcdefgh\\n\", 6554)); } fclose($f);",
        "$f = fopen($p, 'r'); $r = fread($f, 100000000);",
    ),
    (
        "stream_get_contents",
        "$p = tempnam(sys_get_temp_dir(), 'p0'); register_shutdown_function(fn () => unlink($p)); \
         $f = fopen($p, 'w'); for ($i = 0; $i < 640; $i++) { fwrite($f, str_repeat(\"abcdefgh\\n\", 6554)); } fclose($f);",
        "$f = fopen($p, 'r'); $r = stream_get_contents($f);",
    ),
    (
        "stream_get_contents_pipe",
        "",
        "$f = popen('head -c 60000000 /dev/zero', 'r'); $r = stream_get_contents($f);",
    ),
    (
        "fgets_loop_into_array",
        "$p = tempnam(sys_get_temp_dir(), 'p0'); register_shutdown_function(fn () => unlink($p)); \
         $f = fopen($p, 'w'); for ($i = 0; $i < 640; $i++) { fwrite($f, str_repeat(\"abcdefgh\\n\", 6554)); } fclose($f);",
        "$f = fopen($p, 'r'); $r = []; while (($l = fgets($f)) !== false) { $r[] = $l; }",
    ),
    (
        "gzuncompress",
        "$s = gzcompress(str_repeat('a', 60000000));",
        "$r = gzuncompress($s);",
    ),
    (
        "gzdecode",
        "$s = gzencode(str_repeat('a', 60000000));",
        "$r = gzdecode($s);",
    ),
    (
        "bzdecompress",
        "$s = bzcompress(str_repeat('a', 60000000));",
        "$r = bzdecompress($s);",
    ),
    (
        "mb_convert_encoding",
        "$s = str_repeat('a', 6000000);",
        "$r = mb_convert_encoding($s, 'UTF-16');",
    ),
    (
        "mb_strtoupper",
        "$s = str_repeat('a', 9000000);",
        "$r = mb_strtoupper($s);",
    ),
    (
        "iconv",
        "$s = str_repeat('a', 6000000);",
        "$r = iconv('UTF-8', 'UTF-32', $s);",
    ),
    (
        "preg_replace",
        "$s = str_repeat('a', 3000000);",
        "$r = preg_replace('/a/', 'abcdefgh', $s);",
    ),
    (
        "sprintf_pad",
        "",
        "$r = sprintf('%100000000s', 'x');",
    ),
    (
        "base64_encode",
        "$s = str_repeat('a', 14000000);",
        "$r = base64_encode($s);",
    ),
    (
        "bin2hex",
        "$s = str_repeat('a', 9000000);",
        "$r = bin2hex($s);",
    ),
    (
        "strtr_array",
        "$s = str_repeat('a', 3000000);",
        "$r = strtr($s, ['a' => 'abcdefgh']);",
    ),
    (
        "array_pad",
        "",
        "$r = array_pad([], 5000000, 0);",
    ),
    (
        "array_fill_keys",
        "$k = range(1, 300000);",
        "$r = array_fill_keys($k, 'abcdefgh'); $r2 = array_fill_keys($k, 1); $r3 = array_fill_keys($k, 2); $r4 = array_fill_keys($k, 3);",
    ),
    (
        "iterator_to_array_generator",
        "function g() { for (;;) { yield 1; } }",
        "$r = iterator_to_array(g(), false);",
    ),
    (
        "session_decode",
        "session_start(); $s = 'a|' . serialize(array_fill(0, 300000, 'abcdefghij'));",
        "session_decode($s);",
    ),
    (
        "simplexml_load_string",
        "$s = '<r>' . str_repeat('<a>x</a>', 400000) . '</r>';",
        "$r = simplexml_load_string($s); $all = []; foreach ($r->a as $a) { $all[] = $a; }",
    ),
    (
        "dom_load_html",
        "$s = '<p>' . str_repeat('<b>x</b>', 400000);",
        "$d = new DOMDocument(); $d->loadHTML($s);",
    ),
    (
        "str_word_count",
        "$s = str_repeat('ab ', 3000000);",
        "$r = str_word_count($s, 1);",
    ),
    (
        "array_chunk",
        "$a = range(1, 300000);",
        "$r = array_chunk($a, 1);",
    ),
    (
        "array_combine",
        "$a = range(1, 300000);",
        "$r = array_combine($a, $a); $r2 = array_combine($a, $a);",
    ),
    // Found by the trap-mode suite run (plan 3e, V5).
    (
        "mb_detect_encoding_list",
        "$list = array_fill(0, 500000, 'UTF-8');",
        "$r = mb_detect_encoding('hello', $list, false);",
    ),
    (
        "output_add_rewrite_var",
        "$v = str_repeat('a', 3000000);",
        "output_add_rewrite_var($v, $v);",
    ),
    (
        "http_build_query_deep",
        "$a = null; for ($i = 0; $i < 3000; $i++) { $a = [$i => $a]; }",
        "$r = http_build_query($a, 'p');",
    ),
    (
        "pdo_sqlite_fetch_all",
        "$db = new PDO('sqlite::memory:'); \
         $db->exec('CREATE TABLE t (x INTEGER, y TEXT)'); \
         $db->exec(\"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 400000) \
                    INSERT INTO t SELECT x, 'abcdefgh' FROM c\");",
        "$r = $db->query('SELECT * FROM t')->fetchAll();",
    ),
    (
        "pdo_sqlite_fetch_all_key_pair",
        "$db = new PDO('sqlite::memory:'); \
         $db->exec('CREATE TABLE t (x INTEGER, y TEXT)'); \
         $db->exec(\"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 400000) \
                    INSERT INTO t SELECT x, 'abcdefgh' FROM c\");",
        "$r = $db->query('SELECT * FROM t')->fetchAll(PDO::FETCH_KEY_PAIR);",
    ),
    (
        "pdo_sqlite_fetch_all_group",
        "$db = new PDO('sqlite::memory:'); \
         $db->exec('CREATE TABLE t (x INTEGER, y TEXT)'); \
         $db->exec(\"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 400000) \
                    INSERT INTO t SELECT x, 'abcdefgh' FROM c\");",
        "$r = $db->query('SELECT x, y FROM t')->fetchAll(PDO::FETCH_GROUP);",
    ),
    (
        "sqlite3_fetch_all",
        "$db = new SQLite3(':memory:'); \
         $db->exec('CREATE TABLE t (x INTEGER, y TEXT)'); \
         $db->exec(\"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 400000) \
                    INSERT INTO t SELECT x, 'abcdefgh' FROM c\");",
        "$r = $db->query('SELECT * FROM t')->fetchAll();",
    ),
];

/// Opens `$m` (mysqli) on the `MYSQL_TEST_*` server and fills a temporary
/// table `t` with 400 000 rows (server side, outside the limit).
const MYSQLI_SETUP: &str = "$m = new mysqli(getenv('MYSQL_TEST_HOST'), getenv('MYSQL_TEST_USER'), \
         getenv('MYSQL_TEST_PASSWD'), getenv('MYSQL_TEST_DB')); \
     $m->query('SET SESSION cte_max_recursion_depth = 1000000'); \
     $m->query('CREATE TEMPORARY TABLE t (x INT, y VARCHAR(16))'); \
     $m->query(\"INSERT INTO t WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000) \
                SELECT x, 'abcdefgh' FROM c\");";

/// Opens `$m` (mysqli) without a table, for single big rows.
const MYSQLI_CONNECT: &str =
    "$m = new mysqli(getenv('MYSQL_TEST_HOST'), getenv('MYSQL_TEST_USER'), \
         getenv('MYSQL_TEST_PASSWD'), getenv('MYSQL_TEST_DB'));";

/// [`MYSQLI_CONNECT`] for PDO.
const PDO_MYSQL_CONNECT: &str = "$db = new PDO('mysql:host=' . getenv('MYSQL_TEST_HOST') . ';dbname=' . getenv('MYSQL_TEST_DB'), \
         getenv('MYSQL_TEST_USER'), getenv('MYSQL_TEST_PASSWD'));";

/// [`MYSQLI_SETUP`] for PDO: `$db` on the same server.
const PDO_MYSQL_SETUP: &str = "$db = new PDO('mysql:host=' . getenv('MYSQL_TEST_HOST') . ';dbname=' . getenv('MYSQL_TEST_DB'), \
         getenv('MYSQL_TEST_USER'), getenv('MYSQL_TEST_PASSWD')); \
     $db->exec('SET SESSION cte_max_recursion_depth = 1000000'); \
     $db->exec('CREATE TEMPORARY TABLE t (x INT, y VARCHAR(16))'); \
     $db->exec(\"INSERT INTO t WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000) \
                SELECT x, 'abcdefgh' FROM c\");";

/// Cases against a live MySQL server, run when `MYSQL_TEST_HOST` is set
/// (`MYSQL_TEST_USER`, `MYSQL_TEST_PASSWD`, `MYSQL_TEST_DB` as for the
/// php-mysqlnd live tests).
const MYSQL_CASES: &[(&str, &str, &str)] = &[
    // A buffered result reads every row before the call returns.
    (
        "mysqli_query_store",
        MYSQLI_SETUP,
        "$r = $m->query('SELECT * FROM t');",
    ),
    (
        "mysqli_stmt_store_result",
        MYSQLI_SETUP,
        "$s = $m->prepare('SELECT * FROM t'); $s->execute(); $s->store_result();",
    ),
    // Unbuffered: the rows grow in fetch_all / fetchAll.
    (
        "mysqli_fetch_all",
        MYSQLI_SETUP,
        "$r = $m->query('SELECT * FROM t', MYSQLI_USE_RESULT)->fetch_all();",
    ),
    (
        "pdo_mysql_query_store",
        PDO_MYSQL_SETUP,
        "$r = $db->query('SELECT * FROM t');",
    ),
    // One row of 30 MB: two packets, the first 16 MiB.
    (
        "mysqli_big_row_store",
        MYSQLI_CONNECT,
        "$r = $m->query(\"SELECT REPEAT('x', 30000000)\");",
    ),
    (
        "mysqli_big_row_unbuffered",
        MYSQLI_CONNECT,
        "$r = $m->query(\"SELECT REPEAT('x', 30000000)\", MYSQLI_USE_RESULT); $row = $r->fetch_row();",
    ),
    (
        "mysqli_big_row_stmt",
        MYSQLI_CONNECT,
        "$s = $m->prepare(\"SELECT REPEAT('x', 30000000)\"); $s->execute(); $s->store_result();",
    ),
    (
        "pdo_mysql_big_row",
        PDO_MYSQL_CONNECT,
        "$r = $db->query(\"SELECT REPEAT('x', 30000000)\")->fetch();",
    ),
    (
        "pdo_mysql_fetch_all",
        PDO_MYSQL_SETUP,
        "$db->setAttribute(PDO::MYSQL_ATTR_USE_BUFFERED_QUERY, false); \
         $r = $db->query('SELECT * FROM t')->fetchAll();",
    ),
];

/// Cases that do not meet the contract, with what goes wrong. Keep in
/// sync with the run (the test prints every result).
const KNOWN_GAPS: &[(&str, &str)] = &[
    // As php-src, which never calls xmlMemSetup: libxml2 allocates with
    // malloc, outside memory_limit. Counting it was tried (plan 3e, stage
    // 6) and dropped: libxml2 2.9.14, the version Debian 13 and Ubuntu
    // 24.04 ship, loops forever in its HTML parser when an allocation
    // fails, and libxml2's future is uncertain. A threaded server bounds
    // it with the process-wide ceiling (stage 7).
    (
        "dom_load_html",
        "libxml2's parser memory is not counted (as php-src): no fatal",
    ),
];

/// Output sink of a VM.
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

/// The hard ceiling in a child: report it to the parent and end.
fn hard_limit(e: php_memlimit::HardLimitExceeded) -> ! {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "\n#HARD {e}");
    let _ = out.flush();
    std::process::exit(137)
}

fn script(setup: &str, growth: &str) -> String {
    format!(
        "<?php\n\
         register_shutdown_function(function () {{ echo \"\\n#SHUTDOWN \", memory_get_peak_usage(), \"\\n\"; }});\n\
         {setup}\n\
         ini_set('memory_limit', '{LIMIT}');\n\
         memory_reset_peak_usage();\n\
         {growth}\n\
         echo \"\\n#NOT-REACHED\\n\";\n"
    )
}

/// One case on a worker thread with its own allocator, as a threaded
/// server runs it: a baseline request, the case, the next request.
#[test]
fn child_case() {
    let Some(name) = std::env::var(CASE_ENV).ok() else {
        return;
    };
    let &(_, setup, growth) = CASES
        .iter()
        .chain(MYSQL_CASES)
        .find(|c| c.0 == name)
        .expect("known case");
    let src = script(setup, growth);
    assert!(php_memlimit::set_hard_limit_handler(hard_limit));
    let report = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            php_memlimit::attach_thread();
            let sink = Sink::default();
            let opts = VmOptions {
                ini: vec![
                    ("display_errors".into(), "1".into()),
                    ("memory_limit".into(), "256M".into()),
                    ("fatal_error_backtraces".into(), "0".into()),
                ],
            };
            let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL);
            vm.run_script(b"<?php echo memory_get_usage();", "/base.php");
            let base = sink.take();
            vm.request_reset();
            let status = vm.run_script(src.as_bytes(), "/case.php");
            let out = sink.take();
            vm.request_reset();
            vm.run_script(b"<?php echo 'next ok ', memory_get_usage();", "/next.php");
            let next = sink.take();
            format!("{out}\n#STATUS {status}\n#BASE {base}\n#NEXT {next}\n")
        })
        .expect("spawn the worker")
        .join()
        .expect("worker thread");
    print!("{report}");
    let _ = std::io::stdout().flush();
}

/// The value after `#{key} ` in a child's report.
fn field<'a>(report: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("#{key} ");
    report
        .lines()
        .find_map(|l| l.strip_prefix(tag.as_str()))
        .map(str::trim)
}

/// What a child's report shows against the contract; `None` = met.
fn verdict(report: &str, code: Option<i32>) -> (Option<String>, String) {
    let num = |key: &str| field(report, key).and_then(|v| v.parse::<usize>().ok());
    let peak = num("SHUTDOWN");
    let summary = format!(
        "peak {}",
        peak.map_or_else(
            || "?".to_string(),
            |p| format!("{} KiB over", p.saturating_sub(LIMIT) >> 10)
        )
    );
    if let Some(hard) = field(report, "HARD") {
        return (
            Some(format!("process ended at the hard ceiling ({hard})")),
            summary,
        );
    }
    if report.contains("Failed to set memory limit") {
        return (
            Some("the setup needs more than the limit, so the limit never applied".into()),
            summary,
        );
    }
    if code != Some(0) {
        return (Some(format!("child exited with {code:?}")), summary);
    }
    if report.contains("#NOT-REACHED") {
        return (Some("no fatal: the growth succeeded".into()), summary);
    }
    if !report.contains(&format!("Allowed memory size of {LIMIT} bytes exhausted")) {
        return (Some("no memory-limit fatal reported".into()), summary);
    }
    if field(report, "STATUS") != Some("255") {
        return (
            Some(format!("exit status {:?}", field(report, "STATUS"))),
            summary,
        );
    }
    let Some(peak) = peak else {
        return (Some("shutdown function did not run".into()), summary);
    };
    if peak > LIMIT + OVERRUN {
        return (
            Some(format!(
                "overrun {} KiB above the limit",
                (peak - LIMIT) >> 10
            )),
            summary,
        );
    }
    let next = field(report, "NEXT").unwrap_or("");
    let base = num("BASE").unwrap_or(0);
    match next
        .strip_prefix("next ok ")
        .and_then(|u| u.parse::<usize>().ok())
    {
        None => (Some(format!("next request failed: {next:?}")), summary),
        Some(u) if u > base + (1 << 20) => (
            Some(format!(
                "next request starts {} KiB above the baseline",
                (u - base) >> 10
            )),
            summary,
        ),
        Some(_) => (None, summary),
    }
}

#[test]
fn memory_limit_bounds_every_growth_path() {
    if std::env::var_os(CASE_ENV).is_some() {
        return;
    }
    check(CASES);
}

#[test]
fn memory_limit_bounds_mysql_rows() {
    if std::env::var_os(CASE_ENV).is_some() {
        return;
    }
    if std::env::var_os("MYSQL_TEST_HOST").is_none() {
        eprintln!("skipping: MYSQL_TEST_HOST not set");
        return;
    }
    check(MYSQL_CASES);
}

/// Runs `cases` in child processes and holds them to the contract.
fn check(cases: &'static [(&'static str, &'static str, &'static str)]) {
    let exe = std::env::current_exe().expect("test binary path");
    let runs: Vec<_> = cases
        .iter()
        .map(|&(name, _, _)| {
            let exe = exe.clone();
            std::thread::spawn(move || {
                let out = Command::new(exe)
                    .args(["child_case", "--exact", "--nocapture", "--test-threads=1"])
                    .env(CASE_ENV, name)
                    .output()
                    .expect("spawn child");
                let report = String::from_utf8_lossy(&out.stdout).into_owned();
                (name, verdict(&report, out.status.code()), report)
            })
        })
        .collect();
    let mut gaps = BTreeMap::new();
    let mut table = String::new();
    for run in runs {
        let (name, (gap, summary), report) = run.join().expect("child runner");
        let state = gap.as_deref().unwrap_or("ok");
        let _ = writeln!(table, "{name:<30} {summary:<24} {state}");
        if let Some(gap) = gap {
            if std::env::var_os("PHPRUST_MEMLIMIT_P0_VERBOSE").is_some() {
                let _ = writeln!(table, "--- {name}\n{report}");
            }
            gaps.insert(name, gap);
        }
    }
    eprintln!("{table}");
    let known: BTreeMap<&str, &str> = KNOWN_GAPS
        .iter()
        .copied()
        .filter(|(n, _)| cases.iter().any(|c| c.0 == *n))
        .collect();
    let new: Vec<_> = gaps.keys().filter(|n| !known.contains_key(*n)).collect();
    let fixed: Vec<_> = known.keys().filter(|n| !gaps.contains_key(*n)).collect();
    assert!(
        new.is_empty() && fixed.is_empty(),
        "new gaps {new:?}, fixed (remove from KNOWN_GAPS) {fixed:?}\n{table}"
    );
}
