// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of the `php:function` / `registerPHPFunctions()` /
//! `registerPHPFunctionNS()` phpts.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::doc_markdown, clippy::format_collect, clippy::similar_names)]

mod common;

use std::rc::Rc;

use common::*;
use php_libxml::tree::Document;
use php_xsl::{XmlOutput, XslError};

fn load_fixture(name: &str) -> Document {
    load(&fixture(name))
}

/// `prepare.inc` + importing `phpfunc*.xsl`.
fn phpfunc(sheet: &str) -> (Document, Proc, Php) {
    setup(&fixtures());
    let dom = load_fixture("xslt.xml");
    let xsl = load_fixture(sheet);
    let proc = Proc::new();
    import(&proc, &xsl);
    (dom, proc, Php::new())
}

fn var_dump_null() {
    var_dump(&Value::Null);
}

fn dump_xml(proc: &Proc, php: &mut Php, dom: &Document) {
    let r = to_xml_raw(proc, php, dom);
    match r {
        Err(e) => throw(e.into()),
        // An exception thrown during the transformation propagates.
        _ if exception_pending() => {}
        ok => dump_output(&ok),
    }
}

fn try_dump_xml(proc: &Proc, php: &mut Php, dom: &Document) {
    dump_xml(proc, php, dom);
    catch_message();
}

#[test]
fn register_all() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    proc.register_php_functions_all();
    var_dump_null();
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-allfuncs.phpt",
        &take_output(),
    );
}

fn reg_array(proc: &Proc, php: &Php, names: &[&str]) {
    let entries = names.iter().map(|n| (None, (*n).to_string()));
    proc.register_php_functions_array(
        entries,
        |v: &String| php.resolve(v.as_bytes()),
        |v: &String| Ok::<_, Exc>(v.clone().into_bytes()),
    )
    .unwrap();
    var_dump_null();
}

#[test]
fn register_array_multiple() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_array(&proc, &php, &["strpos", "ucwords"]);
    reg_array(&proc, &php, &["strrev", "array_key_exists"]);
    reg_array(&proc, &php, &[]);
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-array-multiple.phpt",
        &take_output(),
    );
}

#[test]
fn register_array_not_allowed() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_array(&proc, &php, &[]);
    try_dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-array-notallowed.phpt",
        &take_output(),
    );
}

#[test]
fn register_array_single() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_array(&proc, &php, &["ucwords"]);
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-array.phpt",
        &take_output(),
    );
}

#[test]
fn func_no_string() {
    let (dom, proc, mut php) = phpfunc("phpfunc-nostring.xsl");
    proc.register_php_functions_all();
    var_dump_null();
    try_dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-funcnostring.phpt",
        &take_output(),
    );
}

#[test]
fn func_undef() {
    let (dom, proc, mut php) = phpfunc("phpfunc-undef.xsl");
    proc.register_php_functions_all();
    var_dump_null();
    try_dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-funcundef.phpt",
        &take_output(),
    );
}

fn reg_name(proc: &Proc, php: &Php, name: &str) {
    proc.register_php_functions_name(name.as_bytes(), |n| php.resolve(n))
        .unwrap();
    var_dump_null();
}

#[test]
fn register_null() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_name(&proc, &php, "ucwords");
    proc.register_php_functions_all();
    var_dump_null();
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-null.phpt",
        &take_output(),
    );
}

#[test]
fn register_string_multiple() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_name(&proc, &php, "ucwords");
    reg_name(&proc, &php, "strpos");
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-string-multiple.phpt",
        &take_output(),
    );
}

#[test]
fn register_string_not_allowed() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_name(&proc, &php, "strpos");
    try_dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-string-notallowed.phpt",
        &take_output(),
    );
}

#[test]
fn register_string() {
    let (dom, proc, mut php) = phpfunc("phpfunc.xsl");
    reg_name(&proc, &php, "ucwords");
    dump_xml(&proc, &mut php, &dom);
    expect(
        "xsltprocessor_registerPHPFunctions-string.phpt",
        &take_output(),
    );
}

