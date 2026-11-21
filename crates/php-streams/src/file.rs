// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Core logic of the file functions in `ext/standard/file.c` and
//! `ext/standard/streamsfuncs.c`.
//!
//! Each function takes plain Rust arguments (paths as bytes, optional
//! integers for nullable parameters) and returns `Ok(None)` where PHP
//! returns `false`. Argument validation failures that PHP raises as
//! `ValueError` are returned as [`ArgError`] for the VM to throw.
//! Warnings are emitted through [`crate::diag`].

use std::fmt;

use crate::context::ContextRef;
use crate::diag;
use crate::open;
use crate::paths;
use crate::stat::StatBuf;
use crate::stream::{
    copy_to_stream, BufferMode, Stream, StreamOption, OPT_OK, SEEK_CUR, SEEK_END, SEEK_SET,
};
use crate::sys;
use crate::util::{find_bytes, os_path, starts_with_ci};
use crate::wrapper::{self, REPORT_ERRORS, USE_PATH};

/// `FILE_USE_INCLUDE_PATH`.
pub const FILE_USE_INCLUDE_PATH: i64 = 1;
/// `FILE_IGNORE_NEW_LINES`.
pub const FILE_IGNORE_NEW_LINES: i64 = 2;
/// `FILE_SKIP_EMPTY_LINES`.
pub const FILE_SKIP_EMPTY_LINES: i64 = 4;
/// `FILE_APPEND`.
pub const FILE_APPEND: i64 = 8;
/// `FILE_NO_DEFAULT_CONTEXT`.
pub const FILE_NO_DEFAULT_CONTEXT: i64 = 16;
/// `LOCK_SH`.
pub const LOCK_SH: i64 = 1;
/// `LOCK_EX`.
pub const LOCK_EX: i64 = 2;
/// `LOCK_UN`.
pub const LOCK_UN: i64 = 3;
/// `LOCK_NB`.
pub const LOCK_NB: i64 = 4;
/// `PHP_SOCK_CHUNK_SIZE` (default `stream_get_line()` length).
pub const SOCK_CHUNK_SIZE: i64 = 8192;

/// An argument `ValueError` (`zend_argument_value_error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    /// 1-based argument number.
    pub arg: u32,
    /// The message after `Argument #N ($name) `.
    pub message: String,
}

impl ArgError {
    fn new(arg: u32, message: impl Into<String>) -> Self {
        ArgError {
            arg,
            message: message.into(),
        }
    }
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Argument #{} {}", self.arg, self.message)
    }
}

impl std::error::Error for ArgError {}

fn use_path(b: bool) -> u32 {
    if b {
        USE_PATH
    } else {
        0
    }
}

fn open_rb(
    filename: &[u8],
    use_include_path: bool,
    context: Option<&ContextRef>,
) -> Option<Stream> {
    let mut opened = None;
    open::open_wrapper(
        filename,
        "rb",
        use_path(use_include_path) | REPORT_ERRORS,
        &mut opened,
        context,
    )
}

/// `file_get_contents()`.
pub fn file_get_contents(
    filename: &[u8],
    use_include_path: bool,
    context: Option<&ContextRef>,
    offset: i64,
    maxlen: Option<i64>,
) -> Result<Option<Vec<u8>>, ArgError> {
    let maxlen = match maxlen {
        None => None,
        Some(m) if m < 0 => {
            return Err(ArgError::new(
                5,
                "($length) must be greater than or equal to 0",
            ))
        }
        Some(m) => Some(usize::try_from(m).unwrap_or(usize::MAX)),
    };
    let Some(mut stream) = open_rb(filename, use_include_path, context) else {
        return Ok(None);
    };
    if stream.ops_label() == "STDIO" {
        stream.set_option(&mut StreamOption::ReadBuffer(BufferMode::None, None));
    }
    if offset != 0 && stream.seek(offset, if offset > 0 { SEEK_SET } else { SEEK_END }) < 0 {
        stream.close();
        diag::warning(format!("Failed to seek to position {offset} in the stream"));
        return Ok(None);
    }
    let contents = stream.copy_to_mem(maxlen).unwrap_or_default();
    stream.close();
    Ok(Some(contents))
}

