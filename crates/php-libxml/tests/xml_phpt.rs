// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of php-src `ext/xml/tests/*.phpt` (libxml flavour). Each test
//! reproduces the PHP script's logic with a recording handler and compares
//! the printed output byte for byte with the `--EXPECT--` section.

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::many_single_char_names, clippy::too_many_lines)]
mod common;

use common::{assert_out, index, list, opt, pairs, print_r, s, values, var_dump, PVal};
use php_libxml::xml::{
    error_code, error_string, HandlerKind, NoHandlers, OptionValue, StructBuilder, XmlError,
    XmlHandlers, XmlOption, XmlParser,
};

type Pos = (i64, i64, i64);

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Start(Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>, Pos),
    End(Vec<u8>, Pos),
    Cdata(Vec<u8>),
    Pi(Option<Vec<u8>>, Option<Vec<u8>>),
    Default(Vec<u8>),
    Unparsed([Option<Vec<u8>>; 5]),
    Notation([Option<Vec<u8>>; 4]),
    ExtRef([Option<Vec<u8>>; 4]),
    StartNs(Option<Vec<u8>>, Option<Vec<u8>>),
    Warning(String),
}

#[derive(Default)]
struct Rec<'p> {
    events: Vec<Ev>,
    parser: Option<&'p XmlParser>,
    ext_ret: i64,
    /// Simulated exception: thrown by the named handler.
    throw_in: Option<&'static str>,
    thrown: bool,
}

impl Rec<'_> {
    fn pos(&self) -> Pos {
        self.parser.map_or((0, 0, 0), |p| {
            (
                p.current_line_number(),
                p.current_column_number(),
                p.current_byte_index(),
            )
        })
    }
    fn maybe_throw(&mut self, which: &str) {
        if self.throw_in == Some(which) {
            self.thrown = true;
        }
    }
}

fn o(x: Option<&[u8]>) -> Option<Vec<u8>> {
    x.map(<[u8]>::to_vec)
}

impl XmlHandlers for Rec<'_> {
    fn aborted(&self) -> bool {
        self.thrown
    }
    fn start_element(&mut self, name: &[u8], attributes: &[(Vec<u8>, Vec<u8>)]) {
        let pos = self.pos();
        self.events
            .push(Ev::Start(name.to_vec(), attributes.to_vec(), pos));
        self.maybe_throw("start");
    }
    fn end_element(&mut self, name: &[u8]) {
        let pos = self.pos();
        self.events.push(Ev::End(name.to_vec(), pos));
        self.maybe_throw("end");
    }
    fn character_data(&mut self, data: &[u8]) {
        self.events.push(Ev::Cdata(data.to_vec()));
        self.maybe_throw("cdata");
    }
    fn processing_instruction(&mut self, target: Option<&[u8]>, data: Option<&[u8]>) {
        self.events.push(Ev::Pi(o(target), o(data)));
    }
    fn default_handler(&mut self, data: &[u8]) {
        self.events.push(Ev::Default(data.to_vec()));
    }
    fn unparsed_entity_decl(
        &mut self,
        a: Option<&[u8]>,
        b: Option<&[u8]>,
        c: Option<&[u8]>,
        d: Option<&[u8]>,
        e: Option<&[u8]>,
    ) {
        self.events
            .push(Ev::Unparsed([o(a), o(b), o(c), o(d), o(e)]));
    }
    fn notation_decl(
        &mut self,
        a: Option<&[u8]>,
        b: Option<&[u8]>,
        c: Option<&[u8]>,
        d: Option<&[u8]>,
    ) {
        self.events.push(Ev::Notation([o(a), o(b), o(c), o(d)]));
    }
    fn external_entity_ref(
        &mut self,
        a: Option<&[u8]>,
        b: Option<&[u8]>,
        c: Option<&[u8]>,
        d: Option<&[u8]>,
    ) -> i64 {
        self.events.push(Ev::ExtRef([o(a), o(b), o(c), o(d)]));
        self.ext_ret
    }
    fn start_namespace_decl(&mut self, prefix: Option<&[u8]>, uri: Option<&[u8]>) {
        self.events.push(Ev::StartNs(o(prefix), o(uri)));
    }
    fn warning(&mut self, message: &str) {
        self.events.push(Ev::Warning(message.to_owned()));
    }
}

fn set_elements(p: &XmlParser, start: bool, end: bool) {
    p.set_handler(HandlerKind::StartElement, start).unwrap();
    p.set_handler(HandlerKind::EndElement, end).unwrap();
}

fn php_str(v: Option<&[u8]>) -> &[u8] {
    v.unwrap_or_default()
}

#[test]
fn xml004_case_folding() {
    let data = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/xmltest.xml"
    ))
    .unwrap();
    let mut out = Vec::new();
    for fold in [false, true] {
        let p = XmlParser::new(None).unwrap();
        p.set_case_folding(fold);
        set_elements(&p, true, true);
        let mut rec = Rec::default();
        // fread($fp, 4096) returns the whole file; feof() is false on the
        // first read, so the final chunk is never flagged final.
        p.parse(&data, false, &mut rec, None).unwrap();
        for ev in rec.events {
            match ev {
                Ev::Start(n, a, _) => {
                    out.push(b'<');
                    out.extend_from_slice(&n);
                    for (k, v) in a {
                        out.extend_from_slice(
                            format!(
                                " {}=\"{}\"",
                                String::from_utf8_lossy(&k),
                                String::from_utf8_lossy(&v)
                            )
                            .as_bytes(),
                        );
                    }
                    out.extend_from_slice(b">\n");
                }
                Ev::End(n, _) => {
                    out.extend_from_slice(b"</");
                    out.extend_from_slice(&n);
                    out.extend_from_slice(b">\n");
                }
                _ => {}
            }
        }
    }
    assert_out(
        &out,
        b"<root id=\"elem1\">\n<elem1>\n<elem2>\n<elem3>\n<elem4>\n</elem4>\n</elem3>\n</elem2>\n</elem1>\n</root>\n\
<ROOT ID=\"elem1\">\n<ELEM1>\n<ELEM2>\n<ELEM3>\n<ELEM4>\n</ELEM4>\n</ELEM3>\n</ELEM2>\n</ELEM1>\n</ROOT>\n",
    );
}

