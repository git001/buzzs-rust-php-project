// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `php-cgi` binary: the CGI/1.1 SAPI (ref: sapi/cgi/cgi_main.c).
//! FastCGI (`-b`, `FCGI_WEB_SERVER_ADDRS`, an accepting socket on stdin)
//! is not supported.
//!
//! It shares the configuration search, the extension list and the startup
//! checks with the `php` binary, and runs the request through the
//! built-in server's request machinery in CGI mode
//! ([`php_cli_server::sapi::CgiInfo`]): the environment becomes
//! `$_SERVER`, the body is read from standard input (`CONTENT_LENGTH`
//! bytes), and the response head (`Status:`, `X-Powered-By:`,
//! `Content-type:`) precedes the output unless `-q`.
//!
//! `run-tests.php` runs it as `php-cgi [-d ...] -C <file>` with
//! `REDIRECT_STATUS=1`, `REQUEST_METHOD`, `QUERY_STRING`,
//! `SCRIPT_FILENAME`/`PATH_TRANSLATED` (the test file), `HTTP_COOKIE`,
//! `CONTENT_TYPE` and `CONTENT_LENGTH` set and the body on standard input.

#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
mod env;
#[allow(dead_code)]
#[path = "../extensions.rs"]
mod extensions;
#[allow(dead_code)]
#[path = "../getopt.rs"]
mod getopt;
#[allow(dead_code)]
#[path = "../memcap.rs"]
mod memcap;
#[path = "../startup.rs"]
mod startup;

use config::Config;
use getopt::{GetOpt, Opt, INVALID};
use php_cli_server::parser::Method;
use php_cli_server::request::Request;
use php_cli_server::sapi::{CgiInfo, CliServerModule, PreparedVm, RequestCtx, ServerOutput};
use php_runtime::{Value, ZStr};
use php_vm::ext::{Args, Extension, VmResult};
use php_vm::{Vm, VmOptions};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::rc::Rc;

/// `PHP_CLI_BUILD_DATE` (shared with the `php` binary's build script).
const BUILD_DATE: &str = env!("PHP_CLI_BUILD_DATE");

/// Stack the VM needs for deep recursion in user code.
const VM_STACK: u64 = 256 << 20;

/// The INI defaults of the CGI SAPI where they differ from what the VM
/// applies for the CLI (`main.c` defaults; the CGI SAPI has no
/// `ini_defaults` and no `HARDCODED_INI`).
const SAPI_INI: &[(&str, &str)] = &[
    ("display_errors", "1"),
    ("html_errors", "1"),
    ("implicit_flush", "0"),
    ("max_execution_time", "30"),
    ("register_argc_argv", "0"),
];

/// The option table (`OPTIONS[]` of `cgi_main.c`).
const OPTIONS: &[Opt] = &[
    Opt {
        ch: b'a',
        need_param: 0,
        name: "interactive",
    },
    Opt {
        ch: b'b',
        need_param: 1,
        name: "bindpath",
    },
    Opt {
        ch: b'C',
        need_param: 0,
        name: "no-chdir",
    },
    Opt {
        ch: b'c',
        need_param: 1,
        name: "php-ini",
    },
    Opt {
        ch: b'd',
        need_param: 1,
        name: "define",
    },
    Opt {
        ch: b'e',
        need_param: 0,
        name: "profile-info",
    },
    Opt {
        ch: b'f',
        need_param: 1,
        name: "file",
    },
    Opt {
        ch: b'h',
        need_param: 0,
        name: "help",
    },
    Opt {
        ch: b'i',
        need_param: 0,
        name: "info",
    },
    Opt {
        ch: b'l',
        need_param: 0,
        name: "syntax-check",
    },
    Opt {
        ch: b'm',
        need_param: 0,
        name: "modules",
    },
    Opt {
        ch: b'n',
        need_param: 0,
        name: "no-php-ini",
    },
    Opt {
        ch: b'q',
        need_param: 0,
        name: "no-header",
    },
    Opt {
        ch: b's',
        need_param: 0,
        name: "syntax-highlight",
    },
    Opt {
        ch: b's',
        need_param: 0,
        name: "syntax-highlighting",
    },
    Opt {
        ch: b'w',
        need_param: 0,
        name: "strip",
    },
    Opt {
        ch: b'?',
        need_param: 0,
        name: "usage",
    },
    Opt {
        ch: b'v',
        need_param: 0,
        name: "version",
    },
    Opt {
        ch: b'T',
        need_param: 1,
        name: "timing",
    },
    Opt {
        ch: b'-',
        need_param: 0,
        name: "",
    },
];