#[test]
fn xslt011() {
    setup(&fixtures());
    echo("Test 11: php:function Support\n");
    let dom = load_fixture("xslt011.xsl");
    let proc = Proc::new();
    import(&proc, &dom);
    let xml = load_fixture("xslt011.xml");
    proc.register_php_functions_all();
    let mut php = Php::new();
    php.define("foobar", |_, a| {
        let second = arg(&a, 1);
        let first = match &a[0] {
            Value::Array(_) => a[0].nodes()[0].text(),
            other => other.as_bytes(),
        };
        let mut out = first;
        out.extend_from_slice(b" - ");
        out.extend_from_slice(&second);
        Ok(Value::Str(out))
    });
    php.define("nodeSet", |_, a| {
        if let Some(Value::Array(items)) = a.first() {
            if !items.is_empty() {
                return Ok(items[0].clone());
            }
        }
        let dom = load_xml("<root>this is from an external DomDocument</root>");
        Ok(Value::Node(TNode::Handle(dom.root().unwrap())))
    });
    php.define("aClass::aStaticFunction", |_, a| Ok(a[0].clone()));
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    expect("xslt011.phpt", &take_output());
}

/// `xpath_callables.inc`'s `createProcessor()`.
fn create_processor(inputs: &[&str]) -> Proc {
    let body: String = inputs
        .iter()
        .map(|i| format!("<xsl:value-of select=\"php:function({i})\" />"))
        .collect();
    let xsl = load_xml(&format!(
        r#"<?xml version="1.0" encoding="iso-8859-1" ?>
    <xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:php="http://php.net/xsl">
    <xsl:template match="//a">{body}</xsl:template>
    </xsl:stylesheet>"#
    ));
    let proc = Proc::new();
    import(&proc, &xsl);
    proc
}

fn callables_php() -> Php {
    let mut php = Php::new();
    php.define("MyClass::dump", |_, a| {
        var_dump(&a[0]);
        let mut s = b"dump: ".to_vec();
        s.extend_from_slice(&arg(&a, 0));
        Ok(Value::Str(s))
    });
    php
}

#[test]
fn callables() {
    setup(&fixtures());
    let input = load_xml(
        r#"<?xml version="1.0" encoding="iso-8859-1"?><a href="https://php.net">hello</a>"#,
    );
    let mut php = callables_php();

    echo("--- Legit cases: none ---\n");
    let proc = create_processor(&["'var_dump', string(@href)"]);
    let _ = to_xml_raw(&proc, &mut php, &input);
    catch_message();

    echo("--- Legit cases: all ---\n");
    let proc = create_processor(&[
        "'var_dump', string(@href)",
        "'MyClass::dump', string(@href)",
    ]);
    proc.register_php_functions_all();
    dump_xml(&proc, &mut php, &input);

    echo("--- Legit cases: set ---\n");
    let proc = create_processor(&[
        "'mydump', string(@href)",
        "'xyz', string(@href)",
        "'var_dump', string(@href)",
    ]);
    proc.register_php_functions_array(
        Vec::<(Option<Vec<u8>>, Callable)>::new(),
        |c| Ok(c.clone()),
        |_| Ok::<_, Exc>(Vec::new()),
    )
    .unwrap();
    let dump = php.resolve(b"MyClass::dump").unwrap();
    let closure: Callable = Rc::new(|_: &mut Php, a: Vec<Value>| {
        var_dump(&a[0]);
        Ok(Value::Null)
    });
    proc.register_php_functions_array(
        vec![
            (Some(b"xyz".to_vec()), dump),
            (Some(b"mydump".to_vec()), closure),
        ],
        |c| Ok(c.clone()),
        |_| Ok::<_, Exc>(Vec::new()),
    )
    .unwrap();
    proc.register_php_functions_name(b"var_dump", |n| php.resolve(n))
        .unwrap();
    dump_xml(&proc, &mut php, &input);

    let proc = create_processor(&["'notinset', string(@href)"]);
    proc.register_php_functions_array(
        Vec::<(Option<Vec<u8>>, Callable)>::new(),
        |c| Ok(c.clone()),
        |_| Ok::<_, Exc>(Vec::new()),
    )
    .unwrap();
    try_dump_xml(&proc, &mut php, &input);

    echo("--- Legit cases: set with cycle ---\n");
    let proc = create_processor(&["'cycle', string(@href)"]);
    let dummy: Callable = Rc::new(|_: &mut Php, a: Vec<Value>| {
        let mut s = b"dummy: ".to_vec();
        s.extend_from_slice(&arg(&a, 0));
        Ok(Value::Str(s))
    });
    proc.register_php_functions_array(
        vec![(Some(b"cycle".to_vec()), dummy)],
        |c| Ok(c.clone()),
        |_| Ok::<_, Exc>(Vec::new()),
    )
    .unwrap();
    dump_xml(&proc, &mut php, &input);
    expect("XSLTProcessor_callables.phpt", &take_output());
}

