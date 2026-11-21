// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/standard/string.c` functions.

use std::cell::RefCell;

use php_runtime::string::{format_long, ZStrBuf, LONG_MAX_LEN};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_stdstr::analysis::{self, CharCounts, WordCount};
use php_stdstr::error::Notices;
use php_stdstr::search::{self, Strtok};
use php_stdstr::transform::{self, ReplaceArg, TrimMode};
use php_stdstr::{compare, slashes, strip_tags};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::{assoc, bytes, check, emit, mem, opt_bytes, str_list, to_str};

thread_local! {
    /// `BG(strtok_string)` / `BG(strtok_last)`.
    static STRTOK: RefCell<Strtok> = RefCell::new(Strtok::new());
}

/// `int|false` from an optional position.
fn pos_result(r: Option<i64>) -> Value {
    r.map_or(Value::Bool(false), Value::Int)
}

/// An optional `?int` argument.
fn opt_int(a: &Args, n: usize) -> Option<i64> {
    a.opt(n).map(|_| a.int(n))
}

fn bin2hex(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::bin2hex(a.zstr(0).as_bytes())))
}

fn hex2bin(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = transform::hex2bin(a.zstr(0).as_bytes(), &mut n);
    emit(vm, n)?;
    Ok(opt_bytes(r))
}

fn strspn(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strspn(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.int(2),
        opt_int(a, 3),
    );
    Ok(Value::Int(r))
}

fn strcspn(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strcspn(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.int(2),
        opt_int(a, 3),
    );
    Ok(Value::Int(r))
}

fn strcoll(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // The C library's strcoll() in the thread's LC_COLLATE (in "C" the
    // byte difference, like strcmp()), up to the first NUL as in php-src.
    let r = php_stdarr::compare::strcoll(a.zstr(0).as_bytes(), a.zstr(1).as_bytes());
    Ok(Value::Int(i64::from(r)))
}

fn trim_impl(vm: &mut Vm, a: &Args, mode: TrimMode) -> VmResult<Value> {
    let s = a.zstr(0);
    let what = a.has(1).then(|| a.zstr(1));
    let mut n = Notices::new();
    let r = transform::trim(
        s.as_bytes(),
        what.as_deref().map(ZStr::as_bytes),
        mode,
        &mut n,
    );
    // `php_trim_int()`: the string itself when nothing was trimmed, the
    // shared empty string when everything was, else a copy.
    let out = if r.len() == s.len() {
        ZStr::clone(&s)
    } else if r.is_empty() {
        ZStr::empty()
    } else {
        ZStr::from(r)
    };
    if !n.is_empty() {
        emit(vm, n)?;
    }
    Ok(Value::Str(out))
}

fn trim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, TrimMode::Both)
}

fn ltrim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, TrimMode::Left)
}

fn rtrim(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    trim_impl(vm, a, TrimMode::Right)
}

fn wordwrap(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let width = if a.has(1) { a.int(1) } else { 75 };
    let brk = if a.has(2) { a.str(2) } else { ZStr::from("\n") };
    let text = a.str(0);
    let cut = a.bool(3);
    if !text.is_empty() && !brk.is_empty() && (width != 0 || !cut) {
        // The buffer php_wordwrap() allocates up front.
        let tl = text.len();
        let alloc = if brk.len() == 1 && !cut {
            tl
        } else if width > 0 {
            let chk = tl / usize::try_from(width).unwrap_or(usize::MAX) + 1;
            tl.saturating_add(chk.saturating_mul(brk.len()))
        } else {
            tl.saturating_mul(brk.len() + 1)
        };
        mem::check(vm, mem::zstr(alloc))?;
    }
    let r = transform::wordwrap(text.as_bytes(), width, brk.as_bytes(), cut);
    Ok(bytes(check(vm, r)?))
}

fn explode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let limit = if a.has(2) { a.int(2) } else { i64::MAX };
    let s = a.zstr(1);
    let sep = a.zstr(0);
    let pieces = mem::occurrences(s.as_bytes(), sep.as_bytes(), false) + 1;
    mem::check(
        vm,
        pieces.saturating_mul(mem::elem(0)).saturating_add(s.len()),
    )?;
    let r = search::explode_slices(sep.as_bytes(), s.as_bytes(), limit);
    let parts = check(vm, r)?;
    // `php_explode()`: the input itself when it is not split
    // (`ZVAL_STR_COPY`), else `zend_string_init_fast` of each piece.
    Ok(Value::Array(ZArray::from_values(parts.into_iter().map(
        |p| {
            Value::Str(if p.len() == s.len() {
                ZStr::clone(&s)
            } else {
                ZStr::init_fast(p)
            })
        },
    ))))
}

