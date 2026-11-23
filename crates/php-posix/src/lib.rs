// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/posix` (ref: `ext/posix/posix.c`, `posix.stub.php`).
//!
//! Every `posix_*` function is a method on [`Posix`], which owns the module
//! global `last_error` (`POSIX_G(last_error)`).  Methods mirror PHP's
//! argument types (`zend_long` → `i64`, `string` → `&[u8]`) and return either
//! the success value or a [`PosixError`] telling the VM what PHP does:
//!
//! * [`PosixError::Failed`] — the call failed, `last_error` has been updated,
//!   the PHP function returns `false`.
//! * [`PosixError::Warning`] — emit `E_WARNING` with the given text (the VM
//!   adds the usual `"posix_xxx(): "` prefix) and return `false`.
//! * [`PosixError::ValueError`] — throw `ValueError` with the given text (the
//!   VM adds the `"posix_xxx(): "` prefix).
//!
//! Composite results ([`Uname`], [`Times`], [`Group`], [`Passwd`],
//! [`Rlimits`]) expose `entries()` giving the exact PHP array keys in order.
//!
//! Argument type juggling (`int|resource` for file descriptors, the
//! "must be of type int|resource" diagnostics, stream → fd casting) and
//! `open_basedir` checks are left to the VM; helpers for the exact texts are
//! provided ([`fd_type_warning`], [`stream_cast_warning`]).

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod sys;

use std::fmt;

/// `POSIX_PID_MIN` on Linux (`INT_MIN`).
pub const PID_MIN: i64 = i32::MIN as i64;
/// `POSIX_PID_MAX` on Linux (`INT_MAX`).
pub const PID_MAX: i64 = i32::MAX as i64;
/// `MAXPATHLEN` on Linux.
pub const MAXPATHLEN: usize = 4096;

/// The `"unlimited"` marker string used by `posix_getrlimit()`.
pub const UNLIMITED: &str = "unlimited";

/// Outcome of a failing `posix_*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PosixError {
    /// The underlying call failed with this errno; `last_error` holds it
    /// (or kept its previous value where PHP does not update it).  Return
    /// `false`.
    Failed(i32),
    /// Emit an `E_WARNING` with this message and return `false`.
    Warning(String),
    /// Throw a `ValueError` with this message.
    ValueError(String),
}

impl fmt::Display for PosixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "failed with errno {e}"),
            Self::Warning(m) | Self::ValueError(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PosixError {}

/// Result alias for `posix_*` operations.
pub type PosixResult<T> = Result<T, PosixError>;

/// A value in a PHP array produced by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// PHP `string`.
    Str(Vec<u8>),
    /// PHP `int`.
    Int(i64),
    /// PHP `null`.
    Null,
    /// A packed list of strings (`members` of a group).
    List(Vec<Vec<u8>>),
}

impl Field {
    fn opt(v: Option<&Vec<u8>>) -> Self {
        v.map_or(Self::Null, |s| Self::Str(s.clone()))
    }
}

/// `posix_uname()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uname {
    pub sysname: Vec<u8>,
    pub nodename: Vec<u8>,
    pub release: Vec<u8>,
    pub version: Vec<u8>,
    pub machine: Vec<u8>,
    /// Present on glibc (`_GNU_SOURCE` + `utsname.domainname`).
    pub domainname: Option<Vec<u8>>,
}

impl Uname {
    /// Array entries in PHP's order.
    pub fn entries(&self) -> Vec<(&'static str, Field)> {
        let mut v = vec![
            ("sysname", Field::Str(self.sysname.clone())),
            ("nodename", Field::Str(self.nodename.clone())),
            ("release", Field::Str(self.release.clone())),
            ("version", Field::Str(self.version.clone())),
            ("machine", Field::Str(self.machine.clone())),
        ];
        if let Some(d) = &self.domainname {
            v.push(("domainname", Field::Str(d.clone())));
        }
        v
    }
}

