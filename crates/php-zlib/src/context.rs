// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Incremental compression contexts: `deflate_init()`/`deflate_add()` and
//! `inflate_init()`/`inflate_add()`/`inflate_get_status()`/
//! `inflate_get_read_len()` (the `DeflateContext` and `InflateContext`
//! classes).
//!
//! Options can be given either as a PHP option table
//! ([`DeflateContext::init`], [`InflateContext::init`], which apply PHP's
//! conversions and produce its exact `TypeError`s), or already typed
//! ([`DeflateContext::new`], [`InflateContext::new`]).
//!
//! When userland passes an *object* as `$options`, PHP first emits the
//! deprecation returned by [`object_options_deprecation`] and then reads the
//! object's initialised properties; the VM does both and passes the
//! property table here.

use php_streams::value::{Key, Value};

use crate::codec::{buffer_size_guess, fallible_resize, oom_error, valid_encoding};
use crate::constants::{z_error, ZLIB_ENCODING_RAW};
use crate::conv::{find, try_get_long, value_name};
use crate::error::{arg_type_error, arg_value_error, Error, Result};
use crate::ffi::ZStream;

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_NEED_DICT: i32 = 2;
const Z_DATA_ERROR: i32 = -3;
const Z_BUF_ERROR: i32 = -5;
const Z_FINISH: i64 = 4;

const ENCODING_MSG: &str =
    "must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE";
const FLUSH_MSG: &str = "must be one of ZLIB_NO_FLUSH, ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH, ZLIB_FULL_FLUSH, ZLIB_BLOCK, or ZLIB_FINISH";

/// The `Error` thrown by `new DeflateContext()`.
pub const DEFLATE_CONSTRUCT_ERROR: &str =
    "Cannot directly construct DeflateContext, use deflate_init() instead";
/// The `Error` thrown by `new InflateContext()`.
pub const INFLATE_CONSTRUCT_ERROR: &str =
    "Cannot directly construct InflateContext, use inflate_init() instead";

/// The deprecation PHP emits (as `func(): …`) when `$options` is an object.
pub fn object_options_deprecation(func: &str) -> String {
    format!(
        "Passing an object for argument #2 $option to {func}() is deprecated, call get_object_vars() first instead"
    )
}

/// A preset dictionary option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dictionary {
    /// A string, used verbatim.
    Bytes(Vec<u8>),
    /// An array of strings; each is followed by a NUL byte. Entries must be
    /// non-empty and NUL-free.
    List(Vec<Vec<u8>>),
}

/// Typed `deflate_init()` options (PHP's defaults via `Default`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeflateOptions {
    /// `level`: -1..=9.
    pub level: i64,
    /// `memory`: 1..=9.
    pub memory: i64,
    /// `window`: 8..=15.
    pub window: i64,
    /// `strategy`: one of the `ZLIB_*` strategies.
    pub strategy: i64,
    /// `dictionary`.
    pub dictionary: Option<Dictionary>,
}

impl Default for DeflateOptions {
    fn default() -> Self {
        DeflateOptions {
            level: -1,
            memory: 8,
            window: 15,
            strategy: 0,
            dictionary: None,
        }
    }
}

/// Typed `inflate_init()` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflateOptions {
    /// `window`: 8..=15.
    pub window: i64,
    /// `dictionary`.
    pub dictionary: Option<Dictionary>,
}

impl Default for InflateOptions {
    fn default() -> Self {
        InflateOptions {
            window: 15,
            dictionary: None,
        }
    }
}

/// `zlib_get_long_option()`.
fn long_option(func: &str, options: &[(Key, Value)], name: &str, default: i64) -> Result<i64> {
    let Some(v) = find(options, name) else {
        return Ok(default);
    };
    try_get_long(v).ok_or_else(|| {
        arg_type_error(
            func,
            2,
            "options",
            &format!(
                "the value for option \"{name}\" must be of type int, {} given",
                value_name(v)
            ),
        )
    })
}

