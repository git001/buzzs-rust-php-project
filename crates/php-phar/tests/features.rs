// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Behaviour that the PHP 8.3 oracle cannot check (no ext/bz2 there, no
//! running VM, php-src master fixes) — exercised directly.

#![expect(clippy::disallowed_methods, reason = "test code")]

use std::path::PathBuf;
use std::rc::Rc;

use php_phar::object::{self, PharFileInfo, PharObject};
use php_phar::registry;
use php_phar::web::{self, Rewrite, ServerValue, WebHost, WebPharArgs, WebResult};

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("php-phar-feat-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn path(d: &std::path::Path, f: &str) -> Vec<u8> {
    d.join(f).to_string_lossy().as_bytes().to_vec()
}

fn writable() {
    registry::reset_request();
    registry::set_ini("phar.readonly", false, true);
    registry::set_ini("phar.require_hash", true, true);
    registry::set_compression_support(true, true);
}

fn content(archive: &[u8], entry: &str) -> Vec<u8> {
    let mut url = b"phar://".to_vec();
    url.extend_from_slice(archive);
    url.push(b'/');
    url.extend_from_slice(entry.as_bytes());
    let mut fi = PharFileInfo::new();
    fi.construct(&url).unwrap();
    let c = fi.get_content().unwrap();
    fi.release();
    c
}

