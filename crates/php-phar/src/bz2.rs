// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Minimal libbz2 binding (the `bzip2.compress` / `bzip2.decompress` stream
//! filters with their default parameters, as phar uses them).
//!
//! This is the only module (together with [`crate::sys`]) containing
//! `unsafe`: it drives a `bz_stream` over caller-owned buffers. The stream is
//! boxed so its address is stable for libbz2's internal back pointer.

use std::ffi::{c_char, c_int, c_uint};

use libbz2_rs_sys as bz;

struct Stream {
    strm: Box<bz::bz_stream>,
    compress: bool,
}

impl Stream {
    // Boxed so the stream keeps a stable address (libbz2 stores a back
    // pointer in its state).
    #[allow(clippy::unnecessary_box_returns)]
    fn new_stream() -> Box<bz::bz_stream> {
        Box::new(bz::bz_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in_lo32: 0,
            total_in_hi32: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out_lo32: 0,
            total_out_hi32: 0,
            state: std::ptr::null_mut(),
            bzalloc: None,
            bzfree: None,
            opaque: std::ptr::null_mut(),
        })
    }

    fn compressor(block_size: c_int, work_factor: c_int) -> Option<Self> {
        let mut strm = Self::new_stream();
        // SAFETY: `strm` is a valid, zeroed bz_stream with NULL allocators
        // (libbz2 then uses malloc/free); it stays boxed until `Drop`.
        let rc = unsafe { bz::BZ2_bzCompressInit(&raw mut *strm, block_size, 0, work_factor) };
        (rc == bz::BZ_OK).then_some(Stream {
            strm,
            compress: true,
        })
    }

    fn decompressor() -> Option<Self> {
        let mut strm = Self::new_stream();
        // SAFETY: as in `compressor`.
        let rc = unsafe { bz::BZ2_bzDecompressInit(&raw mut *strm, 0, 0) };
        (rc == bz::BZ_OK).then_some(Stream {
            strm,
            compress: false,
        })
    }

    /// One `BZ2_bzCompress`/`BZ2_bzDecompress` call over the slices; returns
    /// (status, consumed, produced).
    fn step(&mut self, input: &[u8], out: &mut [u8], action: c_int) -> (c_int, usize, usize) {
        let in_len = c_uint::try_from(input.len()).unwrap_or(c_uint::MAX);
        let out_len = c_uint::try_from(out.len()).unwrap_or(c_uint::MAX);
        self.strm.next_in = input.as_ptr().cast::<c_char>().cast_mut();
        self.strm.avail_in = in_len;
        self.strm.next_out = out.as_mut_ptr().cast::<c_char>();
        self.strm.avail_out = out_len;
        // SAFETY: next_in/next_out point into live slices of the advertised
        // lengths for the duration of the call; libbz2 never writes through
        // next_in. The pointers are cleared again right after the call.
        let rc = unsafe {
            if self.compress {
                bz::BZ2_bzCompress(&raw mut *self.strm, action)
            } else {
                bz::BZ2_bzDecompress(&raw mut *self.strm)
            }
        };
        let consumed = (in_len - self.strm.avail_in) as usize;
        let produced = (out_len - self.strm.avail_out) as usize;
        self.strm.next_in = std::ptr::null_mut();
        self.strm.next_out = std::ptr::null_mut();
        (rc, consumed, produced)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream was successfully initialised in the matching
        // mode (constructors only return initialised streams).
        unsafe {
            if self.compress {
                bz::BZ2_bzCompressEnd(&raw mut *self.strm);
            } else {
                bz::BZ2_bzDecompressEnd(&raw mut *self.strm);
            }
        }
    }
}

/// Compresses like the `bzip2.compress` filter with default parameters
/// (block size 9, work factor 0).
pub fn compress(data: &[u8]) -> Option<Vec<u8>> {
    let mut s = Stream::compressor(9, 0)?;
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    let mut buf = vec![0u8; 8192];
    let mut pos = 0usize;
    loop {
        let (rc, used, made) = s.step(&data[pos..], &mut buf, bz::BZ_FINISH);
        pos += used;
        out.extend_from_slice(&buf[..made]);
        match rc {
            bz::BZ_STREAM_END => return Some(out),
            bz::BZ_FINISH_OK | bz::BZ_RUN_OK | bz::BZ_FLUSH_OK => {}
            _ => return None,
        }
    }
}

/// Decompresses like the `bzip2.decompress` filter. Returns the output
/// produced and whether the stream ended cleanly.
pub fn decompress(data: &[u8]) -> (Vec<u8>, bool) {
    let Some(mut s) = Stream::decompressor() else {
        return (Vec::new(), false);
    };
    let mut out = Vec::with_capacity(data.len() * 3);
    let mut buf = vec![0u8; 8192];
    let mut pos = 0usize;
    loop {
        let (rc, used, made) = s.step(&data[pos..], &mut buf, bz::BZ_RUN);
        pos += used;
        out.extend_from_slice(&buf[..made]);
        match rc {
            bz::BZ_STREAM_END => return (out, true),
            bz::BZ_OK if used > 0 || made > 0 => {}
            _ => return (out, false),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let data = b"hello hello hello hello bzip2".repeat(50);
        let c = super::compress(&data).expect("compress");
        assert!(c.starts_with(b"BZh9"));
        let (d, ok) = super::decompress(&c);
        assert!(ok);
        assert_eq!(d, data);
        let (_, ok) = super::decompress(b"BZh9garbage");
        assert!(!ok);
    }
}
