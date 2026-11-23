// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `var_dump`, `print_r` and `var_export` (`ext/standard/var.c`,
//! `Zend/zend.c`), byte-for-byte compatible with PHP.
//!
//! Output is appended to a caller-supplied buffer so that partial output
//! survives when an object hook throws midway (as in PHP, where these
//! functions write as they go).

use crate::array::ArrayKey;
use crate::error::{
    ErrorLevel, ObjectDump, ObjectKind, PropertyPurpose, PropertySlot, RtResult, RuntimeContext,
};
use crate::float::{format_double, format_double_h};
use crate::value::{Obj, Value};

/// Splits a mangled property name (`"\0Class\0prop"`, `"\0*\0prop"`) into
/// `(class, prop)` like `zend_unmangle_property_name_ex`. Returns `None` for
/// the class when the name is not mangled (or is malformed).
#[must_use]
pub fn unmangle_property_name(name: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if name.first() != Some(&0) {
        return (None, name);
    }
    if name.len() < 3 || name[1] == 0 {
        return (None, name);
    }
    let rest = &name[1..name.len() - 1];
    let Some(class_len) = rest.iter().position(|&c| c == 0) else {
        return (None, name);
    };
    let class = &name[1..=class_len];
    // Anonymous class names contain a NUL themselves ("class@anonymous\0file").
    let after = &name[class_len + 2..];
    let anon_len = after.iter().position(|&c| c == 0).unwrap_or(after.len());
    if class_len + anon_len + 2 != name.len() {
        let full_class_len = class_len + anon_len + 1;
        return (Some(&name[1..=full_class_len]), &name[full_class_len + 2..]);
    }
    (Some(class), after)
}

/// Bytes up to the first NUL (C-string semantics of `%s`).
fn cstr(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |p| &b[..p])
}

fn spaces(out: &mut Vec<u8>, n: usize) {
    out.extend(std::iter::repeat_n(b' ', n));
}

/// Tracks the arrays/objects currently being printed (recursion protection).
#[derive(Default)]
struct Visiting(Vec<usize>);

impl Visiting {
    fn contains(&self, addr: usize) -> bool {
        self.0.contains(&addr)
    }
    fn push(&mut self, addr: usize) {
        self.0.push(addr);
    }
    fn pop(&mut self) {
        self.0.pop();
    }
}

// ---------------------------------------------------------------------------
// var_dump
// ---------------------------------------------------------------------------

/// `var_dump($value)`: appends the dump of one value (level 1) to `out`.
pub fn var_dump(
    out: &mut Vec<u8>,
    value: &Value,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    let mut visiting = Visiting::default();
    var_dump_level(out, value, 1, &mut visiting, ctx)
}

fn var_dump_level(
    out: &mut Vec<u8>,
    value: &Value,
    level: usize,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    if level > 1 {
        spaces(out, level - 1);
    }
    let mut is_ref = false;
    let deref;
    let value = match value {
        Value::Reference(r) => {
            // php-src prints the `&` marker only when a reference has `refcount > 1`
            // (i.e. truly shared, matching `mysqli_result_references`). If `r.refcount() == 1`,
            // format the inner value without the leading `&`.
            is_ref = r.refcount() > 1;
            deref = r.get();
            &deref
        }
        v => v,
    };
    let common: &[u8] = if is_ref { b"&" } else { b"" };
    match value {
        Value::Null => {
            out.extend_from_slice(common);
            out.extend_from_slice(b"NULL\n");
        }
        Value::Bool(b) => {
            out.extend_from_slice(common);
            out.extend_from_slice(if *b {
                b"bool(true)\n"
            } else {
                b"bool(false)\n"
            });
        }
        Value::Int(i) => {
            out.extend_from_slice(common);
            out.extend_from_slice(format!("int({i})\n").as_bytes());
        }
        Value::Float(f) => {
            out.extend_from_slice(common);
            let s = format_double_h(*f, ctx.serialize_precision());
            out.extend_from_slice(format!("float({s})\n").as_bytes());
        }
        Value::Str(s) => {
            out.extend_from_slice(common);
            out.extend_from_slice(format!("string({}) \"", s.len()).as_bytes());
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\"\n");
        }
        Value::Array(a) => {
            if visiting.contains(a.addr()) {
                out.extend_from_slice(b"*RECURSION*\n");
                return Ok(());
            }
            visiting.push(a.addr());
            out.extend_from_slice(common);
            out.extend_from_slice(format!("array({}) {{\n", a.len()).as_bytes());
            let a = a.clone();
            let result = (|| -> RtResult<()> {
                for (k, v) in a.iter() {
                    dump_key(out, k, level);
                    var_dump_level(out, v, level + 2, visiting, ctx)?;
                }
                Ok(())
            })();
            visiting.pop();
            result?;
            if level > 1 {
                spaces(out, level - 1);
            }
            out.extend_from_slice(b"}\n");
        }
        Value::Object(o) => return var_dump_object(out, o, common, level, visiting, ctx),
        Value::Resource(r) => {
            out.extend_from_slice(common);
            let t = r
                .type_name()
                .map_or_else(|| b"Unknown".to_vec(), |t| t.as_bytes().to_vec());
            out.extend_from_slice(format!("resource({}) of type (", r.handle()).as_bytes());
            out.extend_from_slice(&t);
            out.extend_from_slice(b")\n");
        }
        Value::Reference(_) => unreachable!("references never nest"),
    }
    Ok(())
}

