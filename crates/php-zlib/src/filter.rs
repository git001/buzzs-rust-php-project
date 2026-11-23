// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `zlib.inflate` and `zlib.deflate` stream filters
//! (`ext/zlib/zlib_filter.c`), implementing `php-streams`'
//! [`StreamFilter`] protocol. Register them with [`register_filters`].

use std::rc::Rc;

use php_streams::diag::{self, Diagnostic, Severity};
use php_streams::filter::{
    parse_write_seek_mode, Brigade, Bucket, FilterEnv, FilterFactory, FilterFlags, FilterRegistry,
    FilterStatus, Seekable, StreamFilter,
};
use php_streams::value::Value;

use crate::constants::{z_error, MAX_MEM_LEVEL, MAX_WBITS};
use crate::ffi::{Heap, ZStream};

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_BUF_ERROR: i32 = -5;
const Z_NO_FLUSH: i32 = 0;
const Z_SYNC_FLUSH: i32 = 2;
const Z_FULL_FLUSH: i32 = 3;
const Z_FINISH: i32 = 4;

/// Size of the filters' input and output buffers.
const BUF_LEN: usize = 0x8000;

/// Registers the `zlib.*` filter factory (`PHP_MINIT_FUNCTION(zlib)`).
pub fn register_filters(reg: &mut FilterRegistry) -> bool {
    reg.register("zlib.*", Rc::new(ZlibFilterFactory))
}

/// The `zlib.inflate` filter.
#[derive(Debug)]
pub struct InflateFilter {
    z: ZStream,
    window_bits: i32,
    finished: bool,
    outbuf: Vec<u8>,
    write_seekable: Seekable,
}

impl InflateFilter {
    /// Creates the filter with the given `windowBits` (`-15` = raw deflate,
    /// `15` zlib, `31` gzip, `47` auto-detect).
    pub fn new(window_bits: i32, write_seekable: Seekable) -> Option<Self> {
        let z = ZStream::inflate_init(window_bits, Heap::Request).ok()?;
        Some(InflateFilter {
            z,
            window_bits,
            finished: false,
            outbuf: vec![0; BUF_LEN],
            write_seekable,
        })
    }

    fn emit(&self, produced: usize, out: &mut Brigade) -> bool {
        if produced > 0 {
            out.push_back(Bucket::new(&self.outbuf[..produced]));
            true
        } else {
            false
        }
    }
}

impl StreamFilter for InflateFilter {
    fn label(&self) -> &'static str {
        "zlib.inflate"
    }

    fn filter(
        &mut self,
        _env: &mut FilterEnv,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> FilterStatus {
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0usize;
        let flush = if flags.is_closing() {
            Z_FINISH
        } else {
            Z_SYNC_FLUSH
        };
        while let Some(bucket) = input.pop_front() {
            let data = bucket.data;
            let mut bin = 0usize;
            while bin < data.len() && !self.finished {
                // Output is unbounded relative to the input: stop once a
                // memory_limit violation is pending (`ZEND_HAS_OOM()`).
                if php_memlimit::is_pending() {
                    break;
                }
                let desired = (data.len() - bin).min(BUF_LEN);
                let mut outbuf = std::mem::take(&mut self.outbuf);
                let step = self
                    .z
                    .inflate(&data[bin..bin + desired], &mut outbuf, flush);
                self.outbuf = outbuf;
                if step.status == Z_STREAM_END {
                    self.z.end();
                    self.finished = true;
                    exit = FilterStatus::PassOn;
                } else if step.status != Z_OK && step.status != Z_BUF_ERROR {
                    diag::notice(format!("zlib: {}", z_error(step.status)));
                    return FilterStatus::ErrFatal;
                }
                bin += step.consumed;
                if self.emit(step.produced, output) {
                    exit = FilterStatus::PassOn;
                }
            }
            total += data.len();
        }
        if !self.finished && flags.is_closing() {
            let mut status = Z_OK;
            while status == Z_OK && !php_memlimit::is_pending() {
                let mut outbuf = std::mem::take(&mut self.outbuf);
                let step = self.z.inflate(&[], &mut outbuf, Z_FINISH);
                self.outbuf = outbuf;
                status = step.status;
                if self.emit(step.produced, output) {
                    exit = FilterStatus::PassOn;
                }
            }
        }
        if let Some(c) = consumed {
            *c = total;
        }
        exit
    }

    fn seek(&mut self, _env: &mut FilterEnv, _offset: i64, _whence: i32) -> bool {
        if self.finished {
            if self.z.inflate_reinit(self.window_bits) != Z_OK {
                diag::warning("zlib.inflate: failed to reinitialize inflation state");
                return false;
            }
        } else if self.z.reset() != Z_OK {
            diag::warning("zlib.inflate: failed to reset inflation state");
            return false;
        }
        self.finished = false;
        true
    }

    fn read_seekable(&self) -> Seekable {
        Seekable::Start
    }

    fn write_seekable(&self) -> Seekable {
        self.write_seekable
    }
}

