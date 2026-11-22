// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Object-related behaviour (`JsonSerializable`, enums, recursion, resources,
//! decoding to objects), with expectations taken from `/usr/bin/php`.

#![allow(clippy::many_single_char_names)]

use crate::flags::{PARTIAL_OUTPUT_ON_ERROR, PRETTY_PRINT, THROW_ON_ERROR};
use crate::functions::{json_decode, json_encode, json_last_error, json_last_error_msg, JsonState};
use crate::test_host::{Kind, TestHost};
use crate::{decode, encode, EncodeError, ErrorCode};
use php_runtime::{dump, HashTable, Resource, RuntimeError, Value, ZArray, ZStr};

fn enc(host: &mut TestHost, v: &Value, flags: i64) -> (String, i64, String) {
    let mut state = JsonState::new();
    let r = json_encode(&mut state, host, v, flags, 512).expect("no exception");
    let out = match r {
        Value::Str(s) => s.to_string_lossy(),
        Value::Bool(false) => "false".to_string(),
        other => panic!("unexpected {other:?}"),
    };
    (out, json_last_error(&state), json_last_error_msg(&state))
}

fn list(values: Vec<Value>) -> Value {
    Value::Array(ZArray::from_values(values))
}

fn returning(
    v: Value,
) -> impl Fn(&mut TestHost, &php_runtime::Obj) -> php_runtime::RtResult<Value> {
    move |_, _| Ok(v.clone())
}

#[test]
fn json_serializable() {
    let mut h = TestHost::new();
    let s = h.serializable(
        "S",
        &[(b"p", Value::Int(1)), (b"\0S\0q", Value::Int(2))],
        returning(list(vec![Value::Int(1), Value::Int(2)])),
    );
    assert_eq!(enc(&mut h, &s, 0), ("[1,2]".into(), 0, "No error".into()));

    let inner = h.serializable("S", &[], returning(Value::from("inner")));
    let outer = h.serializable("S", &[], returning(inner));
    assert_eq!(enc(&mut h, &outer, 0).0, "\"inner\"");

    let nan = h.serializable("S", &[], returning(Value::Float(f64::NAN)));
    assert_eq!(
        enc(&mut h, &nan, 0),
        (
            "false".into(),
            7,
            "Inf and NaN cannot be JSON encoded".into()
        )
    );
    assert_eq!(enc(&mut h, &nan, PARTIAL_OUTPUT_ON_ERROR).0, "0");
}

#[test]
fn json_serialize_returning_this() {
    let mut h = TestHost::new();
    let o = h.serializable(
        "SelfSer",
        &[(b"a", Value::Int(1)), (b"\0*\0b", Value::Int(2))],
        |_, o| Ok(Value::Object(o.clone())),
    );
    assert_eq!(
        enc(&mut h, &o, 0),
        ("{\"a\":1}".into(), 0, "No error".into())
    );

    // `return [$this];` is a recursion.
    let arr = h.serializable("Arr", &[(b"a", Value::Int(1))], |_, o| {
        Ok(list(vec![Value::Object(o.clone())]))
    });
    assert_eq!(
        enc(&mut h, &arr, 0),
        ("false".into(), 6, "Recursion detected".into())
    );
    assert_eq!(enc(&mut h, &arr, PARTIAL_OUTPUT_ON_ERROR).0, "[null]");
}

#[test]
fn nested_json_encode_sees_outer_guard() {
    // json_encode_recursion_01.phpt: json_encode($this) inside
    // jsonSerialize() fails with a recursion error, then `return $this`.
    let mut h = TestHost::new();
    let o = h.serializable(
        "SerializingTest",
        &[
            (b"a", Value::Int(1)),
            (b"\0SerializingTest\0b", Value::from("hide")),
            (b"\0*\0c", Value::from("protect")),
        ],
        |host, o| {
            let mut state = JsonState::new();
            let r = json_encode(&mut state, host, &Value::Object(o.clone()), 0, 512)?;
            host.log.push(format!("{r:?}"));
            Ok(Value::Object(o.clone()))
        },
    );
    assert_eq!(enc(&mut h, &o, 0).0, "{\"a\":1}");
    assert_eq!(
        h.log,
        vec!["SerializingTest::jsonSerialize()", "Bool(false)"]
    );
}

