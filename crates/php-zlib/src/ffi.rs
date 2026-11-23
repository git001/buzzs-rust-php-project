// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The only module that talks to the C zlib library.
//!
//! Everything here wraps `libz-sys` (which links the system `libz`, the same
//! library PHP is built against, so compressed output is byte-identical).
//! The rest of the crate only uses the safe [`ZStream`] type.
//!
//! A `z_stream` must not move once initialised (zlib stores a back pointer
//! to it in its internal state and verifies it on every call), so it is kept
//! in a `Box`. Input and output buffers are passed as slices on every call;
//! the raw pointers stored in the `z_stream` never outlive that call.

use std::alloc::{self, Layout};
use std::ffi::{c_int, c_uint, CStr};
use std::ptr;

use libz_sys as z;

/// What a single `deflate()`/`inflate()` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The zlib return code (`Z_OK`, `Z_STREAM_END`, …).
    pub status: i32,
    /// Bytes of input consumed.
    pub consumed: usize,
    /// Bytes of output produced.
    pub produced: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Deflate,
    Inflate,
}

/// Which allocator zlib uses for its state (about 268 KiB per deflate
/// stream at the default memLevel 8, 7 KiB + window per inflate stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heap {
    /// The request's counted heap, as php-src's `php_zlib_alloc`
    /// (`safe_emalloc`, under `memory_limit`): every ext/zlib path except
    /// gzFile streams.
    Request,
    /// Plain malloc, outside `memory_limit`: gzFile streams, which php-src
    /// opens through zlib's gzlib (`gzdopen`, libz's own malloc).
    System,
}

/// Bytes in front of every [`Heap::Request`] block holding its size (zlib's
/// free callback gets no size); 16 keeps malloc's alignment.
const HEADER: usize = 16;

/// `php_zlib_alloc()`: zlib's state on the Rust global allocator, which the
/// request's memory counter sees. NULL (`Z_MEM_ERROR`) on overflow or when the
/// allocator refuses.
extern "C" fn request_alloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    let Some(bytes) = (items as usize)
        .checked_mul(size as usize)
        .and_then(|b| b.checked_add(HEADER))
    else {
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(bytes, HEADER) else {
        return ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size (at least HEADER).
    let base = unsafe { alloc::alloc(layout) };
    if base.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `base` is valid for `bytes >= HEADER` bytes and 16-aligned, so
    // the size fits in front and the payload starts HEADER bytes in.
    unsafe {
        base.cast::<usize>().write_unaligned(bytes);
        base.add(HEADER).cast()
    }
}

/// `php_zlib_free()` for [`request_alloc`].
extern "C" fn request_free(_opaque: z::voidpf, address: z::voidpf) {
    if address.is_null() {
        return;
    }
    // SAFETY: zlib only frees pointers from `request_alloc`, which sit
    // HEADER bytes behind a block whose first word is its total size; that
    // size and alignment rebuild the layout it was allocated with.
    unsafe {
        let base = address.cast::<u8>().sub(HEADER);
        let bytes = base.cast::<usize>().read_unaligned();
        alloc::dealloc(base, Layout::from_size_align_unchecked(bytes, HEADER));
    }
}

/// zlib's own `zcalloc()`, for [`Heap::System`].
extern "C" fn system_alloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    // SAFETY: calloc has no preconditions; it returns NULL on failure (or on
    // overflow of items * size), which zlib handles as Z_MEM_ERROR.
    unsafe { libc::calloc(items as usize, size as usize) }
}

/// zlib's own `zcfree()`, for [`Heap::System`].
extern "C" fn system_free(_opaque: z::voidpf, address: z::voidpf) {
    // SAFETY: zlib only passes pointers obtained from `system_alloc`.
    unsafe { libc::free(address) }
}

/// A deflate or inflate stream (`z_stream` plus its init/end bookkeeping).
pub struct ZStream {
    strm: Box<z::z_stream>,
    kind: Kind,
    heap: Heap,
    live: bool,
}

impl std::fmt::Debug for ZStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZStream")
            .field("kind", &self.kind)
            .field("live", &self.live)
            .field("total_in", &self.strm.total_in)
            .field("total_out", &self.strm.total_out)
            .finish_non_exhaustive()
    }
}

// The stream must live at a stable address (zlib keeps a back pointer).
#[allow(clippy::unnecessary_box_returns)]
fn fresh_stream(heap: Heap) -> Box<z::z_stream> {
    let (zalloc, zfree): (z::alloc_func, z::free_func) = match heap {
        Heap::Request => (request_alloc, request_free),
        Heap::System => (system_alloc, system_free),
    };
    Box::new(z::z_stream {
        next_in: ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: ptr::null_mut(),
        state: ptr::null_mut(),
        zalloc,
        zfree,
        opaque: ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    })
}

fn stream_size() -> c_int {
    c_int::try_from(std::mem::size_of::<z::z_stream>()).unwrap_or(c_int::MAX)
}

