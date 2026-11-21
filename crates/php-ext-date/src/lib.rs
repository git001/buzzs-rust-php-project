// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/date` for the phprust VM: binds the `php-date` timelib port through
//! the `php_vm::ext` extension API.
//!
//! * [`functions`] — `date()`, `strtotime()`, `mktime()`, `date_parse()`,
//!   `strftime()`, the timezone listings and the sun functions.
//! * [`datetime`] — `DateTimeInterface`, `DateTime`, `DateTimeImmutable` and
//!   the procedural `date_*()` aliases.
//! * [`timezone`] — `DateTimeZone` and `timezone_*()`.
//! * [`interval`] — `DateInterval` with its property handlers.
//! * [`period`] — `DatePeriod`, its overloaded constructor and iteration.
//! * [`duration`] — `Time\Duration` (PHP 8.6).
//!
//! Native state lives in the objects (see [`objects`]); the per-request
//! globals (`DATEG(timezone)`, `DATEG(last_errors)`) in [`common`].

// Native handlers and object hooks have signatures fixed by the VM (they
// return `VmResult` even when they cannot fail), and the code keeps
// timelib's one-letter field names (y, m, d, h, i, s).
#![allow(clippy::unnecessary_wraps, clippy::many_single_char_names)]

mod common;
mod datetime;
mod duration;
mod functions;
mod interval;
mod objects;
mod period;
mod timezone;

use php_date::api;
use php_runtime::Value;
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{ClassBuilder, Extension, InfoItem, ModuleBuilder, VmResult};
use php_vm::Vm;

/// The `date` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Date;

/// The exception hierarchy added in PHP 8.3 (`parent`, `class`).
const EXCEPTIONS: [(&str, &str); 9] = [
    ("Error", "DateError"),
    ("DateError", "DateObjectError"),
    ("DateError", "DateRangeError"),
    ("Exception", "DateException"),
    ("DateException", "DateInvalidTimeZoneException"),
    ("DateException", "DateInvalidOperationException"),
    ("DateException", "DateMalformedStringException"),
    ("DateException", "DateMalformedIntervalStringException"),
    ("DateException", "DateMalformedPeriodStringException"),
];

/// The current `date.timezone` if it is a valid identifier
/// (`DATEG(default_timezone)`), else `UTC`.
fn valid_ini_timezone(vm: &Vm) -> String {
    vm.ini_string("date.timezone")
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .filter(|s| php_date::tz::is_valid_id(s))
        .unwrap_or_else(|| "UTC".to_string())
}

/// `OnUpdate_date_timezone`: rejects unknown identifiers with a warning;
/// an accepted one becomes the configured default timezone.
fn on_update_timezone(vm: &mut Vm, v: &[u8]) -> VmResult<bool> {
    let name = String::from_utf8_lossy(v).into_owned();
    if php_date::tz::is_valid_id(&name) {
        common::set_configured_tz(Some(v));
        return Ok(true);
    }
    let cur = valid_ini_timezone(vm);
    vm.php_warning(&format!(
        "Invalid date.timezone value '{name}', using '{cur}' instead"
    ))?;
    Ok(false)
}

/// Request startup: the configured default timezone from `date.timezone`
/// (the request's value, also after the previous request's `ini_set()`
/// was undone), and the startup check of `-d`/php.ini values.
fn check_startup_timezone(vm: &mut Vm) {
    let v = vm.ini_string("date.timezone");
    common::set_configured_tz(v.as_deref());
    let Some(v) = v else {
        return;
    };
    let name = String::from_utf8_lossy(&v).into_owned();
    if !php_date::tz::is_valid_id(&name) {
        let _ = vm.raise(
            common::E_WARNING,
            &format!("PHP Startup: Invalid date.timezone value '{name}', using 'UTC' instead"),
        );
    }
}

/// `PHP_MINFO_FUNCTION(date)`.
fn module_info(vm: &mut Vm) -> Vec<InfoItem> {
    let row = |a: &str, b: &str| InfoItem::Row(vec![a.to_string(), b.to_string()]);
    vec![
        InfoItem::TableStart,
        row("date/time support", "enabled"),
        row("timelib version", "2026.02"),
        row(
            "\"Olson\" Timezone Database Version",
            php_date::tz::version(),
        ),
        row("Timezone Database", "internal"),
        row("Default timezone", &php_date::tz::default_zone().name),
        InfoItem::TableEnd,
        InfoItem::IniEntries,
    ]
}

impl Extension for Date {
    fn name(&self) -> &'static str {
        "date"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for (name, default) in [
            ("date.default_latitude", "31.7667"),
            ("date.default_longitude", "35.2333"),
            ("date.sunset_zenith", "90.833333"),
            ("date.sunrise_zenith", "90.833333"),
        ] {
            m.ini(IniDirective::new(
                name,
                Some(default),
                Access::ALL,
                ModifyHandler::None,
                "date",
            ));
        }
        for (name, v) in [
            ("SUNFUNCS_RET_TIMESTAMP", api::SUNFUNCS_RET_TIMESTAMP),
            ("SUNFUNCS_RET_STRING", api::SUNFUNCS_RET_STRING),
            ("SUNFUNCS_RET_DOUBLE", api::SUNFUNCS_RET_DOUBLE),
        ] {
            m.constant_deprecated(
                name,
                Value::Int(v),
                "8.4",
                "as date_sunrise() and date_sunset() were deprecated in 8.1",
            );
        }

        {
            let vm = m.vm();
            for (parent, name) in EXCEPTIONS {
                ClassBuilder::new(name).extends(vm, parent).register(vm);
            }
        }

        datetime::register(m);
        timezone::register(m);
        interval::register(m);
        period::register(m);
        duration::register(m);

        functions::register(m);
        datetime::register_functions(m);

        m.ini_on_modify("date.timezone", on_update_timezone);
        m.on_request_startup(check_startup_timezone);
        m.info(module_info);

        m.on_request_shutdown(|_| {
            common::reset_request_state();
            duration::reset_cache();
        });
    }
}