/// `php_implode()`: the pieces joined straight into the result string
/// (one exactly sized allocation for string and integer pieces), with its
/// single-element case: the element's own string (`zval_get_string`, no
/// copy for a string).
fn implode_zstr(vm: &mut Vm, glue: &[u8], pieces: &ZArray) -> VmResult<ZStr> {
    if pieces.is_empty() {
        return Ok(ZStr::empty());
    }
    if pieces.len() == 1 {
        if let Some((_, v)) = pieces.iter().next() {
            if let Value::Str(s) = v.deref() {
                return Ok(s);
            }
        }
    }
    let known: usize = pieces
        .iter()
        .map(|(_, v)| match v.deref() {
            Value::Str(s) => s.len(),
            Value::Int(n) => php_runtime::string::long_str_len(n),
            _ => 0,
        })
        .sum();
    let total = known.saturating_add(glue.len().saturating_mul(pieces.len().saturating_sub(1)));
    mem::check(vm, mem::zstr(total))?;
    let mut out = ZStrBuf::with_capacity(total);
    for (i, (_, v)) in pieces.iter().enumerate() {
        if i > 0 {
            out.push(glue);
        }
        match v.deref() {
            Value::Str(s) => out.push(s.as_bytes()),
            Value::Int(n) => {
                let mut buf = [0u8; LONG_MAX_LEN];
                out.push(format_long(n, &mut buf));
            }
            other => out.push(to_str(vm, &other)?.as_bytes()),
        }
    }
    Ok(out.finish())
}

fn implode(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // Both arguments are read in place (`Z_PARAM_ARRAY_HT_OR_STR`,
    // `Z_PARAM_ARRAY_HT_OR_NULL`): no copy of either per call.
    let first = a.at_ref(0);
    let second = a.opt_ref(1);
    let (glue, pieces) = match second.as_deref() {
        None => match &*first {
            Value::Array(p) => (ZStr::empty(), p),
            _ => {
                let fname = a.func_name();
                return Err(vm.throw(
                    "TypeError",
                    &format!(
                        "{fname}(): If argument #1 ($separator) is of type string, argument #2 ($array) must be of type array, null given"
                    ),
                ));
            }
        },
        Some(Value::Array(p)) => match &*first {
            Value::Array(_) => {
                return Err(a.type_error(vm, 1, "must be of type string, array given"));
            }
            other => (to_str(vm, other)?, p),
        },
        Some(other) => {
            let msg = format!("must be of type ?array, {} given", crate::type_name(other));
            return Err(a.type_error(vm, 2, &msg));
        }
    };
    let out = implode_zstr(vm, glue.as_bytes(), pieces)?;
    if !out.is_valid_utf8() && implode_valid_utf8(&glue, pieces) {
        out.mark_valid_utf8();
    }
    Ok(Value::Str(out))
}

/// Whether `php_implode` flags its result `IS_STR_VALID_UTF8`: an empty
/// array gives the (flagged) empty string, one element its own string,
/// more the glue's and every piece's copyable concat properties. Integers,
/// floats, bools, null and arrays (`"Array"`, a known string) convert to
/// ASCII; an object's or resource's string is not known here, so it
/// counts as unflagged.
fn implode_valid_utf8(glue: &ZStr, pieces: &ZArray) -> bool {
    let piece = |v: &Value| match v.deref() {
        Value::Str(s) => s.is_valid_utf8(),
        Value::Object(_) | Value::Resource(_) => false,
        _ => true,
    };
    match pieces.len() {
        0 => true,
        1 => pieces.iter().all(|(_, v)| piece(v)),
        _ => glue.is_valid_utf8() && pieces.iter().all(|(_, v)| piece(v)),
    }
}

fn strtok(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = if a.opt(1).is_some() {
        let (s, tok) = (a.str(0), a.str(1));
        STRTOK.with(|t| t.borrow_mut().start(s.as_bytes(), tok.as_bytes(), &mut n))
    } else {
        let tok = a.str(0);
        STRTOK.with(|t| t.borrow_mut().next(tok.as_bytes(), &mut n))
    };
    emit(vm, n)?;
    Ok(opt_bytes(r))
}

// `zend_string_toupper()` / `zend_string_tolower()`: the string itself
// when no byte changes, else one new string converted in place.
fn strtoupper(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(a.zstr(0).to_ascii_uppercase()))
}

fn strtolower(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(a.zstr(0).to_ascii_lowercase()))
}

fn str_increment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = transform::str_increment(a.zstr(0).as_bytes());
    Ok(bytes(check(vm, r)?))
}

fn str_decrement(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = transform::str_decrement(a.zstr(0).as_bytes());
    Ok(bytes(check(vm, r)?))
}

fn strstr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::strstr(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.bool(2),
    )))
}

fn stristr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::stristr(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.bool(2),
    )))
}

fn strrchr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(opt_bytes(search::strrchr(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.bool(2),
    )))
}

fn strpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strpos(a.zstr(0).as_bytes(), a.zstr(1).as_bytes(), a.int(2));
    Ok(pos_result(check(vm, r)?))
}

fn stripos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::stripos(a.zstr(0).as_bytes(), a.zstr(1).as_bytes(), a.int(2));
    Ok(pos_result(check(vm, r)?))
}

fn strrpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strrpos(a.zstr(0).as_bytes(), a.zstr(1).as_bytes(), a.int(2));
    Ok(pos_result(check(vm, r)?))
}

fn strripos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strripos(a.zstr(0).as_bytes(), a.zstr(1).as_bytes(), a.int(2));
    Ok(pos_result(check(vm, r)?))
}

fn str_contains(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_contains(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
    )))
}

fn str_starts_with(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_starts_with(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
    )))
}

fn str_ends_with(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(search::str_ends_with(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
    )))
}