fn clamp_len(len: usize) -> c_uint {
    c_uint::try_from(len).unwrap_or(c_uint::MAX)
}

impl ZStream {
    /// `deflateInit2()`. Returns the zlib error code on failure.
    pub fn deflate_init(
        level: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: i32,
        heap: Heap,
    ) -> Result<Self, i32> {
        let mut strm = fresh_stream(heap);
        // SAFETY: `strm` is a valid, boxed (address-stable) z_stream with
        // allocator callbacks set; zlibVersion() returns a static string.
        let ret = unsafe {
            z::deflateInit2_(
                &raw mut *strm,
                level,
                z::Z_DEFLATED,
                window_bits,
                mem_level,
                strategy,
                z::zlibVersion(),
                stream_size(),
            )
        };
        if ret == z::Z_OK {
            Ok(ZStream {
                strm,
                kind: Kind::Deflate,
                heap,
                live: true,
            })
        } else {
            Err(ret)
        }
    }

    /// `inflateInit2()`. Returns the zlib error code on failure.
    pub fn inflate_init(window_bits: i32, heap: Heap) -> Result<Self, i32> {
        let mut strm = fresh_stream(heap);
        // SAFETY: as for deflate_init.
        let ret = unsafe {
            z::inflateInit2_(&raw mut *strm, window_bits, z::zlibVersion(), stream_size())
        };
        if ret == z::Z_OK {
            Ok(ZStream {
                strm,
                kind: Kind::Inflate,
                heap,
                live: true,
            })
        } else {
            Err(ret)
        }
    }

    /// Runs `deflate()` or `inflate()` once on the given buffers.
    fn run(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Step {
        if !self.live {
            return Step {
                status: z::Z_STREAM_ERROR,
                consumed: 0,
                produced: 0,
            };
        }
        let avail_in = clamp_len(input.len());
        let avail_out = clamp_len(output.len());
        self.strm.next_in = input.as_ptr().cast_mut();
        self.strm.avail_in = avail_in;
        self.strm.next_out = output.as_mut_ptr();
        self.strm.avail_out = avail_out;
        // SAFETY: the stream is initialised; next_in/next_out point to
        // buffers valid for avail_in/avail_out bytes for the whole call, and
        // zlib never writes through next_in.
        let status = unsafe {
            match self.kind {
                Kind::Deflate => z::deflate(&raw mut *self.strm, flush),
                Kind::Inflate => z::inflate(&raw mut *self.strm, flush),
            }
        };
        let consumed = (avail_in - self.strm.avail_in) as usize;
        let produced = (avail_out - self.strm.avail_out) as usize;
        // Do not keep dangling pointers around.
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

    /// One `deflate()` call.
    pub fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Step {
        debug_assert_eq!(self.kind, Kind::Deflate);
        self.run(input, output, flush)
    }

    /// One `inflate()` call.
    pub fn inflate(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Step {
        debug_assert_eq!(self.kind, Kind::Inflate);
        self.run(input, output, flush)
    }

    /// `deflateReset()` / `inflateReset()`.
    pub fn reset(&mut self) -> i32 {
        if !self.live {
            return z::Z_STREAM_ERROR;
        }
        // SAFETY: the stream is initialised.
        unsafe {
            match self.kind {
                Kind::Deflate => z::deflateReset(&raw mut *self.strm),
                Kind::Inflate => z::inflateReset(&raw mut *self.strm),
            }
        }
    }

    /// `deflateParams()`.
    pub fn deflate_params(&mut self, level: i32, strategy: i32) -> i32 {
        if !self.live || self.kind != Kind::Deflate {
            return z::Z_STREAM_ERROR;
        }
        // deflateParams() may emit pending output; give it no buffer, as
        // gzsetparams() only calls it after flushing with Z_BLOCK.
        let mut scratch = [0u8; 0];
        self.strm.next_out = scratch.as_mut_ptr();
        self.strm.avail_out = 0;
        // SAFETY: the stream is initialised; next_out is valid for 0 bytes.
        let ret = unsafe { z::deflateParams(&raw mut *self.strm, level, strategy) };
        self.strm.next_out = ptr::null_mut();
        ret
    }

    /// `deflateSetDictionary()` / `inflateSetDictionary()`.
    pub fn set_dictionary(&mut self, dict: &[u8]) -> i32 {
        if !self.live {
            return z::Z_STREAM_ERROR;
        }
        let len = clamp_len(dict.len());
        // SAFETY: the stream is initialised and `dict` is valid for `len`
        // bytes; zlib copies the dictionary.
        unsafe {
            match self.kind {
                Kind::Deflate => z::deflateSetDictionary(&raw mut *self.strm, dict.as_ptr(), len),
                Kind::Inflate => z::inflateSetDictionary(&raw mut *self.strm, dict.as_ptr(), len),
            }
        }
    }

    /// `deflateEnd()` / `inflateEnd()`; idempotent.
    pub fn end(&mut self) -> i32 {
        if !self.live {
            return z::Z_STREAM_ERROR;
        }
        self.live = false;
        // SAFETY: the stream is initialised and is not used again until it
        // is re-initialised.
        unsafe {
            match self.kind {
                Kind::Deflate => z::deflateEnd(&raw mut *self.strm),
                Kind::Inflate => z::inflateEnd(&raw mut *self.strm),
            }
        }
    }

    /// Re-runs `inflateInit2()` on an ended inflate stream.
    pub fn inflate_reinit(&mut self, window_bits: i32) -> i32 {
        if self.live {
            self.end();
        }
        let mut strm = fresh_stream(self.heap);
        // SAFETY: as for inflate_init.
        let ret = unsafe {
            z::inflateInit2_(&raw mut *strm, window_bits, z::zlibVersion(), stream_size())
        };
        if ret == z::Z_OK {
            self.strm = strm;
            self.kind = Kind::Inflate;
            self.live = true;
        }
        ret
    }

    /// `total_in`.
    pub fn total_in(&self) -> u64 {
        self.strm.total_in
    }

    /// `strm->msg`, if zlib set one.
    pub fn msg(&self) -> Option<String> {
        if self.strm.msg.is_null() {
            return None;
        }
        // SAFETY: zlib sets msg to NULL or to a static NUL-terminated string.
        let s = unsafe { CStr::from_ptr(self.strm.msg) };
        Some(s.to_string_lossy().into_owned())
    }
}

impl Drop for ZStream {
    fn drop(&mut self) {
        if self.live {
            self.end();
        }
    }
}

/// `zlibVersion()`: the version of the linked library.
pub fn zlib_version() -> &'static str {
    // SAFETY: zlibVersion() returns a pointer to a static NUL-terminated
    // string.
    let s = unsafe { CStr::from_ptr(z::zlibVersion()) };
    s.to_str().unwrap_or("")
}

/// `crc32()` over `data`, continuing from `crc`.
#[cfg(test)]
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut crc = z::uLong::from(crc);
    for chunk in data.chunks(c_uint::MAX as usize) {
        // SAFETY: `chunk` is valid for its length.
        crc = unsafe { z::crc32(crc, chunk.as_ptr(), clamp_len(chunk.len())) };
    }
    u32::try_from(crc).unwrap_or(0)
}

/// `dup(fd)` wrapped in a [`std::fs::File`] that owns the duplicate (zlib's
/// `gzdopen(dup(fd), mode)`). Returns `None` when `dup()` fails.
pub fn dup_fd(fd: i32) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: dup() has no memory-safety preconditions; an invalid fd
    // yields -1.
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return None;
    }
    // SAFETY: `d` is a freshly duplicated descriptor owned by nobody else;
    // the File takes ownership and closes it on drop.
    Some(unsafe { std::fs::File::from_raw_fd(d) })
}

