// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/libxml`, `ext/xml` and `ext/xmlwriter` for the phprust VM: binds
//! the `php-libxml` library through the `php_vm::ext` extension API.
//!
//! * [`libxml`] — the `libxml_*()` functions, `LibXMLError`, the `LIBXML_*`
//!   constants, the stream I/O hooks and the shared libxml state API used
//!   by every libxml-based binding (DOM, `SimpleXML`, `XMLReader`, XSL, SOAP).
//! * [`mime_sniff`] — `Content-Type` charset sniffing for documents loaded
//!   over `http://` (used by the stream hooks; public for other bindings).
//! * [`xml`] — `ext/xml`: `XMLParser` and the `xml_*()` functions.
//! * [`xmlwriter`] — `ext/xmlwriter`: the `XMLWriter` class and the
//!   procedural `xmlwriter_*()` functions.
//!
//! Each module is its own [`Extension`] ([`Libxml`], [`Xml`],
//! [`XmlWriter`]) so functions, classes and constants are attributed to the
//! right module; [`Libxml`] must be registered first.

// Native handlers have the fixed `NativeFn` / hook signatures, so many
// infallible ones still return `VmResult`.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps)]

mod image_svg;
pub mod libxml;
pub mod mime_sniff;
pub mod xml;
pub mod xmlwriter;

use php_vm::ext::{Extension, ModuleBuilder};

/// `ext/libxml` (register before every libxml-based extension).
#[derive(Debug, Clone, Copy, Default)]
pub struct Libxml;

impl Extension for Libxml {
    fn name(&self) -> &'static str {
        "libxml"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        libxml::register(m);
        php_ext_exif::register_image_handler(m, &image_svg::HANDLER);
    }
}

/// `ext/xml`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Xml;

impl Extension for Xml {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        // Hosts that register only `Xml` (e.g. other crates' test harnesses)
        // still get the libxml module, attributed to `xml`.
        if m.vm()
            .native_function("libxml_use_internal_errors")
            .is_none()
        {
            libxml::register(m);
        }
        xml::register(m);
    }
}

/// `ext/xmlwriter`.
#[derive(Debug, Clone, Copy, Default)]
pub struct XmlWriter;

impl Extension for XmlWriter {
    fn name(&self) -> &'static str {
        "xmlwriter"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        xmlwriter::register(m);
    }
}