const SECURITY_ALERT: &str = "<b>Security Alert!</b> The PHP CGI cannot be accessed directly.\n\n\
<p>This PHP CGI binary was compiled with force-cgi-redirect enabled.  This\n\
means that a page will only be served up if the REDIRECT_STATUS CGI variable is\n\
set, e.g. via an Apache Action directive.</p>\n\
<p>For more information as to <i>why</i> this behaviour exists, see the <a href=\"https://www.php.net/security.cgi-bin\">\
manual page for CGI security</a>.</p>\n\
<p>For more information about changing this behaviour or re-enabling this webserver,\n\
consult the installation file that came with this distribution, or visit \n\
<a href=\"https://www.php.net/install.windows\">the manual page</a>.</p>\n";

const MODULES_SNIPPET: &[u8] = b"<?php
$m = get_loaded_extensions();
usort($m, 'strcasecmp');
echo \"[PHP Modules]\\n\";
foreach ($m as $e) echo $e, \"\\n\";
echo \"\\n[Zend Modules]\\n\";
$z = get_loaded_extensions(true);
sort($z);
foreach ($z as $e) echo $e, \"\\n\";
echo \"\\n\";
";

/// `php_cgi_usage()`.
fn usage(argv0: &[u8]) -> Vec<u8> {
    let prog = argv0
        .iter()
        .rposition(|&c| c == b'/')
        .map_or(&b"php-cgi"[..], |i| &argv0[i + 1..]);
    let prog = String::from_utf8_lossy(prog);
    format!(
        "Usage: {prog} [-q] [-h] [-s] [-v] [-i] [-f <file>]\n       {prog} <file> [args...]\n  -a               Run interactively\n  -b <address:port>|<port> Bind Path for external FASTCGI Server mode\n  -C               Do not chdir to the script's directory\n  -c <path>|<file> Look for php.ini file in this directory\n  -n               No php.ini file will be used\n  -d foo[=bar]     Define INI entry foo with value 'bar'\n  -e               Generate extended information for debugger/profiler\n  -f <file>        Parse <file>.  Implies `-q'\n  -h               This help\n  -i               PHP information\n  -l               Syntax check only (lint)\n  -m               Show compiled in modules\n  -q               Quiet-mode.  Suppress HTTP Header output.\n  -s               Display colour syntax highlighted source.\n  -v               Version number\n  -w               Display source with stripped comments and whitespace.\n  -T <count>       Measure execution time of script repeated <count> times.\n"
    )
    .into_bytes()
}

/// `php_print_version()` for this SAPI.
fn version_banner() -> String {
    format!(
        "PHP {} (cgi-fcgi) (built: {BUILD_DATE}) (NTS)\nCopyright \u{a9} The PHP Group and Contributors\nZend Engine v{}, Copyright \u{a9} Zend by Perforce\n",
        php_vm::PHP_VERSION,
        php_vm::ZEND_VERSION
    )
}

/// An INI boolean as `OnUpdateBool` reads it (`zend_ini_parse_bool`).
fn ini_bool(v: &str) -> bool {
    let v = v.trim();
    v.eq_ignore_ascii_case("on")
        || v.eq_ignore_ascii_case("yes")
        || v.eq_ignore_ascii_case("true")
        || v.trim_start_matches(['+', '-'])
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<i64>()
            .is_ok_and(|n| n != 0)
}

/// A `cgi.*` boolean from the configuration, else its default.
fn cfg_bool(cfg: &Config, name: &str, default: bool) -> bool {
    cfg.get(name).map_or(default, ini_bool)
}

/// The behaviour chosen on the command line (`PHP_MODE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Behavior {
    #[default]
    Standard,
    Highlight,
    Lint,
    Strip,
}

