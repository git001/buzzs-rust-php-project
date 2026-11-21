// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `Phar::webPhar()` over the VM's SAPI.
//!
//! Under the CLI there is no request (`SG(request_info).request_method` is
//! `NULL`), so after opening the executing archive `webPhar()` returns
//! without routing. Under a web SAPI (`php -S`: `PHP_SAPI` is
//! `cli-server`; mod_php: `apache2handler`) the request comes from
//! `$_SERVER` (`REQUEST_METHOD`, `REQUEST_URI`, which the SAPI fills from
//! `SG(request_info)`), response headers go through the SAPI's header
//! operation, and a PHP entry runs through the include machinery of
//! [`crate::include`] (`phar_file_action()`, `PHAR_MIME_PHP`).

use php_phar::web::{MimeOverride, Rewrite, ServerValue, WebHost, WebPharArgs, WebResult};
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;

use crate::util::{bytes, executed, halt_defined, opt_str, run};

/// The web host: the request of the running SAPI, output through the VM.
struct CliHost<'a> {
    vm: &'a mut Vm,
    rewrite: Option<Value>,
    /// The exception the rewrite callback threw (chained as the previous
    /// exception of the PharException that follows).
    rewrite_exc: Option<php_runtime::Obj>,
    /// `sapi_module.name` (the `PHP_SAPI` constant).
    sapi: String,
}

