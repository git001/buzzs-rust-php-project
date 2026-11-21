// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The built-in server's and CGI's part of the request variables
//! (`sapi_cli_server_register_variables()`, `sapi_cgi_register_variables()`);
//! the rest is [`php_sapi_input::vars`].

use crate::request::Request;
use crate::sapi::RequestCtx;
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_sapi_input::body::{RequestBody, SapiInput};
use php_sapi_input::vars::{register_variable_safe, SapiVariables};
use php_vm::ext::ParseBodyOptions;
use php_vm::{Vm, VmResult};

/// How a `$_SERVER` entry is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// `php_register_known_variable`: stored under the exact name.
    Known,
    /// `php_register_variable_safe`: the name is normalised (`.`/` ` → `_`,
    /// `[` starts an array).
    Filtered,
}

/// What the server knows about itself, for `$_SERVER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFacts<'a> {
    /// `DOCUMENT_ROOT`.
    pub document_root: &'a [u8],
    /// `SERVER_NAME` (the host given to `-S`).
    pub host: &'a [u8],
    /// `SERVER_PORT`.
    pub port: u16,
    /// The router script, if any.
    pub router: Option<&'a [u8]>,
    /// `ip:port` of the client.
    pub client_addr: &'a str,
}

/// The `sapi_cli_server_register_variables()` entries, in order.
pub fn server_entries(req: &Request, facts: &ServerFacts<'_>) -> Vec<(Vec<u8>, Vec<u8>, VarKind)> {
    use VarKind::{Filtered, Known};
    let mut out: Vec<(Vec<u8>, Vec<u8>, VarKind)> = Vec::new();
    let mut known = |k: &str, v: &[u8]| out.push((k.as_bytes().to_vec(), v.to_vec(), Known));
    known("DOCUMENT_ROOT", facts.document_root);
    let addr = facts.client_addr;
    if let Some(colon) = addr.rfind(':') {
        let mut host = &addr[..colon];
        host = host.strip_prefix('[').unwrap_or(host);
        host = host.strip_suffix(']').unwrap_or(host);
        let port: String = addr[colon + 1..].chars().take(7).collect();
        known("REMOTE_ADDR", host.as_bytes());
        known("REMOTE_PORT", port.as_bytes());
    } else {
        known("REMOTE_ADDR", addr.as_bytes());
    }
    known(
        "SERVER_SOFTWARE",
        format!("PHP/{} (Development Server)", php_vm::PHP_VERSION).as_bytes(),
    );
    known(
        "SERVER_PROTOCOL",
        format!(
            "HTTP/{}.{}",
            req.protocol_version / 100,
            req.protocol_version % 100
        )
        .as_bytes(),
    );
    known("SERVER_NAME", facts.host);
    known("SERVER_PORT", facts.port.to_string().as_bytes());
    known("REQUEST_URI", req.uri());
    known("REQUEST_METHOD", req.method.as_str().as_bytes());
    let vpath = req.vpath.clone().unwrap_or_default();
    known("SCRIPT_NAME", &vpath);
    if let Some(pt) = &req.path_translated {
        known("SCRIPT_FILENAME", c_str(pt));
    } else if let Some(r) = facts.router {
        known("SCRIPT_FILENAME", r);
    }
    if let Some(pi) = &req.path_info {
        known("PATH_INFO", pi);
    }
    match &req.path_info {
        Some(pi) if !pi.is_empty() => {
            let mut s = vpath.clone();
            s.extend_from_slice(pi);
            known("PHP_SELF", &s);
        }
        _ => known("PHP_SELF", &vpath),
    }
    if let Some(q) = &req.query_string {
        out.push((b"QUERY_STRING".to_vec(), c_str(q).to_vec(), Filtered));
    }
    for (k, v) in &req.headers {
        let key: Vec<u8> = k
            .iter()
            .map(|&c| {
                if c == b'-' {
                    b'_'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        let mut real = b"HTTP_".to_vec();
        real.extend_from_slice(&key);
        if key == b"CONTENT_TYPE" || key == b"CONTENT_LENGTH" {
            out.push((key, c_str(v).to_vec(), Filtered));
        }
        out.push((real, c_str(v).to_vec(), Filtered));
    }
    out
}

/// The part of `s` before the first NUL (values are C strings).
fn c_str(s: &[u8]) -> &[u8] {
    s.split(|&c| c == 0).next().unwrap_or_default()
}

pub use php_sapi::{handle_auth_data as auth_data, AuthData};

impl SapiInput for RequestCtx {
    fn body(&self) -> &RequestBody {
        &self.body
    }

    /// `sapi_cli_server_read_post()` / `sapi_cgi_read_post()` read the
    /// body the server received whole.
    fn content(&self) -> Option<&[u8]> {
        Some(self.request.content.as_deref().unwrap_or_default())
    }
}

/// A request with what the server knows about itself, for `$_SERVER`.
#[derive(Debug)]
pub struct ServerVars<'a> {
    /// The request.
    pub ctx: &'a RequestCtx,
    /// The server's side.
    pub facts: &'a ServerFacts<'a>,
}

impl SapiInput for ServerVars<'_> {
    fn body(&self) -> &RequestBody {
        self.ctx.body()
    }

    fn content(&self) -> Option<&[u8]> {
        self.ctx.content()
    }
}

impl SapiVariables for ServerVars<'_> {
    fn register_server_variables(&self, vm: &mut Vm, server: &mut ZArray) {
        use std::os::unix::ffi::OsStrExt;
        let Some(cgi) = &self.ctx.cgi else {
            for (k, v, kind) in server_entries(&self.ctx.request, self.facts) {
                let v = Value::Str(ZStr::from(v));
                match kind {
                    VarKind::Known => {
                        server.make_mut().insert(ArrayKey::from_bytes(&k), v);
                    }
                    VarKind::Filtered => register_variable_safe(vm, &k, v, server),
                }
            }
            return;
        };
        // sapi_cgi_register_variables(): the environment, then PHP_SELF.
        for (k, v) in std::env::vars_os() {
            let v = Value::Str(ZStr::from(v.as_bytes()));
            register_variable_safe(vm, k.as_bytes(), v, server);
        }
        let self_ = Value::Str(ZStr::from(cgi.php_self.as_slice()));
        register_variable_safe(vm, b"PHP_SELF", self_, server);
    }
}

