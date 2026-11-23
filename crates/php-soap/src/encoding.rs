// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! SOAP encoding (`php_encoding.c`): PHP values ⇄ XML for every XSD
//! built-in type, structs, SOAP arrays, Apache maps, `any`, `SoapVar`,
//! user `typemap` callbacks, multi-references and the SOAP 1.1/1.2,
//! rpc/document and encoded/literal variations.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use php_runtime::array::ArrayKey;
use php_runtime::numeric::{is_numeric_string, Number};
use php_runtime::{Obj, RuntimeError, Value, ZArray, ZStr};

use crate::bail;
use crate::charset::Charset;
use crate::consts::*;
use crate::encoders::{conv, default_by_type, default_ns_prefix, EncRef, Encoder, ToXml, ToZval};
use crate::error::{soap_fatal, SoapError, SoapResult};
use crate::host::{EnumKind, SoapHost};
use crate::sdl::*;
use crate::xml::{parse_qname, whitespace_collapse, whitespace_replace, Doc, Kind, NodeId, Ns};
use crate::zhash::ZHash;

/// The encoders of a `typemap` option (`soap_create_typemap`).
#[derive(Debug, Clone, Default)]
pub struct Typemap {
    /// `ns:type` (or `type`) → index into `encoders`.
    pub map: ZHash<u32>,
    /// The encoders.
    pub encoders: Vec<Encoder>,
}

impl Typemap {
    /// Looks an entry up.
    #[must_use]
    pub fn find(&self, nscat: &str) -> Option<EncRef> {
        self.map.get(nscat).map(|&i| EncRef::Map(i))
    }
}

#[derive(Debug, Default)]
struct RefMaps {
    /// Encoding: object handle → node that holds it.
    enc: HashMap<u32, NodeId>,
    /// Decoding: (document, node) → decoded value.
    dec: HashMap<(u64, NodeId), Value>,
}

/// Lower-case class comparison.
pub(crate) fn is_class(obj: &Obj, name: &str) -> bool {
    obj.class_name()
        .as_bytes()
        .eq_ignore_ascii_case(name.as_bytes())
}

/// `Z_TYPE_P` as used to index the encoder table.
pub(crate) fn type_code(v: &Value) -> i64 {
    match v {
        Value::Null => IS_NULL,
        Value::Bool(false) => IS_FALSE,
        Value::Bool(true) => IS_TRUE,
        Value::Int(_) => IS_LONG,
        Value::Float(_) => IS_DOUBLE,
        Value::Str(_) => IS_STRING,
        Value::Array(_) => IS_ARRAY,
        Value::Object(_) => IS_OBJECT,
        Value::Resource(_) => IS_RESOURCE,
        Value::Reference(r) => type_code(&r.get()),
    }
}

/// `zend_array_is_list`.
pub(crate) fn is_list(a: &ZArray) -> bool {
    a.is_list()
}

/// The unmangled name of a property-table key.
pub(crate) fn unmangle(key: &[u8]) -> &[u8] {
    php_runtime::dump::unmangle_property_name(key).1
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn str_of(v: Option<&Value>) -> Option<ZStr> {
    match v.map(Value::deref) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// The settings an [`EncodeCtx`] is created from.
#[derive(Debug, Clone, Default)]
pub struct CtxSettings {
    /// SDL.
    pub sdl: Option<Rc<Sdl>>,
    /// Typemap.
    pub typemap: Option<Rc<Typemap>>,
    /// Class map.
    pub class_map: Option<ZArray>,
    /// Features.
    pub features: i64,
    /// SOAP version.
    pub soap_version: i64,
    /// Charset.
    pub charset: Option<Charset>,
}

impl CtxSettings {
    /// Creates a context.
    pub fn make<'h>(&self, host: &'h mut dyn SoapHost) -> EncodeCtx<'h> {
        let mut ctx = EncodeCtx::new(host);
        ctx.sdl.clone_from(&self.sdl);
        ctx.typemap.clone_from(&self.typemap);
        ctx.class_map.clone_from(&self.class_map);
        ctx.features = self.features;
        ctx.soap_version = self.soap_version;
        ctx.charset = self.charset;
        ctx
    }
}

/// The encoding context (the `SOAP_GLOBAL(...)` state of one call).
pub struct EncodeCtx<'h> {
    /// The engine.
    pub host: &'h mut dyn SoapHost,
    /// `SOAP_GLOBAL(sdl)`.
    pub sdl: Option<Rc<Sdl>>,
    /// `SOAP_GLOBAL(typemap)`.
    pub typemap: Option<Rc<Typemap>>,
    /// `SOAP_GLOBAL(class_map)`.
    pub class_map: Option<ZArray>,
    /// `SOAP_GLOBAL(features)`.
    pub features: i64,
    /// `SOAP_GLOBAL(soap_version)`.
    pub soap_version: i64,
    /// `SOAP_GLOBAL(encoding)`.
    pub charset: Option<Charset>,
    refs: Option<RefMaps>,
    cur_uniq_ns: i64,
    cur_uniq_ref: i64,
    decode_depth: u32,
    protected: HashSet<usize>,
    /// The exception a `zend_value_error()` raised while encoding goes on
    /// (`EG(exception)`): the last one thrown wins, like the exception
    /// that ends up in `EG(exception)` when errors chain.
    pending_error: Option<RuntimeError>,
}

impl std::fmt::Debug for EncodeCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodeCtx")
            .field("soap_version", &self.soap_version)
            .field("features", &self.features)
            .finish_non_exhaustive()
    }
}

/// `php_base64_encode`.
#[must_use]
pub fn base64_encode(data: &[u8]) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(T[usize::from(b[0] >> 2)]);
        out.push(T[usize::from(((b[0] & 3) << 4) | (b[1] >> 4))]);
        out.push(if chunk.len() > 1 {
            T[usize::from(((b[1] & 0x0f) << 2) | (b[2] >> 6))]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            T[usize::from(b[2] & 0x3f)]
        } else {
            b'='
        });
    }
    out
}

/// `php_base64_decode` (non-strict: unknown characters are skipped).
#[must_use]
pub fn base64_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in data {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
        }
    }
    out
}

/// `xmlCheckUTF8` (up to the first NUL byte).
fn check_utf8(s: &[u8]) -> bool {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    loop {
        let c = at(i);
        if c == 0 {
            return true;
        }
        if c & 0x80 == 0 {
            i += 1;
        } else if c & 0xe0 == 0xc0 {
            if at(i + 1) & 0xc0 != 0x80 {
                return false;
            }
            i += 2;
        } else if c & 0xf0 == 0xe0 {
            if at(i + 1) & 0xc0 != 0x80 || at(i + 2) & 0xc0 != 0x80 {
                return false;
            }
            i += 3;
        } else if c & 0xf8 == 0xf0 {
            if at(i + 1) & 0xc0 != 0x80 || at(i + 2) & 0xc0 != 0x80 || at(i + 3) & 0xc0 != 0x80 {
                return false;
            }
            i += 4;
        } else {
            return false;
        }
    }
}

