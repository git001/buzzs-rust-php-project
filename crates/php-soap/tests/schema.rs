// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The XML Schema encoding matrix of `ext/soap/tests/schema/*.phpt`: every
//! test is an XSD snippet plus a PHP value; the client request built from
//! it and the value decoded by the server are compared with the expected
//! output.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

mod common;

use common::host::UserClass;
use common::{output_matches, phpt_sections, soap_tests_dir, with_big_stack, Eval, TestHost};
use php_runtime::{Value, ZArray};

fn wsdl(schema: &str, type_: &str, style: &str, use_: &str, attr_form_default: &str) -> String {
    format!(
        r##"<definitions name="InteropTest"
    xmlns:xsd="http://www.w3.org/2001/XMLSchema"
    xmlns:SOAP-ENC="http://schemas.xmlsoap.org/soap/encoding/"
    xmlns:tns="http://test-uri/"
    xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
    xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/"
    xmlns="http://schemas.xmlsoap.org/wsdl/"
    targetNamespace="http://test-uri/"
    >
  <types>
  <schema xmlns="http://www.w3.org/2001/XMLSchema" targetNamespace="http://test-uri/" {attr_form_default}>
   <xsd:import namespace="http://schemas.xmlsoap.org/soap/encoding/" />
   <xsd:import namespace="http://schemas.xmlsoap.org/wsdl/" />
    {schema}
  </schema>
  </types>
  <message name="testMessage">
    <part name="testParam" {type_}/>
  </message>
    <portType name="testPortType">
        <operation name="test">
            <input message="testMessage"/>
        </operation>
    </portType>
    <binding name="testBinding" type="testPortType">
        <soap:binding style="rpc" transport="http://schemas.xmlsoap.org/soap/http"/>
        <operation name="test">
            <soap:operation soapAction="#test" style="{style}"/>
            <input>
                <soap:body use="{use_}" namespace="http://test-uri/" encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"/>
            </input>
        </operation>
    </binding>
    <service name="testService">
   <port name="testPort" binding="tns:testBinding">
     <soap:address location="test://" />
   </port>
 </service>
</definitions>"##
    )
}

/// The body of `test_schema()` from `test_schema.inc`.
#[allow(clippy::too_many_arguments)]
fn test_schema(
    host: &mut TestHost,
    schema: &str,
    type_: &str,
    param: Value,
    style: &str,
    use_: &str,
    afd: &str,
    features: i64,
) -> bool {
    let fname = "/tests/wsdl_schema_test";
    host.files.insert(
        fname.into(),
        wsdl(schema, type_, style, use_, afd).into_bytes(),
    );
    let opts = ZArray::from_pairs([
        ("trace", Value::Int(1)),
        ("exceptions", Value::Int(0)),
        ("features", Value::Int(features)),
    ]);
    let x = match host.new_client("SoapClient", Some(fname), &opts) {
        Ok(x) => x,
        Err(e) => {
            host.report(&e);
            return false;
        }
    };
    let y = match host.new_server(
        Some(fname),
        &ZArray::from_pairs([("features", Value::Int(features))]),
    ) {
        Ok(y) => y,
        Err(e) => {
            host.report(&e);
            return false;
        }
    };
    if let Err(e) = php_soap::server::add_function(host, &y, &Value::from("test")) {
        host.report(&e);
        return false;
    }
    if let Err(e) = php_soap::client::call(host, &x, b"test", &ZArray::from_values([param])) {
        host.report(&e);
        return false;
    }
    let req = php_soap::client::get_last(host, &x, "__last_request");
    let req_bytes = match &req {
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    };
    if style == "rpc" {
        use php_soap::host::SoapHost;
        host.ob_start();
        if let Err(e) = php_soap::server::handle(host, &y, Some(&req_bytes)) {
            host.report(&e);
            return false;
        }
        host.ob_get_clean();
        host.echo(&req_bytes);
        let val = host.globals.get("val").cloned().unwrap_or_default();
        host.var_dump(&val);
    } else {
        host.echo(&req_bytes);
    }
    true
}

