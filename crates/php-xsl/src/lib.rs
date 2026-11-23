// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/xsl` for phprust: `XSLTProcessor` over the system libxslt
//! and libexslt.
//!
//! * [`XsltProcessor`] holds an object's state (stylesheet, parameters,
//!   registered PHP callbacks, security preferences, profiling file and
//!   the declared properties) and implements every method.
//! * Documents are passed as [`XmlDocument`]s: `php-libxml` handles (what
//!   SimpleXML uses) or [`RawDocument`]s wrapping another object model's
//!   `xmlDoc` (DOM).
//! * PHP callbacks (`php:function`, `php:functionString`,
//!   `registerPHPFunctionNS()`) are executed through the [`XslHost`] trait.
//! * libxslt/libxml2 messages are reported through `php_libxml::error`
//!   exactly like PHP's handlers do (buffered until a newline; stored in
//!   the internal error list or displayed as warnings, with libxslt option
//!   names rewritten to `$maxTemplateDepth`/`$maxTemplateVars`).
//! * File I/O done by libxslt (`xsl:include`/`xsl:import`, `document()`,
//!   `transformToUri()`, `xsl:document`) goes through libxml2's I/O
//!   factories, i.e. through `php_libxml::io::StreamHooks` (PHP streams)
//!   once the VM installed them.
//! * [`class`] describes the class, constants and `phpinfo()` rows.
//!
//! All `unsafe` code is in [`ffi`] (declarations) and `raw`; the
//! C-variadic error handlers and libxslt field accessors are in
//! `csrc/glue.c`.

// Prose mentions XPath, SimpleXML, libxslt… far too often for backticks.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::doc_markdown)]

pub mod callbacks;
pub mod class;
pub mod document;
pub mod error;
pub mod ffi;
pub mod host;
pub mod params;
pub mod processor;
mod raw;

pub use callbacks::{CallbackRegistry, Lookup, Mode};
pub use document::{OwnedDocument, RawDocument, ResultDocument, XmlDocument};
pub use error::{ErrorClass, XslError, XslResult};
pub use host::{
    CallbackArg, CallbackContext, CallbackReturn, NodeArg, NodeRef, OwnedNode, ReturnedNode,
    XslHost,
};
pub use params::{ParamKey, Parameters};
pub use processor::{Properties, XmlOutput, XsltProcessor, PROPERTY_NAMES};
