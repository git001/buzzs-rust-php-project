// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Module startup (`php_module_startup()`): the diagnostics PHP raises
//! before any request — INI values rejected or adjusted by their
//! handlers, deprecated or removed directives, and `extension=` /
//! `zend_extension=` entries — displayed the way `php_error_cb()` does
//! while the module is not yet initialised.

use crate::config::Config;
use php_support::diag::{Diagnostic, ErrorLevel};
use php_support::ini::{parse_bool, IniRegistry};
use std::io::Write;

/// Where displayed errors go (`php_get_display_errors_mode()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    Off,
    Stdout,
    Stderr,
}

/// The error-display settings taken from the configuration hash.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Reporter {
    display: Display,
    display_startup: bool,
    html: bool,
    log: bool,
    log_host: StartupLog,
    prepend: String,
    append: String,
}

/// Where the error log goes during startup: the `error_log`, `error_log_mode`
/// and `syslog.*` settings of the configuration, and the CLI/CGI logger
/// (stderr). Dates are in UTC, as before the modules are activated.
#[derive(Debug, Clone)]
struct StartupLog {
    error_log: Option<Vec<u8>>,
    error_log_mode: i64,
    syslog: php_log::SyslogSettings,
}

impl php_log::LogHost for StartupLog {
    fn error_log_mode(&mut self) -> i64 {
        self.error_log_mode
    }

    fn syslog_settings(&mut self) -> php_log::SyslogSettings {
        self.syslog.clone()
    }

    fn sapi_log_message(&mut self, message: &[u8], severity: i32) {
        php_sapi::SapiModule::log_message(&php_sapi::StderrLog, message, severity);
    }
}

fn display_mode(v: &str) -> Display {
    let l = v.trim().to_ascii_lowercase();
    match l.as_str() {
        "stderr" => Display::Stderr,
        "stdout" => Display::Stdout,
        _ if parse_bool(l.as_bytes()) => Display::Stdout,
        _ => Display::Off,
    }
}

/// `error_type_str` of `php_error_cb()`.
#[must_use]
pub fn level_name(level: i32) -> &'static str {
    match level {
        1 | 16 | 64 | 256 => "Fatal error",
        4096 => "Recoverable fatal error",
        2 | 32 | 128 | 512 => "Warning",
        4 => "Parse error",
        8 | 1024 => "Notice",
        8192 | 16384 => "Deprecated",
        _ => "Unknown error",
    }
}