/// The options of the third `php_getopt()` pass (only when not started
/// by a web server).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CommandLine {
    behavior: Behavior,
    no_headers: bool,
    no_chdir: bool,
    script_file: Option<Vec<u8>>,
    /// `-i`, `-m`, `-v`.
    info: Option<u8>,
    /// `-a`: "Interactive mode enabled" is printed (nothing else changes).
    interactive: bool,
    /// Index of the first non-option argument.
    optind: usize,
}

/// The third option pass of `main()` (`!cgi && !fastcgi`).
fn parse_command_line(argv: &[Vec<u8>]) -> CommandLine {
    let mut cl = CommandLine::default();
    let mut g = GetOpt::new(argv, OPTIONS, false);
    while let Some((c, v)) = g.next_opt() {
        match u8::try_from(c).unwrap_or(0) {
            b'a' => cl.interactive = true,
            b'C' => cl.no_chdir = true,
            b'f' => {
                cl.script_file = v;
                cl.no_headers = true;
            }
            b'i' | b'm' | b'v' => {
                cl.info = Some(u8::try_from(c).unwrap_or(0));
                break;
            }
            b'l' => {
                cl.no_headers = true;
                cl.behavior = Behavior::Lint;
            }
            b'q' => cl.no_headers = true,
            b's' => cl.behavior = Behavior::Highlight,
            b'w' => cl.behavior = Behavior::Strip,
            _ => {}
        }
    }
    cl.optind = g.optind;
    cl
}

php_memlimit::install_global_allocator!();

/// An allocation crossed the hard ceiling above `memory_limit`: PHP's
/// message on stderr, then exit 137 without unwinding (as the `php` binary).
fn hard_limit(e: php_memlimit::HardLimitExceeded) -> ! {
    let _ = std::io::stderr().write_all(format!("{e} in Unknown on line 0\n").as_bytes());
    // SAFETY: `_exit` terminates the process immediately; nothing here
    // relies on destructors or atexit handlers (PHP does the same).
    unsafe { libc::_exit(137) }
}

fn main() {
    let argv: Vec<Vec<u8>> = std::env::args_os().map(OsStringExt::into_vec).collect();
    // The request heap: `memory_limit` accounting and the address-space
    // cap, as in the `php` binary.
    memcap::limit_memory();
    php_memlimit::set_hard_limit_handler(hard_limit);
    php_memlimit::set_tracked(php_memlimit::tracked_from_env(
        std::env::var_os("USE_ZEND_ALLOC")
            .as_deref()
            .map(std::os::unix::ffi::OsStrExt::as_bytes),
        std::env::var_os("USE_TRACKED_ALLOC")
            .as_deref()
            .map(std::os::unix::ffi::OsStrExt::as_bytes),
    ));
    // SAFETY: ignoring SIGPIPE like cgi_main.c, so writes to a closed pipe
    // fail with EPIPE instead of killing the process.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // The VM runs on a thread with a large stack (deep recursion).
    let status = std::thread::Builder::new()
        .stack_size(usize::try_from(VM_STACK).unwrap_or(usize::MAX))
        .spawn(move || run(&argv))
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or(255);
    finish(status);
}

/// Flushes the standard streams and ends the process with `status`.
fn finish(status: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(status)
}

fn out(bytes: &[u8]) {
    let mut o = std::io::stdout();
    let _ = o.write_all(bytes);
    let _ = o.flush();
}