fn chunk_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = if a.has(1) { a.int(1) } else { 76 };
    let end = if a.has(2) {
        a.str(2)
    } else {
        ZStr::from("\r\n")
    };
    let s = a.str(0);
    if len > 0 {
        let chunks = s.len() / usize::try_from(len).unwrap_or(usize::MAX) + 1;
        let alloc = s.len().saturating_add(chunks.saturating_mul(end.len()));
        mem::check(vm, mem::zstr(alloc))?;
    }
    let r = transform::chunk_split(s.as_bytes(), len, end.as_bytes());
    Ok(bytes(check(vm, r)?))
}

fn substr(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.zstr(0);
    let r = search::substr(s.as_bytes(), a.int(1), a.opt_int(2));
    // `RETURN_STR_COPY(str)` for the whole string, else
    // `RETURN_STRINGL_FAST` (shared empty / one-character strings).
    Ok(Value::Str(if r.len() == s.len() {
        ZStr::clone(&s)
    } else {
        ZStr::init_fast(r)
    }))
}

/// Walks the values of an `array|int` argument of `substr_replace()` in
/// order (`HashPosition` over the defined slots).
struct Cursor {
    values: Vec<Value>,
    pos: usize,
}

impl Cursor {
    fn new(v: &Value) -> Option<Self> {
        match v {
            Value::Array(arr) => Some(Self {
                values: arr.iter().map(|(_, v)| v.deref()).collect(),
                pos: 0,
            }),
            _ => None,
        }
    }

    fn next(&mut self) -> Option<Value> {
        let v = self.values.get(self.pos).cloned();
        if v.is_some() {
            self.pos += 1;
        }
        v
    }
}

fn substr_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let subject = a.at(0);
    let repl = a.at(1);
    let from = a.at(2);
    let len = a.opt(3);
    let mut repl_cur = Cursor::new(&repl);
    let Value::Array(subjects) = subject else {
        let s = to_str(vm, &subject)?;
        if matches!(from, Value::Array(_)) {
            return Err(crate::throw(vm, &search::substr_replace_array_error(3)));
        }
        if matches!(len, Some(Value::Array(_))) {
            return Err(crate::throw(vm, &search::substr_replace_array_error(4)));
        }
        let r = match &mut repl_cur {
            Some(c) => match c.next() {
                Some(v) => to_str(vm, &v)?,
                None => ZStr::empty(),
            },
            None => to_str(vm, &repl)?,
        };
        let f = vm.value_to_int(&from)?;
        let l = match &len {
            Some(v) => Some(vm.value_to_int(v)?),
            None => None,
        };
        return Ok(bytes(search::substr_replace(
            s.as_bytes(),
            r.as_bytes(),
            f,
            l,
        )));
    };
    let repl_str = if repl_cur.is_none() {
        to_str(vm, &repl)?
    } else {
        ZStr::empty()
    };
    let mut from_cur = Cursor::new(&from);
    let from_long = if from_cur.is_none() {
        vm.value_to_int(&from)?
    } else {
        0
    };
    let mut len_cur = len.as_ref().and_then(Cursor::new);
    let len_long = match (&len, &len_cur) {
        (Some(v), None) => Some(vm.value_to_int(v)?),
        _ => None,
    };
    let mut estimate = 0usize;
    for (_, v) in subjects.iter() {
        let len = match v.deref() {
            Value::Str(s) => s.len(),
            _ => 0,
        };
        let r = match &repl {
            Value::Array(_) => 0,
            _ => repl_str.len(),
        };
        estimate = estimate.saturating_add(mem::elem(len.saturating_add(r)));
    }
    mem::check(vm, estimate)?;
    let mut out = HashTable::new();
    for (k, v) in subjects.iter() {
        let orig = to_str(vm, &v.deref())?;
        let f = match &mut from_cur {
            Some(c) => match c.next() {
                Some(x) => vm.value_to_int(&x)?,
                None => 0,
            },
            None => from_long,
        };
        let l = match &mut len_cur {
            Some(c) => match c.next() {
                Some(x) => Some(vm.value_to_int(&x)?),
                None => None,
            },
            None => len_long,
        };
        let r = match &mut repl_cur {
            Some(c) => match c.next() {
                Some(x) => to_str(vm, &x)?,
                None => ZStr::empty(),
            },
            None => repl_str.clone(),
        };
        let piece = search::substr_replace(orig.as_bytes(), r.as_bytes(), f, l);
        out.insert(k.clone(), bytes(piece));
    }
    Ok(Value::Array(ZArray::from_table(out)))
}

fn quotemeta(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::quotemeta(a.zstr(0).as_bytes())))
}

fn ord(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = transform::ord(a.zstr(0).as_bytes(), &mut n);
    emit(vm, n)?;
    Ok(Value::Int(r))
}

fn chr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let c = transform::chr(a.int(0), &mut n);
    emit(vm, n)?;
    Ok(Value::Str(ZStr::from_byte(c)))
}

fn ucfirst(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::ucfirst(a.zstr(0).as_bytes())))
}

fn lcfirst(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::lcfirst(a.zstr(0).as_bytes())))
}

