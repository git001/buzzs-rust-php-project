// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Caches that outlive a request (opcache's units, the realpath cache, run-
//! time caches, interned strings …) must grow under
//! `php_memlimit::persistent`, as php-src keeps them in persistent or
//! shared memory: otherwise the first request of a worker pays for them in
//! its `memory_get_usage()` and against its `memory_limit`. The same
//! script runs several times on one VM; the first run may report only a
//! little more than the later ones (buffers that keep their capacity, such
//! as the class table, grow once), and the later runs report the same.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::{Vm, VmOptions};

php_memlimit::install_global_allocator!();

/// What the first run may report above the later ones.
const FIRST_RUN_SLACK: i64 = 64 << 10;

const INLINE: &str = r##"<?php
$u0 = memory_get_usage();
interface Shape { function area(): float; }
abstract class Base implements Shape { function __construct(protected float $a) {} }
final class Sq extends Base { function area(): float { return $this->a ** 2; } }
enum Suit: string { case H = 'h'; case S = 's'; }
$f = fn($x) => $x * 2;
$a = array_map($f, range(1, 300));
$s = implode(',', $a);
preg_match_all('/(\d+),(\d+)/', $s, $m);
$r = preg_replace('/[13579]/', 'o', $s);
$j = json_decode(json_encode(['a' => $a, 'b' => Suit::H]), true);
$d = date('Y-m-d', 86400 * 365);
$t = sprintf('%05.2f %s %x', 3.14159, 'x', 255);
$o = new Sq(3.0);
$ref = new ReflectionClass($o);
$sp = new SplObjectStorage(); $sp->attach($o);
$g = (function () { yield 1; yield 2; })();
foreach ($g as $v) {}
try { throw new RuntimeException('x'); } catch (Exception $e) {}
unset($a, $s, $m, $r, $j, $d, $t, $o, $ref, $sp, $g, $e, $f);
printf("#USAGE %d %d\n", $u0, memory_get_usage());
"##;

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

/// Runs `src` four times on one VM; `(start, end)` usage of each run.
fn runs(ini: &[(&str, &str)], src: String) -> Vec<(i64, i64)> {
    let ini: Vec<(String, String)> = ini
        .iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            php_memlimit::attach_thread();
            let sink = Sink::default();
            let mut vm = Vm::with_extensions(
                &VmOptions { ini },
                Box::new(sink.clone()),
                php_extensions::ALL,
            );
            (0..4)
                .map(|i| {
                    vm.run_script(src.as_bytes(), "/cache.php");
                    let out = sink.take();
                    vm.request_reset();
                    let line = out
                        .lines()
                        .find_map(|l| l.strip_prefix("#USAGE "))
                        .unwrap_or_else(|| panic!("run {i}: {out}"))
                        .to_owned();
                    let mut it = line.split(' ').map(|v| v.parse::<i64>().expect("number"));
                    (it.next().expect("start"), it.next().expect("end"))
                })
                .collect()
        })
        .expect("spawn")
        .join()
        .expect("worker")
}

fn check(what: &str, runs: &[(i64, i64)]) {
    eprintln!("{what}: start/end per run {runs:?}");
    for r in &runs[2..] {
        assert_eq!(*r, runs[1], "{what}: later runs differ: {runs:?}");
    }
    let extra = runs[0].1 - runs[1].1;
    assert!(
        (0..=FIRST_RUN_SLACK).contains(&extra),
        "{what}: the first run reports {extra} bytes more: {runs:?} (a cache grew outside persistent)"
    );
}

#[test]
fn inline_script() {
    check("inline", &runs(&[], INLINE.to_owned()));
}

/// opcache keeps the compiled include for later requests: the request
/// that compiles it must not keep paying for it (php-src: the script
/// moves to shared memory). Before, the first run here reported 3.9 MB
/// more than the later ones.
#[test]
fn opcache_cached_include() {
    let mut body = String::from("<?php\n");
    for i in 0..200 {
        body.push_str(&format!("class C{i} {{ private array $p = []; "));
        for j in 0..5 {
            body.push_str(&format!(
                "function m{j}($a, $b = {j}) {{ $x = $a + $b; if ($x > {j}) {{ return str_repeat('x', $x % 7) . '{i}-{j}'; }} return [$a, $b, '{i}']; }} "
            ));
        }
        body.push_str("}\n");
    }
    let (mut file, path) =
        php_streams::temp::open_temporary_fd(None, Some(b"opcache-units"), 0).expect("temp file");
    file.write_all(body.as_bytes()).expect("write include");
    drop(file);
    let path = String::from_utf8(path).expect("utf-8 path");
    let src = format!(
        "<?php\n$u0 = memory_get_usage();\nrequire '{path}';\nprintf(\"#USAGE %d %d\\n\", $u0, memory_get_usage());\n"
    );
    // The include was just written: without this opcache would not cache
    // it (a file changed in the last 2 seconds may still be written to).
    let cached = runs(
        &[
            ("opcache.enable_cli", "1"),
            ("opcache.file_update_protection", "0"),
        ],
        src.clone(),
    );
    let plain = runs(&[("opcache.enable_cli", "0")], src);
    let _ = std::fs::remove_file(&path);
    check("opcache", &cached);
    check("no opcache", &plain);
    // The cached runs do not count the compiled code at all.
    assert!(
        cached[1].1 * 2 < plain[1].1,
        "cached {cached:?} vs plain {plain:?}"
    );
}
