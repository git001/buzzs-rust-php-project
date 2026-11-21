// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `request_parse_body()` under `php -S` (ref: `ext/standard/http.c`,
//! `main/SAPI.c`, `main/php_content_types.c`, `main/rfc1867.c`).
//!
//! php-src runs `ext/standard/tests/http/request_parse_body/*.phpt` through
//! php-cgi; the ones with a request body are replayed here as HTTP
//! requests (run-tests.php's `--POST_RAW--` framing: the `Content-Type`
//! line becomes the header, the other lines joined with `\n` the body; the
//! `--ENV--` method; the `--INI--` settings as `-d`). The other cases are
//! custom-method request parsing (`Request::createFromGlobals()` for
//! PUT/DELETE/PATCH) and how the body is consumed. Every expected output is
//! what php 8.5's `php -S` answers.
//!
//! Ignored by default (needs `cargo build -p php-cli`):
//!
//! ```sh
//! cargo build -p php-cli
//! cargo test -p php-cli-server --test request_parse_body -- --ignored
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

/// Starts `php -n [-d ini]... -t <root> -S 127.0.0.1:0` serving `script`
/// as `/test.php`.
fn start(name: &str, script: &str, ini: &[&str]) -> ServerProc {
    let root =
        std::env::temp_dir().join(format!("php-cli-server-rpb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("test.php"), script).unwrap();
    let mut cmd = Command::new(php_binary());
    cmd.arg("-n");
    for setting in ini {
        cmd.args(["-d", setting]);
    }
    cmd.arg("-t")
        .arg(&root)
        .args(["-S", "127.0.0.1:0"])
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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

/// Sends `method /test.php` with the body and returns the response body,
/// the document root replaced by `%ROOT%`.
fn send(srv: &ServerProc, method: &str, content_type: Option<&str>, body: &[u8]) -> String {
    send_to(srv, method, "/test.php", content_type, body)
}

/// [`send`] to `target` (a path with an optional query string).
fn send_to(
    srv: &ServerProc,
    method: &str,
    target: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut s = loop {
        match TcpStream::connect(("127.0.0.1", srv.port)) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("connect: {e}"),
        }
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(ct) = content_type {
        req.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut raw = req.into_bytes();
    raw.extend_from_slice(body);
    s.write_all(&raw).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let mut body = body.to_string();
    for root in [
        srv.root.clone(),
        std::fs::canonicalize(&srv.root).unwrap_or_default(),
    ] {
        let r = root.to_string_lossy().into_owned();
        if !r.is_empty() {
            body = body.replace(&r, "%ROOT%");
        }
    }
    body
}

/// run-tests.php's `--EXPECTF--` for the `%s` (one or more characters
/// other than a newline) and `%d` (one or more digits) placeholders.
fn expectf(pattern: &[u8], text: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'%', b's', rest @ ..] => (1..=text.len())
            .take_while(|&n| text[n - 1] != b'\n')
            .any(|n| expectf(rest, &text[n..])),
        [b'%', b'd', rest @ ..] => (1..=text.len())
            .take_while(|&n| text[n - 1].is_ascii_digit())
            .any(|n| expectf(rest, &text[n..])),
        [c, rest @ ..] => text.first() == Some(c) && expectf(rest, &text[1..]),
    }
}

/// A php-src `.phpt` test with a request body.
struct Phpt {
    name: &'static str,
    ini: &'static [&'static str],
    method: &'static str,
    content_type: &'static str,
    body: &'static str,
    script: &'static str,
    /// `--EXPECT--`, or `--EXPECTF--` when `expectf`.
    expect: &'static str,
    expectf: bool,
}

fn run_phpt(t: &Phpt) {
    let srv = start(t.name, t.script, t.ini);
    let out = send(&srv, t.method, Some(t.content_type), t.body.as_bytes());
    let (out, expect) = (out.trim(), t.expect.trim());
    if t.expectf {
        assert!(
            expectf(expect.as_bytes(), out.as_bytes()),
            "{}: expected (EXPECTF)\n{expect}\ngot\n{out}",
            t.name
        );
    } else {
        assert_eq!(out, expect, "{}", t.name);
    }
}

/// The start of most case scripts: `show()` prints what a call returns,
/// or the exception it threw with its `previous` chain.
const SHOW: &str = r#"<?php
function show($label, $f) {
    try {
        $r = $f();
        echo $label, ": ", json_encode($r), "\n";
    } catch (Throwable $e) {
        echo $label, ": ", $e::class, ": ", $e->getMessage();
        for ($p = $e->getPrevious(); $p; $p = $p->getPrevious()) {
            echo " <- ", $p::class, ": ", $p->getMessage();
        }
        echo "\n";
    }
}
"#;

/// Handling of custom request methods (PUT, DELETE, PATCH) and what
/// a request parser reports.
const HTTP_REQUEST_HANDLER: &str = r"<?php
// Request parsing for custom methods.
if (in_array($_SERVER['REQUEST_METHOD'] ?? null, ['PUT', 'DELETE', 'PATCH', 'QUERY'], true)) {
    try {
        [$post, $files] = @request_parse_body();
    } catch (\RequestParseBodyException) {
        $post = $_POST;
        $files = $_FILES;
    }
} else {
    $post = $_POST;
    $files = $_FILES;
}
echo json_encode([
    'request' => $post,
    'files' => array_map(
        static fn (array $file) => [
            'clientOriginalName' => $file['name'],
            'clientMimeType' => $file['type'],
            'content' => file_get_contents($file['tmp_name']),
            'uploaded' => is_uploaded_file($file['tmp_name']),
        ],
        $files
    ),
]);
";

/// A request to a script, with the response body php 8.5 gives.
struct Case {
    name: &'static str,
    ini: &'static [&'static str],
    /// The script, after [`SHOW`] when `show`.
    script: &'static str,
    show: bool,
    method: &'static str,
    content_type: Option<&'static str>,
    body: &'static str,
    expect: &'static str,
}

