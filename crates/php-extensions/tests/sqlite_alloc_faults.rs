// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! sqlite runs on php-memlimit's C hooks (`SQLITE_CONFIG_MALLOC`), which
//! return NULL past the limit. Plan 3e (V4) counts a library only where
//! NULL is handled cleanly: the n-th allocation of a sqlite script
//! (`SQLite3` and PDO, in-memory databases) fails, for n swept over the
//! whole script. Every run must end either normally or with the memory
//! fatal, and the next request on the same VM must work. A crash or a hang
//! ends the test binary (or its timeout).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::{Vm, VmOptions};

php_memlimit::install_global_allocator!();

const SCRIPT: &str = r##"<?php
$db = new SQLite3(':memory:');
$db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL)');
$db->exec('CREATE INDEX i ON t (name)');
$db->exec('BEGIN');
$st = $db->prepare('INSERT INTO t (name, v) VALUES (:n, :v)');
for ($i = 0; $i < 200; $i++) { $st->bindValue(':n', "n$i"); $st->bindValue(':v', $i / 3); $st->execute(); $st->reset(); }
$db->exec('COMMIT');
$sum = $db->querySingle('SELECT count(*) FROM t WHERE name LIKE \'n1%\'');
$r = $db->query('SELECT name, v FROM t ORDER BY v DESC LIMIT 5'); $rows = 0;
while ($r->fetchArray()) { $rows++; }
$e = SQLite3::escapeString("it's");
$pdo = new PDO('sqlite::memory:');
$pdo->exec('CREATE TABLE u (a, b)');
$ins = $pdo->prepare('INSERT INTO u VALUES (?, ?)');
for ($i = 0; $i < 100; $i++) { $ins->execute([$i, str_repeat('x', $i)]); }
$all = $pdo->query('SELECT a, length(b) FROM u WHERE a % 2 = 0')->fetchAll();
echo "#DONE $sum $rows $e ", count($all), "\n";
"##;

const EXPECTED: &str = "#DONE 111 5 it''s 50";

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

#[test]
fn every_failing_allocation_ends_cleanly() {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            php_memlimit::attach_thread();
            let sink = Sink::default();
            let opts = VmOptions {
                ini: vec![
                    ("display_errors".into(), "1".into()),
                    ("fatal_error_backtraces".into(), "0".into()),
                ],
            };
            let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL);
            // Warm-up run (lazy initialisation), then count
            // the script's allocations.
            vm.run_script(SCRIPT.as_bytes(), "/warm.php");
            let warm = sink.take();
            assert!(warm.contains(EXPECTED), "{warm}");
            vm.request_reset();
            let before = php_memlimit::current().stats().allocs;
            vm.run_script(SCRIPT.as_bytes(), "/count.php");
            let total = php_memlimit::current().stats().allocs - before;
            sink.take();
            vm.request_reset();

            let step = std::env::var("C_HEAP_FAULTS_STEP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or((total / 400).max(1));
            let mut fatals = 0;
            for n in (1..=total + step).step_by(step) {
                php_memlimit::set_max_allocs_per_request(n as u64);
                vm.run_script(SCRIPT.as_bytes(), "/case.php");
                let out = sink.take();
                php_memlimit::set_max_allocs_per_request(0);
                vm.request_reset();
                let fatal = out.contains("Allowed memory size of");
                assert!(
                    fatal || out.contains(EXPECTED),
                    "allocation {n} of {total}: {out}"
                );
                fatals += usize::from(fatal);
                vm.run_script(b"<?php echo 'next ok';", "/next.php");
                let next = sink.take();
                vm.request_reset();
                assert_eq!(next, "next ok", "after allocation {n}");
            }
            assert!(fatals > 0, "no allocation failed (total {total})");
            println!("{total} allocations, every {step}th failed in turn, {fatals} fatals");
        })
        .expect("spawn")
        .join()
        .expect("worker");
}
