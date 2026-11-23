// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Asymmetric key handling, checked against `/usr/bin/php` output.

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::*;
use php_openssl::{KeyArg, Level, OpenSsl, PhpArray, PhpValue, Throw};

fn details(o: &mut OpenSsl, k: &php_openssl::AsymmetricKey) -> PhpArray {
    o.pkey_get_details(k).expect("details")
}

fn sub<'a>(a: &'a PhpArray, k: &str) -> &'a PhpArray {
    a.get_str(k)
        .and_then(PhpValue::as_array)
        .expect("sub array")
}

fn s<'a>(a: &'a PhpArray, k: &str) -> &'a [u8] {
    a.get_str(k).and_then(PhpValue::as_str).expect("string")
}

#[test]
fn ec_private_key_details() {
    let mut o = OpenSsl::new();
    let url = fixture_url("private_ec.key");
    let k = o
        .pkey_get_private(&KeyArg::Str(&url), Some(b""))
        .unwrap()
        .unwrap();
    let d = details(&mut o, &k);
    assert_eq!(d.get_str("bits"), Some(&PhpValue::Int(256)));
    assert_eq!(d.get_str("type"), Some(&PhpValue::Int(3)));
    let ec = sub(&d, "ec");
    assert_eq!(s(ec, "curve_name"), b"prime256v1");
    assert_eq!(s(ec, "curve_oid"), b"1.2.840.10045.3.1.7");
    assert_eq!(
        s(ec, "x"),
        unhex("3eae216c8587bc1e75add5abf1e8eb8d6a38a9535656e81816d3e0ff12d0c349")
    );
    assert_eq!(
        s(ec, "d"),
        unhex("b3e4aa87b233b5e0c1892e4a3df4efb96bb2b7d624ae4ba8ca25bfe9b6a0e8d9")
    );
    let keys: Vec<_> = d.iter().map(|(k, _)| format!("{k:?}")).collect();
    assert_eq!(keys.len(), 4);
    let pem = o
        .pkey_export(&KeyArg::Key(&k), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        String::from_utf8(pem).unwrap(),
        "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgs+Sqh7IzteDBiS5K\nPfTvuWuyt9YkrkuoyiW/6bag6NmhRANCAAQ+riFshYe8HnWt1avx6OuNajipU1ZW\n6BgW0+D/EtDDSYeQg9ngO8qyo5M6cyh7ORtKZVUy7DP1+W+eocaZC+a6\n-----END PRIVATE KEY-----\n"
    );
}

#[test]
fn rsa_public_details() {
    let mut o = OpenSsl::new();
    let url = fixture_url("public.key");
    let k = o.pkey_get_public(&KeyArg::Str(&url)).unwrap().unwrap();
    assert!(!k.is_private());
    let d = details(&mut o, &k);
    assert_eq!(d.get_str("bits"), Some(&PhpValue::Int(1024)));
    let rsa = sub(&d, "rsa");
    assert_eq!(rsa.len(), 2);
    assert_eq!(s(rsa, "e"), [1, 0, 1]);
    // From a certificate.
    let url = fixture_url("public.crt");
    let k = o.pkey_get_public(&KeyArg::Str(&url)).unwrap().unwrap();
    assert_eq!(
        details(&mut o, &k).get_str("bits"),
        Some(&PhpValue::Int(2048))
    );
}

#[test]
fn passphrases_and_errors() {
    let mut o = OpenSsl::new();
    let url = fixture_url("private_rsa_2048_pass_php.key");
    assert!(o
        .pkey_get_private(&KeyArg::Str(&url), Some(b"php"))
        .unwrap()
        .is_some());
    assert!(o
        .pkey_get_private(&KeyArg::Str(&url), Some(b"wrong"))
        .unwrap()
        .is_none());
    assert_eq!(
        errors(&mut o),
        [
            "error:1C800064:Provider routines::bad decrypt",
            "error:04800065:PEM routines::bad decrypt"
        ]
    );
    // [key, passphrase] arrays.
    let arr = KeyArg::Array {
        key: Some(Box::new(KeyArg::Str(&url))),
        passphrase: Some(b"php".to_vec()),
    };
    assert!(o.pkey_get_private(&arr, None).unwrap().is_some());
    let bad = KeyArg::Array {
        key: Some(Box::new(KeyArg::Str(&url))),
        passphrase: None,
    };
    assert_eq!(
        o.pkey_get_private(&bad, None).unwrap_err(),
        Throw::ValueError("Key array must be of the form array(0 => key, 1 => phrase)".into())
    );
    assert!(o.pkey_get_private(&KeyArg::Other, None).unwrap().is_none());
    let nul = b"file://foo\0bar".to_vec();
    assert_eq!(
        o.pkey_get_private(&KeyArg::Str(&nul), None).unwrap_err(),
        Throw::ArgumentValueError {
            arg: 1,
            message: "must not contain any null bytes".into()
        }
    );
}