fn ucwords(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let delims = if a.has(1) {
        a.str(1)
    } else {
        ZStr::from(transform::UCWORDS_DEFAULT_DELIMITERS)
    };
    let mut n = Notices::new();
    let r = transform::ucwords(a.zstr(0).as_bytes(), delims.as_bytes(), &mut n);
    emit(vm, n)?;
    Ok(bytes(r))
}

fn strtr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let from = a.at(1);
    if a.has(2) {
        // Z_PARAM_STR(from_str)
        let from = match from {
            Value::Array(_) | Value::Resource(_) => {
                let msg = format!("must be of type string, {} given", crate::type_name(&from));
                return Err(a.type_error(vm, 2, &msg));
            }
            Value::Null => {
                vm.php_deprecated(
                    "Passing null to parameter #2 ($from) of type array|string is deprecated",
                )?;
                ZStr::empty()
            }
            other => to_str(vm, &other)?,
        };
        let to = a.str(2);
        if s.is_empty() {
            return Ok(bytes(ZStr::empty()));
        }
        return Ok(bytes(transform::strtr(
            s.as_bytes(),
            from.as_bytes(),
            to.as_bytes(),
        )));
    }
    let Value::Array(pairs) = from else {
        let msg = format!("must be of type array, {} given", crate::type_name(&from));
        return Err(a.type_error(vm, 2, &msg));
    };
    if s.is_empty() {
        return Ok(bytes(ZStr::empty()));
    }
    let mut ps: Vec<(Vec<u8>, Value)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs.iter() {
        let key = match k {
            ArrayKey::Int(i) => i.to_string().into_bytes(),
            ArrayKey::Str(s) => s.as_bytes().to_vec(),
        };
        ps.push((key, v.clone()));
    }
    let mut alloc = s.len();
    let mut n = Notices::new();
    let r = transform::strtr_pairs_with(
        s.as_bytes(),
        &ps,
        &mut n,
        |key, v, notices, capacity| -> VmResult<ZStr> {
            emit(vm, std::mem::take(notices))?;
            let val = to_str(vm, v)?;
            if ps.len() == 1 {
                let count = mem::occurrences(s.as_bytes(), key, false);
                alloc =
                    alloc.saturating_add(count.saturating_mul(val.len().saturating_sub(key.len())));
            } else {
                alloc = alloc.saturating_sub(key.len()).saturating_add(val.len());
            }
            if alloc > capacity {
                mem::check(vm, mem::zstr(alloc - capacity))?;
            }
            Ok(val)
        },
    );
    emit(vm, n)?;
    Ok(bytes(r?))
}

fn strrev(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::strrev(a.zstr(0).as_bytes())))
}

fn similar_text(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (sim, pct) = analysis::similar_text(a.zstr(0).as_bytes(), a.zstr(1).as_bytes());
    if a.has(2) {
        a.set_ref(2, Value::Float(pct));
    }
    Ok(Value::Int(sim))
}

fn addcslashes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut n = Notices::new();
    let r = slashes::addcslashes(a.zstr(0).as_bytes(), a.zstr(1).as_bytes(), &mut n);
    emit(vm, n)?;
    Ok(bytes(r))
}

fn addslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::addslashes(a.zstr(0).as_bytes())))
}

fn stripcslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::stripcslashes(a.zstr(0).as_bytes())))
}

fn stripslashes(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(slashes::stripslashes(a.zstr(0).as_bytes())))
}

/// `php_str_replace_in_subject()`: applies `search`/`replace` to one
/// subject, converting array entries lazily exactly like PHP (so
/// conversion warnings repeat per subject and stop once the subject
/// becomes empty).
/// Reserves the result of replacing every `search` in `subject`.
fn reserve_replace(
    vm: &mut Vm,
    subject: &[u8],
    search: &[u8],
    replace: &[u8],
    ci: bool,
) -> VmResult<()> {
    if replace.len() > search.len() {
        let n = mem::occurrences(subject, search, ci);
        let grown = subject
            .len()
            .saturating_add(n.saturating_mul(replace.len() - search.len()));
        mem::check(vm, mem::zstr(grown))?;
    }
    Ok(())
}

/// `php_str_to_str_ex()` (case-sensitive, non-empty `search`): the
/// subject itself when `search` does not occur (no allocation), else one
/// exactly sized result. Reserves the grown size first, like
/// [`reserve_replace`].
fn replace_cs(vm: &mut Vm, subject: ZStr, search: &[u8], replace: &[u8]) -> VmResult<(ZStr, i64)> {
    let hay = subject.as_bytes();
    let Some(first) = search::memnstr(hay, search) else {
        return Ok((subject, 0));
    };
    let mut count = 1usize;
    let mut p = first + search.len();
    while let Some(r) = search::memnstr(&hay[p..], search) {
        count += 1;
        p += r + search.len();
    }
    let size = if replace.len() > search.len() {
        let grown = hay
            .len()
            .saturating_add(count.saturating_mul(replace.len() - search.len()));
        mem::check(vm, mem::zstr(grown))?;
        grown
    } else {
        hay.len() - count * (search.len() - replace.len())
    };
    let mut out = ZStrBuf::with_capacity(size);
    let mut p = 0;
    let mut at = Some(first);
    while let Some(r) = at {
        out.push(&hay[p..p + r]);
        out.push(replace);
        p += r + search.len();
        at = if p <= hay.len() {
            search::memnstr(&hay[p..], search)
        } else {
            None
        };
    }
    out.push(&hay[p..]);
    Ok((out.finish(), i64::try_from(count).unwrap_or(i64::MAX)))
}

