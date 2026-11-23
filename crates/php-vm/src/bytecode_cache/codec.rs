// buzzs-rust-php-project - Copyright (c) 2026 git001 (Aleks)
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The byte-level codec of the bytecode cache: a [`Writer`] that appends
//! little-endian primitives and collects every string into one string
//! table, a [`Reader`] that reads them back with bounds checks, and the
//! [`Codec`] trait for the value types of the compiled graph.
//!
//! Strings are written once (`zend_file_cache_serialize_interned`): a
//! [`ZStr`] is stored as an index into the string table, together with
//! its interned and valid-UTF-8 flags, which are observable from PHP
//! (`debug_zval_dump()` refcounts, the runtime caches' immutability
//! checks), so a decoded string is interned again exactly when the
//! compiler had interned it.
//!
//! ref: `ext/opcache/zend_file_cache.c` (`zend_file_cache_serialize_zval`,
//! `zend_file_cache_serialize_interned`, `zend_file_cache_serialize_hash`)

use php_runtime::{ArrayKey, BinaryOp, Value, ZArray, ZStr};
use std::collections::HashMap;
use std::fmt;

/// Why a cached image could not be decoded. Any decode error makes the
/// cache entry unusable; the caller compiles the script instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "corrupted bytecode cache entry: {}", self.0)
    }
}

/// Result of a decode step.
pub type DResult<T> = Result<T, DecodeError>;

/// Why a compiled unit could not be encoded: it holds a value that has no
/// compile-time form (an object, a resource or a reference). Such a unit
/// is simply not cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError(pub &'static str);

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot cache compiled script: {}", self.0)
    }
}

/// Result of an encode step.
pub type EResult<T = ()> = Result<T, EncodeError>;

/// String flag: the string was interned.
const STR_INTERNED: u8 = 1;
/// String flag: the string carries `IS_STR_VALID_UTF8`.
const STR_VALID_UTF8: u8 = 2;

/// Appends the encoded form of a compiled graph.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
    /// `(bytes, flags)` of every distinct string, in first-use order.
    strings: Vec<(Vec<u8>, u8)>,
    string_ids: HashMap<(Vec<u8>, u8), u32>,
}

impl Writer {
    /// A new, empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The body written so far.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.buf
    }

    /// Splits the writer into its string table (encoded) and its body.
    #[must_use]
    pub fn finish(self) -> (Vec<u8>, Vec<u8>) {
        let mut table = Writer::new();
        table.count(self.strings.len());
        for (bytes, flags) in &self.strings {
            table.u8(*flags);
            table.bytes(bytes);
        }
        (table.buf, self.buf)
    }

    /// One byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// A bool as one byte.
    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    /// A little-endian `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// A little-endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// A little-endian `i64`.
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// An `f64` by its bit pattern (keeps `-0.0` and NaN payloads).
    pub fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }

    /// A length or index; lengths above `u32::MAX` cannot occur in a
    /// compiled script (op arrays and tables are indexed by `u32`).
    pub fn count(&mut self, n: usize) {
        self.u64(n as u64);
    }

    /// Raw bytes with their length.
    pub fn bytes(&mut self, b: &[u8]) {
        self.count(b.len());
        self.buf.extend_from_slice(b);
    }

    /// A Rust string (warnings, default texts).
    pub fn string(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// A [`ZStr`] as an index into the string table.
    pub fn zstr(&mut self, s: &ZStr) {
        let mut flags = 0;
        if s.is_interned() {
            flags |= STR_INTERNED;
        }
        if s.is_valid_utf8() {
            flags |= STR_VALID_UTF8;
        }
        let key = (s.as_bytes().to_vec(), flags);
        let next = u32::try_from(self.strings.len()).unwrap_or(u32::MAX);
        let id = *self.string_ids.entry(key).or_insert_with_key(|k| {
            self.strings.push(k.clone());
            next
        });
        self.u32(id);
    }
}

/// Reads an encoded graph back.
#[derive(Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    strings: Vec<ZStr>,
}