/// `$_SERVER`, if the request has one.
fn server_array(vm: &Vm) -> Option<ZArray> {
    let mut g = vm.globals.borrow_mut();
    let i = g.find(b"_SERVER")?;
    match g.slots.get(i as usize)?.as_ref()?.deref() {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

impl CliHost<'_> {
    /// Whether a web SAPI serves a request (anything but the CLI).
    fn is_web(&self) -> bool {
        self.sapi != "cli"
    }

    /// `$_SERVER[$key]` under a web SAPI.
    fn server_value(&self, key: &str) -> Option<Value> {
        if !self.is_web() {
            return None;
        }
        server_array(self.vm)?
            .get(&php_runtime::ArrayKey::Str(ZStr::from(key)))
            .cloned()
    }

    /// A string `$_SERVER[$key]` (the `SG(request_info)` field it mirrors).
    fn server_str(&self, key: &str) -> Option<Vec<u8>> {
        match self.server_value(key)?.deref() {
            Value::Str(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    /// The `SG(request_info)` field php-cgi fills from its environment
    /// (`init_request_info()`), read from `$_SERVER` when that has it and
    /// otherwise from the CGI environment: `variables_order` without `S`
    /// leaves `$_SERVER` empty but not the request info.
    fn request_info(&self, key: &str) -> Option<Vec<u8>> {
        self.server_str(key)
            .or_else(|| (self.sapi == CGI_SAPI).then(|| self.getenv(key)).flatten())
    }
}

/// `cgi_sapi_module.name`.
const CGI_SAPI: &str = "cgi-fcgi";

impl WebHost for CliHost<'_> {
    fn sapi_name(&self) -> String {
        self.sapi.clone()
    }
    fn request_method(&self) -> Option<Vec<u8>> {
        self.request_info("REQUEST_METHOD")
    }
    fn request_uri(&self) -> Option<Vec<u8>> {
        // `SG(request_info).request_uri`: php-cgi fills it from the
        // translated `SCRIPT_NAME` (`init_request_info`), the other SAPIs
        // from the request line.
        if self.sapi == CGI_SAPI {
            self.request_info("SCRIPT_NAME")
        } else {
            self.server_str("REQUEST_URI")
        }
    }
    fn has_server(&self) -> bool {
        // `PG(http_globals)[TRACK_VARS_SERVER]` is only set once `$_SERVER`
        // was armed; an empty one (no `S` in `variables_order`) behaves
        // like the unset case, which reads the CGI environment
        // (webphar_cgi_no_path_info.phpt).
        self.is_web() && server_array(self.vm).is_some_and(|a| !a.is_empty())
    }
    fn server_get(&self, key: &str) -> Option<ServerValue> {
        self.server_value(key).map(|v| match v.deref() {
            Value::Str(s) => ServerValue::Str(s.as_bytes().to_vec()),
            _ => ServerValue::Other,
        })
    }
    fn server_set(&mut self, key: &str, value: Vec<u8>) {
        if !self.is_web() {
            return;
        }
        let mut g = self.vm.globals.borrow_mut();
        let Some(i) = g.find(b"_SERVER") else {
            return;
        };
        if let Some(Some(Value::Array(arr))) = g.slots.get_mut(i as usize) {
            arr.make_mut()
                .insert(ZStr::from(key), Value::Str(ZStr::from(value)));
        }
    }
    fn getenv(&self, key: &str) -> Option<Vec<u8>> {
        use std::os::unix::ffi::OsStringExt;
        std::env::var_os(key).map(OsStringExt::into_vec)
    }
    fn header(&mut self, line: &[u8], response_code: Option<u16>) {
        // `sapi_header_op(SAPI_HEADER_REPLACE)`: the CLI ignores headers.
        let Some(hook) = self.vm.response_header_hook else {
            return;
        };
        let code = i64::from(response_code.unwrap_or(0));
        if let Err(t) = hook(self.vm, line, true, code) {
            if self.vm.pending_throw.is_none() {
                self.vm.pending_throw = Some(t);
            }
        }
    }
    fn send_headers(&mut self) -> bool {
        true
    }
    fn write(&mut self, data: &[u8]) {
        self.vm.output(data);
    }
    fn highlight_file(&mut self, path: &[u8]) {
        let _ = self.vm.call_function("highlight_file", vec![bytes(path)]);
    }
    fn include(&mut self, path: &[u8]) -> bool {
        crate::include::run_web_entry(self.vm, path)
    }
    fn rewrite(&mut self, entry: &[u8]) -> Rewrite {
        let Some(cb) = self.rewrite.clone() else {
            return Rewrite::Failed { exception: false };
        };
        match self.vm.call_value(&cb, vec![bytes(entry)]) {
            Ok(Value::Str(s)) => Rewrite::Str(s.as_bytes().to_vec()),
            Ok(Value::Bool(_)) => Rewrite::Bool,
            Ok(_) => Rewrite::Other,
            // `zend_call_function` succeeds with an undefined return value
            // when the callback throws: "must return a string or false",
            // with the callback's exception as its previous one.
            Err(php_vm::ext::Throw::Exception(e)) => {
                self.rewrite_exc = Some(e);
                Rewrite::Other
            }
            Err(t) => {
                self.vm.pending_throw = Some(t);
                Rewrite::Failed { exception: true }
            }
        }
    }
}

/// `Phar::webPhar()`.
pub fn web_phar(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let alias = opt_str(a, 0);
    let index = opt_str(a, 1);
    let f404 = opt_str(a, 2);
    let mimes = a.opt(3).and_then(|v| match v {
        Value::Array(arr) => Some(
            arr.iter()
                .map(|(k, v)| {
                    let key = match k {
                        php_runtime::ArrayKey::Str(s) => s.as_bytes().to_vec(),
                        php_runtime::ArrayKey::Int(i) => i.to_string().into_bytes(),
                    };
                    let m = match v.deref() {
                        Value::Int(i) => MimeOverride::Int(i),
                        Value::Str(s) => MimeOverride::Str(s.as_bytes().to_vec()),
                        _ => MimeOverride::Other,
                    };
                    (key, m)
                })
                .collect(),
        ),
        _ => None,
    });
    let rewrite = a.opt(4);
    let ex = executed(vm);
    let halt = halt_defined(vm);
    let args = WebPharArgs {
        alias: alias.as_deref(),
        index: index.as_deref(),
        f404: f404.as_deref(),
        mimes,
        has_rewrite: rewrite.is_some(),
    };
    let res = {
        let sapi = vm.sapi_name().to_string_lossy();
        let mut host = CliHost {
            vm,
            rewrite,
            rewrite_exc: None,
            sapi,
        };
        let r = php_phar::web::web_phar(&mut host, ex.as_deref(), halt, &args);
        (r, host.rewrite_exc)
    };
    let (res, rewrite_exc) = res;
    if let Some(t) = vm.pending_throw.take() {
        return Err(t);
    }
    let res = match run(vm, || res) {
        Err(php_vm::ext::Throw::Exception(e)) => {
            if let Some(prev) = &rewrite_exc {
                vm.chain_exception(&e, prev);
            }
            return Err(php_vm::ext::Throw::Exception(e));
        }
        other => other?,
    };
    if res == WebResult::Exit {
        return Err(php_vm::ext::Throw::Exit);
    }
    Ok(Value::Null)
}
