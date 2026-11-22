// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! One-shot compression: `zlib_encode()`, `gzdeflate()`, `gzencode()`,
//! `gzcompress()` and their decoding counterparts (`php_zlib_encode()`,
//! `php_zlib_decode()`, `php_zlib_inflate_rounds()`).

use crate::constants::{
    z_error, MAX_MEM_LEVEL, ZLIB_ENCODING_ANY, ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP,
    ZLIB_ENCODING_RAW,
};
use crate::error::{arg_value_error, Error, Result};
use crate::ffi::ZStream;
use php_memlimit::TryGrow;

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_FINISH: i32 = 4;
const Z_NO_FLUSH: i32 = 0;
const Z_DATA_ERROR: i32 = -3;
const Z_MEM_ERROR: i32 = -4;
const Z_BUF_ERROR: i32 = -5;

/// `PHP_ZLIB_BUFFER_SIZE_GUESS()`: `in_len * 1.015 + 23`.
pub(crate) fn buffer_size_guess(in_len: usize) -> usize {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let scaled = (in_len as f64 * 1.015) as usize;
    scaled + 10 + 8 + 4 + 1
}

/// Whether `encoding` is one of the three userland encodings.
pub(crate) fn valid_encoding(encoding: i64) -> bool {
    matches!(
        encoding,
        ZLIB_ENCODING_RAW | ZLIB_ENCODING_GZIP | ZLIB_ENCODING_DEFLATE
    )
}

const ENCODING_MSG: &str =
    "must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE";

/// `php_zlib_encode()`: a single `deflate(Z_FINISH)` into a buffer of
/// [`buffer_size_guess`] bytes, with `MAX_MEM_LEVEL` and the default
/// strategy. `encoding` is the window-bits value (one of the
/// `ZLIB_ENCODING_*` constants); it and `level` must already be valid.
pub fn encode_raw(data: &[u8], encoding: i32, level: i32) -> Result<Vec<u8>> {
    let mut z = ZStream::deflate_init(level, encoding, MAX_MEM_LEVEL, 0)
        .map_err(|st| Error::Warning(z_error(st).into()))?;
    let mut out = vec![0u8; buffer_size_guess(data.len())];
    let step = z.deflate(data, &mut out, Z_FINISH);
    z.end();
    if step.status == Z_STREAM_END {
        out.truncate(step.produced);
        Ok(out)
    } else {
        Err(Error::Warning(z_error(step.status).into()))
    }
}

/// `PHP_ZLIB_ENCODE_FUNC` argument validation and encoding.
fn encode_func(
    func: &str,
    data: &[u8],
    level: i64,
    encoding: i64,
    level_arg: u32,
    encoding_arg: u32,
) -> Result<Vec<u8>> {
    if !(-1..=9).contains(&level) {
        return Err(arg_value_error(
            func,
            level_arg,
            "level",
            "must be between -1 and 9",
        ));
    }
    if !valid_encoding(encoding) {
        return Err(arg_value_error(
            func,
            encoding_arg,
            "encoding",
            ENCODING_MSG,
        ));
    }
    // Both values were range-checked above.
    let (enc, lvl) = (
        i32::try_from(encoding).unwrap_or_default(),
        i32::try_from(level).unwrap_or_default(),
    );
    encode_raw(data, enc, lvl)
}

/// `zlib_encode(string $data, int $encoding, int $level = -1)`.
pub fn zlib_encode(data: &[u8], encoding: i64, level: i64) -> Result<Vec<u8>> {
    encode_func("zlib_encode", data, level, encoding, 3, 2)
}

/// `gzdeflate(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_RAW)`.
pub fn gzdeflate(data: &[u8], level: i64, encoding: i64) -> Result<Vec<u8>> {
    encode_func("gzdeflate", data, level, encoding, 2, 3)
}

/// `gzencode(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_GZIP)`.
pub fn gzencode(data: &[u8], level: i64, encoding: i64) -> Result<Vec<u8>> {
    encode_func("gzencode", data, level, encoding, 2, 3)
}

/// `gzcompress(string $data, int $level = -1, int $encoding = ZLIB_ENCODING_DEFLATE)`.
pub fn gzcompress(data: &[u8], level: i64, encoding: i64) -> Result<Vec<u8>> {
    encode_func("gzcompress", data, level, encoding, 2, 3)
}