/// The excerpt shown by "string '…' is not a valid utf-8 string".
fn utf8_error_excerpt(s: &[u8]) -> String {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    let mut c;
    loop {
        c = at(i);
        i += 1;
        if c == 0 {
            break;
        }
        if c & 0x80 == 0 {
        } else if c & 0xe0 == 0xc0 {
            if at(i) & 0xc0 != 0x80 {
                break;
            }
            i += 1;
        } else if c & 0xf0 == 0xe0 {
            if at(i) & 0xc0 != 0x80 || at(i + 1) & 0xc0 != 0x80 {
                break;
            }
            i += 2;
        } else if c & 0xf8 == 0xf0 {
            if at(i) & 0xc0 != 0x80 || at(i + 1) & 0xc0 != 0x80 || at(i + 2) & 0xc0 != 0x80 {
                break;
            }
            i += 3;
        } else {
            break;
        }
    }
    let nul = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    let mut out: Vec<u8> = s[..nul.min(s.len())].to_vec();
    if c != 0 {
        out.truncate(i - 1);
        out.extend_from_slice(format!("\\x{c:02x}...").as_bytes());
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `calc_dimension_12`.
fn calc_dimension_12(s: &[u8]) -> SoapResult<usize> {
    let mut i = 0;
    let mut flag = false;
    let mut p = 0;
    while p < s.len() && !s[p].is_ascii_digit() && s[p] != b'*' {
        p += 1;
    }
    if p < s.len() && s[p] == b'*' {
        i += 1;
        p += 1;
    }
    while p < s.len() {
        if s[p].is_ascii_digit() {
            if !flag {
                i += 1;
                flag = true;
            }
        } else if s[p] == b'*' {
            bail!("Encoding: '*' may only be first arraySize value in list");
        } else {
            flag = false;
        }
        p += 1;
    }
    Ok(i)
}

fn add_digit(pos: &mut i32, digit: u8) -> SoapResult<()> {
    let d = i32::from(digit);
    if *pos > (i32::MAX - d) / 10 {
        bail!("Encoding: array index out of range");
    }
    *pos = *pos * 10 + d;
    Ok(())
}

/// `get_position_12`.
fn get_position_12(dimension: usize, s: &[u8]) -> SoapResult<Vec<i32>> {
    let mut pos = vec![0i32; dimension.max(1)];
    let mut i: isize = -1;
    let mut flag = false;
    let mut p = 0;
    while p < s.len() && !s[p].is_ascii_digit() && s[p] != b'*' {
        p += 1;
    }
    if p < s.len() && s[p] == b'*' {
        p += 1;
        i += 1;
    }
    while p < s.len() {
        if s[p].is_ascii_digit() {
            if !flag {
                i += 1;
                flag = true;
            }
            if let Some(slot) = usize::try_from(i).ok().and_then(|i| pos.get_mut(i)) {
                add_digit(slot, s[p] - b'0')?;
            }
        } else if s[p] == b'*' {
            bail!("Encoding: '*' may only be first arraySize value in list");
        } else {
            flag = false;
        }
        p += 1;
    }
    Ok(pos)
}

/// `calc_dimension`.
fn calc_dimension(s: &[u8]) -> usize {
    1 + s
        .iter()
        .take_while(|&&c| c != b']')
        .filter(|&&c| c == b',')
        .count()
}

/// `get_position_ex`.
fn get_position_ex(dimension: usize, s: &[u8], pos: &mut [i32]) -> SoapResult<()> {
    for p in pos.iter_mut() {
        *p = 0;
    }
    let mut i = 0;
    for &c in s {
        if c == b']' || i >= dimension {
            break;
        }
        if c.is_ascii_digit() {
            add_digit(&mut pos[i], c - b'0')?;
        } else if c == b',' {
            i += 1;
        }
    }
    Ok(())
}

fn insert_at(arr: &mut ZArray, pos: &[i32], val: Value) {
    let t = arr.make_mut();
    if pos.len() == 1 {
        t.insert(i64::from(pos[0]), val);
        return;
    }
    let slot = t.lookup(i64::from(pos[0]));
    if !matches!(slot, Value::Array(_)) {
        *slot = Value::Array(ZArray::new());
    }
    if let Value::Array(inner) = slot {
        insert_at(inner, &pos[1..], val);
    }
}

impl<'h> EncodeCtx<'h> {
    /// A context with the given engine and no SDL.
    pub fn new(host: &'h mut dyn SoapHost) -> Self {
        Self {
            host,
            sdl: None,
            typemap: None,
            class_map: None,
            features: 0,
            soap_version: SOAP_1_1,
            charset: None,
            refs: None,
            cur_uniq_ns: 0,
            cur_uniq_ref: 0,
            decode_depth: 0,
            protected: HashSet::new(),
            pending_error: None,
        }
    }

    /// Takes the exception left by a `zend_value_error()` that did not
    /// abort encoding (`EG(exception)` checked after a serializer ran).
    pub fn take_pending_error(&mut self) -> Option<RuntimeError> {
        self.pending_error.take()
    }

    /// `zend_value_error()` from a serialization helper: records the
    /// exception and lets the caller go on with a fallback value.
    #[cold]
    fn defer_value_error(&mut self, msg: &str) {
        self.pending_error = Some(RuntimeError::ValueError(msg.into()));
    }

    /// `encode_reset_ns`.
    pub fn reset_ns(&mut self) {
        self.cur_uniq_ns = 0;
        self.cur_uniq_ref = 0;
        self.decode_depth = 0;
        self.refs = Some(RefMaps::default());
    }

    /// `encode_finish`.
    pub fn finish(&mut self) {
        self.cur_uniq_ns = 0;
        self.cur_uniq_ref = 0;
        self.decode_depth = 0;
        self.refs = None;
    }

    /// Resolves an encoder reference.
    #[must_use]
    pub fn enc(&self, e: EncRef) -> Encoder {
        match e {
            EncRef::Map(i) => self
                .typemap
                .as_ref()
                .and_then(|t| t.encoders.get(i as usize).cloned())
                .unwrap_or_else(|| crate::encoders::DEFAULT_ENCODERS[0].to_encoder()),
            other => resolve_static(self.sdl.as_deref(), other)
                .unwrap_or_else(|| crate::encoders::DEFAULT_ENCODERS[0].to_encoder()),
        }
    }

    fn sdl_type_of(&self, e: EncRef) -> Option<TypeId> {
        self.enc(e).sdl_type
    }

    /// `get_conversion`.
    pub fn get_conversion(&self, type_id: i64) -> SoapResult<EncRef> {
        default_by_type(type_id)
            .map(EncRef::Default)
            .ok_or_else(|| soap_fatal("Encoding: Cannot find encoding"))
    }

    fn typemap_find(&self, nscat: &str) -> Option<EncRef> {
        self.typemap.as_ref().and_then(|t| t.find(nscat))
    }

    fn nscat_of(enc: &Encoder) -> Option<String> {
        enc.type_str.as_ref().map(|t| match &enc.ns {
            Some(ns) => format!("{ns}:{t}"),
            None => t.to_string(),
        })
    }

    // ---- namespaces and types on output nodes ----------------------------------

    /// `encode_add_ns`.
    pub fn encode_add_ns(&mut self, doc: &mut Doc, node: NodeId, ns: &[u8]) -> Option<Ns> {
        let mut found = doc.search_ns_by_href(node, ns);
        if found.as_ref().is_some_and(|n| n.prefix.is_none()) {
            found = doc.search_prefixed_ns_by_href(node, ns);
        }
        if found.is_some() {
            return found;
        }
        let root = doc.first_top()?;
        if let Some(prefix) = default_ns_prefix(ns) {
            return doc.new_ns(root, ns, Some(prefix.as_bytes()));
        }
        let prefix = loop {
            self.cur_uniq_ns += 1;
            let p = format!("ns{}", self.cur_uniq_ns);
            if doc.search_ns(node, Some(p.as_bytes())).is_none() {
                break p;
            }
        };
        doc.new_ns(root, ns, Some(prefix.as_bytes()))
    }

    fn set_ns_prop(&mut self, doc: &mut Doc, node: NodeId, ns: &str, name: &[u8], value: &[u8]) {
        let nsp = self.encode_add_ns(doc, node, ns.as_bytes());
        doc.set_ns_prop(node, nsp, name, value);
    }

    fn set_xsi_nil(&mut self, doc: &mut Doc, node: NodeId) {
        self.set_ns_prop(doc, node, XSI_NAMESPACE, b"nil", b"true");
    }

    fn set_xsi_type(&mut self, doc: &mut Doc, node: NodeId, t: &[u8]) {
        self.set_ns_prop(doc, node, XSI_NAMESPACE, b"type", t);
    }

    /// `get_type_str`.
    fn get_type_str(
        &mut self,
        doc: &mut Doc,
        node: NodeId,
        ns: Option<&[u8]>,
        t: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(mut ns) = ns {
            if self.soap_version == SOAP_1_2 && ns == SOAP_1_1_ENC_NAMESPACE.as_bytes() {
                ns = SOAP_1_2_ENC_NAMESPACE.as_bytes();
            } else if self.soap_version == SOAP_1_1 && ns == SOAP_1_2_ENC_NAMESPACE.as_bytes() {
                ns = SOAP_1_1_ENC_NAMESPACE.as_bytes();
            }
            if let Some(p) = self.encode_add_ns(doc, node, ns).and_then(|n| n.prefix) {
                out.extend_from_slice(&p);
                out.push(b':');
            }
        }
        out.extend_from_slice(t);
        out
    }

    fn set_ns_and_type_ex(&mut self, doc: &mut Doc, node: NodeId, ns: Option<&[u8]>, t: &[u8]) {
        let s = self.get_type_str(doc, node, ns, t);
        self.set_xsi_type(doc, node, &s);
    }

    fn set_ns_and_type(&mut self, doc: &mut Doc, node: NodeId, enc: &Encoder) {
        let ns = enc.ns.clone();
        let t = enc.type_str.clone().unwrap_or_else(|| "".into());
        self.set_ns_and_type_ex(doc, node, ns.as_deref().map(str::as_bytes), t.as_bytes());
    }

    fn bogus(doc: &mut Doc, parent: NodeId) -> NodeId {
        let n = doc.new_element(b"BOGUS");
        doc.append_child(parent, n)
    }

    // ---- value helpers ------------------------------------------------------------

    fn zval_string(&mut self, v: &Value) -> SoapResult<Vec<u8>> {
        Ok(v.to_zstr(&mut *self.host)?.as_bytes().to_vec())
    }

    /// `get_serialization_string_from_zval`.
    fn serialization_string(&mut self, v: &Value) -> SoapResult<Vec<u8>> {
        if let Value::Object(o) = v {
            match self.host.enum_kind(o) {
                EnumKind::NotEnum => {}
                EnumKind::Pure => {
                    self.defer_value_error("Non-backed enums have no default serialization");
                    return Ok(Vec::new());
                }
                EnumKind::Backed(b) => return self.zval_string(&b),
            }
        }
        self.zval_string(v)
    }

    /// `get_serialization_long_from_zval`.
    fn serialization_long(&mut self, v: &Value) -> SoapResult<i64> {
        if let Value::Object(o) = v {
            match self.host.enum_kind(o) {
                EnumKind::NotEnum => {}
                EnumKind::Pure => {
                    self.defer_value_error("Non-backed enums have no default serialization");
                    return Ok(0);
                }
                EnumKind::Backed(Value::Int(i)) => return Ok(i),
                EnumKind::Backed(_) => {
                    self.defer_value_error("String-backed enum cannot be serialized as int");
                    return Ok(0);
                }
            }
        }
        Ok(v.to_long(&mut *self.host)?)
    }

    /// `get_zval_property`.
    pub(crate) fn get_property(
        &mut self,
        object: &Value,
        name: &[u8],
    ) -> SoapResult<Option<Value>> {
        match object {
            Value::Object(o) => Ok(self.host.read_property(o, name)?.map(|v| v.deref())),
            Value::Array(a) => Ok(a.get_str(name).map(Value::deref)),
            _ => Ok(None),
        }
    }

    /// `set_zval_property`.
    fn set_property(&mut self, object: &mut Value, name: &[u8], val: Value) -> SoapResult<()> {
        match object {
            Value::Object(o) => {
                let o = o.clone();
                self.host.write_property(&o, name, val)?;
            }
            Value::Array(a) => {
                a.make_mut().insert(ArrayKey::Str(ZStr::from(name)), val);
            }
            _ => {}
        }
        Ok(())
    }

    fn unset_property(&mut self, object: &mut Value, name: &[u8]) -> SoapResult<()> {
        match object {
            Value::Object(o) => {
                let o = o.clone();
                self.host.unset_property(&o, name)?;
            }
            Value::Array(a) => {
                a.make_mut().remove(&ArrayKey::Str(ZStr::from(name)));
            }
            _ => {}
        }
        Ok(())
    }

    fn new_object(&mut self, class: &ZStr) -> SoapResult<Value> {
        Ok(Value::Object(self.host.create_object(class)?))
    }

    // ---- references ------------------------------------------------------------------

    /// `soap_check_zval_ref`.
    fn check_zval_ref(&mut self, doc: &mut Doc, data: &Value, node: NodeId) -> bool {
        let Value::Object(o) = data else {
            return false;
        };
        let handle = o.handle();
        let Some(refs) = self.refs.as_mut() else {
            return false;
        };
        let Some(&node_ptr) = refs.enc.get(&handle) else {
            refs.enc.insert(handle, node);
            return false;
        };
        if node_ptr == node {
            return false;
        }
        if self.soap_version == SOAP_1_1 {
            let id = if let Some(id) = doc.attr_value(node_ptr, "id", None) {
                let mut s = b"#".to_vec();
                s.extend_from_slice(id);
                s
            } else {
                self.cur_uniq_ref += 1;
                let s = format!("#ref{}", self.cur_uniq_ref).into_bytes();
                doc.set_prop(node_ptr, b"id", &s[1..]);
                s
            };
            doc.set_prop(node, b"href", &id);
        } else {
            let id = if let Some(id) = doc.attr_value(node_ptr, "id", Some(SOAP_1_2_ENC_NAMESPACE))
            {
                let mut s = b"#".to_vec();
                s.extend_from_slice(id);
                s
            } else {
                self.cur_uniq_ref += 1;
                let s = format!("#ref{}", self.cur_uniq_ref).into_bytes();
                self.set_ns_prop(doc, node_ptr, SOAP_1_2_ENC_NAMESPACE, b"id", &s[1..]);
                s
            };
            self.set_ns_prop(doc, node, SOAP_1_2_ENC_NAMESPACE, b"ref", &id);
        }
        true
    }

    fn ref_key(doc: &Doc, node: NodeId) -> (u64, NodeId) {
        (doc.uid(), node)
    }

    /// `soap_check_xml_ref`.
    fn check_xml_ref(&self, doc: &Doc, node: NodeId) -> Option<Value> {
        self.refs
            .as_ref()
            .and_then(|r| r.dec.get(&Self::ref_key(doc, node)).cloned())
    }

    /// `soap_add_xml_ref`.
    fn add_xml_ref(&mut self, doc: &Doc, node: NodeId, v: &Value) {
        if let Some(r) = self.refs.as_mut() {
            r.dec.insert(Self::ref_key(doc, node), v.clone());
        }
    }

    /// `check_and_resolve_href`.
    fn check_and_resolve_href(&self, doc: &Doc, data: NodeId) -> SoapResult<NodeId> {
        if !doc.attrs(data).is_empty() {
            if let Some(href) = doc.attr_value(data, "href", None) {
                if let Some(id) = href.strip_prefix(b"#") {
                    return doc
                        .find_by_attribute_recursive(doc.first_top(), "id", id, None)
                        .ok_or_else(|| {
                            soap_fatal(format!("Encoding: Unresolved reference '{}'", lossy(href)))
                        });
                }
                bail!("Encoding: External reference '{}'", lossy(href));
            }
            if let Some(href) = doc.attr_value(data, "ref", Some(SOAP_1_2_ENC_NAMESPACE)) {
                let id = href.strip_prefix(b"#").unwrap_or(href);
                match doc.find_by_attribute_recursive(
                    doc.first_top(),
                    "id",
                    id,
                    Some(SOAP_1_2_ENC_NAMESPACE),
                ) {
                    None => bail!("Encoding: Unresolved reference '{}'", lossy(href)),
                    Some(r) if r == data => {
                        bail!(
                            "Encoding: Violation of id and ref information items '{}'",
                            lossy(href)
                        )
                    }
                    Some(r) => return Ok(r),
                }
            }
        }
        Ok(data)
    }

    // ---- master functions ----------------------------------------------------------

    /// `master_to_xml`.
    pub fn master_to_xml(
        &mut self,
        doc: &mut Doc,
        encode: Option<EncRef>,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        self.master_to_xml_int(doc, encode, data, style, parent, true)
    }

    fn soap_var_slot(&mut self, o: &Obj, name: &str) -> Option<Value> {
        self.host.read_slot(o, name).map(|v| v.deref())
    }

    #[allow(clippy::too_many_lines)]
    fn master_to_xml_int(
        &mut self,
        doc: &mut Doc,
        encode: Option<EncRef>,
        data: &Value,
        style: i64,
        parent: NodeId,
        check_class_map: bool,
    ) -> SoapResult<Option<NodeId>> {
        let data = data.deref();
        if let Value::Object(o) = &data {
            if is_class(o, "SoapVar") {
                let Some(Value::Int(ztype)) = self.soap_var_slot(o, "enc_type") else {
                    bail!("Encoding: SoapVar has no 'enc_type' property");
                };
                let zstype = str_of(self.soap_var_slot(o, "enc_stype").as_ref());
                let zns = str_of(self.soap_var_slot(o, "enc_ns").as_ref());
                let sdl = self.sdl.clone();
                let mut enc = None;
                if let Some(stype) = &zstype {
                    enc = match &zns {
                        Some(ns) => get_encoder(
                            sdl.as_deref(),
                            Some(&lossy(ns.as_bytes())),
                            &lossy(stype.as_bytes()),
                        ),
                        None => get_encoder_ex(sdl.as_deref(), &lossy(stype.as_bytes())),
                    };
                    if enc.is_none() && self.typemap.is_some() {
                        let mut nscat = String::new();
                        if let Some(ns) = &zns {
                            nscat.push_str(&lossy(ns.as_bytes()));
                            nscat.push(':');
                        }
                        nscat.push_str(&lossy(stype.as_bytes()));
                        enc = self.typemap_find(&nscat);
                    }
                }
                let enc = match enc {
                    Some(e) => e,
                    None => self.get_conversion(ztype)?,
                };
                let value = self.soap_var_slot(o, "enc_value").unwrap_or(Value::Null);
                let node = self.master_to_xml(doc, Some(enc), &value, style, parent)?;
                if let Some(node) = node {
                    if style == SOAP_ENCODED || (sdl.is_some() && encode != Some(enc)) {
                        if let Some(stype) = &zstype {
                            self.set_ns_and_type_ex(
                                doc,
                                node,
                                zns.as_ref().map(ZStr::as_bytes),
                                stype.as_bytes(),
                            );
                        }
                    }
                    if let Some(name) = str_of(self.soap_var_slot(o, "enc_name").as_ref()) {
                        doc.set_name(node, name.as_bytes());
                    }
                    if let Some(namens) = str_of(self.soap_var_slot(o, "enc_namens").as_ref()) {
                        let ns = self.encode_add_ns(doc, node, namens.as_bytes());
                        doc.set_ns(node, ns);
                    }
                }
                return Ok(node);
            }
        }
        let mut encode = encode;
        let mut add_type = false;
        if check_class_map {
            if let (Some(class_map), Value::Object(o)) = (self.class_map.clone(), &data) {
                if !self.protected.contains(&(o.handle() as usize)) {
                    let class = o.class_name();
                    for (key, val) in class_map.iter() {
                        let ArrayKey::Str(type_name) = key else {
                            continue;
                        };
                        let Value::Str(cn) = val.deref() else {
                            continue;
                        };
                        if !cn.eq_ignore_ascii_case(class.as_bytes()) {
                            continue;
                        }
                        if let Some(sdl) = self.sdl.clone() {
                            let tn = lossy(type_name.as_bytes());
                            let enc = get_encoder(
                                Some(&sdl),
                                Some(sdl.target_ns.as_deref().unwrap_or("")),
                                &tn,
                            )
                            .or_else(|| find_encoder_by_type_name(&sdl, &tn));
                            if let Some(enc) = enc {
                                if encode != Some(enc) && style == SOAP_LITERAL {
                                    add_type = true;
                                }
                                encode = Some(enc);
                            }
                        }
                        break;
                    }
                }
            }
        }
        let mut encode = encode.unwrap_or(conv(UNKNOWN_TYPE));
        if self.typemap.is_some() {
            if let Some(nscat) = Self::nscat_of(&self.enc(encode)) {
                if let Some(e) = self.typemap_find(&nscat) {
                    encode = e;
                }
            }
        }
        let node = self.to_xml(doc, encode, &data, style, parent)?;
        if add_type {
            if let Some(n) = node {
                let enc = self.enc(encode);
                self.set_ns_and_type(doc, n, &enc);
            }
        }
        Ok(node)
    }

    /// `master_to_zval`.
    pub fn master_to_zval(
        &mut self,
        encode: Option<EncRef>,
        doc: &Doc,
        data: NodeId,
    ) -> SoapResult<Value> {
        let data = self.check_and_resolve_href(doc, data)?;
        let encode = match encode {
            None => conv(UNKNOWN_TYPE),
            Some(e) => {
                let mut result = e;
                if let Some(t) = doc.attr_value(data, "type", Some(XSI_NAMESPACE)) {
                    let sdl = self.sdl.clone();
                    let mut enc = get_encoder_from_prefix(sdl.as_deref(), doc, data, t);
                    if let Some(found) = enc.filter(|&x| x != e) {
                        let mut tmp = Some(found);
                        while let Some(tm) = tmp {
                            let Some(st) = self.sdl_type_of(tm) else {
                                break;
                            };
                            let Some(sdl) = sdl.as_deref() else {
                                break;
                            };
                            let sty = sdl.ty(st);
                            if sty.kind == TypeKind::Complex {
                                break;
                            }
                            if Some(found) == sty.encode || Some(tm) == sty.encode {
                                enc = None;
                                break;
                            }
                            tmp = sty.encode;
                        }
                        if let Some(x) = enc {
                            result = x;
                        }
                    }
                }
                result
            }
        };
        self.master_to_zval_int(encode, doc, data)
    }

    fn master_to_zval_int(&mut self, encode: EncRef, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        let mut encode = encode;
        if self.typemap.is_some() {
            let enc = self.enc(encode);
            if let Some(nscat) = Self::nscat_of(&enc) {
                if let Some(e) = self.typemap_find(&nscat) {
                    encode = e;
                }
            } else if let Some(t) = doc.attr_value(data, "type", Some(XSI_NAMESPACE)) {
                let (prefix, local) = parse_qname(t);
                let mut nscat = String::new();
                if let Some(ns) = doc.search_ns(data, prefix) {
                    nscat.push_str(&lossy(&ns.href));
                    nscat.push(':');
                }
                nscat.push_str(&lossy(local));
                if let Some(e) = self.typemap_find(&nscat) {
                    encode = e;
                }
            }
        }
        if self.decode_depth >= SOAP_MAX_DECODE_DEPTH {
            bail!("Encoding: Nesting level too deep");
        }
        self.host.memory_check()?;
        self.decode_depth += 1;
        let r = self.to_zval(encode, doc, data);
        self.decode_depth -= 1;
        r
    }

    fn to_xml(
        &mut self,
        doc: &mut Doc,
        encode: EncRef,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        let enc = self.enc(encode);
        Ok(Some(match enc.to_xml {
            ToXml::Guess => return self.guess_xml_convert(doc, data, style, parent),
            ToXml::Null => {
                let n = Self::bogus(doc, parent);
                if style == SOAP_ENCODED {
                    self.set_xsi_nil(doc, n);
                }
                n
            }
            ToXml::String | ToXml::Duration => {
                self.to_xml_string(doc, &enc, data, style, parent)?
            }
            ToXml::Long => self.to_xml_long(doc, &enc, data, style, parent)?,
            ToXml::Double => self.to_xml_double(doc, &enc, data, style, parent)?,
            ToXml::Bool => self.to_xml_bool(doc, &enc, data, style, parent)?,
            ToXml::GuessArrayMap => {
                let e = match data {
                    Value::Array(a) if !is_list(a) => conv(APACHE_MAP),
                    Value::Array(_) => conv(SOAP_ENC_ARRAY),
                    _ => conv(IS_NULL),
                };
                return self.master_to_xml(doc, Some(e), data, style, parent);
            }
            ToXml::Object => self.to_xml_object(doc, encode, data, style, parent)?,
            ToXml::Array => self.to_xml_array(doc, encode, data, style, parent)?,
            ToXml::Map => return self.to_xml_map(doc, &enc, data, style, parent),
            ToXml::Base64 => self.to_xml_base64(doc, &enc, data, style, parent)?,
            ToXml::HexBin => self.to_xml_hexbin(doc, &enc, data, style, parent)?,
            ToXml::DateTime => self.to_xml_datetime(
                doc,
                &enc,
                data,
                "%Y-%m-%dT%H:%M:%S",
                "Y-m-d\\TH:i:s.up",
                style,
                parent,
            )?,
            ToXml::Time => {
                self.to_xml_datetime(doc, &enc, data, "%H:%M:%S", "H:i:s.up", style, parent)?
            }
            ToXml::Date => {
                self.to_xml_datetime(doc, &enc, data, "%Y-%m-%d", "Y-m-dp", style, parent)?
            }
            ToXml::GYearMonth => {
                self.to_xml_datetime(doc, &enc, data, "%Y-%m", "Y-mp", style, parent)?
            }
            ToXml::GYear => self.to_xml_datetime(doc, &enc, data, "%Y", "Yp", style, parent)?,
            ToXml::GMonthDay => {
                self.to_xml_datetime(doc, &enc, data, "--%m-%d", "--m-dp", style, parent)?
            }
            ToXml::GDay => {
                self.to_xml_datetime(doc, &enc, data, "---%d", "---dp", style, parent)?
            }
            ToXml::GMonth => {
                self.to_xml_datetime(doc, &enc, data, "--%m--", "--m--p", style, parent)?
            }
            ToXml::List1 => self.to_xml_list(doc, encode, data, style, parent)?,
            ToXml::Any => return self.to_xml_any(doc, data, style, parent),
            ToXml::SdlGuess => return self.sdl_guess_convert_xml(doc, encode, data, style, parent),
            ToXml::User => self.to_xml_user(doc, &enc, data, style, parent)?,
        }))
    }

    fn to_zval(&mut self, encode: EncRef, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        let enc = self.enc(encode);
        match enc.to_zval {
            ToZval::Guess => self.guess_zval_convert(encode, doc, data),
            ToZval::Null => Ok(Value::Null),
            ToZval::String => self.to_zval_string(&enc, doc, data, 0),
            ToZval::StringR => self.to_zval_string(&enc, doc, data, 1),
            ToZval::StringC => self.to_zval_string(&enc, doc, data, 2),
            ToZval::Long => self.to_zval_number(&enc, doc, data, false),
            ToZval::Double => self.to_zval_number(&enc, doc, data, true),
            ToZval::Bool => self.to_zval_bool(&enc, doc, data),
            ToZval::Object => self.to_zval_object(encode, doc, data, None),
            ToZval::Array => self.to_zval_array(encode, doc, data),
            ToZval::Map => self.to_zval_map(doc, data),
            ToZval::Base64 => self.to_zval_base64(&enc, doc, data),
            ToZval::HexBin => self.to_zval_hexbin(&enc, doc, data),
            ToZval::Any => self.to_zval_any(doc, data),
            ToZval::SdlGuess => self.sdl_guess_convert_zval(encode, doc, data),
            ToZval::User => self.to_zval_user(&enc, doc, data),
        }
    }

    // ---- scalars: decoding ------------------------------------------------------------

    fn is_nil(doc: &Doc, data: NodeId) -> bool {
        doc.attr(data, "nil", Some(XSI_NAMESPACE)).is_some()
    }

    /// `to_zval_string` (`mode` 0), `to_zval_stringr` (1), `to_zval_stringc` (2).
    fn to_zval_string(
        &mut self,
        enc: &Encoder,
        doc: &Doc,
        data: NodeId,
        mode: u8,
    ) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let Some(child) = doc.first_child(data) else {
            return Ok(Value::from(""));
        };
        let single = doc.next(child).is_none();
        match doc.kind(child) {
            Kind::Text if single => {
                let raw = doc.content(child);
                let content = match mode {
                    1 => whitespace_replace(raw),
                    2 => whitespace_collapse(raw),
                    _ => raw.to_vec(),
                };
                let out = match self.charset {
                    Some(cs) => cs.from_utf8(&content).unwrap_or(content),
                    None => content,
                };
                Ok(Value::from(out.as_slice()))
            }
            Kind::CData if single => Ok(Value::from(doc.content(child))),
            _ => bail!(
                "Encoding: Type '{}' value must contain a single text or CDATA node",
                enc.type_name()
            ),
        }
    }

    fn to_zval_base64(&mut self, enc: &Encoder, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let Some(child) = doc.first_child(data) else {
            return Ok(Value::from(""));
        };
        let single = doc.next(child).is_none();
        match doc.kind(child) {
            Kind::Text if single => Ok(Value::from(
                base64_decode(&whitespace_collapse(doc.content(child))).as_slice(),
            )),
            Kind::CData if single => Ok(Value::from(base64_decode(doc.content(child)).as_slice())),
            _ => bail!(
                "Encoding: Type '{}' value must contain a single text or CDATA node",
                enc.type_name()
            ),
        }
    }

    fn to_zval_hexbin(&mut self, enc: &Encoder, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let Some(child) = doc.first_child(data) else {
            return Ok(Value::from(""));
        };
        let single = doc.next(child).is_none();
        let content = match doc.kind(child) {
            Kind::Text if single => whitespace_collapse(doc.content(child)),
            Kind::CData if single => doc.content(child).to_vec(),
            _ => bail!(
                "Encoding: Type '{}' value must contain a single text or CDATA node",
                enc.type_name()
            ),
        };
        let content: Vec<u8> = content.into_iter().take_while(|&c| c != 0).collect();
        if content.len() % 2 != 0 {
            bail!(
                "Encoding: Type '{}' value must contain an even number of hexadecimal digits",
                enc.type_name()
            );
        }
        let hex = |c: u8| -> SoapResult<u8> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(soap_fatal(format!(
                    "Encoding: Invalid value for type '{}'",
                    enc.type_name()
                ))),
            }
        };
        let mut out = Vec::with_capacity(content.len() / 2);
        for pair in content.chunks(2) {
            out.push((hex(pair[0])? << 4) | hex(pair[1])?);
        }
        Ok(Value::from(out.as_slice()))
    }

    /// `to_zval_long` / `to_zval_double`.
    fn to_zval_number(
        &mut self,
        enc: &Encoder,
        doc: &Doc,
        data: NodeId,
        double: bool,
    ) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let Some(child) = doc.first_child(data) else {
            return Ok(Value::Null);
        };
        if doc.kind(child) != Kind::Text || doc.next(child).is_some() {
            bail!(
                "Encoding: Type '{}' value must contain a single text or CDATA node",
                enc.type_name()
            );
        }
        let s = whitespace_collapse(doc.content(child));
        let s: Vec<u8> = s.into_iter().take_while(|&c| c != 0).collect();
        #[allow(clippy::cast_precision_loss)]
        match is_numeric_string(&s, false) {
            Some(Number::Int(i)) => Ok(if double {
                Value::Float(i as f64)
            } else {
                Value::Int(i)
            }),
            Some(Number::Float(f)) => Ok(Value::Float(f)),
            None => {
                if double {
                    let up = |p: &[u8]| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p);
                    if up(b"NaN") {
                        return Ok(Value::Float(f64::NAN));
                    } else if up(b"INF") {
                        return Ok(Value::Float(f64::INFINITY));
                    } else if up(b"-INF") {
                        return Ok(Value::Float(f64::NEG_INFINITY));
                    }
                }
                bail!("Encoding: Invalid value for type '{}'", enc.type_name())
            }
        }
    }

    fn to_zval_bool(&mut self, enc: &Encoder, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let Some(child) = doc.first_child(data) else {
            return Ok(Value::Null);
        };
        if doc.kind(child) != Kind::Text || doc.next(child).is_some() {
            bail!(
                "Encoding: Type '{}' value must contain a single text or CDATA node",
                enc.type_name()
            );
        }
        let s = whitespace_collapse(doc.content(child));
        let falsy = s.is_empty()
            || s == b"0"
            || s.eq_ignore_ascii_case(b"f")
            || s.eq_ignore_ascii_case(b"false");
        Ok(Value::Bool(!falsy))
    }

    // ---- scalars: encoding ------------------------------------------------------------

    fn to_xml_string(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let mut s = self.serialization_string(data)?;
        if let Some(cs) = self.charset {
            if let Some(conv) = cs.to_utf8(&s) {
                s = conv;
            }
        }
        if !check_utf8(&s) {
            bail!(
                "Encoding: string '{}' is not a valid utf-8 string",
                utf8_error_excerpt(&s)
            );
        }
        doc.add_text(ret, &s);
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    fn to_xml_base64(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let s = self.serialization_string(data)?;
        doc.add_text(ret, &base64_encode(&s));
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    fn to_xml_hexbin(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let s = match data {
            Value::Str(s) => s.as_bytes().to_vec(),
            other => self.serialization_string(other)?,
        };
        let mut out = Vec::with_capacity(s.len() * 2);
        for b in s {
            out.push(HEX[usize::from(b >> 4)]);
            out.push(HEX[usize::from(b & 15)]);
        }
        doc.add_text(ret, &out);
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    fn to_xml_long(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        if let Value::Float(d) = data {
            let f = d.floor();
            let s = if f.is_nan() {
                "NAN".to_owned()
            } else if f.is_infinite() {
                if f > 0.0 { "INF" } else { "-INF" }.to_owned()
            } else {
                format!("{f:.0}")
            };
            doc.set_content(ret, s.as_bytes());
        } else {
            let l = self.serialization_long(data)?;
            doc.set_content(ret, l.to_string().as_bytes());
        }
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    fn to_xml_double(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let d = data.to_double(&mut *self.host)?;
        let precision = i32::try_from(self.host.precision()).unwrap_or(14);
        let precision = if precision == 0 { 1 } else { precision };
        let s = php_runtime::float::zend_gcvt(d, precision, b'.', b'E');
        doc.set_content(ret, s.as_bytes());
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    fn to_xml_bool(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let b = data.to_bool(&mut *self.host)?;
        doc.set_content(ret, if b { b"true" } else { b"false" });
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    #[allow(clippy::too_many_arguments)]
    fn to_xml_datetime(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        format: &str,
        ext_format: &str,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        match data {
            Value::Int(ts) => {
                let Some(tm) = self.host.localtime(*ts) else {
                    bail!("Encoding: Invalid timestamp {ts}");
                };
                let mut s = format
                    .replace("%Y", &tm.year.to_string())
                    .replace("%m", &format!("{:02}", tm.month))
                    .replace("%d", &format!("{:02}", tm.day))
                    .replace("%H", &format!("{:02}", tm.hour))
                    .replace("%M", &format!("{:02}", tm.minute))
                    .replace("%S", &format!("{:02}", tm.second));
                let tz = format!(
                    "{}{:02}:{:02}",
                    if tm.gmtoff < 0 { '-' } else { '+' },
                    (tm.gmtoff / 3600).abs(),
                    (tm.gmtoff % 3600 / 60).abs()
                );
                if tz == "+00:00" {
                    s.push('Z');
                } else {
                    s.push_str(&tz);
                }
                doc.set_content(ret, s.as_bytes());
            }
            Value::Str(s) => doc.set_content(ret, s.as_bytes()),
            Value::Object(o) if self.host.instance_of(o, "DateTimeInterface") => {
                match self.host.format_date(o, ext_format) {
                    Some(s) => doc.set_content(ret, &s),
                    None => bail!("Encoding: Invalid DateTimeInterface"),
                }
            }
            _ => {}
        }
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    /// `to_xml_list` (also used for unions).
    fn to_xml_list(
        &mut self,
        doc: &mut Doc,
        encode: EncRef,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let enc = self.enc(encode);
        let sdl = self.sdl.clone();
        let list_enc = enc.sdl_type.and_then(|t| {
            let sdl = sdl.as_deref()?;
            let ty = sdl.ty(t);
            if ty.kind == TypeKind::List {
                ty.elements
                    .as_ref()?
                    .first()
                    .and_then(|&e| sdl.ty(e).encode)
            } else {
                None
            }
        });
        let ret = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, ret);
            }
            return Ok(ret);
        }
        let saved = self.refs.take();
        let result = (|| -> SoapResult<Vec<u8>> {
            let items: Vec<Value> = match data {
                Value::Array(a) => a.values().cloned().collect(),
                other => {
                    let s = match other {
                        Value::Str(s) => s.as_bytes().to_vec(),
                        v => self.serialization_string(v)?,
                    };
                    let s: Vec<u8> = s.into_iter().take_while(|&c| c != 0).collect();
                    whitespace_collapse(&s)
                        .split(|&c| c == b' ')
                        .filter(|p| !p.is_empty())
                        .map(Value::from)
                        .collect()
                }
            };
            let mut list: Vec<u8> = Vec::new();
            for item in items {
                let dummy = self.master_to_xml(doc, list_enc, &item, SOAP_LITERAL, ret)?;
                let content = dummy
                    .and_then(|d| doc.first_child(d))
                    .map(|c| doc.content(c).to_vec());
                if let Some(c) = content {
                    if !list.is_empty() {
                        list.push(b' ');
                    }
                    list.extend_from_slice(&c);
                } else {
                    let le = list_enc.map(|e| self.enc(e));
                    bail!(
                        "Encoding: Failed to encode list item of type '{}' for list type '{}'",
                        le.as_ref().map_or("unknown", Encoder::type_name),
                        enc.type_name()
                    );
                }
                if let Some(d) = dummy {
                    doc.unlink(d);
                }
            }
            Ok(list)
        })();
        self.refs = saved;
        let list = result?;
        doc.set_content(ret, &list);
        Ok(ret)
    }

    /// `to_xml_any`.
    fn to_xml_any(
        &mut self,
        doc: &mut Doc,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        if let Value::Array(a) = data {
            let mut ret = None;
            let any = conv(XSD_ANYXML);
            for (k, el) in a.iter() {
                ret = self.master_to_xml(doc, Some(any), el, style, parent)?;
                if let (Some(r), ArrayKey::Str(name)) = (ret, k) {
                    if doc.kind(r) != Kind::TextNoEnc {
                        doc.set_name(r, name.as_bytes());
                    }
                }
            }
            return Ok(ret);
        }
        let s = self.serialization_string(data)?;
        let t = doc.new_text_noenc(&s);
        Ok(Some(doc.append_child(parent, t)))
    }

    /// `to_xml_user`.
    fn to_xml_user(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let mut ret = None;
        if let Some(cb) = enc.map.as_ref().and_then(|m| m.to_xml.clone()) {
            let r = self.host.call_callable(&cb, std::slice::from_ref(data))?;
            if let Value::Str(s) = r {
                if let Ok(parsed) = Doc::parse(s.as_bytes()) {
                    if let Some(first) = parsed.first_top() {
                        ret = Some(doc.import(&parsed, first));
                    }
                }
            }
        }
        let ret = ret.unwrap_or_else(|| doc.new_element(b"BOGUS"));
        let ret = doc.append_child(parent, ret);
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, ret, enc);
        }
        Ok(ret)
    }

    /// `to_zval_user`.
    fn to_zval_user(&mut self, enc: &Encoder, doc: &Doc, node: NodeId) -> SoapResult<Value> {
        if let Some(cb) = enc.map.as_ref().and_then(|m| m.to_zval.clone()) {
            // xmlCopyNode(node, 1) + xmlNodeDump: the copy re-declares
            // the namespaces its element and attribute names use.
            let mut copy = Doc::new();
            let root = copy.import(doc, node);
            let dump = copy.dump_node(root);
            return Ok(self
                .host
                .call_callable(&cb, &[Value::from(dump.as_slice())])?);
        }
        Ok(Value::Null)
    }

    /// `to_zval_any`.
    fn to_zval_any(&mut self, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if let Some(sdl) = self.sdl.clone() {
            if let Some(elements) = &sdl.elements {
                if !doc.name(data).is_empty() {
                    let mut nscat = String::new();
                    if let Some(href) = doc.ns_href(data) {
                        nscat.push_str(&lossy(href));
                        nscat.push(':');
                    }
                    nscat.push_str(&lossy(doc.name(data)));
                    if let Some(&t) = elements.get(&nscat) {
                        if let Some(e) = sdl.ty(t).encode {
                            return self.master_to_zval_int(e, doc, data);
                        }
                    }
                }
            }
        }
        Ok(Value::from(doc.dump_node(data).as_slice()))
    }
}

/// `find_encoder_by_type_name`.
fn find_encoder_by_type_name(sdl: &Sdl, t: &str) -> Option<EncRef> {
    for &e in sdl.encoders.values() {
        let Some(enc) = sdl.encoder(e) else {
            continue;
        };
        if t.starts_with('{') {
            if enc.clark_notation().as_deref() == Some(t) {
                return Some(e);
            }
        } else if enc.type_str.as_deref() == Some(t) {
            return Some(e);
        }
    }
    None
}

fn get_soap_enc_attribute<'d>(doc: &'d Doc, n: NodeId, name: &str) -> Option<&'d [u8]> {
    doc.attr_value(n, name, Some(SOAP_1_1_ENC_NAMESPACE))
        .or_else(|| doc.attr_value(n, name, Some(SOAP_1_2_ENC_NAMESPACE)))
        .filter(|v| !v.is_empty())
}

