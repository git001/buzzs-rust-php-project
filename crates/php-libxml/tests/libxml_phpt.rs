// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Ports of php-src `ext/libxml/tests/*.phpt` that are checkable through
//! the document layer (the DOM calls are replaced by the equivalent
//! parse options).

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use php_libxml::constants::{LIBXML_BIGLINES, LIBXML_DTDLOAD, LIBXML_DTDVALID, LIBXML_NOENT};
use php_libxml::error::{self, DiagnosticLevel};
use php_libxml::io::{
    self, EntityLoaderContext, EntityLoaderResult, EntityRequest, InputStream, StreamHooks,
};
use php_libxml::tree::{Document, ParseOptions};
use php_libxml::xml::{NoHandlers, StructBuilder, XmlParser};

fn reset() {
    error::reset_request_state();
    error::set_diagnostic_handler(None);
    error::take_diagnostics();
    io::set_stream_hooks(None);
}

fn messages() -> Vec<String> {
    error::take_diagnostics()
        .into_iter()
        .map(|d| String::from_utf8_lossy(&d.message).into_owned())
        .collect()
}

type Log = Rc<RefCell<Vec<EntityRequest>>>;

fn install_loader(log: &Log, mut answers: Vec<EntityLoaderResult>) {
    let log = log.clone();
    let answers = RefCell::new({
        answers.reverse();
        answers
    });
    io::set_external_entity_loader(Some(Rc::new(move |req: &EntityRequest| {
        log.borrow_mut().push(req.clone());
        answers
            .borrow_mut()
            .pop()
            .unwrap_or(EntityLoaderResult::Null)
    })));
}

fn stream(s: &str) -> EntityLoaderResult {
    EntityLoaderResult::Stream(Box::new(std::io::Cursor::new(s.as_bytes().to_vec())))
}

#[test]
fn set_external_entity_loader_basic() {
    reset();
    let log = Log::default();
    install_loader(&log, vec![stream("<!ELEMENT foo (#PCDATA)>")]);
    assert!(io::has_external_entity_loader());
    let xml = "<!DOCTYPE foo PUBLIC \"-//FOO/BAR\" \"http://example.com/foobar\">\n<foo>bar</foo>";
    let opts = ParseOptions {
        options: LIBXML_DTDLOAD | LIBXML_DTDVALID,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("valid");
    assert_eq!(doc.root().expect("root").content(), b"bar");
    let log = log.borrow();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].public_id.as_deref(), Some(&b"-//FOO/BAR"[..]));
    assert_eq!(
        log[0].system_id.as_deref(),
        Some(&b"http://example.com/foobar"[..])
    );
    assert_eq!(log[0].context.int_sub_name.as_deref(), Some(&b"foo"[..]));
    assert_eq!(
        log[0].context.ext_sub_uri.as_deref(),
        Some(&b"http://example.com/foobar"[..])
    );
    assert_eq!(
        log[0].context.ext_sub_system.as_deref(),
        Some(&b"-//FOO/BAR"[..])
    );
    assert!(messages().is_empty());
    reset();
    assert!(!io::has_external_entity_loader());
}

#[test]
fn set_external_entity_loader_variation1_parameter_entities() {
    reset();
    let log = Log::default();
    let dtd = "<!ELEMENT foo (#PCDATA)>\n<!ENTITY % fooentity PUBLIC\n   \"-//FOO/ENTITY\"\n   \"fooentity.ent\">\n%fooentity;";
    install_loader(&log, vec![stream(dtd), stream("<!ENTITY fooz \"baz\">")]);
    let xml =
        "<!DOCTYPE foo PUBLIC \"-//FOO/BAR\" \"http://example.com/foobar\">\n<foo>bar&fooz;</foo>";
    let opts = ParseOptions {
        options: LIBXML_DTDLOAD | LIBXML_NOENT,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    assert_eq!(doc.root().expect("root").content(), b"barbaz");
    let log = log.borrow();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].public_id.as_deref(), Some(&b"-//FOO/ENTITY"[..]));
    assert_eq!(
        log[1].system_id.as_deref(),
        Some(&b"http://example.com/fooentity.ent"[..])
    );
    assert_eq!(log[1].context.int_sub_name.as_deref(), Some(&b"foo"[..]));
    let keys: Vec<_> = log[1].context.entries().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        keys,
        ["directory", "intSubName", "extSubURI", "extSubSystem"]
    );
    reset();
}