#[test]
fn json_serialize_exception_propagates() {
    // bug73113.phpt / bug68992.phpt.
    let mut h = TestHost::new();
    let throwing = |_: &mut TestHost, _: &php_runtime::Obj| -> php_runtime::RtResult<Value> {
        Err(RuntimeError::Error("This error is expected".into()))
    };
    let a = h.serializable("T", &[], throwing);
    let b = h.serializable("T", &[], throwing);
    let v = list(vec![a, b]);
    let mut state = JsonState::new();
    let r = json_encode(&mut state, &mut h, &v, 0, 512);
    assert!(matches!(r, Err(RuntimeError::Error(m)) if m == "This error is expected"));
    // Only the first jsonSerialize() ran.
    assert_eq!(h.log.len(), 1);
    assert!(matches!(
        encode(&v, PARTIAL_OUTPUT_ON_ERROR, 512, &mut h),
        Err(EncodeError::Exception(_))
    ));
}

#[test]
fn enums() {
    let mut h = TestHost::new();
    let unit = h.object("U", Kind::UnitEnum(ZStr::from("A")), &[]);
    let backed = h.object(
        "B",
        Kind::BackedEnum(ZStr::from("X"), Value::from("x\"/")),
        &[],
    );
    let int_backed = h.object("I", Kind::BackedEnum(ZStr::from("One"), Value::Int(1)), &[]);
    let msg = "Non-backed enums have no default serialization";
    assert_eq!(enc(&mut h, &unit, 0), ("false".into(), 11, msg.into()));
    assert_eq!(
        enc(
            &mut h,
            &list(vec![unit.clone(), Value::Int(1)]),
            PARTIAL_OUTPUT_ON_ERROR
        ),
        ("[0,1]".into(), 11, msg.into())
    );
    assert_eq!(enc(&mut h, &list(vec![Value::Int(1), unit]), 0).0, "false");
    assert_eq!(enc(&mut h, &backed, 0).0, "\"x\\\"\\/\"");
    let mut t = HashTable::new();
    t.insert(0, int_backed);
    t.insert("k", backed);
    assert_eq!(
        enc(&mut h, &Value::Array(ZArray::from_table(t)), PRETTY_PRINT).0,
        "{\n    \"0\": 1,\n    \"k\": \"x\\\"\\/\"\n}"
    );
}

#[test]
fn recursion() {
    let mut h = TestHost::new();
    let o = h.object("stdClass", Kind::Plain, &[]);
    TestHost::set_prop(&o, b"self", o.clone());
    assert_eq!(
        enc(&mut h, &o, 0),
        ("false".into(), 6, "Recursion detected".into())
    );
    assert_eq!(
        enc(&mut h, &o, PARTIAL_OUTPUT_ON_ERROR).0,
        "{\"self\":null}"
    );
    // $a = ['x' => 1]; $a['me'] = &$a;
    let mut a = Value::Array(ZArray::from_pairs([("x", Value::Int(1))]));
    let r = a.make_ref();
    {
        let mut inner = r.borrow_mut();
        let arr = inner.as_array_mut().expect("array");
        arr.insert("me", Value::Reference(r.clone()));
    }
    assert_eq!(enc(&mut h, &a, 0).0, "false");
    assert_eq!(
        enc(&mut h, &a, PARTIAL_OUTPUT_ON_ERROR),
        (
            "{\"x\":1,\"me\":null}".into(),
            6,
            "Recursion detected".into()
        )
    );
    // Break the cycle so the test does not leak.
    r.set(Value::Null);
    TestHost::set_prop(&o, b"self", Value::Null);
    // The same object twice (not nested) is fine.
    let shared = h.object("stdClass", Kind::Plain, &[(b"v", Value::Int(1))]);
    assert_eq!(
        enc(&mut h, &list(vec![shared.clone(), shared]), 0).0,
        "[{\"v\":1},{\"v\":1}]"
    );
}