#[test]
fn public_private_mismatch_warnings() {
    let mut o = OpenSsl::new();
    let pub_url = fixture_url("public.key");
    let pk = o.pkey_get_public(&KeyArg::Str(&pub_url)).unwrap().unwrap();
    assert!(o
        .pkey_get_private(&KeyArg::Key(&pk), Some(b""))
        .unwrap()
        .is_none());
    assert_eq!(warnings(&mut o), ["Supplied key param is a public key"]);
    let priv_url = fixture_url("private_rsa_1024.key");
    let sk = o
        .pkey_get_private(&KeyArg::Str(&priv_url), Some(b""))
        .unwrap()
        .unwrap();
    assert!(o.pkey_get_public(&KeyArg::Key(&sk)).unwrap().is_none());
    assert_eq!(
        warnings(&mut o),
        ["Don't know how to get public key from this private key"]
    );
    assert!(o
        .pkey_export(&KeyArg::Key(&pk), None, None)
        .unwrap()
        .is_none());
    assert_eq!(
        warnings(&mut o),
        [
            "Supplied key param is a public key",
            "Cannot get key from parameter 1"
        ]
    );
}

#[test]
fn derive_matches_php_tests() {
    let mut o = OpenSsl::new();
    let priv_pem = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VuBCIEIEhxWrgdpbkAlbNgjSjuqodVy4w/SJzmxlsOlUdXUm5r\n-----END PRIVATE KEY-----\n";
    let pub_pem = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VuAyEAaKrFGJv6L6JrVmkxhtU5+xrPZU2MHNQJcsGZY71WhX8=\n-----END PUBLIC KEY-----\n";
    let r = o
        .pkey_derive(&KeyArg::Str(pub_pem), &KeyArg::Str(priv_pem), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        r,
        unhex("76feeefed1d6bc01f3406d87759db371893ec6dc2cb912a130b33c3c91abcb21")
    );
    assert!(o.take_diagnostics().is_empty());
    let _ = o
        .pkey_derive(&KeyArg::Str(pub_pem), &KeyArg::Str(priv_pem), Some(0))
        .unwrap();
    let d = o.take_diagnostics();
    assert_eq!(d[0].level, Level::Deprecated);
    assert!(matches!(
        o.pkey_derive(&KeyArg::Str(pub_pem), &KeyArg::Str(priv_pem), Some(-1)),
        Err(Throw::ArgumentValueError { arg: 3, .. })
    ));
}

