// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The CLI built-in web server: `php -S <addr>:<port> [-t docroot] [router]`
//! (ref: `sapi/cli/php_cli_server.c`).
//!
//! * [`parser`] — the request side of `php_http_parser.c`.
//! * [`request`] — the parsed request, path normalisation and the
//!   URL → file mapping (`PATH_INFO`, directory index files).
//! * [`server`] — the `poll()` event loop, static files, error pages,
//!   `PHP_CLI_SERVER_WORKERS`.
//! * [`sapi`] — one PHP request on a fresh VM: output to the socket,
//!   `header()` & co., the router protocol; [`vars`] fills the
//!   superglobals, [`multipart`] parses uploads, [`headers`] implements
//!   `sapi_header_op()`.
//! * [`status`], [`mime`], [`log`], [`capture`] — response texts, the MIME
//!   table and the console log.
//!
//! The `php` binary calls [`maybe_run`] first; it returns `None` unless
//! `-S` was given.

pub mod cookie;
pub mod headers;
pub mod log;
pub mod mime;
pub mod parser;
pub mod request;
pub mod router;
pub mod sapi;
pub mod server;
pub mod status;
pub mod vars;

pub use sapi::ExtensionFactory;

use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// The INI defaults of the `cli-server` SAPI where they differ from what
/// the VM applies for the CLI (`HARDCODED_INI` is CLI-only; only
/// `display_errors=1` comes from `sapi_cli_ini_defaults`).
const SAPI_INI: &[(&str, &str)] = &[
    ("html_errors", "1"),
    ("implicit_flush", "0"),
    ("max_execution_time", "30"),
    ("register_argc_argv", "0"),
];

/// `ZEND_ATOL()`/`zend_atol`: leading integer with an optional `K`/`M`/`G`
/// suffix.
pub fn atol(s: &[u8]) -> i64 {
    let s = s.trim_ascii();
    let v = headers::strtol(s);
    match s.last().map(u8::to_ascii_lowercase) {
        Some(b'k') => v.saturating_mul(1024),
        Some(b'm') => v.saturating_mul(1024 * 1024),
        Some(b'g') => v.saturating_mul(1024 * 1024 * 1024),
        _ => v,
    }
}

/// The command line of a `php -S` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `-S` value.
    pub address: String,
    /// `-t` value.
    pub document_root: Option<Vec<u8>>,
    /// The first non-option argument.
    pub router: Option<Vec<u8>>,
    /// `-d` settings in order.
    pub ini: Vec<(String, String)>,
    /// Number of `-q` flags.
    pub quiet: i32,
}

/// Options taking a value (`OPTIONS[]` in `php_cli.c`).
const WITH_VALUE: &[u8] = b"cdfrBRFEStz";

/// Parses the arguments like `php_getopt()`; `None` when `-S` is absent or
/// an option that makes the CLI handle the invocation (`-h`, `-v`, `-i`,
/// `-m`, ...) is present.
pub fn parse_args(raw: &[Vec<u8>]) -> Option<Options> {
    let mut o = Options::default();
    let mut server = None;
    let mut i = 0;
    while i < raw.len() {
        let a = &raw[i];
        if a.as_slice() == b"--" || !a.starts_with(b"-") || a.len() == 1 {
            if a.as_slice() != b"--" {
                o.router = Some(a.clone());
            }
            break;
        }
        if a.starts_with(b"--") {
            let name = &a[2..];
            match name {
                b"server" | b"docroot" | b"define" | b"php-ini" | b"file" | b"run"
                | b"process-begin" | b"process-code" | b"process-file" | b"process-end"
                | b"zend-extension" | b"rf" | b"rc" | b"re" | b"rz" | b"ri" => {
                    let v = raw.get(i + 1).cloned().unwrap_or_default();
                    match name {
                        b"server" => server = Some(v),
                        b"docroot" => o.document_root = Some(v),
                        b"define" => o.ini.extend(php_vm::parse_cli_define(&v)),
                        b"php-ini" | b"zend-extension" => {}
                        _ => return None,
                    }
                    i += 2;
                    continue;
                }
                b"no-php-ini" | b"no-chdir" | b"hide-args" | b"profile-info" => {}
                _ => return None,
            }
            i += 1;
            continue;
        }
        let opt = a[1];
        if WITH_VALUE.contains(&opt) {
            let v = if a.len() > 2 {
                a[2..].to_vec()
            } else {
                i += 1;
                raw.get(i).cloned()?
            };
            match opt {
                b'S' => server = Some(v),
                b't' => o.document_root = Some(v),
                b'd' => o.ini.extend(php_vm::parse_cli_define(&v)),
                b'c' | b'z' => {}
                _ => return None,
            }
        } else {
            match opt {
                b'q' => o.quiet += 1,
                b'n' | b'e' | b'H' | b'C' => {}
                _ => return None,
            }
        }
        i += 1;
    }
    o.address = String::from_utf8_lossy(&server?).into_owned();
    Some(o)
}