fn dump_key(out: &mut Vec<u8>, k: &ArrayKey, level: usize) {
    spaces(out, level + 1);
    match k {
        ArrayKey::Int(i) => out.extend_from_slice(format!("[{i}]=>\n").as_bytes()),
        ArrayKey::Str(s) => {
            out.extend_from_slice(b"[\"");
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\"]=>\n");
        }
    }
}

fn dump_property_key(out: &mut Vec<u8>, k: &ArrayKey, level: usize) {
    spaces(out, level + 1);
    match k {
        ArrayKey::Int(i) => out.extend_from_slice(format!("[{i}]=>\n").as_bytes()),
        ArrayKey::Str(s) => {
            out.push(b'[');
            if let (Some(class), prop) = unmangle_property_name(s.as_bytes()) {
                out.push(b'"');
                out.extend_from_slice(cstr(prop));
                if class.first() == Some(&b'*') {
                    out.extend_from_slice(b"\":protected");
                } else {
                    out.extend_from_slice(b"\":\"");
                    out.extend_from_slice(cstr(class));
                    out.extend_from_slice(b"\":private");
                }
            } else {
                out.push(b'"');
                out.extend_from_slice(s.as_bytes());
                out.push(b'"');
            }
            out.extend_from_slice(b"]=>\n");
        }
    }
}