#[test]
fn xml007_iso_8859_1_target() {
    let xml = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><\xe4\xf6\xfc \xfc\xe4\xdf=\"\xc4\xd6\xdc\">\xc4\xdc\xd6</\xe4\xf6\xfc>";
    let p = XmlParser::new(Some(b"ISO-8859-1")).unwrap();
    set_elements(&p, true, true);
    let mut rec = Rec::default();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(xml, &mut rec, &mut b).unwrap();
    // Case folding is ASCII only (PHP 8 strtoupper).
    assert_eq!(
        rec.events[0],
        Ev::Start(
            b"\xe4\xf6\xfc".to_vec(),
            vec![(b"\xfc\xe4\xdf".to_vec(), b"\xc4\xd6\xdc".to_vec())],
            (0, 0, 0)
        )
    );
    assert_out(
        &var_dump(&values(&b)),
        b"array(1) {\n  [0]=>\n  array(5) {\n    [\"tag\"]=>\n    string(3) \"\xe4\xf6\xfc\"\n    [\"type\"]=>\n    string(8) \"complete\"\n    [\"level\"]=>\n    int(1)\n    [\"attributes\"]=>\n    array(1) {\n      [\"\xfc\xe4\xdf\"]=>\n      string(3) \"\xc4\xd6\xdc\"\n    }\n    [\"value\"]=>\n    string(3) \"\xc4\xdc\xd6\"\n  }\n}\n",
    );
}

fn ns_names(xml: &str, sep: &[u8]) -> Vec<u8> {
    let p = XmlParser::new_ns(Some(b"ISO-8859-1"), sep).unwrap();
    set_elements(&p, true, true);
    p.set_case_folding(false);
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), false, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in rec.events {
        if let Ev::Start(n, _, _) = ev {
            out.extend(var_dump(&s(&n)));
        }
    }
    out
}

#[test]
fn xml009_default_namespaces() {
    let xml = "<a xmlns=\"http://example.com/foo\"\n       xmlns:bar=\"http://example.com/bar\"\n       xmlns:baz=\"http://example.com/baz\">\n  <bar:b />\n  <bar:c xmlns:bar=\"http://example.com/foo\"/>\n</a>";
    assert_out(
        &ns_names(xml, b"@"),
        b"string(24) \"http://example.com/foo@a\"\nstring(24) \"http://example.com/bar@b\"\nstring(24) \"http://example.com/foo@c\"\n",
    );
}

#[test]
fn bug25666_namespaces() {
    let xml = "<foo:a xmlns:foo=\"http://example.com/foo\"\n       xmlns:bar=\"http://example.com/bar\"\n       xmlns:baz=\"http://example.com/baz\">\n  <bar:b />\n  <baz:c />\n</foo>";
    assert_out(
        &ns_names(xml, b"@"),
        b"string(24) \"http://example.com/foo@a\"\nstring(24) \"http://example.com/bar@b\"\nstring(24) \"http://example.com/baz@c\"\n",
    );
}

#[test]
fn xml010_attributes() {
    let xml = "<a xmlns=\"http://example.com/foo\"\n    xmlns:bar=\"http://example.com/bar\">\n  <bar:b foo=\"bar\"/>\n  <bar:c bar:nix=\"null\" foo=\"bar\"/>\n</a>";
    let p = XmlParser::new_ns(Some(b"ISO-8859-1"), b"@").unwrap();
    set_elements(&p, true, true);
    p.set_case_folding(false);
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), false, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in rec.events {
        if let Ev::Start(n, attrs, _) = ev {
            out.extend_from_slice(&n);
            out.push(b' ');
            for (k, v) in attrs {
                out.extend_from_slice(&k);
                out.extend_from_slice(b" = ");
                out.extend_from_slice(&v);
                out.push(b' ');
            }
            out.push(b'\n');
        }
    }
    assert_out(
        &out,
        b"http://example.com/foo@a \nhttp://example.com/bar@b foo = bar \nhttp://example.com/bar@c http://example.com/bar@nix = null foo = bar \n",
    );
}

