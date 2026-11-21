// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The engine's side of PHP's error log (`main/main.c`): the settings in
//! effect, the log date in the request's timezone and the SAPI logger.
//! Where a message goes is decided in one place,
//! [`php_log::php_log_err_with_severity`].

use php_runtime::{Value, ZStr};

use crate::func::Func;
use crate::Vm;

impl Vm {
    /// The `syslog.*` settings in effect.
    #[must_use]
    pub fn syslog_settings(&self) -> php_log::SyslogSettings {
        php_log::SyslogSettings::from_ini(
            self.ini.get("syslog.ident"),
            self.ini.get("syslog.facility"),
            self.ini.get("syslog.filter"),
        )
    }

    /// The `error_log` setting (`None` = unset).
    #[must_use]
    pub fn error_log_setting(&self) -> Option<Vec<u8>> {
        self.ini.get("error_log").map(<[u8]>::to_vec)
    }

    /// `php_format_date("d-M-Y H:i:s e", ts, 1)`: the date extension's
    /// `date()` in the default timezone, or UTC without it.
    #[cold]
    pub fn log_date(&mut self, ts: i64) -> String {
        let date = ZStr::from("date");
        if matches!(self.functions.get(&date), Some(Func::Builtin(_))) {
            if let Ok(Value::Str(s)) = self.call_value(
                &Value::Str(date),
                vec![Value::from("d-M-Y H:i:s e"), Value::Int(ts)],
            ) {
                return s.to_string_lossy();
            }
        }
        php_log::date::format_log_date_utc(ts)
    }

    /// Installs the running SAPI's callbacks (`sapi_module`).
    pub fn set_sapi_module(&mut self, module: std::rc::Rc<dyn php_sapi::SapiModule>) {
        self.sapi_module = module;
    }

    /// `php_log_err_with_severity(message, severity)`.
    #[cold]
    pub fn log_err_with_severity(&mut self, message: &[u8], severity: i32) {
        let error_log = self.error_log_setting();
        php_log::php_log_err_with_severity(message, severity, error_log.as_deref(), self);
    }
}

impl php_log::LogHost for Vm {
    fn log_date(&mut self) -> String {
        Vm::log_date(self, php_support::time::now())
    }

    fn error_log_mode(&mut self) -> i64 {
        self.ini.get_long("error_log_mode")
    }

    fn syslog_settings(&mut self) -> php_log::SyslogSettings {
        Vm::syslog_settings(self)
    }

    /// `sapi_module.log_message`. The VM's own output buffer is flushed
    /// first, so that the log line follows the output written before it.
    fn sapi_log_message(&mut self, message: &[u8], severity: i32) {
        self.flush_stdout();
        let module = self.sapi_module.clone();
        module.log_message(message, severity);
    }
}