fn replace_in_subject(
    vm: &mut Vm,
    search: &Value,
    replace: &Value,
    subject: ZStr,
    ci: bool,
) -> VmResult<(ZStr, i64)> {
    if subject.is_empty() {
        return Ok((subject, 0));
    }
    let Value::Array(searches) = search else {
        let s = to_str(vm, search)?;
        let r = to_str(vm, replace)?;
        if !ci && !s.is_empty() {
            return replace_cs(vm, subject, &s, &r);
        }
        reserve_replace(vm, &subject, &s, &r, ci)?;
        let res = transform::str_replace(
            subject.as_bytes(),
            ReplaceArg::One::<&[u8]>(s.as_bytes()),
            ReplaceArg::One::<&[u8]>(r.as_bytes()),
            ci,
        );
        let (out, n) = check(vm, res)?;
        return Ok((if n == 0 { subject } else { ZStr::from(out) }, n));
    };
    let mut repl_cur = Cursor::new(replace);
    let repl_one = if repl_cur.is_none() {
        to_str(vm, replace)?
    } else {
        ZStr::empty()
    };
    let mut subject = subject;
    let mut count = 0;
    for (_, entry) in searches.iter() {
        let s = to_str(vm, &entry.deref())?;
        let r = match &mut repl_cur {
            Some(c) => match c.next() {
                Some(v) => to_str(vm, &v)?,
                None => ZStr::empty(),
            },
            None => repl_one.clone(),
        };
        if s.is_empty() {
            continue;
        }
        if !ci {
            let (out, n) = replace_cs(vm, subject, &s, &r)?;
            subject = out;
            if n > 0 {
                count += n;
                if subject.is_empty() {
                    break;
                }
            }
            continue;
        }
        reserve_replace(vm, &subject, &s, &r, ci)?;
        let res = transform::str_replace(
            subject.as_bytes(),
            ReplaceArg::One::<&[u8]>(s.as_bytes()),
            ReplaceArg::One::<&[u8]>(r.as_bytes()),
            ci,
        );
        let (out, n) = check(vm, res)?;
        if n > 0 {
            count += n;
            subject = ZStr::from(out);
            if subject.is_empty() {
                break;
            }
        }
    }
    Ok((subject, count))
}

fn str_replace_common(vm: &mut Vm, a: &Args, ci: bool) -> VmResult<Value> {
    let search = a.at(0);
    let replace = a.at(1);
    let subject = a.at(2);
    if !matches!(search, Value::Array(_)) && matches!(replace, Value::Array(_)) {
        return Err(a.type_error(
            vm,
            2,
            "must be of type string when argument #1 ($search) is a string",
        ));
    }
    let mut count = 0;
    let result = if let Value::Array(subjects) = subject {
        let mut out = HashTable::new();
        for (k, v) in subjects.iter() {
            let s = to_str(vm, &v.deref())?;
            let (r, n) = replace_in_subject(vm, &search, &replace, s, ci)?;
            count += n;
            out.insert(k.clone(), Value::Str(r));
        }
        Value::Array(ZArray::from_table(out))
    } else {
        let s = to_str(vm, &subject)?;
        let (r, n) = replace_in_subject(vm, &search, &replace, s, ci)?;
        count = n;
        Value::Str(r)
    };
    if a.has(3) {
        a.set_ref(3, Value::Int(count));
    }
    Ok(result)
}

fn str_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    str_replace_common(vm, a, false)
}

fn str_ireplace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    str_replace_common(vm, a, true)
}

fn hebrev(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::hebrev(a.zstr(0).as_bytes(), a.int(1))))
}

fn nl2br(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let xhtml = !a.has(1) || a.bool(1);
    let s = a.str(0);
    let breaks = s.iter().filter(|&&c| c == b'\n' || c == b'\r').count();
    let tag = if xhtml { 6 } else { 4 };
    mem::check(
        vm,
        mem::zstr(s.len().saturating_add(breaks.saturating_mul(tag))),
    )?;
    let out = transform::nl2br(s.as_bytes(), xhtml);
    Ok(bytes(out))
}

fn strip_tags_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let allowed = match a.opt(1) {
        None => None,
        Some(Value::Array(list)) => {
            let mut tags = Vec::with_capacity(list.len());
            for (_, v) in list.iter() {
                tags.push(to_str(vm, &v.deref())?.as_bytes().to_vec());
            }
            strip_tags::allowed_tags_from_list(&tags)
        }
        Some(v) => Some(to_str(vm, &v)?.as_bytes().to_vec()),
    };
    Ok(bytes(strip_tags::strip_tags(
        s.as_bytes(),
        allowed.as_deref(),
    )))
}

/// Results up to this many bytes are built on the stack by `str_repeat()`.
const STR_REPEAT_STACK: usize = 256;

