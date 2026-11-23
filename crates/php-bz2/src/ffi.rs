// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The only module that talks to libbz2 (through `bzip2-sys`).
//!
//! `libbz2-rs-sys` is the Rust port of bzip2 1.0.8 (a c2rust translation),
//! so compressed output is byte-identical to PHP's libbz2. A `bz_stream`
//! keeps a back pointer checked on every call, so it is boxed; buffers are
//! passed as slices per call and the raw pointers never outlive the call.
//!
//! Where libbz2's own state lives follows php-src (see [`Heap`]).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::ptr;

use libbz2_rs_sys as bz;

/// What one `BZ2_bzCompress()`/`BZ2_bzDecompress()` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The libbz2 return code.
    pub status: i32,
    /// Input bytes consumed.
    pub consumed: usize,
    /// Output bytes produced.
    pub produced: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Compress,
    Decompress,
}

/// Which allocator libbz2 uses for its state (up to ~7.6 MB at block size 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heap {
    /// The request's counted heap: the `bzip2.*` stream filters, which
    /// php-src sets up with `php_bz2_alloc` (emalloc, under `memory_limit`).
    Request,
    /// Plain malloc, outside `memory_limit`: `bzcompress()`, `bzdecompress()`
    /// and BZFILE streams, which php-src leaves on libbz2's default
    /// allocator (`bzalloc = NULL`, ext/bz2/bz2.c).
    System,
}

/// libbz2's default `bzalloc` (`malloc(items * size)`), for [`Heap::System`].
unsafe extern "C" fn system_alloc(_opaque: *mut c_void, items: c_int, size: c_int) -> *mut c_void {
    let (Ok(items), Ok(size)) = (usize::try_from(items), usize::try_from(size)) else {
        return ptr::null_mut();
    };
    let Some(bytes) = items.checked_mul(size) else {
        return ptr::null_mut();
    };
    // SAFETY: malloc has no preconditions; NULL becomes BZ_MEM_ERROR.
    unsafe { libc::malloc(bytes) }
}

/// libbz2's default `bzfree`, for [`Heap::System`].
unsafe extern "C" fn system_free(_opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: libbz2 only frees pointers it got from `system_alloc`.
    unsafe { libc::free(address) }
}

/// A compression or decompression stream.
pub struct RawStream {
    strm: Box<bz::bz_stream>,
    kind: Kind,
    live: bool,
}

impl std::fmt::Debug for RawStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawStream")
            .field("kind", &self.kind)
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

// The stream must live at a stable address (libbz2 keeps a back pointer).
#[allow(clippy::unnecessary_box_returns)]
fn fresh(heap: Heap) -> Box<bz::bz_stream> {
    // NULL callbacks select libbz2-rs-sys's Rust allocator, which the
    // request's memory counter sees.
    let (bzalloc, bzfree) = match heap {
        Heap::Request => (None, None),
        Heap::System => (
            Some(system_alloc as unsafe extern "C" fn(_, _, _) -> _),
            Some(system_free as unsafe extern "C" fn(_, _)),
        ),
    };
    Box::new(bz::bz_stream {
        next_in: ptr::null_mut(),
        avail_in: 0,
        total_in_lo32: 0,
        total_in_hi32: 0,
        next_out: ptr::null_mut(),
        avail_out: 0,
        total_out_lo32: 0,
        total_out_hi32: 0,
        state: ptr::null_mut(),
        bzalloc,
        bzfree,
        opaque: ptr::null_mut(),
    })
}

fn clamp(len: usize) -> c_uint {
    c_uint::try_from(len).unwrap_or(c_uint::MAX)
}

impl RawStream {
    /// `BZ2_bzCompressInit(block_size_100k, 0, work_factor)`.
    pub fn compress_init(block_size_100k: i32, work_factor: i32, heap: Heap) -> Result<Self, i32> {
        let mut strm = fresh(heap);
        // SAFETY: `strm` is a zeroed, boxed bz_stream whose allocator
        // callbacks are NULL or a matching malloc/free pair; libbz2
        // validates the numeric parameters.
        let ret =
            unsafe { bz::BZ2_bzCompressInit(&raw mut *strm, block_size_100k, 0, work_factor) };
        if ret == bz::BZ_OK {
            Ok(RawStream {
                strm,
                kind: Kind::Compress,
                live: true,
            })
        } else {
            Err(ret)
        }
    }

    /// `BZ2_bzDecompressInit(0, small)`.
    pub fn decompress_init(small: bool, heap: Heap) -> Result<Self, i32> {
        let mut strm = fresh(heap);
        // SAFETY: as for compress_init.
        let ret = unsafe { bz::BZ2_bzDecompressInit(&raw mut *strm, 0, c_int::from(small)) };
        if ret == bz::BZ_OK {
            Ok(RawStream {
                strm,
                kind: Kind::Decompress,
                live: true,
            })
        } else {
            Err(ret)
        }
    }

