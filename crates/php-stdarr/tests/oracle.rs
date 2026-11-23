// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Table-driven oracle test: replays the cases recorded by
//! `tests/data/gen_oracle.php` (run against the C implementation) and
//! compares results, by-reference arguments, diagnostics and exceptions.
//!
//! Regenerate the fixture with
//! `cargo test -p php-stdarr --test oracle -- --ignored regenerate_oracle`.

// The Mt19937 emulation mirrors C's unsigned/signed conversions.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use php_runtime::serialize::{serialize, unserialize, UnserializeOptions};
use php_runtime::{
    ops, ArrayKey, CollectingContext, HashTable, PhpRef, RtResult, RuntimeError, Value, ZArray,
    ZStr,
};
use php_stdarr::symbols::{SymbolTable, VarSlot};
use php_stdarr::{basic, callback, merge, random, range, setops, sort, stack, symbols};

type Ctx = CollectingContext;
type Cb = Box<dyn FnMut(&mut Ctx, &[Value]) -> RtResult<Value>>;
/// A by-reference function under test: `(ctx, &$array, other args)`.
type ByRefFn<'a> = dyn Fn(&mut Ctx, &PhpRef, &[Value]) -> RtResult<Value> + 'a;

// ---------------------------------------------------------------------------
// Fixture decoding
// ---------------------------------------------------------------------------

fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c) << (18 - 6 * i);
        }
        let k = chunk.len();
        let b = n.to_be_bytes();
        out.extend_from_slice(&b[1..k]);
    }
    out
}

fn unser(data: &[u8]) -> Value {
    let mut ctx = Ctx::new();
    unserialize(data, &UnserializeOptions::default(), &mut ctx)
        .expect("unserialize")
        .expect("valid serialized data")
}

fn try_unser(data: &[u8]) -> Option<Value> {
    let mut ctx = Ctx::new();
    unserialize(data, &UnserializeOptions::default(), &mut ctx)
        .ok()
        .flatten()
}

/// `serialize()`, digested like `digest()` in `gen_oracle.php` when long.
fn ser(v: &Value) -> String {
    let mut ctx = Ctx::new();
    let bytes = serialize(v, &mut ctx).expect("serialize");
    if bytes.len() > 4096 {
        return format!("crc32:{:08x}:{}", crc32(&bytes), bytes.len());
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// CRC-32 (IEEE), as PHP's `crc32()`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// PHP's `unserialize()` builds every non-empty array as a hash table
/// (`zend_hash_real_init_mixed`); rebuild arrays that way so that
/// layout-dependent functions (`array_rand`, packed fast paths) see the
/// same tables as the oracle did.
fn hash_layout(v: &Value) -> Value {
    let Value::Array(a) = v else {
        return v.clone();
    };
    if a.is_empty() {
        return v.clone();
    }
    let mut t = HashTable::with_capacity(a.len());
    for (k, v) in a.iter() {
        t.insert(k.clone(), hash_layout(v));
    }
    if t.is_packed() {
        let dummy = ArrayKey::Str(ZStr::from("\0dummy"));
        t.insert(dummy.clone(), Value::Null);
        t.remove(&dummy);
        t = t.clone();
    }
    t.set_internal_pointer(0);
    Value::Array(ZArray::from_table(t))
}

#[derive(Debug)]
struct Case {
    func: String,
    /// Arguments with PHP's unserialize layout (hash tables).
    args: Vec<Value>,
    /// Arguments as the runtime builds them (packed where possible), which
    /// exercises the packed fast paths; results must not depend on it.
    natural_args: Vec<Value>,
    args_text: String,
    result: Option<String>,
    byref: Option<String>,
    msgs: Vec<String>,
    exc: Option<String>,
}

fn field<'a>(t: &'a HashTable, k: &str) -> &'a Value {
    t.get(&ArrayKey::from(k)).unwrap_or(&Value::Null)
}