#[test]
fn xml011_concat_cdata_and_empty_handlers() {
    let xml = b"<text>start<b /> This &amp; that</text>";
    let p = XmlParser::new(None).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(xml, &mut NoHandlers, &mut b).unwrap();
    assert_out(
        &print_r(&values(&b)),
        b"Array\n(\n    [0] => Array\n        (\n            [tag] => TEXT\n            [type] => open\n            [level] => 1\n            [value] => start\n        )\n\n    [1] => Array\n        (\n            [tag] => B\n            [type] => complete\n            [level] => 2\n        )\n\n    [2] => Array\n        (\n            [tag] => TEXT\n            [value] =>  This & that\n            [type] => cdata\n            [level] => 1\n        )\n\n    [3] => Array\n        (\n            [tag] => TEXT\n            [type] => close\n            [level] => 1\n        )\n\n)\n",
    );
    let p = XmlParser::new(None).unwrap();
    p.set_case_folding(false);
    set_elements(&p, true, true);
    set_elements(&p, true, false);
    let mut rec = Rec::default();
    p.parse(xml, true, &mut rec, None).unwrap();
    let names: Vec<_> = rec
        .events
        .iter()
        .filter_map(|e| match e {
            Ev::Start(n, _, _) => Some(n.clone()),
            Ev::End(..) => panic!("end handler unset"),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec![b"text".to_vec(), b"b".to_vec()]);
}

#[test]
fn bug26528_entities_in_attributes() {
    let p = XmlParser::new(None).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(
        b"<?xml version=\"1.0\"?><test attr=\"angle&lt;bracket\"/>",
        &mut NoHandlers,
        &mut b,
    )
    .unwrap();
    assert_out(
        &var_dump(&values(&b)),
        b"array(1) {\n  [0]=>\n  array(4) {\n    [\"tag\"]=>\n    string(4) \"TEST\"\n    [\"type\"]=>\n    string(8) \"complete\"\n    [\"level\"]=>\n    int(1)\n    [\"attributes\"]=>\n    array(1) {\n      [\"ATTR\"]=>\n      string(13) \"angle<bracket\"\n    }\n  }\n}\n",
    );
}

const BUG26614: [(&str, &str); 3] = [
    ("CDATA", "<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n<data>\n<![CDATA[\nmulti\nline\nCDATA\nblock\n]]>\n</data>"),
    ("Comment", "<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n<data>\n<!-- ATA[\nmulti\nline\nCDATA\nblock\n-->\n</data>"),
    ("Text", "<?xml version=\"1.0\" encoding=\"iso-8859-1\" ?>\n<data>\n-!-- ATA[\nmulti\nline\nCDATA\nblock\n---\n</data>"),
];

#[test]
fn bug26614_positions() {
    // bug26614_libxml_pre2_11.phpt (libxml2 < 2.11): columns are %d.
    for (desc, xml) in BUG26614 {
        let p = XmlParser::new(None).unwrap();
        set_elements(&p, true, true);
        p.set_handler(HandlerKind::CharacterData, true).unwrap();
        let mut rec = Rec {
            parser: Some(&p),
            ..Rec::default()
        };
        assert_eq!(
            p.parse(xml.as_bytes(), true, &mut rec, None),
            Ok(1),
            "{desc}"
        );
        let pos: Vec<_> = rec
            .events
            .iter()
            .filter_map(|e| match e {
                Ev::Start(_, _, (l, _, b)) | Ev::End(_, (l, _, b)) => Some((*l, *b)),
                _ => None,
            })
            .collect();
        assert_eq!(pos, vec![(2, 9), (9, 55)], "{desc}");
    }
}

#[test]
fn bug30875_entities_resolved() {
    let xml = "<!DOCTYPE dtd [\n    <!ENTITY ref \"ent\">\n]>\n<elt att=\"&ref;\">a&ref;</elt>";
    let p = XmlParser::new(None).unwrap();
    let mut b = StructBuilder::new(false);
    p.parse_into_struct(xml.as_bytes(), &mut NoHandlers, &mut b)
        .unwrap();
    assert_out(
        &var_dump(&values(&b)),
        b"array(1) {\n  [0]=>\n  array(5) {\n    [\"tag\"]=>\n    string(3) \"ELT\"\n    [\"type\"]=>\n    string(8) \"complete\"\n    [\"level\"]=>\n    int(1)\n    [\"attributes\"]=>\n    array(1) {\n      [\"ATT\"]=>\n      string(3) \"ent\"\n    }\n    [\"value\"]=>\n    string(4) \"aent\"\n  }\n}\n",
    );
}

#[test]
fn bug35447_bom_and_defaulted_attributes() {
    let data = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE bundle [\n    <!ELEMENT bundle (resource)+>\n    <!ELEMENT resource (#PCDATA)>\n    <!ATTLIST resource\n              key CDATA #REQUIRED\n              type (literal|pattern|sub) \"literal\"\n              >\n]>\n<resource key=\"rSeeYou\">A bient&amp;244;t</resource>";
    let p = XmlParser::new_ns(Some(b"UTF-8"), b":").unwrap();
    p.set_case_folding(false);
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(data.as_bytes(), &mut NoHandlers, &mut b)
        .unwrap();
    assert_out(
        &var_dump(&values(&b)),
        b"array(1) {\n  [0]=>\n  array(5) {\n    [\"tag\"]=>\n    string(8) \"resource\"\n    [\"type\"]=>\n    string(8) \"complete\"\n    [\"level\"]=>\n    int(1)\n    [\"attributes\"]=>\n    array(2) {\n      [\"key\"]=>\n      string(7) \"rSeeYou\"\n      [\"type\"]=>\n      string(7) \"literal\"\n    }\n    [\"value\"]=>\n    string(13) \"A bient&244;t\"\n  }\n}\n",
    );
}

#[test]
fn bug46699_default_handler_ns() {
    let xml = "<a xmlns=\"http://example.com/foo\"\n    xmlns:bar=\"http://example.com/bar\">\n  <bar:b foo=\"bar\">1</bar:b>\n  <bar:c bar:nix=\"null\" foo=\"bar\">2</bar:c>\n</a>";
    let p = XmlParser::new_ns(Some(b"ISO-8859-1"), b"@").unwrap();
    p.set_handler(HandlerKind::Default, true).unwrap();
    p.set_case_folding(false);
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), false, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in rec.events {
        if let Ev::Default(d) = ev {
            out.extend(d);
        }
    }
    // The chunk is not final, so the closing </a> is still buffered.
    assert_out(
        &out,
        b"<a xmlns=\"http://example.com/foo\"\n    xmlns:bar=\"http://example.com/bar\">\n  <bar:b foo=\"bar\">1</bar:b>\n  <bar:c bar:nix=\"null\" foo=\"bar\">2</bar:c>\n</a>",
    );
}

#[test]
fn bug50576_skip_tagstart() {
    let xml = "<?xml version=\"1.0\"?>\n<ns1:listOfAwards xmlns:ns1=\"http://www.fpdsng.com/FPDS\">\n<ns1:count>\n<ns1:total>867</ns1:total>\n</ns1:count>\n</ns1:listOfAwards>";
    let p = XmlParser::new(None).unwrap();
    p.set_skip_tagstart(4).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(xml.as_bytes(), &mut NoHandlers, &mut b)
        .unwrap();
    let mut out = b"Index array\n".to_vec();
    out.extend(print_r(&index(&b)));
    out.extend_from_slice(b"Vals array\n");
    out.extend(print_r(&values(&b)));
    let p = XmlParser::new(None).unwrap();
    set_elements(&p, true, true);
    p.set_skip_tagstart(4).unwrap();
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), false, &mut rec, None).unwrap();
    for ev in rec.events {
        match ev {
            Ev::Start(n, _, _) | Ev::End(n, _) => {
                out.extend(n);
                out.push(b'\n');
            }
            _ => {}
        }
    }
    assert_out(
        &out,
        b"Index array\nArray\n(\n    [LISTOFAWARDS] => Array\n        (\n            [0] => 0\n            [1] => 5\n            [2] => 6\n        )\n\n    [COUNT] => Array\n        (\n            [0] => 1\n            [1] => 3\n            [2] => 4\n        )\n\n    [TOTAL] => Array\n        (\n            [0] => 2\n        )\n\n)\nVals array\nArray\n(\n    [0] => Array\n        (\n            [tag] => LISTOFAWARDS\n            [type] => open\n            [level] => 1\n            [attributes] => Array\n                (\n                    [XMLNS:NS1] => http://www.fpdsng.com/FPDS\n                )\n\n            [value] => \n\n        )\n\n    [1] => Array\n        (\n            [tag] => COUNT\n            [type] => open\n            [level] => 2\n            [value] => \n\n        )\n\n    [2] => Array\n        (\n            [tag] => TOTAL\n            [type] => complete\n            [level] => 3\n            [value] => 867\n        )\n\n    [3] => Array\n        (\n            [tag] => COUNT\n            [value] => \n\n            [type] => cdata\n            [level] => 2\n        )\n\n    [4] => Array\n        (\n            [tag] => COUNT\n            [type] => close\n            [level] => 2\n        )\n\n    [5] => Array\n        (\n            [tag] => LISTOFAWARDS\n            [value] => \n\n            [type] => cdata\n            [level] => 1\n        )\n\n    [6] => Array\n        (\n            [tag] => LISTOFAWARDS\n            [type] => close\n            [level] => 1\n        )\n\n)\nLISTOFAWARDS\nCOUNT\nTOTAL\nTOTAL\nCOUNT\nLISTOFAWARDS\n",
    );
}