/// `posix_times()` result (clock ticks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    pub ticks: i64,
    pub utime: i64,
    pub stime: i64,
    pub cutime: i64,
    pub cstime: i64,
}

impl Times {
    /// Array entries in PHP's order.
    pub fn entries(&self) -> Vec<(&'static str, Field)> {
        vec![
            ("ticks", Field::Int(self.ticks)),
            ("utime", Field::Int(self.utime)),
            ("stime", Field::Int(self.stime)),
            ("cutime", Field::Int(self.cutime)),
            ("cstime", Field::Int(self.cstime)),
        ]
    }
}

/// `posix_getgrnam()` / `posix_getgrgid()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: Option<Vec<u8>>,
    pub passwd: Option<Vec<u8>>,
    pub members: Vec<Vec<u8>>,
    pub gid: i64,
}

impl Group {
    /// Array entries in PHP's order (`name`, `passwd`, `members`, `gid`).
    pub fn entries(&self) -> Vec<(&'static str, Field)> {
        vec![
            ("name", Field::opt(self.name.as_ref())),
            ("passwd", Field::opt(self.passwd.as_ref())),
            ("members", Field::List(self.members.clone())),
            ("gid", Field::Int(self.gid)),
        ]
    }
}

/// `posix_getpwnam()` / `posix_getpwuid()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passwd {
    pub name: Option<Vec<u8>>,
    pub passwd: Option<Vec<u8>>,
    pub uid: i64,
    pub gid: i64,
    pub gecos: Option<Vec<u8>>,
    pub dir: Option<Vec<u8>>,
    pub shell: Option<Vec<u8>>,
}

impl Passwd {
    /// Array entries in PHP's order.
    pub fn entries(&self) -> Vec<(&'static str, Field)> {
        vec![
            ("name", Field::opt(self.name.as_ref())),
            ("passwd", Field::opt(self.passwd.as_ref())),
            ("uid", Field::Int(self.uid)),
            ("gid", Field::Int(self.gid)),
            ("gecos", Field::opt(self.gecos.as_ref())),
            ("dir", Field::opt(self.dir.as_ref())),
            ("shell", Field::opt(self.shell.as_ref())),
        ]
    }
}

/// One resource limit value: a number or `"unlimited"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitValue {
    /// `RLIM_INFINITY` → PHP string `"unlimited"`.
    Unlimited,
    /// A finite limit → PHP int.
    Limit(i64),
}

impl RlimitValue {
    fn from_raw(v: u64) -> Self {
        if v == libc::RLIM_INFINITY {
            Self::Unlimited
        } else {
            // add_assoc_long(rl.rlim_cur): C conversion to zend_long.
            #[allow(clippy::cast_possible_wrap)]
            Self::Limit(v as i64)
        }
    }

    /// The PHP value as a [`Field`].
    pub fn to_field(self) -> Field {
        match self {
            Self::Unlimited => Field::Str(UNLIMITED.as_bytes().to_vec()),
            Self::Limit(n) => Field::Int(n),
        }
    }
}

/// `posix_getrlimit()` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rlimits {
    /// No resource given: `["soft core" => …, "hard core" => …, …]`.
    All(Vec<(String, RlimitValue)>),
    /// A single resource: packed `[soft, hard]`.
    Single(RlimitValue, RlimitValue),
}

/// The `limits[]` table of posix.c (Linux subset), in PHP's order.
#[allow(clippy::cast_possible_wrap)]
pub const RLIMIT_NAMES: &[(i32, &str)] = &[
    (libc::RLIMIT_CORE as i32, "core"),
    (libc::RLIMIT_DATA as i32, "data"),
    (libc::RLIMIT_STACK as i32, "stack"),
    (libc::RLIMIT_AS as i32, "totalmem"),
    (libc::RLIMIT_RSS as i32, "rss"),
    (libc::RLIMIT_NPROC as i32, "maxproc"),
    (libc::RLIMIT_MEMLOCK as i32, "memlock"),
    (libc::RLIMIT_CPU as i32, "cpu"),
    (libc::RLIMIT_FSIZE as i32, "filesize"),
    (libc::RLIMIT_NOFILE as i32, "openfiles"),
];

