// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `bzcompress()` and `bzdecompress()`.

use crate::error::{arg_value_error, Result};
use crate::ffi::RawStream;
use crate::{BZ_FINISH, BZ_FINISH_OK, BZ_OK, BZ_OUTBUFF_FULL, BZ_STREAM_END};
use php_memlimit::TryGrow;

/// What a bz2 function returns on "success": PHP returns either a string
/// or an `int` libbz2 error code (or `false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The resulting string.
    Data(Vec<u8>),
    /// A libbz2 error code (`BZ_*`, negative), returned as an `int`.
    Code(i64),
    /// `false`.
    False,
}

/// `BZ2_bzBuffToBuffCompress()` into a buffer of `dest_len` bytes.
fn buff_to_buff_compress(
    source: &[u8],
    dest_len: usize,
    block_size: i32,
    work_factor: i32,
) -> std::result::Result<Vec<u8>, i32> {
    let mut z = RawStream::compress_init(block_size, work_factor)?;
    let mut dest = vec![0u8; dest_len];
    let step = z.compress(source, &mut dest, BZ_FINISH);
    z.end();
    match step.status {
        BZ_FINISH_OK => Err(BZ_OUTBUFF_FULL),
        BZ_STREAM_END => {
            dest.truncate(step.produced);
            Ok(dest)
        }
        st => Err(st),
    }
}

/// Resizes `buf` to `new_len` bytes (zero-filled) through a fallible
/// allocation, like PHP's `zend_string_safe_frealloc(..., fallible)`:
/// `false`, with `buf` unchanged, when the growth would exceed
/// `memory_limit`. The violation stays recorded (`ZEND_HAS_OOM()`), so the
/// VM raises "Allowed memory size of N bytes exhausted" once the builtin
/// returns.
fn fallible_resize(buf: &mut Vec<u8>, new_len: usize) -> bool {
    let additional = new_len.saturating_sub(buf.len());
    if buf.try_grow(additional).is_err() {
        return false;
    }
    buf.resize(new_len, 0);
    true
}

/// `bzcompress(string $data, int $block_size = 4, int $work_factor = 0):
/// string|int`.
pub fn bzcompress(data: &[u8], block_size: i64, work_factor: i64) -> Result<Output> {
    if !(1..=9).contains(&block_size) {
        return Err(arg_value_error(
            "bzcompress",
            2,
            "block_size",
            "must be between 1 and 9",
        ));
    }
    if !(0..=250).contains(&work_factor) {
        return Err(arg_value_error(
            "bzcompress",
            3,
            "work_factor",
            "must be between 0 and 250",
        ));
    }
    let max = u32::MAX as usize;
    let chunk_len = data
        .len()
        .checked_add(data.len() / 100 + 600)
        .filter(|&l| l <= max);
    let Some(chunk_len) = chunk_len else {
        return Err(arg_value_error(
            "bzcompress",
            1,
            "data",
            &format!("must have a length less than or equal to {max}"),
        ));
    };
    let block = i32::try_from(block_size).unwrap_or(4);
    let work = i32::try_from(work_factor).unwrap_or(0);
    Ok(match buff_to_buff_compress(data, chunk_len, block, work) {
        Ok(v) => Output::Data(v),
        Err(code) => Output::Code(i64::from(code)),
    })
}