fn opt_str(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string_lossy()),
        _ => None,
    }
}

fn load_cases() -> Vec<Case> {
    let text = include_str!("data/oracle.txt");
    text.lines()
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(n, line)| {
            let raw = b64_decode(line);
            assert!(
                try_unser(&raw).is_some(),
                "fixture line {} does not unserialize: {}",
                n + 1,
                String::from_utf8_lossy(&raw)
            );
            let v = unser(&raw);
            let Value::Array(t) = v else {
                panic!("bad case")
            };
            let args_ser = match field(&t, "args") {
                Value::Str(s) => s.as_bytes().to_vec(),
                _ => panic!("no args"),
            };
            assert!(
                try_unser(&args_ser).is_some(),
                "fixture line {} has bad args: {}",
                n + 1,
                String::from_utf8_lossy(&args_ser)
            );
            let natural_args: Vec<Value> = match unser(&args_ser) {
                Value::Array(a) => a.values().cloned().collect(),
                _ => panic!("args not array"),
            };
            let args = natural_args.iter().map(hash_layout).collect();
            let msgs = match field(&t, "msgs") {
                Value::Array(a) => a.values().map(|m| opt_str(m).unwrap_or_default()).collect(),
                _ => vec![],
            };
            Case {
                func: opt_str(field(&t, "func")).unwrap_or_default(),
                args,
                natural_args,
                args_text: String::from_utf8_lossy(&args_ser).into_owned(),
                result: opt_str(field(&t, "result")),
                byref: opt_str(field(&t, "byref")),
                msgs,
                exc: opt_str(field(&t, "exc")),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Callbacks (mirroring $CB in gen_oracle.php)
// ---------------------------------------------------------------------------

fn d(v: &Value) -> Value {
    match v {
        Value::Reference(r) => r.get(),
        other => other.clone(),
    }
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).map(d).unwrap_or_default()
}

fn callback_named(name: &str) -> Option<Cb> {
    let cb: Cb = match name {
        "cmp" => Box::new(|c, a| ops::spaceship(&arg(a, 0), &arg(a, 1), c)),
        "rcmp" => Box::new(|c, a| ops::spaceship(&arg(a, 1), &arg(a, 0), c)),
        "cmp_len" => Box::new(|c, a| {
            let x = arg(a, 0).to_zstr(c)?.len();
            let y = arg(a, 1).to_zstr(c)?.len();
            Ok(Value::Int(x.cmp(&y) as i64))
        }),
        "cmp_case" => Box::new(|c, a| {
            let x = arg(a, 0).to_zstr(c)?;
            let y = arg(a, 1).to_zstr(c)?;
            Ok(Value::Int(i64::from(
                php_runtime::string::binary_strcasecmp(x.as_bytes(), y.as_bytes()).signum(),
            )))
        }),
        "cmp_zero" => Box::new(|_, _| Ok(Value::Int(0))),
        "cmp_float" => Box::new(|c, a| {
            let diff = ops::sub(&arg(a, 0), &arg(a, 1), c)?;
            ops::mul(&diff, &Value::Float(0.1), c)
        }),
        "cmp_str" => Box::new(|c, a| {
            let x = arg(a, 0).to_zstr(c)?;
            let y = arg(a, 1).to_zstr(c)?;
            Ok(Value::Int(i64::from(
                php_runtime::string::binary_strcmp(x.as_bytes(), y.as_bytes()).signum(),
            )))
        }),
        "is_odd" => Box::new(|c, a| {
            let m = ops::modulo(&arg(a, 0), &Value::Int(2), c)?;
            Ok(Value::Bool(ops::is_equal(&m, &Value::Int(1), c)?))
        }),
        "truthy" => Box::new(|c, a| Ok(Value::Bool(arg(a, 0).to_bool(c)?))),
        "double" => Box::new(|c, a| ops::mul(&arg(a, 0), &Value::Int(2), c)),
        "is_str" => Box::new(|_, a| Ok(Value::Bool(matches!(arg(a, 0), Value::Str(_))))),
        "both" => Box::new(|c, a| {
            Ok(Value::Bool(
                matches!(arg(a, 1), Value::Int(_))
                    && ops::is_smaller(&Value::Int(1), &arg(a, 0), c)?,
            ))
        }),
        "sum2" => Box::new(|c, a| ops::add(&arg(a, 0), &arg(a, 1), c)),
        "concat" => Box::new(|c, a| {
            let mut parts = Vec::new();
            for v in a {
                parts.push(php_runtime::dump::var_export(&d(v), c)?);
            }
            Ok(Value::Str(ZStr::from(parts.join(&b","[..]))))
        }),
        "pair" => Box::new(|_, a| Ok(Value::Array(ZArray::from_values([arg(a, 0), arg(a, 1)])))),
        "gt2" => Box::new(|c, a| Ok(Value::Bool(ops::is_smaller(&Value::Int(2), &arg(a, 0), c)?))),
        "walk_append" => Box::new(|c, a| {
            let Some(Value::Reference(r)) = a.first() else {
                panic!("walk callback without reference")
            };
            let mut v = ops::concat(&r.get(), &Value::from("_"), c)?;
            v = ops::concat(&v, &arg(a, 1), c)?;
            let p = arg(a, 2);
            v = ops::concat(&v, &p, c)?;
            r.set(v);
            Ok(Value::Null)
        }),
        "walk_double" => Box::new(|c, a| {
            let Some(Value::Reference(r)) = a.first() else {
                panic!("walk callback without reference")
            };
            let v = ops::mul(&r.get(), &Value::Int(2), c)?;
            r.set(v);
            Ok(Value::Null)
        }),
        "throw" => Box::new(|_, _| Err(RuntimeError::Error("boom".to_string()))),
        "v_k" => Box::new(|c, a| {
            let k = arg(a, 1).to_zstr(c)?;
            let v = arg(a, 0).to_zstr(c)?;
            let mut s = k.as_bytes().to_vec();
            s.push(b'=');
            s.extend_from_slice(v.as_bytes());
            Ok(Value::Str(ZStr::from(s)))
        }),
        _ => return None,
    };
    Some(cb)
}

fn as_cb(v: &Value) -> Option<Cb> {
    match v {
        Value::Str(s) if s.as_bytes().first() == Some(&b'@') => {
            callback_named(&s.to_string_lossy()[1..])
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mt19937 (default engine) for the random functions
// ---------------------------------------------------------------------------

struct Mt {
    state: [u32; 624],
    count: usize,
}

impl Mt {
    fn new(seed: u32) -> Self {
        let mut state = [0u32; 624];
        state[0] = seed;
        for i in 1..624 {
            let prev = state[i - 1];
            state[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        let mut mt = Mt { state, count: 624 };
        mt.reload();
        mt
    }

    fn reload(&mut self) {
        let twist = |m: u32, u: u32, v: u32| -> u32 {
            let mix = (u & 0x8000_0000) | (v & 0x7fff_ffff);
            m ^ (mix >> 1) ^ (0u32.wrapping_sub(v & 1) & 0x9908_b0df)
        };
        let s = &mut self.state;
        for i in 0..(624 - 397) {
            s[i] = twist(s[i + 397], s[i], s[i + 1]);
        }
        for i in (624 - 397)..623 {
            s[i] = twist(s[i + 397 - 624], s[i], s[i + 1]);
        }
        s[623] = twist(s[396], s[623], s[0]);
        self.count = 0;
    }

    fn next32(&mut self) -> u32 {
        if self.count >= 624 {
            self.reload();
        }
        let mut s1 = self.state[self.count];
        self.count += 1;
        s1 ^= s1 >> 11;
        s1 ^= (s1 << 7) & 0x9d2c_5680;
        s1 ^= (s1 << 15) & 0xefc6_0000;
        s1 ^ (s1 >> 18)
    }

    fn range(&mut self, min: i64, max: i64) -> i64 {
        let umax = (max as u64).wrapping_sub(min as u64);
        if umax > u64::from(u32::MAX) {
            let mut result = u64::from(self.next32()) | (u64::from(self.next32()) << 32);
            if umax == u64::MAX {
                return result as i64;
            }
            let umax = umax + 1;
            if umax.is_power_of_two() {
                return (result & (umax - 1)).wrapping_add(min as u64) as i64;
            }
            let limit = u64::MAX - (u64::MAX % umax) - 1;
            while result > limit {
                result = u64::from(self.next32()) | (u64::from(self.next32()) << 32);
            }
            return (result % umax).wrapping_add(min as u64) as i64;
        }
        let umax = umax as u32;
        let mut result = self.next32();
        if umax == u32::MAX {
            return i64::from(result).wrapping_add(min);
        }
        let umax = umax + 1;
        if umax.is_power_of_two() {
            return i64::from(result & (umax - 1)) + min;
        }
        let limit = u32::MAX - (u32::MAX % umax) - 1;
        while result > limit {
            result = self.next32();
        }
        i64::from(result % umax) + min
    }
}

// ---------------------------------------------------------------------------
// Symbol table for extract/compact
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Vars(Vec<(ZStr, Value)>);

impl SymbolTable for Vars {
    fn lookup(&mut self, name: &ZStr) -> VarSlot {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map_or(VarSlot::Missing, |(_, v)| VarSlot::Defined(v.clone()))
    }

    fn assign(&mut self, name: &ZStr, value: Value) -> RtResult<()> {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some((_, Value::Reference(r))) => {
                r.set(value);
            }
            Some((_, slot)) => *slot = value,
            None => self.0.push((name.clone(), value)),
        }
        Ok(())
    }

    fn set(&mut self, name: &ZStr, value: Value) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((name.clone(), value)),
        }
    }
}

fn vars_from(v: &Value) -> Vars {
    let mut vars = Vars::default();
    if let Value::Array(a) = v {
        for (k, v) in a.iter() {
            vars.0.push((k.to_zstr(), v.clone()));
        }
    }
    vars
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

fn arr(v: &Value) -> ZArray {
    match v {
        Value::Array(a) => a.clone(),
        other => panic!("expected array, got {other:?}"),
    }
}

fn int(v: Option<&Value>, default: i64) -> i64 {
    match v {
        Some(Value::Int(i)) => *i,
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Null) | None => default,
        Some(other) => panic!("expected int, got {other:?}"),
    }
}

fn opt_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Null) | None => None,
        other => Some(int(other, 0)),
    }
}

/// `bool` parameter coercion (zpp weak mode) for the scalars in the fixture.
fn boolean(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Int(i)) => *i != 0,
        Some(Value::Str(s)) => !(s.is_empty() || s.as_bytes() == b"0"),
        _ => false,
    }
}