/// The `POSIX_*` constants with their platform values, in registration
/// order (`posix.stub.php`, master).
pub fn constants() -> Vec<(&'static str, i64)> {
    vec![
        ("POSIX_F_OK", i64::from(libc::F_OK)),
        ("POSIX_X_OK", i64::from(libc::X_OK)),
        ("POSIX_W_OK", i64::from(libc::W_OK)),
        ("POSIX_R_OK", i64::from(libc::R_OK)),
        ("POSIX_S_IFREG", i64::from(libc::S_IFREG)),
        ("POSIX_S_IFCHR", i64::from(libc::S_IFCHR)),
        ("POSIX_S_IFBLK", i64::from(libc::S_IFBLK)),
        ("POSIX_S_IFIFO", i64::from(libc::S_IFIFO)),
        ("POSIX_S_IFSOCK", i64::from(libc::S_IFSOCK)),
        ("POSIX_RLIMIT_AS", i64::from(libc::RLIMIT_AS)),
        ("POSIX_RLIMIT_CORE", i64::from(libc::RLIMIT_CORE)),
        ("POSIX_RLIMIT_CPU", i64::from(libc::RLIMIT_CPU)),
        ("POSIX_RLIMIT_DATA", i64::from(libc::RLIMIT_DATA)),
        ("POSIX_RLIMIT_FSIZE", i64::from(libc::RLIMIT_FSIZE)),
        ("POSIX_RLIMIT_LOCKS", i64::from(libc::RLIMIT_LOCKS)),
        ("POSIX_RLIMIT_MEMLOCK", i64::from(libc::RLIMIT_MEMLOCK)),
        ("POSIX_RLIMIT_MSGQUEUE", i64::from(libc::RLIMIT_MSGQUEUE)),
        ("POSIX_RLIMIT_NICE", i64::from(libc::RLIMIT_NICE)),
        ("POSIX_RLIMIT_NOFILE", i64::from(libc::RLIMIT_NOFILE)),
        ("POSIX_RLIMIT_NPROC", i64::from(libc::RLIMIT_NPROC)),
        ("POSIX_RLIMIT_RSS", i64::from(libc::RLIMIT_RSS)),
        ("POSIX_RLIMIT_RTPRIO", i64::from(libc::RLIMIT_RTPRIO)),
        ("POSIX_RLIMIT_RTTIME", i64::from(libc::RLIMIT_RTTIME)),
        (
            "POSIX_RLIMIT_SIGPENDING",
            i64::from(libc::RLIMIT_SIGPENDING),
        ),
        ("POSIX_RLIMIT_STACK", i64::from(libc::RLIMIT_STACK)),
        // RLIM_INFINITY as zend_long.
        ("POSIX_RLIMIT_INFINITY", -1),
        ("POSIX_SC_ARG_MAX", i64::from(libc::_SC_ARG_MAX)),
        ("POSIX_SC_CHILD_MAX", i64::from(libc::_SC_CHILD_MAX)),
        ("POSIX_SC_CLK_TCK", i64::from(libc::_SC_CLK_TCK)),
        ("POSIX_SC_PAGESIZE", i64::from(libc::_SC_PAGESIZE)),
        (
            "POSIX_SC_NPROCESSORS_CONF",
            i64::from(libc::_SC_NPROCESSORS_CONF),
        ),
        (
            "POSIX_SC_NPROCESSORS_ONLN",
            i64::from(libc::_SC_NPROCESSORS_ONLN),
        ),
        ("POSIX_PC_LINK_MAX", i64::from(libc::_PC_LINK_MAX)),
        ("POSIX_PC_MAX_CANON", i64::from(libc::_PC_MAX_CANON)),
        ("POSIX_PC_MAX_INPUT", i64::from(libc::_PC_MAX_INPUT)),
        ("POSIX_PC_NAME_MAX", i64::from(libc::_PC_NAME_MAX)),
        ("POSIX_PC_PATH_MAX", i64::from(libc::_PC_PATH_MAX)),
        ("POSIX_PC_PIPE_BUF", i64::from(libc::_PC_PIPE_BUF)),
        (
            "POSIX_PC_CHOWN_RESTRICTED",
            i64::from(libc::_PC_CHOWN_RESTRICTED),
        ),
        ("POSIX_PC_NO_TRUNC", i64::from(libc::_PC_NO_TRUNC)),
        (
            "POSIX_PC_ALLOC_SIZE_MIN",
            i64::from(libc::_PC_ALLOC_SIZE_MIN),
        ),
        ("POSIX_PC_SYMLINK_MAX", i64::from(libc::_PC_SYMLINK_MAX)),
        ("POSIX_SC_OPEN_MAX", i64::from(libc::_SC_OPEN_MAX)),
    ]
}

