// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `openssl`
//! extension. Expected outputs were checked against `/usr/bin/php`
//! (except for master-only behaviour such as the `*_free` deprecations).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let ext = php_ext_openssl::OpenSsl;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

/// A self-signed EC certificate `$cert` with key `$k`.
const SETUP: &str = r#"
$k = openssl_pkey_new(["private_key_type" => OPENSSL_KEYTYPE_EC, "curve_name" => "prime256v1"]);
$csr = openssl_csr_new(["commonName" => "test"], $k, ["digest_alg" => "sha256"]);
$cert = openssl_csr_sign($csr, null, $k, 1, ["digest_alg" => "sha256"], 42);
"#;

#[test]
fn symmetric_roundtrip() {
    assert_eq!(
        run(
            r#"$c = openssl_encrypt("hello", "aes-128-cbc", "key", 0, "1234567890123456");
            echo $c, " ", openssl_decrypt($c, "aes-128-cbc", "key", 0, "1234567890123456");"#
        ),
        "iYraAmTL1AqP8TpWeyOn+A== hello"
    );
    assert_eq!(
        run(r#"var_dump(openssl_cipher_iv_length("AES-256-GCM"), openssl_digest("abc", "sha1"));"#),
        "int(12)\nstring(40) \"a9993e364706816aba3e25717850c26c9cd0d89d\"\n"
    );
}

#[test]
fn aead_tag_by_reference() {
    assert_eq!(
        run(
            r#"$c = openssl_encrypt("x", "aes-128-gcm", str_repeat("k", 16), OPENSSL_RAW_DATA, str_repeat("i", 12), $tag);
            echo strlen($tag), " ", openssl_decrypt($c, "aes-128-gcm", str_repeat("k", 16), OPENSSL_RAW_DATA, str_repeat("i", 12), $tag);
            $t = "untouched"; openssl_encrypt("x", "aes-128-cbc", "k", 0, str_repeat("i", 16), $t); var_dump($t);"#
        ),
        "16 xNULL\n"
    );
}

#[test]
fn objects_are_opaque() {
    let out = run(&format!(
        r#"{SETUP} var_dump($k, $csr, $cert);
        try {{ new OpenSSLCertificate; }} catch (Error $e) {{ echo $e->getMessage(), "\n"; }}
        try {{ clone $k; }} catch (Error $e) {{ echo $e->getMessage(), "\n"; }}
        try {{ serialize($cert); }} catch (Exception $e) {{ echo $e->getMessage(), "\n"; }}
        try {{ $cert->x = 1; }} catch (Error $e) {{ echo $e->getMessage(), "\n"; }}"#
    ));
    assert_eq!(
        out,
        "object(OpenSSLAsymmetricKey)#1 (0) {\n}\nobject(OpenSSLCertificateSigningRequest)#2 (0) {\n}\nobject(OpenSSLCertificate)#3 (0) {\n}\n\
         Cannot directly construct OpenSSLCertificate, use openssl_x509_read() instead\n\
         Trying to clone an uncloneable object of class OpenSSLAsymmetricKey\n\
         Serialization of 'OpenSSLCertificate' is not allowed\n\
         Cannot create dynamic property OpenSSLCertificate::$x\n"
    );
}

#[test]
fn certificate_and_signatures() {
    let out = run(&format!(
        r#"{SETUP}
        $p = openssl_x509_parse($cert);
        var_dump($p["subject"]["CN"], $p["serialNumber"]);
        var_dump(openssl_x509_check_private_key($cert, $k), openssl_x509_verify($cert, $cert));
        var_dump(openssl_sign("data", $sig, $k, "sha256"), openssl_verify("data", $sig, $cert, OPENSSL_ALGO_SHA256));
        var_dump(openssl_x509_export($cert, $pem), openssl_x509_read($pem) instanceof OpenSSLCertificate);
        var_dump(openssl_pkey_get_details(openssl_pkey_get_public($cert))["bits"]);"#
    ));
    assert_eq!(
        out,
        "string(4) \"test\"\nstring(2) \"42\"\nbool(true)\nint(1)\nbool(true)\nint(1)\nbool(true)\nbool(true)\nint(256)\n"
    );
}

#[test]
fn csr_new_assigns_generated_key() {
    assert_eq!(
        run(r#"$key = null;
            $csr = openssl_csr_new(["commonName" => "x"], $key, ["private_key_type" => OPENSSL_KEYTYPE_EC, "curve_name" => "prime256v1"]);
            var_dump($key instanceof OpenSSLAsymmetricKey, openssl_csr_get_subject($csr)["CN"]);"#),
        "bool(true)\nstring(1) \"x\"\n"
    );
}

#[test]
fn key_arrays_and_errors() {
    assert_eq!(
        run(r#"try { openssl_pkey_get_private([1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
            var_dump(openssl_pkey_get_private("garbage"));
            while ($e = openssl_error_string()) { $n = ($n ?? 0) + 1; }
            var_dump($n > 0, openssl_error_string());"#),
        "Key array must be of the form array(0 => key, 1 => phrase)\nbool(false)\nbool(true)\nbool(false)\n"
    );
}

#[test]
fn null_bytes_in_paths() {
    assert_eq!(
        run(r#"try { openssl_x509_read("file://a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
            try { openssl_pkey_export_to_file("k", "a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }"#),
        "\nWarning: openssl_x509_read(): X.509 Certificate cannot be retrieved in test.php on line 1\n\
         openssl_x509_read(): Argument #1 ($certificate) must not contain any null bytes\n\
         openssl_pkey_export_to_file(): Argument #2 ($output_filename) must not contain any null bytes\n"
    );
}

#[test]
fn seal_and_open() {
    let out = run(&format!(
        r#"{SETUP}
        $rsa = openssl_pkey_new(["private_key_bits" => 1024]);
        $pub = openssl_pkey_get_public(openssl_pkey_get_details($rsa)["key"]);
        var_dump(openssl_seal("secret", $sealed, $ekeys, [$pub], "aes-128-cbc", $iv) > 0, isset($ekeys[0]), strlen($iv));
        var_dump(openssl_open($sealed, $plain, $ekeys[0], $rsa, "aes-128-cbc", $iv), $plain);
        var_dump(openssl_public_encrypt("m", $enc, $pub), openssl_private_decrypt($enc, $dec, $rsa), $dec);"#
    ));
    assert_eq!(
        out,
        "bool(true)\nbool(true)\nint(16)\nbool(true)\nstring(6) \"secret\"\nbool(true)\nbool(true)\nstring(1) \"m\"\n"
    );
}

#[test]
fn random_bytes_strong_result() {
    assert_eq!(
        run(r#"var_dump(strlen(openssl_random_pseudo_bytes(8, $s)), $s);
            try { openssl_random_pseudo_bytes(0, $t); } catch (ValueError $e) { echo $e->getMessage(), "\n"; var_dump($t); }"#),
        "int(8)\nbool(true)\nopenssl_random_pseudo_bytes(): Argument #1 ($length) must be greater than 0\nbool(false)\n"
    );
}

#[test]
fn free_functions_are_deprecated() {
    let out = run(&format!(
        "{SETUP} openssl_pkey_free($k); openssl_free_key($k);"
    ));
    assert!(out.contains("Deprecated: Function openssl_pkey_free() is deprecated since 8.0, as OpenSSLAsymmetricKey objects are freed automatically"));
    assert!(out.contains("Deprecated: Function openssl_free_key() is deprecated since 8.0"));
}

#[test]
fn constants_and_ini() {
    assert_eq!(
        run(
            r#"var_dump(OPENSSL_RAW_DATA, PKCS7_DETACHED, X509_PURPOSE_ANY, ini_get("openssl.cafile"), is_string(OPENSSL_VERSION_TEXT));"#
        ),
        "int(1)\nint(64)\nint(7)\nstring(0) \"\"\nbool(true)\n"
    );
}

#[test]
fn openssl_namespace_classes() {
    assert_eq!(
        run(
            r#"try { Openssl\Session::import("invalid"); } catch (Throwable $e) { echo $e::class, ": ", $e->getMessage(), "\n"; }
            $p = new Openssl\Psk("k", "id"); echo $p->psk, $p->identity, "\n";
            try { new Openssl\Psk(str_repeat("x", 257)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }"#
        ),
        "Openssl\\OpensslException: Failed to import session data\nkid\n\
         Openssl\\Psk::__construct(): Argument #1 ($psk) must not exceed 256 bytes\n"
    );
}
