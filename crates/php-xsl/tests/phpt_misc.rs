// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of the remaining `ext/xsl/tests` phpts: parameters, properties,
//! security preferences, error reporting, stream includes, EXSLT,
//! XInclude, SimpleXML/modern-DOM inputs.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::doc_markdown, clippy::format_collect, clippy::similar_names)]

mod common;

use common::*;
use php_libxml::tree::Document;
use php_xsl::{ParamKey, RawDocument, XslError, XsltProcessor};

fn prepare() -> (Document, Document, Proc, Php) {
    setup(&fixtures());
    let dom = load(&fixture("xslt.xml"));
    let xsl = load(&fixture("xslt.xsl"));
    (dom, xsl, Proc::new(), Php::new())
}

fn report<T>(r: Result<T, XslError>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            echo(e.message());
            echo("\n");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Transformations
// ---------------------------------------------------------------------------

#[test]
fn xslt008_stream_wrapper_includes() {
    for (phpt, sheet) in [
        ("xslt008.phpt", "streamsinclude.xsl"),
        (
            "xslt008-mb.phpt",
            "私はガラスを食べられますstreamsinclude.xsl",
        ),
    ] {
        let (dom, _, proc, mut php) = prepare();
        echo("Test 8: Stream Wrapper Includes ");
        let xsl = load(&fixture(sheet));
        import(&proc, &xsl);
        echo("\n");
        echo(to_xml(&proc, &mut php, &dom).unwrap());
        expect(phpt, &take_output());
    }
}

#[test]
fn xslt009_stream_wrapper_document() {
    let (dom, _, proc, mut php) = prepare();
    // run-tests executes from the php-src root.
    setup(&fixtures().join("../../.."));
    echo("Test 9: Stream Wrapper XPath-Document()");
    let xsl = load(&fixture("documentxpath.xsl"));
    import(&proc, &xsl);
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    expect("xslt009.phpt", &take_output());
}

#[test]
fn xslt010_exslt() {
    setup(&fixtures());
    assert!(Proc::has_exslt_support());
    echo("Test 10: EXSLT Support");
    let dom = load(&fixture("exslt.xsl"));
    let proc = Proc::new();
    import(&proc, &dom);
    let xml = load(&fixture("exslt.xml"));
    let mut php = Php::new();
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    expect("xslt010_gt10129.phpt", &take_output());
}

#[test]
fn xslt012_parameter_array() {
    setup(&fixtures());
    echo("Test 12: Using Associative Array of Parameters");
    let dom = load(&fixture("xslt.xml"));
    let xsl = load(&fixture("xslt012.xsl"));
    let proc = Proc::new();
    import(&proc, &xsl);
    let params = vec![
        (ParamKey::Str(b"foo".to_vec()), "barbar"),
        (ParamKey::Str(b"foo1".to_vec()), "test"),
    ];
    proc.set_parameters(b"", params, None, |v| {
        Ok::<_, XslError>(v.as_bytes().to_vec())
    })
    .unwrap();
    echo("\n");
    let mut php = Php::new();
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt012.phpt", &take_output());
}

#[test]
fn bug26384_keys_clone_the_input() {
    setup(&fixtures());
    let dom = load(&fixture("area_name.xml"));
    let xsl = load(&fixture("area_list.xsl"));
    let proc = Proc::new();
    import(&proc, &xsl);
    let mut php = Php::new();
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    let second = dom
        .root()
        .unwrap()
        .first_child()
        .unwrap()
        .next_sibling()
        .unwrap();
    echo(second.name());
    expect("bug26384.phpt", &take_output());
}

#[test]
fn bug53965_include_relative_to_file_url() {
    setup(&fixtures());
    let base = format!("file://{}/53965", fixtures().display());
    let xml = load(std::path::Path::new(&format!("{base}/collection.xml")));
    let xsl = load(std::path::Path::new(&format!("{base}/collection.xsl")));
    let proc = Proc::new();
    import(&proc, &xsl);
    let mut php = Php::new();
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    expect("bug53965.phpt", &take_output());
}

#[test]
fn bug70535_simplexml_input() {
    setup(&fixtures());
    let input = load_sxe("<root></root>");
    let xsl = load_xml(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="root"><xsl:text>success</xsl:text></xsl:template></xsl:stylesheet>"#,
    );
    let proc = Proc::new();
    import(&proc, &xsl);
    let mut php = Php::new();
    // A SimpleXMLElement passes its node; the processor uses its document.
    let out = to_xml(&proc, &mut php, &input.root().unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&out).contains("success"));
    echo("okey\n");
    expect("bug70535.phpt", &take_output());
}

