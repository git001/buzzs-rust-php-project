// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! String functions: minimal bindings of `php-stdstr` kept so the bare VM
//! (and its unit tests) has the common string builtins. The `php` binary
//! links `php-ext-stdstr`, which re-registers the string.c/html.c/url.c/math.c
//! ones with full PHP semantics (md5/sha1/crc32, basename/dirname, uniqid
//! and `lcg_value` belong to other bindings); fix behaviour there, not here.

use super::util::{arr, b, i, opt, s};
use super::{alias, constant, reg};
use crate::func::Args;
use crate::vm::{Vm, VmResult};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_stdstr::error::{Level, Notices, StdError, StdResult};
use php_stdstr::{analysis, compare, html, math, search, slashes, transform};

impl Vm {
    /// Emits `php-stdstr` notices.
    pub(crate) fn std_notices(&mut self, notices: Notices) -> VmResult<()> {
        for n in notices {
            let level = match n.level {
                Level::Warning => 2,
                Level::Notice => 8,
                Level::Deprecated => 8192,
            };
            self.error(level, &n.to_string())?;
        }
        Ok(())
    }

    /// Converts a `php-stdstr` error to a throw.
    pub(crate) fn std_err(&mut self, e: StdError) -> crate::vm::Throw {
        let msg = e.to_string();
        self.make_throw(e.kind.class_name(), &msg)
    }

    fn std<T>(&mut self, r: StdResult<T>) -> VmResult<T> {
        r.map_err(|e| self.std_err(e))
    }
}

fn bytes(v: Vec<u8>) -> Value {
    Value::Str(ZStr::from(v))
}

fn implode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let first = a.at(0);
    let (sep, pieces) = match a.get(1) {
        None | Some(Value::Null) => match first {
            Value::Array(p) => (ZStr::empty(), p),
            other => {
                return Err(vm.make_throw(
                    "TypeError",
                    &format!(
                        "implode(): Argument #1 ($array) must be of type array, {} given",
                        crate::typecheck::given_type(&other)
                    ),
                ))
            }
        },
        Some(Value::Array(p)) => (vm.to_zstr(&first)?, p),
        Some(other) => {
            return Err(vm.make_throw(
                "TypeError",
                &format!(
                    "implode(): Argument #2 ($array) must be of type ?array, {} given",
                    crate::typecheck::given_type(&other)
                ),
            ))
        }
    };
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    for (_, v) in pieces.iter() {
        if !first {
            out.extend_from_slice(sep.as_bytes());
        }
        first = false;
        let v = v.deref();
        match &v {
            Value::Str(x) => out.extend_from_slice(x.as_bytes()),
            Value::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            other => {
                let z = vm.to_zstr(other)?;
                out.extend_from_slice(z.as_bytes());
            }
        }
    }
    Ok(bytes(out))
}

fn explode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = s(a, 0);
    let st = s(a, 1);
    let limit = if a.has(2) { i(a, 2) } else { i64::MAX };
    let r = search::explode(d.as_bytes(), st.as_bytes(), limit);
    let parts = vm.std(r)?;
    Ok(Value::Array(ZArray::from_values(
        parts.into_iter().map(bytes),
    )))
}

fn str_slice<'a>(a: &'a Args, i: usize, fallback: &'a mut Option<ZStr>) -> &'a [u8] {
    match a.values.get(i) {
        Some(Value::Str(s)) => s.as_bytes(),
        Some(Value::Reference(r)) => {
            *fallback = match &*r.borrow() {
                Value::Str(s) => Some(s.clone()),
                Value::Int(n) => Some(ZStr::from(n.to_string())),
                _ => Some(ZStr::empty()),
            };
            fallback.as_ref().unwrap().as_bytes()
        }
        _ => {
            *fallback = Some(s(a, i));
            fallback.as_ref().unwrap().as_bytes()
        }
    }
}

