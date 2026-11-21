// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `xsl` extension and
//! a test document provider (`TestXml`, standing in for DOM/SimpleXML).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_ext_xsl::access::{self, DomAccess, ImportedNode, XmlSource};
use php_libxml::tree::{Document, ParseOptions};
use php_runtime::{Obj, Value, ZStr};
use php_vm::class::Class;
use php_vm::ext::{acc, Args, ClassBuilder, Extension, ModuleBuilder};
use php_vm::{object, Vm, VmOptions, VmResult};
use php_xsl::{CallbackContext, NodeArg, ResultDocument};

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct TestDoc(Document);

fn ctor(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let xml = a.str(0);
    match Document::parse_memory(xml.as_bytes(), &ParseOptions::default()) {
        Ok(d) => {
            object::set_internal(&a.this_obj(), TestDoc(d));
            Ok(Value::Null)
        }
        Err(_) => Err(vm.throw("Exception", "bad xml")),
    }
}

#[allow(clippy::unnecessary_wraps)] // the native method signature
fn save(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let s = object::internal::<TestDoc, _>(&a.this_obj(), |d| d.0.dump(false, None))
        .unwrap_or_default();
    Ok(Value::Str(ZStr::from(s)))
}

struct Provider;

impl DomAccess for Provider {
    fn name(&self) -> &'static str {
        "dom"
    }

    fn root_classes(&self) -> &'static [&'static str] {
        &["TestXml"]
    }

    fn import(&self, _vm: &mut Vm, obj: &Obj) -> VmResult<Option<ImportedNode>> {
        Ok(object::internal::<TestDoc, _>(obj, |d| {
            ImportedNode::Document(XmlSource::Document(d.0.clone()))
        }))
    }

    fn wrap_document(
        &self,
        vm: &mut Vm,
        doc: ResultDocument,
        class: Option<&Rc<Class>>,
    ) -> VmResult<Value> {
        let cls = match class {
            Some(c) => c.clone(),
            None => vm.find_class(b"testxml").expect("TestXml is registered"),
        };
        let o = vm.new_object(&cls)?;
        object::set_internal(&o, TestDoc(doc.into_document()));
        Ok(Value::Object(o))
    }

    fn node_value(
        &self,
        _vm: &mut Vm,
        _input: &XmlSource,
        node: NodeArg<'_>,
        _ctx: &CallbackContext<'_>,
    ) -> VmResult<Option<Value>> {
        let text = match node {
            // SAFETY: the node is alive during the callback.
            NodeArg::Node(n) => unsafe { n.content() },
            // SAFETY: as above.
            NodeArg::Copy(c) => unsafe { c.node().content() },
            NodeArg::Namespace { href, .. } => href,
        };
        Ok(Some(Value::Str(ZStr::from(text))))
    }
}

static PROVIDER: Provider = Provider;

struct TestXmlExt;

impl Extension for TestXmlExt {
    fn name(&self) -> &'static str {
        "testxml"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        ClassBuilder::new("TestXml")
            .method("__construct(string $xml = '<x/>')", acc::PUBLIC, ctor)
            .method("save(): string", acc::PUBLIC, save)
            .register(m.vm());
        access::register(&PROVIDER);
    }
}

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let xsl = php_ext_xsl::Xsl;
    let test = TestXmlExt;
    let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &test, &xsl];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

const SHEET: &str = r#"$sheet = new TestXml('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl"><xsl:output method="text"/><xsl:param name="p" select="0"/><xsl:template match="/"><xsl:value-of select="/root/a"/>|<xsl:value-of select="$p"/>|<xsl:value-of select="php:function(\'strtoupper\', string(/root/a))"/>|<xsl:value-of select="php:function(\'implode\', \',\', /root/*)"/></xsl:template></xsl:stylesheet>');"#;

#[test]
fn transform_to_xml_with_parameters_and_callbacks() {
    let out = run(&format!(
        r"{SHEET}
        $p = new XSLTProcessor;
        var_dump($p->importStylesheet($sheet));
        $p->setParameter('', 'p', 'P1');
        $p->registerPHPFunctions();
        var_dump($p->transformToXml(new TestXml('<root><a>hi</a><b>yo</b></root>')));"
    ));
    assert_eq!(out, "bool(true)\nstring(14) \"hi|P1|HI|hi,yo\"\n");
}

#[test]
fn restricted_callbacks_and_exceptions() {
    let out = run(&format!(
        r"{SHEET}
        $p = new XSLTProcessor;
        $p->importStylesheet($sheet);
        $p->registerPHPFunctions(['strtoupper']);
        try {{ $p->transformToXml(new TestXml('<root><a>hi</a></root>')); }}
        catch (Error $e) {{ echo get_class($e), ': ', $e->getMessage(), PHP_EOL; }}"
    ));
    assert_eq!(out, "Error: No callback handler \"implode\" registered\n");
}