fn run_case(c: &Case) {
    let mut ini = vec!["html_errors=0"];
    ini.extend_from_slice(c.ini);
    let script = if c.show {
        format!("{SHOW}{}", c.script)
    } else {
        c.script.to_string()
    };
    let srv = start(c.name, &script, &ini);
    let out = send(&srv, c.method, c.content_type, c.body.as_bytes());
    assert_eq!(out, c.expect, "{}", c.name);
}

#[test]
fn expectf_placeholders() {
    assert!(expectf(
        b"string(%d) \"%s\"",
        b"string(14) \"/tmp/phpAbCdEf\""
    ));
    assert!(!expectf(b"string(%d) \"%s\"", b"string() \"x\""));
    assert!(!expectf(b"a%sb", b"a\nb"));
    assert!(expectf(b"a%sb", b"axyb"));
}

/// `invalid_boundary.phpt`: `request_parse_body()` with multipart and invalid boundary.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_invalid_boundary() {
    run_phpt(&Phpt {
        name: "invalid_boundary",
        ini: &[],
        method: "PUT",
        content_type: "multipart/form-data; boundary=\"foobar",
        body: r"empty",
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Invalid boundary in multipart/form-data POST data
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart.phpt`: `request_parse_body()` with multipart.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart() {
    run_phpt(&Phpt {
        name: "multipart",
        ini: &[],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="post_field_name"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file_name"; filename="original_file_name.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332--"#,
        script: r"<?php

[$_POST, $_FILES] = request_parse_body();

var_dump($_POST, $_FILES);

$file_path = __DIR__ . '/put_multipart_uploaded_file.txt';
move_uploaded_file($_FILES['file_name']['tmp_name'], $file_path);
var_dump(file_get_contents($file_path));

?>
",
        expect: r#"array(1) {
  ["post_field_name"]=>
  string(15) "post field data"
}
array(1) {
  ["file_name"]=>
  array(6) {
    ["name"]=>
    string(22) "original_file_name.txt"
    ["full_path"]=>
    string(22) "original_file_name.txt"
    ["type"]=>
    string(10) "text/plain"
    ["tmp_name"]=>
    string(%d) "%s"
    ["error"]=>
    int(0)
    ["size"]=>
    int(9)
  }
}
string(9) "file data""#,
        expectf: true,
    });
}

/// `multipart_garbled.phpt`: `request_parse_body()` with multipart and garbled field.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_garbled() {
    run_phpt(&Phpt {
        name: "multipart_garbled",
        ini: &["max_file_uploads=1"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r"-----------------------------84000087610663814162942123332
Content-Disposition: form-data;
Content-Type: text/plain

post field data
-----------------------------84000087610663814162942123332--",
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: File Upload Mime headers garbled
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_max_files.phpt`: `request_parse_body()` with multipart and exceeding max files.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_max_files() {
    run_phpt(&Phpt {
        name: "multipart_max_files",
        ini: &["max_file_uploads=1"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="file1.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file2"; filename="file2.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Maximum number of allowable file uploads has been exceeded
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_max_input_vars.phpt`: `request_parse_body()` with multipart and exceeding max input vars.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_max_input_vars() {
    run_phpt(&Phpt {
        name: "multipart_max_input_vars",
        ini: &["max_input_vars=1"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field2"

post field data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_max_parts.phpt`: `request_parse_body()` with multipart and exceeding max parts.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_max_parts() {
    run_phpt(&Phpt {
        name: "multipart_max_parts",
        ini: &["max_multipart_body_parts=1"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="post_field_name"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file_name"; filename="original_file_name.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Multipart body parts limit exceeded 1. To increase the limit change max_multipart_body_parts in php.ini.
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_missing_boundary.phpt`: `request_parse_body()` with multipart and missing boundary.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_missing_boundary() {
    run_phpt(&Phpt {
        name: "multipart_missing_boundary",
        ini: &[],
        method: "PUT",
        content_type: "multipart/form-data",
        body: r"empty",
        script: r#"<?php

$stream = fopen('php://memory','r+');

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Missing boundary in multipart/form-data POST data
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_options_max_files.phpt`: `request_parse_body()` `max_file_uploads` option.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_options_max_files() {
    run_phpt(&Phpt {
        name: "multipart_options_max_files",
        ini: &["max_file_uploads=10"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="file1.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file2"; filename="file2.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body([
        'max_file_uploads' => 1,
    ]);
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Maximum number of allowable file uploads has been exceeded
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_options_max_input_vars.phpt`: `request_parse_body()` `max_input_vars` option.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_options_max_input_vars() {
    run_phpt(&Phpt {
        name: "multipart_options_max_input_vars",
        ini: &["max_input_vars=10"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field2"

post field data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body([
        'max_input_vars' => 1,
    ]);
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_options_max_parts.phpt`: `request_parse_body()` `max_multipart_body_parts` option.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_options_max_parts() {
    run_phpt(&Phpt {
        name: "multipart_options_max_parts",
        ini: &["max_multipart_body_parts=10"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="post_field_name"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file_name"; filename="original_file_name.txt"
Content-Type: text/plain

file data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body([
        'max_multipart_body_parts' => 1,
    ]);
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: Multipart body parts limit exceeded 1. To increase the limit change max_multipart_body_parts in php.ini.
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_options_post_max_size.phpt`: `request_parse_body()` `post_max_size` option.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_options_post_max_size() {
    run_phpt(&Phpt {
        name: "multipart_options_post_max_size",
        ini: &["post_max_size=1M"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

post field data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field2"

post file data
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body([
        'post_max_size' => '302',
    ]);
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r"RequestParseBodyException: POST Content-Length of 303 bytes exceeds the limit of 302 bytes
array(0) {
}
array(0) {
}",
        expectf: false,
    });
}

/// `multipart_options_upload_max_filesize.phpt`: `request_parse_body()` `upload_max_filesize` option.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_multipart_options_upload_max_filesize() {
    run_phpt(&Phpt {
        name: "multipart_options_upload_max_filesize",
        ini: &["upload_max_filesize=1M"],
        method: "PUT",
        content_type:
            "multipart/form-data; boundary=---------------------------84000087610663814162942123332",
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: name="file1"; filename="file1.txt"

aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
-----------------------------84000087610663814162942123332--"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body([
        'upload_max_filesize' => '128',
    ]);
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r#"array(0) {
}
array(1) {
  ["file1"]=>
  array(6) {
    ["name"]=>
    string(9) "file1.txt"
    ["full_path"]=>
    string(9) "file1.txt"
    ["type"]=>
    string(0) ""
    ["tmp_name"]=>
    string(0) ""
    ["error"]=>
    int(1)
    ["size"]=>
    int(0)
  }
}"#,
        expectf: false,
    });
}

/// `unsupported_content_type.phpt`: `request_parse_body()` with multipart and unsupported Content-Type.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_unsupported_content_type() {
    run_phpt(&Phpt {
        name: "unsupported_content_type",
        ini: &["max_input_vars=1"],
        method: "PUT",
        content_type: "application/json",
        body: r#"{"hello": "world"}"#,
        script: r#"<?php

try {
    [$_POST, $_FILES] = request_parse_body();
} catch (Throwable $e) {
    echo $e::class, ': ', $e->getMessage(), "\n";
}

var_dump($_POST, $_FILES);

?>
"#,
        expect: r#"RequestParseBodyException: Content-Type "application/json" is not supported
array(0) {
}
array(0) {
}"#,
        expectf: false,
    });
}

/// `urlencoded.phpt`: `request_parse_body()` with urlencoded.
#[test]
#[ignore = "needs target/debug/php"]
fn phpt_urlencoded() {
    run_phpt(&Phpt {
        name: "urlencoded",
        ini: &[],
        method: "PUT",
        content_type: "application/x-www-form-urlencoded",
        body: r"foo=foo&bar[]=1&bar[]=2",
        script: r"<?php

[$_POST, $_FILES] = request_parse_body();

var_dump($_POST, $_FILES);

?>
",
        expect: r#"array(2) {
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
array(0) {
}"#,
        expectf: false,
    });
}

/// Form urlencoded body parsing (PUT).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_urlencoded_put() {
    run_case(&Case {
        name: "custom_method_urlencoded_put",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"{"request":{"foo":"bar"},"files":[]}"#,
    });
}

/// Multipart body parsing (PUT).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_multipart_put() {
    run_case(&Case {
        name: "custom_method_multipart_put",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=boundary"),
        body: "--boundary\r\nContent-Disposition: form-data; name=foo\r\n\r\nbar\r\n--boundary\r\nContent-Disposition: form-data; name=baz; filename=baz.txt\r\nContent-Type: text/plain\r\n\r\nqux\r\n--boundary--",
        expect: r#"{"request":{"foo":"bar"},"files":{"baz":{"clientOriginalName":"baz.txt","clientMimeType":"text\/plain","content":"qux","uploaded":true}}}"#,
    });
}

/// Form urlencoded body parsing (DELETE).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_urlencoded_delete() {
    run_case(&Case {
        name: "custom_method_urlencoded_delete",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "DELETE",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"{"request":{"foo":"bar"},"files":[]}"#,
    });
}

/// Multipart body parsing (DELETE).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_multipart_delete() {
    run_case(&Case {
        name: "custom_method_multipart_delete",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "DELETE",
        content_type: Some("multipart/form-data; boundary=boundary"),
        body: "--boundary\r\nContent-Disposition: form-data; name=foo\r\n\r\nbar\r\n--boundary\r\nContent-Disposition: form-data; name=baz; filename=baz.txt\r\nContent-Type: text/plain\r\n\r\nqux\r\n--boundary--",
        expect: r#"{"request":{"foo":"bar"},"files":{"baz":{"clientOriginalName":"baz.txt","clientMimeType":"text\/plain","content":"qux","uploaded":true}}}"#,
    });
}

/// Form urlencoded body parsing (PATCH).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_urlencoded_patch() {
    run_case(&Case {
        name: "custom_method_urlencoded_patch",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "PATCH",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"{"request":{"foo":"bar"},"files":[]}"#,
    });
}

/// Multipart body parsing (PATCH).
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_multipart_patch() {
    run_case(&Case {
        name: "custom_method_multipart_patch",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "PATCH",
        content_type: Some("multipart/form-data; boundary=boundary"),
        body: "--boundary\r\nContent-Disposition: form-data; name=foo\r\n\r\nbar\r\n--boundary\r\nContent-Disposition: form-data; name=baz; filename=baz.txt\r\nContent-Type: text/plain\r\n\r\nqux\r\n--boundary--",
        expect: r#"{"request":{"foo":"bar"},"files":{"baz":{"clientOriginalName":"baz.txt","clientMimeType":"text\/plain","content":"qux","uploaded":true}}}"#,
    });
}

/// POST still fills `$_POST` at startup.
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_post_uses_post() {
    run_case(&Case {
        name: "custom_method_post_uses_post",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "POST",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"{"request":{"foo":"bar"},"files":[]}"#,
    });
}

/// An unsupported content type throws; falls back to `$_POST`.
#[test]
#[ignore = "needs target/debug/php"]
fn custom_method_put_json_falls_back() {
    run_case(&Case {
        name: "custom_method_put_json_falls_back",
        ini: &[],
        script: HTTP_REQUEST_HANDLER,
        show: false,
        method: "PUT",
        content_type: Some("application/json"),
        body: r#"{"foo":"bar"}"#,
        expect: r#"{"request":[],"files":[]}"#,
    });
}

/// The body stays readable through `php://input` after a urlencoded parse.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_input_after() {
    run_case(&Case {
        name: "put_urlencoded_input_after",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
echo "post: ", json_encode($_POST), "\n";
"#,
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b[]=2&b[]=3",
        expect: r#"rpb: [{"a":"1","b":["2","3"]},[]]
input: "a=1&b[]=2&b[]=3"
post: []
"#,
    });
}

/// A second parse finds the body consumed and replaces the buffer `php://input` reads.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_twice() {
    run_case(&Case {
        name: "put_urlencoded_twice",
        ini: &[],
        script: r#"
show('rpb1', fn() => request_parse_body());
show('rpb2', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2",
        expect: r#"rpb1: [{"a":"1","b":"2"},[]]
rpb2: [[],[]]
input: ""
"#,
    });
}

/// An exceeded `post_max_size` option warns (no exception) and reads nothing.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_post_max_size() {
    run_case(&Case {
        name: "put_urlencoded_post_max_size",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body(['post_max_size' => 3]));
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
show('rpb2', fn() => request_parse_body());
"#,
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2",
        expect: r#"
Warning: request_parse_body(): POST Content-Length of 7 bytes exceeds the limit of 3 bytes in %ROOT%/test.php on line 15
rpb: [[],[]]
input: "a=1&b=2"
rpb2: [[],[]]
"#,
    });
}

/// `max_input_vars` warns for urlencoded bodies (`php_std_post_handler`).
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_max_input_vars() {
    run_case(&Case {
        name: "put_urlencoded_max_input_vars",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['max_input_vars' => 1]));
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2&c=3",
        expect: r#"
Warning: request_parse_body(): Input variables exceeded 1. To increase the limit change max_input_vars in php.ini. in %ROOT%/test.php on line 15
rpb: [{"a":"1","b":"2"},[]]
"#,
    });
}

/// The content type is matched case-insensitively, parameters ignored.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_uppercase_ct() {
    run_case(&Case {
        name: "put_urlencoded_uppercase_ct",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "PUT",
        content_type: Some("Application/X-WWW-Form-URLEncoded; charset=UTF-8"),
        body: r"a=1",
        expect: r#"rpb: [{"a":"1"},[]]
"#,
    });
}

/// Any request method can have its body parsed.
#[test]
#[ignore = "needs target/debug/php"]
fn get_urlencoded() {
    run_case(&Case {
        name: "get_urlencoded",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "GET",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1",
        expect: r#"rpb: [{"a":"1"},[]]
"#,
    });
}

/// A multipart parse consumes the body; `$_FILES` stays empty.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_input_after() {
    run_case(&Case {
        name: "put_multipart_input_after",
        ini: &[],
        script: r#"
show('rpb', function () { [$p, $f] = request_parse_body(); $f['file1']['tmp_name'] = is_uploaded_file($f['file1']['tmp_name']) ? 'UPLOADED' : 'NOT'; return [$p, $f]; });
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
echo "files: ", json_encode($_FILES), "\n";
"#,
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [{"field1":"value one"},{"file1":{"name":"a.txt","full_path":"a.txt","type":"text\/plain","tmp_name":"UPLOADED","error":0,"size":13}}]
input: ""
files: []
"#,
    });
}

/// A second multipart parse replaces `SG(rfc1867_uploaded_files)`.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_twice() {
    run_case(&Case {
        name: "put_multipart_twice",
        ini: &[],
        script: r#"
[$p, $f] = request_parse_body();
$t = $f['file1']['tmp_name'];
echo "uploaded1: ", var_export(is_uploaded_file($t), true), "\n";
show('rpb2', fn() => request_parse_body());
echo "uploaded1 after: ", var_export(is_uploaded_file($t), true), "\n";
echo "exists: ", var_export(file_exists($t), true), "\n";
file_put_contents(__DIR__ . '/tmpname.txt', $t);
"#,
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r"uploaded1: true
rpb2: [[],[]]
uploaded1 after: false
exists: true
",
    });
}

/// After the startup parse of a POST body nothing is left.
#[test]
#[ignore = "needs target/debug/php"]
fn post_urlencoded_then_rpb() {
    run_case(&Case {
        name: "post_urlencoded_then_rpb",
        ini: &[],
        script: r#"
echo "post: ", json_encode($_POST), "\n";
show('rpb', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
echo "post: ", json_encode($_POST), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2",
        expect: r#"post: {"a":"1","b":"2"}
rpb: [[],[]]
input: ""
post: {"a":"1","b":"2"}
"#,
    });
}

/// After the startup multipart parse nothing is left; the startup uploads stop being uploaded files.
#[test]
#[ignore = "needs target/debug/php"]
fn post_multipart_then_rpb() {
    run_case(&Case {
        name: "post_multipart_then_rpb",
        ini: &[],
        script: r#"
$t = $_FILES['file1']['tmp_name'];
echo "post: ", json_encode($_POST), "\n";
echo "uploaded: ", var_export(is_uploaded_file($t), true), "\n";
show('rpb', fn() => request_parse_body());
echo "uploaded after: ", var_export(is_uploaded_file($t), true), "\n";
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
file_put_contents(__DIR__ . '/tmpname.txt', $t);
"#,
        show: true,
        method: "POST",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"post: {"field1":"value one"}
uploaded: true
rpb: [[],[]]
uploaded after: false
input: ""
"#,
    });
}

/// The startup warning, then the exception of the same problem.
#[test]
#[ignore = "needs target/debug/php"]
fn post_multipart_bad_boundary_then_rpb() {
    run_case(&Case {
        name: "post_multipart_bad_boundary_then_rpb",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body());
echo "uploaded: ", json_encode($_FILES), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("multipart/form-data; boundary=\"foobar"),
        body: r"empty",
        expect: r"
Warning: PHP Request Startup: Invalid boundary in multipart/form-data POST data in Unknown on line 0
rpb: RequestParseBodyException: Invalid boundary in multipart/form-data POST data
uploaded: []
",
    });
}

/// A POST body of an unsupported type is read (and the buffer replaced) before the exception.
#[test]
#[ignore = "needs target/debug/php"]
fn post_json_rpb_first() {
    run_case(&Case {
        name: "post_json_rpb_first",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("application/json"),
        body: r#"{"a":1}"#,
        expect: r#"rpb: RequestParseBodyException: Content-Type "application/json" is not supported
input: ""
"#,
    });
}

/// A PUT body of an unsupported type is left alone.
#[test]
#[ignore = "needs target/debug/php"]
fn put_json_rpb() {
    run_case(&Case {
        name: "put_json_rpb",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "PUT",
        content_type: Some("application/json"),
        body: r#"{"a":1}"#,
        expect: r#"rpb: RequestParseBodyException: Content-Type "application/json" is not supported
input: "{\"a\":1}"
"#,
    });
}

/// `enable_post_data_reading=0`: the body is parsed on demand.
#[test]
#[ignore = "needs target/debug/php"]
fn post_no_reading_urlencoded() {
    run_case(&Case {
        name: "post_no_reading_urlencoded",
        ini: &["enable_post_data_reading=0"],
        script: r#"
echo "post: ", json_encode($_POST), "\n";
show('rpb', fn() => request_parse_body());
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2",
        expect: r#"post: []
rpb: [{"a":"1","b":"2"},[]]
input: "a=1&b=2"
"#,
    });
}

/// `enable_post_data_reading=0` with a multipart body.
#[test]
#[ignore = "needs target/debug/php"]
fn post_no_reading_multipart() {
    run_case(&Case {
        name: "post_no_reading_multipart",
        ini: &["enable_post_data_reading=0"],
        script: r#"
echo "post: ", json_encode($_POST), "\n";
show('rpb', function () { [$p, $f] = request_parse_body(); $f['file1']['tmp_name'] = is_uploaded_file($f['file1']['tmp_name']) ? 'UPLOADED' : 'NOT'; return [$p, $f]; });
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"post: []
rpb: [{"field1":"value one"},{"file1":{"name":"a.txt","full_path":"a.txt","type":"text\/plain","tmp_name":"UPLOADED","error":0,"size":13}}]
input: ""
"#,
    });
}

/// Exceptions raised during one parse are chained (the last one is thrown).
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_chain_files() {
    run_case(&Case {
        name: "put_multipart_chain_files",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['max_file_uploads' => 1, 'max_input_vars' => 1]));
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field2"

value two
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file2"; filename="b.txt"
Content-Type: text/plain

file two data
-----------------------------84000087610663814162942123332--"#,
        expect: r"rpb: RequestParseBodyException: Multipart body parts limit exceeded 2. To increase the limit change max_multipart_body_parts in php.ini. <- RequestParseBodyException: Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
",
    });
}

/// `file_uploads=0` skips the files silently.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_file_uploads_off() {
    run_case(&Case {
        name: "put_multipart_file_uploads_off",
        ini: &["file_uploads=0"],
        script: r"
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [{"field1":"value one"},[]]
"#,
    });
}

/// CRLF multipart body with unquoted parameters.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_crlf_standard() {
    run_case(&Case {
        name: "put_multipart_crlf_standard",
        ini: &[],
        script: r"
show('rpb', function () { [$p, $f] = request_parse_body(); $f['baz']['content'] = file_get_contents($f['baz']['tmp_name']); unset($f['baz']['tmp_name']); return [$p, $f]; });
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=boundary"),
        body: "--boundary\r\nContent-Disposition: form-data; name=foo\r\n\r\nbar\r\n--boundary\r\nContent-Disposition: form-data; name=baz; filename=baz.txt\r\nContent-Type: text/plain\r\n\r\nqux\r\n--boundary--",
        expect: r#"rpb: [{"foo":"bar"},{"baz":{"name":"baz.txt","full_path":"baz.txt","type":"text\/plain","error":0,"size":3,"content":"qux"}}]
"#,
    });
}

/// `null` options, case-insensitive option keys.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_options_null() {
    run_case(&Case {
        name: "put_urlencoded_options_null",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(null));
show('rpb', fn() => request_parse_body(['POST_MAX_SIZE' => '1K', 'Max_Input_Vars' => 5]));
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"rpb: [{"foo":"bar"},[]]
rpb: [[],[]]
"#,
    });
}

/// An invalid option throws before the body is read.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_bad_option_then() {
    run_case(&Case {
        name: "put_urlencoded_bad_option_then",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['post_max_size' => []]));
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"rpb: ValueError: Invalid array value in $options argument
rpb: [{"foo":"bar"},[]]
"#,
    });
}