/// The `zlib.deflate` filter.
#[derive(Debug)]
pub struct DeflateFilter {
    z: ZStream,
    /// No flush is pending.
    finished: bool,
    outbuf: Vec<u8>,
    write_seekable: Seekable,
}

impl DeflateFilter {
    /// Creates the filter (`deflateInit2(level, Z_DEFLATED, window_bits,
    /// mem_level, Z_DEFAULT_STRATEGY)`).
    pub fn new(
        level: i32,
        window_bits: i32,
        mem_level: i32,
        write_seekable: Seekable,
    ) -> Option<Self> {
        let z = ZStream::deflate_init(level, window_bits, mem_level, 0, Heap::Request).ok()?;
        Some(DeflateFilter {
            z,
            finished: true,
            outbuf: vec![0; BUF_LEN],
            write_seekable,
        })
    }

    fn step(&mut self, input: &[u8], flush: i32, output: &mut Brigade) -> (i32, usize, bool) {
        let mut outbuf = std::mem::take(&mut self.outbuf);
        let step = self.z.deflate(input, &mut outbuf, flush);
        let emitted = step.produced > 0;
        if emitted {
            output.push_back(Bucket::new(&outbuf[..step.produced]));
        }
        self.outbuf = outbuf;
        (step.status, step.consumed, emitted)
    }
}

impl StreamFilter for DeflateFilter {
    fn label(&self) -> &'static str {
        "zlib.deflate"
    }

    fn filter(
        &mut self,
        _env: &mut FilterEnv,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> FilterStatus {
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0usize;
        let flush_mode = match flags {
            FilterFlags::FlushClose => Z_FULL_FLUSH,
            FilterFlags::FlushInc => Z_SYNC_FLUSH,
            FilterFlags::Normal => Z_NO_FLUSH,
        };
        while let Some(bucket) = input.pop_front() {
            let data = bucket.data;
            let mut bin = 0usize;
            while bin < data.len() {
                let desired = (data.len() - bin).min(BUF_LEN);
                self.finished = flush_mode != Z_NO_FLUSH;
                let (status, used, emitted) =
                    self.step(&data[bin..bin + desired], flush_mode, output);
                if status != Z_OK {
                    return FilterStatus::ErrFatal;
                }
                bin += used;
                if emitted {
                    exit = FilterStatus::PassOn;
                }
            }
            total += data.len();
        }
        if flags.is_closing() || (flags == FilterFlags::FlushInc && !self.finished) {
            let flush = if flags.is_closing() {
                Z_FINISH
            } else {
                Z_SYNC_FLUSH
            };
            let mut status = Z_OK;
            while status == Z_OK {
                let (st, _, emitted) = self.step(&[], flush, output);
                status = st;
                self.finished = true;
                if emitted {
                    exit = FilterStatus::PassOn;
                }
            }
        }
        if let Some(c) = consumed {
            *c = total;
        }
        exit
    }

    fn seek(&mut self, _env: &mut FilterEnv, _offset: i64, _whence: i32) -> bool {
        if self.z.reset() != Z_OK {
            diag::warning("zlib.deflate: failed to reset deflation state");
            return false;
        }
        self.finished = true;
        true
    }

    fn read_seekable(&self) -> Seekable {
        Seekable::Start
    }

    fn write_seekable(&self) -> Seekable {
        self.write_seekable
    }
}

