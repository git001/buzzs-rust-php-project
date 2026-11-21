// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Environment and process information of `ext/standard`: `getenv`/
//! `putenv` (with request-end restoration), `php_uname`,
//! `sys_getloadavg`, `getmypid`/`getmyuid`/`getmygid`/`getmyinode`/
//! `getlastmod`, `getrusage`, `microtime`/`gettimeofday`/`hrtime`,
//! `sleep`/`usleep`/`time_nanosleep`/`time_sleep_until` and `ftok`.

use php_runtime::{
    ArrayKey, ErrorLevel, RtResult, RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};

use crate::sys;

fn value_err(func: &str, n: u32, name: &str, msg: &str) -> RuntimeError {
    RuntimeError::ValueError(format!("{func}(): Argument #{n} (${name}) {msg}"))
}

fn no_nul(func: &str, n: u32, name: &str, s: &[u8]) -> RtResult<()> {
    if s.contains(&0) {
        return Err(value_err(func, n, name, "must not contain any null bytes"));
    }
    Ok(())
}

// ---------------------------------------------------------------- env

/// `putenv()` bookkeeping (`BG(putenv_ht)`): previous values are restored
/// at request shutdown.
#[derive(Debug, Default)]
pub struct EnvState {
    /// Key → previous `name=value` entry (`None` if it was unset).
    entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// `getenv()` without arguments (`php_load_environment_variables`):
/// well-formed entries whose names contain no ` `, `.`, `[`.
#[must_use]
pub fn getenv_all() -> ZArray {
    let mut out = ZArray::new();
    let t = out.make_mut();
    for (k, v) in sys::environ_entries() {
        if k.first() == Some(&b'=') || k.iter().any(|c| matches!(c, b' ' | b'.' | b'[')) {
            continue;
        }
        t.insert(ArrayKey::from_bytes(&k), Value::Str(ZStr::new(v)));
    }
    out
}

/// `getenv($name, $local_only)`; `sapi_value` is what the SAPI's
/// `getenv` hook returned (CLI: none), consulted unless `local_only`.
pub fn getenv(name: Option<&[u8]>, local_only: bool, sapi_value: Option<&[u8]>) -> RtResult<Value> {
    let Some(name) = name else {
        return Ok(Value::Array(getenv_all()));
    };
    no_nul("getenv", 1, "name", name)?;
    if !local_only {
        if let Some(v) = sapi_value {
            return Ok(Value::Str(ZStr::new(v)));
        }
    }
    Ok(sys::getenv(name).map_or(Value::Bool(false), |v| Value::Str(ZStr::new(v))))
}

impl EnvState {
    /// `putenv($assignment)`: `NAME=value` sets, `NAME` unsets.
    pub fn putenv(&mut self, setting: &[u8]) -> RtResult<bool> {
        no_nul("putenv", 1, "assignment", setting)?;
        if setting.is_empty() || setting[0] == b'=' {
            return Err(value_err(
                "putenv",
                1,
                "assignment",
                "must have a valid syntax",
            ));
        }
        let eq = setting.iter().position(|&c| c == b'=');
        let key = &setting[..eq.unwrap_or(setting.len())];
        // A second putenv() of the same name keeps the value from before the
        // first: php-src deletes the old entry first, whose destructor puts
        // that value back, and then records it again as the previous one.
        let previous = match self.entries.iter().position(|(k, _)| k == key) {
            Some(i) => self.entries.remove(i).1,
            None => sys::getenv(key),
        };
        match eq {
            None => sys::unsetenv(key),
            Some(i) => sys::setenv(key, &setting[i + 1..]),
        }
        self.entries.push((key.to_vec(), previous));
        if key.eq_ignore_ascii_case(b"TZ") {
            sys::tzset();
        }
        Ok(true)
    }

