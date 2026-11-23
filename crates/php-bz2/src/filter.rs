// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `bzip2.compress` and `bzip2.decompress` stream filters
//! (`ext/bz2/bz2_filter.c`) for `php-streams`. Register them with
//! [`register_filters`].

use std::rc::Rc;

use php_streams::diag::{self, Diagnostic, Severity};
use php_streams::filter::{
    parse_write_seek_mode, Brigade, Bucket, FilterEnv, FilterFactory, FilterFlags, FilterRegistry,
    FilterStatus, Seekable, StreamFilter,
};
use php_streams::value::Value;

use crate::conv::{find, try_get_long, type_name};
use crate::ffi::{Heap, RawStream};
use crate::{
    BZ_FINISH, BZ_FINISH_OK, BZ_FLUSH, BZ_FLUSH_OK, BZ_OK, BZ_RUN, BZ_RUN_OK, BZ_STREAM_END,
};

/// The filters' input/output buffer size.
const BUF_LEN: usize = 2048;
/// `PHP_BZ2_FILTER_DEFAULT_BLOCKSIZE`.
pub const DEFAULT_BLOCKSIZE: i32 = 4;
/// `PHP_BZ2_FILTER_DEFAULT_WORKFACTOR`.
pub const DEFAULT_WORKFACTOR: i32 = 0;

/// Registers the `bzip2.*` factory (`PHP_MINIT_FUNCTION(bz2)`).
pub fn register_filters(reg: &mut FilterRegistry) -> bool {
    reg.register("bzip2.*", Rc::new(Bz2FilterFactory))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Uninitialized,
    Running,
    Finished,
}

fn emit(outbuf: &[u8], produced: usize, output: &mut Brigade) -> bool {
    if produced > 0 {
        output.push_back(Bucket::new(&outbuf[..produced]));
        true
    } else {
        false
    }
}

/// The `bzip2.decompress` filter.
#[derive(Debug)]
pub struct DecompressFilter {
    strm: Option<RawStream>,
    state: State,
    small: bool,
    concatenated: bool,
    outbuf: Vec<u8>,
    write_seekable: Seekable,
}

impl DecompressFilter {
    /// Creates the filter (`small` = low-memory decompression,
    /// `concatenated` = keep decoding after the end of a stream).
    pub fn new(small: bool, concatenated: bool, write_seekable: Seekable) -> Self {
        DecompressFilter {
            strm: None,
            state: State::Uninitialized,
            small,
            concatenated,
            outbuf: vec![0; BUF_LEN],
            write_seekable,
        }
    }

    fn decompress(&mut self, input: &[u8]) -> (i32, usize, usize) {
        let Some(strm) = self.strm.as_mut() else {
            return (crate::BZ_SEQUENCE_ERROR, 0, 0);
        };
        let st = strm.decompress(input, &mut self.outbuf);
        (st.status, st.consumed, st.produced)
    }

    fn end(&mut self) {
        if let Some(mut s) = self.strm.take() {
            s.end();
        }
    }
}

impl StreamFilter for DecompressFilter {
    fn label(&self) -> &'static str {
        "bzip2.decompress"
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
        while let Some(bucket) = input.pop_front() {
            let data = bucket.data;
            let mut bin = 0usize;
            while bin < data.len() {
                // Output is unbounded relative to the input: stop once a
                // memory_limit violation is pending (`ZEND_HAS_OOM()`).
                if php_memlimit::is_pending() {
                    break;
                }
                if self.state == State::Uninitialized {
                    match RawStream::decompress_init(self.small, Heap::Request) {
                        Ok(s) => self.strm = Some(s),
                        Err(_) => return FilterStatus::ErrFatal,
                    }
                    self.state = State::Running;
                }
                if self.state != State::Running {
                    total += data.len();
                    break;
                }
                let desired = (data.len() - bin).min(BUF_LEN);
                let (status, used, produced) = self.decompress(&data[bin..bin + desired]);
                if status == BZ_STREAM_END {
                    self.end();
                    self.state = if self.concatenated {
                        State::Uninitialized
                    } else {
                        State::Finished
                    };
                } else if status != BZ_OK {
                    diag::notice("bzip2 decompression failed");
                    return FilterStatus::ErrFatal;
                }
                total += used;
                bin += used;
                if emit(&self.outbuf, produced, output) {
                    exit = FilterStatus::PassOn;
                } else if status == BZ_STREAM_END {
                    // No more data to decompress and nothing was produced.
                    return FilterStatus::PassOn;
                }
            }
        }
        if self.state == State::Running && flags.is_closing() {
            let mut status = BZ_OK;
            while status == BZ_OK && !php_memlimit::is_pending() {
                let (st, _, produced) = self.decompress(&[]);
                status = st;
                if emit(&self.outbuf, produced, output) {
                    exit = FilterStatus::PassOn;
                } else if status == BZ_OK {
                    break;
                }
            }
        }
        if let Some(c) = consumed {
            *c = total;
        }
        exit
    }

    fn seek(&mut self, _env: &mut FilterEnv, _offset: i64, _whence: i32) -> bool {
        self.end();
        self.state = State::Uninitialized;
        true
    }

    fn read_seekable(&self) -> Seekable {
        Seekable::Start
    }

    fn write_seekable(&self) -> Seekable {
        self.write_seekable
    }
}