#[test]
fn resolver_returning_null_or_failing() {
    reset();
    let log = Log::default();
    install_loader(
        &log,
        vec![
            EntityLoaderResult::Null,
            EntityLoaderResult::Failed {
                function_name: b"{closure}".to_vec(),
            },
        ],
    );
    let xml = "<!DOCTYPE foo PUBLIC \"-//FOO/BAR\" \"foobar.dtd\">\n<foo>bar</foo>";
    let opts = ParseOptions {
        options: LIBXML_DTDLOAD,
        ..Default::default()
    };
    let _ = Document::parse_memory(xml.as_bytes(), &opts);
    let first = messages();
    assert!(
        first
            .iter()
            .any(|m| m
                .starts_with("Failed to load external entity \"-//FOO/BAR\" in Entity, line: ")),
        "{first:?}"
    );
    // A failing callback's message has no newline, so it is reported
    // together with the next one (PHP's error buffer).
    let _ = Document::parse_memory(xml.as_bytes(), &opts);
    let second = messages();
    assert!(
        second.iter().any(|m| m.starts_with(
            "Call to user entity loader callback '{closure}' has failedFailed to load external entity \"-//FOO/BAR\""
        )),
        "{second:?}"
    );
    // null_returned_by_resolver.phpt: no public id.
    install_loader(&log, vec![EntityLoaderResult::Null]);
    let xml = "<!DOCTYPE foo SYSTEM \"foobar\">\n<foo/>";
    let _ = Document::parse_memory(xml.as_bytes(), &opts);
    let third = messages();
    assert!(
        third.iter().any(|m| m.starts_with(
            "Failed to load external entity because the resolver function returned null"
        )),
        "{third:?}"
    );
    // With internal errors the messages are list entries instead.
    error::use_internal_errors(Some(true));
    install_loader(&log, vec![EntityLoaderResult::Null]);
    let _ = Document::parse_memory(xml.as_bytes(), &opts);
    assert!(messages().is_empty());
    let errs = error::get_errors();
    let e = errs
        .iter()
        .find(|e| {
            e.message
                .starts_with(b"Failed to load external entity because")
        })
        .expect("listed");
    assert_eq!((e.code, e.level), (1, 2));
    reset();
}