/// Warning text for a non-int, non-resource `$file_descriptor` passed to
/// `posix_ttyname()` / `posix_isatty()` (`given` is `zend_zval_value_name`).
/// For `posix_fpathconf()` the same text is thrown as a `TypeError`.
pub fn fd_type_warning(given: &str) -> String {
    format!("Argument #1 ($file_descriptor) must be of type int|resource, {given} given")
}

/// Warning text when a stream resource cannot be cast to a file descriptor.
pub fn stream_cast_warning(stream_label: &str) -> String {
    format!("Could not use stream of type '{stream_label}'")
}

fn fd_range_warning() -> PosixError {
    PosixError::Warning(format!(
        "Argument #1 ($file_descriptor) must be between 0 and {}",
        i32::MAX
    ))
}

fn pid_range_error(arg: u32, name: &str, lower: i64, upper: i64) -> PosixError {
    PosixError::ValueError(format!(
        "Argument #{arg} (${name}) must be between {lower} and {upper}"
    ))
}

fn nul_bytes_error(arg: u32, name: &str) -> PosixError {
    PosixError::ValueError(format!(
        "Argument #{arg} (${name}) must not contain any null bytes"
    ))
}

fn empty_error(arg: u32, name: &str) -> PosixError {
    PosixError::ValueError(format!("Argument #{arg} (${name}) must not be empty"))
}

/// C `zend_long` → `int` conversion (truncation), as PHP passes longs to
/// libc calls taking `int`/`pid_t`.
#[allow(clippy::cast_possible_truncation)]
fn c_int(v: i64) -> i32 {
    v as i32
}

/// C `zend_long` → `unsigned int` conversion (truncation/wrap).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn c_uint(v: i64) -> u32 {
    v as u32
}

/// Truncates at the first NUL byte, as C sees a `Z_PARAM_STRING` argument.
fn c_str_prefix(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |n| &s[..n])
}

/// `expand_filepath()` (`CWD_FILEPATH` mode): makes `path` absolute against
/// `cwd`, normalising `.`/`..`/duplicate slashes; symlinks are resolved when
/// the path exists.  Returns `None` where PHP's returns `NULL` (empty path,
/// path too long).
pub fn expand_filepath(path: &[u8], cwd: &[u8]) -> Option<Vec<u8>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if path.first().is_none_or(|&b| b == 0) {
        return None;
    }
    let path = c_str_prefix(path);
    if path.len() >= MAXPATHLEN - 1 {
        return None;
    }
    let joined = if path[0] == b'/' || cwd.is_empty() {
        path.to_vec()
    } else {
        if path.len() + cwd.len() + 1 >= MAXPATHLEN - 1 {
            return None;
        }
        let mut j = cwd.to_vec();
        if j.last() != Some(&b'/') {
            j.push(b'/');
        }
        j.extend_from_slice(path);
        j
    };
    if let Ok(real) = std::fs::canonicalize(std::ffi::OsStr::from_bytes(&joined)) {
        return Some(real.into_os_string().into_vec());
    }
    let absolute = joined.first() == Some(&b'/');
    let mut parts: Vec<&[u8]> = Vec::new();
    for seg in joined.split(|&b| b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => {
                if absolute || parts.last().is_some_and(|p| *p != b"..") {
                    parts.pop();
                } else {
                    parts.push(seg);
                }
            }
            _ => parts.push(seg),
        }
    }
    let mut out = Vec::with_capacity(joined.len());
    if absolute {
        out.push(b'/');
    }
    out.extend_from_slice(&parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    Some(out)
}