/// A `registerPHPFunctions()` array value in the errors test.
#[derive(Clone)]
enum Entry {
    Name(&'static str),
    Closure,
}

#[test]
fn callables_errors() {
    setup(&fixtures());
    let php = callables_php();
    let proc = create_processor(&[]);
    let report = |r: Result<(), Exc>| {
        if let Err(e) = r {
            echo(e.message);
            echo("\n");
        }
    };
    report(
        proc.register_php_functions_name(b"nonexistent", |n| php.resolve(n))
            .map_err(Exc::from),
    );
    // ZPP rejects a Closure for array|string|null before the method runs.
    echo("XSLTProcessor::registerPHPFunctions(): Argument #1 ($functions) must be of type array|string|null, Closure given\n");
    let array = |entries: Vec<(Option<Vec<u8>>, Entry)>| {
        proc.register_php_functions_array(
            entries,
            |e| match e {
                Entry::Name(n) => php.resolve(n.as_bytes()),
                Entry::Closure => {
                    Ok(Rc::new(|_: &mut Php, _: Vec<Value>| Ok(Value::Null)) as Callable)
                }
            },
            |e| match e {
                Entry::Name(n) => Ok(n.as_bytes().to_vec()),
                Entry::Closure => Err(Exc::new(
                    "Error",
                    "Object of class Closure could not be converted to string",
                )),
            },
        )
    };
    report(array(vec![(None, Entry::Closure)]));
    report(array(vec![(None, Entry::Closure)]));
    report(array(vec![(None, Entry::Name("nonexistent"))]));
    report(array(vec![(Some(Vec::new()), Entry::Closure)]));
    report(array(vec![(Some(b"\0".to_vec()), Entry::Closure)]));
    report(
        proc.register_php_functions_name(b"", |n| php.resolve(n))
            .map_err(Exc::from),
    );
    expect("XSLTProcessor_callables_errors.phpt", &take_output());
}

#[test]
fn edge_cases() {
    setup(&fixtures());
    let test = |input: &str| {
        let xsl = load_xml(&format!(
            r#"<?xml version="1.0" encoding="iso-8859-1" ?>
    <xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:php="http://php.net/xsl">
    <xsl:template match="/">
    <xsl:value-of select="php:function({input})" />
    </xsl:template>
    </xsl:stylesheet>"#
        ));
        let inputdom =
            load_xml("<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n    <today></today>");
        let proc = Proc::new();
        proc.register_php_functions_all();
        import(&proc, &xsl);
        let mut php = Php::new();
        let _ = to_doc(&proc, &mut php, &inputdom);
        catch_message();
    };
    test("");
    test("3");
    expect("php_function_edge_cases.phpt", &take_output());
}

#[test]
fn non_dom_node() {
    setup(&fixtures());
    let dom = load_fixture("xslt_non_dom_node.xsl");
    let proc = Proc::new();
    import(&proc, &dom);
    let xml = load_fixture("xslt011.xml");
    proc.register_php_functions_all();
    let mut php = Php::new();
    php.define("nonDomNode", |_, _| {
        Ok(Value::Object("foo", Some(b"not a DomNode object".to_vec())))
    });
    let _ = to_xml_raw(&proc, &mut php, &xml);
    if let Some(e) = take_exception() {
        assert_eq!(e.class, "TypeError");
        echo(e.message);
    }
    expect("xslt_non_dom_node.phpt", &take_output());
}

const TEST_DATELANG: &str = r#"<?xml version="1.0" encoding="iso-8859-1" ?>
<xsl:stylesheet version="1.0"
xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
xmlns:php="http://php.net/xsl">
<xsl:template match="/">
<xsl:value-of select="php:function('TeSt::dateLang')" />
</xsl:template>
</xsl:stylesheet>"#;

#[test]
fn bug33853_autoload_exit() {
    setup(&fixtures());
    let mut php = Php::new();
    php.define("__autoload", |_, a| {
        var_dump(&a[0]);
        Err(Exc::new("exit", ""))
    });
    let xsl = load_xml(TEST_DATELANG);
    let input = load_xml("<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n<today></today>");
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let _ = to_doc(&proc, &mut php, &input);
    assert_eq!(take_exception().unwrap().class, "exit");
    expect("bug33853.phpt", &take_output());
}

#[test]
fn throw_in_autoload() {
    setup(&fixtures());
    let mut php = Php::new();
    php.define("__autoload", |_, a| {
        var_dump(&a[0]);
        Err(Exc::new("Exception", "Autoload exception"))
    });
    let xsl = load_xml(TEST_DATELANG);
    let input = load_xml("<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n<today></today>");
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let _ = to_doc(&proc, &mut php, &input);
    catch_message();
    echo("===DONE===\n");
    expect("throw_in_autoload.phpt", &take_output());
}

#[test]
fn bug49634_exception_from_callback_with_document_nodes() {
    setup(&fixtures());
    let xml = load_xml("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<root>\n    test\n</root>");
    let dir = fixtures();
    let xsl = load_xml(&format!(
        r#"<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:ext="http://php.net/xsl"
                xsl:extension-element-prefixes="ext"
                exclude-result-prefixes="ext">
    <xsl:output encoding="UTF-8" indent="yes" method="xml" />
    <xsl:template match="/">
        <xsl:value-of select="ext:function('testFunction', document('{}/bug49634.xml')/root)"/>
    </xsl:template>
</xsl:stylesheet>"#,
        dir.display()
    ));
    for _ in 0..10 {
        let mut php = Php::new();
        php.define("testFunction", |_, a| {
            // The node of the document() document arrives as a copy.
            assert_eq!(a[0].nodes().len(), 1);
            assert_eq!(a[0].nodes()[0].name(), b"root");
            Err(Exc::new("Exception", "Test exception."))
        });
        let proc = Proc::new();
        proc.register_php_functions_array(
            vec![(None, "testFunction")],
            |n| php.resolve(n.as_bytes()),
            |n| Ok::<_, Exc>(n.as_bytes().to_vec()),
        )
        .unwrap();
        import(&proc, &xsl);
        silenced(|| to_doc(&proc, &mut php, &xml));
        let e = take_exception().expect("exception");
        assert_eq!(
            (e.class.as_str(), e.message.as_str()),
            ("Exception", "Test exception.")
        );
    }
    assert!(take_output().is_empty());
}

#[test]
fn bug69168_modifying_input_nodes() {
    setup(&fixtures());
    let xml = "<allusers><user><uid>bob</uid></user><user><uid>joe</uid></user></allusers>";
    let xsl = r#"<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
 <xsl:template match="allusers">
    <xsl:for-each select="user">
      <xsl:value-of select="php:function('getPath',uid)"/><br />
    </xsl:for-each>
 </xsl:template>
</xsl:stylesheet>"#;
    let mut php = Php::new();
    php.define("getPath", |_, a| {
        let node = &a[0].nodes()[0];
        let mut v = node.text();
        v.push(b'a');
        node.set_text(&v);
        let mut out = node.path();
        out.extend_from_slice(b" = ");
        out.extend_from_slice(&node.text());
        Ok(Value::Str(out))
    });
    let proc = Proc::new();
    proc.register_php_functions_all();
    let xsl_doc = load_xml(xsl);
    silenced(|| import(&proc, &xsl_doc));
    let xml_doc = load_xml(xml);
    echo(silenced(|| to_xml(&proc, &mut php, &xml_doc)).unwrap());
    let uid = xml_doc
        .root()
        .unwrap()
        .first_child()
        .unwrap()
        .first_child()
        .unwrap();
    var_dump(&Value::Str(uid.path().unwrap()));
    var_dump(&Value::Str(uid.content()));
    expect("bug69168.phpt", &take_output());
}

#[test]
fn bug70078_many_callbacks() {
    setup(&fixtures());
    let dom = Document::new(b"1.0").unwrap();
    let root = dom.create_element(b"root").unwrap();
    dom.set_root(&root).unwrap();
    for _ in 0..=100 {
        let l1 = dom.create_element(b"level1").unwrap();
        root.append_child(&l1).unwrap();
        for _ in 0..=100 {
            let l2 = dom.create_element(b"level2").unwrap();
            l1.append_child(&l2).unwrap();
            for _ in 0..=10 {
                let l3 = dom.create_element(b"level3").unwrap();
                let t = dom.create_text(b"test").unwrap();
                l3.append_child(&t).unwrap();
                l2.append_child(&l3).unwrap();
            }
        }
    }
    let xsl = load_xml(
        r#"<?xml version="1.0" encoding="utf-8"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
    <xsl:template match="root">
        <output>
            <xsl:for-each select="level1">
                <node>
                    <xsl:value-of select="php:function('testPhpFunction', .)" />
                </node>
            </xsl:for-each>
        </output>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    php.define("testPhpFunction", |_, _| Ok(Value::s("test2")));
    let proc = Proc::new();
    proc.register_php_functions_name(b"testPhpFunction", |n| php.resolve(n))
        .unwrap();
    import(&proc, &xsl);
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    expect("bug70078.phpt", &take_output());
}

#[test]
fn gh23897_failed_registration() {
    setup(&fixtures());
    let dom = load_xml("<root><level1/></root>");
    let xsl = load_xml(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:template match="root">
<xsl:value-of select="php:function('testPhpFunction', .)" />
</xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    let proc = Proc::new();
    if let Err(e) = proc.register_php_functions_name(b"testPhpFunction", |n| php.resolve(n)) {
        echo(format!("{}: {}\n", e.class().name(), e.message()));
    }
    import(&proc, &xsl);
    let _ = to_xml_raw(&proc, &mut php, &dom);
    if let Some(e) = take_exception() {
        echo(format!("{}: {}\n", e.class, e.message));
    }
    expect("gh23897.phpt", &take_output());
}

const USERS_XSL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0"
     xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
     xmlns:php="http://php.net/xsl">
<xsl:output method="html" encoding="utf-8" indent="yes"/>
 <xsl:template match="allusers">
  <html><body>
    <h2>Users</h2>
    <table>
    <xsl:for-each select="user">
      <tr><td>
        <xsl:value-of
             select="php:function('ucfirst',string(uid))"/>
      </td></tr>
    </xsl:for-each>
    </table>
  </body></html>
 </xsl:template>
</xsl:stylesheet>"#;

#[test]
fn transform_to_doc_html() {
    setup(&fixtures());
    let xml = load_xml("<allusers>\n <user>\n  <uid>royopa</uid>\n </user>\n</allusers>");
    let xsl = load_xml(USERS_XSL);
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let mut php = Php::new();
    let doc = to_doc(&proc, &mut php, &xml).unwrap();
    var_dump(&Value::Str(doc.as_node().first_child().unwrap().name()));
    expect("xsltprocessor_transformToDoc.phpt", &take_output());
}

#[test]
fn transform_to_xml_html() {
    setup(&fixtures());
    let xml = load_xml("<allusers>\n <user>\n  <uid>bob</uid>\n </user>\n <user>\n  <uid>joe</uid>\n </user>\n</allusers>");
    let xsl = load_xml(USERS_XSL);
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let mut php = Php::new();
    dump_xml(&proc, &mut php, &xml);
    expect("xsltprocessor_transformToXML.phpt", &take_output());
}

#[test]
fn transform_to_uri_php_output() {
    setup(&fixtures());
    let xsl = load_xml(&USERS_XSL.replace("    <h2>Users</h2>\n", ""));
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let mut php = Php::new();
    let n = in_method("XSLTProcessor::transformToUri", || {
        proc.transform_to_uri(&xsl, b"php://output", &mut php)
            .unwrap()
    });
    var_dump(&Value::Int(n));
    expect("xsltprocessor_transformToURI.phpt", &take_output());
}

#[test]
fn gh22621_namespace_nodes_of_other_documents() {
    setup(&fixtures());
    let ext = scratch().join("gh22621_external.xml");
    std::fs::write(
        &ext,
        r#"<root xmlns:custom="urn:custom"><data>secret</data></root>"#,
    )
    .unwrap();
    let uri = format!("file://{}", ext.display());
    let xsl = load_xml(&format!(
        r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:php="http://php.net/xsl">
  <xsl:template match="/">
    <result>
      <xsl:value-of select="php:function('capture_ns', document('{uri}')/*/namespace::*)"/>
    </result>
  </xsl:template>
</xsl:stylesheet>"#
    ));
    let doc = load_xml("<input/>");
    let stored: Rc<std::cell::RefCell<Option<TNode>>> = Rc::default();
    let sink = stored.clone();
    let mut php = Php::new();
    php.define("capture_ns", move |_, a| {
        for n in a[0].nodes() {
            if n.is_namespace() {
                *sink.borrow_mut() = Some(n);
            }
        }
        Ok(Value::s("captured"))
    });
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    echo(to_xml(&proc, &mut php, &doc).unwrap());
    let ns = stored.borrow().clone().unwrap();
    var_dump(&Value::Str(ns.name()));
    var_dump(&Value::Str(ns.text()));
    var_dump(&Value::Str(ns.parent_name()));
    // ownerDocument of the copied parent is the input document.
    if let TNode::Namespace { parent, .. } = &ns {
        // SAFETY: the copy is alive (owned by the stored value).
        let owner = unsafe { (*parent.ptr().as_ptr()).doc };
        var_dump(&Value::Bool(owner == doc.as_ptr()));
    }
    std::fs::remove_file(&ext).unwrap();
    expect("gh22621.phpt", &take_output());
}

#[test]
fn clone_document() {
    setup(&fixtures());
    let xml = load_xml(r#"<?xml version="1.0"?><root><foo>hello</foo></root>"#);
    let xsl = load_xml(
        r#"<?xml version="1.0"?>
<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl" version="1.0">
    <xsl:template match="/root">
        <xsl:value-of select="php:function('test')"/>
        <xsl:value-of select="//root/foo"/>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    let target = xml.clone();
    php.define("test", move |_, _| {
        target
            .root()
            .unwrap()
            .first_child()
            .unwrap()
            .set_content(b"bye");
        Ok(Value::Null)
    });
    for clone in [true, false] {
        let proc = Proc::new();
        proc.register_php_functions_all();
        proc.set_clone_document(Some(clone));
        import(&proc, &xsl);
        echo(to_xml(&proc, &mut php, &xml).unwrap());
    }
    expect("cloneDocument.phpt", &take_output());
}

#[test]
fn special_operations_with_properties() {
    setup(&fixtures());
    let xml = load_xml(r#"<?xml version="1.0"?><root><foo>hello</foo></root>"#);
    let xsl = load_xml(
        r#"<?xml version="1.0"?>
<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl" version="1.0">
    <xsl:template match="/root">
        <xsl:value-of select="php:function('test')"/>
        <xsl:value-of select="//root/foo"/>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    php.define("test", |_, _| {
        echo("Called test\n");
        Ok(Value::Null)
    });
    for (label, prop) in [
        ("--- Unset cloneDocument ---\n", "cloneDocument"),
        ("--- Unset doXInclude ---\n", "doXInclude"),
        ("--- Make properties references ---\n", ""),
    ] {
        echo(label);
        let proc = Proc::new();
        proc.register_php_functions_all();
        proc.unset_property(prop).unwrap();
        import(&proc, &xsl);
        echo(to_xml(&proc, &mut php, &xml).unwrap());
    }
    expect("special_operations_with_properties.phpt", &take_output());
}

#[test]
fn gh23730_import_during_transformation() {
    setup(&fixtures());
    let xml = load_xml("<root><item>a</item></root>");
    let xsl = load_xml(
        r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
  <xsl:template match="/"><xsl:value-of select="php:function('callback', //item)"/></xsl:template>
</xsl:stylesheet>"#,
    );
    let other = load_xml(
        r#"<?xml version="1.0"?><xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/">REPLACED</xsl:template></xsl:stylesheet>"#,
    );
    let proc = Rc::new(Proc::new());
    let import_other = {
        let proc = Rc::downgrade(&proc);
        let other = other.clone();
        move |from: &str| {
            let proc = proc.upgrade().unwrap();
            match proc.import_stylesheet(&other) {
                Ok(_) => echo(format!("{from}: no error\n")),
                Err(e) => echo(format!("{from}: {}: {}\n", e.class().name(), e.message())),
            }
        }
    };
    let mut php = Php::new();
    let cb_import = import_other.clone();
    php.define("callback", move |_, a| {
        cb_import("callback");
        Ok(Value::Node(a[0].nodes()[0].clone()))
    });
    php.finish_hook = Some(Box::new(move || import_other("destructor")));
    proc.register_php_functions_all();
    import(&proc, &xsl);

    dump_xml(&proc, &mut php, &xml);
    let doc = to_doc(&proc, &mut php, &xml).unwrap();
    var_dump(&Value::Str(doc.as_node().content()));
    let uri = scratch().join("gh23730.out");
    let n = proc
        .transform_to_uri(&xml, uri.to_string_lossy().as_bytes(), &mut php)
        .unwrap();
    var_dump(&Value::Bool(n > 0));
    let _ = std::fs::remove_file(&uri);
    php.finish_hook = None;
    var_dump(&Value::Bool(import(&proc, &other)));
    dump_xml(&proc, &mut php, &xml);
    expect("gh23730.phpt", &take_output());
}

#[test]
fn node_list_teardown() {
    setup(&fixtures());
    let xml = load_xml("<root><a/><b/><c/><d/><e/><f/><g/><h/></root>");
    let xsl = load_xml(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:template match="/"><out><xsl:for-each select="/root/*"><xsl:value-of select="php:function('cb', .)"/></xsl:for-each></out></xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    php.define("cb", |_, a| Ok(Value::Str(a[0].nodes()[0].name())));
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    echo("done\n");
    assert!(php.node_list.is_empty());
    assert_eq!(php.finished, 1);
    expect(
        "xsltprocessor_callback_node_list_gc_teardown.phpt",
        &take_output(),
    );
}

// ---------------------------------------------------------------------------
// registerPHPFunctionNS
// ---------------------------------------------------------------------------

fn create_ns_processor(inputs: &[&str]) -> Proc {
    let body: String = inputs
        .iter()
        .map(|i| format!("<xsl:value-of select=\"{i}\" />"))
        .collect();
    let xsl = load_xml(&format!(
        r#"<?xml version="1.0" encoding="iso-8859-1" ?>
    <xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:foo="urn:foo"
    xmlns:bar="urn:bar">
    <xsl:template match="//a">{body}</xsl:template>
    </xsl:stylesheet>"#
    ));
    let proc = Proc::new();
    import(&proc, &xsl);
    proc
}

fn ns(proc: &Proc, php: &Php, uri: &str, name: &str, func: &str) {
    proc.register_php_function_ns(uri.as_bytes(), name.as_bytes(), || {
        php.resolve(func.as_bytes())
            .map_err(|e| Exc::new("TypeError", e))
    })
    .unwrap();
}

#[test]
fn register_php_function_ns() {
    setup(&fixtures());
    let input = load_xml(
        r#"<?xml version="1.0" encoding="iso-8859-1"?><a href="https://php.net">hello</a>"#,
    );
    let mut php = Php::new();
    php.define("TrampolineClass::test", |_, a| {
        var_dump(&Value::s("test"));
        var_dump(&Value::Array(a));
        Ok(Value::s("foo"))
    });
    let state: Rc<std::cell::RefCell<Vec<Value>>> = Rc::default();
    let sink = state.clone();
    php.define("StatefulClass::test", move |_, a| {
        sink.borrow_mut()
            .push(Value::Array(vec![Value::s("test"), a[0].clone()]));
        Ok(a[0].clone())
    });

    echo("--- Legit cases: none ---\n");
    let proc = create_ns_processor(&["foo:var_dump(string(@href))"]);
    silenced(|| to_xml_raw(&proc, &mut php, &input)).ok();
    catch_message();

    echo("--- Legit cases: global function callable ---\n");
    let proc = create_ns_processor(&["foo:var_dump(string(@href))"]);
    ns(&proc, &php, "urn:foo", "var_dump", "var_dump");
    let _ = to_xml_raw(&proc, &mut php, &input);

    echo("--- Legit cases: global string callable ---\n");
    let proc = create_ns_processor(&["foo:var_dump(string(@href))"]);
    ns(&proc, &php, "urn:foo", "var_dump", "var_dump");
    let _ = to_xml_raw(&proc, &mut php, &input);

    echo("--- Legit cases: trampoline callable ---\n");
    let proc = create_ns_processor(&["foo:trampoline(string(@href))"]);
    ns(
        &proc,
        &php,
        "urn:foo",
        "trampoline",
        "TrampolineClass::test",
    );
    dump_xml(&proc, &mut php, &input);

    echo("--- Legit cases: instance class method callable ---\n");
    let proc = create_ns_processor(&["foo:test(string(@href))"]);
    ns(&proc, &php, "urn:foo", "test", "StatefulClass::test");
    dump_xml(&proc, &mut php, &input);
    var_dump(&Value::Array(state.borrow().clone()));

    echo("--- Legit cases: multiple namespaces ---\n");
    let proc = create_ns_processor(&["foo:test(string(@href))", "bar:test(string(@href))"]);
    ns(&proc, &php, "urn:foo", "test", "strrev");
    ns(&proc, &php, "urn:bar", "test", "strtoupper");
    dump_xml(&proc, &mut php, &input);
    expect("registerPHPFunctionNS.phpt", &take_output());
}

#[test]
fn register_php_function_ns_errors() {
    setup(&fixtures());
    let php = Php::new();
    let attempt = |ns: &[u8], name: &[u8]| {
        let proc = create_processor(&[]);
        let r: Result<(), Exc> =
            proc.register_php_function_ns(ns, name, || Ok(php.resolve(b"strtolower").unwrap()));
        if let Err(e) = r {
            assert_eq!(e.class, "ValueError");
            echo(e.message);
            echo("\n");
        }
    };
    attempt(b"http://php.net/xsl", b"strtolower");
    attempt(b"http://php.net/xsl", b"test");
    attempt(b"urn:foo", b"$$$");
    attempt(b"urn:foo", b"x:a");
    attempt(b"urn:foo", b"\0");
    attempt(b"\0", b"strtolower");
    expect("registerPHPFunctionNS_errors.phpt", &take_output());
}

#[test]
fn exslt_override_with_register_php_function_ns() {
    setup(&fixtures());
    let xsl = load_xml(
        r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<xsl:stylesheet version="1.0" 
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:date="http://exslt.org/dates-and-times"
                extension-element-prefixes="date">
<xsl:output method="text"/>
<xsl:template match="date"><xsl:value-of select="date:year(@date)"/></xsl:template>
</xsl:stylesheet>"#,
    );
    let xml = load_xml("<?xml version=\"1.0\"?>\n<page><date date=\"2007-12-31\"/></page>");
    let mut php = Php::new();
    php.define("dummy_year", |_, a| {
        var_dump(&Value::Str(a[0].nodes()[0].text()));
        Ok(Value::s("dummy value"))
    });
    php.define("dummy_exit", |_, a| {
        var_dump(&a[0]);
        Err(Exc::new("exit", "dummy exit"))
    });
    let proc = Proc::new();
    import(&proc, &xsl);
    ns(
        &proc,
        &php,
        "http://exslt.org/dates-and-times",
        "year",
        "dummy_year",
    );
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    echo("\n");
    ns(
        &proc,
        &php,
        "http://www.w3.org/1999/XSL/Transform",
        "value-of",
        "dummy_exit",
    );
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    echo("\n");
    expect(
        "xsltprocessor_exsl_registerPhpFunctionNs.phpt",
        &take_output(),
    );
}

#[test]
fn no_stylesheet() {
    setup(&fixtures());
    let doc = Document::new(b"1.0").unwrap();
    let proc = Proc::new();
    let mut php = Php::new();
    let e = proc.transform_to_doc(&doc, &mut php).unwrap_err();
    assert!(matches!(e, XslError::Error(_)));
    echo(e.message());
    echo("\n");
    expect(
        "xsltprocessor_transformToDoc_no_stylesheet.phpt",
        &take_output(),
    );
    assert_eq!(
        proc.transform_to_xml(&doc, &mut php).unwrap_err().message(),
        "XSLTProcessor::transformToXml() can only be called after a stylesheet has been imported"
    );
    assert!(!proc.is_transforming());
    let _ = XmlOutput::Null;
}
