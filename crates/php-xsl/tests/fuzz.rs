// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Randomised stylesheets, inputs, parameters and callback behaviour:
//! nothing may panic, crash or leave the processor in a transforming
//! state. (Run under valgrind to check memory safety.)

#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(clippy::doc_markdown)]

mod common;

use common::*;
use php_xsl::{ParamKey, XslError};

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n as u64)).unwrap()
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const EXPRS: &[&str] = &[
    ".",
    "/*",
    "//*",
    "@*",
    "string(.)",
    "count(//*)",
    "$p",
    "1 div 0",
    "php:function('cb', .)",
    "php:function('cb', //*, 'x', 3, true())",
    "php:functionString('cb', //*)",
    "php:function('nope')",
    "php:function()",
    "php:function(1)",
    "ns:f(string(.))",
    "ns:g(., .)",
    "document('')",
    "document('/nonexistent/file.xml')",
    "key('k', 'a')",
    "namespace::*",
    "unknown(.)",
    "date:year('2007-12-31')",
    "str:tokenize('a b c')",
    "exsl:node-set($p)",
    "math:max(//*)",
    "1 + ",
];

const INSTRS: &[&str] = &[
    "<xsl:value-of select=\"{e}\"/>",
    "<xsl:for-each select=\"{e}\"><xsl:value-of select=\".\"/></xsl:for-each>",
    "<xsl:apply-templates select=\"{e}\"/>",
    "<xsl:call-template name=\"t\"/>",
    "<xsl:copy-of select=\"{e}\"/>",
    "<xsl:if test=\"{e}\">y</xsl:if>",
    "<xsl:variable name=\"v\" select=\"{e}\"/><xsl:value-of select=\"$v\"/>",
    "<xsl:element name=\"{{name(.)}}\">x</xsl:element>",
    "<xsl:message>m</xsl:message>",
    "<xsl:document href=\"/nonexistent/out.xml\">d</xsl:document>",
    "<xsl:bogus/>",
    "<e a=\"{{{e}}}\"/>",
    "text<br/>",
];

const OUTPUTS: &[&str] = &[
    "",
    "<xsl:output method=\"text\"/>",
    "<xsl:output method=\"html\" indent=\"yes\"/>",
    "<xsl:output method=\"xml\" encoding=\"iso-8859-1\"/>",
    "<xsl:output method=\"xml\" encoding=\"bogus-enc\"/>",
];

const INPUTS: &[&str] = &[
    "<a/>",
    "<a x=\"1\"><b>a</b><b>b</b><c xmlns:q=\"urn:q\">q</c></a>",
    "<?xml version=\"1.0\"?><root><k>a</k><k>b</k></root>",
    "<r xmlns=\"urn:d\"><s/>text</r>",
];

fn stylesheet(rng: &mut Rng) -> String {
    let mut body = String::new();
    for _ in 0..=rng.below(4) {
        let e = rng.pick(EXPRS);
        body.push_str(
            &rng.pick(INSTRS)
                .replace("{e}", e)
                .replace("{{", "{")
                .replace("}}", "}"),
        );
    }
    let recursion = if rng.below(4) == 0 {
        "<xsl:call-template name=\"t\"/>"
    } else {
        ""
    };
    let key = if rng.below(3) == 0 {
        "<xsl:key name=\"k\" match=\"*\" use=\".\"/>"
    } else {
        ""
    };
    let mut s = format!(
        r#"<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
 xmlns:php="http://php.net/xsl" xmlns:ns="urn:ns" xmlns:date="http://exslt.org/dates-and-times"
 xmlns:str="http://exslt.org/strings" xmlns:exsl="http://exslt.org/common" xmlns:math="http://exslt.org/math"
 extension-element-prefixes="date str exsl math">
{output}{key}<xsl:param name="p" select="'d'"/>
<xsl:template match="/">{body}</xsl:template>
<xsl:template match="*"><xsl:value-of select="name()"/><xsl:apply-templates/></xsl:template>
<xsl:template name="t"><xsl:param name="n" select="0"/>{recursion}</xsl:template>
</xsl:stylesheet>"#,
        output = rng.pick(OUTPUTS),
    );
    if rng.below(8) == 0 {
        let cut = rng.below(s.len());
        s.truncate(cut);
    }
    s
}