#[test]
fn dh_compute_key_matches_php() {
    let mut o = OpenSsl::new();
    let priv_pem = b"-----BEGIN PRIVATE KEY-----
MIICJgIBADCCARcGCSqGSIb3DQEDATCCAQgCggEBANn6weB11zG7izhfzM4qsITZ
3q/ORkF6+h3RTn7sh8Ji1MpHt3zHcPfdYFvs7V5SJfNN5Xv9L62RN8GwgxwRWIJr
8VBHfL3LyZNMMgnGBGJR0qmoM48iNd8i2ggZYj+H8WVh2y6tGw1YsDI3AFHpZFkN
TvCT1JHl2JfNEgOgSryBO84KDEWLxWaN/4Nqa9x5R0fxKMLjpWNRzEBBKcVeEHIZ
gzl7VKVJEpYC336sjYJE19ZD0O/gWl+q4WeRpDazDi6LDLZgnoDrUgbNAXtDETKL
gKOnYq+iwRWCQicQmaQvGXntmgdriExVacrRnH8o09ioxcVdtPG8WuLeqJczCvsC
AQIEggEEAoIBAH1yv00aZkw/7IIAJL1fZUrpVeO3xKIQDl982HOKS32+o2mUJWbc
DuDMIOvqiUEltEnFQOqDaJue0ucseJdH5Q9JHlSIhuUQiPB/JfEcPlb2QYzXHuAE
fWS94X0wiSxYgKXIL0XceA3yg5bYhDSR3DntdJrbboyYHt/QGQ8WCWiYEa402ovI
x+r7k3BlGxah33HeuqhMCFAfFvWUhLaj85QEmjHTjVMKeeTlNfBS+nscbCcZvLXd
qanvRxYYGdOhgLTcJe/iUsxmAWVTiqrid8MEvtFrenanawTgnPXAp5WtYTCGcsiQ
TBG24ND/tnZpPoPz/Rwlpo1IL4IbvKGRsfU=
-----END PRIVATE KEY-----
";
    let pub_hex = "29ECC536A85A4AFAD7D63E50545C68CE44A834396886E9A7BAC27E3A08A14C05259BA6E9940FDC512457155A60CAFACD8E43897E1B537A282D39697B75357197B5E3AD7F1826C0216604496AFAEF8999FBCE336C148166AE23E77EC66C0611235110FA8D6180A26425154959FACEC18FEE3EDA68E9355627820C14B44B486C9547ECE62BE72D56A7FAC4747AFCF201D8A4155F63A076234D6BC04DE27E7A7849BF8956E3DB6E51C043CB6FC66889ADE1F8DE756E9194838E8EF8A5CF9C0DD553282DE8A3130CA7752C22C191E5C352AC3BD77EF9270BF37BC807BBDB3F39AE7966B013723E71EAF41082A056D994F64B428183C5BAFEE9C7A41CABCEA868FC34";
    let k = o
        .pkey_get_private(&KeyArg::Str(priv_pem), Some(b""))
        .unwrap()
        .unwrap();
    let r = o.dh_compute_key(&unhex(pub_hex), &k).unwrap();
    assert_eq!(
        &php_openssl::util::bin2hex(&r)[..32],
        b"b0049944fa5d36f364dd02e675dde50f"
    );
    assert_eq!(r.len(), 256);
    // DH key round-trip via components: details → pkey_new.
    let d = details(&mut o, &k);
    let dh = sub(&d, "dh").clone();
    let mut opts = PhpArray::new();
    opts.set("dh", PhpValue::Array(dh));
    let k2 = o.pkey_new(Some(&opts)).unwrap().unwrap();
    assert!(k2.is_private());
    assert_eq!(o.dh_compute_key(&unhex(pub_hex), &k2).unwrap(), r);
}

#[test]
fn pkey_new_generation_and_errors() {
    let mut o = OpenSsl::new();
    let mut opts = PhpArray::new();
    opts.set("private_key_bits", PhpValue::Int(1024));
    let k = o.pkey_new(Some(&opts)).unwrap().unwrap();
    assert_eq!(
        details(&mut o, &k).get_str("bits"),
        Some(&PhpValue::Int(1024))
    );
    let mut opts = PhpArray::new();
    opts.set("private_key_bits", PhpValue::Int(100));
    assert!(o.pkey_new(Some(&opts)).unwrap().is_none());
    assert_eq!(
        warnings(&mut o),
        ["Private key length must be at least 384 bits, configured to 100"]
    );
    let mut opts = PhpArray::new();
    opts.set("private_key_type", PhpValue::Int(3));
    assert!(o.pkey_new(Some(&opts)).unwrap().is_none());
    assert_eq!(
        warnings(&mut o),
        ["Missing configuration value: \"curve_name\" not set"]
    );
    opts.set("curve_name", PhpValue::str("foo"));
    assert!(o.pkey_new(Some(&opts)).unwrap().is_none());
    assert_eq!(
        warnings(&mut o),
        ["Unknown elliptic curve (short) name foo"]
    );
    opts.set("curve_name", PhpValue::str("secp384r1"));
    let k = o.pkey_new(Some(&opts)).unwrap().unwrap();
    assert_eq!(
        details(&mut o, &k).get_str("bits"),
        Some(&PhpValue::Int(384))
    );
    let mut opts = PhpArray::new();
    opts.set("private_key_type", PhpValue::Int(99));
    assert!(o.pkey_new(Some(&opts)).unwrap().is_none());
    assert_eq!(warnings(&mut o), ["Unsupported private key type"]);
    let mut opts = PhpArray::new();
    opts.set("ec", PhpValue::Array(PhpArray::new()));
    assert!(o.pkey_new(Some(&opts)).unwrap().is_none());
    assert_eq!(warnings(&mut o), ["Missing params: curve_name"]);
    for t in [4, 5, 6, 7] {
        let mut opts = PhpArray::new();
        opts.set("private_key_type", PhpValue::Int(t));
        let k = o.pkey_new(Some(&opts)).unwrap().unwrap();
        assert_eq!(details(&mut o, &k).get_str("type"), Some(&PhpValue::Int(t)));
    }
}

