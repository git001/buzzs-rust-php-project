// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/dom` for phprust: `DOMDocument` and friends, plus the
//! spec-compliant PHP 8.4 `Dom\*` API, on top of the system libxml2.
//!
//! The crate ports `ext/dom` closely. Node objects are [`Node`] handles
//! (one per libxml2 node, stored in the node's `_private` field like PHP
//! does); collections are [`NodeList`] / [`NamedNodeMap`]; documents keep
//! their state in a shared [`object::DocRef`]. Every class method is a Rust
//! method returning [`DomResult`], whose error is the exception PHP would
//! throw ([`DomError`]); warnings go through [`error::set_diagnostic_handler`]
//! (DOM's own) and `php_libxml::error` (libxml2's).
//!
//! The VM registers classes generically from [`classes::class_table`] and
//! calls into [`dispatch`].

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod chardata;
pub mod classes;
pub mod constants;
pub(crate) mod css;
pub mod dispatch;
pub mod document;
pub mod element;
pub mod error;
mod ffi;
pub mod host;
pub mod html;
pub(crate) mod html5_parser;
pub(crate) mod html5_serializer;
pub mod implementation;
pub mod inner_outer;
pub mod interop;
pub(crate) mod lexbor;
pub mod lists;
pub mod node;
pub(crate) mod ns;
pub mod object;
pub(crate) mod parse;
pub(crate) mod raw;
pub(crate) mod save;
pub mod stack;
pub mod tokenlist;
pub(crate) mod tree;
pub mod value;
pub mod xml_serializer;
pub mod xpath;

pub(crate) use chardata::attr;

pub use document::DocFlag;
pub use element::AdjacentPosition;
pub use error::{DomError, DomExceptionCode, DomResult};
pub use host::{DomHost, HostCallable, HostStream, NoHost, XPathArg, XPathReturn};
pub use implementation::Implementation;
pub use lists::{CollectionIter, IterKey, NamedNodeMap, NodeList, Offset};
pub use object::Node;
pub use tokenlist::TokenList;
pub use tree::NodeOrString;
pub use value::{ArrayKey, DomValue, NamespaceInfo};
pub use xpath::{Restrict, XPath, XPathResult};