/// The posix module state (`POSIX_G`) plus all `posix_*` functions.
#[derive(Debug, Default, Clone)]
pub struct Posix {
    last_error: i32,
}

impl Posix {
    /// Fresh module state (`last_error = 0`).
    pub fn new() -> Self {
        Self::default()
    }

    fn fail<T>(&mut self, errno: i32) -> PosixResult<T> {
        self.last_error = errno;
        Err(PosixError::Failed(errno))
    }

    fn sys<T>(&mut self, r: Result<T, i32>) -> PosixResult<T> {
        match r {
            Ok(v) => Ok(v),
            Err(e) => self.fail(e),
        }
    }

    /// `posix_get_last_error()` / `posix_errno()`.
    pub fn get_last_error(&self) -> i64 {
        i64::from(self.last_error)
    }

    /// `posix_errno()` (alias of [`Self::get_last_error`]).
    pub fn errno(&self) -> i64 {
        self.get_last_error()
    }

    /// `posix_kill(int $process_id, int $signal): bool`.
    pub fn kill(&mut self, pid: i64, signal: i64) -> PosixResult<()> {
        if !(PID_MIN..=PID_MAX).contains(&pid) {
            return Err(pid_range_error(1, "process_id", PID_MIN, PID_MAX));
        }
        let r = sys::kill(c_int(pid), c_int(signal));
        self.sys(r)
    }

    /// `posix_getpid()`.
    pub fn getpid(&self) -> i64 {
        sys::getpid()
    }

    /// `posix_getppid()`.
    pub fn getppid(&self) -> i64 {
        sys::getppid()
    }

    /// `posix_getuid()`.
    pub fn getuid(&self) -> i64 {
        sys::getuid()
    }

    /// `posix_geteuid()`.
    pub fn geteuid(&self) -> i64 {
        sys::geteuid()
    }

    /// `posix_getgid()`.
    pub fn getgid(&self) -> i64 {
        sys::getgid()
    }

    /// `posix_getegid()`.
    pub fn getegid(&self) -> i64 {
        sys::getegid()
    }

    /// `posix_setuid(int $user_id): bool`.
    pub fn setuid(&mut self, uid: i64) -> PosixResult<()> {
        let r = sys::setuid(c_uint(uid));
        self.sys(r)
    }

    /// `posix_seteuid(int $user_id): bool`.
    pub fn seteuid(&mut self, uid: i64) -> PosixResult<()> {
        let r = sys::seteuid(c_uint(uid));
        self.sys(r)
    }

    /// `posix_setgid(int $group_id): bool`.
    pub fn setgid(&mut self, gid: i64) -> PosixResult<()> {
        let r = sys::setgid(c_uint(gid));
        self.sys(r)
    }

    /// `posix_setegid(int $group_id): bool`.
    pub fn setegid(&mut self, gid: i64) -> PosixResult<()> {
        let r = sys::setegid(c_uint(gid));
        self.sys(r)
    }

    /// `posix_getgroups(): array|false` (packed list of gids).
    pub fn getgroups(&mut self) -> PosixResult<Vec<i64>> {
        let r = sys::getgroups();
        self.sys(r)
    }

