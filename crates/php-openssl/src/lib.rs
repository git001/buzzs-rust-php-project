// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/openssl` for phprust, built on the system OpenSSL 3.x
//! (the same libcrypto `/usr/bin/php` links against, so ciphertexts,
//! signatures and error strings match byte for byte).
//!
//! # Architecture
//!
//! * [`OpenSsl`] is the per-request extension state: PHP's private error
//!   ring buffer behind `openssl_error_string()` and the diagnostics
//!   (warnings / deprecations) raised by the last call. Every
//!   `openssl_*` function is a method on it.
//! * Methods return [`PhpResult`]: `Err` means an exception is thrown
//!   (`ValueError`, …); `Ok` carries the PHP return value (`Option`/`bool`
//!   where PHP returns `false`, plus any by-reference outputs). After every
//!   call the VM drains [`OpenSsl::take_diagnostics`] and raises them as
//!   `E_WARNING`/`E_DEPRECATED` prefixed with `"<function>(): "`.
//! * The three PHP classes are plain Rust values: [`Certificate`]
//!   (`OpenSSLCertificate`), [`CertificateSigningRequest`]
//!   (`OpenSSLCertificateSigningRequest`) and [`AsymmetricKey`]
//!   (`OpenSSLAsymmetricKey`). Parameters accepting "object or string" are
//!   modelled by [`CertArg`], [`CsrArg`] and [`KeyArg`].
//! * Arrays travel as [`PhpArray`] / [`PhpValue`].
//! * All `unsafe` lives in [`ffi`].
//!
//! The TLS stream transport (option table, state machine and a client
//! handshake over any `Read + Write`) lives in [`tls`].
//!
//! # Function map (for the VM bindings)
//!
//! | PHP | Rust |
//! |---|---|
//! | `openssl_encrypt` / `openssl_decrypt` | [`OpenSsl::encrypt`] / [`OpenSsl::decrypt`] |
//! | `openssl_cipher_iv_length` / `_key_length` | [`OpenSsl::cipher_iv_length`] / [`OpenSsl::cipher_key_length`] |
//! | `openssl_digest`, `openssl_pbkdf2`, `openssl_random_pseudo_bytes` | [`OpenSsl::digest`], [`OpenSsl::pbkdf2`], [`OpenSsl::random_pseudo_bytes`] |
//! | `openssl_get_md_methods` / `_cipher_methods` / `_curve_names` / `openssl_get_cert_locations` | `get_*` methods |
//! | `openssl_pkey_new` / `_export` / `_export_to_file` / `_get_public` (`openssl_get_publickey`) / `_get_private` (`openssl_get_privatekey`) / `_get_details` / `_free` (`openssl_free_key`) / `_derive`, `openssl_dh_compute_key` | `pkey_*`, [`OpenSsl::dh_compute_key`] |
//! | `openssl_x509_*` | `x509_*` |
//! | `openssl_csr_*` | `csr_*` |
//! | `openssl_{private,public}_{encrypt,decrypt}`, `openssl_sign`, `openssl_verify`, `openssl_seal`, `openssl_open` | same names |
//! | `openssl_pkcs7_*`, `openssl_cms_*`, `openssl_pkcs12_*`, `openssl_spki_*` | same names |
//! | `openssl_error_string` | [`OpenSsl::error_string`] |
//!
//! Argument-count/type errors (`zpp`) are the VM's job, as is converting
//! zvals into [`KeyArg`]/[`CertArg`]/[`CsrArg`] (objects with `__toString`
//! become strings; non-string scalars become [`KeyArg::Other`] for keys and
//! are stringified for certificates). The three classes are final, not
//! serializable, not cloneable, not comparable, and their constructors throw
//! `Error` with [`CONSTRUCT_ERRORS`].

// The ports deliberately mirror php-src's C functions (one Rust function
// per C function, same variable names such as `n`/`e`/`d`, `dmp1`/`dmq1`)
// so they can be reviewed side by side with the reference.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::struct_field_names
)]

pub mod args;
pub mod asym;
pub mod cms;
pub mod config;
pub mod constants;
pub mod csr;
pub mod error;
pub mod ffi;
pub mod pkcs12;
pub mod pkcs7;
pub mod pkey;
pub mod spki;
pub mod symmetric;
pub mod tls;
pub mod util;
pub mod value;
pub mod x509;

pub use args::{AsymmetricKey, CertArg, Certificate, CertificateSigningRequest, CsrArg, KeyArg};
pub use error::{Diagnostic, ErrorQueue, Level, PhpResult, Throw};
pub use value::{BoolOrInt, Key, PhpArray, PhpValue};

/// `Error` messages thrown by the class constructors, by class name.
pub const CONSTRUCT_ERRORS: [(&str, &str); 3] = [
    (
        "OpenSSLCertificate",
        "Cannot directly construct OpenSSLCertificate, use openssl_x509_read() instead",
    ),
    (
        "OpenSSLCertificateSigningRequest",
        "Cannot directly construct OpenSSLCertificateSigningRequest, use openssl_csr_new() instead",
    ),
    (
        "OpenSSLAsymmetricKey",
        "Cannot directly construct OpenSSLAsymmetricKey, use openssl_pkey_new() instead",
    ),
];

