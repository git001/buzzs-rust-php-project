// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's libxml-based extensions on the system libxml2.
//!
//! * [`error`] / [`io`] — `ext/libxml`: the `libxml_*()` functions, the
//!   `LibXMLError` list, external entity loader and stream hooks.
//! * [`tree`] — a safe, reference-counted document/node layer (the shared
//!   base for DOM, `SimpleXML`, `XMLReader` and XSL). Trees shared between
//!   two object models (`simplexml_import_dom()`, `dom_import_simplexml()`,
//!   `XSLTProcessor::transformToDoc()`) use [`tree::SharedDocument`]:
//!   reference-counted co-ownership mirroring
//!   `php_libxml_node_decrement_resource` / `php_libxml_node_free_list`.
//! * [`xml`] — `ext/xml`: the expat-compatible SAX parser.
//! * [`xmlwriter`] — `ext/xmlwriter`: the `XMLWriter` class.
//! * [`constants`] — the `LIBXML_*` constants.
//!
//! All `unsafe` code is confined to [`ffi`] (declarations) and [`raw`]
//! (sound wrappers); see their module documentation.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod constants;
pub mod error;
pub mod ffi;
pub mod io;
pub mod raw;
pub mod tree;
pub mod xml;
pub mod xmlwriter;
