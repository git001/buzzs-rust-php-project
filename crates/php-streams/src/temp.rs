// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Temporary files (`main/php_open_temporary_file.c`, `tempnam()`,
//! `tmpfile()`, `sys_get_temp_dir()`).

use std::fs::File;

use crate::diag;
use crate::paths;
use crate::state;
use crate::sys;

/// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ON_FALLBACK`.
pub const CHECK_ON_FALLBACK: u32 = 1;
/// `PHP_TMP_FILE_SILENT`.
pub const SILENT: u32 = 2;
/// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ON_EXPLICIT_DIR`.
pub const CHECK_ON_EXPLICIT_DIR: u32 = 4;
/// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ALWAYS`.
pub const CHECK_ALWAYS: u32 = CHECK_ON_FALLBACK | CHECK_ON_EXPLICIT_DIR;

const BASE32: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

/// `php_random_bytes_silent()`, else `php_random_generate_fallback_seed()`
/// (the time and the process, not cryptographic): the random part of a
/// temporary file's name.
fn random_u64() -> u64 {
    php_random::csprng::next_u64().unwrap_or_else(|_| fallback_seed())
}

fn fallback_seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    #[allow(clippy::cast_possible_truncation)]
    let t = t as u64;
    t ^ (u64::from(std::process::id()) << 32) ^ 0x9E37_79B9_7F4A_7C15
}

/// `php_get_temporary_directory()`.
pub fn temp_dir() -> Vec<u8> {
    let sys_temp = state::settings().sys_temp_dir;
    let len = sys_temp.len();
    if len >= 2 && sys_temp[len - 1] == b'/' {
        return sys_temp[..len - 1].to_vec();
    } else if len >= 1 && sys_temp[len - 1] != b'/' {
        return sys_temp;
    }
    if let Some(s) = std::env::var_os("TMPDIR") {
        let s = s.as_encoded_bytes();
        if !s.is_empty() {
            if s.ends_with(b"/") {
                return s[..s.len() - 1].to_vec();
            }
            return s.to_vec();
        }
    }
    b"/tmp".to_vec()
}

fn do_open(path: &[u8], pfx: &[u8]) -> Option<(File, Vec<u8>)> {
    if path.is_empty() {
        return None;
    }
    let dir = paths::realpath(path)?;
    let mut random = random_u64();
    let mut prefix = pfx.to_vec();
    for _ in 0..13 {
        #[allow(clippy::cast_possible_truncation)]
        prefix.push(BASE32[(random % 32) as usize]);
        random /= 32;
    }
    let mut template = dir.clone();
    if template.last() != Some(&b'/') {
        template.push(b'/');
    }
    template.extend_from_slice(&prefix);
    template.extend_from_slice(b"XXXXXX");
    if template.len() >= paths::MAXPATHLEN {
        return None;
    }
    sys::mkstemp(&template).ok()
}

/// The notice `php_open_temporary_fd_ex()` gives when `dir` failed and the
/// file goes to the system temporary directory instead.
pub const FALLBACK_NOTICE: &str = "file created in the system's temporary directory";

/// `php_open_temporary_fd_ex()`: creates a unique file in `dir` (or the
/// system temporary directory), returning the open file and its path.
pub fn open_temporary_fd(
    dir: Option<&[u8]>,
    pfx: Option<&[u8]>,
    flags: u32,
) -> Option<(File, Vec<u8>)> {
    create_temporary(dir, pfx, flags, &mut || {
        if flags & SILENT == 0 {
            diag::notice(FALLBACK_NOTICE);
        }
    })
}

/// [`open_temporary_fd`] for callers that report the notice themselves
/// (the upload parser, which runs without the VM and keeps its messages
/// in order): `on_fallback` runs where `php_open_temporary_fd_ex()` gives
/// [`FALLBACK_NOTICE`], when a given `dir` failed and before the system
/// directory is tried (whose `open_basedir` check may warn in turn).
pub fn create_temporary(
    dir: Option<&[u8]>,
    pfx: Option<&[u8]>,
    flags: u32,
    on_fallback: &mut dyn FnMut(),
) -> Option<(File, Vec<u8>)> {
    let pfx = pfx.unwrap_or(b"tmp.");
    let fallback = || {
        let t = temp_dir();
        if !t.is_empty() && (flags & CHECK_ON_FALLBACK == 0 || paths::check_open_basedir(&t)) {
            do_open(&t, pfx)
        } else {
            None
        }
    };
    let Some(dir) = dir.filter(|d| !d.is_empty()) else {
        return fallback();
    };
    if flags & CHECK_ON_EXPLICIT_DIR != 0 && !paths::check_open_basedir(dir) {
        return None;
    }
    if let Some(r) = do_open(dir, pfx) {
        return Some(r);
    }
    on_fallback();
    fallback()
}

/// Opens a temporary file for a stream (`php_stream_fopen_temporary_file`).
pub fn open_temporary_file(
    dir: Option<&[u8]>,
    pfx: &[u8],
    _silent: bool,
) -> Option<(File, Vec<u8>)> {
    open_temporary_fd(dir, Some(pfx), 0)
}

/// `tempnam($dir, $prefix)`: returns the created file name.
pub fn tempnam(dir: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = crate::pathname::basename(prefix, b"");
    if p.len() >= 64 {
        p.truncate(63);
    }
    open_temporary_fd(Some(dir), Some(&p), CHECK_ALWAYS).map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempnam_creates_file() {
        let _ = diag::take();
        let p = tempnam(b"/tmp", b"/x/y/pre").unwrap();
        let s = String::from_utf8(p.clone()).unwrap();
        assert!(s.starts_with("/tmp/pre"), "{s}");
        assert_eq!(s.len(), "/tmp/pre".len() + 13 + 6);
        assert!(std::path::Path::new(&s).exists());
        std::fs::remove_file(&s).unwrap();
        // nonexistent dir falls back with a notice
        let p = tempnam(b"/nonexistent-dir-xyz", b"t").unwrap();
        let d = diag::take();
        assert_eq!(
            d[0].text(),
            "file created in the system's temporary directory"
        );
        std::fs::remove_file(crate::util::os_path(&p)).unwrap();
    }

    #[test]
    fn temp_dir_setting() {
        state::update_settings(|s| s.sys_temp_dir = b"/var/tmp/".to_vec());
        assert_eq!(temp_dir(), b"/var/tmp");
        state::update_settings(|s| s.sys_temp_dir = b"/".to_vec());
        assert!(!temp_dir().is_empty());
        state::update_settings(|s| s.sys_temp_dir.clear());
    }
}