#[test]
fn rebuilt_property_tables_keep_their_recursion_identity() {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let mut h = TestHost::new();
            let o = h.object("Rebuilt", Kind::RebuiltProperties, &[]);
            TestHost::set_prop(&o, b"self", o.clone());
            assert_eq!(
                enc(&mut h, &o, 0),
                ("false".into(), 1, "Maximum stack depth exceeded".into())
            );

            // The rebuilt table is fresh, but its array property is shared.
            TestHost::set_prop(&o, b"self", list(vec![o.clone()]));
            assert_eq!(
                enc(&mut h, &o, 0),
                ("false".into(), 6, "Recursion detected".into())
            );
            assert_eq!(
                enc(&mut h, &o, PARTIAL_OUTPUT_ON_ERROR),
                (
                    "{\"self\":[{\"self\":null}]}".into(),
                    6,
                    "Recursion detected".into()
                )
            );
            TestHost::set_prop(&o, b"self", Value::Null);
            assert_eq!(
                enc(&mut h, &o, 0),
                ("{\"self\":null}".into(), 0, "No error".into())
            );
        })
        .expect("spawn")
        .join()
        .expect("rebuilt property stack thread");
}

#[test]
fn resources_are_unsupported() {
    let mut h = TestHost::new();
    let fp = Value::Resource(Resource::with_handle(5, "stream", None));
    let msg = "Type is not supported";
    assert_eq!(enc(&mut h, &fp, 0), ("false".into(), 8, msg.into()));
    assert_eq!(
        enc(
            &mut h,
            &list(vec![fp.clone(), Value::Int(1)]),
            PARTIAL_OUTPUT_ON_ERROR
        )
        .0,
        "[null,1]"
    );
    // bug73254.phpt: the error does not leak into the next call.
    let ok = json_encode(&mut JsonState::new(), &mut h, &Value::Int(1), 0, 512);
    assert!(matches!(ok, Ok(Value::Str(_))));
}

#[test]
fn pretty_print_nested_objects() {
    let mut h = TestHost::new();
    let leaf = h.serializable("S", &[], returning(Value::Null));
    let mut t = HashTable::new();
    t.insert("k", leaf);
    let s = h.serializable("S", &[], returning(Value::Array(ZArray::from_table(t))));
    assert_eq!(
        enc(&mut h, &list(vec![s]), PRETTY_PRINT).0,
        "[\n    {\n        \"k\": null\n    }\n]"
    );
    // bug66021.phpt: objects without visible properties print as {}.
    let foo = h.object("Foo", Kind::Plain, &[(b"\0Foo\0bar", Value::from("baz"))]);
    let std = h.object("stdClass", Kind::Plain, &[]);
    assert_eq!(
        enc(
            &mut h,
            &list(vec![Value::empty_array(), std, foo]),
            PRETTY_PRINT
        )
        .0,
        "[\n    [],\n    {},\n    {}\n]"
    );
}