#[test]
fn gh19988_result_document_for_simplexml() {
    setup(&fixtures());
    let sxe = load(&fixture("53965/collection.xml"));
    let dom = load(&fixture("53965/collection.xsl"));
    let proc = Proc::new();
    import(&proc, &dom);
    let mut php = Php::new();
    let result = proc.transform_to_doc(&sxe, &mut php).unwrap().unwrap();
    // The VM wraps the result in a SimpleXMLElement of the returnClass.
    let doc = result.into_document();
    assert!(doc.root().is_some());
    assert_eq!(
        XslError::unknown_return_class("NonExistent").message(),
        "XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a valid class name or null, \"NonExistent\" given"
    );
    assert_eq!(
        XslError::incompatible_return_class("SimpleXMLElement", "DOMDocument").message(),
        "XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a class name compatible with SimpleXMLElement, DOMDocument given"
    );
}

#[test]
fn xinclude_in_document_function() {
    let dir = fixtures().join("xinclude");
    setup(&dir);
    let xml = load_xml("<?xml version=\"1.0\"?><root/>");
    let xsl = load_xml(
        r#"<?xml version="1.0"?>
<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
    <xsl:template match="/root">
        <container>
            <xsl:value-of select="document('data.xml')/data/content"/>
        </container>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let mut php = Php::new();
    for xinclude in [true, false] {
        let proc = Proc::new();
        proc.set_do_xinclude(Some(xinclude));
        import(&proc, &xsl);
        echo(to_xml(&proc, &mut php, &xml).unwrap());
    }
    let raw = std::fs::read(dir.join("xinclude.phpt")).unwrap();
    let expected = String::from_utf8_lossy(&raw);
    let expected = expected.split("--EXPECT--\n").nth(1).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&take_output()).trim(),
        expected.trim()
    );
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[test]
fn bug64137_quotes_in_parameters() {
    setup(&fixtures());
    for input in [
        "",
        "a'",
        "a\"",
        "fo'o\"ba'r\"ba'z",
        "\"\"\"fo'o\"ba'r\"ba'z\"\"\"",
        "'''\"\"\"fo'o\"ba'r\"ba'z\"\"\"'''",
    ] {
        let xml = load_xml("<X/>");
        let xsl = load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="foo"/><xsl:template match="/"><xsl:value-of select="$foo"/></xsl:template></xsl:stylesheet>"#,
        );
        let proc = Proc::new();
        import(&proc, &xsl);
        proc.set_parameter(b"", b"foo", Some(input.as_bytes()))
            .unwrap();
        let mut php = Php::new();
        if let Some(s) = to_xml(&proc, &mut php, &xml) {
            echo(s);
        }
        echo("\n");
    }
    expect("bug64137.phpt", &take_output());
}

#[test]
fn bug48221_empty_parameter_name() {
    for phpt in [
        "bug48221.phpt",
        "xsltprocessor_setparameter-errorquote.phpt",
    ] {
        let (dom, xsl, proc, mut php) = prepare();
        import(&proc, &xsl);
        proc.set_parameter(b"", b"", Some(b"\"'")).unwrap();
        let _ = to_xml(&proc, &mut php, &dom);
        echo("Done\n");
        expect(phpt, &take_output());
    }
}

