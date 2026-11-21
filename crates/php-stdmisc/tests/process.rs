// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `proc_open` / `exec` round trips with real child processes (expectations
//! hand-ported from `ext/standard/tests/general_functions/proc_open*.phpt`,
//! `exec*.phpt` and checked against /usr/bin/php).

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use php_runtime::{CollectingContext, Resource, RtResult, Value, ZArray};
use php_stdmisc::exec;
use php_stdmisc::process::{proc_open, CommandArg, ParentPipe, ProcHost, Process};

mod common;
use common::export;

struct Host;

impl ProcHost for Host {
    fn open_file_fd(&mut self, path: &[u8], mode: &[u8]) -> RtResult<Option<OwnedFd>> {
        let p = std::str::from_utf8(path).unwrap();
        let f = if mode.starts_with(b"w") {
            File::create(p)
        } else if mode.starts_with(b"a") {
            File::options().append(true).create(true).open(p)
        } else {
            File::open(p)
        };
        Ok(f.ok().map(OwnedFd::from))
    }
    fn stream_fd(&mut self, r: &Resource) -> RtResult<Option<RawFd>> {
        Ok(r.data()
            .as_ref()
            .and_then(|d| d.downcast_ref::<RawFd>().copied()))
    }
}

fn spec(items: Vec<(i64, Value)>) -> ZArray {
    ZArray::from_pairs(items)
}

fn arr(items: &[&str]) -> Value {
    Value::Array(ZArray::from_values(items.iter().map(|s| Value::from(*s))))
}

fn open(
    cmd: CommandArg<'_>,
    spec: &ZArray,
    cwd: Option<&[u8]>,
    env: Option<&ZArray>,
    ctx: &mut CollectingContext,
) -> RtResult<Option<(Process, Vec<ParentPipe>)>> {
    proc_open(cmd, spec, cwd, env, &mut Host, ctx)
}

fn read_all(p: ParentPipe) -> String {
    let mut f = File::from(p.fd);
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    s
}

#[test]
fn pipes_round_trip() {
    let mut ctx = CollectingContext::new();
    let sp = spec(vec![(0, arr(&["pipe", "r"])), (1, arr(&["pipe", "w"]))]);
    let (mut proc, mut pipes) = open(CommandArg::Shell(b"cat"), &sp, None, None, &mut ctx)
        .unwrap()
        .unwrap();
    assert_eq!(pipes.len(), 2);
    assert_eq!((pipes[0].index, pipes[0].mode), (0, "w"));
    assert_eq!((pipes[1].index, pipes[1].mode), (1, "r"));
    let out = pipes.pop().unwrap();
    let mut stdin = File::from(pipes.pop().unwrap().fd);
    stdin.write_all(b"hello\nworld").unwrap();
    drop(stdin);
    assert_eq!(read_all(out), "hello\nworld");
    assert_eq!(proc.close(true), 0);
}

#[test]
fn argv_form_env_cwd() {
    let mut ctx = CollectingContext::new();
    let sp = spec(vec![(1, arr(&["pipe", "w"])), (2, arr(&["redirect", "1"]))]);
    let sp2 = spec(vec![(1, arr(&["pipe", "w"]))]);
    // redirect target must be an int
    let e = open(CommandArg::Shell(b"x"), &sp, None, None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("Redirection target must be of type int, string given")
    );
    let env = ZArray::from_pairs([("A", Value::from("b")), ("E", Value::from(""))]);
    let argv = ZArray::from_values([
        Value::from("sh"),
        Value::from("-c"),
        Value::from("echo $A-$E-$(pwd)"),
    ]);
    let (mut proc, mut pipes) = open(
        CommandArg::Argv(&argv),
        &sp2,
        Some(b"/tmp"),
        Some(&env),
        &mut ctx,
    )
    .unwrap()
    .unwrap();
    assert_eq!(read_all(pipes.pop().unwrap()), "b--/tmp\n");
    assert_eq!(proc.command().as_bytes(), b"sh");
    assert_eq!(proc.close(true), 0);
}

#[test]
fn status_and_exit_code_caching() {
    let mut ctx = CollectingContext::new();
    let (mut proc, _) = open(
        CommandArg::Shell(b"exit 3"),
        &ZArray::new(),
        None,
        None,
        &mut ctx,
    )
    .unwrap()
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let st = proc.status();
    let st = export(&Value::Array(st));
    assert!(st.contains("'cached' => true"), "{st}");
    assert!(st.contains("'running' => false"));
    assert!(st.contains("'exitcode' => 3"));
    // Cached: still reported after the first read.
    assert!(export(&Value::Array(proc.status())).contains("'exitcode' => 3"));
    assert_eq!(proc.close(true), 3);
}