fn str_repeat(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = a.str(0);
    let times = a.int(1);
    if times >= 0 && (s.is_empty() || times == 0) {
        return Ok(Value::Str(ZStr::empty()));
    }
    if times == 1 {
        return Ok(Value::Str(s));
    }
    if times > 0 {
        let total = s
            .len()
            .saturating_mul(usize::try_from(times).unwrap_or(usize::MAX));
        mem::check(vm, mem::zstr(total))?;
        // A short result is written straight into one exactly sized
        // string (`zend_string_safe_alloc` + the copies), with no
        // intermediate vector.
        if total <= STR_REPEAT_STACK {
            let mut buf = [0u8; STR_REPEAT_STACK];
            for chunk in buf[..total].chunks_exact_mut(s.len()) {
                chunk.copy_from_slice(s.as_bytes());
            }
            let out = ZStr::from(&buf[..total]);
            if s.is_valid_utf8() {
                out.mark_valid_utf8();
            }
            return Ok(Value::Str(out));
        }
    }
    let r = transform::str_repeat(s.as_bytes(), times);
    let out = ZStr::from(check(vm, r)?);
    // `ZSTR_COPY_CONCAT_PROPERTIES(result, input_str)`.
    if s.is_valid_utf8() {
        out.mark_valid_utf8();
    }
    Ok(Value::Str(out))
}

fn count_chars(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = analysis::count_chars(a.zstr(0).as_bytes(), a.int(1));
    Ok(match check(vm, r)? {
        CharCounts::Array(pairs) => assoc(
            pairs
                .into_iter()
                .map(|(b, c)| (i64::from(b), Value::Int(c))),
        ),
        CharCounts::Bytes(b) => bytes(b),
    })
}

fn strnatcmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strnatcmp(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        false,
    )))
}

fn strnatcasecmp(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(compare::strnatcmp(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        true,
    )))
}

fn substr_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::substr_count(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.int(2),
        opt_int(a, 3),
    );
    Ok(Value::Int(check(vm, r)?))
}

fn str_pad(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let pad = if a.has(2) { a.str(2) } else { ZStr::from(" ") };
    let ty = if a.has(3) {
        a.int(3)
    } else {
        transform::STR_PAD_RIGHT
    };
    let s = a.str(0);
    let len = a.int(1);
    let target = usize::try_from(len).unwrap_or(0);
    if target > s.len() && !pad.is_empty() && (0..=2).contains(&ty) {
        mem::check(vm, mem::zstr(target))?;
    }
    let r = transform::str_pad(s.as_bytes(), len, pad.as_bytes(), ty);
    Ok(bytes(check(vm, r)?))
}

fn str_rot13(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::str_rot13(a.zstr(0).as_bytes())))
}

fn str_shuffle(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut s = a.zstr(0).as_bytes().to_vec();
    if s.len() <= 1 {
        return Ok(bytes(s));
    }
    // php_string_shuffle(): Fisher-Yates over the default (MT) engine,
    // which mt_rand() shares.
    let rand = Value::string("mt_rand");
    let mut left = s.len() - 1;
    while left > 0 {
        let max = i64::try_from(left).unwrap_or(i64::MAX);
        let r = vm.call_value(&rand, vec![Value::Int(0), Value::Int(max)])?;
        let idx = usize::try_from(vm.value_to_int(&r)?).unwrap_or(0).min(left);
        s.swap(left, idx);
        left -= 1;
    }
    Ok(bytes(s))
}

fn str_word_count(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let chars = a.opt(2).map(|_| a.str(2));
    let text = a.str(0);
    if a.int(1) != 0 {
        // Upper bound of the words: runs of word characters.
        let mut words = 0usize;
        let mut prev = false;
        for &c in text.as_bytes() {
            let w = c.is_ascii_alphabetic() || c == b'\'' || c == b'-' || chars.is_some();
            if w && !prev {
                words += 1;
            }
            prev = w;
        }
        mem::check(
            vm,
            words
                .saturating_mul(mem::elem(0))
                .saturating_add(text.len()),
        )?;
    }
    let mut n = Notices::new();
    let r = analysis::str_word_count(
        text.as_bytes(),
        a.int(1),
        chars.as_ref().map(ZStr::as_bytes),
        &mut n,
    );
    emit(vm, n)?;
    Ok(match check(vm, r)? {
        WordCount::Count(c) => Value::Int(c),
        WordCount::Words(w) => str_list(w),
        WordCount::Positions(p) => assoc(p.into_iter().map(|(pos, w)| (pos, bytes(w)))),
    })
}

fn str_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let len = if a.has(1) { a.int(1) } else { 1 };
    let s = a.str(0);
    if let Ok(chunk) = usize::try_from(len) {
        if chunk > 0 {
            let pieces = s.len().div_ceil(chunk);
            mem::check(
                vm,
                pieces.saturating_mul(mem::elem(0)).saturating_add(s.len()),
            )?;
        }
    }
    let r = transform::str_split(s.as_bytes(), len);
    let parts = check(vm, r)?;
    Ok(str_list(parts))
}

fn strpbrk(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = search::strpbrk(a.zstr(0).as_bytes(), a.zstr(1).as_bytes());
    Ok(opt_bytes(check(vm, r)?))
}

