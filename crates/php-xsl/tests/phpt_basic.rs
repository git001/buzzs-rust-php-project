// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of the transformation phpts of `ext/xsl/tests` (output compared
//! with the original `--EXPECT--` sections).

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::doc_markdown, clippy::format_collect, clippy::similar_names)]

mod common;

use common::*;
use php_libxml::tree::Document;

/// `prepare.inc`.
fn prepare() -> (Document, Document, Proc, Php) {
    setup(&fixtures());
    let dom = load(&fixture("xslt.xml"));
    let xsl = load(&fixture("xslt.xsl"));
    (dom, xsl, Proc::new(), Php::new())
}

#[test]
fn xslt001() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 1: Transform To XML String");
    import(&proc, &xsl);
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt001.phpt", &take_output());
}

#[test]
fn xslt002() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 2: Transform To HTML String");
    set_output_attr(&xsl, "method", "html");
    import(&proc, &xsl);
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt002.phpt", &take_output());
}

#[test]
fn xslt003() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 3: Using Parameters");
    import(&proc, &xsl);
    proc.set_parameter(b"", b"foo", Some(b"hello world"))
        .unwrap();
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt003.phpt", &take_output());
}

#[test]
fn xslt004() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 4: Checking UTF8 Output");
    set_output_attr(&xsl, "encoding", "utf-8");
    import(&proc, &xsl);
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt004.phpt", &take_output());
}

#[test]
fn xslt005() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 5: Checking Indent");
    set_output_attr(&xsl, "indent", "yes");
    import(&proc, &xsl);
    echo("\n");
    echo(to_xml(&proc, &mut php, &dom).unwrap());
    echo("\n");
    expect("xslt005.phpt", &take_output());
}

#[test]
fn xslt006() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 6: Transform To Doc");
    import(&proc, &xsl);
    echo("\n");
    let doc = to_doc(&proc, &mut php, &dom).unwrap();
    echo(save_xml(&doc));
    echo("\n");
    expect("xslt006.phpt", &take_output());
}

#[test]
fn xslt007() {
    let (dom, xsl, proc, mut php) = prepare();
    echo("Test 7: Transform To Uri");
    import(&proc, &xsl);
    echo("\n");
    let out = scratch().join("xslt007-out.xml");
    let uri = format!("file://{}", out.display());
    let n = in_method("XSLTProcessor::transformToUri", || {
        proc.transform_to_uri(&dom, uri.as_bytes(), &mut php)
            .unwrap()
    });
    assert!(n > 0);
    echo(std::fs::read(&out).unwrap());
    std::fs::remove_file(&out).unwrap();
    echo("\n");
    expect("xslt007.phpt", &take_output());
}