/// The `bzip2.compress` filter.
#[derive(Debug)]
pub struct CompressFilter {
    strm: RawStream,
    block_size: i32,
    work_factor: i32,
    is_flushed: bool,
    outbuf: Vec<u8>,
    write_seekable: Seekable,
}

impl CompressFilter {
    /// Creates the filter; `None` if libbz2 rejects the parameters.
    pub fn new(block_size: i32, work_factor: i32, write_seekable: Seekable) -> Option<Self> {
        let strm = RawStream::compress_init(block_size, work_factor, Heap::Request).ok()?;
        Some(CompressFilter {
            strm,
            block_size,
            work_factor,
            is_flushed: true,
            outbuf: vec![0; BUF_LEN],
            write_seekable,
        })
    }

    fn compress(&mut self, input: &[u8], action: i32) -> (i32, usize, usize) {
        let st = self.strm.compress(input, &mut self.outbuf, action);
        (st.status, st.consumed, st.produced)
    }
}

impl StreamFilter for CompressFilter {
    fn label(&self) -> &'static str {
        "bzip2.compress"
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
        let action = match flags {
            FilterFlags::FlushClose => BZ_FINISH,
            FilterFlags::FlushInc => BZ_FLUSH,
            FilterFlags::Normal => BZ_RUN,
        };
        while let Some(bucket) = input.pop_front() {
            let data = bucket.data;
            let mut bin = 0usize;
            while bin < data.len() {
                let desired = (data.len() - bin).min(BUF_LEN);
                self.is_flushed = action != BZ_RUN;
                let (status, used, produced) = self.compress(&data[bin..bin + desired], action);
                if status != BZ_RUN_OK && status != BZ_FLUSH_OK && status != BZ_FINISH_OK {
                    return FilterStatus::ErrFatal;
                }
                total += used;
                bin += used;
                if emit(&self.outbuf, produced, output) {
                    exit = FilterStatus::PassOn;
                }
            }
        }
        if flags.is_closing() || (flags == FilterFlags::FlushInc && !self.is_flushed) {
            let (act, again) = if flags.is_closing() {
                (BZ_FINISH, BZ_FINISH_OK)
            } else {
                (BZ_FLUSH, BZ_FLUSH_OK)
            };
            loop {
                let (status, _, produced) = self.compress(&[], act);
                self.is_flushed = true;
                if emit(&self.outbuf, produced, output) {
                    exit = FilterStatus::PassOn;
                }
                if status != again {
                    break;
                }
            }
        }
        if let Some(c) = consumed {
            *c = total;
        }
        exit
    }

    fn seek(&mut self, _env: &mut FilterEnv, _offset: i64, _whence: i32) -> bool {
        self.strm.end();
        self.is_flushed = true;
        if let Ok(s) = RawStream::compress_init(self.block_size, self.work_factor, Heap::Request) {
            self.strm = s;
            true
        } else {
            diag::warning("bzip2.compress: failed to reset compression state");
            false
        }
    }

    fn read_seekable(&self) -> Seekable {
        Seekable::Start
    }

    fn write_seekable(&self) -> Seekable {
        self.write_seekable
    }
}

/// The `bzip2.*` factory (`php_bz2_filter_create()`).
///
/// An object parameter is reported with PHP's deprecation and, because
/// [`Value::Object`] carries no properties, treated as an empty table; the
/// VM should pass objects' properties as an array to get PHP's behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bz2FilterFactory;

fn deprecate_object(name: &str) {
    diag::emit(Diagnostic::new(
        Severity::Deprecated,
        format!(
            "Passing an object for filter parameters for {name} is deprecated, call get_object_vars() first instead"
        ),
    ));
}

fn table(params: &Value) -> &[(php_streams::value::Key, Value)] {
    params.as_array().unwrap_or(&[])
}