/// A quantity string option warns and is used.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_quantity_warning() {
    run_case(&Case {
        name: "put_urlencoded_quantity_warning",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['post_max_size' => '1GB']));
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"foo=bar",
        expect: r#"
Warning: Invalid quantity "1GB": unknown multiplier "B", interpreting as "1" for backwards compatibility in %ROOT%/test.php on line 15

Warning: request_parse_body(): POST Content-Length of 7 bytes exceeds the limit of 1 bytes in %ROOT%/test.php on line 15
rpb: [[],[]]
"#,
    });
}

/// An empty urlencoded body.
#[test]
#[ignore = "needs target/debug/php"]
fn put_empty_body_urlencoded() {
    run_case(&Case {
        name: "put_empty_body_urlencoded",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"",
        expect: r"rpb: [[],[]]
",
    });
}

/// A user error handler does not see the exceptions.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_error_handler() {
    run_case(&Case {
        name: "put_multipart_error_handler",
        ini: &[],
        script: r#"
set_error_handler(function ($no, $str) { echo "handler: $str\n"; return true; });
show('rpb', fn() => request_parse_body(['max_input_vars' => 1]));
"#,
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field2"

value two
-----------------------------84000087610663814162942123332--"#,
        expect: r"rpb: RequestParseBodyException: Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
",
    });
}

