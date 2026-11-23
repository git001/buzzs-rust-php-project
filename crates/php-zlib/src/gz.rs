// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! zlib's `gzFile` (`gzlib.c`, `gzread.c`, `gzwrite.c` of zlib 1.3) over any
//! `Read + Write + Seek`, as used by the `compress.zlib://` wrapper and
//! `gzopen()`/`gzread()`/`gzwrite()`/`gzseek()`/`gzrewind()`/`gzeof()`.
//!
//! PHP opens the inner stream, then calls `gzdopen(dup(fd), mode)`; the
//! wrapper glue does the same with [`GzStream::open`]. The PHP stream ops
//! (`php_gziop_*`) map onto this type as follows:
//!
//! * read: [`GzStream::read`]; after each call the stream's `eof` flag is
//!   [`GzStream::eof`]; on error report [`GzStream::io_error_report`]`("Read",
//!   count)` as a notice when it returns `Some`.
//! * write: [`GzStream::write`] (`0` on error, like `gzwrite()`).
//! * seek: [`GzStream::seek`]; `SEEK_END` is rejected by PHP before reaching
//!   zlib with the warning [`SEEK_END_MESSAGE`].
//! * flush: [`GzStream::flush`]`(Z_SYNC_FLUSH)` (PHP flushes before closing a
//!   written stream).
//! * close: [`GzStream::close`].
//!
//! Compressed output is byte-identical to zlib's: the same buffer sizes
//! (8 KiB input/output), the same `deflate()` call sequence, gzip wrapping
//! with `DEF_MEM_LEVEL`.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::constants::{DEF_MEM_LEVEL, MAX_WBITS, Z_DEFAULT_COMPRESSION};
use crate::ffi::{strerror, Heap, ZStream};

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_NEED_DICT: i32 = 2;
const Z_ERRNO: i32 = -1;
const Z_STREAM_ERROR: i32 = -2;
const Z_DATA_ERROR: i32 = -3;
const Z_MEM_ERROR: i32 = -4;
const Z_BUF_ERROR: i32 = -5;
const Z_NO_FLUSH: i32 = 0;
const Z_FINISH: i32 = 4;

/// `GZBUFSIZE`.
const GZBUFSIZE: usize = 8192;

/// The warning PHP emits for `gzseek($fp, …, SEEK_END)`.
pub const SEEK_END_MESSAGE: &str = "SEEK_END is not supported";

/// `SEEK_SET`.
pub const SEEK_SET: i32 = 0;
/// `SEEK_CUR`.
pub const SEEK_CUR: i32 = 1;
/// `SEEK_END`.
pub const SEEK_END: i32 = 2;

/// A failed `gzFile` operation (zlib's `-1`); details are available from
/// [`GzStream::error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GzError;

impl std::fmt::Display for GzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gzip stream operation failed")
    }
}

impl std::error::Error for GzError {}

/// A byte count as a file offset.
fn off(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    Look,
    Copy,
    Gzip,
}

/// A `gzFile`.
// The flags mirror zlib's `gz_state` one to one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct GzStream<F> {
    inner: F,
    mode: Mode,
    path: String,
    /// Buffer size; zero until buffers are allocated.
    size: usize,
    want: usize,
    /// Input buffer (`want` when reading, `2 * want` when writing).
    inbuf: Vec<u8>,
    /// `strm.next_in` (offset into `inbuf`) and `strm.avail_in`.
    in_next: usize,
    in_avail: usize,
    /// Output buffer (`2 * want` when reading, `want` when writing).
    out: Vec<u8>,
    /// `x.next` (offset into `out`), `x.have`, `x.pos`.
    x_next: usize,
    x_have: usize,
    x_pos: i64,
    /// Writing: `strm.next_out` as an offset into `out`.
    out_pos: usize,
    direct: bool,
    how: How,
    start: u64,
    eof: bool,
    past: bool,
    level: i32,
    strategy: i32,
    reset: bool,
    seek: bool,
    skip: i64,
    err: i32,
    msg: Option<String>,
    errno: Option<i32>,
    z: Option<ZStream>,
}