    /// `posix_getlogin(): string|false`.
    pub fn getlogin(&mut self) -> PosixResult<Vec<u8>> {
        let r = sys::getlogin();
        self.sys(r)
    }

    /// `posix_getpgrp(): int`.
    pub fn getpgrp(&self) -> i64 {
        sys::getpgrp()
    }

    /// `posix_setsid(): int` — returns `-1` on failure without touching
    /// `last_error`, exactly like PHP.
    pub fn setsid(&self) -> i64 {
        sys::setsid()
    }

    /// `posix_setpgid(int $process_id, int $process_group_id): bool`.
    pub fn setpgid(&mut self, pid: i64, group: i64) -> PosixResult<()> {
        if !(0..=PID_MAX).contains(&pid) {
            return Err(pid_range_error(1, "process_id", 0, PID_MAX));
        }
        if !(0..=PID_MAX).contains(&group) {
            return Err(pid_range_error(2, "process_group_id", 0, PID_MAX));
        }
        let r = sys::setpgid(c_int(pid), c_int(group));
        self.sys(r)
    }

    /// `posix_getpgid(int $process_id): int|false`.
    pub fn getpgid(&mut self, pid: i64) -> PosixResult<i64> {
        let r = sys::getpgid(c_int(pid));
        self.sys(r)
    }

    /// `posix_getsid(int $process_id): int|false`.
    pub fn getsid(&mut self, pid: i64) -> PosixResult<i64> {
        if !(0..=PID_MAX).contains(&pid) {
            return Err(pid_range_error(1, "process_id", 0, PID_MAX));
        }
        let r = sys::getsid(c_int(pid));
        self.sys(r)
    }

    /// `posix_uname(): array|false`.
    pub fn uname(&mut self) -> PosixResult<Uname> {
        let r = sys::uname().map(|u| Uname {
            sysname: u.sysname,
            nodename: u.nodename,
            release: u.release,
            version: u.version,
            machine: u.machine,
            domainname: Some(u.domainname),
        });
        self.sys(r)
    }

    /// `posix_times(): array|false`.
    pub fn times(&mut self) -> PosixResult<Times> {
        let r = sys::times().map(|[ticks, utime, stime, cutime, cstime]| Times {
            ticks,
            utime,
            stime,
            cutime,
            cstime,
        });
        self.sys(r)
    }

    /// `posix_ctermid(): string|false`.
    pub fn ctermid(&mut self) -> PosixResult<Vec<u8>> {
        let r = sys::ctermid();
        self.sys(r)
    }

    /// `posix_ttyname($file_descriptor): string|false` for an integer fd
    /// (after the VM's int|resource handling).
    pub fn ttyname(&mut self, fd: i64) -> PosixResult<Vec<u8>> {
        if !(0..=i64::from(i32::MAX)).contains(&fd) {
            self.last_error = libc::EBADF;
            return Err(fd_range_warning());
        }
        let r = sys::ttyname(c_int(fd));
        self.sys(r)
    }

    /// `posix_isatty($file_descriptor): bool` for an integer fd.
    pub fn isatty(&mut self, fd: i64) -> PosixResult<()> {
        if !(0..=i64::from(i32::MAX)).contains(&fd) {
            self.last_error = libc::EBADF;
            return Err(fd_range_warning());
        }
        let r = sys::isatty(c_int(fd));
        self.sys(r)
    }