#[test]
fn transform_to_doc_and_uri() {
    let path = std::env::temp_dir().join(format!("php-ext-xsl-{}.txt", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let out = run(&format!(
        r"{SHEET}
        $p = new XSLTProcessor;
        $p->importStylesheet($sheet);
        $p->registerPHPFunctions();
        $in = new TestXml('<root><a>x</a></root>');
        $d = $p->transformToDoc($in);
        var_dump(get_class($d));
        var_dump($p->transformToUri($in, '{path}'));
        try {{ $p->transformToDoc($in, 'NoSuchClass'); }} catch (TypeError $e) {{ echo $e->getMessage(), PHP_EOL; }}
        try {{ $p->transformToDoc($in, 'stdClass'); }} catch (TypeError $e) {{ echo $e->getMessage(), PHP_EOL; }}"
    ));
    let written = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, "x|0|X|x");
    assert_eq!(
        out,
        "string(7) \"TestXml\"\nint(7)\n\
         XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a valid class name or null, \"NoSuchClass\" given\n\
         XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a class name compatible with TestXml, stdClass given\n"
    );
}

#[test]
fn errors_and_properties() {
    let out = run(r"$p = new XSLTProcessor;
        try { $p->importStylesheet(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), PHP_EOL; }
        try { $p->transformToXml(new TestXml); } catch (Error $e) { echo $e->getMessage(), PHP_EOL; }
        var_dump($p->importStylesheet(new TestXml('<container/>')));
        try { $p->maxTemplateDepth = -1; } catch (ValueError $e) { echo $e->getMessage(), PHP_EOL; }
        $p->maxTemplateVars = '7';
        try { unset($p->maxTemplateVars); } catch (Error $e) { echo $e->getMessage(), PHP_EOL; }
        var_dump($p->maxTemplateVars, $p->getSecurityPrefs(), $p->setSecurityPrefs(0), $p->hasExsltSupport());
        try { clone $p; } catch (Error $e) { echo $e->getMessage(), PHP_EOL; }");
    assert_eq!(
        out,
        "XSLTProcessor::importStylesheet(): Argument #1 ($stylesheet) must be a valid XML node\n\
         XSLTProcessor::transformToXml() can only be called after a stylesheet has been imported\n\
         \nWarning: XSLTProcessor::importStylesheet(): compilation error: element container in test.php on line 4\n\
         \nWarning: XSLTProcessor::importStylesheet(): xsltParseStylesheetProcess : document is not a stylesheet in test.php on line 4\n\
         bool(false)\n\
         XSLTProcessor::$maxTemplateDepth must be greater than or equal to 0\n\
         Cannot unset XSLTProcessor::$maxTemplateVars\n\
         int(7)\nint(44)\nint(44)\nbool(true)\n\
         Trying to clone an uncloneable object of class XSLTProcessor\n"
    );
}

const CALL_SHEET: &str = r#"function sheet($expr) { return new TestXml('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl" xmlns:my="urn:my"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="' . $expr . '"/></xsl:template></xsl:stylesheet>'); }"#;

#[test]
fn callback_errors_propagate() {
    let out = run(&format!(
        r#"{CALL_SHEET}
        spl_autoload_register(function ($c) {{ var_dump($c); throw new Exception("Autoload exception"); }});
        function boom() {{ throw new RuntimeException("boom"); }}
        function obj() {{ return new stdClass; }}
        $in = new TestXml('<a/>');
        foreach (["php:function('undefinedfunc')", "php:function('TeSt::dateLang')", "php:function('boom')", "php:function('obj')"] as $e) {{
            $p = new XSLTProcessor;
            $p->registerPHPFunctions();
            $p->importStylesheet(sheet($e));
            try {{ var_dump($p->transformToXml($in)); }} catch (Throwable $t) {{ echo get_class($t), ': ', $t->getMessage(), PHP_EOL; }}
        }}
        $p = new XSLTProcessor;
        $p->importStylesheet(sheet("my:up('x')"));
        $p->registerPHPFunctionNS('urn:my', 'up', fn ($s) => strtoupper($s) . '!');
        var_dump($p->transformToXml($in));"#
    ));
    assert_eq!(
        out,
        "Error: Invalid callback undefinedfunc, function \"undefinedfunc\" not found or invalid function name\n\
         string(4) \"TeSt\"\n\
         Exception: Autoload exception\n\
         RuntimeException: boom\n\
         TypeError: Only objects that are instances of DOM nodes can be converted to an XPath expression\n\
         string(2) \"X!\"\n"
    );
}

#[test]
fn validated_properties_refuse_references() {
    let out = run(r"$p = new XSLTProcessor;
        try { $r =& $p->maxTemplateVars; } catch (Error $e) { echo $e->getMessage(), PHP_EOL; }
        $p->maxTemplateDepth++;
        $x =& $p->cloneDocument;
        $x = true;
        var_dump($p->maxTemplateDepth > 1, $p->cloneDocument);");
    assert_eq!(
        out,
        "Indirect modification of XSLTProcessor::$maxTemplateVars is not allowed\nbool(true)\nbool(true)\n"
    );
}