impl<'a> Reader<'a> {
    /// A reader over `body`, resolving strings from `string_table` (the
    /// first half of [`Writer::finish`]).
    ///
    /// # Errors
    /// When the string table is truncated or malformed.
    pub fn new(string_table: &[u8], body: &'a [u8]) -> DResult<Self> {
        let mut table = Reader {
            data: string_table,
            pos: 0,
            strings: Vec::new(),
        };
        let n = table.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut strings = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let flags = table.u8()?;
            let bytes = table.bytes()?;
            let s = if flags & STR_INTERNED != 0 {
                ZStr::intern(bytes)
            } else {
                ZStr::from(bytes)
            };
            if flags & STR_VALID_UTF8 != 0 {
                s.mark_valid_utf8();
            }
            strings.push(s);
        }
        table.expect_end()?;
        Ok(Reader {
            data: body,
            pos: 0,
            strings,
        })
    }

    /// Fails unless every byte was consumed.
    ///
    /// # Errors
    /// When trailing bytes remain.
    pub fn expect_end(&self) -> DResult<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(DecodeError("trailing bytes"))
        }
    }

    fn take(&mut self, n: usize) -> DResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(DecodeError("truncated"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> DResult<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// One byte.
    ///
    /// # Errors
    /// When the input is exhausted.
    pub fn u8(&mut self) -> DResult<u8> {
        Ok(self.take(1)?[0])
    }

    /// A bool; any byte other than 0 or 1 is corruption.
    ///
    /// # Errors
    /// When the input is exhausted or the byte is not 0 or 1.
    pub fn bool(&mut self) -> DResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("invalid bool")),
        }
    }

    /// A little-endian `u32`.
    ///
    /// # Errors
    /// When the input is exhausted.
    pub fn u32(&mut self) -> DResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// A little-endian `u64`.
    ///
    /// # Errors
    /// When the input is exhausted.
    pub fn u64(&mut self) -> DResult<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A little-endian `i64`.
    ///
    /// # Errors
    /// When the input is exhausted.
    pub fn i64(&mut self) -> DResult<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// An `f64` from its bit pattern.
    ///
    /// # Errors
    /// When the input is exhausted.
    pub fn f64(&mut self) -> DResult<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    /// A length; bounded by the remaining input so a corrupted length
    /// cannot trigger a huge allocation.
    ///
    /// # Errors
    /// When the input is exhausted or the length exceeds the input.
    pub fn count(&mut self) -> DResult<usize> {
        let n = usize::try_from(self.u64()?).map_err(|_| DecodeError("length overflow"))?;
        if n > self.data.len() - self.pos {
            return Err(DecodeError("length exceeds input"));
        }
        Ok(n)
    }

    /// Raw bytes with their length.
    ///
    /// # Errors
    /// When the input is truncated.
    pub fn bytes(&mut self) -> DResult<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }

    /// A Rust string.
    ///
    /// # Errors
    /// When the input is truncated or not UTF-8.
    pub fn string(&mut self) -> DResult<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| DecodeError("invalid UTF-8"))
    }

    /// A [`ZStr`] from the string table.
    ///
    /// # Errors
    /// When the index is out of range.
    pub fn zstr(&mut self) -> DResult<ZStr> {
        let id = self.u32()? as usize;
        self.strings
            .get(id)
            .cloned()
            .ok_or(DecodeError("string index out of range"))
    }
}

/// A type of the compiled graph that has a cache encoding. Types holding
/// shared `Rc`s (functions, op arrays, classes, declaration tables) are
/// encoded by the graph encoder instead, which keeps their identity.
pub trait Codec: Sized {
    /// Appends `self`.
    ///
    /// # Errors
    /// When `self` holds something without a compile-time form.
    fn encode(&self, w: &mut Writer) -> EResult;

