// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/dom` for the phprust VM: binds the `php-dom` library through the
//! `php_vm::ext` extension API.
//!
//! * Classes, interfaces, the `Dom\AdjacentPosition` enum, constants and
//!   methods are registered generically from
//!   [`php_dom::classes::class_table`] (stub signatures, so the VM performs
//!   PHP's argument parsing); every method goes through one handler
//!   ([`methods::method`]) that dispatches by name to
//!   [`php_dom::dispatch`].
//! * [`state`] holds the native state of DOM objects and the node →
//!   object identity cache (one PHP object per node while referenced).
//! * [`hooks`] implements the object handlers: virtual properties,
//!   `var_dump` shapes, dimensions/count/iteration of collections, clone.
//! * [`host`] implements [`php_dom::DomHost`]: `XPath` PHP callbacks,
//!   `registerNodeClass` class checks and stream I/O through php-streams.
//! * [`xsl`] is the DOM provider of `XSLTProcessor` documents.
//!
//! # API for other bindings
//!
//! [`interop`] lets other libxml-based extensions exchange nodes with DOM:
//! [`interop::import_node`] returns the `xmlNode*`/`xmlDoc*` behind a DOM
//! object (with a keep-alive handle and its API family), and the unsafe
//! [`interop::node_object`] turns an `xmlNode*` into its PHP DOM object
//! (existing object if any, else the class php-src would create;
//! legacy/modern by document kind). Consumers: `XMLReader::expand()`,
//! `simplexml_import_dom()`/`dom_import_simplexml()`, XSL.

// Handlers follow the VM's `(vm, o, a, …)` naming convention.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::many_single_char_names)]

mod errors;
mod hooks;
mod host;
pub mod interop;
mod methods;
mod run;
mod state;
mod xsl;

use std::collections::HashSet;

use php_dom::classes::{class_table, ClassInfo, ClassKind, MethodInfo};
use php_runtime::{Value, ZStr};
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, Extension, ModuleBuilder, VmResult};
use php_vm::func::flags;
use php_vm::Vm;

/// The `dom` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Dom;

/// Classes with `@strict-properties` (no dynamic properties).
const STRICT: &[&str] = &[
    "Dom\\Implementation",
    "Dom\\Node",
    "Dom\\TokenList",
    "Dom\\NamespaceInfo",
];

/// Classes whose objects cannot be cloned (`clone_obj = NULL`).
const NO_CLONE: &[&str] = &[
    "DOMNodeList",
    "DOMNamedNodeMap",
    "Dom\\NodeList",
    "Dom\\NamedNodeMap",
    "Dom\\DtdNamedNodeMap",
    "Dom\\HTMLCollection",
    "Dom\\Implementation",
    "Dom\\TokenList",
    "DOMXPath",
    "Dom\\XPath",
];

/// Collection classes (dimension/count/iteration handlers).
const COLLECTIONS: &[&str] = &[
    "DOMNodeList",
    "DOMNamedNodeMap",
    "Dom\\NodeList",
    "Dom\\NamedNodeMap",
    "Dom\\DtdNamedNodeMap",
    "Dom\\HTMLCollection",
    "Dom\\TokenList",
];

/// Leaks a registration-time string (signatures are `'static` in the VM;
/// this happens once per process).
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The stub signature of a method. `untyped` drops parameter types (for
/// methods whose C implementation checks types itself, `Z_PARAM_VARIADIC('*')`).
fn signature(m: &MethodInfo, untyped: bool) -> &'static str {
    let mut s = String::from(m.name);
    s.push('(');
    for (i, p) in m.params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        if let Some(t) = p.ty.filter(|_| !untyped) {
            s.push_str(t);
            s.push(' ');
        }
        if p.by_ref {
            s.push('&');
        }
        if p.variadic {
            s.push_str("...");
        }
        s.push('$');
        s.push_str(p.name);
        if let Some(d) = p.default {
            s.push_str(" = ");
            s.push_str(d);
        }
    }
    s.push(')');
    if let Some(r) = m.return_type {
        s.push_str(": ");
        s.push_str(r);
    }
    leak(s)
}

/// A property default given as PHP source.
fn default_value(src: &str) -> Value {
    match src {
        "null" => Value::Null,
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        s => s.parse::<i64>().map_or_else(
            |_| Value::Str(ZStr::from(s.trim_matches('"').trim_matches('\''))),
            Value::Int,
        ),
    }
}

