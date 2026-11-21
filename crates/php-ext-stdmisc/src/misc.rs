// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mail()`, `error_log()`, `assert_options()`, `phpinfo()`,
//! `phpcredits()` and `dl()`.

use crate::credits;
use crate::ctx::{global, RtExt};
use php_runtime::{RtResult, Value, ZArray, ZStr};
use php_stdmisc::info::{self, InfoModule, InfoPrinter, InfoSource};
use php_stdmisc::mail::{self, ErrorLogSettings, HeadersArg, LogTarget, MailSettings, ScriptInfo};
use php_stdmisc::misc::{self, AssertState};
use php_stdmisc::system::{self, PageInfo};
use php_vm::ext::{Args, InfoItem, ModuleBuilder, VmResult};
use php_vm::Vm;

/// The `mail.*` INI settings.
fn mail_settings(vm: &Vm) -> MailSettings {
    let s = |n: &str| vm.ini_string(n).unwrap_or_default();
    MailSettings {
        sendmail_path: s("sendmail_path"),
        force_extra_parameters: s("mail.force_extra_parameters"),
        add_x_header: vm.ini_bool("mail.add_x_header"),
        log: s("mail.log"),
        cr_lf_mode: vm
            .ini_string("mail.cr_lf_mode")
            .unwrap_or_else(|| b"crlf".to_vec()),
        mixed_lf_and_crlf: vm.ini_bool("mail.mixed_lf_and_crlf"),
    }
}

/// The executing script's file, line and owner.
fn script_info(vm: &Vm) -> ScriptInfo {
    let (file, line) = vm.current_location();
    let path = vm.script_path.as_bytes();
    let page = PageInfo::for_script((!path.is_empty()).then_some(path));
    ScriptInfo {
        filename: file.as_bytes().to_vec(),
        lineno: line,
        uid: page.uid,
    }
}