fn substr_compare(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = compare::substr_compare(
        a.zstr(0).as_bytes(),
        a.zstr(1).as_bytes(),
        a.int(2),
        opt_int(a, 3),
        a.bool(4),
    );
    Ok(Value::Int(check(vm, r)?))
}

const UTF8_DEPRECATION: &str = "visit the php.net documentation for various alternatives";

fn utf8_encode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::utf8_encode(a.zstr(0).as_bytes())))
}

fn utf8_decode(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(bytes(transform::utf8_decode(a.zstr(0).as_bytes())))
}

fn str_getcsv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    use php_stdstr::csv;
    let sep = a.has(1).then(|| a.str(1));
    let enc = a.has(2).then(|| a.str(2));
    let esc = a.has(3).then(|| a.str(3));
    let mut n = Notices::new();
    let opts = csv::csv_options(
        "str_getcsv",
        2,
        sep.as_ref().map(ZStr::as_bytes),
        enc.as_ref().map(ZStr::as_bytes),
        esc.as_ref().map(ZStr::as_bytes),
        &mut n,
    );
    emit(vm, n)?;
    let opts = check(vm, opts)?;
    let input = a.str(0);
    #[allow(clippy::naive_bytecount)]
    let fields = input.iter().filter(|&&c| c == opts.delimiter).count() + 1;
    mem::check(
        vm,
        fields
            .saturating_mul(mem::elem(0))
            .saturating_add(input.len()),
    )?;
    Ok(match csv::str_getcsv(input.as_bytes(), &opts) {
        Some(fields) => str_list(fields),
        None => Value::Array(ZArray::from_values([Value::Null])),
    })
}

/// `RINIT`: forget the previous request's `strtok()` string.
fn reset_strtok(_vm: &mut Vm) {
    STRTOK.with(|t| *t.borrow_mut() = Strtok::new());
}

/// Registers the `string.c` functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.on_request_startup(reset_strtok);
    m.function_static("bin2hex(string $string): string", bin2hex)
        .function_static("hex2bin(string $string): string|false", hex2bin)
        .function_static("strspn(string $string, string $characters, int $offset = 0, ?int $length = null): int", strspn)
        .function_static("strcspn(string $string, string $characters, int $offset = 0, ?int $length = null): int", strcspn)
        .function_static("strcoll(string $string1, string $string2): int", strcoll)
        .deprecated("8.6", "use Collator::compare() instead")
        .function_static("trim(string $string, string $characters = \" \\f\\n\\r\\t\\v\\0\"): string", trim)
        .function_static("rtrim(string $string, string $characters = \" \\f\\n\\r\\t\\v\\0\"): string", rtrim)
        .function_static("chop(string $string, string $characters = \" \\f\\n\\r\\t\\v\\0\"): string", rtrim)
        .function_static("ltrim(string $string, string $characters = \" \\f\\n\\r\\t\\v\\0\"): string", ltrim)
        .function_static("wordwrap(string $string, int $width = 75, string $break = \"\\n\", bool $cut_long_words = false): string", wordwrap)
        .function_static("explode(string $separator, string $string, int $limit = PHP_INT_MAX): array", explode)
        .function_static("implode(array|string $separator, ?array $array = null): string", implode)
        .function_static("join(array|string $separator, ?array $array = null): string", implode)
        .function_static("strtok(string $string, ?string $token = null): string|false", strtok)
        .function_static("strtoupper(string $string): string", strtoupper)
        .function_static("strtolower(string $string): string", strtolower)
        .function_static("str_increment(string $string): string", str_increment)
        .function_static("str_decrement(string $string): string", str_decrement)
        .function_static("stristr(string $haystack, string $needle, bool $before_needle = false): string|false", stristr)
        .function_static("strstr(string $haystack, string $needle, bool $before_needle = false): string|false", strstr)
        .function_static("strchr(string $haystack, string $needle, bool $before_needle = false): string|false", strstr)
        .function_static("strpos(string $haystack, string $needle, int $offset = 0): int|false", strpos)
        .function_static("stripos(string $haystack, string $needle, int $offset = 0): int|false", stripos)
        .function_static("strrpos(string $haystack, string $needle, int $offset = 0): int|false", strrpos)
        .function_static("strripos(string $haystack, string $needle, int $offset = 0): int|false", strripos)
        .function_static("strrchr(string $haystack, string $needle, bool $before_needle = false): string|false", strrchr)
        .function_static("str_contains(string $haystack, string $needle): bool", str_contains)
        .function_static("str_starts_with(string $haystack, string $needle): bool", str_starts_with)
        .function_static("str_ends_with(string $haystack, string $needle): bool", str_ends_with)
        .function_static("chunk_split(string $string, int $length = 76, string $separator = \"\\r\\n\"): string", chunk_split)
        .function_static("substr(string $string, int $offset, ?int $length = null): string", substr)
        .function_static("substr_replace(array|string $string, array|string $replace, array|int $offset, array|int|null $length = null): string|array", substr_replace)
        .function_static("quotemeta(string $string): string", quotemeta)
        .function_static("ord(string $character): int", ord)
        .function_static("chr(int $codepoint): string", chr)
        .function_static("ucfirst(string $string): string", ucfirst)
        .function_static("lcfirst(string $string): string", lcfirst)
        .function_static("ucwords(string $string, string $separators = \" \\t\\r\\n\\f\\v\"): string", ucwords)
        .function_static("strtr(string $string, mixed $from, ?string $to = null): string", strtr)
        .function_static("strrev(string $string): string", strrev)
        .function_static("similar_text(string $string1, string $string2, &$percent = null): int", similar_text)
        .function_static("addcslashes(string $string, string $characters): string", addcslashes)
        .function_static("addslashes(string $string): string", addslashes)
        .function_static("stripcslashes(string $string): string", stripcslashes)
        .function_static("stripslashes(string $string): string", stripslashes)
        .function_static("str_replace(array|string $search, array|string $replace, array|string $subject, &$count = null): array|string", str_replace)
        .function_static("str_ireplace(array|string $search, array|string $replace, array|string $subject, &$count = null): array|string", str_ireplace)
        .function_static("hebrev(string $string, int $max_chars_per_line = 0): string", hebrev)
        .function_static("nl2br(string $string, bool $use_xhtml = true): string", nl2br)
        .function_static("strip_tags(string $string, array|string|null $allowed_tags = null): string", strip_tags_fn)
        .function_static("str_getcsv(string $string, string $separator = \",\", string $enclosure = UNKNOWN, string $escape = UNKNOWN): array", str_getcsv)
        .function_static("str_repeat(string $string, int $times): string", str_repeat)
        .function_static("count_chars(string $string, int $mode = 0): array|string", count_chars)
        .function_static("strnatcmp(string $string1, string $string2): int", strnatcmp)
        .function_static("strnatcasecmp(string $string1, string $string2): int", strnatcasecmp)
        .function_static("substr_count(string $haystack, string $needle, int $offset = 0, ?int $length = null): int", substr_count)
        .function_static("str_pad(string $string, int $length, string $pad_string = \" \", int $pad_type = STR_PAD_RIGHT): string", str_pad)
        .function_static("str_rot13(string $string): string", str_rot13)
        .function_static("str_shuffle(string $string): string", str_shuffle)
        .function_static("str_word_count(string $string, int $format = 0, ?string $characters = null): array|int", str_word_count)
        .function_static("str_split(string $string, int $length = 1): array", str_split)
        .function_static("strpbrk(string $string, string $characters): string|false", strpbrk)
        .function_static("substr_compare(string $haystack, string $needle, int $offset, ?int $length = null, bool $case_insensitive = false): int", substr_compare)
        .function_static("utf8_encode(string $string): string", utf8_encode)
        .deprecated("8.2", UTF8_DEPRECATION)
        .function_static("utf8_decode(string $string): string", utf8_decode)
        .deprecated("8.2", UTF8_DEPRECATION);
    for (name, v) in [
        ("STR_PAD_LEFT", transform::STR_PAD_LEFT),
        ("STR_PAD_RIGHT", transform::STR_PAD_RIGHT),
        ("STR_PAD_BOTH", transform::STR_PAD_BOTH),
        ("PATHINFO_DIRNAME", transform::PATHINFO_DIRNAME),
        ("PATHINFO_BASENAME", transform::PATHINFO_BASENAME),
        ("PATHINFO_EXTENSION", transform::PATHINFO_EXTENSION),
        ("PATHINFO_FILENAME", transform::PATHINFO_FILENAME),
        ("PATHINFO_ALL", transform::PATHINFO_ALL),
        ("CHAR_MAX", 127),
    ] {
        m.constant(name, Value::Int(v));
    }
}

