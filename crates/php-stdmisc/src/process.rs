// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `proc_open()` and friends (`ext/standard/proc_open.c`, POSIX
//! `posix_spawn` flavour).
//!
//! The VM owns the stream layer, so this module talks to it through the
//! [`ProcHost`] trait (to open `["file", path, mode]` descriptors and to get
//! the descriptor of stream resources) and hands back the parent ends of
//! pipes/sockets/PTYs as [`ParentPipe`]s (owned descriptors plus the
//! `fopen` mode) for the VM to wrap into streams and store in `$pipes`
//! (with `PHP_STREAM_FLAG_NO_SEEK`).
//!
//! The returned [`Process`] is the `process` resource: [`Process::status`]
//! is `proc_get_status()`, [`Process::close`] is the resource destructor
//! (`proc_close()` passes `wait = true` after the VM closed the pipes),
//! [`Process::terminate`] is `proc_terminate()`.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use php_runtime::{
    ErrorLevel, Resource, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};

use crate::reaper;
use crate::sys::{self, FileAction, SpawnError};

/// What the VM's stream layer provides to `proc_open()`.
pub trait ProcHost {
    /// `php_stream_open_wrapper(path, mode, REPORT_ERRORS|STREAM_WILL_CAST)`
    /// followed by a releasing cast to a file descriptor. `Ok(None)` means
    /// failure (warnings already reported).
    fn open_file_fd(&mut self, path: &[u8], mode: &[u8]) -> RtResult<Option<OwnedFd>>;

    /// The descriptor of a stream resource (`php_stream_cast(...,
    /// PHP_STREAM_AS_FD)`), not released. `Ok(None)` means failure
    /// (warnings or the resource type error already reported).
    fn stream_fd(&mut self, resource: &Resource) -> RtResult<Option<RawFd>>;
}