#[test]
fn bzip2_entries_and_archives_roundtrip() {
    writable();
    let d = workdir("bz2");
    let data = b"bzip2 me please ".repeat(40);
    for (name, is_zip) in [("e.phar", false), ("z.zip", true)] {
        let p = path(&d, name);
        let mut o = PharObject::new(is_zip);
        o.construct(&p, None, 0).unwrap();
        o.offset_set(b"a.txt", &data).unwrap();
        o.compress_files(0x2000).unwrap();
        o.release();
        registry::reset_request();
        writable();
        let mut o = PharObject::new(is_zip);
        o.construct(&p, None, 0).unwrap();
        let mut fi = PharFileInfo::new();
        let mut url = b"phar://".to_vec();
        url.extend_from_slice(&p);
        url.extend_from_slice(b"/a.txt");
        fi.construct(&url).unwrap();
        assert!(fi.is_compressed(Some(0x2000)).unwrap(), "{name}");
        assert!(fi.get_compressed_size().unwrap() < i64::try_from(data.len()).unwrap());
        assert_eq!(fi.get_content().unwrap(), data);
        fi.release();
        o.release();
    }
    // whole-archive bzip2
    let p = path(&d, "w.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"x", b"whole").unwrap();
    let c = o.compress(0x2000, None).unwrap();
    o.release();
    assert!(c.fname.ends_with(b"w.phar.bz2"));
    let raw = std::fs::read(String::from_utf8(c.fname.clone()).unwrap()).unwrap();
    assert!(raw.starts_with(b"BZh"));
    registry::reset_request();
    writable();
    let mut o = PharObject::new(false);
    o.construct(&c.fname, None, 0).unwrap();
    assert_eq!(o.is_compressed().unwrap(), Some(0x2000));
    assert_eq!(content(&c.fname, "x"), b"whole");
    o.release();
    registry::reset_request();
}

#[test]
fn decompress_files_keeps_contents() {
    // php-src master fix: PHP 8.3 wrote the compressed bytes as content.
    writable();
    let d = workdir("decomp");
    let p = path(&d, "d.phar");
    let data = b"abcabcabc".repeat(30);
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a.txt", &data).unwrap();
    o.offset_set(b"b.txt", b"bbb").unwrap();
    o.compress_files(0x1000).unwrap();
    o.release();
    registry::reset_request();
    writable();
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    assert!(o.decompress_files().unwrap());
    o.release();
    registry::reset_request();
    writable();
    assert_eq!(content(&p, "a.txt"), data);
    assert_eq!(content(&p, "b.txt"), b"bbb");
    registry::reset_request();
}

struct FakeSsl;

impl php_phar::signature::OpenSslHook for FakeSsl {
    fn sign(&self, data: &[u8], key: &[u8], algo: i64) -> Option<Vec<u8>> {
        let mut d = data.to_vec();
        d.extend_from_slice(key);
        d.extend_from_slice(&algo.to_le_bytes());
        Some(php_hash::sha1(&d).to_vec())
    }

    fn verify(&self, data: &[u8], sig: &[u8], public: &[u8], algo: i64) -> bool {
        self.sign(data, public, algo).as_deref() == Some(sig)
    }
}

#[test]
fn openssl_signatures_through_hook() {
    writable();
    php_phar::signature::set_openssl_hook(Some(Rc::new(FakeSsl)));
    let d = workdir("ssl");
    for (name, algo) in [("s.phar", 0x10), ("t.phar.tar", 0x11), ("z.phar.zip", 0x12)] {
        let p = path(&d, name);
        let mut o = PharObject::new(false);
        o.construct(&p, None, 0).unwrap();
        o.offset_set(b"a", b"signed").unwrap();
        o.set_signature_algorithm(algo, Some(b"KEY")).unwrap();
        o.release();
        let mut pk = p.clone();
        pk.extend_from_slice(b".pubkey");
        std::fs::write(String::from_utf8(pk.clone()).unwrap(), b"KEY").unwrap();
        registry::reset_request();
        registry::set_ini("phar.readonly", true, true);
        let mut o = PharObject::new(false);
        o.construct(&p, None, 0).unwrap();
        let sig = o.get_signature().unwrap().unwrap();
        assert!(sig.hash_type.starts_with("OpenSSL"), "{name}");
        assert_eq!(sig.hash.len(), 40);
        o.release();
        // a wrong public key fails verification
        std::fs::write(String::from_utf8(pk).unwrap(), b"OTHER").unwrap();
        registry::reset_request();
        let mut o = PharObject::new(false);
        let e = o.construct(&p, None, 0).unwrap_err();
        assert!(
            e.message.contains("could not be verified"),
            "{name}: {}",
            e.message
        );
        registry::reset_request();
        writable();
    }
    php_phar::signature::set_openssl_hook(None);
    registry::reset_request();
}

#[test]
fn require_hash_rejects_unsigned_executable_tar() {
    writable();
    let d = workdir("req");
    let p = path(&d, "u.phar.tar");
    // an executable tar (has .phar/stub.php) without signature
    let mut tar = Vec::new();
    let mut add = |name: &str, data: &[u8]| {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..262].copy_from_slice(b"ustar");
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:07o}").as_bytes());
        tar.extend_from_slice(&h);
        tar.extend_from_slice(data);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    };
    add(".phar/stub.php", b"<?php __HALT_COMPILER();");
    add("a.txt", b"hi");
    tar.extend_from_slice(&[0u8; 1024]);
    std::fs::write(String::from_utf8(p.clone()).unwrap(), &tar).unwrap();
    let mut o = PharObject::new(false);
    let e = o.construct(&p, None, 0).unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "tar-based phar \"{}\" does not have a signature",
            String::from_utf8_lossy(&p)
        )
    );
    registry::reset_request();
    registry::set_ini("phar.require_hash", false, true);
    registry::set_ini("phar.readonly", false, true);
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    assert_eq!(o.count().unwrap(), 2);
    o.release();
    registry::set_ini("phar.require_hash", true, true);
    registry::reset_request();
}

#[test]
fn lowercase_halt_compiler_is_found() {
    // GH-20442 (php-src master): the stub search is case-insensitive.
    writable();
    let d = workdir("halt");
    let p = path(&d, "h.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a", b"x").unwrap();
    o.set_stub(b"<?php __halt_compiler();").unwrap();
    o.release();
    registry::reset_request();
    writable();
    assert_eq!(content(&p, "a"), b"x");
    registry::reset_request();
}

