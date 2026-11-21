// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! CLI startup preloading, separate from request execution and auto-prepend.

use crate::config::Config;
use crate::startup::Reporter;
use php_ext_opcache::AccelLog;
use php_vm::{PersistentDeclarations, Throw, Vm};
use std::cell::RefCell;
use std::ffi::CString;
use std::rc::Rc;

thread_local! {
    static DECLARATIONS: RefCell<Option<Rc<PersistentDeclarations>>> = const { RefCell::new(None) };
}

fn enabled(cfg: &Config) -> bool {
    let boolean = |name: &str, default: bool| {
        cfg.get(name)
            .map_or(default, |v| php_support::ini::parse_bool(v.as_bytes()))
    };
    // Runs for the CLI SAPI only: the built-in server does not preload yet.
    php_vm::include::opcache_enabled_for(
        b"cli",
        boolean("opcache.enable", true),
        boolean("opcache.enable_cli", false),
    )
}

/// Run exactly once during CLI module startup, before selecting a request mode.
pub(crate) fn startup(cfg: &Config) -> Result<(), i32> {
    DECLARATIONS.with(|d| d.borrow_mut().take());
    let Some(path) = cfg.get("opcache.preload").filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    if !enabled(cfg) {
        return Ok(());
    }
    let reporter = Reporter::new(cfg);
    // `zend_accel_error()`: opcache's own log, not a PHP error.
    let verbosity = cfg
        .get("opcache.log_verbosity_level")
        .map_or(1, |v| php_support::ini::parse_quantity(v.as_bytes()).0);
    let error_log = cfg.get("opcache.error_log").unwrap_or_default().to_string();
    let accel_log = |kind, message: &str| {
        php_ext_opcache::accel_log(verbosity, &error_log, kind, message);
    };
    // `ACCEL_LOG_FATAL` exits with -2.
    let accel_fatal = |message: &str| {
        accel_log(AccelLog::Fatal, message);
        Err(254)
    };
    if cfg
        .get("opcache.file_cache_only")
        .is_some_and(|v| php_support::ini::parse_bool(v.as_bytes()))
    {
        accel_log(
            AccelLog::Warning,
            "Preloading doesn't work in \"file_cache_only\" mode",
        );
        return Ok(());
    }
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if let Some(user) = cfg.get("opcache.preload_user").filter(|u| !u.is_empty()) {
        if uid != 0 {
            accel_log(
                AccelLog::Warning,
                "\"opcache.preload_user\" is ignored because the current user is not \"root\"",
            );
        } else {
            let Ok(name) = CString::new(user) else {
                return accel_fatal(&format!("Preloading failed to getpwnam(\"{user}\")"));
            };
            // SAFETY: name is NUL-terminated; copy the numeric uid before
            // another libc lookup can overwrite the static passwd entry.
            let target = unsafe {
                let entry = libc::getpwnam(name.as_ptr());
                (!entry.is_null()).then(|| (*entry).pw_uid)
            };
            let Some(target) = target else {
                return accel_fatal(&format!("Preloading failed to getpwnam(\"{user}\")"));
            };
            if target != uid {
                // A cross-user preload requires a child-to-parent declaration
                // transport. Never keep saved root credentials in PHP code.
                crate::err(
                    b"Cross-user preloading requires process-shared declaration persistence\n",
                );
                return Err(1);
            }
        }
    }
    let source = match std::fs::read(path) {
        Ok(source) => source,
        Err(error) => {
            let reason = match error.kind() {
                std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
                _ => error.to_string(),
            };
            reporter.report(2, &format!("PHP Startup: Failed to open stream: {reason}"));
            reporter.report(
                1,
                &format!(
                    "Failed opening required '{path}' (include_path='{}')",
                    cfg.get("include_path").unwrap_or(".:/usr/share/php")
                ),
            );
            return Err(1);
        }
    };
    let mut startup_config = cfg.clone();
    startup_config
        .entries
        .push(("open_basedir".to_string(), String::new()));
    let mut vm = crate::new_vm(&startup_config);
    crate::request_startup(&mut vm);
    let path = php_runtime::ZStr::from(crate::real_path(path.as_bytes()));
    let result = vm.run_preload(&source, &path);
    let persistent = vm.finish_preload(result)?;
    DECLARATIONS.with(|d| *d.borrow_mut() = Some(Rc::new(persistent)));
    Ok(())
}

/// Install a fresh declaration graph in the request's otherwise fresh VM.
pub(crate) fn install(vm: &mut Vm) -> Result<(), Throw> {
    let persistent = DECLARATIONS.with(|d| d.borrow().clone());
    if let Some(persistent) = persistent {
        vm.install_persistent_declarations(&persistent)?;
    }
    Ok(())
}
