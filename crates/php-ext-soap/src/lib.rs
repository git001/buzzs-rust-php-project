// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/soap` for the phprust VM: binds the `php-soap` library (WSDL
//! model, SOAP encoding, client/server state machines, HTTP transport)
//! through the `php_vm::ext` extension API.
//!
//! * [`host`] — the library's `SoapHost` over the VM (objects, calls,
//!   output buffering, `$_SERVER`, sessions, stream contexts) and the
//!   emulation of PHP's SOAP error handler;
//! * [`net`] — the HTTP connector over php-ext-streams socket transports;
//! * [`methods`] — the native methods and functions.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod host;
mod methods;
mod net;

use php_runtime::Value;
use php_soap::classes::PropDef;
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{acc, class_flags, ClassBuilder, ClassHooks, Extension, ModuleBuilder};

/// The `soap` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Soap;

/// A declared property's default value (`None`: uninitialized typed
/// property).
fn prop_default(p: &PropDef) -> Option<Value> {
    Some(match p.default? {
        "false" => Value::Bool(false),
        "true" => Value::Bool(true),
        "0" => Value::Int(0),
        "[]" => Value::Array(php_runtime::ZArray::new()),
        "\"\"" => Value::from(""),
        _ => Value::Null,
    })
}

/// Adds the declared properties of a `php-soap` property table.
fn with_props(mut b: ClassBuilder, props: &'static [PropDef]) -> ClassBuilder {
    for p in props {
        let vis = match p.visibility {
            php_soap::classes::Visibility::Public => acc::PUBLIC,
            php_soap::classes::Visibility::Protected => acc::PROTECTED,
            php_soap::classes::Visibility::Private => acc::PRIVATE,
        };
        b = b.prop(p.name, vis, p.ty, prop_default(p));
    }
    b
}