fn substr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let st = s(a, 0);
    let len = opt(a, 2).map(|_| i(a, 2));
    let sub = search::substr(st.as_bytes(), i(a, 1), len);
    if sub.len() == st.len() {
        return Ok(Value::Str(st));
    }
    if sub.is_empty() {
        return Ok(Value::Str(ZStr::empty()));
    }
    let out = ZStr::from(sub);
    if st.is_valid_utf8() && std::str::from_utf8(sub).is_ok() {
        out.mark_valid_utf8();
    }
    Ok(Value::Str(out))
}

fn pos_result(r: Option<i64>) -> Value {
    r.map_or(Value::Bool(false), Value::Int)
}

fn strpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut fb0 = None;
    let mut fb1 = None;
    let h = str_slice(a, 0, &mut fb0);
    let n = str_slice(a, 1, &mut fb1);
    let r = search::strpos(h, n, i(a, 2));
    Ok(pos_result(vm.std(r)?))
}
fn stripos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut fb0 = None;
    let mut fb1 = None;
    let h = str_slice(a, 0, &mut fb0);
    let n = str_slice(a, 1, &mut fb1);
    let r = search::stripos(h, n, i(a, 2));
    Ok(pos_result(vm.std(r)?))
}
fn strrpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut fb0 = None;
    let mut fb1 = None;
    let h = str_slice(a, 0, &mut fb0);
    let n = str_slice(a, 1, &mut fb1);
    let r = search::strrpos(h, n, i(a, 2));
    Ok(pos_result(vm.std(r)?))
}
fn strripos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut fb0 = None;
    let mut fb1 = None;
    let h = str_slice(a, 0, &mut fb0);
    let n = str_slice(a, 1, &mut fb1);
    let r = search::strripos(h, n, i(a, 2));
    Ok(pos_result(vm.std(r)?))
}

fn opt_bytes(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Bool(false), bytes)
}

fn strstr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::strstr(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        b(a, 2),
    )))
}
fn stristr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::stristr(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        b(a, 2),
    )))
}
fn strrchr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::strrchr(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        b(a, 2),
    )))
}
fn str_contains(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_contains(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
    )))
}
fn str_starts_with(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_starts_with(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
    )))
}
fn str_ends_with(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_ends_with(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
    )))
}

fn substr_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = opt(a, 3).map(|_| i(a, 3));
    let r = search::substr_count(s(a, 0).as_bytes(), s(a, 1).as_bytes(), i(a, 2), len);
    Ok(Value::Int(vm.std(r)?))
}

fn strspn(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = opt(a, 3).map(|_| i(a, 3));
    Ok(Value::Int(search::strspn(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        i(a, 2),
        len,
    )))
}
fn strcspn(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = opt(a, 3).map(|_| i(a, 3));
    Ok(Value::Int(search::strcspn(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        i(a, 2),
        len,
    )))
}

fn strpbrk(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strpbrk(s(a, 0).as_bytes(), s(a, 1).as_bytes());
    Ok(opt_bytes(vm.std(r)?))
}

fn substr_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let subj = a.at(0);
    let rep = a.at(1);
    let off = a.at(2);
    let len = a.get(3).filter(|v| !v.is_null());
    let one = |vm: &mut Vm, st: &[u8], idx: usize| -> VmResult<Vec<u8>> {
        let r = match &rep {
            Value::Array(ra) => ra
                .iter()
                .nth(idx)
                .map(|(_, v)| v.deref())
                .unwrap_or(Value::Str(ZStr::empty())),
            v => v.clone(),
        };
        let r = vm.to_zstr(&r)?;
        let o = match &off {
            Value::Array(oa) => oa
                .iter()
                .nth(idx)
                .map_or(0, |(_, v)| v.deref().as_int().unwrap_or(0)),
            v => vm.to_long(v)?,
        };
        let l = match &len {
            Some(Value::Array(la)) => la
                .iter()
                .nth(idx)
                .map(|(_, v)| v.deref().as_int().unwrap_or(0)),
            Some(v) => Some(vm.to_long(v)?),
            None => None,
        };
        Ok(search::substr_replace(st, r.as_bytes(), o, l))
    };
    match subj {
        Value::Array(sa) => {
            let mut t = HashTable::new();
            for (n, (k, v)) in sa.iter().enumerate() {
                let st = vm.to_zstr(&v.deref())?;
                let r = one(vm, st.as_bytes(), n)?;
                t.insert(k.clone(), bytes(r));
            }
            Ok(Value::Array(ZArray::from_table(t)))
        }
        other => {
            let st = vm.to_zstr(&other)?;
            Ok(bytes(one(vm, st.as_bytes(), 0)?))
        }
    }
}

