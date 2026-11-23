// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Expectations hand-ported from ext/intl/tests (locale_*.phpt).

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_intl::locale::{self, LookupEntry};
use php_intl::UErrorCode;

#[test]
fn locale_is_right_to_left() {
    assert!(!locale::is_right_to_left("en-US"));
    assert!(!locale::is_right_to_left("\\INVALID\\"));
    php_intl::common::set_ini_default_locale(Some("en_US"));
    assert!(!locale::is_right_to_left(""));
    assert!(locale::is_right_to_left("ar"));
}

#[test]
fn locale_subtags() {
    let max = locale::add_likely_subtags("en").unwrap();
    assert_eq!(locale::minimize_subtags(&max).unwrap(), "en");
    assert!(max.len() > 2);
    assert_eq!(locale::add_likely_subtags(&max).unwrap(), max);
    assert_eq!(locale::minimize_subtags("en").unwrap(), "en");
    let e = locale::add_likely_subtags("%%%invalid%%%locale%%%").unwrap_err();
    assert_eq!(
        (e.intl_code(), e.intl_message()),
        (UErrorCode::ILLEGAL_ARGUMENT_ERROR, Some("invalid locale"))
    );
    let e = locale::minimize_subtags("%%%Invalid%%%maximized%%%locale%%%").unwrap_err();
    assert_eq!(e.intl_code(), UErrorCode::ILLEGAL_ARGUMENT_ERROR);
    let e = locale::add_likely_subtags(&"en".repeat(1024)).unwrap_err();
    assert_eq!(e.intl_code(), UErrorCode::ILLEGAL_ARGUMENT_ERROR);
    assert!(locale::minimize_subtags(&max.repeat(1024)).is_err());
}

#[test]
fn locale_bug74993_lookup_keywords() {
    let t = [LookupEntry::Str(b"en"), LookupEntry::Str(b"en-US")];
    assert_eq!(
        locale::lookup(&t, "en-US-u-cu-EUR-tz-deber-fw-mon", true, None)
            .unwrap()
            .unwrap(),
        "en_us"
    );
    let t = [LookupEntry::Str(b"en"), LookupEntry::Str(b"en_US")];
    let r = "en_US@currency=eur;fw=mon;timezone=Europe/Berlin";
    assert_eq!(
        locale::lookup(&t, r, false, None).unwrap().unwrap(),
        "en_US"
    );
    assert_eq!(locale::lookup(&t, r, true, None).unwrap().unwrap(), "en_us");
}

#[test]
fn locale_get_display_keyword_error() {
    let e =
        locale::get_display_keyword_value(&"*".repeat(256), "calendar", Some("en")).unwrap_err();
    assert_eq!(e.intl_message(), Some("name too long"));
    assert_eq!(e.intl_code(), UErrorCode::ILLEGAL_ARGUMENT_ERROR);
    assert_eq!(
        locale::get_display_keyword("currency", Some("fr")).unwrap(),
        "devise"
    );
    assert_eq!(
        locale::get_display_keyword_value("de_DE@calendar=gregorian", "calendar", Some("en"))
            .unwrap(),
        "Gregorian Calendar"
    );
}
