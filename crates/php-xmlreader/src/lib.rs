// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/xmlreader`: the `XMLReader` class over libxml2's
//! `xmlTextReader` pull parser.
//!
//! * [`XmlReader`] — the per-object state with every method of the class
//!   (`read`, `next`, `getAttribute*`, `moveTo*`, `readInnerXml`,
//!   `expand`, schema validation, parser properties…) and the virtual
//!   read-only properties ([`Property`]).
//! * [`Source`] — a loaded document not yet attached to an object: the
//!   static constructors (`XMLReader::open()`/`XML()` called statically,
//!   `fromUri`, `fromString`, `fromStream`) first load the data, then the
//!   VM instantiates the (possibly user-subclassed) object, runs its
//!   constructor and attaches the source with [`XmlReader::attach`].
//! * [`class::class_table`] — the class declaration (constants, methods
//!   with arity and types, virtual properties) for generic registration.
//!
//! # Diagnostics
//!
//! Thrown exceptions are returned as [`ReaderError`] (full message
//! including the `XMLReader::method(): ` prefix where PHP puts one).
//! Warnings/notices that the C extension raises with `php_error_docref`
//! go to the [`ReaderHost`] **without** the `"XMLReader::method(): "`
//! prefix, which the VM adds like it does for every internal function.
//!
//! libxml2's own parse/validation errors are reported through
//! [`php_libxml::error`] (the `LibXMLError` list when internal errors are
//! enabled, otherwise the generic-handler warnings such as
//! `"Entity: line 1: parser error : ..."`, the source line and the caret
//! line), exactly like any other libxml-based extension.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod ffi_extra;

pub mod class;
mod reader;

pub use reader::{IssetKind, LoadError, PropValue, Property, Source, XmlReader};

/// The class of a thrown exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// `Error`.
    Error,
    /// `ValueError`.
    ValueError,
    /// `TypeError`.
    TypeError,
}

impl ErrorClass {
    /// The PHP class name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::ValueError => "ValueError",
            Self::TypeError => "TypeError",
        }
    }
}

/// An exception thrown by an `XMLReader` method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderError {
    /// Exception class.
    pub class: ErrorClass,
    /// Exception message.
    pub message: String,
}

impl ReaderError {
    /// An `Error`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::Error,
            message: message.into(),
        }
    }

    /// A `ValueError`.
    pub fn value_error(message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::ValueError,
            message: message.into(),
        }
    }

    /// `zend_argument_value_error(num, ...)` for `XMLReader::{func}()`.
    pub(crate) fn argument(func: &str, num: u32, name: &str, what: &str) -> Self {
        Self::value_error(format!(
            "XMLReader::{func}(): Argument #{num} (${name}) {what}"
        ))
    }
}

impl std::fmt::Display for ReaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.name(), self.message)
    }
}

impl std::error::Error for ReaderError {}

/// What `XMLReader` needs from the VM.
pub trait ReaderHost {
    /// `php_error_docref(NULL, E_WARNING, message)`.
    fn warning(&mut self, message: &str);
    /// `php_error_docref(NULL, E_NOTICE, message)`.
    fn notice(&mut self, message: &str);
}

/// A [`ReaderHost`] that records diagnostics (useful for tests and for
/// callers that replay them later).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CollectingHost {
    /// `(is_warning, message)` in emission order.
    pub messages: Vec<(bool, String)>,
}

impl ReaderHost for CollectingHost {
    fn warning(&mut self, message: &str) {
        self.messages.push((true, message.to_owned()));
    }
    fn notice(&mut self, message: &str) {
        self.messages.push((false, message.to_owned()));
    }
}

/// Node type constants (`XMLReader::NONE` … `XMLReader::XML_DECLARATION`).
pub mod node_type {
    /// `XMLReader::NONE`.
    pub const NONE: i64 = 0;
    /// `XMLReader::ELEMENT`.
    pub const ELEMENT: i64 = 1;
    /// `XMLReader::ATTRIBUTE`.
    pub const ATTRIBUTE: i64 = 2;
    /// `XMLReader::TEXT`.
    pub const TEXT: i64 = 3;
    /// `XMLReader::CDATA`.
    pub const CDATA: i64 = 4;
    /// `XMLReader::ENTITY_REF`.
    pub const ENTITY_REF: i64 = 5;
    /// `XMLReader::ENTITY`.
    pub const ENTITY: i64 = 6;
    /// `XMLReader::PI`.
    pub const PI: i64 = 7;
    /// `XMLReader::COMMENT`.
    pub const COMMENT: i64 = 8;
    /// `XMLReader::DOC`.
    pub const DOC: i64 = 9;
    /// `XMLReader::DOC_TYPE`.
    pub const DOC_TYPE: i64 = 10;
    /// `XMLReader::DOC_FRAGMENT`.
    pub const DOC_FRAGMENT: i64 = 11;
    /// `XMLReader::NOTATION`.
    pub const NOTATION: i64 = 12;
    /// `XMLReader::WHITESPACE`.
    pub const WHITESPACE: i64 = 13;
    /// `XMLReader::SIGNIFICANT_WHITESPACE`.
    pub const SIGNIFICANT_WHITESPACE: i64 = 14;
    /// `XMLReader::END_ELEMENT`.
    pub const END_ELEMENT: i64 = 15;
    /// `XMLReader::END_ENTITY`.
    pub const END_ENTITY: i64 = 16;
    /// `XMLReader::XML_DECLARATION`.
    pub const XML_DECLARATION: i64 = 17;
}

/// Parser property constants (`XMLReader::LOADDTD` …).
pub mod parser_prop {
    /// `XMLReader::LOADDTD`.
    pub const LOADDTD: i64 = 1;
    /// `XMLReader::DEFAULTATTRS`.
    pub const DEFAULTATTRS: i64 = 2;
    /// `XMLReader::VALIDATE`.
    pub const VALIDATE: i64 = 3;
    /// `XMLReader::SUBST_ENTITIES`.
    pub const SUBST_ENTITIES: i64 = 4;
}