fn strtolower(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::strtolower(s(a, 0).as_bytes())))
}
fn strtoupper(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::strtoupper(s(a, 0).as_bytes())))
}
fn ucfirst(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::ucfirst(s(a, 0).as_bytes())))
}
fn lcfirst(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::lcfirst(s(a, 0).as_bytes())))
}
fn ucwords(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = if a.has(1) {
        s(a, 1)
    } else {
        ZStr::from(" \t\r\n\x0c\x0b")
    };
    let mut n = Notices::new();
    let r = transform::ucwords(s(a, 0).as_bytes(), d.as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(bytes(r))
}

fn trim_impl(vm: &mut Vm, a: &Args, mode: transform::TrimMode) -> VmResult<Value> {
    let st = s(a, 0);
    let mut fb = None;
    let what = if a.has(1) {
        Some(str_slice(a, 1, &mut fb))
    } else {
        None
    };
    let mut n = Notices::new();
    let r = transform::trim(st.as_bytes(), what, mode, &mut n);
    vm.std_notices(n)?;
    if r.len() == st.len() {
        return Ok(Value::Str(st));
    }
    if r.is_empty() {
        return Ok(Value::Str(ZStr::empty()));
    }
    let out = ZStr::from(r);
    if st.is_valid_utf8() && std::str::from_utf8(r).is_ok() {
        out.mark_valid_utf8();
    }
    Ok(Value::Str(out))
}
fn trim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, transform::TrimMode::Both)
}
fn ltrim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, transform::TrimMode::Left)
}
fn rtrim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, transform::TrimMode::Right)
}

fn str_pad(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let pad = if a.has(2) { s(a, 2) } else { ZStr::from(" ") };
    let ty = if a.has(3) { i(a, 3) } else { 1 };
    let r = transform::str_pad(s(a, 0).as_bytes(), i(a, 1), pad.as_bytes(), ty);
    Ok(bytes(vm.std(r)?))
}

fn str_repeat(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let input = s(a, 0);
    let times = i(a, 1);
    if times >= 0 && (input.is_empty() || times == 0) {
        return Ok(Value::Str(ZStr::empty()));
    }
    if times == 1 {
        return Ok(Value::Str(input));
    }
    let r = transform::str_repeat(input.as_bytes(), times);
    Ok(bytes(vm.std(r)?))
}

fn str_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = if a.has(1) { i(a, 1) } else { 1 };
    let r = transform::str_split(s(a, 0).as_bytes(), len);
    let parts = vm.std(r)?;
    Ok(Value::Array(ZArray::from_values(
        parts.into_iter().map(bytes),
    )))
}

fn strrev(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::strrev(s(a, 0).as_bytes())))
}

fn wordwrap(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let w = if a.has(1) { i(a, 1) } else { 75 };
    let br = if a.has(2) { s(a, 2) } else { ZStr::from("\n") };
    let r = transform::wordwrap(s(a, 0).as_bytes(), w, br.as_bytes(), b(a, 3));
    Ok(bytes(vm.std(r)?))
}

fn chunk_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let l = if a.has(1) { i(a, 1) } else { 76 };
    let e = if a.has(2) {
        s(a, 2)
    } else {
        ZStr::from("\r\n")
    };
    let r = transform::chunk_split(s(a, 0).as_bytes(), l, e.as_bytes());
    Ok(bytes(vm.std(r)?))
}

fn nl2br(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let x = !a.has(1) || b(a, 1);
    Ok(bytes(transform::nl2br(s(a, 0).as_bytes(), x)))
}