fn create_decompress(params: Option<&Value>) -> Option<Box<dyn StreamFilter>> {
    let mut write_seekable = Seekable::Always;
    let mut small = false;
    let mut concatenated = false;
    if let Some(p) = params {
        match p {
            Value::Bool(b) => small = *b,
            Value::Array(_) | Value::Object { .. } => {
                if matches!(p, Value::Object { .. }) {
                    deprecate_object("bzip2.decompress");
                }
                write_seekable = parse_write_seek_mode(Some(p))?;
                if let Some(v) = find(table(p), "concatenated") {
                    concatenated = v.is_true();
                }
                if let Some(v) = find(table(p), "small") {
                    small = v.is_true();
                }
            }
            other => {
                diag::warning(format!(
                    "Filter parameters for bzip2.decompress filter must be of type array|object|bool, {} given",
                    type_name(other)
                ));
                return None;
            }
        }
    }
    Some(Box::new(DecompressFilter::new(
        small,
        concatenated,
        write_seekable,
    )))
}

fn int_param(
    params: &Value,
    key: &str,
    type_msg: &str,
    range: std::ops::RangeInclusive<i64>,
    range_msg: &str,
) -> Result<Option<i32>, ()> {
    let Some(v) = find(table(params), key) else {
        return Ok(None);
    };
    let Some(n) = try_get_long(v) else {
        diag::warning(format!(
            "{type_msg} must be of type int, {} given",
            type_name(v)
        ));
        return Err(());
    };
    if !range.contains(&n) {
        diag::warning(format!("{range_msg}, {n} given"));
        return Err(());
    }
    Ok(i32::try_from(n).ok())
}

fn create_compress(params: Option<&Value>) -> Option<Box<dyn StreamFilter>> {
    let mut write_seekable = Seekable::Always;
    let mut block_size = DEFAULT_BLOCKSIZE;
    let mut work_factor = DEFAULT_WORKFACTOR;
    if let Some(p) = params {
        if !matches!(p, Value::Array(_) | Value::Object { .. }) {
            diag::warning(format!(
                "Filter parameters for bzip2.compress filter must be of type array|object, {} given",
                type_name(p)
            ));
            return None;
        }
        if matches!(p, Value::Object { .. }) {
            deprecate_object("bzip2.compress");
        }
        write_seekable = parse_write_seek_mode(Some(p))?;
        if let Some(b) = int_param(
            p,
            "blocks",
            "Number of blocks parameter",
            1..=9,
            "Number of blocks to allocate must be between 1 and 9",
        )
        .ok()?
        {
            block_size = b;
        }
        if let Some(w) = int_param(
            p,
            "work",
            "Work factor parameter",
            0..=250,
            "Work factor must be between 0 and 250",
        )
        .ok()?
        {
            work_factor = w;
        }
    }
    CompressFilter::new(block_size, work_factor, write_seekable)
        .map(|f| Box::new(f) as Box<dyn StreamFilter>)
}

impl FilterFactory for Bz2FilterFactory {
    fn create(
        &self,
        name: &str,
        params: Option<&Value>,
        _persistent: bool,
    ) -> Option<Box<dyn StreamFilter>> {
        if name.eq_ignore_ascii_case("bzip2.decompress") {
            create_decompress(params)
        } else if name.eq_ignore_ascii_case("bzip2.compress") {
            create_compress(params)
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

    const TEXT: &[u8] = b"I am the very model of a modern major general, I've information vegetable, animal, and mineral.";
    const B64: &str = "QlpoNDFBWSZTWRN6QG0AAAoVgECFACA395UgIABIintI1N6mpowIQ0E1MTTAQGYTNcRyMZm5kgW3ib7hVboE7Tmqj3ToGZ5G3q1ZauD2G58hibSck8KS95EEAbx1Cn+LuSKcKEgJvSA2gA==";

    fn b64(s: &str) -> Vec<u8> {
        let alpha = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0;
        for c in s.bytes().filter(|&c| c != b'=') {
            let v = u32::try_from(alpha.iter().position(|&a| a == c).unwrap()).unwrap();
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((acc >> bits) & 0xff).unwrap());
            }
        }
        out
    }

    fn run(
        f: &mut dyn StreamFilter,
        chunks: &[&[u8]],
        flags: FilterFlags,
    ) -> (FilterStatus, Vec<u8>) {
        let mut env = FilterEnv::detached(ChainKind::Write);
        let mut input: Brigade = chunks.iter().map(|c| Bucket::new(*c)).collect();
        let mut out = Brigade::new();
        let st = f.filter(&mut env, &mut input, &mut out, None, flags);
        (st, out.into_iter().flat_map(|b| b.data).collect())
    }

