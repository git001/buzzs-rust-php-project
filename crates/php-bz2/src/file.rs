// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! libbz2's high-level `BZFILE` interface (`BZ2_bzopen`/`bzdopen`,
//! `BZ2_bzread`, `BZ2_bzwrite`, `BZ2_bzflush`, `BZ2_bzclose`,
//! `BZ2_bzerror`) ported over Rust `Read`/`Write` (bzip2 1.0.8
//! `bzlib.c`). Like the C code it decodes a single bzip2 stream and keeps
//! the last error code for `bzerror()`.

use std::io::{self, Read, Write};

use crate::ffi::{Heap, RawStream};
use crate::{
    BZ_DATA_ERROR, BZ_FINISH, BZ_FINISH_OK, BZ_IO_ERROR, BZ_OK, BZ_RUN, BZ_RUN_OK,
    BZ_SEQUENCE_ERROR, BZ_STREAM_END, BZ_UNEXPECTED_EOF,
};

/// `BZ_MAX_UNUSED`: the I/O buffer size.
const BZ_MAX_UNUSED: usize = 5000;

/// `bzerrorstrings[]`.
const ERROR_STRINGS: [&str; 16] = [
    "OK",
    "SEQUENCE_ERROR",
    "PARAM_ERROR",
    "MEM_ERROR",
    "DATA_ERROR",
    "DATA_ERROR_MAGIC",
    "IO_ERROR",
    "UNEXPECTED_EOF",
    "OUTBUFF_FULL",
    "CONFIG_ERROR",
    "???",
    "???",
    "???",
    "???",
    "???",
    "???",
];

/// The text `BZ2_bzerror()` gives for an error code (positive codes are
/// reported as `"OK"`).
pub fn error_string(code: i32) -> &'static str {
    let idx = usize::try_from(-code.min(0)).unwrap_or(0);
    ERROR_STRINGS.get(idx).copied().unwrap_or("???")
}

/// A `BZFILE`.
#[derive(Debug)]
pub struct BzFile<F> {
    handle: F,
    writing: bool,
    buf: Vec<u8>,
    in_next: usize,
    in_avail: usize,
    strm: RawStream,
    last_err: i32,
    /// The byte `myfeof()` pushed back with `ungetc()`.
    peeked: Option<u8>,
    /// `feof()` of the underlying handle.
    eof: bool,
    /// `ferror()` of the underlying handle.
    ferror: bool,
}

/// Interpretation of a `BZ2_bzopen()` mode string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenMode {
    /// `w` given (otherwise reading).
    pub writing: bool,
    /// `s`: small decompression.
    pub small: bool,
    /// A digit: block size (default 9).
    pub block_size_100k: i32,
}

impl OpenMode {
    /// Parses the mode the way `bzopen_or_bzdopen()` does.
    pub fn parse(mode: &str) -> Self {
        let mut m = OpenMode {
            writing: false,
            small: false,
            block_size_100k: 9,
        };
        for b in mode.bytes() {
            match b {
                b'r' => m.writing = false,
                b'w' => m.writing = true,
                b's' => m.small = true,
                b'0'..=b'9' => m.block_size_100k = i32::from(b - b'0'),
                _ => {}
            }
        }
        m
    }
}

impl<F> BzFile<F> {
    /// `BZ2_bzerror()`: the last error code (positive values reported as 0)
    /// and its name.
    pub fn error(&self) -> (i32, &'static str) {
        let err = self.last_err.min(0);
        (err, error_string(err))
    }

    /// The raw last error (`lastErr`, may be `BZ_STREAM_END`).
    pub fn last_error(&self) -> i32 {
        self.last_err
    }

    /// Whether this file writes.
    pub fn is_writing(&self) -> bool {
        self.writing
    }

    /// The underlying handle.
    pub fn get_ref(&self) -> &F {
        &self.handle
    }

    fn set_err(&mut self, err: i32) -> i32 {
        self.last_err = err;
        err
    }
}

impl<F: Read + Write> BzFile<F> {
    /// `BZ2_bzdopen(fd, mode)` over `handle`: `BZ2_bzWriteOpen()` with work
    /// factor 30 or `BZ2_bzReadOpen()`. `None` when libbz2 rejects the
    /// parameters (e.g. block size 0).
    pub fn open(handle: F, mode: &str) -> Option<Self> {
        let m = OpenMode::parse(mode);
        let strm = if m.writing {
            RawStream::compress_init(m.block_size_100k, 30, Heap::System).ok()?
        } else {
            RawStream::decompress_init(m.small, Heap::System).ok()?
        };
        Some(BzFile {
            handle,
            writing: m.writing,
            buf: vec![0; BZ_MAX_UNUSED],
            in_next: 0,
            in_avail: 0,
            strm,
            last_err: BZ_OK,
            peeked: None,
            eof: false,
            ferror: false,
        })
    }