    fn run(&mut self, input: &[u8], output: &mut [u8], action: i32) -> Step {
        if !self.live {
            return Step {
                status: bz::BZ_SEQUENCE_ERROR,
                consumed: 0,
                produced: 0,
            };
        }
        let avail_in = clamp(input.len());
        let avail_out = clamp(output.len());
        self.strm.next_in = input.as_ptr().cast::<c_char>().cast_mut();
        self.strm.avail_in = avail_in;
        self.strm.next_out = output.as_mut_ptr().cast::<c_char>();
        self.strm.avail_out = avail_out;
        // SAFETY: the stream is initialised; next_in/next_out are valid for
        // avail_in/avail_out bytes during the call and libbz2 never writes
        // through next_in.
        let status = unsafe {
            match self.kind {
                Kind::Compress => bz::BZ2_bzCompress(&raw mut *self.strm, action),
                Kind::Decompress => bz::BZ2_bzDecompress(&raw mut *self.strm),
            }
        };
        let consumed = (avail_in - self.strm.avail_in) as usize;
        let produced = (avail_out - self.strm.avail_out) as usize;
        self.strm.next_in = ptr::null_mut();
        self.strm.avail_in = 0;
        self.strm.next_out = ptr::null_mut();
        self.strm.avail_out = 0;
        Step {
            status,
            consumed,
            produced,
        }
    }

    /// `BZ2_bzCompress(action)` with `BZ_RUN`, `BZ_FLUSH` or `BZ_FINISH`.
    pub fn compress(&mut self, input: &[u8], output: &mut [u8], action: i32) -> Step {
        debug_assert_eq!(self.kind, Kind::Compress);
        self.run(input, output, action)
    }

    /// `BZ2_bzDecompress()`.
    pub fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Step {
        debug_assert_eq!(self.kind, Kind::Decompress);
        self.run(input, output, 0)
    }

    /// `total_out` (64-bit).
    #[cfg(test)]
    pub fn total_out(&self) -> u64 {
        (u64::from(self.strm.total_out_hi32) << 32) | u64::from(self.strm.total_out_lo32)
    }

    /// `BZ2_bzCompressEnd()` / `BZ2_bzDecompressEnd()`; idempotent.
    pub fn end(&mut self) {
        if !self.live {
            return;
        }
        self.live = false;
        // SAFETY: the stream is initialised and is not used again.
        unsafe {
            match self.kind {
                Kind::Compress => bz::BZ2_bzCompressEnd(&raw mut *self.strm),
                Kind::Decompress => bz::BZ2_bzDecompressEnd(&raw mut *self.strm),
            };
        }
    }
}

impl Drop for RawStream {
    fn drop(&mut self) {
        self.end();
    }
}

/// `dup(fd)` wrapped in a [`std::fs::File`] owning the duplicate (PHP's
/// `php_bz2_bzdopen()`). `None` when `dup()` fails.
pub fn dup_fd(fd: i32) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: dup() has no memory-safety preconditions; an invalid fd
    // yields -1.
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return None;
    }
    // SAFETY: `d` is a fresh descriptor owned by nobody else; the File
    // takes ownership and closes it on drop.
    Some(unsafe { std::fs::File::from_raw_fd(d) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = RawStream::compress_init(4, 0, Heap::System).unwrap();
        let mut out = vec![0u8; 700];
        let st = c.compress(b"hello hello hello", &mut out, bz::BZ_FINISH);
        assert_eq!(st.status, bz::BZ_STREAM_END);
        assert_eq!(&out[..4], b"BZh4");
        let mut d = RawStream::decompress_init(false, Heap::Request).unwrap();
        let mut back = vec![0u8; 64];
        let st2 = d.decompress(&out[..st.produced], &mut back);
        assert_eq!(st2.status, bz::BZ_STREAM_END);
        assert_eq!(&back[..st2.produced], b"hello hello hello");
        assert_eq!(d.total_out(), 17);
    }

    #[test]
    fn bad_params() {
        assert_eq!(
            RawStream::compress_init(10, 0, Heap::Request).unwrap_err(),
            bz::BZ_PARAM_ERROR
        );
        let mut d = RawStream::decompress_init(true, Heap::System).unwrap();
        let mut out = [0u8; 8];
        assert_eq!(
            d.decompress(b"xyz", &mut out).status,
            bz::BZ_DATA_ERROR_MAGIC
        );
    }
}