#[test]
fn running_and_mount() {
    writable();
    let d = workdir("mount");
    let p = path(&d, "m.phar");
    std::fs::write(d.join("external.txt"), b"external data").unwrap();
    std::fs::create_dir_all(d.join("extdir")).unwrap();
    std::fs::write(d.join("extdir/inner.txt"), b"inner").unwrap();
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"index.php", b"<?php").unwrap();
    let mut exe = b"phar://".to_vec();
    exe.extend_from_slice(&p);
    exe.extend_from_slice(b"/index.php");
    assert_eq!(object::running(Some(&exe), false), p);
    assert_eq!(
        object::running(Some(&exe), true),
        [b"phar://".as_slice(), &p].concat()
    );
    assert_eq!(object::running(Some(b"/x.php"), true), b"");
    object::mount(Some(&exe), b"ext.txt", &path(&d, "external.txt")).unwrap();
    object::mount(Some(&exe), b"dir", &path(&d, "extdir")).unwrap();
    assert_eq!(content(&p, "ext.txt"), b"external data");
    let _ = php_streams::wrapper::register_global(b"phar", Rc::new(php_phar::wrapper::PharWrapper));
    let mut url = b"phar://".to_vec();
    url.extend_from_slice(&p);
    url.extend_from_slice(b"/dir/inner.txt");
    assert_eq!(
        php_streams::file::file_get_contents(&url, false, None, 0, None).unwrap(),
        Some(b"inner".to_vec())
    );
    // now mounted just in time, so it is a regular manifest entry
    assert_eq!(content(&p, "dir/inner.txt"), b"inner");
    let e = object::mount(Some(&exe), b"phar://x", b"/y").unwrap_err();
    assert!(e.message.starts_with("Can only mount internal paths"));
    let e = object::mount(Some(&exe), b"bad", &path(&d, "nothere")).unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "Mounting of bad to {} within phar {} failed",
            String::from_utf8_lossy(&path(&d, "nothere")),
            String::from_utf8_lossy(&p)
        )
    );
    o.release();
    registry::reset_request();
}