fn register_class(vm: &mut Vm, c: &ClassInfo) {
    let mut b = ClassBuilder::new(c.name);
    let mut fl = 0;
    match c.kind {
        ClassKind::Interface => fl |= class_flags::INTERFACE,
        ClassKind::Enum => {
            b = b.backed_enum(c.enum_backing.unwrap_or("string"));
            for (name, v) in c.enum_cases {
                b = b.case(name, Some(Value::Str(ZStr::from(*v))));
            }
        }
        ClassKind::Class => {}
    }
    if c.is_abstract {
        fl |= class_flags::ABSTRACT;
    }
    if c.is_final {
        fl |= class_flags::FINAL;
    }
    if c.is_readonly {
        fl |= class_flags::READONLY;
    }
    if c.not_serializable {
        fl |= class_flags::NOT_SERIALIZABLE;
    }
    if STRICT.contains(&c.name) {
        fl |= class_flags::NO_DYNAMIC_PROPERTIES;
    }
    if NO_CLONE.contains(&c.name) {
        fl |= class_flags::NO_CLONE;
    }
    b = b.flags(fl);
    if let Some(p) = c.parent {
        b = b.extends(vm, p);
    }
    if !c.interfaces.is_empty() {
        b = b.implements(vm, c.interfaces);
    }
    for k in c.constants {
        b = b.constant(k.name, Value::Int(k.value));
    }
    for p in c.properties {
        let mut pf = match p.visibility {
            "private" => acc::PRIVATE,
            "protected" => acc::PROTECTED,
            _ => acc::PUBLIC,
        };
        if p.set_visibility == Some("private") {
            pf |= flags::SET_PRIVATE;
        } else if p.set_visibility == Some("protected") {
            pf |= flags::SET_PROTECTED;
        }
        if p.readonly || c.is_readonly {
            pf |= acc::READONLY;
        }
        if p.is_virtual {
            pf |= flags::VIRTUAL;
        }
        let def = if p.is_virtual {
            None
        } else {
            p.default.map(default_value)
        };
        b = b.prop(p.name, pf, p.ty, def);
    }
    for m in c.methods {
        let mut mf = match m.visibility {
            "private" => acc::PRIVATE,
            "protected" => acc::PROTECTED,
            _ => acc::PUBLIC,
        };
        if m.is_static {
            mf |= acc::STATIC;
        }
        if m.is_final {
            mf |= acc::FINAL;
        }
        let untyped = c.name == "Dom\\TokenList" && matches!(m.name, "add" | "remove");
        let sig = signature(m, untyped);
        if m.is_abstract || c.kind == ClassKind::Interface {
            b = b.abstract_method(sig, mf);
        } else {
            b = b.method(sig, mf, methods::method);
        }
    }
    if c.kind == ClassKind::Class && c.name != "DOMException" && c.name != "Dom\\NamespaceInfo" {
        let h = if COLLECTIONS.contains(&c.name) {
            hooks::collection_hooks()
        } else {
            hooks::object_hooks()
        };
        b = b.hooks(h);
    }
    b.register(vm);
}

/// Registers every class in dependency order.
fn register_classes(vm: &mut Vm) {
    let table = class_table();
    let local: HashSet<&str> = table.iter().map(|c| c.name).collect();
    let mut done: HashSet<&str> = HashSet::new();
    let mut progress = true;
    while progress && done.len() < table.len() {
        progress = false;
        for c in table {
            if done.contains(c.name) {
                continue;
            }
            let deps_ok = c
                .parent
                .iter()
                .chain(c.interfaces.iter())
                .all(|d| !local.contains(d) || done.contains(d));
            if deps_ok {
                register_class(vm, c);
                done.insert(c.name);
                progress = true;
            }
        }
    }
    // `Dom\DOMException` is an alias of `DOMException`.
    if let Some(cls) = vm.find_class(b"DOMException") {
        vm.classes.insert(ZStr::from("dom\\domexception"), cls);
    }
}

/// `dom_import_simplexml()`.
fn import_simplexml(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    xsl::import_simplexml(vm, a)
}

impl Extension for Dom {
    fn name(&self) -> &'static str {
        "dom"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        run::ensure_installed();
        register_classes(m.vm());
        for (name, v) in php_dom::constants::GLOBAL_CONSTANTS {
            if *name == "DOM_PHP_ERR" {
                m.constant_deprecated(name, Value::Int(*v), "8.4", "as it is no longer used");
            } else {
                m.constant(name, Value::Int(*v));
            }
        }
        for (name, v) in php_dom::constants::DOM_NS_CONSTANTS {
            m.constant(name, Value::Int(*v));
        }
        m.function_static(
            "dom_import_simplexml(object $node): DOMAttr|DOMElement",
            import_simplexml,
        );
        m.function_static(
            "Dom\\import_simplexml(object $node): Dom\\Attr|Dom\\Element",
            import_simplexml,
        );
        php_ext_xsl::access::register(&xsl::ACCESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_stub_like() {
        let (_, m) = php_dom::classes::find_method("DOMDocument", "createElement").expect("m");
        assert_eq!(
            signature(m, false),
            "createElement(string $localName, string $value = \"\")"
        );
        let (_, m) = php_dom::classes::find_method("Dom\\Element", "getAttribute").expect("m");
        assert_eq!(
            signature(m, false),
            "getAttribute(string $qualifiedName): ?string"
        );
    }

    #[test]
    fn defaults() {
        assert!(matches!(default_value("0"), Value::Int(0)));
        assert!(matches!(default_value("null"), Value::Null));
        assert!(matches!(default_value("\"x\""), Value::Str(_)));
    }

    #[test]
    fn class_lists_are_known() {
        for n in STRICT.iter().chain(NO_CLONE).chain(COLLECTIONS) {
            assert!(php_dom::classes::find_class(n).is_some(), "{n}");
        }
    }
}