/// `main()` of `cgi_main.c` without the FastCGI loop.
#[allow(clippy::too_many_lines)]
fn run(argv: &[Vec<u8>]) -> i32 {
    php_memlimit::bind_to_current_thread();
    let argv0 = argv.first().cloned().unwrap_or_else(|| b"php-cgi".to_vec());
    let mut environment = env::Env::from_process();
    let cgi = env::is_cgi(&environment);
    let skip_getopt = env::skip_getopt(environment.get("QUERY_STRING").as_deref());

    // First pass: the options needed before startup.
    let mut opts = config::Options {
        binary: std::env::current_exe().ok(),
        cgi: true,
        ..config::Options::default()
    };
    if !skip_getopt {
        let mut g = GetOpt::new(argv, OPTIONS, false);
        while let Some((c, v)) = g.next_opt() {
            match u8::try_from(c).unwrap_or(0) {
                b'c' => opts.path_override = v,
                b'n' => opts.ignore = true,
                b'd' => opts
                    .defines
                    .extend(php_vm::parse_cli_define(&v.unwrap_or_default())),
                _ => {}
            }
        }
    }
    let cfg = Config::load(&opts);
    for w in &cfg.warnings {
        let _ = std::io::stderr().write_all(format!("PHP:  {w}\n").as_bytes());
    }
    if let Err(status) = startup::module_startup(&cfg) {
        return status;
    }

    // `cgi.force_redirect`: refuse to run unless the web server says it
    // redirected the request.
    if cgi && cfg_bool(&cfg, "cgi.force_redirect", true) {
        let var = cfg
            .get("cgi.redirect_status_env")
            .filter(|v| !v.is_empty())
            .unwrap_or("REDIRECT_STATUS");
        if environment.get(var).is_none() {
            out(SECURITY_ALERT.as_bytes());
            return 255;
        }
    }

    // Second pass: help and invalid options.
    if !skip_getopt {
        let mut g = GetOpt::new(argv, OPTIONS, true);
        while let Some((c, _)) = g.next_opt() {
            if c == INVALID || c == i32::from(b'h') || c == i32::from(b'?') {
                out(&usage(&argv0));
                return i32::from(c == INVALID);
            }
        }
    }

    let settings = env::Settings {
        fix_pathinfo: cfg_bool(&cfg, "cgi.fix_pathinfo", true),
        discard_path: cfg_bool(&cfg, "cgi.discard_path", false),
        doc_root: cfg
            .get("doc_root")
            .filter(|v| !v.is_empty())
            .map(|v| v.as_bytes().to_vec()),
    };
    let mut info = env::init_request_info(&mut environment, &settings, &env::RealFs);
    environment.apply();

    let mut cl = CommandLine::default();
    let mut source: Option<Vec<u8>> = None;
    let mut script_argv: Vec<Vec<u8>> = Vec::new();
    if !cgi && !skip_getopt {
        cl = parse_command_line(argv);
        if cl.interactive {
            out(b"Interactive mode enabled\n\n");
        }
        match cl.info {
            Some(b'v') => {
                out(version_banner().as_bytes());
                return 0;
            }
            Some(b'i') => source = Some(b"<?php phpinfo();".to_vec()),
            Some(b'm') => {
                source = Some(MODULES_SNIPPET.to_vec());
                cl.no_headers = true;
            }
            _ => {}
        }
        let optind = cl.optind.min(argv.len());
        if let Some(f) = &cl.script_file {
            // `SG(request_info).argv`: the script, then the arguments.
            info.path_translated = Some(f.clone());
            script_argv = std::iter::once(f.clone())
                .chain(argv[optind..].iter().cloned())
                .collect();
        } else if argv.len() > optind {
            // The script and the arguments after it.
            info.path_translated = Some(argv[optind].clone());
            script_argv = argv[optind..].to_vec();
        }
        // The remaining arguments form the query string; without `-f` the
        // script is one of them (php_optind is not moved past it).
        if info.query_string.is_none() && argv.len() > optind {
            let sep = cfg
                .get("arg_separator.input")
                .and_then(|s| s.as_bytes().first().copied())
                .unwrap_or(b'&');
            info.query_string = Some(argv[optind..].join(&sep));
        }
    }

    // The copy of standard input made for `-s`/`-w`, removed at the end.
    let mut stdin_copy: Option<Vec<u8>> = None;
    match cl.behavior {
        Behavior::Lint => {
            // `goto do_repeat`: the arguments after the first file are
            // linted in turn (`-f` names the first one).
            let mut files = vec![info.path_translated.clone()];
            if !cgi {
                let mut optind = cl.optind;
                while argv.len() > optind + 1 {
                    optind += 1;
                    files.push(Some(argv[optind].clone()));
                }
            }
            let mut status = 0;
            for f in files {
                match lint(&cfg, f.as_deref()) {
                    Some(0) => {}
                    Some(_) => status = 255,
                    None => return 255,
                }
            }
            return status;
        }
        Behavior::Highlight | Behavior::Strip
            if source.is_none()
                && info.path_translated.as_deref().is_none_or(|p| {
                    std::fs::File::open(std::ffi::OsStr::from_bytes(p)).is_ok()
                }) =>
        {
            // Standard input when no file is named ("Standard input code").
            let path = match info.path_translated.clone() {
                Some(p) => p,
                None => {
                    stdin_copy = stdin_to_temp();
                    stdin_copy.clone().unwrap_or_default()
                }
            };
            let mut code = if cl.behavior == Behavior::Highlight {
                b"<?php highlight_file('".to_vec()
            } else {
                b"<?php echo php_strip_whitespace('".to_vec()
            };
            for &c in &path {
                if c == b'\'' || c == b'\\' {
                    code.push(b'\\');
                }
                code.push(c);
            }
            code.extend_from_slice(b"');");
            source = Some(code);
        }
        _ => {}
    }

    // The request body: CONTENT_LENGTH bytes of standard input
    // (`sapi_cgi_read_post`).
    let content = if info.content_length > 0 {
        let mut body = Vec::new();
        let n = u64::try_from(info.content_length).unwrap_or(0);
        let _ = std::io::stdin().lock().take(n).read_to_end(&mut body);
        Some(body)
    } else {
        None
    };

    let request = build_request(&info, content);
    let php_self = env::php_self(
        info.request_uri.as_deref(),
        std::env::var_os("PATH_INFO")
            .as_deref()
            .map(OsStrExt::as_bytes),
        settings.fix_pathinfo,
    );
    let cgi_info = CgiInfo {
        no_headers: cl.no_headers,
        no_chdir: cl.no_chdir,
        php_self,
        source,
        argv: script_argv,
        ..CgiInfo::default()
    };
    let ctx = Rc::new(RequestCtx::new_cgi(request, cgi_info));
    if info.response_code != 200 {
        let _ = ctx.header_op(php_cli_server::headers::HeaderOp::SetStatus(
            info.response_code,
        ));
    }

    let ini = sapi_ini(&cfg);
    let server_cfg = php_cli_server::sapi::ServerConfig {
        document_root: Vec::new(),
        host: String::new(),
        port: 0,
        router: None,
        ini: ini.clone(),
        extensions: extensions::all,
    };
    let prepared = new_prepared_vm(&cfg, ini);
    let (_, vm) = php_cli_server::sapi::run_request(prepared, &ctx, &server_cfg, false);
    // The copy of standard input `stdin_to_temp` may have made.
    if let Some(path) = stdin_copy {
        let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&path));
    }
    let _ = std::io::stdout().flush();
    // The process exits next: skip the VM's destruction.
    std::mem::forget(vm);
    ctx.exit_status.get()
}