    /// `fread()` into `buf[..]` (a full-buffer read like stdio's).
    fn fread(&mut self) -> usize {
        let mut n = 0;
        if let Some(b) = self.peeked.take() {
            self.buf[0] = b;
            n = 1;
        }
        while n < BZ_MAX_UNUSED {
            match self.handle.read(&mut self.buf[n..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.ferror = true;
                    break;
                }
            }
        }
        n
    }

    /// `myfeof()`: `fgetc()` + `ungetc()`.
    fn myfeof(&mut self) -> bool {
        if self.peeked.is_some() {
            return false;
        }
        if self.eof {
            return true;
        }
        let mut b = [0u8; 1];
        loop {
            match self.handle.read(&mut b) {
                Ok(0) => {
                    self.eof = true;
                    return true;
                }
                Ok(_) => {
                    self.peeked = Some(b[0]);
                    return false;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.ferror = true;
                    return true;
                }
            }
        }
    }

    /// `BZ2_bzRead()`: returns the bytes decoded into `out` and sets the
    /// error state (`BZ_OK`, `BZ_STREAM_END` or an error).
    fn bz_read(&mut self, out: &mut [u8]) -> usize {
        self.set_err(BZ_OK);
        if self.writing {
            self.set_err(BZ_SEQUENCE_ERROR);
            return 0;
        }
        if out.is_empty() {
            return 0;
        }
        let mut produced = 0usize;
        loop {
            if self.ferror {
                self.set_err(BZ_IO_ERROR);
                return 0;
            }
            if self.in_avail == 0 && !self.myfeof() {
                let n = self.fread();
                if self.ferror {
                    self.set_err(BZ_IO_ERROR);
                    return 0;
                }
                self.in_next = 0;
                self.in_avail = n;
            }
            let step = self.strm.decompress(
                &self.buf[self.in_next..self.in_next + self.in_avail],
                &mut out[produced..],
            );
            self.in_next += step.consumed;
            self.in_avail -= step.consumed;
            produced += step.produced;
            let ret = step.status;
            if ret != BZ_OK && ret != BZ_STREAM_END {
                self.set_err(ret);
                return 0;
            }
            if ret == BZ_OK && self.myfeof() && self.in_avail == 0 && produced < out.len() {
                self.set_err(BZ_UNEXPECTED_EOF);
                return 0;
            }
            if ret == BZ_STREAM_END {
                self.set_err(BZ_STREAM_END);
                return produced;
            }
            if produced == out.len() {
                self.set_err(BZ_OK);
                return produced;
            }
        }
    }

    /// `BZ2_bzread()`: the number of bytes read, 0 at the end of the
    /// stream, `Err(())`-like `None` on error (C returns -1).
    pub fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.last_err == BZ_STREAM_END {
            return Some(0);
        }
        let len = out.len().min(i32::MAX as usize);
        let n = self.bz_read(&mut out[..len]);
        if self.last_err == BZ_OK || self.last_err == BZ_STREAM_END {
            Some(n)
        } else {
            None
        }
    }

    fn write_out(&mut self, n: usize) -> bool {
        let mut pos = 0;
        while pos < n {
            match self.handle.write(&self.buf[pos..n]) {
                Ok(0) => {
                    self.ferror = true;
                    return false;
                }
                Ok(k) => pos += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.ferror = true;
                    return false;
                }
            }
        }
        true
    }

    /// `BZ2_bzWrite()`.
    fn bz_write(&mut self, data: &[u8]) {
        self.set_err(BZ_OK);
        if !self.writing {
            self.set_err(BZ_SEQUENCE_ERROR);
            return;
        }
        if self.ferror {
            self.set_err(BZ_IO_ERROR);
            return;
        }
        if data.is_empty() {
            return;
        }
        let mut pos = 0;
        loop {
            let mut out = std::mem::take(&mut self.buf);
            let step = self.strm.compress(&data[pos..], &mut out, BZ_RUN);
            self.buf = out;
            pos += step.consumed;
            if step.status != BZ_RUN_OK {
                self.set_err(step.status);
                return;
            }
            if step.produced > 0 && !self.write_out(step.produced) {
                self.set_err(BZ_IO_ERROR);
                return;
            }
            if pos == data.len() {
                self.set_err(BZ_OK);
                return;
            }
        }
    }

    /// `BZ2_bzwrite()`: `Some(len)` on success, `None` on error (-1).
    pub fn write(&mut self, data: &[u8]) -> Option<usize> {
        self.bz_write(data);
        (self.last_err == BZ_OK).then_some(data.len())
    }

    /// `BZ2_bzflush()`: a no-op returning 0.
    pub fn flush(&mut self) -> i32 {
        0
    }

    /// `BZ2_bzWriteClose(abandon = 0)`: finishes the stream.
    fn write_close(&mut self) -> i32 {
        if !self.writing {
            return self.set_err(BZ_SEQUENCE_ERROR);
        }
        if self.ferror {
            return self.set_err(BZ_IO_ERROR);
        }
        if self.last_err == BZ_OK {
            loop {
                let mut out = std::mem::take(&mut self.buf);
                let step = self.strm.compress(&[], &mut out, BZ_FINISH);
                self.buf = out;
                if step.status != BZ_FINISH_OK && step.status != BZ_STREAM_END {
                    return self.set_err(step.status);
                }
                if step.produced > 0 && !self.write_out(step.produced) {
                    return self.set_err(BZ_IO_ERROR);
                }
                if step.status == BZ_STREAM_END {
                    break;
                }
            }
        }
        if self.handle.flush().is_err() {
            self.ferror = true;
            return self.set_err(BZ_IO_ERROR);
        }
        self.set_err(BZ_OK)
    }

    /// `BZ2_bzclose()`: finishes a written stream and returns the handle.
    pub fn close(mut self) -> F {
        if self.writing {
            self.write_close();
        }
        self.strm.end();
        self.handle
    }
}