fn var_dump_object(
    out: &mut Vec<u8>,
    o: &Obj,
    common: &[u8],
    level: usize,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    let class = o.class_name();
    // Enums without __debugInfo print just the case.
    let info = if visiting.contains(o.addr()) {
        out.extend_from_slice(b"*RECURSION*\n");
        return Ok(());
    } else {
        visiting.push(o.addr());
        let r = ctx.object_dump(o, PropertyPurpose::Debug);
        match r {
            Ok(info) => info,
            Err(e) => {
                visiting.pop();
                return Err(e);
            }
        }
    };
    let ObjectDump { kind, properties } = info;
    if let ObjectKind::Enum {
        case_name,
        has_debug_info: false,
        ..
    } = &kind
    {
        visiting.pop();
        out.extend_from_slice(common);
        out.extend_from_slice(b"enum(");
        out.extend_from_slice(class.as_bytes());
        out.extend_from_slice(b"::");
        out.extend_from_slice(case_name.as_bytes());
        out.extend_from_slice(b")\n");
        return Ok(());
    }
    let count = properties.as_ref().map_or(0, |p| {
        p.iter()
            .filter(|(_, s)| matches!(s, PropertySlot::Value(_)))
            .count()
    });
    out.extend_from_slice(common);
    match &kind {
        ObjectKind::Enum { case_name, .. } => {
            out.extend_from_slice(b"enum(");
            out.extend_from_slice(class.as_bytes());
            out.extend_from_slice(b"::");
            out.extend_from_slice(case_name.as_bytes());
            out.extend_from_slice(format!(") ({count}) {{\n").as_bytes());
        }
        ObjectKind::Regular { lazy_prefix } => {
            out.extend_from_slice(lazy_prefix.as_bytes());
            out.extend_from_slice(b"object(");
            out.extend_from_slice(class.as_bytes());
            out.extend_from_slice(format!(")#{} ({count}) {{\n", o.handle()).as_bytes());
        }
        ObjectKind::StdClass => {
            out.extend_from_slice(b"object(");
            out.extend_from_slice(class.as_bytes());
            out.extend_from_slice(format!(")#{} ({count}) {{\n", o.handle()).as_bytes());
        }
    }
    let result = (|| -> RtResult<()> {
        for (k, slot) in properties.iter().flatten() {
            dump_property_key(out, k, level);
            match slot {
                PropertySlot::Value(v) => var_dump_level(out, v, level + 2, visiting, ctx)?,
                PropertySlot::Uninitialized(t) => {
                    spaces(out, level + 1);
                    out.extend_from_slice(b"uninitialized(");
                    out.extend_from_slice(t.as_bytes());
                    out.extend_from_slice(b")\n");
                }
            }
        }
        Ok(())
    })();
    visiting.pop();
    result?;
    if level > 1 {
        spaces(out, level - 1);
    }
    out.extend_from_slice(b"}\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// print_r
// ---------------------------------------------------------------------------

/// `print_r($value, true)`: returns the `print_r` representation.
pub fn print_r(value: &Value, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Vec<u8>> {
    let mut out = Vec::new();
    print_r_into(&mut out, value, ctx)?;
    Ok(out)
}

/// Appends the `print_r` representation of `value` to `out`.
pub fn print_r_into(
    out: &mut Vec<u8>,
    value: &Value,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    let mut visiting = Visiting::default();
    print_r_level(out, value, 0, &mut visiting, ctx)
}

fn print_r_level(
    out: &mut Vec<u8>,
    value: &Value,
    indent: usize,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    match value {
        Value::Array(a) => {
            out.extend_from_slice(b"Array\n");
            if visiting.contains(a.addr()) {
                out.extend_from_slice(b" *RECURSION*");
                return Ok(());
            }
            visiting.push(a.addr());
            let entries: Vec<(ArrayKey, PropertySlot)> = a
                .iter()
                .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                .collect();
            let r = print_hash(out, &entries, indent, false, visiting, ctx);
            visiting.pop();
            r
        }
        Value::Object(o) => {
            let class = o.class_name();
            out.extend_from_slice(cstr(class.as_bytes()));
            if visiting.contains(o.addr()) {
                out.extend_from_slice(b" Object\n *RECURSION*");
                return Ok(());
            }
            let info = ctx.object_dump(o, PropertyPurpose::Debug)?;
            match &info.kind {
                ObjectKind::Enum { backing_type, .. } => {
                    out.extend_from_slice(b" Enum");
                    if let Some(t) = backing_type {
                        out.push(b':');
                        out.extend_from_slice(t.as_bytes());
                    }
                    out.push(b'\n');
                }
                _ => out.extend_from_slice(b" Object\n"),
            }
            visiting.push(o.addr());
            let entries = info.properties.unwrap_or_default();
            let r = print_hash(out, &entries, indent, true, visiting, ctx);
            visiting.pop();
            r
        }
        Value::Float(f) => {
            out.extend_from_slice(format_double(*f, ctx.precision(), false).as_bytes());
            Ok(())
        }
        Value::Reference(r) => {
            let v = r.get();
            print_r_level(out, &v, indent, visiting, ctx)
        }
        other => {
            let s = other.to_zstr(ctx)?;
            out.extend_from_slice(s.as_bytes());
            Ok(())
        }
    }
}

fn print_hash(
    out: &mut Vec<u8>,
    entries: &[(ArrayKey, PropertySlot)],
    indent: usize,
    is_object: bool,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    spaces(out, indent);
    out.extend_from_slice(b"(\n");
    let inner = indent + 4;
    for (k, slot) in entries {
        let PropertySlot::Value(v) = slot else {
            continue;
        };
        spaces(out, inner);
        out.push(b'[');
        match k {
            ArrayKey::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
            ArrayKey::Str(s) if is_object => match unmangle_property_name(s.as_bytes()) {
                (Some(class), prop) => {
                    out.extend_from_slice(prop);
                    if class.first() == Some(&b'*') {
                        out.extend_from_slice(b":protected");
                    } else {
                        out.push(b':');
                        out.extend_from_slice(cstr(class));
                        out.extend_from_slice(b":private");
                    }
                }
                (None, prop) => out.extend_from_slice(prop),
            },
            ArrayKey::Str(s) => out.extend_from_slice(s.as_bytes()),
        }
        out.extend_from_slice(b"] => ");
        print_r_level(out, v, inner + 4, visiting, ctx)?;
        out.push(b'\n');
    }
    spaces(out, indent);
    out.extend_from_slice(b")\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// var_export
// ---------------------------------------------------------------------------

/// `var_export($value, true)`: returns the exported representation. Circular
/// references are exported as `NULL` with the warning "`var_export` does not
/// handle circular references".
pub fn var_export(value: &Value, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut visiting = Visiting::default();
    var_export_level(&mut out, value, 1, &mut visiting, ctx)?;
    Ok(out)
}

/// Escapes a string for `var_export`: `'` and `\` are backslashed and NUL
/// bytes become `' . "\0" . '`.
#[must_use]
pub fn export_string(s: &[u8]) -> Vec<u8> {
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: the length of an existing string"
    )]
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'\'');
    for &c in s {
        match c {
            b'\'' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            0 => out.extend_from_slice(b"' . \"\\0\" . '"),
            _ => out.push(c),
        }
    }
    out.push(b'\'');
    out
}

fn circular(out: &mut Vec<u8>, ctx: &mut (impl RuntimeContext + ?Sized)) -> RtResult<()> {
    out.extend_from_slice(b"NULL");
    ctx.diagnostic(
        ErrorLevel::Warning,
        "var_export does not handle circular references",
    )
}

fn var_export_level(
    out: &mut Vec<u8>,
    value: &Value,
    level: usize,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    match value {
        Value::Null | Value::Resource(_) => out.extend_from_slice(b"NULL"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Int(i) => {
            if *i == i64::MIN {
                out.extend_from_slice(b"-9223372036854775807-1");
            } else {
                out.extend_from_slice(i.to_string().as_bytes());
            }
        }
        Value::Float(f) => {
            out.extend_from_slice(format_double(*f, ctx.serialize_precision(), true).as_bytes());
        }
        Value::Str(s) => out.extend_from_slice(&export_string(s.as_bytes())),
        Value::Array(a) => {
            if visiting.contains(a.addr()) {
                return circular(out, ctx);
            }
            visiting.push(a.addr());
            if level > 1 {
                out.push(b'\n');
                spaces(out, level - 1);
            }
            out.extend_from_slice(b"array (\n");
            let a = a.clone();
            let r = (|| -> RtResult<()> {
                for (k, v) in a.iter() {
                    spaces(out, level + 1);
                    match k {
                        ArrayKey::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
                        ArrayKey::Str(s) => out.extend_from_slice(&export_string(s.as_bytes())),
                    }
                    out.extend_from_slice(b" => ");
                    var_export_level(out, v, level + 2, visiting, ctx)?;
                    out.extend_from_slice(b",\n");
                }
                Ok(())
            })();
            visiting.pop();
            r?;
            if level > 1 {
                spaces(out, level - 1);
            }
            out.push(b')');
        }
        Value::Object(o) => return var_export_object(out, o, level, visiting, ctx),
        Value::Reference(r) => {
            let v = r.get();
            return var_export_level(out, &v, level, visiting, ctx);
        }
    }
    Ok(())
}

fn var_export_object(
    out: &mut Vec<u8>,
    o: &Obj,
    level: usize,
    visiting: &mut Visiting,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    if visiting.contains(o.addr()) {
        return circular(out, ctx);
    }
    visiting.push(o.addr());
    let info = match ctx.object_dump(o, PropertyPurpose::VarExport) {
        Ok(i) => i,
        Err(e) => {
            visiting.pop();
            return Err(e);
        }
    };
    if level > 1 {
        out.push(b'\n');
        spaces(out, level - 1);
    }
    let class = o.class_name();
    let is_enum = matches!(info.kind, ObjectKind::Enum { .. });
    match &info.kind {
        ObjectKind::StdClass => out.extend_from_slice(b"(object) array(\n"),
        ObjectKind::Enum { case_name, .. } => {
            out.push(b'\\');
            out.extend_from_slice(class.as_bytes());
            out.extend_from_slice(b"::");
            out.extend_from_slice(case_name.as_bytes());
        }
        ObjectKind::Regular { .. } => {
            out.push(b'\\');
            out.extend_from_slice(class.as_bytes());
            out.extend_from_slice(b"::__set_state(array(\n");
        }
    }
    let r = (|| -> RtResult<()> {
        if is_enum {
            return Ok(());
        }
        for (k, slot) in info.properties.iter().flatten() {
            let PropertySlot::Value(v) = slot else {
                continue;
            };
            spaces(out, level + 2);
            match k {
                ArrayKey::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
                ArrayKey::Str(s) => {
                    let (_, prop) = unmangle_property_name(s.as_bytes());
                    out.extend_from_slice(&export_string(prop));
                }
            }
            out.extend_from_slice(b" => ");
            var_export_level(out, v, level + 2, visiting, ctx)?;
            out.extend_from_slice(b",\n");
        }
        Ok(())
    })();
    visiting.pop();
    r?;
    if level > 1 && !is_enum {
        spaces(out, level - 1);
    }
    match info.kind {
        ObjectKind::StdClass => out.push(b')'),
        ObjectKind::Regular { .. } => out.extend_from_slice(b"))"),
        ObjectKind::Enum { .. } => {}
    }
    Ok(())
}
