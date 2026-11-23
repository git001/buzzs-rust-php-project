// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `mb_*` functions that walk arrays (and objects):
//! `mb_convert_encoding` on arrays (`php_mb_convert_encoding_recursive`),
//! `mb_check_encoding` on arrays (`php_mb_check_encoding_recursive`) and
//! `mb_convert_variables` (`mb_recursive_*`).

use php_mbstring::strings::check_str_encoding;
use php_mbstring::{Encoding, MbState};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::funcs::OwnedList;
use crate::mem;
use crate::state::{self, bytes, call, opt_str};

/// Nesting limit standing in for `mb_check_stack_limit()`.
const MAX_DEPTH: usize = 4096;

/// Recursion protection (`GC_PROTECT_RECURSION`) by array identity.
#[derive(Default)]
struct Guard(Vec<usize>);

impl Guard {
    fn enter(&mut self, addr: usize) -> bool {
        if self.0.contains(&addr) || self.0.len() >= MAX_DEPTH {
            return false;
        }
        self.0.push(addr);
        true
    }

    fn leave(&mut self) {
        self.0.pop();
    }
}

// ---- mb_convert_encoding -------------------------------------------------

struct Converter<'a> {
    to: &'static Encoding,
    from: &'a [&'static Encoding],
    guard: Guard,
}

enum Item {
    Keep(Value),
    Skip,
}

impl Converter<'_> {
    fn string(&self, s: &[u8]) -> Option<Vec<u8>> {
        state::with(|st| st.convert_encoding_value(s, self.to, self.from))
    }

    /// `php_mb_convert_encoding_recursive`; `None` is PHP's `NULL` result
    /// (recursion detected).
    fn array(&mut self, vm: &mut Vm, input: &ZArray) -> VmResult<Option<HashTable>> {
        if !self.guard.enter(input.addr()) {
            vm.php_warning("Cannot convert recursively referenced values")?;
            return Ok(None);
        }
        let mut out = HashTable::with_capacity(input.len());
        for (key, entry) in input.iter() {
            if mem::pending() {
                break;
            }
            let key = match key {
                ArrayKey::Str(k) => {
                    let conv = self.string(k.as_bytes());
                    state::flush_diagnostics(vm)?;
                    match conv {
                        Some(k) => ArrayKey::Str(ZStr::new(k)),
                        None => continue,
                    }
                }
                ArrayKey::Int(i) => ArrayKey::Int(*i),
            };
            let item = match entry.deref() {
                Value::Str(s) => {
                    let conv = self.string(s.as_bytes());
                    state::flush_diagnostics(vm)?;
                    conv.map_or(Item::Skip, |c| Item::Keep(bytes(c)))
                }
                v @ (Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)) => {
                    Item::Keep(v)
                }
                Value::Array(inner) => {
                    let t = self.array(vm, &inner)?;
                    Item::Keep(Value::Array(t.map_or_else(ZArray::new, ZArray::from_table)))
                }
                _ => {
                    vm.php_warning("Object is not supported")?;
                    Item::Skip
                }
            };
            if let Item::Keep(v) = item {
                // zend_hash_add: an existing key is kept.
                if !out.contains_key(&key) {
                    out.insert(key, v);
                }
            }
        }
        self.guard.leave();
        Ok(Some(out))
    }
}

fn mb_convert_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let input = a.at(0);
    let to = a.str(1);
    let from = match a.opt(2) {
        Some(v) => Some(OwnedList::from_value(vm, &v)?),
        None => None,
    };
    let (to_enc, from_list) = call(vm, a, |st| {
        st.convert_encoding_args(to.as_bytes(), from.as_ref().map(OwnedList::as_list))
    })?;
    match input {
        Value::Array(arr) => {
            let mut c = Converter {
                to: to_enc,
                from: &from_list,
                guard: Guard::default(),
            };
            let t = c.array(vm, &arr)?;
            let v = t.map_or(Value::Null, |t| Value::Array(ZArray::from_table(t)));
            Ok(v)
        }
        other => {
            let s = vm.value_to_string(&other)?;
            let r = state::with(|st| st.convert_encoding_value(s.as_bytes(), to_enc, &from_list));
            state::flush_diagnostics(vm)?;
            Ok(r.map_or(Value::Bool(false), bytes))
        }
    }
}

// ---- mb_check_encoding ---------------------------------------------------