    /// Request shutdown: restores the variables changed by `putenv()`.
    pub fn request_shutdown(&mut self) {
        for (key, prev) in self.entries.drain(..) {
            match prev {
                Some(v) => sys::setenv(&key, &v),
                None => sys::unsetenv(&key),
            }
            if key.eq_ignore_ascii_case(b"TZ") {
                sys::tzset();
            }
        }
    }
}

// ---------------------------------------------------------------- uname etc.

/// `php_uname($mode)`.
pub fn php_uname(mode: &[u8]) -> RtResult<ZStr> {
    if mode.len() != 1 {
        return Err(value_err(
            "php_uname",
            1,
            "mode",
            "must be a single character",
        ));
    }
    let idx = match mode[0] {
        b's' => 0,
        b'n' => 1,
        b'r' => 2,
        b'v' => 3,
        b'm' => 4,
        b'a' => 5,
        _ => {
            return Err(value_err(
                "php_uname",
                1,
                "mode",
                "must be one of \"a\", \"m\", \"n\", \"r\", \"s\", or \"v\"",
            ))
        }
    };
    let Some(u) = sys::uname() else {
        return Ok(ZStr::from("Linux"));
    };
    if idx == 5 {
        return Ok(ZStr::new(u.join(&b' ')));
    }
    Ok(ZStr::new(u[idx].clone()))
}

/// `sys_getloadavg()`.
#[must_use]
pub fn sys_getloadavg() -> Value {
    match sys::getloadavg() {
        Some(l) => Value::Array(ZArray::from_values(l.iter().map(|&x| Value::Float(x)))),
        None => Value::Bool(false),
    }
}

/// `getrusage($mode)` (1 = `RUSAGE_CHILDREN`).
#[must_use]
pub fn getrusage(mode: i64) -> Value {
    match sys::getrusage(mode == 1) {
        Some(fields) => Value::Array(ZArray::from_pairs(
            fields.into_iter().map(|(k, v)| (k, Value::Int(v))),
        )),
        None => Value::Bool(false),
    }
}

/// `getmypid()`.
#[must_use]
pub fn getmypid() -> i64 {
    sys::getpid()
}

/// The script's page information (`php_statpage()`).
#[derive(Debug, Clone, Copy)]
pub struct PageInfo {
    /// Owner uid (`getmyuid()`).
    pub uid: i64,
    /// Owner gid (`getmygid()`).
    pub gid: i64,
    /// Inode (`getmyinode()`, -1 = unknown).
    pub inode: i64,
    /// Modification time (`getlastmod()`, -1 = unknown).
    pub mtime: i64,
}

impl PageInfo {
    /// Stats the primary script (`None` for `php -r` / stdin): falls back
    /// to the process uid/gid and unknown inode/mtime.
    #[must_use]
    pub fn for_script(path: Option<&[u8]>) -> Self {
        if let Some(p) = path {
            if let (Some((ino, mtime)), Some((uid, gid))) = (sys::stat_ino_mtime(p), stat_owner(p))
            {
                return PageInfo {
                    uid,
                    gid,
                    inode: ino,
                    mtime,
                };
            }
        }
        let (uid, gid) = sys::getuid_gid();
        PageInfo {
            uid,
            gid,
            inode: -1,
            mtime: -1,
        }
    }

    fn opt(v: i64) -> Value {
        if v < 0 {
            Value::Bool(false)
        } else {
            Value::Int(v)
        }
    }

    /// `getmyuid()`.
    #[must_use]
    pub fn getmyuid(&self) -> Value {
        Self::opt(self.uid)
    }
    /// `getmygid()`.
    #[must_use]
    pub fn getmygid(&self) -> Value {
        Self::opt(self.gid)
    }
    /// `getmyinode()`.
    #[must_use]
    pub fn getmyinode(&self) -> Value {
        Self::opt(self.inode)
    }
    /// `getlastmod()`.
    #[must_use]
    pub fn getlastmod(&self) -> Value {
        Self::opt(self.mtime)
    }
}

fn stat_owner(path: &[u8]) -> Option<(i64, i64)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(std::ffi::OsStr::from_bytes(path)).ok()?;
    Some((i64::from(md.uid()), i64::from(md.gid())))
}

/// `ftok($filename, $project_id)`.
pub fn ftok(
    filename: &[u8],
    project_id: &[u8],
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<i64> {
    no_nul("ftok", 1, "filename", filename)?;
    if filename.is_empty() {
        return Err(value_err("ftok", 1, "filename", "must not be empty"));
    }
    if project_id.len() != 1 {
        return Err(value_err(
            "ftok",
            2,
            "project_id",
            "must be a single character",
        ));
    }
    let k = sys::ftok(filename, project_id[0]);
    if k == -1 {
        ctx.diagnostic(
            ErrorLevel::Warning,
            &format!("ftok(): ftok() failed - {}", sys::strerror(sys::errno())),
        )?;
    }
    Ok(k)
}

// ---------------------------------------------------------------- time

/// `microtime($as_float)`.
#[must_use]
pub fn microtime(as_float: bool) -> Value {
    let (sec, usec) = sys::gettimeofday();
    microtime_from(sec, usec, as_float)
}

/// `microtime()` formatting for a given `(sec, usec)`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn microtime_from(sec: i64, usec: i64, as_float: bool) -> Value {
    if as_float {
        return Value::Float(sec as f64 + usec as f64 / 1_000_000.0);
    }
    Value::from(format!("{:.8} {sec}", usec as f64 / 1_000_000.0))
}