/// Per-request state of the openssl extension.
#[derive(Debug)]
pub struct OpenSsl {
    errors: Option<ErrorQueue>,
    errors_mark: Option<ErrorQueue>,
    diagnostics: Vec<Diagnostic>,
    /// The default config file, computed on first use: PHP keeps it in
    /// persistent memory from MINIT, so creating the state for an unrelated
    /// call (e.g. `openssl_x509_read()`) must not leave a request
    /// allocation behind (`memory_get_usage()` deltas, bug #79881).
    default_conf: std::cell::OnceCell<Vec<u8>>,
    /// `openssl.cafile` INI value (reported by `openssl_get_cert_locations`).
    pub ini_cafile: Vec<u8>,
    /// `openssl.capath` INI value.
    pub ini_capath: Vec<u8>,
    /// The `open_basedir` INI value (`:`-separated), honoured by path
    /// checks like `php_check_open_basedir`.
    pub open_basedir: Option<Vec<u8>>,
}

impl Default for OpenSsl {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenSsl {
    /// Initialises the library (`OPENSSL_init_ssl(LOAD_CONFIG)`).
    pub fn new() -> Self {
        ffi::init();
        OpenSsl {
            errors: None,
            errors_mark: None,
            diagnostics: Vec::new(),
            default_conf: std::cell::OnceCell::new(),
            ini_cafile: Vec::new(),
            ini_capath: Vec::new(),
            open_basedir: None,
        }
    }

    /// The default configuration file path (`php_openssl_get_conf_filename`),
    /// chosen like PHP's MINIT: `$OPENSSL_CONF`, else `$SSLEAY_CONF`, else
    /// `<X509_get_default_cert_area()>/openssl.cnf`.
    pub fn default_config_file(&self) -> &[u8] {
        self.default_conf.get_or_init(|| {
            std::env::var_os("OPENSSL_CONF")
                .or_else(|| std::env::var_os("SSLEAY_CONF"))
                .map_or_else(
                    || {
                        let area = ffi::default_cert_locations()[5].1.clone();
                        format!("{area}/openssl.cnf").into_bytes()
                    },
                    |v| v.to_string_lossy().into_owned().into_bytes(),
                )
        })
    }

    /// Drains the diagnostics raised since the last call.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    pub(crate) fn warn(&mut self, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Warning,
            message: message.into(),
        });
    }

    pub(crate) fn deprecated(&mut self, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Deprecated,
            message: message.into(),
        });
    }

    /// `php_openssl_store_errors`: moves OpenSSL's thread-local error queue
    /// into PHP's ring buffer.
    pub(crate) fn store_errors(&mut self) {
        let mut code = ffi::err_get_error();
        if code == 0 {
            return;
        }
        let q = self.errors.get_or_insert_with(ErrorQueue::new);
        while code != 0 {
            // PHP stores the code in an `int`.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            q.push(code as u32 as i32);
            code = ffi::err_get_error();
        }
    }

    /// `php_openssl_errors_set_mark`.
    pub(crate) fn errors_set_mark(&mut self) {
        if let Some(e) = self.errors {
            self.errors_mark = Some(e);
        }
    }

    /// `php_openssl_errors_restore_mark`.
    pub(crate) fn errors_restore_mark(&mut self) {
        let Some(e) = self.errors.as_mut() else {
            return;
        };
        match self.errors_mark {
            Some(m) => *e = m,
            None => e.clear(),
        }
    }

    /// `openssl_error_string()`: the oldest queued OpenSSL error message.
    pub fn error_string(&mut self) -> Option<String> {
        self.store_errors();
        let code = self.errors.as_mut()?.pop()?;
        if code == 0 {
            return None;
        }
        // Sign-extend like C's `int` → `unsigned long` conversion.
        #[allow(clippy::cast_sign_loss)]
        let wide = i64::from(code) as u64;
        Some(ffi::err_error_string(wide))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_string_empty() {
        let mut o = OpenSsl::new();
        assert_eq!(o.error_string(), None);
    }

    #[test]
    fn system_errors_are_sign_extended() {
        let mut o = OpenSsl::new();
        assert!(ffi::Bio::file(b"/nonexistent/dir/file", "r").is_none());
        o.store_errors();
        assert_eq!(
            o.error_string().as_deref(),
            Some("error:FFFFFFFF80000002:system library::No such file or directory")
        );
        assert_eq!(
            o.error_string().as_deref(),
            Some("error:10000080:BIO routines::no such file")
        );
        assert_eq!(o.error_string(), None);
    }

    #[test]
    fn mark_restore() {
        let mut o = OpenSsl::new();
        // No queue yet: restore is a no-op.
        o.errors_restore_mark();
        assert!(ffi::Bio::file(b"/nonexistent/x", "r").is_none());
        o.errors_set_mark();
        o.store_errors();
        o.errors_restore_mark();
        assert_eq!(o.error_string(), None);
    }
}