    /// Reads a value written by [`Codec::encode`].
    ///
    /// # Errors
    /// When the input is truncated or malformed.
    fn decode(r: &mut Reader<'_>) -> DResult<Self>;
}

impl Codec for u8 {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.u8(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.u8()
    }
}

impl Codec for bool {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.bool(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.bool()
    }
}

impl Codec for u32 {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.u32(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.u32()
    }
}

impl Codec for i64 {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.i64(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.i64()
    }
}

impl Codec for f64 {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.f64(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.f64()
    }
}

impl Codec for usize {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.count(*self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        usize::try_from(r.u64()?).map_err(|_| DecodeError("usize overflow"))
    }
}

impl Codec for String {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.string(self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.string()
    }
}

impl Codec for ZStr {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.zstr(self);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        r.zstr()
    }
}

impl<T: Codec> Codec for Option<T> {
    fn encode(&self, w: &mut Writer) -> EResult {
        match self {
            None => {
                w.u8(0);
                Ok(())
            }
            Some(v) => {
                w.u8(1);
                v.encode(w)
            }
        }
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(r)?)),
            _ => Err(DecodeError("invalid option tag")),
        }
    }
}

impl<T: Codec> Codec for Vec<T> {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.count(self.len());
        self.iter().try_for_each(|v| v.encode(w))
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        let n = r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(T::decode(r)?);
        }
        Ok(out)
    }
}

impl<T: Codec> Codec for Box<[T]> {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.count(self.len());
        self.iter().try_for_each(|v| v.encode(w))
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(Vec::<T>::decode(r)?.into_boxed_slice())
    }
}

impl<T: Codec> Codec for Box<T> {
    fn encode(&self, w: &mut Writer) -> EResult {
        (**self).encode(w)
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(Box::new(T::decode(r)?))
    }
}

impl<T: Codec, const N: usize> Codec for [T; N] {
    fn encode(&self, w: &mut Writer) -> EResult {
        self.iter().try_for_each(|v| v.encode(w))
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            items.push(T::decode(r)?);
        }
        items
            .try_into()
            .map_err(|_| DecodeError("array length mismatch"))
    }
}

impl<A: Codec, B: Codec> Codec for (A, B) {
    fn encode(&self, w: &mut Writer) -> EResult {
        self.0.encode(w)?;
        self.1.encode(w)
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok((A::decode(r)?, B::decode(r)?))
    }
}

impl<A: Codec, B: Codec, C: Codec> Codec for (A, B, C) {
    fn encode(&self, w: &mut Writer) -> EResult {
        self.0.encode(w)?;
        self.1.encode(w)?;
        self.2.encode(w)
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok((A::decode(r)?, B::decode(r)?, C::decode(r)?))
    }
}

/// Every [`BinaryOp`], in tag order. Decoding maps a tag back through
/// this table, so adding an operator only appends here.
const BINARY_OPS: [BinaryOp; 15] = [
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Mod,
    BinaryOp::Pow,
    BinaryOp::ShiftLeft,
    BinaryOp::ShiftRight,
    BinaryOp::BitOr,
    BinaryOp::BitAnd,
    BinaryOp::BitXor,
    BinaryOp::Concat,
    BinaryOp::BoolXor,
    BinaryOp::BitNot,
    BinaryOp::BoolNot,
];

impl Codec for BinaryOp {
    fn encode(&self, w: &mut Writer) -> EResult {
        // Exhaustive on purpose: a new operator fails to compile here
        // until it gets a tag (appended to `BINARY_OPS`).
        let tag: u8 = match self {
            BinaryOp::Add => 0,
            BinaryOp::Sub => 1,
            BinaryOp::Mul => 2,
            BinaryOp::Div => 3,
            BinaryOp::Mod => 4,
            BinaryOp::Pow => 5,
            BinaryOp::ShiftLeft => 6,
            BinaryOp::ShiftRight => 7,
            BinaryOp::BitOr => 8,
            BinaryOp::BitAnd => 9,
            BinaryOp::BitXor => 10,
            BinaryOp::Concat => 11,
            BinaryOp::BoolXor => 12,
            BinaryOp::BitNot => 13,
            BinaryOp::BoolNot => 14,
        };
        debug_assert_eq!(BINARY_OPS[usize::from(tag)], *self);
        w.u8(tag);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        BINARY_OPS
            .get(usize::from(r.u8()?))
            .copied()
            .ok_or(DecodeError("invalid binary operator"))
    }
}