/// The `upload_max_filesize` option cancels a larger file (`UPLOAD_ERR_INI_SIZE`).
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_upload_max_filesize_opt() {
    run_case(&Case {
        name: "put_multipart_upload_max_filesize_opt",
        ini: &[],
        script: r"
show('rpb', function () { [$p, $f] = request_parse_body(['upload_max_filesize' => 4]); return [$p, $f]; });
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [[],{"file1":{"name":"a.txt","full_path":"a.txt","type":"","tmp_name":"","error":1,"size":0}}]
"#,
    });
}

/// A file within the `upload_max_filesize` option.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_upload_max_filesize_ok() {
    run_case(&Case {
        name: "put_multipart_upload_max_filesize_ok",
        ini: &[],
        script: r"
show('rpb', function () { [$p, $f] = request_parse_body(['upload_max_filesize' => '1K']); $f['file1']['tmp_name'] = strlen($f['file1']['tmp_name']) > 0; return [$p, $f]; });
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [[],{"file1":{"name":"a.txt","full_path":"a.txt","type":"text\/plain","tmp_name":true,"error":0,"size":13}}]
"#,
    });
}

/// `move_uploaded_file()` accepts the files of the parse.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_move() {
    run_case(&Case {
        name: "put_multipart_move",
        ini: &[],
        script: r"<?php
[$p, $f] = request_parse_body();
var_dump(move_uploaded_file($f['file1']['tmp_name'], __DIR__ . '/moved.txt'));
var_dump(file_get_contents(__DIR__ . '/moved.txt'));
var_dump(is_uploaded_file($f['file1']['tmp_name']));
unlink(__DIR__ . '/moved.txt');
",
        show: false,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"bool(true)
string(13) "file one data"
bool(false)
"#,
    });
}