impl<F: Read + Write + Seek> GzStream<F> {
    /// `gzdopen(fd, mode)`: wraps an already-open stream. `fd` is only used
    /// in error messages (`"<fd:N>"`). Returns `None` when the mode is not
    /// acceptable to zlib (no `r`/`w`/`a`, a `+`, or `T` when reading); the
    /// inner stream is handed back in that case.
    pub fn open(mut inner: F, mode: &str, fd: i32) -> Result<Self, F> {
        let mut gz_mode = None;
        let mut append = false;
        let mut level = Z_DEFAULT_COMPRESSION;
        let mut strategy = 0;
        let mut direct = false;
        for b in mode.bytes() {
            match b {
                b'0'..=b'9' => level = i32::from(b - b'0'),
                b'r' => {
                    gz_mode = Some(Mode::Read);
                    append = false;
                }
                b'w' => {
                    gz_mode = Some(Mode::Write);
                    append = false;
                }
                b'a' => {
                    gz_mode = Some(Mode::Write);
                    append = true;
                }
                b'+' => return Err(inner),
                b'f' => strategy = 1,
                b'h' => strategy = 2,
                b'R' => strategy = 3,
                b'F' => strategy = 4,
                b'T' => direct = true,
                _ => {}
            }
        }
        let Some(mode) = gz_mode else {
            return Err(inner);
        };
        if mode == Mode::Read {
            if direct {
                return Err(inner);
            }
            direct = true;
        }
        if append {
            let _ = inner.seek(SeekFrom::End(0));
        }
        let start = if mode == Mode::Read {
            inner.stream_position().unwrap_or(0)
        } else {
            0
        };
        let mut gz = GzStream {
            inner,
            mode,
            path: format!("<fd:{fd}>"),
            size: 0,
            want: GZBUFSIZE,
            inbuf: Vec::new(),
            in_next: 0,
            in_avail: 0,
            out: Vec::new(),
            x_next: 0,
            x_have: 0,
            x_pos: 0,
            out_pos: 0,
            direct,
            how: How::Look,
            start,
            eof: false,
            past: false,
            level,
            strategy,
            reset: false,
            seek: false,
            skip: 0,
            err: Z_OK,
            msg: None,
            errno: None,
            z: None,
        };
        gz.gz_reset();
        Ok(gz)
    }

    /// Whether the stream was opened for writing.
    pub fn is_writing(&self) -> bool {
        self.mode == Mode::Write
    }

    /// The inner stream.
    pub fn get_ref(&self) -> &F {
        &self.inner
    }

    /// The inner stream, mutably.
    pub fn get_mut(&mut self) -> &mut F {
        &mut self.inner
    }

    fn gz_reset(&mut self) {
        self.x_have = 0;
        if self.mode == Mode::Read {
            self.eof = false;
            self.past = false;
            self.how = How::Look;
        } else {
            self.reset = false;
        }
        self.seek = false;
        self.gz_error(Z_OK, None);
        self.x_pos = 0;
        self.in_avail = 0;
    }

    /// `gz_error()`.
    fn gz_error(&mut self, err: i32, msg: Option<&str>) {
        self.msg = None;
        if err != Z_OK && err != Z_BUF_ERROR {
            self.x_have = 0;
        }
        self.err = err;
        if err != Z_ERRNO {
            self.errno = None;
        }
        if let Some(m) = msg {
            if err != Z_MEM_ERROR {
                self.msg = Some(format!("{}: {m}", self.path));
            }
        }
    }

    fn io_error(&mut self, e: &io::Error) {
        let errno = e.raw_os_error().unwrap_or(libc::EIO);
        self.errno = Some(errno);
        self.gz_error(Z_ERRNO, Some(&strerror(errno)));
        self.errno = Some(errno);
    }

    /// `gzerror()`: the error code and message (`"<fd:N>: …"`).
    pub fn error(&self) -> (i32, Option<&str>) {
        let msg = if self.err == Z_MEM_ERROR {
            Some("out of memory")
        } else {
            self.msg.as_deref().or(Some(""))
        };
        (self.err, msg)
    }

    /// `php_gziop_report_errors()`: when the last error was an I/O error
    /// (`Z_ERRNO`), the notice text `"<verb> of <count> bytes failed with
    /// errno=<n> <strerror>"`.
    pub fn io_error_report(&self, verb: &str, count: usize) -> Option<String> {
        if self.err != Z_ERRNO {
            return None;
        }
        let errno = self.errno.unwrap_or(libc::EIO);
        Some(format!(
            "{verb} of {count} bytes failed with errno={errno} {}",
            strerror(errno)
        ))
    }

    // ---------------------------------------------------------------
    // Reading (gzread.c)
    // ---------------------------------------------------------------

    /// `gz_load()`: reads up to `len` bytes into `out[at..]` (or `inbuf`).
    fn gz_load_into(&mut self, which: Buf, at: usize, len: usize) -> Result<usize, GzError> {
        let mut have = 0usize;
        let mut last = 1usize;
        while have < len {
            let buf = match which {
                Buf::In => &mut self.inbuf[at + have..at + len],
                Buf::Out => &mut self.out[at + have..at + len],
            };
            match self.inner.read(buf) {
                Ok(0) => {
                    last = 0;
                    break;
                }
                Ok(n) => {
                    have += n;
                    last = n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.io_error(&e);
                    return Err(GzError);
                }
            }
        }
        if last == 0 {
            self.eof = true;
        }
        Ok(have)
    }

    /// `gz_load()` straight into a caller buffer.
    fn gz_load_user(&mut self, buf: &mut [u8]) -> Result<usize, GzError> {
        let mut have = 0usize;
        let mut last = 1usize;
        while have < buf.len() {
            match self.inner.read(&mut buf[have..]) {
                Ok(0) => {
                    last = 0;
                    break;
                }
                Ok(n) => {
                    have += n;
                    last = n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.io_error(&e);
                    return Err(GzError);
                }
            }
        }
        if last == 0 {
            self.eof = true;
        }
        Ok(have)
    }