/// The `$command` argument.
#[derive(Clone, Copy, Debug)]
pub enum CommandArg<'a> {
    /// A shell command line run with `/bin/sh -c`.
    Shell(&'a [u8]),
    /// `[program, arg1, ...]` executed directly (with `PATH` lookup).
    Argv(&'a ZArray),
}

/// The parent end of a `pipe`/`pty`/`socket` descriptor.
#[derive(Debug)]
pub struct ParentPipe {
    /// The key in `$pipes` (the child descriptor number).
    pub index: i64,
    /// The descriptor (close-on-exec).
    pub fd: OwnedFd,
    /// The stream mode: `"r"`, `"w"` or `"r+"` (PTY).
    pub mode: &'static str,
    /// Whether this is a socket (wrap with a socket stream).
    pub socket: bool,
}

/// A process started by [`proc_open`] (the `process` resource).
#[derive(Debug)]
pub struct Process {
    command: ZStr,
    pid: libc::pid_t,
    cached_exit_status: Option<i32>,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescType {
    Std,
    Pipe,
    Socket,
}

#[derive(Debug)]
struct Descriptor {
    index: i32,
    kind: DescType,
    childend: OwnedFd,
    parentend: Option<OwnedFd>,
    mode: &'static str,
}

fn warn(ctx: &mut (impl RuntimeContext + ?Sized), msg: &str) -> RtResult<()> {
    ctx.diagnostic(ErrorLevel::Warning, &format!("proc_open(): {msg}"))
}

/// `get_string_parameter()`: `Ok(None)` cannot happen (missing → error).
fn string_param(
    ht: &ZArray,
    index: i64,
    name: &str,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<ZStr> {
    match ht.get_int(index) {
        None => Err(RuntimeError::ValueError(format!("Missing {name}"))),
        Some(v) => v.to_zstr(ctx),
    }
}

/// `php_array_to_envp()`.
fn array_to_envp(env: &ZArray, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for (k, v) in env.iter() {
        let s = v.to_zstr(ctx)?;
        if s.is_empty() {
            continue;
        }
        let key = match k {
            php_runtime::ArrayKey::Str(k) if !k.is_empty() => Some(k.as_bytes().to_vec()),
            _ => None,
        };
        let mut e = Vec::new();
        if let Some(k) = key {
            e.extend_from_slice(&k);
            e.push(b'=');
        }
        e.extend_from_slice(s.as_bytes());
        out.push(e);
    }
    Ok(out)
}

/// `get_command_from_array()`: `(program, argv)`.
fn command_from_array(
    arr: &ZArray,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<(ZStr, Vec<Vec<u8>>)> {
    let mut argv = Vec::new();
    for (i, v) in arr.values().enumerate() {
        let s = v.to_zstr(ctx)?;
        if i == 0 && s.is_empty() {
            return Err(RuntimeError::ValueError(
                "First element must contain a non-empty program name".into(),
            ));
        }
        if s.as_bytes().contains(&0) {
            return Err(RuntimeError::ValueError(format!(
                "Command array element {} contains a null byte",
                i + 1
            )));
        }
        argv.push(s.as_bytes().to_vec());
    }
    let command = ZStr::new(argv[0].clone());
    Ok((command, argv))
}

struct Pty {
    master: Option<OwnedFd>,
    slave: Option<OwnedFd>,
}

/// Outcome of one descriptor: `Ok(None)` = failure with warning.
type DescResult = RtResult<Option<Descriptor>>;

#[allow(clippy::too_many_lines)]
fn desc_from_array(
    ht: &ZArray,
    descriptors: &[Descriptor],
    nindex: i32,
    pty: &mut Pty,
    host: &mut dyn ProcHost,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> DescResult {
    let ztype = string_param(ht, 0, "handle qualifier", ctx)?;
    let std = |fd: OwnedFd| Descriptor {
        index: nindex,
        kind: DescType::Std,
        childend: fd,
        parentend: None,
        mode: "",
    };
    match ztype.as_bytes() {
        b"pipe" => {
            let zmode = string_param(ht, 1, "mode parameter for 'pipe'", ctx)?;
            let (r, w) = match sys::pipe() {
                Ok(p) => p,
                Err(e) => {
                    warn(ctx, &format!("Unable to create pipe {}", sys::strerror(e)))?;
                    return Ok(None);
                }
            };
            let (parent, child, mode) = if zmode.as_bytes().starts_with(b"w") {
                (r, w, "r")
            } else {
                (w, r, "w")
            };
            Ok(Some(Descriptor {
                index: nindex,
                kind: DescType::Pipe,
                childend: child,
                parentend: Some(parent),
                mode,
            }))
        }
        b"socket" => match sys::socketpair() {
            Ok((a, b)) => Ok(Some(Descriptor {
                index: nindex,
                kind: DescType::Socket,
                childend: b,
                parentend: Some(a),
                mode: "r+",
            })),
            Err(e) => {
                warn(
                    ctx,
                    &format!("Unable to create socket pair: {}", sys::strerror(e)),
                )?;
                Ok(None)
            }
        },
        b"file" => {
            let zfile = string_param(ht, 1, "file name parameter for 'file'", ctx)?;
            let zmode = string_param(ht, 2, "mode parameter for 'file'", ctx)?;
            Ok(host
                .open_file_fd(zfile.as_bytes(), zmode.as_bytes())?
                .map(std))
        }
        b"redirect" => {
            let target = match ht.get_int(1).map(Value::deref) {
                None => {
                    return Err(RuntimeError::ValueError(
                        "Missing redirection target".into(),
                    ))
                }
                Some(Value::Int(t)) => t,
                Some(v) => {
                    return Err(RuntimeError::ValueError(format!(
                        "Redirection target must be of type int, {} given",
                        v.value_name()
                    )))
                }
            };
            #[allow(clippy::cast_possible_truncation)]
            let target = target as i32;
            let redirect_to = if let Some(d) = descriptors.iter().find(|d| d.index == target) {
                d.childend.as_raw_fd()
            } else {
                if !(0..=2).contains(&target) {
                    warn(ctx, &format!("Redirection target {target} not found"))?;
                    return Ok(None);
                }
                target
            };
            match sys::dup(redirect_to) {
                Ok(fd) => Ok(Some(std(fd))),
                Err(e) => {
                    warn(
                        ctx,
                        &format!(
                            "Failed to dup() for descriptor {nindex}: {}",
                            sys::strerror(e)
                        ),
                    )?;
                    Ok(None)
                }
            }
        }
        b"null" => match sys::open_dev_null() {
            Ok(fd) => Ok(Some(std(fd))),
            Err(e) => {
                warn(
                    ctx,
                    &format!("Failed to open /dev/null: {}", sys::strerror(e)),
                )?;
                Ok(None)
            }
        },
        b"pty" => {
            if pty.master.is_none() {
                match sys::openpty() {
                    Ok((m, s)) => {
                        pty.master = Some(m);
                        pty.slave = Some(s);
                    }
                    Err(e) => {
                        warn(
                            ctx,
                            &format!("Could not open PTY (pseudoterminal): {}", sys::strerror(e)),
                        )?;
                        return Ok(None);
                    }
                }
            }
            let dup_of = |fd: &Option<OwnedFd>| fd.as_ref().map(|f| sys::dup(f.as_raw_fd()));
            if let (Some(Ok(child)), Some(Ok(parent))) = (dup_of(&pty.slave), dup_of(&pty.master)) {
                Ok(Some(Descriptor {
                    index: nindex,
                    kind: DescType::Pipe,
                    childend: child,
                    parentend: Some(parent),
                    mode: "r+",
                }))
            } else {
                warn(ctx, "Could not open PTY (pseudoterminal): dup failed")?;
                Ok(None)
            }
        }
        other => {
            warn(
                ctx,
                &format!(
                    "{} is not a valid descriptor spec/mode",
                    String::from_utf8_lossy(other)
                ),
            )?;
            Ok(None)
        }
    }
}

/// `proc_open($command, $descriptor_spec, &$pipes, $cwd, $env_vars,
/// $options)`. `Ok(None)` is the `false` return (warnings reported).
/// `$options` is accepted and ignored (its keys are Windows-only).
#[allow(clippy::too_many_lines)]
pub fn proc_open(
    command: CommandArg<'_>,
    descriptorspec: &ZArray,
    cwd: Option<&[u8]>,
    env: Option<&ZArray>,
    host: &mut dyn ProcHost,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<Option<(Process, Vec<ParentPipe>)>> {
    let (command_str, argv) = match command {
        CommandArg::Shell(s) => {
            if s.contains(&0) {
                return Err(RuntimeError::ValueError(
                    "proc_open(): Argument #1 ($command) must not contain any null bytes".into(),
                ));
            }
            (ZStr::new(s), None)
        }
        CommandArg::Argv(a) => {
            if a.is_empty() {
                return Err(RuntimeError::ValueError(
                    "proc_open(): Argument #1 ($command) must not be empty".into(),
                ));
            }
            let (c, argv) = command_from_array(a, ctx)?;
            (c, Some(argv))
        }
    };
    if cwd.is_some_and(|c| c.contains(&0)) {
        return Err(RuntimeError::ValueError(
            "proc_open(): Argument #4 ($cwd) must not contain any null bytes".into(),
        ));
    }
    let envp = match env {
        Some(e) => Some(array_to_envp(e, ctx)?),
        None => None,
    };

    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut pty = Pty {
        master: None,
        slave: None,
    };
    for (key, item) in descriptorspec.iter() {
        let php_runtime::ArrayKey::Int(nindex) = key else {
            return Err(RuntimeError::ValueError(
                "proc_open(): Argument #2 ($descriptor_spec) must be an integer indexed array"
                    .into(),
            ));
        };
        #[allow(clippy::cast_possible_truncation)]
        let nindex = *nindex as i32;
        let d = match item.deref() {
            Value::Resource(r) => match host.stream_fd(&r)? {
                Some(fd) => match sys::dup(fd) {
                    Ok(fd) => Some(Descriptor {
                        index: nindex,
                        kind: DescType::Std,
                        childend: fd,
                        parentend: None,
                        mode: "",
                    }),
                    Err(e) => {
                        warn(
                            ctx,
                            &format!(
                                "Failed to dup() for descriptor {}: {}",
                                descriptors.len(),
                                sys::strerror(e)
                            ),
                        )?;
                        None
                    }
                },
                None => None,
            },
            Value::Array(a) => desc_from_array(&a, &descriptors, nindex, &mut pty, host, ctx)?,
            _ => return Err(RuntimeError::ValueError(
                "proc_open(): Argument #2 ($descriptor_spec) must only contain arrays and streams"
                    .into(),
            )),
        };
        match d {
            Some(d) => descriptors.push(d),
            None => return Ok(None),
        }
    }

    // close_parentends_of_pipes() as posix_spawn file actions.
    let mut actions = Vec::new();
    for d in &descriptors {
        if let Some(p) = &d.parentend {
            actions.push(FileAction::Close(p.as_raw_fd()));
        }
        let child = d.childend.as_raw_fd();
        if child == d.index {
            // Same number: a self-dup2 clears close-on-exec in the child.
            actions.push(FileAction::Dup2(child, d.index));
        } else {
            actions.push(FileAction::Dup2(child, d.index));
            actions.push(FileAction::Close(child));
        }
    }
    let chdir_index = cwd.map(|c| {
        actions.push(FileAction::Chdir(c.to_vec()));
        actions.len() - 1
    });
    let mut chdir_failed = None;
    let mut on_err = |i: usize, e: i32| {
        if Some(i) == chdir_index {
            chdir_failed = Some(e);
            true
        } else {
            false
        }
    };
    let result = match &argv {
        Some(argv) => sys::spawn(
            command_str.as_bytes(),
            argv,
            envp.as_deref(),
            &actions,
            true,
            &mut on_err,
        ),
        None => sys::spawn(
            b"/bin/sh",
            &[
                b"sh".to_vec(),
                b"-c".to_vec(),
                command_str.as_bytes().to_vec(),
            ],
            envp.as_deref(),
            &actions,
            false,
            &mut on_err,
        ),
    };
    if let Some(e) = chdir_failed {
        warn(
            ctx,
            &format!(
                "posix_spawn_file_actions_addchdir_np() failed: {}",
                sys::strerror(e)
            ),
        )?;
    }
    let pid = match result {
        Ok(pid) => {
            reaper::forget(pid);
            pid
        }
        Err(SpawnError::Spawn(e)) => {
            warn(ctx, &format!("posix_spawn() failed: {}", sys::strerror(e)))?;
            return Ok(None);
        }
        Err(SpawnError::Action(i, e)) => {
            let msg = match &actions[i] {
                FileAction::Dup2(from, to) => format!(
                    "Unable to copy file descriptor {from} (for pipe) into file descriptor {to}: {}",
                    sys::strerror(e)
                ),
                FileAction::Close(fd) => {
                    format!("Cannot close file descriptor {fd}: {}", sys::strerror(e))
                }
                FileAction::Chdir(_) => sys::strerror(e),
            };
            warn(ctx, &msg)?;
            return Ok(None);
        }
    };

    let mut pipes = Vec::new();
    for d in descriptors {
        drop(d.childend);
        if let Some(fd) = d.parentend {
            pipes.push(ParentPipe {
                index: i64::from(d.index),
                fd,
                mode: d.mode,
                socket: d.kind == DescType::Socket,
            });
        }
    }
    Ok(Some((
        Process {
            command: command_str,
            pid,
            cached_exit_status: None,
            closed: false,
        },
        pipes,
    )))
}

impl Process {
    /// The child's pid.
    #[must_use]
    pub fn pid(&self) -> i64 {
        i64::from(self.pid)
    }

    /// The command string (the program name for the array form).
    #[must_use]
    pub fn command(&self) -> &ZStr {
        &self.command
    }

    /// `waitpid_cached()`.
    fn waitpid_cached(&mut self, options: i32) -> Result<(libc::pid_t, i32), i32> {
        if let Some(st) = self.cached_exit_status {
            return Ok((self.pid, st));
        }
        let r = sys::waitpid(self.pid, options);
        if let Ok((p, st)) = r {
            if p > 0 && libc::WIFEXITED(st) {
                self.cached_exit_status = Some(st);
            }
        }
        r
    }

    /// `proc_get_status()`.
    pub fn status(&mut self) -> ZArray {
        let mut running = true;
        let (mut signaled, mut stopped) = (false, false);
        let (mut exitcode, mut termsig, mut stopsig) = (-1i64, 0i64, 0i64);
        match self.waitpid_cached(libc::WNOHANG | libc::WUNTRACED) {
            Ok((p, st)) if p == self.pid => {
                if libc::WIFEXITED(st) {
                    running = false;
                    exitcode = i64::from(libc::WEXITSTATUS(st));
                }
                if libc::WIFSIGNALED(st) {
                    running = false;
                    signaled = true;
                    termsig = i64::from(libc::WTERMSIG(st));
                }
                if libc::WIFSTOPPED(st) {
                    stopped = true;
                    stopsig = i64::from(libc::WSTOPSIG(st));
                }
            }
            Err(_) => running = false,
            Ok(_) => {}
        }
        ZArray::from_pairs([
            ("command", Value::Str(self.command.clone())),
            ("pid", Value::Int(self.pid())),
            ("cached", Value::Bool(self.cached_exit_status.is_some())),
            ("running", Value::Bool(running)),
            ("signaled", Value::Bool(signaled)),
            ("stopped", Value::Bool(stopped)),
            ("exitcode", Value::Int(exitcode)),
            ("termsig", Value::Int(termsig)),
            ("stopsig", Value::Int(stopsig)),
        ])
    }

    /// `proc_terminate($process, $signal)`.
    #[must_use]
    pub fn terminate(&self, signal: i64) -> bool {
        #[allow(clippy::cast_possible_truncation)]
        sys::kill(self.pid, signal as i32)
    }

    /// The resource destructor (`proc_open_rsrc_dtor`): with `wait`
    /// (`proc_close()`) blocks until the child exits; returns the
    /// `pclose_ret` value (exit code, raw status if signaled, -1 if not
    /// available). The VM must close the pipe streams first. Without
    /// `wait` a child that still runs is left to run and collected by
    /// [`reaper`] once it ends, instead of staying a zombie.
    pub fn close(&mut self, wait: bool) -> i64 {
        if self.closed {
            return -1;
        }
        self.closed = true;
        let options = if wait { 0 } else { libc::WNOHANG };
        let r = loop {
            match self.waitpid_cached(options) {
                Err(libc::EINTR) => {}
                other => break other,
            }
        };
        match r {
            // Still running, and nobody will wait for it: see `reaper`.
            Ok((0, _)) => {
                reaper::adopt(self.pid);
                -1
            }
            Ok((p, st)) if p > 0 => {
                if libc::WIFEXITED(st) {
                    i64::from(libc::WEXITSTATUS(st))
                } else {
                    i64::from(st)
                }
            }
            _ => -1,
        }
    }
}

/// `proc_terminate()`'s default signal (`SIGTERM`).
pub const SIGTERM: i64 = libc::SIGTERM as i64;