/// Array uploads and nested field names.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_array_files() {
    run_case(&Case {
        name: "put_multipart_array_files",
        ini: &[],
        script: r"
show('rpb', function () { [$p, $f] = request_parse_body(); foreach ($f['u']['tmp_name'] as $k => $t) { $f['u']['tmp_name'][$k] = is_uploaded_file($t); } return [$p, $f]; });
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="u[]"; filename="x.txt"
Content-Type: text/plain

xx
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="u[]"; filename="y.txt"

yyy
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="a[b][]"

nested
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [{"a":{"b":["nested"]}},{"u":{"name":["x.txt","y.txt"],"full_path":["x.txt","y.txt"],"type":["text\/plain",""],"tmp_name":[true,true],"error":[0,0],"size":[2,3]}}]
"#,
    });
}

/// A `post_max_size` option below the length warns and parses the buffer startup read.
#[test]
#[ignore = "needs target/debug/php"]
fn post_urlencoded_then_rpb_pms() {
    run_case(&Case {
        name: "post_urlencoded_then_rpb_pms",
        ini: &[],
        script: r#"
show('rpb', fn() => request_parse_body(['post_max_size' => 3]));
echo "input: ", json_encode(file_get_contents('php://input')), "\n";
"#,
        show: true,
        method: "POST",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2",
        expect: r#"
Warning: request_parse_body(): POST Content-Length of 7 bytes exceeds the limit of 3 bytes in %ROOT%/test.php on line 15
rpb: [{"a":"1","b":"2"},[]]
input: "a=1&b=2"
"#,
    });
}

