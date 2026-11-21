// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Full request lifecycles against an in-memory backend (no httpd): the
//! VM runs real scripts and the test inspects status, headers, body and
//! log entries the SAPI produced.

use phprust_apache2::backend::{
    Backend, DirEntry, DirStatus, IntField, LogLevel, MockBackend, StrField, Table,
};
use phprust_apache2::config::ProcessConfig;
use phprust_apache2::request::{self, RunParams, Timer};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// What a request produced.
#[derive(Debug)]
struct Response {
    status: Option<(i32, Option<Vec<u8>>, i32)>,
    content_type: Option<String>,
    headers_out: Vec<(String, String)>,
    body: String,
    logs: Vec<(LogLevel, String)>,
    /// `ap_rflush` calls.
    flushes: usize,
}

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("phprust-apache2-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Request description.
#[derive(Default, Clone)]
struct Req {
    script: &'static str,
    method: &'static str,
    query: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
    dir: Vec<(&'static str, &'static str, bool)>,
    ini: Vec<(String, String)>,
    /// The client is gone before the script runs: every write fails.
    disconnected: bool,
}

fn ini(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn run_sequence(reqs: Vec<(&str, Req)>) -> Vec<Response> {
    let dir = scratch();
    let owned: Vec<(String, Req)> = reqs.into_iter().map(|(n, r)| (n.to_string(), r)).collect();
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut responses = Vec::new();
            for (name, req) in owned {
                let path = dir.join(&name);
                std::fs::write(&path, req.script).unwrap();
                let path_s = path.to_string_lossy().into_owned();
                let mut m = MockBackend::default()
                    .with_str(StrField::Handler, b"application/x-httpd-php")
                    .with_str(StrField::Filename, path_s.as_bytes())
                    .with_str(
                        StrField::Uri,
                        format!(
                            "/{}",
                            Path::new(&path_s).file_name().unwrap().to_string_lossy()
                        )
                        .as_bytes(),
                    )
                    .with_str(
                        StrField::Method,
                        if req.method.is_empty() {
                            b"GET"
                        } else {
                            req.method.as_bytes()
                        },
                    )
                    .with_int(IntField::FileType, 1)
                    .with_int(IntField::ProtoNum, 1001)
                    .with_int(IntField::RequestTime, 1_700_000_000_250_000)
                    .with_int(IntField::IsMain, 1)
                    .with_entry(Table::SubprocessEnv, b"SCRIPT_FILENAME", path_s.as_bytes())
                    .with_entry(
                        Table::SubprocessEnv,
                        b"REQUEST_METHOD",
                        if req.method.is_empty() {
                            b"GET"
                        } else {
                            req.method.as_bytes()
                        },
                    );
                if !req.query.is_empty() {
                    m = m.with_str(StrField::Args, req.query.as_bytes());
                }
                for (k, v) in &req.headers {
                    m = m.with_entry(Table::HeadersIn, k.as_bytes(), v.as_bytes());
                }
                m.body_in.borrow_mut().extend_from_slice(&req.body);
                *m.disconnected.borrow_mut() = req.disconnected;
                for (n, v, admin) in &req.dir {
                    m.dir.borrow_mut().push(DirEntry {
                        name: (*n).to_string(),
                        value: v.as_bytes().to_vec(),
                        status: if *admin {
                            DirStatus::System
                        } else {
                            DirStatus::PerDir
                        },
                        htaccess: false,
                    });
                }
                let shared = Rc::new(m);
                let mut config = ProcessConfig::with_defaults();
                for (k, v) in &req.ini {
                    config.ini.push((k.clone(), v.clone()));
                }
                let params = RunParams {
                    config: Arc::new(config),
                    timer: Arc::new(Timer::default()),
                    highlight: false,
                };
                request::run(Box::new(shared.clone()) as Box<dyn Backend>, &params);
                let s = |v: &[u8]| String::from_utf8_lossy(v).into_owned();
                let resp = Response {
                    status: shared.status.borrow().clone(),
                    content_type: shared.content_type.borrow().as_deref().map(s),
                    headers_out: shared
                        .table_entries(Table::HeadersOut)
                        .iter()
                        .map(|(k, v)| (s(k), s(v)))
                        .collect(),
                    body: s(&shared.body_out.borrow()),
                    logs: shared
                        .logs
                        .borrow()
                        .iter()
                        .map(|(l, m)| (*l, s(m)))
                        .collect(),
                    flushes: *shared.flushes.borrow(),
                };
                responses.push(resp);
            }
            responses
        })
        .unwrap()
        .join()
        .unwrap()
}

fn run(name: &str, req: Req) -> Response {
    run_sequence(vec![(name, req)]).pop().unwrap()
}