fn strtr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let st = s(a, 0);
    if a.has(2) {
        return Ok(bytes(transform::strtr(
            st.as_bytes(),
            s(a, 1).as_bytes(),
            s(a, 2).as_bytes(),
        )));
    }
    let Value::Array(pairs) = a.at(1) else {
        return Err(super::util::type_error(
            vm,
            a,
            2,
            "must be of type array, string given",
        ));
    };
    let mut ps: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (k, v) in pairs.iter() {
        let vs = vm.to_zstr(&v.deref())?;
        ps.push((k.to_bytes(), vs.as_bytes().to_vec()));
    }
    let mut n = Notices::new();
    let r = transform::strtr_pairs(st.as_bytes(), &ps, &mut n);
    vm.std_notices(n)?;
    Ok(bytes(r))
}

fn str_replace_impl(vm: &mut Vm, a: &mut Args, ci: bool) -> VmResult<Value> {
    let search_v = a.at(0);
    let replace_v = a.at(1);
    let subject = a.at(2);
    let mut total = 0i64;
    let search_list: Option<Vec<Vec<u8>>> = match &search_v {
        Value::Array(x) => {
            let mut v = Vec::new();
            for (_, e) in x.iter() {
                v.push(vm.to_zstr(&e.deref())?.as_bytes().to_vec());
            }
            Some(v)
        }
        _ => None,
    };
    let search_one = if search_list.is_none() {
        vm.to_zstr(&search_v)?
    } else {
        ZStr::empty()
    };
    let replace_list: Option<Vec<Vec<u8>>> = match &replace_v {
        Value::Array(x) => {
            let mut v = Vec::new();
            for (_, e) in x.iter() {
                v.push(vm.to_zstr(&e.deref())?.as_bytes().to_vec());
            }
            Some(v)
        }
        _ => None,
    };
    let replace_one = if replace_list.is_none() {
        vm.to_zstr(&replace_v)?
    } else {
        ZStr::empty()
    };
    let mut do_one = |vm: &mut Vm, subj: &[u8]| -> VmResult<Vec<u8>> {
        let sa = match &search_list {
            Some(l) => transform::ReplaceArg::Many(l.as_slice()),
            None => transform::ReplaceArg::One(search_one.as_bytes()),
        };
        let ra = match &replace_list {
            Some(l) => transform::ReplaceArg::Many(l.as_slice()),
            None => transform::ReplaceArg::One(replace_one.as_bytes()),
        };
        let r = transform::str_replace(subj, sa, ra, ci);
        let (out, n) = vm.std(r)?;
        total += n;
        Ok(out)
    };
    let result = match subject {
        Value::Array(sa) => {
            let mut t = HashTable::new();
            for (k, v) in sa.iter() {
                let v = v.deref();
                match &v {
                    Value::Array(_) | Value::Object(_) => {
                        t.insert(k.clone(), v.clone());
                    }
                    other => {
                        let st = vm.to_zstr(other)?;
                        let r = do_one(vm, st.as_bytes())?;
                        t.insert(k.clone(), bytes(r));
                    }
                }
            }
            Value::Array(ZArray::from_table(t))
        }
        other => {
            let st = vm.to_zstr(&other)?;
            bytes(do_one(vm, st.as_bytes())?)
        }
    };
    if a.has(3) {
        a.set_ref(3, Value::Int(total));
    }
    Ok(result)
}

fn str_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    str_replace_impl(vm, a, false)
}
fn str_ireplace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    str_replace_impl(vm, a, true)
}

fn chr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let c = transform::chr(i(a, 0), &mut n);
    vm.std_notices(n)?;
    Ok(Value::Str(ZStr::from_byte(c)))
}

fn ord(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let c = transform::ord(s(a, 0).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(Value::Int(c))
}

fn bin2hex(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::bin2hex(s(a, 0).as_bytes())))
}

fn hex2bin(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = transform::hex2bin(s(a, 0).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(opt_bytes(r))
}

fn str_rot13(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::str_rot13(s(a, 0).as_bytes())))
}