/// The urlencoded `max_input_vars` warning through a throwing error handler.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_error_handler_throws() {
    run_case(&Case {
        name: "put_urlencoded_error_handler_throws",
        ini: &[],
        script: r"
set_error_handler(function ($no, $str) { throw new ErrorException($str); });
show('rpb', fn() => request_parse_body(['max_input_vars' => 1]));
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2&c=3",
        expect: r"rpb: ErrorException: request_parse_body(): Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.
",
    });
}

/// A throwing error handler on the `post_max_size` warning; the body was not read.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_pms_error_handler_throws() {
    run_case(&Case {
        name: "put_urlencoded_pms_error_handler_throws",
        ini: &[],
        script: r"
set_error_handler(function ($no, $str) { throw new ErrorException($str); });
show('rpb', fn() => request_parse_body(['post_max_size' => 2]));
show('rpb', fn() => request_parse_body());
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2&c=3",
        expect: r#"rpb: ErrorException: request_parse_body(): POST Content-Length of 11 bytes exceeds the limit of 2 bytes
rpb: [{"a":"1","b":"2","c":"3"},[]]
"#,
    });
}

/// `max_input_vars` from the INI, then the option on a consumed body.
#[test]
#[ignore = "needs target/debug/php"]
fn put_urlencoded_max_input_vars_ini() {
    run_case(&Case {
        name: "put_urlencoded_max_input_vars_ini",
        ini: &["max_input_vars=2"],
        script: r"
show('rpb', fn() => request_parse_body());
show('rpb', fn() => request_parse_body(['max_input_vars' => 5]));
",
        show: true,
        method: "PUT",
        content_type: Some("application/x-www-form-urlencoded"),
        body: r"a=1&b=2&c=3&d=4",
        expect: r#"
Warning: request_parse_body(): Input variables exceeded 2. To increase the limit change max_input_vars in php.ini. in %ROOT%/test.php on line 15
rpb: [{"a":"1","b":"2","c":"3"},[]]
rpb: [[],[]]
"#,
    });
}

