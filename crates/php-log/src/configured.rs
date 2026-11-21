// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`ConfiguredLog`]: the error log as configured, for logging where no VM
//! is at hand or may be touched.

use std::fmt;
use std::rc::Rc;

use php_sapi::SapiModule;

use crate::{php_log_err_with_severity, LogHost, SyslogSettings};

/// A [`LogHost`] without a VM: the `error_log`, `error_log_mode` and
/// `syslog.*` settings of the configuration (`php.ini`, `-d`), and the
/// SAPI's logger. For what is logged before the modules are activated
/// (startup diagnostics) and after a request could not end normally (a
/// panic: its VM is in an unknown state). Dates are those of
/// [`LogHost::log_date`]: the thread's default timezone.
#[derive(Clone)]
pub struct ConfiguredLog {
    error_log: Option<Vec<u8>>,
    error_log_mode: i64,
    syslog: SyslogSettings,
    sapi: Rc<dyn SapiModule>,
}

impl fmt::Debug for ConfiguredLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredLog")
            .field("error_log", &self.error_log)
            .field("error_log_mode", &self.error_log_mode)
            .field("syslog", &self.syslog)
            .finish_non_exhaustive()
    }
}

impl ConfiguredLog {
    /// The settings `get` returns for the directive names, logging to
    /// `sapi` where PHP logs to the SAPI.
    pub fn from_ini<'a>(get: impl Fn(&str) -> Option<&'a [u8]>, sapi: Rc<dyn SapiModule>) -> Self {
        ConfiguredLog {
            error_log: get("error_log").map(<[u8]>::to_vec),
            error_log_mode: get("error_log_mode")
                .map_or(0o644, |v| php_support::ini::parse_quantity(v).0),
            syslog: SyslogSettings::from_ini(
                get("syslog.ident"),
                get("syslog.facility"),
                get("syslog.filter"),
            ),
            sapi,
        }
    }

    /// `php_log_err_with_severity()` to the configured destination.
    pub fn log(&mut self, message: &[u8], severity: i32) {
        let error_log = self.error_log.clone();
        php_log_err_with_severity(message, severity, error_log.as_deref(), self);
    }
}

impl LogHost for ConfiguredLog {
    fn error_log_mode(&mut self) -> i64 {
        self.error_log_mode
    }

    fn syslog_settings(&mut self) -> SyslogSettings {
        self.syslog.clone()
    }

    fn sapi_log_message(&mut self, message: &[u8], severity: i32) {
        self.sapi.log_message(message, severity);
    }
}

#[cfg(test)]
mod tests {
    use super::ConfiguredLog;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Capture(RefCell<Vec<(Vec<u8>, i32)>>);

    impl php_sapi::SapiModule for Capture {
        fn log_message(&self, message: &[u8], severity: i32) {
            self.0.borrow_mut().push((message.to_vec(), severity));
        }
    }

    #[test]
    fn without_error_log_the_sapi_logs() {
        let sapi = Rc::new(Capture::default());
        let mut log = ConfiguredLog::from_ini(|_| None, sapi.clone());
        log.log(b"PHP Fatal error:  broken", 2);
        assert_eq!(
            sapi.0.borrow().as_slice(),
            &[(b"PHP Fatal error:  broken".to_vec(), 2)]
        );
    }

    #[test]
    fn error_log_file_gets_the_dated_line() {
        use std::os::unix::ffi::OsStrExt;
        let (file, path) =
            php_streams::temp::create_temporary(None, Some(b"configured-log-"), 0, &mut || {})
                .expect("temporary file");
        drop(file);
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path));
        let name = path.as_os_str().as_bytes().to_vec();
        let sapi = Rc::new(Capture::default());
        let mut log = ConfiguredLog::from_ini(
            |k| (k == "error_log").then_some(name.as_slice()),
            sapi.clone(),
        );
        log.log(b"PHP Fatal error:  broken", 2);
        let written = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(written.starts_with('['), "{written}");
        assert!(
            written.ends_with("] PHP Fatal error:  broken\n"),
            "{written}"
        );
        assert!(sapi.0.borrow().is_empty());
    }
}
