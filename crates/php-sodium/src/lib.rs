// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/sodium` for phprust, bound to the system libsodium (1.0.18).
//!
//! Every `sodium_*` PHP function maps to a Rust function of the same name
//! without the `sodium_` prefix, grouped by family:
//!
//! * [`util`] — `sodium_memzero/increment/add/memcmp/compare`,
//!   `sodium_bin2hex/hex2bin/bin2base64/base642bin`, `sodium_pad/unpad`.
//! * [`aead`] — `crypto_aead_{aes256gcm,chacha20poly1305,
//!   chacha20poly1305_ietf,xchacha20poly1305_ietf}_*`.
//! * [`hash`] — `crypto_auth*`, `crypto_shorthash*`, `crypto_generichash*`
//!   (incl. the opaque-string streaming state), `crypto_kdf*`.
//! * [`boxes`] — `crypto_box*` (incl. seal), `crypto_kx*`,
//!   `crypto_scalarmult*`.
//! * [`secretbox`] — `crypto_secretbox*`, `crypto_stream*` (incl.
//!   `XChaCha20` and `xor_ic`).
//! * [`sign`] — `crypto_sign*` (incl. detached, seed keypairs and the
//!   Ed25519→Curve25519 conversions).
//! * [`secretstream`] — `crypto_secretstream_xchacha20poly1305_*`.
//! * [`pwhash`] — `crypto_pwhash*` (Argon2i/Argon2id and
//!   scrypt-salsa208-sha256) and the argon2 `password_hash()` backend
//!   ext/sodium registers when the core lacks libargon2.
//! * [`ristretto`] — `crypto_core_ristretto255_*`,
//!   `crypto_scalarmult_ristretto255*`.
//! * [`consts`] — the constants and the library version.
//!
//! # Conventions
//!
//! Strings are bytes in, bytes out.  Failures are [`Error`] values with
//! PHP's complete message (`SodiumException` or `ValueError`); the VM
//! still performs ordinary parameter parsing (types, arity) itself.
//! `string|false` results are `Option`s; array results are tuples.
//! Functions taking a string by reference (`sodium_add`, `sodium_increment`,
//! `sodium_memzero`, the generichash/secretstream `$state`) take
//! `&mut Vec<u8>`/`&mut [u8]`; the VM separates the string first, raises
//! [`error::not_a_state_reference`] / [`error::STRING_REQUIRED`] when the
//! variable is not a string, and, where PHP does, assigns `null` afterwards
//! (`sodium_memzero`, `sodium_crypto_generichash_final`).
//!
//! The few `E_WARNING`s (`"empty password"`, `"wrong size for the hashed
//! password"`) are raised by PHP with `zend_error()`, i.e. *without* a
//! `function(): ` prefix; they are handed to a `warn` callback.
//!
//! # Availability
//!
//! libsodium 1.0.18 has no AEGIS, `IPCrypt`, XOF (SHAKE/`TurboSHAKE`) or
//! ML-KEM, so — exactly like php-src built against it — those functions and
//! constants do not exist.  AES-256-GCM is compiled in (x86-64/aarch64) but
//! only usable when [`aead::aes256gcm_is_available`] says so.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod ffi;

pub mod aead;
pub mod boxes;
pub mod consts;
pub mod error;
pub mod hash;
pub mod pwhash;
pub mod ristretto;
pub mod secretbox;
pub mod secretstream;
pub mod sign;
pub mod util;

pub use error::{Error, Result};

use std::sync::Once;