struct Outcome {
    result: RtResult<Value>,
    byref: Option<Value>,
}

/// Splits trailing callbacks off a variadic argument list.
fn split_callbacks(args: &[Value], n: usize) -> (Vec<Value>, Vec<Cb>) {
    let split = args.len() - n;
    let cbs = args[split..]
        .iter()
        .map(|v| as_cb(v).expect("callback"))
        .collect();
    (args[..split].to_vec(), cbs)
}

#[allow(clippy::too_many_lines)]
fn dispatch(ctx: &mut Ctx, func: &str, args: &[Value]) -> Outcome {
    let a0 = || arr(&args[0]);
    let ok = |r: RtResult<ZArray>| r.map(Value::Array);
    let simple = |result: RtResult<Value>| Outcome {
        result,
        byref: None,
    };
    let byref_fn = |ctx: &mut Ctx, f: &ByRefFn<'_>| {
        let r = PhpRef::new(args[0].clone());
        let result = f(ctx, &r, &args[1..]);
        let mut after = vec![r.get()];
        after.extend(args[1..].iter().map(|v| {
            if as_cb(v).is_some() {
                Value::Null
            } else {
                v.clone()
            }
        }));
        Outcome {
            result,
            byref: Some(Value::Array(ZArray::from_values(after))),
        }
    };
    let t = Value::Bool(true);
    match func {
        "sort" | "rsort" | "asort" | "arsort" | "ksort" | "krsort" => {
            byref_fn(ctx, &|c, r, rest| {
                let fl = int(rest.first(), 0);
                match func {
                    "sort" => sort::sort(c, r, fl),
                    "rsort" => sort::rsort(c, r, fl),
                    "asort" => sort::asort(c, r, fl),
                    "arsort" => sort::arsort(c, r, fl),
                    "ksort" => sort::ksort(c, r, fl),
                    _ => sort::krsort(c, r, fl),
                }
                .map(|()| t.clone())
            })
        }
        "natsort" => byref_fn(ctx, &|c, r, _| sort::natsort(c, r).map(|()| t.clone())),
        "natcasesort" => byref_fn(ctx, &|c, r, _| sort::natcasesort(c, r).map(|()| t.clone())),
        "usort" | "uasort" | "uksort" => byref_fn(ctx, &|c, r, rest| {
            let mut cb = as_cb(&rest[0]).expect("cb");
            match func {
                "usort" => sort::usort(c, r, &mut cb),
                "uasort" => sort::uasort(c, r, &mut cb),
                _ => sort::uksort(c, r, &mut cb),
            }
            .map(|()| t.clone())
        }),
        "array_multisort" => {
            let refs: Vec<Value> = args
                .iter()
                .map(|v| match v {
                    Value::Array(_) => Value::Reference(PhpRef::new(v.clone())),
                    other => other.clone(),
                })
                .collect();
            let result = sort::array_multisort(ctx, &refs).map(|()| t.clone());
            let after: Vec<Value> = refs.iter().map(d).collect();
            Outcome {
                result,
                byref: Some(Value::Array(ZArray::from_values(after))),
            }
        }
        "array_walk" | "array_walk_recursive" => byref_fn(ctx, &|c, r, rest| {
            let mut cb = as_cb(&rest[0]).expect("cb");
            let ud = rest.get(1);
            if func == "array_walk" {
                callback::array_walk(c, r, &mut cb, ud)
            } else {
                callback::array_walk_recursive(c, r, &mut cb, ud)
            }
            .map(|()| t.clone())
        }),
        "array_push" => byref_fn(ctx, &|_, r, rest| {
            stack::array_push(r, rest).map(Value::Int)
        }),
        "array_unshift" => byref_fn(ctx, &|_, r, rest| {
            stack::array_unshift(r, rest).map(Value::Int)
        }),
        "array_pop" => byref_fn(ctx, &|_, r, _| stack::array_pop(r)),
        "array_shift" => byref_fn(ctx, &|_, r, _| stack::array_shift(r)),
        "array_splice" => byref_fn(ctx, &|c, r, rest| {
            stack::array_splice(
                c,
                r,
                int(rest.first(), 0),
                opt_int(rest.get(1)),
                rest.get(2),
            )
            .map(Value::Array)
        }),
        "count" => {
            simple(basic::count(ctx, "count", &args[0], int(args.get(1), 0)).map(Value::Int))
        }
        "min" => simple(basic::min(ctx, args)),
        "max" => simple(basic::max(ctx, args)),
        "array_sum" => simple(basic::array_sum(ctx, &a0())),
        "array_product" => simple(basic::array_product(ctx, &a0())),
        "array_values" => simple(Ok(Value::Array(basic::array_values(&a0())))),
        "array_keys" => simple(ok(basic::array_keys(
            ctx,
            &a0(),
            args.get(1),
            boolean(args.get(2)),
        ))),
        "array_count_values" => simple(ok(basic::array_count_values(ctx, &a0()))),
        "array_flip" => simple(ok(basic::array_flip(ctx, &a0()))),
        "array_change_key_case" => {
            simple(ok(basic::array_change_key_case(&a0(), int(args.get(1), 0))))
        }
        "array_key_first" => simple(Ok(basic::array_key_first(&a0()))),
        "array_key_last" => simple(Ok(basic::array_key_last(&a0()))),
        "array_first" => simple(Ok(basic::array_first(&a0()))),
        "array_last" => simple(Ok(basic::array_last(&a0()))),
        "array_is_list" => simple(Ok(Value::Bool(basic::array_is_list(&a0())))),
        "array_reverse" => simple(Ok(Value::Array(basic::array_reverse(
            &a0(),
            boolean(args.get(1)),
        )))),
        "array_unique" => simple(ok(setops::array_unique(ctx, &a0(), int(args.get(1), 2)))),
        "array_fill_keys" => simple(ok(basic::array_fill_keys(ctx, &a0(), &args[1]))),
        "array_fill" => simple(ok(basic::array_fill(
            int(args.first(), 0),
            int(args.get(1), 0),
            &args[2],
        ))),
        "array_slice" => simple(Ok(Value::Array(basic::array_slice(
            &a0(),
            int(args.get(1), 0),
            opt_int(args.get(2)),
            boolean(args.get(3)),
        )))),
        "array_chunk" => simple(ok(basic::array_chunk(
            &a0(),
            int(args.get(1), 0),
            boolean(args.get(2)),
        ))),
        "array_pad" => simple(ok(basic::array_pad(&a0(), int(args.get(1), 0), &args[2]))),
        "in_array" => simple(
            basic::in_array(ctx, &args[0], &arr(&args[1]), boolean(args.get(2))).map(Value::Bool),
        ),
        "array_search" => simple(basic::array_search(
            ctx,
            &args[0],
            &arr(&args[1]),
            boolean(args.get(2)),
        )),
        "array_key_exists" => simple(
            basic::array_key_exists(ctx, "array_key_exists", &args[0], &arr(&args[1]))
                .map(Value::Bool),
        ),
        "array_column" => {
            let col = args.get(1).and_then(basic::ColumnKey::from_value);
            let idx = args.get(2).and_then(basic::ColumnKey::from_value);
            simple(ok(basic::array_column(
                ctx,
                &a0(),
                col.as_ref(),
                idx.as_ref(),
            )))
        }
        "array_combine" => simple(ok(basic::array_combine(ctx, &a0(), &arr(&args[1])))),
        "array_map" => {
            let mut cb = as_cb(&args[0]);
            simple(ok(callback::array_map(
                ctx,
                cb.as_mut().map(|b| b as &mut php_stdarr::Callback<'_, Ctx>),
                &args[1..],
            )))
        }
        "array_filter" => {
            let mut cb = args.get(1).and_then(as_cb);
            simple(ok(callback::array_filter(
                ctx,
                &a0(),
                cb.as_mut().map(|b| b as &mut php_stdarr::Callback<'_, Ctx>),
                int(args.get(2), 0),
            )))
        }
        "array_reduce" => {
            let mut cb = as_cb(&args[1]).expect("cb");
            simple(callback::array_reduce(ctx, &a0(), &mut cb, args.get(2)))
        }
        "array_find" | "array_find_key" | "array_any" | "array_all" => {
            let mut cb = as_cb(&args[1]).expect("cb");
            let a = a0();
            simple(match func {
                "array_find" => callback::array_find(ctx, &a, &mut cb),
                "array_find_key" => callback::array_find_key(ctx, &a, &mut cb),
                "array_any" => callback::array_any(ctx, &a, &mut cb).map(Value::Bool),
                _ => callback::array_all(ctx, &a, &mut cb).map(Value::Bool),
            })
        }
        "range" => simple(ok(range::range(ctx, &args[0], &args[1], args.get(2)))),
        "array_merge" => simple(ok(merge::array_merge(ctx, args))),
        "array_merge_recursive" => simple(ok(merge::array_merge_recursive(ctx, args))),
        "array_replace" => simple(ok(merge::array_replace(args))),
        "array_replace_recursive" => simple(ok(merge::array_replace_recursive(args))),
        "array_diff" => simple(ok(setops::array_diff(ctx, args))),
        "array_diff_key" => simple(ok(setops::array_diff_key(ctx, args))),
        "array_diff_assoc" => simple(ok(setops::array_diff_assoc(ctx, args))),
        "array_intersect" => simple(ok(setops::array_intersect(ctx, args))),
        "array_intersect_key" => simple(ok(setops::array_intersect_key(ctx, args))),
        "array_intersect_assoc" => simple(ok(setops::array_intersect_assoc(ctx, args))),
        "array_udiff"
        | "array_uintersect"
        | "array_diff_ukey"
        | "array_intersect_ukey"
        | "array_diff_uassoc"
        | "array_intersect_uassoc"
        | "array_udiff_assoc"
        | "array_uintersect_assoc" => {
            let (arrays, mut cbs) = split_callbacks(args, 1);
            let cb = &mut cbs[0];
            simple(ok(match func {
                "array_udiff" => setops::array_udiff(ctx, &arrays, cb),
                "array_uintersect" => setops::array_uintersect(ctx, &arrays, cb),
                "array_diff_ukey" => setops::array_diff_ukey(ctx, &arrays, cb),
                "array_intersect_ukey" => setops::array_intersect_ukey(ctx, &arrays, cb),
                "array_diff_uassoc" => setops::array_diff_uassoc(ctx, &arrays, cb),
                "array_intersect_uassoc" => setops::array_intersect_uassoc(ctx, &arrays, cb),
                "array_udiff_assoc" => setops::array_udiff_assoc(ctx, &arrays, cb),
                _ => setops::array_uintersect_assoc(ctx, &arrays, cb),
            }))
        }
        "array_udiff_uassoc" | "array_uintersect_uassoc" => {
            let (arrays, mut cbs) = split_callbacks(args, 2);
            let mut k = cbs.pop().expect("key cb");
            let mut dcb = cbs.pop().expect("data cb");
            simple(ok(if func == "array_udiff_uassoc" {
                setops::array_udiff_uassoc(ctx, &arrays, &mut dcb, &mut k)
            } else {
                setops::array_uintersect_uassoc(ctx, &arrays, &mut dcb, &mut k)
            }))
        }
        "extract" => {
            let mut vars = vars_from(&args[0]);
            let prefix = match args.get(3) {
                Some(Value::Str(s)) => Some(s.clone()),
                _ => None,
            };
            let r = symbols::extract(&mut vars, &args[1], int(args.get(2), 0), prefix.as_ref());
            simple(r.and_then(|n| {
                let mut t = HashTable::new();
                for (k, v) in &vars.0 {
                    t.insert(ArrayKey::from_zstr(k), d(v));
                }
                sort::sort_table(
                    ctx,
                    &mut t,
                    true,
                    false,
                    php_stdarr::compare::SortKind::Regular,
                    false,
                )?;
                Ok(Value::Array(ZArray::from_values([
                    Value::Int(n),
                    Value::Array(ZArray::from_table(t)),
                ])))
            }))
        }
        "compact" => {
            let mut vars = vars_from(&args[0]);
            simple(ok(symbols::compact(ctx, &mut vars, &args[1..])))
        }
        f if f.starts_with("seed:") => {
            let seed = int(args.first(), 0);
            let mut mt = Mt::new(seed as u32);
            let mut rng = |a: i64, b: i64| -> RtResult<i64> { Ok(mt.range(a, b)) };
            let rest = &args[1..];
            match &f[5..] {
                "shuffle" => {
                    let r = PhpRef::new(rest[0].clone());
                    let result = random::shuffle(&r, &mut rng).map(|()| t.clone());
                    Outcome {
                        result,
                        byref: Some(Value::Array(ZArray::from_values([r.get()]))),
                    }
                }
                "array_rand" => simple(random::array_rand(
                    &arr(&rest[0]),
                    int(rest.get(1), 1),
                    &mut rng,
                )),
                "str_shuffle" => {
                    let s = match &rest[0] {
                        Value::Str(s) => s.as_bytes().to_vec(),
                        _ => vec![],
                    };
                    simple(random::str_shuffle(&s, &mut rng).map(|v| Value::Str(ZStr::from(v))))
                }
                other => panic!("unknown seeded function {other}"),
            }
        }
        other => panic!("no dispatcher for {other}"),
    }
}

fn exc_text(e: &RuntimeError) -> String {
    match e {
        RuntimeError::Thrown(_) => "Thrown".to_string(),
        other => format!(
            "{}: {}",
            other.class_name().unwrap_or("Error"),
            other.message().unwrap_or("")
        ),
    }
}

#[test]
fn oracle_cases() {
    let cases = load_cases();
    assert!(cases.len() > 1000, "fixture too small: {}", cases.len());
    let mut failures = Vec::new();
    let mut per_func: std::collections::BTreeMap<String, (usize, usize)> =
        std::collections::BTreeMap::default();
    let runs = cases.iter().flat_map(|c| {
        let natural = (c.func != "seed:array_rand").then_some((c, &c.natural_args, " [packed]"));
        std::iter::once((c, &c.args, "")).chain(natural)
    });
    for (case, args, tag) in runs {
        let mut ctx = Ctx::new();
        let out = dispatch(&mut ctx, &case.func, args);
        let (result, exc) = match &out.result {
            Ok(v) => (Some(ser(v)), None),
            Err(e) => (None, Some(exc_text(e))),
        };
        let byref = out.byref.as_ref().map(ser);
        let msgs = ctx.messages();
        let mut problems = Vec::new();
        if result != case.result {
            problems.push(format!(
                "result: got {result:?}\n      want {:?}",
                case.result
            ));
        }
        if exc != case.exc {
            problems.push(format!("exception: got {exc:?}, want {:?}", case.exc));
        }
        if case.byref.is_some() && byref != case.byref {
            problems.push(format!("byref: got {byref:?}\n      want {:?}", case.byref));
        }
        if msgs != case.msgs {
            problems.push(format!(
                "messages: got {msgs:?}\n      want {:?}",
                case.msgs
            ));
        }
        let entry = per_func.entry(case.func.clone()).or_default();
        entry.0 += 1;
        if !problems.is_empty() {
            entry.1 += 1;
            failures.push(format!(
                "{}{tag}({})\n    {}",
                case.func,
                case.args_text,
                problems.join("\n    ")
            ));
        }
    }
    if !failures.is_empty() {
        let summary: Vec<String> = per_func
            .iter()
            .filter(|(_, (_, f))| *f > 0)
            .map(|(k, (n, f))| format!("{k}: {f}/{n}"))
            .collect();
        let shown: Vec<&String> = failures.iter().take(40).collect();
        panic!(
            "{} oracle runs failed ({} cases)\nper function: {}\n\n{}",
            failures.len(),
            cases.len(),
            summary.join(", "),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }
}

/// Regenerates `tests/data/oracle.txt` with the installed `php`.
#[test]
#[ignore = "needs /usr/bin/php; rewrites the fixture"]
fn regenerate_oracle() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");
    let out = std::process::Command::new("php")
        .arg(format!("{dir}/gen_oracle.php"))
        .output()
        .expect("run php");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(format!("{dir}/oracle.txt"), out.stdout).expect("write fixture");
}