/// The INI entries for the VM: the SAPI defaults, then the configuration.
fn sapi_ini(cfg: &Config) -> Vec<(String, String)> {
    SAPI_INI
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .chain(cfg.entries.iter().cloned())
        .collect()
}

/// The request as the built-in server's machinery sees it.
fn build_request(info: &env::RequestInfo, content: Option<Vec<u8>>) -> Request {
    let method_name = info.request_method.clone().unwrap_or_default();
    let mut request = Request {
        method: method_from(&method_name),
        protocol_version: 100,
        query_string: info.query_string.clone(),
        path_translated: info.path_translated.clone(),
        content,
        ..Request::default()
    };
    if !info.content_type.is_empty() {
        request
            .headers
            .push((b"content-type".to_vec(), info.content_type.clone()));
    }
    if let Some(cookie) = std::env::var_os("HTTP_COOKIE") {
        request
            .headers
            .push((b"cookie".to_vec(), cookie.as_bytes().to_vec()));
    }
    if let Some(auth) = &info.authorization {
        request
            .headers
            .push((b"authorization".to_vec(), auth.clone()));
    }
    for (k, v) in std::env::vars_os() {
        if let Some(name) = env::request_header_name(k.as_bytes()) {
            request
                .headers_original_case
                .push((name, v.as_bytes().to_vec()));
        }
    }
    request
}

