// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/curl` for the phprust VM: binds the `php-curl` libcurl wrapper
//! through the `php_vm::ext` extension API (ref: `ext/curl/interface.c`,
//! `multi.c`, `share.c`, `curl_file.c`).
//!
//! * `CurlHandle`, `CurlMultiHandle`, `CurlShareHandle`,
//!   `CurlSharePersistentHandle`: final opaque classes whose native state
//!   lives in [`state`].
//! * Transfer callbacks run PHP code through [`dispatch::Dispatch`], the
//!   [`php_curl::Handler`] lent to libcurl for the duration of
//!   `curl_exec()`/`curl_multi_exec()`.
//! * `CURLOPT_FILE`/`INFILE`/`WRITEHEADER`/`STDERR` use php-ext-streams
//!   stream resources.

// Native function handlers share one signature (`VmResult` even when
// infallible) and use PHP's short argument names.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps, clippy::many_single_char_names)]

mod dispatch;
mod easy;
mod file;
mod mime;
mod multi;
mod setopt;
mod share;
mod state;
mod streams;

use php_runtime::Value;
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{Extension, InfoItem, ModuleBuilder};
use php_vm::Vm;

/// The `curl` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Curl;

impl Extension for Curl {
    fn name(&self) -> &'static str {
        "curl"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        // php-src calls `curl_global_init(CURL_GLOBAL_DEFAULT)` here, at
        // MINIT. It loads the OpenSSL configuration (2.4 M instructions, 4 %
        // of `php -r 'echo 1;'`), so it is deferred to the first handle:
        // `php_curl`'s `Easy::new`, `Multi::new` and `Share::new` run it
        // before any other libcurl call, and nothing before a handle exists
        // can observe the difference. `curl_version()` and `phpinfo()` read
        // `curl_version_info()`, static data libcurl fills without the
        // global init; the constants are registered below as usual.
        m.ini(IniDirective::new(
            "curl.cainfo",
            Some(""),
            Access::SYSTEM,
            ModifyHandler::None,
            "curl",
        ));
        for &(name, value, deprecated) in php_curl::consts::PHP_CONSTANTS {
            match deprecated {
                Some((since, msg)) => m.constant_deprecated(name, Value::Int(value), since, msg),
                None => m.constant(name, Value::Int(value)),
            };
        }
        m.info(module_info);
        easy::register_class(m);
        multi::register_class(m);
        share::register_classes(m);
        file::register(m);
        easy::register_functions(m);
        multi::register_functions(m);
        share::register_functions(m);
    }
}

/// `PHP_MINFO_FUNCTION(curl)`.
fn module_info(_vm: &mut Vm) -> Vec<InfoItem> {
    let row = |cells: &[&str]| InfoItem::Row(cells.iter().map(|c| (*c).to_string()).collect());
    let mut out = vec![InfoItem::TableStart, row(&["cURL support", "enabled"])];
    if let Some(d) = php_curl::version_info() {
        let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        out.push(row(&["cURL Information", &s(&d.version)]));
        out.push(row(&["Age", &d.age.to_string()]));
        if d.features != 0 {
            out.push(row(&["Features"]));
            for (name, bit) in php_curl::version::FEATURES {
                let on = i64::from(d.features) & bit != 0;
                out.push(row(&[name, if on { "Yes" } else { "No" }]));
            }
        }
        let protocols: Vec<String> = d.protocols.iter().map(|p| s(p)).collect();
        out.push(row(&["Protocols", &protocols.join(", ")]));
        out.push(row(&["Host", &s(&d.host)]));
        if !d.ssl_version.is_empty() {
            out.push(row(&["SSL Version", &s(&d.ssl_version)]));
        }
        if !d.libz_version.is_empty() {
            out.push(row(&["ZLib Version", &s(&d.libz_version)]));
        }
        if !d.ares.is_empty() {
            // Sic: ext/curl labels the c-ares version "ZLib Version".
            out.push(row(&["ZLib Version", &s(&d.ares)]));
        }
        if !d.libidn.is_empty() {
            out.push(row(&["libIDN Version", &s(&d.libidn)]));
        }
        if !d.libssh_version.is_empty() {
            out.push(row(&["libSSH Version", &s(&d.libssh_version)]));
        }
    }
    out.push(InfoItem::TableEnd);
    out.push(InfoItem::IniEntries);
    out
}
