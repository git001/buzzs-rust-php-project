// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of php-src `ext/xmlwriter/tests/*.phpt`.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::too_many_lines,
    clippy::format_push_string,
    clippy::type_complexity
)]
mod common;

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use common::assert_out;
use php_libxml::io::{self, InputStream, StreamHooks};
use php_libxml::xmlwriter::{FlushOutput, OpenUriError, XmlWriter};

fn mem() -> XmlWriter {
    XmlWriter::memory().expect("memory writer")
}

fn bytes(w: &XmlWriter) -> Vec<u8> {
    match w.flush(true) {
        FlushOutput::Bytes(b) => b,
        FlushOutput::Written(n) => panic!("expected bytes, got count {n}"),
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("php-libxml-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn t001_open_uri_file() {
    let dir = tmpdir("001");
    let w = XmlWriter::open_uri(b"001.xml", &dir).expect("open");
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"tag1").unwrap();
    w.end_document();
    assert!(matches!(w.flush(true), FlushOutput::Written(_)));
    let path = dir.join("001.xml");
    assert_out(
        &std::fs::read(&path).unwrap(),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tag1/>\n",
    );
    drop(w);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn t002_memory() {
    let w = mem();
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"tag1").unwrap();
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tag1/>\n",
    );
}

#[test]
fn t003_attributes_and_text() {
    let w = mem();
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"tag1").unwrap();
    assert!(w.start_attribute(b"attr1").unwrap());
    w.text(b"attr1_value");
    w.end_attribute();
    w.write_attribute(b"att2", b"att2_value").unwrap();
    w.text(b"Test text for tag1");
    assert!(w.start_element(b"tag2").unwrap());
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tag1 attr1=\"attr1_value\" att2=\"att2_value\">Test text for tag1<tag2/></tag1>\n",
    );
}

#[test]
fn t004_005_pi_and_comments_to_file() {
    let dir = tmpdir("004");
    let w = XmlWriter::open_uri(b"004.xml", &dir).expect("open");
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"tag1").unwrap();
    w.start_pi(b"PHP").unwrap();
    w.text(b"echo $a;");
    w.end_pi();
    w.end_document();
    w.flush(true);
    assert_out(
        &std::fs::read(dir.join("004.xml")).unwrap(),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tag1><?PHP echo $a;?></tag1>\n",
    );
    // 005-mb.phpt: multibyte file name.
    let name = "私はガラスを食べられます005.xml";
    let w = XmlWriter::open_uri(name.as_bytes(), &dir).expect("open");
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"tag1").unwrap();
    w.start_comment();
    w.text(b"comment");
    w.end_comment();
    w.write_comment(b"comment #2");
    w.end_document();
    w.flush(true);
    assert_out(
        &std::fs::read(dir.join(name)).unwrap(),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tag1><!--comment--><!--comment #2--></tag1>\n",
    );
    drop(w);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn t006_dtd_and_ns_element() {
    let w = mem();
    w.start_dtd(b"foo", None, Some(b"urn:bar"));
    w.end_dtd();
    w.start_element(b"foo").unwrap();
    w.write_element_ns(
        Some(b"foo"),
        b"bar",
        Some(b"urn:foo"),
        Some(b"dummy content"),
    )
    .unwrap();
    w.end_element();
    assert_out(
        &bytes(&w),
        b"<!DOCTYPE foo SYSTEM \"urn:bar\"><foo><foo:bar xmlns:foo=\"urn:foo\">dummy content</foo:bar></foo>",
    );
}