#[test]
fn resolver_returning_a_path() {
    reset();
    let dir = std::env::temp_dir().join(format!("php-libxml-loader-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dtd = dir.join("foo.dtd");
    std::fs::write(&dtd, "<!ENTITY greeting \"hello\">").unwrap();
    let log = Log::default();
    install_loader(
        &log,
        vec![EntityLoaderResult::Path(
            dtd.to_string_lossy().as_bytes().to_vec(),
        )],
    );
    let xml = "<!DOCTYPE foo SYSTEM \"whatever.dtd\">\n<foo>&greeting;</foo>";
    let opts = ParseOptions {
        options: LIBXML_DTDLOAD | LIBXML_NOENT,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    assert_eq!(doc.root().expect("root").content(), b"hello");
    std::fs::remove_dir_all(dir).unwrap();
    reset();
}

fn xxe_payload() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "php-libxml-xxe-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let payload = dir.join("payload.txt");
    std::fs::write(&payload, "SECRET_DATA").unwrap();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE test [<!ENTITY xxe SYSTEM \"{}\">]>\n<foo>&xxe;</foo>",
        payload.display()
    );
    (dir, xml)
}

#[test]
fn disable_entity_loader() {
    reset();
    let (dir, xml) = xxe_payload();
    let opts = ParseOptions {
        options: LIBXML_NOENT | LIBXML_DTDLOAD,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    assert!(String::from_utf8_lossy(&doc.dump(false, None)).contains("SECRET_DATA"));
    assert!(!io::disable_entity_loader(true));
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    assert!(!String::from_utf8_lossy(&doc.dump(false, None)).contains("SECRET_DATA"));
    let m = messages();
    // The I/O warning comes from libxml2's internal entity context and is
    // formatted by xmlReportError; the follow-up "Failure to process
    // entity" / "Entity 'xxe' not defined" warnings of the .phpt are only
    // raised by newer libxml2 releases than the system's 2.9.14.
    assert_eq!(
        m[0],
        format!(
            "I/O warning : failed to load external entity \"{}/payload.txt\"",
            dir.display()
        )
    );
    assert!(io::disable_entity_loader(false));
    std::fs::remove_dir_all(dir).unwrap();
    reset();
}

#[test]
fn entity_loading_disabled_by_default() {
    reset();
    let (dir, xml) = xxe_payload();
    let doc = Document::parse_memory(xml.as_bytes(), &ParseOptions::default()).expect("parse");
    assert!(!String::from_utf8_lossy(&doc.dump(false, None)).contains("SECRET_DATA"));
    let p = XmlParser::new(None).unwrap();
    let mut b = StructBuilder::new(true);
    p.parse_into_struct(xml.as_bytes(), &mut NoHandlers, &mut b)
        .unwrap();
    assert!(!format!("{:?}", b.values).contains("SECRET_DATA"));
    let opts = ParseOptions {
        options: LIBXML_NOENT,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    assert!(String::from_utf8_lossy(&doc.dump(false, None)).contains("SECRET_DATA"));
    std::fs::remove_dir_all(dir).unwrap();
    reset();
}

#[test]
fn bug54138_big_lines() {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root>\n");
    for _ in 0..65535 {
        xml.push_str("<foo/>\n");
    }
    xml.push_str("\n<bar/>\n</root>");
    let opts = ParseOptions {
        options: LIBXML_BIGLINES,
        ..Default::default()
    };
    let doc = Document::parse_memory(xml.as_bytes(), &opts).expect("parse");
    let bar = doc
        .root()
        .expect("root")
        .children()
        .into_iter()
        .find(|n| n.name() == b"bar")
        .expect("bar");
    assert_eq!(bar.line(), 65540);
}

#[test]
fn stream_hooks_route_file_loads() {
    reset();
    let opened = Rc::new(RefCell::new(Vec::new()));
    let seen = opened.clone();
    io::set_stream_hooks(Some(StreamHooks {
        open_read: Some(Box::new(move |p: &[u8]| {
            // libxml2 may also probe its XML catalog through the hook.
            if !p.ends_with(b"doc.xml") {
                return None;
            }
            seen.borrow_mut().push(p.to_vec());
            Some(InputStream {
                reader: Box::new(std::io::Cursor::new(b"<doc>via hook</doc>".to_vec())),
                charset: None,
            })
        })),
        open_write: None,
    }));
    let doc = Document::parse_file(b"file:///no/such%20dir/doc.xml", &ParseOptions::default())
        .expect("parse");
    assert_eq!(doc.root().expect("root").content(), b"via hook");
    // file: URIs are unescaped before reaching the stream layer.
    assert_eq!(
        opened.borrow().as_slice(),
        &[b"file:///no/such dir/doc.xml".to_vec()]
    );
    // %00 is refused with a warning.
    let r = Document::parse_file(b"/tmp/a%00b.xml", &ParseOptions::default());
    assert!(r.is_err());
    let d = error::take_diagnostics();
    assert!(d.iter().any(|d| d.level == DiagnosticLevel::Warning
        && d.message == b"URI must not contain percent-encoded NUL bytes"));
    // The entity loader kill switch also blocks hooked loads.
    io::disable_entity_loader(true);
    assert!(Document::parse_file(b"/tmp/x.xml", &ParseOptions::default()).is_err());
    io::disable_entity_loader(false);
    reset();
}

#[test]
fn entity_loader_context_entries() {
    let ctx = EntityLoaderContext::default();
    assert!(ctx.entries().iter().all(|(_, v)| v.is_none()));
}