/// `mb_check_str_encoding`: whether `s` is valid in `enc`. For UTF-8 the
/// answer is the string's `IS_STR_VALID_UTF8` flag, else a validation
/// whose positive result is recorded on the string (not on interned
/// ones), so checking the same string again costs nothing.
fn check_str(s: &ZStr, enc: &'static Encoding) -> bool {
    if enc.is_utf8() {
        s.check_valid_utf8()
    } else {
        check_str_encoding(s.as_bytes(), enc)
    }
}

fn check_recursive(
    vm: &mut Vm,
    arr: &ZArray,
    enc: &'static Encoding,
    guard: &mut Guard,
) -> VmResult<bool> {
    if !guard.enter(arr.addr()) {
        vm.php_warning("Cannot not handle circular references")?;
        return Ok(false);
    }
    let mut valid = true;
    for (key, entry) in arr.iter() {
        if let ArrayKey::Str(k) = key {
            if !check_str(&k, enc) {
                valid = false;
                break;
            }
        }
        match entry.deref() {
            Value::Str(s) => {
                if !check_str(&s, enc) {
                    valid = false;
                }
            }
            Value::Array(inner) => {
                if !check_recursive(vm, &inner, enc, guard)? {
                    valid = false;
                }
            }
            Value::Int(_) | Value::Float(_) | Value::Null | Value::Bool(_) => {}
            _ => valid = false,
        }
    }
    guard.leave();
    Ok(valid)
}

fn mb_check_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let enc_name = opt_str(a, 1);
    let enc_arg = enc_name.as_ref().map(ZStr::as_bytes);
    match a.opt(0) {
        None => call(vm, a, |st| st.mb_check_encoding_noarg(enc_arg)).map(Value::Bool),
        Some(Value::Array(arr)) => {
            let enc = call(vm, a, |st| st.get_encoding(enc_arg, 2, "encoding"))?;
            check_recursive(vm, &arr, enc, &mut Guard::default()).map(Value::Bool)
        }
        Some(v) => {
            let s = vm.value_to_string(&v)?;
            let enc = call(vm, a, |st| st.get_encoding(enc_arg, 2, "encoding"))?;
            Ok(Value::Bool(check_str(&s, enc)))
        }
    }
}

// ---- mb_convert_variables ------------------------------------------------

/// `mb_recursive_find_strings`; `false` means recursion was detected.
fn find_strings(v: &Value, out: &mut Vec<ZStr>, guard: &mut Guard) -> bool {
    match v.deref() {
        Value::Str(s) => {
            out.push(s);
            true
        }
        Value::Array(arr) => {
            if !guard.enter(arr.addr()) {
                return false;
            }
            for (_, e) in arr.iter() {
                if !find_strings(e, out, guard) {
                    return false;
                }
            }
            guard.leave();
            true
        }
        Value::Object(o) => {
            if !guard.enter(o.handle() as usize | (1 << 62)) {
                return false;
            }
            for e in object_values(&o) {
                if !find_strings(&e, out, guard) {
                    return false;
                }
            }
            guard.leave();
            true
        }
        _ => true,
    }
}

/// The property values of an object (declared slots, then dynamic ones).
fn object_values(o: &php_runtime::Obj) -> Vec<Value> {
    let d = php_vm::object::data(o);
    let mut vals: Vec<Value> = d.slots.borrow().iter().flatten().cloned().collect();
    if let Some(dynamic) = d.dynamic.borrow().as_ref() {
        vals.extend(dynamic.values().cloned());
    }
    vals
}

/// Why a conversion stopped (`mb_recursive_convert_variable` returning
/// `true`).
enum Stop {
    /// Recursion detected (or an exception is pending).
    Recursion,
}

/// Depth standing in for `mb_check_stack_limit()`.
const STACK_DEPTH: usize = 10_000;

fn zval_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Resource(_) => "resource",
        _ => "mixed",
    }
}

struct VarConverter {
    from: &'static Encoding,
    to: &'static Encoding,
    /// Arrays being walked (by identity).
    arrays: Vec<usize>,
    /// References being walked.
    refs: Vec<php_runtime::PhpRef>,
    depth: usize,
}

impl VarConverter {
    fn string(&self, s: &[u8]) -> Value {
        bytes(state::with(|st: &mut MbState| {
            st.convert_ex(s, self.to, self.from)
        }))
    }