#[test]
fn t007_indent_and_escaping() {
    let w = mem();
    w.set_indent(true);
    w.set_indent_string(b"   ");
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"root").unwrap();
    w.start_element_ns(Some(b"ns1"), b"child1", Some(b"urn:ns1"))
        .unwrap();
    w.start_attribute_ns(Some(b"ns1"), b"att1", Some(b"urn:ns1"))
        .unwrap();
    w.text(b"a&b");
    w.end_attribute();
    w.write_attribute(b"att2", b"double\" single'").unwrap();
    w.start_attribute_ns(Some(b"ns1"), b"att2", Some(b"urn:ns1"))
        .unwrap();
    w.text(b"<>\"'&");
    w.end_attribute();
    w.write_element(b"chars", Some(b"special characters: <>\"'&"))
        .unwrap();
    w.end_element();
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root>\n   <ns1:child1 ns1:att1=\"a&amp;b\" att2=\"double&quot; single'\" ns1:att2=\"&lt;&gt;&quot;'&amp;\" xmlns:ns1=\"urn:ns1\">\n      <chars>special characters: &lt;&gt;&quot;'&amp;</chars>\n   </ns1:child1>\n</root>\n",
    );
}

#[test]
fn t008_dtd_declarations() {
    let w = mem();
    w.set_indent(true);
    w.start_document(None, Some(b"UTF-8"), None);
    w.start_dtd_entity(b"ent", false).unwrap();
    w.text(b"val");
    w.end_dtd_entity();
    w.write_dtd_entity(b"ent2", b"val2", false, None, None, None)
        .unwrap();
    w.write_dtd_element(b"sxe", b"(elem1+, elem11, elem22*)")
        .unwrap();
    w.write_dtd_attlist(b"sxe", b"id     CDATA  #implied")
        .unwrap();
    w.start_dtd_element(b"elem1").unwrap();
    w.text(b"elem2*");
    w.end_dtd_element();
    w.start_dtd_attlist(b"elem1").unwrap();
    w.text(b"attr1  CDATA  #required\n");
    w.text(b"attr2  CDATA  #implied");
    w.end_dtd_attlist();
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!ENTITY ent \"val\">\n<!ENTITY ent2 \"val2\">\n<!ELEMENT sxe (elem1+, elem11, elem22*)>\n<!ATTLIST sxe id     CDATA  #implied>\n<!ELEMENT elem1 elem2*>\n<!ATTLIST elem1 attr1  CDATA  #required\nattr2  CDATA  #implied>\n",
    );
}

#[test]
fn t009_pi_cdata_indent() {
    let w = mem();
    w.set_indent(true);
    w.start_document(None, Some(b"UTF-8"), None);
    w.start_element(b"root").unwrap();
    w.write_attribute(b"id", b"elem1").unwrap();
    w.start_element(b"elem1").unwrap();
    w.write_attribute(b"attr1", b"first").unwrap();
    w.write_comment(b"start PI");
    w.start_element(b"pi").unwrap();
    w.write_pi(b"php", b"echo \"hello world\"; ").unwrap();
    w.end_element();
    w.start_element(b"cdata").unwrap();
    w.start_cdata();
    w.text(b"<>&\"");
    w.end_cdata();
    w.end_element();
    w.end_element();
    w.end_element();
    w.end_document();
    let out = String::from_utf8(bytes(&w)).unwrap();
    // %w after the PI: libxml >= 2.6.24 adds a newline.
    let expected_prefix = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root id=\"elem1\">\n <elem1 attr1=\"first\">\n  <!--start PI-->\n  <pi><?php echo \"hello world\"; ?>";
    let expected_suffix = "</pi>\n  <cdata><![CDATA[<>&\"]]></cdata>\n </elem1>\n</root>\n";
    assert!(
        out.starts_with(expected_prefix) && out.ends_with(expected_suffix),
        "{out}"
    );
    let middle = &out[expected_prefix.len()..out.len() - expected_suffix.len()];
    assert!(middle.chars().all(char::is_whitespace), "{middle:?}");
}