    /// `gz_avail()`.
    fn gz_avail(&mut self) -> Result<(), GzError> {
        if self.err != Z_OK && self.err != Z_BUF_ERROR {
            return Err(GzError);
        }
        if !self.eof {
            if self.in_avail > 0 {
                self.inbuf
                    .copy_within(self.in_next..self.in_next + self.in_avail, 0);
            }
            let got = self.gz_load_into(Buf::In, self.in_avail, self.size - self.in_avail)?;
            self.in_avail += got;
            self.in_next = 0;
        }
        Ok(())
    }

    /// `gz_look()`.
    fn gz_look(&mut self) -> Result<(), GzError> {
        if self.size == 0 {
            self.inbuf = vec![0; self.want];
            self.out = vec![0; self.want << 1];
            self.size = self.want;
            if let Ok(z) = ZStream::inflate_init(MAX_WBITS + 16, Heap::System) {
                self.z = Some(z);
            } else {
                self.size = 0;
                self.gz_error(Z_MEM_ERROR, Some("out of memory"));
                return Err(GzError);
            }
        }
        if self.in_avail < 2 {
            self.gz_avail()?;
            if self.in_avail == 0 {
                return Ok(());
            }
        }
        if self.in_avail > 1
            && self.inbuf[self.in_next] == 31
            && self.inbuf[self.in_next + 1] == 139
        {
            if let Some(z) = self.z.as_mut() {
                z.reset();
            }
            self.how = How::Gzip;
            self.direct = false;
            return Ok(());
        }
        if !self.direct {
            // Trailing garbage after a gzip stream: ignore it.
            self.in_avail = 0;
            self.eof = true;
            self.x_have = 0;
            return Ok(());
        }
        let n = self.in_avail;
        self.out[..n].copy_from_slice(&self.inbuf[self.in_next..self.in_next + n]);
        self.x_next = 0;
        self.x_have = n;
        self.in_avail = 0;
        self.how = How::Copy;
        self.direct = true;
        Ok(())
    }

    /// `gz_decomp()` into `target`; returns the bytes produced (which become
    /// `x.have`).
    fn gz_decomp(&mut self, target: &mut [u8]) -> Result<usize, GzError> {
        let mut ret = Z_OK;
        let mut produced = 0usize;
        loop {
            if self.in_avail == 0 {
                self.gz_avail()?;
            }
            if self.in_avail == 0 {
                self.gz_error(Z_BUF_ERROR, Some("unexpected end of file"));
                break;
            }
            let Some(z) = self.z.as_mut() else {
                return Err(GzError);
            };
            let step = z.inflate(
                &self.inbuf[self.in_next..self.in_next + self.in_avail],
                &mut target[produced..],
                Z_NO_FLUSH,
            );
            self.in_next += step.consumed;
            self.in_avail -= step.consumed;
            produced += step.produced;
            ret = step.status;
            if ret == Z_STREAM_ERROR || ret == Z_NEED_DICT {
                self.gz_error(
                    Z_STREAM_ERROR,
                    Some("internal error: inflate stream corrupt"),
                );
                return Err(GzError);
            }
            if ret == Z_MEM_ERROR {
                self.gz_error(Z_MEM_ERROR, Some("out of memory"));
                return Err(GzError);
            }
            if ret == Z_DATA_ERROR {
                let msg = z.msg().unwrap_or_else(|| "compressed data error".into());
                self.gz_error(Z_DATA_ERROR, Some(&msg));
                return Err(GzError);
            }
            if !(produced < target.len() && ret != Z_STREAM_END) {
                break;
            }
        }
        if ret == Z_STREAM_END {
            self.how = How::Look;
        }
        Ok(produced)
    }

    /// `gz_fetch()`.
    fn gz_fetch(&mut self) -> Result<(), GzError> {
        loop {
            match self.how {
                How::Look => {
                    self.gz_look()?;
                    if self.how == How::Look {
                        return Ok(());
                    }
                }
                How::Copy => {
                    let len = self.size << 1;
                    self.x_have = self.gz_load_into(Buf::Out, 0, len)?;
                    self.x_next = 0;
                    return Ok(());
                }
                How::Gzip => {
                    let mut out = std::mem::take(&mut self.out);
                    let r = self.gz_decomp(&mut out);
                    self.out = out;
                    self.x_have = r?;
                    self.x_next = 0;
                }
            }
            if !(self.x_have == 0 && (!self.eof || self.in_avail > 0)) {
                return Ok(());
            }
        }
    }