/// Every PHP function ext/sodium defines against libsodium 1.0.18, in
/// registration order (as `get_extension_funcs('sodium')` lists them).
pub const FUNCTIONS: &[&str] = &[
    "sodium_crypto_aead_aes256gcm_is_available",
    "sodium_crypto_aead_aes256gcm_decrypt",
    "sodium_crypto_aead_aes256gcm_encrypt",
    "sodium_crypto_aead_aes256gcm_keygen",
    "sodium_crypto_aead_chacha20poly1305_decrypt",
    "sodium_crypto_aead_chacha20poly1305_encrypt",
    "sodium_crypto_aead_chacha20poly1305_keygen",
    "sodium_crypto_aead_chacha20poly1305_ietf_decrypt",
    "sodium_crypto_aead_chacha20poly1305_ietf_encrypt",
    "sodium_crypto_aead_chacha20poly1305_ietf_keygen",
    "sodium_crypto_aead_xchacha20poly1305_ietf_decrypt",
    "sodium_crypto_aead_xchacha20poly1305_ietf_keygen",
    "sodium_crypto_aead_xchacha20poly1305_ietf_encrypt",
    "sodium_crypto_auth",
    "sodium_crypto_auth_keygen",
    "sodium_crypto_auth_verify",
    "sodium_crypto_box",
    "sodium_crypto_box_keypair",
    "sodium_crypto_box_seed_keypair",
    "sodium_crypto_box_keypair_from_secretkey_and_publickey",
    "sodium_crypto_box_open",
    "sodium_crypto_box_publickey",
    "sodium_crypto_box_publickey_from_secretkey",
    "sodium_crypto_box_seal",
    "sodium_crypto_box_seal_open",
    "sodium_crypto_box_secretkey",
    "sodium_crypto_core_ristretto255_add",
    "sodium_crypto_core_ristretto255_from_hash",
    "sodium_crypto_core_ristretto255_is_valid_point",
    "sodium_crypto_core_ristretto255_random",
    "sodium_crypto_core_ristretto255_scalar_add",
    "sodium_crypto_core_ristretto255_scalar_complement",
    "sodium_crypto_core_ristretto255_scalar_invert",
    "sodium_crypto_core_ristretto255_scalar_mul",
    "sodium_crypto_core_ristretto255_scalar_negate",
    "sodium_crypto_core_ristretto255_scalar_random",
    "sodium_crypto_core_ristretto255_scalar_reduce",
    "sodium_crypto_core_ristretto255_scalar_sub",
    "sodium_crypto_core_ristretto255_sub",
    "sodium_crypto_kx_keypair",
    "sodium_crypto_kx_publickey",
    "sodium_crypto_kx_secretkey",
    "sodium_crypto_kx_seed_keypair",
    "sodium_crypto_kx_client_session_keys",
    "sodium_crypto_kx_server_session_keys",
    "sodium_crypto_generichash",
    "sodium_crypto_generichash_keygen",
    "sodium_crypto_generichash_init",
    "sodium_crypto_generichash_update",
    "sodium_crypto_generichash_final",
    "sodium_crypto_kdf_derive_from_key",
    "sodium_crypto_kdf_keygen",
    "sodium_crypto_pwhash",
    "sodium_crypto_pwhash_str",
    "sodium_crypto_pwhash_str_verify",
    "sodium_crypto_pwhash_str_needs_rehash",
    "sodium_crypto_pwhash_scryptsalsa208sha256",
    "sodium_crypto_pwhash_scryptsalsa208sha256_str",
    "sodium_crypto_pwhash_scryptsalsa208sha256_str_verify",
    "sodium_crypto_scalarmult",
    "sodium_crypto_scalarmult_ristretto255",
    "sodium_crypto_scalarmult_ristretto255_base",
    "sodium_crypto_secretbox",
    "sodium_crypto_secretbox_keygen",
    "sodium_crypto_secretbox_open",
    "sodium_crypto_secretstream_xchacha20poly1305_keygen",
    "sodium_crypto_secretstream_xchacha20poly1305_init_push",
    "sodium_crypto_secretstream_xchacha20poly1305_push",
    "sodium_crypto_secretstream_xchacha20poly1305_init_pull",
    "sodium_crypto_secretstream_xchacha20poly1305_pull",
    "sodium_crypto_secretstream_xchacha20poly1305_rekey",
    "sodium_crypto_shorthash",
    "sodium_crypto_shorthash_keygen",
    "sodium_crypto_sign",
    "sodium_crypto_sign_detached",
    "sodium_crypto_sign_ed25519_pk_to_curve25519",
    "sodium_crypto_sign_ed25519_sk_to_curve25519",
    "sodium_crypto_sign_keypair",
    "sodium_crypto_sign_keypair_from_secretkey_and_publickey",
    "sodium_crypto_sign_open",
    "sodium_crypto_sign_publickey",
    "sodium_crypto_sign_secretkey",
    "sodium_crypto_sign_publickey_from_secretkey",
    "sodium_crypto_sign_seed_keypair",
    "sodium_crypto_sign_verify_detached",
    "sodium_crypto_stream",
    "sodium_crypto_stream_keygen",
    "sodium_crypto_stream_xor",
    "sodium_crypto_stream_xchacha20",
    "sodium_crypto_stream_xchacha20_keygen",
    "sodium_crypto_stream_xchacha20_xor",
    "sodium_crypto_stream_xchacha20_xor_ic",
    "sodium_add",
    "sodium_compare",
    "sodium_increment",
    "sodium_memcmp",
    "sodium_memzero",
    "sodium_pad",
    "sodium_unpad",
    "sodium_bin2hex",
    "sodium_hex2bin",
    "sodium_bin2base64",
    "sodium_base642bin",
    "sodium_crypto_scalarmult_base",
];

/// `sodium_init()` (PHP's `MINIT`), run once before any other call.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: sodium_init() is thread-safe and idempotent; a negative
        // result (no entropy source) makes PHP refuse to start.
        let rc = unsafe { ffi::sodium_init() };
        assert!(rc >= 0, "sodium_init() failed");
        // The opaque state strings mirror these C structs byte for byte; a
        // library with other layouts would make them unsound to use.
        // SAFETY: plain size queries.
        let sizes = unsafe {
            (
                ffi::crypto_generichash_statebytes(),
                ffi::crypto_secretstream_xchacha20poly1305_statebytes(),
            )
        };
        assert_eq!(
            sizes,
            (hash::GENERICHASH_STATE_BYTES, secretstream::STATE_BYTES),
            "unexpected libsodium state sizes"
        );
    });
}

/// `randombytes_buf()` into a fresh buffer of `len` bytes.
pub(crate) fn random(len: usize) -> Vec<u8> {
    init();
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is valid for `len` writable bytes.
    unsafe { ffi::randombytes_buf(buf.as_mut_ptr(), len) };
    buf
}

/// `ZSTR_MAX_LEN` on 64-bit builds (the `zend_string` header is 24 bytes).
pub(crate) const ZSTR_MAX_LEN: usize = usize::MAX - 24 - 8;

/// A pointer for an optional input buffer: libsodium accepts NULL for
/// zero-length inputs, and `[].as_ptr()` is a dangling (but non-null)
/// pointer which it never dereferences.
pub(crate) fn ptr(buf: &[u8]) -> *const u8 {
    buf.as_ptr()
}

/// A NUL-terminated copy of `bytes`, cut at the first NUL like a PHP
/// string handed to a `char *` C API.
pub(crate) fn c_string(bytes: &[u8]) -> Vec<u8> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let mut v = Vec::with_capacity(end + 1);
    v.extend_from_slice(&bytes[..end]);
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn helpers() {
        assert_eq!(super::c_string(b"ab\0cd"), b"ab\0");
        assert_eq!(super::c_string(b""), b"\0");
        assert_eq!(super::random(17).len(), 17);
        assert_ne!(super::random(32), super::random(32));
        assert_eq!(super::FUNCTIONS.len(), 104);
    }
}
