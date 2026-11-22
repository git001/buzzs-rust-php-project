// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The hard ceiling kills the process, so each scenario runs in a child
//! process: the parent test re-executes this test binary filtered to a
//! `child_*` test with `PHP_MEMLIMIT_CHILD` set.

use std::hint::black_box;
use std::io::Write;
use std::process::{Command, Output};

php_memlimit::install_global_allocator!();

const M: usize = 1024 * 1024;
const CHILD_ENV: &str = "PHP_MEMLIMIT_CHILD";

fn run_child(name: &str) -> Output {
    let exe = std::env::current_exe().expect("test binary path");
    Command::new(exe)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn child")
}

fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

#[test]
fn child_null_backstop() {
    if !is_child() {
        return;
    }
    php_memlimit::request_start(Some(10 * M)).expect("limit");
    let v = black_box(vec![0u8; 100 * M]);
    println!("UNREACHABLE {}", v.len());
}

fn exit_137(e: php_memlimit::HardLimitExceeded) -> ! {
    let _ = writeln!(std::io::stderr(), "{e} in test.php on line 5");
    std::process::exit(137);
}

#[test]
fn child_handler() {
    if !is_child() {
        return;
    }
    assert!(php_memlimit::set_hard_limit_handler(exit_137));
    php_memlimit::request_start(Some(10 * M)).expect("limit");
    let v = black_box(vec![0u8; 100 * M]);
    println!("UNREACHABLE {}", v.len());
}

#[test]
fn hard_ceiling_aborts_via_alloc_error() {
    if is_child() {
        return;
    }
    let out = run_child("child_null_backstop");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("memory allocation of 104857600 bytes failed"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("UNREACHABLE"));
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(6), "SIGABRT");
    }
}

#[test]
fn hard_ceiling_calls_handler() {
    if is_child() {
        return;
    }
    let out = run_child("child_handler");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(137), "{stderr}");
    assert!(
        stderr.contains(
            "Hard memory limit of 20971520 bytes exhausted (tried to allocate 104857600 bytes) in test.php on line 5"
        ),
        "{stderr}"
    );
}

#[test]
fn child_trap() {
    if !is_child() {
        return;
    }
    assert!(php_memlimit::set_hard_limit_handler(
        php_memlimit::trap_hard_limit
    ));
    php_memlimit::request_start(Some(10 * M)).expect("limit");
    let v = black_box(vec![0u8; 100 * M]);
    println!("UNREACHABLE {}", v.len());
}

#[test]
fn hard_ceiling_trap_names_the_growth() {
    if is_child() {
        return;
    }
    let out = run_child("child_trap");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "Hard memory limit of 20971520 bytes exhausted (tried to allocate 104857600 bytes): growth not checked against memory_limit"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("child_trap"),
        "backtrace names the caller: {stderr}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("UNREACHABLE"));
    assert_eq!(
        out.status.code(),
        Some(php_memlimit::TRAP_EXIT_STATUS),
        "an exit, not a signal (no core dump)"
    );
}
