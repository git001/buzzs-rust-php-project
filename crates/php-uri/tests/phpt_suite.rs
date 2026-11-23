// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Runs `modules/php-src/ext/uri/tests/**/*.phpt` through a miniature PHP
//! evaluator wired to `php_uri::api`, and checks the pass rate.
//!
//! Set `PHP_URI_PHPT_VERBOSE=1` to print every failure, or
//! `PHP_URI_PHPT_FILTER=substring` to run a subset.

// The evaluator is test tooling that mirrors PHP's dynamic semantics; these
// pedantic lints add noise without improving it.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::many_single_char_names,
    clippy::unused_self,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::wrong_self_convention,
    clippy::items_after_statements
)]

mod phpt;

use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "phpt") {
            out.push(p);
        }
    }
}

#[test]
fn ext_uri_phpt_suite() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/php-src/ext/uri/tests");
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let mut files = Vec::new();
    collect(&root, &mut files);
    let filter = std::env::var("PHP_URI_PHPT_FILTER").ok();
    let verbose = std::env::var("PHP_URI_PHPT_VERBOSE").is_ok();
    let (mut pass, mut fail, mut unsupported) = (0, 0, 0);
    let mut failures = Vec::new();
    for f in &files {
        let name = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        if filter
            .as_ref()
            .is_some_and(|flt| !name.contains(flt.as_str()))
        {
            continue;
        }
        match phpt::run_file(f) {
            phpt::Outcome::Pass => pass += 1,
            phpt::Outcome::Fail { expected, actual } => {
                fail += 1;
                if verbose {
                    eprintln!("FAIL {name}\n--- expected\n{expected}\n--- actual\n{actual}\n");
                }
                failures.push(name);
            }
            phpt::Outcome::Unsupported(why) => {
                unsupported += 1;
                if verbose {
                    eprintln!("SKIP {name}: {why}");
                }
            }
        }
    }
    eprintln!(
        "ext/uri phpt: {pass} passed, {fail} failed, {unsupported} unsupported by the mini evaluator ({} total)",
        pass + fail + unsupported
    );
    for f in &failures {
        eprintln!("  failed: {f}");
    }
    if filter.is_none() {
        assert_eq!(fail, 0, "phpt failures: {failures:?}");
        assert!(
            pass >= MIN_PASSING,
            "only {pass} tests evaluated successfully"
        );
    }
}

/// The number of tests the evaluator is known to run successfully (the
/// rest need VM features such as class declarations, INI settings or
/// `zend_test`).
const MIN_PASSING: usize = 582;