#[test]
fn bug65236_max_depth_warning() {
    let p = XmlParser::new_ns(None, b":").unwrap();
    let mut rec = Rec::default();
    let mut b = StructBuilder::new(false);
    p.parse_into_struct("<blah>".repeat(1000).as_bytes(), &mut rec, &mut b)
        .unwrap();
    let warnings: Vec<_> = rec
        .events
        .iter()
        .filter(|e| matches!(e, Ev::Warning(_)))
        .collect();
    assert_eq!(
        warnings,
        vec![&Ev::Warning(
            "Maximum depth exceeded - Results truncated".into()
        )]
    );
    assert_eq!(b.values.len(), 255);
}

#[test]
fn bug70962_skip_white() {
    let p = XmlParser::new(None).unwrap();
    p.set_skip_white(true);
    let mut b = StructBuilder::new(false);
    p.parse_into_struct(
        b"<a><b>&lt;d&gt;\n &lt;e&gt;</b><![CDATA[  ]]><c>\n \t</c></a>",
        &mut NoHandlers,
        &mut b,
    )
    .unwrap();
    assert_eq!(b.values[1].get_str("value"), Some(&b"<d>\n <e>"[..]));
    assert!(b.values[2].get("value").is_none());
    assert_eq!(b.values.len(), 4);
}