/// Value tags. Objects, resources and references have no compile-time
/// form and are rejected by the encoder.
const V_NULL: u8 = 0;
const V_FALSE: u8 = 1;
const V_TRUE: u8 = 2;
const V_INT: u8 = 3;
const V_FLOAT: u8 = 4;
const V_STR: u8 = 5;
const V_ARRAY: u8 = 6;

impl Codec for Value {
    fn encode(&self, w: &mut Writer) -> EResult {
        match self {
            Value::Null => w.u8(V_NULL),
            Value::Bool(false) => w.u8(V_FALSE),
            Value::Bool(true) => w.u8(V_TRUE),
            Value::Int(i) => {
                w.u8(V_INT);
                w.i64(*i);
            }
            Value::Float(f) => {
                w.u8(V_FLOAT);
                w.f64(*f);
            }
            Value::Str(s) => {
                w.u8(V_STR);
                w.zstr(s);
            }
            Value::Array(a) => {
                w.u8(V_ARRAY);
                encode_array(a, w)?;
            }
            Value::Object(_) => return Err(EncodeError("object in compiled data")),
            Value::Resource(_) => return Err(EncodeError("resource in compiled data")),
            Value::Reference(_) => return Err(EncodeError("reference in compiled data")),
        }
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(match r.u8()? {
            V_NULL => Value::Null,
            V_FALSE => Value::Bool(false),
            V_TRUE => Value::Bool(true),
            V_INT => Value::Int(r.i64()?),
            V_FLOAT => Value::Float(r.f64()?),
            V_STR => Value::Str(r.zstr()?),
            V_ARRAY => Value::Array(decode_array(r)?),
            _ => return Err(DecodeError("invalid value tag")),
        })
    }
}

/// Key tags of [`ArrayKey`].
const K_INT: u8 = 0;
const K_STR: u8 = 1;

impl Codec for ArrayKey {
    fn encode(&self, w: &mut Writer) -> EResult {
        match self {
            ArrayKey::Int(i) => {
                w.u8(K_INT);
                w.i64(*i);
            }
            ArrayKey::Str(s) => {
                w.u8(K_STR);
                w.zstr(s);
            }
        }
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        match r.u8()? {
            K_INT => Ok(ArrayKey::Int(r.i64()?)),
            K_STR => Ok(ArrayKey::Str(r.zstr()?)),
            _ => Err(DecodeError("invalid array key tag")),
        }
    }
}

/// A constant array: its packed flag, next free index and entries in
/// order (`zend_file_cache_serialize_hash`).
fn encode_array(a: &ZArray, w: &mut Writer) -> EResult {
    w.bool(a.is_packed());
    w.i64(a.next_free_element());
    w.count(a.len());
    for (k, v) in a.iter() {
        k.encode(w)?;
        v.encode(w)?;
    }
    Ok(())
}

