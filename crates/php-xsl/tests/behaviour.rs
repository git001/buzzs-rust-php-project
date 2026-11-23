// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Behaviour beyond the phpt ports: profiling, the internal error list,
//! panics in callbacks, re-entrancy, `transformToXml()` returning null.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::doc_markdown)]

mod common;

use std::rc::Rc;

use common::*;
use php_xsl::{XmlOutput, XslError, XsltProcessor};

const IDENTITY_TEXT: &str = r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
<xsl:output method="text"/>
<xsl:template match="/"><xsl:value-of select="/*"/></xsl:template>
</xsl:stylesheet>"#;

#[test]
fn empty_text_output_is_null() {
    setup(&fixtures());
    let proc = Proc::new();
    import(&proc, &load_xml(IDENTITY_TEXT));
    let mut php = Php::new();
    let r = proc.transform_to_xml(&load_xml("<a/>"), &mut php).unwrap();
    assert_eq!(r, XmlOutput::Null);
    let r = proc
        .transform_to_xml(&load_xml("<a>x</a>"), &mut php)
        .unwrap();
    assert_eq!(r, XmlOutput::Str(b"x".to_vec()));
}

#[test]
fn profiling_writes_a_report() {
    setup(&fixtures());
    let proc = Proc::new();
    import(&proc, &load(&fixture("xslt.xsl")));
    let path = scratch().join("profile.txt");
    let _ = std::fs::remove_file(&path);
    assert!(proc
        .set_profiling(Some(path.to_string_lossy().as_bytes()))
        .unwrap());
    let mut php = Php::new();
    to_xml(&proc, &mut php, &load(&fixture("xslt.xml"))).unwrap();
    let report = std::fs::read_to_string(&path).unwrap();
    assert!(report.contains("number"), "{report}");
    assert!(report.contains("Calls"), "{report}");
    std::fs::remove_file(&path).unwrap();
    assert!(proc.set_profiling(None).unwrap());
    assert_eq!(proc.profiling(), None);
    assert_eq!(
        proc.set_profiling(Some(b"a\0b")).unwrap_err().message(),
        "XSLTProcessor::setProfiling(): Argument #1 ($filename) must not contain any null bytes"
    );
}

#[test]
fn internal_errors_collect_libxslt_messages() {
    setup(&fixtures());
    php_libxml::error::use_internal_errors(Some(true));
    let proc = Proc::new();
    let sxe = load_sxe("<container/>");
    assert!(!import(&proc, &sxe));
    assert!(take_output().is_empty());
    let errors = php_libxml::error::get_errors();
    let messages: Vec<String> = errors
        .iter()
        .map(|e| String::from_utf8_lossy(&e.message).into_owned())
        .collect();
    assert_eq!(
        messages,
        vec![
            "compilation error: element container".to_string(),
            "xsltParseStylesheetProcess : document is not a stylesheet".to_string()
        ]
    );
    php_libxml::error::use_internal_errors(Some(false));
}

#[test]
fn panic_in_callback_is_resumed() {
    setup(&fixtures());
    let proc = Proc::new();
    import(
        &proc,
        &load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:template match="/"><xsl:value-of select="php:function('boom')"/><xsl:value-of select="php:function('boom')"/></xsl:template>
</xsl:stylesheet>"#,
        ),
    );
    proc.register_php_functions_all();
    let mut php = Php::new();
    php.define("boom", |_, _| panic!("host bug"));
    let input = load_xml("<a/>");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        proc.transform_to_xml(&input, &mut php)
    }));
    let payload = r.unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"host bug"));
    assert!(!proc.is_transforming());
    // The processor stays usable.
    let mut php = Php::new();
    php.define("boom", |_, _| Ok(Value::s("fine")));
    assert_eq!(
        proc.transform_to_xml(&input, &mut php).unwrap(),
        XmlOutput::Str(b"<?xml version=\"1.0\"?>\nfinefine\n".to_vec())
    );
}

#[test]
fn reentrant_calls_from_callbacks() {
    setup(&fixtures());
    let proc = Rc::new(Proc::new());
    import(
        &proc,
        &load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:output method="text"/>
<xsl:param name="p" select="'unset'"/>
<xsl:template match="/"><xsl:value-of select="php:function('inner', string(/*))"/>|<xsl:value-of select="$p"/></xsl:template>
</xsl:stylesheet>"#,
        ),
    );
    proc.register_php_functions_all();
    let weak = Rc::downgrade(&proc);
    let mut php = Php::new();
    php.define("inner", move |php, a| {
        let proc = weak.upgrade().unwrap();
        assert!(proc.is_transforming());
        // Parameters may change during a transformation (next run sees them).
        proc.set_parameter(b"", b"p", Some(b"set")).unwrap();
        if arg(&a, 0) == b"outer" {
            // A nested transformation with the same processor.
            let inner_doc = load_xml("<x>nested</x>");
            match proc.transform_to_xml(&inner_doc, php).unwrap() {
                XmlOutput::Str(s) => return Ok(Value::Str(s)),
                other => panic!("{other:?}"),
            }
        }
        Ok(Value::Str(arg(&a, 0)))
    });
    let out = proc
        .transform_to_xml(&load_xml("<x>outer</x>"), &mut php)
        .unwrap();
    assert_eq!(out, XmlOutput::Str(b"nested|set|unset".to_vec()));
    assert!(!proc.is_transforming());
    let out = proc
        .transform_to_xml(&load_xml("<x>again</x>"), &mut php)
        .unwrap();
    assert_eq!(out, XmlOutput::Str(b"again|set".to_vec()));
    // One `transformation_finished` per transformation, nested included.
    assert_eq!(php.finished, 3);
}