/// `SG(request_info).request_method` as the server's method enum.
fn method_from(name: &[u8]) -> Method {
    const METHODS: &[Method] = &[
        Method::Delete,
        Method::Get,
        Method::Head,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Query,
        Method::Connect,
        Method::Options,
        Method::Trace,
        Method::Copy,
        Method::Lock,
        Method::Mkcol,
        Method::Move,
        Method::Mkcalendar,
        Method::Propfind,
        Method::Proppatch,
        Method::Search,
        Method::Unlock,
        Method::Report,
        Method::Mkactivity,
        Method::Checkout,
        Method::Merge,
        Method::MSearch,
        Method::Notify,
        Method::Subscribe,
        Method::Unsubscribe,
    ];
    METHODS
        .iter()
        .copied()
        .find(|m| m.as_str().as_bytes() == name)
        .unwrap_or(if name.is_empty() {
            Method::Get
        } else {
            Method::NotImplemented
        })
}

thread_local! {
    /// `php_ini_opened_path` and `php_ini_scanned_files`.
    static INI_FILES: RefCell<(Option<Vec<u8>>, Option<Vec<u8>>)> = const { RefCell::new((None, None)) };
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_loaded_file(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(INI_FILES.with(|f| {
        f.borrow()
            .0
            .as_deref()
            .map_or(Value::Bool(false), |s| Value::Str(ZStr::from(s)))
    }))
}

#[allow(clippy::unnecessary_wraps)]
fn php_ini_scanned_files(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(INI_FILES.with(|f| {
        f.borrow()
            .1
            .as_deref()
            .map_or(Value::Bool(false), |s| Value::Str(ZStr::from(s)))
    }))
}

/// Creates the request's VM (module startup) writing to the request
/// context's output.
fn new_prepared_vm(cfg: &Config, ini: Vec<(String, String)>) -> PreparedVm {
    let opts = VmOptions { ini };
    let mut exts = extensions::all();
    exts.push(Box::new(CliServerModule { cgi: true }));
    let refs: Vec<&dyn Extension> = exts.iter().map(AsRef::as_ref).collect();
    let slot = Rc::new(RefCell::new(None));
    let mut vm = Vm::with_extensions(&opts, Box::new(ServerOutput(slot.clone())), &refs);
    configure(&mut vm, cfg);
    PreparedVm { vm, slot }
}

/// What every VM of this binary gets after module startup.
fn configure(vm: &mut Vm, cfg: &Config) {
    startup::report_module_diagnostics(cfg, &std::mem::take(&mut vm.module_diagnostics));
    vm.set_configuration(cfg.configuration.values.clone());
    vm.set_native_stack_size(usize::try_from(VM_STACK).unwrap_or(usize::MAX));
    INI_FILES.with(|f| *f.borrow_mut() = (cfg.opened_path.clone(), cfg.scanned_files.clone()));
    vm.register_function("php_ini_loaded_file(): string|false", php_ini_loaded_file);
    vm.register_function(
        "php_ini_scanned_files(): string|false",
        php_ini_scanned_files,
    );
}

/// A VM writing straight to standard output (lint mode and the tests of
/// the shared configuration code).
fn new_vm(cfg: &Config) -> Vm {
    let opts = VmOptions { ini: sapi_ini(cfg) };
    let stdout: Box<dyn Write> = Box::new(std::io::stdout());
    let mut exts = extensions::all();
    exts.push(Box::new(CliServerModule { cgi: true }));
    let refs: Vec<&dyn Extension> = exts.iter().map(AsRef::as_ref).collect();
    let mut vm = Vm::with_extensions(&opts, stdout, &refs);
    configure(&mut vm, cfg);
    vm
}

/// `-l`: `php_lint_script()`; "No syntax errors detected in <file>" or
/// "Errors parsing <file>" (`Some(255)`). Without a file standard input is
/// linted as "Standard input code"; a file that cannot be opened prints
/// "No input file specified." and ends the run (`None`).
fn lint(cfg: &Config, path: Option<&[u8]>) -> Option<i32> {
    let (src, shown) = match path {
        Some(path) => {
            let Ok(src) = std::fs::read(std::ffi::OsStr::from_bytes(path)) else {
                out(b"No input file specified.\n");
                return None;
            };
            (src, path.to_vec())
        }
        None => {
            let mut src = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut src);
            (src, b"Standard input code".to_vec())
        }
    };
    let mut vm = new_vm(cfg);
    let ok = vm.lint_script(&src, &ZStr::from(shown.as_slice()));
    let shown = String::from_utf8_lossy(&shown);
    let verdict = if ok {
        format!("No syntax errors detected in {shown}\n")
    } else {
        format!("Errors parsing {shown}\n")
    };
    vm.echo(verdict.as_bytes());
    let _ = vm.shutdown(Ok(()));
    let _ = std::io::stdout().flush();
    std::mem::forget(vm);
    Some(if ok { 0 } else { 255 })
}