#[test]
fn interceptors_resolve_relative_paths() {
    use php_phar::intercept::{self, InterceptStat, StatKind};
    writable();
    let d = workdir("intercept");
    let p = path(&d, "i.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"index.php", b"<?php").unwrap();
    o.offset_set(b"lib/a.php", b"aaa").unwrap();
    let mut exe = b"phar://".to_vec();
    exe.extend_from_slice(&p);
    exe.extend_from_slice(b"/index.php");
    assert_eq!(
        intercept::file_stat(b"lib/a.php", StatKind::Size, Some(&exe)),
        InterceptStat::Skip
    );
    object::intercept_file_funcs();
    match intercept::file_stat(b"lib/a.php", StatKind::Size, Some(&exe)) {
        InterceptStat::Stat(sb) => assert_eq!(sb.size, 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        intercept::file_stat(b"lib", StatKind::IsDir, Some(&exe)),
        InterceptStat::Bool(true)
    );
    assert_eq!(
        intercept::file_stat(b"nope", StatKind::Exists, Some(&exe)),
        InterceptStat::Bool(false)
    );
    assert_eq!(
        intercept::file_stat(b"nope", StatKind::Lstat, Some(&exe)),
        InterceptStat::Warn("Lstat failed for nope".into())
    );
    assert_eq!(
        intercept::file_stat(b"/abs", StatKind::Stat, Some(&exe)),
        InterceptStat::Skip
    );
    assert_eq!(intercept::is_file(b"lib/a.php", Some(&exe)), Some(true));
    assert_eq!(intercept::is_file(b"lib", Some(&exe)), Some(false));
    assert_eq!(intercept::is_link(b"lib/a.php", Some(&exe)), Some(false));
    let mut want = b"phar://".to_vec();
    want.extend_from_slice(&p);
    want.extend_from_slice(b"/lib/a.php");
    assert_eq!(
        intercept::relative_path(b"lib/a.php", false, Some(&exe), b"."),
        Some(want)
    );
    assert_eq!(
        intercept::relative_path(b"missing.php", false, Some(&exe), b"."),
        None
    );
    assert!(intercept::opendir_path(b"lib", Some(&exe)).is_some());
    o.release();
    registry::reset_request();
}

#[test]
fn cache_list_archives_are_copied_on_write() {
    writable();
    let d = workdir("cache");
    let p = path(&d, "c.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a", b"cached").unwrap();
    o.release();
    registry::reset_request();
    writable();
    assert!(registry::load_cache_list(&p));
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    assert_eq!(content(&p, "a"), b"cached");
    o.offset_set(b"b", b"new").unwrap();
    assert_eq!(o.count().unwrap(), 2);
    o.release();
    registry::reset_request();
}

/// A minimal SAPI for `webPhar()`.
#[derive(Default)]
struct Host {
    uri: Vec<u8>,
    script: Vec<u8>,
    path_info: Option<Vec<u8>>,
    headers: Vec<String>,
    codes: Vec<u16>,
    out: Vec<u8>,
    included: Vec<Vec<u8>>,
    highlighted: Vec<Vec<u8>>,
    rewrite: Option<Rewrite>,
}

impl WebHost for Host {
    fn sapi_name(&self) -> String {
        "cgi-fcgi".into()
    }
    fn request_method(&self) -> Option<Vec<u8>> {
        Some(b"GET".to_vec())
    }
    fn request_uri(&self) -> Option<Vec<u8>> {
        Some(self.uri.clone())
    }
    fn has_server(&self) -> bool {
        true
    }
    fn server_get(&self, key: &str) -> Option<ServerValue> {
        match key {
            "SCRIPT_NAME" => Some(ServerValue::Str(self.script.clone())),
            "PATH_INFO" => self.path_info.clone().map(ServerValue::Str),
            _ => None,
        }
    }
    fn server_set(&mut self, _key: &str, _value: Vec<u8>) {}
    fn getenv(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
    fn header(&mut self, line: &[u8], code: Option<u16>) {
        self.headers
            .push(String::from_utf8_lossy(line).into_owned());
        if let Some(c) = code {
            self.codes.push(c);
        }
    }
    fn send_headers(&mut self) -> bool {
        true
    }
    fn write(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
    }
    fn highlight_file(&mut self, path: &[u8]) {
        self.highlighted.push(path.to_vec());
    }
    fn include(&mut self, path: &[u8]) -> bool {
        self.included.push(path.to_vec());
        true
    }
    fn rewrite(&mut self, _entry: &[u8]) -> Rewrite {
        self.rewrite.clone().unwrap_or(Rewrite::Other)
    }
}

fn webphar_archive(d: &std::path::Path) -> Vec<u8> {
    let p = path(d, "web.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a.php", b"hio").unwrap();
    o.offset_set(b"a.jpg", b"hio2").unwrap();
    o.offset_set(b"a.phps", b"<?php function hio(){}").unwrap();
    o.offset_set(b"fronk.gronk", b"hio3").unwrap();
    o.offset_set(b"index.php", b"here is my index").unwrap();
    o.release();
    p
}

fn web(p: &[u8], host: &mut Host, args: &WebPharArgs<'_>) -> WebResult {
    web::web_phar(host, Some(p), true, args).unwrap()
}

#[test]
fn web_phar_front_controller() {
    writable();
    let d = workdir("web");
    let p = webphar_archive(&d);
    let mk = |pi: &str| Host {
        uri: format!("/web.phar{pi}").into_bytes(),
        script: b"/web.phar".to_vec(),
        path_info: (!pi.is_empty()).then(|| pi.as_bytes().to_vec()),
        ..Host::default()
    };
    // PHP file: included
    let mut h = mk("/a.php");
    assert_eq!(web(&p, &mut h, &WebPharArgs::default()), WebResult::Exit);
    assert_eq!(
        h.included,
        vec![[b"phar://".as_slice(), &p, b"/a.php"].concat()]
    );
    // other mime: contents with headers
    let mut h = mk("/a.jpg");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(
        h.headers,
        vec!["Content-type: image/jpeg", "Content-length: 4"]
    );
    assert_eq!(h.out, b"hio2");
    let mut h = mk("/fronk.gronk");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(h.headers[0], "Content-type: application/octet-stream");
    // phps: highlighted
    let mut h = mk("/a.phps");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(h.highlighted.len(), 1);
    // 404
    let mut h = mk("/notfound.php");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(h.codes, vec![404]);
    assert_eq!(
        String::from_utf8(h.out).unwrap(),
        "<html>\n <head>\n  <title>File Not Found</title>\n </head>\n <body>\n  <h1>404 - File Not Found</h1>\n </body>\n</html>"
    );
    // directory request: 301 to the index
    let mut h = mk("");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(h.codes, vec![301]);
    assert_eq!(h.headers[1], "Location: /web.phar/index.php");
    // rewrite returning false: 403
    let mut h = mk("/a.php");
    h.rewrite = Some(Rewrite::Bool);
    web(
        &p,
        &mut h,
        &WebPharArgs {
            has_rewrite: true,
            ..WebPharArgs::default()
        },
    );
    assert_eq!(h.codes, vec![403]);
    assert!(String::from_utf8(h.out)
        .unwrap()
        .contains("403 - File Access Denied"));
    // rewrite to another entry
    let mut h = mk("/hi");
    h.rewrite = Some(Rewrite::Str(b"/a.jpg".to_vec()));
    web(
        &p,
        &mut h,
        &WebPharArgs {
            has_rewrite: true,
            ..WebPharArgs::default()
        },
    );
    assert_eq!(h.out, b"hio2");
    // bad rewrite return value
    let mut h = mk("/hi");
    h.rewrite = Some(Rewrite::Other);
    let e = web::web_phar(
        &mut h,
        Some(&p),
        true,
        &WebPharArgs {
            has_rewrite: true,
            ..WebPharArgs::default()
        },
    )
    .unwrap_err();
    assert_eq!(
        e.message,
        "phar error: rewrite callback must return a string or false"
    );
    // mime override
    let mut h = mk("/a.jpg");
    let args = WebPharArgs {
        mimes: Some(vec![(b"jpg".to_vec(), web::MimeOverride::Int(0))]),
        ..WebPharArgs::default()
    };
    web(&p, &mut h, &args);
    assert_eq!(h.included.len(), 1);
    let mut h = mk("/a.jpg");
    let args = WebPharArgs {
        mimes: Some(vec![(b"jpg".to_vec(), web::MimeOverride::Int(100))]),
        ..WebPharArgs::default()
    };
    let e = web::web_phar(&mut h, Some(&p), true, &args).unwrap_err();
    assert_eq!(e.message, "Unknown mime type specifier used, only Phar::PHP, Phar::PHPS and a mime type string are allowed");
    // PATH_INFO beyond the file: entry found by prefix
    let mut h = mk("/a.jpg/extra/stuff");
    web(&p, &mut h, &WebPharArgs::default());
    assert_eq!(h.out, b"hio2");
    registry::reset_request();
}

#[test]
fn web_phar_extra_path_info_in_a_cached_archive() {
    // cache_list/frontcontroller25.phpt: phar_postprocess_ru_web() also
    // finds the archive in the phar.cache_list cache.
    writable();
    let d = workdir("webcache");
    let p = webphar_archive(&d);
    registry::reset_request();
    assert!(registry::load_cache_list(&p));
    let mut h = Host {
        uri: b"/web.phar/a.jpg/extra/stuff".to_vec(),
        script: b"/web.phar".to_vec(),
        path_info: Some(b"/a.jpg/extra/stuff".to_vec()),
        ..Host::default()
    };
    assert_eq!(web(&p, &mut h, &WebPharArgs::default()), WebResult::Exit);
    assert_eq!(h.codes, Vec::<u16>::new());
    assert_eq!(h.out, b"hio2");
    registry::reset_request();
}

#[test]
fn web_phar_outside_web_returns() {
    struct Cli;
    impl WebHost for Cli {
        fn sapi_name(&self) -> String {
            "cli".into()
        }
        fn request_method(&self) -> Option<Vec<u8>> {
            None
        }
        fn request_uri(&self) -> Option<Vec<u8>> {
            None
        }
        fn has_server(&self) -> bool {
            false
        }
        fn server_get(&self, _: &str) -> Option<ServerValue> {
            None
        }
        fn server_set(&mut self, _: &str, _: Vec<u8>) {}
        fn getenv(&self, _: &str) -> Option<Vec<u8>> {
            None
        }
        fn header(&mut self, _: &[u8], _: Option<u16>) {}
        fn send_headers(&mut self) -> bool {
            true
        }
        fn write(&mut self, _: &[u8]) {}
        fn highlight_file(&mut self, _: &[u8]) {}
        fn include(&mut self, _: &[u8]) -> bool {
            false
        }
        fn rewrite(&mut self, _: &[u8]) -> Rewrite {
            Rewrite::Other
        }
    }
    writable();
    let d = workdir("webcli");
    let p = webphar_archive(&d);
    assert_eq!(
        web::web_phar(&mut Cli, Some(&p), true, &WebPharArgs::default()).unwrap(),
        WebResult::Return
    );
    let e = web::web_phar(&mut Cli, None, true, &WebPharArgs::default()).unwrap_err();
    assert_eq!(
        e.message,
        "cannot initialize a phar outside of PHP execution"
    );
    let e = web::web_phar(
        &mut Cli,
        Some(b"/tmp/x.php"),
        false,
        &WebPharArgs::default(),
    )
    .unwrap_err();
    assert_eq!(e.message, "__HALT_COMPILER(); must be declared in a phar");
    registry::reset_request();
}

#[test]
#[allow(clippy::many_single_char_names)]
fn including_phars_directly() {
    use php_phar::include::{compile_source, CompileSource};
    writable();
    let d = workdir("include");
    let t = path(&d, "t.phar.tar");
    let mut o = PharObject::new(false);
    o.construct(&t, None, 0).unwrap();
    o.offset_set(b"a", b"x").unwrap();
    o.release();
    assert_eq!(
        compile_source(&t),
        CompileSource::Stub([b"phar://".as_slice(), &t, b"/.phar/stub.php"].concat())
    );
    let p = path(&d, "g.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a", b"x").unwrap();
    let c = o.compress(0x1000, None).unwrap();
    o.release();
    registry::reset_request();
    match compile_source(&c.fname) {
        CompileSource::Bytes(b) => {
            assert!(b.starts_with(b"<?php"));
            assert!(b.windows(18).any(|w| w == b"__HALT_COMPILER();"));
        }
        other => panic!("{other:?}"),
    }
    let plain = path(&d, "plain.phar");
    let mut o = PharObject::new(false);
    o.construct(&plain, None, 0).unwrap();
    o.offset_set(b"a", b"x").unwrap();
    o.release();
    assert_eq!(compile_source(&plain), CompileSource::Original);
    registry::reset_request();
}

/// `files/phar_test.inc`: a stub plus a hand-built manifest.
fn phar_test_inc(stub: &[u8], files: &[(&str, &[u8], u32)], alias: &[u8]) -> Vec<u8> {
    let mut entries = Vec::new();
    for (name, cont, ulen) in files {
        entries.extend_from_slice(&u32::try_from(name.len()).unwrap().to_le_bytes());
        entries.extend_from_slice(name.as_bytes());
        for v in [*ulen, 1_141_214_400, *ulen, php_hash::crc32(cont), 0x1B6, 0] {
            entries.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut m = Vec::new();
    m.extend_from_slice(&u32::try_from(files.len()).unwrap().to_le_bytes());
    m.extend_from_slice(&0x1000u16.to_be_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&u32::try_from(alias.len()).unwrap().to_le_bytes());
    m.extend_from_slice(alias);
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&entries);
    let mut out = stub.to_vec();
    out.extend_from_slice(&u32::try_from(m.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&m);
    for (_, cont, _) in files {
        out.extend_from_slice(cont);
    }
    out
}

fn streams() {
    let _ = php_streams::wrapper::register_global(b"phar", Rc::new(php_phar::wrapper::PharWrapper));
    let _ = php_streams::diag::take();
}

fn url(archive: &[u8], entry: &str) -> Vec<u8> {
    [b"phar://".as_slice(), archive, b"/", entry.as_bytes()].concat()
}

#[test]
fn short_entry_is_a_crc_mismatch() {
    // 013.phpt: the manifest claims 2 bytes but the file ends after 1.
    writable();
    registry::set_ini("phar.require_hash", false, true);
    streams();
    let d = workdir("t013");
    let p = path(&d, "013.phar.php");
    std::fs::write(
        String::from_utf8(p.clone()).unwrap(),
        phar_test_inc(b"<?php __HALT_COMPILER(); ?>", &[("a", b"a", 2)], b"hio"),
    )
    .unwrap();
    let r = php_streams::file::file_get_contents(&url(&p, "a"), false, None, 0, None).unwrap();
    assert_eq!(r, None);
    let w = php_streams::diag::take();
    assert!(
        w[0].text().ends_with(&format!(
            "internal corruption of phar \"{}\" (crc32 mismatch on file \"a\")",
            String::from_utf8_lossy(&p)
        )),
        "{}",
        w[0].text()
    );
    registry::set_ini("phar.require_hash", true, true);
    registry::reset_request();
}

#[test]
fn flushing_after_the_file_vanished_fails_like_bug13786() {
    writable();
    let d = workdir("b13786");
    let p = path(&d, "DataArchive.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.add_from_string(b"0", b"file 0").unwrap();
    assert_eq!(content(&p, "0"), b"file 0");
    o.release();
    std::fs::remove_file(String::from_utf8(p.clone()).unwrap()).unwrap();
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    assert_eq!(o.count().unwrap(), 1, "the archive stays registered");
    let e = o.add_from_string(b"1", b"file 1").unwrap_err();
    assert_eq!(e.class, php_phar::error::ExceptionClass::Phar);
    assert_eq!(
        e.message,
        format!(
            "unable to seek to start of file \"0\" while creating new phar \"{}\"",
            String::from_utf8_lossy(&p)
        )
    );
    o.release();
    registry::reset_request();
}

#[test]
fn writing_through_a_read_handle_like_gh19038() {
    writable();
    streams();
    let d = workdir("gh19038");
    let p = path(&d, "gh19038.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.add_from_string(b"file", b"123").unwrap();
    let mut opened = None;
    let mut f =
        php_streams::open::open_wrapper(&url(&p, "file"), "r", 0, &mut opened, None).unwrap();
    assert_eq!(f.seek(3, 0), 0);
    assert_eq!(f.write(b"456").unwrap(), 3);
    assert_eq!(f.seek(0, 0), 0);
    assert_eq!(f.read_vec(100).unwrap(), b"123456");
    f.close();
    o.release();
    registry::reset_request();
}

#[test]
fn mounting_phar_urls_like_copyonwrite4() {
    writable();
    streams();
    let d = workdir("cow4");
    let p = path(&d, "write4.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"tobemounted", b"hi").unwrap();
    o.release();
    registry::reset_request();
    writable();
    assert!(registry::load_cache_list(&p));
    let exe = url(&p, "index.php");
    object::mount(Some(&exe), b"test.txt", &url(&p, "tobemounted")).unwrap();
    assert_eq!(
        php_streams::file::file_get_contents(&url(&p, "test.txt"), false, None, 0, None).unwrap(),
        Some(b"hi".to_vec())
    );
    assert_eq!(content(&p, "test.txt"), b"hi");
    registry::reset_request();
}

#[test]
fn file_info_setters_copy_cached_archives_on_write() {
    writable();
    let d = workdir("ficow");
    let p = path(&d, "c.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"a", b"x").unwrap();
    o.release();
    registry::reset_request();
    writable();
    assert!(registry::load_cache_list(&p));
    let mut fi = PharFileInfo::new();
    fi.construct(&url(&p, "a")).unwrap();
    assert!(fi.entry_info().unwrap().is_persistent);
    fi.chmod(0o755).unwrap();
    assert!(!fi.entry_info().unwrap().is_persistent);
    assert_eq!(fi.entry_info().unwrap().flags & 0o777, 0o755);
    fi.set_metadata(php_runtime::Value::Int(1), b"i:1;".to_vec())
        .unwrap();
    assert!(fi.has_metadata().unwrap());
    fi.release();
    // the cached manifest itself stays untouched (as in PHP)
    registry::reset_request();
    writable();
    let mut fi = PharFileInfo::new();
    fi.construct(&url(&p, "a")).unwrap();
    assert!(fi.entry_info().unwrap().is_persistent);
    assert!(!fi.has_metadata().unwrap());
    fi.release();
    registry::reset_request();
}

#[test]
fn rewritten_archive_files_are_seen_like_gh13833() {
    writable();
    registry::set_ini("phar.require_hash", false, true);
    let d = workdir("gh13833");
    let p = path(&d, "gh13833.phar");
    let fs = String::from_utf8(p.clone()).unwrap();
    std::fs::write(
        &fs,
        phar_test_inc(
            b"<?php\nPhar::mapPhar('hio');\n__HALT_COMPILER(); ?>",
            &[("a", b"a", 1)],
            b"hio",
        ),
    )
    .unwrap();
    assert!(object::map_phar(Some(&p), Some(b"hio"), true).unwrap());
    std::fs::write(
        &fs,
        phar_test_inc(b"<?php __HALT_COMPILER(); ?>", &[("a", b"a", 1)], b"hio"),
    )
    .unwrap();
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.set_metadata(
        php_runtime::Value::Null,
        br#"O:8:"stdClass":1:{s:2:"my";s:6:"friend";}"#.to_vec(),
    )
    .unwrap();
    let copy = format!("{fs}.copy.phar");
    std::fs::copy(&fs, &copy).unwrap();
    let mut o2 = PharObject::new(false);
    let e = o2.construct(copy.as_bytes(), None, 0).unwrap_err();
    assert_eq!(
        e.message,
        format!("internal corruption of phar \"{copy}\" (trying to read past buffer end)")
    );
    o.release();
    registry::set_ini("phar.require_hash", true, true);
    registry::reset_request();
}

#[test]
fn temporary_file_info_keeps_the_copied_archive() {
    // copyonwrite18-22: `$p["test.txt"]->chmod(0444)` on a temporary object
    writable();
    streams();
    let d = workdir("cowtmp");
    let p = path(&d, "c.phar");
    let mut o = PharObject::new(false);
    o.construct(&p, None, 0).unwrap();
    o.offset_set(b"test.txt", b"x").unwrap();
    o.release();
    registry::reset_request();
    writable();
    assert!(registry::load_cache_list(&p));
    let mut fi = PharFileInfo::new();
    fi.construct(&url(&p, "test.txt")).unwrap();
    fi.chmod(0o444).unwrap();
    fi.release();
    let copy = registry::find_by_fname(&p).expect("copy stays registered");
    assert!(!copy.borrow().is_persistent);
    assert_eq!(copy.borrow().refcount, 0);
    let perms =
        php_streams::filestat::php_stat(&url(&p, "test.txt"), php_streams::filestat::FsType::Perms);
    assert_eq!(perms, php_streams::filestat::StatResult::Int(0o100_444));
    registry::reset_request();
}