#[test]
fn ec_explicit_params() {
    let mut o = OpenSsl::new();
    let mut ec = PhpArray::new();
    for (k, v) in [
        ("p", "BDB6F4FE3E8B1D9E0DA8C0D46F4C318CEFE4AFE3B6B8551F"),
        ("a", "BB8E5E8FBC115E139FE6A814FE48AAA6F0ADA1AA5DF91985"),
        ("b", "1854BEBDC31B21B7AEFC80AB0ECD10D5B1B3308E6DBF11C1"),
        ("order", "BDB6F4FE3E8B1D9E0DA8C0D40FC962195DFAE76F56564677"),
        ("g_x", "4AD5F7048DE709AD51236DE65E4D4B482C836DC6E4106640"),
        ("g_y", "02BB3A02D4AAADACAE24817A4CA3A1B014B5270432DB27D2"),
        ("d", "8D0AC65AAEA0D6B96254C65817D4A143A9E7A03876F1A37D"),
    ] {
        ec.set(k, PhpValue::Str(unhex(v)));
    }
    let mut opts = PhpArray::new();
    opts.set("ec", PhpValue::Array(ec));
    let k = o.pkey_new(Some(&opts)).unwrap().unwrap();
    let d = details(&mut o, &k);
    let e = sub(&d, "ec");
    assert_eq!(
        s(e, "x"),
        unhex("98E07AAD50C31F9189EBE6B8B5C70E5DEE59D7A8BC344CC6")
    );
    assert_eq!(
        s(e, "y"),
        unhex("6109D3D96E52D0867B9D05D72D07BE5876A3D973E0E96792")
    );
}

#[test]
fn curve25519_from_components() {
    let mut o = OpenSsl::new();
    let mut ed = PhpArray::new();
    ed.set("priv_key", PhpValue::Str(vec![b'a'; 32]));
    let mut opts = PhpArray::new();
    opts.set("ed25519", PhpValue::Array(ed));
    let k = o.pkey_new(Some(&opts)).unwrap().unwrap();
    let d = details(&mut o, &k);
    assert_eq!(d.get_str("type"), Some(&PhpValue::Int(5)));
    assert_eq!(s(sub(&d, "ed25519"), "priv_key"), [b'a'; 32]);
    assert_eq!(s(sub(&d, "ed25519"), "pub_key").len(), 32);
}

#[test]
fn export_with_passphrase_and_file() {
    let mut o = OpenSsl::new();
    let url = fixture_url("private_rsa_1024.key");
    let k = o
        .pkey_get_private(&KeyArg::Str(&url), Some(b""))
        .unwrap()
        .unwrap();
    let pem = o
        .pkey_export(&KeyArg::Key(&k), Some(b"secret"), None)
        .unwrap()
        .unwrap();
    assert!(pem.starts_with(b"-----BEGIN ENCRYPTED PRIVATE KEY-----"));
    let back = o
        .pkey_get_private(&KeyArg::Str(&pem), Some(b"secret"))
        .unwrap();
    assert!(back.is_some());
    let mut opts = PhpArray::new();
    opts.set("encrypt_key", PhpValue::Bool(false));
    let pem = o
        .pkey_export(&KeyArg::Key(&k), Some(b"secret"), Some(&opts))
        .unwrap()
        .unwrap();
    assert!(pem.starts_with(b"-----BEGIN PRIVATE KEY-----"));
    let path = tmp("export.pem");
    assert!(o
        .pkey_export_to_file(&KeyArg::Key(&k), path.as_bytes(), None, None)
        .unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), pem);
}

#[test]
fn open_basedir_and_long_oid_file() {
    let mut o = OpenSsl::new();
    o.open_basedir = Some(b"/nonexistent-basedir".to_vec());
    let url = fixture_url("public.key");
    assert!(o.pkey_get_public(&KeyArg::Str(&url)).unwrap().is_none());
    let w = warnings(&mut o);
    assert_eq!(w.len(), 1);
    assert!(w[0].starts_with("open_basedir restriction in effect. File("));
    assert!(w[0].ends_with(") is not within the allowed path(s): (/nonexistent-basedir)"));
    o.open_basedir = Some(format!("{}/tests", env!("CARGO_MANIFEST_DIR")).into_bytes());
    assert!(o.pkey_get_public(&KeyArg::Str(&url)).unwrap().is_some());

    let mut o = OpenSsl::new();
    let cnf = tmp("gh9339.cnf");
    std::fs::write(
        &cnf,
        format!(
            "oid_file = /{}\n[ req ]\ndefault_bits = 1024\n",
            "a".repeat(9000)
        ),
    )
    .unwrap();
    let mut opts = PhpArray::new();
    opts.set("config", PhpValue::str(&cnf));
    let _ = o.pkey_new(Some(&opts)).unwrap();
    assert_eq!(
        warnings(&mut o),
        ["Path for oid_file option must be a valid file path"]
    );
}
