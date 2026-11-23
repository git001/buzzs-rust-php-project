// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's ext/soap for phprust.
//!
//! The crate is organised like the C extension:
//!
//! * [`xml`] — the XML tree ext/soap works on, with libxml2-compatible
//!   serialization; documents are parsed by the system libxml2.
//! * [`sdl`], [`wsdl`], [`schema`] — the service description model and the
//!   WSDL 1.1 / XML Schema loaders (`php_sdl.c`, `php_schema.c`);
//!   [`sdl_print`] renders `__getFunctions()`/`__getTypes()`; [`cache`] is
//!   `get_sdl` with the in-memory WSDL cache and the `soap.*` INI table.
//! * [`encoders`], [`encoding`], [`typemap`], [`charset`] — PHP values ⇄ XML
//!   (`php_encoding.c`).
//! * [`packet`] — request/response envelopes and faults
//!   (`serialize_function_call`, `parse_packet_soap`, …).
//! * [`client`], [`server`], [`fault`], [`objects`] — the classes;
//!   [`http`] — the HTTP transport (`php_http.c`).
//! * [`host`] — the [`host::SoapHost`] trait through which everything
//!   object-model-ish and all I/O reaches the engine; [`state`] — the
//!   per-object extension state; [`globals`] — module globals and the
//!   SOAP error handler; [`classes`] / [`consts`] — what the engine
//!   registers.

// The encoder functions keep the names of their C counterparts
// (`to_xml_*`/`to_zval_*`), the namespace constants are used wholesale, and
// short binding names mirror the C sources being ported.
// Long functions with many parameters follow the structure of the C code
// they port (one Rust function per C function), which keeps them reviewable
// against the reference.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(
    clippy::wrong_self_convention,
    clippy::wildcard_imports,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::unused_self,
    clippy::only_used_in_recursion,
    clippy::items_after_statements,
    clippy::needless_late_init
)]

pub mod cache;
pub mod charset;
pub mod classes;
pub mod client;
pub mod consts;
pub mod encoders;
pub mod encoding;
pub mod error;
pub mod fault;
pub mod globals;
pub mod host;
pub mod http;
pub mod objects;
pub mod packet;
pub mod schema;
pub mod sdl;
pub mod sdl_print;
pub mod server;
pub mod state;
pub mod typemap;
pub mod wsdl;
pub mod xml;
pub mod zhash;

/// `zend_zval_type_name` of a value (used in error messages).
#[must_use]
pub fn client_type_name(v: &php_runtime::Value) -> &'static str {
    client::zend_type_name(v)
}

/// The PHP version reported in the default `User-Agent`.
pub const PHP_VERSION: &str = "8.5.0-dev";
