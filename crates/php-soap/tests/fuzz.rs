// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Deterministic mutation fuzzing of the parsers: XML, WSDL/XSD loading,
//! SOAP request handling, URLs and HTTP headers must never panic.

#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};

use common::{soap_tests_dir, with_big_stack, TestHost};
use php_runtime::{Value, ZArray};
use php_soap::wsdl::{load_wsdl, MemoryLoader};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }
}

const TOKENS: &[&[u8]] = &[
    b"<",
    b">",
    b"/>",
    b"</",
    b"\"",
    b"'",
    b"=",
    b":",
    b"xsi:type=\"xsd:int\"",
    b"href=\"#a\"",
    b"id=\"a\"",
    b"SOAP-ENC:arrayType=\"xsd:int[2,3]\"",
    b"xsi:nil=\"1\"",
    b"&amp;",
    b"\xff",
    b"\x00",
    b"maxOccurs=\"unbounded\"",
    b"minOccurs=\"-1\"",
    b"<xsd:element name=\"x\"/>",
    b"ref=\"tns:x\"",
    b"[",
    b"]",
    b",",
    b"*",
];

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..=rng.below(4) {
        let pos = rng.below(v.len() + 1);
        match rng.below(4) {
            0 if !v.is_empty() => {
                let end = (pos + rng.below(16)).min(v.len());
                v.drain(pos.min(v.len())..end);
            }
            1 => {
                let t = TOKENS[rng.below(TOKENS.len())];
                v.splice(pos..pos, t.iter().copied());
            }
            2 if !v.is_empty() => {
                let i = pos.min(v.len() - 1);
                v[i] = u8::try_from(rng.below(256)).unwrap_or(0);
            }
            _ => {
                if v.len() > 2 {
                    let a = rng.below(v.len());
                    let b = rng.below(v.len());
                    let (lo, hi) = (a.min(b), a.max(b));
                    let chunk = v[lo..hi].to_vec();
                    v.splice(pos..pos, chunk);
                }
            }
        }
    }
    v
}

fn seeds(ext: &str, limit: usize) -> Vec<Vec<u8>> {
    let Some(dir) = soap_tests_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == ext) {
                if let Ok(b) = std::fs::read(&p) {
                    out.push(b);
                }
            }
        }
    }
    out.truncate(limit);
    out
}

#[test]
fn wsdl_loader_never_panics() {
    with_big_stack(|| {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        let seeds = seeds("wsdl", 80);
        assert!(!seeds.is_empty() || soap_tests_dir().is_none());
        for seed in &seeds {
            for _ in 0..40 {
                let data = mutate(&mut rng, seed);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    let mut loader = MemoryLoader::default();
                    loader.files.insert("/f.wsdl".into(), data.clone());
                    if let Ok(sdl) = load_wsdl("/f.wsdl", &mut loader) {
                        let _ = php_soap::sdl_print::get_functions(&sdl);
                        let _ = php_soap::sdl_print::get_types(&sdl);
                    }
                }));
                assert!(
                    r.is_ok(),
                    "panic on input:\n{}",
                    String::from_utf8_lossy(&data)
                );
            }
        }
    });
}

#[test]
fn server_request_parsing_never_panics() {
    with_big_stack(|| {
        let mut rng = Rng(0x0dd_ba11_cafe_f00d);
        let base: &[&[u8]] = &[
            b"<?xml version=\"1.0\"?><SOAP-ENV:Envelope xmlns:SOAP-ENV=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:SOAP-ENC=\"http://schemas.xmlsoap.org/soap/encoding/\"><SOAP-ENV:Header><h xmlns=\"urn:h\">1</h></SOAP-ENV:Header><SOAP-ENV:Body><ns1:f xmlns:ns1=\"urn:t\"><a xsi:type=\"SOAP-ENC:Array\" SOAP-ENC:arrayType=\"xsd:int[2]\"><i>1</i><i>2</i></a><b id=\"x\"><c>3</c></b><d href=\"#x\"/><m xsi:type=\"ns2:Map\" xmlns:ns2=\"http://xml.apache.org/xml-soap\"><item><key>k</key><value>v</value></item></m></ns1:f></SOAP-ENV:Body></SOAP-ENV:Envelope>",
            b"<env:Envelope xmlns:env=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:enc=\"http://www.w3.org/2003/05/soap-encoding\"><env:Body><f><a enc:itemType=\"xsd:string\" enc:arraySize=\"2 2\"><i>a</i></a><r enc:ref=\"#q\"/><q enc:id=\"q\">z</q></f></env:Body></env:Envelope>",
        ];
        for seed in base {
            for _ in 0..300 {
                let data = mutate(&mut rng, seed);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    let mut h = TestHost::new();
                    h.add_function("f", |_, args| {
                        Ok(Value::Array(ZArray::from_values(args.iter().cloned())))
                    });
                    let opts = ZArray::from_pairs([("uri", Value::from("urn:t"))]);
                    let s = h.new_server(None, &opts).expect("server");
                    php_soap::server::add_function(&mut h, &s, &Value::from("f")).expect("add");
                    let _ = php_soap::server::handle(&mut h, &s, Some(&data));
                    h.finish()
                }));
                assert!(
                    r.is_ok(),
                    "panic on request:\n{}",
                    String::from_utf8_lossy(&data)
                );
            }
        }
    });
}

#[test]
fn url_and_header_parsers_never_panic() {
    let mut rng = Rng(0xfeed_beef_0000_0001);
    let urls: &[&[u8]] = &[
        b"http://u:p@h:80/p?q#f",
        b"https://[::1]:443/x",
        b"h:80/p",
        b"//x/y",
        b"mailto:a@b",
    ];
    for u in urls {
        for _ in 0..500 {
            let data = mutate(&mut rng, u);
            let _ = php_soap::http::parse_url(&data);
            let _ = php_soap::http::header_value(&data, "Content-Type:");
        }
    }
}