#[test]
fn exceptions_stop_further_callbacks() {
    setup(&fixtures());
    let proc = Proc::new();
    import(
        &proc,
        &load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:template match="/"><xsl:value-of select="php:function('first')"/><xsl:value-of select="php:function('second')"/></xsl:template>
</xsl:stylesheet>"#,
        ),
    );
    proc.register_php_functions_all();
    let mut php = Php::new();
    php.define("first", |_, _| Err(Exc::new("Exception", "first")));
    php.define("second", |_, _| {
        echo("second called");
        Ok(Value::Null)
    });
    let _ = proc.transform_to_xml(&load_xml("<a/>"), &mut php);
    assert_eq!(take_exception().unwrap().message, "first");
    assert!(take_output().is_empty());
}

#[test]
fn returned_external_nodes_and_values() {
    setup(&fixtures());
    let proc = Proc::new();
    import(
        &proc,
        &load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:output method="text"/>
<xsl:template match="/"><xsl:value-of select="php:function('node')/b"/>,<xsl:value-of select="php:function('t')"/>,<xsl:value-of select="php:function('f')"/>,<xsl:value-of select="php:function('n', 1.5, true())"/>,<xsl:value-of select="php:functionString('s', /*)"/>,<xsl:value-of select="count(php:function('nodes', /*/*))"/></xsl:template>
</xsl:stylesheet>"#,
        ),
    );
    proc.register_php_functions_all();
    let mut php = Php::new();
    php.define("node", |_, _| {
        let doc = load_xml("<r><b>external</b></r>");
        Ok(Value::Node(TNode::Handle(doc.root().unwrap())))
    });
    php.define("t", |_, _| Ok(Value::Bool(true)));
    php.define("f", |_, _| Ok(Value::Bool(false)));
    php.define("n", |_, a| {
        assert!(matches!(a[0], Value::Float(f) if (f - 1.5).abs() < f64::EPSILON));
        assert!(matches!(a[1], Value::Bool(true)));
        Ok(Value::Int(42))
    });
    php.define("s", |_, a| {
        assert!(matches!(&a[0], Value::Str(s) if s == b"xy"));
        Ok(a[0].clone())
    });
    php.define("nodes", |_, a| {
        assert_eq!(a[0].nodes().len(), 2);
        Ok(a[0].nodes()[1].clone().into())
    });
    let out = to_xml(&proc, &mut php, &load_xml("<a><c>x</c><c>y</c></a>")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out), "external,true,false,42,xy,1");
}

impl From<TNode> for Value {
    fn from(n: TNode) -> Self {
        Self::Node(n)
    }
}

#[test]
fn key_stylesheets_transform_a_copy() {
    setup(&fixtures());
    let proc = Proc::new();
    import(
        &proc,
        &load_xml(
            r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl">
<xsl:key name="k" match="c" use="."/>
<xsl:output method="text"/>
<xsl:template match="/"><xsl:value-of select="php:function('touch', key('k', 'x'))"/></xsl:template>
</xsl:stylesheet>"#,
        ),
    );
    proc.register_php_functions_all();
    let input = load_xml("<a><c>x</c></a>");
    let mut php = Php::new();
    let input_ptr = input.as_ptr();
    php.define("touch", move |_, a| {
        let n = &a[0].nodes()[0];
        if let TNode::Raw { ptr, keep_doc, .. } = n {
            // The node belongs to the processor's private copy, which the
            // host can keep alive.
            // SAFETY: the node is alive during the callback.
            let doc = unsafe { (*ptr.as_ptr()).doc };
            assert_ne!(doc, input_ptr);
            assert_eq!(
                keep_doc.as_ref().map(php_libxml::tree::Document::as_ptr),
                Some(doc)
            );
        } else {
            panic!("expected a node");
        }
        Ok(Value::s("ok"))
    });
    assert_eq!(to_xml(&proc, &mut php, &input).unwrap(), b"ok");
}

#[test]
fn return_class_check() {
    let is_sub =
        |a: &str, b: &str| a == b || (a == "AdvancedXMLElement" && b == "SimpleXMLElement");
    assert!(XsltProcessor::<u8>::check_return_class(
        "AdvancedXMLElement",
        "SimpleXMLElement",
        "AdvancedXMLElement",
        is_sub
    )
    .is_ok());
    let e: XslError = XsltProcessor::<u8>::check_return_class(
        "SimpleXMLElement",
        "SimpleXMLElement",
        "DOMDocument",
        is_sub,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        "XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a class name compatible with SimpleXMLElement, DOMDocument given"
    );
}

#[test]
fn security_prefs_round_trip_and_constants() {
    let proc = Proc::new();
    assert_eq!(
        proc.get_security_prefs(),
        php_xsl::processor::SECPREF_DEFAULT
    );
    assert_eq!(proc.set_security_prefs(-1), 44);
    assert_eq!(proc.get_security_prefs(), -1);
    let consts = php_xsl::class::constants();
    assert!(consts.iter().any(|(n, _)| *n == "LIBEXSLT_DOTTED_VERSION"));
}

#[test]
fn transform_to_uri_null_bytes_and_failure() {
    setup(&fixtures());
    let proc = Proc::new();
    import(&proc, &load_xml(IDENTITY_TEXT));
    let mut php = Php::new();
    let doc = load_xml("<a>x</a>");
    assert_eq!(
        proc.transform_to_uri(&doc, b"a\0b", &mut php)
            .unwrap_err()
            .message(),
        "XSLTProcessor::transformToUri(): Argument #2 ($uri) must not contain any null bytes"
    );
    let n =
        silenced(|| proc.transform_to_uri(&doc, b"/nonexistent-dir/x/y.xml", &mut php)).unwrap();
    assert_eq!(n, -1);
}