/// The data argument of `file_put_contents()`.
#[derive(Debug)]
pub enum PutData<'a> {
    /// A string (scalars are converted by the VM).
    Bytes(&'a [u8]),
    /// An array: each element converted to a string.
    Chunks(&'a [Vec<u8>]),
    /// A stream resource to copy from.
    Stream(&'a mut Stream),
    /// A value that cannot be written (non-stringable object).
    Invalid,
}

/// `file_put_contents()`: returns the number of bytes written.
pub fn file_put_contents(
    filename: &[u8],
    data: PutData<'_>,
    flags: i64,
    context: Option<&ContextRef>,
) -> Option<i64> {
    let mut mode = "wb";
    if flags & FILE_APPEND != 0 {
        mode = "ab";
    } else if flags & LOCK_EX != 0 {
        if find_bytes(filename, b"://").is_some() && !starts_with_ci(filename, b"file://") {
            diag::warning("Exclusive locks may only be set for regular files");
            return None;
        }
        mode = "cb";
    }
    let mut opened = None;
    let mut stream = open::open_wrapper(
        filename,
        mode,
        use_path(flags & FILE_USE_INCLUDE_PATH != 0) | REPORT_ERRORS,
        &mut opened,
        context,
    )?;
    if flags & LOCK_EX != 0 && (!stream.supports_lock() || stream.lock(sys::LOCK_EX) != 0) {
        stream.close();
        diag::warning("Exclusive locks are not supported for this stream");
        return None;
    }
    if mode == "cb" {
        stream.truncate(0);
    }
    let mut numbytes: i64 = 0;
    match data {
        PutData::Stream(src) => {
            let (ok, len) = copy_to_stream(src, &mut stream, None);
            numbytes = if ok {
                i64::try_from(len).unwrap_or(i64::MAX)
            } else {
                -1
            };
        }
        PutData::Bytes(b) => {
            if !b.is_empty() {
                numbytes = match stream.write(b) {
                    Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
                    Err(_) => -1,
                };
                if numbytes != -1 && numbytes != i64::try_from(b.len()).unwrap_or(i64::MAX) {
                    diag::warning(format!(
                        "Only {numbytes} of {} bytes written, possibly out of free disk space",
                        b.len()
                    ));
                    numbytes = -1;
                }
            }
        }
        PutData::Chunks(chunks) => {
            for c in chunks {
                if c.is_empty() {
                    continue;
                }
                numbytes += i64::try_from(c.len()).unwrap_or(i64::MAX);
                if !matches!(stream.write(c), Ok(n) if n == c.len()) {
                    diag::warning(diag::msg(&[
                        format!("Failed to write {} bytes to ", c.len()).as_bytes(),
                        filename,
                    ]));
                    numbytes = -1;
                    break;
                }
            }
        }
        PutData::Invalid => numbytes = -1,
    }
    let close_result = stream.close();
    if numbytes < 0 || close_result != 0 {
        return None;
    }
    Some(numbytes)
}

/// `file()`: splits the file into lines.
pub fn file(
    filename: &[u8],
    flags: i64,
    context: Option<&ContextRef>,
) -> Result<Option<Vec<Vec<u8>>>, ArgError> {
    if flags
        & !(FILE_USE_INCLUDE_PATH
            | FILE_IGNORE_NEW_LINES
            | FILE_SKIP_EMPTY_LINES
            | FILE_NO_DEFAULT_CONTEXT)
        != 0
    {
        return Err(ArgError::new(2, "($flags) must be a valid flag value"));
    }
    let include_new_line = flags & FILE_IGNORE_NEW_LINES == 0;
    let skip_blank = flags & FILE_SKIP_EMPTY_LINES != 0;
    let Some(mut stream) = open_rb(filename, flags & FILE_USE_INCLUDE_PATH != 0, context) else {
        return Ok(None);
    };
    let mut out = Some(Vec::new());
    if let Some(buf) = stream.copy_to_mem(None) {
        let mac = crate::state::settings().auto_detect_line_endings && detect_mac(&buf);
        out = split_lines_reserving(
            &buf,
            if mac { b'\r' } else { b'\n' },
            include_new_line,
            skip_blank,
        );
    }
    stream.close();
    Ok(out)
}

fn detect_mac(buf: &[u8]) -> bool {
    let cr = buf.iter().position(|&c| c == b'\r');
    let lf = buf.iter().position(|&c| c == b'\n');
    matches!(cr, Some(c) if lf != Some(c + 1) && lf.is_none_or(|l| l >= c))
}

/// The line splitting of `file()` for a given EOL marker.
pub fn split_lines(buf: &[u8], eol: u8, include_new_line: bool, skip_blank: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for_each_line(buf, eol, include_new_line, skip_blank, |line| {
        out.push(line.to_vec());
        true
    });
    out
}

/// [`split_lines`] under `memory_limit`: every line costs PHP a
/// `zend_string` plus a packed-array slot, so once those no longer fit the
/// split stops with `None` (the violation stays recorded and the VM raises
/// "Allowed memory size of N bytes exhausted") instead of growing towards
/// the hard ceiling.
pub fn split_lines_reserving(
    buf: &[u8],
    eol: u8,
    include_new_line: bool,
    skip_blank: bool,
) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let complete = for_each_line(buf, eol, include_new_line, skip_blank, |line| {
        if php_memlimit::try_reserve(zend_string_size(line.len()) + ZVAL_SIZE).is_err() {
            return false;
        }
        out.push(line.to_vec());
        true
    });
    complete.then_some(out)
}

/// `sizeof(zval)`: one packed hash table slot.
const ZVAL_SIZE: usize = 16;

/// Feeds the lines of `buf` to `f` as `file()` splits them; stops (and
/// returns `false`) as soon as `f` does.
#[allow(clippy::many_single_char_names)] // mirrors php_stream_get_line's cursors
fn for_each_line(
    buf: &[u8],
    eol: u8,
    include_new_line: bool,
    skip_blank: bool,
    mut f: impl FnMut(&[u8]) -> bool,
) -> bool {
    let mut s = 0usize;
    let e = buf.len();
    let Some(first) = buf.iter().position(|&c| c == eol) else {
        return e == 0 || f(buf);
    };
    let mut p = first;
    if include_new_line {
        loop {
            p += 1;
            if !f(&buf[s..p]) {
                return false;
            }
            s = p;
            match buf[p..].iter().position(|&c| c == eol) {
                Some(i) => p += i,
                None => break,
            }
        }
    } else {
        loop {
            let windows = usize::from(p != 0 && eol == b'\n' && buf[p - 1] == b'\r');
            if skip_blank && p - s - windows == 0 {
                p += 1;
                s = p;
            } else {
                if !f(&buf[s..p - windows]) {
                    return false;
                }
                p += 1;
                s = p;
            }
            match buf[p.min(e)..].iter().position(|&c| c == eol) {
                Some(i) => p += i,
                None => break,
            }
        }
    }
    s == e || f(&buf[s..e])
}

/// `readfile()`: passes the file to `out`, returning the byte count.
pub fn readfile(
    filename: &[u8],
    use_include_path: bool,
    context: Option<&ContextRef>,
    out: &mut dyn FnMut(&[u8]),
) -> Option<i64> {
    let mut stream = open_rb(filename, use_include_path, context)?;
    let n = stream.passthru(out).unwrap_or(0);
    stream.close();
    Some(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `fpassthru()`.
pub fn fpassthru(stream: &mut Stream, out: &mut dyn FnMut(&[u8])) -> i64 {
    i64::try_from(stream.passthru(out).unwrap_or(0)).unwrap_or(i64::MAX)
}

/// Copies a plain file (used for cross-device renames).
pub fn copy_file_plain(from: &[u8], to: &[u8]) -> std::io::Result<u64> {
    std::fs::copy(os_path(from), os_path(to))
}

/// `php_copy_file_ctx()`.
pub fn copy_file_ctx(
    src: &[u8],
    dest: &[u8],
    src_flags: u32,
    context: Option<&ContextRef>,
) -> bool {
    let stat_flags = if src_flags & wrapper::STREAM_DISABLE_OPEN_BASEDIR != 0 {
        wrapper::URL_STAT_IGNORE_OPEN_BASEDIR
    } else {
        0
    };
    'checks: {
        let Some(src_s) = open::stat_path(src, stat_flags, context) else {
            break 'checks;
        };
        if src_s.is_dir() {
            diag::warning("The first argument to copy() function cannot be a directory");
            return false;
        }
        let Some(dest_s) = open::stat_path(dest, wrapper::URL_STAT_QUIET, context) else {
            break 'checks;
        };
        if dest_s.is_dir() {
            diag::warning("The second argument to copy() function cannot be a directory");
            return false;
        }
        if src_s.ino == 0 || dest_s.ino == 0 {
            let Some(sp) = paths::expand_filepath(src) else {
                return false;
            };
            if let Some(dp) = paths::expand_filepath(dest) {
                if sp == dp {
                    return false;
                }
            }
        } else if src_s.ino == dest_s.ino && src_s.dev == dest_s.dev {
            return false;
        }
    }
    let mut opened = None;
    let Some(mut s) =
        open::open_wrapper(src, "rb", src_flags | REPORT_ERRORS, &mut opened, context)
    else {
        return false;
    };
    let mut ok = false;
    let mut d = open::open_wrapper(dest, "wb", REPORT_ERRORS, &mut opened, context);
    if let Some(d) = d.as_mut() {
        ok = copy_to_stream(&mut s, d, None).0;
    }
    s.close();
    if let Some(mut d) = d {
        if d.close() != 0 {
            ok = false;
        }
    }
    ok
}

/// `copy()`.
pub fn copy(source: &[u8], target: &[u8], context: Option<&ContextRef>) -> bool {
    if let Some(loc) = wrapper::locate(source, 0) {
        if loc.wrapper.is_plain_files() && !paths::check_open_basedir(source) {
            return false;
        }
    }
    copy_file_ctx(source, target, 0, context)
}

/// `rename()`.
pub fn rename(old: &[u8], new: &[u8], context: Option<&ContextRef>) -> bool {
    let Some(w) = wrapper::locate(old, 0) else {
        diag::warning("Unable to locate stream wrapper");
        return false;
    };
    if !w.wrapper.has_rename() {
        diag::warning(format!(
            "{} wrapper does not support renaming",
            w.wrapper.label()
        ));
        return false;
    }
    let other = wrapper::locate(new, 0);
    if !other.is_some_and(|o| std::rc::Rc::ptr_eq(&o.wrapper, &w.wrapper)) {
        diag::warning("Cannot rename a file across wrapper types");
        return false;
    }
    if let Some(r) = w.wrapper.rename(old, new, REPORT_ERRORS, context) {
        r
    } else {
        diag::warning(format!(
            "{} wrapper does not support renaming",
            w.wrapper.label()
        ));
        false
    }
}

/// `unlink()`.
pub fn unlink(filename: &[u8], context: Option<&ContextRef>) -> bool {
    let Some(w) = wrapper::locate(filename, 0) else {
        diag::warning("Unable to locate stream wrapper");
        return false;
    };
    if !w.wrapper.has_unlink() {
        diag::warning(format!("{} does not allow unlinking", w.wrapper.label()));
        return false;
    }
    w.wrapper
        .unlink(filename, REPORT_ERRORS, context)
        .unwrap_or(false)
}

/// `mkdir()`.
pub fn mkdir(dir: &[u8], mode: i64, recursive: bool, context: Option<&ContextRef>) -> bool {
    let opts = REPORT_ERRORS
        | if recursive {
            wrapper::MKDIR_RECURSIVE
        } else {
            0
        };
    open::mkdir(dir, mode, opts, context)
}

/// `rmdir()`.
pub fn rmdir(dir: &[u8], context: Option<&ContextRef>) -> bool {
    open::rmdir(dir, REPORT_ERRORS, context)
}

/// `fclose()`: false (with PHP's warning) for streams that must not be
/// closed manually.
pub fn fclose(stream: &mut Stream) -> bool {
    if stream.has_flag(crate::stream::flags::NO_FCLOSE) {
        diag::warning("cannot close the provided stream, as it must not be manually closed");
        return false;
    }
    stream.close() == 0
}

/// `fgets($stream, $length)`.
pub fn fgets(stream: &mut Stream, length: Option<i64>) -> Result<Option<Vec<u8>>, ArgError> {
    match length {
        None => Ok(stream.get_line(None)),
        Some(l) if l <= 0 => Err(ArgError::new(2, "($length) must be greater than 0")),
        Some(l) => Ok(stream.get_line(Some(usize::try_from(l).unwrap_or(usize::MAX)))),
    }
}

/// `fread($stream, $length)`.
pub fn fread(stream: &mut Stream, length: i64) -> Result<Option<Vec<u8>>, ArgError> {
    if length <= 0 {
        return Err(ArgError::new(2, "($length) must be greater than 0"));
    }
    let len = usize::try_from(length).unwrap_or(usize::MAX);
    // `php_stream_read_to_str()` allocates the whole `zend_string` up
    // front: a length beyond `memory_limit` is the "Allowed memory size"
    // fatal (left recorded for the VM), never a real allocation.
    if php_memlimit::try_reserve(zend_string_size(len)).is_err() {
        return Ok(None);
    }
    Ok(stream.read_vec(len))
}

/// `_ZSTR_STRUCT_SIZE(len)` rounded like `zend_mm`: the 24-byte header, the
/// bytes and the terminating NUL, in 8-byte units.
fn zend_string_size(len: usize) -> usize {
    len.saturating_add(24 + 1).saturating_add(7) & !7
}

/// `fwrite($stream, $data, $length)`.
pub fn fwrite(stream: &mut Stream, data: &[u8], length: Option<i64>) -> Option<i64> {
    let n = match length {
        None => data.len(),
        Some(l) if l <= 0 => 0,
        Some(l) => usize::try_from(l).unwrap_or(usize::MAX).min(data.len()),
    };
    if n == 0 {
        return Some(0);
    }
    stream
        .write(&data[..n])
        .ok()
        .map(|w| i64::try_from(w).unwrap_or(i64::MAX))
}

/// `ftell()`.
pub fn ftell(stream: &Stream) -> Option<i64> {
    let p = stream.tell();
    (p != -1).then_some(p)
}

/// `fseek()`.
pub fn fseek(stream: &mut Stream, offset: i64, whence: i64) -> i64 {
    i64::from(stream.seek(offset, i32::try_from(whence).unwrap_or(-1)))
}

/// `flock()`: returns `(result, wouldblock)`.
pub fn flock(stream: &mut Stream, operation: i64) -> Result<(bool, bool), ArgError> {
    let act = operation & LOCK_UN;
    if !(1..=3).contains(&act) {
        return Err(ArgError::new(
            2,
            "($operation) must be one of LOCK_SH, LOCK_EX, or LOCK_UN",
        ));
    }
    let base = [sys::LOCK_SH, sys::LOCK_EX, sys::LOCK_UN][usize::try_from(act - 1).unwrap_or(0)];
    let op = base
        | if operation & LOCK_NB != 0 {
            sys::LOCK_NB
        } else {
            0
        };
    stream.state.last_errno = 0;
    if stream.lock(op) != 0 {
        let wouldblock = operation != 0 && stream.state.last_errno == sys::EWOULDBLOCK;
        return Ok((false, wouldblock));
    }
    Ok((true, false))
}

/// `ftruncate()`.
pub fn ftruncate(stream: &mut Stream, size: i64) -> Result<bool, ArgError> {
    if size < 0 {
        return Err(ArgError::new(
            2,
            "($size) must be greater than or equal to 0",
        ));
    }
    if !stream.truncate_supported() {
        diag::warning("Can't truncate this stream!");
        return Ok(false);
    }
    Ok(stream.truncate(size) == OPT_OK)
}

/// `fsync()` / `fdatasync()`.
pub fn fsync(stream: &mut Stream, data_only: bool) -> bool {
    if !stream.sync_supported() {
        diag::warning("Can't fsync this stream!");
        return false;
    }
    stream.sync(data_only) == OPT_OK
}

/// `fstat()`.
pub fn fstat(stream: &mut Stream) -> Option<StatBuf> {
    stream.stat()
}

/// `stream_get_contents($stream, $length, $offset)`.
pub fn stream_get_contents(
    stream: &mut Stream,
    maxlen: Option<i64>,
    offset: i64,
) -> Result<Option<Vec<u8>>, ArgError> {
    let maxlen = match maxlen {
        None | Some(-1) => None,
        Some(m) if m < 0 => {
            return Err(ArgError::new(
                2,
                "($length) must be greater than or equal to -1",
            ))
        }
        Some(m) => Some(usize::try_from(m).unwrap_or(usize::MAX)),
    };
    if offset >= 0 {
        let position = stream.tell();
        let mut r = 0;
        if position >= 0 && offset > position {
            r = stream.seek(offset - position, SEEK_CUR);
        } else if offset < position {
            r = stream.seek(offset, SEEK_SET);
        }
        if r != 0 {
            diag::warning(format!("Failed to seek to position {offset} in the stream"));
            return Ok(None);
        }
    }
    Ok(Some(stream.copy_to_mem(maxlen).unwrap_or_default()))
}

/// `stream_get_line($stream, $length, $ending)`.
pub fn stream_get_line(
    stream: &mut Stream,
    length: i64,
    ending: &[u8],
) -> Result<Option<Vec<u8>>, ArgError> {
    if length < 0 {
        return Err(ArgError::new(
            2,
            "($length) must be greater than or equal to 0",
        ));
    }
    let len = if length == 0 { SOCK_CHUNK_SIZE } else { length };
    Ok(stream.get_record(usize::try_from(len).unwrap_or(usize::MAX), ending))
}

/// `stream_copy_to_stream($from, $to, $length, $offset)`.
pub fn stream_copy_to_stream(
    src: &mut Stream,
    dest: &mut Stream,
    maxlen: Option<i64>,
    pos: i64,
) -> Option<i64> {
    if pos > 0 && src.seek(pos, SEEK_SET) < 0 {
        diag::warning(format!("Failed to seek to position {pos} in the stream"));
        return None;
    }
    let max = match maxlen {
        None => None,
        Some(m) if m < 0 => None,
        Some(m) => Some(usize::try_from(m).unwrap_or(usize::MAX)),
    };
    let (ok, len) = copy_to_stream(src, dest, max);
    ok.then(|| i64::try_from(len).unwrap_or(i64::MAX))
}

/// `stream_set_write_buffer()` / `stream_set_read_buffer()`: 0 or -1.
pub fn stream_set_buffer(stream: &mut Stream, read: bool, size: i64) -> i64 {
    let (mode, sz) = if size == 0 {
        (BufferMode::None, None)
    } else {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        (BufferMode::Full, Some(size as usize))
    };
    let ret = if read {
        stream.set_option(&mut StreamOption::ReadBuffer(mode, sz))
    } else {
        stream.set_option(&mut StreamOption::WriteBuffer(mode, sz))
    };
    if ret == 0 {
        0
    } else {
        -1
    }
}

/// `stream_set_chunk_size()`.
pub fn stream_set_chunk_size(stream: &mut Stream, size: i64) -> Result<i64, ArgError> {
    if size <= 0 {
        return Err(ArgError::new(2, "($size) must be greater than 0"));
    }
    if size > i64::from(i32::MAX) {
        return Err(ArgError::new(2, "($size) is too large"));
    }
    let ret = stream.set_chunk_size(usize::try_from(size).unwrap_or(1));
    Ok(if ret > 0 { ret } else { -1 })
}

/// `stream_set_blocking()`.
pub fn stream_set_blocking(stream: &mut Stream, enable: bool) -> bool {
    stream.set_option(&mut StreamOption::Blocking(enable)) != -1
}

/// `stream_set_timeout()`.
pub fn stream_set_timeout(stream: &mut Stream, seconds: i64, microseconds: i64) -> bool {
    let mut sec = seconds + microseconds / 1_000_000;
    let mut usec = microseconds % 1_000_000;
    if usec < 0 {
        sec -= 1;
        usec += 1_000_000;
    }
    stream.set_option(&mut StreamOption::ReadTimeout(sec, usec)) == OPT_OK
}

/// `stream_is_local()` for a stream.
pub fn stream_is_local(stream: &Stream) -> bool {
    stream.wrapper.as_ref().is_some_and(|w| !w.is_url)
}

/// `stream_is_local()` for a URL.
pub fn url_is_local(url: &[u8]) -> bool {
    wrapper::locate(url, 0).is_some_and(|l| !l.wrapper.is_url())
}

/// `fgetcsv()` glue: reads the first line (with `fgets`-style `length`)
/// and hands it to `parse` together with a continuation reader for quoted
/// fields spanning lines. The CSV parser itself lives in `php-stdstr`.
pub fn fgetcsv<R>(
    stream: &mut Stream,
    length: Option<i64>,
    parse: impl FnOnce(Vec<u8>, &mut dyn FnMut() -> Option<Vec<u8>>) -> R,
) -> Result<Option<R>, ArgError> {
    let line = match length {
        None | Some(0) => stream.get_line(None),
        Some(l) if l < 0 || l == i64::MAX => {
            return Err(ArgError::new(
                2,
                "($length) must be between 0 and 9223372036854775806",
            ))
        }
        Some(l) => {
            let size = usize::try_from(l).unwrap_or(usize::MAX - 1) + 1;
            // `emalloc(len + 1)` before reading: beyond `memory_limit` that
            // is the "Allowed memory size" fatal (left recorded for the VM).
            if php_memlimit::try_reserve(size).is_err() {
                return Ok(None);
            }
            stream.get_line(Some(size))
        }
    };
    let Some(line) = line else {
        return Ok(None);
    };
    let mut more = || stream.get_line(None);
    Ok(Some(parse(line, &mut more)))
}

/// `fputcsv()` glue: writes an already formatted record.
pub fn fputcsv_write(stream: &mut Stream, record: &[u8]) -> Option<i64> {
    stream
        .write(record)
        .ok()
        .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

/// `PHP_STREAM_FILTER_READ`.
pub const STREAM_FILTER_READ: i64 = 1;
/// `PHP_STREAM_FILTER_WRITE`.
pub const STREAM_FILTER_WRITE: i64 = 2;
/// `PHP_STREAM_FILTER_ALL`.
pub const STREAM_FILTER_ALL: i64 = 3;

/// `stream_filter_append()` / `stream_filter_prepend()`: attaches
/// `name` to the chains selected by `read_write` (or derived from the
/// stream mode when 0). Returns the id of the last filter attached (the
/// filter resource), or `None` (PHP `false`).
pub fn stream_filter_attach(
    stream: &mut Stream,
    name: &str,
    read_write: i64,
    params: Option<&crate::value::Value>,
    append: bool,
) -> Option<crate::filter::FilterId> {
    use crate::filter::ChainKind;
    let mut rw = read_write;
    if rw & STREAM_FILTER_ALL == 0 {
        if stream.mode.contains(['r', '+']) {
            rw |= STREAM_FILTER_READ;
        }
        if stream.mode.contains(['w', '+', 'a']) {
            rw |= STREAM_FILTER_WRITE;
        }
    }
    let mut last = None;
    for (bit, kind) in [
        (STREAM_FILTER_READ, ChainKind::Read),
        (STREAM_FILTER_WRITE, ChainKind::Write),
    ] {
        if rw & bit == 0 {
            continue;
        }
        let f = wrapper::create_filter(name, params, false)?;
        last = Some(if append {
            stream.append_filter(kind, f)?
        } else {
            stream.prepend_filter(kind, f)
        });
    }
    last
}

/// `stream_filter_remove()`: flushes and detaches the filter.
pub fn stream_filter_remove(stream: &mut Stream, id: crate::filter::FilterId) -> bool {
    if !stream.remove_filter(id) {
        diag::warning("Unable to flush filter, not removing");
        return false;
    }
    true
}

/// `umask($mask)`: sets the mask (when given) and returns the old one.
/// The mask the request found comes back at request end.
pub fn umask(mask: Option<i64>) -> i64 {
    let old = sys::current_umask();
    crate::state::note_request_umask(old);
    if let Some(m) = mask {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        sys::umask((m & 0o777) as u32);
    }
    i64::from(old)
}

/// `realpath()`.
pub fn realpath(path: &[u8]) -> Option<Vec<u8>> {
    let p = paths::realpath(path)?;
    if !paths::check_open_basedir(&p) {
        return None;
    }
    Some(p)
}

/// `readlink()`.
pub fn readlink(path: &[u8]) -> Option<Vec<u8>> {
    if !paths::check_open_basedir(path) {
        return None;
    }
    match std::fs::read_link(os_path(path)) {
        Ok(p) => Some(crate::util::path_bytes(&p)),
        Err(e) => {
            diag::warning(sys::error_text(&e));
            None
        }
    }
}

/// `linkinfo()`: the `st_dev` of the link, or -1.
pub fn linkinfo(path: &[u8]) -> i64 {
    if !paths::check_open_basedir(path) {
        return -1;
    }
    match crate::stat::lstat_path(path) {
        Ok(sb) => sb.dev,
        Err(e) => {
            diag::warning(sys::error_text(&e));
            -1
        }
    }
}

fn is_url_path(p: &[u8]) -> bool {
    wrapper::locate(p, wrapper::STREAM_LOCATE_WRAPPERS_ONLY).is_some()
}

/// `symlink($target, $link)`.
pub fn symlink(target: &[u8], link: &[u8]) -> bool {
    let Some(source_p) = paths::expand_filepath(link) else {
        diag::warning("No such file or directory");
        return false;
    };
    let dir = crate::pathname::dirname(&source_p);
    let Some(dest_p) = paths::expand_filepath_relative(target, Some(&dir)) else {
        diag::warning("No such file or directory");
        return false;
    };
    if is_url_path(&source_p) || is_url_path(&dest_p) {
        diag::warning("Unable to symlink to a URL");
        return false;
    }
    if !paths::check_open_basedir(&dest_p) || !paths::check_open_basedir(&source_p) {
        return false;
    }
    match std::os::unix::fs::symlink(os_path(target), os_path(&source_p)) {
        Ok(()) => true,
        Err(e) => {
            diag::warning(sys::error_text(&e));
            false
        }
    }
}

/// `link($target, $link)`.
pub fn link(target: &[u8], link: &[u8]) -> bool {
    let (Some(source_p), Some(dest_p)) =
        (paths::expand_filepath(link), paths::expand_filepath(target))
    else {
        diag::warning("No such file or directory");
        return false;
    };
    if is_url_path(&source_p) || is_url_path(&dest_p) {
        diag::warning("Unable to link to a URL");
        return false;
    }
    if !paths::check_open_basedir(&dest_p) || !paths::check_open_basedir(&source_p) {
        return false;
    }
    match std::fs::hard_link(os_path(target), os_path(link)) {
        Ok(()) => true,
        Err(e) => {
            diag::warning(sys::error_text(&e));
            false
        }
    }
}

/// `fnmatch()` (see [`crate::fnmatch`]); emits the length warnings.
pub fn fnmatch(pattern: &[u8], filename: &[u8], flags: i32) -> bool {
    match crate::fnmatch::php_fnmatch(pattern, filename, flags) {
        Ok(b) => b,
        Err(msg) => {
            diag::warning(msg);
            false
        }
    }
}

#[cfg(test)]
mod tests;