/// Rebuilds a constant array: a packed array by appending (so it stays
/// packed), a hash by inserting in order.
fn decode_array(r: &mut Reader<'_>) -> DResult<ZArray> {
    let packed = r.bool()?;
    let next_free = r.i64()?;
    let count = r.count()?;
    let mut array = ZArray::with_capacity(count);
    {
        let table = array.make_mut();
        for _ in 0..count {
            let key = ArrayKey::decode(r)?;
            let value = Value::decode(r)?;
            match key {
                ArrayKey::Int(i) if packed && i == table.next_index() => {
                    table
                        .push(value)
                        .map_err(|_| DecodeError("packed array append"))?;
                }
                key => {
                    table.insert(key, value);
                }
            }
        }
    }
    if array.is_packed() != packed || (count > 0 && array.next_free_element() != next_free) {
        return Err(DecodeError("array layout mismatch"));
    }
    Ok(array)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Codec>(v: &T) -> T {
        let mut w = Writer::new();
        v.encode(&mut w).expect("encode");
        let (table, body) = w.finish();
        let mut r = Reader::new(&table, &body).expect("table");
        let out = T::decode(&mut r).expect("decode");
        r.expect_end().expect("consumed");
        out
    }

    #[test]
    fn primitives_and_containers_roundtrip() {
        assert_eq!(roundtrip(&0xdead_beef_u32), 0xdead_beef);
        assert_eq!(roundtrip(&-42_i64), -42);
        assert_eq!(roundtrip(&(-0.0_f64)).to_bits(), (-0.0_f64).to_bits());
        assert!(roundtrip(&f64::NAN).is_nan());
        assert_eq!(roundtrip(&Some(7_u32)), Some(7));
        assert_eq!(roundtrip(&Option::<u32>::None), None);
        assert_eq!(roundtrip(&vec![1_u32, 2, 3]), vec![1, 2, 3]);
        assert_eq!(roundtrip(&[1_u32, 2, 3]), [1, 2, 3]);
        assert_eq!(roundtrip(&String::from("äöü")), "äöü");
        assert_eq!(roundtrip(&(1_u32, true)), (1, true));
    }

    #[test]
    fn strings_keep_bytes_and_interned_flag_and_are_shared() {
        let plain = ZStr::from(&b"plain\xff"[..]);
        let interned = ZStr::intern(b"interned_name");
        let mut w = Writer::new();
        plain.encode(&mut w).expect("encode");
        interned.encode(&mut w).expect("encode");
        interned.encode(&mut w).expect("encode");
        let (table, body) = w.finish();
        let mut r = Reader::new(&table, &body).expect("table");
        let p = r.zstr().expect("plain");
        let i1 = r.zstr().expect("interned");
        let i2 = r.zstr().expect("interned again");
        assert_eq!(p.as_bytes(), b"plain\xff");
        assert!(!p.is_interned());
        assert_eq!(i1.as_bytes(), b"interned_name");
        assert!(i1.is_interned());
        assert_eq!(i2.as_bytes(), b"interned_name");
    }

    #[test]
    fn values_and_nested_arrays_roundtrip() {
        let mut inner = ZArray::new();
        inner.make_mut().push(Value::Int(1)).expect("push");
        inner
            .make_mut()
            .push(Value::Str(ZStr::from("x")))
            .expect("push");
        let mut outer = ZArray::new();
        outer
            .make_mut()
            .insert(ZStr::from("k"), Value::Array(inner.clone()));
        outer.make_mut().insert(7_i64, Value::Float(1.5));
        outer.make_mut().insert(ZStr::from("n"), Value::Null);
        let v = roundtrip(&Value::Array(outer.clone()));
        let Value::Array(back) = v else {
            panic!("not an array")
        };
        assert!(!back.is_packed());
        assert_eq!(back.len(), 3);
        let keys: Vec<String> = back
            .iter()
            .map(|(k, _)| match k {
                ArrayKey::Int(i) => i.to_string(),
                ArrayKey::Str(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
            })
            .collect();
        assert_eq!(keys, ["k", "7", "n"]);
        assert_eq!(back.next_free_element(), outer.next_free_element());
        let Value::Array(inner_back) = roundtrip(&Value::Array(inner)) else {
            panic!("not an array")
        };
        assert!(inner_back.is_packed());
        assert_eq!(inner_back.len(), 2);
    }

    #[test]
    fn objects_cannot_be_encoded_and_garbage_cannot_be_decoded() {
        let mut w = Writer::new();
        assert!(Value::Int(1).encode(&mut w).is_ok());
        let (table, body) = w.finish();
        let truncated = &body[..body.len() - 1];
        let mut r = Reader::new(&table, truncated).expect("table");
        assert!(Value::decode(&mut r).is_err());
        let mut r = Reader::new(&table, &[99]).expect("table");
        assert!(Value::decode(&mut r).is_err());
        let huge_len = 0xffff_ffff_ffff_u64.to_le_bytes();
        assert!(Reader::new(&huge_len, &[]).is_err());
    }
}