fn array_extra<'s>(
    sdl: &'s Sdl,
    t: TypeId,
    attr_key: &str,
    ext_key: &str,
) -> Option<&'s ExtraAttribute> {
    sdl.ty(t)
        .attributes
        .as_ref()?
        .get(attr_key)?
        .extra
        .as_ref()?
        .get(ext_key)
}

fn single_element(sdl: &Sdl, t: TypeId) -> Option<TypeId> {
    sdl.ty(t)
        .elements
        .as_ref()
        .filter(|e| e.len() == 1)
        .and_then(|e| e.first().copied())
}

/// `model_array_element`.
fn model_array_element(sdl: &Sdl, model: &Model) -> Option<TypeId> {
    match &model.kind {
        ModelKind::Element(t) => (model.max_occurs == -1 || model.max_occurs > 1).then_some(*t),
        ModelKind::Sequence(c) | ModelKind::All(c) | ModelKind::Choice(c) => {
            if c.len() == 1 {
                model_array_element(sdl, &c[0])
            } else {
                None
            }
        }
        ModelKind::Group(g) => sdl
            .ty(*g)
            .model
            .as_ref()
            .and_then(|m| model_array_element(sdl, m)),
        _ => None,
    }
}

fn identity(v: &Value) -> Option<usize> {
    match v {
        Value::Object(o) => Some(o.handle() as usize),
        Value::Array(a) => Some(a.addr()),
        _ => None,
    }
}