/// Runs the built-in server when the arguments ask for it (`-S`);
/// returns the process exit status, or `None` for a normal CLI run.
pub fn maybe_run(raw: &[Vec<u8>], extensions: ExtensionFactory) -> Option<i32> {
    let opts = parse_args(raw)?;
    Some(run(&opts, extensions))
}

/// `do_cli_server()`.
pub fn run(opts: &Options, extensions: ExtensionFactory) -> i32 {
    log::set_level(log::LOG_MESSAGE - opts.quiet);
    let document_root: Vec<u8> = if let Some(dr) = &opts.document_root {
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(dr));
        match std::fs::metadata(p) {
            Err(_) => {
                log::raw(
                    format!(
                        "Directory {} does not exist.\n",
                        String::from_utf8_lossy(dr)
                    )
                    .as_bytes(),
                );
                return 1;
            }
            Ok(m) if !m.is_dir() => {
                log::raw(
                    format!("{} is not a directory.\n", String::from_utf8_lossy(dr)).as_bytes(),
                );
                return 1;
            }
            Ok(_) => std::fs::canonicalize(p)
                .map_or_else(|_| dr.clone(), |c| c.into_os_string().into_vec()),
        }
    } else {
        std::env::current_dir().map_or_else(|_| b".".to_vec(), |c| c.into_os_string().into_vec())
    };

    let Some((host, port)) = server::parse_addr(&opts.address) else {
        log::raw(format!("Invalid address: {}\n", opts.address).as_bytes());
        return 1;
    };
    let listener = match server::listen(&host, port) {
        Ok(l) => l,
        Err(reason) => {
            let shown = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            log::logf(
                log::LOG_ERROR,
                &format!("Failed to listen on {shown} (reason: {reason})"),
            );
            return 1;
        }
    };
    if listener.set_nonblocking(true).is_err() {
        log::logf(log::LOG_ERROR, "Failed to make server socket non-blocking");
        return 1;
    }
    let port = listener.local_addr().map_or(port, |a| a.port());
    let workers = server::startup_workers();

    let mut ini: Vec<(String, String)> = SAPI_INI
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    ini.extend(opts.ini.iter().cloned());
    let post_max_size = ini
        .iter()
        .rev()
        .find(|(k, _)| k == "post_max_size")
        .map_or(8 * 1024 * 1024, |(_, v)| atol(v.as_bytes()));
    // The router path is resolved now so that a script changing the
    // working directory does not affect later requests.
    let router = opts.router.as_ref().map(|r| {
        std::fs::canonicalize(std::ffi::OsStr::from_bytes(r))
            .map_or_else(|_| r.clone(), |c| c.into_os_string().into_vec())
    });
    let config = sapi::ServerConfig {
        document_root,
        host: host.clone(),
        port,
        router,
        ini,
        extensions,
    };
    let mut srv = server::Server::new(listener, config, post_max_size);
    server::banner(&host, port);
    server::install_sigint();
    let r = srv.run();
    server::wait_workers(&workers);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn argument_parsing() {
        let o = parse_args(&args(&[
            "-d",
            "a=1",
            "-t",
            "/tmp",
            "-n",
            "-S",
            "localhost:0",
            "router.php",
            "x",
        ]))
        .unwrap();
        assert_eq!(o.address, "localhost:0");
        assert_eq!(o.document_root.as_deref(), Some(&b"/tmp"[..]));
        assert_eq!(o.router.as_deref(), Some(&b"router.php"[..]));
        assert_eq!(o.ini, vec![("a".to_string(), "1".to_string())]);
        let o = parse_args(&args(&["-S127.0.0.1:80", "-q", "-q"])).unwrap();
        assert_eq!(o.address, "127.0.0.1:80");
        assert_eq!(o.quiet, 2);
        assert_eq!(o.router, None);
        assert!(parse_args(&args(&["-r", "echo 1;"])).is_none());
        assert!(parse_args(&args(&["-S", "a:1", "-v"])).is_none());
        assert!(parse_args(&args(&["file.php", "-S", "a:1"])).is_none());
    }

    #[test]
    fn atol_suffixes() {
        assert_eq!(atol(b"8M"), 8 * 1024 * 1024);
        assert_eq!(atol(b" 2k"), 2048);
        assert_eq!(atol(b"10"), 10);
        assert_eq!(atol(b"x"), 0);
    }
}