#[test]
fn bug71592_external_entity_failure_stops() {
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE root [\n  <!ENTITY pic PUBLIC \"image.gif\" \"http://example.org/image.gif\">\n]>\n<root>\n<p>&pic;</p>\n<p></nop>\n</root>";
    let p = XmlParser::new_ns(Some(b"UTF-8"), b":").unwrap();
    p.set_handler(HandlerKind::ExternalEntityRef, true).unwrap();
    let mut rec = Rec {
        ext_ret: 0,
        ..Rec::default()
    };
    p.parse(xml.as_bytes(), false, &mut rec, None).unwrap();
    assert_eq!(p.error_code(), error_code::EXTERNAL_ENTITY_HANDLING);
}

#[test]
fn bug72099_text_only() {
    let p = XmlParser::new_ns(None, b":").unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(b"aaaaaaaaaa", &mut NoHandlers, &mut b)
        .unwrap();
    assert_out(&var_dump(&values(&b)), b"array(0) {\n}\n");
}

#[test]
fn bug72714_tagstart_bounds() {
    let mut out = Vec::new();
    for tagstart in [3_015_809_298_423_721_i64, 20] {
        let p = XmlParser::new(None).unwrap();
        set_elements(&p, true, true);
        if let Err(e) = p.set_skip_tagstart(tagstart) {
            out.extend_from_slice(format!("Warning: xml_parser_set_option(): {e}\n").as_bytes());
        }
        let mut rec = Rec::default();
        p.parse(b"<ns1:total>867</ns1:total>", false, &mut rec, None)
            .unwrap();
        for ev in rec.events {
            if let Ev::Start(n, _, _) = ev {
                out.extend(var_dump(&s(&n)));
            }
        }
    }
    assert_out(
        &out,
        b"Warning: xml_parser_set_option(): Argument #3 ($value) must be between 0 and 2147483647 for option XML_OPTION_SKIP_TAGSTART\nstring(9) \"NS1:TOTAL\"\nstring(0) \"\"\n",
    );
}

#[test]
fn bug81351_partial_then_final() {
    let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n <soap:Body>\n  <X xmlns=\"example.org\">";
    let p = XmlParser::new_ns(Some(b"UTF-8"), b":").unwrap();
    assert_eq!(p.parse(xml.as_bytes(), false, &mut NoHandlers, None), Ok(1));
    assert_eq!(p.error_code(), 0);
    assert_eq!(p.parse(b"Y>", true, &mut NoHandlers, None), Ok(0));
    let code = p.error_code();
    assert!(
        (code == 5 && error_string(code) == "Invalid document end")
            || (code == 77 && error_string(code) == "Tag not finished"),
        "{code}"
    );
}

#[test]
fn bug81481_byte_index_beyond_2gb_style() {
    // Scaled-down variant: byte indexes keep growing across chunks.
    let p = XmlParser::new(Some(b"UTF-8")).unwrap();
    set_elements(&p, true, false);
    let mut rec = Rec {
        parser: Some(&p),
        ..Rec::default()
    };
    p.parse(b"<root><i></i><b/><ext>Hello</ext>", false, &mut rec, None)
        .unwrap();
    let spaces = vec![b' '; 1024 * 1024];
    for _ in 0..3 {
        p.parse(&spaces, false, &mut rec, None).unwrap();
    }
    p.parse(b"<ext></ext><ext></ext></root>", false, &mut rec, None)
        .unwrap();
    let idx: Vec<_> = rec
        .events
        .iter()
        .filter_map(|e| match e {
            Ev::Start(n, _, (_, _, b)) if n == b"EXT" => Some(*b),
            _ => None,
        })
        .collect();
    // Same offsets as the original: 2306867237 = 21 + 2200 MiB + 16.
    assert_eq!(
        idx,
        vec![21, 21 + 3 * 1024 * 1024 + 16, 21 + 3 * 1024 * 1024 + 27]
    );
}

#[test]
fn gh14834_character_data_chunks() {
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE root [\n    <!ENTITY foo \"ent\">\n]>\n<root>\n  <element hint=\"hello&apos;world\">&foo;<![CDATA[ &amp; ]]><?x &amp; ?></element>\n</root>";
    let p = XmlParser::new(None).unwrap();
    p.set_handler(HandlerKind::CharacterData, true).unwrap();
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), true, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in rec.events {
        if let Ev::Cdata(d) = ev {
            out.extend(var_dump(&s(&d)));
        }
    }
    assert_out(
        &out,
        b"string(3) \"\n  \"\nstring(3) \"ent\"\nstring(7) \" &amp; \"\nstring(1) \"\n\"\n",
    );
}

#[test]
fn gh20439_default_handler_chunked_and_self_closing() {
    let input =
        "<!-- xxx --><foo   attr1='\"&lt;&quot;&#9;&#x0A;&#x0D;&#13;𐍅' attr2=\"&quot;&lt;\"></foo>";
    let p = XmlParser::new_ns(Some(b"utf-8"), b":").unwrap();
    p.set_handler(HandlerKind::Default, true).unwrap();
    let mut rec = Rec::default();
    for byte in input.as_bytes() {
        p.parse(std::slice::from_ref(byte), false, &mut rec, None)
            .unwrap();
    }
    p.parse(b"", true, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in &rec.events {
        if let Ev::Default(d) = ev {
            out.extend(var_dump(&s(d)));
        }
    }
    assert_out(
        &out,
        "string(12) \"<!-- xxx -->\"\nstring(71) \"<foo   attr1='\"&lt;&quot;&#9;&#x0A;&#x0D;&#13;𐍅' attr2=\"&quot;&lt;\">\"\nstring(6) \"</foo>\"\n".as_bytes(),
    );

    let p = XmlParser::new_ns(Some(b"utf-8"), b":").unwrap();
    p.set_handler(HandlerKind::Default, true).unwrap();
    let mut rec = Rec::default();
    p.parse(b"<ns:test xmlns:ns='urn:x'   />", true, &mut rec, None)
        .unwrap();
    let mut out = Vec::new();
    for ev in &rec.events {
        if let Ev::Default(d) = ev {
            out.extend(var_dump(&s(d)));
        }
    }
    assert_out(
        &out,
        b"string(29) \"<ns:test xmlns:ns='urn:x'   >\"\nstring(10) \"</ns:test>\"\n",
    );
}

#[test]
fn bug27908_default_handler() {
    let p = XmlParser::new(None).unwrap();
    p.set_handler(HandlerKind::Default, true).unwrap();
    let mut rec = Rec::default();
    p.parse(b"<root></root>", true, &mut rec, None).unwrap();
    assert_eq!(
        rec.events,
        vec![
            Ev::Default(b"<root>".to_vec()),
            Ev::Default(b"</root>".to_vec())
        ]
    );
}

#[test]
fn xml_error_string_basic_libxml() {
    let xmls = [
        "<?xml version=\"1.0\"?><element>",
        "<?xml>",
        "<?xml version=\"dummy\">",
        "<?xml?>",
        "<?xml version=\"1.0\"?><elem></element>",
    ];
    let mut out = Vec::new();
    for xml in xmls {
        let p = XmlParser::new(None).unwrap();
        if p.parse(xml.as_bytes(), true, &mut NoHandlers, None) == Ok(0) {
            out.extend(var_dump(&PVal::Int(p.error_code())));
            out.extend(var_dump(&s(error_string(p.error_code()).as_bytes())));
        }
    }
    let text = String::from_utf8(out).unwrap();
    let first = "int(5)\nstring(20) \"Invalid document end\"\n";
    let first_alt = "int(77)\nstring(16) \"Tag not finished\"\n";
    let rest = "int(47)\nstring(35) \"Processing Instruction not finished\"\nint(57)\nstring(28) \"XML declaration not finished\"\nint(64)\nstring(17) \"Reserved XML Name\"\nint(76)\nstring(14) \"Mismatched tag\"\n";
    assert!(
        text == format!("{first}{rest}") || text == format!("{first_alt}{rest}"),
        "{text}"
    );
}

#[test]
fn xml_parse_into_struct_variation() {
    let simple =
        b"<main><para><note>simple note</note></para><para><note>simple note</note></para></main>";
    let p = XmlParser::new(None).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(simple, &mut NoHandlers, &mut b)
        .unwrap();
    let mut out = b"Index array\n".to_vec();
    out.extend(print_r(&index(&b)));
    out.extend_from_slice(b"\nVals array\n");
    out.extend(print_r(&values(&b)));
    assert_out(
        &out,
        b"Index array\nArray\n(\n    [MAIN] => Array\n        (\n            [0] => 0\n            [1] => 7\n        )\n\n    [PARA] => Array\n        (\n            [0] => 1\n            [1] => 3\n            [2] => 4\n            [3] => 6\n        )\n\n    [NOTE] => Array\n        (\n            [0] => 2\n            [1] => 5\n        )\n\n)\n\nVals array\nArray\n(\n    [0] => Array\n        (\n            [tag] => MAIN\n            [type] => open\n            [level] => 1\n        )\n\n    [1] => Array\n        (\n            [tag] => PARA\n            [type] => open\n            [level] => 2\n        )\n\n    [2] => Array\n        (\n            [tag] => NOTE\n            [type] => complete\n            [level] => 3\n            [value] => simple note\n        )\n\n    [3] => Array\n        (\n            [tag] => PARA\n            [type] => close\n            [level] => 2\n        )\n\n    [4] => Array\n        (\n            [tag] => PARA\n            [type] => open\n            [level] => 2\n        )\n\n    [5] => Array\n        (\n            [tag] => NOTE\n            [type] => complete\n            [level] => 3\n            [value] => simple note\n        )\n\n    [6] => Array\n        (\n            [tag] => PARA\n            [type] => close\n            [level] => 2\n        )\n\n    [7] => Array\n        (\n            [tag] => MAIN\n            [type] => close\n            [level] => 1\n        )\n\n)\n",
    );
}

#[test]
fn options_get_set() {
    // xml_parser_get_option_variation3.phpt + xml_parser_set_option_basic.phpt
    let p = XmlParser::new(None).unwrap();
    assert_eq!(p.get_option(XmlOption::SkipTagstart), OptionValue::Int(0));
    assert_eq!(p.get_option(XmlOption::SkipWhite), OptionValue::Bool(false));
    assert_eq!(
        p.get_option(XmlOption::CaseFolding),
        OptionValue::Bool(true)
    );
    assert_eq!(p.get_option(XmlOption::ParseHuge), OptionValue::Bool(false));
    p.set_skip_tagstart(7).unwrap();
    p.set_skip_white(true);
    p.set_case_folding(false);
    assert_eq!(p.get_option(XmlOption::SkipTagstart), OptionValue::Int(7));
    assert_eq!(p.get_option(XmlOption::SkipWhite), OptionValue::Bool(true));
    assert_eq!(
        p.get_option(XmlOption::CaseFolding),
        OptionValue::Bool(false)
    );
    let p = XmlParser::new_ns(None, b":").unwrap();
    assert_eq!(
        p.get_option(XmlOption::TargetEncoding),
        OptionValue::Str("UTF-8")
    );
    p.set_target_encoding(b"ISO-8859-1").unwrap();
    assert_eq!(
        p.get_option(XmlOption::TargetEncoding),
        OptionValue::Str("ISO-8859-1")
    );
    p.set_target_encoding(b"us-ascii").unwrap();
    assert_eq!(
        p.get_option(XmlOption::TargetEncoding),
        OptionValue::Str("US-ASCII")
    );
    assert_eq!(
        p.set_target_encoding(b"Invalid Encoding"),
        Err(XmlError::UnsupportedTargetEncoding)
    );
    assert_eq!(
        XmlError::UnsupportedTargetEncoding.to_string(),
        "Argument #3 ($value) is not a supported target encoding"
    );
    assert_eq!(p.set_skip_tagstart(-5), Err(XmlError::TagstartOutOfRange));
    assert_eq!(XmlOption::from_i64(42), None);
    assert_eq!(XmlOption::from_i64(5), Some(XmlOption::ParseHuge));
}

#[test]
fn notation_and_unparsed_entity_decls() {
    let xml = "<?xml version=\"1.0\"?>\n<!DOCTYPE dates [\n    <!NOTATION USDATE SYSTEM \"http://www.schema.net/usdate.not\">\n    <!NOTATION AUSDATE SYSTEM \"http://www.schema.net/ausdate.not\">\n    <!NOTATION ISODATE SYSTEM \"http://www.schema.net/isodate.not\">\n    <!ENTITY testUS  SYSTEM \"test_usdate.xml\" NDATA USDATE>\n    <!ENTITY testAUS SYSTEM \"test_ausdate.xml\" NDATA AUSDATE>\n    <!ENTITY testISO SYSTEM \"test_isodate_xml\" NDATA ISODATE>]>\n]>";
    let p = XmlParser::new(None).unwrap();
    p.set_handler(HandlerKind::NotationDecl, true).unwrap();
    p.set_handler(HandlerKind::UnparsedEntityDecl, true)
        .unwrap();
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), true, &mut rec, None).unwrap();
    let mut out = Vec::new();
    for ev in rec.events {
        match ev {
            Ev::Notation([n, b, sys, publ]) => {
                out.extend_from_slice(b"notation_decl_handler called\n...Name=");
                for (label, v) in [
                    (&b""[..], n),
                    (b"\n...Base=", b),
                    (b"\n...System ID=", sys),
                    (b"\n...Public ID=", publ),
                ] {
                    out.extend_from_slice(label);
                    out.extend_from_slice(php_str(v.as_deref()));
                }
                out.push(b'\n');
            }
            Ev::Unparsed([n, b, sys, publ, nn]) => {
                out.extend_from_slice(b"unparsed_entity_decl_handler called\n...Entity name=");
                for (label, v) in [
                    (&b""[..], n),
                    (b"\n...Base=", b),
                    (b"\n...System ID=", sys),
                    (b"\n...Public ID=", publ),
                    (b"\n...Notation name=", nn),
                ] {
                    out.extend_from_slice(label);
                    out.extend_from_slice(php_str(v.as_deref()));
                }
                out.push(b'\n');
            }
            _ => {}
        }
    }
    assert_out(
        &out,
        b"notation_decl_handler called\n...Name=USDATE\n...Base=\n...System ID=http://www.schema.net/usdate.not\n...Public ID=\nnotation_decl_handler called\n...Name=AUSDATE\n...Base=\n...System ID=http://www.schema.net/ausdate.not\n...Public ID=\nnotation_decl_handler called\n...Name=ISODATE\n...Base=\n...System ID=http://www.schema.net/isodate.not\n...Public ID=\nunparsed_entity_decl_handler called\n...Entity name=testUS\n...Base=\n...System ID=test_usdate.xml\n...Public ID=\n...Notation name=USDATE\nunparsed_entity_decl_handler called\n...Entity name=testAUS\n...Base=\n...System ID=test_ausdate.xml\n...Public ID=\n...Notation name=AUSDATE\nunparsed_entity_decl_handler called\n...Entity name=testISO\n...Base=\n...System ID=test_isodate_xml\n...Public ID=\n...Notation name=ISODATE\n",
    );
}

