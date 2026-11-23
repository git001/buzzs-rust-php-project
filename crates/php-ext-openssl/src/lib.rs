// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/openssl` for the phprust VM: binds the `php-openssl` library
//! through the `php_vm::ext` extension API.
//!
//! * every `openssl_*` function of `openssl.stub.php` (aliases
//!   `openssl_get_publickey`, `openssl_get_privatekey`, `openssl_free_key`
//!   included), with PHP's argument handling: union `Class|string`
//!   parameters, the untyped key parameters (`[key, passphrase]` arrays,
//!   `file://` paths, PEM strings, `__toString` objects), by-reference
//!   outputs and the deprecations of the `*_free` functions;
//! * the final classes `OpenSSLCertificate`, `OpenSSLCertificateSigningRequest`
//!   and `OpenSSLAsymmetricKey` (see [`objects`]);
//! * all `OPENSSL_*` / `PKCS7_*` / `X509_PURPOSE_*` constants with the
//!   values of the linked library, and the `openssl.cafile` /
//!   `openssl.capath` INI directives.
//!
//! The library's per-request state ([`php_openssl::OpenSsl`]: the error
//! ring buffer behind `openssl_error_string()`) lives in a thread local
//! that is reset at request shutdown. After every library call the
//! diagnostics it queued are raised as `E_WARNING` / `E_DEPRECATED` /
//! `E_NOTICE` with the usual `func(): ` prefix, and its [`Throw`] values
//! become `ValueError` / `Exception` throwables.
//!
//! The master-only `Openssl\OpensslException`, `Openssl\Psk` and
//! `Openssl\Session` classes are provided too ([`session`]).
//!
//! # TLS stream transports
//!
//! [`xp`] ports `xp_ssl.c` on the `openssl` crate and plugs it into
//! `php_ext_streams::xport`: the `ssl`, `tls`, `tlsv1.0` … `tlsv1.3`
//! transports (clients and servers, accepted connections),
//! `stream_socket_enable_crypto()` on plain sockets (client and server
//! methods, non-blocking handshakes returning `0`),
//! `stream_socket_get_crypto_status()` and
//! `stream_get_meta_data()["crypto"]`, with the `ssl` context options and
//! PHP's warning texts, session resumption (callbacks and server
//! cache), PSK (TLS 1.2 and 1.3), TLS 1.3 early data and renegotiation
//! rate limiting. Like ext/openssl it also registers `tcp`, so plain TCP
//! sockets are `tcp_socket/ssl` streams whose accepted connections inherit
//! the listener's context.

// Native handlers must match the VM's `NativeFn` signature even when they
// cannot fail.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps)]

mod convert;
mod crypto;
mod keys;
mod objects;
mod session;
mod smime;
mod state;
mod x509;
mod xp;

use php_openssl::constants::{self, ConstValue};
use php_runtime::Value;
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{Extension, ModuleBuilder};

/// The `openssl` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenSsl;

impl Extension for OpenSsl {
    fn name(&self) -> &'static str {
        "openssl"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        objects::register(m);
        session::register(m);
        for (name, value) in constants::all() {
            let v = match value {
                ConstValue::Int(i) => Value::Int(i),
                ConstValue::Str(s) => Value::string(s),
            };
            m.constant(name, v);
        }
        m.ini(IniDirective::new(
            "openssl.libctx",
            Some("custom"),
            Access::PERDIR,
            ModifyHandler::None,
            "openssl",
        ));
        for name in ["openssl.cafile", "openssl.capath"] {
            m.ini(IniDirective::new(
                name,
                None,
                Access::PERDIR,
                ModifyHandler::None,
                "openssl",
            ));
        }
        x509::register(m);
        keys::register(m);
        crypto::register(m);
        smime::register(m);
        xp::register();
        m.on_request_shutdown(state::reset);
    }
}