fn parse_classes(file: &str, host: &mut TestHost) {
    let mut lines = file.lines();
    while let Some(l) = lines.next() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("class ") {
            let head: Vec<&str> = rest.trim_end_matches('{').split_whitespace().collect();
            let mut c = UserClass {
                name: head[0].to_owned(),
                parent: head
                    .iter()
                    .position(|w| *w == "extends")
                    .map(|i| head[i + 1].to_owned()),
                ..UserClass::default()
            };
            for l2 in lines.by_ref() {
                let t2 = l2.trim();
                if t2.starts_with('}') {
                    break;
                }
                if let Some(p) = t2.strip_prefix("public $") {
                    let p = p.trim_end_matches(';');
                    let (n, v) = p
                        .split_once('=')
                        .map_or((p, "null"), |(a, b)| (a.trim(), b.trim()));
                    let v = Eval::new(v, host).expr().unwrap_or_default();
                    c.props.push((n.to_owned(), v));
                }
            }
            host.add_class(c);
        }
    }
}

fn run_one(path: &std::path::Path) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let sec = phpt_sections(&src);
    let file = sec.get("FILE").ok_or("no FILE")?;
    let (expected, fmt) = match (sec.get("EXPECT"), sec.get("EXPECTF")) {
        (Some(e), _) => (e.clone(), false),
        (_, Some(e)) => (e.clone(), true),
        _ => return Err("no EXPECT".into()),
    };
    let schema = match file.find("$schema = <<<EOF\n") {
        Some(s) => {
            let start = s + "$schema = <<<EOF\n".len();
            let end = file[start..].find("\nEOF;").ok_or("no EOF")? + start;
            file[start..end].to_owned()
        }
        None => String::new(),
    };
    let call_at = file.find("test_schema($schema").ok_or("no call")?;
    let mut depth = 0;
    let mut end = call_at;
    for (i, c) in file[call_at..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = call_at + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let args_src = file[call_at + "test_schema(".len()..end].to_owned();
    let mut host = TestHost::new();
    host.script = "/tests/test_schema.inc".into();
    parse_classes(file, &mut host);
    host.add_function("test", |h, args| {
        h.globals
            .insert("val".into(), args.first().cloned().unwrap_or_default());
        Ok(Value::Null)
    });
    let mut vars = std::collections::HashMap::new();
    vars.insert("schema".to_owned(), Value::from(schema.as_str()));
    for line in file.lines() {
        if let Some(rest) = line.strip_prefix("$date = ") {
            let v = Eval::new(rest.trim_end_matches(';'), &mut host)
                .expr()
                .map_err(|e| format!("{e:?}"))?;
            vars.insert("date".into(), v);
        }
    }
    let args = {
        let mut ev = Eval::new(&args_src, &mut host);
        ev.vars = vars;
        ev.args().map_err(|e| format!("{e:?}"))?
    };
    let s = |i: usize, d: &str| match args.get(i) {
        Some(Value::Str(s)) => s.to_string_lossy(),
        _ => d.to_owned(),
    };
    let features = match args.get(6) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let ok = test_schema(
        &mut host,
        &s(0, ""),
        &s(1, ""),
        args.get(2).cloned().unwrap_or_default(),
        &s(3, "rpc"),
        &s(4, "encoded"),
        &s(5, ""),
        features,
    );
    if ok && file.contains("echo \"ok\"") {
        host.echo(b"ok");
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
fn schema_matrix() {
    let Some(dir) = soap_tests_dir() else {
        eprintln!("reference tests not available; skipping");
        return;
    };
    let failures = with_big_stack(move || {
        let mut entries: Vec<_> = std::fs::read_dir(dir.join("schema"))
            .expect("schema dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "phpt"))
            .collect();
        entries.sort();
        let mut failures = Vec::new();
        let mut passed = 0;
        for p in &entries {
            // schema086 formats DateTime objects (an engine feature).
            if p.ends_with("schema086.phpt") {
                continue;
            }
            match run_one(p) {
                Ok(()) => passed += 1,
                Err(e) => failures.push(format!(
                    "{}: {e}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
        eprintln!("schema: {passed}/{} passed", entries.len());
        failures
    });
    for f in &failures {
        eprintln!("FAIL {f}\n");
    }
    assert!(
        failures.is_empty(),
        "{} schema tests failed",
        failures.len()
    );
}