#[test]
fn processing_instruction_handler() {
    let xml = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n<?xml-stylesheet href=\"default.xsl\" type=\"text/xml\"?>";
    let p = XmlParser::new(None).unwrap();
    p.set_handler(HandlerKind::ProcessingInstruction, true)
        .unwrap();
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), true, &mut rec, None).unwrap();
    assert_eq!(
        rec.events,
        vec![Ev::Pi(
            Some(b"xml-stylesheet".to_vec()),
            Some(b"href=\"default.xsl\" type=\"text/xml\"".to_vec())
        )]
    );
}

#[test]
fn start_namespace_decl_handler() {
    let xml = "<aw1:book xmlns:aw1=\"http://www.somewhere.com/namespace1\"\n          xmlns:aw2=\"file:/DTD/somewhere.dtd\">\n<aw1:para>Any old text.</aw1:para>\n<aw2:td>An HTML table cell.</aw2:td>\n</aw1:book>";
    let p = XmlParser::new_ns(None, b":").unwrap();
    p.set_case_folding(false);
    p.set_handler(HandlerKind::StartNamespaceDecl, true)
        .unwrap();
    p.set_handler(HandlerKind::EndNamespaceDecl, true).unwrap();
    let mut rec = Rec::default();
    p.parse(xml.as_bytes(), true, &mut rec, None).unwrap();
    assert_eq!(
        rec.events,
        vec![
            Ev::StartNs(
                Some(b"aw1".to_vec()),
                Some(b"http://www.somewhere.com/namespace1".to_vec())
            ),
            Ev::StartNs(
                Some(b"aw2".to_vec()),
                Some(b"file:/DTD/somewhere.dtd".to_vec())
            ),
        ]
    );
}

