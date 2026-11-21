// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end test: starts `target/debug/php -S` on a free port and talks
//! HTTP to it.  Ignored by default (needs `cargo build -p php-cli`):
//!
//! ```sh
//! cargo build -p php-cli
//! cargo test -p php-cli-server --test server -- --ignored
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn php_binary() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::var_os("TEST_PHP_EXECUTABLE")
        .map_or_else(|| root.join("target/debug/php"), PathBuf::from)
}

struct ServerProc {
    child: Child,
    port: u16,
    root: PathBuf,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn start(name: &str, files: &[(&str, &str)], router: Option<&str>) -> ServerProc {
    let root =
        std::env::temp_dir().join(format!("php-cli-server-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (f, content) in files {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    let mut cmd = Command::new(php_binary());
    cmd.args(["-n", "-t"])
        .arg(&root)
        .args(["-S", "127.0.0.1:0"])
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(r) = router {
        cmd.arg(r);
    }
    let mut child = cmd.spawn().expect("spawn php -S");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let banner = lines.next().unwrap().unwrap();
    let port: u16 = banner
        .rsplit(':')
        .next()
        .and_then(|s| s.strip_suffix(") started"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad banner {banner}"));
    // Drain the log in the background.
    std::thread::spawn(move || for _ in lines {});
    ServerProc { child, port, root }
}

fn request(port: u16, raw: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut s = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

fn split(resp: &str) -> (&str, &str) {
    resp.split_once("\r\n\r\n").unwrap_or((resp, ""))
}

#[test]
#[ignore = "needs target/debug/php"]
fn scripts_static_files_and_errors() {
    let srv = start(
        "basic",
        &[
            (
                "index.php",
                "<?php header('X-Test: yes'); setcookie('c', 'v'); echo $_SERVER['REQUEST_METHOD'], ' ', $_GET['a'] ?? '-', ' ', $_POST['p'] ?? '-', ' ', $_COOKIE['k'] ?? '-', ' ', $_SERVER['PATH_INFO'] ?? '-';",
            ),
            ("status.php", "<?php http_response_code(418); echo 'tea';"),
            ("hello.txt", "hello\n"),
            ("style.css", "a{}"),
        ],
        None,
    );
    let r = request(
        srv.port,
        "GET /?a=1 HTTP/1.1\r\nHost: x\r\nCookie: k=v\r\n\r\n",
    );
    let (head, body) = split(&r);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\nDate: "), "{head}");
    assert!(head.contains("\r\nConnection: close\r\nX-Powered-By: PHP/"));
    assert!(head
        .contains("\r\nX-Test: yes\r\nSet-Cookie: c=v\r\nContent-type: text/html; charset=UTF-8"));
    assert_eq!(body, "GET 1 - v -");

    let r = request(
        srv.port,
        "POST /index.php/extra/path HTTP/1.0\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 3\r\n\r\np=2",
    );
    let (head, body) = split(&r);
    assert!(head.starts_with("HTTP/1.0 200 OK"), "{head}");
    assert_eq!(body, "POST - 2 - /extra/path");

    let r = request(srv.port, "GET /status.php HTTP/1.1\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 418 I'm a teapot") || r.starts_with("HTTP/1.1 418 Unknown"),
        "{r}"
    );
    assert!(r.ends_with("tea"));

    let r = request(srv.port, "GET /hello.txt HTTP/1.1\r\n\r\n");
    let (head, body) = split(&r);
    assert!(
        head.contains("Content-Type: text/plain; charset=UTF-8\r\nContent-Length: 6"),
        "{head}"
    );
    assert_eq!(body, "hello\n");

    let r = request(srv.port, "HEAD /style.css HTTP/1.1\r\n\r\n");
    let (head, body) = split(&r);
    assert!(head.contains("Content-Type: text/css; charset=UTF-8\r\nContent-Length: 3"));
    assert_eq!(body, "");

    let r = request(srv.port, "DELETE /hello.txt HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 405 Method Not Allowed"));
    assert!(r.contains("Allow: GET, HEAD, POST\r\n"));

    let r = request(srv.port, "SMURF / HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 501 Not Implemented"));

    // Falls back to index.php with PATH_INFO.
    let r = request(srv.port, "GET /no/such HTTP/1.1\r\n\r\n");
    assert!(split(&r).1.ends_with("/no/such"), "{r}");

    // Malformed request: the connection is closed without a response.
    let r = request(srv.port, "garbage\r\n\r\n");
    assert_eq!(r, "");
}

#[test]
#[ignore = "needs target/debug/php"]
fn router_and_404() {
    let srv = start(
        "router",
        &[
            (
                "router.php",
                "<?php if (str_ends_with($_SERVER['REQUEST_URI'], '.txt')) return false; echo 'routed ', $_SERVER['SCRIPT_NAME'];",
            ),
            ("file.txt", "static"),
            ("page.php", "<?php echo 'page';"),
        ],
        Some("router.php"),
    );
    let r = request(srv.port, "GET /anything HTTP/1.1\r\n\r\n");
    assert_eq!(split(&r).1, "routed /anything");
    let r = request(srv.port, "GET /file.txt HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(split(&r).1, "static");
    let r = request(srv.port, "GET /missing.txt HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 404 Not Found"), "{r}");
    assert!(r.contains("<code class=\"url\">/missing.txt</code>"));
    let r = request(srv.port, "GET /page.php HTTP/1.1\r\n\r\n");
    assert_eq!(split(&r).1, "routed /page.php");
}

#[test]
#[ignore = "needs target/debug/php"]
fn idle_connection_does_not_block_others() {
    let srv = start("idle", &[("index.php", "<?php echo 'ok';")], None);
    let idle = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    let r = request(srv.port, "GET / HTTP/1.1\r\n\r\n");
    assert_eq!(split(&r).1, "ok");
    drop(idle);
    // Chunked request body.
    let r = request(
        srv.port,
        "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    );
    assert_eq!(split(&r).1, "ok");
}

/// A client that hangs up in the middle of a response ends that script
/// (`php_handle_aborted_connection`, without `ignore_user_abort`): its
/// shutdown function sees `connection_status()` 1, the rest of its loop
/// does not run, and the next request is served at once instead of after
/// the script's three seconds.
#[test]
#[ignore = "needs target/debug/php"]
fn a_client_that_hangs_up_frees_the_server() {
    const LONG: &str = r#"<?php
register_shutdown_function(function () {
    file_put_contents(__DIR__ . '/status.txt', 'shutdown ' . connection_status() . "\n", FILE_APPEND);
});
$t = microtime(true);
while (microtime(true) - $t < 3) { echo str_repeat('x', 8192), "\n"; flush(); usleep(1000); }
file_put_contents(__DIR__ . '/status.txt', "loop finished\n", FILE_APPEND);
"#;
    let srv = start(
        "hangup",
        &[("long.php", LONG), ("quick.php", "<?php echo 'ok';")],
        None,
    );
    let mut client = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    client
        .write_all(b"GET /long.php HTTP/1.0\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut some = [0u8; 2000];
    client.read_exact(&mut some).unwrap();
    drop(client);
    let started = Instant::now();
    let r = request(srv.port, "GET /quick.php HTTP/1.0\r\n\r\n");
    let waited = started.elapsed();
    assert_eq!(split(&r).1, "ok");
    assert!(
        waited < Duration::from_secs(2),
        "next request waited {waited:?}"
    );
    let status = std::fs::read_to_string(srv.root.join("status.txt")).unwrap_or_default();
    assert_eq!(status, "shutdown 1\n");
}

/// The start-up path of `ext/curl/tests/server.inc`: a relative router
/// (`.inc`), stderr closed by the parent right after the banner, then an
/// immediate connect probe and a request.
#[test]
#[ignore = "needs target/debug/php"]
fn curl_server_inc_startup() {
    let root = std::env::temp_dir().join(format!("php-cli-server-it-curl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("responder")).unwrap();
    std::fs::write(
        root.join("responder/get.inc"),
        "<?php header('Content-Type: text/plain'); echo 'Hello World!', $_GET['x'] ?? '';",
    )
    .unwrap();
    let mut child = Command::new(php_binary())
        .arg("-t")
        .arg(&root)
        .args(["-n", "-S", "localhost:0", "responder/get.inc"])
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let banner = lines.next().unwrap().unwrap();
    drop(lines); // closes the pipe: later log lines must not kill the server
    let port: u16 = banner
        .rsplit(':')
        .next()
        .and_then(|s| s.strip_suffix(") started"))
        .and_then(|s| s.parse().ok())
        .unwrap();
    // Connect probe, closed without a request.
    drop(TcpStream::connect(("localhost", port)).unwrap());
    let r = request(port, "GET /?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{r:?}");
    assert!(
        r.contains("Content-type: text/plain;charset=UTF-8\r\n"),
        "{r}"
    );
    assert!(r.ends_with("\r\n\r\nHello World!1"), "{r}");
    // Repeated requests stay fast once the server is up.
    let t = Instant::now();
    for _ in 0..5 {
        let r = request(port, "GET / HTTP/1.1\r\n\r\n");
        assert!(r.ends_with("Hello World!"), "{r}");
    }
    eprintln!(
        "startup+first request {:?}, 5 requests {:?}",
        started.elapsed(),
        t.elapsed()
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// The banner reaches a piped stderr within 3 s (idle box), and the
/// console log keeps working afterwards: module startup must not close
/// descriptors 0-2 (the CLI's STDIN/STDOUT/STDERR resources own them).
#[test]
#[ignore = "needs target/debug/php"]
fn banner_on_pipes_and_log_survives_startup() {
    let root =
        std::env::temp_dir().join(format!("php-cli-server-it-banner-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.php"), "<?php echo 'up';").unwrap();
    let mut child = Command::new(php_binary())
        .arg("-n")
        .arg("-t")
        .arg(&root)
        .args(["-S", "127.0.0.1:0"])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let limit = std::env::var("PHP_CLI_SERVER_BANNER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let banner = rx
        .recv_timeout(Duration::from_secs(limit))
        .expect("no startup banner on stderr");
    assert!(
        banner.contains("Development Server (http://127.0.0.1:"),
        "{banner}"
    );
    let port: u16 = banner
        .rsplit(':')
        .next()
        .and_then(|s| s.strip_suffix(") started"))
        .and_then(|s| s.parse().ok())
        .unwrap();
    let r = request(port, "GET / HTTP/1.1\r\n\r\n");
    assert!(r.ends_with("up"), "{r}");
    let accepted = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("no log line");
    assert!(accepted.ends_with(" Accepted"), "{accepted}");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
}
