// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Runs the `.phpt` scripts of `ext/soap/tests` (interop rounds, bug
//! regressions, client/server tests) with the test interpreter over the
//! extension and compares their output with the expected one.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

mod common;

use std::path::{Path, PathBuf};

use common::php::run_file;
use common::{output_matches, phpt_sections, soap_tests_dir, with_big_stack, TestHost};

/// Tests whose scripts need engine features the test interpreter lacks
/// (sessions, references, `__get`/`__set`, Reflection, `unserialize`,
/// visibility-mangled dumps, pcntl, process spawning, generators,
/// `memory_limit` (the `oom_*` tests need the counting allocator), …).
/// They are exercised by the phpt runner once the extension is wired into
/// the VM.
const ENGINE_DEPENDENT: &[&str] = &[
    "GHSA-m33r-qmcv-p97q.phpt",
    "SoapServer/persistent-session-no-free-SoapFault.phpt",
    "bugs/bug28751.phpt",
    "bugs/bug29830.phpt",
    "bugs/bug30045.phpt",
    "bugs/bug30928.phpt",
    "bugs/bug31422.phpt",
    "bugs/bug42151.phpt",
    "bugs/bug46760.phpt",
    "bugs/bug51561.phpt",
    "bugs/bug54911.phpt",
    "bugs/bug69085.phpt",
    "bugs/bug70388.phpt",
    "bugs/bug70469.phpt",
    "bugs/bug70211.phpt",
    "bugs/bug71610.phpt",
    "bugs/bug73037.phpt",
    "bugs/bug73237.phpt",
    "bugs/bug73452.phpt",
    "bugs/bug75306.phpt",
    "bugs/bug77088.phpt",
    "bugs/bug79357.phpt",
    "bugs/gh16237.phpt",
    "bugs/gh16259.phpt",
    "bugs/gh16429.phpt",
    "bugs/gh21421.phpt",
    "bugs/gh22167.phpt",
    "bugs/gh22895.phpt",
    "bugs/segfault_assertion_props.phpt",
    "classmap004.phpt",
    "classmap_private_property_packed_array.phpt",
    "gh15711.phpt",
    "gh16318.phpt",
    "gh22585.phpt",
    "gh23447.phpt",
    "oom_soapclient_call.phpt",
    "oom_soapclient_soapcall.phpt",
    "oom_soapserver_handle_array.phpt",
    "oom_soapserver_handle_headers.phpt",
    "oom_soapserver_handle_parameters.phpt",
    "server003.phpt",
    "server009.phpt",
    "server030.phpt",
    "server031.phpt",
];

/// Outcome of one test.
enum Outcome {
    Pass,
    Skip(String),
    Fail(String),
}