impl EncodeCtx<'_> {
    fn sdl_rc(&self) -> Rc<Sdl> {
        self.sdl.clone().unwrap_or_default()
    }

    /// The xsi:type filter of `master_to_zval`/`guess_zval_convert`: drops
    /// encoders that only restate the expected simple type.
    fn filter_xsi_encoder(&self, found: EncRef) -> Option<EncRef> {
        let Some(sdl) = self.sdl.clone() else {
            return Some(found);
        };
        let mut tmp = Some(found);
        while let Some(tm) = tmp {
            let Some(st) = self.sdl_type_of(tm) else {
                break;
            };
            let ty = sdl.ty(st);
            if ty.kind == TypeKind::Complex {
                break;
            }
            if Some(found) == ty.encode || Some(tm) == ty.encode {
                return None;
            }
            tmp = ty.encode;
        }
        Some(found)
    }

    /// `guess_xml_convert`.
    fn guess_xml_convert(
        &mut self,
        doc: &mut Doc,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        let enc = self.get_conversion(type_code(data))?;
        self.master_to_xml_int(doc, Some(enc), data, style, parent, false)
    }

    /// `guess_zval_convert`.
    fn guess_zval_convert(&mut self, type_: EncRef, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        let data = self.check_and_resolve_href(doc, data)?;
        let mut type_name: Option<Vec<u8>> = None;
        let enc = if Self::is_nil(doc, data) {
            conv(IS_NULL)
        } else {
            let mut enc = None;
            if let Some(t) = doc.attr_value(data, "type", Some(XSI_NAMESPACE)) {
                type_name = Some(t.to_vec());
                let sdl = self.sdl.clone();
                enc = get_encoder_from_prefix(sdl.as_deref(), doc, data, t).filter(|&e| e != type_);
                if let Some(e) = enc {
                    enc = self.filter_xsi_encoder(e);
                }
            }
            match enc {
                Some(e) => e,
                None => {
                    if get_soap_enc_attribute(doc, data, "arrayType").is_some()
                        || doc
                            .attr(data, "arrayType", Some(SOAP_1_1_ENC_NAMESPACE))
                            .is_some()
                        || doc
                            .attr(data, "arrayType", Some(SOAP_1_2_ENC_NAMESPACE))
                            .is_some()
                        || doc
                            .attr(data, "itemType", Some(SOAP_1_1_ENC_NAMESPACE))
                            .is_some()
                        || doc
                            .attr(data, "itemType", Some(SOAP_1_2_ENC_NAMESPACE))
                            .is_some()
                        || doc
                            .attr(data, "arraySize", Some(SOAP_1_1_ENC_NAMESPACE))
                            .is_some()
                        || doc
                            .attr(data, "arraySize", Some(SOAP_1_2_ENC_NAMESPACE))
                            .is_some()
                    {
                        conv(SOAP_ENC_ARRAY)
                    } else if doc.children(data).any(|c| doc.is_element(c)) {
                        conv(SOAP_ENC_OBJECT)
                    } else {
                        conv(XSD_STRING)
                    }
                }
            }
        };
        let ret = self.master_to_zval_int(enc, doc, data)?;
        let details = self.enc(enc);
        if let (Some(_), Some(tn), Some(_)) = (&self.sdl, &type_name, details.sdl_type) {
            let var = self.host.create_object(&ZStr::from("SoapVar"))?;
            self.host
                .write_slot(&var, "enc_type", Value::Int(details.type_id));
            self.host.write_slot(&var, "enc_value", ret);
            let (prefix, local) = parse_qname(tn);
            self.host.write_slot(&var, "enc_stype", Value::from(local));
            if let Some(ns) = doc.search_ns(data, prefix) {
                self.host
                    .write_slot(&var, "enc_ns", Value::from(ns.href.as_slice()));
            }
            return Ok(Value::Object(var));
        }
        Ok(ret)
    }

    /// `sdl_guess_convert_xml`.
    fn sdl_guess_convert_xml(
        &mut self,
        doc: &mut Doc,
        encode: EncRef,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        let enc = self.enc(encode);
        let Some(t) = enc.sdl_type else {
            let ret = self.guess_xml_convert(doc, data, style, parent)?;
            if style == SOAP_ENCODED {
                if let Some(r) = ret {
                    self.set_ns_and_type(doc, r, &enc);
                }
            }
            return Ok(ret);
        };
        let sdl = self.sdl_rc();
        let ty = sdl.ty(t);
        let ret = match ty.kind {
            TypeKind::Simple => match ty.encode {
                Some(te) if te != encode => {
                    self.master_to_xml(doc, Some(te), data, style, parent)?
                }
                _ => self.guess_xml_convert(doc, data, style, parent)?,
            },
            TypeKind::List | TypeKind::Union => {
                Some(self.to_xml_list(doc, encode, data, style, parent)?)
            }
            TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension => {
                let is_array = ty
                    .encode
                    .is_some_and(|e| matches!(self.enc(e).type_id, IS_ARRAY | SOAP_ENC_ARRAY));
                return if is_array {
                    Ok(Some(self.to_xml_array(doc, encode, data, style, parent)?))
                } else {
                    Ok(Some(self.to_xml_object(doc, encode, data, style, parent)?))
                };
            }
        };
        if style == SOAP_ENCODED {
            if let Some(r) = ret {
                self.set_ns_and_type(doc, r, &enc);
            }
        }
        Ok(ret)
    }

    /// `sdl_guess_convert_zval`.
    fn sdl_guess_convert_zval(
        &mut self,
        encode: EncRef,
        doc: &Doc,
        data: NodeId,
    ) -> SoapResult<Value> {
        let enc = self.enc(encode);
        let Some(t) = enc.sdl_type else {
            return self.guess_zval_convert(encode, doc, data);
        };
        let sdl = self.sdl_rc();
        let ty = sdl.ty(t);
        match ty.kind {
            TypeKind::Simple => match ty.encode {
                Some(te) if te != encode => self.master_to_zval_int(te, doc, data),
                _ => self.guess_zval_convert(encode, doc, data),
            },
            TypeKind::List | TypeKind::Union => self.to_zval_string(&enc, doc, data, 2),
            TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension => {
                let is_array = ty
                    .encode
                    .is_some_and(|e| matches!(self.enc(e).type_id, IS_ARRAY | SOAP_ENC_ARRAY));
                if is_array {
                    self.to_zval_array(encode, doc, data)
                } else {
                    self.to_zval_object(encode, doc, data, None)
                }
            }
        }
    }

    /// Follows the base encoders of a restriction/extension down to a
    /// simple (or non-SDL) one.
    fn simple_base(&self, sdl: &Sdl, start: Option<EncRef>) -> Option<EncRef> {
        let mut enc = start;
        while let Some(e) = enc {
            let Some(st) = self.sdl_type_of(e) else {
                break;
            };
            if matches!(
                sdl.ty(st).kind,
                TypeKind::Simple | TypeKind::List | TypeKind::Union
            ) {
                break;
            }
            enc = sdl.ty(st).encode;
        }
        enc
    }

    fn props_of(&mut self, data: &Value) -> Option<ZArray> {
        match data {
            Value::Object(o) => Some(self.host.property_table(o)),
            Value::Array(a) => Some(a.clone()),
            _ => None,
        }
    }

    /// `to_xml_object`.
    #[allow(clippy::too_many_lines)]
    fn to_xml_object(
        &mut self,
        doc: &mut Doc,
        encode: EncRef,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let enc = self.enc(encode);
        if matches!(data, Value::Null) {
            let n = Self::bogus(doc, parent);
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, n);
                self.set_ns_and_type(doc, n, &enc);
            }
            return Ok(n);
        }
        let prop = self.props_of(data);
        let Some(t) = enc.sdl_type else {
            let xml_param = Self::bogus(doc, parent);
            if self.check_zval_ref(doc, data, xml_param) {
                return Ok(xml_param);
            }
            if let Some(prop) = prop {
                for (key, zprop) in prop.iter() {
                    let zprop = zprop.deref();
                    let e = self.get_conversion(type_code(&zprop))?;
                    let property = self.master_to_xml(doc, Some(e), &zprop, style, xml_param)?;
                    if let (ArrayKey::Str(k), Some(p)) = (key, property) {
                        let name = if matches!(data, Value::Object(_)) {
                            unmangle(k.as_bytes())
                        } else {
                            k.as_bytes()
                        };
                        doc.set_name(p, name);
                    }
                }
            }
            if style == SOAP_ENCODED {
                self.set_ns_and_type(doc, xml_param, &enc);
            }
            return Ok(xml_param);
        };
        let sdl = self.sdl_rc();
        let ty = sdl.ty(t).clone();
        let xml_param = if ty.kind == TypeKind::Restriction
            && ty.encode.is_some()
            && ty.encode != Some(encode)
        {
            if let Some(base) = self.simple_base(&sdl, ty.encode) {
                if let Some(tmp) = self.get_property(data, b"_")? {
                    self.master_to_xml(doc, Some(base), &tmp, style, parent)?
                } else if prop.is_none() {
                    self.master_to_xml(doc, Some(base), data, style, parent)?
                } else {
                    Some(Self::bogus(doc, parent))
                }
            } else {
                Some(Self::bogus(doc, parent))
            }
        } else if ty.kind == TypeKind::Extension && ty.encode.is_some() && ty.encode != Some(encode)
        {
            let te = ty.encode;
            let base_complex = te.and_then(|e| self.sdl_type_of(e)).is_some_and(|bt| {
                !matches!(
                    sdl.ty(bt).kind,
                    TypeKind::Simple | TypeKind::List | TypeKind::Union
                )
            });
            if base_complex {
                let id = identity(data);
                if let Some(id) = id {
                    self.protected.insert(id);
                }
                let r = self.master_to_xml(doc, te, data, style, parent);
                if let Some(id) = id {
                    self.protected.remove(&id);
                }
                r?
            } else if let Some(tmp) = self.get_property(data, b"_")? {
                self.master_to_xml(doc, te, &tmp, style, parent)?
            } else if prop.is_none() {
                self.master_to_xml(doc, te, data, style, parent)?
            } else {
                Some(Self::bogus(doc, parent))
            }
        } else {
            Some(Self::bogus(doc, parent))
        };
        let Some(xml_param) = xml_param else {
            return Ok(Self::bogus(doc, parent));
        };
        if self.check_zval_ref(doc, data, xml_param) {
            return Ok(xml_param);
        }
        if let Some(prop) = &prop {
            let array_el = match (data, &ty.attributes, &ty.model) {
                (Value::Array(a), None, Some(m)) if is_list(a) => model_array_element(&sdl, m),
                _ => None,
            };
            if let Some(ael) = array_el {
                let el = sdl.ty(ael).clone();
                for val in prop.values() {
                    let val = val.deref();
                    let property = if matches!(val, Value::Null) && el.nillable {
                        let p = Self::bogus(doc, xml_param);
                        self.set_xsi_nil(doc, p);
                        Some(p)
                    } else {
                        self.master_to_xml(doc, el.encode, &val, style, xml_param)?
                    };
                    if let Some(p) = property {
                        doc.set_name(p, el.name_str().as_bytes());
                        if style == SOAP_LITERAL && el.form == Form::Qualified {
                            if let Some(nsn) = &el.namens {
                                let ns = self.encode_add_ns(doc, p, nsn.as_bytes());
                                doc.set_ns(p, ns);
                            }
                        }
                    }
                }
            } else if let Some(m) = &ty.model {
                self.model_to_xml_object(doc, xml_param, &sdl, m, data, style, true)?;
            }
            if let Some(attrs) = &ty.attributes {
                let saved = self.refs.take();
                let r = self.attributes_to_xml(doc, xml_param, attrs, data);
                self.refs = saved;
                r?;
            }
        }
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, xml_param, &enc);
        }
        Ok(xml_param)
    }

    fn attributes_to_xml(
        &mut self,
        doc: &mut Doc,
        xml_param: NodeId,
        attrs: &ZHash<SdlAttribute>,
        data: &Value,
    ) -> SoapResult<()> {
        for attr in attrs.values() {
            let Some(name) = &attr.name else {
                continue;
            };
            let Some(zattr) = self.get_property(data, name.as_bytes())? else {
                continue;
            };
            let Some(dummy) =
                self.master_to_xml(doc, attr.encode, &zattr, SOAP_LITERAL, xml_param)?
            else {
                continue;
            };
            let content = doc
                .first_child(dummy)
                .filter(|&c| matches!(doc.kind(c), Kind::Text | Kind::CData | Kind::TextNoEnc))
                .map(|c| doc.content(c).to_vec());
            if let Some(content) = content {
                if let Some(fixed) = &attr.fixed {
                    if fixed.as_bytes() != content.as_slice() {
                        bail!(
                            "Encoding: Attribute '{name}' has fixed value '{fixed}' (value '{}' is not allowed)",
                            lossy(&content)
                        );
                    }
                }
                if attr
                    .namens
                    .as_deref()
                    .is_some_and(|ns| ns == XML_NAMESPACE || attr.form == Form::Qualified)
                {
                    let nsn = attr.namens.clone().unwrap_or_default();
                    let nsp = self.encode_add_ns(doc, xml_param, nsn.as_bytes());
                    doc.set_ns_prop(xml_param, nsp, name.as_bytes(), &content);
                } else {
                    doc.set_prop(xml_param, name.as_bytes(), &content);
                }
            }
            doc.unlink(dummy);
        }
        Ok(())
    }

    fn fixed_check(doc: &Doc, property: NodeId, el: &SdlType) -> SoapResult<()> {
        if let (Some(fixed), Some(c)) = (&el.fixed, doc.first_child(property)) {
            if matches!(doc.kind(c), Kind::Text | Kind::CData | Kind::TextNoEnc)
                && doc.content(c) != fixed.as_bytes()
            {
                bail!(
                    "Encoding: Element '{}' has fixed value '{fixed}' (value '{}' is not allowed)",
                    el.name_str(),
                    lossy(doc.content(c))
                );
            }
        }
        Ok(())
    }

    fn qualify(&mut self, doc: &mut Doc, property: NodeId, el: &SdlType, style: i64) {
        if style == SOAP_LITERAL && el.form == Form::Qualified {
            if let Some(nsn) = &el.namens {
                let ns = self.encode_add_ns(doc, property, nsn.as_bytes());
                doc.set_ns(property, ns);
            }
        }
    }

    /// `model_to_xml_object`: 0 = missing, 1 = written, 2 = optional and absent.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn model_to_xml_object(
        &mut self,
        doc: &mut Doc,
        node: NodeId,
        sdl: &Sdl,
        model: &Model,
        object: &Value,
        style: i64,
        strict: bool,
    ) -> SoapResult<i32> {
        match &model.kind {
            ModelKind::Element(t) => {
                let el = sdl.ty(*t);
                let data = self.get_property(object, el.name_str().as_bytes())?;
                if let Some(Value::Null) = data {
                    if !el.nillable && model.min_occurs > 0 && !strict {
                        return Ok(0);
                    }
                }
                if let Some(data) = data {
                    let multi = model.max_occurs == -1 || model.max_occurs > 1;
                    match &data {
                        Value::Array(a) if multi && is_list(a) => {
                            for val in a.values() {
                                let val = val.deref();
                                let property = if matches!(val, Value::Null) && el.nillable {
                                    let p = Self::bogus(doc, node);
                                    self.set_xsi_nil(doc, p);
                                    Some(p)
                                } else {
                                    let p =
                                        self.master_to_xml(doc, el.encode, &val, style, node)?;
                                    if let Some(p) = p {
                                        Self::fixed_check(doc, p, el)?;
                                    }
                                    p
                                };
                                if let Some(p) = property {
                                    doc.set_name(p, el.name_str().as_bytes());
                                    self.qualify(doc, p, el, style);
                                }
                            }
                        }
                        _ => {
                            let property = if matches!(data, Value::Null) && el.nillable {
                                let p = Self::bogus(doc, node);
                                self.set_xsi_nil(doc, p);
                                Some(p)
                            } else if matches!(data, Value::Null) && model.min_occurs == 0 {
                                return Ok(1);
                            } else {
                                let p = self.master_to_xml(doc, el.encode, &data, style, node)?;
                                if let Some(p) = p {
                                    Self::fixed_check(doc, p, el)?;
                                }
                                p
                            };
                            if let Some(p) = property {
                                doc.set_name(p, el.name_str().as_bytes());
                                self.qualify(doc, p, el, style);
                            }
                        }
                    }
                    Ok(1)
                } else if strict && el.nillable && model.min_occurs > 0 {
                    let p = doc.new_element(el.name_str().as_bytes());
                    let p = doc.append_child(node, p);
                    self.set_xsi_nil(doc, p);
                    self.qualify(doc, p, el, style);
                    Ok(1)
                } else if model.min_occurs == 0 {
                    Ok(2)
                } else {
                    if strict {
                        bail!("Encoding: object has no '{}' property", el.name_str());
                    }
                    Ok(0)
                }
            }
            ModelKind::Any => {
                if let Some(data) = self.get_property(object, b"any")? {
                    let enc = conv(XSD_ANYXML);
                    let multi = model.max_occurs == -1 || model.max_occurs > 1;
                    match &data {
                        Value::Array(a) if multi && is_list(a) => {
                            for val in a.values() {
                                self.master_to_xml(doc, Some(enc), val, style, node)?;
                            }
                        }
                        _ => {
                            self.master_to_xml(doc, Some(enc), &data, style, node)?;
                        }
                    }
                    Ok(1)
                } else if model.min_occurs == 0 {
                    Ok(2)
                } else {
                    if strict {
                        bail!("Encoding: object has no 'any' property");
                    }
                    Ok(0)
                }
            }
            ModelKind::Sequence(c) | ModelKind::All(c) => {
                let mut strict = strict;
                for tmp in c {
                    let r = self.model_to_xml_object(
                        doc,
                        node,
                        sdl,
                        tmp,
                        object,
                        style,
                        strict && tmp.min_occurs > 0,
                    )?;
                    if r == 0 && (!strict || tmp.min_occurs > 0) {
                        return Ok(0);
                    }
                    strict = true;
                }
                Ok(1)
            }
            ModelKind::Choice(c) => {
                let mut ret = 0;
                for tmp in c {
                    let r = self.model_to_xml_object(doc, node, sdl, tmp, object, style, false)?;
                    if r == 1 {
                        return Ok(1);
                    } else if r != 0 {
                        ret = 1;
                    }
                }
                Ok(ret)
            }
            ModelKind::Group(g) => match &sdl.ty(*g).model {
                Some(m) => self.model_to_xml_object(
                    doc,
                    node,
                    sdl,
                    m,
                    object,
                    style,
                    strict && model.min_occurs > 0,
                ),
                None => Ok(1),
            },
            ModelKind::GroupRef(_) => Ok(1),
        }
    }

    fn dummy_value(&mut self, encode: Option<EncRef>, content: &str) -> SoapResult<Value> {
        let mut d = Doc::new();
        let n = d.new_element(b"BOGUS");
        d.append_top(n);
        d.set_content(n, content.as_bytes());
        self.master_to_zval(encode, &d, n)
    }

    fn element_value(
        &mut self,
        el: &SdlType,
        doc: &Doc,
        node: NodeId,
        content_node: NodeId,
    ) -> SoapResult<Value> {
        let content = doc
            .first_child(content_node)
            .filter(|&c| !doc.is_element(c))
            .map(|c| doc.content(c).to_vec());
        if let Some(content) = content {
            if let Some(fixed) = &el.fixed {
                if fixed.as_bytes() != content.as_slice() {
                    bail!(
                        "Encoding: Element '{}' has fixed value '{fixed}' (value '{}' is not allowed)",
                        el.name_str(),
                        lossy(&content)
                    );
                }
            }
            self.master_to_zval(el.encode, doc, content_node)
        } else if let Some(fixed) = &el.fixed {
            self.dummy_value(el.encode, fixed)
        } else if let (Some(def), false) = (&el.def, el.nillable) {
            self.dummy_value(el.encode, def)
        } else {
            self.master_to_zval(el.encode, doc, node)
        }
    }

    /// `model_to_zval_object`.
    fn model_to_zval_object(
        &mut self,
        ret: &mut Value,
        sdl: &Sdl,
        model: &Model,
        doc: &Doc,
        data: NodeId,
    ) -> SoapResult<()> {
        match &model.kind {
            ModelKind::Element(t) => {
                let el = sdl.ty(*t);
                let Some(name) = el.name.clone() else {
                    return Ok(());
                };
                let Some(node) = doc.get_node(doc.first_child(data), &name, None) else {
                    return Ok(());
                };
                let r_node = self.check_and_resolve_href(doc, node)?;
                let mut val = self.element_value(el, doc, r_node, r_node)?;
                let mut next = doc.get_node(doc.next(node), &name, None);
                if next.is_some() {
                    let mut arr = ZArray::new();
                    let _ = arr.make_mut().push(val);
                    while let Some(n) = next {
                        let v = self.element_value(el, doc, n, n)?;
                        let _ = arr.make_mut().push(v);
                        next = doc.get_node(doc.next(n), &name, None);
                    }
                    val = Value::Array(arr);
                } else if (!matches!(val, Value::Null) || !el.nillable)
                    && self.features & SOAP_SINGLE_ELEMENT_ARRAYS != 0
                    && (model.max_occurs == -1 || model.max_occurs > 1)
                {
                    val = Value::Array(ZArray::from_values([val]));
                }
                self.set_property(ret, name.as_bytes(), val)?;
            }
            ModelKind::All(c) | ModelKind::Sequence(c) | ModelKind::Choice(c) => {
                let mut any = false;
                for tmp in c {
                    if matches!(tmp.kind, ModelKind::Any) {
                        any = true;
                    } else {
                        self.model_to_zval_object(ret, sdl, tmp, doc, data)?;
                    }
                }
                if any {
                    self.model_to_zval_any(ret, doc, doc.first_child(data))?;
                }
            }
            ModelKind::Group(g) => {
                if let Some(m) = &sdl.ty(*g).model {
                    self.model_to_zval_object(ret, sdl, m, doc, data)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `model_to_zval_any`.
    fn model_to_zval_any(
        &mut self,
        ret: &mut Value,
        doc: &Doc,
        start: Option<NodeId>,
    ) -> SoapResult<()> {
        let mut any: Option<Value> = None;
        let mut name: Option<Vec<u8>> = None;
        let anyxml = conv(XSD_ANYXML);
        let mut node = start;
        while let Some(mut n) = node {
            if self.get_property(ret, doc.name(n))?.is_none() {
                let mut val = self.master_to_zval(Some(anyxml), doc, n)?;
                if let Some(a) = &any {
                    if !matches!(a, Value::Array(_)) {
                        let mut arr = ZArray::new();
                        match &name {
                            Some(nm) => {
                                arr.make_mut().insert(ArrayKey::from_bytes(nm), a.clone());
                            }
                            None => {
                                let _ = arr.make_mut().push(a.clone());
                            }
                        }
                        any = Some(Value::Array(arr));
                    }
                }
                if matches!(&val, Value::Str(s) if s.as_bytes().first() == Some(&b'<')) {
                    name = None;
                    while let Some(next) = doc.next(n) {
                        let val2 = self.master_to_zval(Some(anyxml), doc, next)?;
                        let Value::Str(s2) = &val2 else {
                            break;
                        };
                        let Value::Str(s1) = &val else {
                            break;
                        };
                        if s1.as_bytes().first() != Some(&b'<') {
                            break;
                        }
                        val = Value::Str(ZStr::concat(s1.as_bytes(), s2.as_bytes()));
                        n = next;
                    }
                } else {
                    name = Some(doc.name(n).to_vec());
                }
                match any.as_mut() {
                    None => {
                        if let Some(nm) = name.take() {
                            any = Some(Value::Array(ZArray::from_pairs([(
                                ArrayKey::from_bytes(&nm),
                                val,
                            )])));
                        } else {
                            any = Some(val);
                        }
                    }
                    Some(Value::Array(arr)) => {
                        if let Some(nm) = name.take() {
                            let key = ArrayKey::from_bytes(&nm);
                            let t = arr.make_mut();
                            match t.get_mut(&key) {
                                Some(Value::Array(el)) => {
                                    let _ = el.make_mut().push(val);
                                }
                                Some(_) => {}
                                None => {
                                    t.insert(key, val);
                                }
                            }
                        } else {
                            let _ = arr.make_mut().push(val);
                        }
                    }
                    Some(_) => {}
                }
            }
            node = doc.next(n);
        }
        if let Some(a) = any {
            let key = name.unwrap_or_else(|| b"any".to_vec());
            self.set_property(ret, &key, a)?;
        }
        Ok(())
    }

    /// `to_zval_object` / `to_zval_object_ex`.
    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
    fn to_zval_object(
        &mut self,
        encode: EncRef,
        doc: &Doc,
        data: NodeId,
        pce: Option<ZStr>,
    ) -> SoapResult<Value> {
        let enc = self.enc(encode);
        let std_class = ZStr::from("stdClass");
        let mut ce = pce.clone().unwrap_or_else(|| std_class.clone());
        if pce.is_none() {
            if let (Some(cm), Some(ts)) = (self.class_map.clone(), &enc.type_str) {
                let mut classname = enc
                    .clark_notation()
                    .and_then(|c| cm.get_str(c.as_bytes()).map(Value::deref));
                if classname.is_none() {
                    classname = cm.get_str(ts.as_bytes()).map(Value::deref);
                }
                if let Some(Value::Str(cn)) = classname {
                    if let Some(c) = self.host.fetch_class(cn.as_bytes())? {
                        ce = c;
                    }
                }
            }
        }
        let is_std = ce.eq_ignore_ascii_case(b"stdClass");
        let Some(t) = enc.sdl_type else {
            if Self::is_nil(doc, data) {
                return Ok(Value::Null);
            }
            if let Some(v) = self.check_xml_ref(doc, data) {
                return Ok(v);
            }
            let mut ret = self.new_object(&ce)?;
            self.add_xml_ref(doc, data, &ret);
            let mut trav = doc.first_child(data);
            while let Some(tr) = trav {
                trav = doc.next(tr);
                if !doc.is_element(tr) {
                    continue;
                }
                let tmp_val = self.master_to_zval(None, doc, tr)?;
                let name = doc.name(tr).to_vec();
                match self.get_property(&ret, &name)? {
                    None => {
                        let later = doc.next(tr).and_then(|n| {
                            doc.siblings_from(Some(n))
                                .find(|&x| doc.name(x) == name.as_slice())
                        });
                        if later.is_none() {
                            self.set_property(&mut ret, &name, tmp_val)?;
                        } else {
                            self.set_property(
                                &mut ret,
                                &name,
                                Value::Array(ZArray::from_values([tmp_val])),
                            )?;
                        }
                    }
                    Some(prop) => {
                        let mut arr = match prop {
                            Value::Array(a) => a,
                            other => ZArray::from_values([other]),
                        };
                        let _ = arr.make_mut().push(tmp_val);
                        self.set_property(&mut ret, &name, Value::Array(arr))?;
                    }
                }
            }
            return Ok(ret);
        };
        let sdl = self.sdl_rc();
        let ty = sdl.ty(t).clone();
        let mut redo_any = false;
        let mut ret;
        if ty.kind == TypeKind::Restriction && ty.encode.is_some() && ty.encode != Some(encode) {
            if let Some(base) = self.simple_base(&sdl, ty.encode) {
                if let Some(v) = self.check_xml_ref(doc, data) {
                    return Ok(v);
                }
                ret = self.new_object(&ce)?;
                let b = self.master_to_zval_int(base, doc, data)?;
                self.set_property(&mut ret, b"_", b)?;
            } else {
                if Self::is_nil(doc, data) {
                    return Ok(Value::Null);
                }
                if let Some(v) = self.check_xml_ref(doc, data) {
                    return Ok(v);
                }
                ret = self.new_object(&ce)?;
                self.add_xml_ref(doc, data, &ret);
            }
        } else if ty.kind == TypeKind::Extension && ty.encode.is_some() && ty.encode != Some(encode)
        {
            let te = ty.encode.unwrap_or(encode);
            let base_t = self.sdl_type_of(te);
            let base_complex = base_t.is_some_and(|bt| {
                !matches!(
                    sdl.ty(bt).kind,
                    TypeKind::Simple | TypeKind::List | TypeKind::Union
                )
            });
            if base_complex {
                if Self::is_nil(doc, data) {
                    return Ok(Value::Null);
                }
                if let Some(v) = self.check_xml_ref(doc, data) {
                    return Ok(v);
                }
                let base_enc = self.enc(te);
                let use_ex = !is_std
                    && base_enc.to_zval == ToZval::SdlGuess
                    && base_t.is_some_and(|bt| {
                        let bty = sdl.ty(bt);
                        matches!(
                            bty.kind,
                            TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension
                        ) && bty.encode.is_none_or(|e| {
                            !matches!(self.enc(e).type_id, IS_ARRAY | SOAP_ENC_ARRAY)
                        })
                    });
                ret = if use_ex {
                    self.to_zval_object(te, doc, data, Some(ce.clone()))?
                } else {
                    self.master_to_zval_int(te, doc, data)?
                };
                self.add_xml_ref(doc, data, &ret);
                redo_any = self.get_property(&ret, b"any")?.is_some();
                if let Value::Object(o) = &ret {
                    if !is_std {
                        self.host.set_object_class(o, &ce);
                    }
                }
            } else {
                if let Some(v) = self.check_xml_ref(doc, data) {
                    return Ok(v);
                }
                ret = self.new_object(&ce)?;
                self.add_xml_ref(doc, data, &ret);
                let b = self.master_to_zval_int(te, doc, data)?;
                self.set_property(&mut ret, b"_", b)?;
            }
        } else {
            if Self::is_nil(doc, data) {
                return Ok(Value::Null);
            }
            if let Some(v) = self.check_xml_ref(doc, data) {
                return Ok(v);
            }
            ret = self.new_object(&ce)?;
            self.add_xml_ref(doc, data, &ret);
        }
        if let Some(m) = &ty.model {
            self.model_to_zval_object(&mut ret, &sdl, m, doc, data)?;
            if redo_any {
                if self.get_property(&ret, b"any")?.is_none() {
                    self.model_to_zval_any(&mut ret, doc, doc.first_child(data))?;
                    self.add_xml_ref(doc, data, &ret);
                } else {
                    self.unset_property(&mut ret, b"any")?;
                }
            }
        }
        if let Some(attrs) = &ty.attributes {
            for attr in attrs.values() {
                let Some(name) = &attr.name else {
                    continue;
                };
                let str_val = match doc.attr_any_ns(data, name.as_bytes()) {
                    Some(a) => {
                        if let Some(fixed) = &attr.fixed {
                            if fixed.as_bytes() != a.value.as_slice() {
                                bail!(
                                    "Encoding: Attribute '{name}' has fixed value '{fixed}' (value '{}' is not allowed)",
                                    lossy(&a.value)
                                );
                            }
                        }
                        Some(a.value.clone())
                    }
                    None => attr
                        .fixed
                        .as_ref()
                        .or(attr.def.as_ref())
                        .map(|s| s.as_bytes().to_vec()),
                };
                if let Some(sv) = str_val {
                    let mut d = Doc::new();
                    let n = d.new_element(b"BOGUS");
                    d.append_top(n);
                    d.add_text(n, &sv);
                    let v = self.master_to_zval(attr.encode, &d, n)?;
                    self.set_property(&mut ret, name.as_bytes(), v)?;
                }
            }
        }
        Ok(ret)
    }

    /// `get_array_type`: the common item type of a PHP array.
    fn get_array_type(
        &mut self,
        doc: &mut Doc,
        node: NodeId,
        array: &Value,
        out: &mut Vec<u8>,
    ) -> SoapResult<Option<EncRef>> {
        let Value::Array(a) = array else {
            out.extend_from_slice(b"xsd:anyType");
            return Ok(Some(conv(XSD_ANYTYPE)));
        };
        let mut i = 0;
        let mut different = false;
        let (mut cur_type, mut prev_type) = (0, 0);
        let (mut cur_stype, mut prev_stype): (Option<ZStr>, Option<ZStr>) = (None, None);
        let (mut cur_ns, mut prev_ns): (Option<ZStr>, Option<ZStr>) = (None, None);
        for tmp in a.values() {
            let tmp = tmp.deref();
            match &tmp {
                Value::Object(o) if is_class(o, "SoapVar") => {
                    let Some(Value::Int(t)) = self.soap_var_slot(o, "enc_type") else {
                        bail!("Encoding: SoapVar has no 'enc_type' property");
                    };
                    cur_type = t;
                    if cur_type == UNKNOWN_TYPE {
                        cur_type =
                            type_code(&self.soap_var_slot(o, "enc_value").unwrap_or(Value::Null));
                    }
                    cur_stype = str_of(self.soap_var_slot(o, "enc_stype").as_ref());
                    cur_ns = str_of(self.soap_var_slot(o, "enc_ns").as_ref());
                }
                Value::Array(inner) if !is_list(inner) => {
                    cur_type = APACHE_MAP;
                    cur_stype = None;
                    cur_ns = None;
                }
                other => {
                    cur_type = type_code(other);
                    cur_stype = None;
                    cur_ns = None;
                }
            }
            if i > 0 {
                let differs = |c: &Option<ZStr>, p: &Option<ZStr>| match (c, p) {
                    (Some(c), Some(p)) => c.as_bytes() != p.as_bytes(),
                    (None, Some(_)) => true,
                    _ => false,
                };
                if cur_type != prev_type
                    || differs(&cur_stype, &prev_stype)
                    || differs(&cur_ns, &prev_ns)
                {
                    different = true;
                    break;
                }
            }
            prev_type = cur_type;
            prev_stype.clone_from(&cur_stype);
            prev_ns.clone_from(&cur_ns);
            i += 1;
        }
        if different || i == 0 {
            out.extend_from_slice(b"xsd:anyType");
            return Ok(Some(conv(XSD_ANYTYPE)));
        }
        if let Some(stype) = cur_stype {
            let mut array_type = Vec::new();
            if let Some(ns) = &cur_ns {
                if let Some(p) = self
                    .encode_add_ns(doc, node, ns.as_bytes())
                    .and_then(|n| n.prefix)
                {
                    out.extend_from_slice(&p);
                }
                out.push(b':');
                array_type.extend_from_slice(ns.as_bytes());
                array_type.push(b':');
            }
            out.extend_from_slice(stype.as_bytes());
            array_type.extend_from_slice(stype.as_bytes());
            let sdl = self.sdl.clone();
            Ok(get_encoder_ex(sdl.as_deref(), &lossy(&array_type)))
        } else {
            let e = self.get_conversion(cur_type)?;
            let enc = self.enc(e);
            let ts = enc.type_str.clone().unwrap_or_else(|| "".into());
            let s = self.get_type_str(
                doc,
                node,
                enc.ns.as_deref().map(str::as_bytes),
                ts.as_bytes(),
            );
            out.extend_from_slice(&s);
            Ok(Some(e))
        }
    }

    fn name_array_item(
        &self,
        doc: &mut Doc,
        x: NodeId,
        type_: Option<TypeId>,
        enc: Option<EncRef>,
        ns: Option<&Ns>,
        style: i64,
    ) {
        if let Some(t) = type_ {
            let sdl = self.sdl_rc();
            doc.set_name(x, sdl.ty(t).name_str().as_bytes());
            return;
        }
        if style == SOAP_LITERAL {
            if let Some(ts) = enc.and_then(|e| self.enc(e).type_str) {
                doc.set_name(x, ts.as_bytes());
                doc.set_ns(x, ns.cloned());
                return;
            }
        }
        doc.set_name(x, b"item");
    }

    /// `add_xml_array_elements`.
    #[allow(clippy::too_many_arguments)]
    fn add_xml_array_elements(
        &mut self,
        doc: &mut Doc,
        xml_param: NodeId,
        type_: Option<TypeId>,
        enc: Option<EncRef>,
        ns: Option<&Ns>,
        dims: &[i32],
        data: Option<&Value>,
        style: i64,
    ) -> SoapResult<()> {
        let dimension = dims.len();
        let d0 = dims.first().copied().unwrap_or(0);
        if let Some(Value::Array(a)) = data {
            if self.protected.contains(&a.addr()) {
                return Err(SoapError::Runtime(RuntimeError::ValueError(
                    "Recursive array cannot be encoded".into(),
                )));
            }
            self.protected.insert(a.addr());
            let r = (|| -> SoapResult<i32> {
                let mut j = 0;
                for zdata in a.values() {
                    if j >= d0 {
                        break;
                    }
                    let zdata = zdata.deref();
                    if dimension == 1 {
                        let e = match enc {
                            Some(e) => e,
                            None => self.get_conversion(type_code(&zdata))?,
                        };
                        if let Some(x) =
                            self.master_to_xml(doc, Some(e), &zdata, style, xml_param)?
                        {
                            self.name_array_item(doc, x, type_, enc, ns, style);
                        }
                    } else {
                        self.add_xml_array_elements(
                            doc,
                            xml_param,
                            type_,
                            enc,
                            ns,
                            &dims[1..],
                            Some(&zdata),
                            style,
                        )?;
                    }
                    j += 1;
                }
                Ok(j)
            })();
            self.protected.remove(&a.addr());
            let mut j = r?;
            while j < d0 {
                if dimension == 1 {
                    let x = Self::bogus(doc, xml_param);
                    self.name_array_item(doc, x, type_, enc, ns, style);
                } else {
                    self.add_xml_array_elements(
                        doc,
                        xml_param,
                        type_,
                        enc,
                        ns,
                        &dims[1..],
                        None,
                        style,
                    )?;
                }
                j += 1;
            }
        } else {
            for _ in 0..d0 {
                if dimension == 1 {
                    let x = Self::bogus(doc, xml_param);
                    self.name_array_item(doc, x, type_, enc, ns, style);
                } else {
                    self.add_xml_array_elements(
                        doc,
                        xml_param,
                        type_,
                        enc,
                        ns,
                        &dims[1..],
                        None,
                        style,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn traversable_to_array(&mut self, o: &Obj) -> SoapResult<Value> {
        let pairs = self.host.iterate(o)?;
        let mut arr = ZArray::new();
        for (k, v) in pairs {
            match k.to_array_key(&mut *self.host)? {
                Some(key) => {
                    arr.make_mut().insert(key, v);
                }
                None => {
                    return Err(SoapError::Runtime(RuntimeError::TypeError(format!(
                        "Cannot access offset of type {} on array",
                        k.type_name()
                    ))))
                }
            }
        }
        Ok(Value::Array(arr))
    }

    /// `to_xml_array`.
    #[allow(clippy::too_many_lines)]
    fn to_xml_array(
        &mut self,
        doc: &mut Doc,
        encode: EncRef,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<NodeId> {
        let enc_d = self.enc(encode);
        let sdl_type = enc_d.sdl_type;
        let xml_param = Self::bogus(doc, parent);
        let enc_ns = if self.soap_version == SOAP_1_1 {
            SOAP_1_1_ENC_NAMESPACE
        } else {
            SOAP_1_2_ENC_NAMESPACE
        };
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, xml_param);
                if self.features & SOAP_USE_XSI_ARRAY_TYPE != 0 {
                    self.set_ns_and_type_ex(doc, xml_param, Some(enc_ns.as_bytes()), b"Array");
                } else {
                    self.set_ns_and_type(doc, xml_param, &enc_d);
                }
            }
            return Ok(xml_param);
        }
        let converted;
        let data = match data {
            Value::Object(o) if self.host.instance_of(o, "Traversable") => {
                converted = self.traversable_to_array(o)?;
                &converted
            }
            other => other,
        };
        if let Value::Array(a) = data {
            let i = i32::try_from(a.len()).unwrap_or(i32::MAX);
            let sdl = self.sdl_rc();
            let mut array_type: Vec<u8> = Vec::new();
            let array_size;
            let mut element_type = None;
            let enc_el;
            let dims: Vec<i32>;
            let k11 = format!("{SOAP_1_1_ENC_NAMESPACE}:arrayType");
            let w_at = format!("{WSDL_NAMESPACE}:arrayType");
            let k_item = format!("{SOAP_1_2_ENC_NAMESPACE}:itemType");
            let w_item = format!("{WSDL_NAMESPACE}:itemType");
            let k_size = format!("{SOAP_1_2_ENC_NAMESPACE}:arraySize");
            let w_size = format!("{WSDL_NAMESPACE}:arraySize");
            let join = |d: &[i32]| {
                d.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            if let Some(ext) = sdl_type
                .and_then(|t| array_extra(&sdl, t, &k11, &w_at))
                .cloned()
            {
                let (value, dimension) = match ext.val.rfind('[') {
                    Some(p) => (
                        ext.val[..p].to_owned(),
                        calc_dimension(&ext.val.as_bytes()[p + 1..]),
                    ),
                    None => (ext.val.clone(), 1),
                };
                if let Some(ns) = &ext.ns {
                    enc_el = get_encoder(Some(&sdl), Some(ns), &value);
                    array_type =
                        self.get_type_str(doc, xml_param, Some(ns.as_bytes()), value.as_bytes());
                } else {
                    enc_el = None;
                    array_type.extend_from_slice(value.as_bytes());
                }
                let mut d = vec![0i32; dimension];
                d[0] = i;
                let mut el = data.clone();
                for slot in d.iter_mut().skip(1) {
                    let first = match &el {
                        Value::Array(x) if !x.is_empty() => x.values().next().map(Value::deref),
                        _ => None,
                    };
                    if let Some(f) = first {
                        el = f;
                        *slot = match &el {
                            Value::Array(x) => i32::try_from(x.len()).unwrap_or(i32::MAX),
                            _ => 0,
                        };
                    }
                }
                array_size = join(&d);
                dims = d;
            } else if let Some(ext) = sdl_type
                .and_then(|t| array_extra(&sdl, t, &k_item, &w_item))
                .cloned()
            {
                if let Some(ns) = &ext.ns {
                    enc_el = get_encoder(Some(&sdl), Some(ns), &ext.val);
                    array_type =
                        self.get_type_str(doc, xml_param, Some(ns.as_bytes()), ext.val.as_bytes());
                } else {
                    enc_el = None;
                    array_type.extend_from_slice(ext.val.as_bytes());
                }
                if let Some(size) = sdl_type
                    .and_then(|t| array_extra(&sdl, t, &k_size, &w_size))
                    .cloned()
                {
                    let dimension = calc_dimension_12(size.val.as_bytes())?;
                    let mut d = get_position_12(dimension, size.val.as_bytes())?;
                    if d[0] == 0 {
                        d[0] = i;
                    }
                    array_size = join(&d);
                    dims = d;
                } else {
                    dims = vec![0];
                    array_size = i.to_string();
                }
            } else if let Some(size) = sdl_type
                .and_then(|t| array_extra(&sdl, t, &k_size, &w_size))
                .cloned()
            {
                let dimension = calc_dimension_12(size.val.as_bytes())?;
                let mut d = get_position_12(dimension, size.val.as_bytes())?;
                if d[0] == 0 {
                    d[0] = i;
                }
                array_size = join(&d);
                dims = d;
                let single = sdl_type.and_then(|t| single_element(&sdl, t)).filter(|&e| {
                    sdl.ty(e)
                        .encode
                        .is_some_and(|x| self.enc(x).type_str.is_some())
                });
                if let Some(et) = single {
                    element_type = Some(et);
                    enc_el = sdl.ty(et).encode;
                    let ee = self.enc(enc_el.unwrap_or(encode));
                    let ts = ee.type_str.clone().unwrap_or_else(|| "".into());
                    array_type = self.get_type_str(
                        doc,
                        xml_param,
                        ee.ns.as_deref().map(str::as_bytes),
                        ts.as_bytes(),
                    );
                } else {
                    enc_el = self.get_array_type(doc, xml_param, data, &mut array_type)?;
                }
            } else if let Some(et) = sdl_type.and_then(|t| single_element(&sdl, t)).filter(|&e| {
                sdl.ty(e)
                    .encode
                    .is_some_and(|x| self.enc(x).type_str.is_some())
            }) {
                element_type = Some(et);
                enc_el = sdl.ty(et).encode;
                let ee = self.enc(enc_el.unwrap_or(encode));
                let ts = ee.type_str.clone().unwrap_or_else(|| "".into());
                array_type = self.get_type_str(
                    doc,
                    xml_param,
                    ee.ns.as_deref().map(str::as_bytes),
                    ts.as_bytes(),
                );
                array_size = i.to_string();
                dims = vec![i];
            } else {
                enc_el = self.get_array_type(doc, xml_param, data, &mut array_type)?;
                array_size = i.to_string();
                dims = vec![i];
            }
            if style == SOAP_ENCODED {
                if self.soap_version == SOAP_1_1 {
                    if array_type == b"xsd:anyType" {
                        array_type = b"xsd:ur-type".to_vec();
                    }
                    array_type.push(b'[');
                    array_type.extend_from_slice(array_size.as_bytes());
                    array_type.push(b']');
                    self.set_ns_prop(
                        doc,
                        xml_param,
                        SOAP_1_1_ENC_NAMESPACE,
                        b"arrayType",
                        &array_type,
                    );
                } else {
                    let size = array_size.replace(',', " ");
                    self.set_ns_prop(
                        doc,
                        xml_param,
                        SOAP_1_2_ENC_NAMESPACE,
                        b"itemType",
                        &array_type,
                    );
                    self.set_ns_prop(
                        doc,
                        xml_param,
                        SOAP_1_2_ENC_NAMESPACE,
                        b"arraySize",
                        size.as_bytes(),
                    );
                }
            }
            let item_ns = match enc_el {
                Some(e) => match self.enc(e).ns {
                    Some(ns) => self.encode_add_ns(doc, xml_param, ns.as_bytes()),
                    None => None,
                },
                None => None,
            };
            self.add_xml_array_elements(
                doc,
                xml_param,
                element_type,
                enc_el,
                item_ns.as_ref(),
                &dims,
                Some(data),
                style,
            )?;
        }
        if style == SOAP_ENCODED {
            if self.features & SOAP_USE_XSI_ARRAY_TYPE != 0 {
                self.set_ns_and_type_ex(doc, xml_param, Some(enc_ns.as_bytes()), b"Array");
            } else {
                self.set_ns_and_type(doc, xml_param, &enc_d);
            }
        }
        Ok(xml_param)
    }

    /// `to_zval_array`.
    #[allow(clippy::too_many_lines)]
    fn to_zval_array(&mut self, encode: EncRef, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        let enc = self.enc(encode);
        let sdl = self.sdl.clone();
        let sdl_ref = sdl.as_deref();
        let mut enc_el = None;
        let mut dimension = 1usize;
        let mut dims: Option<Vec<i32>> = None;
        let k11 = format!("{SOAP_1_1_ENC_NAMESPACE}:arrayType");
        let w_at = format!("{WSDL_NAMESPACE}:arrayType");
        let k_item = format!("{SOAP_1_2_ENC_NAMESPACE}:itemType");
        let w_item = format!("{WSDL_NAMESPACE}:itemType");
        let k_size = format!("{SOAP_1_2_ENC_NAMESPACE}:arraySize");
        let w_size = format!("{WSDL_NAMESPACE}:arraySize");
        let sdl_t = enc.sdl_type;
        let ext = |a: &str, b: &str| -> Option<ExtraAttribute> {
            let s = sdl_ref?;
            array_extra(s, sdl_t?, a, b).cloned()
        };
        if let Some(at) = get_soap_enc_attribute(doc, data, "arrayType") {
            let (prefix, type_) = parse_qname(at);
            let ns = doc.search_ns(data, prefix);
            let mut type_ = type_.to_vec();
            if let Some(p) = type_.iter().rposition(|&c| c == b'[') {
                let rest = type_[p + 1..].to_vec();
                type_.truncate(p);
                dimension = calc_dimension(&rest);
                let mut d = vec![0; dimension];
                get_position_ex(dimension, &rest, &mut d)?;
                dims = Some(d);
            }
            if let Some(ns) = ns {
                enc_el = get_encoder(sdl_ref, Some(&lossy(&ns.href)), &lossy(&type_));
            }
        } else if let Some(it) = get_soap_enc_attribute(doc, data, "itemType") {
            let (prefix, type_) = parse_qname(it);
            if let Some(ns) = doc.search_ns(data, prefix) {
                enc_el = get_encoder(sdl_ref, Some(&lossy(&ns.href)), &lossy(type_));
            }
            if let Some(size) = get_soap_enc_attribute(doc, data, "arraySize") {
                dimension = calc_dimension_12(size)?;
                dims = Some(get_position_12(dimension, size)?);
            } else {
                dims = Some(vec![0]);
            }
        } else if let Some(size) = get_soap_enc_attribute(doc, data, "arraySize") {
            dimension = calc_dimension_12(size)?;
            dims = Some(get_position_12(dimension, size)?);
        } else if let Some(e) = ext(&k11, &w_at) {
            let t = match e.val.rfind('[') {
                Some(p) => e.val[..p].to_owned(),
                None => e.val.clone(),
            };
            if let Some(ns) = &e.ns {
                enc_el = get_encoder(sdl_ref, Some(ns), &t);
            }
            dims = Some(vec![0]);
        } else if let Some(e) = ext(&k_item, &w_item) {
            if let Some(ns) = &e.ns {
                enc_el = get_encoder(sdl_ref, Some(ns), &e.val);
            }
            if let Some(size) = ext(&k_size, &w_size) {
                dimension = calc_dimension_12(size.val.as_bytes())?;
                dims = Some(get_position_12(dimension, size.val.as_bytes())?);
            } else {
                dims = Some(vec![0]);
            }
        } else if let Some(size) = ext(&k_size, &w_size) {
            dimension = calc_dimension_12(size.val.as_bytes())?;
            dims = Some(get_position_12(dimension, size.val.as_bytes())?);
            if let (Some(s), Some(t)) = (sdl_ref, sdl_t) {
                if let Some(et) = single_element(s, t) {
                    if let Some(e) = s.ty(et).encode {
                        enc_el = Some(e);
                    }
                }
            }
        } else if let (Some(s), Some(t)) = (sdl_ref, sdl_t) {
            if let Some(et) = single_element(s, t) {
                if let Some(e) = s.ty(et).encode {
                    enc_el = Some(e);
                }
            }
        }
        let dims = if let Some(d) = dims {
            d
        } else {
            dimension = 1;
            vec![0]
        };
        let dimension = dimension.max(1).min(dims.len().max(1));
        let mut pos = vec![0i32; dimension];
        if let Some(off) = get_soap_enc_attribute(doc, data, "offset") {
            let tmp = match off.iter().rposition(|&c| c == b'[') {
                Some(p) => &off[p..],
                None => off,
            };
            get_position_ex(dimension, tmp, &mut pos)?;
        }
        let mut ret = ZArray::new();
        let mut trav = doc.first_child(data);
        while let Some(tr) = trav {
            trav = doc.next(tr);
            if !doc.is_element(tr) {
                continue;
            }
            let tmp_val = self.master_to_zval(enc_el, doc, tr)?;
            if let Some(p) = get_soap_enc_attribute(doc, tr, "position") {
                let tmp = match p.iter().rposition(|&c| c == b'[') {
                    Some(x) => &p[x..],
                    None => p,
                };
                get_position_ex(dimension, tmp, &mut pos)?;
            }
            insert_at(&mut ret, &pos, tmp_val);
            let mut i = dimension;
            while i > 0 {
                i -= 1;
                if pos[i] == i32::MAX {
                    bail!("Encoding: array index out of range");
                }
                pos[i] += 1;
                if pos[i] < dims.get(i).copied().unwrap_or(0) {
                    break;
                }
                if i > 0 {
                    pos[i] = 0;
                }
            }
        }
        Ok(Value::Array(ret))
    }

    /// `to_xml_map`.
    fn to_xml_map(
        &mut self,
        doc: &mut Doc,
        enc: &Encoder,
        data: &Value,
        style: i64,
        parent: NodeId,
    ) -> SoapResult<Option<NodeId>> {
        let xml_param = Self::bogus(doc, parent);
        if matches!(data, Value::Null) {
            if style == SOAP_ENCODED {
                self.set_xsi_nil(doc, xml_param);
            }
            return Ok(Some(xml_param));
        }
        if let Value::Array(a) = data {
            if self.protected.contains(&a.addr()) {
                return Err(SoapError::Runtime(RuntimeError::ValueError(
                    "Recursive array cannot be encoded".into(),
                )));
            }
            self.protected.insert(a.addr());
            let r = (|| -> SoapResult<()> {
                for (k, v) in a.iter() {
                    let item = doc.add_element(xml_param, b"item");
                    let key = doc.add_element(item, b"key");
                    match k {
                        ArrayKey::Str(s) => {
                            if style == SOAP_ENCODED {
                                self.set_xsi_type(doc, key, b"xsd:string");
                            }
                            doc.set_content(key, s.as_bytes());
                        }
                        ArrayKey::Int(i) => {
                            if style == SOAP_ENCODED {
                                self.set_xsi_type(doc, key, b"xsd:int");
                            }
                            doc.set_content(key, i.to_string().as_bytes());
                        }
                    }
                    let v = v.deref();
                    let e = self.get_conversion(type_code(&v))?;
                    if let Some(x) = self.master_to_xml(doc, Some(e), &v, style, item)? {
                        doc.set_name(x, b"value");
                    }
                }
                Ok(())
            })();
            self.protected.remove(&a.addr());
            r?;
        }
        if style == SOAP_ENCODED {
            self.set_ns_and_type(doc, xml_param, enc);
        }
        Ok(Some(xml_param))
    }

    /// `to_zval_map`.
    fn to_zval_map(&mut self, doc: &Doc, data: NodeId) -> SoapResult<Value> {
        if Self::is_nil(doc, data) {
            return Ok(Value::Null);
        }
        if doc.first_child(data).is_none() {
            return Ok(Value::Null);
        }
        let mut ret = ZArray::new();
        let mut trav = doc.first_child(data);
        while let Some(start) = trav {
            let Some(item) = doc.get_node(Some(start), "item", None) else {
                break;
            };
            let Some(key) = doc.get_node(doc.first_child(item), "key", None) else {
                bail!("Encoding: Can't decode apache map, missing key");
            };
            let Some(value) = doc.get_node(doc.first_child(item), "value", None) else {
                bail!("Encoding: Can't decode apache map, missing value");
            };
            let k = self.master_to_zval(None, doc, key)?;
            let v = self.master_to_zval(None, doc, value)?;
            match k {
                Value::Str(s) => {
                    ret.make_mut().insert(ArrayKey::from_zstr(&s), v);
                }
                Value::Int(i) => {
                    ret.make_mut().insert(i, v);
                }
                _ => bail!(
                    "Encoding: Can't decode apache map, only Strings or Longs are allowed as keys"
                ),
            }
            trav = doc.next(item);
        }
        Ok(Value::Array(ret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_php() {
        assert_eq!(base64_encode(b"\x00\xff\xfe abc"), b"AP/+IGFiYw==");
        assert_eq!(base64_encode(b""), b"");
        assert_eq!(base64_encode(b"a"), b"YQ==");
        assert_eq!(base64_encode(b"ab"), b"YWI=");
        // Non-strict decoding skips foreign characters and padding.
        assert_eq!(base64_decode(b"QU Jj\nZA=="), b"ABcd");
        assert_eq!(base64_decode(b"QUJ*jZA"), b"ABcd");
        for data in [&b""[..], b"x", b"xy", b"xyz", b"\x00\x01\x02\x03\xff"] {
            assert_eq!(base64_decode(&base64_encode(data)), data);
        }
    }

    #[test]
    fn utf8_validation_stops_at_nul() {
        assert!(check_utf8(b"plain"));
        assert!(check_utf8("caf\u{e9} \u{20ac} \u{1f600}".as_bytes()));
        assert!(!check_utf8(b"ab\xffcd"));
        assert!(!check_utf8(b"x\xc3"));
        assert!(check_utf8(b"ok\x00\xff"));
        assert_eq!(utf8_error_excerpt(b"ab\xffcd"), "ab\\xff...");
        assert_eq!(utf8_error_excerpt(b"x\xc3"), "x\\xc3...");
    }

    #[test]
    fn soap12_array_dimensions() {
        assert_eq!(calc_dimension_12(b"2 3").unwrap(), 2);
        assert_eq!(calc_dimension_12(b"* 3").unwrap(), 2);
        assert_eq!(calc_dimension_12(b"5").unwrap(), 1);
        assert!(calc_dimension_12(b"3 *").is_err());
        assert_eq!(get_position_12(2, b"1 12").unwrap(), vec![1, 12]);
        assert!(get_position_12(1, b"99999999999").is_err());
    }

    #[test]
    fn soap11_array_positions() {
        assert_eq!(calc_dimension(b"[2,3]"), 2);
        assert_eq!(calc_dimension(b"[4]"), 1);
        let mut pos = [0; 2];
        get_position_ex(2, b"[1,2]", &mut pos).unwrap();
        assert_eq!(pos, [1, 2]);
        get_position_ex(2, b"[7]", &mut pos).unwrap();
        assert_eq!(pos, [7, 0]);
        let mut arr = ZArray::new();
        insert_at(&mut arr, &[1, 0], Value::Int(5));
        let Some(Value::Array(inner)) = arr.get(&ArrayKey::Int(1)) else {
            panic!("nested array expected");
        };
        assert!(matches!(inner.get(&ArrayKey::Int(0)), Some(Value::Int(5))));
    }

    #[test]
    fn value_classification() {
        assert_eq!(type_code(&Value::Null), IS_NULL);
        assert_eq!(type_code(&Value::Bool(true)), IS_TRUE);
        assert_eq!(type_code(&Value::Int(1)), IS_LONG);
        assert_eq!(type_code(&Value::from("s")), IS_STRING);
        let mut a = ZArray::new();
        a.make_mut().insert(0i64, Value::Int(1));
        a.make_mut().insert(1i64, Value::Int(2));
        assert!(is_list(&a));
        a.make_mut().insert(5i64, Value::Int(3));
        assert!(!is_list(&a));
        assert_eq!(unmangle(b"\0Foo\0bar"), b"bar");
        assert_eq!(unmangle(b"\0*\0prot"), b"prot");
        assert_eq!(unmangle(b"plain"), b"plain");
    }
}