/// Option keys are case-insensitive (`MAX_FILE_UPLOADS`).
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_options_case() {
    run_case(&Case {
        name: "put_multipart_options_case",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['MAX_FILE_UPLOADS' => 0]));
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r"rpb: RequestParseBodyException: Maximum number of allowable file uploads has been exceeded
",
    });
}

/// A negative `max_file_uploads` skips files without an exception.
#[test]
#[ignore = "needs target/debug/php"]
fn put_multipart_upload_cnt_neg() {
    run_case(&Case {
        name: "put_multipart_upload_cnt_neg",
        ini: &[],
        script: r"
show('rpb', fn() => request_parse_body(['max_file_uploads' => -1, 'max_multipart_body_parts' => 10]));
",
        show: true,
        method: "PUT",
        content_type: Some("multipart/form-data; boundary=---------------------------84000087610663814162942123332"),
        body: r#"-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="field1"

value one
-----------------------------84000087610663814162942123332
Content-Disposition: form-data; name="file1"; filename="a.txt"
Content-Type: text/plain

file one data
-----------------------------84000087610663814162942123332--"#,
        expect: r#"rpb: [{"field1":"value one"},[]]
"#,
    });
}

/// `php_auto_globals_create_post()`: a startup warning displayed without
/// output buffering sends the headers, and then the POST handler does not
/// run (`$_POST` stays empty); the body is still `php://input`. With
/// output buffering the headers are not sent yet.
#[test]
#[ignore = "needs target/debug/php"]
fn post_after_headers_sent_at_startup() {
    let script = "<?php var_dump(count($_GET), $_POST, file_get_contents('php://input'));";
    let warning = "\nWarning: PHP Request Startup: Input variables exceeded 2. To increase \
the limit change max_input_vars in php.ini. in Unknown on line 0\n";
    for (ob, post) in [
        ("0", "array(0) {\n}\n"),
        ("4096", "array(1) {\n  [\"x\"]=>\n  string(1) \"1\"\n}\n"),
    ] {
        let output_buffering = format!("output_buffering={ob}");
        let ini = [
            "max_input_vars=2",
            "display_errors=1",
            "html_errors=0",
            output_buffering.as_str(),
        ];
        let srv = start(&format!("post_after_headers_sent_{ob}"), script, &ini);
        let out = send_to(
            &srv,
            "POST",
            "/test.php?a=1&b=2&c=3",
            Some("application/x-www-form-urlencoded"),
            b"x=1",
        );
        assert_eq!(
            out,
            format!("{warning}int(2)\n{post}string(3) \"x=1\"\n"),
            "ob={ob}"
        );
    }
}