fn check_dict_entry(func: &str, s: &[u8]) -> Result<()> {
    if s.is_empty() {
        return Err(arg_value_error(
            func,
            2,
            "options",
            "must not contain empty strings",
        ));
    }
    if s.contains(&0) {
        return Err(arg_value_error(
            func,
            2,
            "options",
            "must not contain strings with null bytes",
        ));
    }
    Ok(())
}

/// The first half of `zlib_create_dictionary_string()`: reads the
/// `dictionary` option.
fn dictionary_option(func: &str, options: &[(Key, Value)]) -> Result<Option<Dictionary>> {
    let Some(v) = find(options, "dictionary") else {
        return Ok(None);
    };
    match v {
        Value::Str(s) => Ok(Some(Dictionary::Bytes(s.clone()))),
        Value::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for (_, item) in items {
                let s = crate::conv::try_get_string(item)?;
                check_dict_entry(func, &s)?;
                list.push(s);
            }
            Ok(Some(Dictionary::List(list)))
        }
        other => Err(arg_type_error(
            func,
            2,
            "options",
            &format!(
                "must be of type zero-terminated string or array, {} given",
                value_name(other)
            ),
        )),
    }
}

/// The second half of `zlib_create_dictionary_string()`: validates and
/// flattens a dictionary. `None` means "no dictionary".
fn flatten_dictionary(func: &str, dict: Option<&Dictionary>) -> Result<Option<Vec<u8>>> {
    match dict {
        None => Ok(None),
        Some(Dictionary::Bytes(b)) => Ok(Some(b.clone())),
        Some(Dictionary::List(list)) => {
            if list.is_empty() {
                return Ok(None);
            }
            let mut out = Vec::new();
            for s in list {
                check_dict_entry(func, s)?;
                out.extend_from_slice(s);
                out.push(0);
            }
            Ok(Some(out))
        }
    }
}

/// Window bits for an encoding and a `window` option (`encoding ± (15 -
/// window)`).
fn window_bits(encoding: i64, window: i64) -> i32 {
    let bits = if encoding < 0 {
        encoding + 15 - window
    } else {
        encoding - (15 - window)
    };
    i32::try_from(bits).unwrap_or(15)
}

fn as_i32(v: i64) -> i32 {
    i32::try_from(v).unwrap_or_default()
}

/// A `DeflateContext` object.
#[derive(Debug)]
pub struct DeflateContext {
    z: ZStream,
}

impl DeflateContext {
    /// `deflate_init(int $encoding, array $options = [])` from a PHP option
    /// table (array, or an object's property table).
    pub fn init(encoding: i64, options: &[(Key, Value)]) -> Result<Self> {
        const F: &str = "deflate_init";
        let mut opts = DeflateOptions {
            level: long_option(F, options, "level", -1)?,
            ..DeflateOptions::default()
        };
        check_level(opts.level)?;
        opts.memory = long_option(F, options, "memory", 8)?;
        check_memory(opts.memory)?;
        opts.window = long_option(F, options, "window", 15)?;
        check_window(opts.window)?;
        opts.strategy = long_option(F, options, "strategy", 0)?;
        check_strategy(opts.strategy)?;
        check_encoding(F, encoding)?;
        opts.dictionary = dictionary_option(F, options)?;
        Self::create(encoding, &opts)
    }

    /// `deflate_init()` with typed options.
    pub fn new(encoding: i64, opts: &DeflateOptions) -> Result<Self> {
        check_level(opts.level)?;
        check_memory(opts.memory)?;
        check_window(opts.window)?;
        check_strategy(opts.strategy)?;
        check_encoding("deflate_init", encoding)?;
        Self::create(encoding, opts)
    }

    fn create(encoding: i64, opts: &DeflateOptions) -> Result<Self> {
        let dict = flatten_dictionary("deflate_init", opts.dictionary.as_ref())?;
        let mut z = ZStream::deflate_init(
            as_i32(opts.level),
            window_bits(encoding, opts.window),
            as_i32(opts.memory),
            as_i32(opts.strategy),
        )
        .map_err(|_| Error::Warning("Failed allocating zlib.deflate context".into()))?;
        if let Some(d) = dict {
            z.set_dictionary(&d);
        }
        Ok(DeflateContext { z })
    }

