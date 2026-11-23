// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The SOAP 1.2 test collection (`ext/soap/tests/soap12/*.phpt`): raw
//! requests handled by a `SoapServer` over `soap12-test.wsdl` with the
//! `Soap12test` class; the response must match byte for byte.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

mod common;

use std::rc::Rc;

use common::host::{MethodFn, UserClass};
use common::{output_matches, phpt_sections, soap_tests_dir, with_big_stack, TestHost};
use php_runtime::array::ArrayKey;
use php_runtime::{RtResult, Value, ZArray, ZStr};
use php_soap::host::SoapHost;

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or_default()
}

fn prop(h: &mut TestHost, v: &Value, name: &str) -> Value {
    match v {
        Value::Object(o) => h
            .read_property(o, name.as_bytes())
            .ok()
            .flatten()
            .unwrap_or_default(),
        Value::Array(a) => a.get_str(name.as_bytes()).cloned().unwrap_or_default(),
        _ => Value::Null,
    }
}

fn method(
    name: &str,
    f: impl Fn(&mut TestHost, &php_runtime::Obj, &[Value]) -> RtResult<Value> + 'static,
) -> (String, MethodFn) {
    (name.to_owned(), Rc::new(f))
}

fn soap12_class() -> UserClass {
    let echo = |_: &mut TestHost, _: &php_runtime::Obj, a: &[Value]| Ok(arg(a, 0));
    let mut methods: Vec<(String, MethodFn)> = [
        "echoOk",
        "echoString",
        "echoStringArray",
        "echoInteger",
        "echoIntegerArray",
        "echoFloat",
        "echoFloatArray",
        "echoStruct",
        "echoStructArray",
        "echoBase64",
        "echoDate",
        "echoHexBinary",
        "echoDecimal",
        "echoBoolean",
        "echoNestedStruct",
        "echo2DStringArray",
        "echoNestedArray",
    ]
    .iter()
    .map(|n| method(n, echo))
    .collect();
    methods.push(method("echoVoid", |_, _, _| Ok(Value::Null)));
    methods.push(method("returnVoid", |_, _, _| Ok(Value::Null)));
    methods.push(method("emptyBody", |_, _, _| Ok(Value::Null)));
    methods.push(method("echoStructAsSimpleTypes", |h, _, a| {
        let s = arg(a, 0);
        Ok(Value::Array(ZArray::from_pairs([
            ("outputString", prop(h, &s, "varString")),
            ("outputInteger", prop(h, &s, "varInt")),
            ("outputFloat", prop(h, &s, "varFloat")),
        ])))
    }));
    methods.push(method("echoSimpleTypesAsStruct", |h, _, a| {
        let o = h.new_object("stdClass");
        for (n, i) in [("varString", 0), ("varInt", 1), ("varFloat", 2)] {
            h.write_property(&o, n.as_bytes(), arg(a, i))?;
        }
        Ok(Value::Object(o))
    }));
    methods.push(method("countItems", |_, _, a| {
        Ok(Value::Int(match arg(a, 0) {
            Value::Array(x) => i64::try_from(x.len()).unwrap_or(0),
            Value::Null => 0,
            _ => 1,
        }))
    }));
    methods.push(method("isNil", |_, _, a| {
        Ok(Value::Bool(matches!(arg(a, 0), Value::Null)))
    }));
    methods.push(method("requiredHeader", |h, this, a| {
        h.write_property(this, b"header", arg(a, 0))?;
        Ok(Value::Null)
    }));
    methods.push(method("echoHeader", |h, this, _| {
        Ok(h.read_property(this, b"header")?.unwrap_or_default())
    }));
    methods.push(method("echoResolvedRef", |h, _, a| {
        let r = prop(h, &arg(a, 0), "RelativeReference");
        let base = prop(h, &r, "base").to_zstr(h)?;
        let href = prop(h, &r, "href").to_zstr(h)?;
        Ok(Value::Str(ZStr::concat(base.as_bytes(), href.as_bytes())))
    }));
    methods.push(method("validateCountryCode", |h, _, a| {
        let code = arg(a, 0).to_zstr(h)?;
        if code.len() == 2 {
            Ok(Value::from("OK"))
        } else {
            let hdr = h.new_object("SoapHeader");
            php_soap::objects::soap_header_construct(
                h,
                &hdr,
                b"http://example.org/ts-tests",
                b"validateCountryCodeFault",
                Some(&Value::from("Country code must be 2 letters.")),
                false,
                None,
            )?;
            let f = h.new_object("SoapFault");
            php_soap::fault::construct(
                h,
                &f,
                &Value::from("Client"),
                b"Not a valid country code",
                None,
                None,
                None,
                Some(&Value::Object(hdr)),
            )?;
            Ok(Value::Object(f))
        }
    }));
    UserClass {
        name: "Soap12test".into(),
        parent: None,
        props: vec![("header".into(), Value::Null)],
        methods,
    }
}

fn run_one(dir: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let sec = phpt_sections(&src);
    let file = sec.get("FILE").ok_or("no FILE")?;
    let (expected, fmt) = match (sec.get("EXPECT"), sec.get("EXPECTF")) {
        (Some(e), _) => (e.clone(), false),
        (_, Some(e)) => (e.clone(), true),
        _ => return Err("no EXPECT".into()),
    };
    let start = file.find("<<<EOF\n").ok_or("no heredoc")? + 7;
    let end = file[start..].find("\nEOF;").ok_or("no EOF")? + start;
    let request = file[start..end].to_owned();
    let mut host = TestHost::new();
    host.script = "/tests/soap12/soap12-test.inc".into();
    let wsdl_path = "/tests/soap12/soap12-test.wsdl";
    host.files.insert(
        wsdl_path.into(),
        std::fs::read(dir.join("soap12-test.wsdl")).map_err(|e| e.to_string())?,
    );
    host.add_class(soap12_class());
    let opts = ZArray::from_pairs([
        (ArrayKey::from_bytes(b"soap_version"), Value::Int(2)),
        (
            ArrayKey::from_bytes(b"actor"),
            Value::from("http://example.org/ts-tests/C"),
        ),
    ]);
    let run = |host: &mut TestHost| -> RtResult<()> {
        let server = host.new_server(Some(wsdl_path), &opts)?;
        php_soap::server::set_class(host, &server, ZStr::from("Soap12test"), Vec::new())?;
        php_soap::server::handle(host, &server, Some(request.as_bytes()))?;
        host.echo(b"ok\n");
        Ok(())
    };
    if let Err(e) = run(&mut host) {
        host.report(&e);
    }
    let out = host.finish();
    if output_matches(&expected, &out, fmt) {
        Ok(())
    } else {
        Err(format!(
            "--- expected\n{}\n--- actual\n{}",
            expected.trim(),
            String::from_utf8_lossy(&out).trim()
        ))
    }
}

#[test]
fn soap12_collection() {
    let Some(dir) = soap_tests_dir() else {
        eprintln!("reference tests not available; skipping");
        return;
    };
    let failures = with_big_stack(move || {
        let dir = dir.join("soap12");
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("soap12 dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "phpt"))
            .collect();
        entries.sort();
        let mut failures = Vec::new();
        let mut passed = 0;
        for p in &entries {
            match run_one(&dir, p) {
                Ok(()) => passed += 1,
                Err(e) => failures.push(format!(
                    "{}: {e}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
        eprintln!("soap12: {passed}/{} passed", entries.len());
        failures
    });
    for f in &failures {
        eprintln!("FAIL {f}\n");
    }
    assert!(
        failures.is_empty(),
        "{} soap12 tests failed",
        failures.len()
    );
}