    fn create(name: &str, params: Option<&Value>) -> Option<Box<dyn StreamFilter>> {
        Bz2FilterFactory.create(name, params, false)
    }

    #[test]
    fn compress_matches_phpt() {
        // bz2_filter_compress.phpt: fwrite + fclose.
        let mut c = create("bzip2.compress", None).unwrap();
        let (st, a) = run(c.as_mut(), &[TEXT], FilterFlags::Normal);
        assert_eq!(st, FilterStatus::FeedMe);
        let (_, b) = run(c.as_mut(), &[], FilterFlags::FlushClose);
        assert_eq!([a, b].concat(), b64(B64));
    }

    #[test]
    fn decompress_matches_phpt() {
        let mut d = create("bzip2.decompress", None).unwrap();
        let data = b64(B64);
        let (_, out) = run(d.as_mut(), &[&data[..30], &data[30..]], FilterFlags::Normal);
        assert_eq!(out, TEXT);
    }

    #[test]
    fn bug75776_prefix() {
        let text = b"0123456789abcdef".repeat(1000);
        let mut c = create("bzip2.compress", None).unwrap();
        let (st, out) = run(c.as_mut(), &[&text], FilterFlags::Normal);
        assert_eq!((st, out.len()), (FilterStatus::FeedMe, 0));
        // rewind() flushes the write chain incrementally (BZ_FLUSH).
        let (_, out) = run(c.as_mut(), &[], FilterFlags::FlushInc);
        let hex = out.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        assert_eq!(hex, "425a68343141592653599fe7bbbf0001f389007fe03f002000902980026826aa80003ea9061520c6a41954833a9069520d6a41b54837a9071520e6a41d5483ba9079520f6a41f548");
    }

    #[test]
    fn concatenated_streams() {
        let crate::Output::Data(one) = crate::bzcompress(b"first ", 4, 0).unwrap() else {
            unreachable!()
        };
        let crate::Output::Data(two) = crate::bzcompress(b"second", 4, 0).unwrap() else {
            unreachable!()
        };
        let both = [one, two].concat();
        let p = Value::Array(vec![(Key::str("concatenated"), Value::Bool(true))]);
        let mut d = create("bzip2.decompress", Some(&p)).unwrap();
        assert_eq!(
            run(d.as_mut(), &[&both], FilterFlags::Normal).1,
            b"first second"
        );
        let mut d = create("bzip2.decompress", Some(&Value::Bool(true))).unwrap();
        assert_eq!(run(d.as_mut(), &[&both], FilterFlags::Normal).1, b"first ");
    }

    #[test]
    fn corrupt_and_truncated() {
        let _ = diag::take();
        let plain = b"The quick brown fox jumps over the lazy dog.";
        let crate::Output::Data(c) = crate::bzcompress(plain, 4, 0).unwrap() else {
            unreachable!()
        };
        assert_eq!(c.len(), 81);
        let mut bad = c.clone();
        bad[81 - 15] = b'X';
        let mut d = create("bzip2.decompress", None).unwrap();
        assert_eq!(
            run(d.as_mut(), &[&bad], FilterFlags::Normal).0,
            FilterStatus::ErrFatal
        );
        assert_eq!(diag::take()[0].text(), "bzip2 decompression failed");
        let mut d = create("bzip2.decompress", None).unwrap();
        let (st, out) = run(d.as_mut(), &[&c[..61]], FilterFlags::FlushClose);
        assert_eq!(st, FilterStatus::FeedMe);
        assert!(out.is_empty());
    }

