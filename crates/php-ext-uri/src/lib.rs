// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/uri` for the phprust VM: the `Uri\Rfc3986\*` and `Uri\WhatWg\*`
//! classes, their enums and exceptions, and `Uri\WhatWg\url_percent_encode()`.
//!
//! Parsing, normalization, resolution and every exception message live in
//! the `php-uri` library (`php_uri::api`, one Rust method per PHP method);
//! this crate keeps the parsed URI as native object state, converts values
//! and turns `UriError`s into PHP exceptions (including the
//! `Uri\WhatWg\UrlValidationError` lists of `InvalidUrlException::$errors`
//! and the by-reference `$errors`/`$softErrors` arguments). The enums are
//! native unit enums.

mod common;
pub mod internal_api;
mod rfc3986;
mod whatwg;

use php_vm::ext::{class_flags, ClassBuilder, Extension, InfoItem, ModuleBuilder};
use php_vm::Vm;

/// The `uri` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Uri;

/// The unit enums of `php_uri.stub.php`: name and cases.
const ENUMS: &[(&str, &[&str])] = &[
    (
        "Uri\\Rfc3986\\UriType",
        &[
            "AbsolutePathReference",
            "RelativePathReference",
            "NetworkPathReference",
            "Uri",
        ],
    ),
    (
        "Uri\\Rfc3986\\UriHostType",
        &["IpV4", "IpV6", "IpVFuture", "RegisteredName"],
    ),
    (
        "Uri\\WhatWg\\UrlPercentEncodingMode",
        &[
            "Username",
            "Password",
            "OpaqueHost",
            "Path",
            "OpaquePath",
            "PathSegment",
            "Query",
            "SpecialQuery",
            "FormQuery",
            "Fragment",
        ],
    ),
    (
        "Uri\\UriComparisonMode",
        &["IncludeFragment", "ExcludeFragment"],
    ),
    (
        "Uri\\WhatWg\\UrlHostType",
        &["IpV4", "IpV6", "Domain", "Opaque", "Empty"],
    ),
];

/// Registers a native unit enum.
fn register_enum(vm: &mut Vm, name: &str, cases: &[&str]) {
    let mut b = ClassBuilder::new(name).unit_enum();
    for c in cases {
        b = b.case(c, None);
    }
    b.register(vm);
}

/// `Uri\WhatWg\UrlValidationErrorType` (its cases come from php-uri).
fn register_validation_error_type(vm: &mut Vm) {
    let mut b = ClassBuilder::new(common::VALIDATION_ERROR_TYPE_ENUM).unit_enum();
    for t in php_uri::UrlValidationErrorType::ALL {
        b = b.case(t.name(), None);
    }
    b.register(vm);
}

/// `PHP_MINFO_FUNCTION(uri)` (static build with the bundled uriparser).
fn info(_vm: &mut Vm) -> Vec<InfoItem> {
    vec![
        InfoItem::TableStart,
        InfoItem::Row(vec!["URI support".into(), "active".into()]),
        InfoItem::Row(vec!["uriparser bundled version".into(), "1.0.2".into()]),
        InfoItem::TableEnd,
    ]
}

/// The plain exception hierarchy of `Uri\`.
fn register_exceptions(m: &mut ModuleBuilder<'_>) {
    ClassBuilder::new("Uri\\UriException")
        .extends(m.vm(), "Exception")
        .flags(class_flags::NO_DYNAMIC_PROPERTIES)
        .register(m.vm());
    ClassBuilder::new("Uri\\UriError")
        .extends(m.vm(), "Error")
        .flags(class_flags::NO_DYNAMIC_PROPERTIES)
        .register(m.vm());
    ClassBuilder::new("Uri\\InvalidUriException")
        .extends(m.vm(), "Uri\\UriException")
        .flags(class_flags::NO_DYNAMIC_PROPERTIES)
        .register(m.vm());
}

impl Extension for Uri {
    fn name(&self) -> &'static str {
        "uri"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        // `PHP_MINIT_FUNCTION(uri)` order.
        rfc3986::register(m);
        for (name, cases) in &ENUMS[..2] {
            register_enum(m.vm(), name, cases);
        }
        whatwg::register(m);
        for (name, cases) in &ENUMS[2..4] {
            register_enum(m.vm(), name, cases);
        }
        register_exceptions(m);
        whatwg::register_exception(m);
        let (name, cases) = ENUMS[4];
        register_enum(m.vm(), name, cases);
        whatwg::register_validation_error(m);
        register_validation_error_type(m.vm());
        m.info(info);
    }
}