    /// Converts one slot in place. A slot holding a reference to a string
    /// is replaced by the converted string (as PHP does); references to
    /// arrays and objects are converted through the reference.
    fn slot(
        &mut self,
        vm: &mut Vm,
        arg_num: usize,
        slot: &mut Value,
    ) -> VmResult<Result<(), Stop>> {
        if let Value::Reference(r) = slot {
            let r = r.clone();
            if self.refs.iter().any(|x| php_runtime::PhpRef::ptr_eq(x, &r)) {
                return Ok(Err(Stop::Recursion));
            }
            let inner = r.get();
            if let Value::Str(s) = &inner {
                *slot = self.string(s.as_bytes());
                return Ok(Ok(()));
            }
            self.refs.push(r.clone());
            let res = self.value(vm, arg_num, inner)?;
            self.refs.pop();
            return Ok(match res {
                Ok(nv) => {
                    r.set(nv);
                    Ok(())
                }
                Err(e) => Err(e),
            });
        }
        let inner = std::mem::take(slot);
        let res = self.value(vm, arg_num, inner)?;
        Ok(match res {
            Ok(nv) => {
                *slot = nv;
                Ok(())
            }
            Err(e) => Err(e),
        })
    }

    /// Converts a dereferenced value.
    fn value(&mut self, vm: &mut Vm, arg_num: usize, v: Value) -> VmResult<Result<Value, Stop>> {
        self.depth += 1;
        let r = self.value_inner(vm, arg_num, v);
        self.depth -= 1;
        r
    }

    #[allow(clippy::many_single_char_names)]
    fn value_inner(
        &mut self,
        vm: &mut Vm,
        arg_num: usize,
        v: Value,
    ) -> VmResult<Result<Value, Stop>> {
        if self.depth > STACK_DEPTH {
            return Err(stack_limit_error(vm));
        }
        match v {
            Value::Str(s) => Ok(Ok(self.string(s.as_bytes()))),
            Value::Array(mut arr) => {
                let addr = arr.addr();
                if self.arrays.contains(&addr) {
                    return Ok(Err(Stop::Recursion));
                }
                self.arrays.push(addr);
                let t = arr.make_mut();
                let keys: Vec<ArrayKey> = t.keys().cloned().collect();
                for k in keys {
                    let Some(mut e) = t.get_mut(&k).map(std::mem::take) else {
                        continue;
                    };
                    let res = self.slot(vm, arg_num, &mut e);
                    if let Some(s) = t.get_mut(&k) {
                        *s = e;
                    }
                    if mem::pending() {
                        self.arrays.pop();
                        return Ok(Err(Stop::Recursion));
                    }
                    if res?.is_err() {
                        self.arrays.pop();
                        return Ok(Err(Stop::Recursion));
                    }
                }
                self.arrays.pop();
                Ok(Ok(Value::Array(arr)))
            }
            Value::Object(o) => {
                vm.php_deprecated(&format!(
                    "Passing an object for argument #{arg_num} $vars to mb_convert_variables() is deprecated, call get_object_vars() first instead"
                ))?;
                let addr = o.handle() as usize | (1 << 62);
                if self.arrays.contains(&addr) {
                    return Ok(Err(Stop::Recursion));
                }
                self.arrays.push(addr);
                let d = php_vm::object::data(&o);
                let n = d.slots.borrow().len();
                for i in 0..n {
                    let Some(mut e) = d.slots.borrow_mut().get_mut(i).and_then(Option::take) else {
                        continue;
                    };
                    let res = self.slot(vm, arg_num, &mut e);
                    if let Some(s) = d.slots.borrow_mut().get_mut(i) {
                        *s = Some(e);
                    }
                    if res?.is_err() {
                        self.arrays.pop();
                        return Ok(Err(Stop::Recursion));
                    }
                }
                let keys: Vec<ArrayKey> = d
                    .dynamic
                    .borrow()
                    .as_ref()
                    .map(|t| t.keys().cloned().collect())
                    .unwrap_or_default();
                for k in keys {
                    let Some(mut e) = d
                        .dynamic
                        .borrow_mut()
                        .as_mut()
                        .and_then(|t| t.get_mut(&k).map(std::mem::take))
                    else {
                        continue;
                    };
                    let res = self.slot(vm, arg_num, &mut e);
                    if let Some(t) = d.dynamic.borrow_mut().as_mut() {
                        if let Some(s) = t.get_mut(&k) {
                            *s = e;
                        }
                    }
                    if res?.is_err() {
                        self.arrays.pop();
                        return Ok(Err(Stop::Recursion));
                    }
                }
                self.arrays.pop();
                Ok(Ok(Value::Object(o)))
            }
            other => {
                vm.php_warning(&format!(
                    "Argument #{arg_num} must be of type string|array|object or only contain entries of type string|array|object, {} given",
                    zval_type_name(&other)
                ))?;
                Ok(Ok(other))
            }
        }
    }
}