#[test]
fn throw_on_error() {
    let mut h = TestHost::new();
    let mut state = JsonState::new();
    // A previous error stays when JSON_THROW_ON_ERROR is used.
    let _ = json_decode(&mut state, &mut h, b"[", None, 512, 0);
    assert_eq!(json_last_error(&state), 4);
    let r = json_encode(
        &mut state,
        &mut h,
        &Value::Float(f64::INFINITY),
        THROW_ON_ERROR,
        512,
    );
    assert!(
        matches!(&r, Err(RuntimeError::Error(m)) if m == "JsonException(7): Inf and NaN cannot be JSON encoded"),
        "{r:?}"
    );
    assert_eq!(json_last_error(&state), 4);
    let r = json_decode(&mut state, &mut h, b"{", None, 512, THROW_ON_ERROR);
    assert!(
        matches!(&r, Err(RuntimeError::Error(m)) if m == "JsonException(4): Syntax error near location 1:2"),
        "{r:?}"
    );
    assert_eq!(json_last_error(&state), 4);
    // With JSON_PARTIAL_OUTPUT_ON_ERROR the error is recorded instead.
    let r = json_encode(
        &mut state,
        &mut h,
        &Value::Float(f64::NAN),
        THROW_ON_ERROR | PARTIAL_OUTPUT_ON_ERROR,
        512,
    );
    assert!(matches!(r, Ok(Value::Str(s)) if s == "0"));
    assert_eq!(json_last_error(&state), 7);
    // A successful decode clears the error.
    let _ = json_decode(&mut state, &mut h, b"1", None, 512, 0);
    assert_eq!(json_last_error_msg(&state), "No error");
}

#[test]
fn argument_errors() {
    let mut h = TestHost::new();
    let mut state = JsonState::new();
    let err = |r: php_runtime::RtResult<Value>| match r {
        Err(RuntimeError::ValueError(m)) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        err(json_decode(&mut state, &mut h, b"[]", None, 0, 0)),
        "json_decode(): Argument #3 ($depth) must be greater than 0"
    );
    assert_eq!(
        err(json_decode(&mut state, &mut h, b"[]", None, 1 << 31, 0)),
        "json_decode(): Argument #3 ($depth) must be less than 2147483647"
    );
    assert_eq!(
        err(json_encode(&mut state, &mut h, &Value::Null, 0, 1 << 31)),
        "json_encode(): Argument #3 ($depth) must be between -2147483648 and 2147483647"
    );
    // The empty string is checked before the depth.
    assert!(matches!(
        json_decode(&mut state, &mut h, b"", None, 0, 0),
        Ok(Value::Null)
    ));
    assert_eq!(json_last_error_msg(&state), "Syntax error");
}

fn dump(h: &mut TestHost, v: &Value) -> String {
    let mut out = Vec::new();
    dump::var_dump(&mut out, v, h).expect("var_dump");
    String::from_utf8(out).expect("utf8")
}

fn handles(s: &str) -> Vec<String> {
    s.lines()
        .filter_map(|l| {
            l.find('#')
                .map(|i| l[i..].split(' ').next().unwrap_or("").to_string())
        })
        .collect()
}