/// `bzdecompress(string $data, bool $use_less_memory = false):
/// string|int|false`. Decompresses the first bzip2 stream; truncated
/// input yields the data decoded so far, like PHP.
pub fn bzdecompress(data: &[u8], small: bool) -> Result<Output> {
    if data.len() > u32::MAX as usize {
        return Err(arg_value_error(
            "bzdecompress",
            1,
            "data",
            &format!("must have a length less than or equal to {}", u32::MAX),
        ));
    }
    let Ok(mut z) = RawStream::decompress_init(small) else {
        return Ok(Output::False);
    };
    // In most cases bz2 offers at least 2:1 compression. Both this buffer
    // and its growth are fallible allocations (`zend_string_safe_falloc`,
    // `zend_string_safe_frealloc`): past `memory_limit` the function gives
    // up (`goto failure`) and the VM raises the recorded "Allowed memory
    // size" fatal, instead of the output growing towards the hard limit.
    let mut dest = Vec::new();
    if !fallible_resize(&mut dest, data.len().saturating_mul(2)) {
        z.end();
        return Ok(Output::False);
    }
    let mut used = 0usize;
    let mut pos = 0usize;
    let mut status;
    loop {
        let step = z.decompress(&data[pos..], &mut dest[used..]);
        pos += step.consumed;
        used += step.produced;
        status = step.status;
        if !(status == BZ_OK && pos < data.len()) {
            break;
        }
        // Compression is better than 2:1: grow by the input size.
        if !fallible_resize(&mut dest, used.saturating_add(data.len())) {
            z.end();
            return Ok(Output::False);
        }
    }
    z.end();
    if status == BZ_STREAM_END || status == BZ_OK {
        dest.truncate(used);
        Ok(Output::Data(dest))
    } else {
        Ok(Output::Code(i64::from(status)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LYRIC: &[u8] = b"Life it seems, will fade away\nDrifting further everyday\nGetting lost within myself\nNothing matters no one else";

    fn data(o: Output) -> Vec<u8> {
        match o {
            Output::Data(d) => d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn compress_decompress() {
        let one = data(bzcompress(b"1", 1, 1).unwrap());
        assert!(one.starts_with(b"BZh1"));
        let d = data(bzcompress(LYRIC, 4, 0).unwrap());
        let d2 = data(bzcompress(LYRIC, 1, 10).unwrap());
        let mut d3 = d2.clone();
        d3[3] = b'0';
        assert_eq!(bzdecompress(b"1", true).unwrap(), Output::Code(-5));
        assert_eq!(bzdecompress(&d3, false).unwrap(), Output::Code(-5));
        assert_eq!(bzdecompress(&d3, true).unwrap(), Output::Code(-5));
        for (x, small) in [(&d, false), (&d, true), (&d2, false)] {
            assert_eq!(data(bzdecompress(x, small).unwrap()), LYRIC);
        }
        assert_eq!(bzdecompress(b"", false).unwrap(), Output::Data(Vec::new()));
    }

    #[test]
    fn filter_compatible_bytes() {
        // bug75776.phpt: bzip2.compress over str_repeat('0123456789abcdef', 1000)
        // produces this stream (block size 4, work factor 0).
        let text = b"0123456789abcdef".repeat(1000);
        let d = data(bzcompress(&text, 4, 0).unwrap());
        let hex: String = d.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        // (PHP's test reads before the filter is flushed, so it sees the
        // first 72 bytes.)
        assert!(hex.starts_with("425a68343141592653599fe7bbbf0001f389007fe03f002000902980026826aa80003ea9061520c6a41954833a9069520d6a41b54837a9071520e6a41d5483ba9079520f6a41f548"));
    }

    #[test]
    fn truncated_gives_partial() {
        let text = b"The quick brown fox jumps over the lazy dog.".repeat(50);
        let d = data(bzcompress(&text, 9, 0).unwrap());
        // Cut inside the end-of-stream marker: all data is decoded but the
        // stream never ends; PHP returns what it has.
        let r = bzdecompress(&d[..d.len() - 4], false).unwrap();
        assert!(matches!(r, Output::Data(_)));
    }

    #[test]
    fn fallible_resize_grows_and_shrinks() {
        let mut v = vec![1u8, 2];
        assert!(fallible_resize(&mut v, 5));
        assert_eq!(v, [1, 2, 0, 0, 0]);
        assert!(fallible_resize(&mut v, 1));
        assert_eq!(v, [1]);
    }

    #[test]
    fn decompress_grows_past_two_to_one() {
        // Highly compressible input needs many growth rounds.
        let text = vec![0u8; 1 << 20];
        let d = data(bzcompress(&text, 9, 0).unwrap());
        assert!(d.len() * 2 < text.len());
        assert_eq!(data(bzdecompress(&d, false).unwrap()), text);
    }

    #[test]
    fn errors() {
        assert_eq!(
            bzcompress(LYRIC, 0, 0).unwrap_err().to_string(),
            "ValueError: bzcompress(): Argument #2 ($block_size) must be between 1 and 9"
        );
        assert_eq!(
            bzcompress(LYRIC, 100, 0).unwrap_err().to_string(),
            "ValueError: bzcompress(): Argument #2 ($block_size) must be between 1 and 9"
        );
        assert_eq!(
            bzcompress(LYRIC, 4, -1).unwrap_err().to_string(),
            "ValueError: bzcompress(): Argument #3 ($work_factor) must be between 0 and 250"
        );
        assert_eq!(
            bzcompress(LYRIC, 4, 255).unwrap_err().to_string(),
            "ValueError: bzcompress(): Argument #3 ($work_factor) must be between 0 and 250"
        );
    }
}