#[cfg(test)]
mod tests {
    use super::implode_valid_utf8;
    use php_runtime::{Value, ZArray, ZStr};

    fn list(items: Vec<Value>) -> ZArray {
        ZArray::from_values(items)
    }

    /// The `implode` cases of ext/zend_test/tests/strings_marked_as_utf8.phpt.
    #[test]
    fn implode_flags_follow_php_implode() {
        let a = || Value::Str(ZStr::from_byte(b'a'));
        let b = || Value::Str(ZStr::from_byte(b'b'));
        let ff = || Value::Str(ZStr::from_byte(0xff));
        let empty = ZStr::empty();
        let bar = ZStr::from_byte(b'|');
        let glue_ff = ZStr::from_byte(0xff);
        assert!(implode_valid_utf8(&empty, &list(vec![a(), b()])));
        assert!(implode_valid_utf8(&bar, &list(vec![a(), b()])));
        assert!(!implode_valid_utf8(&empty, &list(vec![ff(), a(), b()])));
        assert!(!implode_valid_utf8(&empty, &list(vec![a(), b(), ff()])));
        assert!(!implode_valid_utf8(&glue_ff, &list(vec![a(), b()])));
        assert!(implode_valid_utf8(&empty, &list(vec![])));
        assert!(implode_valid_utf8(&glue_ff, &list(vec![])));
        assert!(implode_valid_utf8(&empty, &list(vec![a()])));
        assert!(implode_valid_utf8(&glue_ff, &list(vec![a()])));
        let mixed = list(vec![Value::Int(1), Value::Float(1.0), a()]);
        assert!(implode_valid_utf8(&empty, &mixed));
        let unknown = list(vec![a(), Value::Str(ZStr::from("not known"))]);
        assert!(!implode_valid_utf8(&empty, &unknown));
    }
}