    /// `deflate_add(DeflateContext $context, string $data, int $flush_mode =
    /// ZLIB_SYNC_FLUSH)`.
    pub fn add(&mut self, data: &[u8], flush_mode: i64) -> Result<Vec<u8>> {
        if !matches!(flush_mode, 0..=5) {
            return Err(arg_value_error("deflate_add", 3, "flush_mode", FLUSH_MSG));
        }
        if data.is_empty() && flush_mode != Z_FINISH {
            return Ok(Vec::new());
        }
        let flush = as_i32(flush_mode);
        let mut out = Vec::new();
        if !fallible_resize(&mut out, buffer_size_guess(data.len()).max(64)) {
            return Err(oom_error());
        }
        let mut used = 0usize;
        let mut pos = 0usize;
        let status = loop {
            if used == out.len() && !fallible_resize(&mut out, used + 64) {
                return Err(oom_error());
            }
            let step = self.z.deflate(&data[pos..], &mut out[used..], flush);
            pos += step.consumed;
            used += step.produced;
            if !(step.status == Z_OK && used == out.len()) {
                break step.status;
            }
        };
        match status {
            Z_OK => {
                out.truncate(used);
                Ok(out)
            }
            Z_STREAM_END => {
                out.truncate(used);
                self.z.reset();
                Ok(out)
            }
            st => Err(Error::Warning(format!("zlib error ({})", z_error(st)))),
        }
    }
}

fn check_level(level: i64) -> Result<()> {
    if (-1..=9).contains(&level) {
        Ok(())
    } else {
        Err(Error::ValueError(
            "deflate_init(): \"level\" option must be between -1 and 9".into(),
        ))
    }
}

fn check_memory(memory: i64) -> Result<()> {
    if (1..=9).contains(&memory) {
        Ok(())
    } else {
        Err(Error::ValueError(
            "deflate_init(): \"memory\" option must be between 1 and 9".into(),
        ))
    }
}

fn check_window(window: i64) -> Result<()> {
    if (8..=15).contains(&window) {
        Ok(())
    } else {
        Err(Error::ValueError(
            "deflate_init(): \"window\" option must be between 8 and 15".into(),
        ))
    }
}

fn check_strategy(strategy: i64) -> Result<()> {
    if (0..=4).contains(&strategy) {
        Ok(())
    } else {
        Err(Error::ValueError(
            "deflate_init(): \"strategy\" option must be one of ZLIB_FILTERED, ZLIB_HUFFMAN_ONLY, ZLIB_RLE, ZLIB_FIXED, or ZLIB_DEFAULT_STRATEGY".into(),
        ))
    }
}

fn check_encoding(func: &str, encoding: i64) -> Result<()> {
    if valid_encoding(encoding) {
        Ok(())
    } else {
        Err(arg_value_error(func, 1, "encoding", ENCODING_MSG))
    }
}

fn check_inflate_window(window: i64) -> Result<()> {
    if (8..=15).contains(&window) {
        Ok(())
    } else {
        Err(Error::ValueError(format!(
            "zlib window size (logarithm) ({window}) must be within 8..15"
        )))
    }
}

/// An `InflateContext` object.
#[derive(Debug)]
pub struct InflateContext {
    z: ZStream,
    dict: Option<Vec<u8>>,
    status: i32,
}

impl InflateContext {
    /// `inflate_init(int $encoding, array $options = [])` from a PHP option
    /// table.
    pub fn init(encoding: i64, options: &[(Key, Value)]) -> Result<Self> {
        const F: &str = "inflate_init";
        let window = long_option(F, options, "window", 15)?;
        check_inflate_window(window)?;
        check_encoding(F, encoding)?;
        let dictionary = dictionary_option(F, options)?;
        Self::create(encoding, &InflateOptions { window, dictionary })
    }

    /// `inflate_init()` with typed options.
    pub fn new(encoding: i64, opts: &InflateOptions) -> Result<Self> {
        check_inflate_window(opts.window)?;
        check_encoding("inflate_init", encoding)?;
        Self::create(encoding, opts)
    }