#[test]
fn t010_invalid_attribute_names() {
    let dir = tmpdir("010");
    let w = XmlWriter::open_uri(b"010.tmp", &dir).expect("open");
    let mut out = String::new();
    out += &format!("{}\n", w.start_element(b"tag").unwrap());
    out += &format!("{}\n", w.start_attribute(b"attr").unwrap());
    out += &format!("{}\n", w.end_attribute());
    for bad in [&b"-1"[..], b"\""] {
        match w.start_attribute(bad) {
            Err(e) => out += &format!("ValueError: xmlwriter_start_attribute(): {e}\n"),
            Ok(_) => out += "no error\n",
        }
        out += &format!("{}\n", w.end_attribute());
    }
    out += &format!("{}\n", w.end_element());
    w.flush(true);
    drop(w);
    out += &format!(
        "{:?}\n",
        String::from_utf8(std::fs::read(dir.join("010.tmp")).unwrap()).unwrap()
    );
    assert_eq!(
        out,
        "true\ntrue\ntrue\nValueError: xmlwriter_start_attribute(): Argument #2 ($name) must be a valid attribute name, \"-1\" given\nfalse\nValueError: xmlwriter_start_attribute(): Argument #2 ($name) must be a valid attribute name, \"\"\" given\nfalse\ntrue\n\"<tag attr=\\\"\\\"/>\"\n"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn t012_full_end_element() {
    let w = mem();
    w.set_indent(true);
    w.set_indent_string(b"   ");
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"root").unwrap();
    w.start_element_ns(Some(b"ns1"), b"child1", Some(b"urn:ns1"))
        .unwrap();
    w.write_attribute_ns(Some(b"ns1"), b"att1", Some(b"urn:ns1"), b"<>\"'&")
        .unwrap();
    w.write_element(b"chars", Some(b"special characters: <>\"'&"))
        .unwrap();
    w.end_element();
    w.start_element(b"empty").unwrap();
    w.full_end_element();
    w.full_end_element();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root>\n   <ns1:child1 ns1:att1=\"&lt;&gt;&quot;'&amp;\" xmlns:ns1=\"urn:ns1\">\n      <chars>special characters: &lt;&gt;&quot;'&amp;</chars>\n   </ns1:child1>\n   <empty></empty>\n</root>\n",
    );
}

#[test]
fn bug39504_dtd_entities() {
    let w = mem();
    w.start_document(None, Some(b"UTF-8"), None);
    w.start_dtd(b"root", None, None);
    w.write_dtd_entity(b"ent2", b"val2", false, None, None, None)
        .unwrap();
    w.end_dtd();
    w.start_element(b"root").unwrap();
    w.end_document();
    let mut out = bytes(&w);
    out.push(b'\n');
    let w = mem();
    w.start_document(None, Some(b"UTF-8"), None);
    w.start_dtd(b"root", None, None);
    w.write_dtd_entity(
        b"c",
        b"",
        false,
        Some(b"-//W3C//TEXT copyright//EN"),
        Some(b"http://www.w3.org/xmlspec/copyright.xml"),
        None,
    )
    .unwrap();
    w.end_dtd();
    w.start_element(b"root").unwrap();
    w.end_document();
    out.extend(bytes(&w));
    assert_out(
        &out,
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE root [<!ENTITY ent2 \"val2\">]><root/>\n\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE root [<!ENTITY c PUBLIC \"-//W3C//TEXT copyright//EN\" \"http://www.w3.org/xmlspec/copyright.xml\">]><root/>\n",
    );
}

#[test]
fn bug41287_bug41326_ns_elements() {
    let w = mem();
    w.set_indent(true);
    w.start_document(None, None, None);
    w.start_element_ns(Some(b"test"), b"test", Some(b"urn:x-test:"))
        .unwrap();
    w.write_element_ns(Some(b"test"), b"foo", None, Some(b""))
        .unwrap();
    w.write_element_ns(None, b"bar", Some(b"urn:x-test:"), Some(b""))
        .unwrap();
    w.write_element_ns(None, b"bar", Some(b"urn:x-test:"), None)
        .unwrap();
    w.write_element_ns(None, b"bar", Some(b""), Some(b""))
        .unwrap();
    w.end_element();
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\"?>\n<test:test xmlns:test=\"urn:x-test:\">\n <test:foo></test:foo>\n <bar xmlns=\"urn:x-test:\"></bar>\n <bar xmlns=\"urn:x-test:\"/>\n <bar xmlns=\"\"></bar>\n</test:test>\n",
    );
    let w = mem();
    w.set_indent(true);
    w.start_document(None, None, None);
    w.start_element(b"test").unwrap();
    w.write_element(b"foo", None).unwrap();
    w.write_element(b"foo2", Some(b"")).unwrap();
    w.write_element(b"foo3", None).unwrap();
    w.start_element(b"bar").unwrap();
    w.end_element();
    w.end_element();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\"?>\n<test>\n <foo/>\n <foo2></foo2>\n <foo3/>\n <bar/>\n</test>\n",
    );
}