    /// `gz_skip()`.
    fn gz_skip(&mut self, mut len: i64) -> Result<(), GzError> {
        while len > 0 {
            if self.x_have > 0 {
                let n = usize::try_from(len).map_or(self.x_have, |l| l.min(self.x_have));
                self.x_have -= n;
                self.x_next += n;
                self.x_pos += off(n);
                len -= off(n);
            } else if self.eof && self.in_avail == 0 {
                break;
            } else {
                self.gz_fetch()?;
            }
        }
        Ok(())
    }

    /// `gzread()`: reads up to `buf.len()` uncompressed bytes. `Ok(0)` means
    /// end of file; `Err` corresponds to `gzread()` returning -1.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, GzError> {
        if self.mode != Mode::Read || (self.err != Z_OK && self.err != Z_BUF_ERROR) {
            return Err(GzError);
        }
        let len = buf.len().min(i32::MAX as usize);
        let got = self.gz_read(&mut buf[..len]);
        if got == 0 && self.err != Z_OK && self.err != Z_BUF_ERROR {
            return Err(GzError);
        }
        Ok(got)
    }

    /// `gz_read()`.
    fn gz_read(&mut self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        if self.seek {
            self.seek = false;
            if self.gz_skip(self.skip).is_err() {
                return 0;
            }
        }
        let mut got = 0usize;
        while got < buf.len() {
            let mut n = buf.len() - got;
            if self.x_have > 0 {
                n = n.min(self.x_have);
                buf[got..got + n].copy_from_slice(&self.out[self.x_next..self.x_next + n]);
                self.x_next += n;
                self.x_have -= n;
            } else if self.eof && self.in_avail == 0 {
                self.past = true;
                break;
            } else if self.how == How::Look || n < (self.size << 1) {
                if self.gz_fetch().is_err() {
                    return 0;
                }
                continue;
            } else if self.how == How::Copy {
                match self.gz_load_user(&mut buf[got..got + n]) {
                    Ok(m) => n = m,
                    Err(GzError) => return 0,
                }
            } else {
                match self.gz_decomp(&mut buf[got..got + n]) {
                    Ok(m) => {
                        n = m;
                        self.x_have = 0;
                    }
                    Err(GzError) => return 0,
                }
            }
            got += n;
            self.x_pos += off(n);
        }
        got
    }

    /// `gzrewind()`.
    pub fn rewind(&mut self) -> Result<(), GzError> {
        if self.mode != Mode::Read || (self.err != Z_OK && self.err != Z_BUF_ERROR) {
            return Err(GzError);
        }
        if self.inner.seek(SeekFrom::Start(self.start)).is_err() {
            return Err(GzError);
        }
        self.gz_reset();
        Ok(())
    }

    /// `gzeof()`: true once a read was attempted past the end.
    pub fn eof(&self) -> bool {
        self.mode == Mode::Read && self.past
    }

    /// `gztell()`.
    pub fn tell(&self) -> i64 {
        self.x_pos + if self.seek { self.skip } else { 0 }
    }

    /// `gzseek()`: `whence` is `SEEK_SET` or `SEEK_CUR`; returns the new
    /// uncompressed position, or `Err` (zlib's -1). Seeking backwards while
    /// reading rewinds and re-reads; seeking forward while writing writes
    /// zeros (lazily, on the next write/flush/close).
    pub fn seek(&mut self, offset: i64, whence: i32) -> Result<i64, GzError> {
        if self.err != Z_OK && self.err != Z_BUF_ERROR {
            return Err(GzError);
        }
        if whence != SEEK_SET && whence != SEEK_CUR {
            return Err(GzError);
        }
        let mut offset = offset;
        if whence == SEEK_SET {
            offset -= self.x_pos;
        } else if self.seek {
            offset += self.skip;
        }
        self.seek = false;

        if self.mode == Mode::Read && self.how == How::Copy && self.x_pos + offset >= 0 {
            let rel = offset - off(self.x_have);
            if self.inner.seek(SeekFrom::Current(rel)).is_err() {
                return Err(GzError);
            }
            self.x_have = 0;
            self.eof = false;
            self.past = false;
            self.seek = false;
            self.gz_error(Z_OK, None);
            self.in_avail = 0;
            self.x_pos += offset;
            return Ok(self.x_pos);
        }

        if offset < 0 {
            if self.mode != Mode::Read {
                return Err(GzError);
            }
            offset += self.x_pos;
            if offset < 0 {
                return Err(GzError);
            }
            self.rewind()?;
        }

        if self.mode == Mode::Read {
            let n = usize::try_from(offset).map_or(self.x_have, |o| o.min(self.x_have));
            self.x_have -= n;
            self.x_next += n;
            self.x_pos += off(n);
            offset -= off(n);
        }

        if offset != 0 {
            self.seek = true;
            self.skip = offset;
        }
        Ok(self.x_pos + offset)
    }

    // ---------------------------------------------------------------
    // Writing (gzwrite.c)
    // ---------------------------------------------------------------

    /// `gz_init()`.
    fn gz_init(&mut self) -> Result<(), GzError> {
        self.inbuf = vec![0; self.want << 1];
        if !self.direct {
            self.out = vec![0; self.want];
            if let Ok(z) = ZStream::deflate_init(
                self.level,
                MAX_WBITS + 16,
                DEF_MEM_LEVEL,
                self.strategy,
                Heap::System,
            ) {
                self.z = Some(z);
            } else {
                self.gz_error(Z_MEM_ERROR, Some("out of memory"));
                return Err(GzError);
            }
        }
        self.size = self.want;
        if !self.direct {
            self.out_pos = 0;
            self.x_next = 0;
        }
        Ok(())
    }

    fn write_all_inner(&mut self, from: Buf, start: usize, end: usize) -> Result<(), GzError> {
        let mut pos = start;
        while pos < end {
            let buf = match from {
                Buf::In => &self.inbuf[pos..end],
                Buf::Out => &self.out[pos..end],
            };
            match self.inner.write(buf) {
                Ok(0) => {
                    let e = io::Error::from(io::ErrorKind::WriteZero);
                    self.io_error(&e);
                    return Err(GzError);
                }
                Ok(n) => pos += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.io_error(&e);
                    return Err(GzError);
                }
            }
        }
        Ok(())
    }

    /// `gz_comp()`. The input is `inbuf[in_next..in_next + in_avail]`, or
    /// `ext` when given (which is then consumed completely).
    fn gz_comp(&mut self, flush: i32, mut ext: Option<&[u8]>) -> Result<(), GzError> {
        if self.size == 0 {
            self.gz_init()?;
        }
        if self.direct {
            if let Some(data) = ext {
                let mut pos = 0;
                while pos < data.len() {
                    match self.inner.write(&data[pos..]) {
                        Ok(0) => {
                            self.io_error(&io::Error::from(io::ErrorKind::WriteZero));
                            return Err(GzError);
                        }
                        Ok(n) => pos += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            self.io_error(&e);
                            return Err(GzError);
                        }
                    }
                }
            } else {
                let (s, e) = (self.in_next, self.in_next + self.in_avail);
                self.write_all_inner(Buf::In, s, e)?;
                self.in_next = e;
                self.in_avail = 0;
            }
            return Ok(());
        }
        let ext_len = |e: Option<&[u8]>| e.map_or(0, <[u8]>::len);
        if self.reset {
            let avail = if ext.is_some() {
                ext_len(ext)
            } else {
                self.in_avail
            };
            if avail == 0 {
                return Ok(());
            }
            if let Some(z) = self.z.as_mut() {
                z.reset();
            }
            self.reset = false;
        }
        let mut ret = Z_OK;
        loop {
            let avail_out = self.size - self.out_pos;
            if avail_out == 0 || (flush != Z_NO_FLUSH && (flush != Z_FINISH || ret == Z_STREAM_END))
            {
                let (s, e) = (self.x_next, self.out_pos);
                self.write_all_inner(Buf::Out, s, e)?;
                self.x_next = e;
                if avail_out == 0 {
                    self.out_pos = 0;
                    self.x_next = 0;
                }
            }
            let Some(z) = self.z.as_mut() else {
                return Err(GzError);
            };
            let size = self.size;
            let step = if let Some(data) = ext {
                let st = z.deflate(data, &mut self.out[self.out_pos..size], flush);
                ext = Some(&data[st.consumed..]);
                st
            } else {
                let st = z.deflate(
                    &self.inbuf[self.in_next..self.in_next + self.in_avail],
                    &mut self.out[self.out_pos..size],
                    flush,
                );
                self.in_next += st.consumed;
                self.in_avail -= st.consumed;
                st
            };
            self.out_pos += step.produced;
            ret = step.status;
            if ret == Z_STREAM_ERROR {
                self.gz_error(
                    Z_STREAM_ERROR,
                    Some("internal error: deflate stream corrupt"),
                );
                return Err(GzError);
            }
            if step.produced == 0 {
                break;
            }
        }
        if flush == Z_FINISH {
            self.reset = true;
        }
        Ok(())
    }

    /// `gz_zero()`: compresses `len` zero bytes (a pending forward seek).
    fn gz_zero(&mut self, mut len: i64) -> Result<(), GzError> {
        if self.in_avail > 0 {
            self.gz_comp(Z_NO_FLUSH, None)?;
        }
        let mut first = true;
        while len > 0 {
            let n = usize::try_from(len).map_or(self.size, |l| l.min(self.size));
            if first {
                self.inbuf[..n].fill(0);
                first = false;
            }
            self.in_avail = n;
            self.in_next = 0;
            self.x_pos += off(n);
            self.gz_comp(Z_NO_FLUSH, None)?;
            len -= off(n);
        }
        Ok(())
    }

    /// `gzwrite()`: returns the number of bytes accepted, 0 on error.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.mode != Mode::Write || self.err != Z_OK {
            return 0;
        }
        if i32::try_from(data.len()).is_err() {
            self.gz_error(Z_DATA_ERROR, Some("requested length does not fit in int"));
            return 0;
        }
        self.gz_write(data)
    }

    fn gz_write(&mut self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        if self.size == 0 && self.gz_init().is_err() {
            return 0;
        }
        if self.seek {
            self.seek = false;
            if self.gz_zero(self.skip).is_err() {
                return 0;
            }
        }
        if data.len() < self.size {
            let mut rest = data;
            loop {
                if self.in_avail == 0 {
                    self.in_next = 0;
                }
                let have = self.in_next + self.in_avail;
                let copy = (self.size - have).min(rest.len());
                self.inbuf[have..have + copy].copy_from_slice(&rest[..copy]);
                self.in_avail += copy;
                self.x_pos += off(copy);
                rest = &rest[copy..];
                if !rest.is_empty() && self.gz_comp(Z_NO_FLUSH, None).is_err() {
                    return 0;
                }
                if rest.is_empty() {
                    break;
                }
            }
        } else {
            if self.in_avail > 0 && self.gz_comp(Z_NO_FLUSH, None).is_err() {
                return 0;
            }
            for chunk in data.chunks(u32::MAX as usize) {
                self.x_pos += off(chunk.len());
                if self.gz_comp(Z_NO_FLUSH, Some(chunk)).is_err() {
                    return 0;
                }
            }
            self.in_avail = 0;
        }
        data.len()
    }

    /// `gzflush()`: returns the zlib error state (`Z_OK` on success).
    pub fn flush(&mut self, flush: i32) -> i32 {
        if self.mode != Mode::Write || self.err != Z_OK {
            return Z_STREAM_ERROR;
        }
        if !(0..=Z_FINISH).contains(&flush) {
            return Z_STREAM_ERROR;
        }
        if self.seek {
            self.seek = false;
            if self.gz_zero(self.skip).is_err() {
                return self.err;
            }
        }
        let _ = self.gz_comp(flush, None);
        let _ = self.inner.flush();
        self.err
    }

    /// `gzsetparams()`.
    pub fn set_params(&mut self, level: i32, strategy: i32) -> i32 {
        if self.mode != Mode::Write || self.err != Z_OK || self.direct {
            return Z_STREAM_ERROR;
        }
        if level == self.level && strategy == self.strategy {
            return Z_OK;
        }
        if self.seek {
            self.seek = false;
            if self.gz_zero(self.skip).is_err() {
                return self.err;
            }
        }
        if self.size > 0 {
            if self.in_avail > 0 && self.gz_comp(5, None).is_err() {
                return self.err;
            }
            if let Some(z) = self.z.as_mut() {
                z.deflate_params(level, strategy);
            }
        }
        self.level = level;
        self.strategy = strategy;
        Z_OK
    }

    /// `gzclose()`: finishes the gzip stream when writing and returns the
    /// inner stream with zlib's result code (`Z_OK`, `Z_BUF_ERROR` after a
    /// truncated read, or an error).
    pub fn close(mut self) -> (F, i32) {
        let ret = match self.mode {
            Mode::Read => {
                if self.err == Z_BUF_ERROR {
                    Z_BUF_ERROR
                } else {
                    Z_OK
                }
            }
            Mode::Write => {
                let mut ret = Z_OK;
                if self.seek {
                    self.seek = false;
                    if self.gz_zero(self.skip).is_err() {
                        ret = self.err;
                    }
                }
                if self.gz_comp(Z_FINISH, None).is_err() {
                    ret = self.err;
                }
                if self.inner.flush().is_err() {
                    ret = Z_ERRNO;
                }
                ret
            }
        };
        if let Some(mut z) = self.z.take() {
            z.end();
        }
        (self.inner, ret)
    }
}