#[test]
fn set_parameter_no_string_key() {
    let (dom, xsl, proc, mut php) = prepare();
    import(&proc, &xsl);
    let r = proc.set_parameters(
        b"",
        vec![(ParamKey::Int(0), "4"), (ParamKey::Int(1), "abc")],
        None,
        |v| Ok::<_, XslError>(v.as_bytes().to_vec()),
    );
    let e = r.unwrap_err();
    assert!(matches!(e, XslError::ArgumentType { .. }));
    echo(e.message());
    echo("\n");
    let _ = to_xml(&proc, &mut php, &dom);
    expect("xsltprocessor_setparameter-nostring.phpt", &take_output());
}

#[test]
fn get_and_remove_parameter() {
    let (_, xsl, proc, _) = prepare();
    var_dump(
        &proc
            .get_parameter(b"", b"doesnotexist")
            .unwrap()
            .map_or(Value::Bool(false), Value::Str),
    );
    expect(
        "xsltprocessor_getParameter-invalidparam.phpt",
        &take_output(),
    );

    import(&proc, &xsl);
    proc.set_parameter(b"", b"key", Some(b"value")).unwrap();
    var_dump(
        &proc
            .get_parameter(b"", b"key")
            .unwrap()
            .map_or(Value::Bool(false), Value::Str),
    );
    expect("xsltprocessor_getParameter.phpt", &take_output());

    var_dump(&Value::Bool(
        proc.remove_parameter(b"", b"doesnotexist").unwrap(),
    ));
    expect(
        "xsltprocessor_removeParameter-invalidparam.phpt",
        &take_output(),
    );

    proc.remove_parameter(b"", b"key").unwrap();
    var_dump(
        &proc
            .get_parameter(b"", b"key")
            .unwrap()
            .map_or(Value::Bool(false), Value::Str),
    );
    expect("xsltprocessor_removeParameter.phpt", &take_output());
}