/// Resizes `buf` to `new_len` bytes (zero-filled) through a fallible
/// allocation, like PHP's `safe_ferealloc(..., fallible)` /
/// `zend_string_fextend(..., fallible)`: `false`, with `buf` unchanged,
/// when the growth would exceed `memory_limit`. The violation stays
/// recorded (`ZEND_HAS_OOM()`), so the VM raises "Allowed memory size of N
/// bytes exhausted" once the builtin returns (and suppresses the builtin's
/// own warnings meanwhile).
pub(crate) fn fallible_resize(buf: &mut Vec<u8>, new_len: usize) -> bool {
    let additional = new_len.saturating_sub(buf.len());
    if buf.try_grow(additional).is_err() {
        return false;
    }
    buf.resize(new_len, 0);
    true
}

/// The failure of a [`fallible_resize`] (`RETURN_OOM()`): zlib's
/// `"insufficient memory"` warning, which the VM drops while the
/// memory_limit fatal is pending.
pub(crate) fn oom_error() -> Error {
    Error::Warning(z_error(Z_MEM_ERROR).into())
}

/// `php_zlib_inflate_rounds()`: inflates with a growing buffer, at most 100
/// rounds; `max` (0 = unlimited) caps the buffer. The buffer grows through
/// fallible allocations: past `memory_limit` the round fails with
/// `Z_MEM_ERROR`.
fn inflate_rounds(z: &mut ZStream, input: &[u8], max: usize) -> (i32, Vec<u8>) {
    let mut status;
    let mut round = 0;
    let mut pos = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut used = 0usize;
    let mut size = if max != 0 && max < input.len() {
        max
    } else {
        input.len()
    };
    loop {
        if (max != 0 && max <= used) || !fallible_resize(&mut buf, size) {
            status = Z_MEM_ERROR;
        } else {
            let step = z.inflate(&input[pos..], &mut buf[used..size], Z_NO_FLUSH);
            status = step.status;
            pos += step.consumed;
            used += step.produced;
            size += (size >> 3) + 1;
        }
        let more = status == Z_BUF_ERROR || (status == Z_OK && pos < input.len());
        round += 1;
        if !(more && round < 100) {
            break;
        }
    }
    if status == Z_STREAM_END {
        buf.truncate(used);
        (status, buf)
    } else {
        // zpipe.c hack: a stream that ends early is a data error.
        (
            if status == Z_OK { Z_DATA_ERROR } else { status },
            Vec::new(),
        )
    }
}

/// `php_zlib_decode()`: inflates `data` with window bits `encoding`
/// (`ZLIB_ENCODING_ANY` auto-detects gzip/zlib and falls back to raw
/// deflate). `max_len` 0 means unlimited. On failure the warning text is
/// `zError()` of the final status (`"data error"`, `"insufficient
/// memory"`, `"buffer error"`).
pub fn decode_raw(data: &[u8], encoding: i64, max_len: usize) -> Result<Vec<u8>> {
    let mut status = Z_DATA_ERROR;
    if !data.is_empty() {
        // PHP feeds the terminating NUL byte of the zend_string as well.
        let mut input = Vec::with_capacity(data.len() + 1);
        input.extend_from_slice(data);
        input.push(0);
        let mut encoding = encoding;
        loop {
            let enc = i32::try_from(encoding).unwrap_or(15);
            match ZStream::inflate_init(enc) {
                Ok(mut z) => {
                    let (st, out) = inflate_rounds(&mut z, &input, max_len);
                    status = st;
                    if st == Z_STREAM_END {
                        return Ok(out);
                    }
                    if st == Z_DATA_ERROR && encoding == ZLIB_ENCODING_ANY {
                        encoding = ZLIB_ENCODING_RAW;
                        continue;
                    }
                }
                Err(st) => status = st,
            }
            break;
        }
    }
    Err(Error::Warning(z_error(status).into()))
}