/// `php_format_date("d-M-Y H:i:s e", ts)` through the date extension
/// (UTC formatting when `date()` is unavailable).
fn log_date(vm: &mut Vm, ts: i64) -> String {
    if vm.native_function("date").is_none() {
        return mail::format_log_date_utc(ts);
    }
    match vm.call_value(
        &Value::from("date"),
        vec![Value::from("d-M-Y H:i:s e"), Value::Int(ts)],
    ) {
        Ok(Value::Str(s)) => s.to_string_lossy(),
        _ => mail::format_log_date_utc(ts),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn mail_fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (to, subject, message) = (a.str(0), a.str(1), a.str(2));
    let headers = a.at(3);
    let params = a.has(4).then(|| a.str(4));
    let harg = match &headers {
        Value::Str(s) => HeadersArg::Str(s.as_bytes()),
        Value::Array(arr) => HeadersArg::Array(arr),
        _ => HeadersArg::None,
    };
    let settings = mail_settings(vm);
    let script = script_info(vm);
    let date = log_date(vm, now());
    mail::mail_ex(
        to.as_bytes(),
        subject.as_bytes(),
        message.as_bytes(),
        harg,
        params.as_ref().map(ZStr::as_bytes),
        &settings,
        &script,
        &|_| date.clone(),
        vm,
        &mut append_mail_log,
    )
    .vm(vm)
    .map(Value::Bool)
}

/// `php_mail_log_to_file()`: `php_stream_open_wrapper(filename, "a",
/// REPORT_ERRORS | STREAM_DISABLE_OPEN_BASEDIR)`, so an open failure warns
/// as `mail(): Failed to open stream: ...`; an exception thrown by the
/// error handler for it stops the mail from being sent (GH-7875).
fn append_mail_log(vm: &mut Vm, filename: &[u8], line: &[u8]) -> RtResult<()> {
    php_ext_streams::bridge::run(vm, || {
        let mut opened = None;
        if let Some(mut s) = php_streams::open::open_wrapper(
            filename,
            "a",
            php_streams::wrapper::REPORT_ERRORS | php_streams::wrapper::STREAM_DISABLE_OPEN_BASEDIR,
            &mut opened,
            None,
        ) {
            let _ = s.write(line);
            s.close();
        }
    })
    .map_err(|t| vm.runtime_error_from(t))
}

/// The CLI logger with dates from the date extension.
struct VmLog {
    date: String,
}

impl LogTarget for VmLog {
    fn sapi_log_message(&mut self, message: &[u8]) -> bool {
        mail::CliLogTarget.sapi_log_message(message)
    }

    fn format_date(&mut self, _ts: i64) -> String {
        self.date.clone()
    }
}

fn error_log(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let message = a.str(0);
    let ty = if a.has(1) { a.int(1) } else { 0 };
    let dest = a.opt(2).map(|_| a.str(2));
    let headers = a.opt(3).map(|_| a.str(3));
    let settings = ErrorLogSettings {
        error_log: vm.ini_string("error_log").filter(|v| !v.is_empty()),
        error_log_mode: vm
            .ini_string("error_log_mode")
            .and_then(|v| i64::from_str_radix(String::from_utf8_lossy(&v).trim(), 8).ok())
            .unwrap_or(0o644),
    };
    if ty == 3 && dest.as_ref().is_none_or(ZStr::is_empty) {
        // `php_stream_open_wrapper()` on an empty path.
        return Err(vm.throw("ValueError", "Path must not be empty"));
    }
    if ty == 3 {
        if let Some(path) = dest.as_ref().filter(|d| !d.as_bytes().contains(&0)) {
            return Ok(Value::Bool(error_log_to_file(
                vm,
                path.as_bytes(),
                message.as_bytes(),
            )?));
        }
    }
    let mail_settings = mail_settings(vm);
    let script = script_info(vm);
    let mut target = VmLog {
        date: log_date(vm, now()),
    };
    vm.flush_stdout();
    mail::error_log(
        message.as_bytes(),
        ty,
        dest.as_ref().map(ZStr::as_bytes),
        headers.as_ref().map(ZStr::as_bytes),
        &settings,
        &mail_settings,
        &script,
        &mut target,
        vm,
    )
    .vm(vm)
    .map(Value::Bool)
}

/// `_php_error_log_ex()` type 3: `php_stream_open_wrapper(dest, "a",
/// REPORT_ERRORS)`, so wrappers and `open_basedir` apply and failures
/// warn as `error_log(): ...`. Succeeds once the stream opened.
fn error_log_to_file(vm: &mut Vm, path: &[u8], message: &[u8]) -> VmResult<bool> {
    php_ext_streams::bridge::run(vm, || {
        let mut opened = None;
        php_streams::open::open_wrapper(
            path,
            "a",
            php_streams::wrapper::REPORT_ERRORS,
            &mut opened,
            None,
        )
        .is_some_and(|mut s| {
            let _ = s.write(message);
            s.close();
            true
        })
    })
}

fn assert_options(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    vm.raise(8192, "Function assert_options() is deprecated since 8.3")?;
    let mut st = AssertState::default();
    for name in [
        "assert.active",
        "assert.bail",
        "assert.warning",
        "assert.exception",
    ] {
        let v = vm.ini_string(name).unwrap_or_default();
        st.on_ini_update(name, &v);
    }
    // `ASSERTG(cb)` is the startup value; runtime changes go to
    // `ASSERTG(callback)` (kept by the VM for `assert()`).
    let cb = vm
        .ini
        .config_value("assert.callback")
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    st.on_ini_update("assert.callback", &cb);
    st.callback.clone_from(&vm.ext.assert_callback);
    let value = a.get(1);
    let (old, change) = st.assert_options(a.int(0), value.as_ref(), vm).vm(vm)?;
    vm.ext.assert_callback = st.callback;
    if let Some(ch) = change {
        let mut sink: Vec<php_support::Diagnostic> = Vec::new();
        if vm
            .ini
            .ini_set(ch.name, ch.value.as_bytes(), &mut sink)
            .is_none()
        {
            return Ok(Value::Bool(false));
        }
    }
    Ok(old)
}

/// The `assert.*` directives with the value whose setting is deprecated
/// (`OnUpdate{Active,Bail,Warning,Exception}Bool` in
/// `ext/standard/assert.c`), in registration order; `None` is
/// `assert.callback` (deprecated when non-empty).
const ASSERT_INI: [(&str, Option<bool>); 5] = [
    ("assert.active", Some(false)),
    ("assert.bail", Some(true)),
    ("assert.warning", Some(false)),
    ("assert.callback", None),
    ("assert.exception", Some(false)),
];

/// Whether setting `name` to `value` raises "`name` INI setting is
/// deprecated" (`php_must_emit_ini_deprecation()` stages aside).
fn assert_ini_deprecated(name: &str, value: &[u8]) -> bool {
    ASSERT_INI
        .iter()
        .find(|(n, _)| *n == name)
        .is_some_and(|(_, deprecated)| match deprecated {
            Some(b) => misc::ini_parse_bool(value) == *b,
            None => !value.is_empty(),
        })
}

/// `PHP_INI_MH` of the `assert.*` directives for `ini_set()` /
/// `ini_restore()`: the deprecation, and for `assert.callback`
/// (`OnChangeCallback`) the new `ASSERTG(callback)`.
fn assert_ini_modify(vm: &mut Vm, name: &str, value: &[u8]) -> VmResult<bool> {
    if name == "assert.callback" {
        vm.ext.assert_callback = (!value.is_empty()).then(|| Value::Str(ZStr::from(value)));
    }
    if assert_ini_deprecated(name, value) {
        vm.php_deprecated(&format!("{name} INI setting is deprecated"))?;
    }
    Ok(true)
}

/// The startup half of the `assert.*` handlers: "PHP Startup: ... INI
/// setting is deprecated" for configured (`-d`/php.ini) values.
fn assert_startup(vm: &mut Vm) {
    let headers_sent = vm.headers_sent;
    for (name, _) in ASSERT_INI {
        let Some(v) = vm.ini.config_value(name).map(<[u8]>::to_vec) else {
            continue;
        };
        if assert_ini_deprecated(name, &v) {
            let _ = vm.error_at(
                8192,
                &format!("PHP Startup: {name} INI setting is deprecated"),
                &ZStr::from("Unknown"),
                0,
            );
        }
    }
    vm.headers_sent = headers_sent;
}

/// A loaded extension as a `phpinfo()` section.
struct Module {
    name: String,
    ini: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The `PHP_MINFO` section registered with `ModuleBuilder::info`
    /// (`None`: the module only has a version).
    info: Option<Vec<InfoItem>>,
}

impl InfoModule for Module {
    fn name(&self) -> &str {
        &self.name
    }
    fn has_section(&self) -> bool {
        true
    }
    fn print_body(&self, p: &mut InfoPrinter) {
        if let Some(items) = &self.info {
            print_items(p, items, &self.ini);
        } else {
            // `php_info_print_module()` for a module without `info_func`.
            p.table_start();
            p.table_row(&[b"Version", php_vm::PHP_VERSION.as_bytes()]);
            p.table_end();
            p.ini_entries(&self.ini);
        }
    }
}

/// Renders a module's `PHP_MINFO` items (`php_info_print_table_*` calls,
/// `DISPLAY_INI_ENTRIES()` with the module's INI rows).
fn print_items(p: &mut InfoPrinter, items: &[InfoItem], ini: &[(Vec<u8>, Vec<u8>, Vec<u8>)]) {
    for item in items {
        match item {
            InfoItem::TableStart => p.table_start(),
            InfoItem::TableEnd => p.table_end(),
            InfoItem::Header(cols) => {
                let cols: Vec<&[u8]> = cols.iter().map(String::as_bytes).collect();
                p.table_header(&cols);
            }
            InfoItem::ColspanHeader(text) => p.colspan_header(2, text),
            InfoItem::Row(cols) => {
                let cols: Vec<&[u8]> = cols.iter().map(String::as_bytes).collect();
                p.table_row(&cols);
            }
            InfoItem::IniEntries => p.ini_entries(ini),
        }
    }
}

/// `sapi_module.phpinfo_as_text` and `sapi_module.pretty_name` of the
/// SAPI that registered `PHP_SAPI`: the CLI (and phpdbg/embed) print
/// text, the web SAPIs print an HTML page.
fn sapi_info_mode(vm: &Vm) -> (bool, Option<&'static str>) {
    match vm.sapi_name().as_bytes() {
        b"cgi-fcgi" => (false, Some("CGI/FastCGI")),
        b"cli-server" => (false, Some("Built-in HTTP server")),
        b"apache2handler" => (false, Some("Apache 2.0 Handler")),
        b"fpm-fcgi" => (true, Some("FPM/FastCGI")),
        b"phpdbg" => (true, Some("phpdbg")),
        b"embed" => (true, Some("PHP Embedded Library")),
        _ => (true, Some("Command Line Interface")),
    }
}

/// `display_ini_entries()` rows for an extension.
fn ini_rows(vm: &Vm, module: &str, as_text: bool) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let Ok(mut entries) = vm.ini.ini_get_all(Some(module)) else {
        return Vec::new();
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
        .into_iter()
        .map(|e| {
            (
                e.name.into_bytes(),
                info::ini_display(e.local_value.as_deref(), as_text),
                info::ini_display(e.global_value.as_deref(), as_text),
            )
        })
        .collect()
}

/// Names returned by a PHP function returning a list of strings
/// (`stream_get_wrappers()` & co.), or `default`.
fn string_list(vm: &mut Vm, func: &str, default: &[&str]) -> Vec<Vec<u8>> {
    if vm.native_function(func).is_some() {
        if let Ok(Value::Array(a)) = vm.call_value(&Value::from(func), Vec::new()) {
            return a
                .values()
                .filter_map(|v| match v {
                    Value::Str(s) => Some(s.as_bytes().to_vec()),
                    _ => None,
                })
                .collect();
        }
    }
    default.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn constant_str(vm: &Vm, name: &str, default: &str) -> String {
    match vm.constants.get(&ZStr::from(name)) {
        Some(Value::Str(s)) => s.to_string_lossy(),
        _ => default.to_string(),
    }
}

/// Fallback for `stream_get_wrappers()` when the streams extension is absent.
const DEFAULT_WRAPPERS: &[&str] = &[
    "https",
    "ftps",
    "compress.zlib",
    "php",
    "file",
    "glob",
    "data",
    "http",
    "ftp",
    "phar",
];

/// Fallback for `stream_get_transports()` when the streams extension is absent.
const DEFAULT_TRANSPORTS: &[&str] = &[
    "tcp", "udp", "unix", "udg", "ssl", "tls", "tlsv1.0", "tlsv1.1", "tlsv1.2", "tlsv1.3",
];

/// Fallback for `stream_get_filters()` when the streams extension is absent.
const DEFAULT_FILTERS: &[&str] = &[
    "zlib.*",
    "string.rot13",
    "string.toupper",
    "string.tolower",
    "convert.*",
    "consumed",
    "dechunk",
];

/// The general rows of `phpinfo()` from "PHP API" to "`DTrace` Support".
const ENGINE_ROWS: &[(&str, &str)] = &[
    ("PHP API", "20240925"),
    ("PHP Extension", "20240925"),
    ("Zend Extension", "420240925"),
    ("Zend Extension Build", "API420240925,NTS"),
    ("PHP Extension Build", "API20240925,NTS"),
    ("PHP Integer Size", "64 bits"),
    ("Debug Build", "no"),
    ("Thread Safety", "disabled"),
    ("Zend Signal Handling", "enabled"),
    ("Zend Memory Manager", "enabled"),
    ("Zend Multibyte Support", "disabled"),
    ("Zend Max Execution Timers", "disabled"),
    ("IPv6 Support", "enabled"),
    ("DTrace Support", "disabled"),
];

/// The `$_SERVER` `PHP_SELF`/`PHP_AUTH_*` strings and the request arrays
/// shown in the "PHP Variables" section.
#[allow(clippy::type_complexity)]
fn request_vars(vm: &Vm) -> (Vec<(&'static str, Vec<u8>)>, Vec<(&'static str, ZArray)>) {
    let server = match global(vm, b"_SERVER") {
        Some(Value::Array(s)) => s,
        _ => ZArray::new(),
    };
    let mut php_vars = Vec::new();
    for k in ["PHP_SELF", "PHP_AUTH_USER", "PHP_AUTH_PW", "PHP_AUTH_TYPE"] {
        if let Some(Value::Str(s)) = server.get_str(k.as_bytes()).map(Value::deref) {
            php_vars.push((k, s.as_bytes().to_vec()));
        }
    }
    let mut gpcse = Vec::new();
    for k in [
        "_REQUEST", "_GET", "_POST", "_COOKIE", "_FILES", "_SERVER", "_ENV",
    ] {
        if let Some(Value::Array(arr)) = global(vm, k.as_bytes()) {
            if !arr.is_empty() || k == "_SERVER" {
                gpcse.push((k, arr));
            }
        }
    }
    (php_vars, gpcse)
}

fn phpinfo(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let flag = if a.has(0) { a.int(0) } else { info::INFO_ALL };
    if flag & info::INFO_VARIABLES != 0 {
        // php_print_gpcse_array requests each array (zend_is_auto_global),
        // creating a still-armed JIT `$_REQUEST`/`$_SERVER`/`$_ENV`.
        for name in [b"_REQUEST".as_slice(), b"_SERVER", b"_ENV"] {
            vm.is_auto_global(name);
        }
    }
    let version = constant_str(vm, "PHP_VERSION", "8.5.0-dev");
    let (as_text, server_api) = sapi_info_mode(vm);
    let uname = system::php_uname(b"a").vm(vm)?;
    let wrappers = string_list(vm, "stream_get_wrappers", DEFAULT_WRAPPERS);
    let transports = string_list(vm, "stream_get_transports", DEFAULT_TRANSPORTS);
    let filters = string_list(vm, "stream_get_filters", DEFAULT_FILTERS);
    let core_ini = ini_rows(vm, "Core", as_text);
    let mut names: Vec<String> = php_vm::LOADED_EXTENSIONS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for e in vm
        .loaded_extensions
        .iter()
        .cloned()
        .chain(vm.modules_with_info())
    {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&e)) {
            names.push(e);
        }
    }
    let modules: Vec<Module> = names
        .into_iter()
        .map(|name| {
            let ini = ini_rows(vm, &name, as_text);
            let info = vm.module_info(&name);
            Module { name, ini, info }
        })
        .collect();
    let module_refs: Vec<&dyn InfoModule> = modules.iter().map(|m| m as &dyn InfoModule).collect();
    let (php_vars, gpcse) = request_vars(vm);
    let zend_version = format!(
        "Zend Engine v{}, Copyright (c) Zend Technologies\n",
        constant_str(vm, "ZEND_VERSION", "4.5.0-dev")
    );
    let src = InfoSource {
        php_version: &version,
        uname: uname.as_bytes(),
        build_date: "Jan  1 2026 00:00:00",
        build_rows: &[("Configure Command", " './configure' ")],
        server_api,
        config_file_path: "/usr/local/lib",
        loaded_ini: None,
        scan_dir: None,
        scanned_files: None,
        engine_rows: ENGINE_ROWS,
        stream_wrappers: &wrappers,
        stream_transports: &transports,
        stream_filters: &filters,
        zend_version: &zend_version,
        core_ini: &core_ini,
        modules: &module_refs,
        php_vars: &php_vars,
        gpcse: &gpcse,
    };
    // `php-stdmisc` does not render the credits section: print everything
    // up to the variables, then the credits, then the license.
    let mut out = info::print_info(
        flag & !(info::INFO_CREDITS | info::INFO_LICENSE),
        &src,
        as_text,
        vm,
    )
    .vm(vm)?;
    // Without the credits and license, print_info closed the HTML page.
    if !as_text {
        let tail = PAGE_END.len();
        if out.ends_with(PAGE_END) {
            out.truncate(out.len() - tail);
        }
    }
    if flag & info::INFO_CREDITS != 0 {
        let mut p = InfoPrinter::new(as_text);
        p.hr();
        out.extend_from_slice(&p.out);
        out.extend_from_slice(&credits_text(
            CREDITS_ALL & !CREDITS_FULLPAGE,
            as_text,
            &version,
        ));
    }
    if flag & info::INFO_LICENSE != 0 {
        let lic = info::print_info(info::INFO_LICENSE, &src, as_text, vm).vm(vm)?;
        // Keep only the license section of that second rendering.
        let start = if as_text {
            b"phpinfo()\n".len()
        } else {
            info::html_head(&version, "phpinfo()").len()
        };
        let lic = lic.get(start..).unwrap_or(&lic);
        let lic = if as_text {
            lic
        } else {
            lic.strip_suffix(PAGE_END).unwrap_or(lic)
        };
        out.extend_from_slice(lic);
    }
    if !as_text {
        out.extend_from_slice(PAGE_END);
    }
    vm.output(&out);
    Ok(Value::Bool(true))
}

