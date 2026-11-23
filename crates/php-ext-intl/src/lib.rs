// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/intl` for the phprust VM: binds the `php-intl` ICU library through
//! the `php_vm::ext` extension API.
//!
//! The class, method and function declarations come from the php-src
//! stubs ([`stubs`], generated); every entry is bound by its key
//! (`"Class::method"`, or the function name) to a handler in one of the
//! class modules.  Procedural aliases share the method handler: when
//! called without `$this` the object is the first argument (see
//! [`core::target`]).  The intl error model (`intl_error_set()`, the
//! `intl.error_level`/`intl.use_exceptions` INI settings) lives in
//! [`core`].
// `vm`, `a` (arguments), `o` (object) and `s` (string value) are the
// conventional names throughout the binding handlers.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::many_single_char_names
)]

mod breakiter;
mod calendar;
mod collator;
mod converter;
mod core;
mod dateformat;
mod dt;
mod intlchar;
mod iter;
mod locale;
mod misc;
mod msgformat;
mod newfmt;
mod numfmt;
mod registry;
mod resbundle;
mod spoof;
mod stubs;
mod text;
mod timezone;
mod translit;
mod vmbridge;

use std::collections::HashMap;

use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{Extension, InfoItem, ModuleBuilder, NativeFn, VmResult};
use php_vm::Vm;

/// The `intl` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Intl;

/// Stores `intl.default_locale` (`zend_alter_ini_entry` from
/// `Locale::setDefault()`) and syncs the library's copy.
pub(crate) fn set_default_locale_ini(vm: &mut Vm, value: &str) {
    let mut sink: Vec<php_support::Diagnostic> = Vec::new();
    let _ = vm
        .ini
        .ini_set("intl.default_locale", value.as_bytes(), &mut sink);
    sync_default_locale(vm);
}

/// Copies `intl.default_locale` into the library.
pub(crate) fn sync_default_locale(vm: &Vm) {
    let v = vm.ini_string("intl.default_locale");
    let v = v.map(|b| String::from_utf8_lossy(&b).into_owned());
    php_intl::common::set_ini_default_locale(v.as_deref());
}

/// `OnUpdateStringUnempty` for `intl.default_locale`: the library keeps
/// its own copy.
fn on_update_default_locale(_vm: &mut Vm, value: &[u8]) -> VmResult<bool> {
    if value.is_empty() {
        return Ok(false);
    }
    php_intl::common::set_ini_default_locale(Some(&String::from_utf8_lossy(value)));
    Ok(true)
}

/// `OnUpdateErrorLevel`: any non-zero level is deprecated.
fn on_update_error_level(vm: &mut Vm, value: &[u8]) -> VmResult<bool> {
    if php_intl::common::php_str_to_long(value) != 0 {
        vm.php_deprecated(php_intl::error::ERROR_LEVEL_DEPRECATION)?;
    }
    Ok(true)
}

/// `PHP_MINFO(intl)`.
fn module_info(_vm: &mut Vm) -> Vec<InfoItem> {
    let tz = php_intl::timezone::tz_data_version().unwrap_or_default();
    let [major, minor, ..] = php_intl::intlchar::get_unicode_version();
    vec![
        InfoItem::TableStart,
        InfoItem::Header(vec![
            "Internationalization support".into(),
            "enabled".into(),
        ]),
        InfoItem::Row(vec![
            "ICU version".into(),
            php_intl::common::ICU_VERSION.into(),
        ]),
        InfoItem::Row(vec![
            "ICU Data version".into(),
            php_intl::common::ICU_DATA_VERSION.into(),
        ]),
        InfoItem::Row(vec!["ICU TZData version".into(), tz]),
        InfoItem::Row(vec![
            "ICU Unicode version".into(),
            format!("{major}.{minor}"),
        ]),
        InfoItem::TableEnd,
        InfoItem::IniEntries,
    ]
}

fn all_handlers() -> HashMap<&'static str, NativeFn> {
    let mut h = HashMap::new();
    for list in [
        misc::handlers(),
        locale::handlers(),
        collator::handlers(),
        breakiter::handlers(),
        converter::handlers(),
        resbundle::handlers(),
        spoof::handlers(),
        intlchar::handlers(),
        iter::handlers(),
        timezone::handlers(),
        calendar::handlers(),
        dateformat::handlers(),
        numfmt::handlers(),
        msgformat::handlers(),
        newfmt::handlers(),
        text::handlers(),
        translit::handlers(),
    ] {
        for (k, f) in list {
            h.insert(k, f);
        }
    }
    h
}