/// [`php_vm::ext::ParseBodyFn`]: `request_parse_body()` for the current
/// request.
pub fn parse_body(vm: &mut Vm, options: &ParseBodyOptions) -> VmResult<(ZArray, ZArray)> {
    let ctx = crate::sapi::current();
    php_sapi_input::vars::parse_body(vm, options, ctx.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Method;

    fn sample() -> Request {
        Request {
            method: Method::Get,
            protocol_version: 101,
            request_uri: Some(b"/s.php/x?a=1".to_vec()),
            vpath: Some(b"/s.php".to_vec()),
            path_translated: Some(b"/root/s.php".to_vec()),
            path_info: Some(b"/x".to_vec()),
            query_string: Some(b"a=1".to_vec()),
            headers: vec![
                (b"host".to_vec(), b"localhost".to_vec()),
                (b"content-type".to_vec(), b"text/plain".to_vec()),
                (b"x-my-header".to_vec(), b"v".to_vec()),
            ],
            ..Request::default()
        }
    }

    #[test]
    fn server_vars() {
        let facts = ServerFacts {
            document_root: b"/root",
            host: b"localhost",
            port: 8080,
            router: None,
            client_addr: "127.0.0.1:5555",
        };
        let e = server_entries(&sample(), &facts);
        let names: Vec<String> = e
            .iter()
            .map(|(k, _, _)| String::from_utf8_lossy(k).into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "DOCUMENT_ROOT",
                "REMOTE_ADDR",
                "REMOTE_PORT",
                "SERVER_SOFTWARE",
                "SERVER_PROTOCOL",
                "SERVER_NAME",
                "SERVER_PORT",
                "REQUEST_URI",
                "REQUEST_METHOD",
                "SCRIPT_NAME",
                "SCRIPT_FILENAME",
                "PATH_INFO",
                "PHP_SELF",
                "QUERY_STRING",
                "HTTP_HOST",
                "CONTENT_TYPE",
                "HTTP_CONTENT_TYPE",
                "HTTP_X_MY_HEADER"
            ]
        );
        let get = |n: &str| {
            e.iter()
                .find(|(k, _, _)| k == n.as_bytes())
                .map(|(_, v, _)| String::from_utf8_lossy(v).into_owned())
        };
        assert_eq!(get("REMOTE_ADDR").as_deref(), Some("127.0.0.1"));
        assert_eq!(get("REMOTE_PORT").as_deref(), Some("5555"));
        assert_eq!(get("PHP_SELF").as_deref(), Some("/s.php/x"));
        assert_eq!(get("SERVER_PROTOCOL").as_deref(), Some("HTTP/1.1"));
        assert_eq!(get("SERVER_PORT").as_deref(), Some("8080"));
    }

    #[test]
    fn ipv6_client_and_router_filename() {
        let mut r = sample();
        r.path_translated = None;
        r.path_info = None;
        let facts = ServerFacts {
            document_root: b"/root",
            host: b"::1",
            port: 1,
            router: Some(b"/root/router.php"),
            client_addr: "[::1]:4242",
        };
        let e = server_entries(&r, &facts);
        let get = |n: &str| {
            e.iter()
                .find(|(k, _, _)| k == n.as_bytes())
                .map(|(_, v, _)| String::from_utf8_lossy(v).into_owned())
        };
        assert_eq!(get("REMOTE_ADDR").as_deref(), Some("::1"));
        assert_eq!(get("SCRIPT_FILENAME").as_deref(), Some("/root/router.php"));
        assert_eq!(get("PATH_INFO"), None);
        assert_eq!(get("PHP_SELF").as_deref(), Some("/s.php"));
    }

    #[test]
    fn auth() {
        assert_eq!(
            auth_data(Some(b"Basic dXNlcjpwYXNz")),
            (Some(b"user".to_vec()), Some(b"pass".to_vec()), None)
        );
        assert_eq!(
            auth_data(Some(b"Digest x")),
            (None, None, Some(b"x".to_vec()))
        );
    }
}