impl Reporter {
    /// The settings in effect at module startup.
    #[must_use]
    pub fn new(cfg: &Config) -> Self {
        let b =
            |name: &str, default: bool| cfg.get(name).map_or(default, |v| parse_bool(v.as_bytes()));
        Reporter {
            display: display_mode(cfg.get("display_errors").unwrap_or("1")),
            display_startup: b("display_startup_errors", true),
            html: b("html_errors", false),
            log: b("log_errors", false),
            log_host: StartupLog {
                error_log: cfg.get("error_log").map(|v| v.as_bytes().to_vec()),
                error_log_mode: cfg
                    .get("error_log_mode")
                    .map_or(0o644, |v| php_support::ini::parse_quantity(v.as_bytes()).0),
                syslog: php_log::SyslogSettings::from_ini(
                    cfg.get("syslog.ident").map(str::as_bytes),
                    cfg.get("syslog.facility").map(str::as_bytes),
                    cfg.get("syslog.filter").map(str::as_bytes),
                ),
            },
            prepend: cfg
                .get("error_prepend_string")
                .unwrap_or_default()
                .to_string(),
            append: cfg
                .get("error_append_string")
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// Reports `message` raised at `level` during startup (`Unknown` on
    /// line 0).
    pub fn report(&self, level: i32, message: &str) {
        let kind = level_name(level);
        let shown = self.display != Display::Off && self.display_startup;
        if self.log || !shown {
            let line = format!("PHP {kind}:  {message} in Unknown on line 0");
            let error_log = self.log_host.error_log.clone();
            php_log::php_log_err_with_severity(
                line.as_bytes(),
                php_log::severity_of_error_type(i64::from(level)),
                error_log.as_deref(),
                &mut self.log_host.clone(),
            );
        }
        if !shown {
            return;
        }
        let mut out = std::io::stdout();
        if self.html {
            let _ = write!(
                out,
                "{}<br />\n<b>{kind}</b>:  {message} in <b>Unknown</b> on line <b>0</b><br />\n{}",
                self.prepend, self.append
            );
        } else if self.display == Display::Stderr {
            let _ = writeln!(std::io::stderr(), "{kind}: {message} in Unknown on line 0");
        } else {
            let _ = write!(
                out,
                "{}\n{kind}: {message} in Unknown on line 0\n{}",
                self.prepend, self.append
            );
        }
        let _ = out.flush();
    }

    fn diagnostic(&self, d: &Diagnostic) {
        let msg = d.message.trim_end();
        if d.docref {
            self.report(d.level.0, &format!("PHP Startup: {msg}"));
        } else {
            self.report(d.level.0, msg);
        }
    }
}

/// Directives that are deprecated, or no longer available (a fatal
/// startup error), when set to a true value.
const DEPRECATED: &[&str] = &["allow_url_include"];
const REMOVED: &[&str] = &[
    "allow_call_time_pass_reference",
    "asp_tags",
    "define_syslog_variables",
    "highlight.bg",
    "magic_quotes_gpc",
    "magic_quotes_runtime",
    "magic_quotes_sybase",
    "register_globals",
    "register_long_arrays",
    "safe_mode",
    "safe_mode_gid",
    "safe_mode_include_dir",
    "safe_mode_exec_dir",
    "safe_mode_allowed_env_vars",
    "safe_mode_protected_env_vars",
    "zend.ze1_compatibility_mode",
    "track_errors",
    "disable_classes",
];

/// `cfg_get_long()`: the leading integer of a configuration value.
fn cfg_long(cfg: &Config, name: &str) -> i64 {
    let Some(v) = cfg.get(name) else {
        return 0;
    };
    let v = v.trim_start();
    let end = v
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(v.len(), |(i, _)| i);
    v[..end].parse().unwrap_or(0)
}

/// The module names compiled into this binary.
#[allow(dead_code)]
fn builtin_modules() -> Vec<String> {
    let mut v: Vec<String> = php_vm::LOADED_EXTENSIONS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    crate::extensions::with_all_extensions(|exts| {
        v.extend(exts.iter().map(|e| e.name().to_string()));
    });
    v
}

/// Checks whether an extension or zend extension is compiled in,
/// avoiding intermediate string allocations during startup.
fn is_builtin_fast(value: &str, zend: bool) -> bool {
    let base = value.rsplit('/').next().unwrap_or(value);
    let base = base
        .strip_suffix(".so")
        .or_else(|| base.strip_suffix(".dll"))
        .unwrap_or(base);
    let base = base.strip_prefix("php_").unwrap_or(base);
    if zend {
        return base.eq_ignore_ascii_case("opcache");
    }
    if php_vm::LOADED_EXTENSIONS
        .iter()
        .any(|&m| m.eq_ignore_ascii_case(base))
    {
        return true;
    }
    crate::extensions::with_all_extensions(|exts| {
        exts.iter().any(|e| e.name().eq_ignore_ascii_case(base))
    })
}

/// Whether an `extension=`/`zend_extension=` value names something
/// compiled in (`json`, `php_json.dll`, `/path/json.so`, `opcache`).
#[allow(dead_code)]
fn is_builtin(modules: &[String], value: &str, zend: bool) -> bool {
    let base = value.rsplit('/').next().unwrap_or(value);
    let base = base
        .strip_suffix(".so")
        .or_else(|| base.strip_suffix(".dll"))
        .unwrap_or(base);
    let base = base.strip_prefix("php_").unwrap_or(base);
    if zend {
        return base.eq_ignore_ascii_case("opcache");
    }
    modules.iter().any(|e| e.eq_ignore_ascii_case(base))
}

fn dl_error(path: &str) -> String {
    format!("{path}: cannot open shared object file: No such file or directory")
}

/// `php_ini_register_extensions()`: everything is compiled in, so a
/// value naming a built-in module is accepted; a shared object that
/// exists (built for another PHP) is skipped; anything else fails like a
/// missing shared object.
fn load_extensions(r: &Reporter, cfg: &Config) {
    if cfg.zend_extensions.is_empty() && cfg.extensions.is_empty() {
        return;
    }
    let dir = cfg
        .get("extension_dir")
        .unwrap_or(crate::config::EXTENSION_DIR)
        .to_string();
    let join = |name: &str| {
        if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        }
    };
    for z in &cfg.zend_extensions {
        if is_builtin_fast(z, true) {
            continue;
        }
        if z.starts_with('/') {
            if !std::path::Path::new(z).is_file() {
                let _ = writeln!(std::io::stderr(), "Failed loading {z}:  {}", dl_error(z));
            }
            continue;
        }
        let first = join(z);
        let second = join(&format!("{z}.so"));
        if std::path::Path::new(&first).is_file() || std::path::Path::new(&second).is_file() {
            continue;
        }
        r.report(
            32,
            &format!(
                "Failed loading Zend extension '{z}' (tried: {first} ({}), {second} ({}))",
                dl_error(&first),
                dl_error(&second)
            ),
        );
    }
    for e in &cfg.extensions {
        if is_builtin_fast(e, false) {
            continue;
        }
        let first = if e.contains('/') { e.clone() } else { join(e) };
        if std::path::Path::new(&first).is_file() {
            continue;
        }
        let second = join(&format!("{e}.so"));
        r.report(
            32,
            &format!(
                "PHP Startup: Unable to load dynamic library '{e}' (tried: {first} ({}), {second} ({}))",
                dl_error(&first),
                dl_error(&second)
            ),
        );
    }
}

/// Reports what the extensions raised while registering their INI
/// directives (`MINIT`). The CLI builds a VM, and with it the extensions,
/// for every request, but PHP starts its modules once: only the first
/// VM's diagnostics are reported.
pub fn report_module_diagnostics(cfg: &Config, diagnostics: &[Diagnostic]) {
    thread_local! {
        static REPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if REPORTED.with(|r| r.replace(true)) {
        return;
    }
    let r = Reporter::new(cfg);
    for d in diagnostics {
        r.diagnostic(d);
    }
}

/// Runs the startup checks; `Err(status)` when startup fails.
pub fn module_startup(cfg: &Config) -> Result<(), i32> {
    let r = Reporter::new(cfg);
    let mut sink: Vec<Diagnostic> = Vec::new();
    let _ = IniRegistry::with_core(
        cfg.entries
            .iter()
            .map(|(k, v)| (k.clone(), v.as_bytes().to_vec())),
        &mut sink,
    );
    for d in &sink {
        r.diagnostic(d);
    }
    if cfg
        .get("report_memleaks")
        .is_some_and(|v| !parse_bool(v.as_bytes()))
        && !sink.iter().any(|d| d.message.contains("report_memleaks"))
    {
        r.report(
            ErrorLevel::DEPRECATED.0,
            "PHP Startup: Directive 'report_memleaks' is deprecated",
        );
    }
    load_extensions(&r, cfg);
    // `zend_disable_functions()`: these cannot be disabled.
    if let Some(list) = cfg.get("disable_functions") {
        for name in list
            .split([',', ' '])
            .filter(|n| matches!(*n, "exit" | "die" | "clone"))
        {
            r.report(2, &format!("Cannot disable function {name}()"));
        }
    }
    for d in DEPRECATED {
        if cfg_long(cfg, d) != 0 {
            r.report(8192, &format!("Directive '{d}' is deprecated"));
        }
    }
    for d in REMOVED {
        if cfg_long(cfg, d) != 0 {
            r.report(
                16,
                &format!("Directive '{d}' is no longer available in PHP"),
            );
            return Err(254);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_modes() {
        assert_eq!(display_mode("stderr"), Display::Stderr);
        assert_eq!(display_mode("On"), Display::Stdout);
        assert_eq!(display_mode("1"), Display::Stdout);
        assert_eq!(display_mode("0"), Display::Off);
        assert_eq!(display_mode(""), Display::Off);
    }

    #[test]
    fn builtin_names() {
        let m = vec!["json".to_string(), "Core".to_string()];
        assert!(is_builtin(&m, "json", false));
        assert!(is_builtin(&m, "php_json.dll", false));
        assert!(is_builtin(&m, "/usr/lib/php/json.so", false));
        assert!(!is_builtin(&m, "kartoffel", false));
        assert!(is_builtin(&m, "opcache.so", true));
        assert!(!is_builtin(&m, "unknown_ext", true));
    }

    #[test]
    fn cfg_long_prefix() {
        let cfg = Config {
            entries: vec![("a".into(), " 12abc".into()), ("b".into(), "On".into())],
            ..Config::default()
        };
        assert_eq!(cfg_long(&cfg, "a"), 12);
        assert_eq!(cfg_long(&cfg, "b"), 0);
        assert_eq!(cfg_long(&cfg, "c"), 0);
    }

    #[test]
    fn level_names() {
        assert_eq!(level_name(32), "Warning");
        assert_eq!(level_name(8192), "Deprecated");
        assert_eq!(level_name(16), "Fatal error");
    }
}