impl Extension for Soap {
    fn name(&self) -> &'static str {
        "soap"
    }

    // One flat registration table, mirroring `soap.stub.php`.
    #[allow(clippy::too_many_lines)]
    fn register(&self, m: &mut ModuleBuilder<'_>) {
        // INI (`PHP_INI_BEGIN` of soap.c).
        let ini: [(&str, &str, ModifyHandler); 5] = [
            ("soap.wsdl_cache_enabled", "1", ModifyHandler::Bool),
            ("soap.wsdl_cache_dir", "/tmp", ModifyHandler::String),
            ("soap.wsdl_cache_ttl", "86400", ModifyHandler::Long),
            ("soap.wsdl_cache", "1", ModifyHandler::Long),
            ("soap.wsdl_cache_limit", "5", ModifyHandler::Long),
        ];
        for (name, default, handler) in ini {
            m.ini(IniDirective::new(
                name,
                Some(default),
                Access::ALL,
                handler,
                "soap",
            ));
        }

        // Constants.
        for (name, value) in php_soap::consts::php_constants() {
            match php_soap::consts::DEPRECATED_CONSTANTS
                .iter()
                .find(|(n, _, _)| *n == name)
            {
                Some((_, since, msg)) => {
                    m.constant_deprecated(name, value, since, msg);
                }
                None => {
                    m.constant(name, value);
                }
            }
        }

        // Functions.
        m.function_static(
            "use_soap_error_handler(bool $enable = true): bool",
            methods::use_soap_error_handler,
        )
        .function_static("is_soap_fault(mixed $object): bool", methods::is_soap_fault);

        m.on_request_startup(|_| php_soap::globals::request_init());

        let state_hooks = ClassHooks {
            create: Some(methods::create_state),
            ..ClassHooks::default()
        };
        let vm = m.vm();

        ClassBuilder::new("Soap\\Url")
            .flags(
                class_flags::FINAL
                    | class_flags::NOT_SERIALIZABLE
                    | class_flags::NO_CLONE
                    | class_flags::NO_DYNAMIC_PROPERTIES,
            )
            .method("__construct()", acc::PUBLIC, methods::url_construct)
            .register(vm);
        ClassBuilder::new("Soap\\Sdl")
            .flags(
                class_flags::FINAL
                    | class_flags::NOT_SERIALIZABLE
                    | class_flags::NO_CLONE
                    | class_flags::NO_DYNAMIC_PROPERTIES,
            )
            .method("__construct()", acc::PUBLIC, methods::sdl_construct)
            .hooks(state_hooks.clone())
            .register(vm);

        with_props(
            ClassBuilder::new("SoapParam"),
            php_soap::classes::PARAM_PROPERTIES,
        )
        .method(
            "__construct(mixed $data, string $name)",
            acc::PUBLIC,
            methods::param_construct,
        )
        .register(vm);

        with_props(ClassBuilder::new("SoapHeader"), php_soap::classes::HEADER_PROPERTIES)
            .method(
                "__construct(string $namespace, string $name, mixed $data = UNKNOWN, bool $mustUnderstand = false, string|int|null $actor = null)",
                acc::PUBLIC,
                methods::header_construct,
            )
            .register(vm);

        with_props(
            ClassBuilder::new("SoapFault").extends(vm, "Exception"),
            php_soap::classes::FAULT_PROPERTIES,
        )
        .method(
            "__construct(array|string|null $code, string $string, ?string $actor = null, mixed $details = null, ?string $name = null, mixed $headerFault = null, string $lang = \"\")",
            acc::PUBLIC,
            methods::fault_construct,
        )
        .method("__toString(): string", acc::PUBLIC, methods::fault_to_string)
        .register(vm);

        with_props(ClassBuilder::new("SoapVar"), php_soap::classes::VAR_PROPERTIES)
            .method(
                "__construct(mixed $data, ?int $encoding, ?string $typeName = null, ?string $typeNamespace = null, ?string $nodeName = null, ?string $nodeNamespace = null)",
                acc::PUBLIC,
                methods::var_construct,
            )
            .register(vm);

        with_props(ClassBuilder::new("SoapServer"), php_soap::classes::SERVER_PROPERTIES)
            .flags(class_flags::NO_CLONE)
            .hooks(state_hooks.clone())
            .method(
                "__construct(?string $wsdl, array $options = [])",
                acc::PUBLIC,
                methods::server_construct,
            )
            .method(
                "fault(string $code, string $string, string $actor = UNKNOWN, mixed $details = null, string $name = UNKNOWN, string $lang = \"\"): void",
                acc::PUBLIC,
                methods::server_fault,
            )
            .method(
                "addSoapHeader(SoapHeader $header): void",
                acc::PUBLIC,
                methods::server_add_soap_header,
            )
            .method(
                "setPersistence(int $mode): void",
                acc::PUBLIC,
                methods::server_set_persistence,
            )
            .method(
                "setClass(string $class, mixed ...$args): void",
                acc::PUBLIC,
                methods::server_set_class,
            )
            .method(
                "setObject(object $object): void",
                acc::PUBLIC,
                methods::server_set_object,
            )
            .method(
                "getFunctions(): array",
                acc::PUBLIC,
                methods::server_get_functions,
            )
            .method(
                "addFunction($functions): void",
                acc::PUBLIC,
                methods::server_add_function,
            )
            .method(
                "handle(?string $request = null): void",
                acc::PUBLIC,
                methods::server_handle,
            )
            .method(
                "__getLastResponse(): ?string",
                acc::PUBLIC,
                methods::server_get_last_response,
            )
            .register(vm);

        with_props(ClassBuilder::new("SoapClient"), php_soap::classes::CLIENT_PROPERTIES)
            .flags(class_flags::NO_CLONE)
            .hooks(state_hooks)
            .method(
                "__construct(?string $wsdl, array $options = [])",
                acc::PUBLIC,
                methods::client_construct,
            )
            .method("__call(mixed ...$args): mixed", acc::PUBLIC, methods::client_call)
            .method(
                "__soapCall(string $name, array $args, ?array $options = null, $inputHeaders = null, &$outputHeaders = null): mixed",
                acc::PUBLIC,
                methods::client_soap_call,
            )
            .method(
                "__getFunctions(): ?array",
                acc::PUBLIC,
                methods::client_get_functions,
            )
            .method("__getTypes(): ?array", acc::PUBLIC, methods::client_get_types)
            .method(
                "__getLastRequest(): ?string",
                acc::PUBLIC,
                methods::client_get_last_request,
            )
            .method(
                "__getLastResponse(): ?string",
                acc::PUBLIC,
                methods::client_get_last_response,
            )
            .method(
                "__getLastRequestHeaders(): ?string",
                acc::PUBLIC,
                methods::client_get_last_request_headers,
            )
            .method(
                "__getLastResponseHeaders(): ?string",
                acc::PUBLIC,
                methods::client_get_last_response_headers,
            )
            .method(
                "__doRequest(string $request, string $location, string $action, int $version, bool $oneWay = false, ?string $uriParserClass = null): ?string",
                acc::PUBLIC,
                methods::client_do_request,
            )
            .method(
                "__setCookie(string $name, ?string $value = null): void",
                acc::PUBLIC,
                methods::client_set_cookie,
            )
            .method("__getCookies(): array", acc::PUBLIC, methods::client_get_cookies)
            .method(
                "__setSoapHeaders($headers = null): bool",
                acc::PUBLIC,
                methods::client_set_soap_headers,
            )
            .method(
                "__setLocation(?string $location = null): ?string",
                acc::PUBLIC,
                methods::client_set_location,
            )
            .register(vm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prop_defaults() {
        let p = php_soap::classes::CLIENT_PROPERTIES;
        assert!(matches!(prop_default(&p[0]), Some(Value::Null)));
        assert!(matches!(prop_default(&p[4]), Some(Value::Bool(false))));
        let soap_version = p.iter().find(|p| p.name == "_soap_version");
        assert!(soap_version.is_some_and(|p| prop_default(p).is_none()));
    }
}