#[test]
fn toffset_bounds() {
    let p = XmlParser::new(None).unwrap();
    p.set_skip_tagstart(100).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(
        b"<?xml version=\"1.0\"?><test><child/></test>",
        &mut NoHandlers,
        &mut b,
    )
    .unwrap();
    assert_out(
        &var_dump(&values(&b)),
        b"array(3) {\n  [0]=>\n  array(3) {\n    [\"tag\"]=>\n    string(0) \"\"\n    [\"type\"]=>\n    string(4) \"open\"\n    [\"level\"]=>\n    int(1)\n  }\n  [1]=>\n  array(3) {\n    [\"tag\"]=>\n    string(0) \"\"\n    [\"type\"]=>\n    string(8) \"complete\"\n    [\"level\"]=>\n    int(2)\n  }\n  [2]=>\n  array(3) {\n    [\"tag\"]=>\n    string(0) \"\"\n    [\"type\"]=>\n    string(5) \"close\"\n    [\"level\"]=>\n    int(1)\n  }\n}\n",
    );
}

/// gh12254 / bug73135: recursive parsing is refused while parsing.
struct Recursive<'p> {
    parser: &'p XmlParser,
    results: Vec<Result<i64, XmlError>>,
}

impl XmlHandlers for Recursive<'_> {
    fn start_element(&mut self, _name: &[u8], _attributes: &[(Vec<u8>, Vec<u8>)]) {
        let mut b = StructBuilder::new(true);
        self.results.push(
            self.parser
                .parse_into_struct(b"<container/>", &mut NoHandlers, &mut b),
        );
        self.results
            .push(self.parser.parse(b"<x/>", true, &mut NoHandlers, None));
        self.results.push(
            self.parser
                .set_handler(HandlerKind::Default, true)
                .map(|()| 0),
        );
        self.results
            .push(self.parser.set_parse_huge(true).map(|()| 0));
    }
}

#[test]
fn gh12254_recursion_rejected() {
    let p = XmlParser::new(None).unwrap();
    set_elements(&p, true, true);
    let mut h = Recursive {
        parser: &p,
        results: Vec::new(),
    };
    let mut b = StructBuilder::new(true);
    assert_eq!(p.parse_into_struct(b"<container/>", &mut h, &mut b), Ok(1));
    assert_eq!(
        h.results,
        vec![
            Err(XmlError::Recursive),
            Err(XmlError::Recursive),
            Err(XmlError::HandlersWhileParsing),
            Err(XmlError::ParseHugeWhileParsing)
        ]
    );
    assert_eq!(
        XmlError::Recursive.to_string(),
        "Parser must not be called recursively"
    );
    // The parser is usable again afterwards (the document is complete, so
    // more content is an error, as in PHP).
    let mut b = StructBuilder::new(true);
    assert_eq!(
        p.parse_into_struct(b"<container/>", &mut NoHandlers, &mut b),
        Ok(0)
    );
}

#[test]
fn gh15868_exceptions_stop_processing() {
    for (which, xml) in [
        ("start", &b"<container/>"[..]),
        ("end", b"<container/>"),
        ("cdata", b"<root><![CDATA[x]]></root>"),
    ] {
        let p = XmlParser::new(None).unwrap();
        if which == "cdata" {
            p.set_handler(HandlerKind::CharacterData, true).unwrap();
        } else {
            set_elements(&p, true, true);
        }
        let mut rec = Rec {
            throw_in: Some(which),
            ..Rec::default()
        };
        let mut b = StructBuilder::new(true);
        p.parse_into_struct(xml, &mut rec, &mut b).unwrap();
        assert!(rec.thrown);
        // No struct entry is built after the exception.
        assert!(b.values.len() <= 1, "{which}: {:?}", b.values);
    }
}