fn run_phpt(path: &Path) -> Outcome {
    let Ok(raw) = std::fs::read(path) else {
        return Outcome::Skip("unreadable".into());
    };
    // Latin-1 view so that non-UTF-8 test files survive unchanged.
    let src: String = raw.iter().map(|&b| char::from(b)).collect();
    let sec = phpt_sections(&src);
    let Some(file) = sec.get("FILE") else {
        return Outcome::Skip("no FILE".into());
    };
    for unsupported in [
        "CGI",
        "ARGS",
        "STDIN",
        "EXPECTREGEX",
        "FILE_EXTERNAL",
        "PHPDBG",
        "SKIPIF_EXT",
    ] {
        if sec.contains_key(unsupported) {
            return Outcome::Skip(format!("--{unsupported}--"));
        }
    }
    if let Some(skip) = sec.get("SKIPIF") {
        for needle in ["getenv(\"SKIP_ONLINE", "SKIP_SLOW", "PHP_ZTS"] {
            if skip.contains(needle) {
                return Outcome::Skip(format!("SKIPIF {needle}"));
            }
        }
        // Evaluate the remaining conditions like run-tests.php does.
        let mut probe = TestHost::new();
        probe.base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        probe.current_file = path.to_path_buf();
        let code: Vec<u8> = skip
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        let _ = run_file(&mut probe, path, &code);
        let out = String::from_utf8_lossy(&probe.finish())
            .trim()
            .to_ascii_lowercase();
        if out.starts_with("skip") {
            return Outcome::Skip(format!("SKIPIF {out}"));
        }
    }
    if file.contains("stream_socket_server") || file.contains("proc_open") {
        return Outcome::Skip("network server".into());
    }
    let (expected, fmt) = match (sec.get("EXPECT"), sec.get("EXPECTF")) {
        (Some(e), _) => (e.clone(), false),
        (_, Some(e)) => (e.clone(), true),
        _ => return Outcome::Skip("no EXPECT".into()),
    };
    let mut host = TestHost::new();
    // `php_cli_server_start()` is emulated in-process by the interpreter.
    host.include_override
        .insert("php_cli_server.inc".into(), "<?php\n".into());
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let script: PathBuf = path.with_extension("php");
    host.base_dir = dir;
    host.script = script.to_string_lossy().into_owned();
    host.current_file.clone_from(&script);
    host.ini
        .insert("soap.wsdl_cache_enabled".into(), b"1".to_vec());
    if let Some(ini) = sec.get("INI") {
        for line in ini.lines() {
            if let Some((k, v)) = line.split_once('=') {
                host.ini
                    .insert(k.trim().to_owned(), v.trim().as_bytes().to_vec());
            }
        }
    }
    if let Some(get) = sec.get("GET") {
        host.server_vars
            .insert("REQUEST_METHOD".into(), b"GET".to_vec());
        host.server_vars
            .insert("QUERY_STRING".into(), get.trim().as_bytes().to_vec());
    }
    // run-tests.php: POST bodies are trimmed; GZIP_POST/DEFLATE_POST are
    // compressed at level 9 and announced via Content-Encoding.
    for (section, encoding) in [
        ("POST", None),
        ("POST_RAW", None),
        ("GZIP_POST", Some("gzip")),
        ("DEFLATE_POST", Some("deflate")),
    ] {
        let Some(post) = sec.get(section) else {
            continue;
        };
        host.server_vars
            .insert("REQUEST_METHOD".into(), b"POST".to_vec());
        let mut body: Vec<u8> = post
            .trim()
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        if let Some(enc) = encoding {
            body = if enc == "gzip" {
                php_zlib::codec::gzencode(&body, 9, 31)
            } else {
                php_zlib::codec::gzcompress(&body, 9, 15)
            }
            .expect("compress POST body");
            host.server_vars
                .insert("HTTP_CONTENT_ENCODING".into(), enc.as_bytes().to_vec());
        }
        host.request_body = Some(body);
        break;
    }
    php_soap::globals::request_init();
    php_soap::globals::with(|g| g.mem_cache.clear());
    let code: Vec<u8> = file
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    if let Err(e) = run_file(&mut host, &script, &code) {
        host.report(&e);
    }
    let out = host.finish();
    let out: String = out.iter().map(|&b| char::from(b)).collect();
    let out = out.into_bytes();
    if output_matches(&expected, &out, fmt) {
        Outcome::Pass
    } else {
        Outcome::Fail(format!(
            "--- expected\n{}\n--- actual\n{}",
            expected.trim(),
            String::from_utf8_lossy(&out).trim()
        ))
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            if p.file_name()
                .is_some_and(|n| n != "schema" && n != "soap12")
            {
                collect(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "phpt") {
            out.push(p);
        }
    }
}

#[test]
fn soap_phpt_suite() {
    let Some(dir) = soap_tests_dir() else {
        eprintln!("reference tests not available; skipping");
        return;
    };
    let filter = std::env::var("SOAP_PHPT").ok();
    let verbose = std::env::var("SOAP_PHPT_VERBOSE").is_ok();
    let flt2 = filter.clone();
    let (pass, skip, fails) = with_big_stack(move || {
        let filter = flt2;
        let mut files = Vec::new();
        collect(&dir, &mut files);
        files.sort();
        let mut pass = 0;
        let mut skip = 0;
        let mut fails = Vec::new();
        for f in &files {
            let name = f
                .strip_prefix(&dir)
                .unwrap_or(f)
                .to_string_lossy()
                .into_owned();
            if let Some(flt) = &filter {
                if !name.contains(flt.as_str()) {
                    continue;
                }
            }
            if std::env::var("SOAP_PHPT_TRACE").is_ok() {
                eprintln!("RUN {name}");
            }
            match run_phpt(f) {
                Outcome::Pass => pass += 1,
                Outcome::Skip(why) => {
                    skip += 1;
                    if verbose {
                        eprintln!("SKIP {name}: {why}");
                    }
                }
                Outcome::Fail(d) => fails.push((name, d)),
            }
        }
        (pass, skip, fails)
    });
    for (n, d) in &fails {
        if verbose {
            eprintln!("FAIL {n}\n{d}\n");
        } else {
            eprintln!("FAIL {n}");
        }
    }
    eprintln!(
        "phpt: {pass} passed, {skip} skipped, {} failed",
        fails.len()
    );
    let unexpected: Vec<&String> = fails
        .iter()
        .map(|(n, _)| n)
        .filter(|n| !ENGINE_DEPENDENT.contains(&n.as_str()))
        .collect();
    assert!(unexpected.is_empty(), "unexpected failures: {unexpected:?}");
    if filter.is_none() {
        let stale: Vec<&&str> = ENGINE_DEPENDENT
            .iter()
            .filter(|n| !fails.iter().any(|(f, _)| f == *n))
            .collect();
        assert!(
            stale.is_empty(),
            "now passing, drop from ENGINE_DEPENDENT: {stale:?}"
        );
        assert!(pass >= 440, "only {pass} phpt tests passed");
    }
}