fn strcmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strcmp(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
    )))
}
fn strcasecmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strcasecmp(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
    )))
}
fn strncmp(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = compare::strncmp(s(a, 0).as_bytes(), s(a, 1).as_bytes(), i(a, 2));
    Ok(Value::Int(vm.std(r)?))
}
fn strncasecmp(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = compare::strncasecmp(s(a, 0).as_bytes(), s(a, 1).as_bytes(), i(a, 2));
    Ok(Value::Int(vm.std(r)?))
}
fn strnatcmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strnatcmp(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        false,
    )))
}
fn strnatcasecmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strnatcmp(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        true,
    )))
}

fn addslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::addslashes(s(a, 0).as_bytes())))
}
fn stripslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::stripslashes(s(a, 0).as_bytes())))
}
fn addcslashes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = slashes::addcslashes(s(a, 0).as_bytes(), s(a, 1).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(bytes(r))
}
fn stripcslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::stripcslashes(s(a, 0).as_bytes())))
}
fn quotemeta(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::quotemeta(s(a, 0).as_bytes())))
}

fn htmlspecialchars(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let fl = if a.has(1) { i(a, 1) } else { 3 | 8 };
    let de = !a.has(3) || b(a, 3);
    let cs = opt(a, 2).map(|_| s(a, 2));
    let input = s(a, 0);
    let mut n = Notices::new();
    let r = html::htmlspecialchars(
        input.as_bytes(),
        fl,
        cs.as_ref().map(ZStr::as_bytes),
        de,
        &mut n,
    );
    vm.std_notices(n)?;
    // php_escape_html_entities_ex: an empty input is the shared empty
    // string; unchanged input is returned as is unless ENT_DISALLOWED.
    if input.is_empty() {
        return Ok(Value::Str(ZStr::empty()));
    }
    if fl & 128 == 0 && r == input.as_bytes() {
        return Ok(Value::Str(input));
    }
    Ok(bytes(r))
}

fn htmlspecialchars_decode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let fl = if a.has(1) { i(a, 1) } else { 3 | 8 };
    Ok(bytes(html::htmlspecialchars_decode(s(a, 0).as_bytes(), fl)))
}

fn similar_text(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (n, pct) = analysis::similar_text(s(a, 0).as_bytes(), s(a, 1).as_bytes());
    if a.has(2) {
        a.set_ref(2, Value::Float(pct));
    }
    Ok(Value::Int(n))
}

fn levenshtein(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let ci = if a.has(2) { i(a, 2) } else { 1 };
    let cr = if a.has(3) { i(a, 3) } else { 1 };
    let cd = if a.has(4) { i(a, 4) } else { 1 };
    Ok(Value::Int(analysis::levenshtein(
        s(a, 0).as_bytes(),
        s(a, 1).as_bytes(),
        ci,
        cr,
        cd,
    )))
}

fn soundex(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(analysis::soundex(s(a, 0).as_bytes())))
}

fn metaphone(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = analysis::metaphone(s(a, 0).as_bytes(), i(a, 1));
    Ok(bytes(vm.std(r)?))
}

fn dechex(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(math::dechex(i(a, 0))))
}
fn decbin(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(math::decbin(i(a, 0))))
}
fn decoct(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(math::decoct(i(a, 0))))
}

fn num_value(n: math::Number) -> Value {
    match n {
        math::Number::Int(i) => Value::Int(i),
        math::Number::Float(f) => Value::Float(f),
    }
}

fn hexdec(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = math::hexdec(s(a, 0).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(num_value(r))
}
fn bindec(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = math::bindec(s(a, 0).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(num_value(r))
}
fn octdec(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = math::octdec(s(a, 0).as_bytes(), &mut n);
    vm.std_notices(n)?;
    Ok(num_value(r))
}

fn md5(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = php_hash::md5(s(a, 0).as_bytes());
    Ok(if b(a, 1) {
        bytes(d.to_vec())
    } else {
        bytes(transform::bin2hex(&d))
    })
}

fn sha1(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = php_hash::sha1(s(a, 0).as_bytes());
    Ok(if b(a, 1) {
        bytes(d.to_vec())
    } else {
        bytes(transform::bin2hex(&d))
    })
}

fn crc32(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::from(php_hash::crc32(s(a, 0).as_bytes()))))
}