#[derive(Debug, Clone, Copy)]
enum Buf {
    In,
    Out,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write_gz(chunks: &[&[u8]], mode: &str, sync_before_close: bool) -> Vec<u8> {
        let mut gz = GzStream::open(Cursor::new(Vec::new()), mode, 3).unwrap();
        for c in chunks {
            assert_eq!(gz.write(c), c.len());
        }
        if sync_before_close {
            assert_eq!(gz.flush(2), Z_OK);
        }
        let (cur, ret) = gz.close();
        assert_eq!(ret, Z_OK);
        cur.into_inner()
    }

    fn read_all(data: Vec<u8>, chunk: usize) -> (Vec<u8>, bool) {
        let mut gz = GzStream::open(Cursor::new(data), "rb", 3).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0u8; chunk];
        loop {
            let n = gz.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
            if n == 0 || gz.eof() {
                break;
            }
        }
        (out, gz.eof())
    }

    fn hex(b: &[u8]) -> String {
        b.iter().fold(String::new(), |mut acc, x| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{x:02x}");
            acc
        })
    }

    #[test]
    fn mode_rules() {
        assert!(GzStream::open(Cursor::new(Vec::new()), "r+", 3).is_err());
        assert!(GzStream::open(Cursor::new(Vec::new()), "c", 3).is_err());
        assert!(GzStream::open(Cursor::new(Vec::new()), "rT", 3).is_err());
        assert!(GzStream::open(Cursor::new(Vec::new()), "wb9", 3).is_ok());
        assert!(GzStream::open(Cursor::new(Vec::new()), "x", 3).is_err());
    }

    #[test]
    fn write_matches_oracle() {
        // php -r '$f=gzopen("x.gz","w"); gzwrite($f,"hello world\n"); gzclose($f);'
        let out = write_gz(&[b"hello world\n"], "w", true);
        assert_eq!(
            hex(&out),
            "1f8b0800000000000003ca48cdc9c95728cf2fca49e102000000ffff03002d3b08af0c000000"
        );
        // gzwrite("hello"); fflush(); gzwrite("world"); gzclose()
        let out = write_gz(&[b"hello"], "w", true);
        let mut gz = GzStream::open(Cursor::new(out), "w", 3).unwrap();
        // (re-open in write mode on a fresh buffer to replay the sequence)
        gz.get_mut().get_mut().clear();
        gz.get_mut().set_position(0);
        gz.write(b"hello");
        assert_eq!(gz.flush(2), Z_OK);
        gz.write(b"world");
        assert_eq!(gz.flush(2), Z_OK);
        let (cur, _) = gz.close();
        assert_eq!(
            hex(cur.get_ref()),
            "1f8b0800000000000003ca48cdc9c907000000ffff2acf2fca4901000000ffff0300ad20ebf90a000000"
        );
    }

    #[test]
    fn roundtrip_and_eof() {
        let text: Vec<u8> = (0..20000u32)
            .flat_map(|i| i.to_string().into_bytes())
            .collect();
        let packed = write_gz(
            &[&text[..100], &text[100..9000], &text[9000..]],
            "wb",
            false,
        );
        assert_eq!(crate::codec::gzdecode(&packed, 0).unwrap(), text);
        for chunk in [1usize, 7, 8192, 20000, 100_000] {
            let (back, eof) = read_all(packed.clone(), chunk);
            assert_eq!(back, text);
            assert!(eof);
        }
    }

    #[test]
    fn transparent_read_of_plain_file() {
        let (back, eof) = read_all(b"just plain text\nline 2\n".to_vec(), 5);
        assert_eq!(back, b"just plain text\nline 2\n");
        assert!(eof);
        let (back, _) = read_all(Vec::new(), 5);
        assert!(back.is_empty());
    }

    #[test]
    fn concatenated_members_and_trailing_garbage() {
        let mut data = crate::codec::gzencode(b"first ", -1, 31).unwrap();
        data.extend(crate::codec::gzencode(b"second", -1, 31).unwrap());
        data.extend(b"garbage");
        let (back, _) = read_all(data, 4);
        assert_eq!(back, b"first second");
    }

    #[test]
    fn seek_and_tell_reading() {
        let text = b"0123456789abcdefghij".repeat(1000);
        let packed = write_gz(&[&text], "w", false);
        let mut gz = GzStream::open(Cursor::new(packed), "r", 3).unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(gz.seek(15, SEEK_SET), Ok(15));
        assert_eq!(gz.tell(), 15);
        assert_eq!(gz.read(&mut buf), Ok(5));
        assert_eq!(&buf, b"fghij");
        assert_eq!(gz.seek(-10, SEEK_CUR), Ok(10));
        assert_eq!(gz.read(&mut buf), Ok(5));
        assert_eq!(&buf, b"abcde");
        assert_eq!(gz.seek(19_998, SEEK_SET), Ok(19_998));
        let mut big = [0u8; 10];
        assert_eq!(gz.read(&mut big), Ok(2));
        assert!(gz.eof());
        assert_eq!(gz.seek(-1, SEEK_SET), Err(GzError));
        assert_eq!(gz.seek(0, SEEK_END), Err(GzError));
        gz.rewind().unwrap();
        assert!(!gz.eof());
        assert_eq!(gz.read(&mut buf), Ok(5));
        assert_eq!(&buf, b"01234");
        // Seeking past the end is allowed; reads then hit EOF.
        assert_eq!(gz.seek(1_000_000, SEEK_SET), Ok(1_000_000));
        assert_eq!(gz.read(&mut buf), Ok(0));
        assert_eq!(gz.tell(), 20_000);
    }

    #[test]
    fn seek_while_writing_writes_zeros() {
        let mut gz = GzStream::open(Cursor::new(Vec::new()), "w9", 3).unwrap();
        gz.write(b"ab");
        assert_eq!(gz.seek(5, SEEK_SET), Ok(5));
        assert_eq!(gz.tell(), 5);
        // The pending skip is folded in, so this is still a forward seek.
        assert_eq!(gz.seek(-1, SEEK_CUR), Ok(4));
        gz.write(b"cd");
        assert_eq!(gz.flush(2), Z_OK);
        let (cur, _) = gz.close();
        assert_eq!(
            hex(cur.get_ref()),
            "1f8b08000000000002034a4c6260484e01000000ffff030054c4979506000000"
        );
    }

    #[test]
    fn read_mode_rejects_write_and_vice_versa() {
        let mut r = GzStream::open(Cursor::new(Vec::new()), "r", 3).unwrap();
        assert_eq!(r.write(b"x"), 0);
        assert_eq!(r.flush(2), Z_STREAM_ERROR);
        assert_eq!(r.set_params(9, 0), Z_STREAM_ERROR);
        let mut w = GzStream::open(Cursor::new(Vec::new()), "w", 3).unwrap();
        assert_eq!(w.read(&mut [0u8; 4]), Err(GzError));
        assert!(!w.eof());
        assert_eq!(w.set_params(1, 0), Z_OK);
    }

    #[test]
    fn corrupt_data_reports_error() {
        let mut packed = crate::codec::gzencode(&b"abc".repeat(100), -1, 31).unwrap();
        let n = packed.len();
        packed[n / 2] ^= 0xff;
        packed[n / 2 + 1] ^= 0xff;
        let mut gz = GzStream::open(Cursor::new(packed), "r", 7).unwrap();
        let mut buf = [0u8; 1000];
        let r = gz.read(&mut buf);
        let (code, msg) = gz.error();
        if r.is_err() {
            assert_eq!(code, Z_DATA_ERROR);
            assert!(msg.unwrap().starts_with("<fd:7>: "));
        }
        assert!(gz.io_error_report("Read", 1000).is_none());
    }

    #[test]
    fn truncated_stream() {
        let packed =
            crate::codec::gzencode(&b"The quick brown fox jumps".repeat(3), -1, 31).unwrap();
        let cut = packed[..packed.len() - 10].to_vec();
        let (back, eof) = read_all(cut, 100);
        assert!(eof);
        assert!(!back.is_empty());
    }

    #[test]
    fn append_mode_and_levels() {
        let mut cur = Cursor::new(b"PREFIX".to_vec());
        cur.set_position(0);
        let mut gz = GzStream::open(cur, "a9", 3).unwrap();
        gz.write(b"data");
        let (cur, _) = gz.close();
        let v = cur.into_inner();
        assert_eq!(&v[..6], b"PREFIX");
        assert_eq!(crate::codec::gzdecode(&v[6..], 0).unwrap(), b"data");
        // Level 9 sets XFL=2 in the gzip header.
        assert_eq!(v[6 + 8], 2);
    }

    fn crc32b(data: &[u8]) -> u32 {
        crate::ffi::crc32(0, data)
    }

    #[test]
    fn modes_match_oracle() {
        let t: Vec<u8> = (0..3000)
            .flat_map(|i| format!("{i},").into_bytes())
            .collect();
        for (mode, len, crc) in [
            ("wb1", 5601, 0x5596_0304u32),
            ("wf", 5864, 0xbd97_b108),
            ("wh", 5861, 0xfa52_2ec7),
            ("wR", 5861, 0x8788_f847),
            ("wF", 10515, 0x9370_a55c),
            ("wT", 13890, 0xdfed_a452),
            ("w0", 13923, 0x2a11_9c3d),
            ("w9", 6487, 0xbb09_f097),
        ] {
            let out = write_gz(&[&t[..5000], &t[5000..]], mode, true);
            assert_eq!((out.len(), crc32b(&out)), (len, crc), "mode {mode}");
        }
        // gzwrite("abc"); gzseek(20000); gzwrite(9000 x "Q"); gzclose()
        let mut gz = GzStream::open(Cursor::new(Vec::new()), "w", 3).unwrap();
        gz.write(b"abc");
        assert_eq!(gz.seek(20000, SEEK_SET), Ok(20000));
        gz.write(&[b'Q'; 9000]);
        gz.flush(2);
        let (cur, _) = gz.close();
        assert_eq!(
            (cur.get_ref().len(), crc32b(cur.get_ref())),
            (75, 0xc3e5_bdd3)
        );
        // compress.zlib:// with the "level" context option: gzsetparams(1, 0)
        let mut gz = GzStream::open(Cursor::new(Vec::new()), "w", 3).unwrap();
        assert_eq!(gz.set_params(1, 0), Z_OK);
        gz.write(&t);
        gz.flush(2);
        let (cur, _) = gz.close();
        assert_eq!(
            (cur.get_ref().len(), crc32b(cur.get_ref())),
            (5601, 0x5596_0304)
        );
    }

    #[test]
    fn large_writes_go_direct() {
        let big = vec![b'z'; 100_000];
        let packed = write_gz(&[b"head", &big, b"tail"], "w", false);
        let mut expect = b"head".to_vec();
        expect.extend(&big);
        expect.extend(b"tail");
        assert_eq!(crate::codec::gzdecode(&packed, 0).unwrap(), expect);
    }
}
