// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! php-cgi with a script on the command line (`sapi/cgi/cgi_main.c`): the
//! script and its arguments are `SG(request_info).argv`, which
//! `php_build_argv()` turns into `$argv`/`$argc` and `$_SERVER['argv']`/
//! `['argc']` whatever `register_argc_argv` says; the arguments after the
//! options also form the query string, the script included when it is not
//! given with `-f`. Expected output as PHP 8.6's php-cgi prints it.

use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;

const SCRIPT: &str = r#"<?php
echo "argc="; var_dump($argc ?? 'unset');
echo "argv="; var_dump($argv ?? 'unset');
echo "S.argc="; var_dump($_SERVER['argc'] ?? 'unset');
echo "S.argv="; var_dump($_SERVER['argv'] ?? 'unset');
echo "GET="; var_dump($_GET);
"#;

/// The test script as a new file in PHP's temporary directory (a random
/// name created exclusively, `php_open_temporary_fd()`), removed on drop.
struct Script(PathBuf);

impl Script {
    fn new() -> Self {
        use std::io::Write as _;
        use std::os::unix::ffi::OsStringExt;
        let (mut file, path) = php_streams::temp::open_temporary_fd(
            None,
            Some(b"cgi-argv-"),
            php_streams::temp::SILENT,
        )
        .expect("temporary file");
        file.write_all(SCRIPT.as_bytes()).expect("write the script");
        Script(PathBuf::from(std::ffi::OsString::from_vec(path)))
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("UTF-8 path")
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn php_cgi(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .args(["-n", "-q"])
        .args(args)
        .env_remove("QUERY_STRING")
        .env_remove("REQUEST_METHOD")
        .output()
        .expect("spawn php-cgi");
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

/// `var_dump()` of a list of strings.
fn dump_list(items: &[&str]) -> String {
    let mut s = format!("array({}) {{\n", items.len());
    for (i, item) in items.iter().enumerate() {
        let _ = write!(s, "  [{i}]=>\n  string({}) \"{item}\"\n", item.len());
    }
    s.push_str("}\n");
    s
}

fn expected(argv: &[&str], get: &str) -> String {
    let list = dump_list(argv);
    let count = argv.len();
    format!("argc=int({count})\nargv={list}S.argc=int({count})\nS.argv={list}GET={get}")
}

#[test]
fn script_file_option_gives_argv() {
    let script = Script::new();
    let p = script.path();
    for setting in ["register_argc_argv=0", "register_argc_argv=1"] {
        let out = php_cgi(&["-d", setting, "-f", p, "v1=a", "v2=b"]);
        let get =
            "array(2) {\n  [\"v1\"]=>\n  string(1) \"a\"\n  [\"v2\"]=>\n  string(1) \"b\"\n}\n";
        assert_eq!(out, expected(&[p, "v1=a", "v2=b"], get), "{setting}");
    }
    // `--` ends the options.
    let out = php_cgi(&["-f", p, "--", "v1=a"]);
    let get = "array(1) {\n  [\"v1\"]=>\n  string(1) \"a\"\n}\n";
    assert_eq!(out, expected(&[p, "v1=a"], get));
}

#[test]
fn script_argument_gives_argv_and_joins_the_query_string() {
    let script = Script::new();
    let p = script.path();
    let out = php_cgi(&[p, "v1=a", "v2=b"]);
    // The script is part of the query string (php_optind stays on it):
    // its name is a GET variable with `.` turned into `_`.
    let key = p.replace('.', "_");
    let get = format!(
        "array(3) {{\n  [\"{key}\"]=>\n  string(0) \"\"\n  [\"v1\"]=>\n  string(1) \"a\"\n  [\"v2\"]=>\n  string(1) \"b\"\n}}\n"
    );
    assert_eq!(out, expected(&[p, "v1=a", "v2=b"], &get));
}