fn number_format(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let n = a.at(0);
    let dec = i(a, 1);
    let dp = if a.has(2) { s(a, 2) } else { ZStr::from(".") };
    let ts = if a.has(3) { s(a, 3) } else { ZStr::from(",") };
    let num = match n {
        Value::Int(x) => php_support::fmt::Number::Int(x),
        other => php_support::fmt::Number::Float(vm.to_double(&other)?),
    };
    let dec = i32::try_from(dec.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0);
    Ok(bytes(php_support::fmt::number_format(
        num,
        dec,
        dp.as_bytes(),
        ts.as_bytes(),
    )))
}

fn lcg_value(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Float(vm.random_globals().lcg_value()))
}

fn uniqid(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let p = s(a, 0);
    let more = b(a, 1);
    Ok(bytes(vm.random_globals().uniqid(p.as_bytes(), more)))
}

fn str_word_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let fmt = i(a, 1);
    let chars = opt(a, 2).map(|_| s(a, 2));
    let mut n = Notices::new();
    let r = analysis::str_word_count(
        s(a, 0).as_bytes(),
        fmt,
        chars.as_ref().map(ZStr::as_bytes),
        &mut n,
    );
    vm.std_notices(n)?;
    let r = vm.std(r)?;
    Ok(match r {
        analysis::WordCount::Count(c) => Value::Int(c),
        analysis::WordCount::Words(w) => {
            Value::Array(ZArray::from_values(w.into_iter().map(bytes)))
        }
        analysis::WordCount::Positions(p) => {
            let mut t = HashTable::new();
            for (pos, w) in p {
                t.insert(ArrayKey::Int(pos), bytes(w));
            }
            Value::Array(ZArray::from_table(t))
        }
    })
}

fn nl_langinfo(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(false))
}

fn base64_encode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(php_stdstr::base64::base64_encode(s(a, 0).as_bytes())))
}

fn base64_decode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(php_stdstr::base64::base64_decode(
        s(a, 0).as_bytes(),
        b(a, 1),
    )))
}

fn dirname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let lv = if a.has(1) { i(a, 1) } else { 1 };
    let r = transform::dirname(s(a, 0).as_bytes(), lv);
    Ok(bytes(vm.std(r)?))
}

fn basename(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let suf = if a.has(1) { s(a, 1) } else { ZStr::empty() };
    Ok(bytes(transform::basename(
        s(a, 0).as_bytes(),
        suf.as_bytes(),
    )))
}

fn implode_alias_array(a: &Args) -> ZArray {
    arr(a, 0)
}

/// Registers the string functions only the VM provides, among them
/// `Zend/zend_builtin_functions.c`'s comparisons (module `Core`).
pub(crate) fn register(vm: &mut Vm) {
    reg(vm, "strcmp(string $string1, string $string2): int", strcmp);
    reg(
        vm,
        "strcasecmp(string $string1, string $string2): int",
        strcasecmp,
    );
    reg(
        vm,
        "strncmp(string $string1, string $string2, int $length): int",
        strncmp,
    );
    reg(
        vm,
        "strncasecmp(string $string1, string $string2, int $length): int",
        strncasecmp,
    );
    let _ = implode_alias_array;
    reg(
        vm,
        "uniqid(string $prefix = \"\", bool $more_entropy = false): string",
        uniqid,
    );
}