/// `ZEND_HAS_OOM()`: with a delayed memory-limit fatal pending the decoder
/// stops before building anything more (the host raises the fatal).
#[test]
fn pending_oom_stops_decoding() {
    // A real pending violation, on a thread with its own allocator so that
    // the limit is this test's alone.
    std::thread::spawn(|| {
        php_memlimit::attach_thread();
        php_memlimit::request_start(Some(16 << 20)).expect("limit");
        let mut h = TestHost::new();
        assert!(php_memlimit::try_reserve(usize::MAX).is_err());
        let r = decode(br#"[{}, {"x": [1, 2]}]"#, None, 512, 0, &mut h);
        assert!(
            matches!(&r, Err(crate::DecodeError::Json(e)) if e.code == ErrorCode::Syntax),
            "{r:?}"
        );
        // No object was created before the stop.
        php_memlimit::request_end();
        let v = decode(b"{}", None, 512, 0, &mut h).expect("decode");
        assert_eq!(handles(&dump(&mut h, &v)), ["#1"]);
    })
    .join()
    .expect("test thread");
}

#[test]
fn decoded_object_creation_order() {
    // Handle numbers from `php -r 'var_dump(json_decode(...));'`.
    let mut h = TestHost::new();
    let v = decode(
        br#"{"a":{"b":1},"c":{},"d":[{}],"e":{"f":{}}}"#,
        None,
        512,
        0,
        &mut h,
    )
    .expect("decode");
    assert_eq!(
        handles(&dump(&mut h, &v)),
        ["#2", "#1", "#3", "#4", "#6", "#5"]
    );
    let mut h = TestHost::new();
    let v = decode(br#"[{}, {"x": {}}, {"y":1,"z":{}}]"#, None, 512, 0, &mut h).expect("decode");
    assert_eq!(handles(&dump(&mut h, &v)), ["#1", "#3", "#2", "#4", "#5"]);
}

#[test]
fn decoded_objects() {
    let mut h = TestHost::new();
    let v = decode(
        br#"{"0":1,"":2,"a b":[true],"a b":null}"#,
        None,
        512,
        0,
        &mut h,
    )
    .expect("decode");
    assert_eq!(
        dump(&mut h, &v),
        "object(stdClass)#1 (3) {\n  [\"0\"]=>\n  int(1)\n  [\"\"]=>\n  int(2)\n  [\"a b\"]=>\n  NULL\n}\n"
    );
    let v = decode(br#"{"0":1,"1":{"x":[]}}"#, Some(true), 512, 0, &mut h).expect("decode");
    assert_eq!(
        dump(&mut h, &v),
        "array(2) {\n  [0]=>\n  int(1)\n  [1]=>\n  array(1) {\n    [\"x\"]=>\n    array(0) {\n    }\n  }\n}\n"
    );
    // bug68546.phpt
    let e = decode(
        br#"[{"key1": 0, "\u1234": 1, "\u0000": 1}]"#,
        None,
        512,
        0,
        &mut h,
    )
    .unwrap_err()
    .json_error()
    .expect("json error");
    assert_eq!(e.code, ErrorCode::InvalidPropertyName);
    assert_eq!(
        e.msg(),
        "The decoded property name is invalid near location 1:37"
    );
    // Keys starting with NUL are fine in arrays.
    let v = decode(br#"{"\u0000a":1}"#, Some(true), 512, 0, &mut h).expect("decode");
    assert_eq!(dump(&mut h, &v), "array(1) {\n  [\"\0a\"]=>\n  int(1)\n}\n");
}

#[test]
fn collecting_context_is_a_host() {
    let mut ctx = php_runtime::CollectingContext::new();
    let v = list(vec![Value::from("a/b"), Value::Float(0.1)]);
    assert_eq!(
        encode(&v, 0, 512, &mut ctx).expect("encode"),
        b"[\"a\\/b\",0.1]"
    );
    assert!(matches!(
        decode(b"{}", Some(false), 512, 0, &mut ctx),
        Ok(Value::Object(_))
    ));
    assert!(matches!(
        decode(b"{}", Some(true), 512, 0, &mut ctx),
        Ok(Value::Array(a)) if a.is_empty()
    ));
}

#[test]
fn exceptions_leave_last_error() {
    // json_exceptions_error_clearing.phpt
    let mut h = TestHost::new();
    let mut state = JsonState::new();
    assert_eq!(json_last_error(&state), 0);
    let _ = json_decode(&mut state, &mut h, b"\xFF", Some(false), 512, 0);
    assert_eq!(json_last_error(&state), 5);
    let r = json_decode(&mut state, &mut h, b"", Some(false), 512, THROW_ON_ERROR);
    assert!(matches!(r, Err(RuntimeError::Error(m)) if m == "JsonException(4): Syntax error"));
    assert_eq!(json_last_error(&state), 5);
    let r = json_decode(&mut state, &mut h, b"{", Some(false), 512, THROW_ON_ERROR);
    assert!(r.is_err());
    assert_eq!(json_last_error(&state), 5);
    let r = json_encode(
        &mut state,
        &mut h,
        &Value::Float(f64::NAN),
        THROW_ON_ERROR,
        512,
    );
    assert!(matches!(r, Err(RuntimeError::Error(m)) if m.starts_with("JsonException(7)")));
    assert_eq!(json_last_error(&state), 5);
}