/// The state letter of `/proc/<pid>/stat` (`S`, `R`, `Z`, ...), `None`
/// once the pid is gone.
fn proc_state(pid: i64) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..].trim_start().chars().next()
}

/// A resource freed without `proc_close()` leaves its child running (as
/// in php-src) but does not leave it a zombie once it ends: the reaper
/// collects it. Without the reaper it stays `Z` until this process exits.
#[test]
fn unclosed_child_runs_on_and_is_collected() {
    let mut ctx = CollectingContext::new();
    let (mut proc, _) = open(
        CommandArg::Shell(b"exec sleep 1"),
        &ZArray::new(),
        None,
        None,
        &mut ctx,
    )
    .unwrap()
    .unwrap();
    let pid = proc.pid();
    assert_eq!(proc.close(false), -1);
    assert!(
        matches!(proc_state(pid), Some(s) if s != 'Z'),
        "{:?}",
        proc_state(pid)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while proc_state(pid).is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(proc_state(pid), None, "pid {pid} not collected");
}

#[test]
fn terminate_reports_signal() {
    let mut ctx = CollectingContext::new();
    let (mut proc, _) = open(
        CommandArg::Shell(b"sleep 5"),
        &ZArray::new(),
        None,
        None,
        &mut ctx,
    )
    .unwrap()
    .unwrap();
    assert!(proc.terminate(php_stdmisc::process::SIGTERM));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let st = export(&Value::Array(proc.status()));
    assert!(
        st.contains("'signaled' => true") && st.contains("'termsig' => 15"),
        "{st}"
    );
    assert!(st.contains("'cached' => false"));
    // The signal status was consumed by proc_get_status(): proc_close() gets -1.
    assert_eq!(proc.close(true), -1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn errors_and_warnings() {
    let mut ctx = CollectingContext::new();
    let s = |v: Vec<(i64, Value)>| spec(v);
    let bad = ZArray::from_pairs([("a", Value::Int(1))]);
    let e = open(CommandArg::Shell(b"x"), &bad, None, None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("proc_open(): Argument #2 ($descriptor_spec) must be an integer indexed array")
    );
    let e = open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, Value::Int(1))]),
        None,
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        Some("proc_open(): Argument #2 ($descriptor_spec) must only contain arrays and streams")
    );
    let e = open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, arr(&[]))]),
        None,
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(e.message(), Some("Missing handle qualifier"));
    let e = open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, arr(&["pipe"]))]),
        None,
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(e.message(), Some("Missing mode parameter for 'pipe'"));
    let e = open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, arr(&["file", "x"]))]),
        None,
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(e.message(), Some("Missing mode parameter for 'file'"));
    let e = open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, arr(&["redirect"]))]),
        None,
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(e.message(), Some("Missing redirection target"));
    assert!(open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, arr(&["foo"]))]),
        None,
        None,
        &mut ctx
    )
    .unwrap()
    .is_none());
    let redirect7 = Value::Array(ZArray::from_values([
        Value::from("redirect"),
        Value::Int(7),
    ]));
    assert!(open(
        CommandArg::Shell(b"x"),
        &s(vec![(0, redirect7)]),
        None,
        None,
        &mut ctx
    )
    .unwrap()
    .is_none());
    let empty = ZArray::new();
    let e = open(CommandArg::Argv(&empty), &empty, None, None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("proc_open(): Argument #1 ($command) must not be empty")
    );
    let a = ZArray::from_values([Value::from("")]);
    let e = open(CommandArg::Argv(&a), &empty, None, None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("First element must contain a non-empty program name")
    );
    let a = ZArray::from_values([Value::from("a"), Value::from("b\0")]);
    let e = open(CommandArg::Argv(&a), &empty, None, None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("Command array element 2 contains a null byte")
    );
    let a = ZArray::from_values([Value::from("nonexistent_cmd_x")]);
    assert!(open(CommandArg::Argv(&a), &empty, None, None, &mut ctx)
        .unwrap()
        .is_none());
    let e = open(
        CommandArg::Shell(b"x"),
        &empty,
        Some(b"/tm\0p"),
        None,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        Some("proc_open(): Argument #4 ($cwd) must not contain any null bytes")
    );
    assert_eq!(
        ctx.messages(),
        [
            "Warning: proc_open(): foo is not a valid descriptor spec/mode",
            "Warning: proc_open(): Redirection target 7 not found",
            "Warning: proc_open(): posix_spawn() failed: No such file or directory",
        ]
    );
}