/// The `Error` `zend_call_stack_size_error()` throws.
fn stack_limit_error(vm: &mut Vm) -> php_vm::Throw {
    let size = vm.ini_long("zend.max_allowed_stack_size").max(0)
        - vm.ini_long("zend.reserved_stack_size").max(0);
    vm.throw(
        "Error",
        &format!(
            "Maximum call stack size of {size} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
        ),
    )
}

#[allow(clippy::many_single_char_names)]
fn mb_convert_variables(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let to = a.str(0);
    let from = OwnedList::from_value(vm, &a.at(1))?;
    let vars: Vec<Value> = a.values.iter().skip(2).cloned().collect();
    let mut strings = Vec::new();
    let mut recursive = false;
    let multi = match &from {
        OwnedList::Array { list, .. } => list.len() > 1,
        OwnedList::Str(s) => s.contains(&b',') || s.eq_ignore_ascii_case(b"auto"),
    };
    if multi {
        let mut guard = Guard::default();
        for v in &vars {
            if !find_strings(v, &mut strings, &mut guard) {
                recursive = true;
                break;
            }
        }
    }
    let string_refs: Vec<&[u8]> = strings.iter().map(ZStr::as_bytes).collect();
    let all = from.is_all();
    let r = state::with(|st| {
        if recursive {
            // Resolve the arguments (for their errors) before reporting.
            st.convert_variables_encodings(to.as_bytes(), from.as_list(), all, &[])
                .map(|_| None)
        } else {
            st.convert_variables_encodings(to.as_bytes(), from.as_list(), all, &string_refs)
        }
    });
    let r = state::finish(vm, a, r)?;
    if recursive {
        vm.php_warning("Cannot handle recursive references")?;
        return Ok(Value::Bool(false));
    }
    let Some((from_enc, to_enc)) = r else {
        return Ok(Value::Bool(false));
    };
    let mut conv = VarConverter {
        from: from_enc,
        to: to_enc,
        arrays: Vec::new(),
        refs: Vec::new(),
        depth: 0,
    };
    for (i, v) in vars.iter().enumerate() {
        let Value::Reference(r) = v else {
            continue;
        };
        conv.refs.push(r.clone());
        let previous = r.get();
        let res = conv.value(vm, i + 3, previous)?;
        conv.refs.pop();
        match res {
            Ok(nv) => {
                r.set(nv);
            }
            Err(Stop::Recursion) if mem::pending() => return Ok(Value::Bool(false)),
            Err(Stop::Recursion) => {
                vm.php_warning("Cannot handle recursive references")?;
                return Ok(Value::Bool(false));
            }
        }
    }
    Ok(Value::from(from_enc.name))
}

/// Registers the array-walking functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "mb_convert_encoding(array|string $string, string $to_encoding, array|string|null $from_encoding = null): array|string|false",
        mb_convert_encoding,
    )
    .function_static(
        "mb_check_encoding(array|string|null $value = null, ?string $encoding = null): bool",
        mb_check_encoding,
    )
    .function_static(
        "mb_convert_variables(string $to_encoding, array|string $from_encoding, mixed &$var, mixed &...$vars): string|false",
        mb_convert_variables,
    );
}

#[cfg(test)]
mod tests {
    use super::check_str;
    use php_runtime::ZStr;

    /// `mb_check_str_encoding`: a UTF-8 check is answered by and recorded
    /// in the string's `IS_STR_VALID_UTF8` flag; other encodings are
    /// checked every time and record nothing.
    #[test]
    fn utf8_checks_are_cached_on_the_string() {
        let utf8 = php_mbstring::encoding::utf8();
        let s = ZStr::from("caf\u{e9}");
        assert!(!s.is_valid_utf8());
        assert!(check_str(&s, utf8));
        assert!(s.is_valid_utf8());
        assert!(check_str(&s.clone(), utf8));
        let bad = ZStr::new(b"caf\xe9".to_vec());
        assert!(!check_str(&bad, utf8));
        assert!(!bad.is_valid_utf8());
        let latin1 = php_mbstring::encoding::by_cname(b"ISO-8859-1").expect("ISO-8859-1");
        let other = ZStr::from("abc");
        assert!(check_str(&other, latin1));
        assert!(!other.is_valid_utf8(), "only UTF-8 checks record the flag");
    }
}