fn class_opts(name: &str) -> registry::ClassOpts {
    use php_vm::ext::{class_flags, ClassHooks};
    let mut o = registry::ClassOpts::default();
    match name {
        "Collator"
        | "IntlNumberFormatter"
        | "IntlListFormatter"
        | "IntlNumberRangeFormatter"
        | "IntlRelativeDateTimeFormatter" => {
            o.flags |= class_flags::NO_CLONE;
        }
        "ResourceBundle" => {
            o.flags |= class_flags::NO_CLONE;
            o.hooks = ClassHooks {
                read_dimension: Some(resbundle::read_dimension),
                write_dimension: Some(resbundle::write_dimension),
                count: Some(resbundle::count_elements),
                get_iterator: Some(resbundle::get_iterator),
                ..ClassHooks::default()
            };
        }
        "IntlIterator" | "IntlPartsIterator" => {
            o.flags |= class_flags::NO_CLONE;
            o.hooks = ClassHooks {
                get_iterator: Some(iter::get_iterator),
                ..ClassHooks::default()
            };
        }
        "IntlTimeZone" => {
            o.hooks = ClassHooks {
                clone: Some(timezone::clone),
                compare: Some(timezone::compare),
                properties_for: Some(timezone::debug_info),
                ..ClassHooks::default()
            };
        }
        "NumberFormatter" => {
            o.hooks = ClassHooks {
                clone: Some(numfmt::clone),
                ..ClassHooks::default()
            };
        }
        "MessageFormatter" => {
            o.hooks = ClassHooks {
                clone: Some(msgformat::clone),
                ..ClassHooks::default()
            };
        }
        "IntlBreakIterator" | "IntlRuleBasedBreakIterator" | "IntlCodePointBreakIterator" => {
            o.hooks = ClassHooks {
                clone: Some(breakiter::clone),
                compare: Some(breakiter::compare),
                properties_for: Some(breakiter::debug_info),
                get_iterator: Some(breakiter::get_iterator),
                ..ClassHooks::default()
            };
        }
        "UConverter" => {
            o.hooks = ClassHooks {
                create: Some(converter::create),
                clone: Some(converter::clone),
                destruct: Some(converter::destruct),
                ..ClassHooks::default()
            };
        }
        "Spoofchecker" => {
            o.hooks = ClassHooks {
                clone: Some(spoof::clone),
                ..ClassHooks::default()
            };
        }
        "IntlDatePatternGenerator" => {
            o.hooks = ClassHooks {
                clone: Some(intlchar::dtpg_clone),
                ..ClassHooks::default()
            };
        }
        "Transliterator" => {
            o.hooks = ClassHooks {
                clone: Some(translit::clone),
                ..ClassHooks::default()
            };
        }
        "IntlDateFormatter" => {
            o.hooks = ClassHooks {
                clone: Some(dateformat::clone),
                ..ClassHooks::default()
            };
        }
        "IntlCalendar" | "IntlGregorianCalendar" => {
            o.hooks = ClassHooks {
                clone: Some(calendar::clone),
                properties_for: Some(calendar::debug_info),
                ..ClassHooks::default()
            };
        }
        _ => {}
    }
    o
}

impl Extension for Intl {
    fn name(&self) -> &'static str {
        "intl"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        php_vm::gc::register_traverse_with::<core::IntlObj>(core::gc_traverse);
        m.ini(IniDirective::new(
            "intl.default_locale",
            None,
            Access::ALL,
            ModifyHandler::StringUnempty,
            "intl",
        ));
        m.ini(IniDirective::new(
            "intl.error_level",
            Some("0"),
            Access::ALL,
            ModifyHandler::Long,
            "intl",
        ));
        m.ini(IniDirective::new(
            "intl.use_exceptions",
            Some("0"),
            Access::ALL,
            ModifyHandler::Bool,
            "intl",
        ));
        sync_default_locale(m.vm());
        let handlers = all_handlers();
        registry::register(m, &handlers, &mut class_opts);
        m.ini_on_modify("intl.default_locale", on_update_default_locale);
        m.ini_on_modify("intl.error_level", on_update_error_level);
        m.info(module_info);
        m.on_request_startup(|vm| {
            core::reset_global();
            sync_default_locale(vm);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stub_entry_has_a_handler() {
        let h = all_handlers();
        for c in stubs::CLASSES {
            for m in c.methods {
                if c.kind == registry::ClassKind::Interface || m.is_abstract {
                    continue;
                }
                assert!(h.contains_key(m.key), "no handler for {}", m.key);
            }
        }
        for f in stubs::FUNCTIONS {
            assert!(h.contains_key(f.key), "no handler for {}", f.key);
        }
    }

    #[test]
    fn procedural_aliases_share_method_handlers() {
        let get = stubs::FUNCTIONS
            .iter()
            .find(|f| f.sig.starts_with("intlcal_get("))
            .map(|f| f.key);
        assert_eq!(get, Some("IntlCalendar::get"));
        let set = stubs::FUNCTIONS
            .iter()
            .find(|f| f.sig.starts_with("intlcal_set("))
            .and_then(|f| f.deprecated);
        assert!(set.is_some_and(|(since, _)| since == "8.4"));
    }

    #[test]
    fn class_constant_defaults_are_resolved() {
        let create = stubs::CLASSES
            .iter()
            .find(|c| c.name == "IntlDateFormatter")
            .and_then(|c| c.methods.iter().find(|m| m.sig.starts_with("create(")))
            .map(|m| m.sig);
        assert!(create.is_some_and(
            |s| s.contains("int $dateType = 0") && s.contains("object|string|null $timezone")
        ));
        let extract = stubs::FUNCTIONS
            .iter()
            .find(|f| f.sig.starts_with("grapheme_extract("))
            .map(|f| f.sig);
        assert!(extract.is_some_and(|s| s.contains("int $type = 0")));
    }

    #[test]
    fn parents_are_registered_first() {
        let pos = |n: &str| stubs::CLASSES.iter().position(|c| c.name == n);
        assert!(pos("IntlIterator") < pos("IntlPartsIterator"));
        assert!(pos("IntlBreakIterator") < pos("IntlRuleBasedBreakIterator"));
        assert!(pos("IntlCalendar") < pos("IntlGregorianCalendar"));
    }

    #[test]
    fn class_options() {
        use php_vm::ext::class_flags;
        assert_ne!(class_opts("Collator").flags & class_flags::NO_CLONE, 0);
        assert_eq!(class_opts("IntlCalendar").flags & class_flags::NO_CLONE, 0);
        assert!(class_opts("IntlCalendar").hooks.clone.is_some());
        assert!(class_opts("ResourceBundle").hooks.read_dimension.is_some());
        assert!(class_opts("IntlIterator").hooks.get_iterator.is_some());
    }
}
