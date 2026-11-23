// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests: the scripts in `tests/oracle/*.php` are run through the
//! test interpreter and compared with the output of `/usr/bin/php`
//! (8.3, cached in `*.out`; the scripts avoid 8.3-vs-master differences).
//! Regenerate with `cargo test -p php-spl --test oracle -- --ignored`.

#![expect(clippy::disallowed_methods, reason = "test code")]

mod harness;

use std::path::{Path, PathBuf};

/// Replaces the crate directory in script output with `%CRATE%`, so the
/// cached outputs do not depend on where the repository is checked out.
fn crate_relative(out: &[u8]) -> Vec<u8> {
    let dir = env!("CARGO_MANIFEST_DIR").as_bytes();
    let mut v = Vec::with_capacity(out.len());
    let mut i = 0;
    while i < out.len() {
        if out[i..].starts_with(dir) {
            v.extend_from_slice(b"%CRATE%");
            i += dir.len();
        } else {
            v.push(out[i]);
            i += 1;
        }
    }
    v
}

fn scripts() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "php"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn oracle_scripts() {
    let mut failures = Vec::new();
    for p in scripts() {
        let src = std::fs::read(&p).expect("read script");
        let expected = std::fs::read(p.with_extension("out")).expect("read expected output");
        let name = p.to_string_lossy().into_owned();
        let pp = p.clone();
        let got = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let mut it = harness::Interp::new(&name);
                it.dir = pp
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_default();
                it.run_source(&src);
                it.out
            })
            .expect("spawn")
            .join()
            .unwrap_or_else(|_| b"PANIC".to_vec());
        if let Ok(d) = std::env::var("SPL_ORACLE_DUMP") {
            let f = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = std::fs::write(Path::new(&d).join(f + ".got"), &got);
        }
        let got = crate_relative(&got);
        let (e, g) = (harness::normalize(&expected), harness::normalize(&got));
        if e != g {
            failures.push(format!(
                "{}\n--- expected\n{}\n--- got\n{}",
                p.display(),
                String::from_utf8_lossy(&e),
                String::from_utf8_lossy(&g)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
#[ignore = "regenerates the expected outputs with /usr/bin/php"]
fn regen_oracle() {
    for p in scripts() {
        let out = std::process::Command::new("/usr/bin/php")
            .args([
                "-d",
                "memory_limit=1G",
                "-d",
                "error_reporting=-1",
                "-d",
                "display_errors=1",
                "-d",
                "html_errors=0",
                "-d",
                "log_errors=0",
            ])
            .arg(&p)
            .output()
            .expect("run php");
        let mut data = out.stdout;
        data.extend_from_slice(&out.stderr);
        std::fs::write(p.with_extension("out"), crate_relative(&data)).expect("write");
    }
}