/// Registers the bare VM's string functions and constants (`Vm::new`):
/// the SAPIs get them from php-ext-stdstr and the other extensions.
pub(crate) fn register_bare(vm: &mut Vm) {
    reg(vm, "implode($separator, $array = null): string", implode);
    alias(vm, "join", "implode");
    reg(
        vm,
        "explode(string $separator, string $string, int $limit = PHP_INT_MAX): array",
        explode,
    );
    reg(
        vm,
        "substr(string $string, int $offset, ?int $length = null): string",
        substr,
    );
    reg(
        vm,
        "strpos(string $haystack, string $needle, int $offset = 0): int|false",
        strpos,
    );
    reg(
        vm,
        "stripos(string $haystack, string $needle, int $offset = 0): int|false",
        stripos,
    );
    reg(
        vm,
        "strrpos(string $haystack, string $needle, int $offset = 0): int|false",
        strrpos,
    );
    reg(
        vm,
        "strripos(string $haystack, string $needle, int $offset = 0): int|false",
        strripos,
    );
    reg(
        vm,
        "strstr(string $haystack, string $needle, bool $before_needle = false): string|false",
        strstr,
    );
    alias(vm, "strchr", "strstr");
    reg(
        vm,
        "stristr(string $haystack, string $needle, bool $before_needle = false): string|false",
        stristr,
    );
    reg(
        vm,
        "strrchr(string $haystack, string $needle, bool $before_needle = false): string|false",
        strrchr,
    );
    reg(
        vm,
        "str_contains(string $haystack, string $needle): bool",
        str_contains,
    );
    reg(
        vm,
        "str_starts_with(string $haystack, string $needle): bool",
        str_starts_with,
    );
    reg(
        vm,
        "str_ends_with(string $haystack, string $needle): bool",
        str_ends_with,
    );
    reg(
        vm,
        "substr_count(string $haystack, string $needle, int $offset = 0, ?int $length = null): int",
        substr_count,
    );
    reg(
        vm,
        "strspn(string $string, string $characters, int $offset = 0, ?int $length = null): int",
        strspn,
    );
    reg(
        vm,
        "strcspn(string $string, string $characters, int $offset = 0, ?int $length = null): int",
        strcspn,
    );
    reg(
        vm,
        "strpbrk(string $string, string $characters): string|false",
        strpbrk,
    );
    reg(vm, "substr_replace(array|string $string, array|string $replace, array|int $offset, array|int|null $length = null): string|array", substr_replace);
    reg(vm, "strtolower(string $string): string", strtolower);
    reg(vm, "strtoupper(string $string): string", strtoupper);
    reg(vm, "ucfirst(string $string): string", ucfirst);
    reg(vm, "lcfirst(string $string): string", lcfirst);
    reg(
        vm,
        "ucwords(string $string, string $separators = \" \\t\\r\\n\\f\\v\"): string",
        ucwords,
    );
    reg(
        vm,
        "trim(string $string, string $characters = \" \\n\\r\\t\\v\\0\"): string",
        trim,
    );
    reg(
        vm,
        "ltrim(string $string, string $characters = \" \\n\\r\\t\\v\\0\"): string",
        ltrim,
    );
    reg(
        vm,
        "rtrim(string $string, string $characters = \" \\n\\r\\t\\v\\0\"): string",
        rtrim,
    );
    alias(vm, "chop", "rtrim");
    reg(vm, "str_pad(string $string, int $length, string $pad_string = \" \", int $pad_type = STR_PAD_RIGHT): string", str_pad);
    reg(
        vm,
        "str_repeat(string $string, int $times): string",
        str_repeat,
    );
    reg(
        vm,
        "str_split(string $string, int $length = 1): array",
        str_split,
    );
    reg(vm, "strrev(string $string): string", strrev);
    reg(vm, "wordwrap(string $string, int $width = 75, string $break = \"\\n\", bool $cut_long_words = false): string", wordwrap);
    reg(
        vm,
        "chunk_split(string $string, int $length = 76, string $separator = \"\\r\\n\"): string",
        chunk_split,
    );
    reg(
        vm,
        "nl2br(string $string, bool $use_xhtml = true): string",
        nl2br,
    );
    reg(
        vm,
        "strtr(string $string, $from, ?string $to = null): string",
        strtr,
    );
    reg(vm, "str_replace(array|string $search, array|string $replace, string|array $subject, &$count = null): string|array", str_replace);
    reg(vm, "str_ireplace(array|string $search, array|string $replace, string|array $subject, &$count = null): string|array", str_ireplace);
    reg(vm, "chr(int $codepoint): string", chr);
    reg(vm, "ord(string $character): int", ord);
    reg(vm, "bin2hex(string $string): string", bin2hex);
    reg(vm, "hex2bin(string $string): string|false", hex2bin);
    reg(vm, "str_rot13(string $string): string", str_rot13);
    reg(
        vm,
        "strnatcmp(string $string1, string $string2): int",
        strnatcmp,
    );
    reg(
        vm,
        "strnatcasecmp(string $string1, string $string2): int",
        strnatcasecmp,
    );
    reg(vm, "strcoll(string $string1, string $string2): int", strcmp);
    reg(vm, "addslashes(string $string): string", addslashes);
    reg(vm, "stripslashes(string $string): string", stripslashes);
    reg(
        vm,
        "addcslashes(string $string, string $characters): string",
        addcslashes,
    );
    reg(vm, "stripcslashes(string $string): string", stripcslashes);
    reg(vm, "quotemeta(string $string): string", quotemeta);
    reg(vm, "htmlspecialchars(string $string, int $flags = 11, ?string $encoding = null, bool $double_encode = true): string", htmlspecialchars);
    reg(
        vm,
        "htmlspecialchars_decode(string $string, int $flags = 11): string",
        htmlspecialchars_decode,
    );
    reg(
        vm,
        "similar_text(string $string1, string $string2, &$percent = null): int",
        similar_text,
    );
    reg(vm, "levenshtein(string $string1, string $string2, int $insertion_cost = 1, int $replacement_cost = 1, int $deletion_cost = 1): int", levenshtein);
    reg(vm, "soundex(string $string): string", soundex);
    reg(
        vm,
        "metaphone(string $string, int $max_phonemes = 0): string",
        metaphone,
    );
    reg(vm, "dechex(int $num): string", dechex);
    reg(vm, "decbin(int $num): string", decbin);
    reg(vm, "decoct(int $num): string", decoct);
    reg(vm, "hexdec(string $hex_string): int|float", hexdec);
    reg(vm, "bindec(string $binary_string): int|float", bindec);
    reg(vm, "octdec(string $octal_string): int|float", octdec);
    reg(vm, "md5(string $string, bool $binary = false): string", md5);
    reg(
        vm,
        "sha1(string $string, bool $binary = false): string",
        sha1,
    );
    reg(vm, "crc32(string $string): int", crc32);
    reg(vm, "number_format(float $num, int $decimals = 0, ?string $decimal_separator = \".\", ?string $thousands_separator = \",\"): string", number_format);
    reg(vm, "lcg_value(): float", lcg_value);
    reg(
        vm,
        "str_word_count(string $string, int $format = 0, ?string $characters = null): array|int",
        str_word_count,
    );
    reg(vm, "nl_langinfo(int $item): string|false", nl_langinfo);
    reg(
        vm,
        "dirname(string $path, int $levels = 1): string",
        dirname,
    );
    reg(
        vm,
        "basename(string $path, string $suffix = \"\"): string",
        basename,
    );
    reg(vm, "base64_encode(string $string): string", base64_encode);
    reg(
        vm,
        "base64_decode(string $string, bool $strict = false): string|false",
        base64_decode,
    );
    for (n, v) in [
        ("STR_PAD_LEFT", 0),
        ("STR_PAD_RIGHT", 1),
        ("STR_PAD_BOTH", 2),
        ("ENT_QUOTES", 3),
        ("ENT_COMPAT", 2),
        ("ENT_NOQUOTES", 0),
        ("ENT_HTML401", 0),
        ("ENT_SUBSTITUTE", 8),
        ("ENT_IGNORE", 4),
        ("ENT_HTML5", 48),
        ("ENT_XML1", 16),
        ("ENT_XHTML", 32),
        ("ENT_DISALLOWED", 128),
    ] {
        constant(vm, n, Value::Int(v));
    }
}
