// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! libcurl runs on php-memlimit's C hooks (`curl_global_init_mem`), which
//! return NULL past the limit. Plan 3e (V4) counts a library only where
//! NULL is handled cleanly: the n-th allocation of a curl script (HTTP to
//! a local server, `file://`, curl_multi) fails, for n swept over the
//! whole script. Every run must end either normally or with the memory
//! fatal, and the next request on the same VM must work. A crash or a hang
//! ends the test binary (or its timeout).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::rc::Rc;

use php_vm::{Vm, VmOptions};

php_memlimit::install_global_allocator!();

const EXPECTED: &str = "#DONE 200 4096 1 4096 2";

/// A local HTTP/1.1 server answering every request with 4096 bytes and
/// closing the connection.
fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let body = "x".repeat(4096);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Test: a\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

fn script(port: u16, file: &str) -> String {
    format!(
        r##"<?php
$h = curl_init("http://127.0.0.1:{port}/a?q=1");
curl_setopt_array($h, [CURLOPT_RETURNTRANSFER => true, CURLOPT_HTTPHEADER => ['X-A: 1', 'X-B: 2'],
    CURLOPT_USERAGENT => 'faults', CURLOPT_HEADERFUNCTION => fn($c, $l) => strlen($l)]);
$body = curl_exec($h);
$code = curl_getinfo($h, CURLINFO_RESPONSE_CODE);
$f = curl_init('file://{file}');
curl_setopt($f, CURLOPT_RETURNTRANSFER, true);
$fb = curl_exec($f);
$m = curl_multi_init();
$hs = [];
foreach ([1, 2] as $i) {{
    $hs[$i] = curl_init("http://127.0.0.1:{port}/m$i");
    curl_setopt($hs[$i], CURLOPT_RETURNTRANSFER, true);
    curl_multi_add_handle($m, $hs[$i]);
}}
do {{ $st = curl_multi_exec($m, $running); if ($running) curl_multi_select($m, 1.0); }} while ($running && $st == CURLM_OK);
$ok = 0;
foreach ($hs as $x) {{ $ok += strlen((string) curl_multi_getcontent($x)) === 4096; curl_multi_remove_handle($m, $x); }}
echo "#DONE $code ", strlen((string) $body), ' ', (int) ($fb === str_repeat('y', 4096)) , ' ', strlen((string) $body), " $ok\n";
"##
    )
}

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
    let port = serve();
    let (mut tmp, path) =
        php_streams::temp::open_temporary_fd(None, Some(b"curl-faults"), 0).expect("temp file");
    tmp.write_all("y".repeat(4096).as_bytes())
        .expect("write temp file");
    drop(tmp);
    let file = std::path::PathBuf::from(String::from_utf8(path).expect("utf-8 path"));
    let src = script(port, &file.to_string_lossy());
    let only: Option<usize> = std::env::var("C_HEAP_FAULTS_ONLY")
        .ok()
        .and_then(|v| v.parse().ok());
    let trace = std::env::var_os("C_HEAP_FAULTS_TRACE").is_some();
    let result = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            php_memlimit::attach_thread();
            let sink = Sink::default();
            let opts = VmOptions {
                ini: vec![
                    ("display_errors".into(), "1".into()),
                    ("fatal_error_backtraces".into(), "0".into()),
                ],
            };
            let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), php_extensions::ALL);
            // Warm-up run (global init, lazy loading), then count the
            // script's allocations.
            vm.run_script(src.as_bytes(), "/warm.php");
            let warm = sink.take();
            assert!(warm.contains(EXPECTED), "{warm}");
            vm.request_reset();
            let before = php_memlimit::current().stats().allocs;
            vm.run_script(src.as_bytes(), "/count.php");
            let total = php_memlimit::current().stats().allocs - before;
            sink.take();
            vm.request_reset();

            let step = (total / 400).max(1);
            let mut fatals = 0;
            for n in (1..=total + step).step_by(step) {
                if only.is_some_and(|o| o != n) {
                    continue;
                }
                if trace {
                    eprintln!("allocation {n} of {total}");
                }
                php_memlimit::set_max_allocs_per_request(n as u64);
                vm.run_script(src.as_bytes(), "/case.php");
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
        .join();
    let _ = std::fs::remove_file(&file);
    result.expect("worker");
}