/// `strerror(errno)`.
pub fn strerror(errno: i32) -> String {
    // SAFETY: strerror returns a pointer to a NUL-terminated string that
    // stays valid until the next strerror call on this thread; it is copied
    // immediately.
    let p = unsafe { libc::strerror(errno) };
    if p.is_null() {
        return format!("Unknown error {errno}");
    }
    // SAFETY: non-null pointer to a NUL-terminated string (see above).
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_raw() {
        let mut d = ZStream::deflate_init(-1, -15, 9, 0, Heap::Request).unwrap();
        let mut out = vec![0u8; 64];
        let st = d.deflate(b"hello", &mut out, z::Z_FINISH);
        assert_eq!(st.status, z::Z_STREAM_END);
        assert_eq!(
            &out[..st.produced],
            &[0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00]
        );
        let mut i = ZStream::inflate_init(-15, Heap::Request).unwrap();
        let mut back = vec![0u8; 16];
        let st2 = i.inflate(&out[..st.produced], &mut back, z::Z_NO_FLUSH);
        assert_eq!(st2.status, z::Z_STREAM_END);
        assert_eq!(&back[..st2.produced], b"hello");
        assert_eq!(i.total_in(), st.produced as u64);
    }

    #[test]
    fn version_and_crc() {
        assert!(zlib_version().starts_with('1'));
        assert_eq!(crc32(0, b"hello"), 0x3610_a686);
        assert_eq!(strerror(13), "Permission denied");
    }

    #[test]
    fn bad_data_sets_msg() {
        let mut i = ZStream::inflate_init(15, Heap::System).unwrap();
        let mut out = [0u8; 8];
        let st = i.inflate(b"abc", &mut out, z::Z_NO_FLUSH);
        assert_eq!(st.status, z::Z_DATA_ERROR);
        assert_eq!(i.msg().as_deref(), Some("incorrect header check"));
    }
}