    /// `posix_getcwd(): string|false`.
    pub fn getcwd(&mut self) -> PosixResult<Vec<u8>> {
        use std::os::unix::ffi::OsStringExt;
        let r = std::env::current_dir()
            .map(|p| p.into_os_string().into_vec())
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO));
        self.sys(r)
    }

    /// `posix_mkfifo(string $filename, int $permissions): bool`.
    pub fn mkfifo(&mut self, filename: &[u8], permissions: i64) -> PosixResult<()> {
        let path = sys::cstring(filename).ok_or_else(|| nul_bytes_error(1, "filename"))?;
        let r = sys::mkfifo(&path, c_uint(permissions));
        self.sys(r)
    }

    /// `posix_mknod(string $filename, int $flags, int $major = 0, int $minor = 0): bool`.
    pub fn mknod(
        &mut self,
        filename: &[u8],
        flags: i64,
        major: i64,
        minor: i64,
    ) -> PosixResult<()> {
        let path = sys::cstring(filename).ok_or_else(|| nul_bytes_error(1, "filename"))?;
        let mut dev = 0;
        if flags & i64::from(libc::S_IFCHR) != 0 || flags & i64::from(libc::S_IFBLK) != 0 {
            if major == 0 {
                return Err(PosixError::ValueError(
                    "Argument #3 ($major) cannot be 0 for the POSIX_S_IFCHR and POSIX_S_IFBLK modes"
                        .to_owned(),
                ));
            }
            dev = sys::makedev(c_uint(major), c_uint(minor));
        }
        let r = sys::mknod(&path, c_uint(flags), dev);
        self.sys(r)
    }

    fn expand_cwd() -> Vec<u8> {
        use std::os::unix::ffi::OsStringExt;
        std::env::current_dir()
            .map(|p| p.into_os_string().into_vec())
            .unwrap_or_default()
    }

    /// `posix_access(string $filename, int $flags = 0): bool`.
    pub fn access(&mut self, filename: &[u8], flags: i64) -> PosixResult<()> {
        if filename.contains(&0) {
            return Err(nul_bytes_error(1, "filename"));
        }
        let Some(path) = expand_filepath(filename, &Self::expand_cwd()) else {
            return self.fail(libc::EIO);
        };
        let path = sys::cstring(&path).ok_or_else(|| nul_bytes_error(1, "filename"))?;
        let r = sys::access(&path, c_int(flags));
        self.sys(r)
    }

    /// `posix_eaccess(string $filename, int $flags = 0): bool`.
    pub fn eaccess(&mut self, filename: &[u8], flags: i64) -> PosixResult<()> {
        if filename.contains(&0) {
            return Err(nul_bytes_error(1, "filename"));
        }
        let Some(path) = expand_filepath(filename, &Self::expand_cwd()) else {
            return Err(empty_error(1, "filename"));
        };
        let path = sys::cstring(&path).ok_or_else(|| nul_bytes_error(1, "filename"))?;
        let r = sys::eaccess(&path, c_int(flags));
        self.sys(r)
    }

    /// `posix_getgrnam(string $name): array|false`.
    pub fn getgrnam(&mut self, name: &[u8]) -> PosixResult<Group> {
        let name = sys::cstring(c_str_prefix(name)).unwrap_or_default();
        let r = sys::getgrnam(&name).map(group_from_raw);
        self.sys(r)
    }

    /// `posix_getgrgid(int $group_id): array|false`.
    pub fn getgrgid(&mut self, gid: i64) -> PosixResult<Group> {
        let r = sys::getgrgid(c_uint(gid)).map(group_from_raw);
        self.sys(r)
    }

    /// `posix_getpwnam(string $username): array|false`.
    pub fn getpwnam(&mut self, name: &[u8]) -> PosixResult<Passwd> {
        let name = sys::cstring(c_str_prefix(name)).unwrap_or_default();
        let r = sys::getpwnam(&name).map(passwd_from_raw);
        self.sys(r)
    }

    /// `posix_getpwuid(int $user_id): array|false`.
    pub fn getpwuid(&mut self, uid: i64) -> PosixResult<Passwd> {
        let r = sys::getpwuid(c_uint(uid)).map(passwd_from_raw);
        self.sys(r)
    }

    /// `posix_getrlimit(?int $resource = null): array|false`.
    pub fn getrlimit(&mut self, resource: Option<i64>) -> PosixResult<Rlimits> {
        if let Some(res) = resource {
            let r = sys::getrlimit(c_int(res));
            let (cur, max) = self.sys(r)?;
            return Ok(Rlimits::Single(
                RlimitValue::from_raw(cur),
                RlimitValue::from_raw(max),
            ));
        }
        let mut out = Vec::with_capacity(RLIMIT_NAMES.len() * 2);
        for &(res, name) in RLIMIT_NAMES {
            let r = sys::getrlimit(res);
            let (cur, max) = self.sys(r)?;
            out.push((format!("soft {name}"), RlimitValue::from_raw(cur)));
            out.push((format!("hard {name}"), RlimitValue::from_raw(max)));
        }
        Ok(Rlimits::All(out))
    }

    /// `posix_setrlimit(int $resource, int $soft_limit, int $hard_limit): bool`.
    pub fn setrlimit(&mut self, resource: i64, soft: i64, hard: i64) -> PosixResult<()> {
        const MSG: &str = "must be greater than or equal to -1, or POSIX_RLIMIT_INFINITY";
        if soft < -1 {
            return Err(PosixError::ValueError(format!(
                "Argument #2 ($soft_limit) {MSG}"
            )));
        }
        if hard < -1 {
            return Err(PosixError::ValueError(format!(
                "Argument #3 ($hard_limit) {MSG}"
            )));
        }
        if hard > -1 && soft > hard {
            return Err(PosixError::ValueError(format!(
                "Argument #2 ($soft_limit) must be lower or equal to {hard}"
            )));
        }
        #[allow(clippy::cast_sign_loss)]
        let r = sys::setrlimit(c_int(resource), soft as u64, hard as u64);
        self.sys(r)
    }

    /// `posix_strerror(int $error_code): string`.
    pub fn strerror(&self, code: i64) -> Vec<u8> {
        sys::strerror(c_int(code))
    }

    /// `posix_initgroups(string $username, int $group_id): bool`.
    ///
    /// Like PHP, this never updates `last_error`.
    pub fn initgroups(&self, name: &[u8], gid: i64) -> bool {
        if name.is_empty() {
            return false;
        }
        let name = sys::cstring(c_str_prefix(name)).unwrap_or_default();
        sys::initgroups(&name, c_uint(gid)).is_ok()
    }

    /// `posix_sysconf(int $conf_id): int`.
    pub fn sysconf(&self, conf_id: i64) -> i64 {
        sys::sysconf(c_int(conf_id))
    }

    /// `posix_pathconf(string $path, int $name): int|false`.
    pub fn pathconf(&mut self, path: &[u8], name: i64) -> PosixResult<i64> {
        if path.is_empty() {
            return Err(empty_error(1, "path"));
        }
        let path = sys::cstring(path).ok_or_else(|| nul_bytes_error(1, "path"))?;
        let (ret, errno) = sys::pathconf(&path, c_int(name));
        if ret < 0 && errno != 0 {
            return self.fail(errno);
        }
        Ok(ret)
    }

    /// `posix_fpathconf($file_descriptor, int $name): int|false` for an
    /// integer fd.
    pub fn fpathconf(&mut self, fd: i64, name: i64) -> PosixResult<i64> {
        if !(0..=i64::from(i32::MAX)).contains(&fd) {
            self.last_error = libc::EBADF;
            return Err(fd_range_warning());
        }
        let (ret, errno) = sys::fpathconf(c_int(fd), c_int(name));
        if ret < 0 && errno != 0 {
            return self.fail(errno);
        }
        Ok(ret)
    }
}

fn group_from_raw(g: sys::RawGroup) -> Group {
    Group {
        name: g.name,
        passwd: g.passwd,
        members: g.members,
        gid: g.gid,
    }
}

fn passwd_from_raw(p: sys::RawPasswd) -> Passwd {
    Passwd {
        name: p.name,
        passwd: p.passwd,
        uid: p.uid,
        gid: p.gid,
        gecos: p.gecos,
        dir: p.dir,
        shell: p.shell,
    }
}

#[cfg(test)]
mod tests;