/// The end of the HTML `phpinfo()` page.
const PAGE_END: &[u8] = b"</div></body></html>";

/// `PHP_CREDITS_ALL`.
const CREDITS_ALL: i64 = 0xFFFF_FFFF;
/// `PHP_CREDITS_FULLPAGE`.
const CREDITS_FULLPAGE: i64 = 32;

/// `php_print_credits()` in text mode or (`as_text` false) as HTML.
fn credits_text(flag: i64, as_text: bool, php_version: &str) -> Vec<u8> {
    let mut p = InfoPrinter::new(as_text);
    if !as_text && flag & CREDITS_FULLPAGE != 0 {
        p.print(info::html_head(php_version, "phpcredits()"));
    }
    p.print(if as_text {
        "PHP Credits\n"
    } else {
        "<h1>PHP Credits</h1>\n"
    });
    if flag & 1 != 0 {
        p.table_start();
        p.colspan_header(1, "PHP Group");
        p.table_row(&[b"Thies C. Arntzen, Stig Bakken, Shane Caraveo, Andi Gutmans, Rasmus Lerdorf, Sam Ruby, Sascha Schumann, Zeev Suraski, Jim Winstead, Andrei Zmievski"]);
        p.table_end();
    }
    if flag & 2 != 0 {
        p.table_start();
        p.colspan_header(
            1,
            if as_text {
                "Language Design & Concept"
            } else {
                "Language Design &amp; Concept"
            },
        );
        p.table_row(&[b"Andi Gutmans, Rasmus Lerdorf, Zeev Suraski, Marcus Boerger"]);
        p.table_end();
        p.table_start();
        p.colspan_header(2, "PHP Authors");
        p.table_header(&[b"Contribution", b"Authors"]);
        for (k, v) in credits::GENERAL {
            p.table_row(&[k.as_bytes(), v.as_bytes()]);
        }
        p.table_end();
    }
    for (bit, title, header, rows) in [
        (4, "SAPI Modules", "Contribution", credits::SAPI),
        (8, "Module Authors", "Module", credits::EXT),
    ] {
        if flag & bit != 0 {
            p.table_start();
            p.colspan_header(2, title);
            p.table_header(&[header.as_bytes(), b"Authors"]);
            for (k, v) in rows {
                p.table_row(&[k.as_bytes(), v.as_bytes()]);
            }
            p.table_end();
        }
    }
    if flag & 16 != 0 {
        p.table_start();
        p.colspan_header(2, "PHP Documentation");
        for (k, v) in credits::DOCS {
            p.table_row(&[k.as_bytes(), v.as_bytes()]);
        }
        p.table_end();
    }
    if flag & 64 != 0 {
        p.table_start();
        p.colspan_header(1, "PHP Quality Assurance Team");
        p.table_row(&[credits::QA.as_bytes()]);
        p.table_end();
    }
    if flag & 128 != 0 {
        p.table_start();
        p.colspan_header(2, "Websites and Infrastructure team");
        for (k, v) in credits::WEB {
            p.table_row(&[k.as_bytes(), v.as_bytes()]);
        }
        p.table_end();
    }
    if !as_text && flag & CREDITS_FULLPAGE != 0 {
        p.print("</div></body></html>\n");
    }
    p.out
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn phpcredits(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let flag = if a.has(0) { a.int(0) } else { CREDITS_ALL };
    let (as_text, _) = sapi_info_mode(vm);
    let version = constant_str(vm, "PHP_VERSION", "8.5.0-dev");
    vm.output(&credits_text(flag, as_text, &version));
    Ok(Value::Bool(true))
}

/// `MAXPATHLEN`.
const MAXPATHLEN: usize = 4096;

/// `strptime(string $timestamp, string $format): array|false`
/// (ext/standard/datetime.c, deprecated since 8.2).
#[allow(clippy::unnecessary_wraps)]
fn strptime(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let timestamp = a.str(0);
    let format = a.str(1);
    let Some(parsed) = php_stdmisc::datetime::strptime(timestamp.as_bytes(), format.as_bytes())
    else {
        return Ok(Value::Bool(false));
    };
    let mut t = php_runtime::HashTable::new();
    for (name, v) in parsed.fields {
        t.insert_str_raw(ZStr::from(name), Value::Int(v));
    }
    t.insert_str_raw(
        ZStr::from("unparsed"),
        Value::Str(ZStr::from(parsed.unparsed)),
    );
    Ok(Value::Array(ZArray::from_table(t)))
}

/// `dl($extension_filename)`: phprust links its extensions statically, so
/// after php-src's checks every load fails like a missing shared object.
fn dl(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = a.str(0);
    if name.as_bytes().contains(&0) {
        return Err(a.value_error(vm, 1, "must not contain any null bytes"));
    }
    if !vm.ini_bool("enable_dl") {
        vm.php_warning("Dynamically loaded extensions aren't enabled")?;
        return Ok(Value::Bool(false));
    }
    if name.len() >= MAXPATHLEN {
        vm.php_warning(&format!(
            "Filename exceeds the maximum allowed length of {MAXPATHLEN} characters"
        ))?;
        return Ok(Value::Bool(false));
    }
    if name.as_bytes().contains(&b'/') {
        vm.php_warning("Temporary module name should contain only filename")?;
        return Ok(Value::Bool(false));
    }
    let dir = vm.ini_string("extension_dir").unwrap_or_default();
    if dir.is_empty() {
        return Ok(Value::Bool(false));
    }
    let dir = String::from_utf8_lossy(&dir).into_owned();
    let sep = if dir.ends_with('/') { "" } else { "/" };
    let file = name.to_string_lossy();
    let p1 = format!("{dir}{sep}{file}");
    let p2 = format!("{dir}{sep}{file}.so");
    let err = |p: &str| format!("{p}: cannot open shared object file: No such file or directory");
    vm.php_warning(&format!(
        "Unable to load dynamic library '{file}' (tried: {p1} ({}), {p2} ({}))",
        err(&p1),
        err(&p2)
    ))?;
    Ok(Value::Bool(false))
}

/// Registers the functions and constants.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, v) in info::INFO_CONSTANTS {
        m.constant(name, Value::Int(*v));
    }
    for (name, v) in misc::ASSERT_CONSTANTS {
        m.constant_deprecated(
            name,
            Value::Int(*v),
            "8.3",
            "as assert_options() is deprecated",
        );
    }
    m.ini_on_modify("assert.active", |vm, v| {
        assert_ini_modify(vm, "assert.active", v)
    })
    .ini_on_modify("assert.bail", |vm, v| {
        assert_ini_modify(vm, "assert.bail", v)
    })
    .ini_on_modify("assert.warning", |vm, v| {
        assert_ini_modify(vm, "assert.warning", v)
    })
    .ini_on_modify("assert.callback", |vm, v| {
        assert_ini_modify(vm, "assert.callback", v)
    })
    .ini_on_modify("assert.exception", |vm, v| {
        assert_ini_modify(vm, "assert.exception", v)
    })
    .on_request_startup(assert_startup)
    // `PHP_RSHUTDOWN_FUNCTION(assert)`.
    .on_request_shutdown(|vm| vm.ext.assert_callback = None);
    m.function_static(
        "mail(string $to, string $subject, string $message, array|string $additional_headers = [], string $additional_params = \"\"): bool",
        mail_fn,
    )
    .function_static(
        "error_log(string $message, int $message_type = 0, ?string $destination = null, ?string $additional_headers = null): bool",
        error_log,
    )
    .function_static("assert_options(int $option, mixed $value = UNKNOWN): mixed", assert_options)
    .function_static("phpinfo(int $flags = INFO_ALL): true", phpinfo)
    .function_static("phpcredits(int $flags = CREDITS_ALL): true", phpcredits)
    .function_static("dl(string $extension_filename): bool", dl)
    .function_static(
        "strptime(string $timestamp, string $format): array|false",
        strptime,
    )
    .deprecated(
        "8.2",
        "use date_parse_from_format() (for locale-independent parsing), or IntlDateFormatter::parse() (for locale-dependent parsing) instead",
    );
}
