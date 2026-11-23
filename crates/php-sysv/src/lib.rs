// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's System V IPC extensions over libc:
//!
//! * [`msg`] — `ext/sysvmsg` (`msg_*`, the `SysvMessageQueue` object),
//! * [`sem`] — `ext/sysvsem` (`sem_*`, the `SysvSemaphore` object, with
//!   PHP's three-semaphore protocol),
//! * [`shm`] — `ext/sysvshm` (`shm_*`, the `SysvSharedMemory` object, with
//!   PHP's `sysvshm_chunk_head`/`sysvshm_chunk` segment layout),
//! * [`shmop`] — `ext/shmop` (`shmop_*`, the `Shmop` object).
//!
//! The VM wraps each handle type ([`msg::MessageQueue`], [`sem::Semaphore`],
//! [`shm::SharedMemory`], [`shmop::Shmop`]) in the corresponding PHP object;
//! dropping a handle performs what the object's `free_obj` handler does.
//!
//! PHP (un)serialization is the VM's business: functions taking or
//! returning PHP values work on serialized byte strings, and the docs of
//! each function say where the VM must call `serialize()`/`unserialize()`.
//!
//! Failures are a [`SysvError`] describing what PHP does.  Messages never
//! include the `"func(): "` prefix — the VM adds it from the active
//! function name, like `php_error_docref()`/`zend_argument_value_error()`.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "test code"))]

pub mod msg;
pub mod sem;
pub mod shm;
pub mod shmop;
mod sys;

use std::fmt;

/// `IPC_PRIVATE`.
pub const IPC_PRIVATE: i32 = libc::IPC_PRIVATE;

/// What a failing function does in PHP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysvError {
    /// Throw `ValueError` with this message.
    ValueError(String),
    /// Throw `TypeError` with this message.
    TypeError(String),
    /// Throw a plain `Error` with this message.
    Error(String),
    /// Emit `E_WARNING` with this message and return `false`.
    Warning(String),
    /// Return `false` without a diagnostic; carries the `errno` observed
    /// (exposed through by-reference `$error_code` parameters where PHP has
    /// them).
    Failed(i32),
}

impl fmt::Display for SysvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueError(m) | Self::TypeError(m) | Self::Error(m) | Self::Warning(m) => {
                f.write_str(m)
            }
            Self::Failed(e) => write!(f, "failed with errno {e}"),
        }
    }
}

impl std::error::Error for SysvError {}

/// Result alias for this crate.
pub type SysvResult<T> = Result<T, SysvError>;

/// `strerror(errno)` as glibc spells it.
pub fn strerror(errno: i32) -> String {
    sys::strerror(errno)
}

/// Converts a PHP `int $key` to `key_t`, rejecting values that do not
/// round-trip (`"Argument #1 ($key) is out of range"`, GH-9945).
pub fn key_from_arg(key: i64) -> SysvResult<i32> {
    i32::try_from(key)
        .map_err(|_| SysvError::ValueError("Argument #1 ($key) is out of range".into()))
}

/// `ZEND_XLONG_FMT` rendering of a `zend_long` (two's-complement, 64 bit).
#[allow(clippy::cast_sign_loss)]
fn xlong(v: i64) -> String {
    format!("{:x}", v as u64)
}

/// `%x` rendering of a `key_t` (unsigned 32 bit).
#[allow(clippy::cast_sign_loss)]
fn xkey(k: i32) -> String {
    format!("{:x}", k as u32)
}

/// `zend_long` → C `int` (implicit truncating conversion).
#[allow(clippy::cast_possible_truncation)]
fn c_int(v: i64) -> libc::c_int {
    v as libc::c_int
}

#[cfg(test)]
mod tests;