/// `gettimeofday($as_float)`; `tz_offset` returns the local UTC offset in
/// seconds and the DST flag for a timestamp (from the date extension).
pub fn gettimeofday(as_float: bool, tz_offset: impl FnOnce(i64) -> (i64, bool)) -> Value {
    let (sec, usec) = sys::gettimeofday();
    if as_float {
        return microtime_from(sec, usec, true);
    }
    let (offset, dst) = tz_offset(sec);
    Value::Array(ZArray::from_pairs([
        ("sec", Value::Int(sec)),
        ("usec", Value::Int(usec)),
        ("minuteswest", Value::Int(-offset / 60)),
        ("dsttime", Value::Int(i64::from(dst))),
    ]))
}

/// `hrtime($as_number)`.
#[must_use]
pub fn hrtime(as_number: bool) -> Value {
    let t = sys::hrtime_ns();
    #[allow(clippy::cast_possible_wrap)]
    if as_number {
        Value::Int(t as i64)
    } else {
        Value::Array(ZArray::from_values([
            Value::Int((t / 1_000_000_000) as i64),
            Value::Int((t % 1_000_000_000) as i64),
        ]))
    }
}

/// `sleep($seconds)`: the remaining seconds if interrupted.
pub fn sleep(seconds: i64) -> RtResult<i64> {
    let Ok(s) = u32::try_from(seconds) else {
        return Err(value_err(
            "sleep",
            1,
            "seconds",
            &format!("must be between 0 and {}", u32::MAX),
        ));
    };
    Ok(i64::from(sys::sleep(s)))
}

/// `usleep($microseconds)`.
pub fn usleep(microseconds: i64) -> RtResult<()> {
    let Ok(us) = u32::try_from(microseconds) else {
        return Err(value_err(
            "usleep",
            1,
            "microseconds",
            &format!("must be between 0 and {}", u32::MAX),
        ));
    };
    sys::usleep(us);
    Ok(())
}

/// `time_nanosleep($seconds, $nanoseconds)`: `true`, or the remaining
/// time as `["seconds" => .., "nanoseconds" => ..]` when interrupted.
pub fn time_nanosleep(seconds: i64, nanoseconds: i64) -> RtResult<Value> {
    if seconds < 0 {
        return Err(value_err(
            "time_nanosleep",
            1,
            "seconds",
            "must be greater than or equal to 0",
        ));
    }
    if !(0..=999_999_999).contains(&nanoseconds) {
        return Err(value_err(
            "time_nanosleep",
            2,
            "nanoseconds",
            "must be between 0 and 999999999",
        ));
    }
    Ok(match sys::nanosleep(seconds, nanoseconds) {
        Ok(()) => Value::Bool(true),
        Err(Some((s, ns))) => Value::Array(ZArray::from_pairs([
            ("seconds", Value::Int(s)),
            ("nanoseconds", Value::Int(ns)),
        ])),
        Err(None) => Value::Bool(false),
    })
}

/// `time_sleep_until($timestamp)`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub fn time_sleep_until(
    timestamp: f64,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<bool> {
    const NS: u64 = 1_000_000_000;
    let (sec, usec) = sys::gettimeofday();
    let start = sys::hrtime_ns();
    let top = (u64::MAX / NS) as f64;
    if !(timestamp >= 0.0 && timestamp <= top) {
        return Err(value_err(
            "time_sleep_until",
            1,
            "timestamp",
            &format!("must be between 0 and {}", u64::MAX / NS),
        ));
    }
    let target = (timestamp * NS as f64) as u64;
    let current = (sec as u64) * NS + (usec as u64) * 1000;
    if target < current {
        ctx.diagnostic(
            ErrorLevel::Warning,
            "time_sleep_until(): Argument #1 ($timestamp) must be greater than or equal to the current time",
        )?;
        return Ok(false);
    }
    let diff = target - current;
    loop {
        let elapsed = sys::hrtime_ns().saturating_sub(start);
        if elapsed >= diff {
            return Ok(true);
        }
        let rem = diff - elapsed;
        match sys::nanosleep((rem / NS) as i64, (rem % NS) as i64) {
            Ok(()) => return Ok(true),
            Err(Some(_)) => {}
            Err(None) => return Ok(false),
        }
    }
}