    #[test]
    fn param_errors() {
        let _ = diag::take();
        let cases: Vec<(Value, &str)> = vec![
            (Value::str("not an array"), "Filter parameters for bzip2.compress filter must be of type array|object, string given"),
            (Value::Array(vec![(Key::str("blocks"), Value::str("not an int"))]), "Number of blocks parameter must be of type int, string given"),
            (Value::Array(vec![(Key::str("blocks"), Value::str("15"))]), "Number of blocks to allocate must be between 1 and 9, 15 given"),
            (Value::Array(vec![(Key::str("blocks"), Value::str("0"))]), "Number of blocks to allocate must be between 1 and 9, 0 given"),
            (Value::Array(vec![(Key::str("work"), Value::str("not an int"))]), "Work factor parameter must be of type int, string given"),
            (Value::Array(vec![(Key::str("work"), Value::str("251"))]), "Work factor must be between 0 and 250, 251 given"),
            (Value::Array(vec![(Key::str("work"), Value::str("-1"))]), "Work factor must be between 0 and 250, -1 given"),
            (Value::Array(vec![(Key::str("write_seek_mode"), Value::str("nope"))]), "\"write_seek_mode\" filter parameter must be one of \"preserve\", \"reset\", or \"strict\""),
        ];
        for (p, msg) in cases {
            assert!(create("bzip2.compress", Some(&p)).is_none());
            assert_eq!(diag::take()[0].text(), msg);
        }
        assert!(create("bzip2.decompress", Some(&Value::str("x"))).is_none());
        assert_eq!(
            diag::take()[0].text(),
            "Filter parameters for bzip2.decompress filter must be of type array|object|bool, string given"
        );
        assert!(create("bzip2.i_dont_exist", None).is_none());
        let obj = Value::Object {
            class: "ParamsCompress".into(),
            string_value: None,
        };
        assert!(create("bzip2.compress", Some(&obj)).is_some());
        assert!(create("bzip2.decompress", Some(&obj)).is_some());
        let d: Vec<String> = diag::take().iter().map(Diagnostic::text).collect();
        assert_eq!(d, [
            "Passing an object for filter parameters for bzip2.compress is deprecated, call get_object_vars() first instead",
            "Passing an object for filter parameters for bzip2.decompress is deprecated, call get_object_vars() first instead",
        ]);
        let p = Value::Array(vec![(Key::str("write_seek_mode"), Value::str("strict"))]);
        assert_eq!(
            create("bzip2.compress", Some(&p)).unwrap().write_seekable(),
            Seekable::Never
        );
    }

    #[test]
    fn seek_and_chain() {
        let mut env = FilterEnv::detached(ChainKind::Read);
        let mut d = create("bzip2.decompress", None).unwrap();
        let data = b64(B64);
        assert_eq!(
            &run(d.as_mut(), &[&data], FilterFlags::Normal).1[..20],
            b"I am the very model "
        );
        assert!(d.seek(&mut env, 0, 0));
        assert_eq!(run(d.as_mut(), &[&data], FilterFlags::Normal).1, TEXT);
        let mut c = create("bzip2.compress", None).unwrap();
        run(c.as_mut(), &[b"discard me"], FilterFlags::Normal);
        assert!(c.seek(&mut env, 0, 0));
        let (_, a) = run(c.as_mut(), &[TEXT], FilterFlags::Normal);
        let (_, b) = run(c.as_mut(), &[], FilterFlags::FlushClose);
        assert_eq!([a, b].concat(), b64(B64));

        let mut reg = FilterRegistry::new();
        assert!(register_filters(&mut reg));
        let mut chain = FilterChain::default();
        chain.push_back(reg.create("bzip2.compress", None, false).unwrap());
        chain.push_back(reg.create("bzip2.decompress", None, false).unwrap());
        let mut env = FilterEnv::detached(ChainKind::Write);
        // fwrite() (normal), then fclose() (closing flush with no data).
        let (st, out) = chain.run(
            0,
            &mut env,
            Brigade::from(vec![Bucket::new("Hello world, hopefully not broken\n")]),
            None,
            FilterFlags::Normal,
        );
        assert_eq!((st, out.len()), (FilterStatus::FeedMe, 0));
        let (st, out) = chain.run(0, &mut env, Brigade::new(), None, FilterFlags::FlushClose);
        assert_eq!(st, FilterStatus::PassOn);
        assert_eq!(
            out.into_iter().flat_map(|b| b.data).collect::<Vec<_>>(),
            b"Hello world, hopefully not broken\n"
        );
        // Like PHP, data arriving together with the closing flush makes
        // BZ2_bzCompress() return BZ_STREAM_END, which the filter treats as
        // a failure.
        let mut c = create("bzip2.compress", None).unwrap();
        assert_eq!(
            run(c.as_mut(), &[b"x"], FilterFlags::FlushClose).0,
            FilterStatus::ErrFatal
        );
    }

    #[test]
    fn fflush_then_close() {
        // bug81092: fwrite + fflush + stream_filter_remove.
        let data: Vec<u8> = (0..8192u32)
            .map(|i| u8::try_from(i * 7 % 251).unwrap())
            .collect();
        let p = Value::Array(vec![
            (Key::str("blocks"), Value::Int(9)),
            (Key::str("work"), Value::Int(0)),
        ]);
        let mut c = create("bzip2.compress", Some(&p)).unwrap();
        let mut all = run(c.as_mut(), &[&data], FilterFlags::Normal).1;
        all.extend(run(c.as_mut(), &[], FilterFlags::FlushInc).1);
        all.extend(run(c.as_mut(), &[], FilterFlags::FlushClose).1);
        let crate::Output::Data(back) = crate::bzdecompress(&all, false).unwrap() else {
            panic!()
        };
        assert_eq!(back, data);
    }
}