/// The `zlib.*` factory (`php_zlib_filter_create()`).
///
/// PHP reads an object parameter's properties after a deprecation; since
/// [`Value::Object`] carries no properties, an object is treated as an empty
/// table here (the VM should convert objects to arrays first to get PHP's
/// full behaviour).
#[derive(Debug, Clone, Copy, Default)]
pub struct ZlibFilterFactory;

fn options_of(params: &Value) -> &[(php_streams::value::Key, Value)] {
    params.as_array().unwrap_or(&[])
}

fn warn_compression(msg: String) {
    diag::warning(msg);
}

impl FilterFactory for ZlibFilterFactory {
    fn create(
        &self,
        name: &str,
        params: Option<&Value>,
        _persistent: bool,
    ) -> Option<Box<dyn StreamFilter>> {
        if let Some(Value::Object { .. }) = params {
            diag::emit(Diagnostic::new(
                Severity::Deprecated,
                format!(
                    "Passing an object for filter parameters for {name} is deprecated, call get_object_vars() first instead"
                ),
            ));
        }
        let write_seekable = parse_write_seek_mode(params)?;
        if name.eq_ignore_ascii_case("zlib.inflate") {
            let mut window_bits = -MAX_WBITS;
            if let Some(p @ (Value::Array(_) | Value::Object { .. })) = params {
                if let Some(v) = crate::conv::find(options_of(p), "window") {
                    let tmp = v.to_long();
                    if (-i64::from(MAX_WBITS)..=i64::from(MAX_WBITS) + 32).contains(&tmp) {
                        window_bits = i32::try_from(tmp).unwrap_or(-MAX_WBITS);
                    } else {
                        warn_compression(format!(
                            "Invalid parameter given for window size ({tmp})"
                        ));
                    }
                }
            }
            InflateFilter::new(window_bits, write_seekable)
                .map(|f| Box::new(f) as Box<dyn StreamFilter>)
        } else if name.eq_ignore_ascii_case("zlib.deflate") {
            let mut level = -1i32;
            let mut window_bits = -MAX_WBITS;
            let mut mem_level = MAX_MEM_LEVEL;
            let mut set_level = |tmp: i64| {
                if (-1..=9).contains(&tmp) {
                    level = i32::try_from(tmp).unwrap_or(-1);
                } else {
                    warn_compression(format!("Invalid compression level specified. ({tmp})"));
                }
            };
            match params {
                None => {}
                Some(p @ (Value::Array(_) | Value::Object { .. })) => {
                    let ht = options_of(p);
                    if let Some(v) = crate::conv::find(ht, "memory") {
                        let tmp = v.to_long();
                        if (1..=i64::from(MAX_MEM_LEVEL)).contains(&tmp) {
                            mem_level = i32::try_from(tmp).unwrap_or(MAX_MEM_LEVEL);
                        } else {
                            warn_compression(format!(
                                "Invalid parameter given for memory level ({tmp})"
                            ));
                        }
                    }
                    if let Some(v) = crate::conv::find(ht, "window") {
                        let tmp = v.to_long();
                        if (-i64::from(MAX_WBITS)..=i64::from(MAX_WBITS) + 16).contains(&tmp) {
                            window_bits = i32::try_from(tmp).unwrap_or(-MAX_WBITS);
                        } else {
                            warn_compression(format!(
                                "Invalid parameter given for window size ({tmp})"
                            ));
                        }
                    }
                    if let Some(v) = crate::conv::find(ht, "level") {
                        set_level(v.to_long());
                    }
                }
                Some(v @ (Value::Str(_) | Value::Float(_) | Value::Int(_))) => {
                    set_level(v.to_long());
                }
                Some(_) => warn_compression("Invalid filter parameter, ignored".into()),
            }
            DeflateFilter::new(level, window_bits, mem_level, write_seekable)
                .map(|f| Box::new(f) as Box<dyn StreamFilter>)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_streams::filter::{ChainKind, FilterChain};
    use php_streams::value::Key;

    fn run(
        f: &mut dyn StreamFilter,
        chunks: &[&[u8]],
        flags: FilterFlags,
    ) -> (FilterStatus, Vec<u8>) {
        let mut env = FilterEnv::detached(ChainKind::Read);
        let mut input: Brigade = chunks.iter().map(|c| Bucket::new(*c)).collect();
        let mut out = Brigade::new();
        let st = f.filter(&mut env, &mut input, &mut out, None, flags);
        (st, out.into_iter().flat_map(|b| b.data).collect())
    }

    fn create(name: &str, params: Option<&Value>) -> Option<Box<dyn StreamFilter>> {
        ZlibFilterFactory.create(name, params, false)
    }

    #[test]
    fn deflate_then_inflate() {
        let text = b"I am the very model of a modern major general, I've information vegetable, animal, and mineral.";
        let mut d = create("zlib.deflate", None).unwrap();
        let (st, mid) = run(d.as_mut(), &[text], FilterFlags::Normal);
        assert_eq!(st, FilterStatus::FeedMe);
        assert!(mid.is_empty());
        let (st, packed) = run(d.as_mut(), &[], FilterFlags::FlushClose);
        assert_eq!(st, FilterStatus::PassOn);
        assert_eq!(crate::codec::gzinflate(&packed, 0).unwrap(), text);
        // PHP: fwrite + fclose through zlib.deflate on php://memory.
        assert_eq!(
            packed,
            crate::codec::gzdeflate(text, -1, crate::constants::ZLIB_ENCODING_RAW).unwrap()
        );
        let mut i = create("ZLIB.INFLATE", None).unwrap();
        let (st, plain) = run(
            i.as_mut(),
            &[&packed[..10], &packed[10..]],
            FilterFlags::Normal,
        );
        assert_eq!(st, FilterStatus::PassOn);
        assert_eq!(plain, text);
    }

    #[test]
    fn inflate_error_notice() {
        let _ = diag::take();
        let mut i = create("zlib.inflate", None).unwrap();
        let (st, _) = run(
            i.as_mut(),
            &[b"\xff\xff\xff\xff garbage"],
            FilterFlags::Normal,
        );
        assert_eq!(st, FilterStatus::ErrFatal);
        let d = diag::take();
        assert_eq!(d[0].text(), "zlib: data error");
        assert_eq!(d[0].severity, Severity::Notice);
    }

    #[test]
    fn gzip_window_param() {
        let gz = crate::codec::gzencode(b"The quick brown fox", -1, 31).unwrap();
        let p = Value::Array(vec![(Key::str("window"), Value::Int(31))]);
        let mut i = create("zlib.inflate", Some(&p)).unwrap();
        let (_, plain) = run(i.as_mut(), &[&gz], FilterFlags::Normal);
        assert_eq!(plain, b"The quick brown fox");
        let mut d = create("zlib.deflate", Some(&p)).unwrap();
        let (_, out) = run(d.as_mut(), &[b"abc"], FilterFlags::FlushClose);
        assert_eq!(crate::codec::gzdecode(&out, 0).unwrap(), b"abc");
    }

    #[test]
    fn param_warnings() {
        let _ = diag::take();
        assert!(create("zlib.deflate", Some(&Value::Int(10))).is_some());
        assert!(create("zlib.deflate", Some(&Value::Bool(true))).is_some());
        let p = Value::Array(vec![
            (Key::str("memory"), Value::Int(0)),
            (Key::str("window"), Value::Int(99)),
            (Key::str("level"), Value::Int(-5)),
        ]);
        assert!(create("zlib.deflate", Some(&p)).is_some());
        let p = Value::Array(vec![(Key::str("window"), Value::Int(48))]);
        assert!(create("zlib.inflate", Some(&p)).is_some());
        let texts: Vec<String> = diag::take().iter().map(Diagnostic::text).collect();
        assert_eq!(
            texts,
            [
                "Invalid compression level specified. (10)",
                "Invalid filter parameter, ignored",
                "Invalid parameter given for memory level (0)",
                "Invalid parameter given for window size (99)",
                "Invalid compression level specified. (-5)",
                "Invalid parameter given for window size (48)",
            ]
        );
        assert!(create("zlib.nope", None).is_none());
        let obj = Value::Object {
            class: "stdClass".into(),
            string_value: None,
        };
        assert!(create("zlib.inflate", Some(&obj)).is_some());
        assert_eq!(
            diag::take()[0].text(),
            "Passing an object for filter parameters for zlib.inflate is deprecated, call get_object_vars() first instead"
        );
        let bad = Value::Array(vec![(Key::str("write_seek_mode"), Value::str("nope"))]);
        assert!(create("zlib.deflate", Some(&bad)).is_none());
    }

    #[test]
    fn seek_resets() {
        let packed = crate::codec::gzdeflate(b"hello world", -1, -15).unwrap();
        let mut i = create("zlib.inflate", None).unwrap();
        let mut env = FilterEnv::detached(ChainKind::Read);
        assert_eq!(
            run(i.as_mut(), &[&packed], FilterFlags::Normal).1,
            b"hello world"
        );
        assert!(i.seek(&mut env, 0, 0));
        assert_eq!(
            run(i.as_mut(), &[&packed], FilterFlags::Normal).1,
            b"hello world"
        );
        assert_eq!(i.read_seekable(), Seekable::Start);
        let mut d = create("zlib.deflate", None).unwrap();
        run(d.as_mut(), &[b"xxx"], FilterFlags::Normal);
        assert!(d.seek(&mut env, 0, 0));
        let (_, out) = run(d.as_mut(), &[b"abc"], FilterFlags::FlushClose);
        assert_eq!(crate::codec::gzinflate(&out, 0).unwrap(), b"abc");
    }

    #[test]
    fn oracle_fflush_sequence() {
        // fwrite, fflush, fwrite, stream_filter_remove() (PHP 8.3 oracle).
        let mut d = create("zlib.deflate", Some(&Value::Int(9))).unwrap();
        let mut all = run(d.as_mut(), &[b"hello world "], FilterFlags::Normal).1;
        all.extend(run(d.as_mut(), &[], FilterFlags::FlushInc).1);
        all.extend(run(d.as_mut(), &[b"hello again"], FilterFlags::Normal).1);
        all.extend(run(d.as_mut(), &[], FilterFlags::FlushClose).1);
        let hex: String = all.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        assert_eq!(
            hex,
            "ca48cdc9c95728cf2fca495100000000ffffcb00b313d31333f300"
        );
    }

    #[test]
    fn registry() {
        let mut reg = FilterRegistry::new();
        assert!(register_filters(&mut reg));
        let mut chain = FilterChain::default();
        chain.push_back(reg.create("zlib.deflate", None, false).unwrap());
        chain.push_back(reg.create("zlib.inflate", None, false).unwrap());
        let mut env = FilterEnv::detached(ChainKind::Write);
        let (st, out) = chain.run(
            0,
            &mut env,
            Brigade::from(vec![Bucket::new("round trip")]),
            None,
            FilterFlags::FlushClose,
        );
        assert_eq!(st, FilterStatus::PassOn);
        assert_eq!(
            out.into_iter().flat_map(|b| b.data).collect::<Vec<_>>(),
            b"round trip"
        );
    }
}