#[test]
fn req30622_namespaced_parameters() {
    setup(&fixtures());
    let xml = load_xml("<root/>");
    let xsl = load_xml(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                              xmlns:test="http://www.php.net/test">
    <xsl:param name="foo" select="'EMPTY'"/>
    <xsl:param name="test:foo" select="'EMPTY'"/>

    <xsl:template match="/root">
        <xsl:text>Namespace "NULL": </xsl:text>
        <xsl:value-of select="$foo"/>
        <xsl:text>, Namespace "http://www.php.net/test": </xsl:text>
        <xsl:value-of select="$test:foo"/>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let proc = Proc::new();
    let mut php = Php::new();
    import(&proc, &xsl);
    let get = |ns: &[u8]| {
        var_dump(
            &proc
                .get_parameter(ns, b"foo")
                .unwrap()
                .map_or(Value::Bool(false), Value::Str),
        );
    };
    let ns: &[u8] = b"http://www.php.net/test";

    echo("--- Set both empty and non-empty namespace ---\n");
    proc.set_parameter(b"", b"foo", Some(b"SET1")).unwrap();
    proc.set_parameter(ns, b"foo", Some(b"SET2")).unwrap();
    get(b"");
    get(ns);
    echo(to_xml(&proc, &mut php, &xml).unwrap());

    echo("--- Remove empty namespace entry ---\n");
    var_dump(&Value::Bool(proc.remove_parameter(b"", b"foo").unwrap()));
    get(b"");
    get(ns);
    echo(to_xml(&proc, &mut php, &xml).unwrap());

    echo("--- Remove non-empty namespace entry ---\n");
    var_dump(&Value::Bool(proc.remove_parameter(ns, b"foo").unwrap()));
    get(b"");
    get(ns);
    echo(to_xml(&proc, &mut php, &xml).unwrap());

    echo("--- Set via array ---\n");
    let to = |v: &&str| Ok::<_, XslError>(v.as_bytes().to_vec());
    proc.set_parameters(
        b"",
        vec![(ParamKey::Str(b"foo".to_vec()), "SET1")],
        None,
        to,
    )
    .unwrap();
    proc.set_parameters(ns, vec![(ParamKey::Str(b"foo".to_vec()), "SET2")], None, to)
        .unwrap();
    echo(to_xml(&proc, &mut php, &xml).unwrap());
    expect("req30622.phpt", &take_output());
}

#[test]
fn req30622_errors() {
    setup(&fixtures());
    let proc = Proc::new();
    report(proc.set_parameter(b"urn:x", b"{urn:a}x", Some(b"")));
    report(proc.set_parameter(b"urn:x", b"a:b", Some(b"")));
    report(proc.get_parameter(b"urn:x", b"{urn:a}x"));
    report(proc.get_parameter(b"urn:x", b"a:b"));
    report(proc.remove_parameter(b"urn:x", b"{urn:a}x"));
    report(proc.remove_parameter(b"urn:x", b"a:b"));
    proc.set_parameter(b"urn:x", b":b", Some(b"")).unwrap();
    proc.set_parameter(b"urn:x", b":", Some(b"")).unwrap();
    expect("req30622_errors.phpt", &take_output());
}

#[test]
fn set_parameter_null_bytes() {
    setup(&fixtures());
    let proc = Proc::new();
    let to = |v: &&str| Ok::<_, XslError>(v.as_bytes().to_vec());
    report(proc.set_parameter(b"", b"foo\0", Some(b"bar")));
    report(proc.set_parameter(b"", b"foo", Some(b"bar\0")));
    report(proc.set_parameters(
        b"",
        vec![(ParamKey::Str(b"foo\0".to_vec()), "bar")],
        None,
        to,
    ));
    report(proc.set_parameters(
        b"",
        vec![(ParamKey::Str(b"foo".to_vec()), "bar\0")],
        None,
        to,
    ));
    expect("setParameter_null_bytes.phpt", &take_output());
}

/// The values of `setParameter_exceptions_test.phpt`.
#[derive(Clone, Copy)]
enum PValue {
    Null,
    True,
    ThrowingStringable,
    Array,
}

#[test]
fn set_parameter_exceptions() {
    setup(&fixtures());
    let run = |options: Vec<(ParamKey, PValue)>, warning_throws: bool| -> Result<(), Exc> {
        let xml = load_xml("<X/>");
        let xsl = load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:param name="foo"/><xsl:template match="/"><xsl:value-of select="$foo"/></xsl:template></xsl:stylesheet>"#,
        );
        let proc = Proc::new();
        import(&proc, &xsl);
        proc.set_parameters(b"", options, None, |v| match v {
            PValue::Null => Ok(Vec::new()),
            PValue::True => Ok(b"1".to_vec()),
            PValue::ThrowingStringable => Err(Exc::new("Exception", "exception!")),
            PValue::Array => {
                if warning_throws {
                    Err(Exc::new("Exception", "Array to string conversion"))
                } else {
                    echo("\nWarning: Array to string conversion in /test.php on line 1\n");
                    Ok(b"Array".to_vec())
                }
            }
        })?;
        let mut php = Php::new();
        if let Some(s) = to_xml(&proc, &mut php, &xml) {
            echo(s);
        }
        echo("\n");
        Ok(())
    };
    let catch = |r: Result<(), Exc>| {
        if let Err(e) = r {
            echo(e.message);
            echo("\n");
        }
    };
    echo("--- Invalid key ---\n");
    catch(run(vec![(ParamKey::Int(12345), PValue::Null)], false));
    echo("--- Valid key and value, but special cases ---\n");
    catch(run(
        vec![(ParamKey::Str(b"foo".to_vec()), PValue::Null)],
        false,
    ));
    catch(run(
        vec![(ParamKey::Str(b"foo".to_vec()), PValue::True)],
        false,
    ));
    echo("--- Exception from __toString should abort execution ---\n");
    catch(run(
        vec![(ParamKey::Str(b"foo".to_vec()), PValue::ThrowingStringable)],
        false,
    ));
    echo("--- Exception from warning should abort execution ---\n");
    let arrays = || {
        vec![
            (ParamKey::Str(b"foo".to_vec()), PValue::Array),
            (ParamKey::Str(b"foo2".to_vec()), PValue::Array),
        ]
    };
    catch(run(arrays(), true));
    echo("--- Warning may continue execution ---\n");
    catch(run(arrays(), false));
    expect("setParameter_exceptions_test.phpt", &take_output());
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn dump_props(proc: &Proc, id: u32) {
    let p = proc.properties();
    echo(format!("object(XSLTProcessor)#{id} (4) {{\n"));
    echo(format!(
        "  [\"doXInclude\"]=>\n  bool({})\n",
        p.do_xinclude.unwrap()
    ));
    echo(format!(
        "  [\"cloneDocument\"]=>\n  bool({})\n",
        p.clone_document.unwrap()
    ));
    echo(format!(
        "  [\"maxTemplateDepth\"]=>\n  int({})\n",
        p.max_template_depth
    ));
    echo(format!(
        "  [\"maxTemplateVars\"]=>\n  int({})\n",
        p.max_template_vars
    ));
    echo("}\n");
}

#[test]
fn max_template_errors() {
    setup(&fixtures());
    for (phpt, prop) in [
        ("maxTemplateDepth_errors.phpt", "depth"),
        ("maxTemplateVars_errors.phpt", "vars"),
    ] {
        let proc = Proc::new();
        let get = |p: &Proc| {
            let props = p.properties();
            if prop == "depth" {
                props.max_template_depth
            } else {
                props.max_template_vars
            }
        };
        let old = get(&proc);
        // -1, (int)-32.1 and (int)"-1" after the VM's coercion.
        for value in [-1, -32, -1] {
            let r = if prop == "depth" {
                proc.set_max_template_depth(value)
            } else {
                proc.set_max_template_vars(value)
            };
            if let Err(e) = r {
                assert_eq!(e.class(), php_xsl::ErrorClass::ValueError);
                echo(e.message());
                echo("\n");
            }
            var_dump(&Value::Bool(get(&proc) == old));
        }
        expect(phpt, &take_output());
    }
}

#[test]
fn max_template_validation_bypass() {
    setup(&fixtures());
    for (phpt, prop) in [
        (
            "maxTemplateDepth_modification_validation_bypass.phpt",
            "maxTemplateDepth",
        ),
        (
            "maxTemplateVars_modification_validation_bypass.phpt",
            "maxTemplateVars",
        ),
    ] {
        let proc = Proc::new();
        for value in [1, -1] {
            echo(format!("--- Set to {value} ---\n"));
            let r = if prop == "maxTemplateDepth" {
                proc.set_max_template_depth(value)
            } else {
                proc.set_max_template_vars(value)
            };
            report(r);
        }
        echo("--- Get reference ---\n");
        report(Proc::check_indirect_modification(prop));
        echo("--- Unset ---\n");
        report(proc.unset_property(prop));
        echo("--- Dump ---\n");
        dump_props(&proc, 1);
        expect(phpt, &take_output());
    }
}

#[test]
fn new_without_constructor() {
    setup(&fixtures());
    let proc = Proc::new();
    dump_props(&proc, 2);
    expect("new_without_constructor.phpt", &take_output());
    assert_eq!(php_xsl::PROPERTY_NAMES.len(), 4);
}

#[test]
fn has_exslt_support_and_info() {
    setup(&fixtures());
    var_dump(&Value::Bool(Proc::has_exslt_support()));
    expect("xsltprocessor_hasExsltSupport.phpt", &take_output());
    let rows = php_xsl::class::info_rows();
    assert!(rows
        .iter()
        .any(|(k, _)| *k == "libxslt compiled against libxml Version"));
}

#[test]
fn import_invalid_param_message() {
    setup(&fixtures());
    let e = XslError::invalid_node("importStylesheet");
    assert_eq!(e.class(), php_xsl::ErrorClass::TypeError);
    echo(e.message());
    echo("\n");
    expect(
        "xsltprocessor_importStylesheet-invalidparam.phpt",
        &take_output(),
    );
    echo(XslError::clone_retained().message());
    assert_eq!(
        String::from_utf8(take_output()).unwrap(),
        "XSLTProcessor::importStylesheet(): Argument #1 ($stylesheet) must not have its clone retained by __clone()"
    );
}

// ---------------------------------------------------------------------------
// Security preferences
// ---------------------------------------------------------------------------

#[test]
fn bug54446_security_prefs() {
    for (phpt, name) in [
        ("bug54446.phpt", "bug54446test.txt"),
        ("bug54446_with_ini.phpt", "bug54446test_with_ini.txt"),
    ] {
        let (dom, _, proc, mut php) = prepare();
        let outputfile = scratch().join(name);
        let _ = std::fs::remove_file(&outputfile);
        let xsl = load_xml(&format!(
            r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:sax="http://icl.com/saxon"
    extension-element-prefixes="sax">

    <xsl:template match="/">
        <sax:output href="{}" method="text">
            <xsl:value-of select="'0wn3d via PHP and libxslt ...'"/>
        </sax:output>
    </xsl:template>

</xsl:stylesheet>"#,
            outputfile.display()
        ));
        import(&proc, &xsl);
        let check = |should_exist: bool| {
            let exists = outputfile.exists();
            match (should_exist, exists) {
                (false, false) => echo("OK, no file created\n"),
                (true, true) => echo("OK, file exists\n"),
                (false, true) => echo(format!("{} exists, but shouldn't!\n", outputfile.display())),
                (true, false) => echo(format!(
                    "{} doesn't exist, but should!\n",
                    outputfile.display()
                )),
            }
        };
        if let Some(s) = to_xml(&proc, &mut php, &dom) {
            echo(s);
        }
        check(false);
        assert_eq!(proc.set_security_prefs(0), 44);
        if let Some(s) = to_xml(&proc, &mut php, &dom) {
            echo(s);
        }
        check(true);
        std::fs::remove_file(&outputfile).unwrap();
        proc.set_security_prefs(4 | 32 | 8);
        if let Some(s) = to_xml(&proc, &mut php, &dom) {
            echo(s);
        }
        check(false);
        assert_eq!(proc.get_security_prefs(), 44);
        expect(phpt, &take_output());
    }
}

#[test]
fn read_file_security_pref_blocks_document() {
    let (dom, _, proc, mut php) = prepare();
    let xsl = load_xml(&format!(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
<xsl:output method="text"/>
<xsl:template match="/"><xsl:value-of select="count(document('{}')/*)"/></xsl:template>
</xsl:stylesheet>"#,
        fixture("xslt.xml").display()
    ));
    import(&proc, &xsl);
    assert_eq!(to_xml(&proc, &mut php, &dom).unwrap(), b"1");
    assert!(take_output().is_empty());
    proc.set_security_prefs(php_xsl::processor::SECPREF_READ_FILE);
    let out = to_xml(&proc, &mut php, &dom);
    let text = String::from_utf8_lossy(&take_output()).into_owned();
    assert!(text.contains("refused"), "{text}");
    assert_ne!(out.as_deref(), Some(&b"1"[..]));
}

// ---------------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------------

#[test]
fn bug71571_max_template_depth() {
    setup(&fixtures());
    let xsl = load_xml(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
    <xsl:template match="/">
        <xsl:call-template name="recurse"/>
    </xsl:template>

    <xsl:template name="recurse">
        <xsl:call-template name="recurse"/>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let doc = Document::new(b"1.0").unwrap();
    let proc = Proc::new();
    proc.set_max_template_depth(2).unwrap();
    import(&proc, &xsl);
    let mut php = Php::new();
    let _ = to_doc(&proc, &mut php, &doc);
    expect("bug71571_a.phpt", &take_output());
}

#[test]
fn bug71571_max_template_vars() {
    setup(&fixtures());
    let xsl = load_xml(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
    <xsl:template match="/">
        <xsl:call-template name="recurse"/>
    </xsl:template>

    <xsl:template name="recurse">
        <xsl:param name="COUNT">1</xsl:param>
        <xsl:call-template name="recurse">
            <xsl:with-param name="COUNT" select="$COUNT + 1"/>
        </xsl:call-template>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    let doc = Document::new(b"1.0").unwrap();
    let proc = Proc::new();
    proc.set_max_template_depth(1 << 30).unwrap();
    proc.set_max_template_vars(2).unwrap();
    import(&proc, &xsl);
    let mut php = Php::new();
    let _ = to_doc(&proc, &mut php, &doc);
    expect("bug71571_b.phpt", &take_output());
}

#[test]
fn gh21496_not_a_stylesheet() {
    setup(&fixtures());
    let doc = load_xml("<container/>");
    let proc = Proc::new();
    // The comment's owner document (with DOM's URL) is imported.
    var_dump(&Value::Bool(import(&proc, &doc)));
    let sxe = load_sxe("<container/>");
    let proc = Proc::new();
    import(&proc, &sxe);
    expect("gh21496.phpt", &take_output());
}

#[test]
fn bug71540_unknown_xpath_function() {
    // Skipped upstream for libxml2 >= 2.9.3 (different messages); here we
    // check that the errors are reported and the transformation completes.
    setup(&fixtures());
    let xml = load_xml("<allusers>\n <user>\n  <uid>bob</uid>\n </user>\n</allusers>");
    let xsl = load_xml(
        r#"<?xml version="1.0" encoding="UTF-8"?>
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
             select="php:function('test',uid,test(test))"/>
      </td></tr>
    </xsl:for-each>
    </table>
  </body></html>
 </xsl:template>
</xsl:stylesheet>"#,
    );
    let proc = Proc::new();
    proc.register_php_functions_all();
    import(&proc, &xsl);
    let mut php = Php::new();
    let out = to_xml(&proc, &mut php, &xml);
    let text = String::from_utf8_lossy(&take_output()).into_owned();
    assert!(
        text.contains("xmlXPathCompOpEval: function test not found"),
        "{text}"
    );
    assert!(text.contains("Unregistered function"), "{text}");
    // libxslt aborts the result once value-of failed.
    assert!(out.is_none());
}

// ---------------------------------------------------------------------------
// Modern DOM documents
// ---------------------------------------------------------------------------

/// `Dom\XMLDocument::createFromString()` stand-in: a document flagged as
/// modern (the processor then restores `xmlns` attributes as
/// declarations).
fn modern(doc: &Document) -> RawDocument {
    // SAFETY: the test keeps `doc` alive while the RawDocument is used.
    unsafe { RawDocument::new(std::ptr::NonNull::new(doc.as_ptr()).unwrap(), true) }
}

#[test]
fn modern_dom_auto_registration() {
    setup(&fixtures());
    let sheet = load_sxe(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:php="http://php.net/xsl"
    xmlns:str="http://exslt.org/strings"
    xmlns:xsdl="http://www.w3.org/1999/XSL/Transform">
    <xsl:output method="text" omit-xml-declaration="yes" indent="no"/>
    <xsl:template match="/root">
        <xsl:value-of select="php:function('strtoupper', string(./hello))"/>
        <xsl:value-of select="test:reverse(string(./hello))"/>
    </xsl:template>
</xsl:stylesheet>"#,
    );
    // The VM clones the document with php-dom's clone handler, which
    // keeps modern DOM's xmlns attributes; here: a copy to which
    // setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:test',
    // 'urn:test') is applied the way modern DOM stores it (an attribute in
    // the xmlns namespace, whose xmlNs lives outside the tree).
    // SAFETY: plain libxml2 tree construction on a fresh copy we own; the
    // namespace struct is kept in the copy's oldNs list (php-dom's ns
    // mapper owns it in PHP).
    let clone = unsafe {
        let copy = php_xsl::ffi::xmlCopyDoc(sheet.as_ptr(), 1);
        let root = php_libxml::ffi::xmlDocGetRootElement(copy);
        let ns = php_libxml::ffi::xmlNewNs(
            std::ptr::null_mut(),
            c"http://www.w3.org/2000/xmlns/".as_ptr().cast(),
            c"xmlns_".as_ptr().cast(),
        );
        php_libxml::ffi::xmlSetNsProp(
            root,
            ns,
            c"test".as_ptr().cast(),
            c"urn:test".as_ptr().cast(),
        );
        // Freed with the document.
        (*ns).next = (*copy).oldNs;
        (*copy).oldNs = ns;
        php_xsl::OwnedDocument::from_raw(std::ptr::NonNull::new(copy).unwrap())
    };
    let input = load_sxe("<root>\n    <hello>World</hello>\n</root>");
    let proc = Proc::new();
    let mut php = Php::new();
    assert!(proc.import_stylesheet_owned(clone, true, None).unwrap());
    proc.register_php_functions_all();
    proc.register_php_function_ns(b"urn:test", b"reverse", || {
        php.resolve(b"strrev").map_err(XslError::Error)
    })
    .unwrap();
    echo(to_xml(&proc, &mut php, &modern(&input)).unwrap_or_default());
    echo("\n");
    expect("auto_registration_namespaces_new_dom.phpt", &take_output());
}

#[test]
fn modern_dom_namespace_mapper_lifetime() {
    setup(&fixtures());
    let input = load_sxe("<root>\n    <hello>World</hello>\n</root>");
    let xsl = r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:php="http://php.net/xsl"
    xmlns:xsdl="http://www.w3.org/1999/XSL/Transform">
    <xsl:output method="text" omit-xml-declaration="yes" indent="no"/>
    <xsl:template match="/root">
        <xsl:value-of select="php:function('strtoupper', string(./hello))"/>
    </xsl:template>
</xsl:stylesheet>"#;
    let proc = Proc::new();
    for _ in 0..2 {
        let tmp = load_sxe(xsl);
        import(&proc, &modern(&tmp));
    }
    proc.register_php_functions_all();
    let mut php = Php::new();
    echo(to_xml(&proc, &mut php, &modern(&input)).unwrap_or_default());
    echo("\n");
    expect(
        "XSLTProcessor_namespace_mapper_lifetime.phpt",
        &take_output(),
    );
}

#[test]
fn gh21357_modern_stylesheet_with_unused_namespace() {
    setup(&fixtures());
    let sheet = load_sxe(
        r#"<xsl:stylesheet xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:tns="urn:myns" version="1.0">
	<xsl:template match="tns:referee"/>
</xsl:stylesheet>"#,
    );
    let proc = Proc::new();
    var_dump(&Value::Bool(import(&proc, &modern(&sheet))));
    expect("gh21357_1.phpt", &take_output());
}

#[test]
fn processor_is_generic_over_callables() {
    // Any Clone type works as the callable representation.
    let p: XsltProcessor<u32> = XsltProcessor::new();
    p.register_php_functions_name(b"f", |_| Ok(7)).unwrap();
    assert!(!p.has_stylesheet());
}

#[test]
fn get_parameter_strings() {
    setup(&fixtures());
    let proc = Proc::new();
    let get = || {
        var_dump(
            &proc
                .get_parameter(b"", b"foo")
                .unwrap()
                .map_or(Value::Bool(false), Value::Str),
        );
    };
    proc.set_parameter(b"", b"foo", Some(b"bar")).unwrap();
    get();
    proc.remove_parameter(b"", b"foo").unwrap();
    get();
    proc.set_parameter(b"", b"foo", Some("\"abcd\"".repeat(2).as_bytes()))
        .unwrap();
    get();
    proc.remove_parameter(b"", b"foo").unwrap();
    get();
    expect("getParameter.phpt", &take_output());
}

#[test]
fn phpinfo_section() {
    setup(&fixtures());
    // phpinfo() in text mode: "key => value" rows.
    for (k, v) in php_xsl::class::info_rows() {
        echo(format!("{k} => {v}\n"));
    }
    expect("xsl-phpinfo.phpt", &take_output());
}