fn callback(seed: u64) -> Callable {
    std::rc::Rc::new(move |_: &mut Php, args: Vec<Value>| {
        let mut rng = Rng(seed | 1);
        let node = args.iter().find_map(|a| a.nodes().into_iter().next());
        Ok(match rng.below(8) {
            0 => Value::Null,
            1 => Value::Bool(true),
            2 => Value::s("str"),
            3 => node.map_or(Value::Int(3), Value::Node),
            4 => Value::Object("stdClass", None),
            5 => Value::Array(vec![]),
            6 => return Err(Exc::new("Exception", "fuzz")),
            _ => {
                let doc = load_sxe("<ext><y/></ext>");
                Value::Node(TNode::Handle(doc.root().unwrap()))
            }
        })
    })
}

#[test]
fn fuzz_stylesheets() {
    setup(&fixtures());
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut compiled = 0;
    let mut transformed = 0;
    for _ in 0..400u64 {
        take_exception();
        let proc = Proc::new();
        let sheet_text = stylesheet(&mut rng);
        let Ok(sheet) = php_libxml::tree::Document::parse_memory(
            sheet_text.as_bytes(),
            &php_libxml::tree::ParseOptions::default(),
        ) else {
            continue;
        };
        proc.set_clone_document(Some(rng.below(2) == 0));
        let _ = proc.set_max_template_depth(i64::try_from(rng.below(50)).unwrap());
        let _ = proc.set_max_template_vars(i64::try_from(rng.below(50)).unwrap());
        proc.set_do_xinclude(Some(rng.below(2) == 0));
        proc.set_security_prefs([0, 44, 62, -1][rng.below(4)]);
        let imported = silenced(|| proc.import_stylesheet(&sheet)).unwrap();
        compiled += usize::from(imported);
        match rng.below(4) {
            0 => {}
            1 => proc.register_php_functions_all(),
            2 => {
                let _ = proc.register_php_functions_array(
                    vec![(Some(b"cb".to_vec()), callback(rng.next()))],
                    |c| Ok(c.clone()),
                    |_| Ok::<_, XslError>(Vec::new()),
                );
            }
            _ => {
                let _ = proc.register_php_functions_name(b"cb", |_| Ok(callback(rng.next())));
            }
        }
        if rng.below(2) == 0 {
            let _ = proc.register_php_function_ns(b"urn:ns", b"f", || {
                Ok::<_, XslError>(callback(rng.next()))
            });
        }
        let mut junk = Vec::new();
        for _ in 0..rng.below(12) {
            junk.push(u8::try_from(rng.below(256)).unwrap());
        }
        let _ = proc.set_parameter(b"", b"p", Some(&junk));
        let _ = proc.set_parameters(
            b"urn:x",
            vec![(ParamKey::Str(junk.clone()), "v")],
            None,
            |v| Ok::<_, XslError>(v.as_bytes().to_vec()),
        );
        let input = load_sxe(rng.pick(INPUTS));
        let mut php = Php::new();
        php.define("cb", |_, a| Ok(a.first().cloned().unwrap_or(Value::Null)));
        let r = silenced(|| match rng.below(3) {
            0 => proc.transform_to_xml(&input, &mut php).map(|_| ()),
            1 => proc.transform_to_doc(&input, &mut php).map(|_| ()),
            _ => proc
                .transform_to_uri(
                    &input,
                    scratch().join("fuzz.out").to_string_lossy().as_bytes(),
                    &mut php,
                )
                .map(|_| ()),
        });
        if imported {
            assert!(r.is_ok());
            transformed += 1;
        } else {
            assert!(r.is_err());
        }
        assert!(!proc.is_transforming());
        assert!(php.node_list.is_empty());
    }
    take_exception();
    take_output();
    eprintln!("compiled {compiled}, transformed {transformed}");
    assert!(compiled > 150, "{compiled}");
}

#[test]
fn fuzz_api_inputs() {
    let mut rng = Rng(42);
    let proc = Proc::new();
    for _ in 0..2000 {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for _ in 0..rng.below(6) {
            a.push(b"{}:\0ax"[rng.below(6)]);
        }
        for _ in 0..rng.below(6) {
            b.push(b"{}:\0by"[rng.below(6)]);
        }
        let _ = proc.set_parameter(&a, &b, Some(&a));
        let _ = proc.get_parameter(&a, &b);
        let _ = proc.remove_parameter(&b, &a);
        let _ = proc.register_php_functions_name(&a, |_| Ok(callback(1)));
        let _ = proc.register_php_function_ns(&a, &b, || Ok::<_, XslError>(callback(2)));
        let _ = proc.set_profiling(Some(&b));
        let _ = proc.unset_property(std::str::from_utf8(&a).unwrap_or(""));
    }
    assert!(proc.set_profiling(None).unwrap());
}
