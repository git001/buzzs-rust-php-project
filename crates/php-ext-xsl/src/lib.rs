// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/xsl` for the phprust VM: the `XSLTProcessor` class and the
//! `XSL_*`/`LIBXSLT_*`/`LIBEXSLT_*` constants, binding the `php-xsl`
//! library through the `php_vm::ext` extension API.
//!
//! Documents reach the processor through [`access::DomAccess`] providers
//! registered by the DOM and SimpleXML bindings (PHP's
//! `php_libxml_import_node`); see the [`access`] module for the contract.
//! Without a provider every object argument is rejected with "must be a
//! valid XML node", exactly like an object of an unrelated class.
//!
//! libxml2/libxslt messages are collected by `php_libxml::error` and
//! reported as warnings of the running method (or kept in the internal
//! error list under `libxml_use_internal_errors(true)`).

// Prose mentions XPath, SimpleXML, libxslt… far too often for backticks.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::doc_markdown)]

pub mod access;
mod host;
mod methods;

use php_runtime::{Value, ZStr};
use php_vm::ext::{Extension, InfoItem, ModuleBuilder};
use php_vm::Vm;
use php_xsl::class::ConstValue;

/// The `xsl` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Xsl;

impl Extension for Xsl {
    fn name(&self) -> &'static str {
        "xsl"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for (name, value) in php_xsl::class::constants() {
            let v = match value {
                ConstValue::Int(i) => Value::Int(i),
                ConstValue::Str(s) => Value::Str(ZStr::from(s.as_str())),
            };
            m.constant(name, v);
        }
        methods::register_class(m.vm());
        m.info(module_info);
    }
}

/// `PHP_MINFO_FUNCTION(xsl)`.
fn module_info(_vm: &mut Vm) -> Vec<InfoItem> {
    let mut items = vec![InfoItem::TableStart];
    items.extend(
        php_xsl::class::info_rows()
            .into_iter()
            .map(|(k, v)| InfoItem::Row(vec![k.to_string(), v])),
    );
    items.push(InfoItem::TableEnd);
    items
}