    fn create(encoding: i64, opts: &InflateOptions) -> Result<Self> {
        let mut dict = flatten_dictionary("inflate_init", opts.dictionary.as_ref())?;
        let mut z = ZStream::inflate_init(window_bits(encoding, opts.window))
            .map_err(|_| Error::Warning("Failed allocating zlib.inflate context".into()))?;
        if encoding == ZLIB_ENCODING_RAW {
            if let Some(d) = dict.take().filter(|d| !d.is_empty()) {
                if z.set_dictionary(&d) == Z_DATA_ERROR {
                    php_streams::diag::warning(
                        "Dictionary does not match expected dictionary (incorrect adler32 hash)",
                    );
                }
            }
        }
        Ok(InflateContext {
            z,
            dict: dict.filter(|d| !d.is_empty()),
            status: Z_OK,
        })
    }

    /// `inflate_add(InflateContext $context, string $data, int $flush_mode =
    /// ZLIB_SYNC_FLUSH)`.
    pub fn add(&mut self, data: &[u8], flush_mode: i64) -> Result<Vec<u8>> {
        const CHUNK: usize = 8192;
        if !matches!(flush_mode, 0..=5) {
            return Err(arg_value_error("inflate_add", 3, "flush_mode", FLUSH_MSG));
        }
        // Lazy reset, so total_in stays readable until the next call.
        if self.status == Z_STREAM_END {
            self.status = Z_OK;
            self.z.reset();
        }
        if data.is_empty() && flush_mode != Z_FINISH {
            return Ok(Vec::new());
        }
        let flush = as_i32(flush_mode);
        let mut out = vec![0u8; data.len().max(CHUNK)];
        let mut used = 0usize;
        let mut pos = 0usize;
        loop {
            let step = self.z.inflate(&data[pos..], &mut out[used..], flush);
            pos += step.consumed;
            used += step.produced;
            self.status = step.status;
            let out_full = used == out.len();
            // More output space needed: `php_zlib_inflate_grow()` extends
            // the result by a chunk through a fallible allocation; past
            // memory_limit inflate_add() gives up (`RETURN_OOM()`).
            let grow = match step.status {
                Z_OK => out_full,
                Z_BUF_ERROR => flush_mode == Z_FINISH && out_full,
                _ => false,
            };
            if grow {
                let grown = out.len() + CHUNK;
                if !fallible_resize(&mut out, grown) {
                    return Err(oom_error());
                }
                continue;
            }
            match step.status {
                Z_OK | Z_STREAM_END | Z_BUF_ERROR => break,
                Z_NEED_DICT => {
                    let Some(dict) = self.dict.take() else {
                        return Err(Error::Warning(
                            "Inflating this data requires a preset dictionary, please specify it in the options array of inflate_init()".into(),
                        ));
                    };
                    if self.z.set_dictionary(&dict) == Z_DATA_ERROR {
                        return Err(Error::Warning(
                            "Dictionary does not match expected dictionary (incorrect adler32 hash)"
                                .into(),
                        ));
                    }
                }
                st => return Err(Error::Warning(z_error(st).into())),
            }
        }
        out.truncate(used);
        Ok(out)
    }

    /// `inflate_get_status()`: the status of the last `inflate()` call.
    pub fn status(&self) -> i64 {
        i64::from(self.status)
    }