#[test]
fn parse_huge_option() {
    let long = "A".repeat(5_000_000);
    let head = format!("<?xml version=\"1.0\"?><container><{long}/><{long}/><second>foo</second>");
    let tail = "</container>";
    let names = |rec: &Rec| -> Vec<Vec<u8>> {
        rec.events
            .iter()
            .filter_map(|e| match e {
                Ev::Start(n, _, _) => Some(n[..n.len().min(20)].to_vec()),
                _ => None,
            })
            .collect()
    };
    // failure without XML_OPTION_PARSE_HUGE
    let p = XmlParser::new(None).unwrap();
    set_elements(&p, true, true);
    let mut rec = Rec::default();
    assert_eq!(p.parse(head.as_bytes(), true, &mut rec, None), Ok(0));
    assert_eq!(error_string(p.error_code()), "XML_ERR_NAME_REQUIRED");
    assert_eq!(names(&rec), vec![b"CONTAINER".to_vec()]);
    // success with it
    let p = XmlParser::new(None).unwrap();
    p.set_parse_huge(true).unwrap();
    set_elements(&p, true, true);
    let mut rec = Rec::default();
    assert_eq!(p.parse(head.as_bytes(), false, &mut rec, None), Ok(1));
    assert_eq!(p.parse(tail.as_bytes(), true, &mut rec, None), Ok(1));
    assert_eq!(error_string(p.error_code()), "No error");
    assert_eq!(names(&rec).len(), 4);
    let p = XmlParser::new(None).unwrap();
    p.set_parse_huge(true).unwrap();
    let mut b = StructBuilder::new(true);
    assert_eq!(
        p.parse_into_struct(format!("{head}{tail}").as_bytes(), &mut NoHandlers, &mut b),
        Ok(1)
    );
    assert_eq!((b.values.len(), b.index.as_ref().unwrap().len()), (5, 3));
}

#[test]
fn null_arguments_become_false() {
    // `<?target?>` has NULL data; without a PI handler the default handler
    // sees spprintf's "(null)".
    let p = XmlParser::new(None).unwrap();
    p.set_handler(HandlerKind::Default, true).unwrap();
    let mut rec = Rec::default();
    p.parse(b"<a><?t?></a>", true, &mut rec, None).unwrap();
    assert!(
        rec.events.contains(&Ev::Default(b"<?t (null)?>".to_vec())),
        "{:?}",
        rec.events
    );
    assert_eq!(opt(None).clone_kind(), "false");
    assert_eq!(list(vec![]).clone_kind(), "array");
    assert_eq!(pairs(&[]).clone_kind(), "array");
}

trait Kind {
    fn clone_kind(&self) -> &'static str;
}

impl Kind for PVal {
    fn clone_kind(&self) -> &'static str {
        match self {
            PVal::Bool(false) => "false",
            PVal::Arr(_) => "array",
            _ => "other",
        }
    }
}

struct Panicky;

impl XmlHandlers for Panicky {
    fn start_element(&mut self, _name: &[u8], _attributes: &[(Vec<u8>, Vec<u8>)]) {
        panic!("handler panic");
    }
}

#[test]
fn panics_in_handlers_propagate_and_parser_survives() {
    let p = XmlParser::new(None).unwrap();
    set_elements(&p, true, true);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        p.parse(b"<a><b/></a>", true, &mut Panicky, None)
    }));
    assert!(r.is_err());
    assert!(!p.is_parsing());
    // A fresh parse on another parser works normally afterwards.
    let q = XmlParser::new(None).unwrap();
    assert_eq!(q.parse(b"<a/>", true, &mut NoHandlers, None), Ok(1));
}

#[test]
fn many_attributes_are_linear() {
    // oom_xml_parse_attributes / oom_xml_parse_ns: 20k attributes used to
    // take ~12 s because of per-attribute rescans in the attribute array.
    // libxml2 2.9's SAX1 duplicate-attribute check is itself quadratic (PHP
    // pays it too), so our overhead is bounded relative to a bare parse.
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};
    let mut xml = String::from("<root");
    for i in 0..20_000 {
        let _ = write!(xml, " a{i}=\"{i}\"");
    }
    xml.push_str("/>");
    for ns in [false, true] {
        let make = || {
            if ns {
                XmlParser::new_ns(None, b":").unwrap()
            } else {
                XmlParser::new(None).unwrap()
            }
        };
        let bare = make();
        let start = Instant::now();
        assert_eq!(
            bare.parse(xml.as_bytes(), true, &mut NoHandlers, None),
            Ok(1)
        );
        let baseline = start.elapsed();

        let p = make();
        set_elements(&p, true, true);
        let mut rec = Rec::default();
        let mut b = StructBuilder::new(true);
        let start = Instant::now();
        assert_eq!(p.parse_into_struct(xml.as_bytes(), &mut rec, &mut b), Ok(1));
        let full = start.elapsed();
        assert!(
            full < baseline * 3 + Duration::from_secs(2),
            "ns={ns}: {full:?} vs bare {baseline:?}"
        );
        match &rec.events[0] {
            Ev::Start(_, attrs, _) => {
                assert_eq!(attrs.len(), 20_000);
                assert_eq!(attrs[19_999], (b"A19999".to_vec(), b"19999".to_vec()));
            }
            other => panic!("{other:?}"),
        }
        match b.values[0].get("attributes") {
            Some(php_libxml::xml::StructValue::Attributes(a)) => assert_eq!(a.len(), 20_000),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn duplicate_attributes_after_case_folding_keep_first_position() {
    let p = XmlParser::new(None).unwrap();
    set_elements(&p, true, true);
    let mut rec = Rec::default();
    p.parse(b"<r a=\"1\" b=\"2\" A=\"3\"/>", true, &mut rec, None)
        .unwrap();
    match &rec.events[0] {
        Ev::Start(_, attrs, _) => assert_eq!(
            attrs,
            &vec![
                (b"A".to_vec(), b"3".to_vec()),
                (b"B".to_vec(), b"2".to_vec())
            ]
        ),
        other => panic!("{other:?}"),
    }
}