/// Reports `BZ_DATA_ERROR` for callers that need the constant.
pub const DATA_ERROR: i32 = BZ_DATA_ERROR;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn compress(data: &[u8], mode: &str, chunks: usize) -> Vec<u8> {
        let mut f = BzFile::open(Cursor::new(Vec::new()), mode).unwrap();
        for c in data.chunks(chunks) {
            assert_eq!(f.write(c), Some(c.len()));
        }
        f.close().into_inner()
    }

    #[test]
    fn write_read_roundtrip() {
        let text = b"Rising up from the heart of the desert\n".repeat(500);
        let packed = compress(&text, "w", 777);
        assert!(packed.starts_with(b"BZh9"));
        // Default work factor 30 and block size 9 match bzcompress(…, 9, 30).
        assert_eq!(
            crate::bzcompress(&text, 9, 30).unwrap(),
            crate::Output::Data(packed.clone())
        );
        let mut f = BzFile::open(Cursor::new(packed), "r").unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1000];
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, text);
        assert_eq!(f.error(), (0, "OK"));
        assert_eq!(f.last_error(), BZ_STREAM_END);
        assert_eq!(f.read(&mut buf), Some(0));
    }

    #[test]
    fn small_reads() {
        let text = b"Rising up from the heart of the desert".to_vec();
        let packed = compress(&text, "w", 5);
        let mut f = BzFile::open(Cursor::new(packed), "rs").unwrap();
        let mut b = [0u8; 0];
        assert_eq!(f.read(&mut b), Some(0));
        let mut one = [0u8; 1];
        assert_eq!(f.read(&mut one), Some(1));
        assert_eq!(&one, b"R");
        let mut two = [0u8; 2];
        assert_eq!(f.read(&mut two), Some(2));
        assert_eq!(&two, b"is");
        let mut rest = vec![0u8; 100_000];
        let n = f.read(&mut rest).unwrap();
        assert_eq!(&rest[..n], &text[3..]);
    }

    #[test]
    fn errors() {
        let mut f = BzFile::open(Cursor::new(b"not bzip2 data at all".to_vec()), "r").unwrap();
        assert_eq!(f.error(), (0, "OK"));
        assert_eq!(f.read(&mut [0u8; 10]), None);
        assert_eq!(f.error(), (-5, "DATA_ERROR_MAGIC"));
        let mut packed = compress(b"hello world, hello world", "w", 100);
        let n = packed.len();
        packed[n - 8] ^= 0x5a;
        packed[n - 12] ^= 0x5a;
        let mut f = BzFile::open(Cursor::new(packed), "r").unwrap();
        assert_eq!(f.read(&mut [0u8; 100]), None);
        assert_eq!(f.error().1, "DATA_ERROR");
        // Truncated stream.
        let packed = compress(b"hello world, hello world", "w", 100);
        let mut f = BzFile::open(Cursor::new(packed[..20].to_vec()), "r").unwrap();
        assert_eq!(f.read(&mut [0u8; 100]), None);
        assert_eq!(f.error(), (-7, "UNEXPECTED_EOF"));
        // Wrong direction.
        let mut w = BzFile::open(Cursor::new(Vec::new()), "w").unwrap();
        assert_eq!(w.read(&mut [0u8; 4]), None);
        assert_eq!(w.error(), (-1, "SEQUENCE_ERROR"));
        let mut r = BzFile::open(Cursor::new(Vec::new()), "r").unwrap();
        assert_eq!(r.write(b"x"), None);
        assert!(BzFile::open(Cursor::new(Vec::new()), "w0").is_none());
        assert_eq!(error_string(-20), "???");
        assert_eq!(error_string(4), "OK");
    }

    #[test]
    fn mode_parse() {
        assert_eq!(
            OpenMode::parse("wb5"),
            OpenMode {
                writing: true,
                small: false,
                block_size_100k: 5
            }
        );
        assert!(OpenMode::parse("rs").small);
        assert!(!OpenMode::parse("").writing);
    }
}