/// `PHP_ZLIB_DECODE_FUNC` argument validation and decoding.
fn decode_func(func: &str, data: &[u8], max_length: i64, encoding: i64) -> Result<Vec<u8>> {
    let Ok(max) = usize::try_from(max_length) else {
        return Err(arg_value_error(
            func,
            2,
            "max_length",
            "must be greater than or equal to 0",
        ));
    };
    decode_raw(data, encoding, max)
}

/// `zlib_decode(string $data, int $max_length = 0)`: any encoding.
pub fn zlib_decode(data: &[u8], max_length: i64) -> Result<Vec<u8>> {
    decode_func("zlib_decode", data, max_length, ZLIB_ENCODING_ANY)
}

/// `gzinflate(string $data, int $max_length = 0)`: raw deflate.
pub fn gzinflate(data: &[u8], max_length: i64) -> Result<Vec<u8>> {
    decode_func("gzinflate", data, max_length, ZLIB_ENCODING_RAW)
}

/// `gzdecode(string $data, int $max_length = 0)`: gzip.
pub fn gzdecode(data: &[u8], max_length: i64) -> Result<Vec<u8>> {
    decode_func("gzdecode", data, max_length, ZLIB_ENCODING_GZIP)
}

/// `gzuncompress(string $data, int $max_length = 0)`: zlib.
pub fn gzuncompress(data: &[u8], max_length: i64) -> Result<Vec<u8>> {
    decode_func("gzuncompress", data, max_length, ZLIB_ENCODING_DEFLATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess() {
        assert_eq!(buffer_size_guess(0), 23);
        assert_eq!(buffer_size_guess(1000), 1037); // 1000 * 1.015 rounds down in binary
    }

    #[test]
    fn roundtrips() {
        let data = b"The quick brown fox jumps over the lazy dog".repeat(10);
        for enc in [ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_DEFLATE] {
            for level in -1..=9 {
                let c = zlib_encode(&data, enc, level).unwrap();
                assert_eq!(zlib_decode(&c, 0).unwrap(), data);
            }
        }
    }

    #[test]
    fn validation() {
        assert_eq!(
            gzcompress(b"", 10, ZLIB_ENCODING_DEFLATE).unwrap_err(),
            Error::ValueError("gzcompress(): Argument #2 ($level) must be between -1 and 9".into())
        );
        assert_eq!(
            zlib_encode(b"", 1, 0).unwrap_err().message(),
            "zlib_encode(): Argument #2 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE"
        );
        assert_eq!(
            zlib_encode(b"", 99, -2).unwrap_err().message(),
            "zlib_encode(): Argument #3 ($level) must be between -1 and 9"
        );
        assert_eq!(
            gzinflate(b"x", -1).unwrap_err().message(),
            "gzinflate(): Argument #2 ($max_length) must be greater than or equal to 0"
        );
    }

    #[test]
    fn fallible_resize_and_oom_error() {
        let mut v = vec![7u8];
        assert!(fallible_resize(&mut v, 3));
        assert_eq!(v, [7, 0, 0]);
        assert_eq!(oom_error(), Error::Warning("insufficient memory".into()));
    }

    #[test]
    fn decode_large_expansion() {
        // 4 MiB of zeros deflate to a few KiB: many growth rounds.
        let data = vec![0u8; 4 << 20];
        let c = gzencode(&data, 9, ZLIB_ENCODING_GZIP).unwrap();
        assert!(c.len() < 16 * 1024);
        assert_eq!(gzdecode(&c, 0).unwrap(), data);
    }

    #[test]
    fn decode_errors() {
        assert_eq!(
            gzuncompress(b"abc", 0).unwrap_err(),
            Error::Warning("data error".into())
        );
        assert_eq!(
            gzdecode(b"", 0).unwrap_err(),
            Error::Warning("data error".into())
        );
        let packed = gzdeflate(b"aaaaaaaaaaaaaaa", -1, ZLIB_ENCODING_RAW).unwrap();
        assert_eq!(packed.len(), 5);
        assert_eq!(gzinflate(&packed, 15).unwrap(), b"aaaaaaaaaaaaaaa");
        assert_eq!(gzinflate(&packed, 150).unwrap(), b"aaaaaaaaaaaaaaa");
        assert_eq!(
            gzinflate(&packed, 1).unwrap_err(),
            Error::Warning("insufficient memory".into())
        );
    }
}