    /// `inflate_get_read_len()`: `total_in`.
    pub fn read_len(&self) -> i64 {
        i64::try_from(self.z.total_in()).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{zlib_decode, zlib_encode};
    use crate::constants::{
        ZLIB_BLOCK, ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP, ZLIB_FULL_FLUSH, ZLIB_NO_FLUSH,
        ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH,
    };

    fn arr(items: Vec<(&str, Value)>) -> Vec<(Key, Value)> {
        items.into_iter().map(|(k, v)| (Key::str(k), v)).collect()
    }

    fn err(r: Result<impl std::fmt::Debug>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn deflate_init_errors() {
        assert_eq!(
            err(DeflateContext::init(42, &[])),
            "ValueError: deflate_init(): Argument #1 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE"
        );
        for (k, v, msg) in [
            (
                "level",
                42,
                "ValueError: deflate_init(): \"level\" option must be between -1 and 9",
            ),
            (
                "level",
                -2,
                "ValueError: deflate_init(): \"level\" option must be between -1 and 9",
            ),
            (
                "memory",
                0,
                "ValueError: deflate_init(): \"memory\" option must be between 1 and 9",
            ),
            (
                "memory",
                10,
                "ValueError: deflate_init(): \"memory\" option must be between 1 and 9",
            ),
            (
                "window",
                3,
                "ValueError: deflate_init(): \"window\" option must be between 8 and 15",
            ),
            (
                "strategy",
                9,
                "ValueError: deflate_init(): \"strategy\" option must be one of ZLIB_FILTERED, ZLIB_HUFFMAN_ONLY, ZLIB_RLE, ZLIB_FIXED, or ZLIB_DEFAULT_STRATEGY",
            ),
        ] {
            assert_eq!(
                err(DeflateContext::init(
                    ZLIB_ENCODING_DEFLATE,
                    &arr(vec![(k, Value::Int(v))])
                )),
                msg
            );
        }
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![("level", Value::str("foo"))])
            )),
            "TypeError: deflate_init(): Argument #2 ($options) the value for option \"level\" must be of type int, string given"
        );
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![(
                    "window",
                    Value::Object {
                        class: "A".into(),
                        string_value: None
                    }
                )])
            )),
            "TypeError: deflate_init(): Argument #2 ($options) the value for option \"window\" must be of type int, A given"
        );
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![("strategy", Value::Stream(3))])
            )),
            "TypeError: deflate_init(): Argument #2 ($options) the value for option \"strategy\" must be of type int, resource given"
        );
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![("dictionary", Value::Int(3))])
            )),
            "TypeError: deflate_init(): Argument #2 ($options) must be of type zero-terminated string or array, int given"
        );
        let dict = |items: Vec<Value>| {
            arr(vec![(
                "dictionary",
                Value::Array(
                    items
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| (Key::Int(i64::try_from(i).unwrap()), v))
                        .collect(),
                ),
            )])
        };
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &dict(vec![Value::str(" "), Value::str("")])
            )),
            "ValueError: deflate_init(): Argument #2 ($options) must not contain empty strings"
        );
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &dict(vec![Value::str("hello"), Value::str("wor\0ld")])
            )),
            "ValueError: deflate_init(): Argument #2 ($options) must not contain strings with null bytes"
        );
        assert_eq!(
            err(DeflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &dict(vec![
                    Value::str(" "),
                    Value::Object {
                        class: "stdClass".into(),
                        string_value: None
                    }
                ])
            )),
            "Error: Object of class stdClass could not be converted to string"
        );
        // Encoding is checked before the dictionary.
        assert_eq!(
            err(InflateContext::init(
                123_456,
                &arr(vec![("dictionary", Value::str("dict"))])
            )),
            "ValueError: inflate_init(): Argument #1 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE"
        );
        assert_eq!(
            err(InflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![("window", Value::Array(vec![]))])
            )),
            "TypeError: inflate_init(): Argument #2 ($options) the value for option \"window\" must be of type int, array given"
        );
        assert_eq!(
            err(InflateContext::init(
                ZLIB_ENCODING_DEFLATE,
                &arr(vec![("window", Value::Int(3))])
            )),
            "ValueError: zlib window size (logarithm) (3) must be within 8..15"
        );
    }

    #[test]
    fn deflate_add_flush_types_roundtrip() {
        let flushes = [
            ZLIB_SYNC_FLUSH,
            ZLIB_PARTIAL_FLUSH,
            ZLIB_FULL_FLUSH,
            ZLIB_NO_FLUSH,
            ZLIB_BLOCK,
        ];
        for mode in [ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_DEFLATE] {
            for flush_size in [1usize, 4, 32768] {
                for &flush in &flushes {
                    let mut ctx = DeflateContext::new(mode, &DeflateOptions::default()).unwrap();
                    let mut buffer = Vec::new();
                    let mut compressed = Vec::new();
                    let mut plain = Vec::new();
                    for c in b'a'..=b'z' {
                        plain.push(c);
                        buffer.push(c);
                        if buffer.len() >= flush_size {
                            compressed.extend(ctx.add(&buffer, flush).unwrap());
                            buffer.clear();
                        }
                    }
                    compressed.extend(ctx.add(&buffer, Z_FINISH).unwrap());
                    assert_eq!(zlib_decode(&compressed, 0).unwrap(), plain);
                }
            }
        }
        let mut ctx =
            DeflateContext::new(ZLIB_ENCODING_DEFLATE, &DeflateOptions::default()).unwrap();
        assert_eq!(
            err(ctx.add(b"test", 6789)),
            "ValueError: deflate_add(): Argument #3 ($flush_mode) must be one of ZLIB_NO_FLUSH, ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH, ZLIB_FULL_FLUSH, ZLIB_BLOCK, or ZLIB_FINISH"
        );
    }

    #[test]
    fn deflate_reuse_after_finish() {
        let mut ctx =
            DeflateContext::new(ZLIB_ENCODING_DEFLATE, &DeflateOptions::default()).unwrap();
        for c in b'a'..=b'z' {
            ctx.add(&[c], ZLIB_SYNC_FLUSH).unwrap();
        }
        ctx.add(b"", Z_FINISH).unwrap();
        let mut compressed = Vec::new();
        for c in b'a'..=b'z' {
            compressed.extend(ctx.add(&[c], ZLIB_NO_FLUSH).unwrap());
        }
        compressed.extend(ctx.add(b"", Z_FINISH).unwrap());
        assert_eq!(
            zlib_decode(&compressed, 0).unwrap(),
            (b'a'..=b'z').collect::<Vec<_>>()
        );
    }

    #[test]
    fn buffer_full_ends_with_sync_marker() {
        let mut ctx = DeflateContext::new(ZLIB_ENCODING_RAW, &DeflateOptions::default()).unwrap();
        let out = ctx
            .add(
                &[0x25, 0x50, 0x44, 0x46, 0x2d, 0x31, 0x2e, 0x32],
                ZLIB_SYNC_FLUSH,
            )
            .unwrap();
        assert_eq!(&out[out.len() - 4..], &[0, 0, 0xff, 0xff]);
    }

    #[test]
    fn inflate_status_and_read_len() {
        let compressed = zlib_encode(b"Hello world.", ZLIB_ENCODING_DEFLATE, -1).unwrap();
        assert_eq!(compressed.len(), 20);
        let mut junk = compressed.clone();
        junk.extend(b"qebsouesl".repeat(128));
        let mut ctx =
            InflateContext::new(ZLIB_ENCODING_DEFLATE, &InflateOptions::default()).unwrap();
        assert_eq!(ctx.add(&junk, ZLIB_SYNC_FLUSH).unwrap(), b"Hello world.");
        assert_eq!(ctx.status(), 1);
        assert_eq!(ctx.read_len(), 20);

        let mut ctx =
            InflateContext::new(ZLIB_ENCODING_DEFLATE, &InflateOptions::default()).unwrap();
        let mut buf = Vec::new();
        let mut i = 0;
        while ctx.status() == 0 {
            buf.extend(ctx.add(&junk[i..=i], ZLIB_SYNC_FLUSH).unwrap());
            i += 1;
        }
        assert_eq!(i, 20);
        assert_eq!(buf, b"Hello world.");
        ctx.add(&compressed[..12], ZLIB_SYNC_FLUSH).unwrap();
        assert_eq!(ctx.status(), 0);
    }

    #[test]
    fn inflate_byte_by_byte_reuse() {
        let plain: Vec<u8> = (b'a'..=b'z').collect();
        let compressed = crate::codec::gzencode(&plain, -1, ZLIB_ENCODING_GZIP).unwrap();
        let mut ctx = InflateContext::new(ZLIB_ENCODING_GZIP, &InflateOptions::default()).unwrap();
        for _ in 0..2 {
            let mut out = Vec::new();
            for b in &compressed {
                out.extend(ctx.add(std::slice::from_ref(b), ZLIB_SYNC_FLUSH).unwrap());
            }
            out.extend(ctx.add(b"", Z_FINISH).unwrap());
            assert_eq!(out, plain);
        }
    }

    #[test]
    fn dictionaries() {
        let letters: Vec<Value> = (b'a'..=b'z').map(|c| Value::str(vec![c])).collect();
        let as_arr = |v: &[Value]| {
            Value::Array(
                v.iter()
                    .cloned()
                    .enumerate()
                    .map(|(i, v)| (Key::Int(i64::try_from(i).unwrap()), v))
                    .collect(),
            )
        };
        let mut d = DeflateContext::init(
            ZLIB_ENCODING_DEFLATE,
            &arr(vec![("dictionary", as_arr(&letters))]),
        )
        .unwrap();
        let a = d.add(b"abdcde", Z_FINISH).unwrap();
        let mut flat = Vec::new();
        for c in b'a'..=b'z' {
            flat.push(c);
            flat.push(0);
        }
        let mut d2 = DeflateContext::init(
            ZLIB_ENCODING_DEFLATE,
            &arr(vec![("dictionary", Value::str(flat))]),
        )
        .unwrap();
        assert_eq!(d2.add(b"abdcde", Z_FINISH).unwrap(), a);
        let mut i = InflateContext::init(
            ZLIB_ENCODING_DEFLATE,
            &arr(vec![("dictionary", as_arr(&letters))]),
        )
        .unwrap();
        assert_eq!(i.add(&a, Z_FINISH).unwrap(), b"abdcde");
        let mut wrong = vec![Value::str("8")];
        wrong.extend(letters.iter().skip(1).cloned());
        let mut i = InflateContext::init(
            ZLIB_ENCODING_DEFLATE,
            &arr(vec![("dictionary", as_arr(&wrong))]),
        )
        .unwrap();
        assert_eq!(
            i.add(&a, Z_FINISH).unwrap_err(),
            Error::Warning(
                "Dictionary does not match expected dictionary (incorrect adler32 hash)".into()
            )
        );
        let mut i = InflateContext::new(ZLIB_ENCODING_DEFLATE, &InflateOptions::default()).unwrap();
        assert_eq!(i.add(&a, ZLIB_SYNC_FLUSH).unwrap_err(), Error::Warning("Inflating this data requires a preset dictionary, please specify it in the options array of inflate_init()".into()));
    }

    #[test]
    fn raw_dictionary_small_window() {
        let dict = b"the quick brown fox jumps over the lazy dog";
        let data = [&dict[..], b" "].concat().repeat(8);
        let opts = arr(vec![
            ("window", Value::Int(10)),
            ("dictionary", Value::str(&dict[..])),
        ]);
        let mut d = DeflateContext::init(ZLIB_ENCODING_RAW, &opts).unwrap();
        let comp = d.add(&data, Z_FINISH).unwrap();
        let mut i = InflateContext::init(ZLIB_ENCODING_RAW, &opts).unwrap();
        assert_eq!(i.add(&comp, Z_FINISH).unwrap(), data);
    }

    #[test]
    fn oracle_bytes() {
        let mut c = DeflateContext::init(
            ZLIB_ENCODING_GZIP,
            &arr(vec![("level", Value::Int(9)), ("window", Value::Int(9))]),
        )
        .unwrap();
        assert_eq!(
            hex(&c.add(b"hello hello hello", Z_FINISH).unwrap()),
            "1f8b0800000000000203cb48cdc9c957c84090008088f9e511000000"
        );
        let mut c = DeflateContext::init(
            ZLIB_ENCODING_RAW,
            &arr(vec![("dictionary", Value::str("hello"))]),
        )
        .unwrap();
        assert_eq!(
            hex(&c.add(b"hello hello hello", ZLIB_SYNC_FLUSH).unwrap()),
            "ca00110a4824000000ffff"
        );
        assert_eq!(hex(&c.add(b"", Z_FINISH).unwrap()), "0300");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().fold(String::new(), |mut acc, x| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{x:02x}");
            acc
        })
    }
}
