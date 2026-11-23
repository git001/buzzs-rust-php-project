// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A tiny model of the `php_stream` operations the image parsers use.
//!
//! PHP's plain-file and memory streams both allow seeking past the end of the
//! data (reads there simply return nothing) and refuse to seek to a negative
//! position; reads return fewer bytes only at end of data.  [`PhpStream`]
//! reproduces exactly those semantics on top of any [`Read`] + [`Seek`] value
//! and keeps track of the logical position itself, so the underlying reader
//! can be buffered.

use std::io::{BufReader, Read, Seek, SeekFrom};

/// A seekable byte stream with `php_stream` semantics.
#[derive(Debug)]
pub struct PhpStream<R: Read + Seek> {
    inner: BufReader<R>,
    pos: u64,
    len: Option<u64>,
    eof_flag: bool,
    did_read: bool,
}

impl<R: Read + Seek> PhpStream<R> {
    /// Wraps `inner`; the logical position starts at the reader's current
    /// position.
    pub fn new(mut inner: R) -> Self {
        let pos = inner.stream_position().unwrap_or(0);
        Self {
            inner: BufReader::with_capacity(8192, inner),
            pos,
            len: None,
            eof_flag: false,
            did_read: false,
        }
    }

    /// Returns the wrapped reader, positioned at the logical position.
    pub fn into_inner(mut self) -> R {
        let _ = self.inner.seek(SeekFrom::Start(self.pos));
        self.inner.into_inner()
    }

    /// `php_stream_tell()`.
    pub fn tell(&self) -> u64 {
        self.pos
    }

    /// The total length of the data (`SEEK_END` + `tell`).
    pub fn len(&mut self) -> u64 {
        if let Some(len) = self.len {
            return len;
        }
        let len = self.inner.seek(SeekFrom::End(0)).unwrap_or(0);
        let _ = self.inner.seek(SeekFrom::Start(self.pos));
        self.len = Some(len);
        len
    }

    /// Whether the stream is empty.
    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    /// `php_stream_seek(stream, pos, SEEK_SET)`; `false` on failure.
    pub fn seek_set(&mut self, pos: i128) -> bool {
        let Ok(pos) = u64::try_from(pos) else {
            return false;
        };
        if self.inner.seek(SeekFrom::Start(pos)).is_err() {
            return false;
        }
        self.pos = pos;
        self.eof_flag = false;
        true
    }

    /// `php_stream_seek(stream, off, SEEK_CUR)`; `false` on failure.
    pub fn seek_cur(&mut self, off: i128) -> bool {
        self.seek_set(i128::from(self.pos) + off)
    }

    /// `php_stream_rewind()`; `true` on success.
    pub fn rewind(&mut self) -> bool {
        self.seek_set(0)
    }

    /// `php_stream_read()` looped until `buf` is full or the data ends.
    /// Returns the number of bytes read.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut total = 0;
        while total < buf.len() {
            match self.inner.read(&mut buf[total..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
        self.pos += total as u64;
        self.did_read = true;
        if total < buf.len() {
            self.eof_flag = true;
        }
        total
    }

    /// Reads up to `n` bytes into a new vector (shorter at end of data).
    pub fn read_vec(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        // Never allocate more than what can actually be there.
        let avail = self.len().saturating_sub(self.pos);
        let cap = usize::try_from(avail).unwrap_or(usize::MAX).min(n);
        // The file sizes the block: checked against `memory_limit`.
        if php_memlimit::TryGrow::try_grow_exact(&mut out, cap).is_err() {
            self.eof_flag = true;
            return out;
        }
        out.resize(cap, 0);
        let got = self.read(&mut out);
        out.truncate(got);
        if got < n {
            self.eof_flag = true;
        }
        out
    }

    /// `php_stream_getc()`: one byte or `None` at end of data.
    pub fn getc(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        if self.read(&mut b) == 1 {
            Some(b[0])
        } else {
            None
        }
    }

    /// `php_stream_eof()`: a read has hit the end of the data.
    pub fn eof(&mut self) -> bool {
        if self.eof_flag {
            return true;
        }
        self.did_read && self.pos >= self.len()
    }

    /// Reads everything from the current position to the end.
    pub fn read_to_end(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // In blocks whose growth is checked against `memory_limit`: past
        // it the read stops with what it has (the violation is recorded).
        let mut block = [0u8; 8192];
        loop {
            match self.inner.read(&mut block) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if php_memlimit::TryGrow::try_grow(&mut out, n).is_err() {
                        break;
                    }
                    out.extend_from_slice(&block[..n]);
                }
            }
        }
        self.pos += out.len() as u64;
        self.did_read = true;
        self.eof_flag = true;
        out
    }

    /// `php_stream_gets(stream, NULL, 0)`: the next line including its
    /// terminating `"\n"` (or the rest of the data), `None` at end of data.
    pub fn gets(&mut self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        while let Some(c) = self.getc() {
            // A line as long as the file: its growth is checked against
            // `memory_limit`, past it the line ends there.
            if php_memlimit::TryGrow::try_grow(&mut line, 1).is_err() {
                break;
            }
            line.push(c);
            if c == b'\n' {
                break;
            }
        }
        if line.is_empty() {
            None
        } else {
            Some(line)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_and_seek_semantics() {
        let mut s = PhpStream::new(Cursor::new(b"abcdef".to_vec()));
        let mut buf = [0u8; 4];
        assert_eq!(s.read(&mut buf), 4);
        assert_eq!(&buf, b"abcd");
        assert!(!s.eof());
        assert_eq!(s.tell(), 4);
        assert!(s.seek_cur(10));
        assert_eq!(s.read(&mut buf), 0);
        assert!(s.eof());
        assert!(!s.seek_cur(-100));
        assert!(s.seek_set(5));
        assert_eq!(s.getc(), Some(b'f'));
        assert!(s.eof());
        assert_eq!(s.getc(), None);
        assert!(s.rewind());
        assert_eq!(s.gets(), Some(b"abcdef".to_vec()));
        assert_eq!(s.len(), 6);
    }

    #[test]
    fn gets_splits_lines() {
        let mut s = PhpStream::new(Cursor::new(b"a\nbc\n\nd".to_vec()));
        assert_eq!(s.gets(), Some(b"a\n".to_vec()));
        assert_eq!(s.gets(), Some(b"bc\n".to_vec()));
        assert_eq!(s.gets(), Some(b"\n".to_vec()));
        assert_eq!(s.gets(), Some(b"d".to_vec()));
        assert_eq!(s.gets(), None);
    }

    #[test]
    fn into_inner_restores_logical_position() {
        let mut s = PhpStream::new(Cursor::new(vec![0u8; 100_000]));
        let mut b = [0u8; 3];
        s.read(&mut b);
        let mut c = s.into_inner();
        assert_eq!(c.stream_position().unwrap(), 3);
    }
}