#[test]
fn null_file_pty_socket_and_resource() {
    let mut ctx = CollectingContext::new();
    let dir = std::env::temp_dir().join(format!("stdmisc-proc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out.txt");
    let file = Value::Array(ZArray::from_values([
        Value::from("file"),
        Value::from(out.to_str().unwrap()),
        Value::from("w"),
    ]));
    let sp = spec(vec![(1, file), (2, arr(&["null"]))]);
    let (mut proc, pipes) = open(
        CommandArg::Shell(b"echo to-file; echo err >&2"),
        &sp,
        None,
        None,
        &mut ctx,
    )
    .unwrap()
    .unwrap();
    assert!(pipes.is_empty());
    assert_eq!(proc.close(true), 0);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "to-file\n");

    // A stream resource as a descriptor (its fd is dup()ed).
    let f = File::options().append(true).open(&out).unwrap();
    let res = Resource::new("stream", Some(Box::new(f.as_raw_fd())));
    let sp = spec(vec![(1, Value::Resource(res))]);
    let (mut proc, _) = open(CommandArg::Shell(b"echo again"), &sp, None, None, &mut ctx)
        .unwrap()
        .unwrap();
    assert_eq!(proc.close(true), 0);
    drop(f);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "to-file\nagain\n");

    let sp = spec(vec![(0, arr(&["pty"])), (1, arr(&["pty"]))]);
    let (mut proc, pipes) = open(CommandArg::Shell(b"true"), &sp, None, None, &mut ctx)
        .unwrap()
        .unwrap();
    assert_eq!(
        pipes.iter().map(|p| p.mode).collect::<Vec<_>>(),
        ["r+", "r+"]
    );
    drop(pipes);
    assert_eq!(proc.close(true), 0);

    let sp = spec(vec![(1, arr(&["socket"]))]);
    let (mut proc, mut pipes) = open(CommandArg::Shell(b"printf sock"), &sp, None, None, &mut ctx)
        .unwrap()
        .unwrap();
    assert!(pipes[0].socket);
    assert_eq!(read_all(pipes.pop().unwrap()), "sock");
    assert_eq!(proc.close(true), 0);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(ctx.messages().is_empty(), "{:?}", ctx.messages());
}

#[test]
fn exec_family() {
    let mut ctx = CollectingContext::new();
    let (v, out, rc) =
        exec::exec(b"printf 'a \\nb\\t\\n\\n  '", Some(Value::Null), &mut ctx).unwrap();
    assert_eq!(export(&v), "''");
    assert_eq!(
        export(&Value::Array(out.unwrap())),
        "array (\n  0 => 'a',\n  1 => 'b',\n  2 => '',\n  3 => '',\n)"
    );
    assert_eq!(rc, 0);
    // Existing arrays are appended to.
    let prev = Value::Array(ZArray::from_values([Value::from("x")]));
    let (_, out, _) = exec::exec(b"echo y", Some(prev), &mut ctx).unwrap();
    assert_eq!(out.unwrap().len(), 2);
    let (v, _, rc) = exec::exec(b"echo first; echo last; exit 7", None, &mut ctx).unwrap();
    assert_eq!((export(&v), rc), ("'last'".to_string(), 7));

    assert!(exec::shell_exec(b"true", &mut ctx).unwrap().is_null());
    assert_eq!(
        export(&exec::shell_exec(b"echo x; exit 4", &mut ctx).unwrap()),
        "'x\n'"
    );

    let mut printed = Vec::new();
    let r = exec::system(
        b"echo a; echo b; exit 2",
        &mut |b| printed.extend_from_slice(b),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        (export(&r.value), r.status, &printed[..]),
        ("'b'".to_string(), 2, &b"a\nb\n"[..])
    );
    let mut printed = Vec::new();
    let r = exec::passthru(
        b"printf ab",
        &mut |b| printed.extend_from_slice(b),
        &mut ctx,
    )
    .unwrap();
    assert!(r.value.is_null());
    assert_eq!((r.status, &printed[..]), (0, &b"ab"[..]));
    // Killed by a signal: the raw wait status is reported.
    let (_, _, rc) = exec::exec(b"kill -9 $$", None, &mut ctx).unwrap();
    assert_eq!(rc, 9);

    let e = exec::exec(b"", None, &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("exec(): Argument #1 ($command) must not be empty")
    );
    let e = exec::shell_exec(b"a\0b", &mut ctx).unwrap_err();
    assert_eq!(
        e.message(),
        Some("shell_exec(): Argument #1 ($command) must not contain any null bytes")
    );
    let e = exec::escapeshellarg(b"a\0b").unwrap_err();
    assert_eq!(
        e.message(),
        Some("escapeshellarg(): Argument #1 ($arg) must not contain any null bytes")
    );
    let e = exec::escapeshellcmd(b"a\0b").unwrap_err();
    assert_eq!(
        e.message(),
        Some("escapeshellcmd(): Argument #1 ($command) must not contain any null bytes")
    );
}