#[test]
fn bug79344_null_ns_attributes() {
    let w = mem();
    w.set_indent(true);
    w.start_element(b"foo").unwrap();
    w.write_attribute_ns(None, b"test1", None, b"test1")
        .unwrap();
    w.start_attribute_ns(None, b"test2", None).unwrap();
    w.text(b"test2");
    w.end_attribute();
    w.end_element();
    assert_out(
        &w.output_memory(true),
        b"<foo test1=\"test1\" test2=\"test2\"/>\n",
    );
}

#[test]
fn write_dtd_basic() {
    let w = mem();
    assert!(w.write_dtd(b"bla1", Some(b"bla2"), Some(b"bla3"), Some(b"bla4")));
    assert_out(
        &bytes(&w),
        b"<!DOCTYPE bla1 PUBLIC \"bla2\" \"bla3\" [bla4]>",
    );
    assert!(!w.write_dtd(b"", Some(b""), Some(b""), None));
    assert_out(&bytes(&w), b"");
}

#[test]
fn write_attribute_ns_basic() {
    let w = mem();
    w.set_indent(true);
    w.start_document(None, Some(b"UTF-8"), None);
    w.start_element(b"root").unwrap();
    w.write_attribute_ns(
        Some(b"prefix"),
        b"id",
        Some(b"http://www.php.net/uri"),
        b"elem1",
    )
    .unwrap();
    w.start_element(b"elem1").unwrap();
    w.write_attribute(b"attr1", b"first").unwrap();
    w.end_element();
    w.full_end_element();
    w.end_document();
    assert_out(
        &bytes(&w),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root prefix:id=\"elem1\" xmlns:prefix=\"http://www.php.net/uri\">\n <elem1 attr1=\"first\"/>\n</root>\n",
    );
}