/// Standard input copied to a new temporary file (a random name created
/// exclusively, `php_open_temporary_fd()`), for `-s`/`-w` without a file
/// (C reads the stream directly; `highlight_file()` and
/// `php_strip_whitespace()` take a path). Returns its path.
fn stdin_to_temp() -> Option<Vec<u8>> {
    let mut src = Vec::new();
    std::io::stdin().lock().read_to_end(&mut src).ok()?;
    let (mut file, path) = php_streams::temp::open_temporary_fd(
        None,
        Some(b"php-cgi-stdin-"),
        php_streams::temp::SILENT,
    )?;
    if file.write_all(&src).is_err() {
        let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&path));
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn command_line_options() {
        let cl = parse_command_line(&args(&["php-cgi", "-C", "-d", "a=1", "x.php", "y"]));
        assert!(cl.no_chdir);
        assert!(!cl.no_headers);
        assert_eq!(cl.optind, 4);
        let cl = parse_command_line(&args(&["php-cgi", "-q", "-f", "x.php"]));
        assert!(cl.no_headers);
        assert_eq!(cl.script_file.as_deref(), Some(&b"x.php"[..]));
        let cl = parse_command_line(&args(&["php-cgi", "-l", "x.php"]));
        assert_eq!(cl.behavior, Behavior::Lint);
        assert!(cl.no_headers);
        let cl = parse_command_line(&args(&["php-cgi", "-n", "-v"]));
        assert_eq!(cl.info, Some(b'v'));
        let cl = parse_command_line(&args(&["php-cgi", "-s", "x.php"]));
        assert_eq!(cl.behavior, Behavior::Highlight);
    }

    #[test]
    fn usage_names_the_program() {
        let u = String::from_utf8(usage(b"/usr/bin/php-cgi8.5")).unwrap();
        assert!(u.starts_with("Usage: php-cgi8.5 [-q] [-h] [-s] [-v] [-i] [-f <file>]\n"));
        assert!(u.ends_with("repeated <count> times.\n"));
    }

    #[test]
    fn version_banner_names_the_sapi() {
        assert!(version_banner().contains(" (cgi-fcgi) (built: "));
    }

    #[test]
    fn methods() {
        assert_eq!(method_from(b"POST"), Method::Post);
        assert_eq!(method_from(b"PUT"), Method::Put);
        assert_eq!(method_from(b""), Method::Get);
        assert_eq!(method_from(b"BREW"), Method::NotImplemented);
    }

    #[test]
    fn ini_booleans() {
        assert!(ini_bool("1"));
        assert!(ini_bool("On"));
        assert!(!ini_bool("0"));
        assert!(!ini_bool("off"));
        assert!(!ini_bool(""));
    }

    #[test]
    fn request_from_environment() {
        let info = env::RequestInfo {
            request_method: Some(b"POST".to_vec()),
            query_string: Some(b"a=1".to_vec()),
            content_type: b"application/x-www-form-urlencoded".to_vec(),
            content_length: 3,
            path_translated: Some(b"/t/x.php".to_vec()),
            ..env::RequestInfo::default()
        };
        let r = build_request(&info, Some(b"b=2".to_vec()));
        assert_eq!(r.method, Method::Post);
        assert_eq!(
            r.header(b"content-type"),
            Some(&b"application/x-www-form-urlencoded"[..])
        );
        assert_eq!(r.content.as_deref(), Some(&b"b=2"[..]));
        assert_eq!(r.path_translated.as_deref(), Some(&b"/t/x.php"[..]));
    }
}