#[test]
fn hello_world_defaults() {
    let r = run(
        "hello.php",
        Req {
            script: "<?php echo 'Hi ', PHP_SAPI, ' ', php_sapi_name();",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "Hi apache2handler apache2handler");
    assert_eq!(r.status, Some((200, None, 0)));
    assert_eq!(r.content_type.as_deref(), Some("text/html; charset=UTF-8"));
    assert_eq!(
        r.headers_out,
        vec![(
            "X-Powered-By".to_string(),
            format!("PHP/{}", php_vm::PHP_VERSION)
        )]
    );
}

#[test]
fn no_output_still_sends_headers() {
    let r = run(
        "empty.php",
        Req {
            script: "<?php http_response_code(204);",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert_eq!(r.status, Some((204, None, 0)));
}

#[test]
fn headers_and_cookies() {
    let r = run(
        "headers.php",
        Req {
            script: "<?php header('X-A: 1'); header('X-A: 2', false); header('X-B: gone'); header_remove('X-B');\n\
                     header('Content-Type: text/plain'); setcookie('n', 'v w'); setrawcookie('raw', 'x', ['path' => '/p', 'samesite' => 'Lax']);\n\
                     echo implode('|', headers_list());",
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        format!(
            "X-Powered-By: PHP/{}|X-A: 1|X-A: 2|Content-type: text/plain;charset=UTF-8|Set-Cookie: n=v%20w|Set-Cookie: raw=x; path=/p; SameSite=Lax",
            php_vm::PHP_VERSION
        )
    );
    assert_eq!(r.content_type.as_deref(), Some("text/plain;charset=UTF-8"));
    let names: Vec<&str> = r.headers_out.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec!["X-Powered-By", "X-A", "X-A", "Set-Cookie", "Set-Cookie"]
    );
}

#[test]
fn status_line_and_redirects() {
    let r = run(
        "nf.php",
        Req {
            script: "<?php header('HTTP/1.1 404 Not Found'); echo 'x';",
            ..Req::default()
        },
    );
    assert_eq!(r.status, Some((404, Some(b"404 Not Found".to_vec()), 1001)));
    let r = run(
        "loc.php",
        Req {
            script: "<?php header('Location: /there');",
            method: "POST",
            ..Req::default()
        },
    );
    assert_eq!(r.status.map(|s| s.0), Some(303));
    assert!(r
        .headers_out
        .contains(&("Location".to_string(), "/there".to_string())));
}

#[test]
fn late_header_warns_with_where_output_started() {
    let r = run(
        "late.php",
        Req {
            script: "<?php echo 'a';\nheader('X: y'); var_dump(headers_sent($f, $l)); echo basename($f), ':', $l;",
            ..Req::default()
        },
    );
    let path = scratch().join("late.php").display().to_string();
    assert_eq!(
        r.body,
        format!(
            "a<br />\n<b>Warning</b>:  Cannot modify header information - headers already sent by (output started at {path}:1) in <b>{path}</b> on line <b>2</b><br />\nbool(true)\nlate.php:1"
        )
    );
}

#[test]
fn response_code_after_output_names_where_it_started() {
    let r = run(
        "code.php",
        Req {
            script: "<?php echo 'x';\nvar_dump(http_response_code(404), http_response_code());",
            ..Req::default()
        },
    );
    let path = scratch().join("code.php").display().to_string();
    assert_eq!(
        r.body,
        format!(
            "x<br />\n<b>Warning</b>:  http_response_code(): Cannot set response code - headers already sent (output started at {path}:1) in <b>{path}</b> on line <b>2</b><br />\nbool(false)\nint(200)\n"
        )
    );
}

#[test]
fn flush_without_output_sends_headers_without_a_location() {
    let r = run(
        "flush.php",
        Req {
            script: "<?php flush(); var_dump(headers_sent($f, $l), $f, $l); header('X: y');",
            ..Req::default()
        },
    );
    assert!(
        r.body.starts_with(
            "bool(true)\nstring(0) \"\"\nint(0)\n<br />\n<b>Warning</b>:  Cannot modify header information - headers already sent in <b>"
        ),
        "{}",
        r.body
    );
    assert_eq!(r.flushes, 1);
}

#[test]
fn header_callback_runs_before_the_first_output() {
    let r = run(
        "cb.php",
        Req {
            script: "<?php header_register_callback(function () { header('X-CB: ' . (headers_sent() ? 'late' : 'early')); header_remove('X-Powered-By'); });\necho 'body'; header('X-After: 1');",
            ..Req::default()
        },
    );
    assert!(r.body.starts_with("body<br />"), "{}", r.body);
    assert_eq!(
        r.headers_out,
        vec![("X-CB".to_string(), "early".to_string())]
    );
}

#[test]
fn header_callback_output_sends_the_headers() {
    let r = run(
        "cbout.php",
        Req {
            script: "<?php header_register_callback(function () { echo 'cb:'; header('X-Too-Late: 1'); });\necho 'body';",
            ..Req::default()
        },
    );
    let path = scratch().join("cbout.php").display().to_string();
    // The callable's own output sends the headers (php-src's re-entrant
    // sapi_send_headers()); its later header() is too late.
    assert_eq!(
        r.body,
        format!(
            "cb:<br />\n<b>Warning</b>:  Cannot modify header information - headers already sent by (output started at {path}:2) in <b>{path}</b> on line <b>1</b><br />\nbody"
        )
    );
    assert!(!r.headers_out.iter().any(|(k, _)| k == "X-Too-Late"));
}

#[test]
fn header_callback_output_resets_the_content_type() {
    // The nested sapi_send_headers() applies text/plain; the outer one
    // calls php_apache_sapi_send_headers() again with ctx->content_type
    // consumed, so httpd gets the default type (php-src's behaviour).
    let r = run(
        "cbtype.php",
        Req {
            script: "<?php header('Content-Type: text/plain'); header('HTTP/1.1 404 Not Found');\nheader_register_callback(function () { echo 'cb:'; });\necho 'body'; echo implode('|', headers_list());",
            ..Req::default()
        },
    );
    assert!(r.body.starts_with("cb:body"), "{}", r.body);
    assert!(
        r.body.contains("Content-type: text/plain;charset=UTF-8"),
        "headers_list() keeps the script's header: {}",
        r.body
    );
    assert_eq!(r.content_type.as_deref(), Some("text/html; charset=UTF-8"));
    assert_eq!(r.status.map(|s| s.0), Some(404));
}

#[test]
fn header_callback_without_output_keeps_the_content_type() {
    let r = run(
        "cbtype2.php",
        Req {
            script: "<?php header('Content-Type: text/plain');\nheader_register_callback(function () { header('X-CB: 1'); });\necho 'body';",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "body");
    assert_eq!(r.content_type.as_deref(), Some("text/plain;charset=UTF-8"));
}

#[test]
fn head_request_header_callback_output_is_dropped() {
    let r = run(
        "cbhead.php",
        Req {
            script: "<?php header_register_callback(function () { echo 'cb'; });\necho 'body';",
            method: "HEAD",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert_eq!(r.status, Some((200, None, 0)));
}

#[test]
fn header_callback_runs_at_shutdown_without_output() {
    let r = run(
        "cbend.php",
        Req {
            script: "<?php header_register_callback(function () { header('X-Late: 1'); });",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert!(r
        .headers_out
        .contains(&("X-Late".to_string(), "1".to_string())));
}

#[test]
fn implicit_flush_flushes_after_every_write() {
    let r = run(
        "imp.php",
        Req {
            script: "<?php echo 'a'; echo 'b'; echo '';",
            ini: ini(&[("implicit_flush", "1")]),
            ..Req::default()
        },
    );
    assert_eq!(r.body, "ab");
    assert_eq!(r.flushes, 2);
    let r = run(
        "noimp.php",
        Req {
            script: "<?php echo 'a'; echo 'b';",
            ..Req::default()
        },
    );
    assert_eq!(r.flushes, 0, "no flush without implicit_flush");
    let r = run(
        "obimp.php",
        Req {
            script: "<?php ob_implicit_flush(); echo 'a'; ob_implicit_flush(false); echo 'b';",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "ab");
    assert_eq!(r.flushes, 1);
}

#[test]
fn connection_status_defaults() {
    let r = run(
        "cs.php",
        Req {
            script: "<?php var_dump(connection_status(), connection_aborted(), ignore_user_abort(true), ignore_user_abort(), ini_get('ignore_user_abort'));",
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        "int(0)\nbool(false)\nbool(false)\nbool(true)\nstring(1) \"1\"\n"
    );
}

// The bailout of a failed `ap_rwrite` alone goes through the engine's
// process-wide interrupt flag, which concurrently running VMs share: it is
// tested in tests/interrupts.rs.  With `implicit_flush` the failed flush
// that follows the write bails out with the VM at hand.
#[test]
fn implicit_flush_to_a_lost_client_bails_out_after_the_write() {
    let log = scratch().join("abort.log");
    let _ = std::fs::remove_file(&log);
    let r = run(
        "abort.php",
        Req {
            script: "<?php $f = __DIR__ . '/abort.log';\nregister_shutdown_function(function () use ($f) { file_put_contents($f, 'shutdown:' . connection_status() . ':' . var_export(connection_aborted(), true), FILE_APPEND); });\necho 'a';\nfile_put_contents($f, 'after;', FILE_APPEND);",
            disconnected: true,
            ini: ini(&[("implicit_flush", "1")]),
            ..Req::default()
        },
    );
    assert_eq!(r.body, "", "nothing reaches a gone client");
    assert_eq!(r.status.map(|s| s.0), Some(200));
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "shutdown:1:true",
        "php_handle_aborted_connection() bailed out before the next statement"
    );
}

#[test]
fn ignore_user_abort_keeps_running_without_output() {
    let log = scratch().join("ignore.log");
    let _ = std::fs::remove_file(&log);
    let r = run(
        "ignore.php",
        Req {
            script: "<?php $f = __DIR__ . '/ignore.log';\nregister_shutdown_function(function () use ($f) { file_put_contents($f, 'shutdown:' . connection_status(), FILE_APPEND); });\necho 'a';\nfile_put_contents($f, 'after;', FILE_APPEND); echo 'more'; flush();\nfile_put_contents($f, 'flushed;', FILE_APPEND);",
            disconnected: true,
            dir: vec![("ignore_user_abort", "1", false)],
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "after;flushed;shutdown:1"
    );
}

#[test]
fn flush_to_a_lost_client_bails_out() {
    let log = scratch().join("flushabort.log");
    let _ = std::fs::remove_file(&log);
    let r = run(
        "flushabort.php",
        Req {
            script:
                "<?php $f = __DIR__ . '/flushabort.log';\nflush();\nfile_put_contents($f, 'after');",
            disconnected: true,
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert!(!log.exists(), "the bailout happens inside flush()");
}

#[test]
fn head_requests_send_headers_without_a_body() {
    let r = run(
        "head.php",
        Req {
            script: "<?php header('X-H: 1'); echo 'hidden';",
            method: "HEAD",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert_eq!(r.status, Some((200, None, 0)));
    assert!(r
        .headers_out
        .contains(&("X-H".to_string(), "1".to_string())));
}

#[test]
fn session_upload_progress_is_tracked() {
    let save = scratch().join("sessions");
    std::fs::create_dir_all(&save).unwrap();
    let body = b"--B\r\nContent-Disposition: form-data; name=\"PHP_SESSION_UPLOAD_PROGRESS\"\r\n\r\nt\r\n--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nDATA\r\n--B--\r\n";
    let len = body.len().to_string();
    let len_header: &'static str = Box::leak(len.into_boxed_str());
    let save_path = save.display().to_string();
    let r = run(
        "progress.php",
        Req {
            script: "<?php session_id('upload1'); session_start();\n$p = $_SESSION['upload_progress_t'];\nvar_dump($p['done'], $p['bytes_processed'], $p['files'][0]['name'], $p['files'][0]['field_name'], $p['files'][0]['done'], $_POST['PHP_SESSION_UPLOAD_PROGRESS'], $_FILES['f']['error']);",
            method: "POST",
            headers: vec![
                ("Content-Type", "multipart/form-data; boundary=B"),
                ("Content-Length", len_header),
                ("Cookie", "PHPSESSID=upload1"),
            ],
            body: body.to_vec(),
            ini: ini(&[
                ("session.save_path", save_path.as_str()),
                ("session.use_strict_mode", "0"),
                ("session.upload_progress.cleanup", "0"),
                ("session.upload_progress.freq", "0"),
                ("session.upload_progress.min_freq", "0"),
            ]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        format!(
            "bool(true)\nint({})\nstring(5) \"a.txt\"\nstring(1) \"f\"\nbool(true)\nstring(1) \"t\"\nint(0)\n",
            body.len()
        )
    );
}

#[test]
fn request_variables() {
    let r = run(
        "vars.php",
        Req {
            script: "<?php echo json_encode([$_GET, $_POST, $_COOKIE, $_REQUEST, file_get_contents('php://input'), $_SERVER['PHP_SELF'], $_SERVER['REQUEST_TIME'], isset($_SERVER['argv'])]);",
            method: "POST",
            query: "g=1&a[]=x",
            headers: vec![
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Content-Length", "11"),
                ("Cookie", "c=1; d=%20"),
            ],
            body: b"p=2&a[]=yyy".to_vec(),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"[{"g":"1","a":["x"]},{"p":"2","a":["yyy"]},{"c":"1","d":" "},{"g":"1","a":["yyy"],"p":"2","c":"1","d":" "},"p=2&a[]=yyy","\/vars.php",1700000000,false]"#
    );
}

#[test]
fn php_input_for_other_methods() {
    let r = run(
        "put.php",
        Req {
            script: "<?php var_dump(file_get_contents('php://input'), $_POST);",
            method: "PUT",
            body: b"{\"a\":1}".to_vec(),
            ..Req::default()
        },
    );
    assert_eq!(r.body, "string(7) \"{\"a\":1}\"\narray(0) {\n}\n");
}

#[test]
fn basic_auth() {
    let r = run(
        "auth.php",
        Req {
            script: "<?php echo $_SERVER['PHP_AUTH_USER'], ':', $_SERVER['PHP_AUTH_PW'];",
            headers: vec![("Authorization", "Basic dXNlcjpwYXNz")],
            ..Req::default()
        },
    );
    assert_eq!(r.body, "user:pass");
}

#[test]
fn multipart_upload() {
    let body = b"--B\r\nContent-Disposition: form-data; name=\"t\"\r\n\r\nv\r\n--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nDATA\r\n--B--\r\n";
    let r = run(
        "up.php",
        Req {
            script: "<?php $f = $_FILES['f']; echo $_POST['t'], '|', $f['name'], '|', $f['type'], '|', $f['size'], '|', $f['error'], '|', file_get_contents($f['tmp_name']), '|', var_export(is_uploaded_file($f['tmp_name']), true), '|', strlen(file_get_contents('php://input')), '|', $f['tmp_name'];",
            method: "POST",
            headers: vec![
                ("Content-Type", "multipart/form-data; boundary=B"),
                ("Content-Length", "148"),
            ],
            body: body.to_vec(),
            ..Req::default()
        },
    );
    let parts: Vec<&str> = r.body.split('|').collect();
    assert_eq!(
        &parts[..8],
        &["v", "a.txt", "text/plain", "4", "0", "DATA", "true", "0"],
        "{}",
        r.body
    );
    assert!(
        !Path::new(parts[8]).exists(),
        "temporary upload must be removed"
    );
}

#[test]
fn upload_error_constants_are_core_constants() {
    let r = run(
        "uperr.php",
        Req {
            script: "<?php var_dump(UPLOAD_ERR_OK, UPLOAD_ERR_EXTENSION, get_defined_constants(true)['Core']['UPLOAD_ERR_NO_TMP_DIR'] ?? null);",
            ..Req::default()
        },
    );
    assert_eq!(r.body, "int(0)\nint(8)\nint(6)\n");
}

#[test]
fn post_max_size_warning() {
    let r = run(
        "big.php",
        Req {
            script: "<?php var_dump($_POST);",
            method: "POST",
            headers: vec![
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Content-Length", "20"),
            ],
            body: b"a=0123456789abcdefgh".to_vec(),
            dir: vec![("post_max_size", "10", false)],
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        "<br />\n<b>Warning</b>:  PHP Request Startup: POST Content-Length of 20 bytes exceeds the limit of 10 bytes in <b>Unknown</b> on line <b>0</b><br />\narray(0) {\n}\n"
    );
}

#[test]
fn per_dir_and_admin_values() {
    let r = run(
        "ini.php",
        Req {
            script: "<?php var_dump(ini_get('precision'), ini_set('precision', '4'), ini_get('memory_limit'), ini_set('memory_limit', '64M'), set_time_limit(3));",
            dir: vec![("precision", "5", true), ("memory_limit", "100M", false), ("max_execution_time", "9", true)],
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        "string(1) \"5\"\nbool(false)\nstring(4) \"100M\"\nstring(4) \"100M\"\nbool(false)\n"
    );
}

#[test]
fn fatal_without_display_is_500() {
    let r = run(
        "fatal.php",
        Req {
            script: "<?php nope();",
            dir: vec![("display_errors", "0", false)],
            ..Req::default()
        },
    );
    assert_eq!(r.body, "");
    assert_eq!(
        r.status,
        Some((500, Some(b"500 Internal Server Error".to_vec()), 1000))
    );
}

#[test]
fn error_log_goes_to_httpd() {
    let r = run(
        "log.php",
        Req {
            script: "<?php error_log('hello'); error_log('sapi', 4);",
            ..Req::default()
        },
    );
    assert_eq!(
        r.logs,
        vec![
            (LogLevel::Notice, "hello".to_string()),
            (LogLevel::Err, "sapi".to_string())
        ]
    );
}

#[test]
fn display_errors_html() {
    let r = run(
        "warn.php",
        Req {
            script: "<?php\necho $x;",
            ..Req::default()
        },
    );
    assert!(
        r.body
            .starts_with("<br />\n<b>Warning</b>:  Undefined variable $x in <b>"),
        "{}",
        r.body
    );
    assert!(
        r.body.ends_with("warn.php</b> on line <b>2</b><br />\n"),
        "{}",
        r.body
    );
}

#[test]
fn cwd_is_script_dir() {
    let r = run(
        "cwd.php",
        Req {
            script: "<?php echo getcwd();",
            ..Req::default()
        },
    );
    assert_eq!(r.body, scratch().canonicalize().unwrap().to_string_lossy());
}

/// A `&'static str` for [`Req::headers`].
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `Content-Type` and `Content-Length` request headers for `body`.
fn body_headers(content_type: &str, body: &[u8]) -> Vec<(&'static str, &'static str)> {
    vec![
        ("Content-Type", leak(content_type.to_string())),
        ("Content-Length", leak(body.len().to_string())),
    ]
}

/// The boundary of the multipart bodies below (php-src's
/// `request_parse_body` tests use it).
const BOUNDARY: &str = "---------------------------84000087610663814162942123332";

/// A `multipart/form-data` body (CRLF line ends) of `(headers, content)`
/// parts.
fn multipart(parts: &[(&[&str], &str)]) -> Vec<u8> {
    let mut out = String::new();
    for (headers, content) in parts {
        out.push_str("--");
        out.push_str(BOUNDARY);
        out.push_str("\r\n");
        for h in *headers {
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.push_str(content);
        out.push_str("\r\n");
    }
    out.push_str("--");
    out.push_str(BOUNDARY);
    out.push_str("--");
    out.into_bytes()
}

// request_parse_body() (ext/standard/http.c) through mod_php: every
// expected output is what php 8.5's `php -S` answers for the same request
// (both SAPIs read the body through `sapi_module.read_post`).

/// `request_parse_body()`: A PUT urlencoded body (php-src `request_parse_body/urlencoded.phpt`): `$_POST` stays empty and `php://input` still reads the body.
#[test]
fn request_parse_body_urlencoded() {
    let body = b"foo=foo&bar[]=1&bar[]=2".to_vec();
    let r = run(
        "rpb_urlencoded.php",
        Req {
            script: r"<?php
var_dump(request_parse_body(), $_POST, file_get_contents('php://input'));
",
            method: "PUT",
            headers: body_headers("application/x-www-form-urlencoded", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"array(2) {
  [0]=>
  array(2) {
    ["foo"]=>
    string(3) "foo"
    ["bar"]=>
    array(2) {
      [0]=>
      string(1) "1"
      [1]=>
      string(1) "2"
    }
  }
  [1]=>
  array(0) {
  }
}
array(0) {
}
string(23) "foo=foo&bar[]=1&bar[]=2"
"#
    );
}

/// `request_parse_body()`: A PATCH multipart body: the upload is a real uploaded file; `$_FILES` and `php://input` stay empty.
#[test]
fn request_parse_body_multipart() {
    let body = multipart(&[
        (
            &["Content-Disposition: form-data; name=\"field1\""],
            "value one",
        ),
        (
            &[
                "Content-Disposition: form-data; name=\"file1\"; filename=\"a.txt\"",
                "Content-Type: text/plain",
            ],
            "file one data",
        ),
    ]);
    let r = run(
        "rpb_multipart.php",
        Req {
            script: r#"<?php
[$post, $files] = request_parse_body();
$f = $files['file1'];
echo json_encode([$post, $f['name'], $f['full_path'], $f['type'], $f['error'], $f['size'], is_uploaded_file($f['tmp_name']), file_get_contents($f['tmp_name']), $_POST, $_FILES, file_get_contents('php://input')]), "\n";
"#,
            method: "PATCH",
            headers: body_headers("multipart/form-data; boundary=---------------------------84000087610663814162942123332", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"[{"field1":"value one"},"a.txt","a.txt","text\/plain",0,13,true,"file one data",[],[],""]
"#
    );
}

/// `request_parse_body()`: No `Content-Type`: nothing to parse.
#[test]
fn request_parse_body_no_content_type() {
    let body = b"foo=bar".to_vec();
    let r = run(
        "rpb_no_content_type.php",
        Req {
            script: r#"<?php
try { request_parse_body(); } catch (RequestParseBodyException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
"#,
            method: "PUT",
            headers: vec![("Content-Length", leak(body.len().to_string()))],
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r"RequestParseBodyException: Request does not provide a content type
"
    );
}

/// `request_parse_body()`: A content type without a POST handler throws; the body is left for `php://input`.
#[test]
fn request_parse_body_unsupported_content_type() {
    let body = b"{\"a\":1}".to_vec();
    let r = run(
        "rpb_unsupported_content_type.php",
        Req {
            script: r#"<?php
try { request_parse_body(); } catch (RequestParseBodyException $e) { echo $e->getMessage(), "\n"; }
echo file_get_contents('php://input');
"#,
            method: "PUT",
            headers: body_headers("application/json", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"Content-Type "application/json" is not supported
{"a":1}"#
    );
}

/// `request_parse_body()`: The rfc1867 limits throw, chained in the order they were hit (the option values replace the INI ones).
#[test]
fn request_parse_body_chained_limits() {
    let body = multipart(&[
        (
            &["Content-Disposition: form-data; name=\"field1\""],
            "value one",
        ),
        (
            &["Content-Disposition: form-data; name=\"field2\""],
            "value two",
        ),
        (
            &[
                "Content-Disposition: form-data; name=\"file1\"; filename=\"a.txt\"",
                "Content-Type: text/plain",
            ],
            "file one data",
        ),
        (
            &[
                "Content-Disposition: form-data; name=\"file2\"; filename=\"b.txt\"",
                "Content-Type: text/plain",
            ],
            "file two data",
        ),
    ]);
    let r = run(
        "rpb_chained_limits.php",
        Req {
            script: r#"<?php
try { request_parse_body(['max_file_uploads' => 1, 'max_input_vars' => 1]); } catch (RequestParseBodyException $e) { echo $e->getMessage(), ' <- ', $e->getPrevious()->getMessage(), "\n"; }
"#,
            method: "PUT",
            headers: body_headers("multipart/form-data; boundary=---------------------------84000087610663814162942123332", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r"Multipart body parts limit exceeded 2. To increase the limit change max_multipart_body_parts in php.ini. <- Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
"
    );
}

/// `request_parse_body()`: The `post_max_size` option against `Content-Length`; the body is not read, so a second call parses it.
#[test]
fn request_parse_body_post_max_size_option() {
    let body = multipart(&[(
        &["Content-Disposition: form-data; name=\"field1\""],
        "value one",
    )]);
    let r = run(
        "rpb_post_max_size_option.php",
        Req {
            script: r#"<?php
try { request_parse_body(['post_max_size' => 10]); } catch (RequestParseBodyException $e) { echo $e->getMessage(), "\n"; }
var_dump(request_parse_body()[0]);
"#,
            method: "DELETE",
            headers: body_headers("multipart/form-data; boundary=---------------------------84000087610663814162942123332", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"POST Content-Length of 180 bytes exceeds the limit of 10 bytes
array(1) {
  ["field1"]=>
  string(9) "value one"
}
"#
    );
}

/// `request_parse_body()`: The startup POST parse consumed the body: nothing left, and the new (empty) buffer is what `php://input` reads.
#[test]
fn request_parse_body_after_startup_post() {
    let body = b"a=1&b=2".to_vec();
    let r = run(
        "rpb_after_startup_post.php",
        Req {
            script: r"<?php
echo json_encode([$_POST, request_parse_body(), file_get_contents('php://input')]);
",
            method: "POST",
            headers: body_headers("application/x-www-form-urlencoded", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(r.body, r#"[{"a":"1","b":"2"},[[],[]],""]"#);
}

/// `request_parse_body()`: A second multipart parse: nothing left, and the startup uploads stop being uploaded files.
#[test]
fn request_parse_body_after_startup_multipart() {
    let body = multipart(&[
        (
            &["Content-Disposition: form-data; name=\"field1\""],
            "value one",
        ),
        (
            &[
                "Content-Disposition: form-data; name=\"file1\"; filename=\"a.txt\"",
                "Content-Type: text/plain",
            ],
            "file one data",
        ),
    ]);
    let r = run(
        "rpb_after_startup_multipart.php",
        Req {
            script: r"<?php
$t = $_FILES['file1']['tmp_name'];
echo json_encode([$_POST, is_uploaded_file($t), request_parse_body(), is_uploaded_file($t), file_exists($t)]);
",
            method: "POST",
            headers: body_headers("multipart/form-data; boundary=---------------------------84000087610663814162942123332", &body),
            body,
            ini: ini(&[]),
            ..Req::default()
        },
    );
    assert_eq!(
        r.body,
        r#"[{"field1":"value one"},true,[[],[]],false,true]"#
    );
}

/// `request_parse_body()`: A too large boundary is a plain warning (`sapi_error()`), not an exception.
#[test]
fn request_parse_body_boundary_too_large() {
    let body = b"--x\n".to_vec();
    let r = run(
        "rpb_boundary_too_large.php",
        Req {
            script: r"<?php
echo json_encode(request_parse_body());
",
            method: "PUT",
            headers: body_headers(
                &format!("multipart/form-data; boundary={}", "x".repeat(5200)),
                &body,
            ),
            body,
            ini: ini(&[("html_errors", "0")]),
            ..Req::default()
        },
    );
    let path = scratch()
        .join("rpb_boundary_too_large.php")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        r.body,
        r"
Warning: Boundary too large in multipart/form-data POST data in %PATH% on line 2
[[],[]]"
            .replace("%PATH%", &path)
    );
}

/// `request_parse_body()`: `max_input_vars` stays a warning of the function for urlencoded bodies (`php_std_post_handler()`).
#[test]
fn request_parse_body_urlencoded_max_input_vars() {
    let body = b"a=1&b=2&c=3".to_vec();
    let r = run(
        "rpb_urlencoded_max_input_vars.php",
        Req {
            script: r"<?php
echo json_encode(request_parse_body(['max_input_vars' => 1]));
",
            method: "PUT",
            headers: body_headers("application/x-www-form-urlencoded", &body),
            body,
            ini: ini(&[("html_errors", "0")]),
            ..Req::default()
        },
    );
    let path = scratch()
        .join("rpb_urlencoded_max_input_vars.php")
        .to_string_lossy()
        .into_owned();
    assert_eq!(r.body, r#"
Warning: request_parse_body(): Input variables exceeded 1. To increase the limit change max_input_vars in php.ini. in %PATH% on line 2
[{"a":"1","b":"2"},[]]"#.replace("%PATH%", &path));
}

/// `request_parse_body()`: `filter.default` filters the fields like `php_sapi_filter()` (an empty value stays empty).
#[test]
fn request_parse_body_filter_default() {
    let body = b"a=%3Cb%3E&e=&n[]=%22".to_vec();
    let r = run(
        "rpb_filter_default.php",
        Req {
            script: r"<?php
echo json_encode(request_parse_body());
",
            method: "PUT",
            headers: body_headers("application/x-www-form-urlencoded", &body),
            body,
            ini: ini(&[("filter.default", "special_chars")]),
            ..Req::default()
        },
    );
    let expected = r#"[{"a":"&#60;b&#62;","e":"","n":["&#34;"]},[]]"#;
    assert!(r.body.ends_with(expected), "{}", r.body);
}

/// `request_parse_body()`: `filter.default` filters multipart fields, not the file entries.
#[test]
fn request_parse_body_filter_default_multipart() {
    let body = multipart(&[
        (&["Content-Disposition: form-data; name=\"a\""], "<b>"),
        (
            &[
                "Content-Disposition: form-data; name=\"file1\"; filename=\"<x>.txt\"",
                "Content-Type: text/plain",
            ],
            "zz",
        ),
    ]);
    let r = run(
        "rpb_filter_default_multipart.php",
        Req {
            script: r"<?php
[$p, $f] = request_parse_body();
echo json_encode([$p, $f['file1']['name'], $f['file1']['type']]);
",
            method: "PUT",
            headers: body_headers("multipart/form-data; boundary=---------------------------84000087610663814162942123332", &body),
            body,
            ini: ini(&[("filter.default", "special_chars")]),
            ..Req::default()
        },
    );
    let expected = r#"[{"a":"&#60;b&#62;"},"<x>.txt","text\/plain"]"#;
    assert!(r.body.ends_with(expected), "{}", r.body);
}

/// `request_parse_body()`: A validating `filter.default` registers the filter result, whatever its type.
#[test]
fn request_parse_body_filter_default_validate() {
    let body = b"a=12&b=x&c=".to_vec();
    let r = run(
        "rpb_filter_default_validate.php",
        Req {
            script: r"<?php
var_dump(request_parse_body()[0]);
",
            method: "PUT",
            headers: body_headers("application/x-www-form-urlencoded", &body),
            body,
            ini: ini(&[("filter.default", "int")]),
            ..Req::default()
        },
    );
    let expected = r#"array(3) {
  ["a"]=>
  int(12)
  ["b"]=>
  bool(false)
  ["c"]=>
  string(0) ""
}
"#;
    assert!(r.body.ends_with(expected), "{}", r.body);
}

/// Multiple sequential requests running on the same worker thread use the persistent VM,
/// but each request sees a clean environment (functions, classes, constants, globals reset).
#[test]
fn persistent_vm_sequential_requests_clean_state() {
    let responses = run_sequence(vec![
        (
            "pvm_req1.php",
            Req {
                script: r#"<?php
function test_fn() { return 123; }
class TestClass { public static $val = 999; }
define('TEST_CONST', 'hello');
$GLOBALS['test_global'] = 'world';
echo "req1 ok";
"#,
                ..Req::default()
            },
        ),
        (
            "pvm_req2.php",
            Req {
                script: r#"<?php
$fn = function_exists('test_fn') ? 'yes' : 'no';
$cls = class_exists('TestClass') ? 'yes' : 'no';
$cst = defined('TEST_CONST') ? 'yes' : 'no';
$glb = isset($GLOBALS['test_global']) ? 'yes' : 'no';
echo "fn:$fn cls:$cls cst:$cst glb:$glb";
"#,
                ..Req::default()
            },
        ),
    ]);
    assert_eq!(responses[0].body, "req1 ok");
    assert_eq!(responses[1].body, "fn:no cls:no cst:no glb:no");
}

/// The stream layer's request state ends with the request
/// (`php_shutdown_stream_hashes()`, filestat's RSHUTDOWN): each request's
/// `php://input` is its own body, not the one an earlier request on the
/// worker read, and wrappers registered by a script are gone.
#[test]
fn persistent_vm_stream_state_is_per_request() {
    let input = "<?php var_dump(file_get_contents('php://input'));";
    let responses = run_sequence(vec![
        (
            "pvm_in1.php",
            Req {
                script: input,
                method: "PUT",
                body: b"first".to_vec(),
                ..Req::default()
            },
        ),
        (
            "pvm_in2.php",
            Req {
                script: "<?php
var_dump(file_get_contents('php://input'));
var_dump(stream_wrapper_register('pvm', 'stdClass'));
",
                method: "PUT",
                body: b"second".to_vec(),
                ..Req::default()
            },
        ),
        (
            "pvm_in3.php",
            Req {
                script: "<?php var_dump(in_array('pvm', stream_get_wrappers()));",
                ..Req::default()
            },
        ),
    ]);
    assert_eq!(responses[0].body, "string(5) \"first\"\n");
    assert_eq!(responses[1].body, "string(6) \"second\"\nbool(true)\n");
    assert_eq!(responses[2].body, "bool(false)\n");
}