#[test]
fn oo_012_013_cdata_and_raw() {
    let s = b"Text for inclusion within CData tags can include characters like <, >, &, and quotes like ' and \"";
    let w = mem();
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"myDocumentRoot").unwrap();
    w.start_element(b"myElement").unwrap();
    w.start_element(b"cdataElement").unwrap();
    w.write_cdata(s);
    w.end_element();
    w.end_element();
    w.end_element();
    let first = w.output_memory(true);
    let w = mem();
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"myDocumentRoot").unwrap();
    w.start_element(b"myElement").unwrap();
    let mut raw = b"<cdataElement><![CDATA[".to_vec();
    raw.extend_from_slice(s);
    raw.extend_from_slice(b"]]></cdataElement>");
    w.write_raw(&raw);
    w.end_element();
    w.end_element();
    let second = w.output_memory(true);
    let expected = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<myDocumentRoot><myElement><cdataElement><![CDATA[Text for inclusion within CData tags can include characters like <, >, &, and quotes like ' and \"]]></cdataElement></myElement></myDocumentRoot>";
    assert_out(&first, expected);
    assert_out(&second, expected);
}

#[test]
fn to_memory_flush_combinations() {
    let w = mem();
    assert_eq!(w.flush(false), FlushOutput::Bytes(Vec::new()));
    w.start_element(b"foo").unwrap();
    assert_eq!(w.flush(false), FlushOutput::Bytes(b"<foo".to_vec()));
    w.end_element();
    assert_eq!(w.flush(false), FlushOutput::Bytes(b"<foo/>".to_vec()));
    assert_eq!(w.flush(true), FlushOutput::Bytes(b"<foo/>".to_vec()));
    assert_eq!(w.flush(true), FlushOutput::Bytes(Vec::new()));
}

#[derive(Clone, Default)]
struct Shared(Rc<RefCell<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn to_stream_normal_usage_and_encodings() {
    let out = Shared::default();
    let w = XmlWriter::to_sink(Box::new(out.clone())).expect("writer");
    w.start_element(b"root").unwrap();
    w.write_attribute(b"align", b"left").unwrap();
    w.write_comment(b"hello");
    w.end_element();
    assert_eq!(w.flush(true), FlushOutput::Written(38));
    assert!(w.output_memory(true).is_empty());
    drop(w);
    assert_out(&out.0.borrow(), b"<root align=\"left\"><!--hello--></root>");

    let out = Shared::default();
    let w = XmlWriter::to_sink(Box::new(out.clone())).expect("writer");
    w.start_document(None, Some(b"UTF-8"), None);
    w.write_comment("ééé".as_bytes());
    drop(w);
    assert_out(
        &out.0.borrow(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!--ééé-->".as_bytes(),
    );

    let out = Shared::default();
    let w = XmlWriter::to_sink(Box::new(out.clone())).expect("writer");
    w.start_document(None, Some(b"SHIFT_JIS"), None);
    w.write_comment("\u{3041}\u{3041}\u{3041}".as_bytes());
    drop(w);
    assert_out(
        &out.0.borrow(),
        b"<?xml version=\"1.0\" encoding=\"SHIFT_JIS\"?>\n<!--\x82\x9f\x82\x9f\x82\x9f-->",
    );
}

#[test]
fn open_uri_errors() {
    let cwd = std::env::temp_dir();
    assert_eq!(
        XmlWriter::open_uri(b"", &cwd).unwrap_err(),
        OpenUriError::Empty
    );
    assert_eq!(
        XmlWriter::open_uri(b"foo/bar.tmp", &cwd).unwrap_err(),
        OpenUriError::Unresolvable
    );
    assert_eq!(
        XmlWriter::open_uri(b"file:///", &cwd).unwrap_err(),
        OpenUriError::Unresolvable
    );
    assert_eq!(
        XmlWriter::open_uri(b"file://localhost/", &cwd).unwrap_err(),
        OpenUriError::Unresolvable
    );
    let long = format!("file:///{}", "a".repeat(4097));
    assert_eq!(
        XmlWriter::open_uri(long.as_bytes(), &cwd).unwrap_err(),
        OpenUriError::Unresolvable
    );
}

#[test]
fn open_uri_through_stream_hooks() {
    // bug71536 / openUri('php://memory'): non-file URIs go to the hooks.
    let captured: Rc<RefCell<Vec<(Vec<u8>, Shared)>>> = Rc::default();
    let sink = captured.clone();
    io::set_stream_hooks(Some(StreamHooks {
        open_read: Some(Box::new(|_p: &[u8]| -> Option<InputStream> { None })),
        open_write: Some(Box::new(move |p: &[u8]| -> Option<Box<dyn Write>> {
            let s = Shared::default();
            sink.borrow_mut().push((p.to_vec(), s.clone()));
            Some(Box::new(s))
        })),
    }));
    let w = XmlWriter::open_uri(b"php://memory", &std::env::temp_dir()).expect("open");
    w.set_indent(false);
    w.start_document(Some(b"1.0"), Some(b"UTF-8"), None);
    w.start_element(b"response").unwrap();
    drop(w);
    io::set_stream_hooks(None);
    let captured = captured.borrow();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, b"php://memory");
    assert_out(
        &captured[0].1 .0.borrow(),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<response",
    );
}

#[test]
fn set_indent_string_on_file_writer() {
    let dir = tmpdir("indent");
    std::fs::write(dir.join("x.tmp"), b"Hi").unwrap();
    let w = XmlWriter::open_uri(b"x.tmp", &dir).expect("open");
    assert!(w.set_indent_string(b"  "));
    drop(w);
    std::fs::remove_dir_all(dir).unwrap();
}
